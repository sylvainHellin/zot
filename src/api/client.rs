use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::models::ZoteroItem;
use super::webapi::WebApiClient;

const DEFAULT_BASE_URL: &str = "http://localhost:23119/api/users/0";
const PAGE_SIZE: usize = 100;

/// The local API excludes all three non-regular types in one negation; the web
/// API rejects that form (400 "Invalid itemType") and allows a single negated
/// type, so the web path excludes attachments in the query and drops notes and
/// annotations client-side via `is_regular_item()`.
const LOCAL_REGULAR_FILTER: &str = "itemType=-attachment%20-note%20-annotation";
const WEB_REGULAR_FILTER: &str = "itemType=-attachment";

/// Maximum number of item keys per rendered-export request.
///
/// `limit` windows *rows*, and on `/items` a row is any item at all, child
/// attachments and notes included. Those children never render as a
/// bibliography entry, so a batch whose children overflow the window silently
/// loses real entries: 40 keys at `limit=25` rendered 15 entries, at `limit=50`
/// 21, and only at `limit=500` all 40. The rendered endpoints below therefore
/// use `/items/top`, which excludes children, so `Total-Results` is exactly the
/// number of matched top-level items and no window arithmetic is guesswork.
/// Probed against the live library up to 100 keys (100 in, 100 entries out);
/// 50 keeps the URL short with headroom.
pub const FORMAT_KEY_BATCH: usize = 50;

/// A bibliography format the Zotero local API renders server-side.
///
/// All three come back as `text/plain`, CSL JSON included, so they are read as
/// text rather than deserialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Bibtex,
    Ris,
    CslJson,
}

impl ExportFormat {
    /// The value the local API's `format=` query parameter expects.
    pub fn as_param(self) -> &'static str {
        match self {
            Self::Bibtex => "bibtex",
            Self::Ris => "ris",
            Self::CslJson => "csljson",
        }
    }
}

impl std::str::FromStr for ExportFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bibtex" | "bib" => Ok(Self::Bibtex),
            "ris" => Ok(Self::Ris),
            "csljson" | "csl-json" => Ok(Self::CslJson),
            other => {
                bail!("Unknown export format \"{other}\". Valid values: bibtex, ris, csljson.")
            }
        }
    }
}

/// Where reads go: the desktop app's local API, or api.zotero.org.
enum Backend {
    Local,
    Web(WebApiClient),
}

/// Which backend `ZOT_BACKEND` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    /// Local when the app answers, else web (the default).
    Auto,
    Local,
    Web,
}

impl BackendChoice {
    fn from_env() -> Result<Self> {
        match std::env::var("ZOT_BACKEND") {
            Err(_) => Ok(Self::Auto),
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" | "auto" => Ok(Self::Auto),
                "local" => Ok(Self::Local),
                "web" => Ok(Self::Web),
                other => bail!("ZOT_BACKEND=\"{other}\" is not one of auto, local, web."),
            },
        }
    }
}

/// Read client for the Zotero library.
///
/// Reads go to the desktop app's local API when it answers, because that is
/// fast and needs no network. When the app is closed or its local API is off,
/// the same paths are read from api.zotero.org with the key stored by
/// `zot config set-key`, so every read command works on a machine where the
/// app is not running. `ZOT_BACKEND=local|web` forces one side.
pub struct ZoteroClient {
    client: Client,
    base_url: String,
    backend: Backend,
}

impl ZoteroClient {
    /// Connect, announcing a web fallback on stderr.
    pub fn new() -> Result<Self> {
        Self::connect(true)
    }

    /// Connect without the stderr note, for callers that only probe the
    /// library in passing (the search freshness check).
    pub fn new_quiet() -> Result<Self> {
        Self::connect(false)
    }

