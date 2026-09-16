//! Client for the Zotero **web API** (api.zotero.org) — used only by write
//! commands that the local API cannot serve (`edit`, `attach`, `rm`).
//!
//! Reads remain 100% local. Writes here sync back to the desktop app via
//! Zotero sync (typically within seconds when auto-sync is on).

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::config::Config;

const WEB_API_BASE: &str = "https://api.zotero.org";

pub struct WebApiClient {
    client: Client,
    api_key: String,
    user_id: u64,
}

/// A PATCH refused by `If-Unmodified-Since-Version`: the item changed between
/// the read and the write, so nothing was written.
///
/// Typed rather than a plain message, because the remedy depends on the
/// command: re-running is right for `zot edit`, but would add a second item
/// under `zot add`. Its `Display` is the `zot edit` wording, which every
/// caller that does not handle the type itself inherits.
#[derive(Debug)]
pub struct VersionConflict {
    pub key: String,
    /// Version the PATCH body was computed from.
    pub read_version: u64,
}

impl std::fmt::Display for VersionConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Item {} changed on api.zotero.org since it was read at version {}, so nothing was \
             written.\n  Writing now would drop that other change -- re-run the same command to \
             apply yours on top of it.",
            self.key, self.read_version,
        )
    }
}

impl std::error::Error for VersionConflict {}

impl WebApiClient {
    /// Build a client from stored config (`zot config set-key`), resolving and
    /// caching the numeric user ID on first use.
    pub fn from_config() -> Result<Self> {
        let mut cfg = Config::load()?;
        let Some(api_key) = cfg.resolve_api_key() else {
            bail!(
                "No Zotero API key configured.\n  \
                 This command writes via the Zotero web API (the local API is read-only).\n  \
                 Create a write-enabled key at https://www.zotero.org/settings/keys\n  \
                 then run: zot config set-key <KEY>"
            );
        };

        let client = Client::builder()
            .user_agent(format!("zot/{}", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("Failed to create HTTP client")?;

        let user_id = match cfg.user_id {
            Some(id) => id,
            None => {
                let url = format!("{WEB_API_BASE}/keys/current");
                let resp = client
                    .get(&url)
                    .header("Zotero-API-Version", "3")
                    .header("Zotero-API-Key", &api_key)
                    .send()
                    .context("Failed to reach api.zotero.org")?;
                if !resp.status().is_success() {
                    bail!(
                        "API key rejected by api.zotero.org (status {}). \
                         Check the key with: zot config show",
                        resp.status()
                    );
                }
                let v: Value = resp.json().context("Failed to parse /keys/current")?;
                let id = v
                    .get("userID")
                    .and_then(|u| u.as_u64())
                    .context("No userID in /keys/current response")?;
                // Cache for next time (only if the key came from config, env
                // keys may belong to another account -- still fine to cache the
                // id alongside, we re-derive when it mismatches on 403s).
                cfg.user_id = Some(id);
                let _ = cfg.save();
                id
            }
        };

        Ok(Self {
            client,
            api_key,
            user_id,
        })
    }

    fn items_url(&self, suffix: &str) -> String {
        format!("{WEB_API_BASE}/users/{}/items{}", self.user_id, suffix)
    }

    fn collections_url(&self) -> String {
        format!("{WEB_API_BASE}/users/{}/collections", self.user_id)
    }

    fn auth(&self, req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        req.header("Zotero-API-Version", "3")
            .header("Zotero-API-Key", &self.api_key)
    }

    /// Fetch an item's current server-side JSON, distinguishing "not there"
    /// from "could not ask".
    ///
    /// `Ok(None)` is the 404 alone, which is what a caller waiting for a sync
    /// should retry; a revoked key, a server error or a transport failure stay
    /// `Err`, so such a caller gives up on them immediately.
    pub fn get_item_opt(&self, key: &str) -> Result<Option<Value>> {
        let url = self.items_url(&format!("/{key}"));
        let resp = self
            .auth(self.client.get(&url))
            .send()
            .context("Failed to fetch item from web API")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("Web API returned {} fetching {key}", resp.status());
        }
        let v: Value = resp.json().context("Failed to parse item JSON")?;
        Ok(Some(v))
    }

    /// Fetch an item's current server-side JSON (for version + field values).
    pub fn get_item(&self, key: &str) -> Result<Value> {
        match self.get_item_opt(key)? {
            Some(v) => Ok(v),
            None => bail!(
                "Item {key} not found on api.zotero.org.\n  \
                 If it exists locally, it may not have synced up yet -- sync Zotero and retry."
            ),
        }
    }

    /// PATCH an item with the given partial `data` object, using optimistic
    /// concurrency (`If-Unmodified-Since-Version`). Returns the new item version.
    ///
    /// `if_unmodified_version` must be the version of the read `data` was
    /// computed from: a body that merges current state (tags, collections) is
    /// a full replacement, so guarding it with any newer version would let a
    /// concurrent change be silently overwritten. A 412 is therefore fatal --
    /// the merge has to be redone against the new state, which only the caller
    /// can do.
    pub fn patch_item(&self, key: &str, data: &Value, if_unmodified_version: u64) -> Result<u64> {
        let url = self.items_url(&format!("/{key}"));
        let resp = self
            .auth(self.client.patch(&url))
            .header("If-Unmodified-Since-Version", if_unmodified_version.to_string())
            .header("Content-Type", "application/json")
            .body(data.to_string())
            .send()
            .context("Failed to PATCH item")?;
        let status = resp.status();
        if status.is_success() {
            let new_version = resp
                .headers()
                .get("Last-Modified-Version")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(if_unmodified_version + 1);
            return Ok(new_version);
        }
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(VersionConflict {
                key: key.to_string(),
                read_version: if_unmodified_version,
            }
            .into());
        }
        let body = resp.text().unwrap_or_default();
        bail!("Web API PATCH failed (status {status}): {}", body.trim());
    }

