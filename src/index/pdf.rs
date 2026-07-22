use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::panic;
use std::path::Path;
use std::process::Command;

use oxidize_pdf::parser::{PdfDocument, PdfReader};
use oxidize_pdf::text::TextExtractor;

/// Hidden subcommand name used to run extraction in an isolated child process.
pub const EXTRACT_SUBCOMMAND: &str = "__extract-pdf";

/// Skip PDFs larger than this. Such files are almost always scanned books that
/// yield poor text and are the main memory-blowup risk.
const MAX_PDF_BYTES: u64 = 150 * 1024 * 1024; // 150 MB

/// Virtual-memory cap (KB) for the extraction child (~3 GB). A runaway PDF hits
/// an allocation failure in the child instead of OOM-killing the whole indexer.
const EXTRACT_MEM_LIMIT_KB: u64 = 3_000_000;

/// CPU-time cap (seconds) for the extraction child; catches PDFs that spin.
const EXTRACT_CPU_LIMIT_SECS: u64 = 120;

/// Cap on extracted text kept per document (~2 MB ≈ a 1000-page book). Beyond
/// this we truncate: it bounds chunk count, embedding batches, and the bytes
/// piped back to the parent, so a single document can never blow up memory.
const MAX_FULLTEXT_BYTES: usize = 2 * 1024 * 1024;

/// Below this many characters per page (averaged over the document) an
/// extraction that otherwise "succeeded" is flagged `suspicious`: oxidize-pdf
/// can silently under-extract, and that is the failure mode search cares about.
/// Only applied to documents with more than [`SUSPICIOUS_MIN_PAGES`] pages, so
/// short notes and cover pages are not falsely flagged.
const SUSPICIOUS_CHARS_PER_PAGE: usize = 200;
const SUSPICIOUS_MIN_PAGES: usize = 2;

/// Coarse per-item extraction status, persisted alongside the version
/// checkpoint. Drives reporting and the retry decision on the next run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtractStatus {
    /// Every page extracted and the output volume looks plausible.
    Ok,
    /// The item has no PDF child attachment. Not an error; only retried when the
    /// item's Zotero version changes.
    NoAttachment,
    /// The document would not open, or zero pages yielded text.
    Failed,
    /// Some pages extracted and some failed; the extracted pages are indexed.
    Partial,
    /// Every page nominally extracted but the output is implausibly small for
    /// the page count (likely silent under-extraction).
    Suspicious,
}

impl ExtractStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtractStatus::Ok => "ok",
            ExtractStatus::NoAttachment => "no-attachment",
            ExtractStatus::Failed => "failed",
            ExtractStatus::Partial => "partial",
            ExtractStatus::Suspicious => "suspicious",
        }
    }
}

/// Structured result of an extraction attempt for one item. Returned by the
/// parent-side extraction wrapper and recorded in the index store.
#[derive(Debug, Clone)]
pub struct ExtractOutcome {
    pub status: ExtractStatus,
    /// Extracted (cleaned) text. Empty for `NoAttachment` and `Failed`.
    pub text: String,
    /// Human-readable detail: the warning shown to the user and stored for
    /// `zot index issues`. Empty when there is nothing to report (`Ok`).
    pub detail: String,
}

/// What the isolated worker child reports back to the parent as JSON on stdout.
/// The parent turns this into an [`ExtractOutcome`], applying the suspicious
/// heuristic and the final truncation.
#[derive(Debug, Serialize, Deserialize)]
struct WorkerReport {
    /// Total pages in the document.
    page_count: u32,
    /// 1-based page numbers that failed to extract (panic or parse error).
    failed_pages: Vec<u32>,
    /// Cleaned, joined text of the pages that did extract.
    text: String,
    /// Set when the document itself would not open (no page could be read).
    document_error: Option<String>,
}

