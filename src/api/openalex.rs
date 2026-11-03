//! Client for the OpenAlex works API (api.openalex.org), behind `zot discover`.
//!
//! Three questions go to OpenAlex: a keyword or semantic search, the works
//! citing a given work (`filter=cites:W...`), and the works a given work cites
//! (`filter=cited_by:W...`). A seed work is looked up once by its DOI, PMID,
//! PMCID or OpenAlex ID; singleton lookups are free, a list costs $0.0001 and
//! a search $0.001 against the key's $1 daily budget.
//!
//! The key is read from `OPENALEX_API_KEY`, else `~/.config/openalex/key`, and
//! sent as a bearer token so it never appears in a URL or an error message.
//! Without one the API still answers, on a tenth of the budget.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://api.openalex.org";

/// The fields `zot discover` shows or matches on; `select` keeps the response
/// to these instead of the full work record.
const SELECT: &str = "id,doi,ids,display_name,publication_year,type,authorships,\
                      primary_location,cited_by_count,abstract_inverted_index,relevance_score";

/// OpenAlex caps a page at 100 results and a semantic search at 50 in total.
pub const PAGE_MAX: usize = 100;
pub const SEMANTIC_MAX: usize = 50;

/// Retries on a 429 or a 5xx before the error is reported.
const MAX_RETRIES: u32 = 3;

/// What to ask OpenAlex for.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// `search=`: title, abstract and full text, by keyword.
    Keyword(String),
    /// `search.semantic=`: nearest works by embedding, at most 50.
    Semantic(String),
    /// No search, only the filters (a citation list).
    None,
}

/// A citation relation to a seed work, as an OpenAlex filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Relation {
    /// Works that cite the seed.
    CitedBy,
    /// Works the seed cites.
    References,
}

impl Relation {
    fn filter(self, work_id: &str) -> String {
        match self {
            Relation::CitedBy => format!("cites:{work_id}"),
            Relation::References => format!("cited_by:{work_id}"),
        }
    }
}

/// Result order. `Default` leaves it to OpenAlex: relevance for a search,
/// age-adjusted citations for a plain filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sort {
    Default,
    Cited,
    Newest,
}

impl Sort {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "relevance" | "default" => Ok(Sort::Default),
            "cited" | "citations" => Ok(Sort::Cited),
            "newest" | "year" | "date" => Ok(Sort::Newest),
            other => bail!("Unknown --sort {other:?}: use relevance, cited or newest."),
        }
    }

    fn param(self) -> Option<&'static str> {
        match self {
            Sort::Default => None,
            Sort::Cited => Some("cited_by_count:desc"),
            Sort::Newest => Some("publication_date:desc"),
        }
    }
}

/// One request's worth of parameters, independent of paging.
#[derive(Debug, Clone)]
pub struct WorksRequest {
    pub query: Query,
    /// Seed work's OpenAlex ID (`W...`) with the relation to it.
    pub relation: Option<(Relation, String)>,
    /// `publication_year` filter value in OpenAlex syntax (`2021`, `2018-2024`,
    /// `>2019`).
    pub year: Option<String>,
    pub sort: Sort,
    pub limit: usize,
}

