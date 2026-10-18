//! "Word" AI, part 7: preview editing before committing.
//!
//! Every AI edit (a new document, a formatting option) is first built in an **isolated draft**:
//! a private copy of the document that only this module sees. The real document is not touched.
//!
//! The preview window shows the original (locked) and the proposed draft side by side, with every
//! change numbered, framed on the page and listed under one of four categories:
//! Content, Typography, Spacing, Layout.
//!
//! Each change, each category, or the whole draft can be accepted or rejected. Only when the user
//! presses Apply is the document replaced, as ONE Undo step, by the draft with every change that
//! was not accepted taken back to the original. Nothing is applied while the original has been
//! edited since the draft was made.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::f32::consts::PI;
use std::sync::Arc;

use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, Ui, pos2, vec2};
use wordcraft_doc::props::{CharProps, LineSpacing, ParaProps};
use wordcraft_doc::{Block, Document, Paragraph, StoryRef};
use wordcraft_engine::Session;
use wordcraft_layout::{DocLayout, Placed};

use crate::WordApp;
use crate::ai_validate;
use crate::change_marks;
use crate::theme::Tokens;

/// Above this many table-of-contents-sized steps the exact alignment is skipped.
const LCS_LIMIT: usize = 4_000_000;
/// Up to this many blocks in one edited stretch are listed one by one; more are one change.
const SPLIT_LIMIT: usize = 40;

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Content,
    Typography,
    Spacing,
    Layout,
}

impl Category {
    pub const ALL: [Category; 4] = [Category::Content, Category::Typography, Category::Spacing, Category::Layout];

    pub fn label(self) -> &'static str {
        match self {
            Category::Content => "Content",
            Category::Typography => "Typography",
            Category::Spacing => "Spacing",
            Category::Layout => "Layout",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Category::Content => Color32::from_rgb(240, 130, 40),
            Category::Typography => Color32::from_rgb(150, 100, 240),
            Category::Spacing => Color32::from_rgb(20, 165, 170),
            Category::Layout => Color32::from_rgb(70, 130, 240),
        }
    }

    fn slot(self) -> usize {
        self as usize
    }

    /// Spacing and layout are drawn with a dashed frame, content and typography with a solid one.
    fn dashed(self) -> bool {
        matches!(self, Category::Spacing | Category::Layout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug)]
enum Target {
    /// Content: blocks of the original replaced by blocks of the draft (either side may be empty).
    Blocks { del: Vec<usize>, ins: Vec<usize> },
    /// Formatting of paragraphs whose text is unchanged: (original block, draft block).
    Paras(Vec<(usize, usize)>),
    /// One style's definition (which part: the change's category).
    Style { id: String },
    /// The document's default character formatting.
    DefaultFont,
    /// Page size and margins.
    Page,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub category: Category,
    pub title: String,
    pub detail: String,
    target: Target,
    pub verdict: Verdict,
}

/// How the merged body is put together, in document order.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// A block present in both versions (text equal): original index, draft index.
    Same { o: usize, d: usize },
    /// An edited stretch: the draft's blocks if accepted, the original's if not.
    Unit { change: usize },
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Same(usize, usize),
    Del(usize),
    Ins(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Original,
    Draft,
}

struct Tex {
    page: usize,
    left: Option<TextureHandle>,
    right: Option<TextureHandle>,
    canvas: (f32, f32),
}

pub struct Draft {
    pub title: String,
    origin: Document,
    draft: Document,
    /// (document id, revision) of the real document when the draft was made.
    stamp: (u64, u64),
    steps: Vec<Step>,
    pub changes: Vec<Change>,
    origin_layout: Arc<DocLayout>,
    draft_layout: Arc<DocLayout>,
    page: usize,
    selected: Option<usize>,
    hidden: [bool; 4],
    tex: Option<Tex>,
    /// Scroll the list to the selected row once.
    jump: Cell<bool>,
}

#[derive(Default)]
pub struct State {
    draft: Option<Draft>,
}

impl State {
    pub fn open(&mut self, d: Draft) {
        self.draft = Some(d);
    }

    pub fn is_open(&self) -> bool {
        self.draft.is_some()
    }
}

impl Draft {
    pub fn change_count(&self) -> usize {
        self.changes.len()
    }
}

// ---------------------------------------------------------------------------------------------
// Making a draft
// ---------------------------------------------------------------------------------------------

/// Copy the document into a private session, let `edit` change that copy, and work out what
/// changed. The real session is only read.
pub fn build_draft(real: &Session, title: &str, edit: impl FnOnce(&mut Session) -> Result<(), String>) -> Result<Draft, String> {
    let mut tmp = Session::new(real.doc.clone());
    tmp.sel = real.sel.clone();
    // The copy as the private session sees it, so only the AI's own edits show up as changes.
    let origin = tmp.doc.clone();
    edit(&mut tmp)?;
    let draft = tmp.doc.clone();
    let (changes, steps) = diff(&origin, &draft);
    if changes.is_empty() {
        return Err("The draft is identical to your document, so there is nothing to preview.".into());
    }
    let origin_layout = Session::new(origin.clone()).export_layout();
    let draft_layout = tmp.export_layout();
    let mut d = Draft {
        title: title.to_string(),
        origin,
        draft,
        stamp: (real.document_id(), real.rev()),
        steps,
        changes,
        origin_layout,
        draft_layout,
        page: 0,
        selected: None,
        hidden: [false; 4],
        tex: None,
        jump: Cell::new(false),
    };
    if !d.changes.is_empty() {
        d.selected = Some(0);
        d.page = d.page_of(0).unwrap_or(0);
    }
    Ok(d)
}

// ---------------------------------------------------------------------------------------------
// Finding the changes (no AI)
// ---------------------------------------------------------------------------------------------

fn key(b: &Block) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match b {
        Block::Para(p) => {
            0u8.hash(&mut h);
            p.text.hash(&mut h);
        }
        Block::Table(_) => {
            1u8.hash(&mut h);
            serde_json::to_string(b).unwrap_or_default().hash(&mut h);
        }
    }
    h.finish()
}

