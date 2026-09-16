//! `zot unfiled` -- top-level items that are in no collection at all.
//!
//! Pure local read: `/items/top` already excludes child attachments and notes,
//! and an item is unfiled when its `data.collections` is empty.
//!
//! Attachments and notes are reported apart from the rest. A top-level
//! attachment with no parent is not a paper waiting to be filed, it is a stray
//! file that wants reparenting or deleting, so mixing it into the filing list
//! would make the list wrong to act on.

use anyhow::Result;

use crate::api::ZoteroClient;
use crate::api::models::ZoteroItem;
use crate::output::{UnfiledItemOutput, UnfiledOutput, format_output};

pub fn run_unfiled(count: bool, json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;
    let items = client.fetch_top_items()?;

    println!("{}", format_output(&build_output(&items, count), json));
    Ok(())
}

/// Classify the items and shape the report, `count` deciding whether the two
/// listings are carried at all.
///
/// The whole of [`run_unfiled`] apart from the fetch and the print, so a test
/// exercises the real `--count` behaviour rather than a hand-built output.
fn build_output(items: &[ZoteroItem], count: bool) -> UnfiledOutput {
    let (filable, stray) = partition_unfiled(items);
    UnfiledOutput {
        count: filable.len(),
        stray_count: stray.len(),
        items: (!count).then(|| filable.iter().map(|i| item_output(i)).collect()),
        stray: (!count).then(|| stray.iter().map(|i| item_output(i)).collect()),
    }
}

fn item_output(item: &ZoteroItem) -> UnfiledItemOutput {
    UnfiledItemOutput {
        key: item.key.clone(),
        item_type: item.data.item_type.clone(),
        title: item.data.title.clone(),
    }
}

/// True for an item that belongs to no collection.
///
/// Says nothing about the item's type: [`partition_unfiled`] decides whether an
/// unfiled item is a paper to file or a stray file to clean up.
fn is_unfiled(item: &ZoteroItem) -> bool {
    item.data.collections.is_empty()
}

