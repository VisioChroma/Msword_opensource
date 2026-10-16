//! The Components pane (Insert › Quick Parts › Document Components): save the selection as a
//! reusable header, footer, callout, signature block or branded section, insert it into any
//! document (optionally linked to its source), replace a source and refresh linked copies.

use egui::{RichText, Ui};
use serde_json::json;
use wordcraft_engine::cmd::components::{KINDS, caret_linked_copy, linked_copies};

use crate::WordApp;
use crate::theme::semibold;

/// What was typed in the Save row, kept between frames.
#[derive(Clone)]
struct Draft {
    name: String,
    kind: usize,
}

impl Default for Draft {
    fn default() -> Self {
        Draft { name: String::new(), kind: KINDS.len().saturating_sub(1) }
    }
}

fn kind_at(i: usize) -> &'static str {
    KINDS.get(i).copied().unwrap_or("general")
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "header" => "Header",
        "footer" => "Footer",
        "callout" => "Callout box",
        "signature" => "Signature block",
        "branded" => "Branded section",
        _ => "General",
    }
}

/// The linked copies as (name, revision), kept until the document changes (finding them walks
/// the whole document, so it isn't redone on every frame).
fn copies(app: &WordApp, ctx: &egui::Context) -> Vec<(String, u32)> {
    let key = egui::Id::new("components_pane_linked");
    let stamp = (app.session.rev(), app.session.document_id());
    if let Some((seen, rows)) = ctx.data(|d| d.get_temp::<((u64, u64), Vec<(String, u32)>)>(key))
        && seen == stamp
    {
        return rows;
    }
    let rows: Vec<(String, u32)> = linked_copies(&app.session).into_iter().map(|l| (l.name, l.rev)).collect();
    ctx.data_mut(|d| d.insert_temp(key, (stamp, rows.clone())));
    rows
}

pub fn show(app: &mut WordApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    save_row(app, ui);
    ui.add_space(8.0);
    let copies = copies(app, &ctx);
    let names: Vec<(String, String, u32)> = app.session.components.items.iter().map(|(n, c)| (n.clone(), c.kind.clone(), c.rev)).collect();
    let behind = copies.iter().filter(|(n, r)| names.iter().any(|(m, _, rev)| m == n && rev > r)).count();
    linked_row(app, ui, copies.len(), behind);
    ui.separator();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if names.is_empty() {
            ui.label(RichText::new(tl!("No components yet. Select content, name it and press Save.")).weak());
        }
        for kind in KINDS {
            let rows: Vec<&(String, String, u32)> = names.iter().filter(|(_, k, _)| k.as_str() == kind).collect();
            if rows.is_empty() {
                continue;
            }
            ui.label(RichText::new(tl!(kind_label(kind))).font(semibold(12.0)).weak());
            for (name, _, rev) in rows {
                row(app, ui, name, *rev, &copies);
            }
            ui.add_space(6.0);
        }
    });
}

/// Name, kind and Save: the selection becomes a new component, or replaces the source of the
/// one with that name.
fn save_row(app: &mut WordApp, ui: &mut Ui) {
    let id = egui::Id::new("components_pane_draft");
    let mut draft: Draft = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    ui.label(RichText::new(tl!("Save the selection as a component")).font(semibold(13.0)));
    ui.add(egui::TextEdit::singleline(&mut draft.name).hint_text(tl!("Component name")).desired_width(f32::INFINITY));
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("components_pane_kind").selected_text(tl!(kind_label(kind_at(draft.kind)))).show_ui(ui, |ui| {
            for (i, k) in KINDS.iter().enumerate() {
                ui.selectable_value(&mut draft.kind, i, tl!(kind_label(k)));
            }
        });
        let name = draft.name.trim().to_string();
        let exists = app.session.components.items.contains_key(&name);
        let can_save = !name.is_empty() && !app.session.sel.is_collapsed();
        let label = if exists { tl!("Replace source") } else { tl!("Save") };
        if ui.add_enabled(can_save, egui::Button::new(label)).clicked() {
            let mut params = json!({"name": name, "update": exists});
            if !exists {
                params["kind"] = json!(kind_at(draft.kind));
            }
            if app.run("component.save", params).is_ok() {
                draft.name.clear();
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(id, draft));
}

/// Where the caret is, how many linked copies are behind, Refresh and Break Link.
fn linked_row(app: &mut WordApp, ui: &mut Ui, total: usize, behind: usize) {
    if let Some((name, _)) = caret_linked_copy(&app.session) {
        ui.label(RichText::new(crate::i18n::fmt(tl!("The cursor is in a linked copy of {name}."), &[("name", name.as_str())])).small());
    }
    if total > 0 {
        let n = behind.to_string();
        let text = if behind == 0 {
            tl!("Linked copies are up to date.").to_string()
        } else {
            crate::i18n::fmt(tl!("{n} linked copies are out of date."), &[("n", n.as_str())])
        };
        ui.label(text);
    }
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(behind > 0, egui::Button::new(tl!("Refresh linked copies"))).clicked() {
            crate::widgets::run_from_ribbon(app, "component.sync", json!({}));
        }
        let can_break = crate::widgets::enabled(app, "component.unlink");
        if ui.add_enabled(can_break, egui::Button::new(tl!("Break link"))).clicked() {
            crate::widgets::run_from_ribbon(app, "component.unlink", json!({}));
        }
    });
}

fn row(app: &mut WordApp, ui: &mut Ui, name: &str, rev: u32, copies: &[(String, u32)]) {
    let used = copies.iter().filter(|(n, _)| n.as_str() == name).count();
    let behind = copies.iter().any(|(n, r)| n.as_str() == name && *r < rev);
    ui.horizontal(|ui| {
        ui.label(RichText::new(name).font(semibold(13.0)));
        if used > 0 {
            ui.label(RichText::new(format!("× {used}")).weak());
        }
    });
    ui.horizontal_wrapped(|ui| {
        if ui.button(tl!("Insert")).on_hover_text(tl!("Insert a copy at the cursor")).clicked() {
            crate::widgets::run_from_ribbon(app, "component.insert", json!({"name": name}));
        }
        if ui.button(tl!("Insert linked")).on_hover_text(tl!("Insert a copy that can be refreshed from its source")).clicked() {
            crate::widgets::run_from_ribbon(app, "component.insert", json!({"name": name, "linked": true}));
        }
        let has_selection = !app.session.sel.is_collapsed();
        if ui
            .add_enabled(has_selection, egui::Button::new(tl!("Replace source")))
            .on_hover_text(tl!("Replace this component with the selection"))
            .clicked()
        {
            let _ = app.run("component.save", json!({"name": name, "update": true}));
        }
        if ui.button(tl!("Delete")).clicked() {
            let _ = app.run("component.delete", json!({"name": name}));
        }
    });
    if behind {
        ui.label(RichText::new(tl!("Some linked copies are out of date.")).small().weak());
    }
    ui.separator();
}