/// Extract text from a PDF, isolated in a memory/CPU-capped child process.
///
/// The extractor (oxidize-pdf) can allocate large amounts of memory or loop on
/// malformed PDFs, and OOM is not catchable with `catch_unwind`. Running the
/// extraction in a subprocess under `ulimit` means a pathological PDF kills only
/// the child; the parent recovers with a per-item warning and keeps indexing.
///
/// Returns a structured [`ExtractOutcome`]: the caller decides how to index and
/// report based on the status.
pub fn extract_text(path: &Path) -> Result<ExtractOutcome> {
    let meta = std::fs::metadata(path)
        .context(format!("Failed to stat PDF: {}", path.display()))?;
    if meta.len() > MAX_PDF_BYTES {
        return Ok(ExtractOutcome {
            status: ExtractStatus::Failed,
            text: String::new(),
            detail: format!(
                "malformed PDF: too large ({} MB > {} MB limit)",
                meta.len() / (1024 * 1024),
                MAX_PDF_BYTES / (1024 * 1024)
            ),
        });
    }

    let exe = std::env::current_exe().context("Failed to locate zot executable")?;

    // sh -c '<limits>; exec "$0" __extract-pdf "$1"'  <exe>  <path>
    //   $0 = exe, $1 = path. ulimit failures are tolerated (2>/dev/null) so the
    //   extraction still runs (just unbounded) on shells/platforms without it.
    let script = format!(
        "ulimit -v {mem} 2>/dev/null; ulimit -t {cpu} 2>/dev/null; exec \"$0\" {sub} \"$1\"",
        mem = EXTRACT_MEM_LIMIT_KB,
        cpu = EXTRACT_CPU_LIMIT_SECS,
        sub = EXTRACT_SUBCOMMAND,
    );

    let output = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .arg(&exe)
        .arg(path)
        .output()
        .context("Failed to spawn PDF extraction subprocess")?;

    if !output.status.success() {
        // Child was capped/killed (OOM, CPU limit, or a crash before it could
        // emit a report). Treat as a failed extraction, not a hard error, so
        // indexing continues with a per-item warning.
        return Ok(ExtractOutcome {
            status: ExtractStatus::Failed,
            text: String::new(),
            detail: describe_exit_failure(&output),
        });
    }

    let report: WorkerReport = serde_json::from_slice(&output.stdout)
        .context("Failed to parse extraction worker report")?;

    Ok(build_outcome(report))
}

/// Turn a worker report into a final outcome, applying truncation and the
/// suspicious-under-extraction heuristic.
fn build_outcome(report: WorkerReport) -> ExtractOutcome {
    let WorkerReport {
        page_count,
        failed_pages,
        mut text,
        document_error,
    } = report;

    // Document would not open at all.
    if let Some(err) = document_error {
        return ExtractOutcome {
            status: ExtractStatus::Failed,
            text: String::new(),
            detail: format!("malformed PDF: {err}"),
        };
    }

    // Truncate on a char boundary so the UTF-8 stays valid.
    if text.len() > MAX_FULLTEXT_BYTES {
        let mut end = MAX_FULLTEXT_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }

    let extracted_pages = page_count.saturating_sub(failed_pages.len() as u32);

    // Every page failed (or there were no pages) and nothing came out: failed.
    if text.trim().is_empty() || extracted_pages == 0 {
        let reason = if failed_pages.is_empty() {
            "no extractable text".to_string()
        } else {
            format!("all {page_count} pages failed to extract")
        };
        return ExtractOutcome {
            status: ExtractStatus::Failed,
            text: String::new(),
            detail: format!("malformed PDF: {reason}"),
        };
    }

    // Some pages failed but others produced text: partial.
    if !failed_pages.is_empty() {
        let detail = format!(
            "extracted {}/{} pages (failed: {})",
            extracted_pages,
            page_count,
            format_page_list(&failed_pages),
        );
        return ExtractOutcome {
            status: ExtractStatus::Partial,
            text,
            detail,
        };
    }

    // All pages extracted. Guard against silent under-extraction.
    let chars = text.chars().count();
    if page_count as usize > SUSPICIOUS_MIN_PAGES
        && chars < SUSPICIOUS_CHARS_PER_PAGE * page_count as usize
    {
        let per_page = chars / page_count.max(1) as usize;
        return ExtractOutcome {
            status: ExtractStatus::Suspicious,
            text,
            detail: format!(
                "low text volume: {chars} chars over {page_count} pages (~{per_page}/page)"
            ),
        };
    }

    ExtractOutcome {
        status: ExtractStatus::Ok,
        text,
        detail: String::new(),
    }
}

/// Render a list of 1-based page numbers compactly, capping at a few entries.
fn format_page_list(pages: &[u32]) -> String {
    const MAX_SHOWN: usize = 8;
    if pages.len() <= MAX_SHOWN {
        pages
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        let head = pages[..MAX_SHOWN]
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{head}, +{} more", pages.len() - MAX_SHOWN)
    }
}

/// Build a human-readable reason for a child that exited without a clean report
/// (killed by the memory/CPU cap, or crashed).
fn describe_exit_failure(output: &std::process::Output) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = output.status.signal() {
            // SIGKILL/SIGSEGV/SIGABRT here typically mean the memory or CPU cap
            // was hit on a pathological PDF.
            return format!(
                "malformed PDF: extraction killed by signal {sig} (likely hit memory or CPU limit)"
            );
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .last()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .unwrap_or("extraction failed");
    format!("malformed PDF: {detail}")
}

/// Worker run inside the capped child process (the hidden `__extract-pdf`
/// subcommand). Reads the PDF, extracts text page by page (each page wrapped in
/// `catch_unwind` so one bad font does not lose the whole document), cleans
/// whitespace, and writes a JSON [`WorkerReport`] to stdout.
pub fn run_extract_worker(path: &Path) -> Result<()> {
    // Silence any panic spew from the extractor; per-page panics are converted
    // into failed-page entries below.
    panic::set_hook(Box::new(|_| {}));

    let bytes = std::fs::read(path)
        .context(format!("Failed to read PDF: {}", path.display()))?;

    let report = extract_report(bytes);

    let json = serde_json::to_vec(&report).context("Failed to serialize extraction report")?;
    use std::io::Write;
    std::io::stdout()
        .write_all(&json)
        .context("Failed to write extraction report")?;

    Ok(())
}

