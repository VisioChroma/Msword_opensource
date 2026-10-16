//! Reusable document components (Insert › Text › Quick Parts): headers, footers, callout boxes,
//! signature blocks and branded sections saved once and reused in any document.
//!
//! The library lives on the [`Session`], so it follows the person from document to document,
//! and the desktop app keeps it in a file ([`Library::load`]). A component is the content of a
//! selection (which may be inside a header or footer). Inserting one can leave it *linked*: the
//! copy is wrapped in a rich text content control tagged `wc-component:<rev>:<name>` (`w:sdt`,
//! so it survives .docx) and Refresh rewrites every linked copy that is behind its source.
//!
//! Commands: `component.save`, `component.insert`, `component.sync`, `component.unlink`,
//! `component.delete`, `component.list`, `view.componentsPane`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wordcraft_doc::control::{ControlKind, ControlRange};
use wordcraft_doc::edit::Fragment;
use wordcraft_doc::para::OBJ;
use wordcraft_doc::{Block, Document, Pos, StoryRef};

use super::delete_selection;
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session, p};

/// What a component is for. Only a label for the pane: any content can be saved as any kind.
pub const KINDS: [&str; 6] = ["header", "footer", "callout", "signature", "branded", "general"];
/// Start of the tag of a linked copy: `wc-component:<rev>:<name>`.
const TAG_PREFIX: &str = "wc-component:";
const MAX_NAME: usize = 100;
const MAX_COMPONENTS: usize = 500;
/// Largest component (as JSON) and largest library file read.
const MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Most linked copies one Refresh looks at.
const MAX_LINKED: usize = 1000;

/// One saved component.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Component {
    pub kind: String,
    /// Starts at 1 and goes up each time the source is replaced; linked copies remember theirs.
    pub rev: u32,
    pub frag: Fragment,
}

/// The saved components by name.
#[derive(Default)]
pub struct Library {
    pub items: BTreeMap<String, Component>,
    /// The file the desktop app keeps the library in (`None`: headless sessions, tests, web).
    pub path: Option<PathBuf>,
}

impl Library {
    /// Read the library from `path` and keep saving to it. A missing or damaged file leaves the
    /// library empty (it is replaced the next time something is saved).
    pub fn load(&mut self, path: PathBuf) {
        let read = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.len() <= MAX_FILE_BYTES)
            .and_then(|_| std::fs::read(&path).ok())
            .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, Component>>(&bytes).ok());
        if let Some(items) = read {
            self.items = items
                .into_iter()
                .filter(|(name, c)| !name.is_empty() && name.chars().count() <= MAX_NAME && KINDS.contains(&c.kind.as_str()))
                .take(MAX_COMPONENTS)
                .collect();
        }
        self.path = Some(path);
    }

    /// Write the library to its file, if the host wired one up. A failed save keeps the
    /// components in memory and never fails the command.
    fn persist(&self) {
        let Some(path) = self.path.as_deref() else { return };
        let saved = serde_json::to_vec(&self.items).map_err(|e| e.to_string()).and_then(|bytes| {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| e.to_string())
        });
        if let Err(e) = saved {
            log::warn!("components: save failed: {e}");
        }
    }
}

fn tag_for(name: &str, rev: u32) -> String {
    format!("{TAG_PREFIX}{rev}:{name}")
}

/// `(rev, name)` from a linked copy's tag.
fn parse_tag(tag: &str) -> Option<(u32, &str)> {
    let (rev, name) = tag.strip_prefix(TAG_PREFIX)?.split_once(':')?;
    Some((rev.parse().ok()?, name))
}

fn name_param(v: &Value) -> Result<String, CmdError> {
    let raw = p::str(v, "name").ok_or_else(|| CmdError::Params("`name` required".into()))?;
    let name: String = raw.chars().filter(|c| !c.is_control() && *c != OBJ).take(MAX_NAME).collect();
    let name = name.trim().to_string();
    if name.is_empty() { Err(CmdError::Params("`name` is empty".into())) } else { Ok(name) }
}

