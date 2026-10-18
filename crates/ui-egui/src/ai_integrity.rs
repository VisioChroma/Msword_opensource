//! "Word" AI, part 6: citation, plagiarism and research-integrity assistance.
//!
//! Three checks, each started by its own button in the "Citations and integrity" section of the
//! Word panel. Nothing in the document is ever changed by this module.
//!
//! 1. **Check citations.** Without any AI: every `[1]`, `[1, 3-5]` and `(Author, 2020)` marker is
//!    matched against the reference list (a missing entry, a reference that is never cited,
//!    numbering out of order, duplicate entries, a marker before the full stop in some places and
//!    after it in others). A few plain rules also point at sentences with a figure or an "according
//!    to" and no citation. With AI: the model proposes sentences that need a citation or are
//!    sweeping claims without support. Every proposal must quote one sentence of the document word
//!    for word; anything that does not is dropped and counted, never shown.
//! 2. **Verify references.** Each reference is looked up in Crossref (free, no key). A DOI that does
//!    not exist anywhere, a DOI that belongs to a different work, or an entry that matches no
//!    record is flagged. "Not found" is NOT proof of fabrication (books, reports and web pages are
//!    often missing from Crossref), and the panel says so.
//! 3. **Check similarity.** Honest scope: an LLM cannot detect plagiarism by itself. This check
//!    compares long passages with (a) a folder of source files the author provides and (b)
//!    optionally web pages found by a Google-grounded search, which WordCraft downloads and
//!    compares itself. The model only proposes pages; the match is measured by WordCraft. It is not
//!    a replacement for a service such as Turnitin: matches are flagged for the author to review.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{Color32, RichText};
use serde_json::{Value, json};
use wordcraft_engine::Session;

use crate::WordApp;
use crate::ai_net;
use crate::ai_plan::extract_json;
use crate::theme::Tokens;

const MAX_PARAS: usize = 2_000;
const MAX_FINDINGS: usize = 40;
const MAX_REFS: usize = 40;
const MAX_LOCAL_CLAIMS: usize = 12;
const MAX_AI_CHUNKS: usize = 5;
const CHUNK_CHARS: usize = 10_000;
const MAX_QUOTE: usize = 300;
const MAX_WEB_PASSAGES: usize = 5;
const MAX_CORPUS_FILES: usize = 200;
const SHINGLE: usize = 6;
const CORPUS_MATCH: f32 = 0.5;
const WEB_MATCH: f32 = 0.35;

pub const CLAIMS_PROMPT: &str = r#"You are Word, the citation assistant inside the WordCraft document editor.
You receive part of a document (its reference list is not included). Find the places where a reader would ask "says who?".

Reply with ONE JSON object and nothing else (no markdown fences, no commentary):
{"claims": [{"kind": "needs_citation" | "unsupported", "quote": "...", "reason": "one short sentence"}]}

Kinds:
- needs_citation: a factual statement (a statistic, a date, a finding, a definition taken from others, a statement about what research or experts say) that the author did not produce themselves and that has no citation marker such as [3] or (Smith, 2020) in or right after the sentence.
- unsupported: a strong or sweeping claim stated as fact ("always", "proves", "everyone knows", a causal claim) with neither a citation nor any evidence in the text you were given.

Rules:
- "quote" is ONE sentence copied word for word from the text (under 250 characters). Never paraphrase or edit it.
- Do not judge whether a claim is true. Judge only whether the document supports it.
- Skip opinions clearly marked as the author's, the author's own methods and results, common knowledge, headings, and sentences that already carry a citation.
- Never suggest a source, author, title, DOI or URL. Do not rewrite the text.
- An empty list is a correct answer. At most 12 items, most important first."#;

// ---------------------------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sev {
    High,
    Medium,
    Info,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    MissingEntry,
    Uncited,
    NoList,
    Placement,
    Order,
    Duplicate,
    NeedsCitation,
    Unsupported,
    NotFound,
    WrongWork,
    Unverified,
    Similar,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::MissingEntry => "Citation without a reference",
            Kind::Uncited => "Reference never cited",
            Kind::NoList => "No reference list",
            Kind::Placement => "Citation placement",
            Kind::Order => "Citation order",
            Kind::Duplicate => "Duplicate reference",
            Kind::NeedsCitation => "May need a citation",
            Kind::Unsupported => "Unsupported claim",
            Kind::NotFound => "Reference may not exist",
            Kind::WrongWork => "DOI belongs to another work",
            Kind::Unverified => "Could not verify",
            Kind::Similar => "Matching passage",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub kind: Kind,
    pub sev: Sev,
    pub detail: String,
    /// An exact piece of the document, so "Show" can jump to it.
    pub quote: Option<String>,
    pub url: Option<String>,
}

