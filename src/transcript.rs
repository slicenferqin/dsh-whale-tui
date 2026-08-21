//! Session events -> render cells.
//! Wire shapes are the durable dsh-session vocabulary (verified against a
//! live 0.1.0-rc.6 session log):
//!   assistant/chunk: block-start / text-delta / block-end / usage / finish
//!   assistant/message: data.message.content[]
//! Full taxonomy follows docs/01-grok-tui-spec.md section 4.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Notice/Subagent are spec-planned render kinds
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
    #[allow(dead_code)] // stable identity for event dedup (planned)
    pub id: u64,
    /// 产生这个 cell 的 surface 事件 seq（user/message、assistant/message、
    /// tool/result——dsh-session 的 SURFACE_EVENT_TYPES 三件套）。None =
    /// 非 surface cell（tool/call 卡、本地错误条、demo），压缩替换按它
    /// 判定哪些节点被影子化（docs/04 §6.3.2）。
    pub seq: Option<u64>,
    pub kind: CellKind,
    pub title: String,
    pub text: String,
    /// 完整事件载荷，供 raw 视图使用；`text` 保持结构化展示文本。
    pub raw_text: String,
    pub raw: bool,
    /// 工具调用与结果通过 Harness callId 关联。
    pub call_id: Option<String>,
    /// Tool name behind this cell, copied onto the matching result cell so the
    /// result body renders with the same per-tool rules as the call.
    pub tool: Option<String>,
    pub folded: bool,
    pub failed: bool,
    /// Subagent cells carry the child session id (opens the framed view).
    pub link: Option<String>,
}

/// One user turn for the rewind picker: its durable seq plus the render cell
/// that starts it (truncation point for display-level rollback).
#[derive(Debug, Clone, Copy)]
pub struct TurnMarker {
    pub seq: u64,
    pub cell: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub turns: u64,
    pub steps: u64,
    pub llm_ms: u64,
    pub tool_ms: u64,
    pub ttft_ms: u64,
    pub ttft_steps: u64,
    pub decode_ms: u64,
    pub decode_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
struct OpenStep {
    start_time: u64,
    first_token_time: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct UsageSample {
    turn: u64,
    step: u64,
    usage: Usage,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    /// Strict mapping of DSH's `todos` projection vocabulary. The wire contract
    /// is exactly three values; anything else is a protocol surprise and lands
    /// on `Pending` rather than being invented into a new state.
    pub fn parse_wire(raw: &str) -> Self {
        match raw {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => Self::Pending,
        }
    }

    /// Lenient mapping for the tool-argument fallback path, which has to cope
    /// with whatever vocabulary a non-DSH harness uses.
    fn parse(raw: &str) -> Self {
        match raw
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "in_progress" | "active" | "running" | "doing" => Self::InProgress,
            "completed" | "complete" | "done" | "finished" => Self::Completed,
            "cancelled" | "canceled" | "skipped" | "dropped" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub const fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
            Self::Cancelled => "[-]",
        }
    }
}

/// One row of the agent's task list. Harness `todo_write`-style tools publish a
/// full snapshot per call, so each call replaces the list wholesale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

/// Does this tool name look like a todo-list writer? Harnesses spell it
/// `todo_write`, `TodoWrite`, `todos.update`, … so match on the stem rather
/// than pinning one name we would have to chase.
pub fn is_todo_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("todo") || lower.contains("task_list") || lower.contains("tasklist")
}

/// Pull a todo snapshot out of a tool call's arguments. Accepts the shapes we
/// have seen in the wild: `{todos:[…]}`, `{items:[…]}`, `{tasks:[…]}`, or a
/// bare array; each entry may name its text `content`, `text`, `title`, or
/// `task`. Returns None when nothing list-shaped is in there.
pub fn parse_todos(arguments: &str) -> Option<Vec<TodoItem>> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let array = value
        .as_array()
        .or_else(|| value.get("todos").and_then(Value::as_array))
        .or_else(|| value.get("items").and_then(Value::as_array))
        .or_else(|| value.get("tasks").and_then(Value::as_array))
        .or_else(|| value.get("todoList").and_then(Value::as_array))?;
    let items: Vec<TodoItem> = array
        .iter()
        .filter_map(|entry| {
            if let Some(text) = entry.as_str() {
                return Some(TodoItem {
                    text: text.to_string(),
                    status: TodoStatus::Pending,
                });
            }
            let text = ["content", "text", "title", "task", "description"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(Value::as_str))?
                .trim()
                .to_string();
            if text.is_empty() {
                return None;
            }
            let status = ["status", "state"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(Value::as_str))
                .map(TodoStatus::parse)
                .unwrap_or(TodoStatus::Pending);
            Some(TodoItem { text, status })
        })
        .collect();
    // An empty list is a legitimate snapshot ("all done"), but a payload that
    // parsed to nothing usable is not — that would silently blank the pane.
    if items.is_empty() && !array.is_empty() {
        return None;
    }
    Some(items)
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub cells: Vec<Cell>,
    pub selected: Option<usize>,
    pub usage: Usage,
    pub stats: SessionStats,
    pub turns: Vec<TurnMarker>,
    /// Latest task-list snapshot (Ctrl+T pane + inline checklist rendering).
    pub todos: Vec<TodoItem>,
    /// goalId -> highest admitted continuation round, derived from goal-sourced
    /// `user/message` events. The `goal` projection cannot supply this.
    pub goal_rounds: HashMap<String, u64>,
    next_id: u64,
    blocks: HashMap<u64, BlockState>,
    completed_turns: HashSet<u64>,
    open_steps: HashMap<(u64, u64), OpenStep>,
    pending_tools: HashMap<String, u64>,
    last_usage: Option<UsageSample>,
    /// 乐观上屏的用户消息 (cell index, text)：发送即上屏，等 user/message
    /// 回执到达时认领而不是重复入列。
    pending_user: VecDeque<(usize, String)>,
}

