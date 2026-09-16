use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::str::FromStr;

use crate::api::{ExportFormat, FORMAT_KEY_BATCH, ZoteroClient, ZoteroItem};
use crate::collections::{build_tree, resolve_collection_ref};
use crate::commands::collections_cmd::connector_targets;
use crate::commands::index_cmd::strip_html;
use crate::output::{DroppedItemOutput, ExportOutput, format_output, truncate_display};

/// BibTeX fields stripped unless `--raw` is given.
///
/// All four leak the local library rather than describing the work: `file` is
/// an absolute path into the Zotero storage directory, `annote` carries the
/// item's child notes, and `abstract` and `keywords` bloat every entry with a
/// paragraph and the local tags. `note` is left alone because Zotero emits the
/// **Extra** field there, which routinely holds the arXiv ID or a version
/// number, and `urldate` because biblatex pairs it with `url` on `@online` and
/// `@misc` entries.
pub const DEFAULT_STRIP_FIELDS: [&str; 4] = ["file", "annote", "abstract", "keywords"];

pub struct ExportArgs<'a> {
    pub keys: &'a [String],
    pub collection: Option<&'a str>,
    pub format: &'a str,
    pub output: Option<&'a str>,
    pub raw: bool,
    pub json: bool,
}

/// Export items as a bibliography, rendered by Zotero itself.
pub fn run_export(args: ExportArgs) -> Result<()> {
    let format = ExportFormat::from_str(args.format)?;
    if args.keys.is_empty() && args.collection.is_none() {
        bail!("Nothing to export. Pass item keys, or --collection REF.");
    }

    let client = ZoteroClient::new()?;

    let (wanted, bodies, dropped) = match args.collection {
        Some(reference) => export_collection(&client, reference, format)?,
        None => export_keys(&client, args.keys, format)?,
    };

    let output = finish_export(wanted, &bodies, dropped, format, args.raw, args.output)?;
    println!("{}", format_output(&output, args.json));
    Ok(())
}

/// Render a set of item keys, for callers that already have them.
///
/// `zot search --export` uses this; there is no `--raw` or `--output` on that
/// path, so the bibliography always comes back stripped and in memory.
pub fn export_for_keys(keys: &[String], format: &str) -> Result<ExportOutput> {
    let format = ExportFormat::from_str(format)?;
    let client = ZoteroClient::new()?;
    let (wanted, bodies, dropped) = export_keys(&client, keys, format)?;
    finish_export(wanted, &bodies, dropped, format, false, None)
}

/// Merge the bodies, strip private BibTeX fields, warn about drops on stderr,
/// and write the file when `--output` asked for one.
fn finish_export(
    wanted: usize,
    bodies: &[String],
    mut dropped: Vec<DroppedItemOutput>,
    format: ExportFormat,
    raw: bool,
    output: Option<&str>,
) -> Result<ExportOutput> {
    let merged = merge_bodies(bodies, format)?;
    // Only BibTeX carries the private fields, so `--raw` is a no-op for RIS and
    // CSL JSON, which are always passed through as Zotero rendered them.
    let content = if raw || format != ExportFormat::Bibtex {
        merged
    } else {
        strip_bibtex_fields(&merged, &DEFAULT_STRIP_FIELDS)
    };
    let exported = count_entries(&content, format)?;

    // Warnings go to stderr whatever `--json` and `--output` are doing, so
    // stdout stays exactly one document and the file stays exactly the
    // bibliography.
    dropped.sort_by(|a, b| a.title.cmp(&b.title));
    for item in &dropped {
        eprintln!(
            "Warning: no {} entry for \"{}\" ({}) -- {}.",
            format.as_param(),
            item.title,
            item.key,
            item.reason,
        );
    }

    let path = match output {
        Some(path) => {
            let mut body = content.clone();
            if !body.ends_with('\n') {
                body.push('\n');
            }
            std::fs::write(path, body).context(format!("Failed to write {path}"))?;
            Some(path.to_string())
        }
        None => None,
    };

    Ok(ExportOutput {
        format: format.as_param().to_string(),
        requested: wanted,
        exported,
        dropped,
        // Under `--output` the file is the payload; repeating it in the JSON
        // document would double a large export for no gain.
        content: if path.is_some() { None } else { Some(content) },
        path,
    })
}

