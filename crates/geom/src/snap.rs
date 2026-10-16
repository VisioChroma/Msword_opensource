//! Snapping, alignment and spacing maths (page points). Pure functions, no UI.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct R {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl R {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> R {
        R { l: x, t: y, r: x + w, b: y + h }
    }
    pub fn shifted(self, dx: f32, dy: f32) -> R {
        R { l: self.l + dx, t: self.t + dy, r: self.r + dx, b: self.b + dy }
    }
    pub fn union(self, o: R) -> R {
        R { l: self.l.min(o.l), t: self.t.min(o.t), r: self.r.max(o.r), b: self.b.max(o.b) }
    }
    pub fn span(self, a: Axis) -> (f32, f32) {
        match a {
            Axis::X => (self.l, self.r),
            Axis::Y => (self.t, self.b),
        }
    }
    pub fn cross(self, a: Axis) -> (f32, f32) {
        match a {
            Axis::X => (self.t, self.b),
            Axis::Y => (self.l, self.r),
        }
    }
    pub fn anchors(self, a: Axis) -> [f32; 3] {
        let (s0, s1) = self.span(a);
        [s0, (s0 + s1) / 2.0, s1]
    }
}

/// Axis::X: a vertical line at x = pos from y = from to y = to (Axis::Y: the mirror).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guide {
    pub axis: Axis,
    pub pos: f32,
    pub from: f32,
    pub to: f32,
}

/// A measured gap. Axis::X: a horizontal measure from x = start to x = end at y = cross.
/// `equal`: the gap on the other side is the same size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gap {
    pub axis: Axis,
    pub start: f32,
    pub end: f32,
    pub cross: f32,
    pub equal: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snap {
    pub dx: f32,
    pub dy: f32,
    pub guides: Vec<Guide>,
    pub gaps: Vec<Gap>,
}

fn best_delta(m: R, targets: &[f32], a: Axis, th: f32) -> Option<f32> {
    let mut best: Option<f32> = None;
    for src in m.anchors(a) {
        for &t in targets {
            let d = t - src;
            if d.abs() <= th && best.is_none_or(|b| d.abs() < b.abs()) {
                best = Some(d);
            }
        }
    }
    best
}

/// The move that makes the gap to the nearest neighbour before equal the gap to the nearest after.
fn equal_gap_delta(m: R, others: &[R], a: Axis) -> Option<f32> {
    let (m0, m1) = m.span(a);
    let (c0, c1) = m.cross(a);
    let (mut before, mut after): (Option<f32>, Option<f32>) = (None, None);
    for o in others {
        let (o0, o1) = o.span(a);
        let (k0, k1) = o.cross(a);
        if k1 <= c0 || k0 >= c1 {
            continue; // must overlap across the other axis
        }
        if o1 <= m0 {
            before = Some(before.map_or(o1, |b| b.max(o1)));
        } else if o0 >= m1 {
            after = Some(after.map_or(o0, |x| x.min(o0)));
        }
    }
    match (before, after) {
        (Some(b), Some(x)) => Some((b + x - (m1 - m0)) / 2.0 - m0),
        _ => None,
    }
}

/// The nearest neighbour's gap on each side of `s`, per axis (what Figma shows as "24").
pub fn nearest_gaps(s: R, others: &[R]) -> Vec<Gap> {
    let mut out = vec![];
    for axis in [Axis::X, Axis::Y] {
        let (s0, s1) = s.span(axis);
        let (c0, c1) = s.cross(axis);
        let (mut before, mut after): (Option<(f32, f32, f32)>, Option<(f32, f32, f32)>) = (None, None);
        for o in others {
            let (o0, o1) = o.span(axis);
            let (k0, k1) = o.cross(axis);
            let (lo, hi) = (c0.max(k0), c1.min(k1));
            if hi <= lo {
                continue;
            }
            let mid = (lo + hi) / 2.0;
            if o1 <= s0 {
                let g = s0 - o1;
                if before.is_none_or(|b| g < b.0) {
                    before = Some((g, mid, o1));
                }
            } else if o0 >= s1 {
                let g = o0 - s1;
                if after.is_none_or(|x| g < x.0) {
                    after = Some((g, mid, o0));
                }
            }
        }
        let equal = matches!((before, after), (Some(b), Some(x)) if (b.0 - x.0).abs() < 0.5);
        if let Some((_, mid, o1)) = before {
            out.push(Gap { axis, start: o1, end: s0, cross: mid, equal });
        }
        if let Some((_, mid, o0)) = after {
            out.push(Gap { axis, start: s1, end: o0, cross: mid, equal });
        }
    }
    out
}

