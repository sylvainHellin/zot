//! `zot discover`: find literature in OpenAlex and mark what the library
//! already holds.
//!
//! Three modes, combinable with a keyword query:
//!   - a keyword search (`search=`) or, with `--semantic`, an embedding search
//!     over titles and abstracts (`search.semantic=`, at most 50 hits);
//!   - `--cited-by REF`: the works that cite REF (forward chaining);
//!   - `--references REF`: the works REF cites (backward chaining).
//!
//! REF is a Zotero item key, DOI, arXiv ID, PMID, PMCID or OpenAlex work ID.
//! Every hit is checked against the whole library (one listing, about half a
//! second locally): by DOI, arXiv ID or PMID first, then by normalised title,
//! which also catches the preprint of a published paper.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::api::openalex::{
    parse_openalex_id, strip_doi_prefix, OpenAlexClient, Query, Relation, Sort, Work, WorkRef,
    WorksRequest, SEMANTIC_MAX,
};
use crate::api::resolve::{parse_identifier, Identifier};
use crate::api::{ZoteroClient, ZoteroItem};
use crate::output::{format_output, truncate_display, HumanDisplay};

/// A title shorter than this is too generic ("Introduction", "Editorial") to
/// match on alone.
const MIN_TITLE_MATCH_LEN: usize = 20;

/// Upper bound on `--limit`, a guard against paging through a classic paper's
/// tens of thousands of citations by accident.
const LIMIT_MAX: usize = 1000;

pub struct DiscoverArgs<'a> {
    pub query: Option<&'a str>,
    pub semantic: bool,
    pub cited_by: Option<&'a str>,
    pub references: Option<&'a str>,
    pub year: Option<&'a str>,
    pub sort: &'a str,
    pub not_in_library: bool,
    pub abstracts: bool,
    pub limit: usize,
    pub json: bool,
}

pub fn run_discover(args: DiscoverArgs) -> Result<()> {
    let query = args.query.map(str::trim).filter(|q| !q.is_empty());
    let seed_ref = args.cited_by.or(args.references);
    if query.is_none() && seed_ref.is_none() {
        bail!("Nothing to discover: pass a query, --cited-by REF or --references REF.");
    }
    if args.semantic && query.is_none() {
        bail!("--semantic needs a query: a phrase, or a passage of up to 2,000 characters.");
    }
    if args.limit == 0 || args.limit > LIMIT_MAX {
        bail!("--limit takes 1 to {LIMIT_MAX}.");
    }
    if args.semantic && args.limit > SEMANTIC_MAX {
        eprintln!(
            "A semantic search returns at most {SEMANTIC_MAX} works; showing up to {SEMANTIC_MAX}."
        );
    }
    let sort = Sort::parse(args.sort)?;
    if args.semantic && sort != Sort::Default {
        bail!("A semantic search is ordered by similarity; drop --sort.");
    }
    let year = args.year.map(normalize_year).transpose()?;

    let openalex = OpenAlexClient::new()?;
    if !openalex.has_key() && !args.json {
        eprintln!(
            "No OpenAlex key (OPENALEX_API_KEY or ~/.config/openalex/key): searching on the keyless budget."
        );
    }
    // The library is listed lazily: a seed given as a Zotero key needs it
    // before OpenAlex is asked, every other run only afterwards.
    let zotero = ZoteroClient::new_quiet()?;

    let relation = match (args.cited_by, args.references) {
        (Some(_), Some(_)) => bail!("Pass --cited-by or --references, not both."),
        (Some(r), None) => Some((Relation::CitedBy, r)),
        (None, Some(r)) => Some((Relation::References, r)),
        (None, None) => None,
    };
    let seed = match relation {
        Some((relation, raw)) => {
            let work_ref = seed_ref_for(raw, &zotero)?;
            let work = openalex.work(&work_ref)?;
            Some((relation, work))
        }
        None => None,
    };

    let request = WorksRequest {
        query: match query {
            Some(q) if args.semantic => Query::Semantic(q.to_string()),
            Some(q) => Query::Keyword(q.to_string()),
            None => Query::None,
        },
        relation: seed
            .as_ref()
            .map(|(rel, work)| (*rel, work.short_id().to_string())),
        year: year.clone(),
        sort,
        limit: args.limit,
    };
    let found = openalex.works(&request)?;

    let items = zotero
        .fetch_all_items()
        .context("Could not list the library to mark the hits already in it")?;
    let collection_paths = collection_paths(&zotero).unwrap_or_default();
    let library = LibraryIndex::new(&items);

    let mut results = Vec::new();
    let mut in_library = 0;
    let mut hidden = 0;
    for (i, work) in found.works.iter().enumerate() {
        let library_match = library.find(work).map(|(item, by)| LibraryMatch {
            key: item.key.clone(),
            matched_by: by,
            collections: item
                .data
                .collections
                .iter()
                .map(|k| {
                    collection_paths
                        .get(k)
                        .cloned()
                        .unwrap_or_else(|| k.clone())
                })
                .collect(),
        });
        if library_match.is_some() {
            in_library += 1;
            if args.not_in_library {
                hidden += 1;
                continue;
            }
        }
        results.push(hit(i + 1, work, library_match));
    }

    let output = DiscoverOutput {
        mode: mode_label(&request.query, seed.as_ref().map(|(r, _)| *r)),
        query: query.map(String::from),
        seed: seed.as_ref().map(|(_, w)| SeedOutput {
            openalex_id: w.short_id().to_string(),
            doi: w.bare_doi(),
            title: w.title().to_string(),
            year: w.publication_year,
        }),
        year,
        total: found.total,
        fetched: found.works.len(),
        in_library,
        hidden_in_library: hidden,
        cost_usd: found.cost_usd,
        show_abstracts: args.abstracts,
        results,
    };
    println!("{}", format_output(&output, args.json));
    Ok(())
}

