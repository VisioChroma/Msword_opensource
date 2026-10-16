//! Pictures, shapes and text boxes on the canvas: click one to select it, drag it to move it,
//! drag a handle to resize it, drag the rotation handle to turn it (Shift: 15° steps), arrow keys
//! nudge a floating one. The frame itself is [`crate::frame`]; changes go through
//! `arrange.bounds` / `arrange.rotation` / `arrange.nudge`, so they're undoable and scriptable like
//! everything else.
//!
//! While dragging, nothing is laid out or rendered: the preview is the object's own pixels taken
//! from its page's cached texture. The drop is one command, one relayout.
//!
//! A selected connector shows a handle at each end instead of a frame: dragging one onto a shape
//! glues it to the nearest connection site there (`shape.connect`), elsewhere leaves it free.

use egui::{Color32, Painter, Pos2, Rect, Ui};
use serde_json::json;
use wordcraft_doc::para::{InlineObject, OBJ};
use wordcraft_doc::{PartKind, Pos, StoryRef};
use wordcraft_layout::DocLayout;
use wordcraft_layout::hit::ObjectHit;

use crate::WordApp;
use crate::frame::{self, Drag, Grab};
use crate::theme::Tokens;

/// The band along a text box's border that grabs the box rather than its text, screen points.
const EDGE: f32 = 5.0;
/// How opaque the drag preview of an object's pixels is (0–255).
const PREVIEW_OPACITY: u8 = 170;
/// Arrow-key nudge, points (Ctrl/Alt: fine).
const NUDGE: f32 = 6.0;
const NUDGE_FINE: f32 = 1.0;

/// An object being dragged by its frame.
pub struct ObjectDrag {
    object: ObjectHit,
    drag: Drag,
    /// Smallest size it can be resized to, points.
    min: f32,
    picture: bool,
    /// Dragging a connector's end (false: its start, true: its end) to this page point.
    end: Option<(bool, (f32, f32))>,
    /// Guides and gaps shown while a move snaps (see `wordcraft_geom::snap`).
    snap: Option<wordcraft_geom::snap::Snap>,
}

/// How near (screen points) a press must be to a connector's end to grab it.
const END_GRAB: f32 = 7.0;

/// The selected connector's start and end on its page (points), and its layout.
fn connector_ends(app: &WordApp, layout: &DocLayout) -> Option<(ObjectHit, (f32, f32), (f32, f32))> {
    let (pos, InlineObject::Shape { kind, float, .. }) = selected(app)? else { return None };
    if !kind.is_connector() {
        return None;
    }
    let (flip_h, flip_v) = (float.flip_h, float.flip_v);
    let o = layout.object(&pos, app.session.page_hint)?;
    let (a, b) = wordcraft_doc::connector::ends(o.rect.x, o.rect.y, o.rect.w, o.rect.h, flip_h, flip_v);
    Some((o, a, b))
}

fn point_on_screen(pages: &[Rect], scale: f32, page: usize, (x, y): (f32, f32)) -> Option<Pos2> {
    pages.get(page).map(|p| egui::pos2(p.min.x + x * scale, p.min.y + y * scale))
}

/// The selected connector's end under `at` (screen): which (false: start), and the connector.
fn connector_end_at(app: &WordApp, layout: &DocLayout, pages: &[Rect], scale: f32, at: Pos2) -> Option<(ObjectHit, bool)> {
    let (o, a, b) = connector_ends(app, layout)?;
    if hit_locked(app, &o) {
        return None;
    }
    let near = |p: (f32, f32)| point_on_screen(pages, scale, o.page, p).is_some_and(|s| s.distance(at) <= END_GRAB);
    if near(b) {
        Some((o, true))
    } else if near(a) {
        Some((o, false))
    } else {
        None
    }
}

