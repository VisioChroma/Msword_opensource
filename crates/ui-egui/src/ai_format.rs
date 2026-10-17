//! "Word" AI, part 3: intelligent formatting and typography.
//!
//! The model never touches the document. It sees a small profile of it (counts, headings, a text
//! sample, the current page setup) and answers with two or three formatting *options*. This file
//! checks the answer: every number is clamped, every font must come from a short list, and the
//! built-in official rule sets (APA 7, MLA 9) replace the model's numbers whenever the user named
//! one. The chosen option is then applied through ordinary engine commands as one Undo step.
//!
//! Anything the model says it cannot verify (a university's own thesis rules, say) is listed back
//! to the user as unverified; nothing is invented. Fetching and importing real guideline documents
//! is the next feature ("Official guidelines and compliance").

use std::collections::BTreeMap;

use serde_json::{Value, json};
use wordcraft_doc::{Align, Paragraph};
use wordcraft_engine::Session;

use crate::ai_net;
use crate::ai_plan::extract_json;

const MAX_HEADINGS: usize = 30;
const MAX_FIXES: usize = 2000;
/// Fonts whose name contains one of these are treated as deliberate (code, listings).
const MONO_HINTS: &[&str] = &["mono", "courier", "consolas", "menlo"];

pub const SYSTEM_PROMPT: &str = r#"You are Word, the typography assistant inside the WordCraft document editor.
You receive a JSON object with the user's request and a profile of their document (counts, headings,
a text sample, the current page setup). Decide what kind of document it is and propose two or three
formatting options for it.

Reply with ONE JSON object and nothing else (no markdown fences, no commentary):
{
  "document_type": "academic paper | dissertation | meeting brief | resume | business report | letter | other",
  "guideline": "apa7" | "mla9" | "none",
  "unverified": ["each formatting requirement the user mentioned that you cannot confirm from an official source"],
  "summary": "one sentence: why these options fit this document",
  "options": [
    {
      "category": "sans" | "serif" | "mono",
      "heading_font": "...", "body_font": "...",
      "body_size": 11, "title_size": 26, "heading_sizes": [18, 14, 12],
      "line_spacing": 1.15, "space_after": 8, "first_line_indent": 0,
      "align": "left" | "justify", "heading_align": "left" | "center",
      "margins": {"top": 72, "bottom": 72, "left": 72, "right": 72}
    }
  ]
}

Rules:
- Give exactly two or three options, each with a different category. Use "mono" only when it truly suits the document (code, technical logs).
- Fonts must come from this list only. sans: Calibri, Arial, Source Sans 3. serif: Times New Roman, Georgia, Source Serif 4. mono: Courier New, Consolas.
- All sizes, spacing, indents and margins are in points. 72 points = 1 inch. line_spacing is a multiple (1.0 = single).
- Set "guideline" to "apa7" or "mla9" ONLY if the user named that style. Otherwise use "none".
- Never invent an official rule. If the user names a university, journal or conference style you do not have, use "none", choose sensible options for the document type, and list the missing rule in "unverified".
- Match the purpose: a resume is compact (single spacing, narrow-to-normal margins); a meeting brief is short and scannable; an academic paper or dissertation is more spacious (1.5 or double spacing, 72 point margins); a business report is clean and moderately spaced.
- Do not write or change any of the document's text."#;

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Sans,
    Serif,
    Mono,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Sans => "Sans",
            Category::Serif => "Serif",
            Category::Mono => "Mono",
        }
    }

    /// The only fonts offered: two or three per kind, not hundreds.
    fn fonts(self) -> &'static [&'static str] {
        match self {
            Category::Sans => &["Calibri", "Arial", "Source Sans 3"],
            Category::Serif => &["Times New Roman", "Georgia", "Source Serif 4"],
            Category::Mono => &["Courier New", "Consolas"],
        }
    }

    fn parse(s: &str) -> Option<Category> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sans" | "sans-serif" | "sans serif" => Some(Category::Sans),
            "serif" => Some(Category::Serif),
            "mono" | "monospace" => Some(Category::Mono),
            _ => None,
        }
    }
}

