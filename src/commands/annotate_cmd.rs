//! `zot annotate` -- create a highlight annotation on a PDF attachment via the
//! Zotero web API, locating the passage by its first and last words.
//!
//! The PDF is read from local storage (the same resolution as `zot pdf`), the
//! passage and its word boxes are found with poppler (see [`crate::annotate`]),
//! and the annotation item is POSTed to api.zotero.org; it reaches the local
//! library on the next Zotero sync.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

use crate::annotate::{self, Passage, TO_WINDOW};
use crate::api::models::ZoteroItem;
use crate::api::{WebApiClient, ZoteroClient};
use crate::output::{format_output, AnnotateOutput};

/// Zotero's default highlight yellow.
pub const DEFAULT_COLOR: &str = "#ffd400";

pub struct AnnotateArgs<'a> {
    pub key: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub comment: Option<&'a str>,
    pub color: &'a str,
    pub tags: &'a [String],
    /// 1-based physical page restricting the search.
    pub page: Option<usize>,
    pub dry_run: bool,
    pub json: bool,
}

pub fn run_annotate(args: AnnotateArgs) -> Result<()> {
    let color = normalize_color(args.color)?;
    if args.page == Some(0) {
        bail!("--page is 1-based: the first page is 1.");
    }
    let page = args.page.map(|p| p - 1);

    // Fail on a missing key before the PDF work, not after it.
    let web = if args.dry_run {
        None
    } else {
        Some(WebApiClient::from_config()?)
    };

    let client = ZoteroClient::new()?;
    let attachment = resolve_pdf_attachment(&client, args.key)?;
    let path = client
        .get_attachment_path(&attachment.key)?
        .with_context(|| {
            format!(
                "No local file for PDF attachment {}. Sync Zotero (or let it download the file) \
                 and retry.",
                attachment.key
            )
        })?;
    let pdf = Path::new(&path);

    if let Some(p) = page {
        let count = annotate::page_count(pdf)?;
        if p >= count {
            bail!(
                "--page {} is past the end of the PDF, which has {count} page(s).",
                p + 1
            );
        }
    }

    let pages = annotate::extract_words(pdf, page)?;
    if pages.iter().all(|p| p.words.is_empty()) {
        bail!(
            "No text layer in {} (a scan without OCR?), so there is nothing to match.",
            pdf.display()
        );
    }
    let passage = annotate::locate(&pages, args.from, args.to, TO_WINDOW, page)?;
    let geometry = annotate::page_geometry(pdf, passage.page_index)?;
    let page_label = annotate::page_label(pdf, passage.page_index);

    let rects: Vec<[f64; 4]> = passage
        .device_rects
        .iter()
        .map(|r| annotate::round3(geometry.device_to_user(*r)))
        .collect();
    let sort_index = annotate::sort_index(
        passage.page_index,
        passage.char_offset,
        geometry.sort_top(&rects),
    );

    let item = annotation_item(
        &attachment.key,
        &passage,
        &page_label,
        &sort_index,
        &rects,
        args.comment.unwrap_or(""),
        &color,
        args.tags,
    );

    let mut output = AnnotateOutput {
        key: None,
        attachment_key: attachment.key.clone(),
        page_index: passage.page_index,
        page_label,
        text: passage.text.clone(),
        comment: args.comment.unwrap_or("").to_string(),
        color,
        tags: args.tags.to_vec(),
        sort_index,
        rects,
        dry_run: args.dry_run,
        note: "Dry run: nothing was created.".to_string(),
    };

    if let Some(web) = web {
        let key = web.create_item(&item).map_err(|e| {
            e.context(format!(
                "Creating the annotation on attachment {} failed. If the attachment was added \
                 moments ago, it may not have synced up yet: sync Zotero and retry.",
                attachment.key
            ))
        })?;
        output.key = Some(key);
        output.note = "Created via web API; the highlight appears in the local library on the \
                       next Zotero sync."
            .to_string();
    }

    println!("{}", format_output(&output, args.json));
    Ok(())
}

