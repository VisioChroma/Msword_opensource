//! "Word", the AI assistant: a round button at the bottom-right of the window that opens a
//! small chat panel. Describe a document by typing or speaking; optionally research it first.

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{Align2, Color32, Pos2, Sense, vec2};

use crate::theme::Tokens;
use crate::{WordApp, ai_context, ai_format, ai_guidelines, ai_integrity, ai_net, ai_plan, ai_preview, ai_research, ai_voice};

/// A finished request: the plan to build and what to tell the user.
struct Done {
    plan: ai_plan::Plan,
    label: String,
    summary: String,
}

#[derive(Default)]
pub struct State {
    open: bool,
    input: String,
    /// (from the user?, text)
    messages: Vec<(bool, String)>,
    /// `None` = Auto (fallback through every model); `Some(i)` = pinned to `ai_net::CANDIDATES[i]`.
    model: Option<usize>,
    /// Search the web and scholarly sources first, and cite them.
    research: bool,
    /// Put spoken text in the box instead of sending it straight away.
    review_voice: bool,
    busy: Option<Receiver<Result<Done, String>>>,
    listening: Option<ai_voice::Listening>,
    /// "Format document" was pressed; `show` starts it (it needs the document).
    format_request: Option<String>,
    format_busy: Option<Receiver<Result<ai_format::Proposal, String>>>,
    /// The options on offer.
    proposal: Option<ai_format::Proposal>,
    /// The option the user picked; `show` applies it.
    pick: Option<usize>,
    /// "Guidelines and compliance": rules, findings and the file picker.
    guide: ai_guidelines::State,
    /// "Research context": persistent memory of the document, and the continuity checks.
    context: ai_context::State,
    /// "Citations and integrity": citation checks, reference verification, similarity.
    integrity: ai_integrity::State,
    /// "Preview before committing": the isolated draft and its side-by-side review.
    preview: ai_preview::State,
}

pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    let t = Tokens::get(ctx);
    let mut st = std::mem::take(&mut app.ai);
    poll(app, ctx, &mut st);
    ai_guidelines::drive(app, ctx, &mut st.guide, st.model, &mut st.messages);
    ai_context::drive(app, ctx, &mut st.context, st.model, &mut st.messages);
    ai_integrity::drive(app, ctx, &mut st.integrity, st.model, &mut st.messages);
    launcher(ctx, &t, &mut st);
    if st.open {
        panel(ctx, &t, &mut st);
    }
    if let Some(request) = st.format_request.take() {
        start_format(ctx, &mut st, &app.session, request);
    }
    if let Some(i) = st.pick.take() {
        apply_choice(app, &mut st, i);
    }
    ai_preview::show(app, ctx, &mut st.preview, &mut st.messages);
    app.ai = st;
}

// ---- Animated orb: a port of the React "bloop" look, drawn on the CPU into a small texture ----
// Palette from the React `BloopPaletteName.blue` defaults (0..1 RGB).
const ORB_MAIN: [f32; 3] = [0.1, 0.5, 1.0];
const ORB_MID: [f32; 3] = [0.2, 0.4, 0.9];
const ORB_HIGH: [f32; 3] = [0.5, 0.8, 1.0];
/// Texture side in pixels (the orb is ~50 points wide on screen).
const ORB_PX: usize = 96;
/// The orb redraws at most this often.
const ORB_FRAME: f64 = 1.0 / 24.0;

/// The orb's texture and animation clock, kept in egui's own memory so no app state changes.
#[derive(Clone)]
struct OrbTex {
    tex: egui::TextureHandle,
    at: f64,
    phase: f32,
}

fn orb_mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn orb_step(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn orb_hash(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(374_761_393).wrapping_add((y as u32).wrapping_mul(668_265_263));
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h & 0xffff) as f32 / 65535.0
}

fn orb_noise(x: f32, y: f32) -> f32 {
    let (xf, yf) = (x.floor(), y.floor());
    let (fx, fy) = (x - xf, y - yf);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (xi, yi) = (xf as i32, yf as i32);
    let a = orb_hash(xi, yi);
    let b = orb_hash(xi + 1, yi);
    let c = orb_hash(xi, yi + 1);
    let d = orb_hash(xi + 1, yi + 1);
    let top = a + (b - a) * ux;
    let bottom = c + (d - c) * ux;
    top + (bottom - top) * uy
}

/// Soft cloudy noise, roughly 0..1.
fn orb_fbm(x: f32, y: f32) -> f32 {
    let (mut x, mut y) = (x, y);
    let (mut sum, mut amp) = (0.0, 0.5);
    for _ in 0..3 {
        sum += amp * orb_noise(x, y);
        x = x * 2.03 + 17.1;
        y = y * 2.03 + 9.7;
        amp *= 0.5;
    }
    sum / 0.875
}