/// Rule sets built into WordCraft. They override whatever numbers the model proposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guideline {
    Apa7,
    Mla9,
}

impl Guideline {
    pub fn name(self) -> &'static str {
        match self {
            Guideline::Apa7 => "APA 7th edition",
            Guideline::Mla9 => "MLA 9th edition",
        }
    }

    fn parse(s: &str) -> Option<Guideline> {
        match s.trim().to_ascii_lowercase().replace([' ', '-', '_'], "").as_str() {
            "apa7" | "apa" | "apa7thedition" => Some(Guideline::Apa7),
            "mla9" | "mla" | "mla9thedition" => Some(Guideline::Mla9),
            _ => None,
        }
    }

    /// Fonts the style accepts, as (category, font, size in points).
    fn choices(self) -> Vec<Choice> {
        let fonts: [(Category, &str, f32); 3] = match self {
            Guideline::Apa7 => [(Category::Serif, "Times New Roman", 12.0), (Category::Sans, "Calibri", 11.0), (Category::Serif, "Georgia", 11.0)],
            Guideline::Mla9 => [(Category::Serif, "Times New Roman", 12.0), (Category::Serif, "Georgia", 12.0), (Category::Sans, "Arial", 12.0)],
        };
        let heading_aligns = match self {
            Guideline::Apa7 => [Align::Center, Align::Left, Align::Left],
            Guideline::Mla9 => [Align::Left, Align::Left, Align::Left],
        };
        fonts
            .into_iter()
            .map(|(category, font, size)| Choice {
                category,
                heading_font: font.to_string(),
                body_font: font.to_string(),
                body_size: size,
                title_size: size,
                heading_sizes: [size, size, size],
                line_spacing: 2.0,
                space_after: 0.0,
                first_line_indent: 36.0,
                align: Align::Left,
                title_align: Align::Center,
                heading_aligns,
                margins: [72.0, 72.0, 72.0, 72.0],
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub category: Category,
    pub heading_font: String,
    pub body_font: String,
    pub body_size: f32,
    pub title_size: f32,
    pub heading_sizes: [f32; 3],
    pub line_spacing: f32,
    pub space_after: f32,
    pub first_line_indent: f32,
    pub align: Align,
    pub title_align: Align,
    pub heading_aligns: [Align; 3],
    /// Top, bottom, left, right, in points.
    pub margins: [f32; 4],
}

impl Choice {
    /// One line for the option button.
    pub fn describe(&self) -> String {
        let fonts = if self.heading_font == self.body_font { self.body_font.clone() } else { format!("{} / {}", self.heading_font, self.body_font) };
        let [t, b, l, r] = self.margins;
        let (t, b, l, r) = (t / 72.0, b / 72.0, l / 72.0, r / 72.0);
        let margins = if (t - b).abs() < 0.01 && (t - l).abs() < 0.01 && (t - r).abs() < 0.01 { format!("{t:.1} in margins") } else { format!("margins {t:.1}/{b:.1}/{l:.1}/{r:.1} in") };
        format!("{fonts}, {} pt, {} line spacing, {margins}", self.body_size, self.line_spacing)
    }
}

#[derive(Clone, Debug)]
pub struct Proposal {
    pub document_type: String,
    pub guideline: Option<Guideline>,
    pub unverified: Vec<String>,
    pub summary: String,
    pub choices: Vec<Choice>,
    /// Which model answered (set by [`propose`]).
    pub model: String,
    /// Inconsistencies found in the document before any change (set by [`propose`]).
    pub issues: Issues,
}

fn num(o: &Value, key: &str, lo: f32, hi: f32, default: f32) -> f32 {
    o.get(key).and_then(Value::as_f64).map_or(default, |x| (x as f32).clamp(lo, hi))
}

fn pick_font(category: Category, asked: Option<&str>) -> String {
    let list = category.fonts();
    asked
        .and_then(|a| list.iter().find(|f| f.eq_ignore_ascii_case(a.trim())))
        .or_else(|| list.first())
        .map_or_else(|| "Calibri".to_string(), |f| (*f).to_string())
}

fn read_align(o: &Value, key: &str, heading: bool) -> Align {
    match o.get(key).and_then(Value::as_str).map(|x| x.trim().to_ascii_lowercase()).as_deref() {
        Some("justify" | "justified" | "both") if !heading => Align::Justify,
        Some("center" | "centre" | "centered") if heading => Align::Center,
        _ => Align::Left,
    }
}

fn read_choice(o: &Value, category: Category) -> Choice {
    let body_size = num(o, "body_size", 9.0, 16.0, 11.0);
    let mut sizes = [18.0_f32, 14.0, 12.0];
    if let Some(list) = o.get("heading_sizes").and_then(Value::as_array) {
        for (slot, v) in sizes.iter_mut().zip(list) {
            if let Some(x) = v.as_f64() {
                *slot = x as f32;
            }
        }
    }
    // Never smaller than the body, and each level no bigger than the one above.
    let mut above = 32.0_f32;
    for slot in &mut sizes {
        *slot = slot.clamp(body_size, 32.0).min(above);
        above = *slot;
    }
    let [h1, _, _] = sizes;
    let title_size = num(o, "title_size", 16.0, 40.0, 26.0).max(h1);
    let margin = o.get("margins");
    let side = |k: &str| margin.and_then(|m| m.get(k)).and_then(Value::as_f64).map_or(72.0, |x| (x as f32).clamp(36.0, 144.0));
    let heading_align = read_align(o, "heading_align", true);
    Choice {
        category,
        heading_font: pick_font(category, o.get("heading_font").and_then(Value::as_str)),
        body_font: pick_font(category, o.get("body_font").and_then(Value::as_str)),
        body_size,
        title_size,
        heading_sizes: sizes,
        line_spacing: num(o, "line_spacing", 1.0, 2.5, 1.15),
        space_after: num(o, "space_after", 0.0, 24.0, 8.0),
        first_line_indent: num(o, "first_line_indent", 0.0, 54.0, 0.0),
        align: read_align(o, "align", false),
        title_align: heading_align,
        heading_aligns: [heading_align; 3],
        margins: [side("top"), side("bottom"), side("left"), side("right")],
    }
}

fn read_options(v: &Value) -> Vec<Choice> {
    let mut choices: Vec<Choice> = Vec::new();
    for o in v.get("options").and_then(Value::as_array).into_iter().flatten().take(3) {
        let category = o.get("category").and_then(Value::as_str).and_then(Category::parse).unwrap_or(Category::Sans);
        if choices.iter().any(|c| c.category == category) {
            continue;
        }
        choices.push(read_choice(o, category));
    }
    choices
}

/// Check and bound a model reply.
pub fn parse_proposal(reply: &str) -> Result<Proposal, String> {
    let v = extract_json(reply).ok_or("the reply was not a valid formatting proposal")?;
    let guideline = v.get("guideline").and_then(Value::as_str).and_then(Guideline::parse);
    let choices = match guideline {
        Some(g) => g.choices(),
        None => read_options(&v),
    };
    if choices.len() < 2 {
        return Err("the proposal had fewer than two usable options".into());
    }
    let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").trim().chars().take(300).collect::<String>();
    let unverified: Vec<String> = v
        .get("unverified")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().chars().take(200).collect::<String>()).filter(|s| !s.is_empty()).take(8).collect())
        .unwrap_or_default();
    Ok(Proposal { document_type: text("document_type"), guideline, unverified, summary: text("summary"), choices, model: String::new(), issues: Issues::default() })
}

