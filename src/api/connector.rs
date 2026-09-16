//! Client for Zotero's *connector* HTTP endpoints (`/connector/...`).
//!
//! The local read API (`/api/users/0`) is read-only; the connector endpoints
//! (used by the Zotero browser connector) are the only local write path.
//! Verified against Zotero 7:
//!   - `POST /connector/import?session=ID` imports BibTeX/RIS/CSL-JSON and
//!     returns the created items.
//!   - `POST /connector/saveStandaloneAttachment` stores a PDF and triggers
//!     Zotero's metadata recognizer (which creates the parent item).
//!   - `POST /connector/updateSession` retargets a save session (collection,
//!     tags) after the fact.

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;

const CONNECTOR_BASE: &str = "http://localhost:23119/connector";

pub struct ConnectorClient {
    client: Client,
    base_url: String,
}

/// A save target as reported by `getSelectedCollection` (`targets` array).
#[derive(Debug, Clone, Deserialize)]
pub struct SaveTarget {
    pub id: String,
    pub name: String,
    /// Nesting depth in the collection tree (0 = library root).
    #[serde(default)]
    pub level: u32,
}

impl ConnectorClient {
    pub fn new() -> Result<Self> {
        // Generous timeout: getRecognizedItem blocks until PDF metadata
        // recognition finishes (network calls to Crossref etc.).
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .context("Failed to create HTTP client")?;
        let conn = Self {
            client,
            base_url: CONNECTOR_BASE.to_string(),
        };
        conn.ping()?;
        Ok(conn)
    }

    fn ping(&self) -> Result<()> {
        let url = format!("{}/ping", self.base_url);
        match self.client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => Ok(()),
            _ => bail!("Could not reach Zotero connector server. Is Zotero running?"),
        }
    }

    /// Generate a connector-style random session ID.
    pub fn new_session_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut x = (nanos as u64) ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..8)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                CHARS[(x % CHARS.len() as u64) as usize] as char
            })
            .collect()
    }

    /// Import bibliography data (BibTeX/RIS/CSL-JSON, auto-detected by Zotero's
    /// import translators). Returns the raw JSON response (created items).
    pub fn import(&self, body: &str, session_id: &str) -> Result<Value> {
        let url = format!("{}/import?session={}", self.base_url, session_id);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(body.to_string())
            .send()
            .context("Failed to POST to /connector/import")?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Zotero import failed (status {status}): {}",
                text.trim()
            );
        }
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse import response: {}", text.trim()))?;
        Ok(value)
    }

    /// Save a standalone PDF attachment. Zotero stores the file and (with
    /// `autoRecognizeFiles` enabled, the default) retrieves metadata to create
    /// a parent item. Returns the raw JSON response.
    pub fn save_standalone_attachment(
        &self,
        pdf_bytes: Vec<u8>,
        filename: &str,
        session_id: &str,
    ) -> Result<Value> {
        let url = format!("{}/saveStandaloneAttachment", self.base_url);
        let metadata = serde_json::json!({
            "sessionID": session_id,
            "url": format!("file://{}", filename),
            "title": filename,
        });
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/pdf")
            .header("X-Metadata", metadata.to_string())
            .body(pdf_bytes)
            .send()
            .context("Failed to POST to /connector/saveStandaloneAttachment")?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Zotero PDF save failed (status {status}): {}",
                text.trim()
            );
        }
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(value)
    }

    /// Retarget a save session (move saved items to a collection / set tags).
    /// `target` is a tree-view ID as reported by `list_targets` (e.g. "L1" for
    /// My Library, "C123" for a collection). `tags` is comma-separated.
    pub fn update_session(
        &self,
        session_id: &str,
        target: Option<&str>,
        tags: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/updateSession", self.base_url);
        let mut payload = serde_json::json!({ "sessionID": session_id });
        if let Some(t) = target {
            payload["target"] = Value::String(t.to_string());
        }
        if let Some(t) = tags {
            payload["tags"] = Value::String(t.to_string());
        }
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
            .context("Failed to POST to /connector/updateSession")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            bail!(
                "Zotero updateSession failed (status {status}): {}",
                text.trim()
            );
        }
        Ok(())
    }

    /// Wait for PDF metadata recognition in a `saveStandaloneAttachment`
    /// session. Zotero blocks this request on its internal recognition
    /// promise, so a response means recognition settled. Returns
    /// `Some({title, itemType})` for the recognized parent item, `None` if
    /// recognition found nothing (HTTP 204).
    pub fn get_recognized_item(&self, session_id: &str) -> Result<Option<Value>> {
        let url = format!("{}/getRecognizedItem", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "sessionID": session_id }).to_string())
            .send()
            .context("Failed to POST to /connector/getRecognizedItem")?;
        let status = resp.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            bail!(
                "Zotero getRecognizedItem failed (status {status}): {}",
                text.trim()
            );
        }
        let v: Value = resp.json().unwrap_or(Value::Null);
        Ok(Some(v))
    }

    /// List available save targets (library root + collections) as shown in
    /// the connector's target selector.
    pub fn list_targets(&self) -> Result<Vec<SaveTarget>> {
        let url = format!("{}/getSelectedCollection", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-Zotero-Connector-API-Version", "2")
            .body("{}")
            .send()
            .context("Failed to POST to /connector/getSelectedCollection")?;
        let v: Value = resp
            .json()
            .context("Failed to parse getSelectedCollection response")?;
        let targets = v
            .get("targets")
            .and_then(|t| serde_json::from_value::<Vec<SaveTarget>>(t.clone()).ok())
            .unwrap_or_default();
        Ok(targets)
    }
}
