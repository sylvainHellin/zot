# zot

A CLI for querying and maintaining your local [Zotero](https://www.zotero.org/) library.
It offers hybrid semantic search (BM25 keyword plus vector embeddings, with an optional reranker) and write commands to add papers by DOI/arXiv or PDF, edit metadata, attach files, and trash items.

`zot` talks to the Zotero local HTTP API (the desktop app's built-in server at `http://localhost:23119`), so the library never leaves the machine and no Zotero web API key is required for reading.
For semantic search it builds a local index (embeddings plus a Tantivy full-text index) on disk.

## Requirements

- Rust toolchain (`cargo`), installed via [rustup](https://rustup.rs/).
- Zotero desktop running, with the local API enabled under *Settings → Advanced → "Allow other applications on this computer to communicate with Zotero"*.
  The app must be open when you run `zot`.
- First use of `--rerank` downloads a BGE reranker model of about 1 GB (cached by `fastembed`).
  The default embedding model is small and downloads automatically.

## Install

From a clone of this repo:

```bash
git clone <repo-url> zot
cd zot
cargo install --path .
```

This builds an optimized release binary and places it on your `PATH` at `~/.cargo/bin/zot`, so make sure `~/.cargo/bin` is on your `PATH`.

### Update an existing install

Pull the latest changes and reinstall with `--force`, which is required because cargo otherwise refuses to overwrite an already installed package:

```bash
cd zot
git pull
cargo install --path . --force
```

After updating it is worth refreshing the local index with `zot index` so it picks up any indexing fixes.
If you suspect stale data, do a full rebuild with `zot index --force`.

### Build without installing

```bash
cargo build --release       # binary at target/release/zot
cargo run --release -- <args>
```

## Quick start

```bash
# 1. Build the local semantic index (incremental; re-run anytime to sync)
zot index

# 2. Semantic search
zot search "diffusion models for point clouds"

# 3. Live keyword search straight from Zotero (no index needed)
zot find "kalman filter" --everything

# 4. Add a paper by DOI or arXiv ID (also refreshes the index)
zot add 10.1145/361598.361623 --tag to-read
zot add arXiv:2401.12345 --collection "Large Language Models"
zot add --pdf ~/Downloads/paper.pdf     # metadata recognized from the PDF
```

## Commands

| Command | Description |
|---|---|
| `zot index` | Build or update the local search index (incremental). `--force` for a full rebuild, `--status` for index stats and an extraction-status breakdown. |
| `zot index issues` | List every item whose fulltext extraction has a problem (key, title, status, reason). |
| `zot search <query>` | Hybrid semantic search (BM25 plus vector) over the local index. Warns if the index is out of sync with Zotero; `--no-sync-check` skips the check. |
| `zot find <query>` | Live keyword search via the Zotero local API, always in sync and requiring no index. |
| `zot get <key>` | Full metadata for an item. |
| `zot fulltext <key>` | Stored fulltext for an item, from the local index. `--start`, `--end`, and `--max-chars` return a slice. |
| `zot pdf <key>` | Local file path of an item's PDF attachment. |
| `zot tags` | List tags in the library; `--contains` filters. |
| `zot authors` | List authors and creators in the library; `--contains` filters. |
| `zot collections [ref]` | List the collection tree with direct and subtree item counts. `--flat` drops the indentation, `--tree-ids` shows connector IDs, and a key, exact name, or tree-view ID limits the listing to one subtree. |
| `zot unfiled` | List top-level items that are in no collection; `--count` prints the bare number of them. |
| `zot add [id] [--pdf f]` | Add a paper by DOI/arXiv identifier and/or PDF, locally via Zotero's connector API. |
| `zot edit <key>` | Update item metadata, tags, and collection membership (web API plus sync). |
| `zot attach <key> <file>` | Attach a file to an existing item (web API plus sync). |
| `zot rm <key>...` | Move items to the Zotero trash (web API plus sync, restorable). |
| `zot config` | One-time setup of the Zotero web API key for the write commands. |

Add `--json` to any command for machine-readable output, ready to pipe into `jq`.
For scripts, note that progress and log lines go to stderr while only the result JSON goes to stdout, so do not merge the streams with `2>&1` before parsing.

### `search` options

```bash
zot search "graph neural networks" \
  --tag "to-read" \
  --creator "Hamilton" \
  --type journalArticle \
  --collection ABCD1234 \
  --limit 20 \
  --rerank            # apply BGE reranker for higher precision (slower)
```

Before searching, `zot` makes one cheap call to Zotero to check whether the local index is still in sync, diffing item versions the same way `zot index` does.
If the library has changed since the last `zot index`, it prints a note:

```
Note: index may be out of date -- 4 new/updated, 0 removed since last sync. Run `zot index` to update.
```

If Zotero is not reachable, the note instead says freshness could not be verified, and the search still runs against the local index.
In `--json` mode the message is carried as a `note` field instead of printed.
Skip the check with `--no-sync-check` (or `ZOT_NO_SYNC_CHECK=1`) for a fully offline, slightly faster search.

### `find` options

```bash
zot find "transformer" \
  --tag survey --creator "Vaswani" --type conferencePaper \
  --collection ABCD1234 \
  --sort dateAdded --desc \
  --everything \       # search all fields (default: title/creator/year)
  --limit 25
```

### `index`

```bash
zot index            # incremental sync (only changed/new items)
zot index --force    # full rebuild from scratch
zot index --status   # item/chunk/vector counts, extraction-status breakdown, data dir
zot index issues     # list items with extraction problems and the reason
```

Fulltext is extracted from local Zotero data only, in this order per item:
a PDF attachment (child or standalone), a locally stored HTML snapshot, or the note body for top-level notes.
Nothing is ever fetched from the network; to make a URL-only item searchable, attach a snapshot in Zotero (drag the browser address-bar icon onto the item) and re-run `zot index`.

Every item carries a persisted extraction status shown by `--status` and detailed by `issues`:

| Status | Meaning |
| --- | --- |
| `ok` | fulltext extracted and indexed |
| `partial` | some PDF pages failed; the rest is indexed |
| `suspicious` | extraction reported success but yielded implausibly little text (e.g. a scanned PDF with no text layer) |
| `failed` | the file could not be processed (malformed, password-locked) |
| `no-attachment` | nothing local to extract from |

Items with `failed`, `partial`, or `suspicious` status are retried automatically on the next `zot index` run.

## Adding papers (`zot add`)

`zot add` writes through the local connector API, the same endpoints the browser connector uses, so a single-collection add needs no account, no API key, and no sync, just the running Zotero app.
Filing into several collections is the exception: the connector takes one collection, and the rest go through the web API (see below).

```bash
zot add 10.1038/nature14539                 # DOI (also doi.org URLs)
zot add arXiv:2401.12345                    # arXiv ID (also arxiv.org URLs)
zot add --pdf paper.pdf                     # PDF only: Zotero's recognizer
                                            # creates the metadata item
zot add 10.1000/xyz --pdf paper.pdf         # PDF + identifier (see below)
zot add ... --collection KMHNIPDA           # collection key, exact name, or
                                            # tree-view ID (default: library root)
zot add ... --collection Papers --collection "To Read"   # file in several
zot add ... --no-collection                 # library root on purpose, no warning
zot add ... --tag agents --tag to-read      # tags on the new item
zot add ... --force                         # skip the duplicate guard
zot add ... --no-index                      # skip the automatic index refresh
```

Behavior worth knowing:

- Duplicate guard: before adding, the identifier is checked against the library (DOI, URL, extra fields).
  If it matches, the add is refused and reports the existing item's key; `--force` overrides.
- PDF recognition: with `--pdf`, the file is saved and Zotero's metadata recognizer creates the parent item, waiting until recognition finishes.
  An identifier given alongside the PDF is used only for the duplicate check and as a metadata fallback when recognition fails, so verify that the recognized metadata matches.
  If recognition fails entirely, the PDF is kept as a standalone attachment and, when an identifier was given, the metadata is imported separately; join them with `zot attach` or in the Zotero UI.
- Several collections: `--collection` is repeatable and every value is resolved before anything is written, so a typo in the second one fails before the item exists.
  The connector saves into one collection only, so the first is filed on the spot and the rest are added afterwards through the web API, which needs a configured key (`zot config set-key`) and is checked before the add rather than after it.
  A library root (`L1`, or a group library's root) is only accepted as the sole value, since the collections after the first are written by collection key and a root has none; omitting `--collection` targets `L1` anyway.
- The web API only sees the item once Zotero has synced it up, so filing into more than one collection waits for that sync, polling every 2s for up to 60s and reporting progress on stderr.
  If the sync does not arrive in time the add still succeeds and exits 0, reporting the item as partially filed: it names the collections it is in, the ones it is not, and the exact `zot edit KEY --add-collection ...` to run once the sync catches up.
- No collection: the item goes to the library root and becomes an unfiled item, which every collection-based view then misses, so the add warns on stderr and names the `zot edit KEY --add-collection ...` that files it.
  `--no-collection` is the opt-out for a deliberate root add and silences the warning; it cannot be combined with `--collection`, and that contradiction is rejected before anything is written.
  When the add produced only a standalone attachment (a PDF whose metadata Zotero could not recognize), the warning says so instead: that key is a stray to reparent with `zot attach <item-key> <file>`, which is what `zot unfiled` reports about it too.
  The warning is stderr only, so `--json` stdout stays a single document, where the same fact reads as `collections: null`; that key is always serialised, so a strict consumer can test it.
- Index refresh: after a successful add, the search index updates incrementally so the paper is immediately findable via `zot search`.
- No local delete: the connector API cannot remove items, so a mistaken add must be undone with `zot rm` (web API) or in the Zotero UI.
- Identifiers beyond DOI and arXiv (ISBN, PubMed, plain URLs) are on the roadmap, see `BACKLOG.md`.

## Auditing filing (`zot unfiled`)

```bash
zot unfiled            # key, type and title for every unfiled top-level item
zot unfiled --count    # just the number, for a scripted filing-drift check
```

A pure local read that needs no API key and no index: an item is unfiled when it sits at top level and belongs to no collection.

Top-level attachments and notes are counted and listed apart from the rest.
A standalone attachment with no parent is not a paper waiting to be filed, it is a file that wants reparenting or deleting, so acting on the main list never touches it.
Under `--json` the two lists are `items` and `stray`, alongside the `count` and `stray_count` totals; `--count` sets both lists to `null`, which is distinct from the `[]` that means there are none.
Human `--count` prints the unfiled count alone, so `N=$(zot unfiled --count)` is a number; `--count --json` is the way to a script that also wants `stray_count`.

## Editing the library (`zot edit`, `zot attach`, `zot rm`)

Zotero's local API is read-only, so everything that modifies existing items goes through the Zotero web API (api.zotero.org) and reaches the local library on the next sync, usually within seconds when auto-sync is on.
This requires Zotero sync and a one-time key setup:

```bash
# Create a key with write access at https://www.zotero.org/settings/keys
zot config set-key <API-KEY>     # stored + validated once; rerun to rotate
zot config show                  # config path, masked key, user ID
```

The `ZOTERO_API_KEY` env var overrides the stored key when set.

```bash
zot edit A1B2C3D4 --set date=2024 --set "publicationTitle=Nature"
zot edit A1B2C3D4 --add-tag reviewed --rm-tag to-read
zot edit A1B2C3D4 --add-collection ABCD1234 --rm-collection "To Read"
zot edit A1B2C3D4 --patch '{"creators":[{"creatorType":"author","firstName":"Ada","lastName":"Lovelace"}]}'
zot attach A1B2C3D4 paper.pdf --title "Preprint PDF"
zot rm A1B2C3D4 E5F6G7H8         # moves to trash (restorable in the UI)
```

`--set` uses Zotero field names (`title`, `date`, `DOI`, `abstractNote`, `publicationTitle`, and so on), and unknown fields are rejected by the API.
Edits use optimistic concurrency: the item is read once, the write is guarded by that
version, and a conflicting change landing in between aborts the write untouched so the
command can be re-run against the current state.

`--add-collection` and `--rm-collection` are repeatable and take anything `zot collections` accepts: a collection key, an exact name, or a connector tree-view ID.
Every value is resolved before anything is written, so a typo fails without a partial change, and the library root (`L1`) is rejected since "no collection" is not a collection.
The output reports membership before and after (`collections: [Inbox (ABCD1234)] -> [Inbox (ABCD1234), Read (EFGH5678)]`).
A request that changes nothing, adding a collection the item is already in, writes nothing and still exits 0, reporting `No change` with the item's unchanged version.
A filing loop can therefore re-run over items it has already filed without special-casing them.

`zot rm` never deletes permanently, items go to the Zotero trash.

An item created locally moments ago, for example via `zot add`, must sync up before `edit`, `attach`, or `rm` can see it.
If you get "not found on api.zotero.org", sync Zotero and retry.

## Where data lives

The local index is stored in the platform data directory, `~/Library/Application Support/zot/` on macOS and `~/.local/share/zot/` on Linux:

```
  ├── meta.json      # index metadata (model, sync state)
  ├── tantivy/       # BM25 full-text index
  └── vectors.bin    # embedding vectors
```

The web API key lives in the platform config directory, the same directory on macOS and `~/.config/zot/config.json` on Linux.

To reset the index completely, delete the data directory or run `zot index --force`.

## Troubleshooting

- "Could not reach Zotero. Is it running?" means the Zotero desktop app is not open or the local API is disabled.
  Open Zotero and enable the setting under *Settings → Advanced*.
- Search that returns nothing or looks stale usually means a stale index: run `zot index` to sync, or `zot index --force` for a clean rebuild.

## License

MIT (see `Cargo.toml`).
