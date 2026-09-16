//! Collection-graph helpers.
//!
//! Every function here is pure over already-fetched data (`fetch_collections`,
//! `fetch_top_items`, `list_targets`), so the whole module unit tests offline.
//!
//! Two independent views of the same tree exist and have to be reconciled:
//! the read API's collections, identified by an 8-character key, and the
//! connector's save targets, identified by a tree-view ID (`L1` for the user
//! library, `C42` for a collection). Nothing in either payload links the two,
//! so they are paired by full name path (see `pair_connector_ids`).

use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

use crate::api::connector::SaveTarget;
use crate::api::models::{ZoteroCollection, ZoteroItem};

/// Connector tree-view ID of the user's own library ("My Library"). Group
/// libraries get `L2`, `L3`, ...; the read API base path (`/users/0`) always
/// addresses this one.
pub const LIBRARY_ROOT_ID: &str = "L1";

/// Separator for path-joining collection names. A NUL byte cannot occur in a
/// Zotero collection name, so joined paths never collide.
const PATH_SEP: char = '\u{0}';

/// One collection in the assembled tree.
// Only the identity fields are read on the `zot add` path; the rest are the
// payload of `zot collections` (plan 1.2) and are exercised by the unit tests.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CollectionNode {
    pub key: String,
    pub name: String,
    /// Key of the parent collection; `None` for a top-level collection (and
    /// for an orphan, whose parent key is absent from the input).
    pub parent: Option<String>,
    /// 0 for a top-level collection.
    pub depth: usize,
    /// Child keys, in the same order as they appear in the returned `Vec`.
    pub children: Vec<String>,
    /// Items filed directly in this collection.
    pub count_direct: usize,
    /// Distinct items filed anywhere in this collection's subtree. Always
    /// `>= count_direct`, and *not* the sum of the subtree's direct counts:
    /// an item filed in both a parent and a child is one item.
    pub count_tree: usize,
    /// `meta.numItems` as reported by the API, for cross-checking
    /// `count_direct` against Zotero's own tally.
    pub meta_num_items: u32,
    /// `meta.numCollections` as reported by the API, i.e. the number of direct
    /// children Zotero knows about.
    pub meta_num_collections: u32,
}

/// A resolved collection reference.
///
/// The two identifier spaces are populated independently: the connector write
/// path needs `connector_id`, the web API write path needs `key`. Either can
/// be absent when the corresponding view has no unambiguous match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRef {
    /// Collection key, `None` for a library root target such as `L1`.
    pub key: Option<String>,
    /// Connector tree-view ID (`L1`, `C42`).
    pub connector_id: Option<String>,
    /// Display name of the collection (or of the library, for a root target).
    pub name: String,
}

/// Assemble collections into a tree.
///
/// Returns every input collection exactly once, in depth-first preorder with
/// siblings sorted case-insensitively by name (ties broken by key), which is
/// the order Zotero itself displays. Preorder means a parent always precedes
/// its descendants, and the reverse of the returned slice is a valid postorder.
///
/// A collection whose `parentCollection` names a key that is not in the input
/// (an orphan, e.g. when the parent lives in another library, or the list was
/// filtered) is promoted to a root and sorted among the real roots, so nothing
/// is ever silently dropped. Parent cycles, which the API should never produce,
/// are broken at the cycle entry point for the same reason.
pub fn build_tree(collections: &[ZoteroCollection]) -> Vec<CollectionNode> {
    let present: HashSet<&str> = collections.iter().map(|c| c.key.as_str()).collect();

    // Raw parent links, with absent parents and self-parents dropped.
    let mut parent_of: HashMap<&str, Option<&str>> = collections
        .iter()
        .map(|c| {
            let parent = c
                .data
                .parent_collection
                .as_str()
                .filter(|p| *p != c.key && present.contains(p));
            (c.key.as_str(), parent)
        })
        .collect();

    break_parent_cycles(collections, &mut parent_of);

    let mut children_of: HashMap<Option<&str>, Vec<&ZoteroCollection>> = HashMap::new();
    for c in collections {
        let parent = parent_of.get(c.key.as_str()).copied().flatten();
        children_of.entry(parent).or_default().push(c);
    }
    for siblings in children_of.values_mut() {
        siblings.sort_by(|a, b| {
            a.data
                .name
                .to_lowercase()
                .cmp(&b.data.name.to_lowercase())
                .then_with(|| a.key.cmp(&b.key))
        });
    }

    let mut nodes: Vec<CollectionNode> = Vec::with_capacity(collections.len());
    // Preorder DFS over an explicit stack; siblings are pushed in reverse so
    // they pop in sorted order.
    let mut stack: Vec<(&ZoteroCollection, usize)> = children_of
        .get(&None)
        .map(|roots| roots.iter().rev().map(|c| (*c, 0usize)).collect())
        .unwrap_or_default();

    while let Some((collection, depth)) = stack.pop() {
        let children = children_of
            .get(&Some(collection.key.as_str()))
            .map(|kids| kids.iter().map(|c| c.key.clone()).collect())
            .unwrap_or_default();
        nodes.push(CollectionNode {
            key: collection.key.clone(),
            name: collection.data.name.clone(),
            parent: parent_of
                .get(collection.key.as_str())
                .copied()
                .flatten()
                .map(str::to_string),
            depth,
            children,
            count_direct: 0,
            count_tree: 0,
            meta_num_items: collection.meta.num_items,
            meta_num_collections: collection.meta.num_collections,
        });
        if let Some(kids) = children_of.get(&Some(collection.key.as_str())) {
            stack.extend(kids.iter().rev().map(|c| (*c, depth + 1)));
        }
    }

    nodes
}

