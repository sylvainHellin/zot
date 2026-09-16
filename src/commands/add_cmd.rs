//! `zot add` -- add a paper to the library, locally, via Zotero's connector
//! endpoints.
//!
//! Paths:
//!   - identifier only: resolve DOI/arXiv -> BibTeX -> `/connector/import`.
//!   - `--pdf` (with or without identifier): `/connector/saveStandaloneAttachment`,
//!     then Zotero's recognizer creates the parent item. If an identifier was
//!     given and recognition fails, fall back to importing the metadata (the
//!     PDF stays standalone; `zot attach` can join them later).
//!
//! New items are detected by diffing top-level item versions before/after,
//! which works uniformly for both paths.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::api::connector::SaveTarget;
use crate::api::resolve::{self, Identifier};
use crate::api::{ConnectorClient, SearchParams, VersionConflict, WebApiClient, ZoteroClient};
use crate::collections::{CollectionNode, CollectionRef, build_tree, resolve_collection_ref};
use crate::output::{AddCollectionsOutput, AddOutput, AddedItemOutput, format_output};

/// How long to wait for a freshly added item to reach api.zotero.org before
/// giving up on filing the collections after the first. Zotero's auto-sync
/// normally takes seconds; this only has to outlast a slow one.
const SYNC_POLL_TIMEOUT: Duration = Duration::from_secs(60);

/// Gap between `get_item` probes while waiting for that sync.
const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub struct AddArgs<'a> {
    pub identifier: Option<&'a str>,
    pub pdf: Option<&'a str>,
    pub collections: Vec<String>,
    pub tags: Vec<String>,
    pub force: bool,
    pub no_index: bool,
    pub json: bool,
}