    fn connect(announce: bool) -> Result<Self> {
        let choice = BackendChoice::from_env()?;
        if choice == BackendChoice::Web {
            return Self::web();
        }

        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none()) // We need to read 302 Location headers
            .build()
            .context("Failed to create HTTP client")?;
        let local_err = match local_health_check(&client, DEFAULT_BASE_URL) {
            Ok(()) => {
                return Ok(Self {
                    client,
                    base_url: DEFAULT_BASE_URL.to_string(),
                    backend: Backend::Local,
                });
            }
            Err(e) => e,
        };

        if choice == BackendChoice::Local || !WebApiClient::is_configured() {
            if choice == BackendChoice::Local {
                return Err(local_err);
            }
            bail!(
                "{local_err}\n  No web API key is configured either; run `zot config set-key` \
                 to read the library from api.zotero.org when the app is closed."
            );
        }
        let zot = Self::web()?;
        if announce {
            let reason = local_err.to_string();
            let reason = reason.lines().next().unwrap_or_default();
            eprintln!("Note: {reason} Reading from the Zotero web API instead.");
        }
        Ok(zot)
    }

    fn web() -> Result<Self> {
        let web = WebApiClient::from_config()?;
        Ok(Self {
            client: Client::new(),
            base_url: web.library_base(),
            backend: Backend::Web(web),
        })
    }

    /// Whether reads come from api.zotero.org rather than the desktop app.
    pub fn is_web(&self) -> bool {
        matches!(self.backend, Backend::Web(_))
    }

    /// GET a URL under `base_url` on whichever backend is active.
    fn send(&self, url: &str) -> Result<reqwest::blocking::Response> {
        match &self.backend {
            Backend::Local => Ok(self.client.get(url).send()?),
            Backend::Web(web) => web.get_url(url),
        }
    }

    /// The `itemType` filter that keeps regular items, per backend. The web
    /// form still lets notes and annotations through; callers retain
    /// `is_regular_item()` afterwards.
    fn regular_filter(&self) -> &'static str {
        match self.backend {
            Backend::Local => LOCAL_REGULAR_FILTER,
            Backend::Web(_) => WEB_REGULAR_FILTER,
        }
    }

    /// Largest `limit` the backend honours: the web API caps it at 100.
    fn max_limit(&self) -> usize {
        match self.backend {
            Backend::Local => 500,
            Backend::Web(_) => 100,
        }
    }

    /// Fetch {key: version} map for candidate items to index.
    ///
    /// Uses `/items/top` (top-level items only). The local API's
    /// `itemType=-attachment -note -annotation` negation is unreliable for
    /// `format=versions` (it leaks child notes/annotations into the result),
    /// which made the index churn over thousands of non-regular keys that never
    /// produce chunks. Top-level items are exactly the papers/books/etc. plus a
    /// handful of standalone notes/attachments, which the caller drops via
    /// `is_regular_item()` after fetching full data.
    pub fn fetch_item_versions(&self) -> Result<HashMap<String, u64>> {
        let url = format!("{}/items/top?format=versions", self.base_url);
        let resp = self.send(&url).context("Failed to fetch item versions")?;
        let status = resp.status();
        let body = resp.text().context("Failed to read versions response")?;
        if !status.is_success() {
            bail!(
                "Zotero returned {status} fetching item versions: {}",
                body.trim()
            );
        }
        let versions: HashMap<String, u64> =
            serde_json::from_str(&body).context("Failed to parse versions")?;
        Ok(versions)
    }

    /// Fetch a single item by key.
    pub fn fetch_item(&self, key: &str) -> Result<ZoteroItem> {
        let url = format!("{}/items/{}", self.base_url, key);
        let resp = self
            .send(&url)
            .context(format!("Failed to fetch item {key}"))?;
        if !resp.status().is_success() {
            bail!("Item {key} not found (status {})", resp.status());
        }
        let item: ZoteroItem = resp.json().context(format!("Failed to parse item {key}"))?;
        Ok(item)
    }

    /// Fetch multiple items by keys.
    ///
    /// The local API's `?itemKey=k1,k2,...` returns the requested items **plus**
    /// any child annotations of those items, and caps the response at `limit`.
    /// A small `limit` therefore silently truncates real items out of the batch.
    /// We use small key batches with generous headroom, keep only the keys we
    /// actually asked for, and fall back to single-item fetches for anything
    /// still missing -- so the result is exactly the requested items, no matter
    /// how much annotation noise the API mixes in.
    pub fn fetch_items(&self, keys: &[String]) -> Result<Vec<ZoteroItem>> {
        const KEY_BATCH: usize = 40;
        // Headroom so child annotations returned alongside the batch cannot push
        // a requested item past the limit (the web API returns only the
        // requested keys and caps `limit` at 100).
        let fetch_limit = self.max_limit();

        let mut all_items = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for chunk in keys.chunks(KEY_BATCH) {
            let want: std::collections::HashSet<&str> = chunk.iter().map(|s| s.as_str()).collect();
            let url = format!(
                "{}/items?itemKey={}&limit={}",
                self.base_url,
                chunk.join(","),
                fetch_limit,
            );
            let resp = self.send(&url).context("Failed to fetch items batch")?;
            let items: Vec<ZoteroItem> = resp.json().context("Failed to parse items batch")?;
            for item in items {
                if want.contains(item.key.as_str()) && seen.insert(item.key.clone()) {
                    all_items.push(item);
                }
            }
        }

        // Fallback for any requested key not returned by the batch endpoint
        // (e.g. truncated by an unusually annotation-heavy batch).
        for key in keys {
            if !seen.contains(key) {
                if let Ok(item) = self.fetch_item(key) {
                    seen.insert(key.clone());
                    all_items.push(item);
                }
            }
        }

        Ok(all_items)
    }

    /// Fetch a batch of items rendered as a bibliography.
    ///
    /// One HTTP request; the caller batches, because attribution depends on
    /// knowing which keys went into which response (entries carry a citation
    /// key that has no relation to the Zotero item key). At most
    /// [`FORMAT_KEY_BATCH`] keys per call.
    pub fn fetch_items_formatted(&self, keys: &[String], format: ExportFormat) -> Result<String> {
        if keys.is_empty() {
            return Ok(String::new());
        }
        if keys.len() > FORMAT_KEY_BATCH {
            bail!(
                "Rendered export takes at most {FORMAT_KEY_BATCH} keys per request, got {}",
                keys.len()
            );
        }
        let url = format!(
            "{}/items/top?itemKey={}&limit={}&format={}",
            self.base_url,
            keys.join(","),
            FORMAT_KEY_BATCH,
            format.as_param(),
        );
        Ok(self.fetch_rendered(&url)?.0)
    }

    /// Fetch a collection's top-level items rendered as a bibliography
    /// (paginated), returning one body per page for the caller to merge.
    ///
    /// Direct members only, matching every other `--collection` filter in the
    /// CLI; subcollections are not walked.
    pub fn fetch_collection_items_formatted(
        &self,
        collection_key: &str,
        format: ExportFormat,
    ) -> Result<Vec<String>> {
        let mut bodies = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/collections/{}/items/top?limit={}&start={}&format={}",
                self.base_url,
                collection_key,
                PAGE_SIZE,
                start,
                format.as_param(),
            );
            let (body, total) = self.fetch_rendered(&url)?;
            if !body.trim().is_empty() {
                bodies.push(body);
            }
            // Advance by the window, not by the entries returned: the
            // translator drops items it cannot render (a standalone
            // attachment, say), so a page can hold fewer entries than rows.
            start += PAGE_SIZE;
            if start >= total {
                break;
            }
        }
        Ok(bodies)
    }

    /// Fetch a collection's top-level items as JSON (paginated).
    ///
    /// The rendered formats carry no item keys, so the export path needs this
    /// alongside them to know which items it asked for and to name any the
    /// translator drops.
    pub fn fetch_collection_top_items(&self, collection_key: &str) -> Result<Vec<ZoteroItem>> {
        let mut all_items = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/collections/{}/items/top?limit={}&start={}",
                self.base_url, collection_key, PAGE_SIZE, start
            );
            let resp = self.send(&url).context(format!(
                "Failed to fetch items of collection {collection_key}"
            ))?;
            let total: usize = resp
                .headers()
                .get("Total-Results")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let items: Vec<ZoteroItem> = resp.json().context(format!(
                "Failed to parse items of collection {collection_key}"
            ))?;
            let count = items.len();
            all_items.extend(items);
            start += count;
            if count == 0 || start >= total {
                break;
            }
        }
        Ok(all_items)
    }

    /// GET a rendered export, returning the body and its `Total-Results`.
    fn fetch_rendered(&self, url: &str) -> Result<(String, usize)> {
        let resp = self.send(url).context("Failed to fetch rendered export")?;
        let status = resp.status();
        let total: usize = resp
            .headers()
            .get("Total-Results")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // `text()`, not `json()`: every rendered format is served as text/plain.
        let body = resp.text().context("Failed to read rendered export")?;
        if !status.is_success() {
            bail!(
                "Zotero returned {status} rendering the export: {}",
                body.trim()
            );
        }
        Ok((body, total))
    }

    /// Fetch all regular items (paginated).
    pub fn fetch_all_items(&self) -> Result<Vec<ZoteroItem>> {
        let mut all_items = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/items?{}&limit={}&start={}",
                self.base_url,
                self.regular_filter(),
                PAGE_SIZE,
                start
            );
            let resp = self.send(&url).context("Failed to fetch items")?;
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
        all_items.retain(ZoteroItem::is_regular_item);
        Ok(all_items)
    }

    /// Fetch all top-level items (paginated), including standalone
    /// attachments and notes, which `fetch_all_items` filters out. `zot
    /// collections` and `zot unfiled` need the unfiltered set: an item filed
    /// in a collection is top-level, whatever its type.
    #[allow(dead_code)] // consumed by `zot collections` / `zot unfiled` (plan 1.2, 1.5)
    pub fn fetch_top_items(&self) -> Result<Vec<ZoteroItem>> {
        let mut all_items = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/items/top?limit={}&start={}",
                self.base_url, PAGE_SIZE, start
            );
            let resp = self.send(&url).context("Failed to fetch top-level items")?;
            let total: usize = resp
                .headers()
                .get("Total-Results")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let items: Vec<ZoteroItem> = resp.json().context("Failed to parse top-level items")?;
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
        // Explicit limit: the web API defaults to 25 rows, and an item with
        // many notes or annotations would lose attachments past them.
        let url = format!(
            "{}/items/{}/children?limit={}",
            self.base_url,
            item_key,
            self.max_limit()
        );
        let resp = self
            .send(&url)
            .context(format!("Failed to fetch children of {item_key}"))?;
        let children: Vec<ZoteroItem> = resp.json().context("Failed to parse children")?;
        Ok(children)
    }

    /// Get a local file path for an attachment.
    ///
    /// Local backend: follow the local API's 302 to the `file://` URL. Web
    /// backend: see [`Self::web_attachment_path`].
    pub fn get_attachment_path(&self, attachment_key: &str) -> Result<Option<String>> {
        if let Backend::Web(web) = &self.backend {
            let item = self.fetch_item(attachment_key)?;
            return web_attachment_path(web, &item);
        }
        let url = format!("{}/items/{}/file", self.base_url, attachment_key);
        let resp = self
            .send(&url)
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

        let filter = self.regular_filter();
        let mut url = format!(
            "{}/items?q={}&qmode={}&{}&limit={}",
            self.base_url,
            urlencoding::encode(query),
            qmode,
            filter,
            params.limit.unwrap_or(25).min(self.max_limit()),
        );

        if let Some(tag) = &params.tag {
            url.push_str(&format!("&tag={}", urlencoding::encode(tag)));
        }
        if let Some(item_type) = &params.item_type {
            // Override the default exclusion
            url = url.replace(
                filter,
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

        let resp = self.send(&url).context("Search failed")?;
        let mut items: Vec<ZoteroItem> = resp.json().context("Failed to parse search results")?;
        if params.item_type.is_none() {
            items.retain(ZoteroItem::is_regular_item);
        }
        Ok(items)
    }

    /// Fetch all collections in the library (paginated).
    pub fn fetch_collections(&self) -> Result<Vec<super::models::ZoteroCollection>> {
        let mut all = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/collections?limit={}&start={}",
                self.base_url, PAGE_SIZE, start
            );
            let resp = self.send(&url).context("Failed to fetch collections")?;
            let total: usize = resp
                .headers()
                .get("Total-Results")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let cols: Vec<super::models::ZoteroCollection> =
                resp.json().context("Failed to parse collections")?;
            let count = cols.len();
            all.extend(cols);
            start += count;
            if count == 0 || start >= total {
                break;
            }
        }
        Ok(all)
    }

    /// Fetch every item carrying `tag`, children included.
    ///
    /// `/items` rather than `/items/top`, because a tag can sit on a note or an
    /// attachment and a rename that skipped those would leave the old tag alive
    /// on them.
    pub fn fetch_items_by_tag(&self, tag: &str) -> Result<Vec<ZoteroItem>> {
        let mut all_items = Vec::new();
        let mut start = 0;
        loop {
            let url = format!(
                "{}/items?tag={}&limit={}&start={}",
                self.base_url,
                urlencoding::encode(tag),
                PAGE_SIZE,
                start,
            );
            let resp = self.send(&url).context("Failed to fetch items by tag")?;
            let status = resp.status();
            if !status.is_success() {
                bail!("Zotero returned {status} fetching items tagged \"{tag}\"");
            }
            let total: usize = resp
                .headers()
                .get("Total-Results")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let items: Vec<ZoteroItem> = resp.json().context("Failed to parse items by tag")?;
            let count = items.len();
            all_items.extend(items);
            start += count;
            if count == 0 || start >= total {
                break;
            }
        }
        Ok(all_items)
    }

    /// Fetch all tags from the library.
    pub fn fetch_tags(&self) -> Result<Vec<TagInfo>> {
        let mut all_tags = Vec::new();
        let mut start = 0;
        loop {
            let url = format!("{}/tags?limit={}&start={}", self.base_url, PAGE_SIZE, start);
            let resp = self.send(&url).context("Failed to fetch tags")?;
            let tags: Vec<TagInfo> = resp.json().context("Failed to parse tags")?;
            let count = tags.len();
            all_tags.extend(tags);
            start += count;
            // Stop on a short page, not on `Total-Results`: the web API
            // counts distinct tag names there while it pages over one row per
            // name and type, so a name used both by hand and automatically
            // makes the total too small (130 against 235 rows, probed
            // 2026-10-22).
            if count < PAGE_SIZE {
                break;
            }
        }
        Ok(all_tags)
    }
}

