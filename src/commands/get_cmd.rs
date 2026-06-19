use anyhow::Result;

use crate::api::ZoteroClient;
use crate::output::{format_output, CreatorOutput, ItemOutput};

pub fn run_get(key: &str, json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;
    let item = client.fetch_item(key)?;

    let output = ItemOutput {
        key: item.key.clone(),
        title: item.data.title.clone(),
        item_type: item.data.item_type.clone(),
        creators: item
            .data
            .creators
            .iter()
            .map(|c| CreatorOutput {
                name: c.display_name(),
                role: c.creator_type.clone(),
            })
            .collect(),
        date: item.data.date.clone(),
        abstract_note: item.data.abstract_note.clone(),
        tags: item.data.tags.iter().map(|t| t.tag.clone()).collect(),
        doi: item.data.doi.clone(),
        url: item.data.url.clone(),
        publication_title: item.data.publication_title.clone(),
        volume: item.data.volume.clone(),
        pages: item.data.pages.clone(),
        collections: item.data.collections.clone(),
        date_added: item.data.date_added.clone(),
        date_modified: item.data.date_modified.clone(),
        citation_key: item.data.citation_key.clone(),
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