/// One pixel of the orb. `x`, `y` are in -1..1 across the disc; returns straight RGBA (0..1).
fn orb_pixel(x: f32, y: f32, phase: f32, energy: f32) -> [f32; 4] {
    let r2 = x * x + y * y;
    if r2 >= 1.0 {
        return [0.0; 4];
    }
    let alpha = ((1.0 - r2.sqrt()) * (ORB_PX as f32 * 0.5)).clamp(0.0, 1.0);
    let z = (1.0 - r2).sqrt();
    let t = phase * 0.4;

    // Domain-warped clouds, so the colours drift and fold like watercolour.
    let wx = orb_fbm(x * 1.4 + t, y * 1.4 - t * 0.7);
    let wy = orb_fbm(x * 1.4 - t * 0.8 + 5.2, y * 1.4 + t + 1.3);
    let qx = x + (wx - 0.5) * 0.9;
    let qy = y + (wy - 0.5) * 0.9;
    let n = orb_fbm(qx * 1.3 + 3.0, qy * 1.3 - t * 0.5);
    let b = orb_fbm(qx * 1.1 - 7.0 + t * 0.3, qy * 1.1 + 2.0);

    // Deep blue body with a few slightly duller streaks, gently shaded like a sphere.
    let mut col = orb_mix(ORB_MAIN, ORB_MID, orb_step(0.4, 0.8, b) * 0.3);
    let shade = 0.92 + 0.08 * z;
    col = [col[0] * shade, col[1] * shade, col[2] * shade];

    // Lighter patch at the lower left.
    let dx = x + 0.5 + 0.06 * (t * 3.0).sin();
    let dy = y - 0.45;
    let d = (dx * dx + dy * dy).sqrt();
    let patch = orb_step(0.65, 0.1, d) * 0.45 * (0.5 + 0.5 * b);
    col = orb_mix(col, orb_mix(ORB_MAIN, ORB_HIGH, 0.45), patch);

    // Cream cloud across the top, paler cyan toward the upper left.
    let h = -qy;
    let cloud = orb_step(0.35, 0.95, h * 0.9 + (n - 0.5) * 0.9 + energy * 0.3);
    let cyan = orb_step(0.1, -0.7, x + (n - 0.5) * 0.6);
    let cream = orb_mix([1.0, 0.98, 0.92], [0.78, 0.94, 1.0], cyan * 0.8);
    col = orb_mix(col, cream, cloud * 0.95);

    [col[0], col[1], col[2], alpha]
}

fn orb_render(phase: f32, energy: f32) -> egui::ColorImage {
    let n = ORB_PX;
    let mut rgba = vec![0u8; n * n * 4];
    for j in 0..n {
        for i in 0..n {
            let x = (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
            let y = (j as f32 + 0.5) / n as f32 * 2.0 - 1.0;
            let [r, g, b, a] = orb_pixel(x, y, phase, energy);
            let o = (j * n + i) * 4;
            rgba[o] = (r * a * 255.0) as u8;
            rgba[o + 1] = (g * a * 255.0) as u8;
            rgba[o + 2] = (b * a * 255.0) as u8;
            rgba[o + 3] = (a * 255.0) as u8;
        }
    }
    egui::ColorImage::from_rgba_premultiplied([n, n], &rgba)
}

fn launcher(ctx: &egui::Context, _t: &Tokens, st: &mut State) {
    // Look only: which animation to play. Nothing here changes how the assistant works.
    let busy = st.busy.is_some() || st.format_busy.is_some();
    let listening = st.listening.is_some();
    let now = ctx.input(|i| i.time);
    let time = now as f32;
    let ambient = (time * 2.0).sin() * 0.1 + 0.1;
    let (speed, energy, pulse) = if listening {
        (1.8, ambient * 2.0, ambient * 0.3)
    } else if busy {
        (3.5, 0.15 + 0.1 * (time * 3.0).sin(), 0.02 * (time * 6.0).sin())
    } else {
        (1.0, 0.0, 0.012 * (time * 1.2).sin())
    };
    ctx.request_repaint_after(Duration::from_millis(41));

    // Redraw the texture when it is due (and create it the first time).
    let id = egui::Id::new("word-ai-orb-texture");
    let mut orb: Option<OrbTex> = ctx.data(|d| d.get_temp(id));
    if orb.as_ref().is_none_or(|o| now - o.at >= ORB_FRAME) {
        let (dt, phase0) = orb.as_ref().map_or((0.0, 0.0), |o| (((now - o.at) as f32).min(0.1), o.phase));
        let phase = phase0 + dt * speed;
        let img = orb_render(phase, energy);
        let next = match orb.take() {
            Some(mut o) => {
                o.tex.set(img, egui::TextureOptions::LINEAR);
                o.at = now;
                o.phase = phase;
                o
            }
            None => OrbTex { tex: ctx.load_texture("word-ai-orb", img, egui::TextureOptions::LINEAR), at: now, phase },
        };
        ctx.data_mut(|d| d.insert_temp(id, next.clone()));
        orb = Some(next);
    }

    egui::Area::new(egui::Id::new("word-ai-launcher")).order(egui::Order::Foreground).anchor(Align2::RIGHT_BOTTOM, vec2(-22.0, -46.0)).show(
        ctx,
        |ui| {
            let (rect, resp) = ui.allocate_exact_size(vec2(50.0, 50.0), Sense::click());
            let hover = if resp.hovered() { 1.04 } else { 1.0 };
            let r = 22.5 * (1.0 + pulse) * hover;
            let c = rect.center();
            let p = ui.painter();
            p.circle_filled(c + vec2(0.0, 2.0), r, Color32::from_black_alpha(45));
            if let Some(o) = &orb {
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                p.image(o.tex.id(), egui::Rect::from_center_size(c, vec2(2.0 * r, 2.0 * r)), uv, Color32::WHITE);
            }
            if resp.on_hover_text("Word: your AI writing assistant").clicked() {
                st.open = !st.open;
            }
        },
    );
}

/// A microphone button drawn in code; red while recording.
// ---- Chat box: a rounded input card with a light travelling round its border ----

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Plus,
    Search,
    Grid,
    Up,
    Mic,
    Chevron,
    Close,
}

