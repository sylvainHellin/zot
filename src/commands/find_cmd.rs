use anyhow::Result;

use crate::api::resolve::{parse_identifier, Identifier};
use crate::api::{SearchParams, ZoteroClient};
use crate::output::{format_output, FindOutput, FindResultOutput};

/// Arguments of `zot find`, bundled because the command takes more of them
/// than a plain function signature should carry.
pub struct FindArgs<'a> {
    pub query: &'a str,
    pub tag: Option<&'a str>,
    pub creator: Option<&'a str>,
    pub item_type: Option<&'a str>,
    pub collection: Option<&'a str>,
    pub sort: Option<&'a str>,
    pub desc: bool,
    pub everything: bool,
    pub limit: usize,
    pub json: bool,
}

pub fn run_find(args: FindArgs) -> Result<()> {
    let FindArgs {
        query,
        tag,
        creator,
        item_type,
        collection,
        sort,
        desc,
        everything,
        limit,
        json,
    } = args;
    let client = ZoteroClient::new()?;

    // The default quicksearch covers title, creator and year only, so a DOI or
    // arXiv ID never matches there. An identifier-shaped query searches every
    // field instead and keeps the items whose identifier fields carry it, since
    // `everything` also matches PDF fulltext that merely cites it.
    let identifier = match parse_identifier(query) {
        Ok(id @ (Identifier::Doi(_) | Identifier::Arxiv(_))) => Some(id),
        _ => None,
    };
    let (query, everything) = match &identifier {
        Some(id) => (id.dedup_query(), true),
        None => (query, everything),
    };

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

    let mut items = client.search_items(&effective_query, &params)?;
    if let Some(id) = &identifier {
        let needle = id.dedup_query().to_lowercase();
        items.retain(|i| carries_identifier(i, &needle));
    }
    if items.is_empty() && !everything && !json {
        eprintln!(
            "No title, creator or year matches; --everything also searches DOIs, tags, notes and full text."
        );
    }

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

/// Whether an item's DOI, URL or Extra field holds the lowercased identifier.
fn carries_identifier(item: &crate::api::ZoteroItem, needle: &str) -> bool {
    let extra = item
        .data
        .extra
        .get("extra")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    item.data.doi.to_lowercase().contains(needle)
        || item.data.url.to_lowercase().contains(needle)
        || extra.to_lowercase().contains(needle)
}
