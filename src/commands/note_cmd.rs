//! `zot note` -- write a note via the Zotero web API: a child note on a
//! regular item, or new content for an existing note. Markdown input is
//! converted to the HTML Zotero stores; the note syncs down to the local
//! library on the next Zotero sync.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;

use crate::api::WebApiClient;
use crate::output::{format_output, NoteOutput};

pub struct NoteArgs<'a> {
    pub key: &'a str,
    pub file: Option<&'a str>,
    pub text: Option<&'a str>,
    pub html: bool,
    pub tags: &'a [String],
    pub json: bool,
}

pub fn run_note(args: NoteArgs) -> Result<()> {
    let source = read_source(args.file, args.text)?;
    if source.trim().is_empty() {
        bail!("The note is empty");
    }
    let body = if args.html {
        source
    } else {
        markdown_to_html(&source)
    };

    let web = WebApiClient::from_config()?;
    let item = web.get_item(args.key)?;
    let data = item.get("data").context("Item JSON has no data")?;
    let item_type = data.get("itemType").and_then(Value::as_str).unwrap_or("");

    let output = match item_type {
        "note" => {
            let version = item
                .get("version")
                .and_then(Value::as_u64)
                .context("Note JSON has no version")?;
            let mut patch = json!({ "note": body });
            if !args.tags.is_empty() {
                patch["tags"] = Value::Array(merge_tags(data, args.tags));
            }
            web.patch_item(args.key, &patch, version)?;
            NoteOutput {
                note_key: args.key.to_string(),
                parent_key: data
                    .get("parentItem")
                    .and_then(Value::as_str)
                    .map(String::from),
                action: "replaced".to_string(),
                note:
                    "Note content replaced via web API; it updates locally on the next Zotero sync."
                        .to_string(),
            }
        }
        "attachment" | "annotation" => bail!(
            "{} is an {item_type}: Zotero only allows child notes on regular items. \
             Use the parent item key{}",
            args.key,
            data.get("parentItem")
                .and_then(Value::as_str)
                .map(|p| format!(" ({p})"))
                .unwrap_or_default()
        ),
        _ => {
            let tags: Vec<Value> = args.tags.iter().map(|t| json!({ "tag": t })).collect();
            let note_key = web.create_item(&json!({
                "itemType": "note",
                "parentItem": args.key,
                "note": body,
                "tags": tags,
            }))?;
            NoteOutput {
                note_key,
                parent_key: Some(args.key.to_string()),
                action: "created".to_string(),
                note: "Created via web API; the note appears locally on the next Zotero sync."
                    .to_string(),
            }
        }
    };
    println!("{}", format_output(&output, args.json));
    Ok(())
}

/// The note text from `--file` (a path, or `-` for stdin) or `--text`.
fn read_source(file: Option<&str>, text: Option<&str>) -> Result<String> {
    match (file, text) {
        (Some(_), Some(_)) => bail!("Give the note as --file or --text, not both"),
        (None, None) => bail!("Give the note as --file PATH (or - for stdin) or --text TEXT"),
        (None, Some(t)) => Ok(t.to_string()),
        (Some("-"), None) => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("Failed to read the note from stdin")?;
            Ok(s)
        }
        (Some(path), None) => {
            std::fs::read_to_string(path).with_context(|| format!("Failed to read {path}"))
        }
    }
}

/// Markdown to the HTML a Zotero note stores (tables and strikethrough on).
pub fn markdown_to_html(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut out = String::new();
    html::push_html(&mut out, Parser::new_ext(md, opts));
    out
}

/// The note's current tags plus `add`, without duplicates.
fn merge_tags(data: &Value, add: &[String]) -> Vec<Value> {
    let mut tags: Vec<Value> = data
        .get("tags")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for t in add {
        let present = tags
            .iter()
            .any(|v| v.get("tag").and_then(Value::as_str) == Some(t.as_str()));
        if !present {
            tags.push(json!({ "tag": t }));
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_note_html() {
        let html = markdown_to_html("# Title\n\nSome *text*.\n\n- one\n- two\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<em>text</em>"));
        assert!(html.contains("<li>one</li>"));
    }

    #[test]
    fn markdown_tables_are_rendered() {
        let html = markdown_to_html("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn source_needs_exactly_one_input() {
        assert!(read_source(None, None).is_err());
        assert!(read_source(Some("x"), Some("y")).is_err());
        assert_eq!(read_source(None, Some("hi")).unwrap(), "hi");
    }

    #[test]
    fn tags_merge_without_duplicates() {
        let data = json!({ "tags": [{ "tag": "review" }] });
        let tags = merge_tags(&data, &["review".to_string(), "jc".to_string()]);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[1]["tag"], "jc");
    }
}
