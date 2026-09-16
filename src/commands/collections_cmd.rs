use anyhow::{Result, bail};
use std::collections::HashSet;

use crate::api::ZoteroClient;
use crate::api::connector::ConnectorClient;
use crate::collections::{
    CollectionNode, attach_connector_ids, build_tree, resolve_collection_ref, roll_up_counts,
};
use crate::output::{CollectionOutput, CollectionsOutput, format_output};

/// List the collection tree, or one subtree of it.
///
/// `collection` accepts anything `resolve_collection_ref` does: a key, an exact
/// name, or a connector tree-view ID.
pub fn run_collections(
    collection: Option<&str>,
    flat: bool,
    tree_ids: bool,
    json: bool,
) -> Result<()> {
    let client = ZoteroClient::new()?;
    let mut nodes = build_tree(&client.fetch_collections()?);
    let items = client.fetch_top_items()?;
    roll_up_counts(&mut nodes, &items);

    // The connector is only needed to name or show tree-view IDs, and it may be
    // unreachable (Zotero closed, or the connector port taken), which must not
    // break a plain listing.
    let targets = if tree_ids || collection.is_some() {
        match ConnectorClient::new().and_then(|c| c.list_targets()) {
            Ok(targets) => targets,
            Err(e) => {
                if tree_ids {
                    eprintln!("Warning: connector unavailable, tree-view IDs omitted: {e:#}");
                } else {
                    eprintln!(
                        "Warning: connector unavailable, so tree-view IDs cannot be resolved \
                         (a key or exact name still resolves): {e:#}"
                    );
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    if tree_ids {
        attach_connector_ids(&mut nodes, &targets);
    }

    let (listed, root_key) = match collection {
        Some(wanted) => {
            let resolved = resolve_collection_ref(wanted, &nodes, &targets)?;
            let Some(key) = resolved.key else {
                let id = resolved.connector_id.as_deref().unwrap_or(wanted);
                if id.starts_with('L') {
                    bail!(
                        "{id} is the library root, not a collection.\n  \
                         Run `zot collections` with no argument for the whole tree."
                    );
                }
                bail!(
                    "Collection \"{}\" ({id}) is only known to the connector, most likely because \
                     it lives in a group library, which this command cannot list.",
                    resolved.name,
                );
            };
            (subtree(&nodes, &key), Some(key))
        }
        None => (nodes.iter().collect::<Vec<_>>(), None),
    };

    let base_depth = listed.first().map(|n| n.depth).unwrap_or(0);
    let in_scope: HashSet<&str> = listed.iter().map(|n| n.key.as_str()).collect();
    // Distinct items, since an item filed in two listed collections is one item.
    let item_count = items
        .iter()
        .filter(|i| {
            i.data
                .collections
                .iter()
                .any(|c| in_scope.contains(c.as_str()))
        })
        .count();

    let output = CollectionsOutput {
        count: listed.len(),
        item_count,
        root: root_key,
        collections: listed
            .iter()
            .map(|n| CollectionOutput {
                key: n.key.clone(),
                name: n.name.clone(),
                parent: n.parent.clone(),
                depth: n.depth - base_depth,
                count_direct: n.count_direct,
                count_tree: n.count_tree,
                tree_id: n.tree_id.clone(),
            })
            .collect(),
        flat,
        show_tree_ids: tree_ids,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}

/// The contiguous preorder run starting at `key`: `build_tree` emits a parent
/// immediately before its descendants, so a subtree ends at the first node back
/// at or above the root's depth.
fn subtree<'a>(nodes: &'a [CollectionNode], key: &str) -> Vec<&'a CollectionNode> {
    let Some(start) = nodes.iter().position(|n| n.key == key) else {
        return Vec::new();
    };
    let root_depth = nodes[start].depth;
    std::iter::once(&nodes[start])
        .chain(
            nodes[start + 1..]
                .iter()
                .take_while(|n| n.depth > root_depth),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::ZoteroCollection;
    use serde_json::json;

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

    /// Preorder of the assembled tree (siblings sort case-insensitively):
    /// ROOT1, MID, DEEP, DEEPER, ELM, BETA, ROOT2, LAST.
    fn tree() -> Vec<CollectionNode> {
        build_tree(&[
            collection("ROOT1", "Alpha", None),
            collection("MID", "apple", Some("ROOT1")),
            collection("DEEP", "Deep", Some("MID")),
            collection("DEEPER", "Deeper", Some("DEEP")),
            collection("ELM", "Elm", Some("MID")),
            collection("BETA", "Beta", Some("ROOT1")),
            collection("ROOT2", "Zed", None),
            collection("LAST", "Only child", Some("ROOT2")),
        ])
    }

    fn keys<'a>(nodes: &[&'a CollectionNode]) -> Vec<&'a str> {
        nodes.iter().map(|n| n.key.as_str()).collect()
    }

    #[test]
    fn subtree_of_mid_tree_node_stops_before_the_next_sibling() {
        let nodes = tree();
        let preorder: Vec<&str> = nodes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(
            preorder,
            vec![
                "ROOT1", "MID", "DEEP", "DEEPER", "ELM", "BETA", "ROOT2", "LAST"
            ],
            "fixture preorder"
        );
        // BETA sits right after MID's last descendant, at MID's own depth.
        assert_eq!(
            keys(&subtree(&nodes, "MID")),
            vec!["MID", "DEEP", "DEEPER", "ELM"]
        );
    }

    #[test]
    fn subtree_of_leaf_is_the_leaf_alone() {
        let nodes = tree();
        assert_eq!(keys(&subtree(&nodes, "DEEPER")), vec!["DEEPER"]);
        assert_eq!(keys(&subtree(&nodes, "ELM")), vec!["ELM"]);
    }

    #[test]
    fn subtree_of_last_node_in_preorder_does_not_run_off_the_end() {
        let nodes = tree();
        assert_eq!(nodes.last().unwrap().key, "LAST");
        assert_eq!(keys(&subtree(&nodes, "LAST")), vec!["LAST"]);
    }

    #[test]
    fn subtree_of_root_covers_the_whole_nesting() {
        let nodes = tree();
        assert_eq!(
            keys(&subtree(&nodes, "ROOT1")),
            vec!["ROOT1", "MID", "DEEP", "DEEPER", "ELM", "BETA"]
        );
        assert_eq!(keys(&subtree(&nodes, "ROOT2")), vec!["ROOT2", "LAST"]);
    }
}
