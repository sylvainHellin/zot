use anyhow::Result;

use crate::api::ZoteroClient;
use crate::output::{format_output, TagOutput, TagsOutput};

pub fn run_tags(contains: Option<&str>, json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;
    let mut tags = client.fetch_tags()?;

    // Filter by contains if specified
    if let Some(filter) = contains {
        let filter_lower = filter.to_lowercase();
        tags.retain(|t| t.tag.to_lowercase().contains(&filter_lower));
    }

    // Sort by tag name
    tags.sort_by(|a, b| a.tag.to_lowercase().cmp(&b.tag.to_lowercase()));

    let output = TagsOutput {
        count: tags.len(),
        tags: tags
            .into_iter()
            .map(|t| TagOutput {
                tag: t.tag,
                num_items: t.meta.num_items,
            })
            .collect(),
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