// ---------------------------------------------------------------------------------------------
// Looking at the document (no AI)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Issues {
    /// Body paragraphs that carry their own font, size, alignment or spacing.
    pub overrides: usize,
    /// Places where two or more blank paragraphs in a row are used as spacing.
    pub blank_gaps: usize,
    /// Page breaks directly after another page break.
    pub double_breaks: usize,
    /// A page break at the very end, which leaves a blank last page.
    pub trailing_break: bool,
}

pub struct Profile {
    /// What the model gets to see.
    pub summary: Value,
    pub issues: Issues,
}

/// An ordinary body paragraph: Normal style, text, no list, no section end, no picture.
fn is_body(p: &Paragraph) -> bool {
    p.section.is_none() && p.objects.is_empty() && p.props.numbering.is_none() && !p.text.trim().is_empty() && p.props.style.as_deref().is_none_or(|s| s == "Normal")
}

fn has_overrides(p: &Paragraph) -> bool {
    p.runs.iter().any(|r| r.props.font.is_some() || r.props.size.is_some())
        || p.props.align.is_some()
        || p.props.line_spacing.is_some()
        || p.props.space_after.is_some()
        || p.props.indent_first.is_some()
}

pub fn profile(s: &Session) -> Profile {
    let mut styles: BTreeMap<String, usize> = BTreeMap::new();
    let (mut words, mut paragraphs, mut tables) = (0_usize, 0_usize, 0_usize);
    let mut headings: Vec<Value> = Vec::new();
    let mut sample = String::new();
    let mut issues = Issues::default();
    let mut blank_streak = 0_usize;
    let mut prev_break = false;
    for b in &s.doc.body {
        let Some(p) = b.as_para() else {
            tables += 1;
            blank_streak = 0;
            prev_break = false;
            continue;
        };
        paragraphs += 1;
        let style = p.props.style.clone().unwrap_or_else(|| "Normal".to_string());
        *styles.entry(style.clone()).or_insert(0) += 1;
        words += p.text.split_whitespace().count();
        let has_break = p.text.contains(wordcraft_doc::para::PAGE_BREAK);
        if has_break && prev_break {
            issues.double_breaks += 1;
        }
        prev_break = has_break;
        if p.text.trim().is_empty() && !has_break && p.objects.is_empty() && p.section.is_none() {
            blank_streak += 1;
            if blank_streak == 2 {
                issues.blank_gaps += 1;
            }
        } else {
            blank_streak = 0;
        }
        if let Some(level) = style.strip_prefix("Heading").and_then(|d| d.parse::<u8>().ok()) {
            if headings.len() < MAX_HEADINGS {
                headings.push(json!({"level": level, "text": p.text.chars().take(80).collect::<String>()}));
            }
        } else if sample.chars().count() < 700 && is_body(p) {
            sample.extend(p.text.chars().take(300));
            sample.push('\n');
        }
        if is_body(p) && has_overrides(p) {
            issues.overrides += 1;
        }
    }
    // A page break after the last text leaves a blank page.
    for b in s.doc.body.iter().rev() {
        let Some(p) = b.as_para() else { break };
        if p.text.contains(wordcraft_doc::para::PAGE_BREAK) {
            issues.trailing_break = true;
        }
        if !p.text.trim().is_empty() {
            break;
        }
    }
    let sec = &s.doc.last_section;
    let summary = json!({
        "paragraphs": paragraphs,
        "words": words,
        "tables": tables,
        "styles_used": styles,
        "headings": headings,
        "text_sample": sample,
        "current": {
            "page_pt": [sec.page_w, sec.page_h],
            "margins_pt": {"top": sec.margin_top, "bottom": sec.margin_bottom, "left": sec.margin_left, "right": sec.margin_right},
            "body_font": s.doc.styles.default_chr.font,
            "body_size": s.doc.styles.get("Normal").and_then(|n| n.chr.size).or(s.doc.styles.default_chr.size)
        }
    });
    Profile { summary, issues }
}