/// The selected object: the selection is exactly one picture, shape or text box, in the body
/// or in a header/footer.
pub fn selected(app: &WordApp) -> Option<(Pos, &InlineObject)> {
    wordcraft_engine::cmd::objects::object_selection(&app.session).filter(|(p, _)| selectable_story(app, p.story))
}

/// Whether objects of `story` can be selected and dragged on the canvas: the body and
/// headers/footers.
fn selectable_story(app: &WordApp, story: StoryRef) -> bool {
    match story {
        StoryRef::Body => true,
        StoryRef::Part(id) => app.session.doc.parts.get(&id).is_some_and(|p| matches!(p.kind, PartKind::Header | PartKind::Footer)),
    }
}

/// The story whose objects the canvas handles: the header/footer being edited, else the body.
fn active_story(app: &WordApp, layout: &DocLayout) -> StoryRef {
    let story = crate::canvas::text_box_host(app, layout).unwrap_or(app.session.sel.focus.story);
    match story {
        StoryRef::Body => StoryRef::Body,
        StoryRef::Part(id) if selectable_story(app, StoryRef::Part(id)) => StoryRef::Part(id),
        // Notes and text-box text: no top-level objects live here, so grabs find nothing and
        // the press falls through to text handling.
        other => other,
    }
}

/// Whether the object under `hit` keeps its place and size: charts and diagrams can be selected
/// and deleted, not moved or resized (they aren't written back on save yet), so they get no
/// handles, no move cursor and no drag.
fn fixed(app: &WordApp, hit: &ObjectHit) -> bool {
    let pos = hit.pos();
    match app.session.doc.para_at(&pos).and_then(|p| p.object_at(pos.off)) {
        Some(InlineObject::Graphic { .. }) => true,
        Some(o) => o.is_locked(),
        None => false,
    }
}

/// Is the object under `hit` locked?
fn hit_locked(app: &WordApp, hit: &ObjectHit) -> bool {
    let pos = hit.pos();
    app.session.doc.para_at(&pos).and_then(|p| p.object_at(pos.off)).is_some_and(InlineObject::is_locked)
}

/// The padlock badge beside the rotation handle.
fn lock_badge(f: Rect, degrees: f32) -> Rect {
    let rotation = frame::rotate_handle(f, degrees);

    Rect::from_center_size(rotation + egui::vec2(32.0, 0.0), egui::vec2(22.0, 22.0))
}

/// The badge of the selected object; clicking it toggles locking.
fn lock_badge_rect(app: &WordApp, layout: &DocLayout, pages: &[Rect], scale: f32) -> Option<Rect> {
    let (object, editing) = active(app, layout)?;

    if editing || !selectable_story(app, object.story) {
        return None;
    }

    let rect = screen(pages, scale, object.page, object.rect)?;
    Some(lock_badge(rect, object.spin.deg))
}

/// A padlock, drawn in code.
fn paint_lock(painter: &Painter, t: &Tokens, badge: Rect, locked: bool) {
    let center = badge.center();

    painter.circle_filled(center + egui::vec2(0.0, 0.75), 11.5, t.handle_shadow);

    painter.circle(center, 11.0, t.handle, egui::Stroke::new(1.25, t.accent));

    crate::icons::paint(painter, badge.shrink(3.0), if locked { "lock" } else { "unlock" }, t.accent, t.accent);
}

/// The story of the selected text box.
fn selected_text_box(app: &WordApp) -> Option<u32> {
    match selected(app)? {
        (_, InlineObject::Shape { story: Some(id), .. }) if app.session.doc.parts.get(id).is_some_and(|p| p.kind == PartKind::TextBox) => Some(*id),
        _ => None,
    }
}

/// The object showing a frame, and whether its text is being edited (caret in a text box).
pub fn active(app: &WordApp, layout: &DocLayout) -> Option<(ObjectHit, bool)> {
    let hint = app.session.page_hint;
    if let Some((pos, _)) = selected(app) {
        return layout.object(&pos, hint).map(|o| (o, false));
    }
    match app.session.sel.focus.story {
        StoryRef::Part(id) if crate::canvas::in_text_box(app) => layout.text_box(id, hint).map(|o| (o, true)),
        _ => None,
    }
}