/// Promote a collection to a root when its ancestor chain loops, so the DFS
/// terminates and no collection is dropped.
fn break_parent_cycles<'a>(
    collections: &'a [ZoteroCollection],
    parent_of: &mut HashMap<&'a str, Option<&'a str>>,
) {
    // `mark[key] == Some(walk)` means the key is on the current walk; once a
    // walk finishes, its keys are settled and later walks stop at them.
    let mut mark: HashMap<&str, usize> = HashMap::new();
    let mut settled: HashSet<&str> = HashSet::new();

    for (walk, collection) in collections.iter().enumerate() {
        let mut cursor: &str = collection.key.as_str();
        let mut path: Vec<&str> = Vec::new();
        loop {
            if settled.contains(cursor) {
                break;
            }
            if mark.get(cursor) == Some(&walk) {
                // Cycle: cut it at the collection we came back to.
                parent_of.insert(cursor, None);
                break;
            }
            mark.insert(cursor, walk);
            path.push(cursor);
            match parent_of.get(cursor).copied().flatten() {
                Some(parent) => cursor = parent,
                None => break,
            }
        }
        settled.extend(path);
    }
}

/// Fill `count_direct` and `count_tree` on an assembled tree.
///
/// `count_direct` counts items whose `data.collections` names the collection.
/// `count_tree` counts *distinct* items across the subtree: summing direct
/// counts up the tree would double-count the 143 items of this library that
/// belong to more than one collection.
///
/// `items` is expected to be top-level items (`fetch_top_items`); items filed
/// in a collection that is not in `nodes` are ignored.
#[allow(dead_code)] // consumed by `zot collections` (plan 1.2)
pub fn roll_up_counts(nodes: &mut [CollectionNode], items: &[ZoteroItem]) {
    let known: HashSet<String> = nodes.iter().map(|n| n.key.clone()).collect();

    let mut direct: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in items {
        for collection in &item.data.collections {
            if let Some(key) = known.get(collection.as_str()) {
                direct
                    .entry(key.as_str())
                    .or_default()
                    .push(item.key.as_str());
            }
        }
    }

    // `nodes` is preorder, so iterating it backwards visits every descendant
    // before its ancestor: each subtree set is complete when the parent reads it.
    let mut subtree: HashMap<String, HashSet<&str>> = HashMap::new();
    for node in nodes.iter_mut().rev() {
        let own = direct.get(node.key.as_str()).cloned().unwrap_or_default();
        node.count_direct = own.len();
        let mut set: HashSet<&str> = own.into_iter().collect();
        for child in &node.children {
            if let Some(child_set) = subtree.remove(child) {
                set.extend(child_set);
            }
        }
        node.count_tree = set.len();
        subtree.insert(node.key.clone(), set);
    }
}