/// Resolve a seed given on the command line to an OpenAlex lookup.
fn seed_ref_for(raw: &str, zotero: &ZoteroClient) -> Result<WorkRef> {
    let raw = raw.trim();
    if let Some(id) = parse_openalex_id(raw) {
        return Ok(WorkRef::OpenAlex(id));
    }
    if looks_like_item_key(raw) {
        let item = zotero
            .fetch_item(raw)
            .with_context(|| format!("No Zotero item {raw}"))?;
        return item_work_ref(&item).with_context(|| {
            format!(
                "Zotero item {raw} ({}) carries no DOI, arXiv ID or PMID; \
                 pass the paper's OpenAlex ID (W...) instead.",
                truncate_display(&item.data.title, 60)
            )
        });
    }
    match parse_identifier(raw)? {
        Identifier::Doi(doi) => Ok(WorkRef::Doi(doi.to_lowercase())),
        Identifier::Arxiv(id) => Ok(WorkRef::Doi(arxiv_doi(&id))),
        Identifier::Pmid(pmid) => Ok(WorkRef::Pmid(pmid)),
        Identifier::Pmcid(pmcid) => Ok(WorkRef::Pmcid(pmcid)),
        Identifier::Isbn(_) => {
            bail!("OpenAlex indexes books by DOI, not ISBN; pass the book's DOI.")
        }
        Identifier::Url(_) | Identifier::Webpage { .. } => {
            bail!("Pass the paper's DOI, arXiv ID, PMID, Zotero key or OpenAlex ID, not a web page URL.")
        }
    }
}

/// Zotero keys are 8 characters from `23456789ABCDEFGHIJKLMNPQRSTUVWXYZ`; an
/// all-digit string of that length is a PMID instead.
fn looks_like_item_key(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_uppercase())
}

fn arxiv_doi(id: &str) -> String {
    format!("10.48550/arxiv.{}", id.to_lowercase())
}

/// The identifier a library item names itself by: DOI field, then `DOI:`,
/// `arXiv:` and `PMID:` lines in Extra, then an arxiv.org URL.
fn item_work_ref(item: &ZoteroItem) -> Option<WorkRef> {
    let ids = ItemIds::of(item);
    ids.dois
        .into_iter()
        .next()
        .map(WorkRef::Doi)
        .or_else(|| {
            ids.arxiv
                .into_iter()
                .next()
                .map(|a| WorkRef::Doi(arxiv_doi(&a)))
        })
        .or_else(|| ids.pmids.into_iter().next().map(WorkRef::Pmid))
}

/// Every identifier an item carries, normalised for matching.
#[derive(Debug, Default)]
struct ItemIds {
    dois: Vec<String>,
    arxiv: Vec<String>,
    pmids: Vec<String>,
}