/// 事件信封上的 surfaceOp replace 声明（dsh-session：被替换区间 inclusive）。
/// "append" 标记或无 surfaceOp 返回 None。
fn surface_replace_range(event: &Value) -> Option<(u64, u64)> {
    let op = event.get("surfaceOp")?;
    if op.get("op").and_then(Value::as_str) != Some("replace") {
        return None;
    }
    let start = op.get("start").and_then(Value::as_u64)?;
    let end = op.get("end").and_then(Value::as_u64)?;
    Some((start, end))
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub(crate) fn push(
        &mut self,
        kind: CellKind,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.cells.push(Cell {
            id,
            seq: None,
            kind,
            title: title.into(),
            raw_text: String::new(),
            raw: false,
            call_id: None,
            tool: None,
            text: text.into(),
            folded: kind == CellKind::Thinking,
            failed: false,
            link: None,
        });
        self.cells.len() - 1
    }

    /// 压缩替换（docs/04 §6.3.2）：surfaceOp replace 声明的 seq 区间内，
    /// 所有 surface cell 都被摘要影子化。index 跨度内无 seq 的 cell
    /// （tool/call 卡、本地错误条）一并清掉，避免孤儿卡悬在摘要上方；
    /// 腾出的位置放一条 compact 标记，轮次/选中/乐观队列整体重映射。
    fn apply_surface_replace(&mut self, start: u64, end: u64) {
        let mut span: Option<(usize, usize)> = None;
        for (i, cell) in self.cells.iter().enumerate() {
            if matches!(cell.seq, Some(seq) if (start..=end).contains(&seq)) {
                span = Some(match span {
                    None => (i, i),
                    Some((first, _)) => (first, i),
                });
            }
        }
        let Some((a, b)) = span else { return };
        let removed = b + 1 - a;
        self.cells.drain(a..=b);
        let id = self.next_id;
        self.next_id += 1;
        self.cells.insert(
            a.min(self.cells.len()),
            Cell {
                id,
                seq: None,
                kind: CellKind::Notice,
                title: "compact".to_string(),
                text: format!("上下文压缩：{removed} 个会话节点被摘要替换"),
                raw_text: String::new(),
                raw: false,
                call_id: None,
                tool: None,
                folded: false,
                failed: false,
                link: None,
            },
        );
        // 跨度内的下标 = 被压缩掉（rewind 选择器里消失）；跨度后的顺移。
        let remap = |idx: usize| {
            if idx < a {
                Some(idx)
            } else if idx > b {
                Some(idx - removed + 1)
            } else {
                None
            }
        };
        self.turns = self
            .turns
            .iter()
            .filter_map(|m| remap(m.cell).map(|cell| TurnMarker { seq: m.seq, cell }))
            .collect();
        self.selected = self
            .selected
            .and_then(remap)
            .or(Some(a.min(self.cells.len() - 1)));
        self.pending_user = self
            .pending_user
            .iter()
            .filter_map(|(i, text)| remap(*i).map(|i| (i, text.clone())))
            .collect();
    }

    /// 发送即上屏的用户消息：不等运行时回执，先把消息放进会话，回执到达时
    /// 由 user/message 分支按文本认领（见 pending_user）。
    pub(crate) fn push_user_optimistic(&mut self, text: String) {
        let i = self.push(CellKind::User, String::new(), text.clone());
        self.pending_user.push_back((i, text));
        self.selected = Some(i);
    }

    pub(crate) fn from_cell(cell: Cell, selected: bool) -> Self {
        let mut transcript = Self::new();
        transcript.cells.push(cell);
        transcript.selected = selected.then_some(0);
        transcript
    }

    fn event_time(event: &Value) -> Option<u64> {
        event.get("time").and_then(Value::as_u64)
    }

    fn turn_step(data: Option<&Value>) -> Option<(u64, u64)> {
        let data = data?;
        Some((data.get("turn")?.as_u64()?, data.get("step")?.as_u64()?))
    }

    fn usage_from(value: &Value) -> Usage {
        Usage {
            input: value
                .get("inputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: value
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache: value
                .get("cacheReadTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write: value
                .get("cacheWriteTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }
    }

    fn record_usage(&mut self, turn: u64, step: u64, usage: Usage) {
        if let Some(previous) = self
            .last_usage
            .filter(|sample| sample.turn == turn && sample.step == step)
        {
            self.usage.input = self.usage.input.saturating_sub(previous.usage.input);
            self.usage.output = self.usage.output.saturating_sub(previous.usage.output);
            self.usage.cache = self.usage.cache.saturating_sub(previous.usage.cache);
            self.usage.cache_write = self
                .usage
                .cache_write
                .saturating_sub(previous.usage.cache_write);
        }
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.cache = self.usage.cache.saturating_add(usage.cache);
        self.usage.cache_write = self.usage.cache_write.saturating_add(usage.cache_write);
        self.last_usage = Some(UsageSample { turn, step, usage });
    }

    fn is_token_delta(chunk: &Value) -> bool {
        match chunk.get("type").and_then(Value::as_str) {
            Some("text-delta" | "reasoning-delta") => chunk
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            Some("tool-call-delta") => {
                chunk
                    .get("argumentsDelta")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                    || chunk.get("name").is_some()
            }
            _ => false,
        }
    }

    fn record_metrics(&mut self, event: &Value, ty: &str, data: Option<&Value>) {
        let time = Self::event_time(event);
        match ty {
            "step/start" => {
                if let (Some((turn, step)), Some(start_time)) = (Self::turn_step(data), time) {
                    self.open_steps.insert(
                        (turn, step),
                        OpenStep {
                            start_time,
                            first_token_time: None,
                        },
                    );
                }
            }
            "assistant/chunk" => {
                let Some(data) = data else { return };
                let Some((turn, step)) = Self::turn_step(Some(data)) else {
                    return;
                };
                let Some(chunk) = data.get("chunk") else {
                    return;
                };
                if Self::is_token_delta(chunk) {
                    if let Some(open) = self.open_steps.get_mut(&(turn, step)) {
                        if open.first_token_time.is_none() {
                            open.first_token_time = time;
                        }
                    }
                }
                if chunk.get("type").and_then(Value::as_str) == Some("usage") {
                    if let Some(usage) = chunk.get("usage") {
                        self.record_usage(turn, step, Self::usage_from(usage));
                    }
                }
            }
            "assistant/message" => {
                let Some((turn, step)) = Self::turn_step(data) else {
                    return;
                };
                if let Some(usage) = data.and_then(|value| value.get("usage")) {
                    self.record_usage(turn, step, Self::usage_from(usage));
                }
                if let (Some(open), Some(completed_time)) =
                    (self.open_steps.remove(&(turn, step)), time)
                {
                    self.stats.llm_ms = self
                        .stats
                        .llm_ms
                        .saturating_add(completed_time.saturating_sub(open.start_time));
                    if let Some(first_token_time) = open.first_token_time {
                        self.stats.ttft_ms = self
                            .stats
                            .ttft_ms
                            .saturating_add(first_token_time.saturating_sub(open.start_time));
                        self.stats.ttft_steps = self.stats.ttft_steps.saturating_add(1);
                        let output_tokens = data
                            .and_then(|value| value.get("usage"))
                            .map(Self::usage_from)
                            .map(|usage| usage.output)
                            .or_else(|| {
                                self.last_usage
                                    .filter(|sample| sample.turn == turn && sample.step == step)
                                    .map(|sample| sample.usage.output)
                            });
                        if let Some(output_tokens) = output_tokens {
                            self.stats.decode_ms = self
                                .stats
                                .decode_ms
                                .saturating_add(completed_time.saturating_sub(first_token_time));
                            self.stats.decode_tokens =
                                self.stats.decode_tokens.saturating_add(output_tokens);
                        }
                    }
                }
            }
            "tool/call" => {
                if let (Some(call_id), Some(start_time)) = (
                    data.and_then(|value| value.get("callId"))
                        .and_then(Value::as_str),
                    time,
                ) {
                    self.pending_tools.insert(call_id.to_string(), start_time);
                }
            }
            "tool/result" => {
                let call_id = data
                    .and_then(|value| value.get("message"))
                    .and_then(|message| message.get("source"))
                    .and_then(|source| source.get("callId"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        data.and_then(|value| value.get("message"))
                            .and_then(|message| message.get("content"))
                            .and_then(Value::as_array)
                            .and_then(|blocks| {
                                blocks.iter().find_map(|block| {
                                    block.get("toolCallId").and_then(Value::as_str)
                                })
                            })
                    });
                if let (Some(call_id), Some(completed_time)) = (call_id, time) {
                    if let Some(start_time) = self.pending_tools.remove(call_id) {
                        self.stats.tool_ms = self
                            .stats
                            .tool_ms
                            .saturating_add(completed_time.saturating_sub(start_time));
                    }
                }
            }
            "step/end" => {
                if let Some((turn, step)) = Self::turn_step(data) {
                    self.stats.steps = self.stats.steps.saturating_add(1);
                    if self.completed_turns.insert(turn) {
                        self.stats.turns = self.stats.turns.saturating_add(1);
                    }
                    self.open_steps.remove(&(turn, step));
                }
            }
            "turn/end" => self.pending_tools.clear(),
            _ => {}
        }
    }


    pub fn apply(&mut self, event: &Value) {
        // surfaceOp replace（压缩摘要）先处理：丢弃被影子化的区间，
        // 再走正常事件逻辑——摘要消息本身照常入列。
        if let Some((start, end)) = surface_replace_range(event) {
            self.apply_surface_replace(start, end);
        }
        let Some(ty) = event.get("type").and_then(Value::as_str) else {
            return;
        };
        let data = event.get("data");
        self.record_metrics(event, ty, data);
        match ty {
            "user/message" => {
                let source = data.and_then(|d| d.get("source"));
                let kind = source.and_then(|s| s.get("kind")).and_then(Value::as_str);
                // A goal-sourced prompt is the harness admitting a continuation
                // round. This is the ONLY live signal for the round counter: the
                // `goal` projection's `roundsStarted` folds `goal/change` alone,
                // so it is frozen at whatever the last mutation snapshotted.
                if kind == Some("goal") {
                    if let (Some(id), Some(round)) = (
                        source.and_then(|s| s.get("goalId")).and_then(Value::as_str),
                        source.and_then(|s| s.get("round")).and_then(Value::as_u64),
                    ) {
                        let entry = self.goal_rounds.entry(id.to_string()).or_default();
                        *entry = (*entry).max(round);
                    }
                }
                // Only real user prompts belong on the chat surface; plugin /
                // runtime-context injections (system-prompt snapshots, skills
                // catalogs) are model-visible but not user-visible.
                if kind != Some("user") {
                    return;
                }
                let text = text_blocks(data);
                if !text.is_empty() {
                    // 发送时已乐观上屏的同文消息：认领而不是重复入列。
                    let claimed = match self.pending_user.front() {
                        Some((_, pending)) if *pending == text => self.pending_user.pop_front(),
                        _ => None,
                    };
                    if let Some((i, _)) = claimed {
                        if let Some(seq) = event.get("seq").and_then(Value::as_u64) {
                            self.cells[i].seq = Some(seq);
                            self.turns.push(TurnMarker { seq, cell: i });
                        }
                        self.selected = Some(i);
                        return;
                    }
                    let i = self.push(CellKind::User, String::new(), text);
                    if let Some(seq) = event.get("seq").and_then(Value::as_u64) {
                        self.cells[i].seq = Some(seq);
                        self.turns.push(TurnMarker { seq, cell: i });
                    }
                    self.selected = Some(i);
                }
            }
            "assistant/chunk" => {
                let Some(chunk) = data.and_then(|d| d.get("chunk")) else {
                    return;
                };
                let ctype = chunk.get("type").and_then(Value::as_str);
                let index = chunk.get("index").and_then(Value::as_u64);
                match ctype {
                    Some("block-start") => {
                        let block_type = chunk.get("blockType").and_then(Value::as_str);
                        let kind = match block_type {
                            Some("reasoning" | "thinking") => BlockKind::Thinking,
                            Some("text") => BlockKind::Text,
                            // Tool calls have their own durable tool/call event. Creating an
                            // assistant cell here leaves an empty block in the transcript.
                            _ => return,
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
                    Some("text-delta" | "reasoning-delta") => {
                        if let Some(t) = chunk.get("text").and_then(Value::as_str) {
                            let fallback_kind = if ctype == Some("reasoning-delta") {
                                CellKind::Thinking
                            } else {
                                CellKind::Assistant
                            };
                            let target = index
                                .and_then(|i| self.blocks.get(&i))
                                .map(|b| b.cell)
                                .or_else(|| {
                                    self.cells.iter().rposition(|c| c.kind == fallback_kind)
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
                    Some("usage") => {}
                    _ => {}
                }
            }
            "step/start" | "step/end" | "turn/end" => {}
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
                        let streamed = self
                            .cells
                            .iter()
                            .rev()
                            .take_while(|c| c.kind == CellKind::Assistant)
                            .map(|c| c.text.as_str())
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<String>();
                        if streamed != text {
                            let i = self.push(CellKind::Assistant, String::new(), text);
                            self.selected = Some(i);
                        }
                    }
                }
                // 提交消息的 seq 盖到本条消息流式建出的尾部 cell 上：
                // chunk 期间拿不到最终 seq，压缩替换要靠它识别这些节点。
                if let Some(seq) = event.get("seq").and_then(Value::as_u64) {
                    for cell in self.cells.iter_mut().rev() {
                        if !matches!(cell.kind, CellKind::Assistant | CellKind::Thinking)
                            || cell.seq.is_some()
                        {
                            break;
                        }
                        cell.seq = Some(seq);
                    }
                }
            }
            "tool/call" => {
                let name = data
                    .and_then(|d| d.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let arguments = data
                    .and_then(|d| d.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let input = if arguments.is_empty() {
                    data.and_then(|d| d.get("input"))
                        .map(summarize)
                        .unwrap_or_default()
                } else {
                    tool_preview(arguments)
                };
                // A todo tool publishes a whole-list snapshot: keep it for the
                // Ctrl+T pane and show the checklist in the card body instead
                // of raw JSON.
                let todo_snapshot = if is_todo_tool(&name) {
                    let raw = if arguments.is_empty() {
                        data.and_then(|d| d.get("input"))
                            .map(|v| v.to_string())
                            .unwrap_or_default()
                    } else {
                        arguments.to_string()
                    };
                    parse_todos(&raw)
                } else {
                    None
                };
                let input = match &todo_snapshot {
                    Some(items) => render_todo_list(items),
                    None => input,
                };
                // Header reads like Grok's: `Run cargo test`, `Edit src/app.rs`
                // — the tool name alone does not say what the call did.
                let title = if todo_snapshot.is_some() {
                    crate::toolcard::ToolKind::Todo.label().to_string()
                } else {
                    crate::toolcard::header(&name, arguments)
                };
                let i = self.push(CellKind::Tool, title, input);
                self.cells[i].tool = Some(name);
                self.cells[i].raw_text = pretty_json_or_text(arguments);
                self.cells[i].call_id = data
                    .and_then(|d| d.get("callId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // Task lists are the point of the turn — leave them open.
                self.cells[i].folded = todo_snapshot.is_none();
                if let Some(items) = todo_snapshot {
                    self.todos = items;
                }
                self.selected = Some(i);
            }
            "tool/result" => {
                let result_block = data
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .and_then(|blocks| {
                        blocks.iter().find(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool-result")
                        })
                    });
                let failed = data.and_then(|d| d.get("error")).is_some()
                    || result_block
                        .and_then(|block| block.get("isError"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                let text = result_block
                    .and_then(|block| block.get("content"))
                    .and_then(Value::as_array)
                    .and_then(|content| content_text(content))
                    // Keep old logs readable while the persisted rc.6 shape above is primary.
                    .or_else(|| data.and_then(|d| d.get("result")).map(summarize))
                    .unwrap_or_default();
                let i = self.push(CellKind::ToolResult, String::new(), text);
                self.cells[i].seq = event.get("seq").and_then(Value::as_u64);
                self.cells[i].raw_text = self.cells[i].text.clone();
                self.cells[i].call_id = data
                    .and_then(|d| d.get("message"))
                    .and_then(|message| message.get("source"))
                    .and_then(|source| source.get("callId"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        result_block
                            .and_then(|block| block.get("toolCallId"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_string);
                self.cells[i].failed = failed;
                // Inherit the tool identity from the matching call, so the
                // result body folds and colours by the same per-tool rules.
                let call_id = self.cells[i].call_id.clone();
                self.cells[i].tool = call_id.and_then(|id| {
                    self.cells
                        .iter()
                        .rev()
                        .find(|c| c.kind == CellKind::Tool && c.call_id.as_deref() == Some(&id))
                        .and_then(|c| c.tool.clone())
                });
                // Grok 风格：工具输出默认折叠，失败时自动展开。
                self.cells[i].folded = !failed;
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

    pub fn toggle_raw(&mut self, idx: usize) {
        if let Some(cell) = self.cells.get_mut(idx) {
            if !cell.raw_text.is_empty() {
                cell.raw = !cell.raw;
                cell.folded = false;
            }
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

    /// Jump to the next/previous cell of one kind — Grok's Shift+H/L (turn, i.e.
    /// user prompt) and Shift+J/K (assistant reply). Stays put when there is no
    /// further match in that direction rather than wrapping.
    pub fn move_selection_to_kind(&mut self, kind: CellKind, forward: bool) -> bool {
        if self.cells.is_empty() {
            return false;
        }
        let cur = self.selected.unwrap_or(self.cells.len() - 1);
        let found = if forward {
            (cur + 1..self.cells.len()).find(|i| self.cells[*i].kind == kind)
        } else {
            (0..cur).rev().find(|i| self.cells[*i].kind == kind)
        };
        if let Some(i) = found {
            self.selected = Some(i);
            true
        } else {
            false
        }
    }

    /// Grok's Shift+E: collapse everything if anything is open, else expand all.
    pub fn toggle_all_folds(&mut self) -> bool {
        let any_open = self.cells.iter().any(|c| !c.folded);
        for cell in &mut self.cells {
            cell.folded = any_open;
        }
        !any_open
    }
}

fn text_blocks(data: Option<&Value>) -> String {
    let Some(data) = data else {
        return String::new();
    };
    let Some(content) = data.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    content_text(content).unwrap_or_default()
}

pub(crate) fn content_text(content: &[Value]) -> Option<String> {
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

/// Tool-card 折叠预览：bash 显示命令、fs 显示路径、web 显示查询词。
/// Render a todo snapshot as the checklist body of a tool card.
fn render_todo_list(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(no tasks)".to_string();
    }
    items
        .iter()
        .map(|item| format!("{} {}", item.status.marker(), item.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_preview(arguments: &str) -> String {
    let parsed = serde_json::from_str::<Value>(arguments).ok();
    let Some(object) = parsed.as_ref().and_then(Value::as_object) else {
        return summarize(&Value::String(arguments.to_string()));
    };
    for key in ["command", "file_path", "path", "query", "url", "pattern"] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return summarize(&Value::String(value.to_string()));
            }
        }
    }
    summarize(&Value::String(arguments.to_string()))
}

fn pretty_json_or_text(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(ty: &str, data: Value) -> Value {
        json!({"type":ty,"data":data})
    }

    #[test]
    fn applies_real_wire_shapes() {
        let mut t = Transcript::new();
        t.apply(&json!({"type":"user/message","seq":7,"data":{"source":{"kind":"user"},"content":[{"type":"text","text":"hi"}]}}));
        t.apply(&ev(
            "assistant/chunk",
            json!({"chunk":{"type":"block-start","index":0,"blockType":"reasoning"}}),
        ));
        t.apply(&ev(
            "assistant/chunk",
            json!({"chunk":{"type":"reasoning-delta","index":0,"text":"thinking..."}}),
        ));
        t.apply(&ev(
            "assistant/chunk",
            json!({"chunk":{"type":"block-end","index":0}}),
        ));
        assert_eq!(t.cells.len(), 2);
        assert_eq!(t.cells[0].kind, CellKind::User);
        assert_eq!(t.cells[1].kind, CellKind::Thinking);
        assert!(t.cells[1].folded);
        assert_eq!(t.cells[1].text, "thinking...");
        assert_eq!(t.turns.len(), 1);
        assert_eq!(t.turns[0].seq, 7);
    }

    #[test]
    fn surface_replace_shadows_compacted_range() {
        // docs/04 §6.3.2：压缩摘要事件带 surfaceOp replace(start,end)，
        // 区间内的 surface cell 必须离场，rewind 轮次表同步重映射。
        let mut t = Transcript::new();
        let user = |seq: u64, text: &str| {
            json!({"type":"user/message","seq":seq,"data":{"source":{"kind":"user"},"content":[{"type":"text","text":text}]}})
        };
        let assistant = |seq: u64, text: &str, surface_op: Value| {
            json!({"type":"assistant/message","seq":seq,"surfaceOp":surface_op,"data":{"message":{"content":[{"type":"text","text":text}]}}})
        };
        t.apply(&user(1, "first"));
        t.apply(&assistant(2, "answer one", json!("append")));
        t.apply(&user(3, "second"));
        t.apply(&assistant(4, "answer two", json!("append")));
        assert_eq!(t.cells.len(), 4);
        assert_eq!(t.turns.len(), 2);

        // 压缩：seq 1..=2 被摘要替换（first + answer one 离场）。
        t.apply(&assistant(
            5,
            "summary of the first exchange",
            json!({"op":"replace","start":1,"end":2}),
        ));

        let kinds: Vec<CellKind> = t.cells.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [CellKind::Notice, CellKind::User, CellKind::Assistant, CellKind::Assistant],
            "压缩标记顶替被影子化的区间，摘要在尾部入列，实际：{kinds:?}"
        );
        assert!(t.cells[0].text.contains("上下文压缩"));
        assert_eq!(t.cells[1].text, "second");
        // rewind 轮次表：被压缩的 seq 1 消失，seq 3 重映射到新下标 1。
        let turns: Vec<(u64, usize)> = t.turns.iter().map(|m| (m.seq, m.cell)).collect();
        assert_eq!(turns, vec![(3, 1)], "轮次表应重映射，实际 {turns:?}");
        // 选中项必须仍然指向合法 cell。
        assert!(t.selected.is_some_and(|i| i < t.cells.len()));
        // 再次替换一个不存在的区间：无操作，不 panic。
        t.apply(&assistant(
            6,
            "another summary",
            json!({"op":"replace","start":100,"end":200}),
        ));
        assert_eq!(t.cells.len(), 5);
    }

    #[test]
    fn filters_plugin_context_injections() {
        let mut t = Transcript::new();
        t.apply(&json!({"type":"user/message","seq":1,"data":{"source":{"kind":"plugin"},"content":[{"type":"text","text":"skills catalog"}]}}));
        assert!(t.cells.is_empty(), "plugin injections never render");
        assert!(t.turns.is_empty());
    }

    #[test]
    fn parses_real_tool_result_message() {
        let mut t = Transcript::new();
        t.apply(&json!({
            "type": "tool/result",
            "data": {
                "message": {
                    "role": "tool",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": "call-1",
                        "content": [{"type": "text", "text": "tests passed"}],
                        "isError": true
                    }]
                },
                "error": {"name": "ToolError", "code": "EXIT_1"}
            }
        }));

        assert_eq!(t.cells.len(), 1);
        assert_eq!(t.cells[0].kind, CellKind::ToolResult);
        assert_eq!(t.cells[0].text, "tests passed");
        assert!(t.cells[0].failed);
    }

    #[test]
    fn committed_messages_fallback_on_later_turns_without_deltas() {
        let mut t = Transcript::new();
        for text in ["first", "second"] {
            t.apply(&json!({
                "type": "assistant/message",
                "data": {"message": {"content": [{"type": "text", "text": text}]}}
            }));
        }

        assert_eq!(t.cells.len(), 2);
        assert_eq!(t.cells[1].text, "second");
    }

    #[test]
    fn tool_call_stream_start_does_not_create_empty_assistant_cell() {
        let mut t = Transcript::new();
        t.apply(&json!({
            "type": "assistant/chunk",
            "data": {"chunk": {"type": "block-start", "index": 0, "blockType": "tool-call"}}
        }));
        assert!(t.cells.is_empty());
    }

    #[test]
    fn parses_usage_chunk() {
        let mut t = Transcript::new();
        t.apply(&json!({"type":"assistant/chunk","data":{"turn":1,"step":1,"chunk":{"type":"usage","usage":{"inputTokens":24,"outputTokens":1,"cacheReadTokens":11648}}}}));
        assert_eq!(t.usage.input, 24);
        assert_eq!(t.usage.output, 1);
        assert_eq!(t.usage.cache, 11648);
    }
    #[test]
    fn aggregates_whole_session_stats_and_usage() {
        let mut transcript = Transcript::new();
        let events = [
            json!({"type":"step/start","time":1000,"data":{"turn":1,"step":1}}),
            json!({"type":"assistant/chunk","time":1300,"data":{"turn":1,"step":1,"chunk":{"type":"text-delta","index":0,"text":"hi"}}}),
            json!({"type":"tool/call","time":1500,"data":{"turn":1,"step":1,"callId":"call-1","name":"read","arguments":"{}"}}),
            json!({"type":"tool/result","time":1800,"data":{"turn":1,"step":1,"message":{"role":"user","source":{"kind":"tool","callId":"call-1"},"content":[{"type":"tool-result","toolCallId":"call-1","content":[]}]}}}),
            json!({"type":"assistant/chunk","time":1900,"data":{"turn":1,"step":1,"chunk":{"type":"usage","usage":{"inputTokens":100,"outputTokens":30,"cacheReadTokens":900}}}}),
            json!({"type":"assistant/message","time":2300,"data":{"turn":1,"step":1,"usage":{"inputTokens":100,"outputTokens":30,"cacheReadTokens":900},"message":{"content":[{"type":"text","text":"hi"}]}}}),
            json!({"type":"step/end","time":2300,"data":{"turn":1,"step":1}}),
            json!({"type":"step/start","time":3000,"data":{"turn":1,"step":2}}),
            json!({"type":"assistant/chunk","time":3100,"data":{"turn":1,"step":2,"chunk":{"type":"reasoning-delta","index":0,"text":"x"}}}),
            json!({"type":"assistant/chunk","time":3500,"data":{"turn":1,"step":2,"chunk":{"type":"usage","usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":800}}}}),
            json!({"type":"assistant/message","time":3500,"data":{"turn":1,"step":2,"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":800},"message":{"content":[{"type":"text","text":"done"}]}}}),
            json!({"type":"step/end","time":3500,"data":{"turn":1,"step":2}}),
            json!({"type":"turn/end","time":3500,"data":{"turn":1,"reason":{"kind":"completed"}}}),
        ];
        for event in events {
            transcript.apply(&event);
        }

        assert_eq!(transcript.stats.turns, 1);
        assert_eq!(transcript.stats.steps, 2);
        assert_eq!(transcript.stats.llm_ms, 1800);
        assert_eq!(transcript.stats.tool_ms, 300);
        assert_eq!(transcript.stats.ttft_ms, 400);
        assert_eq!(transcript.stats.ttft_steps, 2);
        assert_eq!(transcript.stats.decode_ms, 1400);
        assert_eq!(transcript.stats.decode_tokens, 50);
        assert_eq!(transcript.usage.input, 300);
        assert_eq!(transcript.usage.output, 50);
        assert_eq!(transcript.usage.cache, 1700);
    }
    #[test]
    fn tool_cards_keep_raw_arguments_and_call_ids() {
        let mut transcript = Transcript::new();
        transcript.apply(&json!({
            "type": "tool/call",
            "data": {
                "callId": "call-raw",
                "name": "edit",
                "arguments": "{\"path\":\"src/lib.rs\",\"patch\":\"@@ -1 +1 @@\\n-old\\n+new\"}"
            }
        }));
        transcript.apply(&json!({
            "type": "tool/result",
            "data": {
                "message": {
                    "source": {"kind": "tool", "callId": "call-raw"},
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": "call-raw",
                        "content": [{"type": "text", "text": "@@ -1 +1 @@\n-old\n+new"}]
                    }]
                }
            }
        }));

        assert_eq!(transcript.cells[0].call_id.as_deref(), Some("call-raw"));
        assert!(transcript.cells[0].raw_text.contains("\"patch\""));
        assert_eq!(transcript.cells[1].call_id.as_deref(), Some("call-raw"));
        transcript.toggle_raw(0);
        assert!(transcript.cells[0].raw);
        assert!(!transcript.cells[0].folded);
    }

    #[test]
    fn parses_todo_snapshots_in_the_shapes_harnesses_actually_send() {
        // Claude/DSH style
        let a = parse_todos(r#"{"todos":[{"content":"fix parser","status":"in_progress"},{"content":"add test","status":"pending"}]}"#).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].status, TodoStatus::InProgress);
        assert_eq!(a[1].status, TodoStatus::Pending);

        // alternate container + text key + status spellings
        let b = parse_todos(
            r#"{"items":[{"text":"ship","state":"done"},{"title":"drop","state":"cancelled"}]}"#,
        )
        .unwrap();
        assert_eq!(b[0].status, TodoStatus::Completed);
        assert_eq!(b[1].status, TodoStatus::Cancelled);

        // bare array of strings
        let c = parse_todos(r#"["one","two"]"#).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].status, TodoStatus::Pending);

        // an explicit empty list is a real snapshot
        assert_eq!(parse_todos(r#"{"todos":[]}"#), Some(vec![]));

        // garbage and unrelated payloads must not blank the pane
        assert_eq!(parse_todos("not json"), None);
        assert_eq!(parse_todos(r#"{"command":"ls"}"#), None);
        assert_eq!(parse_todos(r#"{"todos":[{"nope":1}]}"#), None);
    }

    #[test]
    fn todo_tool_names_are_matched_on_the_stem() {
        for name in ["todo_write", "TodoWrite", "todos.update", "task_list_set"] {
            assert!(is_todo_tool(name), "{name} should match");
        }
        for name in ["bash", "read_file", "grep"] {
            assert!(!is_todo_tool(name), "{name} should not match");
        }
    }

    #[test]
    fn todo_tool_call_snapshots_the_list_and_renders_a_checklist() {
        let mut t = Transcript::new();
        t.apply(&serde_json::json!({
            "type": "tool/call", "seq": 1, "time": 10,
            "data": {"callId": "c1", "name": "todo_write",
                     "arguments": "{\"todos\":[{\"content\":\"one\",\"status\":\"completed\"},{\"content\":\"two\",\"status\":\"in_progress\"}]}"}
        }));
        assert_eq!(t.todos.len(), 2);
        assert_eq!(t.todos[0].status, TodoStatus::Completed);
        // rendered as a checklist, left open, not raw JSON
        let cell = t.cells.last().unwrap();
        assert!(cell.text.contains("[x] one"), "got {:?}", cell.text);
        assert!(cell.text.contains("[~] two"), "got {:?}", cell.text);
        assert!(!cell.folded, "task lists stay expanded");

        // a later call replaces the snapshot wholesale
        t.apply(&serde_json::json!({
            "type": "tool/call", "seq": 2, "time": 20,
            "data": {"callId": "c2", "name": "todo_write",
                     "arguments": "{\"todos\":[{\"content\":\"only\",\"status\":\"pending\"}]}"}
        }));
        assert_eq!(t.todos.len(), 1);
        assert_eq!(t.todos[0].text, "only");
    }

    #[test]
    fn non_todo_tool_calls_keep_their_folded_preview() {
        let mut t = Transcript::new();
        t.apply(&serde_json::json!({
            "type": "tool/call", "seq": 1, "time": 10,
            "data": {"callId": "c1", "name": "bash", "arguments": "{\"command\":\"cargo test\"}"}
        }));
        assert!(t.todos.is_empty());
        assert!(t.cells.last().unwrap().folded);
    }
}
