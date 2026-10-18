//! "Word" AI, part 8: automatic validation and safe commit.
//!
//! Before an approved draft touches the real document it goes through four steps, all on a
//! private copy:
//!
//! 1. **Inspect**: look at the actual document (structure, formatting values, headings).
//! 2. **Render**: lay the pages out and draw them, then check for layout problems
//!    (blank pages, stranded headings, objects off the page, pages that fail to draw).
//! 3. **Repair**: fix only *safe formatting* errors (never text), then validate again.
//! 4. **Commit**: the caller (`ai_preview::commit`) replaces the document as ONE Undo step and
//!    checks the result once more, rolling back with Undo if the check fails.
//!
//! Repairs never change what the user wrote: the text fingerprint of the document is compared
//! before and after, and the commit is refused if it differs.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use wordcraft_doc::{Block, Document, Paragraph, StoryRef};
use wordcraft_engine::Session;
use wordcraft_layout::{DocLayout, Page, Placed};

/// Inspect, repair, inspect again: at most this many rounds.
const MAX_ROUNDS: usize = 3;
/// Pages drawn by the render check (a long document is sampled from the start).
const MAX_RENDER_PAGES: usize = 40;
/// The render check draws small thumbnails; it only needs to know whether a page is blank.
const THUMB_SCALE: f32 = 0.2;
/// This many empty paragraphs in a row are reported (once) as spacing done with the Enter key.
const EMPTY_STREAK: usize = 4;
/// Word's largest font size, points.
const MAX_FONT_PT: f32 = 1638.0;
/// Indents and spacing beyond this many points are treated as corrupt.
const MAX_SPACE_PT: f32 = 10_000.0;
/// Most findings listed in a message.
const LIST_MAX: usize = 5;

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    /// Blocks the commit unless repaired.
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Run list does not cover the paragraph text.
    Structure,
    /// A font size, indent or spacing value that is not a usable number.
    BadFormat,
    /// A heading alone at the bottom of a page, its text on the next page.
    StrandedHeading,
    /// A page that lays out to nothing.
    BlankPage,
    /// A floating object completely outside its page.
    ObjectOffPage,
    /// Several empty paragraphs in a row.
    EmptyParagraphs,
    /// Margins leave no room for text.
    Margins,
    /// The page could not be laid out or drawn.
    RenderFailed,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub severity: Severity,
    pub kind: Kind,
    /// Page index (0-based), when the problem is on a page.
    pub page: Option<usize>,
    /// Top-level body block index, when the problem is in a block.
    pub block: Option<usize>,
    pub message: String,
}

impl Issue {
    fn new(severity: Severity, kind: Kind, page: Option<usize>, block: Option<usize>, message: impl Into<String>) -> Issue {
        Issue { severity, kind, page, block, message: message.into() }
    }

    /// Only formatting problems that can be fixed without touching the text.
    pub fn repairable(&self) -> bool {
        matches!(self.kind, Kind::Structure | Kind::BadFormat | Kind::StrandedHeading) && self.block.is_some()
    }

