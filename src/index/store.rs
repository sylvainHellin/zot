use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::*;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, Term};

use super::chunker::{Chunk, ChunkType};
use super::pdf::ExtractStatus;

/// Per-item extraction status, persisted alongside the version checkpoint so
/// `zot index status`/`issues` can report it and so failed/partial items are
/// retried on the next run. Items indexed before status tracking existed simply
/// have no entry here and are reported as "unknown".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemStatusRecord {
    pub status: ExtractStatus,
    /// Human-readable warning detail (empty for `ok`/`no-attachment`).
    #[serde(default)]
    pub detail: String,
    /// Remote Zotero version this status was recorded at.
    #[serde(default)]
    pub version: u64,
    /// Hash of the extracted text at index time, used to skip re-embedding when
    /// a retry produces identical text (0 when there is no text).
    #[serde(default)]
    pub text_hash: u64,
    /// Version of the extraction logic that produced this record. Bumped when a
    /// new fulltext source is added (e.g. HTML snapshots in v0.2.2) so items
    /// previously classified `no-attachment` are re-attempted once against the
    /// new logic, without a forced rebuild. Absent (0) for records written
    /// before this field existed.
    #[serde(default)]
    pub status_version: u32,
}

/// Current extraction-logic version. Bump when a new fulltext source is added
/// so that `no-attachment` items recorded under an older version are re-queued
/// once (see [`IndexStore::stale_no_attachment_keys`]).
///   v1: PDF + note + standalone (v0.2.1)
///   v2: + HTML snapshots (v0.2.2)
pub const STATUS_VERSION: u32 = 2;

/// Metadata about the index state, persisted to meta.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub model_name: String,
    pub model_dim: usize,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub last_sync: String,
    pub item_count: usize,
    pub chunk_count: usize,
    pub items: HashMap<String, u64>, // key -> version
    /// key -> extraction status. Added after `items`; absent for indexes built
    /// before status tracking, hence `#[serde(default)]`.
    #[serde(default)]
    pub item_status: HashMap<String, ItemStatusRecord>,
}

/// Difference between the remote Zotero library and the local index, computed
/// from the two key -> version maps. `to_add` holds keys that are new or whose
/// version changed remotely; `to_delete` holds keys present locally but gone
/// remotely. Both `run_index` (which needs the keys) and the search-time
/// staleness check (which needs the counts) share this one definition.
#[derive(Debug, Clone, Default)]
pub struct SyncDiff {
    pub to_add: Vec<String>,
    pub to_delete: Vec<String>,
}

impl SyncDiff {
    /// True if the local index differs from the remote library.
    pub fn is_stale(&self) -> bool {
        !self.to_add.is_empty() || !self.to_delete.is_empty()
    }
}

/// Diff a remote key -> version map against the local one.
pub fn compute_sync_diff(
    remote_versions: &HashMap<String, u64>,
    local_versions: &HashMap<String, u64>,
) -> SyncDiff {
    let mut to_add = Vec::new();
    for (key, remote_ver) in remote_versions {
        match local_versions.get(key) {
            Some(local_ver) if local_ver == remote_ver => {} // unchanged
            _ => to_add.push(key.clone()),                   // new or updated
        }
    }

    let mut to_delete = Vec::new();
    for key in local_versions.keys() {
        if !remote_versions.contains_key(key) {
            to_delete.push(key.clone());
        }
    }

    SyncDiff { to_add, to_delete }
}

/// Schema field handles for the tantivy index.
struct Fields {
    chunk_id: Field,
    item_key: Field,
    chunk_type: Field,
    text: Field,
    fulltext: Field,
    title: Field,
    creators: Field,
    tags: Field,
    item_type: Field,
    collections: Field,
    char_start: Field,
    char_end: Field,
    // Stored-only fields for retrieval
    date: Field,
    doi: Field,
    abstract_note: Field,
    publication_title: Field,
}

/// The local search index (tantivy + vectors).
pub struct IndexStore {
    base_dir: PathBuf,
    index: Index,
    reader: IndexReader,
    fields: Fields,
    vectors: Vec<Vec<f32>>,
    chunk_ids: Vec<String>, // parallel to vectors
    meta: IndexMeta,
}

/// Data needed to index a single item.
pub struct IndexableItem {
    pub item_key: String,
    pub title: String,
    pub creators: String,
    pub abstract_note: String,
    pub tags: String,
    pub item_type: String,
    pub collections: Vec<String>,
    pub date: String,
    pub doi: String,
    pub publication_title: String,
    // Carried alongside the item for indexing; chunk text is stored per-chunk,
    // so the whole-item copy is not read back directly.
    #[allow(dead_code)]
    pub fulltext: Option<String>,
}

fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();

    let text_opts = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("en_stem")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    let string_opts = STRING | STORED;

    let chunk_id = builder.add_text_field("chunk_id", string_opts.clone());
    let item_key = builder.add_text_field("item_key", string_opts.clone());
    let chunk_type = builder.add_text_field("chunk_type", string_opts.clone());
    let text = builder.add_text_field("text", text_opts.clone());
    let fulltext = builder.add_text_field("fulltext", STORED); // stored only, not indexed (big)
    let title = builder.add_text_field("title", text_opts.clone());
    let creators = builder.add_text_field("creators", text_opts.clone());
    let tags = builder.add_text_field("tags", string_opts.clone());
    let item_type = builder.add_text_field("item_type", string_opts.clone());
    let collections = builder.add_text_field("collections", string_opts.clone());
    let char_start = builder.add_u64_field("char_start", STORED);
    let char_end = builder.add_u64_field("char_end", STORED);
    let date = builder.add_text_field("date", STORED);
    let doi = builder.add_text_field("doi", STORED);
    let abstract_note = builder.add_text_field("abstract_note", STORED);
    let publication_title = builder.add_text_field("publication_title", STORED);

    let schema = builder.build();
    let fields = Fields {
        chunk_id,
        item_key,
        chunk_type,
        text,
        fulltext,
        title,
        creators,
        tags,
        item_type,
        collections,
        char_start,
        char_end,
        date,
        doi,
        abstract_note,
        publication_title,
    };

    (schema, fields)
}

