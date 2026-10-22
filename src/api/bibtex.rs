//! BibTeX to Zotero item JSON, for `zot add` when the desktop app is closed.
//!
//! With the app running, the connector hands BibTeX to Zotero's own import
//! translator. Without it the item is created through the web API, which takes
//! Zotero JSON, so this module does the translator's job for the entries
//! `resolve::fetch_bibtex` produces: doi.org (Crossref, DataCite, mEDRA),
//! arxiv.org, and the entries built here from MEDLINE and OpenLibrary.
//!
//! The mapping is deliberately generous with candidate fields; the caller
//! drops whatever the target item type does not carry, using the item
//! template from the web API (see [`fit_to_template`]).

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// One parsed BibTeX entry: lowercase type, citation key, and fields with
/// lowercase names and raw (still LaTeX) values.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: String,
    pub key: String,
    pub fields: Vec<(String, String)>,
}

impl Entry {
    fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    }

    /// A field with LaTeX decoded and whitespace collapsed.
    fn text(&self, name: &str) -> Option<String> {
        self.get(name).map(delatex).filter(|v| !v.is_empty())
    }
}

/// Parse the first entry of a BibTeX body, skipping `@comment`, `@preamble`
/// and `@string` blocks.
pub fn parse_first_entry(body: &str) -> Result<Entry> {
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        i += 1;
        let start = i;
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        let kind: String = chars[start..i].iter().collect::<String>().to_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || (chars[i] != '{' && chars[i] != '(') {
            continue;
        }
        let close = if chars[i] == '{' { '}' } else { ')' };
        i += 1;
        if matches!(kind.as_str(), "comment" | "preamble" | "string") {
            i = skip_balanced(&chars, i, close);
            continue;
        }
        return parse_body(&chars, i, close, kind);
    }
    bail!("No BibTeX entry found")
}

