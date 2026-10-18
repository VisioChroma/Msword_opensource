//! "Word" AI, part 4: official guidelines and compliance.
//!
//! Guidelines come from three places: a rule set built into WordCraft (APA 7, MLA 9), an uploaded
//! file (PDF, TXT, MD) or link, or a web search for an institution's own pages. The model turns
//! the guideline into structured rules, but a rule is only *enforced* when it quotes the guideline
//! and the quote contains the number or name the rule reports (and, for text files, when the quote
//! really is in the file). Everything else is listed as unverified. Two different values for the
//! same requirement are flagged as a conflict, never resolved by guessing.
//!
//! The document is then checked locally (no AI) against the enforced rules. Nothing is changed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{Color32, RichText};
use serde_json::Value;
use wordcraft_doc::props::LineSpacing;
use wordcraft_doc::{Document, Paragraph, Style};
use wordcraft_engine::Session;

use crate::WordApp;
use crate::ai_plan::extract_json;
use crate::file_dialogs::{AfterPick, FileDialogRequest};
use crate::theme::Tokens;

const MAX_RULES: usize = 40;
const MAX_QUOTE: usize = 300;

pub const SYSTEM_PROMPT: &str = r#"You are Word, the compliance assistant inside the WordCraft document editor.
You receive a formatting or submission guideline (a PDF, a text file, or pages found by a web search) and turn it into structured rules.

Reply with ONE JSON object and nothing else (no markdown fences, no commentary):
{
  "title": "name of the guideline, e.g. Thesis formatting handbook",
  "rules": [
    {"key": "margins", "value": {"top": 1, "bottom": 1, "left": 1.5, "right": 1}, "unit": "in", "quote": "...", "page": 3},
    {"key": "margins", "value": {"all": 2.5}, "unit": "cm", "quote": "...", "page": 3},
    {"key": "fonts", "value": ["Times New Roman", "Arial"], "quote": "...", "page": 3},
    {"key": "body_size", "value": {"min": 11, "max": 12}, "quote": "...", "page": 3},
    {"key": "line_spacing", "value": 2, "quote": "...", "page": 3},
    {"key": "citation_style", "value": "numeric" | "author-year" | "author-page" | "footnote", "quote": "...", "page": 5},
    {"key": "page_limit", "value": 10, "unit": "pages", "excludes": "references and appendices", "quote": "...", "page": 1},
    {"key": "heading_levels", "value": {"max_level": 3, "hierarchical": true}, "quote": "...", "page": 4},
    {"key": "required_sections", "value": ["Abstract", "Introduction", "References"], "quote": "...", "page": 2}
  ],
  "unverified": ["each requirement that is vague, conditional, in a unit you cannot map, or outside the keys above (word counts, file formats, anonymisation...)"],
  "conflicts": ["each place where the guideline contradicts itself, naming both values and pages"]
}

Rules:
- Include a rule only if the guideline states it explicitly. Never fill gaps from what is typical for this publisher, university or field. A missing rule is correct; an invented rule is a failure.
- "quote" is copied word for word from the guideline (under 250 characters) and contains the number or name you report. Do not paraphrase it.
- Report numbers in the unit the guideline uses (in, cm, mm or pt for margins; points for sizes). Never convert.
- If the guideline states the same requirement twice with different values, output both as separate rules and also describe it under "conflicts". Do not pick one.
- line_spacing is a multiple (single = 1, double = 2). If the guideline gives points instead, list it under "unverified".
- "page" is the page number of the quote in the document, when you can tell; otherwise leave it out.
- Do not write or change any text of the user's document."#;

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

/// The things WordCraft can check, in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    Margins,
    Fonts,
    Spacing,
    Citations,
    PageLimit,
    Headings,
    Sections,
}

impl Topic {
    pub const ALL: [Topic; 7] = [Topic::Margins, Topic::Fonts, Topic::Spacing, Topic::Citations, Topic::PageLimit, Topic::Headings, Topic::Sections];

    pub fn label(self) -> &'static str {
        match self {
            Topic::Margins => "Margins",
            Topic::Fonts => "Fonts",
            Topic::Spacing => "Spacing",
            Topic::Citations => "Citations",
            Topic::PageLimit => "Page limit",
            Topic::Headings => "Headings",
            Topic::Sections => "Required sections",
        }
    }

    fn from_key(key: &str) -> Option<Topic> {
        match key {
            "margins" | "margin" => Some(Topic::Margins),
            "fonts" | "font" | "body_size" | "font_size" => Some(Topic::Fonts),
            "line_spacing" | "spacing" => Some(Topic::Spacing),
            "citation_style" | "citations" => Some(Topic::Citations),
            "page_limit" => Some(Topic::PageLimit),
            "heading_levels" | "headings" => Some(Topic::Headings),
            "required_sections" | "sections" => Some(Topic::Sections),
            _ => None,
        }
    }
}

/// A rule and the words of the guideline it rests on.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule<T> {
    pub value: T,
    pub quote: String,
    pub page: Option<u32>,
    pub source: String,
}

/// Page margins in points; `None` = the guideline doesn't say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Margins {
    pub top: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
    pub right: Option<f32>,
}

impl Margins {
    fn is_empty(&self) -> bool {
        self.top.is_none() && self.bottom.is_none() && self.left.is_none() && self.right.is_none()
    }