fn screen(pages: &[Rect], scale: f32, page: usize, r: wordcraft_geom::Rect) -> Option<Rect> {
    pages.get(page).map(|p| frame::to_screen(*p, scale, r))
}

/// Pressing, dragging, releasing or double-clicking on an object (or a frame handle). Returns
/// true when it was about an object, so the text doesn't also get it.
pub fn pointer(app: &mut WordApp, ui: &Ui, resp: &egui::Response, pages: &[Rect], layout: &DocLayout, scale: f32, at: Pos2) -> bool {
    if let Some(mut d) = app.canvas.obj_drag.take() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return true; // cancelled: dropped without a change
        }
        if let Some((which, _)) = d.end {
            if ui.input(|i| i.pointer.primary_down()) {
                if let Some((_, x, y)) = crate::canvas::page_at(pages, layout, scale, at).filter(|(p, ..)| *p == d.object.page) {
                    d.end = Some((which, (x, y)));
                    d.drag.moved = true;
                }
                app.canvas.obj_drag = Some(d);
            } else if d.drag.moved
                && let Some((_, (x, y))) = d.end
            {
                let key = if which { "end" } else { "start" };
                let _ = app.run("shape.connect", json!({key: {"x": x, "y": y, "page": d.object.page}}));
            }
            return true;
        }
        if ui.input(|i| i.pointer.primary_down()) {
            let shift = ui.input(|i| i.modifiers.shift);
            if d.drag.grab == Grab::Rotate {
                d.drag.turn_to(at, pages, scale, shift);
            } else {
                // Pictures keep their proportions from a corner unless Shift; shapes only with Shift.
                d.drag.update(at, pages, scale, d.min, d.picture != shift);
            }
            // Smart guides: snap a move to the other objects and the page margins (Alt: off).
            d.snap = None;
            if d.drag.grab == Grab::Move && d.drag.moved && !ui.input(|i| i.modifiers.alt) {
                use wordcraft_geom::snap::{R, snap_move};
                if let Some(pg) = layout.pages.get(d.drag.to_page) {
                    let picked = wordcraft_engine::cmd::objects::picked_objects(&app.session);
                    let own = d.object.pos();
                    let others: Vec<R> = layout
                        .page_objects(d.drag.to_page)
                        .iter()
                        .filter(|o| o.pos() != own && !picked.contains(&o.pos()))
                        .map(|o| {
                            let b = o.bounds();
                            R::new(b.x, b.y, b.w, b.h)
                        })
                        .collect();
                    let body = pg.body;
                    let lines_x = [body.x, pg.w / 2.0, body.x + body.w];
                    let lines_y = [body.y, pg.h / 2.0, body.y + body.h];
                    let r = d.drag.rect;
                    let s = snap_move(R::new(r.x, r.y, r.w, r.h), &others, R::new(0.0, 0.0, pg.w, pg.h), &lines_x, &lines_y, 6.0 / scale.max(0.01));
                    d.drag.rect.x += s.dx;
                    d.drag.rect.y += s.dy;
                    d.snap = Some(s);
                }
            }
            app.canvas.obj_drag = Some(d);
        } else {
            drop(app, &d);
        }
        return true;
    }
    let pressed = ui.input(|i| i.pointer.primary_pressed()) && resp.contains_pointer();
    if pressed && lock_badge_rect(app, layout, pages, scale).is_some_and(|badge| badge.contains(at)) {
        let _ = app.run("object.toggleLock", json!({}));
        return true;
    }
    if app.canvas.link_from.is_some() && (pressed || ui.input(|i| i.key_pressed(egui::Key::Escape))) {
        link_click(app, layout, pages, scale, at, pressed);
        return true;
    }
    let multi = resp.double_clicked() || resp.triple_clicked();
    if !(pressed || multi) {
        return false;
    }
    // A selected connector's end: drag it to glue it.
    if pressed && let Some((object, which)) = connector_end_at(app, layout, pages, scale, at) {
        let drag = Drag::new(Grab::Move, object.page, object.rect, 0.0, at);
        app.canvas.obj_drag = Some(ObjectDrag { drag, object, min: 0.0, picture: false, end: Some((which, (0.0, 0.0))), snap: None });
        return true;
    }
    let Some((object, grab)) = grab_at(app, layout, pages, scale, at) else { return false };
    // Shift/Ctrl+click adds an object to the selected ones (or takes it out), for Group.
    let adding = ui.input(|i| i.modifiers.shift || i.modifiers.command);
    if pressed && adding && grab == Grab::Move && selected(app).is_some() {
        let _ = app.run("select.addObject", json!({"pos": object.pos()}));
        return true;
    }
    if pressed {
        let pos = object.pos();
        let end = Pos { off: pos.off + OBJ.len_utf8(), ..pos.clone() };
        let _ = app.run("select.range", json!({"anchor": pos, "focus": end}));
        let (min, picture) = match selected(app) {
            Some((_, o)) => (wordcraft_engine::cmd::objects::min_size(o), matches!(o, InlineObject::Image { .. })),
            None => return true,
        };
        // Objects in table cells can be selected and resized, not moved; charts neither.
        if !fixed(app, &object) && (grab != Grab::Move || movable(app, &object)) {
            app.canvas.obj_drag = Some(ObjectDrag {
                drag: Drag::new(grab, object.page, object.rect, object.spin.deg, at),
                object,
                min,
                picture,
                end: None,
                snap: None,
            });
        }
    }
    true
}