    fn describe(&self) -> String {
        match self.page {
            Some(p) => format!("page {}: {}", p + 1, self.message),
            None => self.message.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub pages: usize,
    pub rounds: usize,
    /// What was repaired, one line each.
    pub repaired: Vec<String>,
    /// What is still wrong after the last inspection.
    pub remaining: Vec<Issue>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        self.remaining.iter().any(|i| i.severity == Severity::Error)
    }

    /// The errors that stop a commit, as lines of text.
    pub fn error_lines(&self) -> String {
        lines(self.remaining.iter().filter(|i| i.severity == Severity::Error))
    }

    /// One short report for the chat.
    pub fn summary(&self) -> String {
        let mut out = format!("Checked {} page{} in {} round{}.", self.pages, plural(self.pages), self.rounds, plural(self.rounds));
        if !self.repaired.is_empty() {
            out.push_str("\nRepaired automatically:");
            for r in self.repaired.iter().take(LIST_MAX) {
                out.push_str("\n• ");
                out.push_str(r);
            }
            if self.repaired.len() > LIST_MAX {
                out.push_str(&format!("\n• …and {} more", self.repaired.len() - LIST_MAX));
            }
        }
        let notes: Vec<&Issue> = self.remaining.iter().filter(|i| i.severity != Severity::Error).collect();
        if !notes.is_empty() {
            out.push_str("\nFor your review (not changed):");
            out.push_str(&lines(notes.iter().copied().take(LIST_MAX)));
            if notes.len() > LIST_MAX {
                out.push_str(&format!("\n• …and {} more", notes.len() - LIST_MAX));
            }
        }
        if self.repaired.is_empty() && notes.is_empty() {
            out.push_str(" No problems found.");
        }
        out
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn lines<'a>(it: impl Iterator<Item = &'a Issue>) -> String {
    let mut out = String::new();
    for i in it.take(LIST_MAX) {
        out.push_str("\n• ");
        out.push_str(&i.describe());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Fingerprint: proof that validation never changed the text
// ---------------------------------------------------------------------------------------------

/// A hash of everything the user wrote: paragraph text, and tables as a whole. Formatting
/// repairs on paragraphs do not change it.
pub fn text_fingerprint(doc: &Document) -> u64 {
    let mut h = DefaultHasher::new();
    for b in &doc.body {
        match &**b {
            Block::Para(p) => {
                0u8.hash(&mut h);
                p.text.hash(&mut h);
            }
            Block::Table(_) => {
                1u8.hash(&mut h);
                serde_json::to_string(&**b).unwrap_or_default().hash(&mut h);
            }
        }
    }
    h.finish()
}

// ---------------------------------------------------------------------------------------------
// 1. Inspect the document, 2. render the pages
// ---------------------------------------------------------------------------------------------

fn bad_len(v: Option<f32>) -> bool {
    v.is_some_and(|x| !x.is_finite() || x.abs() > MAX_SPACE_PT)
}

fn bad_size(v: Option<f32>) -> bool {
    v.is_some_and(|x| !x.is_finite() || x <= 0.0 || x > MAX_FONT_PT)
}

fn bad_format(p: &Paragraph) -> bool {
    p.runs.iter().any(|r| bad_size(r.props.size))
        || bad_size(p.mark.size)
        || bad_len(p.props.indent_left)
        || bad_len(p.props.indent_right)
        || bad_len(p.props.indent_first)
        || bad_len(p.props.space_before)
        || bad_len(p.props.space_after)
}

fn is_heading(p: &Paragraph) -> bool {
    p.props.outline_level.is_some_and(|l| l < 9) || p.props.style.as_deref().is_some_and(|s| s.to_ascii_lowercase().starts_with("heading"))
}

fn has_content(pg: &Page) -> bool {
    pg.items
        .iter()
        .any(|it| matches!(it, Placed::Lines { .. } | Placed::Image { .. } | Placed::Shape { .. } | Placed::Graphic { .. } | Placed::Cell { .. }))
}

/// The body block that starts (from its first line) at the top of `pg`, if one does.
fn block_starting_page(pg: &Page) -> Option<usize> {
    for it in &pg.items {
        if let Placed::Lines { story: StoryRef::Body, path, l0, .. } = it {
            let blk = path.0.first().map(|&b| b as usize)?;
            return if *l0 == 0 { Some(blk) } else { None };
        }
    }
    None
}

/// Look at the document and its layout. Pure: nothing is changed.
pub fn inspect(doc: &Document, layout: &DocLayout) -> Vec<Issue> {
    let mut out = Vec::new();

    // The document itself.
    let mut streak = 0usize;
    for (i, b) in doc.body.iter().enumerate() {
        let Block::Para(p) = &**b else {
            streak = 0;
            continue;
        };
        let covered: usize = p.runs.iter().map(|r| r.len).sum();
        if covered != p.text.len() {
            out.push(Issue::new(Severity::Warning, Kind::Structure, None, Some(i), format!("paragraph {} has formatting that does not cover its text", i + 1)));
        }
        if bad_format(p) {
            out.push(Issue::new(Severity::Warning, Kind::BadFormat, None, Some(i), format!("paragraph {} has an unusable font size, indent or spacing value", i + 1)));
        }
        if p.text.is_empty() && p.objects.is_empty() {
            streak += 1;
            if streak == EMPTY_STREAK {
                out.push(Issue::new(Severity::Info, Kind::EmptyParagraphs, None, Some(i), format!("{EMPTY_STREAK} or more empty paragraphs in a row near paragraph {}", i + 1)));
            }
        } else {
            streak = 0;
        }
    }
    for (i, s) in doc.sections() {
        let narrow_w = s.margin_left + s.margin_right >= s.page_w - 36.0;
        let narrow_h = s.margin_top + s.margin_bottom >= s.page_h - 36.0;
        if narrow_w || narrow_h {
            out.push(Issue::new(Severity::Error, Kind::Margins, None, Some(i), "the margins leave no room for text"));
        }
    }

    // The layout.
    if layout.pages.is_empty() {
        out.push(Issue::new(Severity::Error, Kind::RenderFailed, None, None, "the document laid out to no pages"));
        return out;
    }
    for (n, pg) in layout.pages.iter().enumerate() {
        if !has_content(pg) {
            out.push(Issue::new(Severity::Warning, Kind::BlankPage, Some(n), None, "the page is blank"));
        }
        for it in &pg.items {
            if let Placed::Object { rect, .. } = it {
                let vis_w = (rect.x + rect.w).min(pg.w) - rect.x.max(0.0);
                let vis_h = (rect.y + rect.h).min(pg.h) - rect.y.max(0.0);
                if vis_w <= 0.0 || vis_h <= 0.0 {
                    out.push(Issue::new(Severity::Warning, Kind::ObjectOffPage, Some(n), None, "an object lies completely outside the page"));
                }
            }
        }
        // A heading that ends this page while the block after it starts the next one.
        if let Some(next) = layout.pages.get(n + 1)
            && let Some(first) = block_starting_page(next)
            && first > 0
            && let Some(Block::Para(prev)) = doc.body.get(first - 1).map(|b| &**b)
            && is_heading(prev)
            && prev.props.keep_next != Some(true)
        {
            out.push(Issue::new(Severity::Warning, Kind::StrandedHeading, Some(n), Some(first - 1), "a heading is left alone at the bottom of the page"));
        }
    }
    out
}

/// Draw the pages (small) and check they can be drawn and are not blank when they have content.
pub fn render_check(doc: &Document, layout: &DocLayout) -> Vec<Issue> {
    let mut out = Vec::new();
    for (n, pg) in layout.pages.iter().enumerate().take(MAX_RENDER_PAGES) {
        if !(pg.w.is_finite() && pg.h.is_finite() && pg.w > 0.0 && pg.h > 0.0) {
            out.push(Issue::new(Severity::Error, Kind::RenderFailed, Some(n), None, "the page has no usable size"));
            continue;
        }
        let img = wordcraft_render::render_area(doc, pg, 0.0, 0.0, pg.w, pg.h, THUMB_SCALE, &Default::default());
        let px = img.to_straight();
        let expected = (img.width as usize).checked_mul(img.height as usize).and_then(|n| n.checked_mul(4));
        if img.width == 0 || img.height == 0 || expected != Some(px.len()) {
            out.push(Issue::new(Severity::Error, Kind::RenderFailed, Some(n), None, "the page could not be drawn"));
            continue;
        }
        if has_content(pg) {
            let first = px.get(0..4);
            if first.is_some() && px.chunks_exact(4).all(|c| Some(c) == first) {
                out.push(Issue::new(Severity::Warning, Kind::BlankPage, Some(n), None, "the page has content but draws blank"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// 3. Repair safe formatting errors
// ---------------------------------------------------------------------------------------------

/// Fix what `issues` lists and is safe to fix: run lists, unusable numbers, stranded headings.
/// Text is never touched. Returns one line per kind of repair.
pub fn repair(doc: &mut Document, issues: &[Issue]) -> Vec<String> {
    let (mut structure, mut format, mut headings) = (0usize, 0usize, 0usize);
    for issue in issues.iter().filter(|i| i.repairable()) {
        let Some(bi) = issue.block else { continue };
        let Some(slot) = doc.body.get_mut(bi) else { continue };
        let Block::Para(p) = Arc::make_mut(slot) else { continue };
        match issue.kind {
            Kind::Structure => {
                p.normalize();
                structure += 1;
            }
            Kind::BadFormat => {
                for r in &mut p.runs {
                    if bad_size(r.props.size) {
                        r.props.size = None;
                    }
                }
                if bad_size(p.mark.size) {
                    p.mark.size = None;
                }
                for v in [&mut p.props.indent_left, &mut p.props.indent_right, &mut p.props.indent_first, &mut p.props.space_before, &mut p.props.space_after] {
                    if bad_len(*v) {
                        *v = None;
                    }
                }
                format += 1;
            }
            Kind::StrandedHeading => {
                p.props.keep_next = Some(true);
                headings += 1;
            }
            _ => continue,
        }
        p.touch();
    }
    let mut notes = Vec::new();
    if structure > 0 {
        notes.push(format!("rebuilt the formatting of {structure} paragraph{}", plural(structure)));
    }
    if format > 0 {
        notes.push(format!("reset unusable sizes or spacing in {format} paragraph{}", plural(format)));
    }
    if headings > 0 {
        notes.push(format!("kept {headings} heading{} with the text that follows", plural(headings)));
    }
    notes
}

// ---------------------------------------------------------------------------------------------
// Inspect -> render -> repair -> validate again
// ---------------------------------------------------------------------------------------------

/// Run the whole check on a private copy of `doc`. Returns the document (repaired where that
/// was safe) and what was found. The caller's real document is never involved.
pub fn validate(mut doc: Document) -> (Document, Report) {
    let mut report = Report::default();
    let remaining = loop {
        report.rounds += 1;
        let layout = Session::new(doc.clone()).export_layout();
        report.pages = layout.pages.len();
        let mut issues = inspect(&doc, &layout);
        issues.extend(render_check(&doc, &layout));
        if report.rounds >= MAX_ROUNDS || !issues.iter().any(Issue::repairable) {
            break issues;
        }
        let notes = repair(&mut doc, &issues);
        if notes.is_empty() {
            break issues;
        }
        report.repaired.extend(notes);
    };
    report.remaining = remaining;
    (doc, report)
}

/// Check the live document after a commit (no repair). Used to decide whether to roll back.
pub fn verify_committed(doc: &Document, layout: &DocLayout) -> Vec<Issue> {
    let mut v = inspect(doc, layout);
    v.extend(render_check(doc, layout));
    v.retain(|i| i.severity == Severity::Error);
    v
}

pub fn error_text(issues: &[Issue]) -> String {
    lines(issues.iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_document_has_no_errors() {
        let (_, report) = validate(Document::from_text("Hello\nWorld"));
        assert!(!report.has_errors());
        assert!(report.pages >= 1);
    }

    #[test]
    fn unusable_font_size_is_repaired_and_text_is_untouched() {
        let mut doc = Document::from_text("Hello");
        if let Some(slot) = doc.body.get_mut(0)
            && let Block::Para(p) = Arc::make_mut(slot)
        {
            p.mark.size = Some(f32::NAN);
            for r in &mut p.runs {
                r.props.size = Some(-4.0);
            }
        }
        let before = text_fingerprint(&doc);
        let (fixed, report) = validate(doc);
        assert!(!report.repaired.is_empty());
        assert_eq!(text_fingerprint(&fixed), before);
        let still_bad = fixed.body.iter().any(|b| b.as_para().is_some_and(bad_format));
        assert!(!still_bad);
    }

    #[test]
    fn run_list_that_misses_the_text_is_rebuilt() {
        let mut doc = Document::from_text("Hello");
        if let Some(slot) = doc.body.get_mut(0)
            && let Block::Para(p) = Arc::make_mut(slot)
        {
            p.runs.clear();
        }
        let (fixed, report) = validate(doc);
        assert!(!report.repaired.is_empty());
        let ok = fixed.body.iter().all(|b| b.as_para().is_none_or(|p| p.runs.iter().map(|r| r.len).sum::<usize>() == p.text.len()));
        assert!(ok);
    }

    #[test]
    fn fingerprint_changes_with_text() {
        assert_ne!(text_fingerprint(&Document::from_text("a")), text_fingerprint(&Document::from_text("b")));
    }
}