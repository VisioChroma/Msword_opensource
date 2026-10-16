//! The Objects pane (Layout › Arrange › Selection Pane): every picture, shape, text box and
//! group of the document as a list. A click selects the object (and scrolls to it, even when
//! it is hidden under another), the check box adds it to the selection for Group, a double
//! click renames it, the padlock locks or unlocks it.

use egui::{Ui, vec2};
use serde_json::{Value, json};
use wordcraft_doc::Pos;

use crate::WordApp;

/// The list from the engine, kept until the document changes (the list walks the whole
/// document, so it isn't redone on every frame).
fn rows(app: &mut WordApp, ctx: &egui::Context) -> Vec<Value> {
    let key = egui::Id::new("objects_pane_rows");
    let stamp = (app.session.rev(), app.session.document_id());
    if let Some((seen, rows)) = ctx.data(|d| d.get_temp::<((u64, u64), Vec<Value>)>(key))
        && seen == stamp
    {
        return rows;
    }
    let rows = app.session.run("object.list", &json!({})).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    ctx.data_mut(|d| d.insert_temp(key, (stamp, rows.clone())));
    rows
}

pub fn show(app: &mut WordApp, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| {
        for (label, id) in
            [("Group", "arrange.group"), ("Ungroup", "arrange.ungroup"), ("Bring Forward", "arrange.bringForward"), ("Send Backward", "arrange.sendBackward")]
        {
            let enabled = crate::widgets::enabled(app, id);
            if ui.add_enabled(enabled, egui::Button::new(tl!(label))).clicked() {
                let _ = app.run(id, json!({}));
            }
        }
    });
    ui.add_space(6.0);
    let rows = rows(app, ui.ctx());
    if rows.is_empty() {
        ui.label(egui::RichText::new(tl!("No pictures, shapes or text boxes yet.")).weak());
        return;
    }
    let picked = wordcraft_engine::cmd::objects::picked_objects(&app.session);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for r in &rows {
            row(app, ui, r, &picked);
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new(tl!("Double-click a name to rename it.")).small().weak());
    });
}

fn row(app: &mut WordApp, ui: &mut Ui, row: &Value, picked: &[Pos]) {
    let index = row.get("index").and_then(Value::as_u64).unwrap_or(0);
    let name = row.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let locked = row.get("locked").and_then(Value::as_bool).unwrap_or(false);
    let hidden = row.get("hidden").and_then(Value::as_bool).unwrap_or(false);
    let members = row.get("members").and_then(Value::as_u64).unwrap_or(0);
    let pos: Option<Pos> = row.get("pos").and_then(|p| serde_json::from_value(p.clone()).ok());
    let selected = pos.as_ref().is_some_and(|p| picked.contains(p));
    let rename_id = egui::Id::new("objects_pane_rename");
    let editing: Option<(u64, String)> = ui.data(|d| d.get_temp(rename_id));
    ui.horizontal(|ui| {
        let mut on = selected;
        if ui.checkbox(&mut on, "").on_hover_text(tl!("Add to selection")).changed() {
            let _ = app.run("select.addObject", json!({"index": index}));
        }
        let width = (ui.available_width() - 56.0).max(40.0);
        match editing.filter(|(i, _)| *i == index) {
            Some((_, mut text)) => {
                let r = ui.add_sized(vec2(width, 22.0), egui::TextEdit::singleline(&mut text));
                if !r.has_focus() && !r.lost_focus() {
                    r.request_focus();
                }
                ui.data_mut(|d| d.insert_temp(rename_id, (index, text.clone())));
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ui.data_mut(|d| d.remove::<(u64, String)>(rename_id));
                } else if r.lost_focus() {
                    ui.data_mut(|d| d.remove::<(u64, String)>(rename_id));
                    if text.trim() != name {
                        let _ = app.run("object.rename", json!({"pos": pos, "name": text}));
                    }
                }
            }
            None => {
                let label = if members > 0 { format!("{name}  ({members})") } else { name.clone() };
                let r = ui.add_sized(vec2(width, 22.0), egui::Button::selectable(selected, label));
                if r.double_clicked() {
                    ui.data_mut(|d| d.insert_temp(rename_id, (index, name.clone())));
                } else if r.clicked() {
                    let _ = app.run("select.objects", json!({"index": index}));
                }
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let tip = if locked { tl!("Unlock Object") } else { tl!("Lock Object") };
            if crate::widgets::icon_button(ui, if locked { "lock" } else { "unlock" }, tip).clicked() {
                let _ = app.run("object.setLocked", json!({"pos": pos, "locked": !locked}));
            }
            let eye_tip = if hidden { tl!("Show Object") } else { tl!("Hide Object") };
            if crate::widgets::icon_button(ui, if hidden { "eye-off" } else { "eye" }, eye_tip).clicked() {
                let _ = app.run("object.setHidden", json!({"pos": pos, "hidden": !hidden}));
            }
        });
    });
}