/// Resolve a user-supplied collection reference.
///
/// Accepts, in this order: a connector tree-view ID (`L1`, `C42`), a
/// collection key, or an exact (case-sensitive) collection name. An ambiguous
/// name fails with the candidates listed, rather than picking one.
pub fn resolve_collection_ref(
    input: &str,
    nodes: &[CollectionNode],
    targets: &[SaveTarget],
) -> Result<CollectionRef> {
    let wanted = input.trim();
    if wanted.is_empty() {
        bail!("Empty collection reference. Pass a collection key, exact name, or tree-view ID.");
    }

    let key_to_id = pair_connector_ids(nodes, targets);
    let id_to_key: HashMap<&str, &str> = key_to_id
        .iter()
        .map(|(key, id)| (id.as_str(), key.as_str()))
        .collect();

    // 1. Connector tree-view ID.
    if let Some(target) = targets.iter().find(|t| t.id == wanted) {
        return Ok(CollectionRef {
            key: id_to_key.get(target.id.as_str()).map(|k| (*k).to_string()),
            connector_id: Some(target.id.clone()),
            name: target.name.clone(),
        });
    }

    // 2. Collection key.
    if let Some(node) = nodes.iter().find(|n| n.key == wanted) {
        return Ok(CollectionRef {
            key: Some(node.key.clone()),
            connector_id: key_to_id.get(node.key.as_str()).cloned(),
            name: node.name.clone(),
        });
    }

    // 3. Exact collection name.
    let by_name: Vec<&CollectionNode> = nodes.iter().filter(|n| n.name == wanted).collect();
    match by_name.len() {
        1 => {
            let node = by_name[0];
            return Ok(CollectionRef {
                key: Some(node.key.clone()),
                connector_id: key_to_id.get(node.key.as_str()).cloned(),
                name: node.name.clone(),
            });
        }
        0 => {}
        _ => bail!(
            "Collection name \"{wanted}\" is ambiguous ({} matches). \
             Pass a key or tree-view ID instead: {}",
            by_name.len(),
            by_name
                .iter()
                .map(|n| match key_to_id.get(n.key.as_str()) {
                    Some(id) => format!("{} ({})", n.key, id),
                    None => n.key.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }

    // 4. A connector target with no counterpart in `nodes` -- a collection in a
    //    group library, which the read API's `/users/0` view does not list.
    let unpaired: Vec<&SaveTarget> = targets
        .iter()
        .filter(|t| {
            t.name == wanted && t.id.starts_with('C') && !id_to_key.contains_key(t.id.as_str())
        })
        .collect();
    match unpaired.len() {
        1 => Ok(CollectionRef {
            key: None,
            connector_id: Some(unpaired[0].id.clone()),
            name: unpaired[0].name.clone(),
        }),
        0 => bail!(
            "Collection not found: {wanted}\n  \
             Pass a collection key, exact collection name, or tree-view ID (e.g. C42)."
        ),
        _ => bail!(
            "Collection name \"{wanted}\" is ambiguous ({} matches). \
             Pass a tree-view ID instead: {}",
            unpaired.len(),
            unpaired
                .iter()
                .map(|t| format!("{} ({})", t.id, t.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Map collection keys to connector tree-view IDs by full name path.
///
/// The connector reports a flat, preorder list where `level` is the depth
/// (0 = a library root), so the ancestry of each target is recoverable from a
/// stack. Only targets under [`LIBRARY_ROOT_ID`] are considered, since the read
/// API's collections all come from that library. A path shared by two
/// collections (impossible in Zotero, which forbids duplicate sibling names)
/// maps to neither.
fn pair_connector_ids(nodes: &[CollectionNode], targets: &[SaveTarget]) -> HashMap<String, String> {
    let mut path_to_id: HashMap<String, Option<&str>> = HashMap::new();
    let mut ancestors: Vec<&str> = Vec::new();
    let mut in_user_library = false;

    for target in targets {
        if target.level == 0 {
            in_user_library = target.id == LIBRARY_ROOT_ID;
            ancestors.clear();
            continue;
        }
        let depth = target.level as usize - 1;
        if depth > ancestors.len() {
            // A level jump means the list is not the preorder walk we assume;
            // skip the target rather than inventing an ancestry for it.
            continue;
        }
        ancestors.truncate(depth);
        if in_user_library {
            let path = join_path(&ancestors, &target.name);
            path_to_id
                .entry(path)
                .and_modify(|slot| *slot = None)
                .or_insert(Some(target.id.as_str()));
        }
        ancestors.push(target.name.as_str());
    }

    // Node paths, built top-down: preorder guarantees a parent is done first.
    let mut node_path: HashMap<&str, String> = HashMap::new();
    let mut paired: HashMap<String, String> = HashMap::new();
    for node in nodes {
        let path = match &node.parent {
            Some(parent) => match node_path.get(parent.as_str()) {
                Some(prefix) => format!("{prefix}{PATH_SEP}{}", node.name),
                None => node.name.clone(),
            },
            None => node.name.clone(),
        };
        if let Some(Some(id)) = path_to_id.get(&path) {
            paired.insert(node.key.clone(), (*id).to_string());
        }
        node_path.insert(node.key.as_str(), path);
    }
    paired
}

fn join_path(ancestors: &[&str], name: &str) -> String {
    let mut path = String::new();
    for ancestor in ancestors {
        path.push_str(ancestor);
        path.push(PATH_SEP);
    }
    path.push_str(name);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn item(key: &str, collections: &[&str]) -> ZoteroItem {
        serde_json::from_value(json!({
            "key": key,
            "version": 1,
            "data": {
                "key": key,
                "version": 1,
                "itemType": "journalArticle",
                "collections": collections,
            },
        }))
        .expect("item fixture")
    }

    fn target(id: &str, name: &str, level: u32) -> SaveTarget {
        serde_json::from_value(json!({ "id": id, "name": name, "level": level }))
            .expect("target fixture")
    }

    fn keys(nodes: &[CollectionNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.key.as_str()).collect()
    }

    /// Root -> child -> grandchild, with siblings out of alphabetical order in
    /// the input.
    fn nested() -> Vec<ZoteroCollection> {
        vec![
            collection("GRAND1", "Deep", Some("CHILD1")),
            collection("CHILD2", "Beta", Some("ROOT1")),
            collection("ROOT1", "Alpha", None),
            collection("CHILD1", "apple", Some("ROOT1")),
            collection("ROOT2", "Zed", None),
        ]
    }

    #[test]
    fn build_tree_nests_and_sorts_siblings() {
        let nodes = build_tree(&nested());
        // Preorder, siblings case-insensitively by name: "apple" < "Beta".
        // Byte-wise Ord would put "Beta" first, so this pins the lowercasing.
        assert_eq!(
            keys(&nodes),
            vec!["ROOT1", "CHILD1", "GRAND1", "CHILD2", "ROOT2"]
        );
        let depths: Vec<usize> = nodes.iter().map(|n| n.depth).collect();
        assert_eq!(depths, vec![0, 1, 2, 1, 0]);
        assert_eq!(nodes[0].children, vec!["CHILD1", "CHILD2"]);
        assert_eq!(nodes[0].parent, None);
        assert_eq!(nodes[1].parent.as_deref(), Some("ROOT1"));
        assert!(nodes[2].children.is_empty());
    }

    #[test]
    fn build_tree_promotes_orphans_to_roots() {
        let collections = vec![
            collection("ORPHAN", "Missing parent", Some("GONE")),
            collection("ROOT1", "Alpha", None),
        ];
        let nodes = build_tree(&collections);
        assert_eq!(keys(&nodes), vec!["ROOT1", "ORPHAN"]);
        assert_eq!(nodes[1].parent, None);
        assert_eq!(nodes[1].depth, 0);
    }

    #[test]
    fn build_tree_breaks_parent_cycles() {
        let collections = vec![
            collection("A", "A", Some("B")),
            collection("B", "B", Some("A")),
            collection("SELF", "Self", Some("SELF")),
        ];
        let nodes = build_tree(&collections);
        assert_eq!(nodes.len(), 3, "no collection may be dropped");
        assert!(nodes.iter().any(|n| n.key == "SELF" && n.parent.is_none()));
    }

    #[test]
    fn build_tree_handles_empty_input() {
        assert!(build_tree(&[]).is_empty());
    }

    #[test]
    fn build_tree_reads_meta_counts() {
        let raw: ZoteroCollection = serde_json::from_value(json!({
            "key": "K1",
            "version": 3,
            "meta": { "numItems": 7, "numCollections": 2 },
            "data": { "key": "K1", "name": "N", "parentCollection": false },
        }))
        .unwrap();
        let nodes = build_tree(&[raw]);
        assert_eq!(nodes[0].meta_num_items, 7);
        assert_eq!(nodes[0].meta_num_collections, 2);
    }

    #[test]
    fn roll_up_counts_does_not_double_count_items_in_parent_and_child() {
        let collections = vec![
            collection("ROOT1", "Alpha", None),
            collection("CHILD1", "Child", Some("ROOT1")),
        ];
        let mut nodes = build_tree(&collections);
        let items = vec![
            // Filed in both the parent and the child: one distinct item.
            item("SHARED", &["ROOT1", "CHILD1"]),
            item("PARENTONLY", &["ROOT1"]),
            item("CHILDONLY", &["CHILD1"]),
        ];
        roll_up_counts(&mut nodes, &items);

        let root = &nodes[0];
        let child = &nodes[1];
        assert_eq!(child.count_direct, 2);
        assert_eq!(child.count_tree, 2);
        assert_eq!(root.count_direct, 2);
        // Summing direct counts would give 4; there are only 3 distinct items.
        assert_eq!(root.count_tree, 3);
    }

    #[test]
    fn roll_up_counts_unions_across_sibling_subtrees() {
        let collections = vec![
            collection("ROOT1", "Alpha", None),
            collection("CHILD1", "A child", Some("ROOT1")),
            collection("CHILD2", "B child", Some("ROOT1")),
            collection("GRAND1", "Grand", Some("CHILD1")),
        ];
        let mut nodes = build_tree(&collections);
        let items = vec![
            item("I1", &["GRAND1", "CHILD2"]),
            item("I2", &["CHILD1"]),
            item("I3", &["CHILD2"]),
            // Filed nowhere in this tree.
            item("I4", &["ELSEWHERE"]),
        ];
        roll_up_counts(&mut nodes, &items);
        let by_key: HashMap<&str, &CollectionNode> =
            nodes.iter().map(|n| (n.key.as_str(), n)).collect();

        assert_eq!(by_key["ROOT1"].count_direct, 0);
        assert_eq!(by_key["ROOT1"].count_tree, 3);
        assert_eq!(by_key["CHILD1"].count_tree, 2);
        assert_eq!(by_key["GRAND1"].count_tree, 1);
        assert_eq!(by_key["CHILD2"].count_tree, 2);
    }

    /// Tree plus connector targets that mirror it, as the live API reports them.
    fn resolvable() -> (Vec<CollectionNode>, Vec<SaveTarget>) {
        let nodes = build_tree(&[
            collection("ROOT1", "Alpha", None),
            collection("CHILD1", "Shared", Some("ROOT1")),
            collection("ROOT2", "Beta", None),
            collection("CHILD2", "Shared", Some("ROOT2")),
            collection("ROOT3", "Unique", None),
        ]);
        let targets = vec![
            target("L1", "My Library", 0),
            target("C1", "Alpha", 1),
            target("C2", "Shared", 2),
            target("C3", "Beta", 1),
            target("C4", "Shared", 2),
            target("C5", "Unique", 1),
            target("L2", "Group Library", 0),
            target("C9", "Group Only", 1),
        ];
        (nodes, targets)
    }

    #[test]
    fn resolve_collection_ref_by_key() {
        let (nodes, targets) = resolvable();
        let r = resolve_collection_ref("CHILD2", &nodes, &targets).unwrap();
        assert_eq!(r.key.as_deref(), Some("CHILD2"));
        // Paired by path, so the second "Shared" gets C4 rather than C2.
        assert_eq!(r.connector_id.as_deref(), Some("C4"));
        assert_eq!(r.name, "Shared");
    }

    #[test]
    fn resolve_collection_ref_by_name() {
        let (nodes, targets) = resolvable();
        let r = resolve_collection_ref("Unique", &nodes, &targets).unwrap();
        assert_eq!(r.key.as_deref(), Some("ROOT3"));
        assert_eq!(r.connector_id.as_deref(), Some("C5"));
    }

    #[test]
    fn resolve_collection_ref_by_connector_id() {
        let (nodes, targets) = resolvable();
        let r = resolve_collection_ref("C2", &nodes, &targets).unwrap();
        assert_eq!(r.key.as_deref(), Some("CHILD1"));
        assert_eq!(r.connector_id.as_deref(), Some("C2"));

        let root = resolve_collection_ref("L1", &nodes, &targets).unwrap();
        assert_eq!(root.key, None);
        assert_eq!(root.connector_id.as_deref(), Some("L1"));
    }

    #[test]
    fn resolve_collection_ref_by_group_library_name() {
        let (nodes, targets) = resolvable();
        let r = resolve_collection_ref("Group Only", &nodes, &targets).unwrap();
        assert_eq!(r.key, None);
        assert_eq!(r.connector_id.as_deref(), Some("C9"));
    }

    #[test]
    fn resolve_collection_ref_rejects_ambiguous_name() {
        let (nodes, targets) = resolvable();
        let err = resolve_collection_ref("Shared", &nodes, &targets).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(msg.contains("CHILD1 (C2)"), "{msg}");
        assert!(msg.contains("CHILD2 (C4)"), "{msg}");
    }

    #[test]
    fn resolve_collection_ref_reports_not_found() {
        let (nodes, targets) = resolvable();
        let err = resolve_collection_ref("Nope", &nodes, &targets).unwrap_err();
        assert!(format!("{err}").contains("Collection not found: Nope"));

        let empty = resolve_collection_ref("   ", &nodes, &targets).unwrap_err();
        assert!(format!("{empty}").contains("Empty collection reference"));
    }

    #[test]
    fn resolve_collection_ref_without_connector_targets() {
        let (nodes, _) = resolvable();
        let r = resolve_collection_ref("ROOT1", &nodes, &[]).unwrap();
        assert_eq!(r.key.as_deref(), Some("ROOT1"));
        assert_eq!(r.connector_id, None);
    }
}
