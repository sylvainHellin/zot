use anyhow::{Context, Result, bail};
use std::panic;
use std::path::Path;
use std::process::Command;

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

/// Extract text from a PDF, isolated in a memory/CPU-capped child process.
///
/// `pdf_extract` can allocate unbounded memory (observed: 15+ GB) or loop on
/// malformed PDFs, and OOM is not catchable with `catch_unwind`. Running the
/// extraction in a subprocess under `ulimit` means a pathological PDF kills only
/// the child; the parent recovers with a per-item warning and keeps indexing.
pub fn extract_text(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)
        .context(format!("Failed to stat PDF: {}", path.display()))?;
    if meta.len() > MAX_PDF_BYTES {
        bail!(
            "PDF too large ({} MB > {} MB limit), skipping",
            meta.len() / (1024 * 1024),
            MAX_PDF_BYTES / (1024 * 1024)
        );
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
        // Child was capped/killed (OOM, CPU limit, panic, malformed PDF).
        bail!("{}", describe_exit_failure(&output));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Build a human-readable reason for a failed extraction child.
fn describe_exit_failure(output: &std::process::Output) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = output.status.signal() {
            // SIGKILL/SIGSEGV/SIGABRT here typically mean the memory or CPU cap
            // was hit on a pathological PDF.
            return format!(
                "PDF extraction killed by signal {sig} (too large/malformed — likely hit memory or CPU limit)"
            );
        }
    }
    // Non-zero exit: the worker reported a clean extraction error on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .last()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .unwrap_or("malformed PDF");
    format!("PDF extraction failed: {detail}")
}

/// Worker run inside the capped child process (the hidden `__extract-pdf`
/// subcommand). Reads the PDF, extracts and cleans text, writes it to stdout.
/// Returns an error (non-zero exit) on failure so the parent records a warning.
pub fn run_extract_worker(path: &Path) -> Result<()> {
    // Silence pdf-extract's panic spew (e.g. "missing unicode map and
    // encoding"); we convert the panic into a clean error below.
    panic::set_hook(Box::new(|_| {}));

    let bytes = std::fs::read(path)
        .context(format!("Failed to read PDF: {}", path.display()))?;

    let result = panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes));

    let text = match result {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => bail!("PDF extraction error: {e}"),
        Err(_) => bail!("PDF extraction panicked (malformed PDF)"),
    };

    // Normalize whitespace, drop null bytes and blank lines.
    let mut cleaned = text
        .replace('\0', "")
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if cleaned.len() > MAX_FULLTEXT_BYTES {
        // Truncate on a char boundary so the UTF-8 stays valid.
        let mut end = MAX_FULLTEXT_BYTES;
        while end > 0 && !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
    }

    use std::io::Write;
    std::io::stdout()
        .write_all(cleaned.as_bytes())
        .context("Failed to write extracted text")?;

    Ok(())
}
