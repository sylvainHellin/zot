//! Resolve external identifiers (DOI, arXiv, ISBN, PMID, PMCID, web page URL)
//! to BibTeX for import.
//!
//! DOIs go through doi.org content negotiation (Crossref/DataCite/mEDRA all
//! honor `Accept: application/x-bibtex`). arXiv IDs use arxiv.org's BibTeX
//! export endpoint, falling back to the arXiv DataCite DOI
//! (`10.48550/arXiv.<id>`).
//!
//! PMIDs go to NCBI `efetch` once, in MEDLINE form. When the record carries a
//! DOI (tag `LID` or `AID`, marked `[doi]`) the DOI path takes over, because
//! the publisher's BibTeX beats anything derived from MEDLINE; otherwise the
//! MEDLINE text is mapped to BibTeX here. NCBI's idconv API is deliberately
//! not used: it only knows PMC records, so a PubMed-only PMID is
//! indistinguishable from a nonsense one there, and efetch answers both cases
//! in a single request.
//!
//! PMCIDs are the one case idconv is right for: every PMC article is in its
//! index, so [`canonicalize`] maps a PMCID to the DOI (or else the PMID) it
//! carries in one request, and the item is resolved from that.
//!
//! A plain URL is read for the metadata the page embeds (`api::webpage`); a
//! DOI, arXiv ID or PMID found there takes over, else the page becomes a
//! `webpage`-style entry built from its tags.
//!
//! ISBNs go to OpenLibrary's `/api/books?jscmd=data` endpoint, which returns
//! author names inline; the `/isbn/<isbn>.json` form redirects to an edition
//! record that carries author *keys* only, costing one request per author.
//! There is no Google Books fallback: the keyless endpoint answers 429
//! ("Queries per day") for everyone sharing the anonymous project, so it would
//! be a path that never succeeds and whose error would mask OpenLibrary's.

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;

/// A bare all-digit string of at most this many digits is read as a PMID.
/// PMIDs are 8 digits today; ISBNs normalise to exactly 10 or 13 digits, so
/// the two ranges cannot overlap and the spare digit leaves room to grow.
const MAX_PMID_DIGITS: usize = 9;

#[derive(Debug, Clone, PartialEq)]
pub enum Identifier {
    Doi(String),
    Arxiv(String),
    /// Normalised to digits only, with an uppercase `X` check digit.
    Isbn(String),
    Pmid(String),
    /// `PMC` followed by digits, uppercase.
    Pmcid(String),
    /// An http(s) URL that is none of the identifier URLs above, as typed.
    Url(String),
    /// A URL whose page carried no identifier, with the entry built from its
    /// embedded metadata. Only [`canonicalize`] produces it, so the page is
    /// fetched once however often the BibTeX is asked for.
    Webpage {
        url: String,
        bibtex: String,
    },
}

impl Identifier {
    /// The string used for the duplicate check against the library.
    pub fn dedup_query(&self) -> &str {
        match self {
            Identifier::Doi(d) => d,
            Identifier::Arxiv(id) => id,
            Identifier::Isbn(isbn) => isbn,
            Identifier::Pmid(pmid) => pmid,
            Identifier::Pmcid(pmcid) => pmcid,
            Identifier::Url(url) | Identifier::Webpage { url, .. } => url,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Identifier::Doi(d) => format!("DOI {d}"),
            Identifier::Arxiv(id) => format!("arXiv:{id}"),
            Identifier::Isbn(isbn) => format!("ISBN {isbn}"),
            Identifier::Pmid(pmid) => format!("PMID {pmid}"),
            Identifier::Pmcid(pmcid) => format!("PMCID {pmcid}"),
            Identifier::Url(url) => url.clone(),
            Identifier::Webpage { url, .. } => format!("web page {url}"),
        }
    }
}

