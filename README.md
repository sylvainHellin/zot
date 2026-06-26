# zot

A CLI for querying your local [Zotero](https://www.zotero.org/) library with
hybrid semantic search (BM25 keyword + vector embeddings, with an optional
reranker).

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

Add `--json` to any command for machine-readable output (pipe to `jq`).

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

## Where data lives

The local index is stored in the platform data directory, on Linux:

```
~/.local/share/zot/
  ├── meta.json      # index metadata (model, sync state)
  ├── tantivy/       # BM25 full-text index
  └── vectors.bin    # embedding vectors
```

To reset the index completely, delete that directory (or run `zot index --force`).

## Troubleshooting

- **"Could not reach Zotero. Is it running?"** — the Zotero desktop app isn't
  open or the local API is disabled. Open Zotero and enable the setting under
  *Settings → Advanced*.
- **Search returns nothing / looks stale** — run `zot index` to sync, or
  `zot index --force` for a clean rebuild.

## License

MIT (see `Cargo.toml`).