impl ItemIds {
    fn of(item: &ZoteroItem) -> Self {
        let mut ids = ItemIds::default();
        let d = &item.data;
        if !d.doi.trim().is_empty() {
            ids.dois.push(strip_doi_prefix(&d.doi));
        }
        let extra = d.extra.get("extra").and_then(|v| v.as_str()).unwrap_or("");
        for line in extra.lines() {
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match field.trim().to_ascii_lowercase().as_str() {
                "doi" if !value.is_empty() => ids.dois.push(strip_doi_prefix(value)),
                "arxiv" if !value.is_empty() => ids.arxiv.push(bare_arxiv(value)),
                "pmid" if !value.is_empty() => ids.pmids.push(value.to_string()),
                _ => {}
            }
        }
        // Zotero's arXiv translator stores the abstract page URL and a
        // preprint's `archiveID` as `arXiv:2401.12345`.
        for field in [d.url.as_str(), archive_id(item)] {
            let lower = field.to_ascii_lowercase();
            if let Some(rest) = lower.split("arxiv.org/abs/").nth(1) {
                ids.arxiv.push(bare_arxiv(rest));
            } else if let Some(rest) = lower.strip_prefix("arxiv:") {
                ids.arxiv.push(bare_arxiv(rest));
            }
        }
        for doi in &ids.dois {
            if let Some(a) = doi.strip_prefix("10.48550/arxiv.") {
                ids.arxiv.push(a.to_string());
            }
        }
        ids
    }
}

fn archive_id(item: &ZoteroItem) -> &str {
    item.data
        .extra
        .get("archiveID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// An arXiv ID without version suffix, query string or `arXiv:` prefix.
fn bare_arxiv(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_prefix("arxiv:").unwrap_or(&s);
    let s = s.split(['?', '#', ' ']).next().unwrap_or("");
    let s = s.trim_end_matches('/');
    match s.rsplit_once('v') {
        Some((id, version))
            if !id.is_empty()
                && !version.is_empty()
                && version.bytes().all(|b| b.is_ascii_digit()) =>
        {
            id.to_string()
        }
        _ => s.to_string(),
    }
}

/// Lowercase letters and digits with single spaces: punctuation, case and
/// markup differ between OpenAlex and Zotero for the same title.
fn normalize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_space = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// The library's items by identifier and by normalised title.
struct LibraryIndex<'a> {
    by_doi: HashMap<String, &'a ZoteroItem>,
    by_arxiv: HashMap<String, &'a ZoteroItem>,
    by_pmid: HashMap<String, &'a ZoteroItem>,
    by_title: HashMap<String, &'a ZoteroItem>,
}

impl<'a> LibraryIndex<'a> {
    fn new(items: &'a [ZoteroItem]) -> Self {
        let mut index = LibraryIndex {
            by_doi: HashMap::new(),
            by_arxiv: HashMap::new(),
            by_pmid: HashMap::new(),
            by_title: HashMap::new(),
        };
        for item in items.iter().filter(|i| i.is_regular_item()) {
            let ids = ItemIds::of(item);
            for doi in ids.dois {
                index.by_doi.entry(doi).or_insert(item);
            }
            for a in ids.arxiv {
                index.by_arxiv.entry(a).or_insert(item);
            }
            for p in ids.pmids {
                index.by_pmid.entry(p).or_insert(item);
            }
            let title = normalize_title(&item.data.title);
            if title.len() >= MIN_TITLE_MATCH_LEN {
                index.by_title.entry(title).or_insert(item);
            }
        }
        index
    }

    /// The library item holding this work, and what matched.
    fn find(&self, work: &Work) -> Option<(&'a ZoteroItem, &'static str)> {
        if let Some(item) = work.bare_doi().and_then(|d| self.by_doi.get(&d)) {
            return Some((item, "doi"));
        }
        if let Some(item) = work
            .arxiv_id()
            .and_then(|a| self.by_arxiv.get(&bare_arxiv(&a)))
        {
            return Some((item, "arxiv"));
        }
        let pmid = work
            .ids
            .pmid
            .as_deref()
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_string());
        if let Some(item) = pmid.and_then(|p| self.by_pmid.get(&p)) {
            return Some((item, "pmid"));
        }
        let title = normalize_title(work.title());
        if title.len() >= MIN_TITLE_MATCH_LEN {
            if let Some(item) = self.by_title.get(&title) {
                return Some((item, "title"));
            }
        }
        None
    }
}

