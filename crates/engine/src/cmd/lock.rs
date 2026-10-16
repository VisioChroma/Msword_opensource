//! Object locking: a locked picture, shape, text box or group can't be moved, resized, edited
//! or deleted until it is unlocked.
//!
//! Commands: `object.lock`, `object.unlock` (the selected objects). The rule is enforced in
//! `Session::run` the same way Restrict Editing is: after a command ran, every object that was
//! locked must still be there, unchanged (see [`broken`]); if not, the command is undone.

use serde_json::json;
use wordcraft_doc::para::InlineObject;
use wordcraft_doc::{Block, Blocks, Document, Part, Path, Pos, StoryRef, Table};

use super::objects::{picked_objects, selected_all};
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session};

/// Most objects one lock command changes.
const MAX_TARGETS: usize = 10_000;
/// Most objects looked at when only asking whether a button is enabled.
const PEEK: usize = 256;
/// Most locked tables the check keeps track of (hostile files).
const MAX_TABLES: usize = 1000;
/// Tables inside tables are followed this deep.
const MAX_DEPTH: usize = 16;

/// The objects the commands act on: those in the selection, and those added to it (Shift+click).
fn targets(s: &Session, max: usize) -> Vec<Pos> {
    let mut out: Vec<Pos> = selected_all(s, max).into_iter().map(|(pos, _)| pos).collect();
    for pos in picked_objects(s) {
        if !out.contains(&pos) {
            out.push(pos);
        }
    }
    out
}

fn locked_at(s: &Session, pos: &Pos) -> Option<bool> {
    s.doc.para_at(pos)?.object_at(pos.off).map(InlineObject::is_locked)
}

/// Is any selected object locked? (The ribbon shows Unlock instead of Lock.)
pub fn selection_locked(s: &Session) -> bool {
    targets(s, PEEK).iter().any(|p| locked_at(s, p) == Some(true))
}

fn can_lock(s: &Session) -> Option<&'static str> {
    if targets(s, PEEK).iter().any(|p| locked_at(s, p) == Some(false)) { None } else { Some("select a picture, shape or text box that isn't locked") }
}

fn can_unlock(s: &Session) -> Option<&'static str> {
    if selection_locked(s) { None } else { Some("select a locked picture, shape or text box first") }
}

fn set_locked(s: &mut Session, lock: bool) -> CmdResult {
    let list = targets(s, MAX_TARGETS);
    if list.is_empty() {
        return Err(CmdError::Disabled("select a picture, shape or text box first".into()));
    }
    let mut changed = 0usize;
    for pos in &list {
        let para = s.doc.para_mut(pos.story, &pos.path)?;
        let Some(float) = para.object_at_mut(pos.off).and_then(InlineObject::float_mut) else { continue };
        if float.locked == lock {
            continue;
        }
        float.locked = lock;
        changed += 1;
        para.touch();
    }
    Ok(json!({"locked": lock, "changed": changed}))
}
fn can_toggle_lock(s: &Session) -> Option<&'static str> {
    if can_lock(s).is_none() || can_unlock(s).is_none() {
        None
    } else {
        Some("select a picture, shape or text box first")
    }
}

fn toggle_lock(s: &mut Session) -> CmdResult {
    let lock = !selection_locked(s);
    set_locked(s, lock)
}
/// The table the table commands act on: the one whose whole cells are selected (Select Table,
/// Select Row…), else the one the caret is in, else the one the selection started in.
fn target_table(s: &Session) -> Option<(StoryRef, Path)> {
    if let Some(r) = s.cell_range() {
        return Some((r.story, r.table.clone()));
    }
    if let Some((tp, _, _)) = s.sel.focus.path.cell() {
        return Some((s.sel.focus.story, tp));
    }
    let (tp, _, _) = s.sel.anchor.path.cell()?;
    Some((s.sel.anchor.story, tp))
}

/// Is the target table locked? `None` when there is no table to act on.
fn table_locked(s: &Session) -> Option<bool> {
    let (story, tp) = target_table(s)?;
    s.doc.table(story, &tp).map(|t| t.props.locked)
}

