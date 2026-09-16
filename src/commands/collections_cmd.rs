use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::api::ZoteroClient;
use crate::api::connector::{ConnectorClient, SaveTarget};
use crate::api::webapi::WebApiClient;
use crate::collections::{
    CollectionNode, CollectionRef, LIBRARY_ROOT_ID, attach_connector_ids, build_tree,
    resolve_collection_ref, roll_up_counts, sibling_named, sibling_named_ignoring_case,
};
use crate::output::{CollectionCreatedOutput, CollectionOutput, CollectionsOutput, format_output};

/// How long to wait for a collection created on api.zotero.org to be synced
/// back down into the local library. Shorter than `zot add`'s upward wait: the
/// downward direction is stream-driven and normally lands in a second or two,
/// and the command has a useful answer either way.
const SYNC_POLL_TIMEOUT: Duration = Duration::from_secs(20);

/// Gap between local-library probes while waiting for that sync.
const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// List the collection tree, or one subtree of it, or create a collection.
///
/// `collection` names the subtree to list and `parent` the parent of a
/// `create`; both accept anything `resolve_collection_ref` does, a key, an
/// exact name, or a connector tree-view ID. Clap keeps them apart: `parent`
/// requires `create`, and `collection` conflicts with it.
pub fn run_collections(
    collection: Option<&str>,
    create: Option<&str>,
    parent: Option<&str>,
    flat: bool,
    tree_ids: bool,
    json: bool,
) -> Result<()> {
    let client = ZoteroClient::new()?;
    let mut nodes = build_tree(&client.fetch_collections()?);

    if let Some(name) = create {
        return run_create(&client, &nodes, parent, name, json);
    }

    let items = client.fetch_top_items()?;
    roll_up_counts(&mut nodes, &items);

    // The connector is only needed to name or show tree-view IDs, and it may be
    // unreachable (Zotero closed, or the connector port taken), which must not
    // break a plain listing.
    let targets = if tree_ids || collection.is_some() {
        connector_targets(tree_ids)
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

/// Fetch the connector's save targets, degrading to none when the connector is
/// unreachable (Zotero closed, or the connector port taken), which must not
/// break a command that only wanted them to resolve a reference.
fn connector_targets(tree_ids: bool) -> Vec<SaveTarget> {
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
}

/// Create one collection through the web API, under `parent_ref` or at the top
/// level.
///
/// Everything that can fail without writing is checked first: the parent has to
/// resolve, and no sibling may already carry the name. That name check reads
/// the local library while the write goes upstream, so a sibling created
/// seconds ago and not yet synced down is invisible to it.
fn run_create(
    client: &ZoteroClient,
    nodes: &[CollectionNode],
    parent_ref: Option<&str>,
    name: &str,
    json: bool,
) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("Empty collection name. Pass the name to create: --create \"Name\"");
    }

    let (parent_key, parent_name) = match parent_ref {
        Some(wanted) => {
            let targets = connector_targets(false);
            let resolved = resolve_collection_ref(wanted, nodes, &targets)?;
            match create_parent(&resolved)? {
                Some(key) => (Some(key), Some(resolved.name)),
                None => (None, None),
            }
        }
        None => (None, None),
    };

    if let Some(existing) = sibling_named(nodes, parent_key.as_deref(), name) {
        let under = match &parent_name {
            Some(parent) => format!("under \"{parent}\""),
            None => "at the top level".to_string(),
        };
        bail!(
            "A collection named \"{name}\" already exists {under}: [{key}]\n  \
             A second one would make every later `zot collections \"{name}\"` and \
             `--add-collection \"{name}\"` ambiguous.\n  \
             Pick a different name, or use the existing collection by its key [{key}].",
            key = existing.key,
        );
    }
    if let Some(similar) = sibling_named_ignoring_case(nodes, parent_key.as_deref(), name) {
        eprintln!(
            "Warning: sibling \"{}\" [{}] differs from \"{name}\" only by case.",
            similar.name, similar.key,
        );
    }

    let web = WebApiClient::from_config()?;
    let key = web
        .create_collection(name, parent_key.as_deref())
        .with_context(|| format!("Failed to create collection \"{name}\""))?;

    let output = CollectionCreatedOutput {
        synced_local: wait_for_local(client, &key),
        key,
        name: name.to_string(),
        parent: parent_key,
        parent_name,
    };
    println!("{}", format_output(&output, json));
    Ok(())
}