/// Do the actual per-page extraction inside the worker. Never panics out: a
/// document that will not open is reported via `document_error`, and per-page
/// panics/errors are collected into `failed_pages`.
fn extract_report(bytes: Vec<u8>) -> WorkerReport {
    // Opening the document (parse header/xref/page tree) can itself panic on a
    // malformed PDF, so wrap it too.
    let opened = panic::catch_unwind(move || {
        let reader = PdfReader::new(Cursor::new(bytes))?;
        let document = PdfDocument::new(reader);
        let page_count = document.page_count()?;
        Ok::<_, oxidize_pdf::error::PdfError>((document, page_count))
    });

    let (document, page_count) = match opened {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            return WorkerReport {
                page_count: 0,
                failed_pages: Vec::new(),
                text: String::new(),
                document_error: Some(e.to_string()),
            };
        }
        Err(_) => {
            return WorkerReport {
                page_count: 0,
                failed_pages: Vec::new(),
                text: String::new(),
                document_error: Some("document failed to open (panic)".to_string()),
            };
        }
    };

    let mut page_texts: Vec<String> = Vec::new();
    let mut failed_pages: Vec<u32> = Vec::new();

    for i in 0..page_count {
        // A fresh extractor per page keeps a panic on one page from corrupting
        // extractor state used by the next.
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let mut extractor = TextExtractor::new();
            extractor.extract_from_page(&document, i).map(|t| t.text)
        }));
        match result {
            Ok(Ok(text)) => page_texts.push(text),
            Ok(Err(_)) | Err(_) => failed_pages.push(i + 1), // 1-based for humans
        }
    }

    let joined = page_texts.join("\n");
    let cleaned = clean_text(&joined);

    WorkerReport {
        page_count,
        failed_pages,
        text: cleaned,
        document_error: None,
    }
}

/// Normalize whitespace, drop null bytes and blank lines.
fn clean_text(text: &str) -> String {
    text.replace('\0', "")
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(pages: u32, failed: &[u32], text: &str) -> WorkerReport {
        WorkerReport {
            page_count: pages,
            failed_pages: failed.to_vec(),
            text: text.to_string(),
            document_error: None,
        }
    }

    #[test]
    fn ok_when_all_pages_extract_enough_text() {
        let body = "word ".repeat(1000); // ~5000 chars over 3 pages
        let outcome = build_outcome(report(3, &[], &body));
        assert_eq!(outcome.status, ExtractStatus::Ok);
        assert!(outcome.detail.is_empty());
        assert!(!outcome.text.is_empty());
    }

    #[test]
    fn failed_when_document_will_not_open() {
        let r = WorkerReport {
            page_count: 0,
            failed_pages: Vec::new(),
            text: String::new(),
            document_error: Some("bad xref".to_string()),
        };
        let outcome = build_outcome(r);
        assert_eq!(outcome.status, ExtractStatus::Failed);
        assert!(outcome.detail.contains("malformed PDF"));
        assert!(outcome.text.is_empty());
    }

    #[test]
    fn failed_when_all_pages_fail() {
        let outcome = build_outcome(report(3, &[1, 2, 3], ""));
        assert_eq!(outcome.status, ExtractStatus::Failed);
        assert!(outcome.text.is_empty());
    }

    #[test]
    fn partial_when_some_pages_fail() {
        let body = "word ".repeat(500);
        let outcome = build_outcome(report(29, &[9, 24], &body));
        assert_eq!(outcome.status, ExtractStatus::Partial);
        assert!(outcome.detail.contains("27/29"));
        assert!(outcome.detail.contains("9, 24"));
        assert!(!outcome.text.is_empty());
    }

    #[test]
    fn suspicious_when_under_extracted() {
        // 10 pages, tiny text: far below 200 chars/page.
        let outcome = build_outcome(report(10, &[], "a tiny bit of text here"));
        assert_eq!(outcome.status, ExtractStatus::Suspicious);
        assert!(outcome.detail.contains("low text volume"));
    }

    #[test]
    fn short_document_not_flagged_suspicious() {
        // 2 pages is at/under the min-pages threshold: never suspicious.
        let outcome = build_outcome(report(2, &[], "short note"));
        assert_eq!(outcome.status, ExtractStatus::Ok);
    }

    #[test]
    fn page_list_caps_long_lists() {
        let pages: Vec<u32> = (1..=20).collect();
        let s = format_page_list(&pages);
        assert!(s.contains("+12 more"));
    }
}
