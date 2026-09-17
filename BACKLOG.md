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