    /// Create items (POST). Returns the response JSON (`successful` map etc.).
    pub fn create_items(&self, items: &Value) -> Result<Value> {
        let url = self.items_url("");
        let resp = self
            .auth(self.client.post(&url))
            .header("Content-Type", "application/json")
            .body(items.to_string())
            .send()
            .context("Failed to POST items")?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!("Web API item creation failed (status {status}): {}", body.trim());
        }
        let v: Value = serde_json::from_str(&body).context("Failed to parse creation response")?;
        // Per-item failures come back inside a 200 response.
        if let Some(failed) = v.get("failed").and_then(|f| f.as_object()) {
            if !failed.is_empty() {
                bail!("Web API rejected item(s): {}", serde_json::to_string(failed)?);
            }
        }
        Ok(v)
    }

    /// Create one collection (POST). Returns its new key.
    ///
    /// `parent` is the key of the parent collection, or `None` for a top-level
    /// one. The body is an array because Zotero's write endpoints only take
    /// batches, even of one.
    pub fn create_collection(&self, name: &str, parent: Option<&str>) -> Result<String> {
        let body = json!([{
            "name": name,
            "parentCollection": match parent {
                Some(key) => json!(key),
                None => json!(false),
            },
        }]);
        let resp = self
            .auth(self.client.post(self.collections_url()))
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .context("Failed to POST collection")?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Web API collection creation failed (status {status}): {}",
                text.trim()
            );
        }
        let v: Value =
            serde_json::from_str(&text).context("Failed to parse collection creation response")?;
        created_collection_key(&v)
    }

    /// Upload a file for an existing attachment item (Zotero 3-step flow:
    /// authorize -> upload -> register).
    pub fn upload_attachment_file(&self, attachment_key: &str, path: &std::path::Path) -> Result<()> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|f| f.to_str())
            .context("Invalid filename")?;
        let md5_hex = hex(&Md5::digest(&bytes));
        let mtime_ms = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // 1. Authorize upload.
        let url = self.items_url(&format!("/{attachment_key}/file"));
        let form = format!(
            "md5={}&filename={}&filesize={}&mtime={}",
            md5_hex,
            urlencoding::encode(filename),
            bytes.len(),
            mtime_ms,
        );
        let resp = self
            .auth(self.client.post(&url))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("If-None-Match", "*")
            .body(form)
            .send()
            .context("Failed to authorize upload")?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!("Upload authorization failed (status {status}): {}", body.trim());
        }
        let auth: Value = serde_json::from_str(&body).context("Failed to parse upload auth")?;
        if auth.get("exists").and_then(|e| e.as_u64()) == Some(1) {
            return Ok(()); // Identical file already stored.
        }

        // 2. Upload prefix + file + suffix to the storage URL.
        let upload_url = auth
            .get("url")
            .and_then(|u| u.as_str())
            .context("No upload URL in authorization")?;
        let content_type = auth
            .get("contentType")
            .and_then(|c| c.as_str())
            .unwrap_or("application/octet-stream");
        let prefix = auth.get("prefix").and_then(|p| p.as_str()).unwrap_or("");
        let suffix = auth.get("suffix").and_then(|s| s.as_str()).unwrap_or("");
        let mut payload = Vec::with_capacity(prefix.len() + bytes.len() + suffix.len());
        payload.extend_from_slice(prefix.as_bytes());
        payload.extend_from_slice(&bytes);
        payload.extend_from_slice(suffix.as_bytes());
        let resp = self
            .client
            .post(upload_url)
            .header("Content-Type", content_type)
            .body(payload)
            .send()
            .context("Failed to upload file to storage")?;
        if !resp.status().is_success() {
            bail!("Storage upload failed (status {})", resp.status());
        }

        // 3. Register upload.
        let upload_key = auth
            .get("uploadKey")
            .and_then(|k| k.as_str())
            .context("No uploadKey in authorization")?;
        let resp = self
            .auth(self.client.post(&url))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("If-None-Match", "*")
            .body(format!("upload={upload_key}"))
            .send()
            .context("Failed to register upload")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            bail!(
                "Upload registration failed (status {status}): {}",
                body.trim()
            );
        }
        Ok(())
    }
}