pub fn run_add(args: AddArgs) -> Result<()> {
    if args.identifier.is_none() && args.pdf.is_none() {
        bail!("Nothing to add: pass an identifier (DOI/arXiv) and/or --pdf <file>.");
    }

    let local = ZoteroClient::new()?;
    let connector = ConnectorClient::new()?;

    // Parse identifier + duplicate guard.
    let identifier = args
        .identifier
        .map(resolve::parse_identifier)
        .transpose()?;
    if let Some(id) = &identifier {
        if !args.force {
            check_duplicates(&local, id)?;
        }
    }

    // Read the PDF up front so a bad path fails before any write.
    let pdf = args
        .pdf
        .map(|p| -> Result<(Vec<u8>, String)> {
            let path = Path::new(p);
            let bytes =
                std::fs::read(path).with_context(|| format!("Failed to read PDF at {p}"))?;
            if !bytes.starts_with(b"%PDF") {
                bail!("{p} does not look like a PDF file");
            }
            let filename = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("attachment.pdf")
                .to_string();
            Ok((bytes, filename))
        })
        .transpose()?;

    // Resolve every collection before writing anything, so a typo in the
    // second value fails before the item exists.
    let wanted = ResolvedCollections::resolve(&connector, &local, &args.collections)?;
    let target = wanted.connector_target();

    // The connector files into one collection only; the rest go through the
    // web API afterwards. Fail now rather than after the item was created when
    // no API key is configured.
    let web = if wanted.extra_keys().is_empty() {
        None
    } else {
        Some(WebApiClient::from_config().context(
            "Filing into more than one collection needs the Zotero web API: the connector \
             saves into a single collection, so the others are written afterwards.",
        )?)
    };

    let tags_str = if args.tags.is_empty() {
        None
    } else {
        Some(args.tags.join(","))
    };

    // Snapshot versions to detect what the add created.
    let before = local.fetch_item_versions()?;

    let mut warnings: Vec<String> = Vec::new();
    let session = ConnectorClient::new_session_id();

    match (&identifier, &pdf) {
        // Metadata-only import.
        (Some(id), None) => {
            let bibtex = resolve::fetch_bibtex(id)?;
            eprintln!("Resolved {} -> importing into Zotero...", id.display());
            connector.import(&bibtex, &session)?;
            connector.update_session(&session, Some(&target), tags_str.as_deref())?;
        }
        // PDF (with optional identifier for dedup/fallback).
        (id_opt, Some((bytes, filename))) => {
            eprintln!("Saving PDF ({} KB) to Zotero...", bytes.len() / 1024);
            let resp =
                connector.save_standalone_attachment(bytes.clone(), filename, &session)?;
            let can_recognize = resp
                .get("canRecognize")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut recognized = false;
            if can_recognize {
                eprintln!("Waiting for Zotero to recognize the PDF metadata...");
                match connector.get_recognized_item(&session) {
                    Ok(Some(item)) => {
                        recognized = true;
                        if let Some(title) = item.get("title").and_then(|t| t.as_str()) {
                            eprintln!("Recognized: {title}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warnings.push(format!("Recognition check failed: {e:#}")),
                }
            }
            connector.update_session(&session, Some(&target), tags_str.as_deref())?;

            if !recognized {
                if let Some(id) = id_opt {
                    // Fallback: import the metadata so the item exists; the PDF
                    // stays a standalone attachment.
                    warnings.push(format!(
                        "PDF metadata recognition failed; imported metadata for {} separately. \
                         The PDF was saved as a standalone attachment -- join them with \
                         `zot attach <item-key> <file>` (or in the Zotero UI).",
                        id.display()
                    ));
                    let bibtex = resolve::fetch_bibtex(id)?;
                    let fallback_session = ConnectorClient::new_session_id();
                    connector.import(&bibtex, &fallback_session)?;
                    connector.update_session(
                        &fallback_session,
                        Some(&target),
                        tags_str.as_deref(),
                    )?;
                } else {
                    warnings.push(
                        "Zotero could not recognize metadata from the PDF; it was saved as a \
                         standalone attachment."
                            .to_string(),
                    );
                }
            } else if let Some(id) = id_opt {
                warnings.push(format!(
                    "Metadata came from Zotero's PDF recognizer; verify it matches {}.",
                    id.display()
                ));
            }
        }
        (None, None) => unreachable!(),
    }

    // Diff versions to find what was created.
    let after = local.fetch_item_versions()?;
    let new_keys: Vec<String> = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .cloned()
        .collect();

    let mut added: Vec<AddedItemOutput> = Vec::new();
    if !new_keys.is_empty() {
        let items = local.fetch_items(&new_keys)?;
        for item in items {
            added.push(AddedItemOutput {
                key: item.key.clone(),
                title: item.data.title.clone(),
                item_type: item.data.item_type.clone(),
                creators: item.creators_string(),
                date: item.data.date.clone(),
                doi: item.data.doi.clone(),
            });
        }
    } else {
        warnings.push(
            "Could not identify the newly created item (no new top-level keys found).".to_string(),
        );
    }

    // File the collections after the first, once Zotero has synced the item up.
    let extra_keys = wanted.extra_keys();
    let mut pending: Vec<String> = Vec::new();
    let mut fix_command: Option<String> = None;
    // `web` is Some exactly when there are collections left to file.
    if let Some(web) = &web {
        // With a PDF fallback the add can create a standalone attachment
        // alongside the item; the collections belong on the item.
        let item_key = added
            .iter()
            .find(|a| a.item_type != "attachment" && a.item_type != "note")
            .or_else(|| added.first())
            .map(|a| a.key.clone());
        match item_key {
            Some(item_key) => {
                if let Err(reason) = file_extra_collections(web, &item_key, &extra_keys) {
                    // The item exists and is filed in the first collection, so
                    // this is a partial success: report the key and the exact
                    // command that finishes the job, and exit 0.
                    let command = edit_command(&item_key, &extra_keys);
                    warnings.push(format!(
                        "Item [{item_key}] is only partially filed: it is in {}, but not in {}.\n  \
                         {reason:#}\n  Finish filing it with: {command}",
                        wanted.render_first(),
                        wanted.render_extra(),
                    ));
                    pending = wanted.extra_labels();
                    fix_command = Some(command);
                }
            }
            None => {
                pending = wanted.extra_labels();
                warnings.push(format!(
                    "The collections after the first ({}) were not written, because the new \
                     item's key could not be determined.",
                    wanted.render_extra(),
                ));
            }
        }
    }

    let output = AddOutput {
        added,
        collections: wanted.output(pending, fix_command),
        warnings: warnings.clone(),
    };
    println!("{}", format_output(&output, args.json));

    // Refresh the search index so the item is immediately findable.
    if !args.no_index {
        eprintln!("\nUpdating search index...");
        if let Err(e) = super::index_cmd::run_index(false, args.json) {
            eprintln!("Warning: index update failed: {e:#}\n  Run `zot index` manually.");
        }
    }

    Ok(())
}

/// Refuse to add when the identifier already matches library items.
///
/// The `everything` quicksearch also matches PDF fulltext, so a well-cited
/// DOI hits every paper whose references mention it. Only count a hit as a
/// duplicate when the identifier appears in an actual metadata field (DOI,
/// URL, or extra) of a regular item.
fn check_duplicates(local: &ZoteroClient, id: &Identifier) -> Result<()> {
    let params = SearchParams {
        everything: true,
        limit: Some(50),
        ..Default::default()
    };
    let needle = id.dedup_query().to_lowercase();
    let hits: Vec<_> = local
        .search_items(id.dedup_query(), &params)?
        .into_iter()
        .filter(|i| {
            if !i.is_regular_item() {
                return false;
            }
            let extra = i
                .data
                .extra
                .get("extra")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            i.data.doi.to_lowercase().contains(&needle)
                || i.data.url.to_lowercase().contains(&needle)
                || extra.to_lowercase().contains(&needle)
        })
        .collect();
    if !hits.is_empty() {
        let listing: Vec<String> = hits
            .iter()
            .map(|i| format!("  [{}] {}", i.key, i.data.title))
            .collect();
        bail!(
            "{} already appears to be in the library:\n{}\n  Use --force to add anyway.",
            id.display(),
            listing.join("\n")
        );
    }
    Ok(())
}

/// Every `--collection` value resolved to a write target, before anything is
/// written.
///
/// The first collection is the connector save target (the connector takes one
/// and only one), so it needs a tree-view ID; the rest are written by a web API
/// PATCH, so they need collection keys.
#[derive(Debug)]
struct ResolvedCollections {
    refs: Vec<CollectionRef>,
}

impl ResolvedCollections {
    /// Resolve every value, fetching the two views of the tree once.
    ///
    /// With no `--collection` at all nothing is fetched: that path must stay as
    /// cheap as it was before collections existed.
    fn resolve(
        connector: &ConnectorClient,
        local: &ZoteroClient,
        inputs: &[String],
    ) -> Result<Self> {
        if inputs.is_empty() {
            return Ok(Self { refs: Vec::new() });
        }
        let targets = connector.list_targets()?;
        let nodes = build_tree(&local.fetch_collections()?);
        Self::from_parts(inputs, &nodes, &targets)
    }

    /// The pure half of [`Self::resolve`]: no network, so it is unit-testable.
    fn from_parts(
        inputs: &[String],
        nodes: &[CollectionNode],
        targets: &[SaveTarget],
    ) -> Result<Self> {
        let multi = inputs.len() > 1;
        let mut refs: Vec<CollectionRef> = Vec::new();

        for input in inputs {
            let resolved = resolve_collection_ref(input, nodes, targets)?;

            if resolved.key.is_none() {
                let id = resolved.connector_id.as_deref().unwrap_or(input);
                // A library root is a legitimate connector target on its own
                // (it is the default, and a group library's root is reachable
                // no other way), but it has no collection key, which is what
                // every value after the first is written as.
                if id.starts_with('L') && multi {
                    bail!(
                        "--collection {input}: {id} is a library root, not a collection, so it \
                         cannot be one of several --collection values.\n  Only the first is \
                         handed to the connector; the rest are written by collection key, which \
                         a library root does not have.\n  Pass {id} on its own, or name real \
                         collections."
                    );
                }
                if multi {
                    bail!(
                        "--collection {input}: collection \"{}\" ({id}) is only known to the \
                         connector, most likely because it lives in a group library.\n  Filing \
                         into several collections writes the extra ones through the web API, \
                         which only reaches your own library, so pass a single --collection.",
                        resolved.name,
                    );
                }
            }

            if let Some(dup) = refs.iter().find(|r| same_collection(r, &resolved)) {
                bail!(
                    "--collection {input} names the same collection as an earlier value: {}.",
                    render(dup),
                );
            }
            refs.push(resolved);
        }

        // Only the first goes through the connector, so only the first needs a
        // tree-view ID.
        if let Some(first) = refs.first() {
            if first.connector_id.is_none() {
                bail!(
                    "Collection \"{}\" ({}) has no connector save target, so `zot add` cannot \
                     file into it.\n  Pass a tree-view ID (e.g. C42) instead.",
                    first.name,
                    first.key.as_deref().unwrap_or(&inputs[0]),
                );
            }
        }
        Ok(Self { refs })
    }

    /// Connector save target: the first collection, or the library root when
    /// none was named, so an add is deterministic regardless of what is
    /// selected in the Zotero UI.
    fn connector_target(&self) -> String {
        match self.refs.first().and_then(|r| r.connector_id.clone()) {
            Some(id) => id,
            None => crate::collections::LIBRARY_ROOT_ID.to_string(),
        }
    }

    /// Keys of the collections the connector cannot file into, i.e. all but the
    /// first. Every one of them has a key (checked in [`Self::from_parts`]).
    fn extra_keys(&self) -> Vec<String> {
        self.refs
            .iter()
            .skip(1)
            .filter_map(|r| r.key.clone())
            .collect()
    }

    fn extra_labels(&self) -> Vec<String> {
        self.refs.iter().skip(1).map(render).collect()
    }

    fn render_first(&self) -> String {
        self.refs.first().map(render).unwrap_or_default()
    }

    fn render_extra(&self) -> String {
        self.extra_labels().join(", ")
    }

    /// Membership to report on the added item. `pending` is empty unless the
    /// extra collections could not be written.
    fn output(
        &self,
        pending: Vec<String>,
        fix_command: Option<String>,
    ) -> Option<AddCollectionsOutput> {
        if self.refs.is_empty() {
            return None;
        }
        let filed: Vec<String> = self
            .refs
            .iter()
            .map(render)
            .filter(|label| !pending.contains(label))
            .collect();
        Some(AddCollectionsOutput {
            filed,
            pending,
            fix_command,
        })
    }
}

/// `Name (KEY)`, falling back to the tree-view ID for a collection the read API
/// does not list.
fn render(r: &CollectionRef) -> String {
    let id = r
        .key
        .as_deref()
        .or(r.connector_id.as_deref())
        .unwrap_or("?");
    format!("{} ({id})", r.name)
}

/// Two references point at the same collection when their keys match, or, for a
/// collection with no key, their tree-view IDs do.
fn same_collection(a: &CollectionRef, b: &CollectionRef) -> bool {
    match (&a.key, &b.key) {
        (Some(x), Some(y)) => x == y,
        (None, None) => a.connector_id.is_some() && a.connector_id == b.connector_id,
        _ => false,
    }
}

/// `zot edit KEY --add-collection A --add-collection B`: the command that
/// finishes filing by hand after a sync that did not arrive in time.
fn edit_command(item_key: &str, collection_keys: &[String]) -> String {
    let mut cmd = format!("zot edit {item_key}");
    for key in collection_keys {
        cmd.push_str(" --add-collection ");
        cmd.push_str(key);
    }
    cmd
}

/// Add `extra_keys` to the item's collection membership through the web API.
///
/// The item only exists on api.zotero.org once Zotero has synced it up, so the
/// read is retried for [`SYNC_POLL_TIMEOUT`]. The membership sent is the full
/// array, merged from the same read whose version guards the write.
fn file_extra_collections(web: &WebApiClient, item_key: &str, extra_keys: &[String]) -> Result<()> {
    let item = poll_for_item(web, item_key, extra_keys.len())?;
    let version = item
        .get("version")
        .and_then(|v| v.as_u64())
        .context("No version on item")?;
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

    let merged = super::edit_cmd::merge_collections(&current, extra_keys, &[]);
    if merged == current {
        return Ok(());
    }
    let data = json!({ "collections": merged });
    if let Err(e) = web.patch_item(item_key, &data, version) {
        // A 412 is likely here: the item is being synced up at the moment it is
        // read and patched. Report it in this command's terms -- re-running
        // `zot add` would create a second item, so the caller's `zot edit`
        // advice is the only remedy worth printing.
        if e.downcast_ref::<VersionConflict>().is_some() {
            bail!(
                "[{item_key}] changed on api.zotero.org between the read and the write \
                 (version {version}), so the extra collections were not written."
            );
        }
        return Err(e);
    }
    Ok(())
}

/// Read the item from api.zotero.org, retrying until Zotero has synced it up.
///
/// Progress goes to stderr, never stdout, so a minute of waiting does not look
/// like a hang while `--json` stdout stays a single parseable document.
fn poll_for_item(web: &WebApiClient, item_key: &str, extra: usize) -> Result<Value> {
    let started = Instant::now();
    let mut last_report = Duration::ZERO;
    eprintln!(
        "\nWaiting for Zotero to sync [{item_key}] to api.zotero.org, to file it in {extra} \
         more collection(s) (up to {}s)...",
        SYNC_POLL_TIMEOUT.as_secs(),
    );
    loop {
        // Only the 404 means "not synced up yet"; a revoked key or a downed
        // network is not going to fix itself within the timeout.
        if let Some(item) = web.get_item_opt(item_key)? {
            return Ok(item);
        }
        let elapsed = started.elapsed();
        if elapsed + SYNC_POLL_INTERVAL >= SYNC_POLL_TIMEOUT {
            let waited = SYNC_POLL_TIMEOUT.as_secs();
            bail!("Zotero did not sync [{item_key}] up to api.zotero.org within {waited}s.");
        }
        if elapsed - last_report >= Duration::from_secs(10) {
            last_report = elapsed;
            eprintln!(
                "  still waiting ({}s of {}s)...",
                elapsed.as_secs(),
                SYNC_POLL_TIMEOUT.as_secs(),
            );
        }
        std::thread::sleep(SYNC_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolvedCollections, edit_command};
    use crate::api::connector::SaveTarget;
    use crate::api::models::ZoteroCollection;
    use crate::collections::{CollectionNode, build_tree};
    use crate::commands::edit_cmd::merge_collections;
    use crate::output::{AddOutput, format_output};
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

    /// A tree plus the connector targets mirroring it, with one extra target
    /// (`C9`, under the group library `L2`) that no key backs, and one
    /// collection (`ROOT3`) the connector does not report.
    fn resolvable() -> (Vec<CollectionNode>, Vec<SaveTarget>) {
        let nodes = build_tree(&[
            collection("ROOT1", "Alpha", None),
            collection("CHILD1", "Inbox", Some("ROOT1")),
            collection("ROOT2", "Beta", None),
            collection("ROOT3", "Unreported", None),
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

    fn resolved(inputs: &[&str]) -> anyhow::Result<ResolvedCollections> {
        let (nodes, targets) = resolvable();
        ResolvedCollections::from_parts(&v(inputs), &nodes, &targets)
    }

    #[test]
    fn no_collection_targets_the_library_root() {
        let r = resolved(&[]).unwrap();
        assert_eq!(r.connector_target(), "L1");
        assert!(r.extra_keys().is_empty());
        assert!(r.output(Vec::new(), None).is_none());
    }

    #[test]
    fn one_collection_needs_no_web_api_write() {
        let r = resolved(&["Inbox"]).unwrap();
        assert_eq!(r.connector_target(), "C2");
        // Empty: nothing is left for the web API, so no key is required.
        assert!(r.extra_keys().is_empty());
        let out = r.output(Vec::new(), None).unwrap();
        assert_eq!(out.filed, v(&["Inbox (CHILD1)"]));
        assert!(out.pending.is_empty());
    }

    #[test]
    fn the_first_collection_is_the_connector_target_and_the_rest_are_keys() {
        let r = resolved(&["Alpha", "CHILD1", "C3"]).unwrap();
        assert_eq!(r.connector_target(), "C1");
        assert_eq!(r.extra_keys(), v(&["CHILD1", "ROOT2"]));
        assert_eq!(r.extra_labels(), v(&["Inbox (CHILD1)", "Beta (ROOT2)"]));
    }

    #[test]
    fn a_repeated_collection_is_rejected() {
        // The same collection reached three ways: name, key, tree-view ID.
        for pair in [
            ["Inbox", "CHILD1"],
            ["CHILD1", "C2"],
            ["C2", "Inbox"],
            ["Inbox", "Inbox"],
        ] {
            let err = resolved(&pair).unwrap_err().to_string();
            assert!(err.contains("same collection as an earlier value"), "{err}");
            assert!(err.contains("Inbox (CHILD1)"), "{err}");
        }
    }

    #[test]
    fn a_library_root_alone_is_the_connector_target() {
        // L1 is the default target anyway; L2, a group library's root, is
        // reachable no other way.
        for id in ["L1", "L2"] {
            let r = resolved(&[id]).unwrap();
            assert_eq!(r.connector_target(), id);
            assert!(r.extra_keys().is_empty());
        }
    }

    #[test]
    fn a_library_root_is_rejected_alongside_other_collections() {
        for inputs in [vec!["Alpha", "L1"], vec!["L1", "Alpha"], vec!["L2", "Alpha"]] {
            let err = resolved(&inputs).unwrap_err().to_string();
            assert!(err.contains("is a library root, not a collection"), "{err}");
            assert!(err.contains("one of several --collection values"), "{err}");
        }
    }

    #[test]
    fn an_unknown_collection_is_rejected_before_anything_is_written() {
        let err = resolved(&["Alpha", "Nope"]).unwrap_err().to_string();
        assert!(err.contains("Collection not found: Nope"), "{err}");
    }

    #[test]
    fn a_connector_only_collection_is_rejected_alongside_others() {
        // Fine on its own (the connector can file into a group library)...
        let r = resolved(&["Group Only"]).unwrap();
        assert_eq!(r.connector_target(), "C9");
        // ...but the extra collections go through the web API, which cannot.
        let err = resolved(&["Alpha", "Group Only"]).unwrap_err().to_string();
        assert!(err.contains("only known to the connector"), "{err}");
        let err = resolved(&["Group Only", "Alpha"]).unwrap_err().to_string();
        assert!(err.contains("group library"), "{err}");
    }

    #[test]
    fn a_first_collection_without_a_connector_target_is_rejected() {
        let err = resolved(&["ROOT3"]).unwrap_err().to_string();
        assert!(err.contains("no connector save target"), "{err}");
        // Only the first has to be reachable through the connector.
        let r = resolved(&["Alpha", "ROOT3"]).unwrap();
        assert_eq!(r.connector_target(), "C1");
        assert_eq!(r.extra_keys(), v(&["ROOT3"]));
    }

    #[test]
    fn partial_filing_separates_what_landed_from_what_did_not() {
        let r = resolved(&["Alpha", "CHILD1", "C3"]).unwrap();
        let pending = r.extra_labels();
        let command = edit_command("ITEMKEY1", &r.extra_keys());
        let out = r.output(pending, Some(command.clone())).unwrap();
        assert_eq!(out.filed, v(&["Alpha (ROOT1)"]));
        assert_eq!(out.pending, v(&["Inbox (CHILD1)", "Beta (ROOT2)"]));
        assert_eq!(out.fix_command.as_deref(), Some(command.as_str()));
    }

    #[test]
    fn the_degraded_state_is_visible_in_both_output_modes() {
        let r = resolved(&["Alpha", "C3"]).unwrap();
        let command = edit_command("ITEMKEY1", &r.extra_keys());
        let output = AddOutput {
            added: Vec::new(),
            collections: r.output(r.extra_labels(), Some(command)),
            warnings: vec!["Item [ITEMKEY1] is only partially filed".to_string()],
        };

        let human = format_output(&output, false);
        assert!(human.contains("Partially filed: in Alpha (ROOT1)"), "{human}");
        assert!(human.contains("NOT in: Beta (ROOT2)"), "{human}");
        assert!(
            human.contains("Finish with: zot edit ITEMKEY1 --add-collection ROOT2"),
            "{human}"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&format_output(&output, true)).expect("valid JSON");
        assert_eq!(parsed["collections"]["filed"], json!(["Alpha (ROOT1)"]));
        assert_eq!(parsed["collections"]["pending"], json!(["Beta (ROOT2)"]));
        assert_eq!(
            parsed["collections"]["fix_command"],
            json!("zot edit ITEMKEY1 --add-collection ROOT2")
        );

        // The happy path carries neither a pending list nor a fix command.
        let ok = AddOutput {
            added: Vec::new(),
            collections: r.output(Vec::new(), None),
            warnings: Vec::new(),
        };
        assert!(format_output(&ok, false).contains("Filed in: Alpha (ROOT1), Beta (ROOT2)"));
        let parsed: serde_json::Value =
            serde_json::from_str(&format_output(&ok, true)).expect("valid JSON");
        assert_eq!(parsed["collections"]["pending"], json!(null));
        assert_eq!(parsed["collections"]["fix_command"], json!(null));
    }

    #[test]
    fn the_hand_filing_command_names_every_missing_collection() {
        assert_eq!(
            edit_command("ITEMKEY1", &v(&["CHILD1", "ROOT2"])),
            "zot edit ITEMKEY1 --add-collection CHILD1 --add-collection ROOT2"
        );
        assert_eq!(edit_command("ITEMKEY1", &[]), "zot edit ITEMKEY1");
    }

    #[test]
    fn the_extra_collections_are_merged_into_existing_membership() {
        // The connector already filed the item in the first collection, and the
        // PATCH sends the complete array, so it must carry that one too.
        assert_eq!(
            merge_collections(&v(&["ROOT1"]), &v(&["CHILD1", "ROOT2"]), &[]),
            v(&["ROOT1", "CHILD1", "ROOT2"])
        );
        // A collection the item is already in is not duplicated.
        assert_eq!(
            merge_collections(&v(&["ROOT1", "CHILD1"]), &v(&["CHILD1"]), &[]),
            v(&["ROOT1", "CHILD1"])
        );
        // Nothing to add: equal to the current membership, so no write.
        let current = v(&["ROOT1", "CHILD1"]);
        assert_eq!(merge_collections(&current, &v(&["CHILD1"]), &[]), current);
    }
}
