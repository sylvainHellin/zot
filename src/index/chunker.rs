use serde::{Deserialize, Serialize};

/// Parameters for the chunking strategy.
pub const CHUNK_SIZE: usize = 1500; // chars
pub const CHUNK_OVERLAP: usize = 200; // chars

/// A chunk of text with its metadata and position info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Unique ID for this chunk (item_key + chunk index)
    pub chunk_id: String,
    /// The Zotero item key this chunk belongs to
    pub item_key: String,
    /// Whether this is a metadata or fulltext chunk
    pub chunk_type: ChunkType,
    /// The text content of this chunk
    pub text: String,
    /// Start char offset in the original fulltext (only for fulltext chunks)
    pub char_start: usize,
    /// End char offset in the original fulltext (only for fulltext chunks)
    pub char_end: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChunkType {
    Metadata,
    Fulltext,
}

impl ChunkType {
    pub fn as_str(&self) -> &str {
        match self {
            ChunkType::Metadata => "metadata",
            ChunkType::Fulltext => "fulltext",
        }
    }
}

/// Metadata fields for building the metadata chunk.
pub struct ItemText {
    pub item_key: String,
    pub title: String,
    pub authors: String,
    pub abstract_note: String,
    pub tags: String,
    pub fulltext: Option<String>,
}

/// Generate all chunks for an item (Level 1 metadata + Level 2 fulltext).
pub fn chunk_item(item: &ItemText) -> Vec<Chunk> {
    let mut chunks = Vec::new();

    // Level 1: metadata chunk
    let meta_text = build_metadata_text(item);
    chunks.push(Chunk {
        chunk_id: format!("{}_meta", item.item_key),
        item_key: item.item_key.clone(),
        chunk_type: ChunkType::Metadata,
        text: meta_text,
        char_start: 0,
        char_end: 0,
    });

    // Level 2: fulltext chunks (if available)
    if let Some(fulltext) = &item.fulltext {
        if !fulltext.is_empty() {
            let text_chunks = split_into_chunks(fulltext, CHUNK_SIZE, CHUNK_OVERLAP);
            for (i, (text, start, end)) in text_chunks.into_iter().enumerate() {
                chunks.push(Chunk {
                    chunk_id: format!("{}_{}", item.item_key, i),
                    item_key: item.item_key.clone(),
                    chunk_type: ChunkType::Fulltext,
                    text,
                    char_start: start,
                    char_end: end,
                });
            }
        }
    }

    chunks
}

fn build_metadata_text(item: &ItemText) -> String {
    let mut parts = Vec::new();
    if !item.title.is_empty() {
        parts.push(item.title.clone());
    }
    if !item.authors.is_empty() {
        parts.push(item.authors.clone());
    }
    if !item.abstract_note.is_empty() {
        parts.push(item.abstract_note.clone());
    }
    if !item.tags.is_empty() {
        parts.push(item.tags.clone());
    }
    parts.join("\n")
}

/// Split text into fixed-size chunks with overlap.
/// Returns (chunk_text, char_start, char_end) tuples.
fn split_into_chunks(text: &str, size: usize, overlap: usize) -> Vec<(String, usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < len {
        let end = (start + size).min(len);
        let chunk_text: String = chars[start..end].iter().collect();
        chunks.push((chunk_text, start, end));

        if end >= len {
            break;
        }

        // Advance by (size - overlap), but at least 1
        let step = if size > overlap { size - overlap } else { 1 };
        start += step;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_short_text() {
        let chunks = split_into_chunks("hello world", 100, 20);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, "hello world");
        assert_eq!(chunks[0].1, 0);
        assert_eq!(chunks[0].2, 11);
    }

    #[test]
    fn test_split_with_overlap() {
        // 20 chars, chunk size 10, overlap 3 => step=7
        // chunks: [0..10], [7..17], [14..20]
        let text = "01234567890123456789";
        let chunks = split_into_chunks(text, 10, 3);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, "0123456789");
        assert_eq!(chunks[0].1, 0);
        assert_eq!(chunks[0].2, 10);
        assert_eq!(chunks[1].0, "7890123456");
        assert_eq!(chunks[1].1, 7);
        assert_eq!(chunks[1].2, 17);
        assert_eq!(chunks[2].0, "456789");
        assert_eq!(chunks[2].1, 14);
        assert_eq!(chunks[2].2, 20);
    }

    #[test]
    fn test_chunk_item_no_fulltext() {
        let item = ItemText {
            item_key: "ABC123".to_string(),
            title: "Test Title".to_string(),
            authors: "Smith, J.".to_string(),
            abstract_note: "An abstract.".to_string(),
            tags: "tag1, tag2".to_string(),
            fulltext: None,
        };
        let chunks = chunk_item(&item);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, ChunkType::Metadata);
        assert!(chunks[0].text.contains("Test Title"));
    }
}
