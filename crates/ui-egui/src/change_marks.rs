//! Clear, labelled change marks for comparing two rendered pages.
//!
//! Used by Visual review (non-AI) and by the Word AI draft preview. Instead of recolouring every
//! changed pixel red, changed areas are grouped into a few framed regions:
//!
//! * green, solid frame  = something was **added**
//! * red, dashed frame   = something was **removed**
//! * amber, solid frame  = something **changed** (text, font, size, spacing, position)
//!
//! Every frame has a numbered badge, so the meaning never depends on colour alone.

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

/// Size of one cell of the comparison grid, in pixels.
const CELL: usize = 6;
/// How far (in cells) changed cells are grown so words on a line and lines of a paragraph join.
const GROW_X: usize = 3;
const GROW_Y: usize = 2;
/// Never mark more regions than a person can read.
const MAX_MARKS: usize = 60;
/// A pixel darker than this (0-255) counts as ink.
const INK: u32 = 170;
/// Fewer ink pixels than this in a region counts as "empty".
const MIN_INK: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Added,
    Removed,
    Changed,
}

impl Kind {
    pub fn color(self) -> Color32 {
        match self {
            Kind::Added => Color32::from_rgb(34, 160, 80),
            Kind::Removed => Color32::from_rgb(214, 62, 62),
            Kind::Changed => Color32::from_rgb(237, 137, 20),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Added => "Added",
            Kind::Removed => "Removed",
            Kind::Changed => "Changed",
        }
    }
}

/// One framed region, as fractions (0..1) of the page picture.
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub kind: Kind,
}

/// Which of the two pictures is being drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The earlier version (baseline / original).
    Before,
    /// The later version (compared snapshot / proposed draft).
    After,
}

fn dilate(g: &[bool], cw: usize, ch: usize, rx: usize, ry: usize) -> Vec<bool> {
    let mut out = vec![false; g.len()];
    for y in 0..ch {
        for x in 0..cw {
            if !g[y * cw + x] {
                continue;
            }
            for yy in y.saturating_sub(ry)..=(y + ry).min(ch - 1) {
                for xx in x.saturating_sub(rx)..=(x + rx).min(cw - 1) {
                    out[yy * cw + xx] = true;
                }
            }
        }
    }
    out
}

fn ink(px: &[u8], w: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> usize {
    let mut n = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * w + x) * 4;
            let luma = (px[i] as u32 * 30 + px[i + 1] as u32 * 59 + px[i + 2] as u32 * 11) / 100;
            if luma < INK {
                n += 1;
            }
        }
    }
    n
}

/// Compare two straight-alpha RGBA pictures of the same size and return the framed regions in
/// reading order (top to bottom, then left to right).
pub fn find_marks(a: &[u8], b: &[u8], w: usize, h: usize) -> Vec<Mark> {
    if w == 0 || h == 0 || a.len() != w * h * 4 || b.len() != w * h * 4 {
        return Vec::new();
    }
    let cw = w.div_ceil(CELL);
    let ch = h.div_ceil(CELL);
    let mut grid = vec![false; cw * ch];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            if (0..3).any(|k| a[i + k].abs_diff(b[i + k]) > 24) {
                grid[(y / CELL) * cw + x / CELL] = true;
            }
        }
    }
    let grown = dilate(&grid, cw, ch, GROW_X, GROW_Y);
    let mut seen = vec![false; cw * ch];
    let mut marks = Vec::new();
    for start in 0..cw * ch {
        if !grown[start] || seen[start] {
            continue;
        }
        let (mut minx, mut maxx, mut miny, mut maxy) = (cw, 0usize, ch, 0usize);
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(c) = stack.pop() {
            let (cx, cy) = (c % cw, c / cw);
            if grid[c] {
                minx = minx.min(cx);
                maxx = maxx.max(cx);
                miny = miny.min(cy);
                maxy = maxy.max(cy);
            }
            if cx > 0 && grown[c - 1] && !seen[c - 1] {
                seen[c - 1] = true;
                stack.push(c - 1);
            }
            if cx + 1 < cw && grown[c + 1] && !seen[c + 1] {
                seen[c + 1] = true;
                stack.push(c + 1);
            }
            if cy > 0 && grown[c - cw] && !seen[c - cw] {
                seen[c - cw] = true;
                stack.push(c - cw);
            }
            if cy + 1 < ch && grown[c + cw] && !seen[c + cw] {
                seen[c + cw] = true;
                stack.push(c + cw);
            }
        }
        if minx > maxx || miny > maxy {
            continue;
        }
        let x0 = (minx * CELL).saturating_sub(2);
        let y0 = (miny * CELL).saturating_sub(2);
        let x1 = ((maxx + 1) * CELL + 2).min(w);
        let y1 = ((maxy + 1) * CELL + 2).min(h);
        let (ink_a, ink_b) = (ink(a, w, x0, y0, x1, y1), ink(b, w, x0, y0, x1, y1));
        let kind = if ink_a <= MIN_INK && ink_b > MIN_INK {
            Kind::Added
        } else if ink_b <= MIN_INK && ink_a > MIN_INK {
            Kind::Removed
        } else {
            Kind::Changed
        };
        marks.push(Mark { x0: x0 as f32 / w as f32, y0: y0 as f32 / h as f32, x1: x1 as f32 / w as f32, y1: y1 as f32 / h as f32, kind });
    }
    marks.sort_by(|p, q| p.y0.total_cmp(&q.y0).then(p.x0.total_cmp(&q.x0)));
    marks.truncate(MAX_MARKS);
    marks
}

