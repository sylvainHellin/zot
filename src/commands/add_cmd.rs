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

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

use crate::api::resolve::{self, Identifier};
use crate::api::{ConnectorClient, SearchParams, ZoteroClient};
use crate::collections::{LIBRARY_ROOT_ID, build_tree, resolve_collection_ref};
use crate::output::{AddOutput, AddedItemOutput, format_output};

pub struct AddArgs<'a> {
    pub identifier: Option<&'a str>,
    pub pdf: Option<&'a str>,
    pub collection: Option<&'a str>,
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

    // Resolve the save target before writing anything.
    let target = resolve_target(&connector, &local, args.collection)?;
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

    let output = AddOutput {
        added,
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

/// Resolve `--collection` (collection key, name, or raw tree-view ID) to a
/// connector tree-view target ID. Defaults to "L1" (My Library root) so adds
/// are deterministic regardless of what is selected in the Zotero UI.
fn resolve_target(
    connector: &ConnectorClient,
    local: &ZoteroClient,
    collection: Option<&str>,
) -> Result<String> {
    let Some(wanted) = collection else {
        return Ok(LIBRARY_ROOT_ID.to_string());
    };

    let targets = connector.list_targets()?;
    let nodes = build_tree(&local.fetch_collections()?);
    let resolved = resolve_collection_ref(wanted, &nodes, &targets)?;

    resolved.connector_id.ok_or_else(|| {
        anyhow!(
            "Collection \"{}\" ({}) has no connector save target, so `zot add` cannot file into \
             it.\n  Pass a tree-view ID (e.g. C42) instead.",
            resolved.name,
            resolved.key.as_deref().unwrap_or(wanted),
        )
    })
}