fn draw_glyph(p: &egui::Painter, g: Glyph, c: Pos2, ink: Color32) {
    let s = egui::Stroke::new(1.6, ink);
    match g {
        Glyph::Plus => {
            p.line_segment([c + vec2(-6.0, 0.0), c + vec2(6.0, 0.0)], s);
            p.line_segment([c + vec2(0.0, -6.0), c + vec2(0.0, 6.0)], s);
        }
        Glyph::Search => {
            p.circle_stroke(c + vec2(-1.5, -1.5), 5.2, s);
            p.line_segment([c + vec2(2.4, 2.4), c + vec2(6.5, 6.5)], s);
        }
        Glyph::Grid => {
            for (dx, dy) in [(-6.5, -6.5), (0.5, -6.5), (-6.5, 0.5), (0.5, 0.5)] {
                p.rect_stroke(egui::Rect::from_min_size(c + vec2(dx, dy), vec2(6.0, 6.0)), 1.5, s, egui::StrokeKind::Inside);
            }
        }
        Glyph::Up => {
            p.line_segment([c + vec2(0.0, 6.0), c + vec2(0.0, -6.0)], s);
            p.line_segment([c + vec2(-5.0, -1.0), c + vec2(0.0, -6.0)], s);
            p.line_segment([c + vec2(5.0, -1.0), c + vec2(0.0, -6.0)], s);
        }
        Glyph::Mic => {
            p.rect_filled(egui::Rect::from_center_size(c + vec2(0.0, -3.5), vec2(6.0, 10.0)), 3.0, ink);
            let arc: Vec<Pos2> = (0..=12)
                .map(|i| {
                    let a = std::f32::consts::PI * (i as f32 / 12.0);
                    c + vec2(a.cos() * 6.5, 0.5 + a.sin() * 5.5)
                })
                .collect();
            p.add(egui::Shape::line(arc, s));
            p.line_segment([c + vec2(0.0, 6.0), c + vec2(0.0, 9.0)], s);
            p.line_segment([c + vec2(-3.5, 9.0), c + vec2(3.5, 9.0)], s);
        }
        Glyph::Chevron => {
            p.line_segment([c + vec2(-4.0, -2.0), c + vec2(0.0, 2.0)], s);
            p.line_segment([c + vec2(4.0, -2.0), c + vec2(0.0, 2.0)], s);
        }
        Glyph::Close => {
            p.line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], s);
            p.line_segment([c + vec2(5.0, -5.0), c + vec2(-5.0, 5.0)], s);
        }
    }
}

/// A round icon button. `solid` = (fill, icon colour) overrides the look (send, recording).
fn icon_button(
    ui: &mut egui::Ui,
    t: &Tokens,
    g: Glyph,
    label: &str,
    active: bool,
    enabled: bool,
    solid: Option<(Color32, Color32)>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), if enabled { Sense::click() } else { Sense::hover() });
    let (bg, ink) = match solid {
        Some((fill, ink)) => (fill, ink),
        None => {
            let bg = if active {
                t.accent.gamma_multiply(0.22)
            } else if enabled && resp.hovered() {
                t.hover
            } else {
                Color32::TRANSPARENT
            };
            let ink = if !enabled {
                t.text_disabled
            } else if active {
                t.accent
            } else {
                t.icon
            };
            (bg, ink)
        }
    };
    ui.painter().circle_filled(rect.center(), 15.0, bg);
    draw_glyph(ui.painter(), g, rect.center(), ink);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

fn send_button(ui: &mut egui::Ui, t: &Tokens, enabled: bool) -> egui::Response {
    let look = if enabled { (t.accent, t.on_accent) } else { (t.hover, t.text_disabled) };
    icon_button(ui, t, Glyph::Up, "Create document", false, enabled, Some(look))
}