/// Render an explicit list of item keys.
///
/// Returns the number of items asked for, the rendered bodies, and whatever
/// produced no entry.
fn export_keys(
    client: &ZoteroClient,
    keys: &[String],
    format: ExportFormat,
) -> Result<(usize, Vec<String>, Vec<DroppedItemOutput>)> {
    // Fetched for the titles: a rendered entry carries a citation key
    // (`jumperHighlyAccurateProtein2021`) with no relation to the item key, so
    // a warning can only name the item from the JSON metadata.
    let items = client.fetch_items(keys)?;
    let titles = title_map(&items);

    let mut dropped = Vec::new();
    let mut renderable = Vec::new();
    for key in keys {
        match titles.get(key.as_str()) {
            Some(_) => renderable.push(key.clone()),
            None => dropped.push(DroppedItemOutput {
                key: key.clone(),
                title: "(unknown item)".to_string(),
                reason: "no such item in the library".to_string(),
            }),
        }
    }

    let (bodies, missing) = render_batched(client, &renderable, format)?;
    for key in missing {
        dropped.push(DroppedItemOutput {
            title: titles.get(key.as_str()).cloned().unwrap_or_default(),
            key,
            reason: "the Zotero translator produced no entry for it".to_string(),
        });
    }
    Ok((keys.len(), bodies, dropped))
}

/// Render every direct member of a collection.
fn export_collection(
    client: &ZoteroClient,
    reference: &str,
    format: ExportFormat,
) -> Result<(usize, Vec<String>, Vec<DroppedItemOutput>)> {
    let nodes = build_tree(&client.fetch_collections()?);
    let resolved = resolve_collection_ref(reference, &nodes, &connector_targets(false))?;
    let Some(collection_key) = resolved.key else {
        let id = resolved.connector_id.as_deref().unwrap_or(reference);
        if id.starts_with('L') {
            bail!(
                "{id} is the library root, not a collection.\n  \
                 Pass item keys, or a collection reference."
            );
        }
        bail!(
            "Collection \"{}\" ({id}) is only known to the connector, most likely because it \
             lives in a group library, which this command cannot export.",
            resolved.name,
        );
    };

    let items = client.fetch_collection_top_items(&collection_key)?;
    let titles = title_map(&items);
    let keys: Vec<String> = items.iter().map(|i| i.key.clone()).collect();

    let bodies = client.fetch_collection_items_formatted(&collection_key, format)?;
    let rendered: usize = bodies
        .iter()
        .map(|b| count_entries(b, format))
        .sum::<Result<usize>>()?;

    // Attribution is only possible per request, so the expensive per-key probe
    // runs only once the collection is known to be short an entry.
    let mut dropped = Vec::new();
    if came_back_short(rendered, keys.len()) {
        for key in probe_missing(client, &keys, format)? {
            dropped.push(DroppedItemOutput {
                title: titles.get(key.as_str()).cloned().unwrap_or_default(),
                key,
                reason: "the Zotero translator produced no entry for it".to_string(),
            });
        }
    }
    Ok((keys.len(), bodies, dropped))
}

/// Render keys in batches, returning the bodies and the keys that rendered
/// nothing.
///
/// Entries are not attributable to item keys, so a batch that comes back short
/// is re-requested one key at a time. Batches are the common case and cost one
/// request each; the per-key fallback only pays for itself when something was
/// actually dropped.
fn render_batched(
    client: &ZoteroClient,
    keys: &[String],
    format: ExportFormat,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut bodies = Vec::new();
    let mut missing = Vec::new();
    for chunk in keys.chunks(FORMAT_KEY_BATCH) {
        let body = client.fetch_items_formatted(chunk, format)?;
        if came_back_short(count_entries(&body, format)?, chunk.len()) {
            missing.extend(probe_missing(client, chunk, format)?);
        }
        if !body.trim().is_empty() {
            bodies.push(body);
        }
    }
    Ok((bodies, missing))
}

