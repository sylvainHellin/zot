use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A Zotero library item (article, book, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoteroItem {
    pub key: String,
    pub version: u64,
    #[serde(default)]
    pub meta: ZoteroMeta,
    pub data: ZoteroItemData,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoteroMeta {
    #[serde(default, rename = "creatorSummary")]
    pub creator_summary: Option<String>,
    #[serde(default, rename = "parsedDate")]
    pub parsed_date: Option<String>,
    #[serde(default, rename = "numChildren")]
    pub num_children: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoteroItemData {
    pub key: String,
    pub version: u64,
    #[serde(rename = "itemType")]
    pub item_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub date: String,
    #[serde(default, rename = "abstractNote")]
    pub abstract_note: String,
    #[serde(default)]
    pub creators: Vec<Creator>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub collections: Vec<String>,
    #[serde(default, rename = "publicationTitle")]
    pub publication_title: String,
    #[serde(default, rename = "DOI")]
    pub doi: String,
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "dateAdded")]
    pub date_added: String,
    #[serde(default, rename = "dateModified")]
    pub date_modified: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub volume: String,
    #[serde(default)]
    pub pages: String,
    #[serde(default, rename = "citationKey")]
    pub citation_key: String,
    #[serde(default, rename = "contentType")]
    pub content_type: String,
    #[serde(default, rename = "parentItem")]
    pub parent_item: String,
    /// HTML body of a note item (empty for non-notes).
    #[serde(default)]
    pub note: String,

    // Catch any other fields we don't explicitly model
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Creator {
    #[serde(default, rename = "firstName")]
    pub first_name: String,
    #[serde(default, rename = "lastName")]
    pub last_name: String,
    #[serde(default, rename = "creatorType")]
    pub creator_type: String,
    // Some creators only have `name` (single-field)
    #[serde(default)]
    pub name: Option<String>,
}

impl Creator {
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.name {
            name.clone()
        } else if !self.last_name.is_empty() && !self.first_name.is_empty() {
            format!("{}, {}", self.last_name, self.first_name)
        } else if !self.last_name.is_empty() {
            self.last_name.clone()
        } else {
            self.first_name.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub tag: String,
    #[serde(default, rename = "type")]
    pub tag_type: Option<u32>,
}

impl ZoteroItem {
    /// Check if this is a "real" item (not attachment, note, or annotation)
    pub fn is_regular_item(&self) -> bool {
        !matches!(
            self.data.item_type.as_str(),
            "attachment" | "note" | "annotation"
        )
    }

    /// True for any top-level attachment (no parent item).
    pub fn is_standalone_attachment(&self) -> bool {
        self.data.item_type == "attachment" && self.data.parent_item.is_empty()
    }

    /// True for a top-level PDF attachment (the item itself is the file, with no
    /// parent item). Such items are indexed by extracting their own file rather
    /// than a child's.
    pub fn is_standalone_pdf_attachment(&self) -> bool {
        self.is_standalone_attachment() && self.data.content_type == "application/pdf"
    }

    /// True for a top-level HTML snapshot attachment (no parent item). Zotero
    /// stores saved web pages as `contentType == "text/html"`; these are
    /// indexed by extracting readable text from their own file.
    pub fn is_standalone_html_attachment(&self) -> bool {
        self.is_standalone_attachment() && self.data.content_type == "text/html"
    }

    /// True for a top-level note (no parent item).
    pub fn is_standalone_note(&self) -> bool {
        self.data.item_type == "note" && self.data.parent_item.is_empty()
    }

    pub fn creators_string(&self) -> String {
        self.data
            .creators
            .iter()
            .filter(|c| c.creator_type == "author")
            .map(|c| c.display_name())
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn tags_string(&self) -> String {
        self.data
            .tags
            .iter()
            .map(|t| t.tag.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Collection info from the Zotero API.
/// Reserved for a future `collections` listing command; not constructed yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoteroCollection {
    pub key: String,
    pub version: u64,
    pub data: ZoteroCollectionData,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoteroCollectionData {
    pub key: String,
    pub name: String,
    #[serde(default, rename = "parentCollection")]
    pub parent_collection: serde_json::Value,
}
