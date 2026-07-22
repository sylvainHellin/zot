use std::panic;
use std::path::Path;

use dom_smoothie::{Article, Config, Readability, TextMode};

use super::pdf::{ExtractOutcome, ExtractStatus};

/// Skip HTML snapshots larger than this on disk. Full web-page snapshots are
/// usually tens to a few hundred KB; a multi-megabyte file is either a
/// media-heavy archive or an inlined data blob, where readability parsing is
/// slow and rarely improves the result. The whole-body strip fallback still
/// runs on a truncated copy (see [`MAX_HTML_BYTES_FOR_STRIP`]).
const MAX_HTML_BYTES: u64 = 12 * 1024 * 1024; // 12 MB

/// Hard cap on characters kept from the whole-body strip fallback, so a huge
/// snapshot that defeats readability cannot blow up chunk count / embeddings.
const MAX_STRIP_CHARS: usize = 2 * 1024 * 1024;

/// Cap on HTML elements dom_smoothie parses, bounding memory and time on a
/// pathological document. Ordinary article snapshots are well under this.
const MAX_ELEMENTS_TO_PARSE: usize = 500_000;

/// Below this many characters, a readability extraction is treated as
/// implausible (nav-only or a stub) and we fall back to stripping the whole
/// body. Also the threshold under which a whole-page snapshot is flagged
/// `suspicious`: a real article is comfortably longer than this.
const MIN_PLAUSIBLE_CHARS: usize = 250;

/// Extract readable article text from a locally stored Zotero HTML snapshot.
///
/// Snapshots are full web pages (nav, sidebars, footers, cookie banners). We
/// run the pure-Rust readability port (dom_smoothie, a Readability.js port) to
/// isolate the main article, then take its plain-text rendering. If readability
/// fails or returns implausibly little text, we fall back to stripping HTML tags
/// over the whole body and note that in the detail.
///
/// Parsing runs in-process: dom_smoothie is pure Rust over a bounded DOM
/// (`max_elements_to_parse`), with a file-size cap and `catch_unwind` guarding
/// against a panic on malformed markup. A bad snapshot degrades to the strip
/// fallback or a `failed` status; it never takes down the indexer.
pub fn extract_snapshot(path: &Path, source: &str) -> ExtractOutcome {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return failed(format!("{source}: cannot stat file: {e}"));
        }
    };
    if meta.len() > MAX_HTML_BYTES {
        return failed(format!(
            "{source}: too large ({} MB > {} MB limit)",
            meta.len() / (1024 * 1024),
            MAX_HTML_BYTES / (1024 * 1024)
        ));
    }

    let html = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            // Some snapshots are saved in a non-UTF-8 encoding; recover a
            // lossy UTF-8 view rather than failing outright.
            match std::fs::read(path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => return failed(format!("{source}: unreadable file: {e}")),
            }
        }
    };

    if html.trim().is_empty() {
        return failed(format!("{source}: empty file"));
    }

    build_outcome(&html, source)
}

/// Run readability with the strip fallback and classify the result. Split out
/// from I/O so it is unit-testable on raw HTML.
fn build_outcome(html: &str, source: &str) -> ExtractOutcome {
    let readable = readability_text(html);

    match readable {
        Some(text) if text.chars().count() >= MIN_PLAUSIBLE_CHARS => ExtractOutcome {
            status: ExtractStatus::Ok,
            text,
            detail: format!("{source}: extracted (readability)"),
        },
        _ => {
            // Readability failed or returned too little: strip the whole body.
            let stripped = strip_and_cap(html);
            if stripped.chars().count() < MIN_PLAUSIBLE_CHARS {
                return ExtractOutcome {
                    status: ExtractStatus::Suspicious,
                    text: stripped.clone(),
                    detail: format!(
                        "{source}: low text volume ({} chars, fallback strip)",
                        stripped.chars().count()
                    ),
                };
            }
            ExtractOutcome {
                status: ExtractStatus::Ok,
                text: stripped,
                detail: format!("{source}: extracted (fallback strip)"),
            }
        }
    }
}

/// Run dom_smoothie readability over the snapshot, returning the article's
/// plain-text content. Returns `None` if readability errors or panics; the
/// caller then falls back to the whole-body strip.
fn readability_text(html: &str) -> Option<String> {
    // dom_smoothie is pure Rust but can panic on adversarial markup; contain it
    // so a single bad snapshot degrades to the strip fallback, not a crash.
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let cfg = Config {
            text_mode: TextMode::Formatted,
            max_elements_to_parse: MAX_ELEMENTS_TO_PARSE,
            ..Default::default()
        };
        let mut readability = Readability::new(html, None, Some(cfg)).ok()?;
        let article: Article = readability.parse().ok()?;
        Some(article.text_content.to_string())
    }));

    match result {
        Ok(Some(text)) => {
            let cleaned = clean_text(&text);
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        }
        Ok(None) | Err(_) => None,
    }
}

/// Whole-body HTML-to-text fallback: strip tags, drop `<script>`/`<style>`
/// contents, decode a small entity set, collapse whitespace, and cap length.
/// Mirrors the note strip in `index_cmd` but also skips script/style bodies,
/// which snapshots always carry and notes never do.
fn strip_and_cap(html: &str) -> String {
    let text = strip_html(html);
    if text.chars().count() > MAX_STRIP_CHARS {
        text.chars().take(MAX_STRIP_CHARS).collect()
    } else {
        text
    }
}