/// A linked copy in the document.
pub struct Linked {
    pub name: String,
    pub rev: u32,
    /// Where its content control starts.
    pub start: Pos,
}

/// Every linked copy in the document (body, headers, footers, text boxes), in no set order.
pub fn linked_copies(s: &Session) -> Vec<Linked> {
    let mut out = Vec::new();
    let stories = std::iter::once(StoryRef::Body).chain(s.doc.parts.keys().map(|k| StoryRef::Part(*k)));
    for story in stories {
        for r in s.doc.control_ranges(story) {
            if let Some((rev, name)) = parse_tag(&r.control.tag) {
                out.push(Linked { name: name.to_string(), rev, start: r.start.clone() });
                if out.len() >= MAX_LINKED {
                    return out;
                }
            }
        }
    }
    out
}

/// The innermost linked copy the caret is in.
fn caret_linked(s: &Session) -> Option<ControlRange> {
    let f = &s.sel.focus;
    // Cheap test first: most carets are in no content control at all.
    s.doc.control_at(f)?;
    s.doc.control_ranges(f.story).into_iter().filter(|r| r.contains(f) && parse_tag(&r.control.tag).is_some()).max_by(|a, b| a.start.cmp(&b.start))
}

/// The name and revision of the linked copy the caret is in (the pane shows it).
pub fn caret_linked_copy(s: &Session) -> Option<(String, u32)> {
    let r = caret_linked(s)?;
    let (rev, name) = parse_tag(&r.control.tag)?;
    Some((name.to_string(), rev))
}

/// Does the fragment hold a linked copy? (Saving that would nest one component in another.)
fn holds_linked_copy(frag: &Fragment) -> bool {
    let mut d = Document::new();
    d.body = frag.blocks.iter().cloned().map(Arc::new).collect();
    d.control_ranges(StoryRef::Body).iter().any(|r| parse_tag(&r.control.tag).is_some())
}

fn save(s: &mut Session, v: &Value) -> CmdResult {
    let name = name_param(v)?;
    let update = p::bool(v, "update").unwrap_or(false);
    let old = s.components.items.get(&name);
    if old.is_some() && !update {
        return Err(CmdError::Failed(format!("a component called {name:?} already exists; save with `update` to replace its source")));
    }
    if old.is_none() && s.components.items.len() >= MAX_COMPONENTS {
        return Err(CmdError::Failed(format!("the library holds {MAX_COMPONENTS} components at most")));
    }
    let kind = match p::str(v, "kind") {
        Some(k) => KINDS
            .iter()
            .copied()
            .find(|x| *x == k)
            .map(str::to_string)
            .ok_or_else(|| CmdError::Params(format!("`kind` is one of {}", KINDS.join(", "))))?,
        None => old.map_or_else(|| "general".to_string(), |c| c.kind.clone()),
    };
    let rev = old.map_or(1, |c| c.rev.saturating_add(1));
    let (a, b) = s.sel.ordered();
    if a == b {
        return Err(CmdError::Params("select the content to save".into()));
    }
    let frag = s.doc.copy_range(&a, &b);
    if frag.blocks.is_empty() {
        return Err(CmdError::Failed("nothing to save in the selection".into()));
    }
    if holds_linked_copy(&frag) {
        return Err(CmdError::Failed("select the content inside the linked component, not the component itself".into()));
    }
    if !serde_json::to_vec(&frag).is_ok_and(|bytes| bytes.len() <= MAX_COMPONENT_BYTES) {
        return Err(CmdError::Failed("the selection is too large to save as a component".into()));
    }
    s.components.items.insert(name.clone(), Component { kind: kind.clone(), rev, frag });
    s.components.persist();
    Ok(json!({"name": name, "kind": kind, "rev": rev}))
}

fn delete(s: &mut Session, v: &Value) -> CmdResult {
    let name = name_param(v)?;
    if s.components.items.remove(&name).is_none() {
        return Err(CmdError::Params(format!("no component called {name:?}")));
    }
    s.components.persist();
    Ok(json!({"deleted": name}))
}

