use anyhow::Result;
use std::collections::HashMap;

use crate::api::ZoteroClient;
use crate::index::{
    BgeSmallEmbedder, Embedder, ExtractOutcome, ExtractStatus, IndexStore, IndexableItem,
    ItemStatusRecord, ItemText, SyncDiff, chunk_item, compute_sync_diff, text_hash,
};
use crate::output::{IndexIssueOutput, IndexIssuesOutput, IndexStatusOutput, format_output};

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
    let SyncDiff {
        mut to_add,
        to_delete,
    } = compute_sync_diff(&remote_versions, &local_versions);

    // Also re-attempt items whose extraction previously failed / was partial /
    // looked suspicious, even if their Zotero version is unchanged, so the
    // extractor upgrade actually reaches them. `no-attachment` items are not
    // retried here (only on a version change), so an attachment-less library
    // does not re-run every invocation. Dedup against the version-based set and
    // only retry items still present remotely.
    let retry_count = {
        use std::collections::HashSet;
        let already: HashSet<String> = to_add.iter().cloned().collect();
        // Status-tracked items that previously failed/partial/suspicious, plus a
        // one-time bootstrap for pre-tracking items that are indexed but have no
        // fulltext (extraction failed before status tracking existed).
        let mut extra_set: HashSet<String> = store.retry_keys().into_iter().collect();
        extra_set.extend(store.untracked_keys_without_fulltext()?);
        let extra: Vec<String> = extra_set
            .into_iter()
            .filter(|k| remote_versions.contains_key(k) && !already.contains(k))
            .collect();
        let n = extra.len();
        to_add.extend(extra);
        n
    };

    let total_remote = remote_versions.len();
    eprintln!(
        "Library: {} items. To index: {} new/updated, {} retry (prev. failed), {} deleted, {} unchanged.",
        total_remote,
        to_add.len() - retry_count,
        retry_count,
        to_delete.len(),
        total_remote.saturating_sub(to_add.len() - retry_count),
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

            // Find PDF and extract text, classifying the outcome.
            let outcome = extract_fulltext(&client, &item.key);
            let fulltext = if outcome.text.is_empty() {
                None
            } else {
                Some(outcome.text.clone())
            };
            if !outcome.detail.is_empty() {
                match outcome.status {
                    ExtractStatus::Partial | ExtractStatus::Suspicious => {
                        eprintln!("\n    Warning: {} ({}): {}", item.key, outcome.status.as_str(), outcome.detail);
                    }
                    ExtractStatus::Failed => {
                        eprintln!("\n    Warning: {}: {}", item.key, outcome.detail);
                    }
                    _ => {}
                }
            }

            let new_hash = text_hash(&outcome.text);
            let version = remote_versions.get(&item.key).copied().unwrap_or(0);

            // Skip re-embedding when a retry produced byte-identical text and the
            // item is already indexed: just refresh its recorded status/version.
            // This keeps genuinely unchanged items (typically attachment-less)
            // from re-embedding on every run.
            let unchanged = store
                .status_of(&item.key)
                .map(|r| r.text_hash == new_hash && local_versions.contains_key(&item.key))
                .unwrap_or(false);
            if unchanged {
                store.record_status(
                    &item.key,
                    ItemStatusRecord {
                        status: outcome.status,
                        detail: outcome.detail.clone(),
                        version,
                        text_hash: new_hash,
                    },
                );
                indexed_versions.insert(item.key.clone(), version);
                continue;
            }

            // Guard against a transient retry failure wiping previously-good
            // text. When this run's extraction is empty but the item already
            // has indexed chunks with non-empty fulltext, keep those chunks:
            // skip the delete + re-add, still record the new status/warning,
            // but preserve the stored text_hash so a later successful retry is
            // still detected as changed.
            if outcome.text.is_empty() && store.has_fulltext(&item.key)? {
                let prev_hash = store
                    .status_of(&item.key)
                    .map(|r| r.text_hash)
                    .unwrap_or(new_hash);
                store.record_status(
                    &item.key,
                    ItemStatusRecord {
                        status: outcome.status,
                        detail: outcome.detail.clone(),
                        version,
                        text_hash: prev_hash,
                    },
                );
                indexed_versions.insert(item.key.clone(), version);
                continue;
            }

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

            // Record the extraction status alongside the version so reporting
            // and retry decisions have it next run.
            store.record_status(
                &item.key,
                ItemStatusRecord {
                    status: outcome.status,
                    detail: outcome.detail.clone(),
                    version,
                    text_hash: new_hash,
                },
            );

            // Only record a version for items that produced >=1 chunk. Items that
            // yielded zero chunks (or were never returned by the API) are left out
            // so the next incremental run retries them.
            if !chunks.is_empty() {
                indexed_versions.insert(item.key.clone(), version);
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

    // Break the items down by recorded extraction status. Items indexed before
    // status tracking existed have no record and are counted as "unknown".
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for record in store.item_status().values() {
        *counts.entry(record.status.as_str()).or_insert(0) += 1;
    }
    let tracked: usize = counts.values().sum();
    let unknown = meta.item_count.saturating_sub(tracked);

    // Fixed order so the display is stable and reads worst-first.
    let mut status_breakdown = Vec::new();
    for label in ["ok", "partial", "suspicious", "failed", "no-attachment"] {
        if let Some(&c) = counts.get(label) {
            status_breakdown.push((label.to_string(), c));
        }
    }
    if unknown > 0 {
        status_breakdown.push(("unknown".to_string(), unknown));
    }

    let output = IndexStatusOutput {
        item_count: meta.item_count,
        chunk_count: meta.chunk_count,
        vector_count: store.vector_count(),
        items_without_fulltext,
        status_breakdown,
        model_name: meta.model_name.clone(),
        model_dim: meta.model_dim,
        last_sync: meta.last_sync.clone(),
        data_dir,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}

/// List every item with a non-`ok` extraction status: failed, partial,
/// suspicious, or no-attachment. Items indexed before status tracking existed
/// are not listed (their status is unknown, not known-bad).
pub fn run_index_issues(json: bool) -> Result<()> {
    let store = IndexStore::open_or_create("BGESmallENV15", 384)?;

    let mut issues: Vec<IndexIssueOutput> = Vec::new();
    for (key, record) in store.item_status() {
        if record.status == ExtractStatus::Ok {
            continue;
        }
        let title = store.title_of(key).unwrap_or_default();
        issues.push(IndexIssueOutput {
            key: key.clone(),
            title: truncate(&title, 80),
            status: record.status.as_str().to_string(),
            detail: record.detail.clone(),
        });
    }

    // Stable order: worst status first, then by key.
    let rank = |s: &str| match s {
        "failed" => 0,
        "partial" => 1,
        "suspicious" => 2,
        "no-attachment" => 3,
        _ => 4,
    };
    issues.sort_by(|a, b| {
        rank(&a.status)
            .cmp(&rank(&b.status))
            .then_with(|| a.key.cmp(&b.key))
    });

    let output = IndexIssuesOutput {
        count: issues.len(),
        issues,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}

/// Resolve an item's PDF attachment and extract text, classifying the outcome.
///
/// A resolution error (Zotero API failure, missing/unreadable file) or the
/// absence of any PDF child yields `no-attachment`: not a warning, but eligible
/// for retry only when the item's version changes. A found PDF delegates to the
/// isolated extractor, whose `ExtractStatus` (ok/failed/partial/suspicious) is
/// passed through.
fn extract_fulltext(client: &ZoteroClient, item_key: &str) -> ExtractOutcome {
    let children = match client.fetch_children(item_key) {
        Ok(c) => c,
        Err(_) => return no_attachment("could not fetch attachments"),
    };
    for child in &children {
        if child.data.content_type == "application/pdf" {
            match client.get_attachment_path(&child.key) {
                Ok(Some(path)) => {
                    let pdf_path = std::path::Path::new(&path);
                    if pdf_path.exists() {
                        match crate::index::pdf::extract_text(pdf_path) {
                            Ok(outcome) => return outcome,
                            Err(e) => {
                                return ExtractOutcome {
                                    status: ExtractStatus::Failed,
                                    text: String::new(),
                                    detail: format!("malformed PDF: {e}"),
                                };
                            }
                        }
                    }
                }
                Ok(None) | Err(_) => {}
            }
        }
    }
    no_attachment("no PDF attachment found")
}

fn no_attachment(detail: &str) -> ExtractOutcome {
    ExtractOutcome {
        status: ExtractStatus::NoAttachment,
        text: String::new(),
        detail: detail.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.min(s.len())])
    } else {
        s.to_string()
    }
}