/// Line the two block lists up: common start and end first, then the longest common stretch.
fn align(a: &[u64], b: &[u64]) -> Vec<Op> {
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let am = &a[pre..a.len() - suf];
    let bm = &b[pre..b.len() - suf];
    let mut ops: Vec<Op> = (0..pre).map(|i| Op::Same(i, i)).collect();
    if am.len() * bm.len() <= LCS_LIMIT {
        let (n, m) = (am.len(), bm.len());
        let w = m + 1;
        let mut t = vec![0u32; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * w + j] = if am[i] == bm[j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if am[i] == bm[j] {
                ops.push(Op::Same(pre + i, pre + j));
                i += 1;
                j += 1;
            } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
                ops.push(Op::Del(pre + i));
                i += 1;
            } else {
                ops.push(Op::Ins(pre + j));
                j += 1;
            }
        }
        while i < n {
            ops.push(Op::Del(pre + i));
            i += 1;
        }
        while j < m {
            ops.push(Op::Ins(pre + j));
            j += 1;
        }
    } else {
        ops.extend((0..am.len()).map(|i| Op::Del(pre + i)));
        ops.extend((0..bm.len()).map(|j| Op::Ins(pre + j)));
    }
    for k in 0..suf {
        ops.push(Op::Same(a.len() - suf + k, b.len() - suf + k));
    }
    ops
}

fn spacing_differs(o: &ParaProps, d: &ParaProps) -> bool {
    o.indent_left != d.indent_left
        || o.indent_right != d.indent_right
        || o.indent_first != d.indent_first
        || o.space_before != d.space_before
        || o.space_after != d.space_after
        || o.line_spacing != d.line_spacing
        || o.contextual_spacing != d.contextual_spacing
}

fn copy_spacing(to: &mut ParaProps, from: &ParaProps) {
    to.indent_left = from.indent_left;
    to.indent_right = from.indent_right;
    to.indent_first = from.indent_first;
    to.space_before = from.space_before;
    to.space_after = from.space_after;
    to.line_spacing = from.line_spacing;
    to.contextual_spacing = from.contextual_spacing;
}

fn layout_differs(o: &ParaProps, d: &ParaProps) -> bool {
    o.align != d.align
        || o.keep_next != d.keep_next
        || o.keep_lines != d.keep_lines
        || o.page_break_before != d.page_break_before
        || o.widow_control != d.widow_control
        || o.outline_level != d.outline_level
}

fn copy_layout(to: &mut ParaProps, from: &ParaProps) {
    to.align = from.align;
    to.keep_next = from.keep_next;
    to.keep_lines = from.keep_lines;
    to.page_break_before = from.page_break_before;
    to.widow_control = from.widow_control;
    to.outline_level = from.outline_level;
}

/// [typography, spacing, layout] differences of two paragraphs with the same text.
fn para_diff(o: &Paragraph, d: &Paragraph) -> [bool; 3] {
    [o.runs != d.runs || o.props.style != d.props.style, spacing_differs(&o.props, &d.props), layout_differs(&o.props, &d.props)]
}

fn revert_para(p: &mut Paragraph, o: &Paragraph, flags: [bool; 3]) {
    if flags[0] {
        p.runs = o.runs.clone();
        p.props.style = o.props.style.clone();
    }
    if flags[1] {
        copy_spacing(&mut p.props, &o.props);
    }
    if flags[2] {
        copy_layout(&mut p.props, &o.props);
    }
}

// ----- plain-language descriptions ----------------------------------------------------------

fn fmt_pt(x: Option<f32>) -> String {
    x.map_or_else(|| "style default".to_string(), |v| format!("{} pt", (v * 10.0).round() / 10.0))
}

fn fmt_font(x: &Option<String>) -> String {
    x.clone().unwrap_or_else(|| "style default".to_string())
}

fn fmt_ls(x: Option<LineSpacing>) -> String {
    match x {
        None => "style default".to_string(),
        Some(LineSpacing::Multiple(v)) => format!("{}x", (v * 100.0).round() / 100.0),
        Some(LineSpacing::AtLeast(v)) => format!("at least {} pt", (v * 10.0).round() / 10.0),
        Some(LineSpacing::Exactly(v)) => format!("exactly {} pt", (v * 10.0).round() / 10.0),
    }
}

fn fmt_align(x: Option<wordcraft_doc::Align>) -> String {
    x.map_or_else(|| "style default".to_string(), |a| format!("{a:?}").to_lowercase())
}

fn typo_detail(o: &CharProps, d: &CharProps) -> String {
    let mut v = Vec::new();
    if o.font != d.font {
        v.push(format!("font {} → {}", fmt_font(&o.font), fmt_font(&d.font)));
    }
    if o.size != d.size {
        v.push(format!("size {} → {}", fmt_pt(o.size), fmt_pt(d.size)));
    }
    if o.bold != d.bold {
        v.push(format!("bold {} → {}", o.bold.unwrap_or(false), d.bold.unwrap_or(false)));
    }
    if v.is_empty() {
        v.push("character formatting".to_string());
    }
    v.join("; ")
}

fn spacing_detail(o: &ParaProps, d: &ParaProps) -> String {
    let mut v = Vec::new();
    if o.line_spacing != d.line_spacing {
        v.push(format!("line spacing {} → {}", fmt_ls(o.line_spacing), fmt_ls(d.line_spacing)));
    }
    if o.space_before != d.space_before {
        v.push(format!("before {} → {}", fmt_pt(o.space_before), fmt_pt(d.space_before)));
    }
    if o.space_after != d.space_after {
        v.push(format!("after {} → {}", fmt_pt(o.space_after), fmt_pt(d.space_after)));
    }
    if o.indent_first != d.indent_first {
        v.push(format!("first-line indent {} → {}", fmt_pt(o.indent_first), fmt_pt(d.indent_first)));
    }
    if o.indent_left != d.indent_left || o.indent_right != d.indent_right {
        v.push("side indents".to_string());
    }
    if v.is_empty() {
        v.push("paragraph spacing".to_string());
    }
    v.join("; ")
}