/// Ask the models (with fallback) for formatting options. Runs on a worker thread.
pub fn propose(request: &str, profile: Profile, only: Option<usize>) -> Result<Proposal, String> {
    let request = if request.trim().is_empty() { "Format this document sensibly for its purpose." } else { request.trim() };
    let user = json!({"request": request, "document": profile.summary}).to_string();
    let reply = ai_net::ask(SYSTEM_PROMPT, &user, only, |text| parse_proposal(text).is_ok())?;
    let mut proposal = parse_proposal(&reply.text)?;
    proposal.model = reply.label;
    proposal.issues = profile.issues;
    Ok(proposal)
}

// ---------------------------------------------------------------------------------------------
// Applying an option
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Applied {
    /// Paragraphs whose own overrides were brought in line.
    pub fixed: usize,
    /// Paragraphs with monospace text, left alone on purpose.
    pub left_alone: usize,
}

/// A body paragraph (top-level block index) and what about it to correct.
struct Fix {
    block: usize,
    font: bool,
    size: bool,
    align: bool,
    spacing: bool,
}

fn is_mono(font: &str) -> bool {
    let lower = font.to_ascii_lowercase();
    MONO_HINTS.iter().any(|h| lower.contains(h))
}

fn align_name(a: Align) -> &'static str {
    match a {
        Align::Left => "left",
        Align::Center => "center",
        Align::Right => "right",
        Align::Justify => "justify",
        Align::Distribute => "distribute",
    }
}

