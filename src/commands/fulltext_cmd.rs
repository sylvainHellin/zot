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

    // Offsets are character positions, matching the `char_start`/`char_end`
    // the chunker records and `zot search` prints. Slicing by byte index here
    // would return a shifted window on any non-ASCII text, and panic outright
    // when the index lands inside a multibyte character.
    let total_chars = fulltext.chars().count();
    let start_pos = start.unwrap_or(0).min(total_chars);
    let mut end_pos = end.unwrap_or(total_chars).min(total_chars);

    // Apply max_chars limit
    if let Some(max) = max_chars {
        end_pos = end_pos.min(start_pos + max);
    }

    if start_pos >= total_chars {
        bail!("Start position {start_pos} exceeds fulltext length {total_chars}");
    }

    let slice = char_slice(&fulltext, start_pos, end_pos);

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
        text: slice,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}

/// Character-indexed substring `[start, end)`, safe on multibyte text.
fn char_slice(s: &str, start: usize, end: usize) -> String {
    s.chars().skip(start).take(end.saturating_sub(start)).collect()
}

#[cfg(test)]
mod tests {
    use super::char_slice;

    #[test]
    fn slices_ascii_by_position() {
        assert_eq!(char_slice("abcdef", 1, 4), "bcd");
        assert_eq!(char_slice("abcdef", 0, 6), "abcdef");
    }

    #[test]
    fn slices_multibyte_text_by_character_not_byte() {
        // "üü" is 2 chars but 4 bytes: byte slicing would panic on [0..1] and
        // return the wrong window for any offset past a multibyte character.
        let text = "üüabc";
        assert_eq!(char_slice(text, 0, 1), "ü");
        assert_eq!(char_slice(text, 2, 5), "abc");
    }

    #[test]
    fn out_of_range_end_is_clamped_not_panicking() {
        assert_eq!(char_slice("abc", 1, 99), "bc");
        assert_eq!(char_slice("abc", 5, 9), "");
        assert_eq!(char_slice("abc", 3, 1), "");
    }
}