fn layout_detail(o: &ParaProps, d: &ParaProps) -> String {
    let mut v = Vec::new();
    if o.align != d.align {
        v.push(format!("alignment {} → {}", fmt_align(o.align), fmt_align(d.align)));
    }
    if o.keep_next != d.keep_next || o.keep_lines != d.keep_lines || o.page_break_before != d.page_break_before || o.widow_control != d.widow_control {
        v.push("pagination (keep together, page break)".to_string());
    }
    if o.outline_level != d.outline_level {
        v.push("outline level".to_string());
    }
    if v.is_empty() {
        v.push("layout".to_string());
    }
    v.join("; ")
}

fn para_font_size(p: &Paragraph) -> CharProps {
    CharProps {
        font: p.runs.iter().find_map(|r| r.props.font.clone()),
        size: p.runs.iter().find_map(|r| r.props.size),
        ..Default::default()
    }
}

fn snippet(b: &Block) -> String {
    match b {
        Block::Table(_) => "a table".to_string(),
        Block::Para(p) => {
            let pb = wordcraft_doc::para::PAGE_BREAK;
            if p.text.contains(pb) && p.text.trim_matches(pb).trim().is_empty() {
                return "a page break".to_string();
            }
            let t = p.text.trim();
            if t.is_empty() {
                return "a blank paragraph".to_string();
            }
            let mut s: String = t.chars().take(70).collect();
            if t.chars().count() > 70 {
                s.push('…');
            }
            format!("“{s}”")
        }
    }
}

fn kind_of(b: &Block) -> &'static str {
    match b {
        Block::Table(_) => "table",
        Block::Para(p) => match p.props.style.as_deref() {
            Some("Title") => "title",
            Some(s) if s.starts_with("Heading") => "heading",
            _ if p.props.numbering.is_some() => "list item",
            _ => "paragraph",
        },
    }
}

fn describe_unit(origin: &Document, draft: &Document, del: &[usize], ins: &[usize]) -> (String, String) {
    fn at(d: &Document, i: usize) -> &Block {
        &d.body[i]
    }
    let ob = |i: usize| at(origin, i);
    let db = |i: usize| at(draft, i);
    let more = |v: &[usize]| if v.len() > 1 { format!(" (+{} more)", v.len() - 1) } else { String::new() };
    match (del.is_empty(), ins.is_empty()) {
        (true, false) => (format!("Added {}{}", kind_of(db(ins[0])), more(ins)), snippet(db(ins[0]))),
        (false, true) => (format!("Removed {}{}", kind_of(ob(del[0])), more(del)), snippet(ob(del[0]))),
        _ => {
            let (o, d) = (del[0], ins[0]);
            (format!("Rewrote {}{}", kind_of(db(d)), more(ins)), format!("{} → {}", snippet(ob(o)), snippet(db(d))))
        }
    }
}

// ----- page setup ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Setup {
    w: f32,
    h: f32,
    landscape: bool,
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
}

fn setups(doc: &Document) -> Vec<Setup> {
    let mut v: Vec<Setup> = doc
        .body
        .iter()
        .filter_map(|b| b.as_para())
        .filter_map(|p| p.section.as_ref())
        .map(|s| Setup { w: s.page_w, h: s.page_h, landscape: s.landscape, top: s.margin_top, bottom: s.margin_bottom, left: s.margin_left, right: s.margin_right })
        .collect();
    let s = &doc.last_section;
    v.push(Setup { w: s.page_w, h: s.page_h, landscape: s.landscape, top: s.margin_top, bottom: s.margin_bottom, left: s.margin_left, right: s.margin_right });
    v
}

fn apply_setups(doc: &mut Document, list: &[Setup]) {
    let idx: Vec<usize> = doc.body.iter().enumerate().filter(|(_, b)| b.as_para().is_some_and(|p| p.section.is_some())).map(|(i, _)| i).collect();
    for (k, i) in idx.into_iter().enumerate() {
        let Some(s) = list.get(k) else { break };
        if let Block::Para(p) = Arc::make_mut(&mut doc.body[i]) {
            if let Some(sec) = p.section.as_mut() {
                sec.page_w = s.w;
                sec.page_h = s.h;
                sec.landscape = s.landscape;
                sec.margin_top = s.top;
                sec.margin_bottom = s.bottom;
                sec.margin_left = s.left;
                sec.margin_right = s.right;
            }
            p.touch();
        }
    }
    if let Some(s) = list.last() {
        let sec = &mut doc.last_section;
        sec.page_w = s.w;
        sec.page_h = s.h;
        sec.landscape = s.landscape;
        sec.margin_top = s.top;
        sec.margin_bottom = s.bottom;
        sec.margin_left = s.left;
        sec.margin_right = s.right;
    }
}

fn setup_detail(o: &[Setup], d: &[Setup]) -> String {
    let inch = |pt: f32| format!("{:.2} in", pt / 72.0);
    let mut v = Vec::new();
    for (a, b) in o.iter().zip(d) {
        if a.top != b.top {
            v.push(format!("top {} → {}", inch(a.top), inch(b.top)));
        }
        if a.bottom != b.bottom {
            v.push(format!("bottom {} → {}", inch(a.bottom), inch(b.bottom)));
        }
        if a.left != b.left {
            v.push(format!("left {} → {}", inch(a.left), inch(b.left)));
        }
        if a.right != b.right {
            v.push(format!("right {} → {}", inch(a.right), inch(b.right)));
        }
        if a.w != b.w || a.h != b.h || a.landscape != b.landscape {
            v.push("page size or orientation".to_string());
        }
        if !v.is_empty() {
            break;
        }
    }
    if o.len() > 1 {
        v.push(format!("{} sections", o.len()));
    }
    v.join("; ")
}

// ----- the diff ------------------------------------------------------------------------------

fn push(changes: &mut Vec<Change>, category: Category, title: String, detail: String, target: Target) -> usize {
    changes.push(Change { category, title, detail, target, verdict: Verdict::Pending });
    changes.len() - 1
}

