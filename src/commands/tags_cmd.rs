//! `zot tags` -- list tags, judge them against the controlled vocabulary, and
//! clean them up in bulk.
//!
//! Listing and checking are local reads. `--purge-automatic` and `--rename`
//! write through api.zotero.org (the local API is read-only) and follow the
//! `zot collections --rm` pattern: blast radius first, typed confirmation
//! second, writes last.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};

use crate::api::ZoteroClient;
use crate::api::client::TagInfo;
use crate::api::models::ZoteroItem;
use crate::api::webapi::{TAG_DELETE_BATCH, VersionConflict, WebApiClient};
use crate::config::{Config, Vocabulary};
use crate::output::{
    TagCheckOutput, TagIssueOutput, TagOutput, TagRenamedOutput, TagsOutput, TagsPurgedOutput,
    format_output,
};

/// How many tags or items a confirmation summary lists before summarising the
/// rest.
const SAMPLE_SIZE: usize = 10;

/// Below this many objects a destructive tag command asks for `yes`; at or
/// above it, the user types something specific to the operation. The line is
/// where a list stops being readable in one glance, which is the point at
/// which "yes" stops meaning "I read it".
const SMALL_SCOPE: usize = 10;

/// Item fields a tag must not restate.
///
/// Identifier-ish fields only. A tag repeating a word from the title or the
/// abstract is ordinary; a tag repeating the standard number is the noise the
/// filing convention bans, because the field already carries it in full.
const REDUNDANT_FIELDS: [&str; 7] = [
    "number",
    "callNumber",
    "seriesNumber",
    "reportNumber",
    "status",
    "ISBN",
    "DOI",
];

/// Arguments of `zot tags`.
pub struct TagsArgs<'a> {
    pub contains: Option<&'a str>,
    pub automatic: bool,
    pub purge_automatic: bool,
    pub rename: Option<(&'a str, &'a str)>,
    pub check: bool,
    pub unused: bool,
    pub force: bool,
    pub json: bool,
}

/// List tags, or act on them.
///
/// Clap keeps the modes apart (they share an `ArgGroup`), so at most one of
/// `automatic`, `purge_automatic`, `rename`, `check` and `unused` is ever set;
/// none of them is the plain listing this command has always done.
pub fn run_tags(args: TagsArgs) -> Result<()> {
    let TagsArgs {
        contains,
        automatic,
        purge_automatic,
        rename,
        check,
        unused,
        force,
        json,
    } = args;

    if let Some((old, new)) = rename {
        return run_rename(old, new, force, json);
    }
    if purge_automatic {
        return run_purge_automatic(force, json);
    }

    let client = ZoteroClient::new()?;
    let mut tags = client.fetch_tags()?;

    // Filter by contains if specified
    if let Some(filter) = contains {
        let filter_lower = filter.to_lowercase();
        tags.retain(|t| t.tag.to_lowercase().contains(&filter_lower));
    }

    // Sort by tag name
    tags.sort_by_key(|t| t.tag.to_lowercase());

    if check {
        return run_check(&client, &tags, json);
    }

    let heading = if automatic {
        tags.retain(TagInfo::is_automatic);
        "Automatic tags"
    } else if unused {
        tags.retain(|t| t.meta.num_items == 0);
        "Unused tags"
    } else {
        "Tags"
    };

    println!("{}", format_output(&tags_output(&tags, heading), json));
    Ok(())
}

/// Turn a tag list into the output type, summing the uses as it goes.
fn tags_output(tags: &[TagInfo], heading: &str) -> TagsOutput {
    TagsOutput {
        count: tags.len(),
        uses: tags.iter().map(|t| t.meta.num_items).sum(),
        tags: tags
            .iter()
            .map(|t| TagOutput {
                tag: t.tag.clone(),
                num_items: t.meta.num_items,
                automatic: t.is_automatic(),
            })
            .collect(),
        heading: heading.to_string(),
        // The plain listing's header line predates the use total and is left
        // exactly as it was; the filtered listings are new and can say more.
        show_uses: heading != "Tags",
    }
}

// ---- --check ----