/// Create Link's click: link the box it started from to the text box under `at` (anywhere on it),
/// if `pressed`; either way, linking is over.
fn link_click(app: &mut WordApp, layout: &DocLayout, pages: &[Rect], scale: f32, at: Pos2, pressed: bool) {
    let Some(from) = app.canvas.link_from.take() else { return };
    if !pressed {
        return;
    }
    let target = crate::canvas::page_at(pages, layout, scale, at).and_then(|(page, x, y)| {
        layout.find_object(page, |o| o.page == page && o.text_box.is_some() && o.rect.contains(wordcraft_geom::Point::new(x, y)))
    });
    let Some(target) = target else {
        app.status(tl!("Not linked: click a text box"));
        return;
    };
    let end = Pos { off: from.off + OBJ.len_utf8(), ..from.clone() };
    let _ = app.run("select.range", json!({"anchor": from, "focus": end}));
    if let Err(e) = app.run("shape.link", json!({"target": target.pos()})) {
        app.status(e);
    }
}

/// Right-click at `at`: the picture, shape or text box under it becomes the selection (as a click
/// would, without a drag), so the context menu acts on it. Returns whether there was one.
pub fn select_at(app: &mut WordApp, layout: &DocLayout, pages: &[Rect], scale: f32, at: Pos2) -> bool {
    if crate::canvas::editing_header_footer(app, layout) {
        return false;
    }
    let Some((object, _)) = grab_at(app, layout, pages, scale, at) else { return false };
    let pos = object.pos();
    // Already selected (alone or among several picked for Group): keep the selection.
    if !wordcraft_engine::cmd::objects::picked_objects(&app.session).contains(&pos) {
        let end = Pos { off: pos.off + OBJ.len_utf8(), ..pos.clone() };
        let _ = app.run("select.range", json!({"anchor": pos, "focus": end}));
    }
    true
}

