//! `zot add <url>`: what a web page says about itself.
//!
//! Zotero's browser connector runs a site translator in the page, which zot
//! cannot do: the connector API does not expose translators, and Zotero's
//! translation-server would be a Docker sidecar for a tool that otherwise
//! needs nothing but Zotero. What zot does instead is what Zotero's own
//! "Embedded Metadata" translator does, the generic one every site falls back
//! to: read the `<meta>` tags. Highwire Press `citation_*` tags come first,
//! since Google Scholar requires them and nearly every publisher, repository
//! and journal system emits them; Dublin Core (`dc.*`), PRISM (`prism.*`),
//! Open Graph (`og:*`) and the `<title>` fill in behind them.
//!
//! A DOI, arXiv ID or PMID found that way hands the add over to the
//! identifier path, whose metadata is better than any page's. Only a page
//! without one becomes an entry built from its tags, an `@article` when the
//! tags name a journal and an `@online` (Zotero's `webpage`) otherwise.

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;

use super::resolve::{bibtex_entry, citekey, parse_identifier, Identifier};

/// Pages larger than this are not read past it; the `<head>`, where the tags
/// live, always comes first.
const MAX_PAGE_BYTES: usize = 5 * 1024 * 1024;

/// A fetched page: the URL it ended up at after redirects, and its HTML.
pub struct Page {
    pub final_url: String,
    pub html: String,
}