/// Body paragraphs whose own formatting would fight the new styles.
fn find_fixes(s: &Session, c: &Choice, old_base: f32) -> (Vec<Fix>, usize) {
    let mut fixes = Vec::new();
    let mut left_alone = 0;
    for (block, b) in s.doc.body.iter().enumerate() {
        let Some(p) = b.as_para() else { continue };
        if !is_body(p) {
            continue;
        }
        if p.runs.iter().any(|r| r.props.font.as_deref().is_some_and(is_mono)) {
            if has_overrides(p) {
                left_alone += 1;
            }
            continue;
        }
        let long = p.text.chars().count() > 60;
        let font = p.runs.iter().any(|r| r.props.font.as_deref().is_some_and(|f| !f.eq_ignore_ascii_case(&c.body_font)));
        // Sizes near the old body size are strays; very different ones are deliberate.
        let size = p.runs.iter().any(|r| r.props.size.is_some_and(|x| (x - old_base).abs() <= 3.0 && (x - c.body_size).abs() > 0.1));
        let align = long && p.props.align.is_some_and(|a| a != c.align);
        let spacing = long && (p.props.line_spacing.is_some() || p.props.space_after.is_some() || p.props.indent_first.is_some());
        if (font || size || align || spacing) && fixes.len() < MAX_FIXES {
            fixes.push(Fix { block, font, size, align, spacing });
        }
    }
    (fixes, left_alone)
}

/// Runs engine commands, joining every edit after the first into one Undo step.
struct Edit<'a> {
    s: &'a mut Session,
    first: bool,
}

impl Edit<'_> {
    fn run(&mut self, id: &str, params: Value) -> Result<(), String> {
        if !self.first {
            self.s.join_next_undo();
        }
        self.first = false;
        self.s.run(id, &params).map(|_| ()).map_err(|e| format!("{id}: {e}"))
    }

    fn has_style(&self, id: &str) -> bool {
        self.s.doc.styles.get(id).is_some()
    }
}

