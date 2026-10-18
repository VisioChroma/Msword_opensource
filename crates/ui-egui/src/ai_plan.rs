//! "Word" AI, part 1: turn a plain-language goal into a document.
//!
//! The model never touches the document. It returns a JSON *plan*; this file checks it, then
//! builds the document through ordinary engine commands, as one Undo step.

use serde_json::{Value, json};
use wordcraft_engine::Session;

const MAX_BLOCKS: usize = 400;
const MAX_ITEMS: usize = 60;
const MAX_ROWS: usize = 60;
const MAX_COLS: usize = 12;

pub const SYSTEM_PROMPT: &str = r#"You are Word, the writing assistant inside the WordCraft document editor.
The user describes a document in plain language. Decide the document type, the audience, the
structure, the depth and the format yourself, then write the complete document.

Reply with ONE JSON object and nothing else (no markdown fences, no commentary):
{
  "document_type": "short label, e.g. briefing memo",
  "audience": "who it is for",
  "summary": "one sentence: what you made and how long it is",
  "blocks": [
    {"type": "title", "text": "..."},
    {"type": "heading", "level": 1, "text": "..."},
    {"type": "paragraph", "text": "..."},
    {"type": "bullets", "items": ["...", "..."]},
    {"type": "numbered", "items": ["...", "..."]},
    {"type": "table", "header": ["Col A", "Col B"], "rows": [["a", "b"]]},
    {"type": "page_break"}
  ]
}

Rules:
- Start with exactly one "title" block. Use heading levels 1-3 only, in order.
- Match depth to the request: a meeting brief is 1-2 pages, a report or paper is longer. Write full, useful paragraphs, not placeholders.
- Plain text only inside "text" and "items": no markdown symbols, no HTML, no numbering typed by hand.
- Never invent statistics, quotations, citations, URLs or named studies. If a fact needs a source, state the claim in general terms and add "[source needed]" after it.
- Use a table only when the content is genuinely tabular, and keep it under 8 columns.
- Write in the language the user wrote their request in."#;

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub document_type: String,
    pub audience: String,
    pub summary: String,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Title(String),
    Heading(u8, String),
    Paragraph(String),
    Bullets(Vec<String>),
    Numbered(Vec<String>),
    Table { header: Vec<String>, rows: Vec<Vec<String>> },
    PageBreak,
}

fn text_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).take(MAX_ITEMS).collect())
        .unwrap_or_default()
}

/// The JSON object inside a model reply (tolerates code fences and chatter around it).
pub fn extract_json(reply: &str) -> Option<Value> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    serde_json::from_str(reply.get(start..=end)?).ok()
}

