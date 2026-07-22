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
        // Status-tracked items that previously failed/partial/suspicious, plus
        // two one-time bootstraps: pre-tracking items that are indexed but have
        // no fulltext (extraction failed before status tracking existed), and
        // version-recorded items with no metadata chunk at all (standalone
        // attachments/notes skipped before v0.2.1). Also `no-attachment` items
        // recorded under an older extraction-logic version, so a newly added
        // fulltext source (HTML snapshots in v0.2.2) reaches them once; after
        // that they carry the current status version and stop re-queuing.
        let mut extra_set: HashSet<String> = store.retry_keys().into_iter().collect();
        extra_set.extend(store.untracked_keys_without_fulltext()?);
        extra_set.extend(store.keys_without_metadata_chunk()?);
        extra_set.extend(store.stale_no_attachment_keys());
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
        // Indexable items are regular items plus top-level standalone PDF
        // attachments and notes (the item itself carries the content). Other
        // non-regular items (child annotations, non-PDF child attachments that
        // slipped into the batch) are skipped but still version-recorded below.
        let regular_items: Vec<_> = items.iter().filter(|i| is_indexable(i)).collect();
        let total = regular_items.len();

        // Invariant: any fetched item we deliberately skip due to type
        // (annotations, non-indexable attachments) must still be recorded with
        // its remote version so finalize persists it and it does not perpetually
        // re-queue in `to_add` on the next incremental run. Regular items that
        // yielded zero chunks are intentionally NOT recorded here so the next
        // run retries them; indexable-item success/failure is handled below.
        for item in items.iter().filter(|i| !is_indexable(i)) {
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

            // Resolve fulltext (child PDF, own file, or note body) and classify.
            let outcome = resolve_fulltext(&client, item);
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
                        status_version: 0,
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
                        status_version: 0,
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
                    status_version: 0,
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

/// True for items we index directly: regular items, plus top-level standalone
/// attachments and notes. Standalone PDFs and notes carry extractable content;
/// other standalone attachments (text/html, text/plain) still get a metadata
/// chunk and a clear `no-attachment` status so nothing remains "unknown".
fn is_indexable(item: &crate::api::ZoteroItem) -> bool {
    item.is_regular_item() || item.is_standalone_attachment() || item.is_standalone_note()
}

/// Resolve an item's fulltext and classify the outcome, dispatching on type:
///
/// - a regular item: extract the first PDF child attachment, else the first
///   HTML snapshot child attachment;
/// - a top-level standalone PDF attachment: extract the item's own file;
/// - a top-level standalone HTML snapshot attachment: extract readable text from
///   its own file (mirrors the standalone-PDF path);
/// - a top-level note: index the note body as plain text;
/// - anything else that yields no fulltext: record `no-attachment` with a clear
///   detail so it never remains "unknown".
///
/// A found PDF delegates to the isolated extractor, whose `ExtractStatus`
/// (ok/failed/partial/suspicious) is passed through. An HTML snapshot is parsed
/// in-process via the readability extractor. A resolution error or the absence
/// of a usable attachment yields `no-attachment`: not a warning, but eligible
/// for retry only when the item's version changes.
fn resolve_fulltext(client: &ZoteroClient, item: &crate::api::ZoteroItem) -> ExtractOutcome {
    if item.is_standalone_pdf_attachment() {
        return extract_own_pdf(client, &item.key);
    }
    if item.is_standalone_html_attachment() {
        return extract_own_html(client, &item.key);
    }
    if item.is_standalone_note() {
        return note_outcome(&item.data.note);
    }
    if item.data.item_type == "attachment" {
        // Top-level attachment of an unsupported type (e.g. text/plain, image):
        // extraction of these formats is out of scope. Record a clear status.
        let ct = if item.data.content_type.is_empty() {
            "unknown".to_string()
        } else {
            item.data.content_type.clone()
        };
        return no_attachment(&format!("standalone attachment ({ct}): not extractable"));
    }
    extract_child_fulltext(client, &item.key)
}

/// Extract a regular item's child fulltext, preferring a PDF child over an HTML
/// snapshot child. A PDF is always preferred when present (even if it fails to
/// extract, its status is reported). Only when no PDF child exists do we look
/// for a saved HTML snapshot (`contentType == "text/html"`).
fn extract_child_fulltext(client: &ZoteroClient, item_key: &str) -> ExtractOutcome {
    let children = match client.fetch_children(item_key) {
        Ok(c) => c,
        Err(_) => return no_attachment("could not fetch attachments"),
    };

    // Prefer a PDF child: it is the richest source and always wins if present.
    for child in &children {
        if child.data.content_type == "application/pdf" {
            match client.get_attachment_path(&child.key) {
                Ok(Some(path)) => {
                    let pdf_path = std::path::Path::new(&path);
                    if pdf_path.exists() {
                        return run_pdf_extraction(pdf_path, "");
                    }
                }
                Ok(None) | Err(_) => {}
            }
        }
    }

    // No usable PDF child: fall back to an HTML snapshot child if one exists.
    for child in &children {
        if child.data.content_type == "text/html" {
            match client.get_attachment_path(&child.key) {
                Ok(Some(path)) => {
                    let html_path = std::path::Path::new(&path);
                    if html_path.exists() {
                        return crate::index::html::extract_snapshot(html_path, "html snapshot");
                    }
                    return no_attachment("html snapshot: file missing on disk");
                }
                Ok(None) | Err(_) => {}
            }
        }
    }

    no_attachment("no PDF or HTML attachment found")
}

/// Extract a standalone PDF attachment's own file (the item is the attachment).
fn extract_own_pdf(client: &ZoteroClient, item_key: &str) -> ExtractOutcome {
    match client.get_attachment_path(item_key) {
        Ok(Some(path)) => {
            let pdf_path = std::path::Path::new(&path);
            if pdf_path.exists() {
                run_pdf_extraction(pdf_path, "standalone PDF")
            } else {
                no_attachment("standalone PDF: file missing on disk")
            }
        }
        Ok(None) => no_attachment("standalone PDF: no file resolved"),
        Err(_) => no_attachment("standalone PDF: could not resolve file path"),
    }
}

/// Extract a standalone HTML snapshot attachment's own file (the item is the
/// snapshot). Mirrors [`extract_own_pdf`] but runs the readability extractor.
fn extract_own_html(client: &ZoteroClient, item_key: &str) -> ExtractOutcome {
    match client.get_attachment_path(item_key) {
        Ok(Some(path)) => {
            let html_path = std::path::Path::new(&path);
            if html_path.exists() {
                crate::index::html::extract_snapshot(html_path, "standalone html snapshot")
            } else {
                no_attachment("standalone html snapshot: file missing on disk")
            }
        }
        Ok(None) => no_attachment("standalone html snapshot: no file resolved"),
        Err(_) => no_attachment("standalone html snapshot: could not resolve file path"),
    }
}

/// Run the isolated PDF extractor on a resolved path, tagging the detail with a
/// source label (e.g. "standalone PDF") where helpful.
fn run_pdf_extraction(pdf_path: &std::path::Path, source: &str) -> ExtractOutcome {
    match crate::index::pdf::extract_text(pdf_path) {
        Ok(mut outcome) => {
            if !source.is_empty() && outcome.status == ExtractStatus::Ok && outcome.detail.is_empty()
            {
                outcome.detail = format!("{source}: extracted");
            }
            outcome
        }
        Err(e) => ExtractOutcome {
            status: ExtractStatus::Failed,
            text: String::new(),
            detail: format!("malformed PDF: {e}"),
        },
    }
}

/// Turn a note's HTML body into an extraction outcome: strip tags to plain text.
/// Non-empty notes are `Ok` (source "note content"); empty notes are recorded as
/// `no-attachment` with an "empty note" detail so they do not remain "unknown".
fn note_outcome(note_html: &str) -> ExtractOutcome {
    let text = strip_html(note_html);
    if text.is_empty() {
        return no_attachment("empty note");
    }
    ExtractOutcome {
        status: ExtractStatus::Ok,
        text,
        detail: "note content".to_string(),
    }
}

fn no_attachment(detail: &str) -> ExtractOutcome {
    ExtractOutcome {
        status: ExtractStatus::NoAttachment,
        text: String::new(),
        detail: detail.to_string(),
    }
}

/// Minimal HTML-to-text: drop tags, decode a small set of common entities, and
/// collapse whitespace. Deliberately lightweight (no HTML crate); good enough
/// for Zotero note bodies, which are simple formatted HTML.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                // Treat a closed tag as a soft break so words do not run together.
                out.push(' ');
            }
            _ if in_tag => {}
            _ => out.push(ch),
        }
    }
    let decoded = decode_entities(&out);
    // Collapse runs of whitespace (including the spaces injected for tags) into
    // single spaces and trim.
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode the handful of HTML entities that show up in Zotero note bodies.
/// Single left-to-right pass so decoded output is never rescanned: a literally
/// escaped entity like `&amp;lt;` decodes to the text `&lt;`, not to `<`.
fn decode_entities(s: &str) -> String {
    const ENTITIES: [(&str, &str); 7] = [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        match ENTITIES.iter().find(|(e, _)| rest.starts_with(e)) {
            Some((entity, replacement)) => {
                out.push_str(replacement);
                rest = &rest[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.min(s.len())])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags_and_collapses_whitespace() {
        let html = "<div data-schema-version=\"9\"><h1>Title</h1>\n<p>Hello <b>world</b>.</p></div>";
        assert_eq!(strip_html(html), "Title Hello world .");
    }

    #[test]
    fn strip_html_decodes_common_entities() {
        let html = "<p>Tom &amp; Jerry &lt;3 caf&#233;</p>";
        // &#233; is not in the decoded set, so it is left as-is (rare in notes).
        assert_eq!(strip_html(html), "Tom & Jerry <3 caf&#233;");
        assert_eq!(strip_html("a&nbsp;b"), "a b");
    }

    #[test]
    fn strip_html_amp_decoded_before_entity_bodies() {
        // A literally-escaped entity (&amp;lt;) must survive as text, not become <.
        assert_eq!(strip_html("x &amp;lt; y"), "x &lt; y");
    }

    #[test]
    fn note_outcome_ok_for_nonempty_note() {
        let outcome = note_outcome("<p>Some note body.</p>");
        assert_eq!(outcome.status, ExtractStatus::Ok);
        assert_eq!(outcome.text, "Some note body.");
        assert_eq!(outcome.detail, "note content");
    }

    #[test]
    fn note_outcome_empty_note_is_no_attachment() {
        let outcome = note_outcome("<div></div>");
        assert_eq!(outcome.status, ExtractStatus::NoAttachment);
        assert!(outcome.text.is_empty());
        assert_eq!(outcome.detail, "empty note");
    }
}
