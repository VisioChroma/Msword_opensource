//! "Word" AI, part 5: persistent research context and continuity.
//!
//! WordCraft keeps a *research memory* for the document: the research questions, objectives,
//! methodology, reported results, conclusions and defined terms of every chapter, how the
//! chapters build on each other, and the writer's own decisions ("use British spelling").
//! The memory is saved next to the document (`thesis.docx.wordcraft-context.json`), so it
//! survives restarts and long projects. Only chapters that changed since the last update are
//! read again.
//!
//! The model proposes; WordCraft verifies. Every fact the model records and every problem it
//! reports must quote the document, and the quote must really be in a paragraph of the document.
//! Anything else is dropped and counted, never shown. Three checks need no AI at all: repeated
//! passages in different chapters, numbers in a conclusion that no other chapter reports, and a
//! term defined in two places. The memory can also be handed to the writing assistant, so new
//! text stays consistent with it.
//!
//! Nothing in the document is ever changed by this module.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{Color32, RichText};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wordcraft_engine::Session;

use crate::WordApp;
use crate::ai_net;
use crate::ai_plan::extract_json;
use crate::theme::Tokens;

const MAX_CHAPTERS: usize = 60;
const MAX_CHAPTER_CHARS: usize = 14_000;
const MAX_FACTS: usize = 25;
const MAX_QUOTE: usize = 300;
const MAX_FINDINGS: usize = 30;
const MAX_LOCAL: usize = 10;
const MAX_PARAS: usize = 1_500;
const MAX_PROMPT_BLOCK: usize = 5_000;
const MAX_DECISION: usize = 300;
const SIMILAR: f32 = 0.6;
const FILE_SUFFIX: &str = ".wordcraft-context.json";

pub const DIGEST_PROMPT: &str = r#"You are Word, the research-continuity assistant inside the WordCraft document editor.
You receive ONE chapter of a long document. Record what the chapter itself states, so that the other chapters can be checked against it later.

Reply with ONE JSON object and nothing else (no markdown fences, no commentary):
{
  "summary": "two or three sentences: what this chapter does",
  "depends_on": ["titles of other chapters this one explicitly builds on or refers to"],
  "questions": [{"text": "a research question", "quote": "..."}],
  "objectives": [{"text": "an aim or objective", "quote": "..."}],
  "methods": [{"text": "a methodological choice (design, sample, instrument, analysis)", "quote": "..."}],
  "results": [{"text": "a finding the chapter reports, with its numbers", "quote": "..."}],
  "conclusions": [{"text": "a claim the chapter draws from results", "quote": "..."}],
  "terms": [{"term": "...", "meaning": "...", "quote": "..."}]
}

Rules:
- Record only what the chapter explicitly states. A missing item is correct; an invented item is a failure. Empty lists are fine.
- "quote" is copied word for word from ONE paragraph of the chapter (under 200 characters) and contains the number or term you report. Never paraphrase it. Table rows are shown with cells separated by " | "; a quote from a table must come from a single cell.
- "results" are findings the chapter reports. "conclusions" are claims drawn from them. Do not turn a plan, a hope or a hypothesis into a result.
- "terms" are only terms the chapter defines or fixes the meaning of.
- "depends_on" may only use titles from the list of other chapters you are given.
- Do not add facts from your own knowledge. Do not write or change any text of the document.
- Write in the language of the chapter."#;

pub const CHECK_PROMPT: &str = r#"You are Word, the continuity checker inside the WordCraft document editor.
You receive the research memory of a long document: for each chapter a summary, the questions it asks, its objectives, methods, reported results, conclusions and defined terms. Every item carries a quote from the document.

Find real problems ACROSS chapters. Reply with ONE JSON object and nothing else (no markdown fences, no commentary):
{"findings": [{"kind": "contradiction" | "repetition" | "unsupported_conclusion" | "result_mismatch" | "terminology", "detail": "one or two sentences", "evidence": [{"quote": "..."}]}]}

Kinds:
- contradiction: two statements that cannot both be true (numbers, dates, sample sizes, methods, definitions).
- repetition: the same explanation given in more than one chapter.
- unsupported_conclusion: a conclusion that no reported result supports.
- result_mismatch: a conclusion that says something different from the result it rests on (direction, size, significance).
- terminology: the same concept under different names, or one term used with two meanings.

Rules:
- Every finding needs evidence: quotes copied word for word from the memory you were given. Never invent or edit a quote.
- contradiction, repetition, result_mismatch and terminology need at least two quotes from different places. unsupported_conclusion needs the quote of the conclusion.
- Report only what the quotes show. If you are unsure, leave it out. An empty list is a correct answer.
- Do not comment on style or grammar and do not suggest rewrites.
- At most 15 findings, most serious first."#;

// ---------------------------------------------------------------------------------------------
// The memory
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub text: String,
    pub quote: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Term {
    pub term: String,
    pub meaning: String,
    pub quote: String,
}

/// What one chapter says. `hash` is a fingerprint of its text: unchanged chapters are not re-read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Chapter {
    pub title: String,
    pub hash: u64,
    pub summary: String,
    pub depends_on: Vec<String>,
    pub questions: Vec<Fact>,
    pub objectives: Vec<Fact>,
    pub methods: Vec<Fact>,
    pub results: Vec<Fact>,
    pub conclusions: Vec<Fact>,
    pub terms: Vec<Term>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub version: u32,
    /// The writer's own decisions. Typed by the user, never written by the model.
    pub decisions: Vec<String>,
    pub chapters: Vec<Chapter>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FactKind {
    Questions,
    Objectives,
    Methods,
    Results,
    Conclusions,
}

