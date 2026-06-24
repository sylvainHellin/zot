use anyhow::{Context, Result};
use fastembed::TextRerank;

/// Result of a reranking operation.
#[derive(Debug, Clone)]
pub struct RerankResult {
    pub index: usize,
    pub score: f32,
}

/// Trait for reranking models -- allows future model swaps.
pub trait Reranker: Send + Sync {
    fn rerank(&mut self, query: &str, documents: &[&str], top_n: usize) -> Result<Vec<RerankResult>>;
    /// Human-readable model name. Used for diagnostics / future model selection.
    #[allow(dead_code)]
    fn name(&self) -> &str;
}

/// BGE-reranker-base via fastembed ONNX.
pub struct BgeRerankerBase {
    model: TextRerank,
}

impl BgeRerankerBase {
    pub fn new() -> Result<Self> {
        let model = TextRerank::try_new(
            fastembed::RerankInitOptions::new(fastembed::RerankerModel::BGERerankerBase)
                .with_show_download_progress(true),
        )
        .context("Failed to initialize BGE-reranker-base model")?;

        Ok(Self { model })
    }
}

impl Reranker for BgeRerankerBase {
    fn rerank(&mut self, query: &str, documents: &[&str], top_n: usize) -> Result<Vec<RerankResult>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let docs_owned: Vec<String> = documents.iter().map(|d| d.to_string()).collect();
        let results = self
            .model
            .rerank(query.to_string(), docs_owned, true, None)
            .context("Reranking failed")?;

        let mut reranked: Vec<RerankResult> = results
            .into_iter()
            .map(|r| RerankResult {
                index: r.index,
                score: r.score as f32,
            })
            .collect();

        // Sort by score descending
        reranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        reranked.truncate(top_n);

        Ok(reranked)
    }

    fn name(&self) -> &str {
        "BGERerankerBase"
    }
}
