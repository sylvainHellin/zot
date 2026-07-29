# zot

A CLI for querying **and maintaining** your local
[Zotero](https://www.zotero.org/) library: hybrid semantic search (BM25
keyword + vector embeddings, with an optional reranker), plus write commands
to add papers by DOI/arXiv or PDF, edit metadata, attach files, and trash
items.

`zot` talks to the **Zotero local HTTP API** (the desktop app's built-in server
at `http://localhost:23119`), so your library never leaves your machine and no
Zotero web API key is required. For semantic search it builds a local index
(embeddings + a Tantivy full-text index) on disk.

## Requirements

- **Rust toolchain** (`cargo`) — install via [rustup](https://rustup.rs/).
- **Zotero desktop** running, with the local API enabled:
  *Settings → Advanced → "Allow other applications on this computer to
  communicate with Zotero"*. The app must be open when you run `zot`.
- First use of `--rerank` downloads a ~1 GB BGE reranker model (cached by
  `fastembed`); the default embedding model is small and downloads automatically.

## Install

From a clone of this repo:

```bash
git clone <repo-url> zot
cd zot
cargo install --path .
```

This builds an optimized release binary and places it on your `PATH` at
`~/.cargo/bin/zot` (make sure `~/.cargo/bin` is on your `PATH`).

### Update an existing install

Pull the latest changes and reinstall with `--force` (required — without it,
cargo refuses because the package is already installed):

```bash
cd zot
git pull
cargo install --path . --force
```

`--force` overwrites the existing `~/.cargo/bin/zot` binary in place.

> **Tip:** after updating, it's worth refreshing your local index
> (`zot index`) so it reflects any indexing fixes. If you suspect stale data,
> do a full rebuild with `zot index --force`.

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
| `zot index` | Build/update the local search index (incremental). `--force` for a full rebuild, `--status` to show index stats. |
| `zot search <query>` | Hybrid semantic search (BM25 + vector) over the local index. Warns if the index is out of sync with Zotero (`--no-sync-check` to skip). |
| `zot find <query>` | Live keyword search via the Zotero local API — always in sync, no index required. |
| `zot get <key>` | Full metadata for an item. |
| `zot fulltext <key>` | Stored fulltext for an item (from the local index). |
| `zot pdf <key>` | Local file path of an item's PDF attachment. |
| `zot tags` | List tags in the library. |
| `zot authors` | List authors/creators in the library. |
| `zot add [id] [--pdf f]` | Add a paper by DOI/arXiv identifier and/or PDF (local, via Zotero's connector API). |
| `zot edit <key>` | Update item metadata (web API + sync). |
| `zot attach <key> <file>` | Attach a file to an existing item (web API + sync). |
| `zot rm <key>...` | Move items to the Zotero trash (web API + sync; restorable). |
| `zot config` | One-time setup of the Zotero web API key for the write commands. |

Add `--json` to any command for machine-readable output (pipe to `jq`).
Note for scripts: progress/log lines go to stderr; only the result JSON is on
stdout — don't merge the streams with `2>&1` before parsing.

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

Before searching, `zot` makes one cheap call to Zotero to check whether the
local index is still in sync (it diffs item versions, the same way `zot index`
does). If the library has changed since the last `zot index`, it prints a note:

```
Note: index may be out of date -- 4 new/updated, 0 removed since last sync. Run `zot index` to update.
```

If Zotero is not reachable, the note instead says freshness could not be
verified, and the search still runs against the local index. In `--json` mode
the message is carried as a `note` field instead of printed. Skip the check with
`--no-sync-check` (or `ZOT_NO_SYNC_CHECK=1`) for a fully offline, slightly faster
search.

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
zot index --status   # show item/chunk/vector counts, model, last sync, data dir
```

## Adding papers (`zot add`)

`zot add` writes through the **local** connector API (the same endpoints the
browser connector uses), so it needs no account, no API key, and no sync —
just the running Zotero app.

```bash
zot add 10.1038/nature14539                 # DOI (also doi.org URLs)
zot add arXiv:2401.12345                    # arXiv ID (also arxiv.org URLs)
zot add --pdf paper.pdf                     # PDF only: Zotero's recognizer
                                            # creates the metadata item
zot add 10.1000/xyz --pdf paper.pdf         # PDF + identifier (see below)
zot add ... --collection KMHNIPDA           # collection key, exact name, or
                                            # tree-view ID (default: library root)
zot add ... --tag agents --tag to-read      # tags on the new item
zot add ... --force                         # skip the duplicate guard
zot add ... --no-index                      # skip the automatic index refresh
```

Behavior worth knowing:

- **Duplicate guard:** before adding, the identifier is checked against the
  library (DOI/URL/extra fields). If it matches, the add is refused with the
  existing item's key — use `--force` to override.
- **PDF recognition:** with `--pdf`, the file is saved and Zotero's metadata
  recognizer creates the parent item (waits until recognition finishes). If an
  identifier was also given it is only used for the duplicate check and as a
  metadata fallback when recognition fails — verify the recognized metadata
  matches. If recognition fails entirely, the PDF is kept as a standalone
  attachment and (when an identifier was given) the metadata is imported
  separately; join them with `zot attach` or in the Zotero UI.
- **Index refresh:** after a successful add, the search index updates
  incrementally so the paper is immediately findable via `zot search`.
- **No local delete:** the connector API cannot remove items, so a mistaken
  add must be undone with `zot rm` (web API) or in the Zotero UI.
- Identifiers beyond DOI/arXiv (ISBN, PubMed, plain URLs) are on the roadmap
  (see `BACKLOG.md`).

## Editing the library (`zot edit`, `zot attach`, `zot rm`)

Zotero's local API is **read-only**, so everything that modifies *existing*
items goes through the Zotero **web API** (api.zotero.org) and reaches the
local library on the next sync (usually seconds with auto-sync on). This
requires Zotero sync and a one-time key setup:

```bash
# Create a key with write access at https://www.zotero.org/settings/keys
zot config set-key <API-KEY>     # stored + validated once; rerun to rotate
zot config show                  # config path, masked key, user ID
```

The `ZOTERO_API_KEY` env var overrides the stored key when set.

```bash
zot edit A1B2C3D4 --set date=2024 --set "publicationTitle=Nature"
zot edit A1B2C3D4 --add-tag reviewed --rm-tag to-read
zot edit A1B2C3D4 --patch '{"creators":[{"creatorType":"author","firstName":"Ada","lastName":"Lovelace"}]}'
zot attach A1B2C3D4 paper.pdf --title "Preprint PDF"
zot rm A1B2C3D4 E5F6G7H8         # moves to trash (restorable in the UI)
```

`--set` uses Zotero field names (`title`, `date`, `DOI`, `abstractNote`,
`publicationTitle`, ...); unknown fields are rejected by the API. Edits use
optimistic concurrency (version-checked; retried once on conflict). `zot rm`
never deletes permanently — items go to the Zotero trash.

Note: an item created locally moments ago (e.g. via `zot add`) must sync up
before `edit`/`attach`/`rm` can see it; if you get "not found on
api.zotero.org", sync Zotero and retry.

## Where data lives

The local index is stored in the platform data directory, on Linux:

```
~/.local/share/zot/
  ├── meta.json      # index metadata (model, sync state)
  ├── tantivy/       # BM25 full-text index
  └── vectors.bin    # embedding vectors
```

The web API key lives in the platform config directory
(`~/.config/zot/config.json` on Linux).

To reset the index completely, delete that directory (or run `zot index --force`).

## Troubleshooting

- **"Could not reach Zotero. Is it running?"** — the Zotero desktop app isn't
  open or the local API is disabled. Open Zotero and enable the setting under
  *Settings → Advanced*.
- **Search returns nothing / looks stale** — run `zot index` to sync, or
  `zot index --force` for a clean rebuild.

## License

MIT (see `Cargo.toml`).
