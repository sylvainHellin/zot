mod api;
mod commands;
mod index;
mod output;
mod search;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "zot", about = "CLI for querying Zotero libraries with hybrid semantic search")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output as JSON (for piping to jq or programmatic use)
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Build/update the local search index
    Index {
        /// Force full rebuild (ignore existing index)
        #[arg(long)]
        force: bool,

        /// Show index status
        #[arg(long)]
        status: bool,
    },

    /// Hybrid semantic search (BM25 + vector) using local index
    Search {
        /// Search query
        query: String,

        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,

        /// Filter by creator/author
        #[arg(long)]
        creator: Option<String>,

        /// Filter by item type (e.g. journalArticle, conferencePaper)
        #[arg(long, name = "type")]
        item_type: Option<String>,

        /// Filter by collection key
        #[arg(long)]
        collection: Option<String>,

        /// Maximum number of results
        #[arg(long, default_value = "10")]
        limit: usize,

        /// Apply BGE reranker for higher precision (slower, downloads 1GB model on first use)
        #[arg(long)]
        rerank: bool,
    },

    /// Keyword search via Zotero REST API (live, always in sync)
    Find {
        /// Search query
        query: String,

        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,

        /// Filter by creator/author
        #[arg(long)]
        creator: Option<String>,

        /// Filter by item type
        #[arg(long, name = "type")]
        item_type: Option<String>,

        /// Filter by collection key
        #[arg(long)]
        collection: Option<String>,

        /// Sort field (e.g. dateAdded, title, date)
        #[arg(long)]
        sort: Option<String>,

        /// Sort descending
        #[arg(long)]
        desc: bool,

        /// Search all fields (default: title/creator/year)
        #[arg(long)]
        everything: bool,

        /// Maximum number of results
        #[arg(long, default_value = "25")]
        limit: usize,
    },

    /// Get full metadata for an item
    Get {
        /// Zotero item key
        key: String,
    },

    /// Get stored fulltext for an item (from local index)
    Fulltext {
        /// Zotero item key
        key: String,

        /// Start character position
        #[arg(long)]
        start: Option<usize>,

        /// End character position
        #[arg(long)]
        end: Option<usize>,

        /// Maximum number of characters to return
        #[arg(long)]
        max_chars: Option<usize>,
    },

    /// Get local PDF file path for an item
    Pdf {
        /// Zotero item key
        key: String,
    },

    /// List tags in the library
    Tags {
        /// Filter tags containing this string
        #[arg(long)]
        contains: Option<String>,
    },

    /// List authors/creators in the library
    Authors {
        /// Filter authors containing this string
        #[arg(long)]
        contains: Option<String>,
    },

    /// (internal) Extract text from a single PDF in an isolated subprocess.
    #[command(name = "__extract-pdf", hide = true)]
    ExtractPdf {
        /// Path to the PDF file
        path: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;

    let result = match cli.command {
        Commands::Index { force, status } => {
            if status {
                commands::index_cmd::run_index_status(json)
            } else {
                commands::index_cmd::run_index(force, json)
            }
        }
        Commands::Search {
            query,
            tag,
            creator,
            item_type,
            collection,
            limit,
            rerank,
        } => commands::search_cmd::run_search(
            &query,
            tag.as_deref(),
            creator.as_deref(),
            item_type.as_deref(),
            collection.as_deref(),
            limit,
            rerank,
            json,
        ),
        Commands::Find {
            query,
            tag,
            creator,
            item_type,
            collection,
            sort,
            desc,
            everything,
            limit,
        } => commands::find_cmd::run_find(
            &query,
            tag.as_deref(),
            creator.as_deref(),
            item_type.as_deref(),
            collection.as_deref(),
            sort.as_deref(),
            desc,
            everything,
            limit,
            json,
        ),
        Commands::Get { key } => commands::get_cmd::run_get(&key, json),
        Commands::Fulltext {
            key,
            start,
            end,
            max_chars,
        } => commands::fulltext_cmd::run_fulltext(&key, start, end, max_chars, json),
        Commands::Pdf { key } => commands::pdf_cmd::run_pdf(&key, json),
        Commands::Tags { contains } => {
            commands::tags_cmd::run_tags(contains.as_deref(), json)
        }
        Commands::Authors { contains } => {
            commands::authors_cmd::run_authors(contains.as_deref(), json)
        }
        Commands::ExtractPdf { path } => {
            index::pdf::run_extract_worker(std::path::Path::new(&path))
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