/// Probe the desktop app's local API at `base_url`.
fn local_health_check(client: &Client, base_url: &str) -> Result<()> {
    // The root path returns 404 ("No endpoint found") which is fine -- it means Zotero is running.
    // A connection error means Zotero is not running.
    let root_url = base_url.replace("/api/users/0", "/");
    if client.get(&root_url).send().is_err() {
        bail!("Could not reach the Zotero desktop app (expected at {root_url}).");
    }

    // Zotero is reachable, but the local API (the `/api/...` endpoints we rely on) is
    // disabled by default. Probe it now so every command fails early with a clear,
    // actionable message instead of a cryptic JSON parse error later.
    let probe_url = format!("{base_url}/items?format=versions&limit=1");
    if let Ok(resp) = client.get(&probe_url).send() {
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

/// The Zotero data directory holding `storage/`: `ZOTERO_DATA_DIR`, else
/// `~/Zotero`, the desktop app's default on every platform.
fn zotero_data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ZOTERO_DATA_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join("Zotero"))
}

/// Where downloaded attachment files are kept: `<cache>/zot/attachments/`.
fn attachment_cache_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "zot").map(|d| d.cache_dir().join("attachments"))
}

/// Cache path of one attachment file, laid out as Zotero's own storage
/// (`<KEY>/<filename>`).
pub fn attachment_cache_path(key: &str, filename: &str) -> Option<PathBuf> {
    attachment_cache_dir().map(|d| d.join(key).join(filename))
}

