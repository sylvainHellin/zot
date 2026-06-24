use serde::Serialize;
use serde_json;

/// Format output as either human-readable or JSON.
pub fn format_output<T: Serialize + HumanDisplay>(data: &T, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("JSON error: {e}"))
    } else {
        data.human_display()
    }
}

/// Trait for human-readable display of data types.
pub trait HumanDisplay {
    fn human_display(&self) -> String;
}

// ---- Output data types ----

#[derive(Debug, Serialize)]
pub struct SearchOutput {
    pub query: String,
    pub result_count: usize,
    pub results: Vec<SearchResultOutput>,
}

#[derive(Debug, Serialize)]
pub struct SearchResultOutput {
    pub key: String,
    pub title: String,
    pub item_type: String,
    pub creators: String,
    pub date: String,
    pub score: f32,
    pub snippet: String,
    pub char_start: u64,
    pub char_end: u64,
    pub chunk_type: String,
}

impl HumanDisplay for SearchOutput {
    fn human_display(&self) -> String {
        let mut out = format!(
            "Search: \"{}\"\nResults: {}\n",
            self.query, self.result_count
        );
        for (i, r) in self.results.iter().enumerate() {
            out.push_str(&format!(
                "\n{}. [{}] {} (score: {:.3})\n   {} | {} | {}\n",
                i + 1,
                r.key,
                r.title,
                r.score,
                r.creators,
                r.date,
                r.item_type,
            ));
            if !r.snippet.is_empty() {
                // Truncate snippet for display
                let snippet = if r.snippet.len() > 200 {
                    format!("{}...", &r.snippet[..200])
                } else {
                    r.snippet.clone()
                };
                out.push_str(&format!("   > {}\n", snippet.replace('\n', " ")));
            }
            if r.chunk_type == "fulltext" {
                out.push_str(&format!(
                    "   chars {}-{}\n",
                    r.char_start, r.char_end
                ));
            }
        }
        out
    }
}

#[derive(Debug, Serialize)]
pub struct FindOutput {
    pub query: String,
    pub result_count: usize,
    pub results: Vec<FindResultOutput>,
}

#[derive(Debug, Serialize)]
pub struct FindResultOutput {
    pub key: String,
    pub title: String,
    pub item_type: String,
    pub creators: String,
    pub date: String,
    pub tags: Vec<String>,
    pub doi: String,
}

impl HumanDisplay for FindOutput {
    fn human_display(&self) -> String {
        let mut out = format!(
            "Find: \"{}\"\nResults: {}\n",
            self.query, self.result_count
        );
        for (i, r) in self.results.iter().enumerate() {
            out.push_str(&format!(
                "\n{}. [{}] {}\n   {} | {} | {}\n",
                i + 1,
                r.key,
                r.title,
                r.creators,
                r.date,
                r.item_type,
            ));
            if !r.tags.is_empty() {
                out.push_str(&format!("   tags: {}\n", r.tags.join(", ")));
            }
        }
        out
    }
}

#[derive(Debug, Serialize)]
pub struct ItemOutput {
    pub key: String,
    pub title: String,
    pub item_type: String,
    pub creators: Vec<CreatorOutput>,
    pub date: String,
    pub abstract_note: String,
    pub tags: Vec<String>,
    pub doi: String,
    pub url: String,
    pub publication_title: String,
    pub volume: String,
    pub pages: String,
    pub collections: Vec<String>,
    pub date_added: String,
    pub date_modified: String,
    pub citation_key: String,
}

#[derive(Debug, Serialize)]
pub struct CreatorOutput {
    pub name: String,
    pub role: String,
}

impl HumanDisplay for ItemOutput {
    fn human_display(&self) -> String {
        let mut out = format!("[{}] {}\n", self.key, self.title);
        out.push_str(&format!("Type: {}\n", self.item_type));

        if !self.creators.is_empty() {
            let names: Vec<&str> = self.creators.iter().map(|c| c.name.as_str()).collect();
            out.push_str(&format!("Authors: {}\n", names.join("; ")));
        }
        if !self.date.is_empty() {
            out.push_str(&format!("Date: {}\n", self.date));
        }
        if !self.publication_title.is_empty() {
            out.push_str(&format!("Publication: {}\n", self.publication_title));
        }
        if !self.doi.is_empty() {
            out.push_str(&format!("DOI: {}\n", self.doi));
        }
        if !self.url.is_empty() {
            out.push_str(&format!("URL: {}\n", self.url));
        }
        if !self.tags.is_empty() {
            out.push_str(&format!("Tags: {}\n", self.tags.join(", ")));
        }
        if !self.abstract_note.is_empty() {
            let abs = if self.abstract_note.len() > 500 {
                format!("{}...", &self.abstract_note[..500])
            } else {
                self.abstract_note.clone()
            };
            out.push_str(&format!("\nAbstract:\n{}\n", abs));
        }
        out
    }
}

#[derive(Debug, Serialize)]
pub struct FulltextOutput {
    pub key: String,
    pub title: String,
    pub total_chars: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl HumanDisplay for FulltextOutput {
    fn human_display(&self) -> String {
        format!(
            "[{}] {} (chars {}-{} of {})\n\n{}",
            self.key, self.title, self.start, self.end, self.total_chars, self.text
        )
    }
}

#[derive(Debug, Serialize)]
pub struct TagsOutput {
    pub count: usize,
    pub tags: Vec<TagOutput>,
}

#[derive(Debug, Serialize)]
pub struct TagOutput {
    pub tag: String,
    pub num_items: u32,
}

impl HumanDisplay for TagsOutput {
    fn human_display(&self) -> String {
        let mut out = format!("Tags: {}\n\n", self.count);
        for t in &self.tags {
            out.push_str(&format!("  {} ({})\n", t.tag, t.num_items));
        }
        out
    }
}

#[derive(Debug, Serialize)]
pub struct AuthorsOutput {
    pub count: usize,
    pub authors: Vec<String>,
}

impl HumanDisplay for AuthorsOutput {
    fn human_display(&self) -> String {
        let mut out = format!("Authors: {}\n\n", self.count);
        for a in &self.authors {
            out.push_str(&format!("  {}\n", a));
        }
        out
    }
}

#[derive(Debug, Serialize)]
pub struct PdfOutput {
    pub key: String,
    pub path: Option<String>,
}

impl HumanDisplay for PdfOutput {
    fn human_display(&self) -> String {
        match &self.path {
            Some(p) => p.clone(),
            None => format!("No PDF found for item {}", self.key),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct IndexStatusOutput {
    pub item_count: usize,
    pub chunk_count: usize,
    pub vector_count: usize,
    pub items_without_fulltext: usize,
    pub model_name: String,
    pub model_dim: usize,
    pub last_sync: String,
    pub data_dir: String,
}

impl HumanDisplay for IndexStatusOutput {
    fn human_display(&self) -> String {
        format!(
            "Index Status\n  Items: {}\n  Chunks: {}\n  Vectors: {}\n  Items without fulltext: {}\n  Model: {} (dim {})\n  Last sync: {}\n  Data dir: {}",
            self.item_count,
            self.chunk_count,
            self.vector_count,
            self.items_without_fulltext,
            self.model_name,
            self.model_dim,
            if self.last_sync.is_empty() {
                "never"
            } else {
                &self.last_sync
            },
            self.data_dir,
        )
    }
}
