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

/// Collection info from the Zotero API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoteroCollection {
    pub key: String,
    pub version: u64,
    pub data: ZoteroCollectionData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoteroCollectionData {
    pub key: String,
    pub name: String,
    #[serde(default, rename = "parentCollection")]
    pub parent_collection: serde_json::Value,
}
