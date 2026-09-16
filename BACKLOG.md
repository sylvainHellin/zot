# Backlog

## More `zot add` identifier types -- ISBN, PubMed, plain URL

`zot add` supports DOI and arXiv today. Worth adding later:

- **ISBN**: no content-negotiation service; OpenLibrary
  (`https://openlibrary.org/isbn/<isbn>.json`) or Google Books, mapped to
  BibTeX before `/connector/import`.
- **PubMed ID (PMID/PMCID)**: NCBI E-utilities (`efetch` with
  `rettype=medline`) or the idconv API to get a DOI, then the existing DOI
  path.
- **Plain URL**: hardest -- needs Zotero's web translators. The connector's
  `/connector/saveSnapshot` saves a webpage item without translation; real
  translator-based saving expects the connector to run the translator
  browser-side. A translation-server sidecar would cover this properly.

Origin: 2026-07-27, write-support work; deferred per review of
PLAN-write-support.md.

## zot export -- export items to BibTeX / RIS / CSL-JSON

Add an `export` subcommand that serializes items to a bibliography format, so a
set of keys (or a search result, or a collection) can be handed to a reference
manager in one step. Today this requires hitting the Zotero local API by hand
(`/api/users/0/items?itemKey=...&format=bibtex`), which is undiscoverable and
needs post-processing to strip private notes.

Proposed shape:

```
zot export KEY [KEY ...]                 # explicit keys
zot export --collection COLLKEY          # a whole collection
zot search "query" --export bibtex       # pipe a search result out
zot export KEY --format bibtex|ris|csljson   # default: bibtex
zot export KEY --output refs.bib             # default: stdout
```

Implementation notes:
- The Zotero local API already renders these formats: append `&format=bibtex`
  (also `ris`, `csljson`) to the `items` endpoint used in
  `src/api/client.rs::fetch_items` (`DEFAULT_BASE_URL` =
  `http://localhost:23119/api/users/0`). No new translator needed for the
  common cases.
- BibTeX comes back with private fields that should be stripped by default
  (`file`, `annote`, `abstract`, `keywords`); add `--raw` to keep everything.
- The translator silently drops some items (observed: an arXiv `conferencePaper`
  returned nothing). Detect missing keys by diffing requested vs returned and
  warn, or fall back to building a minimal entry from the item metadata.
- Related: a `zot collections` discovery command (list/create) would round out
  the workflow -- creating a collection still needs the Zotero UI today. Now
  specified below under "zot collections".

Origin: 2026-06-05, exporting a 12-item reference set for the ECPPM 2026 paper;
had to curl the local API and post-process in Python.

## Collection filing -- the API constraints that shape all of it

Shared background for the four entries that follow. Established by probing a
live Zotero 7 instance on 2026-09-16; re-probe before trusting it.

- **The local API is read-only.** `PATCH
  http://localhost:23119/api/users/0/items/<key>` returns **501**. Reads are
  fine, writes are not. There is no local write path.
- **The debug bridge is not available.** `GET /debug-bridge/execute?token=x`
  returns **404** (it needs an opt-in pref). So there is no local JS escape
  hatch for `Zotero.Sync.Runner.sync()` or direct collection mutation.
- **The connector can only express one collection.**
  `POST /connector/updateSession` takes a single `target` (`L1` = My Library,
  `C<n>` = a collection, as listed by `POST /connector/getSelectedCollection`)
  plus a comma-separated `tags` string. It *retargets* a save session, it does
  not add membership, so multi-collection filing is impossible through it.
- **Therefore multi-collection membership must go through the web API**:
  `PATCH /users/<id>/items/<key>` with a complete `collections` array. The array
  is replaced wholesale, never merged.
- **web -> local sync is prompt, local -> web is not.** Verified: a
  `zot edit 3ER9U8F7 --patch '{"collections":["KMHNIPDA"]}'` returned version
  16817 and the local library reported `Last-Modified-Version: 16817` with the
  new membership immediately. The reverse direction is the problem: an item
  just created by `zot add` does not exist on api.zotero.org until Zotero
  syncs up, which is the already-documented "not found on api.zotero.org"
  error. Any post-add web patch needs a bounded poll, not a single attempt.

## zot collections --create -- create a collection

`zot collections` reads the tree but cannot add to it, so a filing script that
wants a collection which does not exist yet has to stop and hand the job to the
Zotero UI.

```
zot collections --create NAME [--parent KEY]
```

Implementation notes:
- `POST /users/<id>/collections` on the web API, so it belongs with the other
  `WebApiClient` writes in `src/api/webapi.rs` and inherits the same sync
  caveat: the new collection reaches the local library and the connector only
  on the next Zotero sync.
- `--parent` should accept anything `resolve_collection_ref`
  (`src/collections.rs`) already takes: a key, an exact name, or a connector
  tree-view ID. No `--parent` means top level.
- Refuse a name that already exists under the same parent. Zotero allows the
  duplicate, and every later `zot collections NAME` or `--add-collection NAME`
  against it becomes ambiguous.

Origin: 2026-09-16, auditing why new items were landing in Zotero's unfiled
items (12 found).

## Collection filing on add and edit

Filing is currently easy to get wrong in two separate ways. Sylvain's rule is
that every added item belongs in at least one `2 Library` topic collection,
plus a `1 References` paper collection when it is being cited by a specific
manuscript. The CLI cannot express that in one command.