/// A dashed line between two points.
fn dashed_line(p: &Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    let v = b - a;
    let len = v.length();
    if len < 1.0 {
        return;
    }
    let dir = v / len;
    let (dash, gap) = (6.0_f32, 4.0_f32);
    let mut t = 0.0_f32;
    while t < len {
        let e = (t + dash).min(len);
        p.line_segment([a + dir * t, a + dir * e], stroke);
        t += dash + gap;
    }
}

/// A dashed rectangle outline.
pub fn dashed_rect(p: &Painter, r: Rect, stroke: Stroke) {
    dashed_line(p, r.left_top(), r.right_top(), stroke);
    dashed_line(p, r.right_top(), r.right_bottom(), stroke);
    dashed_line(p, r.right_bottom(), r.left_bottom(), stroke);
    dashed_line(p, r.left_bottom(), r.left_top(), stroke);
}

/// A numbered round badge (white ring, coloured disc, white number).
pub fn badge(p: &Painter, center: Pos2, text: &str, color: Color32) {
    p.circle_filled(center, 10.5, Color32::WHITE);
    p.circle_filled(center, 9.0, color);
    p.text(center, Align2::CENTER_CENTER, text, FontId::proportional(11.0), Color32::WHITE);
}

/// Draw the marks over the picture shown in `image`. The Before picture shows removed and
/// changed regions; the After picture shows added and changed ones.
pub fn paint(painter: &Painter, image: Rect, marks: &[Mark], side: Side) {
    for (i, m) in marks.iter().enumerate() {
        let shown = !matches!((m.kind, side), (Kind::Added, Side::Before) | (Kind::Removed, Side::After));
        if !shown {
            continue;
        }
        let rect = Rect::from_min_max(
            pos2(image.left() + m.x0 * image.width(), image.top() + m.y0 * image.height()),
            pos2(image.left() + m.x1 * image.width(), image.top() + m.y1 * image.height()),
        );
        let color = m.kind.color();
        painter.rect_filled(rect, 3.0, color.gamma_multiply(0.16));
        let stroke = Stroke::new(2.0, color);
        if m.kind == Kind::Removed {
            dashed_rect(painter, rect, stroke);
        } else {
            painter.rect_stroke(rect, 3.0, stroke, StrokeKind::Outside);
        }
        badge(painter, rect.left_top() + vec2(0.0, 0.0), &(i + 1).to_string(), color);
    }
}

/// "■ 2 Added   ■ 1 Removed   ■ 3 Changed", plus how to read the frames.
pub fn legend(ui: &mut Ui, marks: &[Mark]) {
    ui.horizontal_wrapped(|ui| {
        for kind in [Kind::Added, Kind::Removed, Kind::Changed] {
            let n = marks.iter().filter(|m| m.kind == kind).count();
            let (r, _) = ui.allocate_exact_size(vec2(11.0, 11.0), Sense::hover());
            ui.painter().rect_filled(r, 2.0, kind.color());
            ui.label(format!("{n} {}", kind.label()));
            ui.add_space(8.0);
        }
        ui.label(egui::RichText::new("Numbered frames match on both pages. Dashed = removed.").small().weak());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_pictures_have_no_marks() {
        let (w, h) = (60, 60);
        let a = vec![255u8; w * h * 4];
        assert!(find_marks(&a, &a, w, h).is_empty());
    }

    #[test]
    fn a_new_block_of_ink_is_one_added_mark_and_its_reverse_is_removed() {
        let (w, h) = (60, 60);
        let blank = vec![255u8; w * h * 4];
        let mut inked = blank.clone();
        for y in 20..30 {
            for x in 10..40 {
                let i = (y * w + x) * 4;
                inked[i] = 0;
                inked[i + 1] = 0;
                inked[i + 2] = 0;
            }
        }
        let added = find_marks(&blank, &inked, w, h);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].kind, Kind::Added);
        let removed = find_marks(&inked, &blank, w, h);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].kind, Kind::Removed);
    }

    #[test]
    fn ink_in_both_pictures_is_a_change() {
        let (w, h) = (60, 60);
        let mut a = vec![255u8; w * h * 4];
        let mut b = a.clone();
        for y in 20..30 {
            for x in 10..40 {
                let i = (y * w + x) * 4;
                a[i] = 0;
                a[i + 1] = 0;
                a[i + 2] = 0;
            }
            for x in 10..25 {
                let i = (y * w + x) * 4;
                b[i] = 0;
                b[i + 1] = 0;
                b[i + 2] = 0;
            }
        }
        let marks = find_marks(&a, &b, w, h);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].kind, Kind::Changed);
    }
}