/// Check and bound a reply.
pub fn parse_plan(reply: &str) -> Result<Plan, String> {
    let v = extract_json(reply).ok_or("the reply was not a valid document plan")?;
    let raw = v.get("blocks").and_then(Value::as_array).ok_or("the plan has no blocks")?;
    let mut blocks = Vec::new();
    for b in raw.iter().take(MAX_BLOCKS) {
        let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "title" => {
                let t = text_of(b, "text");
                if !t.is_empty() {
                    blocks.push(Block::Title(t));
                }
            }
            "heading" => {
                let t = text_of(b, "text");
                let level = b.get("level").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3) as u8;
                if !t.is_empty() {
                    blocks.push(Block::Heading(level, t));
                }
            }
            "paragraph" => {
                let t = text_of(b, "text");
                if !t.is_empty() {
                    blocks.push(Block::Paragraph(t));
                }
            }
            "bullets" | "numbered" => {
                let items = strings(b, "items");
                if !items.is_empty() {
                    blocks.push(if kind == "bullets" { Block::Bullets(items) } else { Block::Numbered(items) });
                }
            }
            "table" => {
                let mut header = strings(b, "header");
                header.truncate(MAX_COLS);
                let cols = header.len();
                let rows: Vec<Vec<String>> = b
                    .get("rows")
                    .and_then(Value::as_array)
                    .map(|rs| {
                        rs.iter()
                            .take(MAX_ROWS)
                            .map(|r| {
                                let mut row: Vec<String> = r
                                    .as_array()
                                    .map(|c| c.iter().take(cols).map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect())
                                    .unwrap_or_default();
                                row.resize(cols, String::new());
                                row
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if cols > 0 && !rows.is_empty() {
                    blocks.push(Block::Table { header, rows });
                }
            }
            "page_break" => blocks.push(Block::PageBreak),
            _ => {}
        }
    }
    if blocks.is_empty() {
        return Err("the plan had nothing usable in it".into());
    }
    Ok(Plan { document_type: text_of(&v, "document_type"), audience: text_of(&v, "audience"), summary: text_of(&v, "summary"), blocks })
}

/// Runs engine commands, joining every edit after the first into one Undo step.
struct Builder<'a> {
    s: &'a mut Session,
    first: bool,
    /// The caret paragraph is empty and unused, so the next block goes into it.
    fresh: bool,
}

impl Builder<'_> {
    fn edit(&mut self, id: &str, params: Value) -> Result<(), String> {
        if !self.first {
            self.s.join_next_undo();
        }
        self.first = false;
        self.s.run(id, &params).map(|_| ()).map_err(|e| e.to_string())
    }

    fn para(&mut self, style: &str, text: &str) -> Result<(), String> {
        if !self.fresh {
            self.edit("text.newParagraph", json!({}))?;
        }
        self.edit("para.style", json!({"style": style}))?;
        self.edit("text.insert", json!({"text": text, "raw": true}))?;
        self.fresh = false;
        Ok(())
    }

    fn list(&mut self, items: &[String], command: &str) -> Result<(), String> {
        for item in items {
            self.para("Normal", item)?;
            // The same kind of list under an existing one continues it.
            self.edit(command, json!({}))?;
        }
        Ok(())
    }

    fn table(&mut self, header: &[String], rows: &[Vec<String>]) -> Result<(), String> {
        if !self.fresh {
            self.edit("text.newParagraph", json!({}))?;
            self.edit("para.style", json!({"style": "Normal"}))?;
        }
        let cols = header.len();
        self.edit("insert.table", json!({"rows": rows.len() + 1, "cols": cols}))?;
        let cells = header.iter().chain(rows.iter().flatten()).collect::<Vec<_>>();
        for (i, cell) in cells.into_iter().enumerate() {
            // Tab selects the next cell's content, so typing replaces it.
            if i > 0 {
                self.edit("text.tab", json!({}))?;
            }
            if !cell.is_empty() {
                self.edit("text.insert", json!({"text": cell, "raw": true}))?;
                if i < cols {
                    self.edit("select.paragraph", json!({}))?;
                    self.edit("format.bold", json!({}))?;
                }
            }
        }
        // Word keeps an empty paragraph after a table: carry on there.
        self.s.run("caret.docEnd", &json!({})).map_err(|e| e.to_string())?;
        self.fresh = true;
        Ok(())
    }
}

fn build(b: &mut Builder, plan: &Plan) -> Result<usize, String> {
    let mut done = 0;
    for block in &plan.blocks {
        match block {
            Block::Title(t) => b.para("Title", t)?,
            Block::Heading(n, t) => b.para(&format!("Heading{n}"), t)?,
            Block::Paragraph(t) => b.para("Normal", t)?,
            Block::Bullets(items) => b.list(items, "para.bullets")?,
            Block::Numbered(items) => b.list(items, "para.numbering")?,
            Block::Table { header, rows } => b.table(header, rows)?,
            Block::PageBreak => {
                b.edit("text.pageBreak", json!({}))?;
                b.fresh = false;
            }
        }
        done += 1;
    }
    Ok(done)
}

/// Append `plan` at the end of the document as one Undo step. On a failure half-way the
/// partial result is undone. Returns how many blocks went in.
pub fn apply_plan(s: &mut Session, plan: &Plan) -> Result<usize, String> {
    s.run("caret.docEnd", &json!({})).map_err(|e| e.to_string())?;
    let fresh = s.doc.para_at(&s.sel.focus).is_some_and(|p| p.text.is_empty());
    let mut b = Builder { s, first: true, fresh };
    let r = build(&mut b, plan);
    if r.is_err() && !b.first {
        let _ = b.s.run("edit.undo", &json!({}));
    }
    r
}
