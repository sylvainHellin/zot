use anyhow::Result;
use std::collections::BTreeSet;

use crate::api::ZoteroClient;
use crate::output::{format_output, AuthorsOutput};

pub fn run_authors(contains: Option<&str>, json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;

    // Fetch all items and extract unique creators
    let items = client.fetch_all_items()?;
    let mut authors = BTreeSet::new();

    for item in &items {
        for creator in &item.data.creators {
            let name = creator.display_name();
            if !name.is_empty() {
                authors.insert(name);
            }
        }
    }

    let mut author_list: Vec<String> = authors.into_iter().collect();

    // Filter by contains if specified
    if let Some(filter) = contains {
        let filter_lower = filter.to_lowercase();
        author_list.retain(|a| a.to_lowercase().contains(&filter_lower));
    }

    let output = AuthorsOutput {
        count: author_list.len(),
        authors: author_list,
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