/// A pill with an optional icon, a label and an optional dropdown arrow. Lit when `active`.
fn chip(ui: &mut egui::Ui, t: &Tokens, g: Option<Glyph>, text: &str, arrow: bool, active: bool) -> egui::Response {
    let ink = if active { t.accent } else { t.text };
    let galley = ui.painter().layout_no_wrap(text.to_string(), egui::FontId::proportional(13.0), ink);
    let mut w = 20.0 + galley.size().x;
    if g.is_some() {
        w += 18.0;
    }
    if arrow {
        w += 14.0;
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    let bg = if active {
        t.accent.gamma_multiply(0.22)
    } else if resp.hovered() {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    let p = ui.painter();
    p.rect_filled(rect, 14.0, bg);
    p.rect_stroke(rect, 14.0, egui::Stroke::new(1.0, if active { t.accent } else { t.border }), egui::StrokeKind::Inside);
    let cy = rect.center().y;
    let mut x = rect.min.x + 10.0;
    if let Some(g) = g {
        draw_glyph(p, g, egui::pos2(x + 7.0, cy), ink);
        x += 18.0;
    }
    p.galley(egui::pos2(x, cy - galley.size().y / 2.0), galley.clone(), ink);
    x += galley.size().x;
    if arrow {
        draw_glyph(p, Glyph::Chevron, egui::pos2(x + 9.0, cy + 1.0), ink);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A point on the outline of a rounded rectangle; `f` in 0..1 goes once round, clockwise.
fn perimeter_point(rect: egui::Rect, r: f32, f: f32) -> Pos2 {
    use egui::pos2;
    use std::f32::consts::{FRAC_PI_2, PI};
    let w = (rect.width() - 2.0 * r).max(0.0);
    let h = (rect.height() - 2.0 * r).max(0.0);
    let arc = FRAC_PI_2 * r;
    let total = 2.0 * w + 2.0 * h + 4.0 * arc;
    let mut s = f.rem_euclid(1.0) * total;
    let (l, tp, rt, bt) = (rect.min.x, rect.min.y, rect.max.x, rect.max.y);
    if s <= w {
        return pos2(l + r + s, tp);
    }
    s -= w;
    if s <= arc {
        let a = -FRAC_PI_2 + s / r;
        return pos2(rt - r + a.cos() * r, tp + r + a.sin() * r);
    }
    s -= arc;
    if s <= h {
        return pos2(rt, tp + r + s);
    }
    s -= h;
    if s <= arc {
        let a = s / r;
        return pos2(rt - r + a.cos() * r, bt - r + a.sin() * r);
    }
    s -= arc;
    if s <= w {
        return pos2(rt - r - s, bt);
    }
    s -= w;
    if s <= arc {
        let a = FRAC_PI_2 + s / r;
        return pos2(l + r + a.cos() * r, bt - r + a.sin() * r);
    }
    s -= arc;
    if s <= h {
        return pos2(l, bt - r - s);
    }
    s -= h;
    let a = PI + s / r;
    pos2(l + r + a.cos() * r, tp + r + a.sin() * r)
}

/// The card's background, its border, and a soft light that circles the border every 6 seconds.
fn paint_card(ui: &egui::Ui, t: &Tokens, rect: egui::Rect, time: f32) {
    const R: f32 = 18.0;
    const N: usize = 160;
    let p = ui.painter();
    p.rect_filled(rect, R, t.input);
    p.rect_stroke(rect, R, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    let head = (time / 6.0).fract();
    let edge = rect.shrink(1.0);
    let mut prev = perimeter_point(edge, R - 1.0, 0.0);
    for i in 1..=N {
        let f = i as f32 / N as f32;
        let pt = perimeter_point(edge, R - 1.0, f);
        let mut d = (f - 0.5 / N as f32 - head).abs();
        if d > 0.5 {
            d = 1.0 - d;
        }
        let k = (1.0 - d / 0.16).clamp(0.0, 1.0);
        if k > 0.02 {
            let k = k * k;
            p.line_segment([prev, pt], egui::Stroke::new(3.5, t.accent.gamma_multiply(k * 0.25)));
            p.line_segment([prev, pt], egui::Stroke::new(1.6, t.accent.gamma_multiply(k)));
        }
        prev = pt;
    }
}
fn mix_col(a: Color32, b: Color32, k: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k) as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// A small version of the orb, used as Word's avatar.
fn paint_avatar(p: &egui::Painter, c: Pos2, r: f32) {
    p.circle_filled(c, r, Color32::from_rgb(26, 128, 255));
    p.circle_filled(c + vec2(-0.1 * r, -0.45 * r), r * 0.55, Color32::from_rgba_unmultiplied(255, 255, 255, 120));
}

/// One skeleton line: a pill with a light band sweeping across it.
fn skeleton_bar(p: &egui::Painter, t: &Tokens, rect: egui::Rect, time: f32, seed: f32) {
    let base = t.hover;
    let hi = if t.dark { mix_col(t.hover, Color32::WHITE, 0.2) } else { Color32::WHITE };
    let r = rect.height() / 2.0;
    let w = rect.width();
    let cols = ((w / 4.0).ceil() as usize).max(2);
    let cy = rect.center().y;
    let sweep = (time * 0.7 - seed).rem_euclid(1.0) * 1.5 - 0.25;
    let mut mesh = egui::Mesh::default();
    for i in 0..=cols {
        let f = i as f32 / cols as f32;
        let x = rect.min.x + f * w;
        let edge = (x - rect.min.x).min(rect.max.x - x).min(r);
        let hh = (r * r - (r - edge) * (r - edge)).max(0.0).sqrt();
        let k = (1.0 - (f - sweep).abs() / 0.22).clamp(0.0, 1.0);
        let k = k * k * (3.0 - 2.0 * k);
        let col = mix_col(base, hi, k);
        mesh.colored_vertex(egui::pos2(x, cy - hh), col);
        mesh.colored_vertex(egui::pos2(x, cy + hh), col);
    }
    for i in 0..cols as u32 {
        let b = i * 2;
        mesh.add_triangle(b, b + 1, b + 2);
        mesh.add_triangle(b + 1, b + 3, b + 2);
    }
    p.add(egui::Shape::mesh(mesh));
}

/// "Word is working": the avatar, a short status, and three shimmering lines.
fn skeleton(ui: &mut egui::Ui, t: &Tokens, time: f32, status: &str) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        paint_avatar(ui.painter(), r.center(), 9.0);
        ui.label(egui::RichText::new(status).small().color(t.text_dim));
    });
    ui.add_space(6.0);
    let w = ui.available_width();
    for (i, frac) in [1.0_f32, 0.86, 0.58].into_iter().enumerate() {
        let (rect, _) = ui.allocate_exact_size(vec2(w * frac, 10.0), Sense::hover());
        skeleton_bar(ui.painter(), t, rect, time, i as f32 * 0.12);
        ui.add_space(4.0);
    }
}

/// Four bouncing bars, shown while the microphone is listening.
fn wave(ui: &mut egui::Ui, t: &Tokens, time: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 16.0), Sense::hover());
    for i in 0..4 {
        let h = 4.0 + 10.0 * (0.5 + 0.5 * (time * 7.0 + i as f32 * 1.3).sin());
        let x = rect.min.x + 2.0 + i as f32 * 5.5;
        ui.painter().rect_filled(egui::Rect::from_center_size(egui::pos2(x, rect.center().y), vec2(3.0, h)), 1.5, t.accent);
    }
}

