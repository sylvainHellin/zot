//! Client for the Zotero **web API** (api.zotero.org).
//!
//! Every write goes through here, because the local API is read-only; writes
//! sync back to the desktop app via Zotero sync (typically within seconds when
//! auto-sync is on). Reads go to the local API while the desktop app runs, and
//! fall back to this client when it does not (see `ZoteroClient::new`).

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};
use reqwest::blocking::Client;
use serde_json::{json, Value};

use crate::config::Config;

const WEB_API_BASE: &str = "https://api.zotero.org";

/// Maximum number of tags the web API deletes in one request (v3 limit; more
/// than this is answered with 413).
pub const TAG_DELETE_BATCH: usize = 50;

/// How many times a request throttled by api.zotero.org is retried before the
/// command gives up and says so.
const MAX_RETRIES: u32 = 3;

/// Ceiling on any single wait asked for by `Backoff` or `Retry-After`. A
/// server that asks for an hour gets a command that fails with a message
/// instead of a terminal that looks hung.
const MAX_WAIT_SECS: u64 = 120;

pub struct WebApiClient {
    client: Client,
    api_key: String,
    user_id: u64,
    /// [`WEB_API_BASE`] in use; a field so the tests can point the client at a
    /// local stub server.
    base: String,
}

/// A write refused by `If-Unmodified-Since-Version`: the object changed
/// between the read and the write, so nothing was written.
///
/// Typed rather than a plain message, because the remedy depends on the
/// command: re-running is right for `zot edit`, but would add a second item
/// under `zot add`. Its `Display` is the `zot edit` wording, which every
/// caller that does not handle the type itself inherits.
#[derive(Debug)]
pub struct VersionConflict {
    pub key: String,
    /// Version the write was computed from.
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
    /// Whether a key is available at all (env or config), so a caller can
    /// decide on a fallback without turning "no key" into an error.
    pub fn is_configured() -> bool {
        Config::load()
            .map(|cfg| cfg.resolve_api_key().is_some())
            .unwrap_or(false)
    }

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
            base: WEB_API_BASE.to_string(),
        })
    }

    /// Root of the user library on api.zotero.org, the web counterpart of the
    /// local API's `http://localhost:23119/api/users/0`. Every read path below
    /// it is the same on both, which is what lets `ZoteroClient` swap one for
    /// the other.
    pub fn library_base(&self) -> String {
        format!("{}/users/{}", self.base, self.user_id)
    }

    /// Authenticated GET of an absolute URL, honouring throttling like every
    /// other request here.
    pub fn get_url(&self, url: &str) -> Result<reqwest::blocking::Response> {
        self.send_retrying(|| self.auth(self.client.get(url)))
    }

    /// Download an attachment's file into `dest`, returning `false` when the
    /// server holds no file for it (404: never uploaded, or stored on WebDAV).
    ///
    /// `/file/view` rather than `/file`: for a web snapshot `/file` returns the
    /// ZIP Zotero stores, `/file/view` the main file unpacked, and for any
    /// other attachment both return the same bytes. The body is written to a
    /// sibling temp file and renamed, so an interrupted download never leaves
    /// a truncated file under the final name.
    pub fn download_attachment(&self, key: &str, dest: &std::path::Path) -> Result<bool> {
        let url = self.items_url(&format!("/{key}/file/view"));
        let resp = self.get_url(&url)?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !status.is_success() {
            bail!("Web API returned {status} downloading the file of {key}");
        }
        let bytes = resp
            .bytes()
            .with_context(|| format!("Failed to read the file of {key}"))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let tmp = dest.with_extension("part");
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("Failed to move the download to {}", dest.display()))?;
        Ok(true)
    }

    fn items_url(&self, suffix: &str) -> String {
        format!("{}/items{suffix}", self.library_base())
    }

    fn collections_url(&self, suffix: &str) -> String {
        format!("{}/collections{suffix}", self.library_base())
    }

    fn tags_url(&self, suffix: &str) -> String {
        format!("{}/tags{suffix}", self.library_base())
    }

    fn auth(&self, req: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        req.header("Zotero-API-Version", "3")
            .header("Zotero-API-Key", &self.api_key)
    }

    /// Send a request, honouring Zotero's two throttling signals.
    ///
    /// `Backoff: <n>` can ride on any response, successful ones included, and
    /// means "pause everything for n seconds"; `429` (and `503` during
    /// maintenance) carries `Retry-After` and means "this request did not
    /// happen, come back later". The first is honoured after the fact by
    /// sleeping before the response is handed back, the second by retrying the
    /// same request. `build` is a closure rather than a request because a
    /// retried request has to be constructed again from scratch.
    ///
    /// Every request to api.zotero.org goes through here, writes included: a
    /// throttled write did not happen, so sending it again is safe. Creating
    /// POSTs also carry a `Zotero-Write-Token` (see [`write_token`]), which
    /// keeps even a retry of a request the server did process from creating
    /// the object twice.
    fn send_retrying(
        &self,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response> {
        let mut attempt = 0;
        loop {
            let resp = build().send().context("Failed to reach api.zotero.org")?;
            let status = resp.status();
            let throttled = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE;
            if throttled && attempt < MAX_RETRIES {
                let wait = header_secs(resp.headers(), "Retry-After")
                    .or_else(|| header_secs(resp.headers(), "Backoff"))
                    .unwrap_or(1 << attempt)
                    .min(MAX_WAIT_SECS);
                eprintln!("api.zotero.org returned {status}; waiting {wait}s before retrying...");
                std::thread::sleep(std::time::Duration::from_secs(wait));
                attempt += 1;
                continue;
            }
            if let Some(secs) = header_secs(resp.headers(), "Backoff") {
                let wait = secs.min(MAX_WAIT_SECS);
                eprintln!("api.zotero.org asked for a {wait}s backoff; pausing...");
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
            return Ok(resp);
        }
    }

    /// The library's current version, which is what a library-level write
    /// (`delete_tags`) has to guard itself with.
    ///
    /// Read from the `Last-Modified-Version` header of the cheapest read there
    /// is: every response carries it, and `format=versions&limit=1` keeps the
    /// body to a single entry.
    pub fn library_version(&self) -> Result<u64> {
        let url = self.items_url("?format=versions&limit=1");
        let resp = self.send_retrying(|| self.auth(self.client.get(&url)))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("Web API returned {status} reading the library version");
        }
        resp.headers()
            .get("Last-Modified-Version")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .context("No Last-Modified-Version header on the library read")
    }

    /// How many items on api.zotero.org carry `tag`.
    ///
    /// Read from `Total-Results` on a one-row request, so the count costs one
    /// small response whatever the library holds. Both tag types match, as on
    /// the local API, which is what makes the two counts comparable.
    pub fn count_items_with_tag(&self, tag: &str) -> Result<u64> {
        let url = self.items_url(&format!(
            "?tag={}&format=keys&limit=1",
            urlencoding::encode(tag),
        ));
        let resp = self.send_retrying(|| self.auth(self.client.get(&url)))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("Web API returned {status} counting items tagged \"{tag}\"");
        }
        resp.headers()
            .get("Total-Results")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .context("No Total-Results header on the tagged-item count")
    }

    /// Delete up to [`TAG_DELETE_BATCH`] tags from the whole library at once.
    ///
    /// One request removes each tag from every item that carries it, which is
    /// why this is a library-level write guarded by the *library* version
    /// rather than any item's. A 412 therefore means anything at all changed in
    /// the library since `if_unmodified_version` was read, not just these tags;
    /// it comes back as a [`VersionConflict`] keyed `library` so the caller can
    /// re-read and retry rather than bail blindly.
    ///
    /// The tags are URL-encoded individually and joined with a literal `||`,
    /// which the server splits on before decoding. `Zotero-Write-Token` is
    /// deliberately absent: the docs say it is for unversioned writes only.
    pub fn delete_tags(&self, tags: &[String], if_unmodified_version: u64) -> Result<()> {
        if tags.is_empty() {
            return Ok(());
        }
        if tags.len() > TAG_DELETE_BATCH {
            bail!(
                "Refusing to delete {} tags in one request; the web API takes at most \
                 {TAG_DELETE_BATCH}.",
                tags.len(),
            );
        }
        let url = self.tags_url(&format!("?tag={}", encode_tag_list(tags)));
        let resp = self.send_retrying(|| {
            self.auth(self.client.delete(&url)).header(
                "If-Unmodified-Since-Version",
                if_unmodified_version.to_string(),
            )
        })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // 428 is the docs' second versioning status; they do not say which
        // condition picks which, so both mean "re-read the version and retry".
        if status == reqwest::StatusCode::PRECONDITION_FAILED
            || status == reqwest::StatusCode::PRECONDITION_REQUIRED
        {
            return Err(VersionConflict {
                key: "library".to_string(),
                read_version: if_unmodified_version,
            }
            .into());
        }
        let body = resp.text().unwrap_or_default();
        bail!(
            "Web API tag DELETE failed (status {status}): {}",
            body.trim()
        );
    }

    /// Fetch an item's current server-side JSON, distinguishing "not there"
    /// from "could not ask".
    ///
    /// `Ok(None)` is the 404 alone, which is what a caller waiting for a sync
    /// should retry; a revoked key, a server error or a transport failure stay
    /// `Err`, so such a caller gives up on them immediately.
    pub fn get_item_opt(&self, key: &str) -> Result<Option<Value>> {
        let url = self.items_url(&format!("/{key}"));
        let resp = self.send_retrying(|| self.auth(self.client.get(&url)))?;
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
        let body = data.to_string();
        let resp = self.send_retrying(|| {
            self.auth(self.client.patch(&url))
                .header(
                    "If-Unmodified-Since-Version",
                    if_unmodified_version.to_string(),
                )
                .header("Content-Type", "application/json")
                .body(body.clone())
        })?;
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

    /// The empty item of `item_type`, listing every field that type carries
    /// (`GET /items/new`, no library or key needed).
    pub fn item_template(&self, item_type: &str) -> Result<Value> {
        let url = format!(
            "{}/items/new?itemType={}",
            self.base,
            urlencoding::encode(item_type)
        );
        let resp = self.get_url(&url)?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Web API has no template for item type {item_type} ({status}): {}",
                body.trim()
            );
        }
        serde_json::from_str(&body).context("Failed to parse the item template")
    }

    /// Create one item and return its key.
    pub fn create_item(&self, item: &Value) -> Result<String> {
        let resp = self.create_items(&Value::Array(vec![item.clone()]))?;
        resp.pointer("/successful/0/key")
            .or_else(|| resp.pointer("/success/0"))
            .and_then(Value::as_str)
            .map(String::from)
            .context("No item key in the creation response")
    }

    /// Create items (POST). Returns the response JSON (`successful` map etc.).
    pub fn create_items(&self, items: &Value) -> Result<Value> {
        let url = self.items_url("");
        let body = items.to_string();
        let token = write_token();
        let resp = self.send_retrying(|| {
            self.auth(self.client.post(&url))
                .header("Content-Type", "application/json")
                .header("Zotero-Write-Token", &token)
                .body(body.clone())
        })?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            bail!(
                "api.zotero.org had already processed this item creation (write token reused), \
                 so it was not written a second time.\n  An earlier attempt went through: check \
                 the library for the new item before re-running."
            );
        }
        if !status.is_success() {
            bail!(
                "Web API item creation failed (status {status}): {}",
                body.trim()
            );
        }
        let v: Value = serde_json::from_str(&body).context("Failed to parse creation response")?;
        // Per-item failures come back inside a 200 response.
        if let Some(failed) = v.get("failed").and_then(|f| f.as_object()) {
            if !failed.is_empty() {
                bail!(
                    "Web API rejected item(s): {}",
                    serde_json::to_string(failed)?
                );
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
        let url = self.collections_url("");
        let body = body.to_string();
        let token = write_token();
        let resp = self.send_retrying(|| {
            self.auth(self.client.post(&url))
                .header("Content-Type", "application/json")
                .header("Zotero-Write-Token", &token)
                .body(body.clone())
        })?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            bail!(
                "api.zotero.org had already processed this collection creation (write token \
                 reused), so it was not written a second time.\n  An earlier attempt went \
                 through: check the collection tree before re-running."
            );
        }
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

    /// Fetch a collection's current server-side JSON (for its version).
    ///
    /// The version lives at the top level as well as under `data`; both track
    /// the same number, and the top-level one is read here because it is
    /// present even on responses that omit `data`.
    pub fn get_collection(&self, key: &str) -> Result<Value> {
        let url = self.collections_url(&format!("/{key}"));
        let resp = self.send_retrying(|| self.auth(self.client.get(&url)))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!(
                "Collection {key} not found on api.zotero.org.\n  \
                 If it exists locally, it may not have synced up yet -- sync Zotero and retry."
            );
        }
        if !resp.status().is_success() {
            bail!(
                "Web API returned {} fetching collection {key}",
                resp.status()
            );
        }
        resp.json().context("Failed to parse collection JSON")
    }

    /// DELETE one collection, using optimistic concurrency
    /// (`If-Unmodified-Since-Version`).
    ///
    /// **Permanent.** Zotero has no trash for collections, so unlike
    /// `patch_item`'s `deleted: true` there is nothing to restore afterwards.
    /// The delete cascades: the collection's descendants go with it. Items
    /// filed in any of them survive and become unfiled.
    ///
    /// `if_unmodified_version` must come from the caller's own read of this
    /// collection, as for `patch_item`: a 412 means a concurrent write landed
    /// in the window between that read and this DELETE, so it is fatal rather
    /// than something to retry. That window is all it covers. It says nothing
    /// about what the collection gained earlier, because Zotero versions
    /// collections individually and creating a subcollection bumps the child's
    /// version, not the parent's; `meta.numCollections` is the field that
    /// reports a subcollection the caller never saw.
    pub fn delete_collection(&self, key: &str, if_unmodified_version: u64) -> Result<()> {
        let url = self.collections_url(&format!("/{key}"));
        let resp = self.send_retrying(|| {
            self.auth(self.client.delete(&url)).header(
                "If-Unmodified-Since-Version",
                if_unmodified_version.to_string(),
            )
        })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(VersionConflict {
                key: key.to_string(),
                read_version: if_unmodified_version,
            }
            .into());
        }
        let body = resp.text().unwrap_or_default();
        bail!("Web API DELETE failed (status {status}): {}", body.trim());
    }

    /// Upload a file for an existing attachment item (Zotero 3-step flow:
    /// authorize -> upload -> register).
    pub fn upload_attachment_file(
        &self,
        attachment_key: &str,
        path: &std::path::Path,
    ) -> Result<()> {
        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
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
        let resp = self.send_retrying(|| {
            self.auth(self.client.post(&url))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("If-None-Match", "*")
                .body(form.clone())
        })?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Upload authorization failed (status {status}): {}",
                body.trim()
            );
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
        let register = format!("upload={upload_key}");
        let resp = self.send_retrying(|| {
            self.auth(self.client.post(&url))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("If-None-Match", "*")
                .body(register.clone())
        })?;
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

