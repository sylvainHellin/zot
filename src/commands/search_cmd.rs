use anyhow::Result;

use crate::index::{BgeSmallEmbedder, BgeRerankerBase, IndexStore, SearchFilters};
use crate::output::{format_output, SearchOutput, SearchResultOutput};
use crate::search::hybrid_search;

pub fn run_search(
    query: &str,
    tag: Option<&str>,
    creator: Option<&str>,
    item_type: Option<&str>,
    collection: Option<&str>,
    limit: usize,
    rerank: bool,
    json: bool,
) -> Result<()> {
    let store = IndexStore::open_or_create("BGESmallENV15", 384)?;

    if store.meta().chunk_count == 0 {
        anyhow::bail!("Index is empty. Run `zot index` first.");
    }

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
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