impl FactKind {
    const ALL: [FactKind; 5] = [FactKind::Questions, FactKind::Objectives, FactKind::Methods, FactKind::Results, FactKind::Conclusions];

    fn label(self) -> &'static str {
        match self {
            FactKind::Questions => "Research questions",
            FactKind::Objectives => "Objectives",
            FactKind::Methods => "Methodology",
            FactKind::Results => "Results",
            FactKind::Conclusions => "Conclusions",
        }
    }
}

fn facts_of(c: &Chapter, k: FactKind) -> &[Fact] {
    match k {
        FactKind::Questions => &c.questions,
        FactKind::Objectives => &c.objectives,
        FactKind::Methods => &c.methods,
        FactKind::Results => &c.results,
        FactKind::Conclusions => &c.conclusions,
    }
}

impl Memory {
    pub fn is_empty(&self) -> bool {
        self.chapters.is_empty() && self.decisions.is_empty()
    }

    fn facts(&self, k: FactKind) -> Vec<(&str, &Fact)> {
        let mut out = Vec::new();
        for c in &self.chapters {
            for f in facts_of(c, k) {
                out.push((c.title.as_str(), f));
            }
        }
        out
    }

    fn term_count(&self) -> usize {
        self.chapters.iter().map(|c| c.terms.len()).sum()
    }

    /// What the writing assistant is told, so new text agrees with the rest of the document.
    fn prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Research memory for this document. Keep everything you write consistent with it: use the terms as defined, do not contradict the reported results, and do not repeat explanations already given in earlier chapters (refer back to them instead).\n",
        );
        for kind in FactKind::ALL {
            let items = self.facts(kind);
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("\n{}:\n", kind.label()));
            for (chapter, f) in items.into_iter().take(12) {
                out.push_str(&format!("- {} ({chapter})\n", f.text));
            }
        }
        let mut terms = Vec::new();
        for c in &self.chapters {
            for t in &c.terms {
                terms.push(format!("- {}: {}\n", t.term, t.meaning));
            }
        }
        if !terms.is_empty() {
            out.push_str("\nTerminology:\n");
            for line in terms.into_iter().take(20) {
                out.push_str(&line);
            }
        }
        if !self.decisions.is_empty() {
            out.push_str("\nThe author's writing decisions (follow them):\n");
            for d in &self.decisions {
                out.push_str(&format!("- {d}\n"));
            }
        }
        if !self.chapters.is_empty() {
            out.push_str("\nChapters so far:\n");
            for c in &self.chapters {
                out.push_str(&format!("- {}: {}\n", c.title, c.summary));
            }
        }
        Some(clip_head(&out, MAX_PROMPT_BLOCK))
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// A stable fingerprint (FNV-1a), so the saved memory keeps meaning across Rust versions.
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

/// Keep the start and the end of a long text; the middle is replaced by a marker.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let mut tail: Vec<char> = text.chars().rev().take(max / 3).collect();
    tail.reverse();
    let tail: String = tail.into_iter().collect();
    format!("{head}\n[... the middle of this chapter is left out ...]\n{tail}")
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// The first `n` characters of a paragraph: always an exact piece of it, so it can be found again.
fn lead(p: &str, n: usize) -> String {
    p.trim().chars().take(n).collect()
}

// ---------------------------------------------------------------------------------------------
// Reading the document: chapters
// ---------------------------------------------------------------------------------------------

/// One chapter of the document as plain text.
#[derive(Clone, Debug, Default)]
pub struct Part {
    pub title: String,
    /// What the model reads (table rows have their cells joined with " | ").
    pub text: String,
    /// Every paragraph and table cell, for finding quotes again.
    pub paras: Vec<String>,
    lines: Vec<String>,
}

fn finish_part(mut part: Part, parts: &mut Vec<Part>) {
    if part.paras.is_empty() {
        return;
    }
    part.text = part.lines.join("\n");
    let base = part.title.clone();
    let mut n = 2;
    while parts.iter().any(|p| p.title == part.title) {
        part.title = format!("{base} ({n})");
        n += 1;
    }
    parts.push(part);
}

