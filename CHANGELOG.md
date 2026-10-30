# Changelog

Notable changes per release. Dates are release dates.

## Unreleased

### Added

- Every command works without the Zotero desktop app: when the local API does not answer and a web API key is configured, reads go to api.zotero.org instead, with one note on stderr. `ZOT_BACKEND=auto|local|web` overrides the choice.
- `zot pdf` and `zot index` find attachment files without the app: the copy in the Zotero data directory when its MD5 matches, else a download cached under `<cache>/zot/attachments/`.
- `zot add` without the app creates the item through the web API from the resolved BibTeX, with every collection and tag in one request, and uploads the PDF. A PDF without an identifier is matched by the DOI or arXiv ID on its first page.
- `zot find` takes a DOI, doi.org URL or arXiv ID as the query and finds the item by its identifier fields; the default title/creator/year search never matched them. An empty default search now points at `--everything` on stderr.
- `zot annotate KEY --from WORDS --to WORDS` creates a Zotero highlight, with `--comment`, `--color` and `--tag`, on a passage found by its first and last words; `--page` restricts the search and `--dry-run` only locates it. Word boxes come from poppler (`pdftotext`, `pdfinfo`), which now has to be installed for this command, and are mapped to PDF user space through the page's MediaBox and rotation.
- `zot tags --automatic` and `--unused` list the tags Zotero imported itself and the tags on no items, with use counts.
- `zot tags --check` flags every tag outside the controlled vocabulary, including tags that only restate the item type or an identifier field, and every item that breaks a facet's cardinality (two states, two `kind:`). The vocabulary is a list of facets, bare or prefixed, each with its values and cardinality, read from `vocabulary` in the config file; a default ships so the check works unconfigured.
- `zot tags --purge-automatic` deletes every automatic tag library-wide, 50 per request, and says how to stop Zotero re-importing them. A name that is automatic on some items and manual on others is kept and reported, since the API deletes a tag by name from every item at once.
- `zot tags --rename OLD NEW` rewrites one tag everywhere and merges it into an existing tag when there is one. Both destructive paths print the blast radius and take a typed confirmation, as `zot collections --rm` does, with `--force` for scripts.

### Fixed

- Tag listing no longer stops early on the web API, whose `Total-Results` counts distinct tag names rather than the rows it pages over.
- Tag type is read from `meta.type`, where the API actually puts it on `/tags`, so automatic tags are no longer indistinguishable from manual ones.
- `zot tags --rename` checks its item list against api.zotero.org before asking, and sweeps the old tag only once the server agrees nothing carries it, so an item tagged on another device cannot be silently stripped instead of renamed.
- `--contains` is rejected with `--purge-automatic` and `--rename` instead of being silently ignored by them.
- Web API requests honour `Backoff` and retry on `429`/`503` with `Retry-After` instead of failing on a throttled response.

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