/// Parse a user-supplied identifier. Accepts bare DOIs, doi.org URLs,
/// `arXiv:ID`, bare arXiv IDs (new-style `2401.12345`), arxiv.org URLs,
/// `ISBN:`/bare ISBN-10 and ISBN-13 (hyphenated or spaced),
/// `PMID:`/`PubMed:`/bare PubMed IDs and pubmed URLs, `PMCID:`/bare PMCIDs
/// and PMC article URLs, and finally any other http(s) URL as a web page.
/// Only the shape is checked here; nothing is fetched.
#[allow(
    clippy::string_slice,
    reason = "the index comes from find(\"arxiv.org/\"), an ASCII match, so it is a char boundary"
)]
pub fn parse_identifier(input: &str) -> Result<Identifier> {
    let s = input.trim();

    // URL forms
    if let Some(rest) = s
        .strip_prefix("https://doi.org/")
        .or_else(|| s.strip_prefix("http://doi.org/"))
        .or_else(|| s.strip_prefix("https://dx.doi.org/"))
        .or_else(|| s.strip_prefix("http://dx.doi.org/"))
    {
        return Ok(Identifier::Doi(rest.to_string()));
    }
    if let Some(pos) = s.find("arxiv.org/") {
        let rest = &s[pos + "arxiv.org/".len()..];
        let id = rest
            .strip_prefix("abs/")
            .or_else(|| rest.strip_prefix("pdf/"))
            .unwrap_or(rest)
            .trim_end_matches(".pdf")
            .trim_end_matches('/');
        if !id.is_empty() {
            return Ok(Identifier::Arxiv(strip_arxiv_version(id)));
        }
    }
    if is_web_url(s) {
        if let Some(id) = ncbi_url_identifier(s) {
            return Ok(id);
        }
        return Ok(Identifier::Url(s.to_string()));
    }
    if s.starts_with("www.") && !s.contains(char::is_whitespace) {
        return Ok(Identifier::Url(format!("https://{s}")));
    }

    // Prefixed forms
    if let Some(rest) = s.strip_prefix("doi:").or_else(|| s.strip_prefix("DOI:")) {
        return Ok(Identifier::Doi(rest.trim().to_string()));
    }
    let lower = s.to_lowercase();
    if let Some(rest) = lower.strip_prefix("arxiv:") {
        return Ok(Identifier::Arxiv(strip_arxiv_version(rest.trim())));
    }
    if let Some(rest) = lower.strip_prefix("isbn:") {
        let raw = rest.trim();
        return match normalize_isbn(raw) {
            Some(isbn) => Ok(Identifier::Isbn(isbn)),
            None => bail!(
                "Not a valid ISBN: {raw}\n  \
                 An ISBN is 10 or 13 digits (hyphens and spaces are fine, ISBN-10 may end in \
                 X) and its check digit has to match."
            ),
        };
    }
    if let Some(rest) = lower
        .strip_prefix("pmid:")
        .or_else(|| lower.strip_prefix("pubmed:"))
    {
        let raw = rest.trim();
        if !looks_like_pmid(raw) {
            bail!(
                "Not a valid PubMed ID: {raw}\n  \
                 A PMID is a plain number of up to {MAX_PMID_DIGITS} digits, with no leading zero."
            );
        }
        return Ok(Identifier::Pmid(raw.to_string()));
    }
    if let Some(rest) = lower.strip_prefix("pmcid:") {
        let raw = rest.trim();
        return normalize_pmcid(raw).map(Identifier::Pmcid).with_context(|| {
            format!("Not a valid PMCID: {raw}\n  A PMCID is PMC followed by digits, e.g. PMC3531190.")
        });
    }
    if let Some(pmcid) = lower
        .starts_with("pmc")
        .then(|| normalize_pmcid(s))
        .flatten()
    {
        return Ok(Identifier::Pmcid(pmcid));
    }

    // Bare forms. DOI and arXiv come first, unchanged: everything below is
    // digits-only, so neither `10.xxxx/yyy` nor `2401.12345` can reach it.
    if s.starts_with("10.") && s.contains('/') {
        // The arXiv DataCite DOI is still a DOI; fine either way.
        return Ok(Identifier::Doi(s.to_string()));
    }
    if looks_like_arxiv_id(s) {
        return Ok(Identifier::Arxiv(strip_arxiv_version(s)));
    }
    // ISBN before PMID, though the two cannot collide: an ISBN normalises to
    // exactly 10 or 13 digits and a PMID is at most MAX_PMID_DIGITS of them.
    // A 10- or 13-digit number whose check digit is wrong is therefore neither,
    // and falls through to the error rather than being taken for a PMID.
    if let Some(isbn) = normalize_isbn(s) {
        return Ok(Identifier::Isbn(isbn));
    }
    if looks_like_pmid(s) {
        return Ok(Identifier::Pmid(s.to_string()));
    }

    // A bare ISBN-shaped number only reaches here because its check digit
    // failed; the list above names ISBN without saying why this one was not
    // read as one.
    let hint = if looks_like_bare_isbn_shape(s) {
        "\n  It has the shape of an ISBN, but its check digit does not match."
    } else {
        ""
    };
    bail!(
        "Could not recognize identifier: {input}\n  \
         Supported: DOI (10.xxxx/...), doi.org URL, arXiv ID (2401.12345), arxiv.org URL, \
         ISBN-10/ISBN-13 (also ISBN:...), PubMed ID (also PMID:... or PubMed:...), \
         PMCID (PMC3531190), or a web page URL (https://...).{hint}"
    );
}

/// An unprefixed value shaped like an ISBN: 10 or 13 characters, all digits
/// apart from an optional trailing `X` check digit. Used only to explain a
/// rejection, never to accept one.
fn looks_like_bare_isbn_shape(s: &str) -> bool {
    let len = s.chars().count();
    matches!(len, 10 | 13)
        && s.chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_digit() || (len == 10 && i == 9 && (c == 'X' || c == 'x')))
}

fn is_web_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && url::Url::parse(s).is_ok_and(|u| u.host_str().is_some())
}

/// The PubMed or PMC record a PubMed or PMC article URL points at, read off
/// the path: `pubmed.ncbi.nlm.nih.gov/<pmid>/`,
/// `pmc.ncbi.nlm.nih.gov/articles/<pmcid>/` and the older
/// `www.ncbi.nlm.nih.gov/pmc/articles/<pmcid>/` and `.../pubmed/<pmid>`.
fn ncbi_url_identifier(s: &str) -> Option<Identifier> {
    let url = url::Url::parse(s).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if !host.ends_with("ncbi.nlm.nih.gov") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    let pmcid = segments
        .windows(2)
        .find(|w| w[0] == "articles")
        .and_then(|w| normalize_pmcid(w[1]));
    if let Some(pmcid) = pmcid {
        return Some(Identifier::Pmcid(pmcid));
    }
    let pmid = if host.starts_with("pubmed.") {
        segments.first().copied()
    } else {
        segments.windows(2).find(|w| w[0] == "pubmed").map(|w| w[1])
    };
    pmid.filter(|p| looks_like_pmid(p))
        .map(|p| Identifier::Pmid(p.to_string()))
}

/// `PMC` and digits, uppercased; `None` for anything else. A bare number is
/// not accepted, since it is a PMID.
fn normalize_pmcid(s: &str) -> Option<String> {
    let upper = s.trim().to_ascii_uppercase();
    let digits = upper.strip_prefix("PMC")?;
    (!digits.is_empty() && digits.len() <= 10 && digits.chars().all(|c| c.is_ascii_digit()))
        .then_some(upper)
}

/// A PMID as typed: a plain number, no separators, no leading zero.
fn looks_like_pmid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PMID_DIGITS
        && !s.starts_with('0')
        && s.chars().all(|c| c.is_ascii_digit())
}

/// Strip hyphens and spaces, uppercase the check digit, and verify it.
/// `None` when the input is not an ISBN at all or its check digit is wrong, so
/// a bare 13-digit number that happens not to be an ISBN is never accepted.
fn normalize_isbn(s: &str) -> Option<String> {
    let core: String = s
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\u{2010}' | '\u{2013}'))
        .map(|c| if c == 'x' { 'X' } else { c })
        .collect();
    if !isbn_check_digit_ok(&core) {
        return None;
    }
    Some(core)
}

/// ISBN-10 is mod 11 over weights 10..1 with `X` standing for 10; ISBN-13 is
/// mod 10 over alternating weights 1 and 3.
fn isbn_check_digit_ok(core: &str) -> bool {
    let digits: Vec<char> = core.chars().collect();
    match digits.len() {
        10 => {
            let mut sum = 0u32;
            for (i, c) in digits.iter().enumerate() {
                let v = match c {
                    'X' if i == 9 => 10,
                    c if c.is_ascii_digit() => u32::from(*c as u8 - b'0'),
                    _ => return false,
                };
                sum += v * (10 - u32::try_from(i).unwrap_or(0));
            }
            sum.is_multiple_of(11)
        }
        13 => {
            let mut sum = 0u32;
            for (i, c) in digits.iter().enumerate() {
                let Some(v) = c.to_digit(10) else {
                    return false;
                };
                sum += if i.is_multiple_of(2) { v } else { v * 3 };
            }
            sum.is_multiple_of(10)
        }
        _ => false,
    }
}