impl IndexStore {
    /// Get the platform-appropriate data directory.
    pub fn data_dir() -> Result<PathBuf> {
        let proj_dirs = directories::ProjectDirs::from("", "", "zot")
            .context("Could not determine data directory")?;
        Ok(proj_dirs.data_dir().to_path_buf())
    }

    /// Open an existing index or create a new one.
    pub fn open_or_create(model_name: &str, model_dim: usize) -> Result<Self> {
        let base_dir = Self::data_dir()?;
        Self::open_at(base_dir, model_name, model_dim)
    }

    /// Open or create an index rooted at a specific base directory.
    fn open_at(base_dir: PathBuf, model_name: &str, model_dim: usize) -> Result<Self> {
        let tantivy_dir = base_dir.join("tantivy");
        let vectors_path = base_dir.join("vectors.bin");
        let meta_path = base_dir.join("meta.json");

        fs::create_dir_all(&tantivy_dir)
            .context("Failed to create index directory")?;

        let (schema, fields) = build_schema();

        // Try to open existing index, or create new
        let index = if tantivy_dir.join("meta.json").exists() {
            Index::open_in_dir(&tantivy_dir)
                .context("Failed to open existing tantivy index")?
        } else {
            Index::create_in_dir(&tantivy_dir, schema.clone())
                .context("Failed to create tantivy index")?
        };

        // Register the en_stem tokenizer
        index.tokenizers().register(
            "en_stem",
            tantivy::tokenizer::TextAnalyzer::builder(
                tantivy::tokenizer::SimpleTokenizer::default(),
            )
            .filter(tantivy::tokenizer::RemoveLongFilter::limit(40))
            .filter(tantivy::tokenizer::LowerCaser)
            .filter(tantivy::tokenizer::Stemmer::default())
            .build(),
        );

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("Failed to create index reader")?;

        // Load metadata
        let meta = if meta_path.exists() {
            let meta_str = fs::read_to_string(&meta_path).context("Failed to read meta.json")?;
            serde_json::from_str(&meta_str).context("Failed to parse meta.json")?
        } else {
            IndexMeta {
                model_name: model_name.to_string(),
                model_dim,
                chunk_size: super::chunker::CHUNK_SIZE,
                chunk_overlap: super::chunker::CHUNK_OVERLAP,
                last_sync: String::new(),
                item_count: 0,
                chunk_count: 0,
                items: HashMap::new(),
                item_status: HashMap::new(),
            }
        };

        // Check model mismatch
        if !meta.items.is_empty() && meta.model_name != model_name {
            bail!(
                "Index was built with model '{}' but current model is '{}'. Run `zot index --force` to rebuild.",
                meta.model_name,
                model_name
            );
        }

        // Load vectors
        let (vectors, chunk_ids) = if vectors_path.exists() {
            load_vectors(&vectors_path, model_dim)?
        } else {
            (Vec::new(), Vec::new())
        };

        // Integrity check: tantivy should hold exactly one document per vector
        // (one per chunk). A mismatch means a previous run was interrupted
        // mid-write (e.g. OOM during `--force`), leaving meta.json/vectors.bin
        // out of sync with the tantivy segments. Warn so the user can rebuild.
        let num_docs = reader.searcher().num_docs() as usize;
        if num_docs != vectors.len() {
            eprintln!(
                "Warning: index looks inconsistent ({} search docs vs {} vectors). \
                 Run `zot index --force` to rebuild.",
                num_docs,
                vectors.len(),
            );
        }

        Ok(Self {
            base_dir,
            index,
            reader,
            fields,
            vectors,
            chunk_ids,
            meta,
        })
    }

    /// Get current metadata.
    pub fn meta(&self) -> &IndexMeta {
        &self.meta
    }

    /// Get stored item versions.
    pub fn item_versions(&self) -> &HashMap<String, u64> {
        &self.meta.items
    }

    /// Get stored per-item extraction status.
    pub fn item_status(&self) -> &HashMap<String, ItemStatusRecord> {
        &self.meta.item_status
    }

    /// Record the extraction status for an item (in memory; persisted by the
    /// next checkpoint/finalize). Stamps the record with the current
    /// [`STATUS_VERSION`] so a later logic upgrade can identify and re-queue
    /// stale `no-attachment` classifications.
    pub fn record_status(&mut self, key: &str, mut record: ItemStatusRecord) {
        record.status_version = STATUS_VERSION;
        self.meta.item_status.insert(key.to_string(), record);
    }

    /// Look up the previously recorded status for an item, if any.
    pub fn status_of(&self, key: &str) -> Option<&ItemStatusRecord> {
        self.meta.item_status.get(key)
    }