/// GET a page as a browser would ask for it.
///
/// The user agent is honest but browser-shaped: a bare `zot/x.y` is turned
/// away by more sites than one that starts with `Mozilla/5.0 (compatible;`.
/// A PDF is refused with the remedy rather than parsed as HTML.
pub fn fetch(url: &str) -> Result<Page> {
    let client = Client::builder()
        .user_agent(format!(
            "Mozilla/5.0 (compatible; zot/{}; +https://github.com/sylvainHellin/zot)",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")?;
    let resp = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5")
        .send()
        .with_context(|| format!("Failed to fetch {url}"))?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    if !status.is_success() {
        bail!(
            "{url} answered {status}, so its metadata could not be read.\n  \
             Publishers often refuse non-browser clients; pass the article's DOI instead."
        );
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("application/pdf") {
        bail!(
            "{url} is a PDF, not a web page.\n  \
             Download it and run: zot add --pdf <file> (with the DOI if you have it)."
        );
    }
    let bytes = resp
        .bytes()
        .with_context(|| format!("Failed to read {url}"))?;
    let body = &bytes[..bytes.len().min(MAX_PAGE_BYTES)];
    Ok(Page {
        final_url,
        html: String::from_utf8_lossy(body).into_owned(),
    })
}

/// The DOI a URL carries in its path or query, as publishers put it there:
/// `dl.acm.org/doi/10.1145/3597503`, `link.springer.com/article/10.1007/...`,
/// `onlinelibrary.wiley.com/doi/full/10.1002/...`, `?doi=10.1109/...`.
///
/// The DOI runs to the end of the path, because a DOI suffix may itself hold
/// slashes; only the view segments publishers append (`/full`, `/abstract`,
/// `/pdf`, ...) are cut off, and a trailing `.pdf`.
pub fn doi_in_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let path = urlencoding::decode(parsed.path()).ok()?.into_owned();
    let from_path = path
        .split('/')
        .position(is_doi_prefix)
        .map(|i| path.split('/').skip(i).collect::<Vec<_>>());
    let candidate = match from_path {
        Some(mut segments) => {
            while segments.len() > 2
                && matches!(
                    segments.last().map(|s| s.to_ascii_lowercase()).as_deref(),
                    Some(
                        "" | "full"
                            | "abstract"
                            | "abs"
                            | "pdf"
                            | "epdf"
                            | "html"
                            | "fulltext"
                            | "references"
                            | "figures"
                            | "summary"
                    )
                )
            {
                segments.pop();
            }
            segments.join("/")
        }
        None => parsed
            .query_pairs()
            .find(|(k, v)| k.eq_ignore_ascii_case("doi") && v.starts_with("10."))
            .map(|(_, v)| v.into_owned())?,
    };
    let doi = candidate.trim_end_matches(".pdf").to_string();
    let (prefix, suffix) = doi.split_once('/')?;
    (is_doi_prefix(prefix) && !suffix.is_empty()).then_some(doi)
}

/// `10.` followed by a registrant code of four or more digits (`10.1145`),
/// optionally with sub-codes (`10.1000.10`).
fn is_doi_prefix(seg: &str) -> bool {
    let Some(code) = seg.strip_prefix("10.") else {
        return false;
    };
    let first = code.split('.').next().unwrap_or("");
    first.len() >= 4
        && code
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// The `<meta>` tags of a page, by lowercased `name` or `property`, in
/// document order, plus its `<title>`.
#[derive(Debug, Default)]
pub struct PageMeta {
    tags: Vec<(String, String)>,
    title: Option<String>,
}

/// Titles bot walls and error pages answer with 200. A page titled so is not
/// the page that was asked for, and must not become an item.
const BLOCKED_TITLES: [&str; 7] = [
    "just a moment",
    "attention required",
    "access denied",
    "are you a robot",
    "security check",
    "verifying you are human",
    "page not found",
];

impl PageMeta {
    /// Scan `html` for `<meta>` tags and the `<title>`. Tags inside comments
    /// are skipped; anything else malformed is ignored rather than fatal.
    #[allow(
        clippy::string_slice,
        reason = "every index comes from find() on ASCII needles in the lowercased copy, which \
                  has the same byte offsets as the original because only ASCII is lowercased"
    )]
    pub fn parse(html: &str) -> Self {
        let lower = html.to_ascii_lowercase();
        let mut meta = PageMeta::default();
        let mut i = 0;
        while let Some(rel) = lower[i..].find('<') {
            let start = i + rel;
            if lower[start..].starts_with("<!--") {
                i = lower[start..]
                    .find("-->")
                    .map_or(lower.len(), |e| start + e + 3);
                continue;
            }
            let Some(end_rel) = lower[start..].find('>') else {
                break;
            };
            let end = start + end_rel;
            if lower[start..].starts_with("<meta") {
                let attrs = parse_attributes(&html[start + 5..end]);
                let key = attrs
                    .iter()
                    .find(|(k, _)| k == "name" || k == "property")
                    .map(|(_, v)| v.trim().to_ascii_lowercase());
                let content = attrs.iter().find(|(k, _)| k == "content").map(|(_, v)| v);
                if let (Some(key), Some(content)) = (key, content) {
                    let content = collapse(&decode_entities(content));
                    if !content.is_empty() {
                        meta.tags.push((key, content));
                    }
                }
            } else if lower[start..].starts_with("<title") && meta.title.is_none() {
                if let Some(close) = lower[end..].find("</title") {
                    let text = collapse(&decode_entities(&html[end + 1..end + close]));
                    if !text.is_empty() {
                        meta.title = Some(text);
                    }
                }
            } else if lower[start..].starts_with("<script") || lower[start..].starts_with("<style")
            {
                // Skip the body, where a `<` is code rather than markup.
                let close = if lower[start..].starts_with("<script") {
                    "</script"
                } else {
                    "</style"
                };
                i = lower[end..].find(close).map_or(lower.len(), |c| end + c);
                continue;
            }
            i = end + 1;
        }
        meta
    }

    /// The first non-empty value of the first tag name that has one.
    fn first(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| {
            self.tags
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }

    /// Every value of the first tag name that has any.
    fn all(&self, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|name| {
                self.tags
                    .iter()
                    .filter(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .collect::<Vec<_>>()
            })
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }

    /// The identifier the page names for itself: a DOI, else an arXiv ID,
    /// else a PMID.
    pub fn identifier(&self) -> Option<Identifier> {
        const DOI_TAGS: [&str; 7] = [
            "citation_doi",
            "bepress_citation_doi",
            "prism.doi",
            "dc.identifier.doi",
            "dc.identifier",
            "dcterms.identifier",
            "doi",
        ];
        let doi = DOI_TAGS.iter().find_map(|name| {
            self.tags
                .iter()
                .filter(|(k, _)| k == name)
                .find_map(|(_, v)| normalize_doi(v))
        });
        if let Some(doi) = doi {
            return Some(Identifier::Doi(doi));
        }
        if let Some(Ok(id @ Identifier::Arxiv(_))) = self
            .first(&["citation_arxiv_id"])
            .map(|a| parse_identifier(&format!("arXiv:{a}")))
        {
            return Some(id);
        }
        if let Some(Ok(id @ Identifier::Pmid(_))) = self
            .first(&["citation_pmid"])
            .map(|p| parse_identifier(&format!("PMID:{p}")))
        {
            return Some(id);
        }
        None
    }

    /// A BibTeX entry for the page, for when it names no identifier. `url` is
    /// where the page was read from, `today` the access date (`YYYY-MM-DD`).
    pub fn to_bibtex(&self, url: &str, today: &str) -> Result<String> {
        let title = self
            .first(&["citation_title", "dc.title", "og:title", "twitter:title"])
            .or_else(|| self.title.clone())
            .with_context(|| format!("{url} has no title, so there is nothing to add."))?;
        let lowered = title.to_lowercase();
        if BLOCKED_TITLES.iter().any(|b| lowered.starts_with(b)) {
            bail!(
                "{url} answered with a page titled \"{title}\", a bot check or error page \
                 rather than the page itself.\n  Pass the article's DOI instead, or add it from \
                 the browser with the Zotero connector."
            );
        }

        let authors: Vec<String> = self
            .all(&["citation_author", "dc.creator", "author", "article:author"])
            .into_iter()
            .filter(|a| !a.starts_with("http"))
            .collect();
        let date = self
            .first(&[
                "citation_publication_date",
                "citation_date",
                "citation_cover_date",
                "citation_online_date",
                "dc.date.issued",
                "dc.date",
                "dcterms.issued",
                "prism.publicationdate",
                "article:published_time",
            ])
            .map(|d| normalize_date(&d));
        let journal = self.first(&["citation_journal_title", "prism.publicationname"]);
        let conference = self.first(&["citation_conference_title"]);
        let thesis_school = self.first(&["citation_dissertation_institution"]);
        let report_institution = self.first(&["citation_technical_report_institution"]);

        let kind = if journal.is_some() {
            "article"
        } else if conference.is_some() {
            "inproceedings"
        } else if thesis_school.is_some() {
            "phdthesis"
        } else if report_institution.is_some() {
            "techreport"
        } else {
            "online"
        };

        let mut fields: Vec<(&str, String)> = vec![("title", title.clone())];
        if !authors.is_empty() {
            fields.push(("author", authors.join(" and ")));
        }
        if let Some(d) = &date {
            fields.push(("date", d.clone()));
        }
        let mut push = |name: &'static str, value: Option<String>| {
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                fields.push((name, v));
            }
        };
        push("journal", journal);
        push("booktitle", conference);
        push("school", thesis_school);
        push("institution", report_institution);
        push("number", self.first(&["citation_technical_report_number"]));
        push("volume", self.first(&["citation_volume", "prism.volume"]));
        if kind != "techreport" {
            push("number", self.first(&["citation_issue", "prism.number"]));
        }
        let first_page = self.first(&["citation_firstpage", "prism.startingpage"]);
        let last_page = self.first(&["citation_lastpage", "prism.endingpage"]);
        push(
            "pages",
            match (first_page, last_page) {
                (Some(f), Some(l)) if f != l => Some(format!("{f}-{l}")),
                (Some(f), _) => Some(f),
                _ => None,
            },
        );
        push(
            "publisher",
            self.first(&["citation_publisher", "dc.publisher", "dcterms.publisher"]),
        );
        push("issn", self.first(&["citation_issn", "prism.issn"]));
        push("isbn", self.first(&["citation_isbn"]));
        push(
            "language",
            self.first(&["citation_language", "dc.language"]),
        );
        push(
            "abstract",
            self.first(&[
                "citation_abstract",
                "dc.description",
                "dcterms.abstract",
                "description",
                "og:description",
            ]),
        );
        if kind == "online" {
            push("organization", self.first(&["og:site_name"]));
        }
        fields.push(("url", url.to_string()));
        fields.push(("urldate", today.to_string()));

        let year = date.as_deref().and_then(|d| d.get(..4)).map(str::to_string);
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        let key = citekey(
            authors.first().map(String::as_str),
            year.as_deref(),
            &title,
            &host,
        );
        Ok(bibtex_entry(kind, &key, &fields))
    }
}