/// New-style arXiv IDs: NNNN.NNNNN with optional vN suffix.
fn looks_like_arxiv_id(s: &str) -> bool {
    let core = strip_arxiv_version(s);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 2
        && parts[0].len() == 4
        && (4..=5).contains(&parts[1].len())
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

#[allow(
    clippy::string_slice,
    reason = "the index comes from rfind('v'), an ASCII match, so it is a char boundary"
)]
fn strip_arxiv_version(s: &str) -> String {
    if let Some(pos) = s.rfind('v') {
        if pos > 0 && s[pos + 1..].chars().all(|c| c.is_ascii_digit()) && !s[pos + 1..].is_empty() {
            return s[..pos].to_string();
        }
    }
    s.to_string()
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("zot/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")
}

/// Turn an identifier that only points at a record into the one the record
/// is resolved by: a PMCID into its DOI or PMID, a URL into whatever its page
/// names, or into a [`Identifier::Webpage`]. Every other identifier comes back
/// unchanged and costs nothing.
///
/// `zot add` calls this before the duplicate check, so the check runs against
/// the identifier the item will actually carry: an item added from a PMCID
/// through its DOI is found again by that DOI.
pub fn canonicalize(id: Identifier) -> Result<Identifier> {
    match id {
        Identifier::Pmcid(pmcid) => {
            let resolved = resolve_pmcid(&http_client()?, &pmcid)?;
            eprintln!("PMCID {pmcid} -> {}", resolved.display());
            Ok(resolved)
        }
        Identifier::Url(url) => resolve_url(&url),
        other => Ok(other),
    }
}

/// The DOI of a PMC article, else its PMID, from one idconv request.
fn resolve_pmcid(client: &Client, pmcid: &str) -> Result<Identifier> {
    let url = format!(
        "https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/\
         ?format=json&tool=zot&ids={}",
        urlencoding::encode(pmcid),
    );
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to look up PMCID {pmcid} at NCBI"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("PMC ID conversion for {pmcid} failed with status {status}");
    }
    let body = resp
        .text()
        .context("Failed to read the PMC ID conversion")?;
    idconv_identifier(&body, pmcid)
}

/// Pick the identifier out of an idconv response. Pure: the caller does the
/// HTTP.
///
/// The DOI wins over the PMID for the same reason as in
/// [`fetch_pubmed_bibtex`]: the publisher's BibTeX is richer. An unknown PMCID
/// is answered with 200 and a record carrying `status: error`.
fn idconv_identifier(body: &str, pmcid: &str) -> Result<Identifier> {
    let parsed: Value = serde_json::from_str(body)
        .with_context(|| format!("NCBI returned unparseable JSON for PMCID {pmcid}"))?;
    let record = parsed
        .pointer("/records/0")
        .with_context(|| format!("NCBI returned no record for PMCID {pmcid}"))?;
    if record.get("status").and_then(Value::as_str) == Some("error") {
        let message = record
            .get("errmsg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        bail!("PMC has no record for PMCID {pmcid}: {message}");
    }
    if let Some(doi) = record
        .get("doi")
        .and_then(Value::as_str)
        .filter(|d| d.starts_with("10."))
    {
        return Ok(Identifier::Doi(doi.to_string()));
    }
    // The PMID comes back as a number; accept a string too.
    let pmid = match record.get("pmid") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    match pmid.filter(|p| looks_like_pmid(p)) {
        Some(pmid) => Ok(Identifier::Pmid(pmid)),
        None => bail!(
            "PMCID {pmcid} carries neither a DOI nor a PubMed ID, so there is no record to \
             resolve it from."
        ),
    }
}

/// A page's identifier, else the page itself as a [`Identifier::Webpage`].
///
/// The page's own tags are read first, because they are authoritative. A DOI
/// in the URL path (`/doi/10.1145/...`, `/article/10.1007/...`) is the
/// fallback for a page that names none or cannot be read at all, which is
/// common: publishers answer non-browser clients with a bot wall.
fn resolve_url(url: &str) -> Result<Identifier> {
    let url_doi = super::webpage::doi_in_url(url);
    eprintln!("Reading {url} for its metadata...");
    let page = match super::webpage::fetch(url) {
        Ok(page) => page,
        Err(e) => {
            return match url_doi {
                Some(doi) => {
                    eprintln!("Could not read the page ({e:#}); using the DOI in its URL.");
                    Ok(Identifier::Doi(doi))
                }
                None => Err(e),
            };
        }
    };
    let meta = super::webpage::PageMeta::parse(&page.html);
    if let Some(id) = meta.identifier() {
        eprintln!("Found {} on the page.", id.display());
        return Ok(id);
    }
    if let Some(doi) = url_doi {
        eprintln!("Found DOI {doi} in the URL.");
        return Ok(Identifier::Doi(doi));
    }
    // A redirect can land on an identifier URL (a short link to arxiv.org).
    if page.final_url != url {
        if let Ok(id @ (Identifier::Doi(_) | Identifier::Arxiv(_) | Identifier::Pmid(_))) =
            parse_identifier(&page.final_url)
        {
            return Ok(id);
        }
        if let Ok(Identifier::Pmcid(pmcid)) = parse_identifier(&page.final_url) {
            return canonicalize(Identifier::Pmcid(pmcid));
        }
    }
    let bibtex = meta.to_bibtex(&page.final_url, &super::webpage::today())?;
    Ok(Identifier::Webpage {
        url: page.final_url,
        bibtex,
    })
}

/// Fetch BibTeX for an identifier from the corresponding external service.
pub fn fetch_bibtex(id: &Identifier) -> Result<String> {
    let client = http_client()?;

    match id {
        Identifier::Doi(doi) => fetch_doi_bibtex(&client, doi),
        Identifier::Arxiv(aid) => {
            // arxiv.org has a direct BibTeX export; fall back to the DataCite
            // DOI that arXiv assigns to every paper.
            match fetch_arxiv_bibtex(&client, aid) {
                Ok(bib) => Ok(bib),
                Err(_) => fetch_doi_bibtex(&client, &format!("10.48550/arXiv.{aid}")),
            }
        }
        Identifier::Isbn(isbn) => fetch_isbn_bibtex(&client, isbn),
        Identifier::Pmid(pmid) => fetch_pubmed_bibtex(&client, pmid),
        Identifier::Webpage { bibtex, .. } => Ok(bibtex.clone()),
        Identifier::Pmcid(_) | Identifier::Url(_) => fetch_bibtex(&canonicalize(id.clone())?),
    }
}

fn fetch_doi_bibtex(client: &Client, doi: &str) -> Result<String> {
    let url = format!("https://doi.org/{}", urlencoding::encode(doi));
    let resp = client
        .get(&url)
        .header("Accept", "application/x-bibtex; charset=utf-8")
        .send()
        .with_context(|| format!("Failed to resolve DOI {doi}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("DOI not found: {doi}");
    }
    if !status.is_success() {
        bail!("DOI resolution for {doi} failed with status {status}");
    }
    let body = resp.text().context("Failed to read DOI response")?;
    validate_bibtex(&body, doi)?;
    Ok(body)
}

fn fetch_arxiv_bibtex(client: &Client, arxiv_id: &str) -> Result<String> {
    let url = format!("https://arxiv.org/bibtex/{arxiv_id}");
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to fetch arXiv BibTeX for {arxiv_id}"))?;
    if !resp.status().is_success() {
        bail!("arXiv BibTeX export failed with status {}", resp.status());
    }
    let body = resp.text().context("Failed to read arXiv response")?;
    validate_bibtex(&body, arxiv_id)?;
    Ok(body)
}

/// One `efetch` call in MEDLINE form, then either the DOI it carries or a
/// BibTeX entry mapped from the MEDLINE text.
///
/// `tool=zot` is sent because NCBI asks callers to identify themselves; `email`
/// is not, because it would bake a personal address into every request an
/// open-source binary makes. The rate limit (3 requests/second anonymously) is
/// moot here: an add makes exactly one call.
fn fetch_pubmed_bibtex(client: &Client, pmid: &str) -> Result<String> {
    let url = format!(
        "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi\
         ?db=pubmed&rettype=medline&retmode=text&tool=zot&id={}",
        urlencoding::encode(pmid),
    );
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to fetch PubMed record for PMID {pmid}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("PubMed lookup for PMID {pmid} failed with status {status}");
    }
    let body = resp.text().context("Failed to read PubMed response")?;
    // An unknown PMID is answered with 200 and an empty body, so the status
    // alone never says "not found".
    if !body.contains("PMID-") {
        bail!("PubMed has no record for PMID {pmid}");
    }

    if let Some(doi) = medline_doi(&body) {
        // The publisher's own BibTeX is richer than anything MEDLINE carries.
        if let Ok(bib) = fetch_doi_bibtex(client, &doi) {
            return Ok(bib);
        }
    }
    medline_to_bibtex(&body, pmid)
}

/// OpenLibrary in one request. `jscmd=data` returns author names inline, which
/// the edition record behind `/isbn/<isbn>.json` does not.
fn fetch_isbn_bibtex(client: &Client, isbn: &str) -> Result<String> {
    let url =
        format!("https://openlibrary.org/api/books?format=json&jscmd=data&bibkeys=ISBN:{isbn}");
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to look up ISBN {isbn} at OpenLibrary"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("OpenLibrary lookup for ISBN {isbn} failed with status {status}");
    }
    let body = resp.text().context("Failed to read OpenLibrary response")?;
    openlibrary_to_bibtex(&body, isbn)
}

