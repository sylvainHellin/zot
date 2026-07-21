use anyhow::Result;
use std::collections::HashMap;

use crate::api::ZoteroClient;
use crate::index::{
    BgeSmallEmbedder, Embedder, IndexStore, IndexableItem, ItemText, chunk_item,
};
use crate::output::{IndexStatusOutput, format_output};

/// Commit + persist progress every this many regular items, so an interrupted
/// run (crash, OOM, Ctrl-C) leaves a consistent index that the next run resumes.
const CHECKPOINT_EVERY: usize = 20;

pub fn run_index(force: bool, _json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;
    let mut embedder = BgeSmallEmbedder::new()?;

    let mut store = IndexStore::open_or_create(embedder.name(), embedder.dim())?;

    if force {
        eprintln!("Force rebuild: clearing existing index...");
        store.clear()?;
    }

    // 1. Fetch current versions from Zotero
    eprintln!("Fetching item versions from Zotero...");
    let remote_versions = client.fetch_item_versions()?;
    let local_versions = store.item_versions().clone();

    // 2. Compute diff
    let mut to_add: Vec<String> = Vec::new();
    let mut to_delete: Vec<String> = Vec::new();

    for (key, remote_ver) in &remote_versions {
        match local_versions.get(key) {
            Some(local_ver) if local_ver == remote_ver => {} // unchanged
            _ => to_add.push(key.clone()),                   // new or updated
        }
    }

    for key in local_versions.keys() {
        if !remote_versions.contains_key(key) {
            to_delete.push(key.clone());
        }
    }

    let total_remote = remote_versions.len();
    eprintln!(
        "Library: {} items. To index: {} new/updated, {} deleted, {} unchanged.",
        total_remote,
        to_add.len(),
        to_delete.len(),
        total_remote - to_add.len(),
    );

    // 3. Delete removed items
    if !to_delete.is_empty() {
        eprintln!("Removing {} deleted items...", to_delete.len());
        store.delete_items(&to_delete)?;
    }

    // 4. Fetch and index new/updated items
    // Track items that were actually indexed with >=1 chunk this run, mapped to
    // their remote version, so only those get a recorded version in finalize.
    let mut indexed_versions: HashMap<String, u64> = HashMap::new();
    if !to_add.is_empty() {
        eprintln!("Fetching {} items from Zotero...", to_add.len());
        let items = client.fetch_items(&to_add)?;

        let mut writer = store.open_writer()?;
        let regular_items: Vec<_> = items.iter().filter(|i| i.is_regular_item()).collect();
        let total = regular_items.len();

        // Invariant: any fetched item we deliberately skip due to type
        // (attachment/note/annotation) must still be recorded with its remote
        // version so finalize persists it and it does not perpetually re-queue
        // in `to_add` on the next incremental run. Regular items that yielded
        // zero chunks are intentionally NOT recorded here so the next run
        // retries them; regular-item success/failure is handled below.
        for item in items.iter().filter(|i| !i.is_regular_item()) {
            if let Some(version) = remote_versions.get(&item.key) {
                indexed_versions.insert(item.key.clone(), *version);
            }
        }

        for (i, item) in regular_items.iter().enumerate() {
            eprint!(
                "\r  [{}/{}] {} ",
                i + 1,
                total,
                truncate(&item.data.title, 60),
            );

            // Find PDF and extract text
            let fulltext = match extract_fulltext(&client, &item.key) {
                Ok(text) => Some(text),
                Err(e) => {
                    eprintln!("\n    Warning: no fulltext for {}: {}", item.key, e);
                    None
                }
            };

            let indexable = IndexableItem {
                item_key: item.key.clone(),
                title: item.data.title.clone(),
                creators: item.creators_string(),
                abstract_note: item.data.abstract_note.clone(),
                tags: item.tags_string(),
                item_type: item.data.item_type.clone(),
                collections: item.data.collections.clone(),
                date: item.data.date.clone(),
                doi: item.data.doi.clone(),
                publication_title: item.data.publication_title.clone(),
                fulltext: fulltext.clone(),
            };

            let item_text = ItemText {
                item_key: item.key.clone(),
                title: item.data.title.clone(),
                authors: item.creators_string(),
                abstract_note: item.data.abstract_note.clone(),
                tags: item.tags_string(),
                fulltext: fulltext.clone(),
            };

            let chunks = chunk_item(&item_text);

            // Embed all chunks
            let chunk_texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
            let embeddings = embedder.embed(&chunk_texts)?;

            let ft = fulltext.unwrap_or_default();
            store.add_item(&writer, &indexable, &chunks, &embeddings, &ft)?;

            // Only record a version for items that produced >=1 chunk. Items that
            // yielded zero chunks (or were never returned by the API) are left out
            // so the next incremental run retries them.
            if !chunks.is_empty() {
                if let Some(version) = remote_versions.get(&item.key) {
                    indexed_versions.insert(item.key.clone(), *version);
                }
            }

            // Periodic checkpoint: commit the tantivy writer, persist vectors +
            // meta for everything indexed so far, then open a fresh writer.
            if (i + 1) % CHECKPOINT_EVERY == 0 && i + 1 < total {
                store.commit_writer(writer)?;
                store.checkpoint(&remote_versions, &indexed_versions)?;
                writer = store.open_writer()?;
                eprintln!("\n  Checkpoint: {}/{} items committed", i + 1, total);
            }
        }

        store.commit_writer(writer)?;
        eprintln!(); // newline after progress
    }

    // 5. Finalize
    store.finalize(&remote_versions, indexed_versions)?;

    let meta = store.meta();
    eprintln!(
        "Done. Indexed {} items, {} chunks, {} vectors.",
        meta.item_count, meta.chunk_count, store.vector_count()
    );

    Ok(())
}

pub fn run_index_status(json: bool) -> Result<()> {
    let store = IndexStore::open_or_create("BGESmallENV15", 384)?;
    let meta = store.meta();
    let data_dir = IndexStore::data_dir()?.display().to_string();

    let items_without_fulltext = store.count_items_without_fulltext()?;

    let output = IndexStatusOutput {
        item_count: meta.item_count,
        chunk_count: meta.chunk_count,
        vector_count: store.vector_count(),
        items_without_fulltext,
        model_name: meta.model_name.clone(),
        model_dim: meta.model_dim,
        last_sync: meta.last_sync.clone(),
        data_dir,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}

fn extract_fulltext(client: &ZoteroClient, item_key: &str) -> Result<String> {
    let children = client.fetch_children(item_key)?;
    for child in &children {
        if child.data.content_type == "application/pdf" {
            if let Some(path) = client.get_attachment_path(&child.key)? {
                let pdf_path = std::path::Path::new(&path);
                if pdf_path.exists() {
                    return crate::index::pdf::extract_text(pdf_path);
                }
            }
        }
    }
    anyhow::bail!("No PDF attachment found")
}

fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_does_not_split_multibyte_characters() {
        assert_eq!(truncate("Datenübergabe", 6), "Datenü...");
        assert_eq!(truncate("short", 60), "short");
    }
}