/// `doi:10.1/x`, `https://doi.org/10.1/x` and `10.1/x` all become `10.1/x`;
/// anything that is not a DOI is `None` (`dc.identifier` also carries ISSNs
/// and URLs).
fn normalize_doi(v: &str) -> Option<String> {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    let start = lower.find("10.")?;
    let head = lower.get(..start)?;
    let allowed = head.is_empty()
        || head == "doi:"
        || head == "doi: "
        || head.ends_with("doi.org/")
        || head == "info:doi/";
    if !allowed {
        return None;
    }
    let doi = v.get(start..)?.trim().to_string();
    let (prefix, suffix) = doi.split_once('/')?;
    (is_doi_prefix(prefix) && !suffix.is_empty()).then_some(doi)
}

/// `2020/05/12`, `2020-05-12T10:00:00Z` and `2020-05-12` all become
/// `2020-05-12`; a year-month keeps its two parts, and anything else is kept
/// as given for Zotero's own date parser.
fn normalize_date(d: &str) -> String {
    let d = d.trim().replace('/', "-");
    let parts: Vec<&str> = d
        .split(['T', ' '])
        .next()
        .unwrap_or("")
        .split('-')
        .collect();
    let numeric = parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if numeric && parts[0].len() == 4 && parts.len() <= 3 {
        return parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if i == 0 {
                    p.to_string()
                } else {
                    format!("{p:0>2}")
                }
            })
            .collect::<Vec<_>>()
            .join("-");
    }
    d
}

