//! `zot add` -- add a paper to the library, locally, via Zotero's connector
//! endpoints.
//!
//! Paths:
//!   - identifier only: resolve DOI/arXiv/ISBN/PMID -> BibTeX -> `/connector/import`.
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
    pub no_collection: bool,
    pub tags: Vec<String>,
    pub force: bool,
    pub no_index: bool,
    pub json: bool,
}

pub fn run_add(args: AddArgs) -> Result<()> {
    check_collection_flags(args.no_collection, &args.collections)?;
    if args.identifier.is_none() && args.pdf.is_none() {
        bail!("Nothing to add: pass an identifier (DOI/arXiv/ISBN/PMID) and/or --pdf <file>.");
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
            .find(|a| !is_standalone(a))
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

    if let Some(warning) = unfiled_warning(&args.collections, args.no_collection, &added) {
        eprintln!("{warning}");
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

/// An attachment or note with no parent of its own: `zot unfiled` calls it a
/// stray, and it is not the item the collections belong on.
fn is_standalone(a: &AddedItemOutput) -> bool {
    a.item_type == "attachment" || a.item_type == "note"
}

/// The stderr warning for an add that landed in the library root without being
/// asked to, or `None` when there is nothing to warn about.
///
/// An item in the library root is invisible to every collection-based view, so
/// silence would hide it. The warning goes to stderr and deliberately not into
/// `warnings`: that list is the record of a requested write that did not land,
/// and a script watching it would start flagging every deliberate root add. The
/// JSON carries the same fact, as `collections: null`.
///
/// A failed PDF recognition creates a standalone attachment and nothing else.
/// That key is a stray to reparent, which is exactly what `zot unfiled` says
/// about it, so the warning has to say the same rather than tell the user to
/// file it.
fn unfiled_warning(
    collections: &[String],
    no_collection: bool,
    added: &[AddedItemOutput],
) -> Option<String> {
    if !collections.is_empty() || no_collection {
        return None;
    }
    let item = added
        .iter()
        .find(|a| !is_standalone(a))
        .or_else(|| added.first())?;
    let key = &item.key;
    let silence = "Pass --no-collection to add to the library root on purpose and silence this.";
    if is_standalone(item) {
        let (kind, remedy) = if item.item_type == "note" {
            ("note", "Reparent it in the Zotero UI.".to_string())
        } else {
            (
                "attachment",
                "Put it under its item with: zot attach <item-key> <file> (or reparent it in \
                 the Zotero UI)."
                    .to_string(),
            )
        };
        Some(format!(
            "\nWarning: no --collection given, and [{key}] was saved as a standalone {kind}, \
             which `zot unfiled` reports as a stray to reparent, not as an item to file.\n  \
             {remedy}\n  {silence}"
        ))
    } else {
        Some(format!(
            "\nWarning: no --collection given, so [{key}] is an unfiled item.\n  \
             File it with: zot edit {key} --add-collection <key|name|C42>\n  {silence}"
        ))
    }
}

/// `--no-collection` is the opt-out from filing, so naming a collection
/// alongside it asks for two different things at once.
///
/// Checked before anything else in [`run_add`], so the contradiction costs no
/// network call and can never half-write an item.
fn check_collection_flags(no_collection: bool, collections: &[String]) -> Result<()> {
    if no_collection && !collections.is_empty() {
        bail!(
            "--no-collection contradicts --collection {}: one files the item, the other \
             deliberately does not.\n  Drop --no-collection to file it, or drop --collection \
             to add it to the library root.",
            collections.join(" --collection "),
        );
    }
    Ok(())
}

/// Refuse to add when the identifier already matches library items.
///
/// The `everything` quicksearch also matches PDF fulltext, so a well-cited
/// DOI hits every paper whose references mention it. Only count a hit as a
/// duplicate when the identifier appears in an actual metadata field (DOI,
/// URL, or extra) of a regular item.
///
/// An ISBN does not go through the quicksearch at all. Zotero stores an ISBN
/// hyphenated (`9781119287537` is rewritten to `978-1-119-28753-7` on import)
/// and the quicksearch matches substrings but not across the hyphens, so no
/// query on the bare digits can return the book. The books are enumerated and
/// their ISBN fields compared instead; see [`isbn_field_matches`].
///
/// One gap is left: a PMID resolved through its DOI produces an item carrying
/// that DOI and no PMID, so re-adding the same PMID is not caught; adding its
/// DOI is. That is a gap in the guard, not a reason to reach for `--force`.
fn check_duplicates(local: &ZoteroClient, id: &Identifier) -> Result<()> {
    let params = SearchParams {
        everything: true,
        limit: Some(50),
        ..Default::default()
    };
    let needle = id.dedup_query().to_lowercase();
    let mut hits: Vec<_> = local
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

    // The second, ISBN-only pass: an empty query listing every book, which a
    // personal library counts in the tens, then a field comparison the
    // quicksearch cannot do.
    if let Identifier::Isbn(isbn) = id {
        let books = SearchParams {
            everything: true,
            item_type: Some("book".to_string()),
            limit: Some(100),
            ..Default::default()
        };
        for item in local.search_items("", &books)? {
            let field = item
                .data
                .extra
                .get("ISBN")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if isbn_field_matches(field, isbn) && !hits.iter().any(|h| h.key == item.key) {
                hits.push(item);
            }
        }
    }

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

/// True when a Zotero `ISBN` field names the book `needle` identifies.
///
/// Two things make this more than a string compare. The field is stored
/// hyphenated, so both sides are reduced to their alphanumerics (an ISBN-10
/// check digit may be `X`, hence alphanumerics and not digits). And the field
/// can hold several ISBNs separated by whitespace, the print and the
/// electronic edition, so each token is compared on its own rather than the
/// field as a whole.
///
/// `needle` is compared as given: re-hyphenating it would need the ISBN range
/// table, which is not worth carrying to answer a yes/no question.
fn isbn_field_matches(field: &str, needle: &str) -> bool {
    fn core(s: &str) -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }
    let want = core(needle);
    if want.is_empty() {
        return false;
    }
    field.split_whitespace().any(|token| core(token) == want)
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
    use super::{
        ResolvedCollections, check_collection_flags, edit_command, isbn_field_matches,
        unfiled_warning,
    };
    use crate::api::connector::SaveTarget;
    use crate::api::models::ZoteroCollection;
    use crate::collections::{CollectionNode, build_tree};
    use crate::commands::edit_cmd::merge_collections;
    use crate::output::{AddOutput, AddedItemOutput, format_output};
    use serde_json::json;

    /// Only the key and the item type decide what the unfiled warning says.
    fn added(key: &str, item_type: &str) -> AddedItemOutput {
        AddedItemOutput {
            key: key.to_string(),
            title: format!("Title of {key}"),
            item_type: item_type.to_string(),
            creators: String::new(),
            date: String::new(),
            doi: String::new(),
        }
    }

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
    fn no_collection_flag_and_a_named_collection_are_rejected_together() {
        let err = check_collection_flags(true, &v(&["Alpha", "C3"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--no-collection contradicts --collection"), "{err}");
        assert!(err.contains("Alpha --collection C3"), "{err}");

        // Either flag on its own is fine, as is neither.
        assert!(check_collection_flags(true, &[]).is_ok());
        assert!(check_collection_flags(false, &v(&["Alpha"])).is_ok());
        assert!(check_collection_flags(false, &[]).is_ok());
    }

    #[test]
    fn an_unfiled_add_serialises_collections_as_an_explicit_null() {
        // The absent key would be the only machine-readable signal that the add
        // landed in the library root, and a strict consumer cannot test a key
        // that is not there: `d["collections"]` raises in Python, and
        // `data.collections === null` is false in JS.
        let out = AddOutput {
            added: vec![added("ITEMKEY1", "journalArticle")],
            collections: None,
            warnings: Vec::new(),
        };
        let text = format_output(&out, true);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(
            parsed.as_object().expect("object").contains_key("collections"),
            "{text}"
        );
        assert_eq!(parsed["collections"], json!(null));
    }

    #[test]
    fn an_add_with_no_collection_and_no_opt_out_warns_about_the_new_item() {
        let items = vec![added("ITEMKEY1", "journalArticle")];
        let w = unfiled_warning(&[], false, &items).expect("warning");
        assert!(w.contains("[ITEMKEY1] is an unfiled item"), "{w}");
        assert!(
            w.contains("File it with: zot edit ITEMKEY1 --add-collection <key|name|C42>"),
            "{w}"
        );
        assert!(w.contains("Pass --no-collection"), "{w}");
    }

    #[test]
    fn the_unfiled_warning_is_silent_whenever_the_root_add_was_deliberate() {
        let items = vec![added("ITEMKEY1", "journalArticle")];
        // Asked for the library root on purpose.
        assert!(unfiled_warning(&[], true, &items).is_none());
        // Filed somewhere, so nothing is unfiled.
        assert!(unfiled_warning(&v(&["Alpha"]), false, &items).is_none());
        // Nothing was created, so there is no key to name.
        assert!(unfiled_warning(&[], false, &[]).is_none());
    }

    #[test]
    fn the_unfiled_warning_names_the_item_rather_than_its_attachment() {
        // A recognized PDF add creates both; the collections, and the warning,
        // belong on the parent item whatever order they come back in.
        let items = vec![added("SLJFJADB", "attachment"), added("ITEMKEY1", "journalArticle")];
        let w = unfiled_warning(&[], false, &items).expect("warning");
        assert!(w.contains("[ITEMKEY1] is an unfiled item"), "{w}");
        assert!(!w.contains("SLJFJADB"), "{w}");
    }

    #[test]
    fn a_standalone_attachment_is_reported_as_a_stray_not_as_an_item_to_file() {
        // Failed recognition with no identifier: the attachment is all there is,
        // and `zot unfiled` calls it a stray, so telling the user to file it
        // would contradict the other command about the same key.
        let items = vec![added("SLJFJADB", "attachment")];
        let w = unfiled_warning(&[], false, &items).expect("warning");
        assert!(w.contains("[SLJFJADB] was saved as a standalone attachment"), "{w}");
        assert!(w.contains("stray to reparent, not as an item to file"), "{w}");
        assert!(w.contains("zot attach <item-key> <file>"), "{w}");
        assert!(!w.contains("--add-collection"), "{w}");

        let items = vec![added("NOTE0001", "note")];
        let w = unfiled_warning(&[], false, &items).expect("warning");
        assert!(w.contains("[NOTE0001] was saved as a standalone note"), "{w}");
        assert!(w.contains("Reparent it in the Zotero UI"), "{w}");
        assert!(!w.contains("--add-collection"), "{w}");
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

    #[test]
    fn a_hyphenated_stored_isbn_matches_the_bare_needle() {
        // Live from the library on 2026-09-16: QZ3UQGNL stores one ISBN,
        // SJR838XV two, and both are the same BIM Handbook.
        assert!(isbn_field_matches("978-1-119-28753-7", "9781119287537"));
        assert!(isbn_field_matches(
            "978-1-119-28753-7 978-1-119-28756-8",
            "9781119287537"
        ));
        // The second ISBN of that pair matches just as well.
        assert!(isbn_field_matches(
            "978-1-119-28753-7 978-1-119-28756-8",
            "9781119287568"
        ));
        // An ISBN-10 X check digit, either case, on either side.
        assert!(isbn_field_matches("0-439-42089-X", "043942089X"));
        assert!(isbn_field_matches("043942089x", "043942089X"));
        // A book with no ISBN recorded matches nothing, and neither does an
        // empty needle against a stored value.
        assert!(!isbn_field_matches("", "9781119287537"));
        assert!(!isbn_field_matches("978-1-119-28753-7", ""));
        // A different book, and a near miss in the check digit.
        assert!(!isbn_field_matches("978-0-262-03561-3", "9781119287537"));
        assert!(!isbn_field_matches("978-1-119-28753-7", "9781119287538"));
        // The tokens are compared one at a time: the concatenation of a
        // two-ISBN field is not an ISBN anyone can hold.
        assert!(!isbn_field_matches(
            "978-1-119-28753-7 978-1-119-28756-8",
            "97811192875379781119287568"
        ));
    }
}