/// Whether a rendered body is short of the keys that were asked for, and so
/// worth the per-key probe.
///
/// The translator can only ever produce fewer entries than items requested, so
/// a body that is not short is taken as complete and costs no extra request.
fn came_back_short(rendered: usize, requested: usize) -> bool {
    rendered < requested
}

/// Re-request each key alone to find which ones the translator refuses.
fn probe_missing(
    client: &ZoteroClient,
    keys: &[String],
    format: ExportFormat,
) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    for key in keys {
        let body = client.fetch_items_formatted(std::slice::from_ref(key), format)?;
        if count_entries(&body, format)? == 0 {
            missing.push(key.clone());
        }
    }
    Ok(missing)
}

/// Display names for the warning about dropped items.
///
/// A standalone note has no title, and it is exactly the kind of item the
/// translator refuses, so it falls back to its own first line rather than to a
/// bare item type that tells the reader nothing about which note it was. The
/// body is HTML, so it is stripped to text first; otherwise the reader sees
/// `<div data-schema-version=` where the identifying words should be.
fn title_map(items: &[ZoteroItem]) -> HashMap<&str, String> {
    items
        .iter()
        .map(|i| {
            let title = if !i.data.title.is_empty() {
                i.data.title.clone()
            } else if !i.data.note.is_empty() {
                // Line by line, so the heading a note usually opens with is
                // what names it, rather than the whole body run together.
                let text = i
                    .data
                    .note
                    .lines()
                    .map(strip_html)
                    .find(|l| !l.is_empty())
                    .unwrap_or_default();
                let first = truncate_display(&text, 60);
                // `note note: ...` reads as a stutter, so an item that is
                // already a note just says `note:`.
                if i.data.item_type == "note" {
                    format!("note: {first}")
                } else {
                    format!("{} note: {first}", i.data.item_type)
                }
            } else {
                format!("untitled {}", i.data.item_type)
            };
            (i.key.as_str(), title)
        })
        .collect()
}

/// Join per-batch bodies into one document.
///
/// BibTeX and RIS concatenate, but two CSL JSON bodies are two JSON arrays and
/// concatenating them is not valid JSON, so CSL JSON is parsed and re-emitted.
/// That re-serialization is why `--format csljson` always comes out
/// pretty-printed rather than passed through byte for byte.
pub fn merge_bodies(bodies: &[String], format: ExportFormat) -> Result<String> {
    if format == ExportFormat::CslJson {
        let mut all: Vec<serde_json::Value> = Vec::new();
        for body in bodies {
            if body.trim().is_empty() {
                continue;
            }
            let page: Vec<serde_json::Value> =
                serde_json::from_str(body).context("Failed to parse CSL JSON from Zotero")?;
            all.extend(page);
        }
        return serde_json::to_string_pretty(&all).context("Failed to serialize CSL JSON");
    }
    let parts: Vec<&str> = bodies
        .iter()
        .map(|b| b.trim_matches('\n'))
        .filter(|b| !b.is_empty())
        .collect();
    Ok(parts.join("\n\n"))
}

/// Count the bibliography entries in a rendered body.
pub fn count_entries(body: &str, format: ExportFormat) -> Result<usize> {
    match format {
        ExportFormat::Bibtex => Ok(count_bibtex_entries(body)),
        // Every RIS record opens with a type tag; tags are only recognized at
        // the start of a line, so a stray `TY  - ` inside a wrapped value would
        // have broken the record for Zotero too.
        ExportFormat::Ris => Ok(body.lines().filter(|l| l.starts_with("TY  - ")).count()),
        ExportFormat::CslJson => {
            if body.trim().is_empty() {
                return Ok(0);
            }
            let items: Vec<serde_json::Value> =
                serde_json::from_str(body).context("Failed to parse CSL JSON from Zotero")?;
            Ok(items.len())
        }
    }
}

/// Count `@type{...}` entries, ignoring any `@` that falls inside a value.
fn count_bibtex_entries(body: &str) -> usize {
    let mut depth: i32 = 0;
    let mut count = 0;
    for line in body.lines() {
        if depth <= 0 && line.starts_with('@') {
            count += 1;
        }
        depth += brace_delta(line);
    }
    count
}