/// Flag every tag the controlled vocabulary does not allow.
fn run_check(client: &ZoteroClient, tags: &[TagInfo], json: bool) -> Result<()> {
    let cfg = Config::load()?;
    let vocab = cfg.vocabulary();
    // Field redundancy is a property of the items carrying the tag, so this
    // read is what turns "unknown tag" into "unknown tag that the `number`
    // field already says".
    let items = client.fetch_all_items()?;
    let by_tag = index_items_by_tag(&items);

    let issues: Vec<TagIssueOutput> = tags
        .iter()
        .filter_map(|t| {
            let redundancy = by_tag
                .get(t.tag.as_str())
                .and_then(|carriers| field_redundancy(carriers, &t.tag));
            let (issue, detail) = classify_tag(t, &vocab, redundancy.as_deref())?;
            Some(TagIssueOutput {
                tag: t.tag.clone(),
                num_items: t.meta.num_items,
                automatic: t.is_automatic(),
                issue: issue.to_string(),
                detail,
            })
        })
        .collect();

    let output = TagCheckOutput {
        checked: tags.len(),
        flagged: issues.len(),
        vocabulary_source: vocabulary_source(&cfg),
        issues,
    };
    println!("{}", format_output(&output, json));
    Ok(())
}

/// Where the vocabulary the check just applied came from.
fn vocabulary_source(cfg: &Config) -> String {
    if cfg.vocabulary_is_configured() {
        let path = Config::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "the config file".to_string());
        format!("vocabulary from {path}")
    } else {
        "built-in default vocabulary; add `vocabulary` to the config file to change it"
            .to_string()
    }
}

/// Which items carry which tag, by exact tag text (Zotero tags are
/// case-sensitive, so folding case here would merge two distinct tags).
fn index_items_by_tag(items: &[ZoteroItem]) -> HashMap<&str, Vec<&ZoteroItem>> {
    let mut map: HashMap<&str, Vec<&ZoteroItem>> = HashMap::new();
    for item in items {
        for tag in &item.data.tags {
            map.entry(tag.tag.as_str()).or_default().push(item);
        }
    }
    map
}

/// The verdict on one tag, as a rule class and the sentence explaining it.
///
/// The rules run worst-first, so a tag gets the one verdict that says what to
/// do about it: an automatic tag goes with `--purge-automatic` whatever else is
/// wrong with it, a field-redundant tag gets dropped rather than renamed, and
/// only then does the vocabulary have anything to say.
fn classify_tag(
    tag: &TagInfo,
    vocab: &Vocabulary,
    redundancy: Option<&str>,
) -> Option<(&'static str, String)> {
    if tag.is_automatic() {
        return Some((
            "automatic",
            "imported by Zotero from publisher or arXiv subject headings; \
             delete with --purge-automatic"
                .to_string(),
        ));
    }
    if let Some(detail) = redundancy {
        return Some(("field-redundant", detail.to_string()));
    }
    match vocabulary_verdict(&tag.tag, vocab) {
        VocabVerdict::Allowed => {
            if tag.meta.num_items == 0 {
                Some(("unused", "in the vocabulary, but on no items".to_string()))
            } else {
                None
            }
        }
        VocabVerdict::UnknownValue { facet, allowed } => Some((
            "unknown-facet-value",
            format!(
                "not a value of the {facet} facet (allowed: {})",
                allowed.join(", ")
            ),
        )),
        VocabVerdict::Unknown => Some((
            "out-of-vocabulary",
            "neither an allowed bare tag nor a known facet prefix".to_string(),
        )),
    }
}

/// What the vocabulary makes of one tag.
#[derive(Debug, PartialEq, Eq)]
enum VocabVerdict {
    Allowed,
    /// The prefix is a known facet, the value is not one of its own.
    UnknownValue {
        facet: String,
        allowed: Vec<String>,
    },
    /// No facet prefix and not an allowed bare tag.
    Unknown,
}

/// Judge one tag against the vocabulary.
///
/// A facet with no configured values is open, so `concept:title-block` passes
/// without the config listing every concept; a facet with values is closed, and
/// an unknown value there is a typo or a term nobody agreed on yet. Matching
/// folds case, because `Method:RAG` is the same mistake as a wrong value, not a
/// different tag to chase separately.
fn vocabulary_verdict(tag: &str, vocab: &Vocabulary) -> VocabVerdict {
    let lower = tag.trim().to_lowercase();
    for facet in &vocab.facets {
        let prefix = facet.prefix_with_colon().to_lowercase();
        if prefix.len() < 2 {
            continue;
        }
        if let Some(rest) = lower.strip_prefix(&prefix) {
            let value = rest.trim().to_string();
            let allowed: Vec<String> = facet.values.iter().map(|v| v.to_lowercase()).collect();
            if !value.is_empty() && (facet.is_open() || allowed.contains(&value)) {
                return VocabVerdict::Allowed;
            }
            return VocabVerdict::UnknownValue {
                facet: facet.prefix_with_colon(),
                allowed: if facet.is_open() {
                    vec!["any non-empty value".to_string()]
                } else {
                    facet.values.clone()
                },
            };
        }
    }
    if vocab
        .allowed_tags
        .iter()
        .any(|t| t.trim().to_lowercase() == lower)
    {
        return VocabVerdict::Allowed;
    }
    VocabVerdict::Unknown
}