fn finding(kind: Kind, sev: Sev, detail: impl Into<String>, quote: Option<String>) -> Finding {
    Finding { kind, sev, detail: detail.into(), quote, url: None }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// AI proposals thrown away because their quotes are not in the document.
    pub dropped: usize,
    pub notes: Vec<String>,
    pub label: String,
    pub checked: usize,
    pub confirmed: usize,
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn fnv(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn clip_head(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The first `n` characters: always an exact piece of the text, so it can be found again.
fn lead(p: &str, n: usize) -> String {
    p.trim().chars().take(n).collect()
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

fn shingles(w: &[String]) -> BTreeSet<u64> {
    w.windows(SHINGLE).map(|win| fnv(&win.join(" "))).collect()
}

/// How much of `a` is found in `b` (0.0 to 1.0).
fn containment(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> f32 {
    if a.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f32 / a.len() as f32
}

/// The share of the title's words (3+ letters) that the entry contains.
fn coverage(title: &str, entry: &str) -> f32 {
    let t: Vec<String> = words(title).into_iter().filter(|w| w.chars().count() >= 3).collect();
    if t.is_empty() {
        return 0.0;
    }
    let e: BTreeSet<String> = words(entry).into_iter().collect();
    t.iter().filter(|w| e.contains(*w)).count() as f32 / t.len() as f32
}

fn window(p: &str, from: usize, to: usize) -> String {
    p.chars().skip(from).take(to.saturating_sub(from)).collect()
}

fn amber() -> Color32 {
    Color32::from_rgb(214, 160, 20)
}

// ---------------------------------------------------------------------------------------------
// Reading the document: body text and the reference list
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct DocText {
    /// Every paragraph and table cell before the reference list.
    pub body: Vec<String>,
    /// One entry per paragraph of the reference list.
    pub refs: Vec<String>,
    pub found_refs_heading: bool,
}

fn is_refs_title(t: &str) -> bool {
    let t = t.trim().trim_end_matches(':').trim().to_lowercase();
    ["references", "reference list", "list of references", "bibliography", "works cited", "literature cited", "sources cited"].contains(&t.as_str())
}

fn read_doc(s: &Session) -> DocText {
    let mut out = DocText::default();
    let mut in_refs = false;
    for b in &s.doc.body {
        if let Some(table) = b.as_table() {
            if !in_refs {
                for row in &table.rows {
                    for cell in &row.cells {
                        for cb in &cell.blocks {
                            if let Some(cp) = cb.as_para() {
                                let tx = cp.text.trim();
                                if !tx.is_empty() {
                                    out.body.push(tx.to_string());
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }
        let Some(p) = b.as_para() else { continue };
        let text = p.text.trim();
        if text.is_empty() {
            continue;
        }
        let style = p.props.style.as_deref().unwrap_or("Normal");
        let is_heading = style.starts_with("Heading") || style == "Title";
        if is_refs_title(text) && (is_heading || text.chars().count() <= 24) {
            in_refs = true;
            out.found_refs_heading = true;
            continue;
        }
        if in_refs {
            if is_heading {
                in_refs = false;
            } else {
                out.refs.push(text.to_string());
                continue;
            }
        }
        out.body.push(text.to_string());
    }
    out
}

fn quote_found(quote: &str, paras: &[String]) -> bool {
    let q = quote.trim();
    !q.is_empty() && q.chars().count() <= MAX_QUOTE && paras.iter().any(|p| p.contains(q))
}

// ---------------------------------------------------------------------------------------------
// Finding citation markers: [3], [1, 3-5], (Smith, 2020), Smith (2020)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Cite {
    Num(usize),
    Author { surname: String, year: String },
}

#[derive(Clone, Debug)]
struct Mark {
    cite: Cite,
    /// Character positions in the paragraph (end is exclusive).
    start: usize,
    end: usize,
}

fn numeric_cites(inner: &str) -> Vec<Cite> {
    if inner.trim().is_empty() || !inner.chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | ';' | ' ' | '-' | '–' | '—')) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in inner.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut it = part.split(['-', '–', '—']).map(str::trim).filter(|x| !x.is_empty());
        let a = it.next().and_then(|x| x.parse::<usize>().ok());
        let b = it.next().and_then(|x| x.parse::<usize>().ok());
        match (a, b) {
            (Some(a), Some(b)) if b >= a && b - a <= 50 => out.extend((a..=b).map(Cite::Num)),
            (Some(a), _) => out.push(Cite::Num(a)),
            _ => {}
        }
    }
    out.retain(|c| matches!(c, Cite::Num(n) if (1..=999).contains(n)));
    out
}

/// "2019" or "2019a" gives the year.
fn year_of(word: &str) -> Option<String> {
    let w = word.trim_matches(|c: char| !c.is_alphanumeric());
    let digits: String = w.chars().take(4).collect();
    if digits.chars().count() == 4 && digits.chars().all(|c| c.is_ascii_digit()) && (digits.starts_with("19") || digits.starts_with("20")) {
        let rest: String = w.chars().skip(4).collect();
        if rest.is_empty() || (rest.chars().count() == 1 && rest.chars().all(|c| c.is_ascii_lowercase())) {
            return Some(digits);
        }
    }
    None
}

fn is_name(w: &str) -> bool {
    const NOT_NAMES: &[&str] = &["See", "Cf", "Compare", "Also", "For", "In", "Figure", "Table", "Eq", "Fig", "Section", "Chapter", "Appendix"];
    let mut cs = w.chars();
    cs.next().is_some_and(char::is_uppercase) && w.chars().count() >= 2 && !NOT_NAMES.contains(&w)
}

fn author_cites(inner: &str, chars: &[char], open: usize) -> Vec<Cite> {
    let mut out = Vec::new();
    for part in inner.split(';') {
        let ws: Vec<&str> = part.split_whitespace().collect();
        let Some(year) = ws.iter().find_map(|w| year_of(w)) else { continue };
        let mut surname: Option<String> = ws.iter().map(|w| w.trim_matches(|c: char| !c.is_alphabetic())).find(|w| is_name(w)).map(str::to_string);
        if surname.is_none() {
            // "Smith (2020)" and "Smith et al. (2020)": the name just before the bracket.
            let before: String = chars.get(..open).unwrap_or(&[]).iter().collect();
            surname = before
                .split_whitespace()
                .rev()
                .map(|w| w.trim_matches(|c: char| !c.is_alphabetic()))
                .filter(|w| !w.is_empty() && !matches!(w.to_lowercase().as_str(), "al" | "et"))
                .next()
                .filter(|w| is_name(w))
                .map(str::to_string);
        }
        if let Some(s) = surname {
            out.push(Cite::Author { surname: s.to_lowercase(), year });
        }
    }
    out
}

fn scan_marks(text: &str) -> Vec<Mark> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c == '[' || c == '(' {
            let close = if c == '[' { ']' } else { ')' };
            let mut j = i + 1;
            while let Some(&d) = chars.get(j) {
                if d == close || j - i > 160 {
                    break;
                }
                j += 1;
            }
            if chars.get(j) == Some(&close) {
                let inner: String = chars.get(i + 1..j).unwrap_or(&[]).iter().collect();
                let found = if c == '[' { numeric_cites(&inner) } else { author_cites(&inner, &chars, i) };
                if !found.is_empty() {
                    for cite in found {
                        out.push(Mark { cite, start: i, end: j + 1 });
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Sentence ranges (character positions, end exclusive).
fn sentences(p: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) {
            out.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < chars.len() {
        out.push((start, chars.len()));
    }
    out
}

/// Is there a citation in the sentence that holds `quote`, or just after its full stop?
fn sentence_has_marker(paras: &[String], quote: &str) -> bool {
    let q = quote.trim();
    let Some(p) = paras.iter().find(|p| p.contains(q)) else { return false };
    let Some(byte) = p.find(q) else { return false };
    let from = p.get(..byte).map_or(0, |h| h.chars().count());
    let chars: Vec<char> = p.chars().collect();
    let mut end = from + q.chars().count();
    if !q.ends_with(['.', '!', '?']) {
        while let Some(&c) = chars.get(end) {
            if matches!(c, '.' | '!' | '?') {
                break;
            }
            end += 1;
        }
    }
    let to = (end + 14).min(chars.len());
    let w: String = chars.get(from..to).unwrap_or(&[]).iter().collect();
    !scan_marks(&w).is_empty()
}

// ---------------------------------------------------------------------------------------------
// The reference list
// ---------------------------------------------------------------------------------------------

/// "[3] Smith..." or "3. Smith..." gives 3.
fn ref_number(entry: &str) -> Option<usize> {
    let t = entry.trim_start();
    let (body, bracket) = match t.strip_prefix('[') {
        Some(r) => (r, true),
        None => (t, false),
    };
    let digits: String = body.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let rest = body.get(digits.len()..)?;
    let ok = if bracket {
        rest.starts_with(']')
    } else {
        (rest.starts_with('.') || rest.starts_with(')')) && rest.chars().nth(1).is_some_and(char::is_whitespace)
    };
    if !ok {
        return None;
    }
    digits.parse::<usize>().ok().filter(|n| (1..=999).contains(n))
}

/// The entry without its "[3]" or "3." label.
fn strip_label(entry: &str) -> String {
    let t = entry.trim();
    if ref_number(t).is_some() {
        return t.split_once(char::is_whitespace).map_or(t, |(_, r)| r).trim().to_string();
    }
    t.to_string()
}

fn find_doi(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 3 < chars.len() {
        let starts = chars.get(i) == Some(&'1') && chars.get(i + 1) == Some(&'0') && chars.get(i + 2) == Some(&'.');
        let boundary = i == 0 || chars.get(i - 1).is_some_and(|c| !c.is_alphanumeric());
        if starts && boundary {
            let mut j = i + 3;
            while chars.get(j).is_some_and(char::is_ascii_digit) {
                j += 1;
            }
            let digits = j - (i + 3);
            if (4..=9).contains(&digits) && chars.get(j) == Some(&'/') {
                let mut k = j + 1;
                while chars.get(k).is_some_and(|c| !c.is_whitespace()) {
                    k += 1;
                }
                let raw: String = chars.get(i..k).unwrap_or(&[]).iter().collect();
                let doi = raw.trim_end_matches(['.', ',', ';', ')', ']', '>', '"', '\'']).to_string();
                if doi.chars().count() > j - i + 1 {
                    return Some(doi);
                }
            }
        }
        i += 1;
    }
    None
}

fn entry_matches(entry: &str, surname: &str, year: &str) -> bool {
    let low = entry.to_lowercase();
    low.contains(surname) && low.contains(year)
}

// ---------------------------------------------------------------------------------------------
// Check 1a: citations against the reference list (no AI)
// ---------------------------------------------------------------------------------------------

fn structure_findings(doc: &DocText) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut marks: Vec<(usize, Mark)> = Vec::new();
    for (pi, p) in doc.body.iter().enumerate().take(MAX_PARAS) {
        for m in scan_marks(p) {
            marks.push((pi, m));
        }
    }
    let around = |pi: usize, m: &Mark| -> Option<String> { doc.body.get(pi).map(|p| window(p, m.start.saturating_sub(50), m.end)) };

    if marks.is_empty() {
        if !doc.refs.is_empty() {
            out.push(finding(
                Kind::Uncited,
                Sev::Medium,
                "The document has a reference list but I found no citations in the text. I look for [1], [1, 2], [1-3] and (Author, 2020) styles.",
                None,
            ));
        }
        return out;
    }
    if doc.refs.is_empty() {
        let first = marks.first().and_then(|(pi, m)| around(*pi, m));
        out.push(finding(
            Kind::NoList,
            Sev::High,
            format!(
                "The text has {} citation(s) but I found no reference list. I look for a heading called References, Bibliography or Works Cited.",
                marks.len()
            ),
            first,
        ));
        return out;
    }

    let labels: Vec<Option<usize>> = doc.refs.iter().map(|e| ref_number(e)).collect();
    let numbered = labels.iter().filter(|l| l.is_some()).count() * 2 > doc.refs.len();
    let id_of = |i: usize| -> Option<usize> { if numbered { labels.get(i).copied().flatten() } else { Some(i + 1) } };

    // A citation with no entry.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (pi, m) in &marks {
        match &m.cite {
            Cite::Num(n) => {
                let exists = (0..doc.refs.len()).any(|i| id_of(i) == Some(*n));
                if !exists && seen.insert(format!("n{n}")) {
                    out.push(finding(
                        Kind::MissingEntry,
                        Sev::High,
                        format!("Citation [{n}] has no matching entry in the reference list (the list has {} entries).", doc.refs.len()),
                        around(*pi, m),
                    ));
                }
            }
            Cite::Author { surname, year } => {
                let exists = doc.refs.iter().any(|e| entry_matches(e, surname, year));
                if !exists && seen.insert(format!("a{surname}{year}")) {
                    out.push(finding(
                        Kind::MissingEntry,
                        Sev::High,
                        format!("The citation for \"{surname}\" ({year}) has no matching entry in the reference list."),
                        around(*pi, m),
                    ));
                }
            }
        }
    }

    // An entry nobody cites.
    for (i, e) in doc.refs.iter().enumerate() {
        let cited = marks.iter().any(|(_, m)| match &m.cite {
            Cite::Num(n) => id_of(i) == Some(*n),
            Cite::Author { surname, year } => entry_matches(e, surname, year),
        });
        if !cited {
            out.push(finding(Kind::Uncited, Sev::Medium, "This reference is listed but never cited in the text.", Some(lead(e, 80))));
        }
    }

    // Numbers in order of first appearance (many numeric styles want that).
    if numbered {
        let mut seq: Vec<usize> = Vec::new();
        for (_, m) in &marks {
            if let Cite::Num(n) = &m.cite
                && !seq.contains(n)
            {
                seq.push(*n);
            }
        }
        if let Some(pos) = seq.iter().enumerate().position(|(k, n)| *n != k + 1) {
            let got = seq.get(pos).copied().unwrap_or(0);
            out.push(finding(
                Kind::Order,
                Sev::Info,
                format!("The citations are not numbered in order of first appearance: [{got}] comes where [{}] would be expected. Ignore this if your style orders the list another way.", pos + 1),
                None,
            ));
        }
    }

    // The same work listed twice.
    let mut keys: BTreeMap<String, usize> = BTreeMap::new();
    for (i, e) in doc.refs.iter().enumerate() {
        let key = match find_doi(e) {
            Some(d) => d.to_lowercase(),
            None => words(&strip_label(e)).into_iter().take(14).collect::<Vec<_>>().join(" "),
        };
        if key.chars().count() < 25 {
            continue;
        }
        match keys.get(&key) {
            Some(first) => out.push(finding(
                Kind::Duplicate,
                Sev::Medium,
                format!("This looks like entry {} again. Remove one of them.", first + 1),
                Some(lead(e, 80)),
            )),
            None => {
                keys.insert(key, i);
            }
        }
    }

    // Marker before the full stop in some places and after it in others.
    let mut before: Vec<(usize, Mark)> = Vec::new();
    let mut after: Vec<(usize, Mark)> = Vec::new();
    for (pi, m) in &marks {
        if !matches!(m.cite, Cite::Num(_)) {
            continue;
        }
        let Some(p) = doc.body.get(*pi) else { continue };
        let chars: Vec<char> = p.chars().collect();
        let prev = chars.get(..m.start).unwrap_or(&[]).iter().rev().find(|c| !c.is_whitespace()).copied();
        let next = chars.get(m.end..).unwrap_or(&[]).iter().find(|c| !c.is_whitespace()).copied();
        if prev.is_some_and(|c| matches!(c, '.' | '!' | '?')) {
            after.push((*pi, m.clone()));
        } else if next.is_some_and(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?')) {
            before.push((*pi, m.clone()));
        }
    }
    let total = before.len() + after.len();
    if !before.is_empty() && !after.is_empty() && total >= 4 {
        let (minority, side, majority_side) = if before.len() <= after.len() { (&before, "before", "after") } else { (&after, "after", "before") };
        if minority.len() * 10 <= total * 3 {
            for (pi, m) in minority.iter().take(8) {
                out.push(finding(
                    Kind::Placement,
                    Sev::Info,
                    format!("This citation sits {side} the full stop; most of yours sit {majority_side} it. Pick one style."),
                    around(*pi, m),
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Check 1b: sentences that may need a citation
// ---------------------------------------------------------------------------------------------

fn has_figure(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if !c.is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut dot = false;
        while let Some(&d) = chars.get(j) {
            if d.is_ascii_digit() {
                j += 1;
            } else if d == '.' && !dot && chars.get(j + 1).is_some_and(char::is_ascii_digit) {
                dot = true;
                j += 1;
            } else {
                break;
            }
        }
        if dot || chars.get(j) == Some(&'%') {
            return true;
        }
        i = j;
    }
    s.split_whitespace().any(|w| year_of(w).is_some())
}

fn claim_signal(s: &str) -> Option<&'static str> {
    const PHRASES: &[&str] = &[
        "according to",
        "studies show",
        "study shows",
        "research shows",
        "research has shown",
        "has been shown",
        "it is well known",
        "experts",
        "survey",
        "statistics",
        "evidence suggests",
        "scholars",
        "previous studies",
        "recent studies",
        "it has been reported",
    ];
    let low = s.to_lowercase();
    if PHRASES.iter().any(|k| low.contains(k)) {
        return Some("a statement about what research or experts say");
    }
    if has_figure(s) {
        return Some("a specific figure or date");
    }
    None
}

/// Plain rules, no AI. Deliberately modest: the author's own results are skipped.
fn local_claims(doc: &DocText) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in doc.body.iter().take(MAX_PARAS) {
        let chars: Vec<char> = p.chars().collect();
        for (s, e) in sentences(p) {
            let sent: String = chars.get(s..e).unwrap_or(&[]).iter().collect();
            let sent = sent.trim();
            if sent.split_whitespace().count() < 8 {
                continue;
            }
            let tail = (e + 14).min(chars.len());
            let win: String = chars.get(s..tail).unwrap_or(&[]).iter().collect();
            if !scan_marks(&win).is_empty() {
                continue;
            }
            let low = sent.to_lowercase();
            let own = [" we ", " our ", " my ", "table ", "figure ", "fig. "].iter().any(|k| low.contains(k))
                || low.starts_with("we ")
                || low.starts_with("our ");
            if own {
                continue;
            }
            let Some(why) = claim_signal(sent) else { continue };
            out.push(finding(Kind::NeedsCitation, Sev::Info, format!("This sentence has {why} but no citation."), Some(lead(sent, 120))));
            if out.len() >= MAX_LOCAL_CLAIMS {
                return out;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Check 1c: claims without support (AI, every proposal verified against the document)
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Progress {
    done: AtomicUsize,
    total: AtomicUsize,
}

fn parse_claims(reply: &str, paras: &[String]) -> Result<(Vec<Finding>, usize), String> {
    let v = extract_json(reply).ok_or("the reply was not valid JSON")?;
    let mut out = Vec::new();
    let mut dropped = 0;
    for item in v.get("claims").and_then(Value::as_array).into_iter().flatten().take(15) {
        let quote = str_of(item, "quote");
        let reason = str_of(item, "reason");
        let kind = match item.get("kind").and_then(Value::as_str) {
            Some("needs_citation") => Kind::NeedsCitation,
            Some("unsupported") => Kind::Unsupported,
            _ => {
                dropped += 1;
                continue;
            }
        };
        if !quote_found(&quote, paras) {
            dropped += 1;
            continue;
        }
        // A sentence that already carries a citation is not what the writer needs to hear about.
        if sentence_has_marker(paras, &quote) {
            continue;
        }
        let detail = if reason.is_empty() { kind.label().to_string() } else { clip_head(&reason, 300) };
        out.push(finding(kind, Sev::Medium, detail, Some(quote)));
    }
    Ok((out, dropped))
}

fn ai_claims(doc: &DocText, only: Option<usize>, progress: &Progress) -> Result<(Vec<Finding>, usize, String), String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for p in doc.body.iter().take(MAX_PARAS) {
        if !cur.is_empty() && cur.chars().count() + p.chars().count() > CHUNK_CHARS {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(p);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks.truncate(MAX_AI_CHUNKS);
    progress.total.store(chunks.len(), Ordering::Relaxed);

    let mut found = Vec::new();
    let mut dropped = 0;
    let mut label = String::new();
    let mut errors: Vec<String> = Vec::new();
    for chunk in &chunks {
        let result = ai_net::ask(CLAIMS_PROMPT, chunk, only, |t| extract_json(t).is_some_and(|v| v.get("claims").is_some()));
        match result {
            Ok(reply) => match parse_claims(&reply.text, &doc.body) {
                Ok((f, d)) => {
                    found.extend(f);
                    dropped += d;
                    label = reply.label;
                }
                Err(e) => errors.push(e),
            },
            Err(e) => errors.push(e),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    if label.is_empty() && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok((found, dropped, label))
}

fn check_citations(doc: &DocText, use_ai: bool, only: Option<usize>, progress: &Progress) -> Report {
    let mut r = Report::default();
    if doc.body.is_empty() {
        r.notes.push("There is no text in the document yet.".into());
        return r;
    }
    if !doc.found_refs_heading {
        r.notes.push("I found no heading called References, Bibliography or Works Cited, so the reference-list checks are limited.".into());
    }
    r.findings.extend(structure_findings(doc));

    let mut local = local_claims(doc);
    if use_ai {
        match ai_claims(doc, only, progress) {
            Ok((found, dropped, label)) => {
                // The model's sentence replaces a plain-rule hit on the same text.
                local.retain(|l| {
                    let lq = l.quote.as_deref().unwrap_or("");
                    !found.iter().any(|f| f.quote.as_deref().is_some_and(|fq| fq.contains(lq) || lq.contains(fq)))
                });
                r.findings.extend(found);
                r.dropped = dropped;
                r.label = label;
            }
            Err(e) => r.notes.push(format!("The AI check could not run, so only the plain rules ran.\n{}", clip_head(&e, 400))),
        }
    } else {
        r.notes.push("Local checks only: no AI was used.".into());
    }
    r.findings.extend(local);
    r.findings.sort_by_key(|f| f.sev);
    r.findings.truncate(MAX_FINDINGS);
    r
}

// ---------------------------------------------------------------------------------------------
// Check 2: do the references exist? (Crossref and doi.org)
// ---------------------------------------------------------------------------------------------
#[allow(dead_code)]
enum Verdict {
    /// Found in Crossref (the DOI).
    Verified(String),
    /// The DOI exists but belongs to another work.
    WrongWork { doi: String, title: String },
    /// The DOI exists at another registry (not Crossref); the title was not checked.
    Exists(String),
    /// The DOI does not exist anywhere.
    DoiMissing(String),
    /// No matching record.
    Unverified,
    /// The lookup itself failed (network, rate limit).
    Failed(String),
}

fn verify_refs(doc: &DocText, progress: &Progress) -> Report {
    let mut r = Report::default();
    if doc.refs.is_empty() {
        r.notes.push("I found no reference list. I look for a heading called References, Bibliography or Works Cited.".into());
        return r;
    }
    let entries: Vec<&String> = doc.refs.iter().take(MAX_REFS).collect();
    if doc.refs.len() > MAX_REFS {
        r.notes.push(format!("Only the first {MAX_REFS} of {} references were checked.", doc.refs.len()));
    }
    progress.total.store(entries.len(), Ordering::Relaxed);
    let mut failed = 0;
    let mut first_error = String::new();
    for e in entries {
        r.checked += 1;
        let quote = Some(lead(e, 80));
        match verify_one(e) {
            Verdict::Verified(_) | Verdict::Exists(_) => r.confirmed += 1,
            Verdict::WrongWork { doi, title } => r.findings.push(finding(
                Kind::WrongWork,
                Sev::High,
                format!("The DOI {doi} exists, but it belongs to a different work: \"{}\". The entry may be wrong or invented.", clip_head(&title, 160)),
                quote,
            )),
            Verdict::DoiMissing(doi) => r.findings.push(finding(
                Kind::NotFound,
                Sev::High,
                format!("The DOI {doi} does not exist. A mistyped or made-up DOI is a common sign of a fabricated reference."),
                quote,
            )),
            Verdict::Unverified => r.findings.push(finding(
                Kind::Unverified,
                Sev::Medium,
                "No matching record in Crossref. Books, reports, theses and web pages are often missing there, so this is not proof the reference is wrong. Check it by hand.",
                quote,
            )),
            Verdict::Failed(e) => {
                failed += 1;
                if first_error.is_empty() {
                    first_error = e;
                }
            }
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    if failed > 0 {
        r.notes.push(format!("{failed} lookup(s) failed and were skipped ({}). Run the check again to retry.", clip_head(&first_error, 120)));
    }
    r.findings.sort_by_key(|f| f.sev);
    r
}

#[cfg(target_arch = "wasm32")]
fn verify_one(_entry: &str) -> Verdict {
    Verdict::Failed("reference checks work in the desktop app".into())
}

#[cfg(target_arch = "wasm32")]
fn find_pages(_passage: &str) -> Result<Vec<String>, String> {
    Err("web matching works in the desktop app".into())
}

#[cfg(target_arch = "wasm32")]
fn fetch_text(_url: &str) -> Result<String, String> {
    Err("web matching works in the desktop app".into())
}

#[cfg(not(target_arch = "wasm32"))]
use net::{fetch_text, find_pages, verify_one};

#[cfg(not(target_arch = "wasm32"))]
mod net {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Verdict, coverage, find_doi, strip_label, strip_tags};

    pub fn get(url: &str, secs: u64) -> Result<(u16, String), String> {
        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(secs))).http_status_as_error(false).build();
        let agent = ureq::Agent::new_with_config(config);
        let mut resp = agent.get(url).header("User-Agent", "WordCraft (citation check)").call().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        Ok((status, text))
    }

    fn post_json(url: &str, header: (&str, &str), body: &Value) -> Result<Value, String> {
        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(90))).http_status_as_error(false).build();
        let agent = ureq::Agent::new_with_config(config);
        let mut resp = agent.post(url).header(header.0, header.1).send_json(body).map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn enc(s: &str) -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }

    /// Like `enc`, but a DOI's slash stays a slash.
    fn enc_path(s: &str) -> String {
        s.split('/').map(enc).collect::<Vec<_>>().join("/")
    }

    /// Google's search result links are redirects: follow one hop to get the real page address.
    fn resolve(url: &str) -> String {
        if !url.contains("vertexaisearch.cloud.google.com") {
            return url.to_string();
        }
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(8)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        agent
            .get(url)
            .call()
            .ok()
            .and_then(|r| r.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string))
            .unwrap_or_else(|| url.to_string())
    }

    /// The DOI is not in Crossref: does it exist at another registry (DataCite, mEDRA...)?
    fn elsewhere(doi: &str) -> Verdict {
        match get(&format!("https://doi.org/api/handles/{}", enc_path(doi)), 15) {
            Ok((200, body)) => {
                let code = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("responseCode").and_then(Value::as_i64));
                if code == Some(1) { Verdict::Exists(doi.to_string()) } else { Verdict::DoiMissing(doi.to_string()) }
            }
            Ok((404, _)) => Verdict::DoiMissing(doi.to_string()),
            Ok((s, _)) => Verdict::Failed(format!("doi.org answered HTTP {s}")),
            Err(e) => Verdict::Failed(e),
        }
    }

    pub fn verify_one(entry: &str) -> Verdict {
        let entry = strip_label(entry);
        if let Some(doi) = find_doi(&entry) {
            let url = format!("https://api.crossref.org/works/{}", enc_path(&doi));
            return match get(&url, 20) {
                Err(e) => Verdict::Failed(e),
                Ok((200, body)) => {
                    let title = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v.pointer("/message/title/0").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_default();
                    if title.is_empty() || coverage(&title, &entry) >= 0.7 { Verdict::Verified(doi) } else { Verdict::WrongWork { doi, title } }
                }
                Ok((404, _)) => elsewhere(&doi),
                Ok((s, _)) => Verdict::Failed(format!("Crossref answered HTTP {s}")),
            };
        }
        let q: String = entry.chars().take(250).collect();
        let url = format!("https://api.crossref.org/works?query.bibliographic={}&rows=3&select=DOI,title,issued", enc(&q));
        match get(&url, 20) {
            Err(e) => Verdict::Failed(e),
            Ok((200, body)) => {
                let v: Value = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => return Verdict::Failed(e.to_string()),
                };
                let items = v.pointer("/message/items").and_then(Value::as_array).cloned().unwrap_or_default();
                for it in &items {
                    let title = it.pointer("/title/0").and_then(Value::as_str).unwrap_or("");
                    let doi = it.get("DOI").and_then(Value::as_str).unwrap_or("");
                    let year = it.pointer("/issued/date-parts/0/0").and_then(Value::as_i64).map(|y| y.to_string()).unwrap_or_default();
                    if !title.is_empty() && !doi.is_empty() && coverage(title, &entry) >= 0.8 && (year.is_empty() || entry.contains(&year)) {
                        return Verdict::Verified(doi.to_string());
                    }
                }
                Verdict::Unverified
            }
            Ok((s, _)) => Verdict::Failed(format!("Crossref answered HTTP {s}")),
        }
    }

    /// Pages that Google Search finds for a passage. The model only searches; WordCraft decides
    /// whether a page really matches.
    pub fn find_pages(passage: &str) -> Result<Vec<String>, String> {
        let key = crate::ai_net::key(crate::ai_net::Provider::Gemini).ok_or("web matching needs GEMINI_API_KEY (it uses Google Search grounding)")?;
        let prompt = format!(
            "Search the web for pages that contain this exact wording or a close copy of it. Search for the sentence in quotation marks.\n\nText: \"{passage}\"\n\nName only pages that really contain it. If you find none, say so."
        );
        let body = json!({
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
            "tools": [{"google_search": {}}],
            "generationConfig": {"temperature": 0.0}
        });
        let v =
            post_json("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent", ("x-goog-api-key", &key), &body)
                .map_err(|e| format!("web search failed: {e}"))?;
        let uris: Vec<String> = v
            .pointer("/candidates/0/groundingMetadata/groundingChunks")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|c| c.pointer("/web/uri").and_then(Value::as_str).map(str::to_string)).take(4).collect())
            .unwrap_or_default();
        let resolved: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = uris.iter().map(|u| s.spawn(move || resolve(u))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).filter(|u| !u.is_empty()).collect()
        });
        Ok(resolved)
    }

    /// The readable text of a web page.
    pub fn fetch_text(url: &str) -> Result<String, String> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err("not a web address".into());
        }
        let (status, body) = get(url, 15)?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        Ok(strip_tags(&body.chars().take(600_000).collect::<String>()))
    }
}

// ---------------------------------------------------------------------------------------------
// Check 3: matching passages
// ---------------------------------------------------------------------------------------------

fn drop_blocks(html: &str, open: &str, close: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut pos = 0;
    while let Some(rel) = lower.get(pos..).and_then(|s| s.find(open)) {
        let start = pos + rel;
        out.push_str(html.get(pos..start).unwrap_or(""));
        match lower.get(start..).and_then(|s| s.find(close)) {
            Some(end_rel) => pos = start + end_rel + close.len(),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(html.get(pos..).unwrap_or(""));
    out
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn strip_tags(html: &str) -> String {
    let html = drop_blocks(&drop_blocks(html, "<script", "</script>"), "<style", "</style>");
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

fn read_corpus(dir: &str) -> Result<Vec<(String, BTreeSet<u64>)>, String> {
    let rd = std::fs::read_dir(dir.trim()).map_err(|e| format!("{}: {e}", dir.trim()))?;
    let mut out = Vec::new();
    for entry in rd.flatten().take(5_000) {
        if out.len() >= MAX_CORPUS_FILES {
            break;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).unwrap_or_default();
        if !matches!(ext.as_str(), "txt" | "md" | "markdown") {
            continue;
        }
        if entry.metadata().map(|m| m.len() > 5_000_000).unwrap_or(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        out.push((name, shingles(&words(&text))));
    }
    if out.is_empty() {
        return Err("that folder has no .txt or .md files".into());
    }
    Ok(out)
}

fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.split(':').next().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase()
}

fn check_similarity(doc: &DocText, corpus_dir: &str, web: bool, progress: &Progress) -> Report {
    let mut r = Report::default();
    // Long passages, with their word fingerprints.
    let mut passages: Vec<(&String, BTreeSet<u64>)> = Vec::new();
    for p in doc.body.iter().take(MAX_PARAS) {
        let w = words(p);
        if w.len() >= 25 {
            passages.push((p, shingles(&w)));
        }
    }
    if passages.is_empty() {
        r.notes.push("No paragraph is long enough (25 words) to compare.".into());
        return r;
    }
    let mut ran = false;

    if !corpus_dir.trim().is_empty() {
        match read_corpus(corpus_dir) {
            Ok(files) => {
                ran = true;
                r.notes.push(format!("Compared with {} file(s) in {}.", files.len(), corpus_dir.trim()));
                for (p, sh) in &passages {
                    let best = files.iter().map(|(name, fsh)| (containment(sh, fsh), name)).max_by(|a, b| a.0.total_cmp(&b.0));
                    if let Some((score, name)) = best
                        && score >= CORPUS_MATCH
                    {
                        let pct = (score * 100.0).round() as u32;
                        let sev = if score >= 0.8 { Sev::High } else { Sev::Medium };
                        r.findings.push(finding(
                            Kind::Similar,
                            sev,
                            format!("About {pct}% of this passage's wording appears in \"{name}\". If you used it, quote and cite it; if not, rewrite the passage."),
                            Some(lead(p, 100)),
                        ));
                    }
                }
            }
            Err(e) => r.notes.push(format!("The source folder could not be read: {e}")),
        }
    }

    if web {
        // A few passages spread over the document.
        let step = (passages.len() / MAX_WEB_PASSAGES).max(1);
        let picked: Vec<&(&String, BTreeSet<u64>)> = passages.iter().step_by(step).take(MAX_WEB_PASSAGES).collect();
        progress.total.store(picked.len(), Ordering::Relaxed);
        let mut web_error = String::new();
        for (p, sh) in picked {
            let query: String = p.split_whitespace().take(30).collect::<Vec<_>>().join(" ");
            match find_pages(&query) {
                Err(e) => {
                    if web_error.is_empty() {
                        web_error = e;
                    }
                }
                Ok(urls) => {
                    ran = true;
                    let mut best: Option<(f32, String)> = None;
                    for url in urls {
                        let Ok(text) = fetch_text(&url) else { continue };
                        let score = containment(sh, &shingles(&words(&text)));
                        if best.as_ref().is_none_or(|(b, _)| score > *b) {
                            best = Some((score, url));
                        }
                    }
                    if let Some((score, url)) = best
                        && score >= WEB_MATCH
                    {
                        let pct = (score * 100.0).round() as u32;
                        let host = host_of(&url);
                        let cited = doc.refs.iter().any(|e| e.contains(&url) || (!host.is_empty() && e.to_lowercase().contains(&host)));
                        let (sev, advice) = if cited {
                            (
                                Sev::Info,
                                "You cite this source. If the wording is copied, put it in quotation marks with a page number, or rewrite it in your own words.",
                            )
                        } else {
                            (Sev::Medium, "This page is not in your references. If you used it, cite it; if not, rewrite the passage.")
                        };
                        let mut f = finding(
                            Kind::Similar,
                            sev,
                            format!("About {pct}% of this passage's wording appears on {host}. {advice}"),
                            Some(lead(p, 100)),
                        );
                        f.url = Some(url);
                        r.findings.push(f);
                    }
                }
            }
            progress.done.fetch_add(1, Ordering::Relaxed);
        }
        if !web_error.is_empty() {
            r.notes.push(format!("Web matching problem: {}", clip_head(&web_error, 200)));
        }
        if ran {
            r.notes.push(format!(
                "Web: searched for {} passage(s). Only pages that were downloaded and really match are listed.",
                progress.total.load(Ordering::Relaxed)
            ));
        }
    }

    if !ran {
        r.notes.push("Nothing was compared. Choose a folder of source files (.txt or .md) or tick the web search.".into());
    }
    r.findings.sort_by_key(|f| f.sev);
    r.findings.truncate(MAX_FINDINGS);
    r
}

// ---------------------------------------------------------------------------------------------
// The panel section and its plumbing
// ---------------------------------------------------------------------------------------------

enum Job {
    Cites(Report),
    Refs(Report),
    Similarity(Report),
}

#[derive(Default)]
pub struct State {
    cites: Option<Report>,
    refs: Option<Report>,
    sim: Option<Report>,
    local_only: bool,
    corpus_dir: String,
    web: bool,
    busy: Option<Receiver<Job>>,
    progress: Option<Arc<Progress>>,
    cite_request: bool,
    refs_request: bool,
    sim_request: bool,
    clear_request: bool,
    show_request: Option<String>,
}

fn show_report(ui: &mut egui::Ui, t: &Tokens, salt: &str, title: &str, r: &Report, show: &mut Option<String>) {
    ui.separator();
    let head = if r.checked > 0 {
        format!("{title}: {} of {} confirmed, {} to look at", r.confirmed, r.checked, r.findings.len())
    } else {
        format!("{title}: {} to look at", r.findings.len())
    };
    ui.label(RichText::new(head).strong());
    for n in &r.notes {
        ui.add(egui::Label::new(RichText::new(n.as_str()).small().color(t.text_dim)).wrap());
    }
    egui::ScrollArea::vertical().id_salt(salt).max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
        for f in &r.findings {
            ui.add_space(4.0);
            let colour = match f.sev {
                Sev::High => t.red,
                Sev::Medium => amber(),
                Sev::Info => t.text_dim,
            };
            ui.label(RichText::new(f.kind.label()).strong().color(colour));
            ui.add(egui::Label::new(RichText::new(f.detail.as_str()).small()).wrap());
            if let Some(q) = &f.quote {
                ui.horizontal_wrapped(|ui| {
                    ui.add(egui::Label::new(RichText::new(format!("\"{q}\"")).small().italics()).wrap());
                    if ui.small_button("Show").clicked() {
                        *show = Some(q.clone());
                    }
                });
            }
            if let Some(u) = &f.url {
                ui.hyperlink(u.as_str());
            }
        }
    });
}

/// The "Citations and integrity" section of the Word panel.
pub fn ui(ui: &mut egui::Ui, t: &Tokens, st: &mut State) {
    egui::CollapsingHeader::new("Citations and integrity").default_open(false).show(ui, |ui| {
        ui.label(
            RichText::new(
                "Word checks that citations and the reference list agree, looks for claims with no support, checks that references exist, and flags passages that match other sources. Every finding quotes your document. Nothing is changed.",
            )
            .small()
            .color(t.text_dim),
        );
        let busy = st.busy.is_some();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Check citations"))
                .on_hover_text("Citations against the reference list, sentences that may need a citation, and claims with no support.")
                .clicked()
            {
                st.cite_request = true;
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Verify references"))
                .on_hover_text("Looks each reference up in Crossref and doi.org. A reference that is not found is flagged for you to check, not called fake.")
                .clicked()
            {
                st.refs_request = true;
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Check similarity"))
                .on_hover_text("Compares long passages with your source folder and, if ticked, with web pages a search finds.")
                .clicked()
            {
                st.sim_request = true;
            }
        });
        ui.checkbox(&mut st.local_only, "Local checks only (no AI)").on_hover_text("Citation checks use plain rules only. Reference and similarity checks never need the AI model.");
        ui.label(RichText::new("Source folder for similarity (.txt, .md)").small().color(t.text_dim));
        ui.add(egui::TextEdit::singleline(&mut st.corpus_dir).desired_width(f32::INFINITY).hint_text("e.g. C:\\papers\\sources"));
        ui.checkbox(&mut st.web, "Also search the web for matching pages")
            .on_hover_text("Uses GEMINI_API_KEY (Google Search). Downloads the pages it finds and measures the match itself. Slow; checks up to 5 passages.");
        ui.label(
            RichText::new("Similarity needs source texts to compare with. An AI model alone cannot guarantee plagiarism detection; matches are for you to review.")
                .small()
                .color(t.text_dim),
        );
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                let (done, total) = st.progress.as_ref().map_or((0, 0), |p| (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed)));
                let msg = if total > 0 { format!("Word is working... {} of {}", (done + 1).min(total), total) } else { "Word is working...".to_string() };
                ui.label(RichText::new(msg).color(t.text_dim));
            });
        }
        if st.cites.is_some() || st.refs.is_some() || st.sim.is_some() {
            if ui.add_enabled(!busy, egui::Button::new("Clear results")).clicked() {
                st.clear_request = true;
            }
        }
        if let Some(r) = &st.cites {
            show_report(ui, t, "word-ai-integrity-cites", "Citations", r, &mut st.show_request);
        }
        if let Some(r) = &st.refs {
            show_report(ui, t, "word-ai-integrity-refs", "References", r, &mut st.show_request);
        }
        if let Some(r) = &st.sim {
            show_report(ui, t, "word-ai-integrity-sim", "Similarity", r, &mut st.show_request);
        }
    });
}

fn spawn_job(ctx: &egui::Context, work: impl FnOnce() -> Job + Send + 'static) -> Result<Receiver<Job>, String> {
    let (tx, rx) = channel();
    let wake = ctx.clone();
    std::thread::Builder::new()
        .name("word-ai-integrity".into())
        .spawn(move || {
            let _ = tx.send(work());
            wake.request_repaint();
        })
        .map(|_| rx)
        .map_err(|e| e.to_string())
}

fn summary(title: &str, r: &Report) -> String {
    let mut m = if r.checked > 0 {
        format!("{title} finished: {} of {} confirmed, {} to look at.", r.confirmed, r.checked, r.findings.len())
    } else if r.findings.is_empty() {
        format!("{title} finished: I found nothing to flag.")
    } else {
        format!("{title} finished: {} possible problem(s).", r.findings.len())
    };
    for f in r.findings.iter().take(12) {
        m.push_str(&format!("\n- {}: {}", f.kind.label(), f.detail));
    }
    if r.findings.len() > 12 {
        m.push_str(&format!("\n...and {} more in the Citations and integrity section.", r.findings.len() - 12));
    }
    if r.dropped > 0 {
        m.push_str(&format!("\n\nI dropped {} suggestion(s) whose quotes are not in your document.", r.dropped));
    }
    for n in &r.notes {
        m.push_str(&format!("\n\n{n}"));
    }
    m.push_str("\n\nUse Show to jump to a passage. Nothing in your document was changed.");
    if !r.label.is_empty() {
        m.push_str(&format!(" Checked by {}.", r.label));
    }
    m
}

/// Called every frame from `ai_word::show`: requests from the panel and finished jobs.
pub fn drive(app: &mut WordApp, ctx: &egui::Context, st: &mut State, only: Option<usize>, messages: &mut Vec<(bool, String)>) {
    if let Some(quote) = st.show_request.take() {
        match app.session.run("select.text", &json!({"text": quote})) {
            Ok(_) => app.canvas.scroll_to_caret = true,
            Err(_) => messages.push((false, "I couldn't find that passage any more. The document has changed since the check.".into())),
        }
        ctx.request_repaint();
    }
    if std::mem::take(&mut st.clear_request) && st.busy.is_none() {
        st.cites = None;
        st.refs = None;
        st.sim = None;
    }

    if st.cite_request && st.busy.is_none() {
        st.cite_request = false;
        messages.push((true, "Check citations".into()));
        let doc = read_doc(&app.session);
        let use_ai = !st.local_only;
        let progress = Arc::new(Progress::default());
        let shared = Arc::clone(&progress);
        match spawn_job(ctx, move || Job::Cites(check_citations(&doc, use_ai, only, &shared))) {
            Ok(rx) => {
                st.busy = Some(rx);
                st.progress = Some(progress);
            }
            Err(e) => messages.push((false, format!("Couldn't start the request: {e}"))),
        }
    }
    if st.refs_request && st.busy.is_none() {
        st.refs_request = false;
        messages.push((true, "Verify references".into()));
        let doc = read_doc(&app.session);
        let progress = Arc::new(Progress::default());
        let shared = Arc::clone(&progress);
        match spawn_job(ctx, move || Job::Refs(verify_refs(&doc, &shared))) {
            Ok(rx) => {
                st.busy = Some(rx);
                st.progress = Some(progress);
            }
            Err(e) => messages.push((false, format!("Couldn't start the request: {e}"))),
        }
    }
    if st.sim_request && st.busy.is_none() {
        st.sim_request = false;
        messages.push((true, "Check similarity".into()));
        let doc = read_doc(&app.session);
        let dir = st.corpus_dir.clone();
        let web = st.web;
        let progress = Arc::new(Progress::default());
        let shared = Arc::clone(&progress);
        match spawn_job(ctx, move || Job::Similarity(check_similarity(&doc, &dir, web, &shared))) {
            Ok(rx) => {
                st.busy = Some(rx);
                st.progress = Some(progress);
            }
            Err(e) => messages.push((false, format!("Couldn't start the request: {e}"))),
        }
    }

    if let Some(rx) = &st.busy {
        match rx.try_recv() {
            Ok(job) => {
                st.busy = None;
                st.progress = None;
                match job {
                    Job::Cites(r) => {
                        messages.push((false, summary("Citation check", &r)));
                        st.cites = Some(r);
                    }
                    Job::Refs(r) => {
                        messages.push((false, summary("Reference check", &r)));
                        st.refs = Some(r);
                    }
                    Job::Similarity(r) => {
                        messages.push((false, summary("Similarity check", &r)));
                        st.sim = Some(r);
                    }
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(150)),
            Err(TryRecvError::Disconnected) => {
                st.busy = None;
                st.progress = None;
                messages.push((false, "The request stopped unexpectedly.".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_and_author_citations_are_found() {
        let marks = scan_marks("Rates rose [1, 3–4] and fell (Smith & Jones, 2019; Lee, 2020a).");
        assert_eq!(marks.len(), 5);
    }

    #[test]
    fn reference_labels_are_read() {
        assert_eq!(ref_number("[12] Smith, J."), Some(12));
        assert_eq!(ref_number("3. Lee, K."), Some(3));
        assert_eq!(ref_number("Smith, J. (2019)."), None);
    }

    #[test]
    fn a_doi_is_found_without_its_full_stop() {
        let doi = find_doi("Smith (2019). Title. J. https://doi.org/10.1038/s41586-020-2649-2.");
        assert_eq!(doi.as_deref(), Some("10.1038/s41586-020-2649-2"));
    }

    #[test]
    fn title_coverage_is_full_for_a_matching_entry() {
        assert!(coverage("Deep learning for chemistry", "Lee (2019). Deep learning for chemistry. Nature.") > 0.9);
    }

    #[test]
    fn a_citation_without_a_reference_is_flagged() {
        let doc = DocText { body: vec!["Water boils at 100 degrees [2].".into()], refs: vec!["[1] Lee, 2019. A.".into()], found_refs_heading: true };
        let found = structure_findings(&doc);
        assert!(found.iter().any(|f| f.kind == Kind::MissingEntry));
        assert!(found.iter().any(|f| f.kind == Kind::Uncited));
    }
}
