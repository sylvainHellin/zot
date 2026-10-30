//! Locate a passage in a PDF by its first and last words, for `zot annotate`.
//!
//! Word boxes come from poppler's `pdftotext -raw -bbox`, which reports every
//! word with its bounding box in content-stream order. oxidize-pdf (the
//! indexer's extractor) only exposes one box per text-show operator, which is a
//! whole line in many PDFs, so it cannot place a highlight on a word.
//!
//! Stream order rather than poppler's `-bbox-layout` reading order, because the
//! layout analysis interleaves table cells and moves paragraphs around
//! figures, which made a passage either vanish or silently swallow words of a
//! neighbouring column. Stream order is also what pdf.js, and so Zotero's
//! reader, sees. Its cost is that lines are not given, so [`group_lines`]
//! rebuilds them from the word boxes.
//!
//! Page geometry (MediaBox, CropBox, rotation) comes from poppler's `pdfinfo`,
//! so both numbers are produced by the same engine; page labels come from the
//! document catalog, read with oxidize-pdf.
//!
//! Everything below [`extract_words`] and [`page_geometry`] is pure and
//! unit-tested: tokenising, matching, grouping words into lines, merging word
//! boxes into one rectangle per line, and mapping poppler's device space back
//! to PDF user space.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// How far past the start of `--from` the end of `--to` may lie, in word
/// tokens. Keeps a common closing phrase from matching half a page later.
pub const TO_WINDOW: usize = 400;

/// CPU-time cap (seconds) for the poppler children, as for the indexer's
/// extraction worker: a PDF that makes them spin fails instead of hanging.
const POPPLER_CPU_LIMIT_SECS: u64 = 120;

/// Virtual-memory cap (KB) for the poppler children (~3 GB).
const POPPLER_MEM_LIMIT_KB: u64 = 3_000_000;

/// One word as poppler reports it, in device space: points from the top-left
/// corner of the rotated MediaBox, y growing downward.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfWord {
    pub text: String,
    /// `[x_min, y_min, x_max, y_max]` in device space.
    pub bbox: [f64; 4],
    /// Paragraph number on the page, from [`group_lines`]: consecutive lines
    /// that wrap directly below each other share it.
    pub block: usize,
    /// Line number on the page, from [`group_lines`].
    pub line: usize,
}

/// The words of one physical page.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfPage {
    /// 0-based physical page index.
    pub index: usize,
    pub words: Vec<PdfWord>,
}

/// A word token for matching: the normalized text of one word, or of two when
/// a word hyphenated across a line break is joined back together.
#[derive(Debug, Clone, PartialEq)]
struct Token {
    norm: String,
    first_word: usize,
    last_word: usize,
}

/// A located passage on one page.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub page_index: usize,
    /// Index of the first and last word in `PdfPage::words`.
    pub first_word: usize,
    pub last_word: usize,
    /// The words of the passage joined by spaces, a hyphenated line break
    /// joined back into one word.
    pub text: String,
    /// Characters of page text (words joined by single spaces) before the
    /// passage, for the middle field of Zotero's sort index.
    pub char_offset: usize,
    /// One rectangle per text line, in device space, top line first.
    pub device_rects: Vec<[f64; 4]>,
}

/// Lowercase a word and drop everything that is not a letter or digit, so
/// matching ignores case and punctuation. Typographic ligatures are expanded,
/// since a PDF may carry `ﬁ` where the query has `fi`.
pub fn normalize(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => {}
        }
    }
    out
}

/// Normalized tokens of a query, empty ones (pure punctuation) dropped.
pub fn query_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(normalize)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Whether a word ends in a hyphen that may be a line-break hyphenation.
fn ends_with_hyphen(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(
        chars.next_back(),
        Some('-' | '\u{2010}' | '\u{2011}' | '\u{00AD}')
    ) && chars.next().is_some()
}

/// Whether word `i` is hyphenated at the end of its line and continues on the
/// next line of the same block, so the two read as one word.
fn joins_next(words: &[PdfWord], i: usize) -> bool {
    let (Some(w), Some(next)) = (words.get(i), words.get(i + 1)) else {
        return false;
    };
    ends_with_hyphen(&w.text) && next.line != w.line && next.block == w.block
}

