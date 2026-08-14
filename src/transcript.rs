//! Session events -> render cells.
//! Wire shapes are the durable dsh-session vocabulary (verified against a
//! live 0.1.0-rc.6 session log):
//!   assistant/chunk: block-start / text-delta / block-end / usage / finish
//!   assistant/message: data.message.content[]
//! Full taxonomy follows docs/01-grok-tui-spec.md section 4.

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    User,
    Assistant,
    Thinking,
    Tool,
    ToolResult,
    Notice,
    Subagent,
}

#[derive(Debug, Clone)]
pub struct Cell {
    pub id: u64,
    pub kind: CellKind,
    pub title: String,
    pub text: String,
    pub folded: bool,
    pub failed: bool,
}

/// One user turn for the rewind picker: its durable seq plus the render cell
/// that starts it (truncation point for display-level rollback).
#[derive(Debug, Clone, Copy)]
pub struct TurnMarker {
    pub seq: u64,
    pub cell: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone, Copy)]
struct BlockState {
    kind: BlockKind,
    cell: usize,
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub cells: Vec<Cell>,
    pub selected: Option<usize>,
    pub usage: Usage,
    pub turns: Vec<TurnMarker>,
    next_id: u64,
    blocks: HashMap<u64, BlockState>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub(crate) fn push(&mut self, kind: CellKind, title: impl Into<String>, text: impl Into<String>) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.cells.push(Cell {
            id,
            kind,
            title: title.into(),
            text: text.into(),
            folded: kind == CellKind::Thinking,
            failed: false,
        });
        self.cells.len() - 1
    }

    pub fn apply(&mut self, event: &Value) {
        let Some(ty) = event.get("type").and_then(Value::as_str) else { return };
        let data = event.get("data");
        match ty {
            "user/message" => {
                // Only real user prompts belong on the chat surface; plugin /
                // runtime-context injections (system-prompt snapshots, skills
                // catalogs) are model-visible but not user-visible.
                let kind = data
                    .and_then(|d| d.get("source"))
                    .and_then(|s| s.get("kind"))
                    .and_then(Value::as_str);
                if kind != Some("user") {
                    return;
                }
                let text = text_blocks(data);
                if !text.is_empty() {
                    let i = self.push(CellKind::User, String::new(), text);
                    if let Some(seq) = event.get("seq").and_then(Value::as_u64) {
                        self.turns.push(TurnMarker { seq, cell: i });
                    }
                    self.selected = Some(i);
                }
            }
            "assistant/chunk" => {
                let Some(chunk) = data.and_then(|d| d.get("chunk")) else { return };
                let ctype = chunk.get("type").and_then(Value::as_str);
                let index = chunk.get("index").and_then(Value::as_u64);
                match ctype {
                    Some("block-start") => {
                        let block_type = chunk.get("blockType").and_then(Value::as_str);
                        let kind = if block_type == Some("reasoning") || block_type == Some("thinking") {
                            BlockKind::Thinking
                        } else {
                            BlockKind::Text
                        };
                        let (cell_kind, title) = match kind {
                            BlockKind::Thinking => (CellKind::Thinking, "Thinking".to_string()),
                            BlockKind::Text => (CellKind::Assistant, String::new()),
                        };
                        let cell = self.push(cell_kind, title, String::new());
                        if let Some(idx) = index {
                            self.blocks.insert(idx, BlockState { kind, cell });
                        }
                        self.selected = Some(cell);
                    }
                    Some("text-delta") => {
                        if let Some(t) = chunk.get("text").and_then(Value::as_str) {
                            let target = index
                                .and_then(|i| self.blocks.get(&i))
                                .map(|b| b.cell)
                                .or_else(|| {
                                    self.cells
                                        .iter()
                                        .rposition(|c| c.kind == CellKind::Assistant)
                                });
                            if let Some(i) = target {
                                self.cells[i].text.push_str(t);
                                self.selected = Some(i);
                            }
                        }
                    }
                    Some("block-end") => {
                        if let Some(idx) = index {
                            if let Some(bs) = self.blocks.remove(&idx) {
                                if bs.kind == BlockKind::Text {
                                    self.cells[bs.cell].folded = false;
                                }
                                self.selected = Some(bs.cell);
                            }
                        }
                    }
                    Some("usage") => {
                        if let Some(u) = chunk.get("usage") {
                            self.usage.input = u.get("inputTokens").and_then(Value::as_u64).unwrap_or(self.usage.input);
                            self.usage.output = u.get("outputTokens").and_then(Value::as_u64).unwrap_or(self.usage.output);
                            self.usage.cache = u.get("cacheReadTokens").and_then(Value::as_u64).unwrap_or(self.usage.cache);
                        }
                    }
                    _ => {}
                }
            }
            "assistant/message" => {
                self.blocks.clear();
                // Deltas already built the cells; the committed message is the
                // fallback for providers that skip deltas.
                if let Some(content) = data
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                {
                    if let Some(text) = content_text(content) {
                        if !self.cells.iter().any(|c| c.kind == CellKind::Assistant && !c.text.is_empty()) {
                            let i = self.push(CellKind::Assistant, String::new(), text);
                            self.selected = Some(i);
                        }
                    }
                }
            }
            "tool/call" => {
                let name = data
                    .and_then(|d| d.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = data
                    .and_then(|d| d.get("input"))
                    .map(summarize)
                    .or_else(|| data.and_then(|d| d.get("arguments")).map(summarize))
                    .unwrap_or_default();
                let i = self.push(CellKind::Tool, name, input);
                self.cells[i].folded = true;
                self.selected = Some(i);
            }
            "tool/result" => {
                let failed = data
                    .and_then(|d| d.get("status"))
                    .and_then(Value::as_str)
                    == Some("failed");
                let text = data.and_then(|d| d.get("result")).map(summarize).unwrap_or_default();
                let i = self.push(CellKind::ToolResult, String::new(), text);
                self.cells[i].failed = failed;
                self.selected = Some(i);
            }
            _ => {}
        }
    }

    pub fn toggle_fold(&mut self, idx: usize) {
        if let Some(c) = self.cells.get_mut(idx) {
            c.folded = !c.folded;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.cells.is_empty() {
            self.selected = None;
            return;
        }
        let n = self.cells.len() as isize;
        let cur = self.selected.map(|s| s as isize).unwrap_or(n - 1);
        let next = (cur + delta).clamp(0, n - 1) as usize;
        self.selected = Some(next);
    }
}

fn text_blocks(data: Option<&Value>) -> String {
    let Some(data) = data else { return String::new() };
    let Some(content) = data.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    content_text(content).unwrap_or_default()
}

fn content_text(content: &[Value]) -> Option<String> {
    let mut out = String::new();
    for b in content {
        if b.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = b.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn summarize(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > 400 {
        let head: String = one_line.chars().take(400).collect();
        format!("{} …", head)
    } else {
        one_line
    }
}