/// Hex MD5 of a file, or `None` when it cannot be read.
fn file_md5(path: &Path) -> Option<String> {
    use md5::{Digest, Md5};
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Md5::digest(&bytes)))
}

/// Whether `path` exists and, when Zotero recorded an MD5, matches it. A
/// stale copy (the attachment's file was replaced since) fails the check.
fn is_current_copy(path: &Path, md5: Option<&str>) -> bool {
    if !path.is_file() {
        return false;
    }
    match md5 {
        Some(want) => file_md5(path).is_some_and(|got| got.eq_ignore_ascii_case(want)),
        None => true,
    }
}

/// Resolve an attachment to a local file path while reading from the web API.
///
/// - `linked_file`: the recorded path, when it exists on this machine.
/// - `imported_file` / `imported_url`: the copy under the Zotero data
///   directory when one exists and matches the server's MD5 (a synced but
///   closed desktop install), else a cached download, else a fresh download
///   into the cache. `None` when the server holds no file.
/// - `linked_url` and anything else: `None`, there is no file.
fn web_attachment_path(web: &WebApiClient, item: &ZoteroItem) -> Result<Option<String>> {
    let field = |name: &str| {
        item.data
            .extra
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let link_mode = field("linkMode").unwrap_or_default();
    match link_mode {
        "linked_file" => {
            let Some(path) = field("path") else {
                return Ok(None);
            };
            Ok(Path::new(path).is_file().then(|| path.to_string()))
        }
        "imported_file" | "imported_url" => {
            let Some(filename) = field("filename") else {
                return Ok(None);
            };
            // A filename is a single path component; refuse anything that
            // could escape the storage or cache directory.
            if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
                bail!("Attachment {} has an unsafe filename: {filename}", item.key);
            }
            let md5 = field("md5");
            if let Some(dir) = zotero_data_dir() {
                let local = dir.join("storage").join(&item.key).join(filename);
                if is_current_copy(&local, md5) {
                    return Ok(Some(local.to_string_lossy().into_owned()));
                }
            }
            let cache = attachment_cache_path(&item.key, filename)
                .context("Could not determine the cache directory")?;
            if is_current_copy(&cache, md5) {
                return Ok(Some(cache.to_string_lossy().into_owned()));
            }
            if !web.download_attachment(&item.key, &cache)? {
                return Ok(None);
            }
            if !is_current_copy(&cache, md5) {
                bail!(
                    "Downloaded file of {} does not match the MD5 Zotero recorded",
                    item.key
                );
            }
            Ok(Some(cache.to_string_lossy().into_owned()))
        }
        _ => Ok(None),
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
    #[serde(default)]
    pub meta: TagMeta,
}

impl TagInfo {
    /// Whether Zotero created this tag itself from publisher or arXiv subject
    /// headings (`meta.type == 1`) rather than a person typing it.
    pub fn is_automatic(&self) -> bool {
        self.meta.tag_type == Some(1)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagMeta {
    #[serde(default, rename = "numItems")]
    pub num_items: u32,
    /// 1 = automatic, 0 or absent = manual. On `/tags` the type lives here,
    /// under `meta`, not at the top level as it does inside an item's
    /// `data.tags` (verified against the local API, Zotero 9.0.4).
    #[serde(default, rename = "type")]
    pub tag_type: Option<u32>,
}

use serde::{Deserialize, Serialize};