fn insert(s: &mut Session, v: &Value) -> CmdResult {
    let name = name_param(v)?;
    let keep_link = p::bool(v, "linked").unwrap_or(false);
    let comp = s.components.items.get(&name).cloned().ok_or_else(|| CmdError::Params(format!("no component called {name:?}")))?;
    let at = delete_selection(s)?;
    // Whole paragraphs are wrapped by a block-level control, so a linked copy of several
    // paragraphs (or a table) needs a paragraph of its own, or text beside it would be wrapped.
    let several = comp.frag.blocks.len() > 1 || comp.frag.blocks.iter().any(|b| matches!(b, Block::Table(_)));
    if keep_link && several && s.doc.para_at(&at).is_some_and(|q| !q.is_empty()) {
        return Err(CmdError::Disabled("a linked component with several paragraphs goes in an empty paragraph".into()));
    }
    let end = s.doc.insert_fragment(&at, &comp.frag)?;
    if !keep_link {
        s.sel = Selection::caret(end);
        return Ok(json!({"name": name, "linked": false, "rev": comp.rev}));
    }
    // A rich text content control around what was inserted, tagged with the source and revision.
    s.sel = Selection { anchor: at, focus: end };
    super::controls::insert(s, &json!({"title": name, "tag": tag_for(&name, comp.rev)}), ControlKind::RichText)?;
    // The caret goes after the component, as after any inserted content.
    let w = OBJ.len_utf8();
    if let Some(r) = s.doc.control_at(&s.sel.focus) {
        s.sel = Selection::caret(Pos { off: r.end.off + w, ..r.end.clone() });
    }
    Ok(json!({"name": name, "linked": true, "rev": comp.rev}))
}

/// Rewrite the linked copies that are behind their source (all of them with `force`).
fn sync(s: &mut Session, v: &Value) -> CmdResult {
    let only = p::str(v, "name");
    let force = p::bool(v, "force").unwrap_or(false);
    let mut todo = linked_copies(s);
    // Last first, so the positions of the copies still to do stay valid.
    todo.sort_by(|a, b| b.start.cmp(&a.start));
    let (mut updated, mut locked, mut missing) = (0usize, 0usize, 0usize);
    for l in todo {
        if only.is_some_and(|n| n != l.name) {
            continue;
        }
        let Some(comp) = s.components.items.get(&l.name) else {
            missing += 1;
            continue;
        };
        if l.rev >= comp.rev && !force {
            continue;
        }
        let (frag, rev) = (comp.frag.clone(), comp.rev);
        let Some(r) = s.doc.control_ranges(l.start.story).into_iter().find(|r| r.start == l.start) else { continue };
        if !r.control.lock.can_edit() {
            locked += 1;
            continue;
        }
        let a = r.content_start();
        s.doc.delete_range(&a, &r.end)?;
        s.doc.insert_fragment(&a, &frag)?;
        if let Some(c) = s.doc.control_mut(&r.start) {
            c.tag = tag_for(&l.name, rev);
            c.showing_placeholder = false;
        }
        updated += 1;
    }
    Ok(json!({"updated": updated, "locked": locked, "sourceMissing": missing}))
}

/// Remove the content control of the linked copy at the caret; its content stays as it is.
fn unlink(s: &mut Session, _: &Value) -> CmdResult {
    let r = caret_linked(s).ok_or_else(|| CmdError::Disabled("put the cursor in a linked component first".into()))?;
    if !r.control.lock.can_delete() {
        return Err(CmdError::Failed("this component's content control can't be deleted".into()));
    }
    let w = OBJ.len_utf8();
    s.doc.para_mut(r.end.story, &r.end.path)?.delete(r.end.off, r.end.off + w)?;
    s.doc.para_mut(r.start.story, &r.start.path)?.delete(r.start.off, r.start.off + w)?;
    s.sel = Selection::caret(s.doc.clamp(&r.start));
    Ok(json!({"unlinked": true}))
}