/// Index just past the delimiter closing a block opened before `i`.
fn skip_balanced(chars: &[char], mut i: usize, close: char) -> usize {
    let mut depth = 0usize;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            c if c == close && depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

fn parse_body(chars: &[char], mut i: usize, close: char, kind: String) -> Result<Entry> {
    let skip_ws = |i: &mut usize| {
        while *i < chars.len() && chars[*i].is_whitespace() {
            *i += 1;
        }
    };
    skip_ws(&mut i);
    let start = i;
    while i < chars.len() && chars[i] != ',' && chars[i] != close {
        i += 1;
    }
    let key = chars[start..i].iter().collect::<String>().trim().to_string();
    let mut fields = Vec::new();
    loop {
        skip_ws(&mut i);
        if i < chars.len() && chars[i] == ',' {
            i += 1;
            skip_ws(&mut i);
        }
        if i >= chars.len() || chars[i] == close {
            break;
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ',' && chars[i] != close {
            i += 1;
        }
        let name = chars[start..i]
            .iter()
            .collect::<String>()
            .trim()
            .to_lowercase();
        if i >= chars.len() || chars[i] != '=' {
            continue;
        }
        i += 1;
        let mut value = String::new();
        loop {
            skip_ws(&mut i);
            if i >= chars.len() {
                break;
            }
            match chars[i] {
                '{' => {
                    let end = skip_balanced(chars, i + 1, '}');
                    value.extend(&chars[i + 1..end.saturating_sub(1)]);
                    i = end;
                }
                '"' => {
                    let mut j = i + 1;
                    let mut depth = 0usize;
                    while j < chars.len() && !(chars[j] == '"' && depth == 0) {
                        match chars[j] {
                            '{' => depth += 1,
                            '}' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                        j += 1;
                    }
                    value.extend(&chars[i + 1..j.min(chars.len())]);
                    i = (j + 1).min(chars.len());
                }
                _ => {
                    let start = i;
                    while i < chars.len()
                        && !chars[i].is_whitespace()
                        && chars[i] != ','
                        && chars[i] != '#'
                        && chars[i] != close
                    {
                        i += 1;
                    }
                    let token: String = chars[start..i].iter().collect();
                    value.push_str(&expand_macro(&token));
                }
            }
            skip_ws(&mut i);
            if i < chars.len() && chars[i] == '#' {
                i += 1;
                continue;
            }
            break;
        }
        if !name.is_empty() {
            fields.push((name, value));
        }
    }
    if fields.is_empty() {
        bail!("BibTeX entry {key} has no fields");
    }
    Ok(Entry { kind, key, fields })
}

/// The month macros BibTeX predefines; anything else is kept verbatim.
fn expand_macro(token: &str) -> String {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = token.to_lowercase();
    match MONTHS.iter().position(|m| *m == lower) {
        Some(n) => format!("{}", n + 1),
        None => token.to_string(),
    }
}

/// Decode the LaTeX found in resolver BibTeX into plain Unicode text.
pub fn delatex(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                let next = chars[i + 1];
                if "&%$#_{}".contains(next) {
                    out.push(next);
                    i += 2;
                    continue;
                }
                if "'`^\"~=.".contains(next) {
                    // \'e, \'{e}, \'{\i}
                    let (base, used) = accent_argument(&chars, i + 2);
                    match base.and_then(|b| accented(next, b)) {
                        Some(ch) => out.push(ch),
                        None => out.extend(base),
                    }
                    i += 2 + used;
                    continue;
                }
                // A control word.
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let word: String = chars[start..j].iter().collect();
                if word.is_empty() {
                    // `\ ` or `\\`: a space or a line break.
                    out.push(' ');
                    i += 2;
                    continue;
                }
                if matches!(word.as_str(), "c" | "v" | "u" | "H" | "r" | "k" | "d" | "b") {
                    let (base, used) = accent_argument(&chars, j);
                    let accent = match word.as_str() {
                        "c" => ',',
                        "v" => 'v',
                        "u" => 'u',
                        "H" => 'H',
                        "r" => 'r',
                        "k" => 'k',
                        _ => ' ',
                    };
                    match base.and_then(|b| accented(accent, b)) {
                        Some(ch) => out.push(ch),
                        None => out.extend(base),
                    }
                    i = j + used;
                    continue;
                }
                // A control word swallows the space after it, as in TeX.
                i = j;
                if chars.get(i) == Some(&' ') {
                    i += 1;
                }
                // Unknown words are formatting commands (\emph, \textit,
                // \mathrm, ...): drop the command and keep the argument,
                // whose braces the stripping below removes.
                if let Some(rep) = control_word(&word) {
                    out.push_str(rep);
                }
                continue;
            }
            '{' | '}' | '$' => i += 1,
            '~' => {
                out.push(' ');
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                if chars.get(i + 2) == Some(&'-') {
                    out.push('\u{2014}');
                    i += 3;
                } else {
                    out.push('\u{2013}');
                    i += 2;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Crossref BibTeX sometimes carries HTML entities from the deposited
    // XML (`Knowledge Discovery &amp; Data Mining`).
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The letter an accent command applies to, and how many chars it took:
/// `e`, `{e}`, `{\i}` or ` e`.
fn accent_argument(chars: &[char], i: usize) -> (Option<char>, usize) {
    let mut j = i;
    while j < chars.len() && chars[j] == ' ' {
        j += 1;
    }
    match chars.get(j) {
        Some('{') => {
            let end = skip_balanced(chars, j + 1, '}');
            let inner: String = chars[j + 1..end.saturating_sub(1)].iter().collect();
            let base = match inner.trim() {
                "\\i" => Some('i'),
                "\\j" => Some('j'),
                other => other.chars().next(),
            };
            (base, end - i)
        }
        Some('\\') if chars.get(j + 1) == Some(&'i') => (Some('i'), j + 2 - i),
        Some(&c) => (Some(c), j + 1 - i),
        None => (None, j - i),
    }
}

/// Precomposed character for `accent` over `base`, covering the Latin letters
/// that occur in author names and titles.
fn accented(accent: char, base: char) -> Option<char> {
    let (plain, marked) = match accent {
        '\'' => ("aeiouyAEIOUYcnszlrgCNSZLRG", "áéíóúýÁÉÍÓÚÝćńśźĺŕǵĆŃŚŹĹŔǴ"),
        '`' => ("aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
        '^' => ("aeiouAEIOUcgshjwyCGSHJWY", "âêîôûÂÊÎÔÛĉĝŝĥĵŵŷĈĜŜĤĴŴŶ"),
        '"' => ("aeiouyAEIOUY", "äëïöüÿÄËÏÖÜŸ"),
        '~' => ("anoANOiuIU", "ãñõÃÑÕĩũĨŨ"),
        '=' => ("aeiouAEIOU", "āēīōūĀĒĪŌŪ"),
        '.' => ("zZeEgGI", "żŻėĖġĠİ"),
        ',' => ("cCsStT", "çÇşŞţŢ"),
        'v' => ("csznrdteCSZNRDTE", "čšžňřďťěČŠŽŇŘĎŤĚ"),
        'u' => ("agAG", "ăğĂĞ"),
        'H' => ("ouOU", "őűŐŰ"),
        'r' => ("auAU", "åůÅŮ"),
        'k' => ("aeAE", "ąęĄĘ"),
        _ => return None,
    };
    let idx = plain.chars().position(|p| p == base)?;
    marked.chars().nth(idx)
}

fn control_word(word: &str) -> Option<&'static str> {
    Some(match word {
        "ss" => "ß",
        "o" => "ø",
        "O" => "Ø",
        "ae" => "æ",
        "AE" => "Æ",
        "oe" => "œ",
        "OE" => "Œ",
        "aa" => "å",
        "AA" => "Å",
        "l" => "ł",
        "L" => "Ł",
        "i" => "ı",
        "textendash" => "\u{2013}",
        "textemdash" => "\u{2014}",
        "textquoteright" | "textquotesingle" => "'",
        "textquoteleft" => "'",
        "textquotedblleft" | "textquotedblright" => "\"",
        "textregistered" => "®",
        "texttrademark" => "™",
        "textcopyright" => "©",
        "textdegree" => "°",
        "textbackslash" => "\\",
        "ldots" | "dots" | "textellipsis" => "…",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "mu" => "μ",
        "pm" => "±",
        "times" => "×",
        "&" => "&",
        _ => return None,
    })
}

/// Split a BibTeX name list on top-level ` and `.
fn split_names(raw: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    let words: Vec<&str> = raw.split_whitespace().collect();
    for word in words {
        if depth == 0 && word.eq_ignore_ascii_case("and") && !current.is_empty() {
            names.push(std::mem::take(&mut current));
            continue;
        }
        for c in word.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        names.push(current);
    }
    names
}

/// Split at top-level commas.
fn split_commas(raw: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut depth = 0usize;
    for c in raw.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(String::new());
                continue;
            }
            _ => {}
        }
        parts.last_mut().expect("never empty").push(c);
    }
    parts.into_iter().map(|p| p.trim().to_string()).collect()
}

/// One BibTeX name as a Zotero creator.
fn creator(raw: &str, creator_type: &str) -> Value {
    let trimmed = raw.trim();
    // `{World Health Organization}`: a corporate name, kept whole.
    if trimmed.starts_with('{') && skip_balanced(&trimmed.chars().collect::<Vec<_>>(), 1, '}') == trimmed.chars().count() {
        return json!({ "creatorType": creator_type, "name": delatex(trimmed) });
    }
    let parts = split_commas(trimmed);
    let (first, last) = match parts.len() {
        1 => {
            let words: Vec<&str> = trimmed.split_whitespace().collect();
            if words.len() == 1 {
                return json!({ "creatorType": creator_type, "name": delatex(trimmed) });
            }
            // "First von Last": the last name starts at the first lowercase
            // word after the first, else it is the final word.
            let split = (1..words.len() - 1)
                .find(|&k| words[k].chars().next().is_some_and(char::is_lowercase))
                .unwrap_or(words.len() - 1);
            (words[..split].join(" "), words[split..].join(" "))
        }
        // "Last, First" and "Last, Jr, First".
        2 => (parts[1].clone(), parts[0].clone()),
        _ => (parts[2..].join(" "), format!("{}, {}", parts[0], parts[1])),
    };
    json!({
        "creatorType": creator_type,
        "firstName": delatex(&first),
        "lastName": delatex(&last),
    })
}

/// `YYYY-MM` when the month is known, else the year, else whatever `date`
/// holds.
fn date(entry: &Entry) -> Option<String> {
    if let Some(d) = entry.text("date") {
        return Some(d);
    }
    let year = entry.text("year")?;
    let month = entry.text("month").and_then(|m| {
        if let Ok(n) = m.parse::<u32>() {
            return (1..=12).contains(&n).then_some(n);
        }
        let lower = m.to_lowercase();
        const NAMES: [&str; 12] = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        NAMES
            .iter()
            .position(|n| lower.starts_with(n))
            .map(|p| p as u32 + 1)
    });
    Some(match month {
        Some(m) => format!("{year}-{m:02}"),
        None => year,
    })
}

/// The arXiv identifier of an entry, when it is an arXiv record.
fn arxiv_id(entry: &Entry) -> Option<String> {
    let prefix = entry
        .text("archiveprefix")
        .or_else(|| entry.text("eprinttype"))
        .unwrap_or_default()
        .to_lowercase();
    if prefix == "arxiv" {
        if let Some(e) = entry.text("eprint") {
            return Some(e.trim_start_matches("arXiv:").to_string());
        }
    }
    let doi = entry.text("doi").unwrap_or_default();
    let prefix = "10.48550/arxiv.";
    doi.get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .and_then(|_| doi.get(prefix.len()..))
        .map(String::from)
}

/// Map a parsed entry to Zotero item JSON with every candidate field set; run
/// it through [`fit_to_template`] before posting.
pub fn to_zotero(entry: &Entry) -> Value {
    let arxiv = arxiv_id(entry);
    let item_type = match entry.kind.as_str() {
        _ if arxiv.is_some() && entry.text("journal").is_none() => "preprint",
        "article" => "journalArticle",
        "inproceedings" | "conference" | "proceedings" => "conferencePaper",
        "book" => "book",
        "inbook" | "incollection" => "bookSection",
        "phdthesis" | "mastersthesis" | "thesis" => "thesis",
        "techreport" | "report" => "report",
        _ => "document",
    };

    let mut data = Map::new();
    data.insert("itemType".into(), json!(item_type));
    let mut set = |name: &str, value: Option<String>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            data.entry(name.to_string()).or_insert(json!(v));
        }
    };
    set("title", entry.text("title"));
    set("abstractNote", entry.text("abstract"));
    set("date", date(entry));
    set("url", entry.text("url"));
    set("DOI", entry.text("doi"));
    set("ISBN", entry.text("isbn"));
    set("ISSN", entry.text("issn"));
    set("volume", entry.text("volume"));
    set("publisher", entry.text("publisher"));
    set("place", entry.text("address").or_else(|| entry.text("location")));
    set("series", entry.text("series"));
    set("edition", entry.text("edition"));
    set("language", entry.text("language"));
    let pages = entry.text("pages");
    match item_type {
        "journalArticle" => {
            set("publicationTitle", entry.text("journal").or_else(|| entry.text("journaltitle")));
            set("issue", entry.text("number").or_else(|| entry.text("issue")));
            set("pages", pages);
        }
        "conferencePaper" => {
            set("proceedingsTitle", entry.text("booktitle"));
            set("conferenceName", entry.text("eventtitle"));
            set("pages", pages);
        }
        "bookSection" => {
            set("bookTitle", entry.text("booktitle"));
            set("pages", pages);
        }
        "book" => {
            set("seriesNumber", entry.text("number"));
            set("numPages", pages);
        }
        "thesis" => {
            set("university", entry.text("school").or_else(|| entry.text("institution")));
            set(
                "thesisType",
                Some(match entry.kind.as_str() {
                    "phdthesis" => "PhD thesis".to_string(),
                    "mastersthesis" => "Master's thesis".to_string(),
                    _ => entry.text("type").unwrap_or_default(),
                }),
            );
        }
        "report" => {
            set("institution", entry.text("institution"));
            set("reportNumber", entry.text("number"));
            set("reportType", entry.text("type"));
            set("pages", pages);
        }
        "preprint" => {
            set("repository", Some("arXiv".to_string()));
            set("archiveID", arxiv.as_ref().map(|id| format!("arXiv:{id}")));
            if data.get("DOI").is_none() {
                if let Some(id) = &arxiv {
                    data.insert("DOI".into(), json!(format!("10.48550/arXiv.{id}")));
                }
            }
            if data.get("url").is_none() {
                if let Some(id) = &arxiv {
                    data.insert("url".into(), json!(format!("https://arxiv.org/abs/{id}")));
                }
            }
        }
        _ => {}
    }
    // `note` carries `PMID: n` from the MEDLINE path; Zotero keeps such
    // lines in Extra, which is also what the duplicate guard reads.
    if let Some(note) = entry.text("note") {
        data.insert("extra".into(), json!(note));
    }

    // Theses and reports have no editor creator type; Zotero rejects the
    // item outright over an invalid one, so those editors become
    // contributors.
    let editor = if matches!(item_type, "thesis" | "report") {
        "contributor"
    } else {
        "editor"
    };
    let mut creators = Vec::new();
    for (field, kind) in [("author", "author"), ("editor", editor)] {
        if let Some(raw) = entry.get(field) {
            creators.extend(split_names(raw).iter().map(|n| creator(n, kind)));
        }
    }
    data.insert("creators".into(), Value::Array(creators));
    Value::Object(data)
}