fn diff(origin: &Document, draft: &Document) -> (Vec<Change>, Vec<Step>) {
    let a: Vec<u64> = origin.body.iter().map(|b| key(b)).collect();
    let b: Vec<u64> = draft.body.iter().map(|b| key(b)).collect();
    let ops = align(&a, &b);
    let mut changes: Vec<Change> = Vec::new();
    let mut steps: Vec<Step> = Vec::new();

    // Content: stretches of the document that were added, removed or rewritten.
    let mut k = 0;
    while k < ops.len() {
        match ops[k] {
            Op::Same(o, d) => {
                steps.push(Step::Same { o, d });
                k += 1;
            }
            _ => {
                let (mut del, mut ins) = (Vec::new(), Vec::new());
                while k < ops.len() {
                    match ops[k] {
                        Op::Del(o) => del.push(o),
                        Op::Ins(d) => ins.push(d),
                        Op::Same(..) => break,
                    }
                    k += 1;
                }
                let units: Vec<(Vec<usize>, Vec<usize>)> = if del.len().max(ins.len()) <= SPLIT_LIMIT {
                    (0..del.len().max(ins.len()))
                        .map(|i| (del.get(i).map(|&x| vec![x]).unwrap_or_default(), ins.get(i).map(|&x| vec![x]).unwrap_or_default()))
                        .collect()
                } else {
                    vec![(del, ins)]
                };
                for (del, ins) in units {
                    let (title, detail) = describe_unit(origin, draft, &del, &ins);
                    let idx = push(&mut changes, Category::Content, title, detail, Target::Blocks { del, ins });
                    steps.push(Step::Unit { change: idx });
                }
            }
        }
    }

    // Styles: Normal, Title, Headings and any other style the draft redefined.
    for ds in &draft.styles.styles {
        let Some(os) = origin.styles.get(&ds.id) else { continue };
        let name = if ds.name.is_empty() { ds.id.clone() } else { ds.name.clone() };
        if os.chr != ds.chr {
            push(&mut changes, Category::Typography, format!("Style “{name}”"), typo_detail(&os.chr, &ds.chr), Target::Style { id: ds.id.clone() });
        }
        if spacing_differs(&os.para, &ds.para) {
            push(&mut changes, Category::Spacing, format!("Style “{name}”"), spacing_detail(&os.para, &ds.para), Target::Style { id: ds.id.clone() });
        }
        if layout_differs(&os.para, &ds.para) {
            push(&mut changes, Category::Layout, format!("Style “{name}”"), layout_detail(&os.para, &ds.para), Target::Style { id: ds.id.clone() });
        }
    }
    if origin.styles.default_chr != draft.styles.default_chr {
        push(
            &mut changes,
            Category::Typography,
            "Document default font".to_string(),
            typo_detail(&origin.styles.default_chr, &draft.styles.default_chr),
            Target::DefaultFont,
        );
    }

    // Paragraphs with the same text whose own formatting differs.
    let mut per: [Vec<(usize, usize)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for step in &steps {
        if let Step::Same { o, d } = *step
            && let (Some(po), Some(pd)) = (origin.body[o].as_para(), draft.body[d].as_para())
        {
            let flags = para_diff(po, pd);
            for (slot, on) in flags.into_iter().enumerate() {
                if on {
                    per[slot].push((o, d));
                }
            }
        }
    }
    let cats = [Category::Typography, Category::Spacing, Category::Layout];
    let what = ["font, size or style", "spacing", "alignment or pagination"];
    for (slot, pairs) in per.into_iter().enumerate() {
        if pairs.is_empty() {
            continue;
        }
        if pairs.len() <= SPLIT_LIMIT {
            for (o, d) in pairs {
                let (po, pd) = (origin.body[o].as_para(), draft.body[d].as_para());
                let detail = match (po, pd) {
                    (Some(po), Some(pd)) => match slot {
                        0 => {
                            if po.props.style != pd.props.style {
                                format!("style {} → {}", po.props.style.as_deref().unwrap_or("Normal"), pd.props.style.as_deref().unwrap_or("Normal"))
                            } else {
                                typo_detail(&para_font_size(po), &para_font_size(pd))
                            }
                        }
                        1 => spacing_detail(&po.props, &pd.props),
                        _ => layout_detail(&po.props, &pd.props),
                    },
                    _ => String::new(),
                };
                push(&mut changes, cats[slot], format!("Paragraph {}: {}", d + 1, what[slot]), detail, Target::Paras(vec![(o, d)]));
            }
        } else {
            let n = pairs.len();
            push(
                &mut changes,
                cats[slot],
                format!("{n} paragraphs: {}", what[slot]),
                "These paragraphs carry their own formatting that differs from the draft.".to_string(),
                Target::Paras(pairs),
            );
        }
    }

    // Page setup.
    let (so, sd) = (setups(origin), setups(draft));
    if so.len() == sd.len() && so != sd {
        push(&mut changes, Category::Layout, "Page setup".to_string(), setup_detail(&so, &sd), Target::Page);
    }
    (changes, steps)
}

// ---------------------------------------------------------------------------------------------
// Merging: the draft, with every change that was not accepted taken back
// ---------------------------------------------------------------------------------------------

impl Draft {
    fn counts(&self) -> (usize, usize, usize) {
        let acc = self.changes.iter().filter(|c| c.verdict == Verdict::Accepted).count();
        let rej = self.changes.iter().filter(|c| c.verdict == Verdict::Rejected).count();
        (acc, rej, self.changes.len() - acc - rej)
    }

    fn merged(&self) -> Result<Document, String> {
        let (acc, _, _) = self.counts();
        if acc == 0 {
            return Err("Nothing is accepted yet. Accept at least one change.".into());
        }
        if acc == self.changes.len() {
            return Ok(self.draft.clone());
        }
        let accepted = |i: usize| self.changes[i].verdict == Verdict::Accepted;
        let mut doc = self.draft.clone();

        // Paragraph formatting that was not accepted, by draft block: [typography, spacing, layout].
        let mut undo: HashMap<usize, [bool; 3]> = HashMap::new();
        for (i, c) in self.changes.iter().enumerate() {
            if accepted(i) {
                continue;
            }
            if let Target::Paras(pairs) = &c.target {
                let slot = match c.category {
                    Category::Typography => 0,
                    Category::Spacing => 1,
                    _ => 2,
                };
                for &(_, d) in pairs {
                    undo.entry(d).or_insert([false; 3])[slot] = true;
                }
            }
        }

        let mut body: Vec<Arc<Block>> = Vec::with_capacity(self.draft.body.len());
        for step in &self.steps {
            match *step {
                Step::Same { o, d } => {
                    let flags = undo.get(&d).copied();
                    let reverted: Option<Paragraph> = match (flags, self.origin.body[o].as_para(), self.draft.body[d].as_para()) {
                        (Some(f), Some(po), Some(pd)) => {
                            let mut p = pd.clone();
                            revert_para(&mut p, po, f);
                            p.touch();
                            Some(p)
                        }
                        _ => None,
                    };
                    body.push(match reverted {
                        Some(p) => Arc::new(Block::Para(p)),
                        None => self.draft.body[d].clone(),
                    });
                }
                Step::Unit { change } => {
                    if let Target::Blocks { del, ins } = &self.changes[change].target {
                        if accepted(change) {
                            body.extend(ins.iter().map(|&i| self.draft.body[i].clone()));
                        } else {
                            body.extend(del.iter().map(|&i| self.origin.body[i].clone()));
                        }
                    }
                }
            }
        }
        doc.body = body;

        for (i, c) in self.changes.iter().enumerate() {
            if accepted(i) {
                continue;
            }
            match &c.target {
                Target::Style { id } => {
                    if let (Some(os), Some(ms)) = (self.origin.styles.get(id), doc.styles.get_mut(id)) {
                        match c.category {
                            Category::Typography => ms.chr = os.chr.clone(),
                            Category::Spacing => copy_spacing(&mut ms.para, &os.para),
                            Category::Layout => copy_layout(&mut ms.para, &os.para),
                            Category::Content => {}
                        }
                    }
                }
                Target::DefaultFont => doc.styles.default_chr = self.origin.styles.default_chr.clone(),
                Target::Page => {
                    let so = setups(&self.origin);
                    if so.len() == setups(&doc).len() {
                        apply_setups(&mut doc, &so);
                    }
                }
                _ => {}
            }
        }
        doc.ensure_nonempty();
        Ok(doc)
    }

    /// The page (in the version the change shows on) where a change first appears.
    fn page_of(&self, i: usize) -> Option<usize> {
        let (block, layout) = match &self.changes.get(i)?.target {
            Target::Blocks { del, ins } => {
                if let Some(&b) = ins.first() {
                    (b, &self.draft_layout)
                } else if let Some(&b) = del.first() {
                    (b, &self.origin_layout)
                } else {
                    return None;
                }
            }
            Target::Paras(p) => (p.first()?.1, &self.draft_layout),
            _ => return None,
        };
        let mut page = 0;
        for (n, pg) in layout.pages.iter().enumerate() {
            if pg.first_block <= block {
                page = n;
            } else {
                break;
            }
        }
        Some(page)
    }

    fn total_pages(&self) -> usize {
        self.origin_layout.pages.len().max(self.draft_layout.pages.len()).max(1)
    }
}

/// Validate the merged draft on a private copy, replace the real document as one Undo step,
/// check the result, and roll back if it is broken.
fn commit(app: &mut WordApp, d: &Draft) -> Result<String, String> {
    // The original must not have changed while the preview was open.
    if d.stamp != (app.session.document_id(), app.session.rev()) {
        return Err("Your document changed after this draft was made. Nothing was applied. Discard the draft and ask Word again.".into());
    }
    let (acc, rej, pend) = d.counts();
    let merged = d.merged()?;

    // Inspect, render, repair, validate again: all on a private copy.
    let expected = ai_validate::text_fingerprint(&merged);
    let (checked, report) = ai_validate::validate(merged);
    if ai_validate::text_fingerprint(&checked) != expected {
        return Err("The automatic check would have changed your text, so nothing was applied. Your document is unchanged.".into());
    }
    if report.has_errors() {
        return Err(format!("The draft did not pass validation, so nothing was applied. Your document is unchanged.{}", report.error_lines()));
    }

    // Commit through the session as ONE Undo step.
    app.session.checkpoint("Word: apply draft");
    app.session.doc = checked;
    app.session.clamp_selection();
    app.session.touch();
    app.session.relayout();

    // Validate the saved document; if it is broken, roll back with Undo.
    let layout = app.session.layout();
    let broken = ai_validate::verify_committed(&app.session.doc, &layout);
    if !broken.is_empty() {
        app.session.undo();
        app.session.relayout();
        return Err(format!("The applied document failed its final check, so it was rolled back. Your document is as it was.{}", ai_validate::error_text(&broken)));
    }
    app.canvas.scroll_to_caret = true;

    let left = rej + pend;
    let mut m = format!("Applied {acc} of {} changes from “{}”.", d.changes.len(), d.title);
    if left > 0 {
        m.push_str(&format!("\n{left} were left out; those parts of your document stay as they were."));
    }
    m.push('\n');
    m.push_str(&report.summary());
    m.push_str("\nCtrl+Z undoes all of it.");
    Ok(m)
}

// ---------------------------------------------------------------------------------------------
// Showing it
// ---------------------------------------------------------------------------------------------

enum Act {
    Select(usize),
    One(usize, Verdict),
    Category(Category, Verdict),
    All(Verdict),
    Toggle(Category),
    Page(usize),
    Apply,
    Discard,
}

fn page_size(l: &DocLayout, page: usize) -> (f32, f32) {
    l.pages.get(page).map_or((0.0, 0.0), |p| (p.w, p.h))
}

fn render(doc: &Document, layout: &DocLayout, page: usize, w: f32, h: f32, scale: f32, ctx: &egui::Context, name: &str) -> Option<TextureHandle> {
    let pg = layout.pages.get(page)?;
    let img = wordcraft_render::render_area(doc, pg, 0.0, 0.0, w, h, scale, &Default::default());
    let size = [img.width as usize, img.height as usize];
    let px = img.to_straight();
    if size[0] == 0 || size[1] == 0 || px.len() != size[0] * size[1] * 4 {
        return None;
    }
    Some(ctx.load_texture(name, egui::ColorImage::from_rgba_unmultiplied(size, &px), egui::TextureOptions::LINEAR))
}

/// Top-level block index → (x0, y0, x1, y1) in page points, for the paragraphs on one page.
fn block_rects(layout: &DocLayout, page: usize) -> BTreeMap<usize, [f32; 4]> {
    let mut out: BTreeMap<usize, [f32; 4]> = BTreeMap::new();
    let Some(pg) = layout.pages.get(page) else { return out };
    for it in &pg.items {
        if let Placed::Lines { story: StoryRef::Body, path, para, l0, l1, y, .. } = it {
            let Some(&blk) = path.0.first() else { continue };
            let (Some(first), Some(last)) = (para.lines.get(*l0), para.lines.get(l1.saturating_sub(1))) else { continue };
            let top = *y;
            let bottom = *y + (last.top + last.height - first.top);
            let e = out.entry(blk as usize).or_insert([pg.body.x, top, pg.body.x + pg.body.w, bottom]);
            e[1] = e[1].min(top);
            e[3] = e[3].max(bottom);
        }
    }
    out
}

fn style_is(p: &Paragraph, id: &str) -> bool {
    p.props.style.as_deref().unwrap_or("Normal") == id
}

/// The blocks on this page (in this version) that a change is about.
fn anchors(c: &Change, side: Side, doc: &Document, rects: &BTreeMap<usize, [f32; 4]>) -> Vec<usize> {
    match &c.target {
        Target::Blocks { del, ins } => {
            let list = if side == Side::Original { del } else { ins };
            list.iter().copied().filter(|i| rects.contains_key(i)).collect()
        }
        Target::Paras(pairs) => pairs.iter().map(|&(o, d)| if side == Side::Original { o } else { d }).filter(|i| rects.contains_key(i)).collect(),
        Target::Style { id } => rects.keys().copied().filter(|&i| doc.body.get(i).and_then(|b| b.as_para()).is_some_and(|p| style_is(p, id))).collect(),
        _ => Vec::new(),
    }
}

fn verdict_color(c: &Change) -> Color32 {
    match c.verdict {
        Verdict::Pending => c.category.color(),
        Verdict::Accepted => Color32::from_rgb(34, 160, 80),
        Verdict::Rejected => Color32::from_gray(135),
    }
}

/// A padlock drawn in code.
fn lock_icon(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    let body = Rect::from_center_size(c + vec2(0.0, 3.0), vec2(11.0, 8.0));
    p.rect_filled(body, 1.5, color);
    let arc: Vec<egui::Pos2> = (0..=12)
        .map(|i| {
            let a = PI * (i as f32 / 12.0);
            c + vec2(-a.cos() * 3.6, -1.0 - a.sin() * 4.0)
        })
        .collect();
    p.add(egui::Shape::line(arc, Stroke::new(1.6, color)));
}

fn page_picture(ui: &mut Ui, tex: Option<&TextureHandle>, width: f32, canvas: (f32, f32)) -> Rect {
    let h = if canvas.0 > 0.0 { width * canvas.1 / canvas.0 } else { width * 1.3 };
    let (rect, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    match tex {
        Some(t) => {
            ui.painter().image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            ui.painter().rect_filled(rect, 2.0, Color32::from_gray(245));
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, "No page in this version", FontId::proportional(13.0), Color32::from_gray(120));
        }
    }
    ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::from_gray(120)), StrokeKind::Outside);
    rect
}

