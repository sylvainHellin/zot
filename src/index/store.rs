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

    /// Delete all data for specific item keys from the index.
    pub fn delete_items(&mut self, keys: &[String]) -> Result<()> {
        let mut writer: IndexWriter = self
            .index
            .writer(50_000_000)
            .context("Failed to create index writer")?;

        for key in keys {
            writer.delete_term(Term::from_field_text(self.fields.item_key, key));
            self.meta.items.remove(key);

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
        let mut persisted: HashMap<String, u64> = self
            .meta
            .items
            .iter()
            .filter(|(k, _)| remote_versions.contains_key(*k))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (key, version) in indexed_versions {
            persisted.insert(key, version);
        }
        self.meta.items = persisted;
        self.meta.item_count = self.meta.items.len();
        self.meta.chunk_count = self.chunk_ids.len();
        self.meta.last_sync = chrono_now();
        self.meta.model_name = self.meta.model_name.clone();

        // Save vectors
        save_vectors(
            &self.base_dir.join("vectors.bin"),
            &self.vectors,
            &self.chunk_ids,
            self.meta.model_dim,
        )?;

        // Save metadata
        let meta_str = serde_json::to_string_pretty(&self.meta)?;
        fs::write(self.base_dir.join("meta.json"), meta_str)?;

        // Reload reader
        self.reader.reload()?;

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
        self.meta.item_count = 0;
        self.meta.chunk_count = 0;

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
}

/// Filters applicable to both BM25 and vector search.
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    pub tag: Option<String>,
    pub creator: Option<String>, // used for BM25 text matching, not exact filter
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