    /// Keys that should be re-attempted next run even if their Zotero version is
    /// unchanged: anything that previously failed, extracted partially, or
    /// looked suspicious. Items with status `no-attachment` are intentionally
    /// excluded here; they are only retried when their version changes (via
    /// `compute_sync_diff`), so an attachment-less library does not re-run on
    /// every invocation.
    pub fn retry_keys(&self) -> Vec<String> {
        self.meta
            .item_status
            .iter()
            .filter(|(_, r)| {
                matches!(
                    r.status,
                    ExtractStatus::Failed | ExtractStatus::Partial | ExtractStatus::Suspicious
                )
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// `no-attachment` items whose status was recorded under an older extraction
    /// version than [`STATUS_VERSION`]. Re-queued once so a newly added fulltext
    /// source (e.g. HTML snapshots) reaches items previously written off as
    /// having no usable attachment, without a forced rebuild. After the retry
    /// the record carries the current `status_version`, so an item that still
    /// has no attachment is not re-queued on the next run.
    pub fn stale_no_attachment_keys(&self) -> Vec<String> {
        self.meta
            .item_status
            .iter()
            .filter(|(_, r)| {
                r.status == ExtractStatus::NoAttachment && r.status_version < STATUS_VERSION
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Delete all data for specific item keys from the index.
    pub fn delete_items(&mut self, keys: &[String]) -> Result<()> {
        let mut writer: IndexWriter = self
            .index
            .writer(50_000_000)
            .context("Failed to create index writer")?;

        for key in keys {
            writer.delete_term(Term::from_field_text(self.fields.item_key, key));
            self.meta.items.remove(key);
            self.meta.item_status.remove(key);

            // Remove from vectors
            let mut i = 0;
            while i < self.chunk_ids.len() {
                if chunk_belongs_to(&self.chunk_ids[i], key) {
                    self.chunk_ids.remove(i);
                    self.vectors.remove(i);
                } else {
                    i += 1;
                }
            }
        }

        writer.commit().context("Failed to commit deletions")?;
        Ok(())
    }

    /// Open a writer for batch operations. Call `commit_writer` when done.
    pub fn open_writer(&self) -> Result<IndexWriter> {
        self.index
            .writer(50_000_000)
            .context("Failed to create index writer")
    }

    /// Add chunks and their embeddings for an item using an existing writer.
    pub fn add_item(
        &mut self,
        writer: &IndexWriter,
        item: &IndexableItem,
        chunks: &[Chunk],
        embeddings: &[Vec<f32>],
        fulltext: &str,
    ) -> Result<()> {
        // Delete old data for this item first (tantivy docs)
        writer.delete_term(Term::from_field_text(self.fields.item_key, &item.item_key));

        // Purge any existing vectors/chunk_ids for this item so re-ingest is
        // idempotent (mirror delete_items, which matches chunk_ids by key prefix).
        let mut i = 0;
        while i < self.chunk_ids.len() {
            if chunk_belongs_to(&self.chunk_ids[i], &item.item_key) {
                self.chunk_ids.remove(i);
                self.vectors.remove(i);
            } else {
                i += 1;
            }
        }

        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            let mut doc = tantivy::TantivyDocument::default();
            doc.add_text(self.fields.chunk_id, &chunk.chunk_id);
            doc.add_text(self.fields.item_key, &item.item_key);
            doc.add_text(self.fields.chunk_type, chunk.chunk_type.as_str());
            doc.add_text(self.fields.text, &chunk.text);
            doc.add_text(self.fields.title, &item.title);
            doc.add_text(self.fields.creators, &item.creators);
            // Store each tag as a separate field value for filtering
            for tag in item.tags.split(", ") {
                if !tag.is_empty() {
                    doc.add_text(self.fields.tags, tag);
                }
            }
            doc.add_text(self.fields.item_type, &item.item_type);
            doc.add_u64(self.fields.char_start, chunk.char_start as u64);
            doc.add_u64(self.fields.char_end, chunk.char_end as u64);
            doc.add_text(self.fields.date, &item.date);
            doc.add_text(self.fields.doi, &item.doi);
            doc.add_text(self.fields.abstract_note, &item.abstract_note);
            doc.add_text(self.fields.publication_title, &item.publication_title);

            // Store fulltext only on metadata chunk (avoid duplication)
            if chunk.chunk_type == ChunkType::Metadata {
                doc.add_text(self.fields.fulltext, fulltext);
            }

            for collection in &item.collections {
                doc.add_text(self.fields.collections, collection);
            }

            writer.add_document(doc)?;

            // Add to vector store
            self.chunk_ids.push(chunk.chunk_id.clone());
            self.vectors.push(embedding.clone());
        }

        Ok(())
    }

    /// Commit and consume a writer.
    pub fn commit_writer(&self, mut writer: IndexWriter) -> Result<()> {
        writer.commit().context("Failed to commit")?;
        Ok(())
    }

    /// Finalize after indexing: save vectors, update metadata, reload reader.
    ///
    /// The persisted version map is rebuilt rather than blindly overwritten with
    /// the full remote map:
    ///   (a) previously-known versions for items still present remotely that were
    ///       NOT reprocessed this run are kept, and
    ///   (b) `indexed_versions` carries the remote version for each item that was
    ///       successfully indexed with >=1 chunk this run (overlaid on top).
    /// Items absent from `remote_versions` (deleted) are dropped. Items that were
    /// queued for indexing but yielded zero chunks (or were never returned by the
    /// API) are intentionally not recorded, so the next incremental run retries them.
    pub fn finalize(
        &mut self,
        remote_versions: &HashMap<String, u64>,
        indexed_versions: HashMap<String, u64>,
    ) -> Result<()> {
        self.rebuild_items_map(remote_versions, &indexed_versions);
        self.write_to_disk()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Persist progress mid-run (after committing the tantivy writer) without
    /// reloading the reader. Leaves a consistent on-disk index so an interrupted
    /// run resumes the remaining items instead of trusting a stale meta.json.
    pub fn checkpoint(
        &mut self,
        remote_versions: &HashMap<String, u64>,
        indexed_versions: &HashMap<String, u64>,
    ) -> Result<()> {
        self.rebuild_items_map(remote_versions, indexed_versions);
        self.write_to_disk()
    }

    /// Recompute the persisted version map: keep previously-known items still
    /// present remotely, then overlay items indexed this run. Items absent from
    /// `remote_versions` (deleted) are dropped; items not in either map (queued
    /// but zero-chunk / never fetched) are left out so the next run retries them.
    fn rebuild_items_map(
        &mut self,
        remote_versions: &HashMap<String, u64>,
        indexed_versions: &HashMap<String, u64>,
    ) {
        let mut persisted: HashMap<String, u64> = self
            .meta
            .items
            .iter()
            .filter(|(k, _)| remote_versions.contains_key(*k))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (key, version) in indexed_versions {
            persisted.insert(key.clone(), *version);
        }
        self.meta.items = persisted;

        // Drop status entries for items no longer present remotely so the
        // status map does not grow stale keys.
        self.meta
            .item_status
            .retain(|k, _| remote_versions.contains_key(k));
    }

    /// Write the current in-memory state (vectors + meta) to disk atomically
    /// enough that meta.json never claims more than the vectors/tantivy hold.
    fn write_to_disk(&mut self) -> Result<()> {
        self.meta.item_count = self.meta.items.len();
        self.meta.chunk_count = self.chunk_ids.len();
        self.meta.last_sync = chrono_now();

        save_vectors(
            &self.base_dir.join("vectors.bin"),
            &self.vectors,
            &self.chunk_ids,
            self.meta.model_dim,
        )?;

        let meta_str = serde_json::to_string_pretty(&self.meta)?;
        fs::write(self.base_dir.join("meta.json"), meta_str)?;

        Ok(())
    }

    /// Clear the entire index for a force rebuild.
    pub fn clear(&mut self) -> Result<()> {
        let mut writer: IndexWriter = self
            .index
            .writer(50_000_000)
            .context("Failed to create index writer")?;
        writer.delete_all_documents()?;
        writer.commit()?;

        self.vectors.clear();
        self.chunk_ids.clear();
        self.meta.items.clear();
        self.meta.item_status.clear();
        self.meta.item_count = 0;
        self.meta.chunk_count = 0;

        // Persist the emptied state immediately. The tantivy deletion above is
        // already committed to disk; if we crash before finalize without doing
        // this, the stale meta.json/vectors.bin would make the next run believe
        // the (now-empty) index is full and skip everything.
        self.reader.reload()?;
        self.write_to_disk()?;

        Ok(())
    }

    /// BM25 search with optional filters. Returns (chunk_id, score) pairs.
    pub fn bm25_search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        let searcher = self.reader.searcher();
        let query_parser = QueryParser::for_index(
            &self.index,
            vec![self.fields.text, self.fields.title, self.fields.creators],
        );

        let parsed_query = query_parser
            .parse_query(query)
            .context("Failed to parse search query")?;

        // Build filtered query if needed
        let final_query: Box<dyn tantivy::query::Query> = if filters.has_any() {
            let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            clauses.push((Occur::Must, parsed_query));

            if let Some(tag) = &filters.tag {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.tags, tag),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            if let Some(item_type) = &filters.item_type {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.item_type, item_type),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            if let Some(collection) = &filters.collection {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.collections, collection),
                        IndexRecordOption::Basic,
                    )),
                ));
            }

            Box::new(BooleanQuery::new(clauses))
        } else {
            parsed_query
        };

        let top_docs = searcher
            .search(&*final_query, &TopDocs::with_limit(limit))
            .context("BM25 search failed")?;

        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            if let Some(chunk_id) = doc
                .get_first(self.fields.chunk_id)
                .and_then(|v| v.as_str())
            {
                results.push((chunk_id.to_string(), score));
            }
        }

