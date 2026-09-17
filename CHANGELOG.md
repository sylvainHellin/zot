# Changelog

Notable changes per release. Dates are release dates.

## v0.4.0 (2026-09-17)

Collections became first-class, and a bibliography can leave the library without curl.

### Added

- `zot collections` prints the tree with direct and recursive item counts. `--flat` gives one line each, `--tree-ids` shows the connector IDs. An item in several children counts once in the recursive total.
- `zot collections --create NAME [--parent REF]` and `--rm REF`. The delete prints the blast radius first and asks for `yes` on a leaf, or the collection's own name on a subtree.
- `zot unfiled` lists top-level items that are in no collection, with `--count` for a scripted filing-drift check.
- `zot add --collection` is repeatable, so an item lands in several collections as it is added.
- `zot edit --add-collection` / `--rm-collection` refile an existing item without hand-writing the whole `collections` array.
- `zot export` renders BibTeX, RIS or CSL JSON from explicit keys, a whole collection, or a search via `zot search --export`. `--output` writes a file.
- `zot add` accepts ISBNs and PubMed IDs alongside DOIs, arXiv IDs and PDFs, bare or prefixed. ISBNs resolve through OpenLibrary, PMIDs through one NCBI efetch.
- `zot --version`.
- `zot get` surfaces `shortTitle`.

### Changed

- `zot add` with no `--collection` now warns that the item is unfiled instead of silently dropping it into the library root.
- Anywhere a collection is named, a key, an exact name, or a tree-view ID (`C42`) all work. An ambiguous name refuses rather than guessing.
- BibTeX export strips `file`, `annote`, `abstract` and `keywords` by default, since they describe the library rather than the work; `--raw` keeps them.
- Items Zotero's translator cannot render are named by title on stderr rather than vanishing from the export.

### Fixed

- Web API writes are guarded with the version the request body was computed from, so a concurrent edit is rejected instead of silently overwritten.
- UTF-8 char-boundary panics and a byte/char offset mismatch in fulltext slicing.

## v0.3.0 (2026-07-29)

Write support. Until now the CLI could only read.

### Added

- `zot add` (DOI and arXiv, with `--pdf`), `zot edit`, `zot attach`, `zot rm`, and `zot config set-key`.
- The split that still shapes the tool: `add` goes through Zotero's local connector and needs the desktop app running but no key, while `edit`, `attach` and `rm` go through api.zotero.org and need the one-time key but work with the app closed.

### Fixed

- Index writes are crash-safe, and `--force` recovers an index left broken by an interrupted run.

## v0.2.2 (2026-07-22)

### Added

- Local HTML snapshots count as a fulltext source, so a saved webpage becomes searchable.

## v0.2.1 (2026-07-22)

### Added

- Standalone attachments and notes are indexed, not just PDFs with a parent item.

## v0.2.0 (2026-07-22)

First usable release: hybrid semantic search over a local index, plus the live-API and metadata read paths.

### Added

- `search` (BM25 + vector + RRF, with `--rerank`), `find`, `get`, `fulltext`, `pdf`, `tags`, `authors`, `index`.
- Per-item extraction status (ok / partial / suspicious / failed / no-attachment), surfaced by `zot index issues`, so a gap in the index is visible rather than silent.
- A stale-index warning when the index no longer matches Zotero.

### Changed

- PDF extraction moved from pdf-extract to oxidize-pdf, making the binary self-contained.

### Fixed

- Indexing OOM, index corruption, and incremental indexing silently skipping zero-chunk items.