/// Tokenise a page. Words that normalize to nothing (a lone dash, a bullet)
/// are skipped, so the query need not reproduce them.
fn page_tokens(words: &[PdfWord]) -> Vec<Token> {
    let mut tokens = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        if joins_next(words, i) {
            let norm = normalize(&words[i].text) + &normalize(&words[i + 1].text);
            if !norm.is_empty() {
                tokens.push(Token {
                    norm,
                    first_word: i,
                    last_word: i + 1,
                });
            }
            i += 2;
            continue;
        }
        let norm = normalize(&words[i].text);
        if !norm.is_empty() {
            tokens.push(Token {
                norm,
                first_word: i,
                last_word: i,
            });
        }
        i += 1;
    }
    tokens
}

/// How many tokens may be glued together to bridge a difference in word
/// splitting between the query and the PDF.
const MAX_JOIN: usize = 3;

/// Match `query` against the page tokens starting at `at`, returning the
/// index one past the last page token consumed.
///
/// Tokens normally pair one to one, but the query and the PDF need not split
/// words the same way: poppler makes a superscript its own word (`Kern` `3`
/// where a reader copied `Kern3`), and a reader may split what the PDF holds
/// as one word. So up to [`MAX_JOIN`] consecutive tokens on either side may
/// stand for one on the other, provided their concatenation is equal. Each
/// branch needs an exact string equality, so the backtracking stays tiny.
fn match_at(tokens: &[Token], at: usize, query: &[String]) -> Option<usize> {
    let Some(q) = query.first() else {
        return Some(at);
    };
    let t = &tokens.get(at)?.norm;
    if t == q {
        if let Some(end) = match_at(tokens, at + 1, &query[1..]) {
            return Some(end);
        }
    }
    // Several page tokens spell one query token.
    if q.starts_with(t.as_str()) {
        let mut acc = t.clone();
        for k in 1..MAX_JOIN {
            let Some(next) = tokens.get(at + k) else {
                break;
            };
            acc.push_str(&next.norm);
            if &acc == q {
                if let Some(end) = match_at(tokens, at + k + 1, &query[1..]) {
                    return Some(end);
                }
            }
            if !q.starts_with(acc.as_str()) {
                break;
            }
        }
    }
    // One page token spells several query tokens.
    if t.starts_with(q.as_str()) {
        let mut acc = q.clone();
        for k in 1..MAX_JOIN.min(query.len()) {
            acc.push_str(&query[k]);
            if &acc == t {
                if let Some(end) = match_at(tokens, at + 1, &query[k + 1..]) {
                    return Some(end);
                }
            }
            if !t.starts_with(acc.as_str()) {
                break;
            }
        }
    }
    None
}

/// Find `from` and then the nearest `to` starting at or after it within
/// `window` tokens, on one page. Returns the first and last word indices.
///
/// The first occurrence of `from` that has a `to` after it wins; an earlier
/// `from` with no `to` in reach is skipped rather than failing the search.
fn find_on_page(
    words: &[PdfWord],
    from: &[String],
    to: &[String],
    window: usize,
) -> (Option<(usize, usize)>, bool) {
    let tokens = page_tokens(words);
    let mut from_seen = false;
    for i in 0..tokens.len() {
        let Some(from_end) = match_at(&tokens, i, from) else {
            continue;
        };
        from_seen = true;
        let limit = (i + window).min(tokens.len());
        for j in i..limit {
            if let Some(to_end) = match_at(&tokens, j, to) {
                let end = to_end.max(from_end) - 1;
                return (Some((tokens[i].first_word, tokens[end].last_word)), true);
            }
        }
    }
    (None, from_seen)
}

/// Merge word boxes into one rectangle per line, top line first (ties broken
/// left to right, which orders the columns of a two-column line).
pub fn merge_line_rects(words: &[PdfWord]) -> Vec<[f64; 4]> {
    let mut lines: Vec<(usize, [f64; 4])> = Vec::new();
    for w in words {
        match lines.iter_mut().find(|(line, _)| *line == w.line) {
            Some((_, r)) => {
                r[0] = r[0].min(w.bbox[0]);
                r[1] = r[1].min(w.bbox[1]);
                r[2] = r[2].max(w.bbox[2]);
                r[3] = r[3].max(w.bbox[3]);
            }
            None => lines.push((w.line, w.bbox)),
        }
    }
    let mut rects: Vec<[f64; 4]> = lines.into_iter().map(|(_, r)| r).collect();
    rects.sort_by(|a, b| a[1].total_cmp(&b[1]).then(a[0].total_cmp(&b[0])));
    rects
}