        Ok(results)
    }

    /// Vector similarity search. Returns (chunk_id, score) pairs.
    pub fn vector_search(
        &self,
        query_embedding: &[f32],
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        if self.vectors.is_empty() {
            return Ok(Vec::new());
        }

        // If we have filters, get the set of allowed chunk IDs first
        let allowed_chunks: Option<std::collections::HashSet<String>> = if filters.has_any() {
            let searcher = self.reader.searcher();
            let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

            // Match all, then filter
            clauses.push((
                Occur::Must,
                Box::new(tantivy::query::AllQuery),
            ));

            if let Some(tag) = &filters.tag {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.tags, tag),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            if let Some(item_type) = &filters.item_type {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.item_type, item_type),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            if let Some(collection) = &filters.collection {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.collections, collection),
                        IndexRecordOption::Basic,
                    )),
                ));
            }

            let filter_query = BooleanQuery::new(clauses);
            let top_docs = searcher.search(
                &filter_query,
                &TopDocs::with_limit(self.chunk_ids.len()),
            )?;

            let mut allowed = std::collections::HashSet::new();
            for (_, doc_address) in top_docs {
                let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
                if let Some(cid) = doc
                    .get_first(self.fields.chunk_id)
                    .and_then(|v| v.as_str())
                {
                    allowed.insert(cid.to_string());
                }
            }
            Some(allowed)
        } else {
            None
        };

        // Brute-force cosine similarity
        let mut scores: Vec<(String, f32)> = self
            .vectors
            .iter()
            .zip(self.chunk_ids.iter())
            .filter(|(_, cid)| {
                allowed_chunks
                    .as_ref()
                    .map_or(true, |allowed| allowed.contains(*cid))
            })
            .map(|(vec, cid)| {
                let score = cosine_similarity(query_embedding, vec);
                (cid.clone(), score)
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        scores.truncate(limit);

        Ok(scores)
    }

    /// Retrieve chunk data by chunk ID.
    pub fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkData>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.chunk_id, chunk_id),
            IndexRecordOption::Basic,
        );
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;

        if let Some((_, doc_address)) = top_docs.first() {
            let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
            Ok(Some(doc_to_chunk_data(&doc, &self.fields)))
        } else {
            Ok(None)
        }
    }

    /// Retrieve all chunks for an item key.
    pub fn get_item_chunks(&self, item_key: &str) -> Result<Vec<ChunkData>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.item_key, item_key),
            IndexRecordOption::Basic,
        );
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1000))?;

        let mut chunks = Vec::new();
        for (_, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            chunks.push(doc_to_chunk_data(&doc, &self.fields));
        }

        Ok(chunks)
    }

    /// Get the stored fulltext for an item (from its metadata chunk).
    pub fn get_fulltext(&self, item_key: &str) -> Result<Option<String>> {
        let searcher = self.reader.searcher();

        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.item_key, item_key),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.chunk_type, "metadata"),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);

        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;

        if let Some((_, doc_address)) = top_docs.first() {
            let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
            let fulltext = doc
                .get_first(self.fields.fulltext)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Ok(fulltext)
        } else {
            Ok(None)
        }
    }

    /// True if the item is indexed and its metadata chunk carries non-empty
    /// fulltext. Used to protect previously-good text from being overwritten by
    /// a transient (empty) retry extraction.
    pub fn has_fulltext(&self, item_key: &str) -> Result<bool> {
        Ok(self
            .get_fulltext(item_key)?
            .map(|s| !s.is_empty())
            .unwrap_or(false))
    }

    /// Get the number of indexed vectors.
    pub fn vector_count(&self) -> usize {
        self.vectors.len()
    }

    /// Count indexed items that have no stored fulltext.
    ///
    /// Fulltext is stored on each item's metadata chunk. An item whose metadata
    /// chunk has an empty (or missing) fulltext value counts as "no fulltext";
    /// this covers items that only produced a metadata chunk (e.g. PDF extraction
    /// failed or no PDF was attached). Scans the local index only.
    pub fn count_items_without_fulltext(&self) -> Result<usize> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.chunk_type, "metadata"),
            IndexRecordOption::Basic,
        );
        let top_docs = searcher.search(&query, &TopDocs::with_limit(self.chunk_ids.len().max(1)))?;

        let mut count = 0;
        for (_, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            let has_fulltext = doc
                .get_first(self.fields.fulltext)
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if !has_fulltext {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Item keys that are indexed but have no stored fulltext AND no recorded
    /// extraction status. These are items from before status tracking existed
    /// whose PDF extraction previously failed or was skipped; they must be
    /// re-attempted once so the new extractor reaches them (a one-time bootstrap;
    /// after the retry they gain a status record and follow the normal path).
    pub fn untracked_keys_without_fulltext(&self) -> Result<Vec<String>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.chunk_type, "metadata"),
            IndexRecordOption::Basic,
        );
        let top_docs =
            searcher.search(&query, &TopDocs::with_limit(self.chunk_ids.len().max(1)))?;

        let mut keys = Vec::new();
        for (_, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            let has_fulltext = doc
                .get_first(self.fields.fulltext)
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if has_fulltext {
                continue;
            }
            if let Some(key) = doc.get_first(self.fields.item_key).and_then(|v| v.as_str()) {
                if !self.meta.item_status.contains_key(key) {
                    keys.push(key.to_string());
                }
            }
        }
        Ok(keys)
    }

    /// Item keys that have a recorded version but no metadata chunk in the
    /// index. Before v0.2.1, top-level standalone attachments and notes were
    /// version-recorded but never chunked, so incremental runs saw them as
    /// unchanged and they stayed invisible (no fulltext, no status). Re-queue
    /// them once; after indexing they carry a metadata chunk and follow the
    /// normal path (analogous one-time bootstrap to
    /// [`untracked_keys_without_fulltext`]).
    pub fn keys_without_metadata_chunk(&self) -> Result<Vec<String>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.chunk_type, "metadata"),
            IndexRecordOption::Basic,
        );
        let top_docs =
            searcher.search(&query, &TopDocs::with_limit(self.chunk_ids.len().max(1)))?;

        let mut have_chunk: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            if let Some(key) = doc.get_first(self.fields.item_key).and_then(|v| v.as_str()) {
                have_chunk.insert(key.to_string());
            }
        }

        Ok(self
            .meta
            .items
            .keys()
            .filter(|k| !have_chunk.contains(*k))
            .cloned()
            .collect())
    }

    /// Fetch the stored title for an item (from its metadata chunk). Empty if
    /// the item is not indexed or has no title.
    pub fn title_of(&self, item_key: &str) -> Result<String> {
        let searcher = self.reader.searcher();
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.item_key, item_key),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.chunk_type, "metadata"),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;
        if let Some((_, doc_address)) = top_docs.first() {
            let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
            Ok(doc
                .get_first(self.fields.title)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string())
        } else {
            Ok(String::new())
        }
    }
}