    fn describe(&self) -> String {
        let side = |v: Option<f32>| v.map_or("-".to_string(), inches);
        match (self.top, self.bottom, self.left, self.right) {
            (Some(t), Some(b), Some(l), Some(r)) if t == b && t == l && t == r => format!("{} on all sides", inches(t)),
            _ => format!("top {}, bottom {}, left {}, right {}", side(self.top), side(self.bottom), side(self.left), side(self.right)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SizeRange {
    pub min: f32,
    pub max: f32,
}

impl SizeRange {
    fn describe(&self) -> String {
        if (self.max - self.min).abs() < 0.05 { format!("{} pt", num(self.min)) } else { format!("{}-{} pt", num(self.min), num(self.max)) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiteStyle {
    Numeric,
    AuthorYear,
    AuthorPage,
    Footnote,
}

impl CiteStyle {
    fn label(self) -> &'static str {
        match self {
            CiteStyle::Numeric => "numbered, like [1]",
            CiteStyle::AuthorYear => "author-year, like (Smith, 2020)",
            CiteStyle::AuthorPage => "author-page, like (Smith 45)",
            CiteStyle::Footnote => "footnotes",
        }
    }

    fn parse(s: &str) -> Option<CiteStyle> {
        match s.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "numeric" | "numbered" | "ieee" | "vancouver" => Some(CiteStyle::Numeric),
            "author-year" | "apa" | "harvard" => Some(CiteStyle::AuthorYear),
            "author-page" | "mla" => Some(CiteStyle::AuthorPage),
            "footnote" | "footnotes" | "endnote" | "endnotes" => Some(CiteStyle::Footnote),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageLimit {
    pub pages: u32,
    /// What the guideline says is not counted (empty = nothing).
    pub excludes: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeadingRule {
    pub max_level: Option<u8>,
    /// No skipped levels (a Heading 3 straight after a Heading 1).
    pub hierarchical: bool,
}

/// Structured rules from one or more guidelines. Only verified rules are in the `Option` fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    pub title: String,
    pub origin: String,
    pub margins: Option<Rule<Margins>>,
    pub fonts: Option<Rule<Vec<String>>>,
    pub size: Option<Rule<SizeRange>>,
    pub spacing: Option<Rule<f32>>,
    pub citations: Option<Rule<CiteStyle>>,
    pub page_limit: Option<Rule<PageLimit>>,
    pub headings: Option<Rule<HeadingRule>>,
    pub sections: Option<Rule<Vec<String>>>,
    /// Unverified or conflicting requirements tied to one topic.
    pub flags: Vec<(Topic, String)>,
    /// Other requirements WordCraft can't check or confirm.
    pub notes: Vec<String>,
    /// Web pages the rules came from (search results): open them and confirm.
    pub pages: Vec<String>,
}

impl Rules {
    pub fn name(&self) -> String {
        match (self.title.is_empty(), self.origin.is_empty()) {
            (false, false) => format!("{} ({})", self.title, self.origin),
            (false, true) => self.title.clone(),
            _ => self.origin.clone(),
        }
    }

    fn has(&self, t: Topic) -> bool {
        match t {
            Topic::Margins => self.margins.is_some(),
            Topic::Fonts => self.fonts.is_some() || self.size.is_some(),
            Topic::Spacing => self.spacing.is_some(),
            Topic::Citations => self.citations.is_some(),
            Topic::PageLimit => self.page_limit.is_some(),
            Topic::Headings => self.headings.is_some(),
            Topic::Sections => self.sections.is_some(),
        }
    }

    /// What the guideline asks for, in a few words (None: nothing enforceable).
    pub fn describe(&self, t: Topic) -> Option<String> {
        match t {
            Topic::Margins => self.margins.as_ref().map(|r| r.value.describe()),
            Topic::Fonts => {
                let fonts = self.fonts.as_ref().map(|r| r.value.join(", "));
                let size = self.size.as_ref().map(|r| r.value.describe());
                match (fonts, size) {
                    (Some(f), Some(s)) => Some(format!("{f}, {s}")),
                    (Some(x), None) | (None, Some(x)) => Some(x),
                    (None, None) => None,
                }
            }
            Topic::Spacing => self.spacing.as_ref().map(|r| format!("{} line spacing", num(r.value))),
            Topic::Citations => self.citations.as_ref().map(|r| r.value.label().to_string()),
            Topic::PageLimit => self.page_limit.as_ref().map(|r| {
                if r.value.excludes.is_empty() {
                    format!("at most {} pages", r.value.pages)
                } else {
                    format!("at most {} pages, excluding {}", r.value.pages, r.value.excludes)
                }
            }),
            Topic::Headings => self.headings.as_ref().map(|r| {
                let mut parts = Vec::new();
                if let Some(m) = r.value.max_level {
                    parts.push(format!("up to Heading {m}"));
                }
                if r.value.hierarchical {
                    parts.push("no skipped levels".to_string());
                }
                parts.join(", ")
            }),
            Topic::Sections => self.sections.as_ref().map(|r| r.value.join(", ")),
        }
    }

    /// The chat message shown after loading.
    fn summary(&self) -> String {
        let got: Vec<&str> = Topic::ALL.iter().filter(|t| self.has(**t)).map(|t| t.label()).collect();
        let mut m = if got.is_empty() {
            format!("I read {}, but found no rule I could confirm from its own words.", self.name())
        } else {
            format!("Rules from {}: {}.", self.name(), got.join(", "))
        };
        let flagged = self.flags.len() + self.notes.len();
        if flagged > 0 {
            m.push_str(&format!("\n{flagged} requirement(s) are unconfirmed or conflicting. I will flag them and not enforce them."));
        }
        m
    }

    fn clear(&mut self, t: Topic) {
        match t {
            Topic::Margins => self.margins = None,
            Topic::Fonts => {
                self.fonts = None;
                self.size = None;
            }
            Topic::Spacing => self.spacing = None,
            Topic::Citations => self.citations = None,
            Topic::PageLimit => self.page_limit = None,
            Topic::Headings => self.headings = None,
            Topic::Sections => self.sections = None,
        }
    }

    /// Rules of a second guideline: equal values stay, differing values become a conflict and
    /// neither is enforced.
    pub fn merge(&mut self, o: Rules) {
        merge_one(Topic::Margins, &mut self.margins, o.margins, Margins::describe, &mut self.flags);
        merge_one(Topic::Fonts, &mut self.fonts, o.fonts, |v: &Vec<String>| v.join(", "), &mut self.flags);
        merge_one(Topic::Fonts, &mut self.size, o.size, SizeRange::describe, &mut self.flags);
        merge_one(Topic::Spacing, &mut self.spacing, o.spacing, |v: &f32| num(*v), &mut self.flags);
        merge_one(Topic::Citations, &mut self.citations, o.citations, |v: &CiteStyle| v.label().to_string(), &mut self.flags);
        merge_one(Topic::PageLimit, &mut self.page_limit, o.page_limit, |v: &PageLimit| format!("{} pages", v.pages), &mut self.flags);
        merge_one(Topic::Headings, &mut self.headings, o.headings, |v: &HeadingRule| format!("{:?}", v.max_level), &mut self.flags);
        merge_one(Topic::Sections, &mut self.sections, o.sections, |v: &Vec<String>| v.join(", "), &mut self.flags);
        self.flags.extend(o.flags);
        self.notes.extend(o.notes);
        self.pages.extend(o.pages);
        if !o.title.is_empty() {
            self.title = if self.title.is_empty() { o.title } else { format!("{} + {}", self.title, o.title) };
        }
        self.origin = format!("{} + {}", self.origin, o.origin);
        let conflicted: Vec<Topic> = self.flags.iter().filter(|(_, m)| m.starts_with("conflicting")).map(|(t, _)| *t).collect();
        for t in conflicted {
            self.clear(t);
        }
    }
}

fn merge_one<T: PartialEq>(
    topic: Topic,
    mine: &mut Option<Rule<T>>,
    theirs: Option<Rule<T>>,
    describe: impl Fn(&T) -> String,
    flags: &mut Vec<(Topic, String)>,
) {
    let Some(theirs) = theirs else { return };
    if flags.iter().any(|(t, m)| *t == topic && m.starts_with("conflicting")) {
        return;
    }
    match mine.take() {
        None => *mine = Some(theirs),
        Some(m) if m.value == theirs.value => *mine = Some(m),
        Some(m) => flags
            .push((topic, format!("conflicting values: {} ({}) vs {} ({}).", describe(&m.value), m.source, describe(&theirs.value), theirs.source))),
    }
}

/// One value per topic: identical repeats collapse, differing repeats are a conflict (flagged, no rule).
fn settle<T: PartialEq>(topic: Topic, list: Vec<Rule<T>>, describe: impl Fn(&T) -> String, flags: &mut Vec<(Topic, String)>) -> Option<Rule<T>> {
    let mut it = list.into_iter();
    let first = it.next()?;
    let differing: Vec<Rule<T>> = it.filter(|r| r.value != first.value).collect();
    if differing.is_empty() {
        return Some(first);
    }
    let at = |p: Option<u32>| p.map_or(String::new(), |p| format!(" (p. {p})"));
    let mut msg = format!("conflicting values: {}{}", describe(&first.value), at(first.page));
    for r in &differing {
        msg.push_str(&format!(" vs {}{}", describe(&r.value), at(r.page)));
    }
    msg.push_str(". Not enforced until you decide which applies.");
    flags.push((topic, msg));
    None
}

fn num(x: f32) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn inches(pt: f32) -> String {
    format!("{} in", num(pt / 72.0))
}

// ---------------------------------------------------------------------------------------------
// Reading the model's answer (every number bounded, every rule needs its quote)
// ---------------------------------------------------------------------------------------------

/// Margin values to points, rounded to half a point (so 1 in and 2.54 cm agree).
fn to_pt(value: f64, unit: &str) -> Option<f32> {
    let pt = match unit.trim().to_ascii_lowercase().as_str() {
        "in" | "inch" | "inches" | "\"" => value * 72.0,
        "cm" => value * 72.0 / 2.54,
        "mm" => value * 72.0 / 25.4,
        "pt" | "point" | "points" => value,
        _ => return None,
    };
    let pt = (pt * 2.0).round() / 2.0;
    (pt.is_finite() && pt > 0.0 && pt <= 400.0).then_some(pt as f32)
}

fn numbers_in(text: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() || (c == '.' && !cur.is_empty() && !cur.contains('.')) {
            cur.push(c);
            continue;
        }
        if !cur.is_empty() {
            if let Ok(n) = cur.trim_end_matches('.').parse::<f64>() {
                out.push(n);
            }
            cur.clear();
        }
    }
    out
}

fn word_value(w: &str) -> Option<f64> {
    Some(match w {
        "single" | "one" => 1.0,
        "double" | "two" => 2.0,
        "triple" | "three" => 3.0,
        "four" => 4.0,
        "five" => 5.0,
        "six" => 6.0,
        "seven" => 7.0,
        "eight" => 8.0,
        "nine" => 9.0,
        "ten" => 10.0,
        "eleven" => 11.0,
        "twelve" => 12.0,
        "fifteen" => 15.0,
        "twenty" => 20.0,
        "thirty" => 30.0,
        _ => return None,
    })
}

/// Whether the quoted guideline text contains this number (as digits or as a word).
fn quote_has(quote: &str, want: f64) -> bool {
    let near = |x: f64| (x - want).abs() < 0.011;
    if numbers_in(quote).into_iter().any(near) {
        return true;
    }
    let lower = quote.to_lowercase().replace('-', " ");
    if (want - 1.5).abs() < 0.011 && lower.contains("one and a half") {
        return true;
    }
    lower.split(|c: char| !c.is_alphabetic()).filter_map(word_value).any(near)
}

fn norm(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' | '\u{00ad}' => '-',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn section_variants(name: &str) -> Vec<String> {
    const REFS: &[&str] = &["references", "reference list", "bibliography", "works cited", "literature cited"];
    let n = name.trim().to_lowercase();
    if REFS.contains(&n.as_str()) { REFS.iter().map(|s| (*s).to_string()).collect() } else { vec![n] }
}

fn mk<T>(value: T, quote: &str, page: Option<u32>, source: &str) -> Rule<T> {
    Rule { value, quote: quote.to_string(), page, source: source.to_string() }
}

fn strings(v: &Value, max: usize, len: usize) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter().filter_map(Value::as_str).map(|s| s.trim().chars().take(len).collect::<String>()).filter(|s| !s.is_empty()).take(max).collect()
        })
        .unwrap_or_default()
}

/// Check and bound a model reply. `source_text`: the guideline's own text when we have it (a text
/// file), so quotes can be matched against it.
pub fn parse_rules(reply: &str, origin: &str, source_text: Option<&str>) -> Result<Rules, String> {
    let v = extract_json(reply).ok_or("the reply was not valid guideline rules")?;
    let empty: Vec<Value> = Vec::new();
    let list = v.get("rules").and_then(Value::as_array).unwrap_or(&empty);
    let null = Value::Null;
    let haystack = source_text.map(norm);

    let title: String = v.get("title").and_then(Value::as_str).unwrap_or("").trim().chars().take(120).collect();
    let mut r = Rules { title, origin: origin.to_string(), ..Rules::default() };
    let (mut margins, mut fonts, mut sizes, mut spacings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut cites, mut limits, mut heads, mut secs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for item in list.iter().take(MAX_RULES) {
        let key = item.get("key").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
        let Some(topic) = Topic::from_key(&key) else { continue };
        let quote: String = item
            .get("quote")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_QUOTE)
            .collect();
        let page = item.get("page").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()).filter(|p| (1..10_000).contains(p));
        let val = item.get("value").unwrap_or(&null);
        let label = topic.label().to_lowercase();
        let nq = norm(&quote);

        if quote.is_empty() {
            r.flags.push((topic, format!("{label}: the guideline wasn't quoted, so I did not enforce it.")));
            continue;
        }
        if !haystack.as_deref().is_none_or(|h| h.contains(nq.as_str())) {
            r.flags.push((topic, format!("{label}: the quoted text is not in the file, so I did not enforce it.")));
            continue;
        }
        let ql = quote.to_lowercase();

        match key.as_str() {
            "margins" | "margin" => {
                let unit = item.get("unit").and_then(Value::as_str).unwrap_or("");
                let all = val.get("all").and_then(Value::as_f64);
                let mut raws: Vec<f64> = Vec::new();
                let mut side = |k: &str| {
                    let x = val.get(k).and_then(Value::as_f64).or(all);
                    if let Some(n) = x {
                        raws.push(n);
                    }
                    x.and_then(|n| to_pt(n, unit))
                };
                let m = Margins { top: side("top"), bottom: side("bottom"), left: side("left"), right: side("right") };
                if m.is_empty() {
                    r.flags.push((topic, "margins: I could not read the values or the unit, so I did not enforce them.".into()));
                } else if !raws.iter().all(|n| quote_has(&quote, *n)) {
                    r.flags.push((topic, "margins: the quote does not contain every number reported, so I did not enforce them.".into()));
                } else {
                    margins.push(mk(m, &quote, page, origin));
                }
            }
            "fonts" | "font" => {
                let asked = strings(val, 8, 60);
                let (found, missing): (Vec<String>, Vec<String>) = asked.into_iter().partition(|f| ql.contains(f.to_lowercase().as_str()));
                if !missing.is_empty() {
                    r.flags.push((topic, format!("fonts: {} not found in the quote, left out.", missing.join(", "))));
                }
                if !found.is_empty() {
                    fonts.push(mk(found, &quote, page, origin));
                } else if missing.is_empty() {
                    r.flags.push((topic, "fonts: no font names were given.".into()));
                }
            }
            "body_size" | "font_size" => {
                let min = val.get("min").and_then(Value::as_f64).or(val.as_f64());
                let max = val.get("max").and_then(Value::as_f64).or(val.as_f64()).or(min);
                let min = min.or(max);
                match (min, max) {
                    (Some(a), Some(b)) => {
                        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                        if !(4.0..=72.0).contains(&lo) || !(4.0..=72.0).contains(&hi) {
                            r.flags.push((topic, "size: the values are not plausible font sizes, so I did not enforce them.".into()));
                        } else if !quote_has(&quote, lo) || !quote_has(&quote, hi) {
                            r.flags.push((topic, "size: the quote does not contain the sizes reported, so I did not enforce them.".into()));
                        } else {
                            sizes.push(mk(SizeRange { min: lo as f32, max: hi as f32 }, &quote, page, origin));
                        }
                    }
                    _ => r.flags.push((topic, "size: I could not read the sizes.".into())),
                }
            }
            "line_spacing" | "spacing" => match val.as_f64() {
                Some(n) if (0.5..=4.0).contains(&n) && quote_has(&quote, n) => {
                    spacings.push(mk(((n * 100.0).round() / 100.0) as f32, &quote, page, origin))
                }
                Some(_) => r.flags.push((
                    topic,
                    "spacing: the quote does not contain the value reported (or it is not a line-spacing multiple), so I did not enforce it.".into(),
                )),
                None => r.flags.push((topic, "spacing: I could not read the value.".into())),
            },
            "citation_style" | "citations" => {
                let style = val.as_str().and_then(CiteStyle::parse);
                let mentions =
                    ["cit", "refer", "apa", "mla", "ieee", "chicago", "harvard", "vancouver", "numbered", "footnote", "endnote", "author", "["]
                        .iter()
                        .any(|k| ql.contains(*k));
                match style {
                    Some(s) if mentions => cites.push(mk(s, &quote, page, origin)),
                    _ => r.flags.push((topic, "citations: I could not confirm the citation style from the quote, so I did not enforce it.".into())),
                }
            }
            "page_limit" => {
                let unit = item.get("unit").and_then(Value::as_str).unwrap_or("pages").to_ascii_lowercase();
                let n = val.as_f64().filter(|n| n.fract() == 0.0 && (1.0..=2000.0).contains(n));
                let excludes: String = item.get("excludes").and_then(Value::as_str).unwrap_or("").trim().chars().take(80).collect();
                match n {
                    Some(n) if unit.starts_with("page") && quote_has(&quote, n) => {
                        limits.push(mk(PageLimit { pages: n as u32, excludes }, &quote, page, origin))
                    }
                    Some(_) if !unit.starts_with("page") => {
                        r.flags.push((topic, format!("page limit: the limit is in {unit}, not pages, so I did not enforce it.")))
                    }
                    _ => r.flags.push((topic, "page limit: the quote does not contain the number reported, so I did not enforce it.".into())),
                }
            }
            "heading_levels" | "headings" => {
                let max = val.get("max_level").and_then(Value::as_u64).filter(|n| (1..=9).contains(n)).and_then(|n| u8::try_from(n).ok());
                let hierarchical = val.get("hierarchical").and_then(Value::as_bool).unwrap_or(false);
                let ok = (ql.contains("heading") || ql.contains("level")) && max.is_none_or(|m| quote_has(&quote, f64::from(m)));
                if ok && (max.is_some() || hierarchical) {
                    heads.push(mk(HeadingRule { max_level: max, hierarchical }, &quote, page, origin));
                } else {
                    r.flags.push((topic, "headings: the quote does not support the levels reported, so I did not enforce them.".into()));
                }
            }
            "required_sections" | "sections" => {
                let asked = strings(val, 20, 60);
                let (found, missing): (Vec<String>, Vec<String>) =
                    asked.into_iter().partition(|s| section_variants(s).iter().any(|x| nq.contains(x.as_str())));
                if !missing.is_empty() {
                    r.flags.push((topic, format!("required sections: {} not found in the quote, left out.", missing.join(", "))));
                }
                if !found.is_empty() {
                    secs.push(mk(found, &quote, page, origin));
                }
            }
            _ => {}
        }
    }

    for (key, prefix) in [("unverified", ""), ("conflicts", "Conflict inside the guideline: ")] {
        for s in v.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(8) {
            let s: String = s.trim().chars().take(240).collect();
            if !s.is_empty() {
                r.notes.push(format!("{prefix}{s}"));
            }
        }
    }

    r.margins = settle(Topic::Margins, margins, Margins::describe, &mut r.flags);
    r.fonts = settle(Topic::Fonts, fonts, |v: &Vec<String>| v.join(", "), &mut r.flags);
    r.size = settle(Topic::Fonts, sizes, SizeRange::describe, &mut r.flags);
    r.spacing = settle(Topic::Spacing, spacings, |v: &f32| num(*v), &mut r.flags);
    r.citations = settle(Topic::Citations, cites, |v: &CiteStyle| v.label().to_string(), &mut r.flags);
    r.page_limit = settle(Topic::PageLimit, limits, |v: &PageLimit| format!("{} pages", v.pages), &mut r.flags);
    r.headings = settle(Topic::Headings, heads, |v: &HeadingRule| format!("max level {:?}", v.max_level), &mut r.flags);
    r.sections = settle(Topic::Sections, secs, |v: &Vec<String>| v.join(", "), &mut r.flags);

    if Topic::ALL.iter().all(|t| !r.has(*t)) && r.flags.is_empty() && r.notes.is_empty() {
        return Err("no rules were found in the guideline".into());
    }
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// Built-in rule sets (no AI, no network)
// ---------------------------------------------------------------------------------------------

/// APA 7 or MLA 9, when the user typed exactly that.
pub fn builtin(name: &str) -> Option<Rules> {
    let n = name.trim().to_ascii_lowercase().replace(['-', '_'], " ");
    let n = n.split_whitespace().collect::<Vec<_>>().join(" ");
    let rest_ok = |rest: &str, ok: &[&str]| ok.contains(&rest.trim());
    let (label, apa) = if let Some(rest) = n.strip_prefix("apa") {
        if !rest_ok(rest, &["", "7", "7th", "7th ed", "7th edition", "paper", "student paper", "7 paper", "7 student paper"]) {
            return None;
        }
        ("APA 7th edition", true)
    } else if let Some(rest) = n.strip_prefix("mla") {
        if !rest_ok(rest, &["", "9", "9th", "9th ed", "9th edition", "paper", "9 paper"]) {
            return None;
        }
        ("MLA 9th edition", false)
    } else {
        return None;
    };
    let src = format!("built-in {label}");
    let quote = "Built into WordCraft";
    let mut r = Rules { title: label.to_string(), origin: "built-in rule set".into(), ..Rules::default() };
    r.margins = Some(mk(Margins { top: Some(72.0), bottom: Some(72.0), left: Some(72.0), right: Some(72.0) }, quote, None, &src));
    r.spacing = Some(mk(2.0, quote, None, &src));
    if apa {
        let fonts = ["Times New Roman", "Calibri", "Arial", "Georgia", "Lucida Sans Unicode", "Computer Modern"].map(String::from).to_vec();
        r.fonts = Some(mk(fonts, quote, None, &src));
        r.citations = Some(mk(CiteStyle::AuthorYear, quote, None, &src));
        r.headings = Some(mk(HeadingRule { max_level: Some(5), hierarchical: true }, quote, None, &src));
        r.notes.push("Font size depends on the font (APA lists 12 pt Times New Roman; 11 pt Calibri, Arial or Georgia; 10 pt Lucida Sans Unicode or Computer Modern), so size is not checked.".into());
        r.notes.push("Title page, running head and reference-list formatting are not checked.".into());
    } else {
        r.citations = Some(mk(CiteStyle::AuthorPage, quote, None, &src));
        r.notes.push("MLA asks for an easily readable font, so fonts and size are not checked.".into());
        r.notes.push("Header, Works Cited page and heading format are not checked.".into());
    }
    r.notes.push("This is a built-in summary of the style. Your instructor or publisher may require differences: load their own guideline to check against it.".into());
    Some(r)
}

// ---------------------------------------------------------------------------------------------
// Looking at the document (no AI)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Flag,
    NotSpecified,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "Pass",
            Status::Fail => "Fail",
            Status::Flag => "Flag",
            Status::NotSpecified => "No rule",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub topic: Topic,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq)]
struct CiteCounts {
    numeric: usize,
    author_year: usize,
    author_page: usize,
}

/// What the checks need from the document, read once.
#[derive(Debug, Default)]
struct Facts {
    pages: usize,
    /// Per section: top, bottom, left, right (points).
    margins: Vec<[f32; 4]>,
    normal_font: Option<String>,
    normal_size: Option<f32>,
    normal_spacing: Option<LineSpacing>,
    heading_fonts: Vec<(String, Option<String>)>,
    /// Fonts and sizes (tenths of a point) set directly on body text, with paragraph counts.
    body_fonts: BTreeMap<String, usize>,
    body_sizes: BTreeMap<i64, usize>,
    /// Own line spacing on long body paragraphs.
    para_spacing: Vec<LineSpacing>,
    headings: Vec<(u8, String)>,
    titles: Vec<String>,
    cites: CiteCounts,
}

fn is_body(p: &Paragraph) -> bool {
    p.section.is_none()
        && p.objects.is_empty()
        && p.props.numbering.is_none()
        && !p.text.trim().is_empty()
        && p.props.style.as_deref().is_none_or(|s| s == "Normal")
}

fn is_refs_heading(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    section_variants("references").iter().any(|v| *v == t)
}

/// A style property, following `basedOn` a few steps.
fn resolve<T>(doc: &Document, id: &str, pick: impl Fn(&Style) -> Option<T>) -> Option<T> {
    let mut cur = id.to_string();
    for _ in 0..8 {
        let Some(st) = doc.styles.get(&cur) else { break };
        if let Some(v) = pick(st) {
            return Some(v);
        }
        match &st.based_on {
            Some(b) => cur = b.clone(),
            None => break,
        }
    }
    None
}

impl Facts {
    fn read(s: &mut Session) -> Facts {
        let pages = s.layout().pages.len();
        let doc = &s.doc;
        let mut f = Facts { pages, ..Facts::default() };
        f.margins = doc.sections().into_iter().map(|(_, p)| [p.margin_top, p.margin_bottom, p.margin_left, p.margin_right]).collect();
        f.normal_font = resolve(doc, "Normal", |st| st.chr.font.clone()).or_else(|| doc.styles.default_chr.font.clone());
        f.normal_size = resolve(doc, "Normal", |st| st.chr.size).or(doc.styles.default_chr.size);
        f.normal_spacing = resolve(doc, "Normal", |st| st.para.line_spacing).or(doc.styles.default_para.line_spacing);
        for id in ["Title", "Heading1", "Heading2", "Heading3"] {
            if doc.styles.get(id).is_some() {
                let font = resolve(doc, id, |st| st.chr.font.clone()).or_else(|| doc.styles.default_chr.font.clone());
                f.heading_fonts.push((id.to_string(), font));
            }
        }
        let mut in_refs = false;
        let mut cite_text = String::new();
        for b in &doc.body {
            let Some(p) = b.as_para() else { continue };
            let style = p.props.style.as_deref().unwrap_or("Normal");
            if let Some(level) = style.strip_prefix("Heading").and_then(|d| d.parse::<u8>().ok()) {
                let text = p.text.trim().to_string();
                if is_refs_heading(&text) {
                    in_refs = true;
                }
                f.headings.push((level, text));
                continue;
            }
            if style == "Title" {
                f.titles.push(p.text.trim().to_string());
                continue;
            }
            if !in_refs && !p.text.trim().is_empty() {
                cite_text.push_str(&p.text);
                cite_text.push('\n');
            }
            if !is_body(p) {
                continue;
            }
            let (mut fonts, mut sizes): (Vec<&str>, Vec<i64>) = (Vec::new(), Vec::new());
            for run in &p.runs {
                if let Some(font) = run.props.font.as_deref()
                    && !fonts.contains(&font)
                {
                    fonts.push(font);
                }
                if let Some(sz) = run.props.size {
                    let key = (sz * 10.0).round() as i64;
                    if !sizes.contains(&key) {
                        sizes.push(key);
                    }
                }
            }
            for font in fonts {
                *f.body_fonts.entry(font.to_string()).or_insert(0) += 1;
            }
            for key in sizes {
                *f.body_sizes.entry(key).or_insert(0) += 1;
            }
            if p.text.chars().count() > 60
                && let Some(ls) = p.props.line_spacing
            {
                f.para_spacing.push(ls);
            }
        }
        f.cites = count_citations(&cite_text);
        f
    }
}

fn is_year(tok: &str) -> bool {
    let t = tok.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    t.len() == 4 && t.parse::<u32>().is_ok_and(|y| (1500..=2100).contains(&y))
}

fn classify(open: char, inner: &str, c: &mut CiteCounts) {
    let t = inner.trim();
    if t.is_empty() {
        return;
    }
    if open == '[' {
        let only = t.chars().all(|ch| ch.is_ascii_digit() || matches!(ch, ',' | ' ' | '-' | '\u{2013}'));
        if only && t.chars().any(|ch| ch.is_ascii_digit()) {
            c.numeric += 1;
        }
        return;
    }
    for part in t.split(';') {
        let p = part.trim();
        if !p.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        if p.split(|ch: char| !ch.is_ascii_alphanumeric()).any(is_year) {
            c.author_year += 1;
            continue;
        }
        let mut words = p.split_whitespace();
        let Some(last) = words.next_back() else { continue };
        let page_like = last.len() <= 9
            && last.chars().any(|ch| ch.is_ascii_digit())
            && last.chars().all(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '\u{2013}'));
        let rest: Vec<&str> = words.collect();
        let names = !rest.is_empty()
            && rest.iter().all(|w| w.chars().all(|ch| ch.is_alphabetic() || matches!(ch, '.' | '&' | '\'' | '-' | ',' | '\u{2019}')));
        if page_like && names {
            c.author_page += 1;
        }
    }
}

/// Counts of numbered `[1]`, author-year `(Smith, 2020)` and author-page `(Smith 45)` citations.
/// A rough scan: it can miscount, and says so in the report.
fn count_citations(text: &str) -> CiteCounts {
    let chars: Vec<char> = text.chars().collect();
    let mut counts = CiteCounts::default();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let close = match c {
            '(' => ')',
            '[' => ']',
            _ => {
                i += 1;
                continue;
            }
        };
        let mut j = i + 1;
        while let Some(&d) = chars.get(j) {
            if d == close || d == '(' || d == '[' || j - i > 120 {
                break;
            }
            j += 1;
        }
        if chars.get(j) == Some(&close) {
            let inner: String = chars.get(i + 1..j).map(|s| s.iter().collect()).unwrap_or_default();
            classify(c, &inner, &mut counts);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    counts
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(40).collect();
    if s.chars().count() > 40 { format!("{t}...") } else { t }
}

fn judge_margins(m: &Margins, f: &Facts) -> (Status, String) {
    if f.margins.is_empty() {
        return (Status::Flag, "The document has no page setup I can read.".into());
    }
    let want = [m.top, m.bottom, m.left, m.right];
    let names = ["top", "bottom", "left", "right"];
    let mut bad: Vec<String> = Vec::new();
    for (i, sec) in f.margins.iter().enumerate() {
        for (k, name) in names.iter().enumerate() {
            let (Some(Some(w)), Some(have)) = (want.get(k).copied(), sec.get(k).copied()) else { continue };
            if (have - w).abs() > 1.5 {
                let place = if f.margins.len() > 1 { format!(" in section {}", i + 1) } else { String::new() };
                bad.push(format!("{name} margin is {} (needs {}){place}", inches(have), inches(w)));
            }
        }
    }
    if bad.is_empty() {
        (Status::Pass, "Page margins match.".into())
    } else {
        let more = bad.len().saturating_sub(4);
        let mut d = bad.into_iter().take(4).collect::<Vec<_>>().join("; ");
        if more > 0 {
            d.push_str(&format!("; and {more} more"));
        }
        (Status::Fail, d)
    }
}

fn judge_fonts(allowed: Option<&[String]>, size: Option<&SizeRange>, f: &Facts) -> (Status, String) {
    let mut bad: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    if let Some(allowed) = allowed {
        let mut used: Vec<(String, String)> = Vec::new();
        match &f.normal_font {
            Some(n) => used.push(("Body style".into(), n.clone())),
            None if f.body_fonts.is_empty() => unresolved.push("the body font isn't set, so I can't tell which it is".into()),
            None => {}
        }
        for (id, font) in &f.heading_fonts {
            if let Some(font) = font {
                used.push((id.clone(), font.clone()));
            }
        }
        for (font, n) in &f.body_fonts {
            used.push((format!("{n} paragraph(s)"), font.clone()));
        }
        for (place, font) in used {
            if font.starts_with('+') {
                unresolved.push(format!("{place} uses the theme font {font}"));
            } else if !allowed.iter().any(|a| a.eq_ignore_ascii_case(font.trim())) {
                let line = format!("{place}: {font}");
                if !bad.contains(&line) {
                    bad.push(line);
                }
            }
        }
        if !bad.is_empty() {
            bad.insert(0, format!("not allowed ({})", allowed.join(", ")));
        }
    }
    if let Some(sz) = size {
        let ok = |x: f32| x >= sz.min - 0.1 && x <= sz.max + 0.1;
        match f.normal_size {
            Some(x) if !ok(x) => bad.push(format!("body text is {} pt (needs {})", num(x), sz.describe())),
            None if f.body_sizes.is_empty() => unresolved.push("the body size isn't set".into()),
            _ => {}
        }
        for (k, n) in &f.body_sizes {
            let x = *k as f32 / 10.0;
            if !ok(x) {
                bad.push(format!("{n} paragraph(s) at {} pt", num(x)));
            }
        }
    }
    if !bad.is_empty() {
        (Status::Fail, bad.join("; "))
    } else if !unresolved.is_empty() {
        (Status::Flag, format!("Can't confirm: {}.", unresolved.join("; ")))
    } else {
        let font = f.normal_font.clone().unwrap_or_default();
        let size = f.normal_size.map(|x| format!(" {} pt", num(x))).unwrap_or_default();
        (Status::Pass, format!("Body text: {font}{size}."))
    }
}

fn judge_spacing(want: f32, f: &Facts) -> (Status, String) {
    let near = |x: f32| (x - want).abs() <= 0.06;
    let mut bad: Vec<String> = Vec::new();
    match f.normal_spacing {
        Some(LineSpacing::Multiple(m)) if !near(m) => bad.push(format!("body text is {} (needs {})", num(m), num(want))),
        Some(LineSpacing::Multiple(_)) => {}
        Some(_) => return (Status::Flag, "Body text uses exact or at-least spacing, but the guideline gives a multiple.".into()),
        None => return (Status::Flag, "Line spacing isn't set in the Normal style, so I can't confirm it.".into()),
    }
    let off = f.para_spacing.iter().filter(|ls| !matches!(ls, LineSpacing::Multiple(m) if near(*m))).count();
    if off > 0 {
        bad.push(format!("{off} paragraph(s) have their own spacing"));
    }
    if bad.is_empty() { (Status::Pass, format!("Body text is {} spaced.", num(want))) } else { (Status::Fail, bad.join("; ")) }
}

fn judge_citations(style: CiteStyle, f: &Facts) -> (Status, String) {
    let c = &f.cites;
    let (own, others) = match style {
        CiteStyle::Numeric => (c.numeric, c.author_year + c.author_page),
        CiteStyle::AuthorYear => (c.author_year, c.numeric + c.author_page),
        CiteStyle::AuthorPage => (c.author_page, c.numeric + c.author_year),
        CiteStyle::Footnote => return (Status::Flag, "Footnote and endnote citations are not checked yet.".into()),
    };
    if own + others == 0 {
        (Status::Flag, "I found no citations in the text, so I can't check the style.".into())
    } else if others > 0 {
        (
            Status::Fail,
            format!(
                "Expected {}, found {own} of that style and {others} of another (rough scan of the text before the reference list).",
                style.label()
            ),
        )
    } else {
        (Status::Pass, format!("{own} citation(s), all {} (rough scan).", style.label()))
    }
}

fn judge_pages(r: &PageLimit, f: &Facts) -> (Status, String) {
    let limit = usize::try_from(r.pages).unwrap_or(usize::MAX);
    if f.pages <= limit {
        (Status::Pass, format!("{} of {} pages allowed.", f.pages, r.pages))
    } else if !r.excludes.is_empty() {
        (Status::Flag, format!("{} pages in total. The limit is {} pages and excludes {}, which I can't separate out.", f.pages, r.pages, r.excludes))
    } else {
        (Status::Fail, format!("{} pages; the limit is {}.", f.pages, r.pages))
    }
}

fn judge_headings(r: &HeadingRule, f: &Facts) -> (Status, String) {
    if f.headings.is_empty() {
        return (Status::Flag, "No Heading styles are used, so I can't check the levels.".into());
    }
    let mut bad: Vec<String> = Vec::new();
    if let Some(max) = r.max_level {
        let over: BTreeSet<u8> = f.headings.iter().map(|(l, _)| *l).filter(|l| *l > max).collect();
        if !over.is_empty() {
            let list = over.iter().map(|l| format!("Heading {l}")).collect::<Vec<_>>().join(", ");
            bad.push(format!("{list} used; the limit is Heading {max}"));
        }
    }
    if r.hierarchical {
        let mut prev = 0u8;
        for (lvl, text) in &f.headings {
            if *lvl > prev.saturating_add(1) && bad.len() < 4 {
                bad.push(format!(
                    "\"{}\" is Heading {lvl} after {}",
                    short(text),
                    if prev == 0 { "nothing".to_string() } else { format!("Heading {prev}") }
                ));
            }
            prev = *lvl;
        }
    }
    if bad.is_empty() { (Status::Pass, format!("{} heading(s) follow the rules.", f.headings.len())) } else { (Status::Fail, bad.join("; ")) }
}

fn judge_sections(names: &[String], f: &Facts) -> (Status, String) {
    if f.headings.is_empty() && f.titles.is_empty() {
        return (Status::Flag, "No Heading styles are used, so I can't find the sections.".into());
    }
    let heads: Vec<String> = f.headings.iter().map(|(_, t)| t.to_lowercase()).chain(f.titles.iter().map(|t| t.to_lowercase())).collect();
    let missing: Vec<&String> = names.iter().filter(|n| !section_variants(n).iter().any(|v| heads.iter().any(|h| h.contains(v.as_str())))).collect();
    if missing.is_empty() {
        (Status::Pass, "All required sections have a heading.".into())
    } else {
        (Status::Fail, format!("No heading found for: {}.", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")))
    }
}

fn judge(t: Topic, r: &Rules, f: &Facts) -> (Status, String) {
    let none = (Status::NotSpecified, String::new());
    match t {
        Topic::Margins => r.margins.as_ref().map_or(none, |x| judge_margins(&x.value, f)),
        Topic::Fonts if r.fonts.is_none() && r.size.is_none() => none,
        Topic::Fonts => judge_fonts(r.fonts.as_ref().map(|x| x.value.as_slice()), r.size.as_ref().map(|x| &x.value), f),
        Topic::Spacing => r.spacing.as_ref().map_or(none, |x| judge_spacing(x.value, f)),
        Topic::Citations => r.citations.as_ref().map_or(none, |x| judge_citations(x.value, f)),
        Topic::PageLimit => r.page_limit.as_ref().map_or(none, |x| judge_pages(&x.value, f)),
        Topic::Headings => r.headings.as_ref().map_or(none, |x| judge_headings(&x.value, f)),
        Topic::Sections => r.sections.as_ref().map_or(none, |x| judge_sections(&x.value, f)),
    }
}

/// Add the unverified or conflicting notes of a topic; they turn a Pass into a Flag.
fn finish(t: Topic, r: &Rules, status: Status, detail: String) -> Finding {
    let notes: Vec<&str> = r.flags.iter().filter(|(ft, _)| *ft == t).map(|(_, m)| m.as_str()).collect();
    let (mut status, mut detail) = (status, detail);
    if !notes.is_empty() {
        if matches!(status, Status::Pass | Status::NotSpecified) {
            status = Status::Flag;
        }
        if !detail.is_empty() {
            detail.push(' ');
        }
        detail.push_str(&notes.join(" "));
    }
    if detail.is_empty() {
        detail = "Not in the guidelines.".into();
    }
    Finding { topic: t, status, detail }
}

fn check_facts(r: &Rules, f: &Facts) -> Vec<Finding> {
    Topic::ALL
        .iter()
        .map(|&t| {
            let (status, detail) = judge(t, r, f);
            finish(t, r, status, detail)
        })
        .collect()
}

/// Check the open document against the rules. Reads the document; changes nothing.
pub fn check(s: &mut Session, rules: &Rules) -> Vec<Finding> {
    let facts = Facts::read(s);
    check_facts(rules, &facts)
}

/// The chat message after a check.
pub fn report(rules: &Rules, findings: &[Finding]) -> String {
    let count = |s: Status| findings.iter().filter(|f| f.status == s).count();
    let mut m = format!(
        "Checked your document against {}.\n{} passed, {} failed, {} flagged.",
        rules.name(),
        count(Status::Pass),
        count(Status::Fail),
        count(Status::Flag)
    );
    for f in findings.iter().filter(|f| f.status != Status::NotSpecified) {
        m.push_str(&format!("\n- {}: {}. {}", f.topic.label(), f.status.label(), f.detail));
    }
    if !rules.notes.is_empty() {
        m.push_str("\n\nNot checked, or not confirmed:");
        for n in &rules.notes {
            m.push_str(&format!("\n- {n}"));
        }
    }
    if !rules.pages.is_empty() {
        m.push_str("\n\nThese rules came from a web search. Open the sources and confirm them:");
        for p in &rules.pages {
            m.push_str(&format!("\n- {p}"));
        }
    }
    m.push_str("\n\nI only enforce rules that quote the guideline. Nothing in your document was changed. To fix problems, use Format document with the style name.");
    m
}

// ---------------------------------------------------------------------------------------------
// Getting the guideline: built-in, file, link or web search
// ---------------------------------------------------------------------------------------------

/// Background work: turn what the user typed into rules.
pub fn load(input: &str, only: Option<usize>) -> Result<Rules, String> {
    if let Some(r) = builtin(input) {
        return Ok(r);
    }
    fetch(input, only)
}

#[cfg(target_arch = "wasm32")]
fn fetch(_input: &str, _only: Option<usize>) -> Result<Rules, String> {
    Err("Reading guideline files and searching for guidelines works in the desktop app. APA 7 and MLA 9 work here.".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch(input: &str, only: Option<usize>) -> Result<Rules, String> {
    net::fetch(input, only)
}

#[cfg(not(target_arch = "wasm32"))]
mod net {
    use std::path::Path;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Rules, SYSTEM_PROMPT, parse_rules};
    use crate::ai_net;

    const MAX_FILE: u64 = 10 * 1024 * 1024;
    const MAX_TEXT_CHARS: usize = 40_000;
    const MODEL: &str = "gemini-2.5-flash";
    const MODEL_LABEL: &str = "Gemini 2.5 Flash";

    fn agent(secs: u64) -> ureq::Agent {
        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(secs))).http_status_as_error(false).build();
        ureq::Agent::new_with_config(config)
    }

    fn gemini(body: &Value) -> Result<Value, String> {
        let key = ai_net::key(ai_net::Provider::Gemini).ok_or("GEMINI_API_KEY is not set (reading PDFs and searching for guidelines need Gemini)")?;
        let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{MODEL}:generateContent");
        let mut resp = agent(120).post(&url).header("x-goog-api-key", key.as_str()).send_json(body).map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn reply_text(v: &Value) -> String {
        v.pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<String>())
            .unwrap_or_default()
    }

    /// The pages Google Search grounding says it used: "title - address".
    fn grounding_pages(v: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let chunks = v.pointer("/candidates/0/groundingMetadata/groundingChunks").and_then(Value::as_array);
        for c in chunks.into_iter().flatten().take(8) {
            let uri = c.pointer("/web/uri").and_then(Value::as_str).unwrap_or("");
            let title = c.pointer("/web/title").and_then(Value::as_str).unwrap_or("");
            if uri.is_empty() {
                continue;
            }
            let line = format!("{title} - {uri}");
            if !out.contains(&line) {
                out.push(line);
            }
        }
        out
    }

    fn is_http(s: &str) -> bool {
        s.starts_with("http://") || s.starts_with("https://")
    }

    fn is_pdf_link(s: &str) -> bool {
        s.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase().ends_with(".pdf")
    }

    pub fn fetch(input: &str, only: Option<usize>) -> Result<Rules, String> {
        let input = input.trim().trim_matches('"').trim_matches('\'');
        if input.is_empty() {
            return Err("Type a style, a file path, a link or a name first.".into());
        }
        let path = Path::new(input);
        if path.is_file() {
            return import_file(path, only);
        }
        if is_http(input) && is_pdf_link(input) {
            return import_link(input);
        }
        search(input)
    }

    fn import_file(path: &Path, only: Option<usize>) -> Result<Rules, String> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("guidelines").to_string();
        let meta = std::fs::metadata(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
        if meta.len() > MAX_FILE {
            return Err(format!("{name} is larger than 10 MB."));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
        if bytes.starts_with(b"%PDF") {
            return import_pdf(&bytes, &name);
        }
        if bytes.contains(&0) {
            return Err(format!("{name} is not a PDF or a text file. Use PDF, TXT or MD."));
        }
        import_text(&String::from_utf8_lossy(&bytes), &name, only)
    }

    fn import_link(url: &str) -> Result<Rules, String> {
        let mut resp = agent(60).get(url).call().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("The link answered HTTP {status}."));
        }
        let bytes = resp.body_mut().read_to_vec().map_err(|e| format!("Couldn't download the PDF: {e}"))?;
        if !bytes.starts_with(b"%PDF") {
            return Err("The link did not return a PDF.".into());
        }
        let name = url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("guidelines.pdf").to_string();
        import_pdf(&bytes, &name)
    }

    fn import_pdf(bytes: &[u8], name: &str) -> Result<Rules, String> {
        let data = wordcraft_engine::cmd::insert::base64_encode(bytes);
        let body = json!({
            "systemInstruction": {"parts": [{"text": SYSTEM_PROMPT}]},
            "contents": [{"role": "user", "parts": [
                {"inlineData": {"mimeType": "application/pdf", "data": data}},
                {"text": "This PDF is the guideline. Extract its formatting and submission rules. Give the page number of each quote."}
            ]}],
            "generationConfig": {"responseMimeType": "application/json", "temperature": 0.1}
        });
        let v = gemini(&body)?;
        let mut rules = parse_rules(&reply_text(&v), &format!("PDF {name}"), None)?;
        rules.notes.push("A PDF's quotes can't be matched against its text here, so only numbers are cross-checked against each quote. Compare the quoted pages with the PDF if a rule matters.".into());
        let _ = MODEL_LABEL;
        Ok(rules)
    }

    fn import_text(text: &str, name: &str, only: Option<usize>) -> Result<Rules, String> {
        let text: String = text.chars().take(MAX_TEXT_CHARS).collect();
        let user = json!({"source": name, "guideline_text": text}).to_string();
        let origin = format!("file {name}");
        let reply = ai_net::ask(SYSTEM_PROMPT, &user, only, |t| parse_rules(t, "x", Some(&text)).is_ok())?;
        parse_rules(&reply.text, &origin, Some(&text))
    }

    fn search(query: &str) -> Result<Rules, String> {
        let query: String = query.chars().take(200).collect();
        if query.chars().count() < 3 {
            return Err("Type a longer name, e.g. a university, journal or conference.".into());
        }
        let user = format!(
            "Find the OFFICIAL formatting and submission guidelines for: {query}\n\
             Use only pages published by the institution, publisher, journal, conference or organisation itself (author instructions, thesis handbook, style guide). \
             Do not use blogs, forums or third-party summaries. If you cannot find an official page, return an empty \"rules\" list and say so under \"unverified\". \
             Quote word for word from the page you used. Reply with the JSON object only."
        );
        let body = json!({
            "systemInstruction": {"parts": [{"text": SYSTEM_PROMPT}]},
            "contents": [{"role": "user", "parts": [{"text": user}]}],
            "tools": [{"google_search": {}}],
            "generationConfig": {"temperature": 0.1}
        });
        let v = gemini(&body)?;
        let pages = grounding_pages(&v);
        let mut rules = parse_rules(&reply_text(&v), &format!("web search: {query}"), None)?;
        if pages.is_empty() {
            // No page was returned: whatever the model said came from memory. Enforce nothing.
            let mut kept = Rules { title: rules.title.clone(), origin: rules.origin.clone(), ..Rules::default() };
            kept.notes.push("The search returned no source page, so I enforce none of these rules.".into());
            kept.notes.append(&mut rules.notes);
            return Ok(kept);
        }
        rules.pages = pages;
        rules.notes.push("Rules from a web search are not matched against the page text. Open the sources and confirm them.".into());
        Ok(rules)
    }
}

// ---------------------------------------------------------------------------------------------
// The panel section and its plumbing
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct State {
    input: String,
    /// Add the next guideline to the current rules instead of replacing them.
    add: bool,
    rules: Option<Rules>,
    findings: Option<Vec<Finding>>,
    busy: Option<Receiver<Result<Rules, String>>>,
    load_request: Option<String>,
    check_request: bool,
    browse_request: bool,
    waiting_pick: bool,
    can_browse: bool,
}

fn colour(s: Status, t: &Tokens) -> Color32 {
    match s {
        Status::Pass => Color32::from_rgb(46, 160, 90),
        Status::Fail => t.red,
        Status::Flag => Color32::from_rgb(214, 160, 20),
        Status::NotSpecified => t.text_dim,
    }
}

/// The "Guidelines and compliance" section of the Word panel.
pub fn ui(ui: &mut egui::Ui, t: &Tokens, st: &mut State) {
    egui::CollapsingHeader::new("Guidelines and compliance").default_open(false).show(ui, |ui| {
        ui.label(
            RichText::new(
                "Type a style (APA 7, MLA 9), the path or link of a guideline PDF or text file, or the name of a university, journal or conference.",
            )
            .small()
            .color(t.text_dim),
        );
        let busy = st.busy.is_some();
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 90.0).max(120.0);
            ui.add(egui::TextEdit::singleline(&mut st.input).desired_width(width).hint_text("APA 7, a PDF path, IEEE conference..."));
            if ui.add_enabled(st.can_browse && !busy, egui::Button::new("Browse...")).clicked() {
                st.browse_request = true;
            }
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy && !st.input.trim().is_empty(), egui::Button::new("Load rules")).clicked() {
                st.load_request = Some(st.input.trim().to_string());
            }
            if ui.add_enabled(st.rules.is_some() && !busy, egui::Button::new("Check document")).clicked() {
                st.check_request = true;
            }
            if ui.add_enabled(st.rules.is_some() && !busy, egui::Button::new("Clear rules")).clicked() {
                st.rules = None;
                st.findings = None;
            }
        });
        ui.checkbox(&mut st.add, "Add to the current rules (conflicts are flagged)");
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Word is reading the guidelines...").color(t.text_dim));
            });
        }
        let Some(rules) = &st.rules else { return };
        ui.separator();
        ui.label(RichText::new(rules.name()).strong());
        egui::ScrollArea::vertical().id_salt("word-ai-guide-scroll").max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("word-ai-guide-grid").num_columns(3).striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
                for topic in Topic::ALL {
                    ui.label(RichText::new(topic.label()).strong());
                    match st.findings.as_ref().and_then(|fs| fs.iter().find(|f| f.topic == topic)) {
                        Some(f) => {
                            ui.label(RichText::new(f.status.label()).strong().color(colour(f.status, t)));
                            ui.add(egui::Label::new(RichText::new(f.detail.as_str()).small()).wrap());
                        }
                        None => {
                            ui.label(RichText::new("-").color(t.text_dim));
                            let want = rules.describe(topic).unwrap_or_else(|| "Not in the guidelines".to_string());
                            ui.add(egui::Label::new(RichText::new(want).small().color(t.text_dim)).wrap());
                        }
                    }
                    ui.end_row();
                }
            });
            if !rules.notes.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Not checked, or not confirmed").small().strong());
                for n in &rules.notes {
                    ui.add(egui::Label::new(RichText::new(format!("- {n}")).small().color(t.text_dim)).wrap());
                }
            }
            if !rules.pages.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Sources to confirm").small().strong());
                for p in &rules.pages {
                    ui.add(egui::Label::new(RichText::new(format!("- {p}")).small().color(t.text_dim)).wrap());
                }
            }
        });
    });
}