/// The PDF attachment behind `key`: the item itself when it is a PDF
/// attachment, else the single PDF among its children.
fn resolve_pdf_attachment(client: &ZoteroClient, key: &str) -> Result<ZoteroItem> {
    let item = client.fetch_item(key)?;
    if item.data.item_type == "attachment" {
        if item.data.content_type != "application/pdf" {
            bail!(
                "Attachment {key} is not a PDF (content type \"{}\").",
                item.data.content_type
            );
        }
        return Ok(item);
    }
    if !item.is_regular_item() {
        bail!(
            "{key} is a {}, not a PDF attachment or a parent item.",
            item.data.item_type
        );
    }
    let mut pdfs: Vec<ZoteroItem> = client
        .fetch_children(key)?
        .into_iter()
        .filter(|c| c.data.item_type == "attachment" && c.data.content_type == "application/pdf")
        .collect();
    match pdfs.len() {
        0 => bail!("Item {key} has no PDF attachment."),
        1 => Ok(pdfs.remove(0)),
        _ => {
            let listed: Vec<String> = pdfs
                .iter()
                .map(|p| {
                    let filename = p
                        .data
                        .extra
                        .get("filename")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    format!("  {}  {}  {filename}", p.key, p.data.title)
                })
                .collect();
            bail!(
                "Item {key} has {} PDF attachments; pass the attachment key instead:\n{}",
                pdfs.len(),
                listed.join("\n")
            );
        }
    }
}

/// `#RRGGBB`, lowercased, with or without the leading `#` on input.
fn normalize_color(color: &str) -> Result<String> {
    let hex = color.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("--color expects a hex colour like #ffd400, got \"{color}\".");
    }
    Ok(format!("#{}", hex.to_ascii_lowercase()))
}

/// The annotation item as the web API takes it. `annotationPosition` is a
/// JSON string, not an object, holding the page index and the rects in PDF
/// user space.
#[allow(clippy::too_many_arguments)]
fn annotation_item(
    attachment_key: &str,
    passage: &Passage,
    page_label: &str,
    sort_index: &str,
    rects: &[[f64; 4]],
    comment: &str,
    color: &str,
    tags: &[String],
) -> Value {
    let position = json!({ "pageIndex": passage.page_index, "rects": rects });
    json!({
        "itemType": "annotation",
        "parentItem": attachment_key,
        "annotationType": "highlight",
        "annotationText": passage.text,
        "annotationComment": comment,
        "annotationColor": color,
        "annotationPageLabel": page_label,
        "annotationSortIndex": sort_index,
        "annotationPosition": position.to_string(),
        "tags": tags.iter().map(|t| json!({ "tag": t })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_validated_and_normalized() {
        assert_eq!(normalize_color("#FFD400").unwrap(), "#ffd400");
        assert_eq!(normalize_color("a28ae5").unwrap(), "#a28ae5");
        assert!(normalize_color("#fff").is_err());
        assert!(normalize_color("yellow").is_err());
    }

    #[test]
    fn annotation_item_carries_the_position_as_a_json_string() {
        let passage = Passage {
            page_index: 2,
            first_word: 0,
            last_word: 1,
            text: "two words".into(),
            char_offset: 0,
            device_rects: vec![],
        };
        let item = annotation_item(
            "ABCD1234",
            &passage,
            "iii",
            "00002|000000|00064",
            &[[54.0, 718.0, 100.0, 728.0]],
            "a comment",
            "#ffd400",
            &["t1".to_string()],
        );
        assert_eq!(item["parentItem"], "ABCD1234");
        assert_eq!(item["annotationType"], "highlight");
        assert_eq!(item["tags"], json!([{ "tag": "t1" }]));
        let position: Value =
            serde_json::from_str(item["annotationPosition"].as_str().unwrap()).unwrap();
        assert_eq!(
            position,
            json!({ "pageIndex": 2, "rects": [[54.0, 718.0, 100.0, 728.0]] })
        );
    }
}
