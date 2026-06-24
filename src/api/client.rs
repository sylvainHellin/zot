use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use std::collections::HashMap;

use super::models::ZoteroItem;

const DEFAULT_BASE_URL: &str = "http://localhost:23119/api/users/0";
const PAGE_SIZE: usize = 100;

pub struct ZoteroClient {
    client: Client,
    base_url: String,
}

impl ZoteroClient {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none()) // We need to read 302 Location headers
            .build()
            .context("Failed to create HTTP client")?;

        let zot = Self {
            client,
            base_url: DEFAULT_BASE_URL.to_string(),
        };

        // Health check
        zot.health_check()?;

        Ok(zot)
    }

    fn health_check(&self) -> Result<()> {
        // The root path returns 404 ("No endpoint found") which is fine -- it means Zotero is running.
        // A connection error means Zotero is not running.
        let root_url = self.base_url.replace("/api/users/0", "/");
        if self.client.get(&root_url).send().is_err() {
            bail!("Could not reach Zotero. Is it running?\n  (expected at {root_url})");
        }

        // Zotero is reachable, but the local API (the `/api/...` endpoints we rely on) is
        // disabled by default. Probe it now so every command fails early with a clear,
        // actionable message instead of a cryptic JSON parse error later.
        let probe_url = format!("{}/items?format=versions&limit=1", self.base_url);
        if let Ok(resp) = self.client.get(&probe_url).send() {
            if resp.status() == reqwest::StatusCode::FORBIDDEN {
                let body = resp.text().unwrap_or_default();
                if body.contains("Local API is not enabled") {
                    bail!(
                        "Zotero is running, but its local API is disabled.\n  \
                         Enable it in Zotero: Settings -> Advanced -> Config Editor,\n  \
                         set `httpServer.localAPI.enabled` to true (then restart Zotero)."
                    );
                }
                bail!("Zotero local API returned 403 Forbidden: {}", body.trim());
            }
        }
        Ok(())
    }

    /// Fetch {key: version} map for all non-attachment/note/annotation items.
    pub fn fetch_item_versions(&self) -> Result<HashMap<String, u64>> {
        let url = format!(
            "{}/items?format=versions&itemType=-attachment%20-note%20-annotation",
            self.base_url
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .context("Failed to fetch item versions")?;
        let status = resp.status();
        let body = resp.text().context("Failed to read versions response")?;
        if !status.is_success() {
            bail!("Zotero returned {status} fetching item versions: {}", body.trim());
        }
        let versions: HashMap<String, u64> =
            serde_json::from_str(&body).context("Failed to parse versions")?;
        Ok(versions)
    }

    /// Fetch a single item by key.
    pub fn fetch_item(&self, key: &str) -> Result<ZoteroItem> {
        let url = format!("{}/items/{}", self.base_url, key);
        let resp = self
            .client
            .get(&url)
            .send()
            .context(format!("Failed to fetch item {key}"))?;
        if !resp.status().is_success() {
            bail!("Item {key} not found (status {})", resp.status());
        }
        let item: ZoteroItem = resp.json().context(format!("Failed to parse item {key}"))?;
        Ok(item)
    }

    /// Fetch multiple items by keys (batched).
    pub fn fetch_items(&self, keys: &[String]) -> Result<Vec<ZoteroItem>> {
        let mut all_items = Vec::new();
        for chunk in keys.chunks(PAGE_SIZE) {
            let keys_str = chunk.join(",");
            let url = format!(
                "{}/items?itemKey={}&limit={}",
                self.base_url,
                keys_str,
                chunk.len()
            );
            let resp = self
                .client
                .get(&url)
                .send()
                .context("Failed to fetch items batch")?;
            let items: Vec<ZoteroItem> = resp.json().context("Failed to parse items batch")?;
            all_items.extend(items);
        }
        Ok(all_items)
    }

    /// Fetch all regular items (paginated).
    pub fn fetch_all_items(&self) -> Result<Vec<ZoteroItem>> {
        let mut all_items = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/items?itemType=-attachment%20-note%20-annotation&limit={}&start={}",
                self.base_url, PAGE_SIZE, start
            );
            let resp = self
                .client
                .get(&url)
                .send()
                .context("Failed to fetch items")?;
            let total: usize = resp
                .headers()
                .get("Total-Results")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let items: Vec<ZoteroItem> = resp.json().context("Failed to parse items")?;
            let count = items.len();
            all_items.extend(items);
            start += count;
            if count == 0 || start >= total {
                break;
            }
        }
        Ok(all_items)
    }

    /// Fetch children of an item (to find attachments).
    pub fn fetch_children(&self, item_key: &str) -> Result<Vec<ZoteroItem>> {
        let url = format!("{}/items/{}/children", self.base_url, item_key);
        let resp = self
            .client
            .get(&url)
            .send()
            .context(format!("Failed to fetch children of {item_key}"))?;
        let children: Vec<ZoteroItem> = resp.json().context("Failed to parse children")?;
        Ok(children)
    }

    /// Get the local file path for an attachment by following the 302 redirect.
    pub fn get_attachment_path(&self, attachment_key: &str) -> Result<Option<String>> {
        let url = format!("{}/items/{}/file", self.base_url, attachment_key);
        let resp = self
            .client
            .get(&url)
            .send()
            .context(format!("Failed to get file for {attachment_key}"))?;

        if resp.status() == reqwest::StatusCode::FOUND {
            if let Some(location) = resp.headers().get("Location") {
                let loc = location.to_str().unwrap_or("");
                if let Some(path) = loc.strip_prefix("file://") {
                    let decoded = urlencoding::decode(path)
                        .unwrap_or_else(|_| path.into())
                        .into_owned();
                    return Ok(Some(decoded));
                }
            }
        }
        Ok(None)
    }

    /// Search items via Zotero's built-in search (keyword/quicksearch).
    pub fn search_items(&self, query: &str, params: &SearchParams) -> Result<Vec<ZoteroItem>> {
        let qmode = if params.everything {
            "everything"
        } else {
            "titleCreatorYear"
        };

        let mut url = format!(
            "{}/items?q={}&qmode={}&itemType=-attachment%20-note%20-annotation&limit={}",
            self.base_url,
            urlencoding::encode(query),
            qmode,
            params.limit.unwrap_or(25),
        );

        if let Some(tag) = &params.tag {
            url.push_str(&format!("&tag={}", urlencoding::encode(tag)));
        }
        if let Some(item_type) = &params.item_type {
            // Override the default exclusion
            url = url.replace(
                "itemType=-attachment%20-note%20-annotation",
                &format!("itemType={}", urlencoding::encode(item_type)),
            );
        }
        if let Some(collection) = &params.collection {
            url = url.replace(
                &format!("{}/items?", self.base_url),
                &format!("{}/collections/{}/items?", self.base_url, collection),
            );
        }
        if let Some(sort) = &params.sort {
            url.push_str(&format!("&sort={}", urlencoding::encode(sort)));
            if params.desc {
                url.push_str("&direction=desc");
            }
        }

        let resp = self.client.get(&url).send().context("Search failed")?;
        let items: Vec<ZoteroItem> = resp.json().context("Failed to parse search results")?;
        Ok(items)
    }

    /// Fetch all tags from the library.
    pub fn fetch_tags(&self) -> Result<Vec<TagInfo>> {
        let mut all_tags = Vec::new();
        let mut start = 0;
        loop {
            let url = format!("{}/tags?limit={}&start={}", self.base_url, PAGE_SIZE, start);
            let resp = self
                .client
                .get(&url)
                .send()
                .context("Failed to fetch tags")?;
            let total: usize = resp
                .headers()
                .get("Total-Results")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let tags: Vec<TagInfo> = resp.json().context("Failed to parse tags")?;
            let count = tags.len();
            all_tags.extend(tags);
            start += count;
            if count == 0 || start >= total {
                break;
            }
        }
        Ok(all_tags)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub tag: Option<String>,
    // Populated from the `--creator` flag. The Zotero `titleCreatorYear`/`everything`
    // qmode already matches on creator via the free-text query, so this is not sent
    // as a separate filter param yet -- kept for an explicit creator filter later.
    #[allow(dead_code)]
    pub creator: Option<String>,
    pub item_type: Option<String>,
    pub collection: Option<String>,
    pub sort: Option<String>,
    pub desc: bool,
    pub everything: bool,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagInfo {
    pub tag: String,
    #[serde(default, rename = "type")]
    pub tag_type: Option<u32>,
    #[serde(default)]
    pub meta: TagMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagMeta {
    #[serde(default, rename = "numItems")]
    pub num_items: u32,
}

use serde::{Deserialize, Serialize};
