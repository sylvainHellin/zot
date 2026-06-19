use anyhow::{Context, Result, bail};
use std::path::Path;
use std::panic;

/// Extract text content from a PDF file.
/// Catches panics from pdf-extract (some PDFs have malformed fonts/encodings).
pub fn extract_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).context(format!("Failed to read PDF: {}", path.display()))?;

    // pdf-extract can panic on malformed PDFs, so catch panics
    let result = panic::catch_unwind(|| {
        pdf_extract::extract_text_from_mem(&bytes)
    });

    let text = match result {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => bail!("PDF extraction error: {e}"),
        Err(_) => bail!("PDF extraction panicked (malformed PDF)"),
    };

    // Clean up extracted text: normalize whitespace, remove null bytes
    let cleaned = text
        .replace('\0', "")
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(cleaned)
}