/// What a press at `at` would grab: a handle of the shown frame, else an object of the story
/// being edited (body, or the header/footer).
fn grab_at(app: &WordApp, layout: &DocLayout, pages: &[Rect], scale: f32, at: Pos2) -> Option<(ObjectHit, Grab)> {
    let story = active_story(app, layout);
    let handle = active(app, layout).filter(|(o, _)| o.story == story && !fixed(app, o)).and_then(|(o, _)| {
        let f = screen(pages, scale, o.page, o.rect)?;
        if frame::rotate_handle_at(f, o.spin.deg, at) {
            return Some((o, Grab::Rotate));
        }
        frame::handle_at(f, o.spin.deg, at).map(|h| (o, Grab::Resize(h)))
    });
    handle.or_else(|| {
        let (page, x, y) = crate::canvas::page_at(pages, layout, scale, at)?;
        // Selectable: body objects (table cells too) or the header/footer's own objects;
        // notes' objects aren't yet.
        layout.object_at_in(page, x, y, EDGE / scale.max(0.01), story).map(|o| (o, Grab::Move))
    })
}

/// Objects that can be dragged to a new place: those in the own paragraphs of the body or of a
/// header/footer.
fn movable(app: &WordApp, o: &ObjectHit) -> bool {
    o.path.depth() == 0 && selectable_story(app, o.story)
}

/// Commit a finished drag as one command.
fn drop(app: &mut WordApp, d: &ObjectDrag) {
    if !d.drag.moved {
        return;
    }
    let r = d.drag.rect;
    let params = match d.drag.grab {
        Grab::Rotate => {
            let _ = app.run("arrange.rotation", json!({"degrees": d.drag.deg}));
            return;
        }
        Grab::Move => json!({"page": d.drag.to_page, "x": r.x, "y": r.y}),
        // An inline object stays in the text; a floating one keeps its far edges put.
        Grab::Resize(_) if d.object.floating() && movable(app, &d.object) => {
            json!({"width": r.w, "height": r.h, "page": d.drag.page, "x": r.x, "y": r.y})
        }
        Grab::Resize(_) => json!({"width": r.w, "height": r.h}),
    };
    let _ = app.run("arrange.bounds", params);
}