/// The passage text: words joined by spaces, a line-break hyphenation joined
/// back without its hyphen.
fn passage_text(words: &[PdfWord], first: usize, last: usize) -> String {
    let mut text = String::new();
    for i in first..=last {
        let w = &words[i].text;
        if i < last && joins_next(words, i) {
            let mut chars = w.chars();
            chars.next_back();
            text.push_str(chars.as_str());
        } else {
            text.push_str(w);
            if i < last {
                text.push(' ');
            }
        }
    }
    text
}

/// Locate a passage by its first and last words.
///
/// Every page is searched in order unless `page` (0-based) restricts it. A
/// passage must lie on one page; one that starts on a page and ends on the
/// next is reported as such rather than silently cut.
pub fn locate(
    pages: &[PdfPage],
    from: &str,
    to: &str,
    window: usize,
    page: Option<usize>,
) -> Result<Passage> {
    let from_q = query_tokens(from);
    let to_q = query_tokens(to);
    if from_q.is_empty() {
        bail!("--from has no letters or digits to match.");
    }
    if to_q.is_empty() {
        bail!("--to has no letters or digits to match.");
    }

    let mut from_pages: Vec<usize> = Vec::new();
    for p in pages
        .iter()
        .filter(|p| page.is_none_or(|want| p.index == want))
    {
        let (found, from_seen) = find_on_page(&p.words, &from_q, &to_q, window);
        if let Some((first, last)) = found {
            let char_offset = p.words[..first]
                .iter()
                .map(|w| w.text.chars().count() + 1)
                .sum();
            return Ok(Passage {
                page_index: p.index,
                first_word: first,
                last_word: last,
                text: passage_text(&p.words, first, last),
                char_offset,
                device_rects: merge_line_rects(&p.words[first..=last]),
            });
        }
        if from_seen {
            from_pages.push(p.index + 1);
        }
    }

    let scope = match page {
        Some(i) => format!("on page {}", i + 1),
        None => "anywhere in the PDF".to_string(),
    };
    if from_pages.is_empty() {
        bail!("--from \"{from}\" not found {scope}.");
    }
    let listed = from_pages
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "--from \"{from}\" found on page(s) {listed}, but no --to \"{to}\" follows it within \
         {window} words on the same page.\n  A highlight cannot span pages: if the passage runs onto \
         the next page, annotate each page's part separately."
    );
}

/// The geometry of one page, as `pdfinfo -box` reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    /// `[x1, y1, x2, y2]` in user space, normalized so x1 < x2 and y1 < y2.
    pub media_box: [f64; 4],
    pub crop_box: [f64; 4],
    /// Clockwise page rotation: 0, 90, 180 or 270.
    pub rotation: i32,
}