/// A `Backoff`/`Retry-After` header as whole seconds.
///
/// Only the delta-seconds form is read. `Retry-After` may also carry an HTTP
/// date; Zotero documents it as seconds, and an unparseable value falls back to
/// the caller's own schedule rather than to a guess.
fn header_secs(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
}

/// Build the `tag=` value of a multi-tag DELETE.
///
/// Each tag is percent-encoded, then joined with a bare `||`. The separator
/// stays literal on purpose: the server has to split the query before decoding
/// it, otherwise a tag containing a pipe would be indistinguishable from the
/// separator. `urlencoding::encode` encodes `|` as `%7C`, so a tag can never
/// forge one.
fn encode_tag_list(tags: &[String]) -> String {
    tags.iter()
        .map(|t| urlencoding::encode(t).into_owned())
        .collect::<Vec<_>>()
        .join("||")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh `Zotero-Write-Token`: 32 hex characters, unique per write.
///
/// The server remembers a token for twelve hours and answers a second request
/// carrying it with 412 instead of writing again, which is what makes resending
/// a creating POST safe. Uniqueness is all that matters, so an MD5 over the
/// clock, the process ID and a counter does the job without a random-number
/// dependency.
fn write_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    hex(&Md5::digest(seed.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::{created_collection_key, encode_tag_list, write_token, WebApiClient};
    use reqwest::blocking::Client;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A one-shot HTTP stub on localhost: it answers the given raw responses in
    /// order, one connection each, and records the head of every request it
    /// received. Returns the base URL and the recorded request heads.
    fn stub_server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for response in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                log.lock().unwrap().push(head);
                reader.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        (base, seen)
    }

    fn stub_client(base: String) -> WebApiClient {
        WebApiClient {
            client: Client::new(),
            api_key: "test-key".to_string(),
            user_id: 1,
            base,
        }
    }

    /// A raw HTTP/1.1 response that closes its connection, so every request
    /// the client sends arrives on a fresh one.
    fn response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// `Retry-After: 1` keeps the test at one second of sleeping.
    fn throttled() -> String {
        response("429 Too Many Requests", "Retry-After: 1\r\n", "")
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    #[test]
    fn a_throttled_item_creation_is_retried_with_the_same_write_token() {
        let ok = response(
            "200 OK",
            "Content-Type: application/json\r\n",
            r#"{"success":{"0":"ABCD1234"},"failed":{}}"#,
        );
        let (base, seen) = stub_server(vec![throttled(), ok]);
        let key = stub_client(base)
            .create_item(&json!({ "itemType": "note" }))
            .unwrap();
        assert_eq!(key, "ABCD1234");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one throttled POST, one retry");
        let tokens: Vec<_> = seen
            .iter()
            .map(|h| header(h, "Zotero-Write-Token").unwrap())
            .collect();
        assert_eq!(tokens[0].len(), 32);
        assert_eq!(tokens[0], tokens[1]);
    }

    #[test]
    fn a_reused_write_token_is_reported_as_an_earlier_success() {
        let refused = response("412 Precondition Failed", "", "");
        let (base, _) = stub_server(vec![throttled(), refused]);
        let err = stub_client(base)
            .create_items(&json!([{ "itemType": "note" }]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already processed"), "{err}");
    }

    #[test]
    fn write_tokens_are_unique() {
        assert_ne!(write_token(), write_token());
    }

    #[test]
    fn encode_tag_list_keeps_the_separator_literal_and_encodes_the_tags() {
        let tags = vec!["Computer Science".to_string(), "a|b".to_string()];
        assert_eq!(encode_tag_list(&tags), "Computer%20Science||a%7Cb");
    }

    #[test]
    fn encode_tag_list_handles_one_tag() {
        assert_eq!(encode_tag_list(&["cs.AI".to_string()]), "cs.AI");
    }

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
        assert!(
            err.contains("Parent collection ZZZZZZZZ not found"),
            "{err}"
        );
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
        assert!(
            err.contains("neither a created collection nor a failure"),
            "{err}"
        );
    }
}