/// Draw the frame of the active object, or the drag preview.
pub fn paint(app: &WordApp, painter: &Painter, t: &Tokens, layout: &DocLayout, pages: &[Rect], scale: f32) {
    if let Some(d) = app.canvas.obj_drag.as_ref().filter(|d| d.end.is_some()) {
        paint_end_drag(app, d, painter, t, layout, pages, scale);
        return;
    }
    if let Some((o, a, b)) = connector_ends(app, layout) {
        let locked = hit_locked(app, &o);
        if !locked {
            let glued = match selected(app) {
                Some((_, InlineObject::Shape { extra, .. })) => (extra.start.is_some(), extra.end.is_some()),
                _ => (false, false),
            };
            for (p, on) in [(a, glued.0), (b, glued.1)] {
                if let Some(c) = point_on_screen(pages, scale, o.page, p) {
                    painter.circle(c, 4.5, if on { t.accent } else { Color32::WHITE }, egui::Stroke::new(1.2, t.accent));
                }
            }
        }
        if let Some(f) = screen(pages, scale, o.page, o.rect) {
            paint_lock(painter, t, lock_badge(f, o.spin.deg), locked);
        }
        return;
    }
    if let Some(d) = app.canvas.obj_drag.as_ref().filter(|d| d.drag.moved) {
        let Some(to) = screen(pages, scale, d.drag.to_page, d.drag.rect) else { return };
        if d.drag.grab == Grab::Rotate {
            frame::paint_turning(painter, t, to, d.drag.deg);
            return;
        }
        // The object's own pixels, from its page's cached texture: moved, or scaled for an
        // unturned picture. Resizing reflows a text box (and redraws a shape), so that's just the
        // new outline.
        let turned = !d.object.spin.is_identity();
        let pixels = (d.picture && !turned) || d.drag.grab == Grab::Move;
        if !pixels {
            frame::paint_outline(painter, t, to, d.drag.deg);
        } else if let (Some(tex), Some(pg)) = (app.canvas.page_texture(d.object.page), layout.pages.get(d.object.page)) {
            // A turned object's pixels fill its rotated bounds, which move with it.
            let r = d.object.bounds();
            let (dx, dy) = (d.drag.rect.x - d.drag.start.x, d.drag.rect.y - d.drag.start.y);
            let moved = wordcraft_geom::Rect::new(r.x + dx, r.y + dy, r.w, r.h);
            let to = if turned { screen(pages, scale, d.drag.to_page, moved).unwrap_or(to) } else { to };
            let (w, h) = (pg.w.max(1.0), pg.h.max(1.0));
            let uv = Rect::from_min_max(egui::pos2(r.x / w, r.y / h), egui::pos2(r.right() / w, r.bottom() / h));
            painter.image(tex.id(), to, uv, Color32::from_white_alpha(PREVIEW_OPACITY));
        }
        frame::paint(painter, t, to, d.drag.deg, false, false, false);
        if let (Some(s), Some(page)) = (d.snap.as_ref(), pages.get(d.drag.to_page)) {
            paint_snap(painter, s, *page, scale);
        }
        return;
    }
    // The others selected along with it (Shift+click): their outlines.
    for p in &app.session.also_selected {
        if let Some(o) = layout.object(p, app.session.page_hint)
            && let Some(f) = screen(pages, scale, o.page, o.rect)
        {
            frame::paint(painter, t, f, o.spin.deg, false, false, false);
        }
    }
    if let Some((o, editing)) = active(app, layout)
        && let Some(f) = screen(pages, scale, o.page, o.rect)
    {
        // Handles where it can be resized and turned (body and header/footer objects,
        // not charts): else a plain frame.
        let handles = selectable_story(app, o.story) && !fixed(app, &o);
        frame::paint(painter, t, f, o.spin.deg, editing, handles, handles);
        if !editing && o.floating() {
            let with_text = wordcraft_engine::cmd::objects::selected(&app.session)
                .and_then(|(_, ob)| ob.frame().map(|(_, _, fl)| wordcraft_engine::cmd::objects::moves_with_text(fl)))
                .unwrap_or(false);
            if with_text && let Some(c) = point_on_screen(pages, scale, o.page, (o.origin.x - 14.0, o.origin.y + 9.0)) {
                paint_anchor(painter, t, c);
            }
        }
        if !editing && selectable_story(app, o.story) {
            paint_lock(painter, t, lock_badge(f, o.spin.deg), hit_locked(app, &o));
        }
    }
}
/// A small anchor beside the paragraph a floating object moves with (shown while it's selected).
fn paint_anchor(painter: &Painter, t: &Tokens, c: Pos2) {
    let st = egui::Stroke::new(1.4, t.accent);
    painter.circle_stroke(c + egui::vec2(0.0, -5.0), 2.0, st);
    painter.line_segment([c + egui::vec2(0.0, -3.0), c + egui::vec2(0.0, 6.0)], st);
    painter.line_segment([c + egui::vec2(-3.0, -0.5), c + egui::vec2(3.0, -0.5)], st);
    painter.line_segment([c + egui::vec2(0.0, 6.0), c + egui::vec2(-5.0, 2.5)], st);
    painter.line_segment([c + egui::vec2(0.0, 6.0), c + egui::vec2(5.0, 2.5)], st);
}
/// The red guide lines and the gap measurements shown while a move snaps (Figma style).
fn paint_snap(painter: &Painter, s: &wordcraft_geom::snap::Snap, page: Rect, scale: f32) {
    use wordcraft_geom::snap::Axis;
    let red = Color32::from_rgb(242, 72, 34);
    let magenta = Color32::from_rgb(229, 46, 176);
    let at = |x: f32, y: f32| egui::pos2(page.min.x + x * scale, page.min.y + y * scale);
    for g in &s.guides {
        let (a, b) = match g.axis {
            Axis::X => (at(g.pos, g.from), at(g.pos, g.to)),
            Axis::Y => (at(g.from, g.pos), at(g.to, g.pos)),
        };
        painter.line_segment([a, b], egui::Stroke::new(1.0, red));
        for p in [a, b] {
            painter.circle_filled(p, 2.5, red);
        }
    }
    for gap in &s.gaps {
        let color = if gap.equal { magenta } else { red };
        let (a, b) = match gap.axis {
            Axis::X => (at(gap.start, gap.cross), at(gap.end, gap.cross)),
            Axis::Y => (at(gap.cross, gap.start), at(gap.cross, gap.end)),
        };
        painter.line_segment([a, b], egui::Stroke::new(1.0, color));
        let tick = match gap.axis {
            Axis::X => egui::vec2(0.0, 4.0),
            Axis::Y => egui::vec2(4.0, 0.0),
        };
        for p in [a, b] {
            painter.line_segment([p - tick, p + tick], egui::Stroke::new(1.0, color));
        }
        // The gap in points, in a small label in the middle.
        let label = format!("{}", (gap.end - gap.start).round() as i32);
        let galley = painter.layout_no_wrap(label, egui::FontId::proportional(10.0), Color32::WHITE);
        let c = a.lerp(b, 0.5);
        let r = Rect::from_center_size(c, galley.size() + egui::vec2(8.0, 4.0));
        painter.rect_filled(r, egui::CornerRadius::same(3), color);
        painter.galley(r.center() - galley.size() / 2.0, galley, Color32::WHITE);
    }
}
/// A connector's end being dragged: a line from its other end to the pointer, and the connection
/// sites of the shapes near the pointer (the one it would glue to, filled).
fn paint_end_drag(app: &WordApp, d: &ObjectDrag, painter: &Painter, t: &Tokens, layout: &DocLayout, pages: &[Rect], scale: f32) {
    let Some((which, at)) = d.end.filter(|_| d.drag.moved) else { return };
    let Some((_, a, b)) = connector_ends(app, layout) else { return };
    let fixed = if which { a } else { b };
    let snap = wordcraft_engine::cmd::connectors::SNAP;
    let mut best: Option<((f32, f32), f32)> = None;
    if let Some(pg) = layout.pages.get(d.object.page) {
        for it in &pg.items {
            let wordcraft_layout::Placed::Object { rect, spin, story: StoryRef::Body, path, off, .. } = it else { continue };
            let pos = Pos { story: StoryRef::Body, path: path.clone(), off: *off };
            let Some(InlineObject::Shape { kind, .. }) = app.session.doc.para_at(&pos).and_then(|p| p.object_at(pos.off)) else { continue };
            if kind.is_connector() || pos == d.object.pos() {
                continue;
            }
            let r = [rect.x, rect.y, rect.w, rect.h];
            // Sites of shapes the pointer is near.
            if at.0 < rect.x - 4.0 * snap || at.0 > rect.right() + 4.0 * snap || at.1 < rect.y - 4.0 * snap || at.1 > rect.bottom() + 4.0 * snap {
                continue;
            }
            for (i, _) in wordcraft_doc::connector::sites(*kind, rect.w, rect.h).iter().enumerate() {
                let Some(p) = wordcraft_doc::connector::site_point(*kind, r, *spin, i as u32) else { continue };
                let dist = ((p.0 - at.0).powi(2) + (p.1 - at.1).powi(2)).sqrt();
                if let Some(c) = point_on_screen(pages, scale, d.object.page, p) {
                    painter.circle_stroke(c, 3.5, egui::Stroke::new(1.0, t.accent));
                }
                if dist <= snap && best.is_none_or(|b| dist < b.1) {
                    best = Some((p, dist));
                }
            }
        }
    }
    let to = best.map_or(at, |b| b.0);
    if let (Some(f), Some(e)) = (point_on_screen(pages, scale, d.object.page, fixed), point_on_screen(pages, scale, d.object.page, to)) {
        painter.line_segment([f, e], egui::Stroke::new(1.5, t.accent));
        painter.circle(e, 4.5, if best.is_some() { t.accent } else { Color32::WHITE }, egui::Stroke::new(1.2, t.accent));
    }
}

