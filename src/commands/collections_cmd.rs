use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::api::ZoteroClient;
use crate::api::connector::{ConnectorClient, SaveTarget};
use crate::api::models::ZoteroItem;
use crate::api::webapi::{VersionConflict, WebApiClient};
use crate::collections::{
    CollectionNode, CollectionRef, LIBRARY_ROOT_ID, attach_connector_ids, build_tree,
    resolve_collection_ref, roll_up_counts, sibling_named, sibling_named_ignoring_case,
};
use crate::output::{
    CollectionCreatedOutput, CollectionDeletedOutput, CollectionOutput, CollectionRemovedOutput,
    CollectionsOutput, format_output,
};

/// How long to wait for a collection created on api.zotero.org to be synced
/// back down into the local library. Shorter than `zot add`'s upward wait: the
/// downward direction is stream-driven and normally lands in a second or two,
/// and the command has a useful answer either way.
const SYNC_POLL_TIMEOUT: Duration = Duration::from_secs(20);

/// Gap between local-library probes while waiting for that sync.
const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Arguments of `zot collections`, bundled because the command now takes more
/// of them than a plain function signature should carry.
pub struct CollectionsArgs<'a> {
    pub collection: Option<&'a str>,
    pub create: Option<&'a str>,
    pub parent: Option<&'a str>,
    pub rm: Option<&'a str>,
    pub force: bool,
    pub flat: bool,
    pub tree_ids: bool,
    pub json: bool,
}

