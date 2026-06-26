use anyhow::Result;

use crate::api::ZoteroClient;
use crate::index::{BgeSmallEmbedder, BgeRerankerBase, IndexStore, SearchFilters, compute_sync_diff};
use crate::output::{format_output, SearchOutput, SearchResultOutput};
use crate::search::hybrid_search;

#[allow(clippy::too_many_arguments)]
pub fn run_search(
    query: &str,
    tag: Option<&str>,
    creator: Option<&str>,
    item_type: Option<&str>,
    collection: Option<&str>,
    limit: usize,
    rerank: bool,
    no_sync_check: bool,
    json: bool,
) -> Result<()> {
    let store = IndexStore::open_or_create("BGESmallENV15", 384)?;

    if store.meta().chunk_count == 0 {
        anyhow::bail!("Index is empty. Run `zot index` first.");
    }

    // Check whether the local index is in sync with Zotero before querying it.
    // This is the only place `search` touches Zotero; it must stay non-fatal so
    // search keeps working offline. The result is surfaced as a note on the
    // output (top line in human mode, `note` field in JSON) rather than an error.
    let note = if no_sync_check || std::env::var_os("ZOT_NO_SYNC_CHECK").is_some() {
        None
    } else {
        sync_note(&store)
    };

    eprintln!("Loading embedding model...");
    let mut embedder = BgeSmallEmbedder::new()?;

    let mut reranker_instance = if rerank {
        eprintln!("Loading reranker model...");
        Some(BgeRerankerBase::new()?)
    } else {
        None
    };

    let filters = SearchFilters {
        tag: tag.map(String::from),
        creator: creator.map(String::from),
        item_type: item_type.map(String::from),
        collection: collection.map(String::from),
    };

    eprintln!("Searching...");
    let results = hybrid_search(
        &store,
        &mut embedder,
        reranker_instance.as_mut().map(|r| r as &mut dyn crate::index::Reranker),
        query,
        &filters,
        limit,
    )?;

    let output = SearchOutput {
        query: query.to_string(),
        result_count: results.len(),
        results: results
            .into_iter()
            .map(|r| SearchResultOutput {
                key: r.chunk.item_key,
                title: r.chunk.title,
                item_type: r.chunk.item_type,
                creators: r.chunk.creators,
                date: r.chunk.date,
                score: r.score,
                snippet: r.chunk.text,
                char_start: r.chunk.char_start,
                char_end: r.chunk.char_end,
                chunk_type: r.chunk.chunk_type,
            })
            .collect(),
        // Carried on the output object so it renders at the top in human mode and
        // appears as a `note` field in JSON. Omitted entirely when in sync.
        note,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}

/// Build an index-freshness note, or `None` if the index is in sync.
///
/// Reaches Zotero's local API for the current version map and diffs it against
/// the index. Any failure to reach Zotero (app closed, local API disabled) is
/// turned into a note saying freshness could not be verified, never an error --
/// search must keep working from the local index alone.
fn sync_note(store: &IndexStore) -> Option<String> {
    let client = match ZoteroClient::new() {
        Ok(c) => c,
        Err(_) => {
            return Some(
                "Note: Zotero not reachable -- cannot verify the index is up to date.".to_string(),
            );
        }
    };

    let remote_versions = match client.fetch_item_versions() {
        Ok(v) => v,
        Err(_) => {
            return Some(
                "Note: Zotero not reachable -- cannot verify the index is up to date.".to_string(),
            );
        }
    };

    let diff = compute_sync_diff(&remote_versions, store.item_versions());
    if !diff.is_stale() {
        return None;
    }
    Some(format!(
        "Note: index may be out of date -- {} new/updated, {} removed since last sync. \
         Run `zot index` to update.",
        diff.to_add.len(),
        diff.to_delete.len(),
    ))
}
