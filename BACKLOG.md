# Backlog

## `zot add` by PMCID and by plain URL

`zot add` supports DOI, arXiv, ISBN and PMID identifiers today. A PMCID
(`PMC3531190`) is not recognised; NCBI's `idconv` API maps PMC to PMID, which
would be one extra request on an unambiguous prefix. The remaining form is the
hardest: a plain URL needs Zotero's web translators. The connector's
`/connector/saveSnapshot` saves a webpage item without translation; real
translator-based saving expects the connector to run the translator
browser-side. A translation-server sidecar would cover this properly, at the
cost of a Docker runtime dependency on a tool that currently needs nothing but
Zotero itself.

Origin: 2026-07-27, write-support work; deferred per review of
PLAN-write-support.md.

## Collection filing -- the API constraints that shape all of it

Background for any further collection or write work. Established by probing a
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