/// List the collection tree, or one subtree of it, create a collection, or
/// delete one.
///
/// `collection` names the subtree to list, `parent` the parent of a `create`,
/// and `rm` the collection to delete; all three accept anything
/// `resolve_collection_ref` does, a key, an exact name, or a connector
/// tree-view ID. Clap keeps them apart: `parent` requires `create`, `force`
/// requires `rm`, and `collection`, `create` and `rm` conflict with each other.
pub fn run_collections(args: CollectionsArgs) -> Result<()> {
    let CollectionsArgs {
        collection,
        create,
        parent,
        rm,
        force,
        flat,
        tree_ids,
        json,
    } = args;
    let client = ZoteroClient::new()?;
    let mut nodes = build_tree(&client.fetch_collections()?);

    if let Some(name) = create {
        return run_create(&client, &nodes, parent, name, json);
    }
    if let Some(wanted) = rm {
        return run_delete(&client, &nodes, wanted, force, json);
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
pub fn connector_targets(tree_ids: bool) -> Vec<SaveTarget> {
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
        synced_local: wait_for_local(client, &key, true),
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

/// Wait for a write made upstream to show up in the local library: `want`
/// is `true` for a creation (wait until `key` appears), `false` for a deletion
/// (wait until it is gone).
///
/// The write lands on api.zotero.org, and `zot collections` reads the local
/// API, so reporting "created" or "deleted" without checking would point at a
/// tree that does not agree yet. Progress goes to stderr, never stdout, so
/// `--json` stdout stays a single document. A local API that starts failing
/// mid-wait is reported as "not synced" rather than as a failed write: the
/// write already happened either way.
fn wait_for_local(client: &ZoteroClient, key: &str, want: bool) -> bool {
    let started = Instant::now();
    loop {
        match client.fetch_collections() {
            Ok(collections) => {
                if collections.iter().any(|c| c.key == key) == want {
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
            let what = if want { "add" } else { "remove" };
            eprintln!(
                "Waiting for Zotero to sync the {what} of [{key}] down to the local library \
                 (up to {}s)...",
                SYNC_POLL_TIMEOUT.as_secs(),
            );
        }
        std::thread::sleep(SYNC_POLL_INTERVAL);
    }
}

/// How many collections a `--rm` scope may hold before the server-side
/// reconciliation is refused instead of run: it costs one GET per node, issued
/// one after another, so a deep subtree would sit at a blank terminal for
/// several seconds before the summary appears.
const RECONCILE_MAX_NODES: usize = 50;

/// Everything one `--rm` destroys, worked out before anything is written.
///
/// Built from the local library alone, so it is a snapshot of what Zotero has
/// synced down; `reconcile_with_server` is what checks that snapshot against
/// api.zotero.org, which is where the delete actually cascades.
#[derive(Debug)]
struct DeleteScope {
    key: String,
    name: String,
    /// Key of the parent; `None` for a top-level collection.
    parent: Option<String>,
    /// The named collection first, then its descendants in preorder.
    removed: Vec<CollectionRemovedOutput>,
    /// Distinct top-level items filed anywhere in `removed`.
    item_count: usize,
    /// Of those, the ones filed in no collection outside `removed`, which the
    /// delete therefore leaves unfiled.
    unfiled_count: usize,
}

impl DeleteScope {
    fn descendant_count(&self) -> usize {
        self.removed.len() - 1
    }
}

/// How the delete is authorised.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// `--force` was passed: no prompt.
    Forced,
    /// Prompt, and go ahead only on exactly this answer.
    Prompt(String),
}

/// Delete one collection through the web API, after telling the user exactly
/// what goes with it and getting that confirmed.
fn run_delete(
    client: &ZoteroClient,
    nodes: &[CollectionNode],
    wanted: &str,
    force: bool,
    json: bool,
) -> Result<()> {
    let targets = connector_targets(false);
    let resolved = resolve_collection_ref(wanted, nodes, &targets)?;
    let key = delete_target(&resolved)?;
    let items = client.fetch_top_items()?;
    let scope = delete_scope(nodes, &items, &key)?;

    let web = WebApiClient::from_config()?;
    // The summary is local, the cascade is server-side, so the two have to be
    // shown to agree before a confirmation can mean anything. `--force` waives
    // the confirmation, and these reads with it.
    if !force {
        reconcile_with_server(&web, nodes, &scope)?;
    }

    // Everything the user reads goes to stderr, so `--json` stdout stays one
    // document even when the prompt and the summary are on screen.
    let gate = delete_gate(&scope, force, std::io::stdin().is_terminal())?;
    eprint!("{}", delete_summary(&scope));
    if let Gate::Prompt(expected) = &gate {
        eprint!("{}", prompt_line(expected));
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("Failed to read the confirmation from stdin")?;
        if !confirmed(&answer, expected) {
            bail!("Aborted: nothing was deleted.");
        }
    }

    for target in delete_requests(&scope) {
        let version = web
            .get_collection(target)?
            .get("version")
            .and_then(|v| v.as_u64())
            .context("No version on collection")?;
        if let Err(e) = web.delete_collection(target, version) {
            // A 412 means something else wrote to this collection in the
            // milliseconds between the version read just above and the DELETE:
            // a rename, a move, an item filed into it from another device. It
            // says nothing about the interval the user spent reading the
            // summary, which is what `reconcile_with_server` covers.
            if e.downcast_ref::<VersionConflict>().is_some() {
                bail!(
                    "[{target}] changed on api.zotero.org between the read and the write \
                     (version {version}), so nothing was deleted.\n  \
                     Sync Zotero so the local library catches up with that change, then \
                     re-run `zot collections --rm` to see what it contains now."
                );
            }
            return Err(e);
        }
    }

    let output = CollectionDeletedOutput {
        synced_local: wait_for_local(client, &scope.key, false),
        descendant_count: scope.descendant_count(),
        key: scope.key,
        name: scope.name,
        parent: scope.parent,
        removed: scope.removed,
        item_count: scope.item_count,
        unfiled_count: scope.unfiled_count,
        note: "Permanent: Zotero has no trash for collections, so unlike `zot rm` this cannot \
               be undone."
            .to_string(),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}

/// The key a resolved `--rm` points at.
///
/// Unlike `--parent`, there is no "top level" fallback: a reference the read
/// API cannot address has to fail, because guessing here deletes something.
fn delete_target(resolved: &CollectionRef) -> Result<String> {
    if let Some(key) = &resolved.key {
        return Ok(key.clone());
    }
    match resolved.connector_id.as_deref() {
        Some(id) if id.starts_with('L') => bail!(
            "{} ({id}) is a library root, not a collection, and cannot be deleted.",
            resolved.name,
        ),
        Some(id) => bail!(
            "Collection \"{}\" ({id}) is only known to the connector, most likely because it \
             lives in a group library, which this command cannot write to.",
            resolved.name,
        ),
        None => bail!(
            "Collection \"{}\" resolved to no key, so there is nothing to delete.",
            resolved.name,
        ),
    }
}

/// Work out what deleting `key` would destroy.
///
/// `item_count` counts distinct items, since an item filed in both a parent and
/// a child is one item; `unfiled_count` is the subset filed nowhere else, which
/// is the only number that describes real loss of structure.
fn delete_scope(
    nodes: &[CollectionNode],
    items: &[ZoteroItem],
    key: &str,
) -> Result<DeleteScope> {
    let doomed = subtree(nodes, key);
    let Some(root) = doomed.first() else {
        bail!("Collection {key} is not in the local library, so there is nothing to delete.");
    };
    let base_depth = root.depth;
    let in_scope: HashSet<&str> = doomed.iter().map(|n| n.key.as_str()).collect();

    let mut item_count = 0;
    let mut unfiled_count = 0;
    for item in items {
        let filed = &item.data.collections;
        if filed.iter().any(|c| in_scope.contains(c.as_str())) {
            item_count += 1;
            if filed.iter().all(|c| in_scope.contains(c.as_str())) {
                unfiled_count += 1;
            }
        }
    }

    Ok(DeleteScope {
        key: root.key.clone(),
        name: root.name.clone(),
        parent: root.parent.clone(),
        removed: doomed
            .iter()
            .map(|n| CollectionRemovedOutput {
                key: n.key.clone(),
                name: n.name.clone(),
                depth: n.depth - base_depth,
            })
            .collect(),
        item_count,
        unfiled_count,
    })
}

/// The DELETEs the scope needs, in order.
///
/// Exactly one, the named collection, whatever the subtree looks like: a
/// collection DELETE on api.zotero.org cascades server-side. Probed on
/// 2026-09-16 with a root, a child and a grandchild; deleting the root alone
/// returned 204 and a GET of all three then returned 404. Issuing a DELETE per
/// descendant would send writes for collections the server has already removed,
/// each of which would need a version read that now 404s.
fn delete_requests(scope: &DeleteScope) -> Vec<&str> {
    vec![scope.key.as_str()]
}

/// Check the scope the user is about to be shown against api.zotero.org.
///
/// The summary is built from the local library, the DELETE cascades on the
/// server, and the two part company whenever a subcollection was created
/// somewhere else (another device, the browser connector, a `--create` whose
/// own sync-down wait timed out) and has not been synced down yet. Such a
/// child is absent from the summary, absent from the descendant count that
/// picks the prompt, and destroyed all the same.
///
/// Every node in the scope is checked, not just the named root: an unseen
/// grandchild can hang under a child the local tree does show. The
/// `If-Unmodified-Since-Version` guard on the DELETE cannot stand in for this,
/// because Zotero versions collections one by one and inserting a child does
/// not touch the parent's version (probed 2026-09-18: a parent at version
/// 16895 stayed at 16895 when a child was created under it, while its
/// `meta.numCollections` went from 0 to 1).
fn reconcile_with_server(
    web: &WebApiClient,
    nodes: &[CollectionNode],
    scope: &DeleteScope,
) -> Result<()> {
    if scope.removed.len() > RECONCILE_MAX_NODES {
        bail!(
            "\"{}\" [{}] covers {} collections, more than the {} this command will check \
             against api.zotero.org one request at a time.\n  \
             Delete the subtrees separately, or pass --force once you have synced Zotero and \
             checked in its UI what the subtree holds.",
            scope.name,
            scope.key,
            scope.removed.len(),
            RECONCILE_MAX_NODES,
        );
    }
    // At roughly 0.4s a request, anything past a handful is a visibly quiet
    // terminal, and quiet before a delete prompt reads like a hang.
    if scope.removed.len() > 5 {
        eprintln!(
            "Checking {} collection(s) against api.zotero.org before asking...",
            scope.removed.len(),
        );
    }
    for doomed in &scope.removed {
        let local_children = nodes
            .iter()
            .find(|n| n.key == doomed.key)
            .map_or(0, |n| n.children.len());
        let value = web.get_collection(&doomed.key)?;
        let server_children = value
            .get("meta")
            .and_then(|m| m.get("numCollections"))
            .and_then(|n| n.as_u64())
            .with_context(|| {
                format!("No meta.numCollections on collection {} from the web API", doomed.key)
            })?;
        if !child_counts_agree(local_children, server_children) {
            bail!(
                "api.zotero.org has {server_children} subcollection(s) directly under \"{}\" [{}], \
                 but the local library this summary is built from has {local_children}, so the \
                 delete would cascade into collection(s) it cannot show you. Nothing was \
                 deleted.\n  \
                 Sync Zotero so the local library catches up, then re-run \
                 `zot collections --rm`.",
                doomed.name,
                doomed.key,
            );
        }
    }
    Ok(())
}

/// Whether one collection's local child count matches the server's.
///
/// Any disagreement is disqualifying, in either direction: more on the server
/// means children the summary never listed, fewer means the local tree is
/// describing something the server no longer has, and neither is a state to
/// delete from.
fn child_counts_agree(local_children: usize, server_num_collections: u64) -> bool {
    server_num_collections == local_children as u64
}

/// Decide how a delete is authorised, or refuse when it cannot be.
///
/// The expected answer grows with the blast radius: a leaf takes `yes`, while a
/// collection with descendants takes its own name, typed out. The failure this
/// guards against is a user who under-estimates what hangs below the name they
/// passed, and re-typing the name is what makes them read the list above the
/// prompt. A separate opt-in flag would not: it would be added to the command
/// line before the summary is ever printed, and `--force` already covers the
/// scripted case that a second flag would end up serving.
fn delete_gate(scope: &DeleteScope, force: bool, stdin_is_tty: bool) -> Result<Gate> {
    if force {
        return Ok(Gate::Forced);
    }
    if !stdin_is_tty {
        bail!(
            "Refusing to delete \"{}\" [{}] without confirmation: stdin is not a terminal, so \
             there is nobody to ask.\n  \
             This would permanently remove {} collection(s) and unfile {} item(s), and Zotero \
             has no trash for collections.\n  \
             Re-run it interactively, or pass --force if you have already checked what it \
             removes.",
            scope.name,
            scope.key,
            scope.removed.len(),
            scope.unfiled_count,
        );
    }
    if scope.descendant_count() == 0 {
        Ok(Gate::Prompt("yes".to_string()))
    } else {
        Ok(Gate::Prompt(scope.name.clone()))
    }
}

/// Whether a typed answer authorises the delete.
///
/// Only surrounding whitespace is forgiven, the newline `read_line` leaves
/// above all. Case is not: `YES` and `y` are near-misses of `yes`, and a
/// near-miss of an irreversible delete has to abort. `expected` is compared as
/// it stands, so a collection whose name has leading or trailing whitespace
/// can never be confirmed at the prompt, which fails in the direction that
/// keeps the collection.
fn confirmed(answer: &str, expected: &str) -> bool {
    answer.trim() == expected
}

/// The line asking for `expected`.
fn prompt_line(expected: &str) -> String {
    if expected == "yes" {
        "Type yes to delete it permanently, anything else aborts: ".to_string()
    } else {
        format!("Type the collection's name (\"{expected}\") to confirm, anything else aborts: ")
    }
}

/// What the user reads before confirming: the whole blast radius, in one block.
fn delete_summary(scope: &DeleteScope) -> String {
    let mut out = format!(
        "About to permanently delete collection \"{}\" [{}].\n",
        scope.name, scope.key,
    );
    if scope.descendant_count() == 0 {
        out.push_str("  It has no subcollections.\n");
    } else {
        out.push_str(&format!(
            "  It also takes {} descendant collection(s) with it:\n",
            scope.descendant_count(),
        ));
        for c in scope.removed.iter().skip(1) {
            out.push_str(&format!(
                "    {:indent$}{} [{}]\n",
                "",
                c.name,
                c.key,
                indent = (c.depth - 1) * 2,
            ));
        }
    }
    if scope.item_count == 0 {
        out.push_str("  No items are filed there.\n");
    } else {
        out.push_str(&format!(
            "  {} item(s) are filed there; {} of them would end up in no collection at all.\n",
            scope.item_count, scope.unfiled_count,
        ));
    }
    out.push_str(
        "  The items themselves are NOT deleted, only unfiled; they stay in the library.\n  \
         Zotero has no trash for collections: unlike `zot rm`, this cannot be undone.\n",
    );
    out
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

    fn scope_of(key: &str, items: &[ZoteroItem]) -> DeleteScope {
        delete_scope(&tree(), items, key).expect("scope")
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

    #[test]
    fn rm_flag_rejects_what_cannot_be_deleted() {
        assert_eq!(
            delete_target(&reference(Some("MID"), Some("C3"), "apple")).unwrap(),
            "MID"
        );

        // L1 is a valid --parent but never a valid --rm: there is no
        // "delete the library".
        let root = delete_target(&reference(None, Some("L1"), "My Library"))
            .unwrap_err()
            .to_string();
        assert!(root.contains("library root"), "{root}");

        let group = delete_target(&reference(None, Some("C9"), "Group Only"))
            .unwrap_err()
            .to_string();
        assert!(group.contains("group library"), "{group}");

        let nothing = delete_target(&reference(None, None, "Nowhere"))
            .unwrap_err()
            .to_string();
        assert!(nothing.contains("nothing to delete"), "{nothing}");
    }

    #[test]
    fn delete_scope_of_a_leaf_covers_the_leaf_alone() {
        let scope = scope_of("DEEPER", &[]);
        assert_eq!(scope.key, "DEEPER");
        assert_eq!(scope.name, "Deeper");
        assert_eq!(scope.parent.as_deref(), Some("DEEP"));
        assert_eq!(scope.descendant_count(), 0);
        assert_eq!(scope.removed.len(), 1);
        assert_eq!(scope.removed[0].depth, 0);
    }

    #[test]
    fn delete_scope_of_a_parent_lists_its_descendants_in_preorder_rebased_to_zero() {
        let scope = scope_of("MID", &[]);
        let listed: Vec<(&str, usize)> = scope
            .removed
            .iter()
            .map(|c| (c.key.as_str(), c.depth))
            .collect();
        assert_eq!(
            listed,
            vec![("MID", 0), ("DEEP", 1), ("DEEPER", 2), ("ELM", 1)]
        );
        assert_eq!(scope.descendant_count(), 3);
    }

    #[test]
    fn delete_scope_counts_each_item_once_and_separates_those_left_unfiled() {
        let items = [
            // Filed in two doomed collections: one item, and nothing survives
            // to hold it.
            item("I1", &["MID", "DEEP"]),
            // Filed in a doomed collection and a surviving one: counted, but
            // it keeps a home.
            item("I2", &["DEEPER", "BETA"]),
            // Filed only outside the subtree.
            item("I3", &["BETA"]),
            // Filed nowhere at all: already unfiled, so not this delete's doing.
            item("I4", &[]),
        ];
        let scope = scope_of("MID", &items);
        assert_eq!(scope.item_count, 2);
        assert_eq!(scope.unfiled_count, 1);
    }

    #[test]
    fn delete_scope_rejects_a_key_the_local_library_does_not_have() {
        let err = delete_scope(&tree(), &[], "NOPE").unwrap_err().to_string();
        assert!(err.contains("nothing to delete"), "{err}");
    }

    #[test]
    fn delete_requests_is_the_named_collection_alone_because_the_server_cascades() {
        // Four collections go, one DELETE goes out: api.zotero.org removes the
        // descendants itself (probed 2026-09-16).
        let scope = scope_of("MID", &[]);
        assert_eq!(scope.removed.len(), 4);
        assert_eq!(delete_requests(&scope), vec!["MID"]);

        let leaf = scope_of("DEEPER", &[]);
        assert_eq!(delete_requests(&leaf), vec!["DEEPER"]);
    }

    #[test]
    fn delete_gate_asks_for_yes_on_a_leaf_and_for_the_name_on_a_parent() {
        let leaf = delete_gate(&scope_of("DEEPER", &[]), false, true).unwrap();
        assert_eq!(leaf, Gate::Prompt("yes".to_string()));

        let parent = delete_gate(&scope_of("MID", &[]), false, true).unwrap();
        assert_eq!(parent, Gate::Prompt("apple".to_string()));

        assert!(prompt_line("yes").contains("Type yes"));
        assert!(prompt_line("apple").contains("\"apple\""));
    }

    #[test]
    fn delete_gate_refuses_when_there_is_no_terminal_to_confirm_at() {
        let items = [item("I1", &["MID"])];
        let err = delete_gate(&scope_of("MID", &items), false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stdin is not a terminal"), "{err}");
        assert!(err.contains("4 collection(s)"), "{err}");
        assert!(err.contains("unfile 1 item(s)"), "{err}");
        assert!(err.contains("--force"), "{err}");
    }

    #[test]
    fn delete_gate_skips_the_prompt_under_force_with_or_without_a_terminal() {
        assert_eq!(delete_gate(&scope_of("MID", &[]), true, false).unwrap(), Gate::Forced);
        assert_eq!(delete_gate(&scope_of("MID", &[]), true, true).unwrap(), Gate::Forced);
    }

    #[test]
    fn delete_summary_of_an_empty_leaf_says_there_is_nothing_below_it() {
        let summary = delete_summary(&scope_of("DEEPER", &[]));
        assert!(summary.contains("\"Deeper\" [DEEPER]"), "{summary}");
        assert!(summary.contains("no subcollections"), "{summary}");
        assert!(summary.contains("No items are filed there"), "{summary}");
    }

    #[test]
    fn delete_summary_of_a_parent_names_every_descendant_and_both_item_counts() {
        let items = [item("I1", &["MID", "DEEP"]), item("I2", &["DEEPER", "BETA"])];
        let summary = delete_summary(&scope_of("MID", &items));
        assert!(summary.contains("3 descendant collection(s)"), "{summary}");
        for key in ["DEEP", "DEEPER", "ELM"] {
            assert!(summary.contains(&format!("[{key}]")), "missing {key}: {summary}");
        }
        assert!(
            summary.contains("2 item(s) are filed there; 1 of them would end up in no collection"),
            "{summary}"
        );
    }

    #[test]
    fn delete_summary_indents_each_descendant_under_its_own_parent() {
        // Depth is rebased to the deleted collection, so MID's children sit at
        // the base indent and DEEPER, one level below DEEP, sits two columns
        // further in. Flattening the subtree would still name every key, so the
        // shape is what has to be asserted.
        let summary = delete_summary(&scope_of("MID", &[]));
        assert!(summary.contains("\n    Deep [DEEP]\n"), "{summary}");
        assert!(summary.contains("\n      Deeper [DEEPER]\n"), "{summary}");
        assert!(summary.contains("\n    Elm [ELM]\n"), "{summary}");

        // From ROOT1 the same three collections are each one level deeper.
        let from_root = delete_summary(&scope_of("ROOT1", &[]));
        assert!(from_root.contains("\n    apple [MID]\n"), "{from_root}");
        assert!(from_root.contains("\n      Deep [DEEP]\n"), "{from_root}");
        assert!(from_root.contains("\n        Deeper [DEEPER]\n"), "{from_root}");
    }

    #[test]
    fn confirmed_accepts_only_the_demanded_token_once_whitespace_is_stripped() {
        // What `read_line` hands over.
        assert!(confirmed("yes\n", "yes"));
        assert!(confirmed("yes\r\n", "yes"));
        assert!(confirmed("  yes  \n", "yes"));

        // Near-misses of an irreversible delete abort.
        assert!(!confirmed("YES\n", "yes"));
        assert!(!confirmed("Yes\n", "yes"));
        assert!(!confirmed("y\n", "yes"));
        assert!(!confirmed("yes please\n", "yes"));
        assert!(!confirmed("", "yes"));
        assert!(!confirmed("\n", "yes"));
    }

    #[test]
    fn confirmed_treats_a_typed_collection_name_the_same_way() {
        // A trailing space is the prompt's own noise, not a different answer.
        assert!(confirmed("apple ", "apple"));
        assert!(confirmed("apple\n", "apple"));
        assert!(!confirmed("Apple\n", "apple"));
        assert!(!confirmed("apples\n", "apple"));

        // Whitespace inside the name is part of it, and only the ends are
        // forgiven.
        assert!(confirmed("  Reading list\n", "Reading list"));
        assert!(!confirmed("Reading  list\n", "Reading list"));
        assert!(!confirmed("Readinglist\n", "Reading list"));

        // A name padded with whitespace cannot be confirmed at all, because
        // the answer is trimmed and the name is not. That direction keeps the
        // collection; --force is the way out.
        assert!(!confirmed("Padded \n", "Padded "));
    }

    #[test]
    fn child_counts_agree_only_when_the_server_sees_what_the_local_tree_shows() {
        // (local children, server meta.numCollections).
        assert!(child_counts_agree(0, 0));
        assert!(child_counts_agree(3, 3));
        // The case that made this necessary: a child created upstream that the
        // local library has not synced down, so the summary would list none.
        assert!(!child_counts_agree(0, 1));
        assert!(!child_counts_agree(2, 3));
        // The other direction is no better a state to delete from.
        assert!(!child_counts_agree(1, 0));
    }

    #[test]
    fn delete_summary_always_states_that_items_survive_and_nothing_can_be_undone() {
        for key in ["DEEPER", "MID", "ROOT1"] {
            let summary = delete_summary(&scope_of(key, &[item("I1", &["DEEP"])]));
            assert!(summary.contains("NOT deleted, only unfiled"), "{key}: {summary}");
            assert!(summary.contains("cannot be undone"), "{key}: {summary}");
        }
    }
}