fn can_unlink(s: &Session) -> Option<&'static str> {
    if caret_linked(s).is_some() { None } else { Some("put the cursor in a linked component first") }
}

fn list(s: &mut Session, _: &Value) -> CmdResult {
    let components: Vec<Value> = s
        .components
        .items
        .iter()
        .map(|(name, c)| {
            let text: String = c.frag.plain_text().chars().take(80).collect();
            json!({"name": name, "kind": c.kind, "rev": c.rev, "text": text})
        })
        .collect();
    let copies: Vec<Value> = linked_copies(s)
        .into_iter()
        .map(|l| {
            let behind = s.components.items.get(&l.name).map(|c| c.rev > l.rev);
            json!({"name": l.name, "rev": l.rev, "behind": behind, "pos": super::pos_json(&l.start)})
        })
        .collect();
    Ok(json!({"components": components, "linked": copies}))
}

pub fn specs() -> Vec<CommandSpec> {
    // Labels and location reuse strings every language catalog already translates.
    const LOC: &str = "Insert › Text › Quick Parts";
    vec![
        CommandSpec::new("component.save", "Create Source", LOC, save)
            .params(r#"{"name": string, "kind"?: "header"|"footer"|"callout"|"signature"|"branded"|"general", "update"?: bool (replace the source of an existing component)}  (saves the selection)"#)
            .pure(),
        CommandSpec::new("component.insert", "Quick Parts", LOC, insert)
            .params(r#"{"name": string, "linked"?: bool (keep the copy linked to its source)}"#),
        CommandSpec::new("component.sync", "Refresh", LOC, sync)
            .params(r#"{"name"?: string (only copies of this component), "force"?: bool (also copies that are up to date)} → {"updated", "locked", "sourceMissing"}"#),
        CommandSpec::new("component.unlink", "Break Link", LOC, unlink)
            .params(r#"{}  (the linked component at the cursor becomes ordinary content)"#)
            .when(can_unlink),
        CommandSpec::new("component.delete", "Delete", LOC, delete).params(r#"{"name": string}"#).pure(),
        CommandSpec::new("component.list", "Quick Parts", LOC, list)
            .params(r#"{} → {"components": [{name, kind, rev, text}], "linked": [{name, rev, behind, pos}]}"#)
            .pure(),
        CommandSpec::new("view.componentsPane", "Quick Parts", LOC, |s, v| {
            let on = v.get("value").and_then(Value::as_bool).unwrap_or(!s.view.components_pane);
            s.view.components_pane = on;
            Ok(json!({"value": on}))
        })
        .params(r#"{"value"?: bool}"#)
        .pure(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_copy_follows_its_source() {
        let mut s = Session::new(Document::from_text("Brand"));
        s.run("select.all", &json!({})).unwrap();
        s.run("component.save", &json!({"name": "Brand", "kind": "branded"})).unwrap();
        assert!(s.run("component.save", &json!({"name": "Brand"})).is_err(), "replacing needs `update`");
        s.run("caret.docEnd", &json!({})).unwrap();
        s.run("component.insert", &json!({"name": "Brand", "linked": true})).unwrap();
        assert_eq!(linked_copies(&s).len(), 1);

        // The source changes; the copy is behind until Refresh.
        let c = s.components.items.get_mut("Brand").unwrap();
        c.frag = Fragment::from_text("Logo v2");
        c.rev = 2;
        let r = s.run("component.sync", &json!({})).unwrap();
        assert_eq!(r["updated"], 1);
        assert!(s.doc.plain_text(StoryRef::Body).contains("Logo v2"));
        assert_eq!(linked_copies(&s)[0].rev, 2);

        // Break Link keeps the content and drops the link.
        let at = linked_copies(&s)[0].start.clone();
        s.sel = Selection::caret(Pos { off: at.off + OBJ.len_utf8(), ..at });
        s.run("component.unlink", &json!({})).unwrap();
        assert!(linked_copies(&s).is_empty());
        assert!(s.doc.plain_text(StoryRef::Body).contains("Logo v2"));
    }
}