impl Draft {
    fn ensure_textures(&mut self, ctx: &egui::Context) {
        let total = self.total_pages();
        if self.page >= total {
            self.page = total - 1;
        }
        if self.tex.as_ref().is_some_and(|t| t.page == self.page) {
            return;
        }
        let (lw, lh) = page_size(&self.origin_layout, self.page);
        let (rw, rh) = page_size(&self.draft_layout, self.page);
        let (w, h) = (lw.max(rw), lh.max(rh));
        let ok = w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 && w <= 20_000.0 && h <= 20_000.0;
        let scale = if ok { (900.0 / w.max(h)).min(1.0) } else { 1.0 };
        let (left, right) = if ok {
            (
                render(&self.origin, &self.origin_layout, self.page, w, h, scale, ctx, "ai_preview_original"),
                render(&self.draft, &self.draft_layout, self.page, w, h, scale, ctx, "ai_preview_draft"),
            )
        } else {
            (None, None)
        };
        self.tex = Some(Tex { page: self.page, left, right, canvas: if ok { (w, h) } else { (612.0, 792.0) } });
    }

    /// Frames and numbered badges over one page picture. A click on a frame selects the change.
    fn overlay(&self, ui: &mut Ui, rect: Rect, side: Side, acts: &mut Vec<Act>) {
        let (layout, doc) = match side {
            Side::Original => (&self.origin_layout, &self.origin),
            Side::Draft => (&self.draft_layout, &self.draft),
        };
        let canvas = self.tex.as_ref().map_or((612.0, 792.0), |t| t.canvas);
        let scale = rect.width() / canvas.0.max(1.0);
        let at = |x: f32, y: f32| pos2(rect.left() + x * scale, rect.top() + y * scale);
        let rects = block_rects(layout, self.page);
        let painter = ui.painter().clone();
        for (i, c) in self.changes.iter().enumerate() {
            if self.hidden[c.category.slot()] {
                continue;
            }
            let selected = self.selected == Some(i);
            let color = verdict_color(c);
            let mut boxes: Vec<(Rect, bool)> = Vec::new();
            for blk in anchors(c, side, doc, &rects) {
                let r = rects[&blk];
                boxes.push((Rect::from_min_max(at(r[0] - 3.0, r[1] - 2.0), at(r[2] + 3.0, r[3] + 2.0)), true));
            }
            if matches!(c.target, Target::Page)
                && let Some(pg) = layout.pages.get(self.page)
            {
                boxes.push((Rect::from_min_max(at(pg.body.x, pg.body.y), at(pg.body.x + pg.body.w, pg.body.y + pg.body.h)), false));
            }
            let removed_here = side == Side::Original && matches!(&c.target, Target::Blocks { ins, .. } if ins.is_empty());
            for (b, fill) in boxes {
                if fill {
                    painter.rect_filled(b, 3.0, color.gamma_multiply(if selected { 0.30 } else { 0.14 }));
                }
                let stroke = Stroke::new(if selected { 3.0 } else { 2.0 }, color);
                if c.category.dashed() || removed_here || !fill {
                    change_marks::dashed_rect(&painter, b, stroke);
                } else {
                    painter.rect_stroke(b, 3.0, stroke, StrokeKind::Outside);
                }
                change_marks::badge(&painter, b.left_top(), &(i + 1).to_string(), color);
                let resp = ui.interact(b, egui::Id::new(("ai-preview-box", i, b.min.y as i32, side == Side::Draft)), Sense::click());
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    acts.push(Act::Select(i));
                }
            }
        }
    }

    fn ui(&self, ui: &mut Ui, t: &Tokens, stale: bool) -> Vec<Act> {
        let mut acts: Vec<Act> = Vec::new();
        let total = self.total_pages();
        let (acc, rej, pend) = self.counts();

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&self.title).strong());
            ui.label(RichText::new("Your document is not changed until you press Apply.").small().color(t.text_dim));
        });
        if stale {
            ui.label(RichText::new("Your document changed after this draft was made. Applying is switched off: discard this draft and ask Word again.").color(t.red));
        }

        // Category chips (also show or hide that category's frames) and page buttons.
        ui.horizontal_wrapped(|ui| {
            for cat in Category::ALL {
                let n = self.changes.iter().filter(|c| c.category == cat).count();
                let off = self.hidden[cat.slot()];
                let col = if off || n == 0 { t.text_disabled } else { cat.color() };
                let fill = if off || n == 0 { Color32::TRANSPARENT } else { cat.color().gamma_multiply(0.15) };
                let chip = egui::Button::new(RichText::new(format!("{}  {n}", cat.label())).color(col)).stroke(Stroke::new(1.5, col)).fill(fill);
                if ui.add_enabled(n > 0, chip).on_hover_text("Show or hide the frames of this category").clicked() {
                    acts.push(Act::Toggle(cat));
                }
            }
            ui.separator();
            if ui.add_enabled(self.page > 0, egui::Button::new("Previous page")).clicked() {
                acts.push(Act::Page(self.page - 1));
            }
            ui.label(format!("Page {} / {total}", self.page + 1));
            if ui.add_enabled(self.page + 1 < total, egui::Button::new("Next page")).clicked() {
                acts.push(Act::Page(self.page + 1));
            }
        });
        ui.label(RichText::new("Solid frame: content or font changed. Dashed frame: spacing, layout or removed text. The number is the same in the list.").small().color(t.text_dim));
        ui.separator();

        let avail = ui.available_width();
        let pw = ((avail - 60.0) * 0.30).clamp(180.0, 420.0);
        let canvas = self.tex.as_ref().map_or((612.0, 792.0), |x| x.canvas);
        let page_h = pw * canvas.1 / canvas.0.max(1.0);

        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(pw);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    lock_icon(ui.painter(), r.center(), t.text_dim);
                    ui.label(RichText::new("Original").strong());
                    ui.label(RichText::new("locked").small().color(t.text_dim));
                });
                let rect = page_picture(ui, self.tex.as_ref().and_then(|x| x.left.as_ref()), pw, canvas);
                self.overlay(ui, rect, Side::Original, &mut acts);
            });
            ui.vertical(|ui| {
                ui.add_space(page_h * 0.45);
                ui.label(RichText::new("draft  →").italics().color(t.text_dim));
            });
            ui.vertical(|ui| {
                ui.set_width(pw);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Proposed draft").strong());
                    ui.label(RichText::new(format!("{} changes", self.changes.len())).small().color(t.text_dim));
                });
                let rect = page_picture(ui, self.tex.as_ref().and_then(|x| x.right.as_ref()), pw, canvas);
                self.overlay(ui, rect, Side::Draft, &mut acts);
                ui.horizontal(|ui| {
                    let sel = self.selected;
                    let can = sel.is_some();
                    if ui.add_enabled(can, egui::Button::new(RichText::new("Accept").color(t.green))).clicked()
                        && let Some(i) = sel
                    {
                        acts.push(Act::One(i, Verdict::Accepted));
                    }
                    if ui.add_enabled(can, egui::Button::new(RichText::new("Reject").color(t.red))).clicked()
                        && let Some(i) = sel
                    {
                        acts.push(Act::One(i, Verdict::Rejected));
                    }
                    if let Some(i) = sel {
                        ui.label(RichText::new(format!("#{}", i + 1)).small().color(t.text_dim));
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_min_width(300.0);
                egui::ScrollArea::vertical().id_salt("ai-preview-list").max_height(page_h + 40.0).auto_shrink([false, false]).show(ui, |ui| {
                    for cat in Category::ALL {
                        let idxs: Vec<usize> = (0..self.changes.len()).filter(|&i| self.changes[i].category == cat).collect();
                        if idxs.is_empty() {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(cat.label()).strong().color(cat.color()));
                            ui.label(RichText::new(idxs.len().to_string()).small().color(t.text_dim));
                            if ui.small_button("Accept all").clicked() {
                                acts.push(Act::Category(cat, Verdict::Accepted));
                            }
                            if ui.small_button("Reject all").clicked() {
                                acts.push(Act::Category(cat, Verdict::Rejected));
                            }
                        });
                        for i in idxs {
                            let c = &self.changes[i];
                            let selected = self.selected == Some(i);
                            let fill = if selected { cat.color().gamma_multiply(0.14) } else { Color32::TRANSPARENT };
                            let row = egui::Frame::new().fill(fill).inner_margin(4.0).corner_radius(4.0).show(ui, |ui| {
                                ui.horizontal_top(|ui| {
                                    let (br, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                                    change_marks::badge(ui.painter(), br.center(), &(i + 1).to_string(), verdict_color(c));
                                    ui.vertical(|ui| {
                                        let title = ui.add(egui::Label::new(RichText::new(&c.title).strong().small()).sense(Sense::click()));
                                        if title.clicked() {
                                            acts.push(Act::Select(i));
                                        }
                                        if !c.detail.is_empty() {
                                            ui.label(RichText::new(&c.detail).small().color(t.text_dim));
                                        }
                                        ui.horizontal(|ui| {
                                            let a = c.verdict == Verdict::Accepted;
                                            let r = c.verdict == Verdict::Rejected;
                                            if ui.selectable_label(a, "Accept").clicked() {
                                                acts.push(Act::One(i, if a { Verdict::Pending } else { Verdict::Accepted }));
                                            }
                                            if ui.selectable_label(r, "Reject").clicked() {
                                                acts.push(Act::One(i, if r { Verdict::Pending } else { Verdict::Rejected }));
                                            }
                                        });
                                    });
                                });
                            });
                            if selected && self.jump.take() {
                                row.response.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                        ui.add_space(6.0);
                    }
                });
            });
        });

        ui.separator();
        ui.label(format!("{acc} accepted · {rej} rejected · {pend} undecided (left out when you apply)"));
        ui.horizontal_wrapped(|ui| {
            if ui.button("Accept all").clicked() {
                acts.push(Act::All(Verdict::Accepted));
            }
            if ui.button("Reject all").clicked() {
                acts.push(Act::All(Verdict::Rejected));
            }
            ui.separator();
            if ui.add_enabled(acc > 0 && !stale, egui::Button::new(RichText::new("Validate and apply accepted changes").strong())).clicked() {
                acts.push(Act::Apply);
            }
            if ui.add_enabled(!stale, egui::Button::new("Accept all, validate and apply")).clicked() {
                acts.push(Act::All(Verdict::Accepted));
                acts.push(Act::Apply);
            }
            if ui.button("Discard draft").clicked() {
                acts.push(Act::Discard);
            }
        });
        acts
    }
}

