# zot

A CLI for querying and maintaining your local [Zotero](https://www.zotero.org/) library.
It offers hybrid semantic search (BM25 keyword plus vector embeddings, with an optional reranker) and write commands to add papers by DOI/arXiv/ISBN/PubMed ID or PDF, edit metadata, attach files, and trash items.

`zot` reads through the Zotero local HTTP API (the desktop app's built-in server at `http://localhost:23119`) whenever the app is running, since that is fastest and needs no key.
When the app is closed it falls back to the Zotero web API (api.zotero.org) with the key from `zot config set-key`, so every command also works on a machine where the app is not open (see [Without the desktop app](#without-the-desktop-app)).
For semantic search it builds a local index (embeddings plus a Tantivy full-text index) on disk.

## Requirements

- Rust toolchain (`cargo`), installed via [rustup](https://rustup.rs/).
- Zotero desktop running, with the local API enabled under *Settings → Advanced → "Allow other applications on this computer to communicate with Zotero"*, or a web API key configured with `zot config set-key` for use without the app.
  With both, the app is used when it answers and the web API otherwise.
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

# 4. Add a paper by DOI, arXiv ID, ISBN or PubMed ID (also refreshes the index)
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
| `zot tags` | List tags in the library; `--contains` filters the listing and `--check`. `--automatic` lists only the tags Zotero imported itself, `--unused` those on no items, `--check` flags everything outside the controlled vocabulary plus the items that break a facet's cardinality. `--purge-automatic` deletes every automatic tag and `--rename OLD NEW` merges one tag into another, both library-wide and after a typed confirmation (`--force` skips it). |
| `zot authors` | List authors and creators in the library; `--contains` filters. |
| `zot collections [ref]` | List the collection tree with direct and subtree item counts. `--flat` drops the indentation, `--tree-ids` shows connector IDs, and a key, exact name, or tree-view ID limits the listing to one subtree. `--create NAME` creates a collection instead, under `--parent <ref>` or at the top level (web API plus sync). `--rm <ref>` permanently deletes one and every subcollection under it, after a typed confirmation (`--force` skips it); the items in it are unfiled, not deleted. |
| `zot unfiled` | List top-level items that are in no collection; `--count` prints the bare number of them. |
| `zot export <key>...` | Export items as a bibliography rendered by Zotero. `--collection <ref>` exports a whole collection instead, `--format bibtex\|ris\|csljson` picks the format, `--output PATH` writes a file, `--raw` keeps the private BibTeX fields. |
| `zot add [id] [--pdf f]` | Add a paper by DOI/arXiv/ISBN/PubMed identifier and/or PDF, locally via Zotero's connector API. |
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

### `export` options

```bash
zot export 6V9QI2NL VZUBFXFP                    # BibTeX on stdout
zot export --collection "EC3-2026" --output refs.bib   # a collection to a file
zot export --collection C39                     # by connector tree-view ID
zot export KEY --format csljson                 # or ris
zot search "protein folding" --export bibtex    # the search hits as a bibliography
```

Zotero renders the bibliography itself, so the output is exactly what its BibTeX, RIS and CSL JSON translators produce.
`--collection` takes the same reference as everywhere else in the CLI, a key, an exact name, or a connector tree-view ID, and exports the collection's direct members without walking subcollections.

BibTeX arrives with fields that describe your library rather than the work, and they are stripped by default:

| Field | Why |
|---|---|
| `file` | absolute path into your Zotero storage directory |
| `annote` | the item's child notes |
| `abstract` | a paragraph per entry, rarely wanted in a `.bib` |
| `keywords` | your tags, not the publisher's |

`--raw` keeps all of them.
`note` and `urldate` are left alone: Zotero puts the **Extra** field in `note`, which routinely holds the arXiv ID or a version number, and `urldate` comes from the access date, which biblatex pairs with `url` on `@online` and `@misc` entries.
RIS and CSL JSON carry none of these, so `--raw` changes nothing there and both are always passed through as Zotero rendered them, except that CSL JSON is re-serialized (and so pretty-printed with two-space indent) because merging several pages into one array requires parsing it.

Some items have no translator output at all: a standalone note or attachment, for instance.
A rendered entry's citation key (`jumperHighlyAccurateProtein2021`) has no relation to the Zotero item key, so `zot` counts entries per request and, when a batch comes back short, re-requests its keys one at a time to find exactly which item was dropped.
Each one is named on stderr:

```
Warning: no bibtex entry for "note: Curriculum" (VETJB3WE) -- the Zotero translator produced no entry for it.
```

Warnings always go to stderr, so stdout stays exactly the bibliography, and a `--output` file stays exactly the bibliography.
Under `--json` stdout is one document instead: `{format, requested, exported, path, content}`.
`path` and `content` are both always present with exactly one of them non-null, so the shape does not vary between runs: `content` holds the bibliography as a string, and `--output` sets `path` instead and leaves `content` null, since the file is then the payload.
A `dropped` array appears alongside them only when something was dropped.
`zot search --export` nests that same object under an `export` key alongside the search results, so one `--json` document still carries both.

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
zot add 978-0-262-03561-3                   # ISBN-10 or ISBN-13 (also ISBN:...,
                                            # hyphens and spaces optional)
zot add 23193287                            # PubMed ID (also PMID:... / PubMed:...)
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

- Identifier forms: a bare value is read as a DOI when it starts with `10.` and contains a `/`, as an arXiv ID when it looks like `2401.12345`, as an ISBN when it normalises to 10 or 13 digits **and its check digit matches**, and as a PubMed ID when it is a plain number of up to 9 digits with no leading zero.
  The check digit is what keeps a 13-digit order number or timestamp from resolving as a book; a number that fails it is rejected rather than guessed at.
  The `ISBN:`, `PMID:` and `PubMed:` prefixes (case-insensitive) say which one you mean and give a type-specific error when the value is malformed.
- Where the metadata comes from: DOIs use doi.org content negotiation and arXiv IDs use arxiv.org's BibTeX export, both as before.
  A PubMed ID makes one NCBI `efetch` call in MEDLINE form; when the record carries a DOI the publisher's BibTeX is used instead, since it is richer, and otherwise the MEDLINE record is mapped to BibTeX directly.
  An ISBN goes to OpenLibrary (`openlibrary.org/api/books`), which answers with author names inline in a single request, and is mapped to a BibTeX `@book`.
  There is no Google Books fallback: its keyless endpoint answers 429 for everyone sharing the anonymous project, so a second lookup would only delay the error.
- Duplicate guard: before adding, the identifier is checked against the library (DOI, URL, extra fields).
  If it matches, the add is refused and reports the existing item's key; `--force` overrides.
  An ISBN is checked differently, because Zotero stores it hyphenated and its quicksearch does not match across the hyphens: the books are listed and their `ISBN` fields compared digit by digit, which catches a book whether it is stored hyphenated or not and whether its field holds one ISBN or the print and electronic ones together.
  One gap remains: a PubMed ID resolved through its DOI leaves an item carrying that DOI and no PMID, so re-adding the same PMID is not caught (re-adding its DOI is).
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
- Adding by plain URL is on the roadmap as Plane issue PERSO-196; it needs Zotero's web translators, which the connector API does not expose.
- With the desktop app closed, `zot add` creates the item through the web API instead (see the next section).

## Without the desktop app

Every command first probes the local API.
When the app does not answer and a web API key is configured, `zot` prints one note on stderr and serves the command from api.zotero.org instead.
The web library is the synced one, so it can be ahead of a desktop install that has not synced for a while.
`ZOT_BACKEND=local` or `ZOT_BACKEND=web` forces one side; the default `auto` picks as above.

What changes on the web side:

- Reads (`find`, `get`, `authors`, `tags`, `unfiled`, `collections`, `export`, the `search` sync check, and the reads behind `edit`) use the same queries against api.zotero.org and return the same output, a little slower.
- `zot pdf` and `zot index` need the file on disk: a copy under the Zotero data directory (`~/Zotero`, or `ZOTERO_DATA_DIR`) is used when its MD5 matches the server's, and otherwise the file is downloaded once into `<cache>/zot/attachments/<key>/` and reused while it stays current.
- `zot add` with an identifier converts the resolved BibTeX to a Zotero item and creates it in one request with all its collections and tags, so there is no sync wait and no partial filing.
  A PDF is uploaded as the item's attachment.
  A PDF without an identifier has no recognizer to go to, so `zot add` looks for a DOI or arXiv ID on its first page and uses that, with a warning to check the match; when it finds none, the PDF is saved as a standalone attachment and the warning says to re-run with the identifier.
- Collections resolve by key or exact name only, since tree-view IDs such as `C42` and the library root `L1` come from the app.

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

## Creating collections (`zot collections --create`)

```bash
zot collections --create "Reading list"                    # at the top level
zot collections --create "2026" --parent ABCD1234          # under a collection, by key
zot collections --create "2026" --parent "Reading list"    # parent by exact name
zot collections --create "2026" --parent C42               # parent by connector tree-view ID
```

`--parent` takes anything the rest of the tool takes: a collection key, an exact name, or a tree-view ID.
Omit it to create at the top level; the library root `L1` names the same place but only resolves while Zotero is running, since tree-view IDs come from the connector.
The parent is resolved before the write, so an unknown one fails without creating anything.
`--parent` is only meaningful with `--create` and is rejected without it, as is the listing positional together with `--create`.
This is a web API write and needs the same key as `zot edit`.

A name a sibling under the same parent already carries is refused, naming the existing key.
Zotero itself accepts the duplicate (verified against api.zotero.org), and every later `zot collections NAME` or `--add-collection NAME` would then be ambiguous.
The comparison is exact, so two siblings differing only by case stay individually addressable; creating one prints a warning on stderr.
The check reads the local library while the write goes upstream, so a sibling created seconds ago and not yet synced down is invisible to it and the duplicate goes through.

The new collection reaches the local library on the next sync.
The command waits up to 20 seconds for it and reports whether it arrived (`synced_local` under `--json`); a collection that has not arrived yet exists upstream regardless and appears in `zot collections` once Zotero syncs, so re-running `--create` would make a second one rather than retry the first.

The output is the created collection alone, so `--create` cannot be combined with the listing flags `--flat` and `--tree-ids`.

## Deleting a collection (`zot collections --rm`)

```bash
zot collections --rm ABCD1234            # by key
zot collections --rm "Reading list"      # by exact name
zot collections --rm C42                 # by connector tree-view ID
zot collections --rm ABCD1234 --force    # no prompt, for scripts
```

**This is permanent.**
Zotero has no trash for collections, so unlike `zot rm`, which moves items to a trash you can empty or restore from in the Zotero UI, a deleted collection cannot be brought back from anywhere.

The items are not touched.
Deleting a collection removes the grouping, not its contents: every item in it stays in the library, keeps any other collection it was filed in, and turns up in `zot unfiled` if that was its only one.

The delete cascades.
Deleting a collection deletes every subcollection beneath it, in one request, because api.zotero.org removes the descendants itself.
That is the case worth being careful about, and it is why the command prints the whole subtree before asking.

Before writing anything, it reports the collection's name and key, every descendant collection that goes with it, how many items are filed in that subtree, and how many of those would end up in no collection at all.
Then it asks for confirmation on stderr, so `--json` stdout stays a single document:

- a collection with no subcollections takes `yes`;
- a collection with subcollections takes its own name, typed out, since the danger there is not knowing what hangs below the name you passed;
- anything else aborts without writing.

`--force` skips the prompt.
Without it, a non-interactive stdin (a pipe, a cron job, an agent) is refused rather than prompted or silently allowed, and the refusal names what would have been removed.

The summary is built from the local library while the cascade happens on api.zotero.org.
Before asking anything, the command therefore reads every collection in the subtree from the server and compares its `meta.numCollections` with the children it is about to list.
A subcollection created elsewhere and not yet synced down makes those counts disagree: the command refuses and tells you to sync Zotero, instead of prompting about a subtree it cannot show in full.
A subtree of more than 50 collections is refused too, since checking it would mean 50 requests before the prompt; delete it in parts, or pass `--force` once you have checked it in the Zotero UI.
The DELETE itself carries the collection's version, which rejects a concurrent write landing between that version being read and the delete going out.
The command then waits up to 20 seconds for the removal to sync down and reports whether it arrived (`synced_local` under `--json`); until it does, `zot collections` still lists the tree even though it is gone upstream.

`--rm` is rejected together with `--create`, `--parent`, the listing positional, `--flat` and `--tree-ids`.

## Tag hygiene (`zot tags`)

```bash
zot tags                                     # everything, with use counts
zot tags --automatic                         # only what Zotero imported itself
zot tags --unused                            # tags on zero items
zot tags --check                             # what breaks the vocabulary
zot tags --rename "cs.AI" "method:llm"       # merge or normalise one tag
zot tags --purge-automatic                   # delete every automatic tag
```

Zotero writes two kinds of tag. A manual one is typed by a person; an automatic one is a publisher or arXiv subject heading Zotero attaches on import.
The flag lives on the item-tag pair, not on the name, so the same name can be automatic on one item and manual on another; `zot tags` lists such a name twice, once per type.
The automatic set is self-duplicating (`Artificial Intelligence (cs.AI)` next to `Computer Science - Artificial Intelligence`) and describes topics a collection tree models better, which is what `--automatic` and `--purge-automatic` exist to clear out.

`--contains` filters the listing and `--check`. It is rejected with `--purge-automatic` and `--rename`, which act on the whole library and would otherwise ignore it.

### The controlled vocabulary (`--check`)

`--check` judges every tag against the vocabulary and reports what fails and why:

| Class | Meaning |
|---|---|
| `automatic` | Imported by Zotero, never typed; goes with `--purge-automatic`. If the same name is also typed by hand elsewhere, the entry says so, because a purge cannot remove it without taking the manual assignments too. |
| `field-redundant` | Only restates the item type or an identifier field, like a `standard` tag on `itemType: standard`, or `ISO 12006` where `number` already reads `DIN EN ISO 12006-2:2020-07`. Every item carrying the tag has to restate it, so a tag that says something real elsewhere is left alone. |
| `unknown-facet-value` | Known prefix, unknown value: `method:transformers` where `method:` is closed. |
| `out-of-vocabulary` | Neither an allowed bare tag nor a value of any facet. |
| `unused` | In the vocabulary, but on no items. |

A second pass judges the items rather than the names, because cardinality is a property of an item's whole tag set:

| Class | Meaning |
|---|---|
| `facet-conflict` | The item carries more tags of one facet than it allows, like `to-read` next to `reading`. Listed one item at a time, with keys. |
| missing | The item carries none of a facet that requires exactly one. Reported as a count with a few example keys, since on a library mid-migration that is most of them. |

Only top-level regular items are judged; child notes and attachments carry tags of their own and are not filed by hand.

The vocabulary lives in the config (`~/.config/zot/config.json` on Linux), so the CLI and any filing convention read one source:

```json
{
  "vocabulary": {
    "allowed_tags": [],
    "facets": [
      { "name": "state", "values": ["to-read", "reading", "read"], "cardinality": "exactly-one" },
      { "name": "flag", "values": ["urgent", "graded"] },
      { "name": "kind", "prefix": "kind:", "values": ["survey", "benchmark", "dataset", "position", "thesis"], "cardinality": "at-most-one" },
      { "name": "domain", "prefix": "domain:", "values": ["aec", "bim", "construction-mgmt", "facility-mgmt"] },
      { "name": "method", "prefix": "method:", "values": ["llm", "vlm", "agent", "rag", "kg", "cv"] },
      { "name": "concept", "prefix": "concept:", "values": [] },
      { "name": "course", "prefix": "course:", "values": [], "cardinality": "at-most-one" }
    ]
  }
}
```

A facet with a `prefix` matches tags carrying it; one without is bare, and its `values` are whole tags (`to-read`, `urgent`).
A prefixed facet with empty `values` is open: any non-empty value passes, which is what a growing `concept:` list needs.
A facet with values is closed, and anything else under that prefix is a typo or a term nobody agreed on.
`cardinality` is `any` (the default), `at-most-one` or `exactly-one`, and is what the per-item pass enforces.
`allowed_tags` is the escape hatch: bare tags allowed outside every facet, with no cardinality rule.
That block is also the built-in default, so `--check` is useful before the config has a `vocabulary` key at all; the output says which of the two it used.

### Purging and renaming

Both write through api.zotero.org and both ask first, printing the blast radius on stderr so `--json` stdout stays one document.

`--purge-automatic` names how many tags and how many item uses go, lists the ten most used, and takes the tag count typed out as confirmation (`yes` below ten tags).
Deletion is batched 50 tags per request, the web API maximum, and each batch is guarded with the library version it was computed from.
The items themselves are never touched, only the tags come off them.
A name that is automatic on some items and manual on others is kept, listed in the summary with both counts: the API deletes a tag by name from every item at once, so purging it would destroy hand-made assignments.
Remove those one item at a time with `zot edit <key> --rm-tag "<tag>"`.
On completion it says the part that decides whether the cleanup lasts: Zotero re-imports automatic tags on every new item unless *Settings → General → "Automatically tag items with keywords and subject headings"* is unchecked (`extensions.zotero.automaticTags`).
Without that, the count is back within a month.

`--rename OLD NEW` rewrites the tag on every item carrying it, then deletes the old tag library-wide.
When `NEW` already exists the two merge, which cannot be undone (nothing records which item carried which), so a merge always takes the old tag typed out; a plain rename under ten items takes `yes`.
The renamed tag is manual even when the old one was automatic, which is what stops Zotero treating it as its own again.
Each item is re-read from the server immediately before its write, because a tag PATCH replaces the whole array, and a concurrent edit stops the run with a message rather than overwriting it.

The item list is built from the local library while the closing sweep is a library-wide delete on the server, so before asking anything the command compares its list with the number of items api.zotero.org reports for that tag.
A disagreement means an item tagged elsewhere and not yet synced down, which the sweep would strip rather than rename; the command refuses and tells you to sync, or to pass `--force` to rename the local items anyway.
The sweep itself runs only when the server agrees no item still carries the old tag; otherwise the tag is left in place and the output says how many stragglers stopped it.

`--force` skips the prompt for scripts.
Without it, a non-interactive stdin is refused rather than prompted, and the refusal names what would have been removed.

## Zotero API constraints behind collection writes

Established by probing a live Zotero 7 instance on 2026-09-16; re-probe before relying on them for new write work.

- The local API is read-only: `PATCH http://localhost:23119/api/users/0/items/<key>` returns 501, so there is no local write path.
- The debug bridge (`/debug-bridge/execute`) returns 404 unless an opt-in pref is set, so there is no local JavaScript escape hatch for triggering a sync or editing collections.
- The connector's `POST /connector/updateSession` retargets a save session to one collection (`L1` for My Library, `C<n>` as listed by `POST /connector/getSelectedCollection`) plus a tags string; it does not add membership, so it cannot file an item into several collections.
- Multi-collection membership therefore goes through the web API: `PATCH /users/<id>/items/<key>` with the complete `collections` array, which replaces the item's membership wholesale rather than merging.
- Web to local sync is prompt (a web patch showed up in the local library at the same version immediately), but local to web is not: an item just created through the desktop app does not exist on api.zotero.org until Zotero syncs, so any web patch right after a local add needs a bounded poll, not a single attempt.

## Where data lives

The local index is stored in the platform data directory, `~/Library/Application Support/zot/` on macOS and `~/.local/share/zot/` on Linux:

```
  ├── meta.json      # index metadata (model, sync state)
  ├── tantivy/       # BM25 full-text index
  └── vectors.bin    # embedding vectors
```

The web API key and the controlled tag vocabulary live in the platform config directory, the same directory on macOS and `~/.config/zot/config.json` on Linux.

To reset the index completely, delete the data directory or run `zot index --force`.

## Troubleshooting

- "Could not reach Zotero. Is it running?" means the Zotero desktop app is not open or the local API is disabled, and no web API key is configured to fall back on.
  Open Zotero and enable the setting under *Settings → Advanced*, or run `zot config set-key`.
- Search that returns nothing or looks stale usually means a stale index: run `zot index` to sync, or `zot index --force` for a clean rebuild.

## Open work

Open work (bugs and features) is tracked in Sylvain's self-hosted Plane, project Personal (`PERSO`), module Technical Leverage, with issue titles prefixed `zot:`.
Commit messages cite the Plane identifier, for example PERSO-196.

## License

MIT (see `Cargo.toml`).