/// Collection key to its full path (`Parent / Child`).
fn collection_paths(zotero: &ZoteroClient) -> Result<HashMap<String, String>> {
    let collections = zotero.fetch_collections()?;
    let by_key: HashMap<&str, (&str, Option<&str>)> = collections
        .iter()
        .map(|c| {
            (
                c.key.as_str(),
                (c.data.name.as_str(), c.data.parent_collection.as_str()),
            )
        })
        .collect();
    let mut paths = HashMap::new();
    for key in by_key.keys() {
        let mut parts = Vec::new();
        let mut cursor = Some(*key);
        // The depth bound guards against a parent cycle in corrupt data.
        while let (Some(k), true) = (cursor, parts.len() < 32) {
            let Some((name, parent)) = by_key.get(k) else {
                break;
            };
            parts.push(*name);
            cursor = *parent;
        }
        parts.reverse();
        paths.insert(key.to_string(), parts.join(" / "));
    }
    Ok(paths)
}

/// `>2019`, `<2021`, `2018-2024` or `2021`, from what a user types: the same,
/// or `2020-` and `-2020` for open ranges.
fn normalize_year(spec: &str) -> Result<String> {
    let s = spec.trim();
    let year = |y: &str| -> Result<u32> {
        let y = y.trim();
        if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) {
            Ok(y.parse()?)
        } else {
            bail!("--year {spec:?}: use 2021, 2018-2024, 2020- (from), -2020 (until), >2019 or <2021.")
        }
    };
    if let Some(rest) = s.strip_prefix('>').or_else(|| s.strip_prefix('<')) {
        year(rest)?;
        return Ok(s.to_string());
    }
    match s.split_once('-') {
        Some((from, "")) => Ok(format!(">{}", year(from)? - 1)),
        Some(("", until)) => Ok(format!("<{}", year(until)? + 1)),
        Some((from, until)) => {
            let (f, u) = (year(from)?, year(until)?);
            if f > u {
                bail!("--year {spec:?}: the range runs backwards.");
            }
            Ok(format!("{f}-{u}"))
        }
        None => Ok(year(s)?.to_string()),
    }
}

fn mode_label(query: &Query, relation: Option<Relation>) -> String {
    let search = match query {
        Query::Keyword(_) => Some("keyword search"),
        Query::Semantic(_) => Some("semantic search"),
        Query::None => None,
    };
    let rel = relation.map(|r| match r {
        Relation::CitedBy => "cited-by",
        Relation::References => "references",
    });
    match (rel, search) {
        (Some(r), Some(s)) => format!("{r}, {s}"),
        (Some(r), None) => r.to_string(),
        (None, Some(s)) => s.to_string(),
        (None, None) => String::new(),
    }
}

fn hit(rank: usize, work: &Work, library: Option<LibraryMatch>) -> DiscoverHit {
    let doi = work.bare_doi();
    DiscoverHit {
        rank,
        openalex_id: work.short_id().to_string(),
        url: doi
            .as_ref()
            .map(|d| format!("https://doi.org/{d}"))
            .or_else(|| work.landing_page()),
        doi,
        title: work.title().to_string(),
        authors: work.authors(),
        year: work.publication_year,
        venue: work.venue(),
        work_type: work.work_type.clone(),
        cited_by_count: work.cited_by_count,
        relevance_score: work.relevance_score,
        library,
        abstract_text: work.abstract_text(),
    }
}

#[derive(Debug, Serialize)]
struct DiscoverOutput {
    mode: String,
    query: Option<String>,
    seed: Option<SeedOutput>,
    year: Option<String>,
    /// Works matching in OpenAlex.
    total: u64,
    /// Works fetched, of which `results` lists all but the hidden ones.
    fetched: usize,
    in_library: usize,
    hidden_in_library: usize,
    cost_usd: f64,
    #[serde(skip)]
    show_abstracts: bool,
    results: Vec<DiscoverHit>,
}

#[derive(Debug, Serialize)]
struct SeedOutput {
    openalex_id: String,
    doi: Option<String>,
    title: String,
    year: Option<i64>,
}

#[derive(Debug, Serialize)]
struct DiscoverHit {
    rank: usize,
    openalex_id: String,
    doi: Option<String>,
    url: Option<String>,
    title: String,
    authors: Vec<String>,
    year: Option<i64>,
    venue: Option<String>,
    #[serde(rename = "type")]
    work_type: Option<String>,
    cited_by_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    relevance_score: Option<f64>,
    /// The library item holding this work, or `null`.
    library: Option<LibraryMatch>,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
}

#[derive(Debug, Serialize)]
struct LibraryMatch {
    key: String,
    /// `doi`, `arxiv`, `pmid` or `title`; a title match can be another
    /// version of the work (preprint against published).
    matched_by: &'static str,
    collections: Vec<String>,
}