/// Where a resolved `--parent` points: `Some(key)` for a collection, `None`
/// for the top level of the user's own library.
///
/// A reference the read API cannot address (a group library, or a collection in
/// one) has to fail rather than fall back to the top level, which would file
/// the new collection somewhere the caller did not ask for.
fn create_parent(resolved: &CollectionRef) -> Result<Option<String>> {
    if let Some(key) = &resolved.key {
        return Ok(Some(key.clone()));
    }
    match resolved.connector_id.as_deref() {
        Some(id) if id == LIBRARY_ROOT_ID => Ok(None),
        Some(id) if id.starts_with('L') => bail!(
            "{} ({id}) is another library's root. `zot collections --create` writes to My \
             Library only.",
            resolved.name,
        ),
        Some(id) => bail!(
            "Collection \"{}\" ({id}) is only known to the connector, most likely because it \
             lives in a group library, which this command cannot write to.",
            resolved.name,
        ),
        None => bail!(
            "Collection \"{}\" resolved to no key and no tree-view ID, so there is nothing to \
             create under.",
            resolved.name,
        ),
    }
}

/// Wait for a collection created upstream to appear in the local library.
///
/// The write lands on api.zotero.org, and `zot collections` reads the local
/// API, so reporting "created" without checking would point at a tree that does
/// not have it yet. Progress goes to stderr, never stdout, so `--json` stdout
/// stays a single document. A local API that starts failing mid-wait is
/// reported as "not synced" rather than as a failed creation: the collection
/// exists either way.
fn wait_for_local(client: &ZoteroClient, key: &str) -> bool {
    let started = Instant::now();
    loop {
        match client.fetch_collections() {
            Ok(collections) => {
                if collections.iter().any(|c| c.key == key) {
                    return true;
                }
            }
            Err(e) => {
                eprintln!("Warning: could not read the local library while waiting: {e:#}");
                return false;
            }
        }
        if started.elapsed() + SYNC_POLL_INTERVAL >= SYNC_POLL_TIMEOUT {
            return false;
        }
        if started.elapsed() < SYNC_POLL_INTERVAL {
            eprintln!(
                "Waiting for Zotero to sync [{key}] down to the local library (up to {}s)...",
                SYNC_POLL_TIMEOUT.as_secs(),
            );
        }
        std::thread::sleep(SYNC_POLL_INTERVAL);
    }
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

    fn reference(key: Option<&str>, connector_id: Option<&str>, name: &str) -> CollectionRef {
        CollectionRef {
            key: key.map(str::to_string),
            connector_id: connector_id.map(str::to_string),
            name: name.to_string(),
        }
    }

    #[test]
    fn parent_flag_pointing_at_a_collection_yields_its_key() {
        let parent = create_parent(&reference(Some("ROOT1"), Some("C1"), "Alpha")).unwrap();
        assert_eq!(parent.as_deref(), Some("ROOT1"));
    }

    #[test]
    fn parent_flag_pointing_at_the_library_root_is_the_top_level() {
        // L1 is a valid --parent, not an error: it means "no parent", the same
        // place omitting --parent creates in.
        let parent = create_parent(&reference(None, Some("L1"), "My Library")).unwrap();
        assert_eq!(parent, None);
    }

    #[test]
    fn parent_flag_rejects_what_the_read_api_cannot_address() {
        let group_root = create_parent(&reference(None, Some("L2"), "Group Library"))
            .unwrap_err()
            .to_string();
        assert!(group_root.contains("another library's root"), "{group_root}");

        let group_collection = create_parent(&reference(None, Some("C9"), "Group Only"))
            .unwrap_err()
            .to_string();
        assert!(group_collection.contains("group library"), "{group_collection}");

        let nothing = create_parent(&reference(None, None, "Nowhere"))
            .unwrap_err()
            .to_string();
        assert!(nothing.contains("nothing to create under"), "{nothing}");
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