/// Split the unfiled items into the ones worth filing and the stray top-level
/// attachments and notes.
///
/// Anything with a parent item falls out of both lists: a child attachment is
/// filed with its parent and has no collection membership of its own, so
/// reporting it as unfiled would be noise. `/items/top` does not return
/// children anyway; the filter is here so the classification is correct for any
/// item list, not only that one.
fn partition_unfiled(items: &[ZoteroItem]) -> (Vec<&ZoteroItem>, Vec<&ZoteroItem>) {
    let mut filable = Vec::new();
    let mut stray = Vec::new();
    for item in items.iter().filter(|i| is_unfiled(i)) {
        if item.is_regular_item() {
            filable.push(item);
        } else if item.is_standalone_attachment() || item.is_standalone_note() {
            stray.push(item);
        }
    }
    (filable, stray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `collections` and `parentItem` are the two fields the partition reads;
    /// the rest of the item is whatever `ZoteroItemData` defaults to.
    fn item(key: &str, item_type: &str, collections: &[&str], parent: &str) -> ZoteroItem {
        serde_json::from_value(json!({
            "key": key,
            "version": 1,
            "data": {
                "key": key,
                "version": 1,
                "itemType": item_type,
                "title": format!("Title of {key}"),
                "collections": collections,
                "parentItem": parent,
            },
        }))
        .expect("item fixture")
    }

    fn keys(items: &[&ZoteroItem]) -> Vec<String> {
        items.iter().map(|i| i.key.clone()).collect()
    }

    #[test]
    fn an_item_is_unfiled_exactly_when_it_is_in_no_collection() {
        assert!(is_unfiled(&item("AAAA1111", "journalArticle", &[], "")));
        assert!(!is_unfiled(&item(
            "BBBB2222",
            "journalArticle",
            &["KMHNIPDA"],
            ""
        )));
        // Membership in several collections is no less filed than one.
        assert!(!is_unfiled(&item(
            "CCCC3333",
            "journalArticle",
            &["KMHNIPDA", "ABCD1234"],
            ""
        )));
    }

    #[test]
    fn a_standalone_attachment_or_note_is_stray_rather_than_filable() {
        let items = vec![
            item("AAAA1111", "journalArticle", &[], ""),
            item("SLJFJADB", "attachment", &[], ""),
            item("NOTE0001", "note", &[], ""),
        ];
        let (filable, stray) = partition_unfiled(&items);
        assert_eq!(keys(&filable), vec!["AAAA1111"]);
        assert_eq!(keys(&stray), vec!["SLJFJADB", "NOTE0001"]);
    }

    #[test]
    fn a_child_attachment_appears_in_neither_list() {
        // A child's own `collections` is empty, so only the parent check keeps
        // it out. `fetch_top_items` does not return children, but the
        // classification must not depend on that.
        let items = vec![
            item("AAAA1111", "journalArticle", &[], ""),
            item("CHILDPDF", "attachment", &[], "AAAA1111"),
            item("CHILDNOT", "note", &[], "AAAA1111"),
            item("ANNOT001", "annotation", &[], "CHILDPDF"),
        ];
        let (filable, stray) = partition_unfiled(&items);
        assert_eq!(keys(&filable), vec!["AAAA1111"]);
        assert!(stray.is_empty(), "{:?}", keys(&stray));
    }

    #[test]
    fn a_filed_attachment_is_not_reported_either() {
        // Stray is about being unfiled first; an attachment someone deliberately
        // filed is not this command's business.
        let items = vec![item("SLJFJADB", "attachment", &["KMHNIPDA"], "")];
        let (filable, stray) = partition_unfiled(&items);
        assert!(filable.is_empty());
        assert!(stray.is_empty());
    }

    #[test]
    fn all_filable_all_stray_and_empty_inputs_each_partition_cleanly() {
        let all_filable = vec![
            item("AAAA1111", "journalArticle", &[], ""),
            item("BBBB2222", "conferencePaper", &[], ""),
        ];
        let (filable, stray) = partition_unfiled(&all_filable);
        assert_eq!(keys(&filable), vec!["AAAA1111", "BBBB2222"]);
        assert!(stray.is_empty());

        let all_stray = vec![
            item("SLJFJADB", "attachment", &[], ""),
            item("NOTE0001", "note", &[], ""),
        ];
        let (filable, stray) = partition_unfiled(&all_stray);
        assert!(filable.is_empty());
        assert_eq!(keys(&stray), vec!["SLJFJADB", "NOTE0001"]);

        let (filable, stray) = partition_unfiled(&[]);
        assert!(filable.is_empty());
        assert!(stray.is_empty());
    }

    #[test]
    fn count_mode_omits_the_listings_but_still_serialises() {
        let items = vec![
            item("AAAA1111", "journalArticle", &[], ""),
            item("SLJFJADB", "attachment", &[], ""),
        ];
        let counted = build_output(&items, true);
        assert!(counted.items.is_none());
        assert!(counted.stray.is_none());
        let parsed: serde_json::Value =
            serde_json::from_str(&format_output(&counted, true)).expect("valid JSON");
        assert_eq!(parsed["count"], json!(1));
        assert_eq!(parsed["stray_count"], json!(1));
        // null, not [], so "not listed" cannot be read as "there are none".
        assert_eq!(parsed["items"], json!(null));
        assert_eq!(parsed["stray"], json!(null));
        // Human `--count` is the bare filable number, so `N=$(zot unfiled
        // --count)` needs no parsing.
        assert_eq!(format_output(&counted, false), "1");

        let listed = build_output(&items, false);
        assert_eq!(listed.count, 1);
        assert_eq!(listed.stray_count, 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&format_output(&listed, true)).expect("valid JSON");
        assert_eq!(parsed["items"][0]["key"], json!("AAAA1111"));
        assert_eq!(parsed["stray"][0]["key"], json!("SLJFJADB"));
        assert_eq!(parsed["stray"][0]["item_type"], json!("attachment"));

        let human = format_output(&listed, false);
        assert!(human.contains("Unfiled items: 1"), "{human}");
        assert!(human.contains("[AAAA1111]"), "{human}");
        assert!(
            human.contains("Stray top-level attachments and notes: 1"),
            "{human}"
        );
        assert!(human.contains("[SLJFJADB]"), "{human}");
    }
}
