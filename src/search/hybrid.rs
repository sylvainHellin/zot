use anyhow::Result;
use std::collections::HashMap;

use crate::index::{ChunkData, Embedder, IndexStore, Reranker, SearchFilters};

/// A search result with metadata.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk: ChunkData,
    pub score: f32,
}

/// RRF constant (standard value from the RRF paper).
const RRF_K: f32 = 60.0;

/// Perform hybrid search: BM25 + vector + RRF fusion, optionally with reranking.
pub fn hybrid_search(
    store: &IndexStore,
    embedder: &mut dyn Embedder,
    reranker: Option<&mut dyn Reranker>,
    query: &str,
    filters: &SearchFilters,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let candidates = limit * 5; // over-fetch for fusion

    // 1. BM25 search
    let bm25_results = store.bm25_search(query, filters, candidates)?;

    // 2. Vector search
    let query_embedding = embedder.embed(&[query])?;
    let vector_results = if !query_embedding.is_empty() {
        store.vector_search(&query_embedding[0], filters, candidates)?
    } else {
        Vec::new()
    };

    // 3. RRF fusion
    let fused = rrf_fusion(&bm25_results, &vector_results);

    // Over-fetch to account for deduplication by item_key
    // (multiple chunks from the same paper collapse to one result)
    let rerank_limit = if reranker.is_some() {
        (limit * 5).min(fused.len())
    } else {
        (limit * 5).min(fused.len())
    };
    let top_candidates: Vec<(String, f32)> = fused.into_iter().take(rerank_limit).collect();

    if top_candidates.is_empty() {
        return Ok(Vec::new());
    }

    // 4. Fetch chunk data for candidates
    let mut chunk_map: HashMap<String, ChunkData> = HashMap::new();
    for (chunk_id, _) in &top_candidates {
        if let Some(chunk) = store.get_chunk(chunk_id)? {
            chunk_map.insert(chunk_id.clone(), chunk);
        }
    }

    // 5. Optionally rerank
    if let Some(reranker) = reranker {
        let doc_texts: Vec<String> = top_candidates
            .iter()
            .filter_map(|(cid, _)| chunk_map.get(cid).map(|c| c.text.clone()))
            .collect();
        let doc_refs: Vec<&str> = doc_texts.iter().map(|s| s.as_str()).collect();

        let reranked = if !doc_refs.is_empty() {
            reranker.rerank(query, &doc_refs, limit)?
        } else {
            Vec::new()
        };

        let mut seen_items: HashMap<String, f32> = HashMap::new();
        let mut results = Vec::new();

        let valid_candidates: Vec<&(String, f32)> = top_candidates
            .iter()
            .filter(|(cid, _)| chunk_map.contains_key(cid))
            .collect();

        for rr in &reranked {
            if rr.index >= valid_candidates.len() {
                continue;
            }
            let (chunk_id, _) = valid_candidates[rr.index];
            if let Some(chunk) = chunk_map.get(chunk_id) {
                let existing = seen_items.get(&chunk.item_key).copied().unwrap_or(-1.0);
                if rr.score > existing {
                    results.retain(|r: &SearchResult| r.chunk.item_key != chunk.item_key);
                    seen_items.insert(chunk.item_key.clone(), rr.score);
                    results.push(SearchResult {
                        chunk: chunk.clone(),
                        score: rr.score,
                    });
                }
            }
        }

        if !results.is_empty() {
            results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
            results.truncate(limit);
            return Ok(results);
        }
        // Fall through to RRF-only results if reranker returned nothing
    }

    // 6. Build results from RRF scores (no reranker, or reranker returned nothing)
    let mut seen_items: HashMap<String, f32> = HashMap::new();
    let mut results = Vec::new();

    for (chunk_id, score) in &top_candidates {
        if let Some(chunk) = chunk_map.get(chunk_id) {
            let existing = seen_items.get(&chunk.item_key).copied().unwrap_or(-1.0);
            if *score > existing {
                results.retain(|r: &SearchResult| r.chunk.item_key != chunk.item_key);
                seen_items.insert(chunk.item_key.clone(), *score);
                results.push(SearchResult {
                    chunk: chunk.clone(),
                    score: *score,
                });
            }
            if results.len() >= limit {
                break;
            }
        }
    }

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    results.truncate(limit);

    Ok(results)
}

/// Reciprocal Rank Fusion: merge two ranked lists.
fn rrf_fusion(
    list_a: &[(String, f32)],
    list_b: &[(String, f32)],
) -> Vec<(String, f32)> {
    let mut scores: HashMap<String, f32> = HashMap::new();

    for (rank, (id, _)) in list_a.iter().enumerate() {
        *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
    }

    for (rank, (id, _)) in list_b.iter().enumerate() {
        *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
    }

    let mut fused: Vec<(String, f32)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    fused
}