fn apply_inner(e: &mut Edit<'_>, c: &Choice, fixes: &[Fix]) -> Result<(), String> {
    let spacing = |v: f32| json!({"rule": "multiple", "value": v});

    // 1. Theme fonts: heading and body fonts in every style that uses them.
    e.run("design.themeFonts", json!({"heading": c.heading_font, "body": c.body_font}))?;

    // 2. Body style: size, line spacing, paragraph spacing, first-line indent, alignment.
    e.run(
        "styles.modify",
        json!({
            "style": "Normal",
            "chr": {"size": c.body_size},
            "para": {
                "lineSpacing": spacing(c.line_spacing),
                "spaceBefore": 0.0,
                "spaceAfter": c.space_after,
                "indentFirst": c.first_line_indent,
                "align": align_name(c.align)
            }
        }),
    )?;

    // 3. Heading hierarchy. Double-spaced documents double-space their headings too.
    let double = c.line_spacing >= 1.9;
    let head_spacing = if double { c.line_spacing } else { c.line_spacing.min(1.15) };
    let [h1, h2, h3] = c.heading_sizes;
    let [a1, a2, a3] = c.heading_aligns;
    let levels: [(&str, f32, Align, f32, f32); 4] = [
        ("Title", c.title_size, c.title_align, 0.0, if double { 0.0 } else { 12.0 }),
        ("Heading1", h1, a1, if double { 0.0 } else { 18.0 }, if double { 0.0 } else { 6.0 }),
        ("Heading2", h2, a2, if double { 0.0 } else { 12.0 }, if double { 0.0 } else { 4.0 }),
        ("Heading3", h3, a3, if double { 0.0 } else { 10.0 }, if double { 0.0 } else { 4.0 }),
    ];
    for (id, size, align, before, after) in levels {
        if !e.has_style(id) {
            continue;
        }
        let chr = if id == "Title" { json!({"size": size}) } else { json!({"size": size, "bold": true}) };
        e.run(
            "styles.modify",
            json!({
                "style": id,
                "chr": chr,
                "para": {
                    "align": align_name(align),
                    "lineSpacing": spacing(head_spacing),
                    "spaceBefore": before,
                    "spaceAfter": after,
                    "indentFirst": 0.0,
                    "keepNext": true,
                    "keepLines": true
                }
            }),
        )?;
    }
    // Styles built on Normal must not inherit the first-line indent.
    if c.first_line_indent > 0.0 {
        for id in ["ListParagraph", "Caption"] {
            if e.has_style(id) {
                e.run("styles.modify", json!({"style": id, "para": {"indentFirst": 0.0}}))?;
            }
        }
    }

    // 4. Margins, for every section.
    e.run("caret.docEnd", json!({}))?;
    e.run("select.all", json!({}))?;
    let [top, bottom, left, right] = c.margins;
    e.run("layout.margins", json!({"top": top, "bottom": bottom, "left": left, "right": right}))?;

    // 5. Paragraphs with their own overrides, brought in line one by one.
    for f in fixes {
        e.run("caret.set", json!({"pos": {"block": f.block, "off": 0}}))?;
        e.run("select.paragraph", json!({}))?;
        if f.font {
            e.run("format.font", json!({"name": c.body_font}))?;
        }
        if f.size {
            e.run("format.size", json!({"size": c.body_size}))?;
        }
        if f.align {
            e.run("para.align", json!({"value": align_name(c.align)}))?;
        }
        if f.spacing {
            e.run("para.lineSpacing", json!({"value": c.line_spacing}))?;
            e.run("para.spacing", json!({"before": 0.0, "after": c.space_after}))?;
            e.run("para.indents", json!({"firstLine": c.first_line_indent}))?;
        }
    }
    Ok(())
}

/// Apply `choice` to the whole document as one Undo step. On a failure half-way, everything done
/// so far is undone. The user's selection is put back.
pub fn apply(s: &mut Session, choice: &Choice) -> Result<Applied, String> {
    let saved = s.sel.clone();
    let old_base = s.doc.styles.get("Normal").and_then(|st| st.chr.size).or(s.doc.styles.default_chr.size).unwrap_or(11.0);
    let (fixes, left_alone) = find_fixes(s, choice, old_base);
    let mut e = Edit { s: &mut *s, first: true };
    let result = apply_inner(&mut e, choice, &fixes);
    if result.is_err() && !e.first {
        let _ = e.s.run("edit.undo", &json!({}));
    }
    s.sel = saved;
    result.map(|()| Applied { fixed: fixes.len(), left_alone })
}

// ---------------------------------------------------------------------------------------------
// What to tell the user
// ---------------------------------------------------------------------------------------------