/// The pointer over an object or a handle (or dragging): its cursor.
pub fn cursor(app: &WordApp, layout: &DocLayout, pages: &[Rect], scale: f32, at: Pos2) -> Option<egui::CursorIcon> {
    if let Some(d) = &app.canvas.obj_drag {
        return Some(if d.end.is_some() { egui::CursorIcon::Crosshair } else { d.drag.grab.cursor() });
    }
    if connector_end_at(app, layout, pages, scale, at).is_some() {
        return Some(egui::CursorIcon::Crosshair);
    }
    grab_at(app, layout, pages, scale, at).map(|(o, g)| if fixed(app, &o) { egui::CursorIcon::Default } else { g.cursor() })
}

/// Keys for a selected object: arrows nudge a floating one; typing or Enter goes into a text box.
/// Returns true if handled.
pub fn key(app: &mut WordApp, key: egui::Key, m: egui::Modifiers) -> bool {
    if selected(app).is_none() {
        return false;
    }
    let step = if m.ctrl || m.alt || m.command { NUDGE_FINE } else { NUDGE };
    let (dx, dy) = match key {
        egui::Key::ArrowLeft => (-step, 0.0),
        egui::Key::ArrowRight => (step, 0.0),
        egui::Key::ArrowUp => (0.0, -step),
        egui::Key::ArrowDown => (0.0, step),
        egui::Key::Enter => return enter_text_box(app),
        _ => return false,
    };
    // Inline objects move with the text: arrows move the caret as usual.
    app.run("arrange.nudge", json!({"dx": dx, "dy": dy})).is_ok()
}