impl PageGeometry {
    /// Map a device-space rectangle from `pdftotext -bbox-layout` to PDF user
    /// space, `[x_min, y_min, x_max, y_max]`, which is where Zotero's
    /// `annotationPosition` rects live.
    ///
    /// pdftotext renders against the MediaBox with the page's /Rotate applied
    /// (poppler's `GfxState` page CTM); this inverts that transform for each
    /// rotation, then re-normalizes the corners.
    pub fn device_to_user(&self, r: [f64; 4]) -> [f64; 4] {
        let [mx1, my1, mx2, my2] = self.media_box;
        let map = |xd: f64, yd: f64| -> (f64, f64) {
            match self.rotation {
                90 => (yd + mx1, xd + my1),
                180 => (mx2 - xd, yd + my1),
                270 => (mx2 - yd, my2 - xd),
                _ => (xd + mx1, my2 - yd),
            }
        };
        let (ax, ay) = map(r[0], r[1]);
        let (bx, by) = map(r[2], r[3]);
        [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
    }

    /// The `TTTTT` field of Zotero's sort index: distance in points from the
    /// top of the visible page (the CropBox) down to the top of the passage,
    /// floored, never negative. Only the order of annotations on one page
    /// depends on it.
    pub fn sort_top(&self, user_rects: &[[f64; 4]]) -> u64 {
        let top_y = user_rects
            .iter()
            .map(|r| r[3])
            .fold(f64::NEG_INFINITY, f64::max);
        let top = (self.crop_box[3] - top_y).floor();
        if top.is_finite() && top > 0.0 {
            top as u64
        } else {
            0
        }
    }
}

/// Zotero's `annotationSortIndex`: `PPPPP|OOOOOO|TTTTT`.
pub fn sort_index(page_index: usize, char_offset: usize, top: u64) -> String {
    format!(
        "{:05}|{:06}|{:05}",
        page_index.min(99_999),
        char_offset.min(999_999),
        top.min(99_999)
    )
}

/// Round to three decimals, like the reference script and Zotero's own rects.
pub fn round3(r: [f64; 4]) -> [f64; 4] {
    r.map(|v| (v * 1000.0).round() / 1000.0)
}

// ---- poppler ----

/// Run a poppler tool under the same CPU and memory caps as the indexer's
/// extraction worker, returning its stdout.
fn run_poppler(tool: &str, args: &[String]) -> Result<String> {
    let script = format!(
        "ulimit -v {POPPLER_MEM_LIMIT_KB} 2>/dev/null; ulimit -t {POPPLER_CPU_LIMIT_SECS} \
         2>/dev/null; exec \"$0\" \"$@\""
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .arg(tool)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run {tool}"))?;
    if output.status.code() == Some(127) {
        bail!(
            "{tool} not found. zot annotate needs poppler's command-line tools: \
             `brew install poppler` on macOS, `apt install poppler-utils` on Debian/Ubuntu."
        );
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{tool} failed ({}): {}",
            output.status,
            stderr.lines().last().unwrap_or("").trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every word of the PDF (or of one 0-based page) with its box, from
/// `pdftotext -raw -bbox`, grouped into lines.
pub fn extract_words(pdf: &Path, page: Option<usize>) -> Result<Vec<PdfPage>> {
    let mut args = vec!["-raw".to_string(), "-bbox".to_string()];
    if let Some(p) = page {
        args.extend([
            "-f".into(),
            (p + 1).to_string(),
            "-l".into(),
            (p + 1).to_string(),
        ]);
    }
    args.push(pdf.to_string_lossy().into_owned());
    args.push("-".to_string());
    let xhtml = run_poppler("pdftotext", &args)?;
    let mut pages = parse_bbox(&xhtml, page.unwrap_or(0));
    for p in &mut pages {
        group_lines(&mut p.words);
    }
    Ok(pages)
}

/// Number the lines and paragraphs of a page's words, in stream order.
///
/// A word continues the previous word's line when the two overlap vertically
/// by at least half the smaller height and it lies to the right without a gap
/// wider than three line heights (a jump to another column starts a new
/// line). A new line continues the paragraph when it starts directly below
/// the previous line, at most one line height further down, and wraps back
/// to the left; that is what lets a word hyphenated at the end of a line be
/// joined to its continuation.
pub fn group_lines(words: &mut [PdfWord]) {
    let (mut line, mut block) = (0, 0);
    for i in 0..words.len() {
        if i > 0 {
            let a = words[i - 1].bbox;
            let b = words[i].bbox;
            let (ha, hb) = (a[3] - a[1], b[3] - b[1]);
            let overlap = a[3].min(b[3]) - a[1].max(b[1]);
            let same_line = overlap > 0.5 * ha.min(hb).max(0.1)
                && b[0] > a[0] - 1.0
                && b[0] - a[2] < 3.0 * ha.max(hb);
            if !same_line {
                let wraps_below = b[1] > a[1] && b[1] - a[3] < ha.max(hb) && b[0] < a[2];
                if !wraps_below {
                    block += 1;
                }
                line += 1;
            }
        }
        words[i].line = line;
        words[i].block = block;
    }
}

/// Page count and the geometry of one 0-based page, from `pdfinfo -box`.
pub fn page_geometry(pdf: &Path, page: usize) -> Result<PageGeometry> {
    let n = (page + 1).to_string();
    let args = vec![
        "-box".to_string(),
        "-f".to_string(),
        n.clone(),
        "-l".to_string(),
        n,
        pdf.to_string_lossy().into_owned(),
    ];
    let info = run_poppler("pdfinfo", &args)?;
    parse_pdfinfo_boxes(&info, page + 1)
        .with_context(|| format!("pdfinfo reported no MediaBox for page {}", page + 1))
}

/// Number of pages, from `pdfinfo`.
pub fn page_count(pdf: &Path) -> Result<usize> {
    let info = run_poppler("pdfinfo", &[pdf.to_string_lossy().into_owned()])?;
    info.lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse().ok())
        .context("pdfinfo reported no page count")
}

/// Parse the `MediaBox`, `CropBox` and `rot` lines pdfinfo prints for page
/// `page_no` (1-based). The CropBox falls back to the MediaBox.
fn parse_pdfinfo_boxes(info: &str, page_no: usize) -> Option<PageGeometry> {
    let mut media = None;
    let mut crop = None;
    let mut rotation = 0;
    for line in info.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("Page") {
            continue;
        }
        if parts.next().and_then(|n| n.parse::<usize>().ok()) != Some(page_no) {
            continue;
        }
        let field = parts.next().unwrap_or("");
        let nums: Vec<f64> = parts.filter_map(|v| v.parse().ok()).collect();
        match field {
            "MediaBox:" if nums.len() == 4 => media = Some(normalize_box(&nums)),
            "CropBox:" if nums.len() == 4 => crop = Some(normalize_box(&nums)),
            "rot:" if !nums.is_empty() => rotation = (nums[0] as i32).rem_euclid(360),
            _ => {}
        }
    }
    let media_box = media?;
    Some(PageGeometry {
        media_box,
        crop_box: crop.unwrap_or(media_box),
        rotation,
    })
}

fn normalize_box(v: &[f64]) -> [f64; 4] {
    [
        v[0].min(v[2]),
        v[1].min(v[3]),
        v[0].max(v[2]),
        v[1].max(v[3]),
    ]
}

/// Value of `name="..."` inside one tag.
fn attr(tag: &str, name: &str) -> Option<f64> {
    let (_, rest) = tag.split_once(&format!(" {name}=\""))?;
    let (value, _) = rest.split_once('"')?;
    value.parse().ok()
}

/// Undo the five XML entities pdftotext escapes, plus numeric references.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let Some((entity, tail)) = after.split_once(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e => e
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = tail;
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse `pdftotext -bbox` XHTML into pages of words, with line and paragraph
/// numbers still unset (see [`group_lines`]). `first_index` is the 0-based
/// index of the first page in the output (`-f` minus one).
///
/// The format is regular enough for a tag scanner: each `<page>` holds
/// `<word xMin= yMin= xMax= yMax=>text</word>` elements.
pub fn parse_bbox(xhtml: &str, first_index: usize) -> Vec<PdfPage> {
    let mut pages: Vec<PdfPage> = Vec::new();
    let mut rest = xhtml;
    while let Some((_, after)) = rest.split_once('<') {
        let Some((tag, tail)) = after.split_once('>') else {
            break;
        };
        rest = tail;
        let name = tag.split_whitespace().next().unwrap_or("");
        match name {
            "page" => pages.push(PdfPage {
                index: first_index + pages.len(),
                words: Vec::new(),
            }),
            "word" => {
                let Some((text, tail)) = rest.split_once("</word>") else {
                    break;
                };
                rest = tail;
                let (Some(x0), Some(y0), Some(x1), Some(y1)) = (
                    attr(tag, "xMin"),
                    attr(tag, "yMin"),
                    attr(tag, "xMax"),
                    attr(tag, "yMax"),
                ) else {
                    continue;
                };
                if let Some(page) = pages.last_mut() {
                    page.words.push(PdfWord {
                        text: unescape(text),
                        bbox: [x0, y0, x1, y1],
                        block: 0,
                        line: 0,
                    });
                }
            }
            _ => {}
        }
    }
    pages
}

// ---- page labels ----

/// The label of a 0-based page from the catalog's `/PageLabels` number tree,
/// falling back to the 1-based page number when the PDF defines none or the
/// tree cannot be read.
pub fn page_label(pdf: &Path, page_index: usize) -> String {
    let fallback = (page_index + 1).to_string();
    let path = pdf.to_path_buf();
    // oxidize-pdf can panic on a malformed file; a label is never worth
    // failing the command over.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(move || read_label_ranges(&path));
    std::panic::set_hook(prev);
    match result {
        Ok(Some(ranges)) => label_for(&ranges, page_index).unwrap_or(fallback),
        _ => fallback,
    }
}

/// One `/PageLabels` range: it starts at `start` (0-based page index).
#[derive(Debug, Clone, PartialEq)]
pub struct LabelRange {
    pub start: usize,
    /// `D`, `R`, `r`, `A`, `a`, or `None` for prefix-only labels.
    pub style: Option<String>,
    pub prefix: String,
    /// Numeric value of the range's first page (`/St`, default 1).
    pub first: i64,
}

/// Label of `page_index` given the ranges, or `None` when no range covers it.
pub fn label_for(ranges: &[LabelRange], page_index: usize) -> Option<String> {
    let range = ranges
        .iter()
        .filter(|r| r.start <= page_index)
        .max_by_key(|r| r.start)?;
    let n = range.first + (page_index - range.start) as i64;
    let number = match range.style.as_deref() {
        Some("D") => n.to_string(),
        Some("R") => roman(n).to_uppercase(),
        Some("r") => roman(n),
        Some("A") => letters(n).to_uppercase(),
        Some("a") => letters(n),
        _ => String::new(),
    };
    Some(format!("{}{number}", range.prefix))
}

fn roman(mut n: i64) -> String {
    const TABLE: [(i64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for (value, numeral) in TABLE {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// PDF letter numbering: a..z, then aa..zz, then aaa..zzz.
fn letters(n: i64) -> String {
    if n < 1 {
        return String::new();
    }
    let letter = (b'a' + ((n - 1) % 26) as u8) as char;
    let count = ((n - 1) / 26 + 1) as usize;
    std::iter::repeat_n(letter, count).collect()
}

fn read_label_ranges(path: &Path) -> Option<Vec<LabelRange>> {
    use oxidize_pdf::parser::{PdfDocument, PdfObject, PdfReader};

    let mut reader = PdfReader::open(path).ok()?;
    let catalog = reader.catalog().ok()?.clone();
    let document = PdfDocument::new(reader);
    let root = document.resolve(catalog.get("PageLabels")?).ok()?;

    let mut ranges = Vec::new();
    let mut stack = vec![(root, 0u32)];
    while let Some((node, depth)) = stack.pop() {
        // A cycle or absurd depth in a malformed tree must not loop forever.
        if depth > 32 || ranges.len() > 10_000 {
            return None;
        }
        let dict = node.as_dict()?;
        if let Some(kids) = dict.get("Kids").and_then(|k| document.resolve(k).ok()) {
            for kid in kids.as_array()?.0.iter() {
                stack.push((document.resolve(kid).ok()?, depth + 1));
            }
        }
        let Some(nums) = dict.get("Nums").and_then(|n| document.resolve(n).ok()) else {
            continue;
        };
        for pair in nums.as_array()?.0.chunks(2) {
            let [key, value] = pair else { continue };
            let start = document.resolve(key).ok()?.as_integer()?;
            let value = document.resolve(value).ok()?;
            let label = value.as_dict()?;
            let get = |k: &str| label.get(k).and_then(|o| document.resolve(o).ok());
            ranges.push(LabelRange {
                start: usize::try_from(start).ok()?,
                style: get("S").and_then(|s| s.as_name().map(|n| n.0.clone())),
                prefix: get("P")
                    .and_then(|p| p.as_string().map(|s| decode_pdf_text(&s.0)))
                    .unwrap_or_default(),
                first: get("St")
                    .and_then(|s| match s {
                        PdfObject::Integer(i) => Some(i),
                        _ => None,
                    })
                    .unwrap_or(1),
            });
        }
    }
    Some(ranges)
}

/// A PDF text string: UTF-16BE with a byte-order mark, else PDFDocEncoding,
/// read as Latin-1 (identical for every printable label character).
fn decode_pdf_text(bytes: &[u8]) -> String {
    if let Some(utf16) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    bytes.iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x0: f64, y0: f64, block: usize, line: usize) -> PdfWord {
        let width = 6.0 * text.chars().count() as f64;
        PdfWord {
            text: text.to_string(),
            bbox: [x0, y0, x0 + width, y0 + 10.0],
            block,
            line,
        }
    }

    /// Two lines of one block, the first ending in a line-break hyphen, then
    /// a second block.
    fn sample_page() -> PdfPage {
        PdfPage {
            index: 3,
            words: vec![
                word("The", 50.0, 100.0, 1, 1),
                word("Quick,", 80.0, 100.0, 1, 1),
                word("brown", 125.0, 100.0, 1, 1),
                word("fox", 160.0, 100.0, 1, 1),
                word("jum-", 185.0, 100.0, 1, 1),
                word("ped", 50.0, 112.0, 1, 2),
                word("over", 75.0, 112.0, 1, 2),
                word("–", 105.0, 112.0, 1, 2),
                word("the", 115.0, 112.0, 1, 2),
                word("lazy", 140.0, 112.0, 1, 2),
                word("dog.", 50.0, 300.0, 2, 3),
            ],
        }
    }

    #[test]
    fn normalize_drops_case_punctuation_and_expands_ligatures() {
        assert_eq!(normalize("Quick,"), "quick");
        assert_eq!(normalize("“Eﬃcient”"), "efficient");
        assert_eq!(normalize("Müller's"), "müllers");
        assert_eq!(normalize("–"), "");
        assert_eq!(
            query_tokens("  the  QUICK, – brown "),
            ["the", "quick", "brown"]
        );
    }

    #[test]
    fn match_is_case_and_punctuation_insensitive() {
        let page = sample_page();
        let p = locate(&[page], "the quick", "brown fox", TO_WINDOW, None).unwrap();
        assert_eq!((p.page_index, p.first_word, p.last_word), (3, 0, 3));
        assert_eq!(p.text, "The Quick, brown fox");
        assert_eq!(p.char_offset, 0);
    }

    #[test]
    fn match_skips_punctuation_words_and_joins_line_break_hyphens() {
        let page = sample_page();
        // "jumped" spans two words; the lone dash is not part of the query.
        let p = locate(&[page], "fox jumped", "over the lazy", TO_WINDOW, None).unwrap();
        assert_eq!((p.first_word, p.last_word), (3, 9));
        assert_eq!(p.text, "fox jumped over – the lazy");
        // "The Quick, brown " = 4 + 7 + 6 characters with their spaces.
        assert_eq!(p.char_offset, 17);
    }

    #[test]
    fn match_bridges_different_word_splits() {
        let words: Vec<PdfWord> = [
            "Roman",
            "Kern",
            "3",
            "1",
            "Virtual",
            "EU-headquartered",
            "vendors",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| word(t, 10.0 * i as f64, 100.0, 1, 1))
        .collect();
        let page = PdfPage { index: 0, words };
        let pages = std::slice::from_ref(&page);
        // The query glues what poppler split (a superscript)...
        let p = locate(pages, "Roman Kern3 1Virtual", "vendors", TO_WINDOW, None).unwrap();
        assert_eq!((p.first_word, p.last_word), (0, 6));
        // ...or splits what the PDF holds as one word.
        let p = locate(pages, "EU headquartered", "vendors", TO_WINDOW, None).unwrap();
        assert_eq!((p.first_word, p.last_word), (5, 6));
        // A mere prefix is no match.
        assert!(locate(pages, "Kern3 1Virt", "vendors", TO_WINDOW, None).is_err());
    }

    #[test]
    fn to_must_follow_within_the_window() {
        let page = sample_page();
        let err = locate(std::slice::from_ref(&page), "lazy", "the", TO_WINDOW, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no --to"), "{err}");
        assert!(err.contains("page(s) 4"), "{err}");
        assert!(locate(std::slice::from_ref(&page), "the quick", "lazy", 3, None).is_err());
        assert!(locate(&[page], "the quick", "lazy", 8, None).is_ok());
    }

    #[test]
    fn missing_from_and_page_filter_are_reported() {
        let page = sample_page();
        let err = locate(std::slice::from_ref(&page), "zebra", "dog", TO_WINDOW, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found anywhere"), "{err}");
        let err = locate(&[page], "the quick", "fox", TO_WINDOW, Some(0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found on page 1"), "{err}");
    }

    #[test]
    fn to_overlapping_from_still_covers_from() {
        let page = sample_page();
        let p = locate(&[page], "the quick brown", "quick", TO_WINDOW, None).unwrap();
        assert_eq!((p.first_word, p.last_word), (0, 2));
    }

    #[test]
    fn rects_merge_per_line_top_first() {
        let page = sample_page();
        let rects = merge_line_rects(&page.words[3..=10]);
        assert_eq!(
            rects,
            vec![
                [160.0, 100.0, 209.0, 110.0],
                [50.0, 112.0, 164.0, 122.0],
                [50.0, 300.0, 74.0, 310.0],
            ]
        );
    }

    fn geometry(media: [f64; 4], rotation: i32) -> PageGeometry {
        PageGeometry {
            media_box: media,
            crop_box: media,
            rotation,
        }
    }

    #[test]
    fn device_to_user_flips_y_and_honours_the_media_box_origin() {
        let g = geometry([0.0, 0.0, 612.0, 792.0], 0);
        assert_eq!(
            g.device_to_user([54.0, 64.0, 100.0, 74.0]),
            [54.0, 718.0, 100.0, 728.0]
        );
        let g = geometry([10.0, 20.0, 622.0, 812.0], 0);
        assert_eq!(
            g.device_to_user([54.0, 64.0, 100.0, 74.0]),
            [64.0, 738.0, 110.0, 748.0]
        );
    }

    #[test]
    fn device_to_user_inverts_each_rotation() {
        // A point at user (100, 700) on a 612 x 792 page, as poppler places it.
        let media = [0.0, 0.0, 612.0, 792.0];
        let cases = [
            (0, (100.0, 92.0)),
            (90, (700.0, 100.0)),
            (180, (512.0, 700.0)),
            (270, (92.0, 512.0)),
        ];
        for (rotation, (xd, yd)) in cases {
            let g = geometry(media, rotation);
            assert_eq!(
                g.device_to_user([xd, yd, xd, yd]),
                [100.0, 700.0, 100.0, 700.0],
                "rotation {rotation}"
            );
        }
    }

    #[test]
    fn sort_index_is_zero_padded() {
        let g = geometry([0.0, 0.0, 612.0, 792.0], 0);
        let top = g.sort_top(&[[54.0, 718.0, 100.0, 728.0], [54.0, 700.0, 90.0, 710.0]]);
        assert_eq!(top, 64);
        assert_eq!(sort_index(2, 1234, top), "00002|001234|00064");
        assert_eq!(g.sort_top(&[[0.0, 790.0, 1.0, 800.0]]), 0);
    }

    #[test]
    fn parse_bbox_reads_pages_words_and_entities() {
        let xhtml = r#"<doc>
  <page width="612.000000" height="792.000000">
    <word xMin="54.000000" yMin="54.381000" xMax="88.074000" yMax="64.344000">R&amp;D</word>
    <word xMin="91.647000" yMin="54.381000" xMax="102.078000" yMax="64.344000">&lt;for&gt;</word>
  </page>
  <page width="612.000000" height="792.000000">
  </page>
</doc>"#;
        let pages = parse_bbox(xhtml, 4);
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].index, pages[1].index), (4, 5));
        let w = &pages[0].words;
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].text, "R&D");
        assert_eq!(w[1].text, "<for>");
        assert_eq!(w[0].bbox, [54.0, 54.381, 88.074, 64.344]);
        assert!(pages[1].words.is_empty());
    }

    #[test]
    fn group_lines_splits_wraps_columns_and_paragraphs() {
        let at = |x0: f64, y0: f64, x1: f64| PdfWord {
            text: "w".into(),
            bbox: [x0, y0, x1, y0 + 10.0],
            block: 99,
            line: 99,
        };
        let mut words = vec![
            at(50.0, 100.0, 80.0),
            // A raised superscript still belongs to the line.
            at(81.0, 97.0, 85.0),
            at(90.0, 100.0, 120.0),
            // Wrap to the next line of the same paragraph.
            at(50.0, 111.0, 80.0),
            // Same height, but past a column gap: a new line and paragraph.
            at(300.0, 111.0, 330.0),
            // Back to the left after a blank line: a new paragraph.
            at(50.0, 140.0, 80.0),
        ];
        group_lines(&mut words);
        let lines: Vec<usize> = words.iter().map(|w| w.line).collect();
        let blocks: Vec<usize> = words.iter().map(|w| w.block).collect();
        assert_eq!(lines, [0, 0, 0, 1, 2, 3]);
        assert_eq!(blocks, [0, 0, 0, 0, 1, 2]);
    }

    #[test]
    fn parse_pdfinfo_boxes_picks_the_page() {
        let info = "Pages:           8\n\
                    Page    2 size:  612 x 792 pts (letter)\n\
                    Page    2 rot:   90\n\
                    Page    2 MediaBox:      0.00     0.00   612.00   792.00\n\
                    Page    2 CropBox:      10.00    20.00   600.00   780.00\n";
        let g = parse_pdfinfo_boxes(info, 2).unwrap();
        assert_eq!(g.media_box, [0.0, 0.0, 612.0, 792.0]);
        assert_eq!(g.crop_box, [10.0, 20.0, 600.0, 780.0]);
        assert_eq!(g.rotation, 90);
        assert!(parse_pdfinfo_boxes(info, 3).is_none());
    }

    #[test]
    fn page_labels_follow_the_ranges() {
        let ranges = vec![
            LabelRange {
                start: 0,
                style: Some("r".into()),
                prefix: String::new(),
                first: 1,
            },
            LabelRange {
                start: 4,
                style: Some("D".into()),
                prefix: String::new(),
                first: 1,
            },
            LabelRange {
                start: 10,
                style: Some("A".into()),
                prefix: "App-".into(),
                first: 27,
            },
        ];
        assert_eq!(label_for(&ranges, 0).as_deref(), Some("i"));
        assert_eq!(label_for(&ranges, 3).as_deref(), Some("iv"));
        assert_eq!(label_for(&ranges, 4).as_deref(), Some("1"));
        assert_eq!(label_for(&ranges, 9).as_deref(), Some("6"));
        assert_eq!(label_for(&ranges, 10).as_deref(), Some("App-AA"));
        assert_eq!(roman(1994), "mcmxciv");
    }
}