/// Whether a tag only restates something the items already record in a field.
///
/// Every carrier has to restate it, not just one: a tag matching the `number`
/// of one standard while saying something real on five other items is a tag,
/// not noise. A single short token is not enough to judge on either, which
/// keeps `AI` from matching the letters inside a DOI.
fn field_redundancy(items: &[&ZoteroItem], tag: &str) -> Option<String> {
    let needle = tokens(tag);
    if needle.is_empty() || items.is_empty() {
        return None;
    }
    if needle.len() == 1 && needle[0].len() < 3 {
        return None;
    }
    let mut example: Option<String> = None;
    for item in items {
        let found = item_redundancy(item, &needle)?;
        example.get_or_insert(found);
    }
    let example = example?;
    Some(format!(
        "all {} item(s) carrying it already say so in {example}",
        items.len(),
    ))
}

/// Where one item already records what the tag says, if it does.
fn item_redundancy(item: &ZoteroItem, needle: &[String]) -> Option<String> {
    if tokens(&item.data.item_type) == needle {
        return Some(format!("itemType ({})", item.data.item_type));
    }
    for field in REDUNDANT_FIELDS {
        let value = item
            .data
            .extra
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if value.is_empty() {
            continue;
        }
        if token_run_contains(&tokens(value), needle) {
            return Some(format!("{field} (\"{value}\")"));
        }
    }
    None
}

/// Lowercase alphanumeric runs, split at letter/digit boundaries.
///
/// `DIN EN ISO 12006-2:2020-07` and `ISO12006` both yield `iso`, `12006`, so a
/// tag matches a field however either of them is punctuated.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_is_digit = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        let is_digit = c.is_numeric();
        if !current.is_empty() && is_digit != current_is_digit {
            out.push(std::mem::take(&mut current));
        }
        current_is_digit = is_digit;
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether `needle` appears as a consecutive run inside `hay`.
fn token_run_contains(hay: &[String], needle: &[String]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

// ---- --purge-automatic ----

/// What a purge would destroy.
#[derive(Debug)]
struct PurgeScope {
    /// Tag names, most used first.
    tags: Vec<String>,
    /// Item uses those tags account for.
    uses: u32,
    /// `(tag, uses)` for the head of the list, for the summary.
    sample: Vec<(String, u32)>,
}

/// How a destructive tag command is authorised.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// `--force` was passed: no prompt.
    Forced,
    /// Prompt, and go ahead only on exactly this answer.
    Prompt(String),
}