/// Whether this host shows a file picker whose answer comes back here.
fn can_pick(app: &WordApp) -> bool {
    app.services.open_async.is_none() && (app.services.file_dialog.is_some() || app.services.pick_open.is_some())
}

/// Called every frame from `ai_word::show`: file picks, starting the load, receiving rules, checks.
pub fn drive(app: &mut WordApp, ctx: &egui::Context, st: &mut State, only: Option<usize>, messages: &mut Vec<(bool, String)>) {
    st.can_browse = can_pick(app);

    // A file was picked in the dialog opened by Browse...
    if st.waiting_pick && !app.file_dialog_open() {
        st.waiting_pick = false;
        if let Some(p) = app.dialog_pick.take() {
            st.input = p;
        }
    }
    if st.browse_request {
        st.browse_request = false;
        st.waiting_pick = true;
        let _ = app.ask_file(FileDialogRequest::Open { purpose: "guidelines".into() }, AfterPick::ForDialog);
        ctx.request_repaint();
    }

    // Start loading on a worker thread.
    if let Some(input) = st.load_request.take() {
        messages.push((true, format!("Load guidelines: {input}")));
        let (tx, rx) = channel();
        let wake = ctx.clone();
        let spawned = std::thread::Builder::new().name("word-ai-guide".into()).spawn(move || {
            let r = load(&input, only);
            let _ = tx.send(r);
            wake.request_repaint();
        });
        match spawned {
            Ok(_) => st.busy = Some(rx),
            Err(e) => messages.push((false, format!("Couldn't start the request: {e}"))),
        }
    }

    // Receive the rules.
    if let Some(rx) = &st.busy {
        match rx.try_recv() {
            Ok(result) => {
                st.busy = None;
                match result {
                    Ok(rules) => {
                        let mut msg = rules.summary();
                        if st.add
                            && let Some(cur) = st.rules.as_mut()
                        {
                            cur.merge(rules);
                            msg.push_str("\nAdded to the current rules; values that disagree are flagged and not enforced.");
                        } else {
                            st.rules = Some(rules);
                        }
                        st.findings = None;
                        st.check_request = true;
                        messages.push((false, msg));
                    }
                    Err(e) => messages.push((false, format!("I couldn't read those guidelines.\n{e}"))),
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(150)),
            Err(TryRecvError::Disconnected) => {
                st.busy = None;
                messages.push((false, "The request stopped unexpectedly.".into()));
            }
        }
    }

    // Check the document.
    if st.check_request {
        st.check_request = false;
        if let Some(rules) = &st.rules {
            let findings = check(&mut app.session, rules);
            messages.push((false, report(rules, &findings)));
            st.findings = Some(findings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(reply: &str) -> Rules {
        parse_rules(reply, "test", None).unwrap()
    }

    #[test]
    fn numbers_are_found_as_digits_and_as_words() {
        assert!(quote_has("Margins must be 1 inch on all sides.", 1.0));
        assert!(quote_has("Use 2.54 cm margins", 2.54));
        assert!(quote_has("Text must be double-spaced", 2.0));
        assert!(quote_has("one and a half spacing", 1.5));
        assert!(!quote_has("12 pt Times New Roman", 11.0));
    }

    #[test]
    fn a_rule_needs_its_numbers_in_its_quote() {
        let r = rules(r#"{"rules":[{"key":"margins","value":{"all":1.5},"unit":"in","quote":"Use 1 inch margins","page":2}]}"#);
        assert!(r.margins.is_none());
        assert!(r.flags.iter().any(|(t, _)| *t == Topic::Margins));
    }

    #[test]
    fn a_quoted_rule_is_kept_and_converted_to_points() {
        let r = rules(r#"{"rules":[{"key":"margins","value":{"all":2.54},"unit":"cm","quote":"Set 2.54 cm margins","page":2}]}"#);
        let m = r.margins.unwrap();
        assert_eq!(m.value.top, Some(72.0));
        assert_eq!(m.page, Some(2));
    }

    #[test]
    fn a_rule_without_a_quote_is_not_enforced() {
        let r = rules(r#"{"rules":[{"key":"line_spacing","value":2}],"unverified":["font must be readable"]}"#);
        assert!(r.spacing.is_none());
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn differing_values_are_a_conflict_and_neither_is_enforced() {
        let r = rules(
            r#"{"rules":[
            {"key":"margins","value":{"all":1},"unit":"in","quote":"Margins: 1 inch","page":2},
            {"key":"margins","value":{"all":1.5},"unit":"in","quote":"Margins: 1.5 inches","page":9}]}"#,
        );
        assert!(r.margins.is_none());
        assert!(r.flags.iter().any(|(t, m)| *t == Topic::Margins && m.starts_with("conflicting")));
    }

    #[test]
    fn identical_repeats_collapse_and_units_agree() {
        let r = rules(
            r#"{"rules":[
            {"key":"margins","value":{"all":1},"unit":"in","quote":"Margins: 1 inch"},
            {"key":"margins","value":{"all":2.54},"unit":"cm","quote":"Margins: 2.54 cm"}]}"#,
        );
        assert!(r.margins.is_some());
        assert!(r.flags.is_empty());
    }

    #[test]
    fn with_a_text_file_the_quote_must_be_in_it() {
        let reply = r#"{"rules":[{"key":"line_spacing","value":2,"quote":"Double-space everything."}]}"#;
        assert!(parse_rules(reply, "f", Some("Please double-space everything.")).unwrap().spacing.is_some());
        assert!(parse_rules(reply, "f", Some("Use single spacing.")).unwrap().spacing.is_none());
    }

    #[test]
    fn a_second_guideline_that_disagrees_is_flagged_on_merge() {
        let mut a = builtin("APA 7").unwrap();
        let b = rules(r#"{"rules":[{"key":"line_spacing","value":1.5,"quote":"Use 1.5 line spacing"}]}"#);
        a.merge(b);
        assert!(a.spacing.is_none());
        assert!(a.flags.iter().any(|(t, m)| *t == Topic::Spacing && m.starts_with("conflicting")));
        assert!(a.margins.is_some());
    }

    #[test]
    fn built_in_styles_are_exact_names_only() {
        assert!(builtin("APA 7").is_some());
        assert!(builtin("mla 9th edition").is_some());
        assert!(builtin("APA 6").is_none());
        assert!(builtin("APA journal of psychology").is_none());
    }

    #[test]
    fn citations_are_counted_by_style() {
        let c = count_citations("As shown (Smith, 2019; Lee et al., 2020) and [1], [2-3], later (Brown 45). In 2019 (see below).");
        assert_eq!(c.author_year, 2);
        assert_eq!(c.numeric, 2);
        assert_eq!(c.author_page, 1);
    }

    #[test]
    fn wrong_margins_fail_and_unknown_things_are_flagged() {
        let r = builtin("APA 7").unwrap();
        let f = Facts { pages: 3, margins: vec![[72.0, 72.0, 90.0, 72.0]], ..Facts::default() };
        let out = check_facts(&r, &f);
        let m = out.iter().find(|x| x.topic == Topic::Margins).unwrap();
        assert_eq!(m.status, Status::Fail);
        assert!(m.detail.contains("left"));
        // No Heading styles and no spacing set: flagged, not passed.
        let h = out.iter().find(|x| x.topic == Topic::Headings).unwrap();
        assert_eq!(h.status, Status::Flag);
        let s = out.iter().find(|x| x.topic == Topic::Spacing).unwrap();
        assert_eq!(s.status, Status::Flag);
    }

    #[test]
    fn page_limit_and_sections() {
        let r = rules(
            r#"{"rules":[{"key":"page_limit","value":2,"unit":"pages","excludes":"references","quote":"Papers are limited to 2 pages"},
            {"key":"required_sections","value":["Abstract","References"],"quote":"Include an Abstract and a References section"}]}"#,
        );
        let f = Facts { pages: 3, headings: vec![(1, "Abstract".into())], ..Facts::default() };
        let out = check_facts(&r, &f);
        assert_eq!(out.iter().find(|x| x.topic == Topic::PageLimit).unwrap().status, Status::Flag);
        assert_eq!(out.iter().find(|x| x.topic == Topic::Sections).unwrap().status, Status::Fail);
    }
}