/// The DOI a MEDLINE record carries, from `LID` or `AID`, both of which mark it
/// with a trailing `[doi]`. Only those two tags are read: a `CIN` (comment-in)
/// line quotes another article's DOI in prose, and picking that up would import
/// the wrong paper.
fn medline_doi(text: &str) -> Option<String> {
    medline_fields(text)
        .into_iter()
        .filter(|(tag, _)| tag == "LID" || tag == "AID")
        .find_map(|(_, value)| {
            value
                .strip_suffix("[doi]")
                .map(str::trim)
                .filter(|d| d.starts_with("10."))
                .map(String::from)
        })
}

/// Split MEDLINE text into `(tag, value)` pairs. The tag occupies the first
/// four columns, column five is `-`, and a line starting with whitespace
/// continues the previous value (abstracts and addresses wrap at six spaces).
fn medline_fields(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = out.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() > 5 && bytes[4] == b'-' {
            if let (Some(tag), Some(value)) = (line.get(..4), line.get(5..)) {
                out.push((tag.trim().to_string(), value.trim().to_string()));
            }
        }
    }
    out
}

/// Map a MEDLINE record to a BibTeX `@article`. Pure: the caller does the HTTP.
///
/// Used only when the record has no DOI, since a DOI gets better BibTeX from
/// the publisher.
fn medline_to_bibtex(text: &str, pmid: &str) -> Result<String> {
    let fields = medline_fields(text);
    let first = |tag: &str| -> Option<String> {
        fields
            .iter()
            .find(|(t, _)| t == tag)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let all = |tag: &str| -> Vec<String> {
        fields
            .iter()
            .filter(|(t, _)| t == tag)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .collect()
    };

    // MEDLINE ends most titles with a full stop that BibTeX does not want.
    // `trim_end_matches` drops every trailing one, so a title ending in an
    // ellipsis loses all three; that is better than keeping a stray "..".
    let title = first("TI")
        .map(|t| t.trim_end_matches('.').trim().to_string())
        .filter(|t| !t.is_empty())
        .with_context(|| format!("PubMed record for PMID {pmid} has no title"))?;

    // FAU is the full name ("Benson, Dennis A"), already in BibTeX's
    // "Last, First" order; AU is the abbreviated form and only a fallback.
    let mut authors = all("FAU");
    if authors.is_empty() {
        authors = all("AU");
    }
    let journal = first("JT").or_else(|| first("TA"));
    // DP is free-form and compound ("2013 Jan", "1953 Apr 25").
    let year = first("DP").as_deref().and_then(find_year);

    let mut entry: Vec<(&str, String)> = Vec::new();
    entry.push(("title", title.clone()));
    if !authors.is_empty() {
        entry.push(("author", authors.join(" and ")));
    }
    if let Some(j) = journal {
        entry.push(("journal", j));
    }
    if let Some(y) = &year {
        entry.push(("year", y.clone()));
    }
    if let Some(v) = first("VI") {
        entry.push(("volume", v));
    }
    if let Some(i) = first("IP") {
        entry.push(("number", i));
    }
    if let Some(p) = first("PG") {
        entry.push(("pages", p));
    }
    if let Some(d) = medline_doi(text) {
        entry.push(("doi", d));
    }
    if let Some(a) = first("AB") {
        entry.push(("abstract", a));
    }
    // Zotero's BibTeX translator maps `note` onto the Extra field, which is
    // where a PMID belongs and what the duplicate guard reads.
    entry.push(("note", format!("PMID: {pmid}")));

    let key = citekey(
        authors.first().map(String::as_str),
        year.as_deref(),
        &title,
        pmid,
    );
    Ok(bibtex_entry("article", &key, &entry))
}

/// Map an OpenLibrary `/api/books?jscmd=data` response to a BibTeX `@book`.
/// Pure: the caller does the HTTP.
fn openlibrary_to_bibtex(body: &str, isbn: &str) -> Result<String> {
    let parsed: Value = serde_json::from_str(body)
        .with_context(|| format!("OpenLibrary returned unparseable JSON for ISBN {isbn}"))?;
    // An ISBN OpenLibrary does not know answers 200 with `{}`.
    let record = parsed.get(format!("ISBN:{isbn}")).with_context(|| {
        format!(
            "No book found for ISBN {isbn}. OpenLibrary \
             (openlibrary.org/api/books) has no record for it."
        )
    })?;

    let text = |key: &str| -> Option<String> {
        record
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    // `authors` and `publishers` are arrays of objects carrying a `name`.
    let names = |key: &str| -> Vec<String> {
        record
            .get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("name").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut title = text("title")
        .with_context(|| format!("OpenLibrary record for ISBN {isbn} has no title"))?;
    if let Some(subtitle) = text("subtitle") {
        title = format!("{}: {subtitle}", title.trim_end_matches(':').trim());
    }
    let authors = names("authors");
    // `publish_date` is free-form ("3 January 2017", "2018", "c1985").
    let year = text("publish_date").as_deref().and_then(find_year);

    let mut entry: Vec<(&str, String)> = Vec::new();
    entry.push(("title", title.clone()));
    if !authors.is_empty() {
        entry.push(("author", authors.join(" and ")));
    }
    if let Some(p) = names("publishers").first() {
        entry.push(("publisher", p.clone()));
    }
    if let Some(y) = &year {
        entry.push(("year", y.clone()));
    }
    if let Some(pages) = record.get("number_of_pages").and_then(Value::as_u64) {
        entry.push(("pages", pages.to_string()));
    }
    entry.push(("isbn", isbn.to_string()));

    let key = citekey(
        authors.first().map(String::as_str),
        year.as_deref(),
        &title,
        isbn,
    );
    Ok(bibtex_entry("book", &key, &entry))
}

/// The first plausible 4-digit year in a free-form date string.
fn find_year(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.windows(4).find_map(|w| {
        let ok = (w[0] == '1' || w[0] == '2') && w.iter().all(char::is_ascii_digit);
        ok.then(|| w.iter().collect::<String>())
    })
}

/// `lastnameYEARword`, restricted to ASCII alphanumerics so the key is always a
/// legal BibTeX citation key. Falls back to the identifier when the record has
/// no author, since a key is mandatory and `@book{,` does not parse.
pub(crate) fn citekey(
    first_author: Option<&str>,
    year: Option<&str>,
    title: &str,
    fallback: &str,
) -> String {
    let ascii = |s: &str| -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_lowercase()
    };
    // "Benson, Dennis A" (MEDLINE) and "Ian Goodfellow" (OpenLibrary) both
    // reduce to the surname.
    let last = first_author
        .map(|a| match a.split_once(',') {
            Some((surname, _)) => surname.to_string(),
            None => a.split_whitespace().last().unwrap_or(a).to_string(),
        })
        .map(|s| ascii(&s))
        .unwrap_or_default();
    let word = title
        .split_whitespace()
        .map(ascii)
        .find(|w| !w.is_empty())
        .unwrap_or_default();
    let stem = if last.is_empty() {
        ascii(fallback)
    } else {
        last
    };
    format!("{stem}{}{word}", year.unwrap_or_default())
}

/// Render a BibTeX entry in the shape Zotero's importer expects: a citation
/// key, then one TAB-indented `field = {value},` per line.
///
/// `url` is written verbatim, because Zotero's importer and `api::bibtex` both
/// read it raw: escaping the `_` or `%` of a URL would put a backslash into
/// the stored link. Only the braces and whitespace that would break the field
/// are percent-encoded, which leaves the URL equivalent.
pub(crate) fn bibtex_entry(kind: &str, key: &str, fields: &[(&str, String)]) -> String {
    let mut out = format!("@{kind}{{{key},\n");
    for (name, value) in fields {
        out.push('\t');
        out.push_str(name);
        out.push_str(" = {");
        if *name == "url" {
            out.push_str(
                &value
                    .trim()
                    .replace('{', "%7B")
                    .replace('}', "%7D")
                    .replace(' ', "%20"),
            );
        } else {
            out.push_str(&bibtex_escape(value));
        }
        out.push_str("},\n");
    }
    out.push_str("}\n");
    out
}

/// Escape every character that would otherwise end the field, start a comment
/// or unbalance the braces. Titles like "C{\\'a}" or "Cost & Time 50% $ #1 a_b"
/// come straight out of a catalogue, so nothing here can be assumed safe.
fn bibtex_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\textbackslash{}"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '&' | '%' | '$' | '#' | '_' => {
                out.push('\\');
                out.push(c);
            }
            '~' => out.push_str("\\textasciitilde{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            _ => out.push(c),
        }
    }
    out
}