### 1. `zot add --collection` should be repeatable

Today it is `Option<&str>` (`AddArgs.collection`, `src/commands/add_cmd.rs:25`),
so two-collection filing takes two commands across two APIs:

```bash
zot add 10.xxxx/yyy --collection KMHNIPDA
zot edit KEY --patch '{"collections":["KMHNIPDA","ZSL8LTE2"]}'
```

Proposed: `--collection` repeatable, like `--tag` already is.

```
zot add 10.xxxx/yyy --collection KMHNIPDA --collection ZSL8LTE2
```

Implementation notes:
- `resolve_target()` (`src/commands/add_cmd.rs:242`) already accepts a key, an
  exact name, or a raw tree ID (`C42`) and errors helpfully on ambiguity.
  Extend it to a `Vec`, resolving each independently so a typo in the second
  collection fails before anything is written.
- Use the first resolved collection as the connector `target` (that path is
  unchanged and needs no key), then patch the full array via the web API for
  the rest. Per the constraints section, that patch needs a bounded poll for
  the item to appear upstream: retry `get_item` every ~2s up to ~60s, then warn
  with the exact `zot edit --add-collection` command to run by hand rather than
  failing silently.
- The item is already filed in collection one at that point, so a failed poll
  degrades to "partially filed", never to unfiled. Say so in the warning.

### 2. `zot add` should not silently default to the library root

With no `--collection`, `resolve_target` returns the library root and the item
becomes an unfiled item. Nothing in the output says so, which is how 12 items
accumulated there unnoticed.

Proposed: warn on stderr by default ("no --collection given; <title> is now an
unfiled item"), and add `--no-collection` as the explicit opt-out for the rare
standalone add. A hard error is the alternative, but it would break the
legitimate "add now, file in the Zotero UI later" flow.

Origin: 2026-09-16, same audit. The two-API dance and the silent root default
were both found while checking whether the skill's filing rule was enforceable.

## zot unfiled -- list items in no collection

No way to audit filing drift. Finding the 12 unfiled items required fetching
all 463 top-level items and filtering on an empty `data.collections` in Python.

```
zot unfiled                  # key, type, title for every unfiled top-level item
zot unfiled --count          # just the number, for a scripted health check
```

Implementation notes:
- Pure local read, no key needed, no index needed: page `/items/top` and keep
  items whose `data.collections` is empty. The same item fetch that
  `zot collections` needs for its counts, so the two share a helper.
- Filter attachments and notes out by default. The audit turned up a stray
  top-level `attachment` item (`SLJFJADB`, "norms") among real papers, which is
  a different kind of problem and deserves its own line in the output rather
  than being mixed in with unfiled papers.

Origin: 2026-09-16, same audit.

## zot tags -- vocabulary enforcement and bulk cleanup

`zot tags` lists tags but cannot judge or change them, so a controlled
vocabulary has no enforcement point and import noise accumulates unchecked.

State of the library on 2026-09-16: 61 manual tags (44 of them used exactly
once) and **113 automatic tags across 383 uses**, the latter entirely
arXiv/publisher subject headings. The automatic set is self-duplicating,
carrying both arXiv spellings of the same category ("Artificial Intelligence
(cs.AI)" 45 uses alongside "Computer Science - Artificial Intelligence" 21),
and it encodes a topic the collection tree already models better. 52 items
carry automatic tags and nothing else.

```
zot tags --automatic                 # list only type=1 tags, with use counts
zot tags --purge-automatic           # delete every type=1 tag, library-wide
zot tags --rename OLD NEW            # merge/normalise one tag everywhere
zot tags --check                     # flag tags outside the controlled vocabulary
zot tags --unused                    # tags on zero items
```

Implementation notes:
- Tag type is already on the wire: each entry in an item's `data.tags` is
  `{tag, type}` where `type: 1` means automatic and an absent `type` means
  manual. `zot tags` currently collapses both, which is why the noise is
  invisible.
- Deletion is a single web API call per batch, not per item:
  `DELETE /users/<id>/tags?tag=<url-encoded>||<tag2>||...` removes a tag from
  every item at once, up to 50 tags per request. Rename has no direct endpoint;
  it is add-new + delete-old across the affected items.
- `--check` needs the vocabulary to live somewhere. Put it in the config
  (`~/.config/zot/config.json`) as a list of allowed bare tags plus allowed
  facet prefixes, so the CLI and the agent skill read the same source rather
  than drifting apart.
- Purging automatic tags fixes the backlog but not the inflow. Zotero keeps
  adding them on import unless Settings -> General -> "Automatically tag items
  with keywords and subject headings" is unchecked. `--purge-automatic` should
  say so on completion, otherwise the count is back within a month.
- Several manual tags are redundant with a field rather than with the tree, and
  should be dropped rather than renamed: the `standard` tag matches
  `itemType == standard` exactly (11 items, both directions), and tags like
  `ISO 12006` / `DIN 1356` / `NCS` restate the `number` field that already
  reads `DIN EN ISO 12006-2:2020-07`. A `--check` rule for field-redundant tags
  would catch this class.

Origin: 2026-09-16, designing a controlled tag vocabulary; the crosscutting
axis (domain, genre, method) has to live in tags because a collection tree can
only express one axis, which only works if the vocabulary is enforced.
