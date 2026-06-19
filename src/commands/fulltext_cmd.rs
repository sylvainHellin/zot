use anyhow::{bail, Context, Result};

use crate::index::IndexStore;
use crate::output::{format_output, FulltextOutput};

pub fn run_fulltext(
    key: &str,
    start: Option<usize>,
    end: Option<usize>,
    max_chars: Option<usize>,
    json: bool,
) -> Result<()> {
    let store = IndexStore::open_or_create("BGESmallENV15", 384)?;

    let fulltext = store
        .get_fulltext(key)?
        .context(format!("No fulltext found for item {key}. Is it indexed?"))?;

    let total_chars = fulltext.len();
    let start_pos = start.unwrap_or(0).min(total_chars);
    let mut end_pos = end.unwrap_or(total_chars).min(total_chars);

    // Apply max_chars limit
    if let Some(max) = max_chars {
        end_pos = end_pos.min(start_pos + max);
    }

    if start_pos >= total_chars {
        bail!("Start position {start_pos} exceeds fulltext length {total_chars}");
    }

    let slice = &fulltext[start_pos..end_pos];

    // Get title from the metadata chunk
    let chunks = store.get_item_chunks(key)?;
    let title = chunks
        .first()
        .map(|c| c.title.clone())
        .unwrap_or_default();

    let output = FulltextOutput {
        key: key.to_string(),
        title,
        total_chars,
        start: start_pos,
        end: end_pos,
        text: slice.to_string(),
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