impl WorksRequest {
    /// The `filter` parameter, or `None` when nothing filters.
    fn filter(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some((relation, id)) = &self.relation {
            parts.push(relation.filter(id));
        }
        if let Some(year) = &self.year {
            parts.push(format!("publication_year:{year}"));
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }

    /// Query-string pairs for one page, `select` included.
    fn params(&self, page: usize, per_page: usize) -> Vec<(&'static str, String)> {
        let mut params = vec![("select", SELECT.to_string())];
        match &self.query {
            Query::Keyword(q) => params.push(("search", q.clone())),
            Query::Semantic(q) => params.push(("search.semantic", q.clone())),
            Query::None => {}
        }
        if let Some(filter) = self.filter() {
            params.push(("filter", filter));
        }
        if let Some(sort) = self.sort.param() {
            params.push(("sort", sort.to_string()));
        }
        params.push(("per_page", per_page.to_string()));
        params.push(("page", page.to_string()));
        params
    }
}

/// A page of results as OpenAlex returns it.
#[derive(Debug, Deserialize)]
struct WorksPage {
    meta: PageMeta,
    #[serde(default)]
    results: Vec<Work>,
}

#[derive(Debug, Deserialize)]
struct PageMeta {
    #[serde(default)]
    count: u64,
    #[serde(default)]
    cost_usd: Option<f64>,
}

/// The works found and what they cost.
#[derive(Debug)]
pub struct Works {
    pub works: Vec<Work>,
    /// Matching works in OpenAlex, of which `works` is the first page(s).
    pub total: u64,
    pub cost_usd: f64,
}

/// One work, with only the fields in [`SELECT`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Work {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub doi: Option<String>,
    #[serde(default)]
    pub ids: WorkIds,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub publication_year: Option<i64>,
    #[serde(default, rename = "type")]
    pub work_type: Option<String>,
    #[serde(default)]
    pub authorships: Vec<Authorship>,
    #[serde(default)]
    pub primary_location: Option<Location>,
    #[serde(default)]
    pub cited_by_count: u64,
    #[serde(default)]
    pub abstract_inverted_index: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub relevance_score: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WorkIds {
    #[serde(default)]
    pub pmid: Option<String>,
    #[serde(default)]
    pub pmcid: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Authorship {
    #[serde(default)]
    pub author: AuthorRef,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthorRef {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Location {
    #[serde(default)]
    pub source: Option<SourceRef>,
    #[serde(default)]
    pub landing_page_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SourceRef {
    #[serde(default)]
    pub display_name: Option<String>,
}

impl Work {
    /// `W...`, without the `https://openalex.org/` prefix.
    pub fn short_id(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }

    /// The DOI without its `https://doi.org/` prefix, lowercased.
    pub fn bare_doi(&self) -> Option<String> {
        self.doi
            .as_deref()
            .map(strip_doi_prefix)
            .filter(|d| !d.is_empty())
    }

    /// The arXiv ID when the DOI is arXiv's own (`10.48550/arXiv.2401.12345`).
    pub fn arxiv_id(&self) -> Option<String> {
        self.bare_doi()?
            .strip_prefix("10.48550/arxiv.")
            .map(String::from)
    }

    pub fn title(&self) -> &str {
        self.display_name.as_deref().unwrap_or("")
    }

    pub fn authors(&self) -> Vec<String> {
        self.authorships
            .iter()
            .filter_map(|a| a.author.display_name.clone())
            .collect()
    }

    pub fn venue(&self) -> Option<String> {
        self.primary_location
            .as_ref()?
            .source
            .as_ref()?
            .display_name
            .clone()
    }

    pub fn landing_page(&self) -> Option<String> {
        self.primary_location.as_ref()?.landing_page_url.clone()
    }

    /// The abstract in reading order, rebuilt from OpenAlex's inverted index
    /// (word to positions), or `None` when the work has none.
    pub fn abstract_text(&self) -> Option<String> {
        let index = self.abstract_inverted_index.as_ref()?;
        let mut words: Vec<(u64, &str)> = Vec::new();
        for (word, positions) in index {
            for pos in positions.as_array().into_iter().flatten() {
                if let Some(p) = pos.as_u64() {
                    words.push((p, word.as_str()));
                }
            }
        }
        if words.is_empty() {
            return None;
        }
        words.sort_by_key(|(p, _)| *p);
        Some(
            words
                .into_iter()
                .map(|(_, w)| w)
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

/// Lowercased DOI with any `https://doi.org/`, `doi.org/` or `doi:` prefix cut.
pub fn strip_doi_prefix(doi: &str) -> String {
    let lower = doi.trim().to_lowercase();
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi.org/",
        "doi:",
    ] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    lower
}

/// How a seed work is named for a singleton lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkRef {
    OpenAlex(String),
    Doi(String),
    Pmid(String),
    Pmcid(String),
}

impl WorkRef {
    /// The path segment after `/works/`.
    fn path(&self) -> String {
        match self {
            WorkRef::OpenAlex(id) => id.clone(),
            WorkRef::Doi(doi) => format!("doi:{doi}"),
            WorkRef::Pmid(pmid) => format!("pmid:{pmid}"),
            WorkRef::Pmcid(pmcid) => format!("pmcid:{pmcid}"),
        }
    }

    pub fn display(&self) -> String {
        match self {
            WorkRef::OpenAlex(id) => format!("OpenAlex {id}"),
            WorkRef::Doi(doi) => format!("DOI {doi}"),
            WorkRef::Pmid(pmid) => format!("PMID {pmid}"),
            WorkRef::Pmcid(pmcid) => format!("PMCID {pmcid}"),
        }
    }
}

/// `W` plus digits, bare or as an openalex.org URL.
pub fn parse_openalex_id(s: &str) -> Option<String> {
    let s = s.trim();
    let tail = s
        .strip_prefix("https://openalex.org/")
        .or_else(|| s.strip_prefix("http://openalex.org/"))
        .or_else(|| s.strip_prefix("openalex.org/"))
        .or_else(|| s.strip_prefix("https://api.openalex.org/works/"))
        .unwrap_or(s);
    let upper = tail.to_ascii_uppercase();
    let digits = upper.strip_prefix('W')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(upper)
}

pub struct OpenAlexClient {
    client: Client,
    key: Option<String>,
    base: String,
}

impl OpenAlexClient {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(format!("zot/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            key: read_key(),
            base: API_BASE.to_string(),
        })
    }

    #[cfg(test)]
    fn with_base(base: &str, key: Option<&str>) -> Self {
        Self {
            client: Client::new(),
            key: key.map(String::from),
            base: base.to_string(),
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    fn auth(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    /// Send, retrying a 429 or 5xx up to [`MAX_RETRIES`] times. The wait is
    /// `Retry-After` when the server names one, else 1, 2, 4 s.
    fn send(&self, build: impl Fn() -> RequestBuilder) -> Result<Response> {
        let mut attempt = 0;
        loop {
            let resp = self
                .auth(build())
                .send()
                .context("OpenAlex did not answer")?;
            let status = resp.status();
            let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if !retryable || attempt >= MAX_RETRIES {
                return Ok(resp);
            }
            let wait = resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(1 << attempt)
                .min(60);
            thread::sleep(Duration::from_secs(wait));
            attempt += 1;
        }
    }

    /// Look a single work up; free of charge.
    pub fn work(&self, work: &WorkRef) -> Result<Work> {
        let url = format!("{}/works/{}", self.base, work.path());
        let resp = self.send(|| self.client.get(&url).query(&[("select", SELECT)]))?;
        if resp.status() == StatusCode::NOT_FOUND {
            bail!("OpenAlex has no work for {}.", work.display());
        }
        let resp = check(resp)?;
        resp.json().context("OpenAlex returned an unreadable work")
    }

    /// Run a list or search request, paging until `limit` works are in or the
    /// results run out.
    pub fn works(&self, req: &WorksRequest) -> Result<Works> {
        let cap = match req.query {
            Query::Semantic(_) => SEMANTIC_MAX,
            _ => usize::MAX,
        };
        let limit = req.limit.min(cap);
        let per_page = limit.clamp(1, PAGE_MAX);
        let url = format!("{}/works", self.base);
        let mut out = Works {
            works: Vec::new(),
            total: 0,
            cost_usd: 0.0,
        };
        let mut page = 1;
        while out.works.len() < limit {
            let params = req.params(page, per_page);
            let resp = check(self.send(|| self.client.get(&url).query(&params))?)?;
            let body: WorksPage = resp
                .json()
                .context("OpenAlex returned an unreadable result page")?;
            out.total = body.meta.count;
            out.cost_usd += body.meta.cost_usd.unwrap_or(0.0);
            let got = body.results.len();
            out.works.extend(body.results);
            // A semantic search answers in one go; a short page is the last.
            if got < per_page || matches!(req.query, Query::Semantic(_)) {
                break;
            }
            page += 1;
        }
        out.works.truncate(limit);
        Ok(out)
    }
}

/// Turn a non-success status into an error carrying OpenAlex's own message.
fn check(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().unwrap_or_default();
    let detail: Option<ApiError> = serde_json::from_str(&body).ok();
    let message = detail
        .as_ref()
        .and_then(|d| d.message.clone().or_else(|| d.error.clone()))
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    let code = detail.and_then(|d| d.code).unwrap_or_default();
    if code == "key_not_activated" {
        bail!(
            "The OpenAlex key is not activated yet: click the link in the activation email, \
             or unset OPENALEX_API_KEY to search without a key."
        );
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        bail!(
            "OpenAlex refused the request as over budget or over the rate limit (429): {message}"
        );
    }
    bail!("OpenAlex answered {status}: {message}");
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

/// `OPENALEX_API_KEY`, else the first line of `~/.config/openalex/key`.
fn read_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENALEX_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }
    let path = key_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let key = text.lines().next()?.trim().to_string();
    (!key.is_empty()).then_some(key)
}

fn key_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/openalex/key"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn request(query: Query) -> WorksRequest {
        WorksRequest {
            query,
            relation: None,
            year: None,
            sort: Sort::Default,
            limit: 25,
        }
    }

    #[test]
    fn citation_relations_map_to_the_two_filters() {
        let mut req = request(Query::None);
        req.relation = Some((Relation::CitedBy, "W1".into()));
        assert_eq!(req.filter().as_deref(), Some("cites:W1"));
        req.relation = Some((Relation::References, "W1".into()));
        req.year = Some(">2019".into());
        assert_eq!(
            req.filter().as_deref(),
            Some("cited_by:W1,publication_year:>2019")
        );
        assert_eq!(request(Query::None).filter(), None);
    }

    #[test]
    fn params_carry_one_search_and_the_sort() {
        let mut req = request(Query::Semantic("text".into()));
        req.sort = Sort::Cited;
        let params = req.params(1, 25);
        assert!(params.contains(&("search.semantic", "text".into())));
        assert!(!params.iter().any(|(k, _)| *k == "search"));
        assert!(params.contains(&("sort", "cited_by_count:desc".into())));
        assert!(params.contains(&("per_page", "25".into())));
    }

    #[test]
    fn openalex_ids_parse_bare_and_as_urls() {
        assert_eq!(
            parse_openalex_id("W2741809807").as_deref(),
            Some("W2741809807")
        );
        assert_eq!(
            parse_openalex_id("w2741809807").as_deref(),
            Some("W2741809807")
        );
        assert_eq!(
            parse_openalex_id("https://openalex.org/W2741809807").as_deref(),
            Some("W2741809807")
        );
        assert_eq!(parse_openalex_id("W"), None);
        assert_eq!(parse_openalex_id("A5023888391"), None);
        assert_eq!(parse_openalex_id("W12x"), None);
    }

    #[test]
    fn a_work_reads_its_doi_arxiv_id_and_abstract() {
        let work: Work = serde_json::from_value(serde_json::json!({
            "id": "https://openalex.org/W42",
            "doi": "https://doi.org/10.48550/arXiv.2401.12345",
            "display_name": "A title",
            "authorships": [{"author": {"display_name": "Ada Lovelace"}}],
            "primary_location": {"source": {"display_name": "arXiv"}},
            "abstract_inverted_index": {"world": [1], "Hello": [0], "again": [3], "hello": [2]}
        }))
        .unwrap();
        assert_eq!(work.short_id(), "W42");
        assert_eq!(
            work.bare_doi().as_deref(),
            Some("10.48550/arxiv.2401.12345")
        );
        assert_eq!(work.arxiv_id().as_deref(), Some("2401.12345"));
        assert_eq!(work.authors(), vec!["Ada Lovelace"]);
        assert_eq!(work.venue().as_deref(), Some("arXiv"));
        assert_eq!(
            work.abstract_text().as_deref(),
            Some("Hello world hello again")
        );
    }

    #[test]
    fn a_work_without_optional_fields_still_parses() {
        let work: Work = serde_json::from_value(serde_json::json!({
            "id": "https://openalex.org/W1", "doi": null, "primary_location": null,
            "abstract_inverted_index": null
        }))
        .unwrap();
        assert_eq!(work.bare_doi(), None);
        assert_eq!(work.venue(), None);
        assert_eq!(work.abstract_text(), None);
    }

    /// Answers the given bodies in order, one connection each, and records
    /// every request head.
    fn stub(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (base, seen)
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn page(ids: std::ops::Range<u32>, count: u64) -> String {
        let results: Vec<_> = ids
            .map(|i| serde_json::json!({"id": format!("https://openalex.org/W{i}")}))
            .collect();
        serde_json::json!({"meta": {"count": count, "cost_usd": 0.0001}, "results": results})
            .to_string()
    }

    #[test]
    fn paging_stops_at_the_limit_and_sends_the_key_as_a_header() {
        let (base, seen) = stub(vec![
            http("200 OK", &page(0..100, 250)),
            http("200 OK", &page(100..200, 250)),
        ]);
        let client = OpenAlexClient::with_base(&base, Some("secret"));
        let mut req = request(Query::None);
        req.limit = 150;
        let works = client.works(&req).unwrap();
        assert_eq!(works.works.len(), 150);
        assert_eq!(works.total, 250);
        assert!((works.cost_usd - 0.0002).abs() < 1e-9);
        let heads = seen.lock().unwrap();
        assert_eq!(heads.len(), 2);
        assert!(heads[0].contains("page=1") && heads[1].contains("page=2"));
        assert!(heads[0]
            .to_lowercase()
            .contains("authorization: bearer secret"));
        assert!(!heads[0].contains("api_key"));
    }

    #[test]
    fn a_throttled_page_is_retried() {
        let throttled = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
            .to_string();
        let (base, seen) = stub(vec![throttled, http("200 OK", &page(0..3, 3))]);
        let client = OpenAlexClient::with_base(&base, None);
        let works = client.works(&request(Query::Keyword("x".into()))).unwrap();
        assert_eq!(works.works.len(), 3);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_inactive_key_names_the_activation_link() {
        let body = r#"{"error":"Forbidden","code":"key_not_activated","message":"..."}"#;
        let (base, _) = stub(vec![http("403 Forbidden", body)]);
        let client = OpenAlexClient::with_base(&base, Some("k"));
        let err = client.works(&request(Query::None)).unwrap_err().to_string();
        assert!(err.contains("not activated"), "{err}");
    }

    #[test]
    fn a_missing_seed_is_reported_by_its_identifier() {
        let (base, _) = stub(vec![http("404 Not Found", "{}")]);
        let client = OpenAlexClient::with_base(&base, None);
        let err = client
            .work(&WorkRef::Doi("10.1/none".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("DOI 10.1/none"), "{err}");
    }
}