/// Delete every automatic tag in the library.
fn run_purge_automatic(force: bool, json: bool) -> Result<()> {
    let client = ZoteroClient::new()?;
    let mut tags = client.fetch_tags()?;
    tags.retain(TagInfo::is_automatic);

    if tags.is_empty() {
        let output = TagsPurgedOutput {
            deleted_count: 0,
            uses_removed: 0,
            batches: 0,
            deleted: Vec::new(),
            note: "  No automatic tags in the library; nothing was deleted.".to_string(),
        };
        println!("{}", format_output(&output, json));
        return Ok(());
    }

    let scope = purge_scope(&tags);
    let web = WebApiClient::from_config()?;

    // Everything the user reads goes to stderr, so `--json` stdout stays one
    // document even when the prompt and the summary are on screen.
    let gate = purge_gate(&scope, force, std::io::stdin().is_terminal())?;
    eprint!("{}", purge_summary(&scope));
    ask(&gate, "nothing was deleted")?;

    let mut deleted: Vec<String> = Vec::new();
    let mut batches = 0;
    for chunk in scope.tags.chunks(TAG_DELETE_BATCH) {
        delete_tag_batch(&web, chunk, deleted.len())?;
        deleted.extend_from_slice(chunk);
        batches += 1;
    }

    let output = TagsPurgedOutput {
        deleted_count: deleted.len(),
        uses_removed: scope.uses,
        batches,
        deleted,
        note: PURGE_NOTE.to_string(),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}

/// What the user is told once the purge has run.
///
/// The deletion is only half the fix: Zotero re-imports these tags with every
/// new paper until the setting is off, so a purge without this note buys about
/// a month.
const PURGE_NOTE: &str =
    "  Zotero keeps adding automatic tags on import: uncheck Settings -> General -> \
     \"Automatically tag items with keywords and subject headings\" \
     (pref extensions.zotero.automaticTags), or they are back within a month.\n  \
     The deletions reach the desktop app on the next Zotero sync.";

/// Delete one batch of tags, re-reading the library version each time.
///
/// The version has to be read per batch rather than once: every batch bumps it,
/// and Zotero syncing in the background bumps it too. A 412 says the library
/// moved under the write, so the version is re-read and the batch retried; only
/// a run of conflicts is fatal, and then the message says how far the purge
/// got, because the earlier batches are already gone.
fn delete_tag_batch(web: &WebApiClient, chunk: &[String], done: usize) -> Result<()> {
    const VERSION_RETRIES: u32 = 3;
    for attempt in 0..VERSION_RETRIES {
        let version = web.library_version()?;
        match web.delete_tags(chunk, version) {
            Ok(()) => return Ok(()),
            Err(e) if e.downcast_ref::<VersionConflict>().is_some() => {
                if attempt + 1 == VERSION_RETRIES {
                    bail!(
                        "The library on api.zotero.org kept changing between reading its \
                         version and deleting the next {} tag(s), so the purge stopped after \
                         {done} tag(s).\n  \
                         That usually means Zotero is syncing; wait for it to settle and \
                         re-run `zot tags --purge-automatic`.",
                        chunk.len(),
                    );
                }
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop returns or bails on its last attempt")
}

/// Work out what purging the automatic tags would remove.
fn purge_scope(tags: &[TagInfo]) -> PurgeScope {
    let mut ordered: Vec<(String, u32)> = tags
        .iter()
        .map(|t| (t.tag.clone(), t.meta.num_items))
        .collect();
    // Most used first: the head of that list is what lets the user recognise
    // the set they are about to delete.
    ordered.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    PurgeScope {
        uses: ordered.iter().map(|(_, n)| n).sum(),
        sample: ordered.iter().take(SAMPLE_SIZE).cloned().collect(),
        tags: ordered.into_iter().map(|(t, _)| t).collect(),
    }
}

/// Decide how a purge is authorised, or refuse when it cannot be.
///
/// A handful of tags takes `yes`; a real purge takes the tag count typed out,
/// which is the one number in the summary that says how big this is.
fn purge_gate(scope: &PurgeScope, force: bool, stdin_is_tty: bool) -> Result<Gate> {
    if force {
        return Ok(Gate::Forced);
    }
    if !stdin_is_tty {
        bail!(
            "Refusing to delete {} automatic tag(s) without confirmation: stdin is not a \
             terminal, so there is nobody to ask.\n  \
             This would remove them from {} item use(s) library-wide, and a deleted tag can \
             only be re-typed, not restored.\n  \
             Re-run it interactively, or pass --force if you have already checked what it \
             removes.",
            scope.tags.len(),
            scope.uses,
        );
    }
    if scope.tags.len() < SMALL_SCOPE {
        Ok(Gate::Prompt("yes".to_string()))
    } else {
        Ok(Gate::Prompt(scope.tags.len().to_string()))
    }
}

/// What the user reads before confirming a purge.
fn purge_summary(scope: &PurgeScope) -> String {
    let mut out = format!(
        "About to delete {} automatic tag(s) from the whole library, covering {} item use(s).\n",
        scope.tags.len(),
        scope.uses,
    );
    out.push_str("  Most used:\n");
    for (tag, uses) in &scope.sample {
        out.push_str(&format!("    {tag} ({uses})\n"));
    }
    if scope.tags.len() > scope.sample.len() {
        out.push_str(&format!(
            "    ... and {} more (see `zot tags --automatic`).\n",
            scope.tags.len() - scope.sample.len(),
        ));
    }
    out.push_str(
        "  The items are NOT deleted, only these tags come off them.\n  \
         Permanent: a deleted tag can be re-typed, not restored.\n",
    );
    out
}

// ---- --rename ----

/// What a rename would touch.
#[derive(Debug)]
struct RenameScope {
    old: String,
    new: String,
    /// Keys of the items carrying the old tag, in library order.
    item_keys: Vec<String>,
    /// How many of those already carry the new tag, so the two merge there.
    merges: usize,
    /// The old tag is automatic, so the rename also turns it manual.
    was_automatic: bool,
    /// Titles of the head of the list, for the summary.
    sample: Vec<String>,
}

/// Rename one tag everywhere, merging it into the target when that exists.
fn run_rename(old: &str, new: &str, force: bool, json: bool) -> Result<()> {
    let (old, new) = (old.trim(), new.trim());
    if old.is_empty() || new.is_empty() {
        bail!("Both the old and the new tag have to be non-empty.");
    }
    if old == new {
        bail!("\"{old}\" is already the name it would be renamed to; nothing to do.");
    }

    let client = ZoteroClient::new()?;
    let tags = client.fetch_tags()?;
    let Some(info) = tags.iter().find(|t| t.tag == old) else {
        bail!(
            "No tag \"{old}\" in the library (tags are case-sensitive).\n  \
             Find the exact spelling with: zot tags --contains \"{}\"",
            old.chars().take(12).collect::<String>(),
        );
    };
    let target_exists = tags.iter().any(|t| t.tag == new);
    let items = client.fetch_items_by_tag(old)?;
    if items.is_empty() {
        bail!(
            "\"{old}\" is on no items, so there is nothing to rename.\n  \
             Remove the empty tag in the Zotero tag selector instead."
        );
    }

    let scope = rename_scope(old, new, info, &items);
    let web = WebApiClient::from_config()?;

    let gate = rename_gate(&scope, target_exists, force, std::io::stdin().is_terminal())?;
    eprint!("{}", rename_summary(&scope, target_exists));
    ask(&gate, "nothing was renamed")?;

    let mut updated = 0;
    for key in &scope.item_keys {
        // Re-read from the web API rather than reusing the local copy: the
        // PATCH replaces the whole tags array, so it has to be computed from
        // the state the version guard names.
        let server = web.get_item(key)?;
        let version = server
            .get("version")
            .and_then(|v| v.as_u64())
            .context("No version on item")?;
        let current = server
            .pointer("/data/tags")
            .and_then(|t| t.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        let Some(next) = renamed_tags(current, old, new) else {
            // The server copy no longer carries the old tag (already renamed,
            // or the local library is ahead of a sync): nothing to write.
            continue;
        };
        if let Err(e) = web.patch_item(key, &json!({ "tags": next }), version) {
            if e.downcast_ref::<VersionConflict>().is_some() {
                bail!(
                    "Item {key} changed on api.zotero.org between the read and the write, so \
                     the rename stopped after {updated} item(s).\n  \
                     Re-run `zot tags --rename \"{old}\" \"{new}\"`: it picks up where it left \
                     off, since it only touches items that still carry \"{old}\"."
                );
            }
            return Err(e);
        }
        updated += 1;
    }

    // Sweep: the tag record survives an item rewrite that left it on nothing,
    // and only a library-level DELETE removes it. Reached only when every
    // PATCH landed, so it can never delete a tag the rename failed to move.
    let version = web.library_version()?;
    let removed_old_tag = web
        .delete_tags(std::slice::from_ref(&scope.old), version)
        .is_ok();

    let output = TagRenamedOutput {
        old: scope.old,
        new: scope.new,
        items_updated: updated,
        items_merged: scope.merges,
        removed_old_tag,
        note: rename_note(scope.was_automatic, removed_old_tag),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}

/// The trailing note of a rename.
fn rename_note(was_automatic: bool, removed_old_tag: bool) -> Option<String> {
    let mut parts = Vec::new();
    if was_automatic {
        parts.push(
            "The old tag was automatic; the new one is manual, so Zotero will not re-import \
             it."
                .to_string(),
        );
    }
    if !removed_old_tag {
        parts.push(
            "The old tag record could not be swept afterwards; check `zot tags --unused` and \
             remove it in Zotero if it lingers."
                .to_string(),
        );
    }
    parts.push("The changes reach the desktop app on the next Zotero sync.".to_string());
    Some(parts.join(" "))
}

/// Work out what renaming `old` to `new` would touch.
fn rename_scope(old: &str, new: &str, info: &TagInfo, items: &[ZoteroItem]) -> RenameScope {
    RenameScope {
        old: old.to_string(),
        new: new.to_string(),
        merges: items
            .iter()
            .filter(|i| i.data.tags.iter().any(|t| t.tag == new))
            .count(),
        was_automatic: info.is_automatic(),
        sample: items
            .iter()
            .take(SAMPLE_SIZE)
            .map(|i| {
                let title = if i.data.title.is_empty() {
                    i.data.item_type.clone()
                } else {
                    crate::output::truncate_display(&i.data.title, 60)
                };
                format!("{title} [{}]", i.key)
            })
            .collect(),
        item_keys: items.iter().map(|i| i.key.clone()).collect(),
    }
}

/// Decide how a rename is authorised, or refuse when it cannot be.
///
/// A rename is reversible by renaming back, so a handful of items takes `yes`.
/// What is not reversible is a merge: once two tags are one, nothing records
/// which items carried which, so a merge always takes the old tag typed out.
fn rename_gate(
    scope: &RenameScope,
    target_exists: bool,
    force: bool,
    stdin_is_tty: bool,
) -> Result<Gate> {
    if force {
        return Ok(Gate::Forced);
    }
    if !stdin_is_tty {
        bail!(
            "Refusing to rename \"{}\" on {} item(s) without confirmation: stdin is not a \
             terminal, so there is nobody to ask.\n  \
             Re-run it interactively, or pass --force if you have already checked what it \
             changes.",
            scope.old,
            scope.item_keys.len(),
        );
    }
    if target_exists || scope.item_keys.len() >= SMALL_SCOPE {
        Ok(Gate::Prompt(scope.old.clone()))
    } else {
        Ok(Gate::Prompt("yes".to_string()))
    }
}

/// What the user reads before confirming a rename.
fn rename_summary(scope: &RenameScope, target_exists: bool) -> String {
    let mut out = format!(
        "About to rename tag \"{}\" to \"{}\" on {} item(s).\n",
        scope.old,
        scope.new,
        scope.item_keys.len(),
    );
    if target_exists {
        out.push_str(&format!(
            "  \"{}\" already exists ({} of these items carry it), so the two tags merge into \
             one and the split cannot be recovered.\n",
            scope.new, scope.merges,
        ));
    }
    if scope.was_automatic {
        out.push_str(&format!(
            "  \"{}\" is an automatic tag; the renamed one is manual, which is what stops \
             Zotero re-importing it.\n",
            scope.old,
        ));
    }
    for title in &scope.sample {
        out.push_str(&format!("    {title}\n"));
    }
    if scope.item_keys.len() > scope.sample.len() {
        out.push_str(&format!(
            "    ... and {} more.\n",
            scope.item_keys.len() - scope.sample.len(),
        ));
    }
    out.push_str(
        "  Each item's tag list is rewritten on api.zotero.org, then the old tag is deleted \
         library-wide.\n",
    );
    out
}

/// The item's tags after the rename, or `None` when it does not carry the old
/// one.
///
/// The whole array is returned because a PATCH replaces it wholesale. The
/// renamed entry loses its `type`, so renaming an automatic tag produces a
/// manual one, which is the point: a tag that stays automatic is re-imported
/// and re-purged forever. A duplicate is dropped rather than written twice,
/// since the item may already carry the target tag.
fn renamed_tags(
    current: &[serde_json::Value],
    old: &str,
    new: &str,
) -> Option<Vec<serde_json::Value>> {
    if !current
        .iter()
        .any(|t| t.get("tag").and_then(|s| s.as_str()) == Some(old))
    {
        return None;
    }
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(current.len());
    let mut seen: Vec<String> = Vec::with_capacity(current.len());
    for entry in current {
        let name = entry.get("tag").and_then(|s| s.as_str()).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let (name, value) = if name == old {
            (new.to_string(), json!({ "tag": new }))
        } else {
            (name.to_string(), entry.clone())
        };
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        out.push(value);
    }
    Some(out)
}

// ---- confirmation ----

/// Put the question, and abort unless it is answered exactly.
fn ask(gate: &Gate, nothing_happened: &str) -> Result<()> {
    let Gate::Prompt(expected) = gate else {
        return Ok(());
    };
    eprint!("{}", prompt_line(expected));
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("Failed to read the confirmation from stdin")?;
    if !confirmed(&answer, expected) {
        bail!("Aborted: {nothing_happened}.");
    }
    Ok(())
}

/// Whether a typed answer authorises the write.
///
/// Only surrounding whitespace is forgiven, the newline `read_line` leaves
/// above all. Case is not: `YES` is a near-miss of `yes`, and a near-miss of an
/// irreversible write has to abort.
fn confirmed(answer: &str, expected: &str) -> bool {
    answer.trim() == expected
}

/// The line asking for `expected`.
fn prompt_line(expected: &str) -> String {
    if expected == "yes" {
        "Type yes to go ahead, anything else aborts: ".to_string()
    } else if expected.chars().all(|c| c.is_ascii_digit()) {
        format!("Type the number of tags ({expected}) to confirm, anything else aborts: ")
    } else {
        format!("Type the tag (\"{expected}\") to confirm, anything else aborts: ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::TagMeta;
    use serde_json::json;

    fn tag(name: &str, num_items: u32, automatic: bool) -> TagInfo {
        TagInfo {
            tag: name.to_string(),
            meta: TagMeta {
                num_items,
                tag_type: if automatic { Some(1) } else { Some(0) },
            },
        }
    }

    fn item(item_type: &str, extra: serde_json::Value, tags: &[&str]) -> ZoteroItem {
        let mut data = json!({
            "key": "AAAABBBB",
            "version": 1,
            "itemType": item_type,
            "tags": tags.iter().map(|t| json!({"tag": t})).collect::<Vec<_>>(),
        });
        if let Some(fields) = extra.as_object() {
            for (k, v) in fields {
                data[k] = v.clone();
            }
        }
        serde_json::from_value(json!({
            "key": "AAAABBBB",
            "version": 1,
            "data": data,
        }))
        .expect("fixture item")
    }

    #[test]
    fn vocabulary_accepts_bare_tags_and_closed_facet_values() {
        let vocab = Vocabulary::default();
        assert_eq!(vocabulary_verdict("to-read", &vocab), VocabVerdict::Allowed);
        assert_eq!(vocabulary_verdict("method:rag", &vocab), VocabVerdict::Allowed);
        assert_eq!(vocabulary_verdict("kind:survey", &vocab), VocabVerdict::Allowed);
    }

    #[test]
    fn vocabulary_accepts_any_value_of_an_open_facet() {
        let vocab = Vocabulary::default();
        assert_eq!(
            vocabulary_verdict("concept:title-block", &vocab),
            VocabVerdict::Allowed
        );
        assert_eq!(
            vocabulary_verdict("course:bim-advanced-ss26", &vocab),
            VocabVerdict::Allowed
        );
    }

    #[test]
    fn vocabulary_rejects_an_unknown_value_of_a_closed_facet() {
        let vocab = Vocabulary::default();
        match vocabulary_verdict("method:transformers", &vocab) {
            VocabVerdict::UnknownValue { facet, allowed } => {
                assert_eq!(facet, "method:");
                assert!(allowed.contains(&"rag".to_string()));
            }
            other => panic!("expected UnknownValue, got {other:?}"),
        }
    }

    #[test]
    fn vocabulary_rejects_an_empty_facet_value() {
        let vocab = Vocabulary::default();
        assert!(matches!(
            vocabulary_verdict("concept:", &vocab),
            VocabVerdict::UnknownValue { .. }
        ));
    }

    #[test]
    fn vocabulary_rejects_an_unprefixed_unknown_tag() {
        let vocab = Vocabulary::default();
        assert_eq!(vocabulary_verdict("AEC", &vocab), VocabVerdict::Unknown);
    }

    #[test]
    fn tokens_split_words_punctuation_and_letter_digit_boundaries() {
        assert_eq!(
            tokens("DIN EN ISO 12006-2:2020-07"),
            ["din", "en", "iso", "12006", "2", "2020", "07"]
        );
        assert_eq!(tokens("ISO12006"), ["iso", "12006"]);
        assert_eq!(tokens("  "), Vec::<String>::new());
    }

    #[test]
    fn a_tag_restating_the_number_field_is_field_redundant() {
        let items = [item(
            "standard",
            json!({ "number": "DIN EN ISO 12006-2:2020-07" }),
            &["ISO 12006"],
        )];
        let refs: Vec<&ZoteroItem> = items.iter().collect();
        let detail = field_redundancy(&refs, "ISO 12006").expect("flagged");
        assert!(detail.contains("number"), "{detail}");
    }

    #[test]
    fn a_tag_restating_the_item_type_is_field_redundant() {
        let items = [item("standard", json!({}), &["standard"])];
        let refs: Vec<&ZoteroItem> = items.iter().collect();
        let detail = field_redundancy(&refs, "standard").expect("flagged");
        assert!(detail.contains("itemType"), "{detail}");
    }

    #[test]
    fn a_tag_only_some_carriers_restate_is_not_field_redundant() {
        let items = [
            item("standard", json!({}), &["standard"]),
            item("journalArticle", json!({}), &["standard"]),
        ];
        let refs: Vec<&ZoteroItem> = items.iter().collect();
        assert!(field_redundancy(&refs, "standard").is_none());
    }

    #[test]
    fn a_two_letter_tag_is_never_field_redundant() {
        let items = [item(
            "journalArticle",
            json!({ "DOI": "10.1000/ai" }),
            &["AI"],
        )];
        let refs: Vec<&ZoteroItem> = items.iter().collect();
        assert!(field_redundancy(&refs, "AI").is_none());
    }

    #[test]
    fn classify_puts_automatic_first() {
        let vocab = Vocabulary::default();
        let t = tag("Artificial Intelligence (cs.AI)", 45, true);
        let (class, _) = classify_tag(&t, &vocab, Some("whatever")).expect("flagged");
        assert_eq!(class, "automatic");
    }

    #[test]
    fn classify_prefers_field_redundancy_over_the_vocabulary() {
        let vocab = Vocabulary::default();
        let t = tag("ISO 12006", 3, false);
        let (class, detail) = classify_tag(&t, &vocab, Some("number says so")).expect("flagged");
        assert_eq!(class, "field-redundant");
        assert_eq!(detail, "number says so");
    }

    #[test]
    fn classify_passes_a_vocabulary_tag_in_use() {
        let vocab = Vocabulary::default();
        assert!(classify_tag(&tag("method:rag", 4, false), &vocab, None).is_none());
    }

    #[test]
    fn classify_flags_a_vocabulary_tag_on_no_items_as_unused() {
        let vocab = Vocabulary::default();
        let (class, _) = classify_tag(&tag("method:cv", 0, false), &vocab, None).expect("flagged");
        assert_eq!(class, "unused");
    }

    #[test]
    fn renamed_tags_replaces_the_old_entry_and_drops_its_type() {
        let current = vec![json!({"tag": "cs.AI", "type": 1}), json!({"tag": "read"})];
        let next = renamed_tags(&current, "cs.AI", "method:llm").expect("rewritten");
        assert_eq!(
            next,
            vec![json!({"tag": "method:llm"}), json!({"tag": "read"})]
        );
    }

    #[test]
    fn renamed_tags_merges_into_an_existing_target() {
        let current = vec![json!({"tag": "AI"}), json!({"tag": "method:llm"})];
        let next = renamed_tags(&current, "AI", "method:llm").expect("rewritten");
        assert_eq!(next, vec![json!({"tag": "method:llm"})]);
    }

    #[test]
    fn renamed_tags_returns_none_when_the_old_tag_is_absent() {
        let current = vec![json!({"tag": "read"})];
        assert!(renamed_tags(&current, "AI", "method:llm").is_none());
    }

    #[test]
    fn purge_scope_orders_by_use_and_sums_them() {
        let tags = [tag("rare", 1, true), tag("common", 45, true)];
        let scope = purge_scope(&tags);
        assert_eq!(scope.tags, ["common", "rare"]);
        assert_eq!(scope.uses, 46);
    }

    #[test]
    fn a_purge_gate_refuses_a_non_terminal_without_force() {
        let scope = purge_scope(&[tag("a", 1, true)]);
        let err = purge_gate(&scope, false, false).unwrap_err().to_string();
        assert!(err.contains("stdin is not a terminal"), "{err}");
    }

    #[test]
    fn a_big_purge_asks_for_the_tag_count() {
        let tags: Vec<TagInfo> = (0..12).map(|i| tag(&format!("t{i}"), 1, true)).collect();
        let scope = purge_scope(&tags);
        assert_eq!(
            purge_gate(&scope, false, true).unwrap(),
            Gate::Prompt("12".to_string())
        );
    }

    #[test]
    fn a_small_purge_asks_for_yes() {
        let scope = purge_scope(&[tag("a", 1, true)]);
        assert_eq!(
            purge_gate(&scope, false, true).unwrap(),
            Gate::Prompt("yes".to_string())
        );
        assert_eq!(purge_gate(&scope, true, true).unwrap(), Gate::Forced);
    }

    #[test]
    fn a_merging_rename_asks_for_the_tag_name() {
        let items = [item("journalArticle", json!({}), &["AI", "method:llm"])];
        let scope = rename_scope("AI", "method:llm", &tag("AI", 1, false), &items);
        assert_eq!(scope.merges, 1);
        assert_eq!(
            rename_gate(&scope, true, false, true).unwrap(),
            Gate::Prompt("AI".to_string())
        );
    }

    #[test]
    fn a_small_plain_rename_asks_for_yes() {
        let items = [item("journalArticle", json!({}), &["AI"])];
        let scope = rename_scope("AI", "method:llm", &tag("AI", 1, false), &items);
        assert_eq!(
            rename_gate(&scope, false, false, true).unwrap(),
            Gate::Prompt("yes".to_string())
        );
    }

    #[test]
    fn confirmation_is_case_sensitive_and_forgives_only_whitespace() {
        assert!(confirmed(" yes \n", "yes"));
        assert!(!confirmed("YES\n", "yes"));
        assert!(!confirmed("y\n", "yes"));
    }
}
