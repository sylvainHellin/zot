//! `zot attach` -- attach a file to an existing item via the Zotero web API
//! (create attachment item, then the 3-step storage upload). The file syncs
//! down to the local library on the next Zotero sync.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::path::Path;

use crate::api::WebApiClient;
use crate::output::{AttachOutput, format_output};

pub fn run_attach(key: &str, file: &str, title: Option<&str>, json: bool) -> Result<()> {
    let path = Path::new(file);
    if !path.is_file() {
        bail!("File not found: {file}");
    }
    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .context("Invalid filename")?;
    let content_type = guess_content_type(filename);
    let title = title.unwrap_or(filename);

    let web = WebApiClient::from_config()?;

    // Verify the parent exists server-side (clear error if not synced yet).
    let parent = web.get_item(key)?;
    let parent_title = parent
        .pointer("/data/title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    // 1. Create the attachment item.
    let items = json!([{
        "itemType": "attachment",
        "linkMode": "imported_file",
        "parentItem": key,
        "title": title,
        "contentType": content_type,
        "filename": filename,
    }]);
    let resp = web.create_items(&items)?;
    let attachment_key = resp
        .pointer("/successful/0/key")
        .or_else(|| resp.pointer("/success/0"))
        .and_then(|k| k.as_str())
        .context("No attachment key in creation response")?
        .to_string();

    // 2+3. Upload the file bytes to Zotero storage.
    eprintln!("Uploading {filename} ({} KB)...", path.metadata()?.len() / 1024);
    web.upload_attachment_file(&attachment_key, path)?;

    let output = AttachOutput {
        key: key.to_string(),
        parent_title,
        attachment_key,
        filename: filename.to_string(),
        note: "Attached via web API; the file appears locally on the next Zotero sync."
            .to_string(),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}

fn guess_content_type(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    match lower.rsplit('.').next().unwrap_or("") {
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "html" | "htm" => "text/html",
        "txt" | "md" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}