impl HumanDisplay for DiscoverOutput {
    fn human_display(&self) -> String {
        let mut out = String::new();
        let what = match (&self.seed, &self.query) {
            (Some(seed), q) => {
                let rel = if self.mode.starts_with("cited-by") {
                    "Works citing"
                } else {
                    "Works cited by"
                };
                let year = seed.year.map(|y| format!(" ({y})")).unwrap_or_default();
                let mut s = format!(
                    "{rel} \"{}\"{year} [{}]",
                    truncate_display(&seed.title, 80),
                    seed.openalex_id
                );
                if let Some(q) = q {
                    s.push_str(&format!(", matching \"{}\"", truncate_display(q, 60)));
                }
                s
            }
            (None, Some(q)) => {
                let kind = if self.mode.starts_with("semantic") {
                    "Semantic"
                } else {
                    "Keyword"
                };
                format!("{kind} search \"{}\"", truncate_display(q, 80))
            }
            (None, None) => String::new(),
        };
        out.push_str(&what);
        if let Some(year) = &self.year {
            out.push_str(&format!(", year {year}"));
        }
        out.push('\n');
        out.push_str(&format!(
            "{} works in OpenAlex, {} shown, {} already in the library",
            self.total, self.fetched, self.in_library
        ));
        if self.hidden_in_library > 0 {
            out.push_str(" (hidden)");
        }
        out.push('\n');

        for r in &self.results {
            let mark = if r.library.is_some() { "✓" } else { "·" };
            out.push_str(&format!("\n{:>3}. {mark} {}\n", r.rank, r.title));
            let mut meta = vec![authors_short(&r.authors)];
            if let Some(y) = r.year {
                meta.push(y.to_string());
            }
            if let Some(v) = &r.venue {
                meta.push(truncate_display(v, 50));
            }
            meta.push(format!("cited {}", r.cited_by_count));
            out.push_str(&format!("     {}\n", meta.join(" | ")));
            let id = match &r.doi {
                Some(doi) => format!("{doi} | {}", r.openalex_id),
                None => r.openalex_id.clone(),
            };
            out.push_str(&format!("     {id}\n"));
            if let Some(m) = &r.library {
                let by = if m.matched_by == "title" {
                    " (title match, check the version)"
                } else {
                    ""
                };
                let cols = if m.collections.is_empty() {
                    "unfiled".to_string()
                } else {
                    m.collections.join("; ")
                };
                out.push_str(&format!("     in library: {}{by} in {cols}\n", m.key));
            }
            if self.show_abstracts {
                if let Some(a) = &r.abstract_text {
                    out.push_str(&format!("     {}\n", truncate_display(a, 400)));
                }
            }
        }
        let more = self.total.saturating_sub(self.fetched as u64);
        if more > 0 && !self.mode.starts_with("semantic") {
            out.push_str(&format!(
                "\n{more} more in OpenAlex: raise --limit or narrow with a query or --year.\n"
            ));
        }
        out.push_str(&format!("OpenAlex cost: ${:.4}\n", self.cost_usd));
        out
    }
}

