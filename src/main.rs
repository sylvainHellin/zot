mod api;
mod collections;
mod commands;
mod config;
mod index;
mod output;
mod search;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "zot",
    version,
    about = "CLI for querying Zotero libraries with hybrid semantic search"
)]
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

        /// Sub-action (e.g. `issues` to list items with extraction problems)
        #[command(subcommand)]
        action: Option<IndexAction>,
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

        /// Skip the "is the index up to date?" check against Zotero (faster, offline)
        #[arg(long)]
        no_sync_check: bool,

        /// Print the hits as a bibliography instead: bibtex, ris or csljson
        #[arg(long, value_name = "FORMAT")]
        export: Option<String>,
    },

    /// Export items as a bibliography rendered by Zotero (BibTeX, RIS, CSL JSON)
    Export {
        /// Zotero item key(s)
        keys: Vec<String>,

        /// Export a whole collection instead: key, exact name, or tree-view ID
        /// like C42 (direct members only)
        #[arg(long, value_name = "REF", conflicts_with = "keys")]
        collection: Option<String>,

        /// Output format: bibtex, ris or csljson
        #[arg(long, default_value = "bibtex")]
        format: String,

        /// Write to this file instead of stdout (overwritten if it exists)
        #[arg(long, value_name = "PATH")]
        output: Option<String>,

        /// Keep the private BibTeX fields (file, annote, abstract, keywords,
        /// note, urldate) that are stripped by default
        #[arg(long)]
        raw: bool,
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

    /// List the collection tree with item counts, or create a collection
    Collections {
        /// Show only this collection's subtree (key, exact name, or tree-view
        /// ID like C42)
        #[arg(conflicts_with = "create")]
        collection: Option<String>,

        /// Create a collection with this name (web API plus sync)
        #[arg(long, value_name = "NAME", conflicts_with_all = ["flat", "tree_ids"])]
        create: Option<String>,

        /// Parent for --create (key, exact name, or tree-view ID like C42);
        /// omit to create at the top level
        #[arg(long, value_name = "REF", requires = "create")]
        parent: Option<String>,

        /// One line per collection, without the tree indentation
        #[arg(long)]
        flat: bool,

        /// Show connector tree-view IDs (C42) next to the collection keys
        #[arg(long)]
        tree_ids: bool,
    },

    /// List top-level items that are in no collection
    Unfiled {
        /// Print only the counts, for a scripted filing-drift check
        #[arg(long)]
        count: bool,
    },

    /// Add a paper to the library (by DOI/arXiv/ISBN/PMID identifier and/or PDF)
    Add {
        /// Identifier: DOI (10.xxxx/..., doi.org URL), arXiv ID/URL,
        /// ISBN-10/ISBN-13, or PubMed ID
        identifier: Option<String>,

        /// PDF file to save (with an identifier: used with Zotero's
        /// recognizer; alone: metadata is recognized from the PDF)
        #[arg(long)]
        pdf: Option<String>,

        /// Target collection (key, exact name, or tree-view ID like C42),
        /// repeatable. Default: library root. The first is filed by the
        /// connector, any further one via the web API after Zotero syncs.
        #[arg(long = "collection")]
        collections: Vec<String>,

        /// Add without filing: keep the library-root default and skip the
        /// unfiled warning. Cannot be combined with --collection.
        #[arg(long)]
        no_collection: bool,

        /// Tag(s) to set on the new item (repeatable)
        #[arg(long = "tag")]
        tags: Vec<String>,

        /// Add even if the identifier already matches items in the library
        #[arg(long)]
        force: bool,

        /// Skip the automatic search-index refresh after adding
        #[arg(long)]
        no_index: bool,
    },

    /// Update metadata of an existing item (via Zotero web API + sync)
    Edit {
        /// Zotero item key
        key: String,

        /// Set a field: --set field=value (repeatable; Zotero field names,
        /// e.g. title, date, publicationTitle, DOI, abstractNote)
        #[arg(long = "set")]
        sets: Vec<String>,

        /// Add a tag (repeatable)
        #[arg(long = "add-tag")]
        add_tags: Vec<String>,

        /// Remove a tag (repeatable)
        #[arg(long = "rm-tag")]
        rm_tags: Vec<String>,

        /// File the item in a collection: key, exact name, or tree-view ID
        /// (repeatable)
        #[arg(long = "add-collection")]
        add_collections: Vec<String>,

        /// Remove the item from a collection: key, exact name, or tree-view ID
        /// (repeatable)
        #[arg(long = "rm-collection")]
        rm_collections: Vec<String>,

        /// Raw JSON object merged into the item data (for complex fields,
        /// e.g. '{"creators":[...]}')
        #[arg(long)]
        patch: Option<String>,
    },

    /// Attach a file to an existing item (via Zotero web API + sync)
    Attach {
        /// Zotero item key of the parent item
        key: String,

        /// File to attach (PDF, EPUB, ...)
        file: String,

        /// Attachment title (default: filename)
        #[arg(long)]
        title: Option<String>,
    },

    /// Move items to the Zotero trash (via Zotero web API + sync)
    Rm {
        /// Zotero item key(s)
        #[arg(required = true)]
        keys: Vec<String>,
    },

    /// Configure zot (Zotero web API key for write commands)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// (internal) Extract text from a single PDF in an isolated subprocess.
    #[command(name = "__extract-pdf", hide = true)]
    ExtractPdf {
        /// Path to the PDF file
        path: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Store the Zotero web API key (create one at zotero.org/settings/keys)
    SetKey {
        /// API key with write access
        key: String,
    },
    /// Show the stored configuration (key is masked)
    Show,
}

#[derive(Subcommand)]
enum IndexAction {
    /// List indexed items whose PDF extraction failed, was partial, looked
    /// suspicious, or had no attachment.
    Issues,
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;

    let result = match cli.command {
        Commands::Index {
            force,
            status,
            action,
        } => match action {
            Some(IndexAction::Issues) => commands::index_cmd::run_index_issues(json),
            None => {
                if status {
                    commands::index_cmd::run_index_status(json)
                } else {
                    commands::index_cmd::run_index(force, json)
                }
            }
        },
        Commands::Search {
            query,
            tag,
            creator,
            item_type,
            collection,
            limit,
            rerank,
            no_sync_check,
            export,
        } => commands::search_cmd::run_search(commands::search_cmd::SearchArgs {
            query: &query,
            tag: tag.as_deref(),
            creator: creator.as_deref(),
            item_type: item_type.as_deref(),
            collection: collection.as_deref(),
            limit,
            rerank,
            no_sync_check,
            export: export.as_deref(),
            json,
        }),
        Commands::Export {
            keys,
            collection,
            format,
            output,
            raw,
        } => commands::export_cmd::run_export(commands::export_cmd::ExportArgs {
            keys: &keys,
            collection: collection.as_deref(),
            format: &format,
            output: output.as_deref(),
            raw,
            json,
        }),
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
        Commands::Collections {
            collection,
            create,
            parent,
            flat,
            tree_ids,
        } => commands::collections_cmd::run_collections(
            collection.as_deref(),
            create.as_deref(),
            parent.as_deref(),
            flat,
            tree_ids,
            json,
        ),
        Commands::Unfiled { count } => commands::unfiled_cmd::run_unfiled(count, json),
        Commands::Add {
            identifier,
            pdf,
            collections,
            no_collection,
            tags,
            force,
            no_index,
        } => commands::add_cmd::run_add(commands::add_cmd::AddArgs {
            identifier: identifier.as_deref(),
            pdf: pdf.as_deref(),
            collections,
            no_collection,
            tags,
            force,
            no_index,
            json,
        }),
        Commands::Edit {
            key,
            sets,
            add_tags,
            rm_tags,
            add_collections,
            rm_collections,
            patch,
        } => commands::edit_cmd::run_edit(commands::edit_cmd::EditArgs {
            key: &key,
            sets: &sets,
            add_tags: &add_tags,
            rm_tags: &rm_tags,
            add_collections: &add_collections,
            rm_collections: &rm_collections,
            patch: patch.as_deref(),
            json,
        }),
        Commands::Attach { key, file, title } => {
            commands::attach_cmd::run_attach(&key, &file, title.as_deref(), json)
        }
        Commands::Rm { keys } => commands::rm_cmd::run_rm(&keys, json),
        Commands::Config { action } => match action {
            ConfigAction::SetKey { key } => commands::config_cmd::run_set_key(&key, json),
            ConfigAction::Show => commands::config_cmd::run_show(json),
        },
        Commands::ExtractPdf { path } => {
            index::pdf::run_extract_worker(std::path::Path::new(&path))
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