fn panel(ctx: &egui::Context, t: &Tokens, st: &mut State) {
    let mut close = false;
    let mut send: Option<String> = None;
    let mut mic = false;
    let mut format = false;
    let mut pick: Option<usize> = None;
    let mut dismiss = false;
    // Whether the Tools dropdown was open last frame (only used to light its button).
    let tools_id = egui::Id::new("word-ai-tools-open");
    let mut tools_open: bool = ctx.data(|d| d.get_temp(tools_id)).unwrap_or(false);
    let time = ctx.input(|i| i.time) as f32;
    // A fixed-size box that always fits the screen. Everything inside scrolls instead of growing.
    let height = (ctx.content_rect().height() - 190.0).clamp(300.0, 470.0);
    let frame = egui::Frame::new()
        .fill(t.panel)
        .stroke(egui::Stroke::new(1.0, t.border))
        .corner_radius(22.0)
        .inner_margin(14.0)
        .shadow(egui::Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(70) });
    egui::Window::new("Word")
        .id(egui::Id::new("word-ai-panel"))
        .title_bar(false)
        .frame(frame)
        .collapsible(false)
        .fixed_size(vec2(376.0, height))
        .anchor(Align2::RIGHT_BOTTOM, vec2(-22.0, -108.0))
        .show(ctx, |ui| {
            let listening = st.listening.is_some();
            let stopping = st.listening.as_ref().is_some_and(|l| l.stopping);
            let can_talk = !stopping && (listening || st.busy.is_none());
            let can = st.busy.is_none() && !listening && !st.input.trim().is_empty();
            let can_format = st.format_busy.is_none() && !listening;

            // ---- Header ----
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                paint_avatar(ui.painter(), r.center(), 14.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(egui::RichText::new("Word").strong().size(15.0));
                    ui.label(egui::RichText::new("AI writing assistant").small().color(t.text_dim));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, t, Glyph::Close, "Close", false, true, None).on_hover_text("Close").clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(8.0);

            const CARD_H: f32 = 104.0;
            let avail = ui.available_size();
            let body_h = (avail.y - CARD_H - 8.0).max(80.0);

            // ---- Chat. The only part that scrolls. ----
            ui.allocate_ui(vec2(avail.x, body_h), |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("word-ai-chat-scroll")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if !ai_net::any_key() {
                            ui.label(egui::RichText::new("No API key found. Set GEMINI_API_KEY or GROQ_API_KEY, then restart WordCraft.").color(t.red).small());
                        }
                        let idle = st.busy.is_none() && st.format_busy.is_none() && st.listening.is_none();
                        if st.messages.is_empty() && idle {
                            ui.vertical_centered(|ui| {
                                ui.add_space(18.0);
                                let (r, _) = ui.allocate_exact_size(vec2(46.0, 46.0), Sense::hover());
                                paint_avatar(ui.painter(), r.center(), 23.0);
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new("How can I help?").size(17.0).strong());
                                ui.label(egui::RichText::new("Describe a document, or use + to format the one you have.").small().color(t.text_dim));
                                ui.add_space(10.0);
                                for (label, prompt) in [
                                    ("Professional document on AI agents", "Create a professional document on AI agents for tomorrow's meeting."),
                                    ("Meeting brief for tomorrow", "Create a one-page meeting brief for tomorrow's meeting."),
                                    ("Résumé for a software engineer", "Create a one-page résumé for a software engineer."),
                                ] {
                                    if chip(ui, t, None, label, false, false).clicked() {
                                        st.input = prompt.to_string();
                                    }
                                    ui.add_space(2.0);
                                }
                            });
                        }
                        for (mine, text) in &st.messages {
                            ui.add_space(12.0);
                            if *mine {
                                let maxw = ui.available_width() * 0.82;
                                ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                                    egui::Frame::new()
                                        .fill(t.accent)
                                        .corner_radius(egui::CornerRadius { nw: 16, ne: 16, sw: 16, se: 4 })
                                        .inner_margin(egui::Margin::symmetric(12, 8))
                                        .show(ui, |ui| {
                                            ui.set_max_width(maxw);
                                            ui.label(egui::RichText::new(text).color(t.on_accent));
                                        });
                                });
                            } else {
                                ui.horizontal(|ui| {
                                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                                    paint_avatar(ui.painter(), r.center(), 9.0);
                                    ui.label(egui::RichText::new("Word").small().strong().color(t.text_dim));
                                });
                                ui.add_space(4.0);
                                ui.label(text);
                            }
                        }
                        if let Some(l) = &st.listening {
                            if l.stopping {
                                skeleton(ui, t, time, "Transcribing…");
                            } else {
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    wave(ui, t, time);
                                    ui.label(egui::RichText::new("Listening… press the mic again when you're done").small().color(t.text_dim));
                                });
                            }
                        }
                        if st.busy.is_some() {
                            skeleton(ui, t, time, if st.research { "Researching and writing…" } else { "Writing…" });
                        }
                        if st.format_busy.is_some() {
                            skeleton(ui, t, time, "Choosing a layout…");
                        }
                        if let Some(p) = &st.proposal {
                            ui.add_space(12.0);
                            ui.label(egui::RichText::new("Pick a layout").small().strong());
                            for (i, c) in p.choices.iter().enumerate() {
                                ui.horizontal_wrapped(|ui| {
                                    if ui.button(format!("Option {} · {}", i + 1, c.category.label())).clicked() {
                                        pick = Some(i);
                                    }
                                    ui.label(egui::RichText::new(c.describe()).small().color(t.text_dim));
                                });
                            }
                            if ui.small_button("Dismiss").clicked() {
                                dismiss = true;
                            }
                        }
                    });
            });
            ui.add_space(8.0);

            // ---- Input card (fixed size, so nothing can overlap) ----
            let (card, _) = ui.allocate_exact_size(vec2(avail.x, CARD_H), Sense::hover());
            paint_card(ui, t, card, time);
            let inner = card.shrink2(vec2(14.0, 10.0));
            let text_rect = egui::Rect::from_min_size(inner.min, vec2(inner.width(), 46.0));
            let bar_rect = egui::Rect::from_min_max(egui::pos2(inner.min.x, inner.max.y - 30.0), inner.max);

            let r = ui
                .scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("word-ai-input-scroll")
                        .max_height(text_rect.height())
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut st.input)
                                    .frame(egui::Frame::NONE)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(2)
                                    .hint_text("What should I write?"),
                            )
                        })
                        .inner
                })
                .inner;
            let ctrl_enter = r.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            if can && ctrl_enter {
                send = Some(st.input.trim().to_string());
            }

            // Toolbar, left: + menu, Research, Tools dropdown.
            ui.scope_builder(egui::UiBuilder::new().max_rect(bar_rect).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let plus = icon_button(ui, t, Glyph::Plus, "More actions", false, true, None).on_hover_text("More: format document, clear chat, voice options");
                let _ = egui::Popup::menu(&plus).close_behavior(egui::PopupCloseBehavior::CloseOnClick).show(|ui| {
                    if ui
                        .add_enabled(can_format, egui::Button::new("Format document"))
                        .on_hover_text("Offers two or three layouts for the whole document. Type a style first (e.g. APA 7) if you have one. Nothing changes until you pick.")
                        .clicked()
                    {
                        format = true;
                    }
                    if ui.button("Clear chat").clicked() && st.busy.is_none() {
                        st.messages.clear();
                    }
                    ui.checkbox(&mut st.review_voice, "Review voice text before sending");
                });
                if chip(ui, t, Some(Glyph::Search), "Research", false, st.research)
                    .on_hover_text("Research with sources: searches the web and scholarly databases first, cites real sources by number and flags claims it can't support. Slower.")
                    .clicked()
                {
                    st.research = !st.research;
                }
                let tools_btn = icon_button(ui, t, Glyph::Grid, "Tools", tools_open, true, None);
                let shown = egui::Popup::from_toggle_button_response(&tools_btn)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        ui.set_min_width(330.0);
                        ui.label(egui::RichText::new("Tools").strong());
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                            ai_guidelines::ui(ui, t, &mut st.guide);
                            ai_context::ui(ui, t, &mut st.context);
                            ai_integrity::ui(ui, t, &mut st.integrity);
                        });
                    });
                tools_open = shown.is_some();
            });

            // Toolbar, right (added right to left): send, mic, model.
            ui.scope_builder(egui::UiBuilder::new().max_rect(bar_rect).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if send_button(ui, t, can).on_hover_text("Create document (Ctrl+Enter)").clicked() && can {
                    send = Some(st.input.trim().to_string());
                }
                let tip = if listening { "Stop and transcribe" } else { "Speak your request" };
                let solid = if listening { Some((t.red, t.on_accent)) } else { None };
                if icon_button(ui, t, Glyph::Mic, tip, false, can_talk, solid).on_hover_text(tip).clicked() && can_talk {
                    mic = true;
                }
                let current = st.model.and_then(|i| ai_net::CANDIDATES.get(i)).map_or("Auto", |c| c.label);
                let short: String = if current.chars().count() > 9 { current.chars().take(8).chain(std::iter::once('…')).collect() } else { current.to_string() };
                let model_btn = chip(ui, t, None, &short, true, false).on_hover_text(format!("Model: {current}"));
                let _ = egui::Popup::menu(&model_btn).close_behavior(egui::PopupCloseBehavior::CloseOnClick).show(|ui| {
                    ui.selectable_value(&mut st.model, None, "Auto (falls back)");
                    for (i, c) in ai_net::CANDIDATES.iter().enumerate() {
                        ui.selectable_value(&mut st.model, Some(i), c.label);
                    }
                });
            });
        });
    ctx.data_mut(|d| d.insert_temp(tools_id, tools_open));
    if close {
        st.open = false;
    }
    if mic {
        toggle_mic(st);
    }
    if format {
        st.format_request = Some(st.input.trim().to_string());
    }
    if dismiss {
        st.proposal = None;
    }
    if pick.is_some() {
        st.pick = pick;
    }
    if let Some(prompt) = send {
        start(ctx, st, prompt);
    }
}
fn toggle_mic(st: &mut State) {
    match st.listening.as_mut() {
        Some(l) => l.stop(),
        None => match ai_voice::start() {
            Ok(l) => st.listening = Some(l),
            Err(e) => st.messages.push((false, format!("Microphone: {e}"))),
        },
    }
}

