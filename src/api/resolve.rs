//! Resolve external identifiers (DOI, arXiv) to BibTeX for import.
//!
//! DOIs go through doi.org content negotiation (Crossref/DataCite/mEDRA all
//! honor `Accept: application/x-bibtex`). arXiv IDs use arxiv.org's BibTeX
//! export endpoint, falling back to the arXiv DataCite DOI
//! (`10.48550/arXiv.<id>`).

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;

#[derive(Debug, Clone, PartialEq)]
pub enum Identifier {
    Doi(String),
    Arxiv(String),
}

impl Identifier {
    /// The string used for the duplicate check against the library.
    pub fn dedup_query(&self) -> &str {
        match self {
            Identifier::Doi(d) => d,
            Identifier::Arxiv(id) => id,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Identifier::Doi(d) => format!("DOI {d}"),
            Identifier::Arxiv(id) => format!("arXiv:{id}"),
        }
    }
}

/// Parse a user-supplied identifier. Accepts bare DOIs, doi.org URLs,
/// `arXiv:ID`, bare arXiv IDs (new-style `2401.12345`), and arxiv.org URLs.
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

    // Prefixed forms
    if let Some(rest) = s.strip_prefix("doi:").or_else(|| s.strip_prefix("DOI:")) {
        return Ok(Identifier::Doi(rest.trim().to_string()));
    }
    let lower = s.to_lowercase();
    if let Some(rest) = lower.strip_prefix("arxiv:") {
        return Ok(Identifier::Arxiv(strip_arxiv_version(rest.trim())));
    }

    // Bare forms
    if s.starts_with("10.") && s.contains('/') {
        // The arXiv DataCite DOI is still a DOI; fine either way.
        return Ok(Identifier::Doi(s.to_string()));
    }
    if looks_like_arxiv_id(s) {
        return Ok(Identifier::Arxiv(strip_arxiv_version(s)));
    }

    bail!(
        "Could not recognize identifier: {input}\n  \
         Supported: DOI (10.xxxx/...), doi.org URL, arXiv ID (2401.12345), arxiv.org URL.\n  \
         Other identifier types (ISBN, PubMed, plain URLs) are on the roadmap."
    );
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

fn strip_arxiv_version(s: &str) -> String {
    if let Some(pos) = s.rfind('v') {
        if pos > 0 && s[pos + 1..].chars().all(|c| c.is_ascii_digit()) && !s[pos + 1..].is_empty() {
            return s[..pos].to_string();
        }
    }
    s.to_string()
}

/// Fetch BibTeX for an identifier from the corresponding external service.
pub fn fetch_bibtex(id: &Identifier) -> Result<String> {
    let client = Client::builder()
        .user_agent(format!("zot/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")?;

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

}