/// Remove whole fields from a BibTeX stream.
///
/// Works on Zotero's emitted shape: `@type{citekey,` on its own line, then one
/// `\tfield = {value},` per line, then `}`. Values are never wrapped but do
/// nest braces (`title = {...{AlphaFold}}`), so the cut is made on brace depth
/// rather than at the first `}`, and a value spanning several lines is dropped
/// whole. Field names match in full, so `annote` never takes `note` with it.
pub fn strip_bibtex_fields(bibtex: &str, fields: &[&str]) -> String {
    let mut out = String::with_capacity(bibtex.len());
    let mut depth: i32 = 0;
    // Entry depth to drop back to, while discarding a multi-line field value.
    let mut dropping_to: Option<i32> = None;

    for line in bibtex.split_inclusive('\n') {
        let delta = brace_delta(line);
        if let Some(target) = dropping_to {
            depth += delta;
            if depth <= target {
                dropping_to = None;
            }
            continue;
        }
        // Fields sit at depth 1, directly inside the entry braces. Anything
        // deeper is a continuation line of a value that happens to contain an
        // `=`, and must not be mistaken for a field of its own.
        if depth == 1 {
            if let Some(name) = field_name(line) {
                if fields.iter().any(|f| f.eq_ignore_ascii_case(name)) {
                    let entry_depth = depth;
                    depth += delta;
                    if depth > entry_depth {
                        dropping_to = Some(entry_depth);
                    }
                    continue;
                }
            }
        }
        out.push_str(line);
        depth += delta;
    }
    out
}

/// Net brace depth change across a line, honouring TeX's `\{` and `\}`.
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