/// Strip HTML to text, dropping the contents of `<script>` and `<style>`
/// elements entirely (a raw snapshot is full of both). Decodes common entities
/// and collapses whitespace.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        if bytes[i] == b'<' {
            // Skip script/style blocks wholesale, including their text content.
            if let Some(skip_to) = skip_raw_block(&lower, i, "script")
                .or_else(|| skip_raw_block(&lower, i, "style"))
            {
                i = skip_to;
                out.push(' ');
                continue;
            }
            // Otherwise skip just the tag.
            match html[i..].find('>') {
                Some(rel) => {
                    i += rel + 1;
                    out.push(' ');
                }
                None => break, // unterminated tag: stop
            }
        } else {
            // Copy this UTF-8 char.
            let ch = html[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// If an opening `<tag ...>` starts at `pos` in `lower`, return the byte index
/// just past its matching `</tag>` (or end of input). Used to drop the entire
/// body of `<script>`/`<style>`.
fn skip_raw_block(lower: &str, pos: usize, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    if !lower[pos..].starts_with(&open) {
        return None;
    }
    // Confirm this is `<tag` followed by whitespace, `>`, or `/` (not e.g.
    // `<styles>`).
    let after = lower[pos + open.len()..].chars().next();
    match after {
        Some(c) if c.is_whitespace() || c == '>' || c == '/' => {}
        None => {}
        _ => return None,
    }
    let close = format!("</{tag}>");
    match lower[pos..].find(&close) {
        Some(rel) => Some(pos + rel + close.len()),
        None => Some(lower.len()), // unterminated: drop the rest
    }
}

/// Decode the handful of HTML entities common in article bodies. Single
/// left-to-right pass so decoded output is never rescanned.
fn decode_entities(s: &str) -> String {
    const ENTITIES: [(&str, &str); 8] = [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&mdash;", "-"),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        match ENTITIES.iter().find(|(e, _)| rest.starts_with(e)) {
            Some((entity, replacement)) => {
                out.push_str(replacement);
                rest = &rest[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Normalize whitespace and drop null bytes / blank lines from readability text.
fn clean_text(text: &str) -> String {
    text.replace('\0', "")
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn failed(detail: String) -> ExtractOutcome {
    ExtractOutcome {
        status: ExtractStatus::Failed,
        text: String::new(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readability_extracts_article_body() {
        let html = r#"<html><head><title>My Post</title></head><body>
            <nav>Home About Contact</nav>
            <article><h1>Real Title</h1>
            <p>This is the first substantial paragraph of the article body. It is long
            enough to be picked up by the readability scorer as the main content, well
            past the two hundred and fifty character plausibility threshold that guards
            against nav-only extractions. More sentences here to add weight to it.</p>
            <p>A second paragraph continues the article with additional detail so the
            candidate clearly wins over the surrounding navigation and footer noise.</p>
            </article>
            <footer>Copyright 2026</footer></body></html>"#;
        let outcome = build_outcome(html, "html snapshot");
        assert_eq!(outcome.status, ExtractStatus::Ok);
        assert!(outcome.detail.contains("readability"));
        assert!(outcome.text.contains("first substantial paragraph"));
        // Nav/footer boilerplate should be dropped by readability.
        assert!(!outcome.text.contains("Home About Contact"));
    }

    #[test]
    fn falls_back_to_strip_when_readability_finds_little() {
        // All body text sits in elements readability discards as boilerplate
        // (footer with an unlikely-candidate class), so readability yields
        // nothing plausible and we strip the whole body instead. The stripped
        // text is long enough to be plausible.
        let body = "word ".repeat(200);
        let html = format!(
            "<html><body><footer class=\"footer\">{body}</footer>\
             <script>var x = 1; ignore_this_script_text();</script>\
             <style>.a {{ color: red; }}</style></body></html>"
        );
        let outcome = build_outcome(&html, "html snapshot");
        assert_eq!(outcome.status, ExtractStatus::Ok);
        assert!(outcome.detail.contains("fallback strip"));
        // Script/style contents must not leak into the text.
        assert!(!outcome.text.contains("ignore_this_script_text"));
        assert!(!outcome.text.contains("color: red"));
        assert!(outcome.text.contains("word"));
    }

    #[test]
    fn suspicious_when_no_plausible_text_anywhere() {
        let html = "<html><body><nav>Menu</nav></body></html>";
        let outcome = build_outcome(html, "html snapshot");
        assert_eq!(outcome.status, ExtractStatus::Suspicious);
        assert!(outcome.detail.contains("low text volume"));
    }

    #[test]
    fn strip_html_drops_script_and_style_and_decodes() {
        let html = "<p>Tom &amp; Jerry</p><script>bad()</script><style>x{}</style><p>end</p>";
        let text = strip_html(html);
        assert!(text.contains("Tom & Jerry"));
        assert!(text.contains("end"));
        assert!(!text.contains("bad()"));
        assert!(!text.contains("x{}"));
    }
}