/// The background work: plain writing, or research then writing.
/// The background work: plain writing, or research then writing.
fn work(prompt: &str, only: Option<usize>, research: bool, memory: Option<&str>) -> Result<Done, String> {
    if research {
        let o = ai_research::run(prompt, only)?;
        return Ok(Done { plan: o.plan, label: o.label, summary: o.summary });
    }
    let user = match memory {
        Some(m) => format!("{m}\n\nRequest:\n{prompt}"),
        None => prompt.to_string(),
    };
    let reply = ai_net::ask(ai_plan::SYSTEM_PROMPT, &user, only, |text| ai_plan::parse_plan(text).is_ok())?;
    let plan = ai_plan::parse_plan(&reply.text)?;
    Ok(Done { plan, label: reply.label, summary: String::new() })
}

fn start(ctx: &egui::Context, st: &mut State, prompt: String) {
    st.messages.push((true, prompt.clone()));
    st.input.clear();
    let (tx, rx) = channel();
    let wake = ctx.clone();
    let only = st.model;
    let research = st.research;
    let memory = st.context.prompt_block();
    let spawned = std::thread::Builder::new().name("word-ai".into()).spawn(move || {
        let r = work(&prompt, only, research, memory.as_deref());
        let _ = tx.send(r);
        wake.request_repaint();
    });
    match spawned {
        Ok(_) => st.busy = Some(rx),
        Err(e) => st.messages.push((false, format!("Couldn't start the request: {e}"))),
    }
}