/// The field name of a `field = {value},` line, or `None` when the line is not
/// a field assignment.
fn field_name(line: &str) -> Option<&str> {
    let (name, _) = line.trim_start().split_once('=')?;
    let name = name.trim_end();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const STRIP: [&str; 4] = DEFAULT_STRIP_FIELDS;

    /// `itemType`, `title` and `note` are the three fields `title_map` reads;
    /// the rest of the item is whatever `ZoteroItemData` defaults to.
    fn item(key: &str, item_type: &str, title: &str, note: &str) -> ZoteroItem {
        serde_json::from_value(json!({
            "key": key,
            "version": 1,
            "data": {
                "key": key,
                "version": 1,
                "itemType": item_type,
                "title": title,
                "note": note,
            },
        }))
        .expect("item fixture")
    }

    /// Two entries in Zotero's exact emitted shape: tab indent, `= {`, trailing
    /// comma on every field line, `}` alone, blank line between entries.
    fn sample() -> String {
        "\n@article{jumperHighlyAccurateProtein2021,\n\
         \ttitle = {Highly accurate prediction with {AlphaFold}},\n\
         \tabstract = {Proteins are essential. Nested {braces} here too.},\n\
         \turl = {http://dx.doi.org/10.1038/s41586-021-03819-2},\n\
         \turldate = {2024-03-01},\n\
         \tkeywords = {biology,structure},\n\
         \tfile = {/home/u/Zotero/storage/ABCD/paper.pdf},\n\
         \tyear = {2021},\n\
         }\n\
         \n\
         @inproceedings{zhaoDETRsBeatYOLOs2024,\n\
         \ttitle = {DETRs Beat {YOLOs}},\n\
         \tnote = {arXiv:2305.17926 [cs]},\n\
         \tannote = {another one},\n\
         \tpages = {16965--16974},\n\
         }\n"
            .to_string()
    }

    #[test]
    fn strips_every_default_field() {
        let out = strip_bibtex_fields(&sample(), &STRIP);
        for field in STRIP {
            assert!(
                !out.contains(&format!("\t{field} = ")),
                "{field} survived stripping:\n{out}"
            );
        }
        // And keeps everything else.
        assert!(out.contains("\ttitle = {Highly accurate prediction with {AlphaFold}},"));
        assert!(out.contains("\turl = {http://dx.doi.org/10.1038/s41586-021-03819-2},"));
        assert!(out.contains("\tyear = {2021},"));
        assert!(out.contains("\tpages = {16965--16974},"));
    }

    /// `note` is Zotero's Extra field (arXiv IDs, version numbers) and
    /// `urldate` is the companion `url` carries on `@online`, so neither is
    /// private to the library and neither may be stripped by default.
    #[test]
    fn the_default_set_keeps_note_and_urldate() {
        assert_eq!(STRIP, ["file", "annote", "abstract", "keywords"]);
        let out = strip_bibtex_fields(&sample(), &STRIP);
        assert!(out.contains("\turldate = {2024-03-01},"), "{out}");
        assert!(out.contains("\tnote = {arXiv:2305.17926 [cs]},"), "{out}");
    }

    #[test]
    fn keeps_entry_structure_intact() {
        let out = strip_bibtex_fields(&sample(), &STRIP);
        assert_eq!(count_bibtex_entries(&out), 2);
        assert!(out.contains("@article{jumperHighlyAccurateProtein2021,"));
        assert!(out.contains("@inproceedings{zhaoDETRsBeatYOLOs2024,"));
        assert_eq!(out.matches("\n}\n").count(), 2);
    }

    #[test]
    fn nested_braces_do_not_cut_the_value_short() {
        let input =
            "@article{k,\n\tabstract = {A {nested} {b{r}ace} pile},\n\ttitle = {Kept},\n}\n";
        let out = strip_bibtex_fields(input, &STRIP);
        assert_eq!(out, "@article{k,\n\ttitle = {Kept},\n}\n");
    }

    #[test]
    fn multi_line_value_is_dropped_whole() {
        let input = "@article{k,\n\
                     \tabstract = {line one\n\
                     line two {nested}\n\
                     line three},\n\
                     \ttitle = {Kept},\n\
                     }\n";
        let out = strip_bibtex_fields(input, &STRIP);
        assert_eq!(out, "@article{k,\n\ttitle = {Kept},\n}\n");
    }

    #[test]
    fn multi_line_value_that_is_kept_survives_whole() {
        let input = "@article{k,\n\
                     \ttitle = {line one\n\
                     line two},\n\
                     \tannote = {gone},\n\
                     }\n";
        let out = strip_bibtex_fields(input, &STRIP);
        assert_eq!(out, "@article{k,\n\ttitle = {line one\nline two},\n}\n");
    }

    #[test]
    fn a_field_name_that_is_a_prefix_of_another_is_not_stripped() {
        let input = "@article{k,\n\tnote = {kept},\n\tannote = {gone},\n\tfilename = {kept},\n\
                     \tfile = {gone},\n}\n";
        let out = strip_bibtex_fields(input, &STRIP);
        assert_eq!(out, "@article{k,\n\tnote = {kept},\n\tfilename = {kept},\n}\n");
    }

    #[test]
    fn an_entry_with_nothing_to_strip_is_returned_byte_for_byte() {
        let input = "@book{k,\n\ttitle = {Nothing private},\n\tyear = {1999},\n}\n";
        assert_eq!(strip_bibtex_fields(input, &STRIP), input);
    }

    #[test]
    fn an_entry_whose_fields_are_all_stripped_stays_valid() {
        let input = "@misc{k,\n\tannote = {a},\n\tfile = {b},\n}\n";
        assert_eq!(strip_bibtex_fields(input, &STRIP), "@misc{k,\n}\n");
    }

    #[test]
    fn raw_passthrough_strips_nothing() {
        let input = sample();
        assert_eq!(strip_bibtex_fields(&input, &[]), input);
    }

    #[test]
    fn a_value_containing_an_at_sign_line_is_not_a_new_entry() {
        let input = "@article{k,\n\ttitle = {Mail\n@example.com\nend},\n}\n";
        assert_eq!(count_bibtex_entries(input), 1);
    }

    #[test]
    fn brace_delta_ignores_escaped_braces() {
        assert_eq!(brace_delta("\ttitle = {a \\{ b},"), 0);
        assert_eq!(brace_delta("\ttitle = {a \\} b},"), 0);
        assert_eq!(brace_delta("\ttitle = {open"), 1);
    }

    #[test]
    fn field_name_rejects_non_assignments() {
        assert_eq!(field_name("\ttitle = {x},"), Some("title"));
        assert_eq!(field_name("\tmonth = jul,"), Some("month"));
        assert_eq!(field_name("@article{key,"), None);
        assert_eq!(field_name("}"), None);
        assert_eq!(field_name("some prose = with spaces"), None);
    }

    #[test]
    fn counts_entries_per_format() {
        assert_eq!(count_entries(&sample(), ExportFormat::Bibtex).unwrap(), 2);
        let ris = "TY  - JOUR\nTI  - One\nER  - \n\nTY  - CONF\nTI  - Two\nER  - \n";
        assert_eq!(count_entries(ris, ExportFormat::Ris).unwrap(), 2);
        let csl = "[{\"id\":\"a\"},{\"id\":\"b\"},{\"id\":\"c\"}]";
        assert_eq!(count_entries(csl, ExportFormat::CslJson).unwrap(), 3);
    }

    #[test]
    fn counts_nothing_in_an_empty_body() {
        for format in [ExportFormat::Bibtex, ExportFormat::Ris, ExportFormat::CslJson] {
            assert_eq!(count_entries("", format).unwrap(), 0);
        }
    }

    #[test]
    fn merges_bibtex_with_one_blank_line_between_entries() {
        let bodies = vec![
            "\n@article{a,\n\ttitle = {A},\n}\n".to_string(),
            "\n@article{b,\n\ttitle = {B},\n}\n".to_string(),
        ];
        let merged = merge_bodies(&bodies, ExportFormat::Bibtex).unwrap();
        assert_eq!(
            merged,
            "@article{a,\n\ttitle = {A},\n}\n\n@article{b,\n\ttitle = {B},\n}"
        );
        assert_eq!(count_entries(&merged, ExportFormat::Bibtex).unwrap(), 2);
    }

    #[test]
    fn merges_csljson_into_a_single_array() {
        let bodies = vec![
            "[{\"id\":\"a\"}]".to_string(),
            "[{\"id\":\"b\"},{\"id\":\"c\"}]".to_string(),
        ];
        let merged = merge_bodies(&bodies, ExportFormat::CslJson).unwrap();
        assert_eq!(count_entries(&merged, ExportFormat::CslJson).unwrap(), 3);
        assert!(merged.starts_with('['));
        assert!(merged.ends_with(']'));
        // The README promises two-space pretty-printing, so plain
        // `to_string` must not pass this.
        assert_eq!(
            merged,
            "[\n  {\n    \"id\": \"a\"\n  },\n  {\n    \"id\": \"b\"\n  },\n  \
             {\n    \"id\": \"c\"\n  }\n]"
        );
    }

    #[test]
    fn a_titled_item_is_named_by_its_title() {
        let items = [item("AAAA1111", "journalArticle", "Highly accurate", "")];
        let map = title_map(&items);
        assert_eq!(map.get("AAAA1111").unwrap(), "Highly accurate");
    }

    /// Regression: this used to read `note note: <div data-schema-version=`,
    /// doubling the word and showing markup instead of the heading.
    #[test]
    fn a_standalone_note_is_named_by_its_first_line_of_text() {
        let body = "<div data-schema-version=\"9\"><h1>Curriculum</h1>\n\
                    <p></p>\n<ol>\n<li>\nManning -- textbook\n</li>\n</ol>\n</div>";
        let items = [item("VETJB3WE", "note", "", body)];
        let map = title_map(&items);
        assert_eq!(map.get("VETJB3WE").unwrap(), "note: Curriculum");
    }

    #[test]
    fn a_note_on_a_non_note_item_still_names_its_type() {
        let items = [item("BBBB2222", "attachment", "", "<p>Scan of page 3</p>")];
        let map = title_map(&items);
        assert_eq!(map.get("BBBB2222").unwrap(), "attachment note: Scan of page 3");
    }

    #[test]
    fn a_note_whose_opening_lines_are_only_markup_skips_them() {
        let items = [item("CCCC3333", "note", "", "<div>\n<h1>\nMeeting minutes\n</h1>\n</div>")];
        let map = title_map(&items);
        assert_eq!(map.get("CCCC3333").unwrap(), "note: Meeting minutes");
    }

    #[test]
    fn a_long_note_first_line_is_truncated_for_display() {
        let long = "x".repeat(120);
        let items = [item("DDDD4444", "note", "", &format!("<p>{long}</p>"))];
        let map = title_map(&items);
        let title = map.get("DDDD4444").unwrap();
        assert_eq!(title, &format!("note: {}...", "x".repeat(60)));
    }

    #[test]
    fn an_item_with_neither_title_nor_note_falls_back_to_its_type() {
        let items = [item("EEEE5555", "attachment", "", "")];
        let map = title_map(&items);
        assert_eq!(map.get("EEEE5555").unwrap(), "untitled attachment");
    }

    #[test]
    fn raw_keeps_the_private_fields_in_the_bibliography() {
        let out =
            finish_export(2, &[sample()], Vec::new(), ExportFormat::Bibtex, true, None).unwrap();
        let content = out.content.as_deref().unwrap();
        assert!(content.contains("\tfile = {/home/u/Zotero/storage/ABCD/paper.pdf},"));
        assert!(content.contains("\tannote = {another one},"));
        assert_eq!(out.exported, 2);
        assert_eq!(out.requested, 2);
        assert!(out.path.is_none());
    }

    /// `--raw` is a no-op for RIS and CSL JSON, which never carry the private
    /// fields, so both go through the same untouched branch.
    #[test]
    fn a_non_bibtex_format_is_passed_through_even_without_raw() {
        let ris = "TY  - JOUR\nTI  - One\nN1  - private thought\nER  - \n".to_string();
        let bodies = std::slice::from_ref(&ris);
        let out = finish_export(1, bodies, Vec::new(), ExportFormat::Ris, false, None).unwrap();
        assert_eq!(out.content.as_deref().unwrap(), ris.trim_matches('\n'));
        assert_eq!(out.format, "ris");
        assert_eq!(out.exported, 1);
    }

    #[test]
    fn without_raw_bibtex_is_stripped() {
        let out =
            finish_export(2, &[sample()], Vec::new(), ExportFormat::Bibtex, false, None).unwrap();
        let content = out.content.as_deref().unwrap();
        assert!(!content.contains("\tfile = "));
        assert!(!content.contains("\tannote = "));
        assert!(content.contains("\tnote = {arXiv:2305.17926 [cs]},"));
    }

    #[test]
    fn a_short_body_asks_for_the_per_key_probe() {
        assert!(came_back_short(11, 12));
        assert!(came_back_short(0, 1));
    }

    #[test]
    fn a_complete_body_costs_no_extra_request() {
        assert!(!came_back_short(12, 12));
        assert!(!came_back_short(0, 0));
        // The translator cannot invent entries, but a surplus must not probe.
        assert!(!came_back_short(13, 12));
    }

    #[test]
    fn parses_format_names_and_their_aliases() {
        for (input, expected) in [
            ("bibtex", ExportFormat::Bibtex),
            ("BibTeX", ExportFormat::Bibtex),
            ("bib", ExportFormat::Bibtex),
            (" ris ", ExportFormat::Ris),
            ("csljson", ExportFormat::CslJson),
            ("csl-json", ExportFormat::CslJson),
        ] {
            assert_eq!(ExportFormat::from_str(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn rejects_an_unknown_format_by_name() {
        let err = ExportFormat::from_str("endnote").unwrap_err().to_string();
        assert!(err.contains("endnote"), "{err}");
        assert!(err.contains("bibtex, ris, csljson"), "{err}");
    }

    #[test]
    fn format_query_params_are_what_the_local_api_expects() {
        assert_eq!(ExportFormat::Bibtex.as_param(), "bibtex");
        assert_eq!(ExportFormat::Ris.as_param(), "ris");
        assert_eq!(ExportFormat::CslJson.as_param(), "csljson");
    }
}