fn guides_for(s: R, others: &[R], page: R, lines_x: &[f32], lines_y: &[f32]) -> Vec<Guide> {
    let mut v = vec![];
    for axis in [Axis::X, Axis::Y] {
        let (c0, c1) = s.cross(axis);
        let (p0, p1) = page.cross(axis);
        let lines = if axis == Axis::X { lines_x } else { lines_y };
        for a in s.anchors(axis) {
            for o in others {
                for t in o.anchors(axis) {
                    if (a - t).abs() < 0.01 {
                        let (k0, k1) = o.cross(axis);
                        v.push(Guide { axis, pos: a, from: c0.min(k0), to: c1.max(k1) });
                    }
                }
            }
            for &t in lines {
                if (a - t).abs() < 0.01 {
                    v.push(Guide { axis, pos: a, from: p0, to: p1 });
                }
            }
        }
    }
    v
}

/// Snap a dragged frame. `moving` is where the pointer puts it (page points); `others` are the
/// other objects on that page; `lines_x`/`lines_y` are page lines (margins, page centre);
/// `th` is the snap distance in page points.
pub fn snap_move(moving: R, others: &[R], page: R, lines_x: &[f32], lines_y: &[f32], th: f32) -> Snap {
    let mut tx: Vec<f32> = others.iter().flat_map(|o| o.anchors(Axis::X)).collect();
    tx.extend_from_slice(lines_x);
    let mut ty: Vec<f32> = others.iter().flat_map(|o| o.anchors(Axis::Y)).collect();
    ty.extend_from_slice(lines_y);

    let pick = |a: Option<f32>, b: Option<f32>| match (a, b) {
        (Some(x), Some(y)) => {
            if x.abs() <= y.abs() {
                x
            } else {
                y
            }
        }
        (Some(x), None) | (None, Some(x)) => x,
        _ => 0.0,
    };
    let dx = pick(
        best_delta(moving, &tx, Axis::X, th),
        equal_gap_delta(moving, others, Axis::X).filter(|d| d.abs() <= th),
    );
    let dy = pick(
        best_delta(moving, &ty, Axis::Y, th),
        equal_gap_delta(moving, others, Axis::Y).filter(|d| d.abs() <= th),
    );
    let s = moving.shifted(dx, dy);
    Snap { dx, dy, guides: guides_for(s, others, page, lines_x, lines_y), gaps: nearest_gaps(s, others) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    Top,
    Middle,
    Bottom,
}

/// How far each rect moves (dx, dy) to line up with `reference`. (Used by Part 2.)
pub fn align_deltas(rects: &[R], mode: Align, reference: R) -> Vec<(f32, f32)> {
    let cx = (reference.l + reference.r) / 2.0;
    let cy = (reference.t + reference.b) / 2.0;
    rects
        .iter()
        .map(|r| match mode {
            Align::Left => (reference.l - r.l, 0.0),
            Align::Center => (cx - (r.l + r.r) / 2.0, 0.0),
            Align::Right => (reference.r - r.r, 0.0),
            Align::Top => (0.0, reference.t - r.t),
            Align::Middle => (0.0, cy - (r.t + r.b) / 2.0),
            Align::Bottom => (0.0, reference.b - r.b),
        })
        .collect()
}

/// Offset along `axis` for each rect so the gaps between neighbours are equal; the first and
/// last (in order along the axis) stay put. Fewer than 3 rects: no change. (Used by Part 2.)
pub fn distribute_deltas(rects: &[R], axis: Axis) -> Vec<f32> {
    let n = rects.len();
    let mut out = vec![0.0; n];
    if n < 3 {
        return out;
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| rects[a].span(axis).0.total_cmp(&rects[b].span(axis).0));
    let first = rects[idx[0]].span(axis).0;
    let last = rects.iter().map(|r| r.span(axis).1).fold(f32::MIN, f32::max);
    let total: f32 = rects
        .iter()
        .map(|r| {
            let s = r.span(axis);
            s.1 - s.0
        })
        .sum();
    let gap = (last - first - total) / (n as f32 - 1.0);
    let mut cursor = first;
    for &i in &idx {
        let (s0, s1) = rects[i].span(axis);
        out[i] = cursor - s0;
        cursor += (s1 - s0) + gap;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_to_a_neighbours_top_edge() {
        let others = [R::new(100.0, 50.0, 40.0, 40.0)];
        let page = R::new(0.0, 0.0, 612.0, 792.0);
        let s = snap_move(R::new(200.0, 53.0, 40.0, 40.0), &others, page, &[], &[], 6.0);
        assert_eq!(s.dy, -3.0);
        assert!(s.guides.iter().any(|g| g.axis == Axis::Y && g.pos == 50.0));
    }

    #[test]
    fn distributes_evenly() {
        let r = [R::new(0.0, 0.0, 10.0, 10.0), R::new(20.0, 0.0, 10.0, 10.0), R::new(90.0, 0.0, 10.0, 10.0)];
        assert_eq!(distribute_deltas(&r, Axis::X), vec![0.0, 25.0, 0.0]);
    }
}