use anyhow::Result;

use crate::api::ZoteroClient;
use crate::output::{format_output, PdfOutput};

pub fn run_pdf(key: &str, json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;
    let children = client.fetch_children(key)?;

    let mut pdf_path: Option<String> = None;
    for child in &children {
        if child.data.content_type == "application/pdf" {
            if let Some(path) = client.get_attachment_path(&child.key)? {
                pdf_path = Some(path);
                break;
            }
        }
    }

    let output = PdfOutput {
        key: key.to_string(),
        path: pdf_path,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