/// Draw the preview window (if a draft is open) and carry out what the user chose.
pub fn show(app: &mut WordApp, ctx: &egui::Context, st: &mut State, messages: &mut Vec<(bool, String)>) {
    let Some(mut draft) = st.draft.take() else { return };
    let t = Tokens::get(ctx);
    let stale = draft.stamp != (app.session.document_id(), app.session.rev());
    draft.ensure_textures(ctx);
    let mut open = true;
    let mut acts: Vec<Act> = Vec::new();
    egui::Window::new("Preview draft").id(egui::Id::new("word-ai-preview")).open(&mut open).collapsible(false).resizable(true).default_size(vec2(1100.0, 720.0)).show(ctx, |ui| {
        egui::ScrollArea::both().id_salt("ai-preview-outer").auto_shrink([false, false]).show(ui, |ui| {
            acts = draft.ui(ui, &t, stale);
        });
    });

    let mut keep = open;
    if !open {
        messages.push((false, "Draft discarded. Your document was not changed.".to_string()));
    }
    for a in acts {
        if !keep {
            break;
        }
        match a {
            Act::Select(i) => {
                draft.selected = Some(i);
                draft.jump.set(true);
                if let Some(p) = draft.page_of(i) {
                    draft.page = p;
                }
            }
            Act::One(i, v) => {
                if let Some(c) = draft.changes.get_mut(i) {
                    c.verdict = v;
                }
            }
            Act::Category(cat, v) => {
                for c in draft.changes.iter_mut().filter(|c| c.category == cat) {
                    c.verdict = v;
                }
            }
            Act::All(v) => {
                for c in &mut draft.changes {
                    c.verdict = v;
                }
            }
            Act::Toggle(cat) => draft.hidden[cat.slot()] = !draft.hidden[cat.slot()],
            Act::Page(p) => draft.page = p,
            Act::Apply => match commit(app, &draft) {
                Ok(msg) => {
                    messages.push((false, msg));
                    keep = false;
                }
                Err(e) => messages.push((false, format!("I couldn't apply the draft.\n{e}"))),
            },
            Act::Discard => {
                messages.push((false, "Draft discarded. Your document was not changed.".to_string()));
                keep = false;
            }
        }
        ctx.request_repaint();
    }
    if keep {
        st.draft = Some(draft);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(s: &str) -> Vec<u64> {
        s.chars().map(|c| c as u64).collect()
    }

    #[test]
    fn identical_lists_align_one_to_one() {
        let ops = align(&keys("abc"), &keys("abc"));
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().all(|o| matches!(o, Op::Same(i, j) if i == j)));
    }

    #[test]
    fn an_insert_in_the_middle_is_one_insert() {
        let ops = align(&keys("abd"), &keys("abcd"));
        let ins = ops.iter().filter(|o| matches!(o, Op::Ins(_))).count();
        let del = ops.iter().filter(|o| matches!(o, Op::Del(_))).count();
        assert_eq!((ins, del), (1, 0));
        assert!(ops.iter().any(|o| matches!(o, Op::Ins(2))));
    }

    #[test]
    fn a_rewrite_is_a_delete_and_an_insert() {
        let ops = align(&keys("abc"), &keys("axc"));
        let ins = ops.iter().filter(|o| matches!(o, Op::Ins(1))).count();
        let del = ops.iter().filter(|o| matches!(o, Op::Del(1))).count();
        assert_eq!((ins, del), (1, 1));
    }

    #[test]
    fn appended_blocks_are_inserts_at_the_end() {
        let ops = align(&keys("ab"), &keys("abcd"));
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::Ins(_))).count(), 2);
        assert!(matches!(ops[0], Op::Same(0, 0)) && matches!(ops[1], Op::Same(1, 1)));
    }
}