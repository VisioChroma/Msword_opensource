//! The Objects pane: the pictures, shapes, text boxes and groups of the body as a list with
//! names and lock state, and the commands its rows use: rename one, lock or unlock one by
//! position, open and close the pane.
//!
//! Commands: `object.list`, `object.rename`, `object.setLocked`, `view.objectsPane`.

use std::collections::HashMap;

use serde_json::{Value, json};
use wordcraft_doc::para::{InlineObject, ShapeKind};
use wordcraft_doc::{Pos, StoryRef};

use crate::{CmdError, CmdResult, CommandSpec, Session, p};

/// Longest object name kept (characters).
const MAX_NAME: usize = 255;

/// "TextBox" as "Text Box".
fn words(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// What kind of object this is, as the pane names it.
fn kind_name(o: &InlineObject) -> String {
    match o {
        InlineObject::Image { .. } => "Picture".into(),
        InlineObject::Graphic { graphic, .. } => words(&format!("{:?}", graphic.kind)),
        InlineObject::Shape { kind, .. } => words(&format!("{kind:?}")),
        InlineObject::Group { canvas: Some(_), .. } => "Drawing Canvas".into(),
        InlineObject::Group { .. } => "Group".into(),
        _ => "Object".into(),
    }
}

fn kind_id(o: &InlineObject) -> &'static str {
    match o {
        InlineObject::Image { .. } => "picture",
        InlineObject::Graphic { .. } => "graphic",
        InlineObject::Shape { kind: ShapeKind::TextBox, .. } => "textBox",
        InlineObject::Shape { .. } => "shape",
        InlineObject::Group { .. } => "group",
        _ => "object",
    }
}

/// The drawings of the body in document order (the same order `select.objects` counts in).
fn objects(s: &Session) -> Vec<(Pos, InlineObject)> {
    let mut v = Vec::new();
    for path in s.doc.para_paths(StoryRef::Body) {
        let Some(para) = s.doc.para(StoryRef::Body, &path) else { continue };
        for off in para.object_offsets() {
            if let Some(o) = para.object_at(off).filter(|o| o.is_drawing()) {
                v.push((Pos { story: StoryRef::Body, path: path.clone(), off }, o.clone()));
            }
        }
    }
    v
}

fn list(s: &mut Session, _: &Value) -> CmdResult {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let rows = objects(s)
        .into_iter()
        .enumerate()
        .map(|(i, (pos, o))| {
            let label = kind_name(&o);
            let n = counts.entry(label.clone()).or_insert(0);
            *n += 1;
            let custom = o.frame().map(|(_, _, f)| f.name.clone()).unwrap_or_default();
            let name = if custom.is_empty() { format!("{label} {n}") } else { custom.clone() };
            let members = match &o {
                InlineObject::Group { children, .. } => children.len(),
                _ => 0,
            };
            json!({
                "index": i,
                "kind": kind_id(&o),
                "name": name,
                "custom": !custom.is_empty(),
                "pos": super::pos_json(&pos),
                "locked": o.is_locked(),
                "hidden": o.frame().is_some_and(|(_, _, f)| f.hidden),
                "members": members,
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

/// The drawing `v["pos"]` names.
fn object_pos(s: &Session, v: &Value) -> Result<Pos, CmdError> {
    let pos = v.get("pos").and_then(super::parse_pos).ok_or_else(|| CmdError::Params("`pos` required".into()))?;
    if s.doc.para_at(&pos).and_then(|q| q.object_at(pos.off)).is_some_and(InlineObject::is_drawing) {
        Ok(pos)
    } else {
        Err(CmdError::Params("no picture or shape there".into()))
    }
}

fn rename(s: &mut Session, v: &Value) -> CmdResult {
    let pos = object_pos(s, v)?;
    let name: String = p::str(v, "name").unwrap_or("").trim().chars().take(MAX_NAME).collect();
    let para = s.doc.para_mut(pos.story, &pos.path)?;
    if let Some(float) = para.object_at_mut(pos.off).and_then(InlineObject::float_mut) {
        float.name = name.clone();
    }
    para.touch();
    Ok(json!({"name": name}))
}

fn set_locked(s: &mut Session, v: &Value) -> CmdResult {
    let pos = object_pos(s, v)?;
    let lock = v.get("locked").and_then(Value::as_bool).ok_or_else(|| CmdError::Params("`locked` (true or false) required".into()))?;
    let para = s.doc.para_mut(pos.story, &pos.path)?;
    if let Some(float) = para.object_at_mut(pos.off).and_then(InlineObject::float_mut) {
        float.locked = lock;
    }
    para.touch();
    Ok(json!({"locked": lock}))
}
fn set_hidden(s: &mut Session, v: &Value) -> CmdResult {
    let pos = object_pos(s, v)?;
    let hide = v.get("hidden").and_then(Value::as_bool).ok_or_else(|| CmdError::Params("`hidden` (true or false) required".into()))?;
    let para = s.doc.para_mut(pos.story, &pos.path)?;
    if let Some(float) = para.object_at_mut(pos.off).and_then(InlineObject::float_mut) {
        float.hidden = hide;
    }
    para.touch();
    Ok(json!({"hidden": hide}))
}
pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("object.list", "Selection Pane", "Layout › Arrange", list)
            .params(r#"{}  (every picture, shape, text box and group: index, kind, name, pos, locked)"#)
            .pure(),
        CommandSpec::new("object.rename", "Selection Pane", "Layout › Arrange", rename)
            .params(r#"{"pos": Pos, "name": text}  (an empty name goes back to the automatic one)"#),
        CommandSpec::new("object.setLocked", "Lock Object", "Layout › Arrange", set_locked)
            .params(r#"{"pos": Pos, "locked": bool}  (lock or unlock the object at pos, selected or not)"#),
        CommandSpec::new("object.setHidden", "Hide Object", "Layout › Arrange", set_hidden)
            .params(r#"{"pos": Pos, "hidden": bool}  (hide or show the object at pos)"#),
        CommandSpec::new("view.objectsPane", "Selection Pane", "Layout › Arrange", |s, v| {
            let on = v.get("value").and_then(Value::as_bool).unwrap_or(!s.view.objects_pane);
            s.view.objects_pane = on;
            Ok(json!({"value": on}))
        })
        .params(r#"{"value"?: bool}"#)
        .pure(),
    ]
}