/// Up to three names, then `et al.`.
fn authors_short(authors: &[String]) -> String {
    match authors.len() {
        0 => "(no authors)".to_string(),
        1..=3 => authors.join(", "),
        _ => format!("{} et al.", authors[..3].join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, data: serde_json::Value) -> ZoteroItem {
        let mut d = serde_json::json!({
            "key": key, "version": 1, "itemType": "journalArticle", "title": ""
        });
        d.as_object_mut()
            .unwrap()
            .extend(data.as_object().unwrap().clone());
        serde_json::from_value(serde_json::json!({ "key": key, "version": 1, "data": d })).unwrap()
    }

    fn work(data: serde_json::Value) -> Work {
        serde_json::from_value(data).unwrap()
    }

    #[test]
    fn years_normalise_to_openalex_syntax() {
        assert_eq!(normalize_year("2021").unwrap(), "2021");
        assert_eq!(normalize_year("2018-2024").unwrap(), "2018-2024");
        assert_eq!(normalize_year("2020-").unwrap(), ">2019");
        assert_eq!(normalize_year("-2020").unwrap(), "<2021");
        assert_eq!(normalize_year(">2019").unwrap(), ">2019");
        assert!(normalize_year("2024-2018").is_err());
        assert!(normalize_year("21").is_err());
        assert!(normalize_year("last year").is_err());
    }

    #[test]
    fn item_keys_are_told_from_pmids() {
        assert!(looks_like_item_key("YJA4MMNJ"));
        assert!(looks_like_item_key("2T574FHP"));
        assert!(!looks_like_item_key("23193287"));
        assert!(!looks_like_item_key("yja4mmnj"));
        assert!(!looks_like_item_key("10.1/abc"));
    }

    #[test]
    fn arxiv_ids_lose_version_and_prefix() {
        assert_eq!(bare_arxiv("2401.12345v3"), "2401.12345");
        assert_eq!(bare_arxiv("arXiv:2401.12345"), "2401.12345");
        assert_eq!(bare_arxiv("2401.12345v2?context=cs"), "2401.12345");
        assert_eq!(bare_arxiv("cs/0112017v1"), "cs/0112017");
        assert_eq!(bare_arxiv("2401.12345"), "2401.12345");
    }

    #[test]
    fn titles_normalise_across_punctuation_and_case() {
        assert_eq!(
            normalize_title("BERT: Pre-training of Deep  Bidirectional Transformers!"),
            "bert pre training of deep bidirectional transformers"
        );
        assert_eq!(normalize_title("  Über—Größe "), "über größe");
    }

    #[test]
    fn hits_match_by_doi_then_arxiv_then_pmid_then_title() {
        let items = vec![
            item(
                "DOIITEM1",
                serde_json::json!({"DOI": "https://doi.org/10.1000/ABC"}),
            ),
            item(
                "ARXIVIT1",
                serde_json::json!({"itemType": "preprint", "url": "https://arxiv.org/abs/2401.12345v2"}),
            ),
            item(
                "PMIDITEM",
                serde_json::json!({"extra": "PMID: 23193287\nfoo: bar"}),
            ),
            item(
                "TITLEIT1",
                serde_json::json!({"title": "Attention Is All You Need, Really"}),
            ),
            item("SHORTTI1", serde_json::json!({"title": "Introduction"})),
            item(
                "NOTEITEM",
                serde_json::json!({"itemType": "note", "DOI": "10.9/x"}),
            ),
        ];
        let index = LibraryIndex::new(&items);
        let find = |w: serde_json::Value| index.find(&work(w)).map(|(i, by)| (i.key.clone(), by));

        assert_eq!(
            find(serde_json::json!({"doi": "https://doi.org/10.1000/abc"})),
            Some(("DOIITEM1".into(), "doi"))
        );
        assert_eq!(
            find(serde_json::json!({"doi": "https://doi.org/10.48550/arXiv.2401.12345"})),
            Some(("ARXIVIT1".into(), "arxiv"))
        );
        assert_eq!(
            find(serde_json::json!({"ids": {"pmid": "https://pubmed.ncbi.nlm.nih.gov/23193287"}})),
            Some(("PMIDITEM".into(), "pmid"))
        );
        assert_eq!(
            find(serde_json::json!({"doi": "https://doi.org/10.2/other",
                                    "display_name": "Attention is all you need - really"})),
            Some(("TITLEIT1".into(), "title"))
        );
        assert_eq!(
            find(serde_json::json!({"display_name": "Introduction"})),
            None
        );
        assert_eq!(
            find(serde_json::json!({"doi": "https://doi.org/10.9/x"})),
            None
        );
    }

    #[test]
    fn an_item_names_its_seed_by_doi_then_arxiv_then_pmid() {
        let by_doi = item(
            "A2345678",
            serde_json::json!({"DOI": "10.1/X", "extra": "PMID: 5"}),
        );
        assert_eq!(item_work_ref(&by_doi), Some(WorkRef::Doi("10.1/x".into())));
        let by_arxiv = item(
            "B2345678",
            serde_json::json!({"extra": "arXiv: 2401.12345\nPMID: 5"}),
        );
        assert_eq!(
            item_work_ref(&by_arxiv),
            Some(WorkRef::Doi("10.48550/arxiv.2401.12345".into()))
        );
        let by_pmid = item("C2345678", serde_json::json!({"extra": "PMID: 5"}));
        assert_eq!(item_work_ref(&by_pmid), Some(WorkRef::Pmid("5".into())));
        assert_eq!(
            item_work_ref(&item("D2345678", serde_json::json!({}))),
            None
        );
    }

    #[test]
    fn long_author_lists_shorten() {
        let names: Vec<String> = ["A", "B", "C", "D"].iter().map(|s| s.to_string()).collect();
        assert_eq!(authors_short(&names[..2]), "A, B");
        assert_eq!(authors_short(&names), "A, B, C et al.");
        assert_eq!(authors_short(&[]), "(no authors)");
    }
}