/// Pull the created collection's key out of a Zotero write response.
///
/// Zotero answers a write with 200 and a per-index verdict, so a rejected
/// object arrives looking like a success and has to be turned back into an
/// error here. Only one collection is ever sent, hence index `0`.
fn created_collection_key(resp: &Value) -> Result<String> {
    if let Some((index, failure)) = resp
        .get("failed")
        .and_then(|f| f.as_object())
        .and_then(|f| f.iter().next())
    {
        let message = failure
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("no message given");
        match failure.get("code").and_then(|c| c.as_u64()) {
            Some(code) => bail!("Zotero rejected the collection (code {code}): {message}"),
            None => bail!("Zotero rejected the collection (index {index}): {message}"),
        }
    }
    // `successful` carries the whole object, `success` only the key; either
    // alone is enough to name what was created.
    resp.pointer("/successful/0/key")
        .or_else(|| resp.pointer("/success/0"))
        .and_then(|k| k.as_str())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "Zotero reported neither a created collection nor a failure: {}",
                crate::output::truncate_display(&resp.to_string(), 300)
            )
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::created_collection_key;
    use serde_json::json;

    /// Captured from `POST /users/<id>/collections` on api.zotero.org,
    /// 2026-09-16, trimmed of `library` and `links`.
    #[test]
    fn created_collection_key_reads_a_successful_write() {
        let resp = json!({
            "successful": {
                "0": {
                    "key": "XMJ5WEXR",
                    "version": 16842,
                    "meta": { "numCollections": 0, "numItems": 0 },
                    "data": {
                        "key": "XMJ5WEXR",
                        "version": 16842,
                        "name": "zot-test-root",
                        "parentCollection": false,
                        "relations": {},
                    },
                },
            },
            "success": { "0": "XMJ5WEXR" },
            "unchanged": {},
            "failed": {},
        });
        assert_eq!(created_collection_key(&resp).unwrap(), "XMJ5WEXR");
    }

    /// Same endpoint, same day, sending `parentCollection: "ZZZZZZZZ"`. The
    /// status was 200: only the body says it failed.
    #[test]
    fn created_collection_key_turns_a_failed_entry_into_an_error() {
        let resp = json!({
            "successful": {},
            "success": {},
            "unchanged": {},
            "failed": {
                "0": {
                    "code": 409,
                    "message": "Parent collection ZZZZZZZZ not found",
                    "data": { "collection": "ZZZZZZZZ" },
                },
            },
        });
        let err = created_collection_key(&resp).unwrap_err().to_string();
        assert!(err.contains("409"), "{err}");
        assert!(err.contains("Parent collection ZZZZZZZZ not found"), "{err}");
    }

    #[test]
    fn created_collection_key_falls_back_to_the_success_map() {
        let resp = json!({ "success": { "0": "ABCD1234" }, "failed": {} });
        assert_eq!(created_collection_key(&resp).unwrap(), "ABCD1234");
    }

    #[test]
    fn created_collection_key_rejects_a_body_reporting_nothing() {
        let resp = json!({ "successful": {}, "success": {}, "failed": {} });
        let err = created_collection_key(&resp).unwrap_err().to_string();
        assert!(err.contains("neither a created collection nor a failure"), "{err}");
    }
}