/// Today's date in UTC as `YYYY-MM-DD`, for `urldate`.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_date(i64::try_from(secs / 86_400).unwrap_or(0))
}

/// Days since 1970-01-01 to a proleptic Gregorian `YYYY-MM-DD` (Howard
/// Hinnant's `civil_from_days`).
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The `name="value"` pairs of a tag's attribute text, names lowercased.
/// Values may be double-quoted, single-quoted or bare.
fn parse_attributes(s: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        let name_start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' && chars[i] != '/' {
            i += 1;
        }
        let name: String = chars[name_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            if !name.is_empty() {
                out.push((name, String::new()));
            }
            continue;
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let value: String = match chars.get(i) {
            Some(&q) if q == '"' || q == '\'' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != q {
                    i += 1;
                }
                let v = chars[start..i].iter().collect();
                i += 1;
                v
            }
            _ => {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                chars[start..i].iter().collect()
            }
        };
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// Decode the named entities metadata actually uses and every numeric one.
fn decode_entities(s: &str) -> String {
    const NAMED: [(&str, char); 12] = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
        ("nbsp", ' '),
        ("ndash", '\u{2013}'),
        ("mdash", '\u{2014}'),
        ("rsquo", '\u{2019}'),
        ("lsquo", '\u{2018}'),
        ("rdquo", '\u{201D}'),
        ("ldquo", '\u{201C}'),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(rest.get(..pos).unwrap_or(""));
        let after = rest.get(pos + 1..).unwrap_or("");
        let decoded = after.find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let name = after.get(..semi)?;
            let c = if let Some(num) = name.strip_prefix('#') {
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => num.parse().ok()?,
                };
                char::from_u32(code)?
            } else {
                NAMED.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)?
            };
            Some((c, semi))
        });
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = after.get(semi + 1..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::bibtex;

    /// The head of an OJS article page, trimmed: Highwire tags, no DOI.
    const OJS: &str = r#"<!DOCTYPE html><html><head>
        <meta charset="utf-8">
        <title>Digital twins in construction | Journal of IT in Construction</title>
        <!-- <meta name="citation_doi" content="10.9999/commented-out"> -->
        <meta name="citation_title" content="Digital twins in construction: a review">
        <meta name="citation_author" content="M&#252;ller, Anna">
        <meta name="citation_author" content="Smith, John">
        <meta name="citation_publication_date" content="2021/03/05">
        <meta name="citation_journal_title" content="Journal of Information Technology in Construction">
        <meta name="citation_volume" content="26">
        <meta name="citation_issue" content="3">
        <meta name="citation_firstpage" content="101">
        <meta name="citation_lastpage" content="120">
        <meta name='citation_issn' content='1874-4753' />
        <script>if (a < b) { document.write('<meta name="citation_title" content="x">'); }</script>
        </head><body></body></html>"#;

    /// A blog post: Open Graph only.
    const BLOG: &str = r#"<html><head>
        <title>Ignored when og:title exists</title>
        <meta property="og:title" content="Why BIM &#8211; and why now">
        <meta property="og:site_name" content="Example Blog">
        <meta property="article:published_time" content="2024-11-02T08:30:00+00:00">
        <meta property="article:author" content="https://example.com/authors/jane">
        <meta name="author" content="Jane Doe">
        <meta name="description" content="A short post.">
        </head></html>"#;

    #[test]
    fn highwire_tags_and_comments_and_scripts_are_read_correctly() {
        let meta = PageMeta::parse(OJS);
        assert_eq!(
            meta.identifier(),
            None,
            "the commented-out DOI must not count"
        );
        assert_eq!(
            meta.first(&["citation_title"]).as_deref(),
            Some("Digital twins in construction: a review")
        );
        assert_eq!(meta.all(&["citation_author"]).len(), 2);
    }

    #[test]
    fn a_journal_page_without_a_doi_becomes_an_article() {
        let bib = PageMeta::parse(OJS)
            .to_bibtex("https://www.itcon.org/paper/2021/5", "2026-11-03")
            .unwrap();
        assert!(bib.starts_with("@article{"), "{bib}");
        let item = bibtex::bibtex_to_zotero(&bib).unwrap();
        assert_eq!(item["itemType"], "journalArticle");
        assert_eq!(item["title"], "Digital twins in construction: a review");
        assert_eq!(
            item["publicationTitle"],
            "Journal of Information Technology in Construction"
        );
        assert_eq!(item["date"], "2021-03-05");
        assert_eq!(item["pages"], "101-120");
        assert_eq!(item["issue"], "3");
        assert_eq!(item["url"], "https://www.itcon.org/paper/2021/5");
        assert_eq!(item["creators"][1]["lastName"], "Smith");
    }

    #[test]
    fn an_open_graph_page_becomes_a_webpage_with_site_and_access_date() {
        let bib = PageMeta::parse(BLOG)
            .to_bibtex("https://blog.example.com/why_bim?x=1%202", "2026-11-03")
            .unwrap();
        assert!(bib.starts_with("@online{doe2024why,"), "{bib}");
        // The URL is written raw: no backslash before `_` or `%`.
        assert!(
            bib.contains("url = {https://blog.example.com/why_bim?x=1%202}"),
            "{bib}"
        );
        let item = bibtex::bibtex_to_zotero(&bib).unwrap();
        assert_eq!(item["itemType"], "webpage");
        assert_eq!(item["title"], "Why BIM \u{2013} and why now");
        assert_eq!(item["websiteTitle"], "Example Blog");
        assert_eq!(item["accessDate"], "2026-11-03");
        assert_eq!(item["date"], "2024-11-02");
        assert_eq!(item["url"], "https://blog.example.com/why_bim?x=1%202");
        assert_eq!(item["creators"][0]["lastName"], "Doe");
        assert_eq!(item["creators"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_page_naming_its_doi_hands_over_to_the_doi() {
        let html = r#"<meta name="dc.identifier" content="ISSN 1234-5678">
            <meta name="DC.Identifier" content="doi:10.1016/j.autcon.2020.103456">"#;
        assert_eq!(
            PageMeta::parse(html).identifier(),
            Some(Identifier::Doi("10.1016/j.autcon.2020.103456".into()))
        );
        let html = r#"<meta content="https://doi.org/10.1038/nature14539" name="citation_doi">"#;
        assert_eq!(
            PageMeta::parse(html).identifier(),
            Some(Identifier::Doi("10.1038/nature14539".into()))
        );
        let html = r#"<meta name="citation_arxiv_id" content="2401.12345">"#;
        assert_eq!(
            PageMeta::parse(html).identifier(),
            Some(Identifier::Arxiv("2401.12345".into()))
        );
    }

    #[test]
    fn a_bot_wall_is_refused_rather_than_added() {
        let html = "<html><head><title>Just a moment...</title></head></html>";
        let err = PageMeta::parse(html)
            .to_bibtex(
                "https://www.sciencedirect.com/science/article/pii/S1",
                "2026-11-03",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("bot check"), "{err}");
        let err = PageMeta::parse("<html></html>")
            .to_bibtex("https://example.com/", "2026-11-03")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no title"), "{err}");
    }

    #[test]
    fn dois_are_read_out_of_publisher_urls() {
        for (url, doi) in [
            (
                "https://dl.acm.org/doi/10.1145/3597503.3639187",
                Some("10.1145/3597503.3639187"),
            ),
            (
                "https://dl.acm.org/doi/pdf/10.1145/3597503",
                Some("10.1145/3597503"),
            ),
            (
                "https://onlinelibrary.wiley.com/doi/full/10.1111/mice.12345/abstract",
                Some("10.1111/mice.12345"),
            ),
            (
                "https://link.springer.com/chapter/10.1007/978-3-030-51295-8_5",
                Some("10.1007/978-3-030-51295-8_5"),
            ),
            (
                "https://example.org/view?doi=10.1109%2F5.771073",
                Some("10.1109/5.771073"),
            ),
            ("https://example.org/paper/10.12/x", None),
            ("https://www.nature.com/articles/s41586-021-03819-2", None),
        ] {
            assert_eq!(doi_in_url(url).as_deref(), doi, "{url}");
        }
    }

    #[test]
    fn dates_and_entities_normalise() {
        assert_eq!(normalize_date("2021/3/5"), "2021-03-05");
        assert_eq!(normalize_date("2024-11-02T08:30:00Z"), "2024-11-02");
        assert_eq!(normalize_date("2020"), "2020");
        assert_eq!(normalize_date("March 2020"), "March 2020");
        assert_eq!(
            decode_entities("a &amp; b &#x2013; &#233; &bogus; & c"),
            "a & b \u{2013} \u{e9} &bogus; & c"
        );
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(20_395), "2025-11-03");
        assert_eq!(civil_date(11_016), "2000-02-29");
    }
}