/// Split the document at its Heading 1 paragraphs (Heading 2 if there are none, otherwise the
/// whole document is one chapter).
fn split_parts(s: &Session) -> Vec<Part> {
    let doc = &s.doc;
    let level = [1u8, 2].into_iter().find(|lvl| {
        let name = format!("Heading{lvl}");
        doc.body.iter().filter_map(|b| b.as_para()).any(|p| p.props.style.as_deref() == Some(name.as_str()))
    });
    let split_style = level.map(|l| format!("Heading{l}"));
    let first = if split_style.is_some() { "Front matter" } else { "Whole document" };
    let mut parts: Vec<Part> = Vec::new();
    let mut cur = Part { title: first.to_string(), ..Part::default() };
    for b in &doc.body {
        if let Some(table) = b.as_table() {
            for row in &table.rows {
                let mut cells: Vec<String> = Vec::new();
                for cell in &row.cells {
                    let mut texts: Vec<String> = Vec::new();
                    for cb in &cell.blocks {
                        if let Some(cp) = cb.as_para() {
                            let tx = cp.text.trim();
                            if !tx.is_empty() {
                                texts.push(tx.to_string());
                            }
                        }
                    }
                    cur.paras.extend(texts.iter().cloned());
                    cells.push(texts.join(" "));
                }
                let line = cells.join(" | ");
                if !line.replace('|', "").trim().is_empty() {
                    cur.lines.push(line);
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
        if split_style.as_deref() == Some(style) {
            let next = Part { title: text.to_string(), ..Part::default() };
            finish_part(std::mem::replace(&mut cur, next), &mut parts);
            continue;
        }
        cur.paras.push(text.to_string());
        cur.lines.push(text.to_string());
    }
    finish_part(cur, &mut parts);
    parts.truncate(MAX_CHAPTERS);
    parts
}

/// The chapter whose text contains `quote`, if any.
fn locate(parts: &[Part], quote: &str) -> Option<String> {
    let q = quote.trim();
    if q.is_empty() || q.chars().count() > MAX_QUOTE {
        return None;
    }
    parts.iter().find(|part| part.paras.iter().any(|p| p.contains(q))).map(|part| part.title.clone())
}

fn quote_found(quote: &str, paras: &[String]) -> bool {
    let q = quote.trim();
    !q.is_empty() && q.chars().count() <= MAX_QUOTE && paras.iter().any(|p| p.contains(q))
}

// ---------------------------------------------------------------------------------------------
// Updating the memory (AI, one chapter at a time)
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Progress {
    done: AtomicUsize,
    total: AtomicUsize,
}

#[derive(Default)]
pub struct Stats {
    read: usize,
    reused: usize,
    dropped: usize,
    errors: Vec<String>,
    label: String,
}

fn read_facts(v: &Value, key: &str, paras: &[String], dropped: &mut usize) -> Vec<Fact> {
    let mut out = Vec::new();
    for item in v.get(key).and_then(Value::as_array).into_iter().flatten().take(MAX_FACTS) {
        let text = str_of(item, "text");
        let quote = str_of(item, "quote");
        if text.is_empty() {
            continue;
        }
        if quote_found(&quote, paras) {
            out.push(Fact { text, quote });
        } else {
            *dropped += 1;
        }
    }
    out
}

/// Check a chapter digest: keep only what quotes the chapter. Returns the chapter and how many
/// items were dropped.
fn parse_digest(reply: &str, part: &Part, hash: u64, titles: &[String]) -> Result<(Chapter, usize), String> {
    let v = extract_json(reply).ok_or("the reply was not valid JSON")?;
    let mut dropped = 0;
    let mut depends_on: Vec<String> = Vec::new();
    for d in v.get("depends_on").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        let want = d.trim().to_lowercase();
        if let Some(t) = titles.iter().find(|t| **t != part.title && t.to_lowercase() == want)
            && !depends_on.contains(t)
        {
            depends_on.push(t.clone());
        }
    }
    let mut terms = Vec::new();
    for item in v.get("terms").and_then(Value::as_array).into_iter().flatten().take(MAX_FACTS) {
        let term = str_of(item, "term");
        let meaning = str_of(item, "meaning");
        let quote = str_of(item, "quote");
        if term.is_empty() {
            continue;
        }
        if quote_found(&quote, &part.paras) {
            terms.push(Term { term, meaning, quote });
        } else {
            dropped += 1;
        }
    }
    let chapter = Chapter {
        title: part.title.clone(),
        hash,
        summary: clip_head(&str_of(&v, "summary"), 500),
        depends_on,
        questions: read_facts(&v, "questions", &part.paras, &mut dropped),
        objectives: read_facts(&v, "objectives", &part.paras, &mut dropped),
        methods: read_facts(&v, "methods", &part.paras, &mut dropped),
        results: read_facts(&v, "results", &part.paras, &mut dropped),
        conclusions: read_facts(&v, "conclusions", &part.paras, &mut dropped),
        terms,
    };
    Ok((chapter, dropped))
}

/// Read every chapter that is new or changed; keep the rest as it was.
fn update(parts: &[Part], old: &Memory, only: Option<usize>, progress: &Progress) -> Result<(Memory, Stats), String> {
    if parts.is_empty() {
        return Err("There is no text in the document yet.".into());
    }
    progress.total.store(parts.len(), Ordering::Relaxed);
    let titles: Vec<String> = parts.iter().map(|p| p.title.clone()).collect();
    let mut chapters = Vec::new();
    let mut stats = Stats::default();
    for part in parts {
        let hash = fnv(&part.text);
        if let Some(c) = old.chapters.iter().find(|c| c.title == part.title && c.hash == hash) {
            chapters.push(c.clone());
            stats.reused += 1;
            progress.done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let others = titles.iter().filter(|t| **t != part.title).cloned().collect::<Vec<_>>().join("; ");
        let user = format!("Chapter title: {}\nOther chapters: {}\n\nChapter text:\n{}", part.title, others, clip(&part.text, MAX_CHAPTER_CHARS));
        let result = ai_net::ask(DIGEST_PROMPT, &user, only, |t| extract_json(t).is_some())
            .and_then(|reply| parse_digest(&reply.text, part, hash, &titles).map(|(c, d)| (c, d, reply.label)));
        match result {
            Ok((chapter, dropped, label)) => {
                stats.read += 1;
                stats.dropped += dropped;
                stats.label = label;
                chapters.push(chapter);
            }
            Err(e) => {
                stats.errors.push(format!("{}: {}", part.title, clip_head(&e, 300)));
                // Keep what was known, so one failed request does not lose a chapter.
                if let Some(c) = old.chapters.iter().find(|c| c.title == part.title) {
                    chapters.push(c.clone());
                }
            }
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    if stats.read == 0 && stats.reused == 0 {
        return Err(stats.errors.join("\n"));
    }
    Ok((Memory { version: 1, decisions: old.decisions.clone(), chapters }, stats))
}

fn update_message(m: &Memory, s: &Stats) -> String {
    let count = |k: FactKind| m.facts(k).len();
    let mut out = format!(
        "Research memory updated: {} chapter(s) read, {} unchanged.\nRemembering {} question(s), {} objective(s), {} method note(s), {} result(s), {} conclusion(s) and {} defined term(s).",
        s.read,
        s.reused,
        count(FactKind::Questions),
        count(FactKind::Objectives),
        count(FactKind::Methods),
        count(FactKind::Results),
        count(FactKind::Conclusions),
        m.term_count(),
    );
    if s.dropped > 0 {
        out.push_str(&format!("\nI dropped {} item(s) whose quotes I could not find in your document.", s.dropped));
    }
    if !s.errors.is_empty() {
        out.push_str("\n\nThese chapters could not be read (run Update memory again to retry):");
        for e in &s.errors {
            out.push_str(&format!("\n- {e}"));
        }
    }
    if !s.label.is_empty() {
        out.push_str(&format!("\n\nRead by {}.", s.label));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Checking continuity
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Contradiction,
    Repetition,
    Unsupported,
    ResultMismatch,
    Terminology,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Contradiction => "Contradiction",
            Kind::Repetition => "Repeated explanation",
            Kind::Unsupported => "Unsupported conclusion",
            Kind::ResultMismatch => "Result and conclusion disagree",
            Kind::Terminology => "Terminology",
        }
    }

    fn from_key(key: &str) -> Option<Kind> {
        match key {
            "contradiction" => Some(Kind::Contradiction),
            "repetition" => Some(Kind::Repetition),
            "unsupported_conclusion" => Some(Kind::Unsupported),
            "result_mismatch" => Some(Kind::ResultMismatch),
            "terminology" => Some(Kind::Terminology),
            _ => None,
        }
    }

    /// Quotes needed before a finding is believed.
    fn min_evidence(self) -> usize {
        match self {
            Kind::Unsupported => 1,
            _ => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Evidence {
    pub chapter: String,
    pub quote: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub kind: Kind,
    pub detail: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// AI findings thrown away because their quotes are not in the document.
    pub dropped: usize,
    pub note: Option<String>,
    pub label: String,
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

fn shingles(w: &[String]) -> BTreeSet<u64> {
    w.windows(5).map(|win| fnv(&win.join(" "))).collect()
}

/// Passages repeated in different chapters (no AI).
fn repeated(parts: &[Part]) -> Vec<Finding> {
    let mut items: Vec<(usize, &str, BTreeSet<u64>)> = Vec::new();
    'collect: for (pi, part) in parts.iter().enumerate() {
        for p in &part.paras {
            if items.len() >= MAX_PARAS {
                break 'collect;
            }
            let w = words(p);
            if w.len() >= 25 {
                items.push((pi, p.as_str(), shingles(&w)));
            }
        }
    }
    let mut out = Vec::new();
    for (i, a) in items.iter().enumerate() {
        for b in items.iter().skip(i + 1) {
            if a.0 == b.0 {
                continue;
            }
            let inter = a.2.intersection(&b.2).count();
            let union = (a.2.len() + b.2.len()).saturating_sub(inter);
            if union == 0 {
                continue;
            }
            let sim = inter as f32 / union as f32;
            if sim < SIMILAR {
                continue;
            }
            let title = |idx: usize| parts.get(idx).map(|p| p.title.clone()).unwrap_or_default();
            out.push(Finding {
                kind: Kind::Repetition,
                detail: format!("These passages are about {}% alike. Say it once and refer back to it.", (sim * 100.0).round() as u32),
                evidence: vec![Evidence { chapter: title(a.0), quote: lead(a.1, 120) }, Evidence { chapter: title(b.0), quote: lead(b.1, 120) }],
            });
            if out.len() >= MAX_LOCAL {
                return out;
            }
        }
    }
    out
}

/// "12.5" and "12.50" are the same figure.
fn norm_figure(tok: &str) -> String {
    if tok.contains('.') { tok.trim_end_matches('0').trim_end_matches('.').to_string() } else { tok.to_string() }
}

/// Decimal numbers and percentages in a paragraph, each with a little of the text around it.
fn scan_figures(text: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if !c.is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dot = false;
        while let Some(&d) = chars.get(i) {
            if d.is_ascii_digit() {
                i += 1;
            } else if d == '.' && !dot && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                dot = true;
                i += 1;
            } else {
                break;
            }
        }
        let end = i;
        let pct = chars.get(i) == Some(&'%');
        if dot || pct {
            let num: String = chars.get(start..end).unwrap_or(&[]).iter().collect();
            let tok = format!("{}{}", norm_figure(&num), if pct { "%" } else { "" });
            let from = start.saturating_sub(50);
            let to = (end + 50).min(chars.len());
            let window: String = chars.get(from..to).unwrap_or(&[]).iter().collect();
            out.push((tok, window.trim().to_string()));
        }
    }
    out
}

fn is_wrap_up(title: &str) -> bool {
    let t = title.to_lowercase();
    ["conclusion", "discussion", "summary", "abstract"].iter().any(|k| t.contains(k))
}

/// Figures in a conclusion that no other chapter reports (no AI).
fn unsupported_figures(parts: &[Part]) -> Vec<Finding> {
    let mut known: BTreeSet<String> = BTreeSet::new();
    let mut any = false;
    for part in parts.iter().filter(|p| !is_wrap_up(&p.title)) {
        any = true;
        for p in &part.paras {
            for (tok, _) in scan_figures(p) {
                known.insert(tok);
            }
        }
    }
    let mut out = Vec::new();
    if !any {
        return out;
    }
    for part in parts.iter().filter(|p| is_wrap_up(&p.title)) {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for p in &part.paras {
            for (tok, window) in scan_figures(p) {
                if known.contains(&tok) || !seen.insert(tok.clone()) {
                    continue;
                }
                out.push(Finding {
                    kind: Kind::Unsupported,
                    detail: format!(
                        "{tok} appears in \"{}\" but no other chapter reports it. It may be a typo, a rounding difference, or a result that is not shown.",
                        part.title
                    ),
                    evidence: vec![Evidence { chapter: part.title.clone(), quote: window }],
                });
                if out.len() >= MAX_LOCAL {
                    return out;
                }
            }
        }
    }
    out
}

/// A term the memory records as defined in more than one chapter (no AI).
fn double_definitions(m: &Memory, parts: &[Part]) -> Vec<Finding> {
    let mut by_term: BTreeMap<String, Vec<(&str, &Term)>> = BTreeMap::new();
    for c in &m.chapters {
        for t in &c.terms {
            by_term.entry(t.term.to_lowercase()).or_default().push((c.title.as_str(), t));
        }
    }
    let mut out = Vec::new();
    for (name, defs) in by_term {
        let mut evidence: Vec<Evidence> = Vec::new();
        for (_, t) in &defs {
            if let Some(chapter) = locate(parts, &t.quote)
                && !evidence.iter().any(|e| e.quote == t.quote.trim())
            {
                evidence.push(Evidence { chapter, quote: t.quote.trim().to_string() });
            }
        }
        let chapters: BTreeSet<&str> = evidence.iter().map(|e| e.chapter.as_str()).collect();
        if chapters.len() < 2 {
            continue;
        }
        out.push(Finding {
            kind: Kind::Terminology,
            detail: format!("\"{name}\" is defined in {} places. Make sure the definitions agree, or define it once.", chapters.len()),
            evidence,
        });
        if out.len() >= MAX_LOCAL {
            break;
        }
    }
    out
}

fn memory_json(m: &Memory, per_kind: usize, quote_chars: usize) -> Value {
    let facts = |c: &Chapter, k: FactKind| -> Vec<Value> {
        facts_of(c, k).iter().take(per_kind).map(|f| json!({"text": f.text, "quote": clip_head(&f.quote, quote_chars)})).collect()
    };
    let chapters: Vec<Value> = m
        .chapters
        .iter()
        .map(|c| {
            json!({
                "title": c.title,
                "summary": c.summary,
                "depends_on": c.depends_on,
                "questions": facts(c, FactKind::Questions),
                "objectives": facts(c, FactKind::Objectives),
                "methods": facts(c, FactKind::Methods),
                "results": facts(c, FactKind::Results),
                "conclusions": facts(c, FactKind::Conclusions),
                "terms": c.terms.iter().take(per_kind).map(|t| json!({"term": t.term, "meaning": t.meaning, "quote": clip_head(&t.quote, quote_chars)})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({"chapters": chapters})
}

/// The cross-chapter check by the model; every finding is verified against the document.
fn ai_check(parts: &[Part], memory: &Memory, only: Option<usize>) -> Result<(Vec<Finding>, usize, String), String> {
    let mut text = memory_json(memory, 10, 160).to_string();
    if text.chars().count() > 60_000 {
        text = memory_json(memory, 4, 100).to_string();
    }
    let reply = ai_net::ask(CHECK_PROMPT, &text, only, |t| extract_json(t).is_some_and(|v| v.get("findings").is_some()))?;
    let v = extract_json(&reply.text).ok_or("the reply was not valid JSON")?;
    let mut out = Vec::new();
    let mut dropped = 0;
    for item in v.get("findings").and_then(Value::as_array).into_iter().flatten().take(MAX_FINDINGS) {
        let kind = item.get("kind").and_then(Value::as_str).and_then(Kind::from_key);
        let detail = str_of(item, "detail");
        let mut evidence: Vec<Evidence> = Vec::new();
        for e in item.get("evidence").and_then(Value::as_array).into_iter().flatten().take(4) {
            let quote = str_of(e, "quote");
            if quote.is_empty() || evidence.iter().any(|x| x.quote == quote) {
                continue;
            }
            if let Some(chapter) = locate(parts, &quote) {
                evidence.push(Evidence { chapter, quote });
            }
        }
        match kind {
            Some(kind) if !detail.is_empty() && evidence.len() >= kind.min_evidence() => out.push(Finding { kind, detail, evidence }),
            _ => dropped += 1,
        }
    }
    Ok((out, dropped, reply.label))
}

fn analyse(parts: &[Part], memory: &Memory, only: Option<usize>, local_only: bool) -> Report {
    let mut report = Report::default();
    if parts.is_empty() {
        report.note = Some("There is no text in the document yet.".into());
        return report;
    }
    if local_only {
        report.note = Some("Local checks only: no AI was used.".into());
    } else if memory.chapters.is_empty() {
        report.note = Some("Press Update memory first so the AI check has something to read. Only the local checks ran.".into());
    } else {
        match ai_check(parts, memory, only) {
            Ok((found, dropped, label)) => {
                report.findings.extend(found);
                report.dropped = dropped;
                report.label = label;
            }
            Err(e) => report.note = Some(format!("The AI check could not run, so only the local checks ran.\n{}", clip_head(&e, 400))),
        }
    }
    report.findings.extend(unsupported_figures(parts));
    report.findings.extend(repeated(parts));
    report.findings.extend(double_definitions(memory, parts));
    report.findings.truncate(MAX_FINDINGS);
    report
}

fn report_message(r: &Report) -> String {
    let mut m = if r.findings.is_empty() {
        "Continuity check finished: I found nothing to flag.".to_string()
    } else {
        format!("Continuity check finished: {} possible problem(s).", r.findings.len())
    };
    for f in &r.findings {
        m.push_str(&format!("\n- {}: {}", f.kind.label(), f.detail));
    }
    if r.dropped > 0 {
        m.push_str(&format!("\n\nI dropped {} suggestion(s) whose quotes are not in your document.", r.dropped));
    }
    if let Some(n) = &r.note {
        m.push_str(&format!("\n\n{n}"));
    }
    m.push_str(
        "\n\nEvery finding quotes your document: use Show in the Research context section to jump to it. Nothing in your document was changed.",
    );
    if !r.label.is_empty() {
        m.push_str(&format!(" Checked by {}.", r.label));
    }
    m
}

// ---------------------------------------------------------------------------------------------
// Saving next to the document
// ---------------------------------------------------------------------------------------------

fn store_path(s: &Session) -> Option<PathBuf> {
    let p = s.path.as_ref()?;
    let mut name = p.file_name()?.to_os_string();
    name.push(FILE_SUFFIX);
    Some(p.with_file_name(name))
}

fn load_memory(path: &std::path::Path) -> Result<Memory, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// `Ok(None)` when the document has not been saved yet (there is no place to keep the memory).
fn save_memory(s: &Session, m: &Memory) -> Result<Option<PathBuf>, String> {
    let Some(path) = store_path(s) else { return Ok(None) };
    let json = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

// ---------------------------------------------------------------------------------------------
// The panel section and its plumbing
// ---------------------------------------------------------------------------------------------

enum Job {
    Updated(Result<(Memory, Stats), String>),
    Checked(Report),
}

pub struct State {
    memory: Memory,
    loaded_for: Option<PathBuf>,
    report: Option<Report>,
    decision_input: String,
    local_only: bool,
    write_with_memory: bool,
    busy: Option<Receiver<Job>>,
    progress: Option<Arc<Progress>>,
    update_request: bool,
    check_request: bool,
    clear_request: bool,
    add_decision: Option<String>,
    remove_decision: Option<usize>,
    show_request: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            memory: Memory::default(),
            loaded_for: None,
            report: None,
            decision_input: String::new(),
            local_only: false,
            write_with_memory: true,
            busy: None,
            progress: None,
            update_request: false,
            check_request: false,
            clear_request: false,
            add_decision: None,
            remove_decision: None,
            show_request: None,
        }
    }
}

impl State {
    /// What to put in front of a writing request, so new text agrees with the memory.
    pub fn prompt_block(&self) -> Option<String> {
        if self.write_with_memory { self.memory.prompt_block() } else { None }
    }
}

fn amber() -> Color32 {
    Color32::from_rgb(214, 160, 20)
}

fn fact_section(ui: &mut egui::Ui, t: &Tokens, memory: &Memory, kind: FactKind, show: &mut Option<String>) {
    let items = memory.facts(kind);
    if items.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(format!("{} ({})", kind.label(), items.len())).id_salt(("word-ai-context-facts", kind.label())).show(ui, |ui| {
        for (chapter, f) in items {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{chapter}:")).small().color(t.text_dim));
                ui.add(egui::Label::new(RichText::new(f.text.as_str()).small()).wrap());
                if ui.small_button("Show").clicked() {
                    *show = Some(f.quote.clone());
                }
            });
        }
    });
}

/// The "Research context" section of the Word panel.
pub fn ui(ui: &mut egui::Ui, t: &Tokens, st: &mut State) {
    egui::CollapsingHeader::new("Research context").default_open(false).show(ui, |ui| {
        ui.label(
            RichText::new(
                "Word remembers each chapter's questions, objectives, methods, results, conclusions and terms, and checks the chapters against each other. Every finding quotes your document.",
            )
            .small()
            .color(t.text_dim),
        );
        let busy = st.busy.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Update memory"))
                .on_hover_text("Reads the chapters that are new or changed since the last update. Chapters are the Heading 1 sections.")
                .clicked()
            {
                st.update_request = true;
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Check continuity"))
                .on_hover_text("Looks for contradictions, repeated explanations, unsupported conclusions and results that disagree with conclusions.")
                .clicked()
            {
                st.check_request = true;
            }
        });
        ui.checkbox(&mut st.local_only, "Local checks only (no AI)").on_hover_text("Repeated passages, figures in a conclusion that no other chapter reports, and terms defined twice.");
        ui.checkbox(&mut st.write_with_memory, "Use the memory when Word writes new text")
            .on_hover_text("Create document tells the model what the memory holds, so new text keeps your terms and results. Not used with Research with sources.");
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                let (done, total) = st.progress.as_ref().map_or((0, 0), |p| (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed)));
                let msg = if total > 0 { format!("Word is reading chapter {} of {}...", (done + 1).min(total), total) } else { "Word is working...".to_string() };
                ui.label(RichText::new(msg).color(t.text_dim));
            });
        }
        if st.loaded_for.is_none() && !st.memory.is_empty() {
            ui.label(RichText::new("Save the document to keep this memory between sessions.").small().color(amber()));
        }

        if !st.memory.chapters.is_empty() {
            ui.separator();
            ui.label(RichText::new(format!("{} chapter(s) remembered", st.memory.chapters.len())).strong());
            for kind in FactKind::ALL {
                fact_section(ui, t, &st.memory, kind, &mut st.show_request);
            }
            let terms = st.memory.term_count();
            if terms > 0 {
                egui::CollapsingHeader::new(format!("Terminology ({terms})")).id_salt("word-ai-context-terms").show(ui, |ui| {
                    for c in &st.memory.chapters {
                        for term in &c.terms {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(format!("{}:", term.term)).small().strong());
                                ui.add(egui::Label::new(RichText::new(term.meaning.as_str()).small()).wrap());
                                if ui.small_button("Show").clicked() {
                                    st.show_request = Some(term.quote.clone());
                                }
                            });
                        }
                    }
                });
            }
            egui::CollapsingHeader::new("How the chapters connect").id_salt("word-ai-context-links").show(ui, |ui| {
                for c in &st.memory.chapters {
                    ui.add(egui::Label::new(RichText::new(c.title.as_str()).small().strong()).wrap());
                    if !c.summary.is_empty() {
                        ui.add(egui::Label::new(RichText::new(c.summary.as_str()).small().color(t.text_dim)).wrap());
                    }
                    if !c.depends_on.is_empty() {
                        ui.add(egui::Label::new(RichText::new(format!("builds on: {}", c.depends_on.join(", "))).small().color(t.text_dim)).wrap());
                    }
                    ui.add_space(2.0);
                }
            });
        }

        ui.separator();
        ui.label(RichText::new(format!("Writing decisions ({})", st.memory.decisions.len())).small().strong());
        let mut remove: Option<usize> = None;
        for (i, d) in st.memory.decisions.iter().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(RichText::new(d.as_str()).small()).wrap());
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if remove.is_some() {
            st.remove_decision = remove;
        }
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 60.0).max(120.0);
            ui.add(egui::TextEdit::singleline(&mut st.decision_input).desired_width(width).hint_text("e.g. Use British spelling; say \"respondents\""));
            if ui.add_enabled(!st.decision_input.trim().is_empty(), egui::Button::new("Add")).clicked() {
                st.add_decision = Some(st.decision_input.trim().to_string());
                st.decision_input.clear();
            }
        });
        if ui.add_enabled(!busy && !st.memory.is_empty(), egui::Button::new("Clear memory")).clicked() {
            st.clear_request = true;
        }

        if let Some(r) = &st.report {
            ui.separator();
            ui.label(RichText::new(format!("{} possible problem(s)", r.findings.len())).strong());
            if let Some(n) = &r.note {
                ui.add(egui::Label::new(RichText::new(n.as_str()).small().color(t.text_dim)).wrap());
            }
            egui::ScrollArea::vertical().id_salt("word-ai-context-scroll").max_height(280.0).auto_shrink([false, true]).show(ui, |ui| {
                for f in &r.findings {
                    ui.add_space(4.0);
                    let colour = if f.kind == Kind::Contradiction || f.kind == Kind::ResultMismatch { t.red } else { amber() };
                    ui.label(RichText::new(f.kind.label()).strong().color(colour));
                    ui.add(egui::Label::new(RichText::new(f.detail.as_str()).small()).wrap());
                    for e in &f.evidence {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("{}:", e.chapter)).small().color(t.text_dim));
                            ui.add(egui::Label::new(RichText::new(format!("\"{}\"", e.quote)).small().italics()).wrap());
                            if ui.small_button("Show").clicked() {
                                st.show_request = Some(e.quote.clone());
                            }
                        });
                    }
                }
            });
        }
    });
}