fn poll(app: &mut WordApp, ctx: &egui::Context, st: &mut State) {
    // Voice: a transcript (or an error) arrived.
    if let Some(l) = &st.listening {
        match l.poll() {
            None => ctx.request_repaint_after(Duration::from_millis(150)),
            Some(result) => {
                st.listening = None;
                match result {
                    Err(e) => st.messages.push((false, format!("Voice input failed: {e}"))),
                    Ok(text) => {
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            st.messages.push((false, "I didn't hear anything. Press the mic and try again.".into()));
                        } else if st.review_voice || st.busy.is_some() {
                            if !st.input.trim().is_empty() {
                                st.input.push(' ');
                            }
                            st.input.push_str(&text);
                        } else {
                            start(ctx, st, text);
                        }
                    }
                }
                ctx.request_repaint();
            }
        }
    }
    // Formatting: the options are ready.
    if let Some(rx) = &st.format_busy {
        match rx.try_recv() {
            Ok(result) => {
                st.format_busy = None;
                match result {
                    Ok(p) => {
                        st.messages.push((false, ai_format::intro(&p)));
                        st.proposal = Some(p);
                    }
                    Err(e) => st.messages.push((false, format!("I couldn't work out a formatting plan.\n{e}"))),
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(150)),
            Err(TryRecvError::Disconnected) => {
                st.format_busy = None;
                st.messages.push((false, "The request stopped unexpectedly.".into()));
            }
        }
    }
    // Writing: the document is ready.
    let Some(rx) = &st.busy else { return };
    match rx.try_recv() {
        Ok(result) => {
            st.busy = None;
            let msg = match result {
                Ok(done) => finish(app, st, done),
                Err(e) => format!("I couldn't create the document.\n{e}"),
            };
            st.messages.push((false, msg));
            ctx.request_repaint();
        }
        Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(150)),
        Err(TryRecvError::Disconnected) => {
            st.busy = None;
            st.messages.push((false, "The request stopped unexpectedly.".into()));
        }
    }
}