fn validate_bibtex(body: &str, id: &str) -> Result<()> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with('@') {
        bail!(
            "Unexpected response resolving {id} (not BibTeX): {}",
            body.chars().take(200).collect::<String>().trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identifiers() {
        assert_eq!(
            parse_identifier("10.1145/3597503").unwrap(),
            Identifier::Doi("10.1145/3597503".into())
        );
        assert_eq!(
            parse_identifier("https://doi.org/10.1/x").unwrap(),
            Identifier::Doi("10.1/x".into())
        );
        assert_eq!(
            parse_identifier("arXiv:2401.12345v2").unwrap(),
            Identifier::Arxiv("2401.12345".into())
        );
        assert_eq!(
            parse_identifier("2401.12345").unwrap(),
            Identifier::Arxiv("2401.12345".into())
        );
        assert_eq!(
            parse_identifier("https://arxiv.org/abs/2401.12345").unwrap(),
            Identifier::Arxiv("2401.12345".into())
        );
        assert_eq!(
            parse_identifier("https://arxiv.org/pdf/2401.12345.pdf").unwrap(),
            Identifier::Arxiv("2401.12345".into())
        );
        assert!(parse_identifier("not-an-id").is_err());
    }

    // Captured live from efetch on 2026-09-16 (PMID 23193287), trimmed to the
    // tags the mapper reads plus the CIN line that quotes a foreign DOI.
    const MEDLINE: &str = concat!(
        "\n",
        "PMID- 23193287\n",
        "OWN - NLM\n",
        "IS  - 1362-4962 (Electronic)\n",
        "VI  - 41\n",
        "IP  - Database issue\n",
        "DP  - 2013 Jan\n",
        "TI  - GenBank.\n",
        "PG  - D36-42\n",
        // Synthesised, and placed ahead of LID on purpose: a genuinely
        // [doi]-marked value under a tag that is neither LID nor AID, so
        // dropping the tag filter would pick this one up first.
        "RIN - 10.1093/nar/gks9999 [doi]\n",
        "LID - 10.1093/nar/gks1195 [doi]\n",
        "AB  - GenBank(R) is a comprehensive database that contains publicly available \n",
        "      nucleotide sequences for almost 260 000 formally described species. Daily \n",
        "      data exchange ensures worldwide coverage.\n",
        "FAU - Benson, Dennis A\n",
        "AU  - Benson DA\n",
        "AD  - National Center for Biotechnology Information, Building 38A, Bethesda, MD \n",
        "      20894, USA.\n",
        "FAU - Cavanaugh, Mark\n",
        "AU  - Cavanaugh M\n",
        "TA  - Nucleic Acids Res\n",
        "JT  - Nucleic acids research\n",
        "CIN - Nature. 2013 Apr 25;496(7446):434. doi: 10.1038/496434b. PMID: 23619685\n",
    );

    // A record with neither FAU/AU nor any [doi]-marked tag, and a title full
    // of TeX metacharacters, which is what a catalogue hands over unasked.
    const MEDLINE_BARE: &str = concat!(
        "\n",
        "PMID- 13054692\n",
        "VI  - 171\n",
        "DP  - 1953 Apr 25\n",
        "TI  - Cost & schedule: 50% of $1M for {BIM} model_A #1.\n",
        "PG  - 737-8\n",
        "TA  - Nature\n",
        "AID - 0006-291X(75)90508-2 [pii]\n",
    );

    // Captured live from openlibrary.org/api/books?jscmd=data on 2026-09-16.
    const OPENLIBRARY: &str = r#"{"ISBN:9780262035613": {
        "url": "http://openlibrary.org/books/OL26455783M/Deep_Learning",
        "title": "Deep Learning",
        "authors": [{"name": "Ian Goodfellow"}, {"name": "Yoshua Bengio"},
                    {"name": "Aaron Courville"}],
        "number_of_pages": 800,
        "identifiers": {"isbn_13": ["9780262035613"]},
        "publishers": [{"name": "MIT Press"}],
        "publish_date": "3 January 2017"}}"#;

    // Same endpoint, hand-built rather than captured: the bibkey and the
    // publisher come from the live record for ISBN 9780000000002, but that
    // record is "The three voices of poetry" and does carry a publish_date and
    // a page count. Here the title and subtitle are synthesised to pack in
    // every TeX metacharacter, and `authors`, `publish_date` and
    // `number_of_pages` are left out to exercise the fallbacks.
    const OPENLIBRARY_BARE: &str = r#"{"ISBN:9780000000002": {
        "title": "Cost & Time",
        "subtitle": "100% of {what} #1 costs_now",
        "publishers": [{"name": "Deutscher Grenzverein"}]}}"#;

    #[test]
    fn isbn_check_digits_decide_what_is_an_isbn() {
        // ISBN-13 and ISBN-10 of the same book, bare, hyphenated and spaced.
        assert_eq!(
            normalize_isbn("9780262035613").as_deref(),
            Some("9780262035613")
        );
        assert_eq!(
            normalize_isbn("978-0-262-03561-3").as_deref(),
            Some("9780262035613")
        );
        assert_eq!(
            normalize_isbn("978 0 262 03561 3").as_deref(),
            Some("9780262035613")
        );
        assert_eq!(normalize_isbn("0262035618").as_deref(), Some("0262035618"));
        assert_eq!(
            normalize_isbn("0-262-03561-8").as_deref(),
            Some("0262035618")
        );

        // An X check digit counts as 10, and is normalised to uppercase.
        assert_eq!(normalize_isbn("043942089X").as_deref(), Some("043942089X"));
        assert_eq!(
            normalize_isbn("0-439-42089-x").as_deref(),
            Some("043942089X")
        );

        // Wrong check digit, either length.
        assert!(normalize_isbn("9780262035614").is_none());
        assert!(normalize_isbn("0262035619").is_none());
        // X anywhere but the last position is not a check digit.
        assert!(normalize_isbn("04X942089X").is_none());
        // A 13-digit number that is not an ISBN: a phone number, a timestamp,
        // an order reference. Accepting it would resolve the wrong thing.
        assert!(normalize_isbn("1234567890123").is_none());
        // Neither length.
        assert!(normalize_isbn("123456789").is_none());
        assert!(normalize_isbn("12345678901").is_none());
    }

    #[test]
    fn parses_the_new_identifier_forms() {
        for input in [
            "9780262035613",
            "978-0-262-03561-3",
            "978 0 262 03561 3",
            "ISBN:9780262035613",
            "isbn: 978-0-262-03561-3",
        ] {
            assert_eq!(
                parse_identifier(input).unwrap(),
                Identifier::Isbn("9780262035613".into()),
                "{input}"
            );
        }
        assert_eq!(
            parse_identifier("0-439-42089-x").unwrap(),
            Identifier::Isbn("043942089X".into())
        );

        for input in [
            "23193287",
            "PMID:23193287",
            "pmid: 23193287",
            "PubMed:23193287",
        ] {
            assert_eq!(
                parse_identifier(input).unwrap(),
                Identifier::Pmid("23193287".into()),
                "{input}"
            );
        }

        // A bare number is a PMID only up to MAX_PMID_DIGITS digits; beyond
        // that it has to be a valid ISBN or it is nothing at all.
        assert_eq!(parse_identifier("1").unwrap(), Identifier::Pmid("1".into()));
        assert!(parse_identifier("1234567890").is_err());
        assert!(parse_identifier("1234567890123").is_err());
        assert!(parse_identifier("0123456").is_err());
        // An explicit prefix says what the value is meant to be, so a bad one
        // gets an error about that type rather than the generic list.
        let err = parse_identifier("ISBN:9780262035614")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Not a valid ISBN"), "{err}");
        let err = parse_identifier("PMID:abc").unwrap_err().to_string();
        assert!(err.contains("Not a valid PubMed ID"), "{err}");
    }

    #[test]
    fn the_pmid_digit_limit_is_where_isbn_length_begins() {
        // MAX_PMID_DIGITS digits is still a PMID.
        assert_eq!(
            parse_identifier("999999999").unwrap(),
            Identifier::Pmid("999999999".into())
        );
        assert_eq!("999999999".len(), MAX_PMID_DIGITS);
        // One digit more is ISBN-10 length, so it is read as an ISBN or as
        // nothing: a valid ISBN-10 resolves, a bad check digit errors, and
        // neither outcome is a PMID.
        assert_eq!(
            parse_identifier("0262035618").unwrap(),
            Identifier::Isbn("0262035618".into())
        );
        // Ten nines pass the ISBN-10 check digit, so even that is a book
        // rather than a PMID.
        assert_eq!(
            parse_identifier("9999999999").unwrap(),
            Identifier::Isbn("9999999999".into())
        );
        assert!(parse_identifier("9999999998").is_err());
    }

    #[test]
    fn an_isbn_pasted_from_a_pdf_keeps_its_typographic_dashes() {
        // A copy-paste out of a PDF brings U+2010 HYPHEN or U+2013 EN DASH
        // rather than ASCII '-'.
        assert_eq!(
            normalize_isbn("978\u{2010}0\u{2010}262\u{2010}03561\u{2010}3").as_deref(),
            Some("9780262035613")
        );
        assert_eq!(
            normalize_isbn("978\u{2013}0\u{2013}262\u{2013}03561\u{2013}3").as_deref(),
            Some("9780262035613")
        );
        // Mixed with ASCII hyphens and spaces, as a wrapped line produces.
        assert_eq!(
            normalize_isbn("978\u{2010}0-262 03561\u{2013}3").as_deref(),
            Some("9780262035613")
        );
    }

    #[test]
    fn a_bare_isbn_with_a_bad_check_digit_says_so() {
        let err = parse_identifier("9781119287538").unwrap_err().to_string();
        assert!(err.contains("check digit does not match"), "{err}");
        let err = parse_identifier("026203561X").unwrap_err().to_string();
        assert!(err.contains("check digit does not match"), "{err}");
        // Nothing ISBN-shaped, so no hint about check digits.
        let err = parse_identifier("not-an-id").unwrap_err().to_string();
        assert!(!err.contains("check digit"), "{err}");
    }

    #[test]
    fn pmcids_and_urls_parse_without_a_request() {
        for input in [
            "PMC3531190",
            "pmc3531190",
            "PMCID: PMC3531190",
            "https://pmc.ncbi.nlm.nih.gov/articles/PMC3531190/",
            "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC3531190",
        ] {
            assert_eq!(
                parse_identifier(input).unwrap(),
                Identifier::Pmcid("PMC3531190".into()),
                "{input}"
            );
        }
        assert_eq!(
            parse_identifier("https://pubmed.ncbi.nlm.nih.gov/23193287/").unwrap(),
            Identifier::Pmid("23193287".into())
        );
        assert_eq!(
            parse_identifier("https://www.itcon.org/paper/2021/5").unwrap(),
            Identifier::Url("https://www.itcon.org/paper/2021/5".into())
        );
        assert_eq!(
            parse_identifier("www.example.com/post").unwrap(),
            Identifier::Url("https://www.example.com/post".into())
        );
        // Identifier URLs keep their own meaning.
        assert_eq!(
            parse_identifier("https://doi.org/10.1/x").unwrap(),
            Identifier::Doi("10.1/x".into())
        );
        assert_eq!(
            parse_identifier("https://arxiv.org/abs/2401.12345").unwrap(),
            Identifier::Arxiv("2401.12345".into())
        );
        assert!(parse_identifier("PMCID:12345").is_err());
        assert!(parse_identifier("PMC").is_err());
    }

    /// Captured from pmc.ncbi.nlm.nih.gov idconv on 2026-11-03.
    #[test]
    fn an_idconv_record_prefers_the_doi_and_reports_an_unknown_pmcid() {
        let found = r#"{"status":"ok","records":[{"doi":"10.1093/nar/gks1195",
            "pmcid":"PMC3531190","pmid":23193287,"requested-id":"PMC3531190"}]}"#;
        assert_eq!(
            idconv_identifier(found, "PMC3531190").unwrap(),
            Identifier::Doi("10.1093/nar/gks1195".into())
        );
        let no_doi = r#"{"records":[{"pmcid":"PMC1","pmid":123}]}"#;
        assert_eq!(
            idconv_identifier(no_doi, "PMC1").unwrap(),
            Identifier::Pmid("123".into())
        );
        let missing = r#"{"status":"ok","records":[{"pmcid":"PMC99999999",
            "status":"error","errmsg":"Identifier not found in PMC"}]}"#;
        let err = idconv_identifier(missing, "PMC99999999")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Identifier not found in PMC"), "{err}");
    }

    #[test]
    fn a_url_with_an_underscore_is_written_raw() {
        let bib = bibtex_entry("online", "k", &[("url", "https://x.org/a_b%20c".into())]);
        assert!(bib.contains("url = {https://x.org/a_b%20c}"), "{bib}");
    }

    #[test]
    fn the_new_variants_display_and_dedup_as_themselves() {
        let isbn = Identifier::Isbn("9780262035613".into());
        assert_eq!(isbn.display(), "ISBN 9780262035613");
        assert_eq!(isbn.dedup_query(), "9780262035613");
        let pmid = Identifier::Pmid("23193287".into());
        assert_eq!(pmid.display(), "PMID 23193287");
        assert_eq!(pmid.dedup_query(), "23193287");
    }

    #[test]
    fn a_medline_record_maps_to_a_bibtex_article() {
        let bib = medline_to_bibtex(MEDLINE, "23193287").unwrap();
        assert!(bib.starts_with("@article{benson2013genbank,\n"), "{bib}");
        assert!(bib.contains("\ttitle = {GenBank},\n"), "{bib}");
        // FAU, not the abbreviated AU, and already in "Last, First" order.
        assert!(
            bib.contains("\tauthor = {Benson, Dennis A and Cavanaugh, Mark},\n"),
            "{bib}"
        );
        // JT wins over TA.
        assert!(
            bib.contains("\tjournal = {Nucleic acids research},\n"),
            "{bib}"
        );
        // DP is compound: only the year belongs in the year field.
        assert!(bib.contains("\tyear = {2013},\n"), "{bib}");
        assert!(bib.contains("\tvolume = {41},\n"), "{bib}");
        assert!(bib.contains("\tnumber = {Database issue},\n"), "{bib}");
        assert!(bib.contains("\tpages = {D36-42},\n"), "{bib}");
        assert!(bib.contains("\tdoi = {10.1093/nar/gks1195},\n"), "{bib}");
        // The Extra field, which is where the duplicate guard looks.
        assert!(bib.contains("\tnote = {PMID: 23193287},\n"), "{bib}");
        assert!(bib.ends_with("}\n"), "{bib}");

        // The wrapped abstract is rejoined on one line, and the address block
        // that wraps the same way does not leak into it.
        assert!(
            bib.contains(
                "\tabstract = {GenBank(R) is a comprehensive database that contains publicly \
                 available nucleotide sequences for almost 260 000 formally described species. \
                 Daily data exchange ensures worldwide coverage.},\n"
            ),
            "{bib}"
        );
        assert!(!bib.contains("Bethesda"), "{bib}");
    }

    #[test]
    fn the_doi_comes_only_from_a_doi_marked_lid_or_aid() {
        assert_eq!(medline_doi(MEDLINE).as_deref(), Some("10.1093/nar/gks1195"));
        // The CIN line quotes the DOI of a commenting article in prose; taking
        // it would resolve to the wrong paper entirely.
        assert!(!medline_doi(MEDLINE).unwrap().contains("496434b"));
        // AID without the [doi] marker is a publisher item id, not a DOI.
        assert_eq!(medline_doi(MEDLINE_BARE), None);
        // Neither does a [doi]-marked value under any other tag count.
        assert!(!medline_doi(MEDLINE).unwrap().contains("gks9999"));
    }

    #[test]
    fn a_medline_record_without_authors_or_doi_still_maps() {
        let bib = medline_to_bibtex(MEDLINE_BARE, "13054692").unwrap();
        // No author line at all rather than an empty one, and the citekey falls
        // back to the PMID because there is no surname to build it from.
        assert!(!bib.contains("author = "), "{bib}");
        assert!(!bib.contains("doi = "), "{bib}");
        assert!(!bib.contains("journal = {Nature\n"), "{bib}");
        assert!(bib.starts_with("@article{130546921953cost,\n"), "{bib}");
        // TA is the fallback journal name when JT is missing.
        assert!(bib.contains("\tjournal = {Nature},\n"), "{bib}");

        // Every TeX metacharacter is escaped, so the braces stay balanced.
        assert!(
            bib.contains(
                "\ttitle = {Cost \\& schedule: 50\\% of \\$1M for \\{BIM\\} model\\_A \\#1},\n"
            ),
            "{bib}"
        );
        assert_eq!(
            bib.chars().filter(|c| *c == '{').count(),
            bib.chars().filter(|c| *c == '}').count(),
            "{bib}"
        );

        // A record with no title is not a record worth importing.
        assert!(medline_to_bibtex("PMID- 42\nVI  - 1\n", "42").is_err());
    }

    #[test]
    fn an_openlibrary_record_maps_to_a_bibtex_book() {
        let bib = openlibrary_to_bibtex(OPENLIBRARY, "9780262035613").unwrap();
        assert!(bib.starts_with("@book{goodfellow2017deep,\n"), "{bib}");
        assert!(bib.contains("\ttitle = {Deep Learning},\n"), "{bib}");
        assert!(
            bib.contains("\tauthor = {Ian Goodfellow and Yoshua Bengio and Aaron Courville},\n"),
            "{bib}"
        );
        assert!(bib.contains("\tpublisher = {MIT Press},\n"), "{bib}");
        // publish_date is free-form prose; only the year survives.
        assert!(bib.contains("\tyear = {2017},\n"), "{bib}");
        assert!(bib.contains("\tpages = {800},\n"), "{bib}");
        assert!(bib.contains("\tisbn = {9780262035613},\n"), "{bib}");
        assert!(bib.ends_with("}\n"), "{bib}");
    }

    #[test]
    fn an_openlibrary_record_without_authors_year_or_pages_still_maps() {
        let bib = openlibrary_to_bibtex(OPENLIBRARY_BARE, "9780000000002").unwrap();
        assert!(!bib.contains("author = "), "{bib}");
        assert!(!bib.contains("year = "), "{bib}");
        assert!(!bib.contains("pages = "), "{bib}");
        // No surname to key on, so the ISBN stands in.
        assert!(bib.starts_with("@book{9780000000002cost,\n"), "{bib}");
        // The subtitle joins the title, and both are escaped.
        assert!(
            bib.contains("\ttitle = {Cost \\& Time: 100\\% of \\{what\\} \\#1 costs\\_now},\n"),
            "{bib}"
        );
        assert_eq!(
            bib.chars().filter(|c| *c == '{').count(),
            bib.chars().filter(|c| *c == '}').count(),
            "{bib}"
        );
    }

    #[test]
    fn an_isbn_openlibrary_does_not_know_is_reported_as_not_found() {
        // OpenLibrary answers an unknown bibkey with 200 and `{}`, so the
        // status code never says "not found" and the body has to.
        let err = openlibrary_to_bibtex("{}", "9783161484100")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("No book found for ISBN 9783161484100"),
            "{err}"
        );
        assert!(err.contains("OpenLibrary"), "{err}");
        // A record filed under some other ISBN is not this book either.
        assert!(openlibrary_to_bibtex(OPENLIBRARY, "9780262035620").is_err());
        assert!(openlibrary_to_bibtex("not json", "9780262035613").is_err());
    }

    #[test]
    fn a_free_form_date_yields_its_year_or_nothing() {
        assert_eq!(find_year("3 January 2017").as_deref(), Some("2017"));
        assert_eq!(find_year("c1985").as_deref(), Some("1985"));
        assert_eq!(find_year("1953 Apr 25").as_deref(), Some("1953"));
        assert_eq!(find_year("2013 Jan").as_deref(), Some("2013"));
        assert_eq!(find_year("n.d."), None);
        assert_eq!(find_year(""), None);
    }
}