fn spawn_job(ctx: &egui::Context, work: impl FnOnce() -> Job + Send + 'static) -> Result<Receiver<Job>, String> {
    let (tx, rx) = channel();
    let wake = ctx.clone();
    std::thread::Builder::new()
        .name("word-ai-context".into())
        .spawn(move || {
            let _ = tx.send(work());
            wake.request_repaint();
        })
        .map(|_| rx)
        .map_err(|e| e.to_string())
}

/// Called every frame from `ai_word::show`: storage, requests from the panel, finished jobs.
pub fn drive(app: &mut WordApp, ctx: &egui::Context, st: &mut State, only: Option<usize>, messages: &mut Vec<(bool, String)>) {
    // The memory lives with the document: load it when the document changes.
    let now = store_path(&app.session);
    if now != st.loaded_for {
        // An unsaved document that has just been saved keeps its memory; any other change of
        // document starts from what is saved for it.
        let adopt = st.loaded_for.is_none() && now.is_some() && !st.memory.is_empty();
        st.loaded_for = now.clone();
        st.report = None;
        if adopt {
            if let Err(e) = save_memory(&app.session, &st.memory) {
                messages.push((false, format!("I couldn't save the research memory.\n{e}")));
            }
        } else {
            st.memory = Memory::default();
            if let Some(p) = &now
                && p.exists()
            {
                match load_memory(p) {
                    Ok(m) => st.memory = m,
                    Err(e) => messages.push((false, format!("I couldn't read the research memory saved with this document.\n{e}"))),
                }
            }
        }
    }

    if let Some(quote) = st.show_request.take() {
        match app.session.run("select.text", &json!({"text": quote})) {
            Ok(_) => app.canvas.scroll_to_caret = true,
            Err(_) => messages.push((false, "I couldn't find that passage any more. The document has changed since the memory was made.".into())),
        }
        ctx.request_repaint();
    }

    let mut changed = false;
    if let Some(d) = st.add_decision.take() {
        let d = clip_head(&d, MAX_DECISION);
        if !st.memory.decisions.contains(&d) {
            st.memory.decisions.push(d);
            changed = true;
        }
    }
    if let Some(i) = st.remove_decision.take()
        && i < st.memory.decisions.len()
    {
        st.memory.decisions.remove(i);
        changed = true;
    }
    if st.clear_request && st.busy.is_none() {
        st.clear_request = false;
        st.memory = Memory::default();
        st.report = None;
        if let Some(p) = store_path(&app.session) {
            let _ = std::fs::remove_file(p);
        }
        messages.push((false, "Research memory cleared.".into()));
    }
    if changed && let Err(e) = save_memory(&app.session, &st.memory) {
        messages.push((false, format!("I couldn't save the research memory.\n{e}")));
    }

    // Start a job.
    if st.update_request {
        st.update_request = false;
        if st.busy.is_none() {
            messages.push((true, "Update research memory".into()));
            let parts = split_parts(&app.session);
            let memory = st.memory.clone();
            let progress = Arc::new(Progress::default());
            let shared = Arc::clone(&progress);
            match spawn_job(ctx, move || Job::Updated(update(&parts, &memory, only, &shared))) {
                Ok(rx) => {
                    st.busy = Some(rx);
                    st.progress = Some(progress);
                }
                Err(e) => messages.push((false, format!("Couldn't start the request: {e}"))),
            }
        }
    }
    if st.check_request {
        st.check_request = false;
        if st.busy.is_none() {
            messages.push((true, "Check continuity".into()));
            let parts = split_parts(&app.session);
            let memory = st.memory.clone();
            let local_only = st.local_only;
            match spawn_job(ctx, move || Job::Checked(analyse(&parts, &memory, only, local_only))) {
                Ok(rx) => st.busy = Some(rx),
                Err(e) => messages.push((false, format!("Couldn't start the request: {e}"))),
            }
        }
    }

    // Receive a finished job.
    if let Some(rx) = &st.busy {
        match rx.try_recv() {
            Ok(job) => {
                st.busy = None;
                st.progress = None;
                match job {
                    Job::Updated(Ok((mut memory, stats))) => {
                        // Decisions typed while the chapters were being read are kept.
                        memory.decisions = st.memory.decisions.clone();
                        let mut msg = update_message(&memory, &stats);
                        st.memory = memory;
                        st.report = None;
                        match save_memory(&app.session, &st.memory) {
                            Ok(Some(path)) => msg.push_str(&format!("\nSaved to {}.", path.display())),
                            Ok(None) => msg.push_str("\nSave the document to keep this memory between sessions."),
                            Err(e) => msg.push_str(&format!("\nI couldn't save the memory: {e}")),
                        }
                        messages.push((false, msg));
                    }
                    Job::Updated(Err(e)) => messages.push((false, format!("I couldn't update the research memory.\n{e}"))),
                    Job::Checked(report) => {
                        messages.push((false, report_message(&report)));
                        st.report = Some(report);
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

    fn part(title: &str, paras: &[&str]) -> Part {
        let paras: Vec<String> = paras.iter().map(|s| s.to_string()).collect();
        Part { title: title.into(), text: paras.join("\n"), lines: paras.clone(), paras }
    }

    #[test]
    fn a_digest_keeps_only_what_quotes_the_chapter() {
        let p = part("Results", &["Accuracy rose to 91.5% on the test set."]);
        let reply = r#"{"summary":"s","results":[{"text":"accuracy 91.5%","quote":"Accuracy rose to 91.5% on the test set."},{"text":"made up","quote":"Accuracy was 99%"}]}"#;
        let (c, dropped) = parse_digest(reply, &p, 1, &["Results".to_string()]).unwrap();
        assert_eq!(c.results.len(), 1);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn a_figure_in_a_conclusion_needs_a_result() {
        let parts = vec![
            part("Results", &["Accuracy was 91.5% overall."]),
            part("Conclusion", &["We reached 95.2% accuracy.", "An accuracy of 91.50% is good."]),
        ];
        let found = unsupported_figures(&parts);
        assert_eq!(found.len(), 1);
        assert!(found[0].detail.contains("95.2"));
    }

    #[test]
    fn repeated_explanations_in_two_chapters_are_found() {
        let text = "Gradient descent updates every parameter by stepping against the slope of the loss, using a small learning rate so that the updates stay stable and the loss decreases from one step to the next over many training epochs.";
        let parts = vec![part("Background", &[text]), part("Method", &[text])];
        assert_eq!(repeated(&parts).len(), 1);
    }

    #[test]
    fn quotes_are_found_in_the_chapter_that_holds_them() {
        let parts = vec![part("One", &["Alpha beta gamma."]), part("Two", &["Delta epsilon."])];
        assert_eq!(locate(&parts, "Delta epsilon").as_deref(), Some("Two"));
        assert_eq!(locate(&parts, "Zeta"), None);
    }

    #[test]
    fn the_memory_survives_a_round_trip() {
        let m = Memory {
            version: 1,
            decisions: vec!["British spelling".into()],
            chapters: vec![Chapter { title: "A".into(), hash: u64::MAX, ..Chapter::default() }],
        };
        let back: Memory = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}