/// Stable hash of extracted text, used to skip re-embedding when a retry yields
/// identical text. Uses the default hasher; only compared against itself within
/// one machine's index, so cross-platform stability is not required.
pub fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Filters applicable to both BM25 and vector search.
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    pub tag: Option<String>,
    // Populated from `--creator`; creator matching happens through the BM25 query
    // text rather than an exact filter, so this field is not read here directly.
    #[allow(dead_code)]
    pub creator: Option<String>,
    pub item_type: Option<String>,
    pub collection: Option<String>,
}

impl SearchFilters {
    pub fn has_any(&self) -> bool {
        self.tag.is_some() || self.item_type.is_some() || self.collection.is_some()
    }
}

/// Data extracted from a tantivy document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkData {
    pub chunk_id: String,
    pub item_key: String,
    pub chunk_type: String,
    pub text: String,
    pub title: String,
    pub creators: String,
    pub tags: String,
    pub item_type: String,
    pub date: String,
    pub doi: String,
    pub abstract_note: String,
    pub publication_title: String,
    pub char_start: u64,
    pub char_end: u64,
}

fn doc_to_chunk_data(doc: &tantivy::TantivyDocument, fields: &Fields) -> ChunkData {
    let get_text = |field: Field| -> String {
        doc.get_first(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let get_u64 = |field: Field| -> u64 {
        doc.get_first(field)
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };

    ChunkData {
        chunk_id: get_text(fields.chunk_id),
        item_key: get_text(fields.item_key),
        chunk_type: get_text(fields.chunk_type),
        text: get_text(fields.text),
        title: get_text(fields.title),
        creators: get_text(fields.creators),
        tags: get_text(fields.tags),
        item_type: get_text(fields.item_type),
        date: get_text(fields.date),
        doi: get_text(fields.doi),
        abstract_note: get_text(fields.abstract_note),
        publication_title: get_text(fields.publication_title),
        char_start: get_u64(fields.char_start),
        char_end: get_u64(fields.char_end),
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn chrono_now() -> String {
    // Simple ISO 8601 timestamp without chrono dependency
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{now}")
}

/// Returns true if `chunk_id` was emitted for `key` exactly.
///
/// Chunks use the `"{key}_meta"` and `"{key}_{i}"` formats (see
/// `chunker::chunk_item`), so any chunk whose id is exactly `key` or starts
/// with `"{key}_"` belongs to `key`. A bare `starts_with(key)` would falsely
/// match items whose keys share `key` as a strict prefix (e.g. key="ABC"
/// matching chunk_id="ABCD_meta").
fn chunk_belongs_to(chunk_id: &str, key: &str) -> bool {
    chunk_id == key || chunk_id.starts_with(&format!("{}_", key))
}

/// Save vectors and chunk IDs to a binary file.
fn save_vectors(
    path: &Path,
    vectors: &[Vec<f32>],
    chunk_ids: &[String],
    dim: usize,
) -> Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;

    let count = vectors.len() as u32;
    let dim = dim as u32;
    file.write_all(&count.to_le_bytes())?;
    file.write_all(&dim.to_le_bytes())?;

    // Write chunk IDs as length-prefixed strings
    for cid in chunk_ids {
        let len = cid.len() as u32;
        file.write_all(&len.to_le_bytes())?;
        file.write_all(cid.as_bytes())?;
    }

    // Write vectors
    for vec in vectors {
        for val in vec {
            file.write_all(&val.to_le_bytes())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::chunker::{Chunk, ChunkType};

    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("zot_test_{tag}_{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn indexable(key: &str) -> IndexableItem {
        IndexableItem {
            item_key: key.to_string(),
            title: "Test".to_string(),
            creators: String::new(),
            abstract_note: String::new(),
            tags: String::new(),
            item_type: "journalArticle".to_string(),
            collections: Vec::new(),
            date: String::new(),
            doi: String::new(),
            publication_title: String::new(),
            fulltext: None,
        }
    }

    fn make_chunks(key: &str, n: usize) -> Vec<Chunk> {
        let mut chunks = vec![Chunk {
            chunk_id: format!("{key}_meta"),
            item_key: key.to_string(),
            chunk_type: ChunkType::Metadata,
            text: "meta".to_string(),
            char_start: 0,
            char_end: 0,
        }];
        for i in 0..n {
            chunks.push(Chunk {
                chunk_id: format!("{key}_{i}"),
                item_key: key.to_string(),
                chunk_type: ChunkType::Fulltext,
                text: format!("chunk {i}"),
                char_start: 0,
                char_end: 0,
            });
        }
        chunks
    }

    // compute_sync_diff classifies keys into new/updated (to_add) and removed
    // (to_delete), and is_stale() reflects whether either is non-empty.
    #[test]
    fn compute_sync_diff_classifies_keys() {
        let mut local = HashMap::new();
        local.insert("SAME".to_string(), 3u64);
        local.insert("CHANGED".to_string(), 3u64);
        local.insert("GONE".to_string(), 9u64);

        let mut remote = HashMap::new();
        remote.insert("SAME".to_string(), 3u64); // unchanged
        remote.insert("CHANGED".to_string(), 4u64); // version bumped
        remote.insert("NEW".to_string(), 1u64); // new remotely
        // GONE absent remotely -> removed.

        let diff = compute_sync_diff(&remote, &local);

        let mut add = diff.to_add.clone();
        add.sort();
        assert_eq!(add, vec!["CHANGED".to_string(), "NEW".to_string()]);
        assert_eq!(diff.to_delete, vec!["GONE".to_string()]);
        assert!(diff.is_stale());

        // Identical maps -> not stale.
        let clean = compute_sync_diff(&local, &local);
        assert!(!clean.is_stale());
        assert!(clean.to_add.is_empty() && clean.to_delete.is_empty());
    }

    // Regression test for vector-store duplication on re-ingest (issue #1, bug 2).
    // Re-adding the same item_key must purge its stale vectors, so vector_count
    // equals the chunk count of the latest add, not the sum of both adds.
    #[test]
    fn add_item_dedups_vectors_on_reingest() {
        let dim = 3;
        let dir = unique_temp_dir("dedup");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        let key = "ITEMKEY1";

        // First add: metadata + 2 fulltext chunks = 3 vectors.
        let chunks1 = make_chunks(key, 2);
        let emb1: Vec<Vec<f32>> = (0..chunks1.len()).map(|_| vec![0.1, 0.2, 0.3]).collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable(key), &chunks1, &emb1, "")
            .unwrap();
        store.commit_writer(writer).unwrap();
        assert_eq!(store.vector_count(), 3);

        // Second add (re-ingest with changed/fewer chunks): metadata + 1 fulltext = 2.
        let chunks2 = make_chunks(key, 1);
        let emb2: Vec<Vec<f32>> = (0..chunks2.len()).map(|_| vec![0.4, 0.5, 0.6]).collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable(key), &chunks2, &emb2, "")
            .unwrap();
        store.commit_writer(writer).unwrap();

        // Must equal the second add's chunk count (2), not the sum (5).
        assert_eq!(store.vector_count(), 2);
        assert!(store.chunk_ids.iter().all(|c| c.starts_with(key)));

        fs::remove_dir_all(&dir).ok();
    }

    // finalize must not blindly record the full remote map: only items that were
    // indexed this run (passed in indexed_versions) plus previously-known items
    // still present remotely get a recorded version. Zero-chunk/never-fetched
    // items are left out for retry; deleted items are dropped.
    #[test]
    fn finalize_only_records_indexed_and_known_present() {
        let dim = 3;
        let dir = unique_temp_dir("finalize");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        // Seed a previously-known item.
        store.meta.items.insert("OLD_PRESENT".to_string(), 5);
        store.meta.items.insert("OLD_DELETED".to_string(), 7);

        let mut remote = HashMap::new();
        remote.insert("OLD_PRESENT".to_string(), 5); // unchanged, still remote
        remote.insert("NEW_OK".to_string(), 10); // indexed this run
        remote.insert("NEW_FAILED".to_string(), 11); // queued but zero chunks, must retry
        // OLD_DELETED absent from remote -> dropped.

        let mut indexed = HashMap::new();
        indexed.insert("NEW_OK".to_string(), 10);

        store.finalize(&remote, indexed).unwrap();

        let items = &store.meta.items;
        assert_eq!(items.get("OLD_PRESENT"), Some(&5));
        assert_eq!(items.get("NEW_OK"), Some(&10));
        assert!(!items.contains_key("NEW_FAILED"), "failed item must be retried");
        assert!(!items.contains_key("OLD_DELETED"), "deleted item must be dropped");
        assert_eq!(items.len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    // Direct checks for the chunk_belongs_to predicate. Keys that share a
    // strict prefix (e.g. "ABC" and "ABCD") must NOT be cross-matched: a chunk
    // emitted for "ABCD" starts with "ABCD_" but not with "ABC_".
    #[test]
    fn chunk_belongs_to_handles_prefix_keys() {
        // Owns: exact id or "{key}_...".
        assert!(chunk_belongs_to("ABC", "ABC"));
        assert!(chunk_belongs_to("ABC_meta", "ABC"));
        assert!(chunk_belongs_to("ABC_0", "ABC"));
        assert!(chunk_belongs_to("ABC_42", "ABC"));

        // Foreign: id starts with the candidate key but the next char is not '_'.
        assert!(!chunk_belongs_to("ABCD_meta", "ABC"));
        assert!(!chunk_belongs_to("ABCD_0", "ABC"));
        assert!(!chunk_belongs_to("ABCMeta", "ABC"));
        assert!(!chunk_belongs_to("ABCMeta_0", "ABC"));

        // Reverse direction: shorter key is a prefix of a longer candidate.
        assert!(!chunk_belongs_to("ABC", "AB"));
        assert!(!chunk_belongs_to("ABC_meta", "AB"));

        // Empty key and unrelated ids.
        assert!(!chunk_belongs_to("", "ABC"));
        assert!(!chunk_belongs_to("XYZ_meta", "ABC"));
    }

    // Regression: add_item must not purge vectors belonging to another item
    // whose key has this item's key as a strict prefix. Bug would have wiped
    // ABCD's vectors when adding ABC, leaving ABCD unsearchable until a force
    // rebuild.
    #[test]
    fn add_item_does_not_purge_prefixed_other_item_vectors() {
        let dim = 3;
        let dir = unique_temp_dir("add_prefix");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        let key_long = "ABCD";
        let key_short = "ABC"; // strict prefix of key_long

        // Add the longer (ABCD) item first: meta + 2 fulltext = 3 vectors.
        let chunks_long = make_chunks(key_long, 2);
        let emb_long: Vec<Vec<f32>> = (0..chunks_long.len())
            .map(|_| vec![0.1, 0.2, 0.3])
            .collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable(key_long), &chunks_long, &emb_long, "")
            .unwrap();
        store.commit_writer(writer).unwrap();
        assert_eq!(store.vector_count(), 3);

        // Now add the shorter (ABC) item: meta + 1 fulltext = 2 vectors. The
        // old `starts_with(key)` predicate would have purged all of ABCD's
        // vectors here (they all start with "ABC"). The fix leaves them alone.
        let chunks_short = make_chunks(key_short, 1);
        let emb_short: Vec<Vec<f32>> = (0..chunks_short.len())
            .map(|_| vec![0.4, 0.5, 0.6])
            .collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable(key_short), &chunks_short, &emb_short, "")
            .unwrap();
        store.commit_writer(writer).unwrap();

        // ABCD's 3 vectors survive + ABC's 2 vectors are added = 5 total.
        // Pre-fix this would have been 2 (only ABC's after the bad purge).
        assert_eq!(store.vector_count(), 5);
        let abcd_count = store.chunk_ids.iter().filter(|c| c.starts_with("ABCD")).count();
        let abc_count = store.chunk_ids.iter().filter(|c| c.starts_with("ABC")).count();
        assert_eq!(abcd_count, 3, "ABCD's vectors must not be purged by ABC");
        assert_eq!(abc_count, 5, "ABC's chunks (3 ABCD + 2 ABC) all start with ABC");

        fs::remove_dir_all(&dir).ok();
    }

    // has_fulltext reflects whether an item's metadata chunk carries non-empty
    // fulltext. This is the guard index_cmd uses to keep previously-good text
    // from being wiped by a transient (empty) retry extraction.
    #[test]
    fn has_fulltext_reflects_stored_fulltext() {
        let dim = 3;
        let dir = unique_temp_dir("has_fulltext");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        // Not indexed yet.
        assert!(!store.has_fulltext("GOOD").unwrap());

        // Item indexed with non-empty fulltext.
        let good = make_chunks("GOOD", 1);
        let emb_good: Vec<Vec<f32>> = (0..good.len()).map(|_| vec![0.1, 0.2, 0.3]).collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable("GOOD"), &good, &emb_good, "real text")
            .unwrap();
        store.commit_writer(writer).unwrap();
        store.reader.reload().unwrap();
        assert!(store.has_fulltext("GOOD").unwrap());

        // Item indexed with empty fulltext (extraction failed).
        let empty = make_chunks("EMPTY", 1);
        let emb_empty: Vec<Vec<f32>> = (0..empty.len()).map(|_| vec![0.4, 0.5, 0.6]).collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable("EMPTY"), &empty, &emb_empty, "")
            .unwrap();
        store.commit_writer(writer).unwrap();
        store.reader.reload().unwrap();
        assert!(!store.has_fulltext("EMPTY").unwrap());

        fs::remove_dir_all(&dir).ok();
    }

    // keys_without_metadata_chunk finds version-recorded items with no metadata
    // chunk (standalone attachments/notes skipped before v0.2.1) so they are
    // re-queued once instead of requiring a --force rebuild.
    #[test]
    fn keys_without_metadata_chunk_finds_unchunked_items() {
        let dim = 3;
        let dir = unique_temp_dir("no_meta_chunk");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        // INDEXED has a metadata chunk; SKIPPED only has a recorded version.
        let chunks = make_chunks("INDEXED", 1);
        let emb: Vec<Vec<f32>> = (0..chunks.len()).map(|_| vec![0.1, 0.2, 0.3]).collect();
        let writer = store.open_writer().unwrap();
        store
            .add_item(&writer, &indexable("INDEXED"), &chunks, &emb, "")
            .unwrap();
        store.commit_writer(writer).unwrap();
        store.reader.reload().unwrap();
        store.meta.items.insert("INDEXED".to_string(), 1);
        store.meta.items.insert("SKIPPED".to_string(), 2);

        let keys = store.keys_without_metadata_chunk().unwrap();
        assert_eq!(keys, vec!["SKIPPED".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    // Regression: delete_items must not purge vectors belonging to another item
    // whose key has this item's key as a strict prefix.
    #[test]
    fn delete_items_does_not_purge_prefixed_other_item_vectors() {
        let dim = 3;
        let dir = unique_temp_dir("del_prefix");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        // Add two items with prefix-sharing keys (meta + 1 fulltext = 2 each).
        for key in ["ABCD", "ABC"] {
            let chunks = make_chunks(key, 1);
            let emb: Vec<Vec<f32>> = (0..chunks.len())
                .map(|_| vec![0.1, 0.2, 0.3])
                .collect();
            let writer = store.open_writer().unwrap();
            store
                .add_item(&writer, &indexable(key), &chunks, &emb, "")
                .unwrap();
            store.commit_writer(writer).unwrap();
        }
        assert_eq!(store.vector_count(), 4);

        // Delete only the shorter (prefix) key.
        store.delete_items(&["ABC".to_string()]).unwrap();

        // ABC's 2 vectors gone, ABCD's 2 vectors remain.
        assert_eq!(store.vector_count(), 2);
        assert!(
            store.chunk_ids.iter().all(|c| c.starts_with("ABCD")),
            "ABCD's vectors must not be purged when deleting ABC",
        );

        fs::remove_dir_all(&dir).ok();
    }

    // stale_no_attachment_keys must return exactly the no-attachment items
    // recorded under an older status version: not current-version ones, and
    // not other statuses regardless of version.
    #[test]
    fn stale_no_attachment_keys_selects_only_outdated_no_attachment() {
        let dim = 3;
        let dir = unique_temp_dir("stale_na");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        let rec = |status: ExtractStatus, sv: u32| ItemStatusRecord {
            status,
            detail: String::new(),
            version: 1,
            text_hash: 0,
            status_version: sv,
        };

        // Old-version no-attachment (pre-field records deserialize as 0): stale.
        store
            .meta
            .item_status
            .insert("OLD_NA".to_string(), rec(ExtractStatus::NoAttachment, 0));
        // Current-version no-attachment: not stale.
        store.meta.item_status.insert(
            "CUR_NA".to_string(),
            rec(ExtractStatus::NoAttachment, STATUS_VERSION),
        );
        // Old-version but not no-attachment: handled by retry_keys, not here.
        store
            .meta
            .item_status
            .insert("OLD_OK".to_string(), rec(ExtractStatus::Ok, 0));

        let keys = store.stale_no_attachment_keys();
        assert_eq!(keys, vec!["OLD_NA".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    // record_status must stamp the current STATUS_VERSION, so a re-attempted
    // item stops appearing in stale_no_attachment_keys afterwards.
    #[test]
    fn record_status_stamps_current_status_version() {
        let dim = 3;
        let dir = unique_temp_dir("stamp_sv");
        let mut store = IndexStore::open_at(dir.clone(), "TestModel", dim).unwrap();

        store.record_status(
            "KEY1",
            ItemStatusRecord {
                status: ExtractStatus::NoAttachment,
                detail: String::new(),
                version: 1,
                text_hash: 0,
                status_version: 0, // caller value is overridden by record_status
            },
        );

        let rec = store.status_of("KEY1").unwrap();
        assert_eq!(rec.status_version, STATUS_VERSION);
        assert!(store.stale_no_attachment_keys().is_empty());

        fs::remove_dir_all(&dir).ok();
    }
}

/// Load vectors and chunk IDs from a binary file.
fn load_vectors(path: &Path, expected_dim: usize) -> Result<(Vec<Vec<f32>>, Vec<String>)> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;

    let mut buf4 = [0u8; 4];
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;
    file.read_exact(&mut buf4)?;
    let dim = u32::from_le_bytes(buf4) as usize;

    if dim != expected_dim {
        bail!(
            "Vector dimension mismatch: file has {dim}, expected {expected_dim}"
        );
    }

    // Read chunk IDs
    let mut chunk_ids = Vec::with_capacity(count);
    for _ in 0..count {
        file.read_exact(&mut buf4)?;
        let len = u32::from_le_bytes(buf4) as usize;
        let mut id_buf = vec![0u8; len];
        file.read_exact(&mut id_buf)?;
        chunk_ids.push(String::from_utf8(id_buf)?);
    }

    // Read vectors
    let mut vectors = Vec::with_capacity(count);
    for _ in 0..count {
        let mut vec = Vec::with_capacity(dim);
        for _ in 0..dim {
            file.read_exact(&mut buf4)?;
            vec.push(f32::from_le_bytes(buf4));
        }
        vectors.push(vec);
    }

    Ok((vectors, chunk_ids))
}


