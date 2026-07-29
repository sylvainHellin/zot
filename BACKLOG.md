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
  the workflow -- creating a collection still needs the Zotero UI today.

Origin: 2026-06-05, exporting a 12-item reference set for the ECPPM 2026 paper;
had to curl the local API and post-process in Python.