/// The chat message shown when the options arrive.
pub fn intro(p: &Proposal) -> String {
    let kind = if p.document_type.is_empty() { "document" } else { p.document_type.as_str() };
    let mut m = match p.guideline {
        Some(g) => format!("This looks like a {kind}. I'm using WordCraft's built-in {} rules for margins, font and size, line spacing and indents.", g.name()),
        None => format!("This looks like a {kind}. These are suggestions for its purpose, not an official rule set."),
    };
    if !p.summary.is_empty() {
        m.push('\n');
        m.push_str(&p.summary);
    }
    let i = &p.issues;
    let mut found: Vec<String> = Vec::new();
    if i.overrides > 0 {
        found.push(format!("{} body paragraphs carry their own font, size, alignment or spacing (I'll bring them in line)", i.overrides));
    }
    if i.blank_gaps > 0 {
        found.push(format!("{} places use several blank paragraphs in a row as spacing (left as they are)", i.blank_gaps));
    }
    if i.double_breaks > 0 {
        found.push(format!("{} page breaks directly follow another page break (left as they are)", i.double_breaks));
    }
    if i.trailing_break {
        found.push("a page break at the very end leaves a blank last page (left as it is)".to_string());
    }
    if !found.is_empty() {
        m.push_str("\n\nFound in your document:");
        for f in &found {
            m.push_str("\n- ");
            m.push_str(f);
        }
    }
    if !p.unverified.is_empty() {
        m.push_str("\n\nNot verified, so I did not apply them:");
        for u in &p.unverified {
            m.push_str("\n- ");
            m.push_str(u);
        }
    }
    m.push_str("\n\nPick an option below. Nothing changes until you do.");
    m
}

/// The chat message shown after an option was applied.
pub fn done_message(p: &Proposal, index: usize, c: &Choice, done: &Applied) -> String {
    let n = index + 1;
    let kind = c.category.label();
    let mut m = format!("Applied option {n} ({kind}): {}.", c.describe());
    m.push_str("\nStyles updated: Normal, Title and Heading 1 to 3. Page margins set for every section.");
    if done.fixed > 0 {
        m.push_str(&format!("\n{} paragraphs with their own formatting were brought in line.", done.fixed));
    }
    if done.left_alone > 0 {
        m.push_str(&format!("\n{} paragraphs with monospace text were left alone on purpose.", done.left_alone));
    }
    if let Some(g) = p.guideline {
        m.push_str(&format!("\nChecked against: {} (built-in: margins, font and size, spacing, indents). Title page, running head and reference formatting are not checked yet.", g.name()));
    }
    let model = &p.model;
    m.push_str(&format!("\n\nSuggested by {model}. Ctrl+Z undoes all of it, and you can pick another option."));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_clamped_and_fonts_come_from_the_list() {
        let reply = r#"{"document_type":"report","guideline":"none","options":[
            {"category":"sans","body_font":"Comic Sans","body_size":99,"line_spacing":9,"margins":{"top":1,"bottom":500}},
            {"category":"serif","body_font":"Georgia"}]}"#;
        let p = parse_proposal(reply).unwrap();
        assert_eq!(p.choices.len(), 2);
        let c = &p.choices[0];
        assert_eq!(c.body_font, "Calibri");
        assert_eq!(c.body_size, 16.0);
        assert_eq!(c.line_spacing, 2.5);
        assert_eq!(c.margins[0], 36.0);
        assert_eq!(c.margins[1], 144.0);
        assert_eq!(p.choices[1].body_font, "Georgia");
    }

    #[test]
    fn a_named_official_style_overrides_the_model() {
        let reply = r#"{"guideline":"APA 7","options":[{"category":"mono","line_spacing":1.0}]}"#;
        let p = parse_proposal(reply).unwrap();
        assert_eq!(p.guideline, Some(Guideline::Apa7));
        assert_eq!(p.choices.len(), 3);
        assert!(p.choices.iter().all(|c| c.line_spacing == 2.0 && c.margins == [72.0; 4]));
    }

    #[test]
    fn fewer_than_two_options_is_rejected() {
        assert!(parse_proposal(r#"{"options":[{"category":"sans"}]}"#).is_err());
        assert!(parse_proposal("no json here").is_err());
    }

    #[test]
    fn heading_sizes_never_go_below_the_body_or_grow_downwards() {
        let o: Value = serde_json::from_str(r#"{"body_size":12,"heading_sizes":[10,30,20]}"#).unwrap();
        let c = read_choice(&o, Category::Serif);
        assert_eq!(c.heading_sizes, [12.0, 12.0, 12.0]);
    }
}