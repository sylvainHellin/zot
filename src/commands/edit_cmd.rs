//! `zot edit` -- update item metadata via the Zotero web API.
//!
//! The local API is read-only, so edits go through api.zotero.org and sync
//! back to the desktop app (usually within seconds with auto-sync on).

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::api::WebApiClient;
use crate::output::{EditOutput, format_output};

pub fn run_edit(
    key: &str,
    sets: &[String],
    add_tags: &[String],
    rm_tags: &[String],
    patch: Option<&str>,
    json: bool,
) -> Result<()> {
    if sets.is_empty() && add_tags.is_empty() && rm_tags.is_empty() && patch.is_none() {
        bail!(
            "Nothing to change. Use --set field=value, --add-tag, --rm-tag, or --patch <json>."
        );
    }

    let web = WebApiClient::from_config()?;

    // Start from --patch JSON (arbitrary fields, e.g. creators), then layer
    // --set pairs on top.
    let mut data: Map<String, Value> = match patch {
        Some(raw) => serde_json::from_str::<Value>(raw)
            .context("--patch is not valid JSON")?
            .as_object()
            .cloned()
            .context("--patch must be a JSON object")?,
        None => Map::new(),
    };

    for pair in sets {
        let Some((field, value)) = pair.split_once('=') else {
            bail!("--set expects field=value, got: {pair}");
        };
        data.insert(field.trim().to_string(), json!(value));
    }

    // Tag changes need the current tag list.
    if !add_tags.is_empty() || !rm_tags.is_empty() {
        let item = web.get_item(key)?;
        let mut tags: Vec<String> = item
            .pointer("/data/tags")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("tag").and_then(|s| s.as_str()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        tags.retain(|t| !rm_tags.contains(t));
        for t in add_tags {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
        let tag_objs: Vec<Value> = tags.iter().map(|t| json!({ "tag": t })).collect();
        data.insert("tags".to_string(), Value::Array(tag_objs));
    }

    let changed_fields: Vec<String> = data.keys().cloned().collect();
    let new_version = web.patch_item(key, &Value::Object(data))?;

    let output = EditOutput {
        key: key.to_string(),
        version: new_version,
        changed: changed_fields,
        note: "Updated via web API; the change reaches the local library on the next Zotero sync."
            .to_string(),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}