/// With a text box selected, put the caret at the end of its text (typing goes there).
pub fn enter_text_box(app: &mut WordApp) -> bool {
    let Some(id) = selected_text_box(app) else { return false };
    // A box linked from another shows (and edits) the first box's story.
    let id = app.session.doc.text_box_chain(id).and_then(|c| c.first().copied()).unwrap_or(id);
    let end = app.session.doc.end_of(StoryRef::Part(id));
    app.run("caret.set", json!({"pos": end})).is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wordcraft_doc::para::{InlineObject, ShapeKind};
    use wordcraft_doc::{Document, Pos};

    use super::*;

    /// An app whose first paragraph holds `obj`, and where it's laid out.
    fn app_with(obj: InlineObject) -> (WordApp, ObjectHit) {
        let mut app = WordApp::new(wordcraft_engine::Session::new(Document::new()), crate::Services::default());
        app.session.doc.insert_object(&Pos::body(0, 0), obj, &Default::default()).unwrap();
        let hit = app.session.layout().object(&Pos::body(0, 0), 0).expect("laid out");
        (app, hit)
    }

    #[test]
    fn charts_get_no_handles_or_drag_but_shapes_do() {
        let chart =
            InlineObject::Graphic { w: 200.0, h: 100.0, alt: String::new(), float: Default::default(), graphic: Arc::new(Default::default()) };
        let (app, hit) = app_with(chart);
        assert!(fixed(&app, &hit));
        let shape = InlineObject::Shape {
            kind: ShapeKind::Rectangle,
            w: 50.0,
            h: 50.0,
            fill: None,
            stroke: None,
            stroke_width: 1.0,
            float: Default::default(),
            story: None,
            freeform: None,
            effects: Default::default(),
            extra: Default::default(),
        };
        let (app, hit) = app_with(shape);
        assert!(!fixed(&app, &hit));
    }
}
