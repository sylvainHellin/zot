use anyhow::Result;

use crate::api::{SearchParams, ZoteroClient};
use crate::output::{format_output, FindOutput, FindResultOutput};

pub fn run_find(
    query: &str,
    tag: Option<&str>,
    creator: Option<&str>,
    item_type: Option<&str>,
    collection: Option<&str>,
    sort: Option<&str>,
    desc: bool,
    everything: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let client = ZoteroClient::new()?;

    // If creator filter is set, append it to the query (REST API doesn't have a separate creator param)
    let effective_query = match creator {
        Some(c) => format!("{} {}", query, c),
        None => query.to_string(),
    };

    let params = SearchParams {
        tag: tag.map(String::from),
        creator: creator.map(String::from),
        item_type: item_type.map(String::from),
        collection: collection.map(String::from),
        sort: sort.map(String::from),
        desc,
        everything,
        limit: Some(limit),
    };

    let items = client.search_items(&effective_query, &params)?;

    let output = FindOutput {
        query: query.to_string(),
        result_count: items.len(),
        results: items
            .into_iter()
            .filter(|item| item.is_regular_item())
            .map(|item| FindResultOutput {
                key: item.key.clone(),
                title: item.data.title.clone(),
                item_type: item.data.item_type.clone(),
                creators: item.creators_string(),
                date: item.data.date.clone(),
                tags: item.data.tags.iter().map(|t| t.tag.clone()).collect(),
                doi: item.data.doi.clone(),
            })
            .collect(),
    };

    println!("{}", format_output(&output, json));
    Ok(())
}