/// Keep only the fields the item type has, according to its web API
/// template. A DOI, ISBN or ISSN the type lacks goes into Extra as
/// `DOI: ...`, Zotero's own convention, so it is not lost; any other unknown
/// field is dropped.
pub fn fit_to_template(mut item: Value, template: &Value) -> Result<Value> {
    let allowed = template
        .as_object()
        .context("Item template is not an object")?;
    let data = item.as_object_mut().context("Item is not an object")?;
    let mut extra_lines: Vec<String> = Vec::new();
    let keys: Vec<String> = data.keys().cloned().collect();
    for key in keys {
        if allowed.contains_key(&key) {
            continue;
        }
        if let Some(v) = data.remove(&key) {
            if matches!(key.as_str(), "DOI" | "ISBN" | "ISSN") {
                if let Some(s) = v.as_str() {
                    extra_lines.push(format!("{key}: {s}"));
                }
            }
        }
    }
    if !extra_lines.is_empty() {
        let existing = data
            .get("extra")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut lines = extra_lines;
        if !existing.is_empty() {
            lines.push(existing);
        }
        data.insert("extra".into(), json!(lines.join("\n")));
    }
    Ok(item)
}

/// Parse BibTeX and produce Zotero item JSON in one step.
pub fn bibtex_to_zotero(body: &str) -> Result<Value> {
    let entry = parse_first_entry(body)?;
    Ok(to_zotero(&entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_crossref_entry() {
        let bib = "@article{Smith_2020, title={A {GPU} study of {\\\"u}ber-fast \\& small things}, \
                   volume={12}, ISSN={1234-5678}, url={http://dx.doi.org/10.1/x}, DOI={10.1/x}, \
                   number={3}, journal={Journal of Things}, publisher={Elsevier BV}, \
                   author={Smith, John and M{\\\"u}ller, J{\\\"o}rg and van der Berg, Anna}, \
                   year={2020}, month=jan, pages={1--10} }";
        let item = bibtex_to_zotero(bib).unwrap();
        assert_eq!(item["itemType"], "journalArticle");
        assert_eq!(item["title"], "A GPU study of über-fast & small things");
        assert_eq!(item["publicationTitle"], "Journal of Things");
        assert_eq!(item["issue"], "3");
        assert_eq!(item["pages"], "1\u{2013}10");
        assert_eq!(item["date"], "2020-01");
        assert_eq!(item["DOI"], "10.1/x");
        assert_eq!(item["creators"][1]["lastName"], "Müller");
        assert_eq!(item["creators"][1]["firstName"], "Jörg");
        assert_eq!(item["creators"][2]["lastName"], "van der Berg");
        assert_eq!(item["creators"][2]["firstName"], "Anna");
    }

    #[test]
    fn an_arxiv_misc_entry_becomes_a_preprint() {
        let bib = "@misc{vaswani2017attention,\n      title={Attention Is All You Need},\n      \
                   author={Ashish Vaswani and Noam Shazeer and Aidan N. Gomez},\n      \
                   year={2017},\n      eprint={1706.03762},\n      archivePrefix={arXiv},\n      \
                   primaryClass={cs.CL},\n      url={https://arxiv.org/abs/1706.03762},\n}";
        let item = bibtex_to_zotero(bib).unwrap();
        assert_eq!(item["itemType"], "preprint");
        assert_eq!(item["archiveID"], "arXiv:1706.03762");
        assert_eq!(item["repository"], "arXiv");
        assert_eq!(item["DOI"], "10.48550/arXiv.1706.03762");
        assert_eq!(item["creators"][2]["firstName"], "Aidan N.");
        assert_eq!(item["creators"][2]["lastName"], "Gomez");
    }

    #[test]
    fn a_datacite_arxiv_doi_entry_becomes_a_preprint() {
        let bib = "@misc{https://doi.org/10.48550/arxiv.2401.12345, doi = {10.48550/ARXIV.2401.12345}, \
                   author = {Doe, Jane}, title = {Thing}, publisher = {arXiv}, year = {2024}}";
        let item = bibtex_to_zotero(bib).unwrap();
        assert_eq!(item["itemType"], "preprint");
        assert_eq!(item["archiveID"], "arXiv:2401.12345");
    }

    #[test]
    fn corporate_and_single_names_stay_whole() {
        let bib = "@book{x, title={T}, author={{World Health Organization} and Plato}, \
                   publisher={WHO}, year={2019}, pages={300}, isbn={9781119287537}}";
        let item = bibtex_to_zotero(bib).unwrap();
        assert_eq!(item["itemType"], "book");
        assert_eq!(item["creators"][0]["name"], "World Health Organization");
        assert_eq!(item["creators"][1]["name"], "Plato");
        assert_eq!(item["numPages"], "300");
    }

    #[test]
    fn quoted_values_concatenation_and_comments_parse() {
        let bib = "@comment{ignore me}\n@inproceedings(k, title = \"Deep \" # {Nets}, \
                   booktitle = {Proc. of {X}}, author = \"Lee, Kim\", year = 2021)";
        let entry = parse_first_entry(bib).unwrap();
        assert_eq!(entry.kind, "inproceedings");
        let item = to_zotero(&entry);
        assert_eq!(item["title"], "Deep Nets");
        assert_eq!(item["proceedingsTitle"], "Proc. of X");
        assert_eq!(item["date"], "2021");
    }

    #[test]
    fn accents_in_every_spelling_decode() {
        assert_eq!(delatex("{\\'e}t{\\'{e}} \\c{c}a \\v{S}koda Ma{\\ss}e {\\o}"), "été ça Škoda Maße ø");
        assert_eq!(delatex("na\\\"{\\i}ve \\textendash{} x"), "naïve \u{2013} x");
        assert_eq!(delatex("Discovery &amp; Data"), "Discovery & Data");
    }

    /// Live check against the real resolvers: every entry type they produce
    /// converts to an item whose fields all exist in the web API template.
    /// `cargo test -- --ignored live_resolver_entries` (network, API key).
    #[test]
    #[ignore]
    fn live_resolver_entries_fit_their_templates() {
        use crate::api::WebApiClient;
        use crate::api::resolve::{fetch_bibtex, parse_identifier};
        let web = WebApiClient::from_config().unwrap();
        for id in [
            "10.1016/j.autcon.2023.104812",
            "10.1007/978-3-030-51295-8_5",
            "10.1145/3292500.3330701",
            "arXiv:1706.03762",
            "9781119287537",
            "23193287",
        ] {
            let bib = fetch_bibtex(&parse_identifier(id).unwrap()).unwrap();
            let item = bibtex_to_zotero(&bib).unwrap();
            let template = web
                .item_template(item["itemType"].as_str().unwrap())
                .unwrap();
            let fitted = fit_to_template(item.clone(), &template).unwrap();
            println!("{id}\n  {fitted}\n");
            assert!(!fitted["title"].as_str().unwrap_or_default().is_empty(), "{id}");
            assert!(fitted["title"].as_str().unwrap().chars().all(|c| c != '{' && c != '\\'), "{id}");
            assert!(!fitted["creators"].as_array().unwrap().is_empty(), "{id}");
        }
    }

    #[test]
    fn fields_the_type_lacks_are_dropped_or_moved_to_extra() {
        let item = json!({ "itemType": "bookSection", "title": "T", "DOI": "10.1/x",
                           "ISSN": "1", "bogus": "y", "extra": "PMID: 1" });
        let template = json!({ "itemType": "bookSection", "title": "", "extra": "", "creators": [] });
        let fitted = fit_to_template(item, &template).unwrap();
        assert!(fitted.get("bogus").is_none());
        assert!(fitted.get("DOI").is_none());
        assert_eq!(fitted["extra"], "DOI: 10.1/x\nISSN: 1\nPMID: 1");
    }
}