/// Is the target table locked? (The Table Layout button shows Unlock instead of Lock.)
pub fn caret_table_locked(s: &Session) -> bool {
    table_locked(s) == Some(true)
}

fn can_lock_table(s: &Session) -> Option<&'static str> {
    match table_locked(s) {
        Some(false) => None,
        Some(true) => Some("this table is already locked"),
        None => Some("put the cursor in a table first"),
    }
}

fn can_unlock_table(s: &Session) -> Option<&'static str> {
    match table_locked(s) {
        Some(true) => None,
        Some(false) => Some("this table isn't locked"),
        None => Some("put the cursor in a table first"),
    }
}

fn can_toggle_table_lock(s: &Session) -> Option<&'static str> {
    if table_locked(s).is_some() { None } else { Some("put the cursor in a table first") }
}

/// Lock (`Some(true)`), unlock (`Some(false)`) or toggle (`None`) the target table.
fn set_table_locked(s: &mut Session, lock: Option<bool>) -> CmdResult {
    let Some((story, tp)) = target_table(s) else {
        return Err(CmdError::Disabled("put the cursor in a table first".into()));
    };
    let table = s.doc.table_mut(story, &tp)?;
    let value = lock.unwrap_or(!table.props.locked);
    table.props.locked = value;
    Ok(json!({"locked": value}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
                CommandSpec::new(
            "object.toggleLock",
            "Lock Object",
            "Layout › Arrange",
            |s, _| toggle_lock(s),
        )
        .params(r#"{} (toggle locking for the selected objects)"#)
        .key("Mod+Alt+L")
        .when(can_toggle_lock),
        CommandSpec::new("object.lock", "Lock Object", "Layout › Arrange", |s, _| set_locked(s, true))
            .params(r#"{} (the selected pictures, shapes, text boxes and groups)"#)
            .when(can_lock),
        CommandSpec::new("object.unlock", "Unlock Object", "Layout › Arrange", |s, _| set_locked(s, false))
            .params(r#"{} (the selected pictures, shapes, text boxes and groups)"#)
            .when(can_unlock),
                CommandSpec::new("table.toggleLock", "Lock Table", "Table Layout › Table", |s, _| set_table_locked(s, None))
            .params(r#"{} (locks the table the cursor is in, or unlocks it)"#)
            .when(can_toggle_table_lock),
        CommandSpec::new("table.lock", "Lock Table", "Table Layout › Table", |s, _| set_table_locked(s, Some(true)))
            .params(r#"{} (the table the cursor is in)"#)
            .when(can_lock_table),
        CommandSpec::new("table.unlock", "Unlock Table", "Table Layout › Table", |s, _| set_table_locked(s, Some(false)))
            .params(r#"{} (the table the cursor is in)"#)
            .when(can_unlock_table),
    ]
}

/// A locked object as it was: the object, and for a text box the text inside it.
/// A locked object as it was: the object, and the content of every text box in it (text,
/// character and paragraph formatting, alignment and direction).
#[derive(PartialEq)]
struct Locked {
    object: InlineObject,
    parts: Vec<Part>,
}

/// Every locked object in the document, in order.
fn locked_objects(doc: &Document) -> Vec<Locked> {
    let mut out = Vec::new();
    let stories = std::iter::once(StoryRef::Body).chain(doc.parts.keys().map(|k| StoryRef::Part(*k)));
    for story in stories {
        for path in doc.para_paths(story) {
            let Some(p) = doc.para(story, &path) else { continue };
            if p.objects.is_empty() {
                continue;
            }
            for off in p.object_offsets() {
                let Some(o) = p.object_at(off) else { continue };
                if !o.is_locked() {
                    continue;
                }
                                // The content of the text box, or of every text box inside a group.
                let parts: Vec<Part> = o.text_boxes().iter().filter_map(|id| doc.parts.get(id).cloned()).collect();
                out.push(Locked { object: o.clone(), parts });
            }
        }
    }
    out
}

/// Commands that may change a locked object: locking itself, alt text, undo/redo (the person's
/// own history) and file commands (which replace the whole document).
fn exempt(id: &str) -> bool {
    matches!(
        id,
        "object.toggleLock"
            | "object.lock"
            | "object.unlock"
            | "object.setLocked"
            | "object.setHidden"
            | "object.rename"
            | "object.altText"
            | "picture.altText"
            | "table.toggleLock"
            | "table.lock"
            | "table.unlock"
            | "table.altText"
            | "edit.undo"
            | "edit.undoTo"
            | "edit.redo"
    ) || id.starts_with("file.")
}

/// Did command `id` move, resize, edit or delete a locked object? Every object locked before
/// must still be there, unchanged and in the same order (new ones, such as a pasted copy, are
/// fine).
/// Every locked table in the document, in order. A locked table is kept whole (its text,
/// formatting, rows, columns and anything nested in it).
fn locked_tables(doc: &Document) -> Vec<Table> {
    let mut out = Vec::new();
    let stories = std::iter::once(StoryRef::Body).chain(doc.parts.keys().map(|k| StoryRef::Part(*k)));
    for story in stories {
        if let Some(blocks) = doc.story(story) {
            collect_tables(blocks, &mut out, 0);
        }
    }
    out
}

fn collect_tables(blocks: &Blocks, out: &mut Vec<Table>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for b in blocks {
        if out.len() >= MAX_TABLES {
            return;
        }
        let Block::Table(t) = &**b else { continue };
        if t.props.locked {
            out.push(t.clone());
        } else {
            for row in &t.rows {
                for cell in &row.cells {
                    collect_tables(&cell.blocks, out, depth + 1);
                }
            }
        }
    }
}

/// Is a table that was locked missing or changed?
fn tables_broken(before: &Document, after: &Document) -> bool {
    let was = locked_tables(before);
    if was.is_empty() {
        return false;
    }
    let now = locked_tables(after);
    let mut rest = now.iter();
    !was.iter().all(|b| rest.any(|a| a == b))
}

/// Is an object that was locked missing or changed?
fn objects_broken(before: &Document, after: &Document) -> bool {
    let was = locked_objects(before);
    if was.is_empty() {
        return false;
    }
    let now = locked_objects(after);
    let mut rest = now.iter();
    !was.iter().all(|b| rest.any(|a| a == b))
}

/// Did command `id` move, resize, edit or delete a locked object or table? Every one locked
/// before must still be there, unchanged and in the same order (new ones, such as a pasted copy,
/// are fine).
pub(crate) fn broken(id: &str, before: &Document, after: &Document, sel: &Selection) -> bool {
    if exempt(id) {
        return false;
    }
    // Typing at a caret in the body can't reach an object, so skip that scan on every keystroke.
    // Inside a table cell it can change a locked table: only the tables are looked at then.
    if id == "text.insert" && sel.is_collapsed() && sel.focus.story == StoryRef::Body {
        return sel.focus.path.cell().is_some() && tables_broken(before, after);
    }
    objects_broken(before, after) || tables_broken(before, after)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_locked_table_refuses_changes_until_unlocked() {
        let mut s = Session::new(Document::new());
        s.run("insert.table", &json!({"rows": 2, "cols": 2})).unwrap();
        s.run("text.insert", &json!({"text": "A"})).unwrap();
        s.run("table.toggleLock", &json!({})).unwrap();
        assert!(caret_table_locked(&s));
        let before = s.doc.clone();
        assert!(s.run("text.insert", &json!({"text": "x"})).is_err());
        assert!(s.run("table.insertRowBelow", &json!({})).is_err());
        assert!(s.run("table.deleteTable", &json!({})).is_err());
        assert_eq!(s.doc, before, "nothing changed");
        s.run("table.toggleLock", &json!({})).unwrap();
        s.run("text.insert", &json!({"text": "x"})).unwrap();
    }
}