/// Build the plan into the document; the message to show.
/// Build the plan into a private draft and open the preview; the message to show.
fn finish(app: &mut WordApp, st: &mut State, done: Done) -> String {
    let title = if done.plan.document_type.is_empty() { "New document".to_string() } else { format!("New {}", done.plan.document_type) };
    match ai_preview::build_draft(&app.session, &title, |s| ai_plan::apply_plan(s, &done.plan).map(|_| ())) {
        Ok(draft) => {
            let n = draft.change_count();
            st.preview.open(draft);
            let mut kind = done.plan.document_type.clone();
            if !done.plan.audience.is_empty() {
                kind = format!("{kind} for {}", done.plan.audience);
            }
            let mut m = format!(
                "Draft ready: {kind}. {n} changes to review in the preview window. Your document has not been changed yet.\n{}",
                done.plan.summary
            );
            if !done.summary.is_empty() {
                m.push_str("\n\n");
                m.push_str(&done.summary);
            }
            m.push_str(&format!("\n\nWritten by {}. Press Apply in the preview to add it, or Discard to drop it.", done.label));
            m
        }
        Err(e) => format!("I couldn't prepare the draft.\n{e}"),
    }
}
/// Look at the document, then ask the models for formatting options on a worker thread.
fn start_format(ctx: &egui::Context, st: &mut State, session: &wordcraft_engine::Session, request: String) {
    let shown = if request.is_empty() { "Format my document".to_string() } else { format!("Format my document: {request}") };
    st.messages.push((true, shown));
    st.input.clear();
    st.proposal = None;
    let profile = ai_format::profile(session);
    let (tx, rx) = channel();
    let wake = ctx.clone();
    let only = st.model;
    let spawned = std::thread::Builder::new().name("word-ai-format".into()).spawn(move || {
        let r = ai_format::propose(&request, profile, only);
        let _ = tx.send(r);
        wake.request_repaint();
    });
    match spawned {
        Ok(_) => st.format_busy = Some(rx),
        Err(e) => st.messages.push((false, format!("Couldn't start the request: {e}"))),
    }
}

/// Apply the option the user picked. The options stay on offer so another can be tried.
/// Build the option the user picked into a private draft and open the preview.
fn apply_choice(app: &mut WordApp, st: &mut State, i: usize) {
    let Some(p) = st.proposal.clone() else { return };
    let Some(choice) = p.choices.get(i) else { return };
    let title = format!("Layout option {} ({})", i + 1, choice.category.label());
    let msg = match ai_preview::build_draft(&app.session, &title, |s| ai_format::apply(s, choice).map(|_| ())) {
        Ok(draft) => {
            let n = draft.change_count();
            st.preview.open(draft);
            format!(
                "Previewing option {} ({}): {}.\n{n} changes to review in the preview window. Your document has not been changed yet.\n\nSuggested by {}.",
                i + 1,
                choice.category.label(),
                choice.describe(),
                p.model
            )
        }
        Err(e) => format!("I couldn't prepare that layout.\n{e}"),
    };
    st.messages.push((false, msg));
}
