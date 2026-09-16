//! `zot edit` -- update item metadata via the Zotero web API.
//!
//! The local API is read-only, so edits go through api.zotero.org and sync
//! back to the desktop app (usually within seconds with auto-sync on).

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use crate::api::connector::{ConnectorClient, SaveTarget};
use crate::api::{WebApiClient, ZoteroClient};
use crate::collections::{CollectionNode, build_tree, resolve_collection_ref};
use crate::output::{EditOutput, format_output};

pub struct EditArgs<'a> {
    pub key: &'a str,
    pub sets: &'a [String],
    pub add_tags: &'a [String],
    pub rm_tags: &'a [String],
    pub add_collections: &'a [String],
    pub rm_collections: &'a [String],
    pub patch: Option<&'a str>,
    pub json: bool,
}

pub fn run_edit(args: EditArgs) -> Result<()> {
    let EditArgs {
        key,
        sets,
        add_tags,
        rm_tags,
        add_collections,
        rm_collections,
        patch,
        json,
    } = args;

    if sets.is_empty()
        && add_tags.is_empty()
        && rm_tags.is_empty()
        && add_collections.is_empty()
        && rm_collections.is_empty()
        && patch.is_none()
    {
        bail!(
            "Nothing to change. Use --set field=value, --add-tag, --rm-tag, \
             --add-collection, --rm-collection, or --patch <json>."
        );
    }

    // Resolve every collection reference before touching the library, so a typo
    // in the second value fails before the first one is written.
    let collection_edit = if add_collections.is_empty() && rm_collections.is_empty() {
        None
    } else {
        Some(ResolvedCollections::resolve(add_collections, rm_collections)?)
    };

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

    // One read, whose version guards the write: tag and collection edits are
    // full-array replacements computed from this snapshot, so the PATCH must be
    // rejected if anything landed on the item after it.
    let tags_change = !add_tags.is_empty() || !rm_tags.is_empty();
    let item = web.get_item(key)?;
    let version = item
        .get("version")
        .and_then(|v| v.as_u64())
        .context("No version on item")?;

    if tags_change {
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

    // Collection membership is a plain array of collection keys.
    let mut collections_change: Option<String> = None;
    let mut collections_noop: Option<String> = None;
    if let Some(edit) = collection_edit.as_ref() {
        let current: Vec<String> = item
            .pointer("/data/collections")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let merged = merge_collections(&current, &edit.add, &edit.rm);
        if merged == current {
            // A no-op must not look like a write: skip the field entirely, and
            // say so rather than reporting "collections" as changed. It is
            // still a success -- the requested membership holds.
            collections_noop = Some(format!(
                "Collection membership already matches the request, left untouched: {}. \
                 Nothing was written.",
                edit.render(&current),
            ));
        } else {
            collections_change = Some(format!(
                "collections: {} -> {}",
                edit.render(&current),
                edit.render(&merged),
            ));
            data.insert(
                "collections".to_string(),
                Value::Array(merged.iter().map(|k| json!(k)).collect()),
            );
        }
    }

    if data.is_empty() {
        // An idempotent request the library already satisfies succeeds: report
        // the untouched item rather than failing a well-formed instruction.
        if let Some(note) = collections_noop {
            let output = EditOutput {
                key: key.to_string(),
                version,
                changed: Vec::new(),
                note,
            };
            println!("{}", format_output(&output, json));
            return Ok(());
        }
        bail!("Nothing was written: the requested change is already the item's state.");
    }

    let changed_fields: Vec<String> = data
        .keys()
        .map(|field| match (field.as_str(), collections_change.as_ref()) {
            ("collections", Some(detail)) => detail.clone(),
            _ => field.clone(),
        })
        .collect();
    let new_version = web.patch_item(key, &Value::Object(data), version)?;

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

/// Collection references resolved to keys, plus the key -> name map needed to
/// print membership readably.
#[derive(Debug)]
struct ResolvedCollections {
    add: Vec<String>,
    rm: Vec<String>,
    names: HashMap<String, String>,
}

impl ResolvedCollections {
    /// Resolve every `--add-collection` / `--rm-collection` value up front.
    ///
    /// Anything [`resolve_collection_ref`] accepts works: a collection key, an
    /// exact name, or a connector tree-view ID.
    fn resolve(add: &[String], rm: &[String]) -> Result<Self> {
        let client = ZoteroClient::new()?;
        let nodes = build_tree(&client.fetch_collections()?);

        // The connector is only needed for tree-view IDs and may be down
        // (Zotero closed, port taken), which must not break key/name refs.
        let targets = match ConnectorClient::new().and_then(|c| c.list_targets()) {
            Ok(targets) => targets,
            Err(e) => {
                eprintln!(
                    "Warning: connector unavailable, so tree-view IDs cannot be resolved \
                     (a key or exact name still resolves): {e:#}"
                );
                Vec::new()
            }
        };

        Self::from_parts(add, rm, &nodes, &targets)
    }

    /// The pure half of [`Self::resolve`]: no network, so it is unit-testable.
    fn from_parts(
        add: &[String],
        rm: &[String],
        nodes: &[CollectionNode],
        targets: &[SaveTarget],
    ) -> Result<Self> {
        let names: HashMap<String, String> = nodes
            .iter()
            .map(|n| (n.key.clone(), n.name.clone()))
            .collect();

        let resolve_all = |inputs: &[String], flag: &str| -> Result<Vec<String>> {
            let mut keys = Vec::new();
            for input in inputs {
                let resolved = resolve_collection_ref(input, nodes, targets)?;
                let Some(key) = resolved.key else {
                    let id = resolved.connector_id.as_deref().unwrap_or(input);
                    if id.starts_with('L') {
                        bail!(
                            "{flag} {input}: {id} is the library root, not a collection.\n  \
                             An item sits in the library root by being in no collection at all, \
                             so name a real collection instead."
                        );
                    }
                    bail!(
                        "{flag} {input}: collection \"{}\" ({id}) is only known to the connector, \
                         most likely because it lives in a group library, which this command \
                         cannot edit.",
                        resolved.name,
                    );
                };
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            Ok(keys)
        };

        let add = resolve_all(add, "--add-collection")?;
        let rm = resolve_all(rm, "--rm-collection")?;
        if let Some(both) = add.iter().find(|k| rm.contains(k)) {
            let name = names.get(both).cloned().unwrap_or_else(|| both.clone());
            bail!("Collection {name} ({both}) is both added and removed in the same call.");
        }
        Ok(Self { add, rm, names })
    }

    /// `[Name (KEY), Other (KEY2)]`, or `[]` for an item in no collection.
    fn render(&self, keys: &[String]) -> String {
        let rendered: Vec<String> = keys
            .iter()
            .map(|k| match self.names.get(k) {
                Some(name) => format!("{name} ({k})"),
                None => k.clone(),
            })
            .collect();
        format!("[{}]", rendered.join(", "))
    }
}

/// Merge collection membership: drop everything in `rm`, then append each entry
/// of `add` that is not already present. The order of surviving entries is
/// preserved, so an unrelated edit does not reshuffle the array.
///
/// Shared with `zot add`, which files the collections the connector could not
/// take through the same web API write.
pub(super) fn merge_collections(current: &[String], add: &[String], rm: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = current
        .iter()
        .filter(|c| !rm.contains(c))
        .cloned()
        .collect();
    for key in add {
        if !merged.contains(key) {
            merged.push(key.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{ResolvedCollections, merge_collections};
    use crate::api::connector::SaveTarget;
    use crate::api::models::ZoteroCollection;
    use crate::collections::{CollectionNode, build_tree};
    use serde_json::json;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn collection(key: &str, name: &str, parent: Option<&str>) -> ZoteroCollection {
        serde_json::from_value(json!({
            "key": key,
            "version": 1,
            "meta": { "numItems": 0, "numCollections": 0 },
            "data": {
                "key": key,
                "name": name,
                "parentCollection": match parent {
                    Some(p) => json!(p),
                    None => json!(false),
                },
            },
        }))
        .expect("collection fixture")
    }

    fn target(id: &str, name: &str, level: u32) -> SaveTarget {
        serde_json::from_value(json!({ "id": id, "name": name, "level": level }))
            .expect("target fixture")
    }

    /// A tree plus the connector targets that mirror it, with one extra target
    /// (`C9`, under the group library `L2`) that no key backs.
    fn resolvable() -> (Vec<CollectionNode>, Vec<SaveTarget>) {
        let nodes = build_tree(&[
            collection("ROOT1", "Alpha", None),
            collection("CHILD1", "Inbox", Some("ROOT1")),
            collection("ROOT2", "Beta", None),
        ]);
        let targets = vec![
            target("L1", "My Library", 0),
            target("C1", "Alpha", 1),
            target("C2", "Inbox", 2),
            target("C3", "Beta", 1),
            target("L2", "Group Library", 0),
            target("C9", "Group Only", 1),
        ];
        (nodes, targets)
    }

    fn resolved(add: &[&str], rm: &[&str]) -> anyhow::Result<ResolvedCollections> {
        let (nodes, targets) = resolvable();
        ResolvedCollections::from_parts(&v(add), &v(rm), &nodes, &targets)
    }

    #[test]
    fn adding_and_removing_the_same_collection_is_rejected() {
        // The same collection reached two ways: by key on one flag, by name on
        // the other.
        let err = resolved(&["CHILD1"], &["Inbox"]).unwrap_err().to_string();
        assert!(err.contains("both added and removed"), "{err}");
        assert!(err.contains("Inbox (CHILD1)"), "{err}");
    }

    #[test]
    fn library_root_is_rejected() {
        let err = resolved(&["L1"], &[]).unwrap_err().to_string();
        assert!(err.contains("is the library root, not a collection"), "{err}");
    }

    #[test]
    fn connector_only_collection_is_rejected() {
        let err = resolved(&[], &["Group Only"]).unwrap_err().to_string();
        assert!(err.contains("only known to the connector"), "{err}");
        assert!(err.contains("group library"), "{err}");
    }

    #[test]
    fn a_repeated_value_yields_one_key() {
        let r = resolved(&["CHILD1", "Inbox", "C2"], &[]).unwrap();
        assert_eq!(r.add, v(&["CHILD1"]));
        assert!(r.rm.is_empty());
    }

    #[test]
    fn render_names_known_keys_and_falls_back_to_the_bare_key() {
        let r = resolved(&["CHILD1"], &[]).unwrap();
        assert_eq!(
            r.render(&v(&["CHILD1", "ROOT2"])),
            "[Inbox (CHILD1), Beta (ROOT2)]"
        );
        assert_eq!(r.render(&[]), "[]");
        assert_eq!(r.render(&v(&["ZZZZZZZZ"])), "[ZZZZZZZZ]");
    }

    #[test]
    fn add_to_empty_membership() {
        assert_eq!(merge_collections(&[], &v(&["AAA"]), &[]), v(&["AAA"]));
        assert_eq!(
            merge_collections(&[], &v(&["AAA", "BBB"]), &[]),
            v(&["AAA", "BBB"])
        );
    }

    #[test]
    fn adding_a_duplicate_is_a_no_op() {
        let current = v(&["AAA", "BBB"]);
        assert_eq!(merge_collections(&current, &v(&["AAA"]), &[]), current);
    }

    #[test]
    fn remove_one_of_several() {
        let current = v(&["AAA", "BBB", "CCC"]);
        assert_eq!(
            merge_collections(&current, &[], &v(&["BBB"])),
            v(&["AAA", "CCC"])
        );
    }

    #[test]
    fn removing_a_non_member_is_a_no_op() {
        let current = v(&["AAA", "BBB"]);
        assert_eq!(merge_collections(&current, &[], &v(&["ZZZ"])), current);
    }

    #[test]
    fn add_and_remove_in_one_call() {
        let current = v(&["AAA", "BBB"]);
        assert_eq!(
            merge_collections(&current, &v(&["CCC"]), &v(&["AAA"])),
            v(&["BBB", "CCC"])
        );
    }

    #[test]
    fn existing_order_is_preserved_and_additions_append() {
        let current = v(&["CCC", "AAA", "BBB"]);
        assert_eq!(
            merge_collections(&current, &v(&["AAA", "DDD"]), &[]),
            v(&["CCC", "AAA", "BBB", "DDD"])
        );
    }

    #[test]
    fn removing_everything_leaves_an_empty_array() {
        let current = v(&["AAA", "BBB"]);
        assert!(merge_collections(&current, &[], &v(&["AAA", "BBB"])).is_empty());
    }
}
