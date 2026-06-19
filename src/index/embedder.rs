use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// Trait for embedding models -- allows future model swaps.
pub trait Embedder: Send + Sync {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dim(&self) -> usize;
    fn name(&self) -> &str;
}

/// BGE-small-en-v1.5 embedder via fastembed ONNX.
pub struct BgeSmallEmbedder {
    model: TextEmbedding,
}

impl BgeSmallEmbedder {
    pub fn new() -> Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(true),
        )
        .context("Failed to initialize BGE-small-en-v1.5 embedding model")?;

        Ok(Self { model })
    }
}

impl Embedder for BgeSmallEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let texts_owned: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
        let embeddings = self
            .model
            .embed(texts_owned, None)
            .context("Embedding failed")?;
        Ok(embeddings)
    }

    fn dim(&self) -> usize {
        384
    }

    fn name(&self) -> &str {
        "BGESmallENV15"
    }
}
