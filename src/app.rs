//! App state machine: run state, focus, Esc semantics (docs/01 section 2.5),
//! follow-up queue, selection/scroll, and the blocking dialogs (approval
//! prompt + ask_user_question card, docs/01 section 2.4).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};

use crate::bus::{AppEvent, Cmd};
use crate::clipboard;
use crate::files::{fuzzy_filter, list_files};
use crate::resume::{list_sessions, read_session_events, SessionSummary};
use crate::term::TermKind;
use crate::theme::Theme;
use crate::transcript::{content_text, CellKind, Transcript};

pub const DOUBLE_ESC_MS: u128 = 800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Starting,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Scrollback,
    Prompt,
}

#[derive(Debug, Clone, Copy)]
pub enum EscArm {
    None,
    ClearArmed(Instant),
    RewindArmed(Instant),
}

/// Destructive globals Grok gates behind a double-press within 1s: quitting
/// (throws away the draft) and starting a new session (throws away context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Quit,
    NewSession,
}

pub const CONFIRM_MS: u128 = 1000;

/// Permission prompt (docs/01 section 3.3). Options come from the bridge:
/// allowed-once / rejected (+ always-* rows later).
#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub request_id: String,
    pub tool_name: String,
    pub reason: String,
    pub input: String,
    pub call_id: Option<String>,
    pub options: Vec<String>,
    pub selected: usize,
    /// docs/01 section 2.4: Esc parks the keyboard in the scrollback while
    /// the card stays visible; Tab hands the keyboard back to the card.
    pub parked: bool,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub question: String,
    pub header: String,
    pub detail: String,
    pub plan_approve: Option<String>,
    pub options: Vec<String>,
    pub multi_select: bool,
}

/// ask_user_question card (docs/01 section 2.4).
#[derive(Debug, Clone)]
pub struct AskDialog {
    pub request_id: String,
    pub questions: Vec<Question>,
    pub current: usize,
    /// Chosen option indices per question.
    pub answers: Vec<Vec<usize>>,
    /// Keyboard cursor per question, independent from the committed selection.
    pub cursors: Vec<usize>,
    /// plan-review: free-form feedback typed after pressing s.
    pub feedback: String,
    pub taking_feedback: bool,
    pub detail_scroll: usize,
    pub custom_text: String,
    pub taking_text: bool,
    pub parked: bool,
}

impl AskDialog {
    fn has_pending_input(&self) -> bool {
        self.taking_text || self.taking_feedback || !self.custom_text.is_empty() || !self.feedback.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ResumePicker {
    pub items: Vec<SessionSummary>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: String,
    pub placement: String,
    pub preview: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueueView {
    pub selected: usize,
    pub editing: bool,
    pub draft: String,
}

#[derive(Debug, Clone)]
pub struct HistoryView {
    pub query: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct TasksView {
    pub rows: Vec<TaskRow>,
    pub selected: usize,
}

/// Ctrl+T pane over the latest task-list snapshot (docs/01 section 10).
#[derive(Debug, Clone)]
pub struct TodosView {
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct SubagentView {
    pub child_id: String,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct InfoDialog {
    pub rows: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct BlockView {
    pub cell: crate::transcript::Cell,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct ThemePicker {
    pub original: crate::theme::Theme,
    pub selected: usize,
}

#[derive(Debug, Clone, Default)]
pub struct HarnessCapabilities {
    pub models: bool,
    pub permissions: bool,
    pub plan_mode: bool,
    pub compaction: bool,
    pub jobs: bool,
    pub user_questions: bool,
    pub session_search: bool,
    pub commands: bool,
    pub tools: bool,
}

#[derive(Debug, Clone)]
pub struct HarnessCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}
#[derive(Debug, Clone)]
pub struct PaletteRow {
    pub label: String,
    pub action: String,
    pub shortcut: Option<String>,
    pub section: &'static str,
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub rows: Vec<PaletteRow>,
    pub filter: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutSection {
    Essentials,
    Input,
    Navigation,
    Actions,
    Panels,
    Session,
}

impl ShortcutSection {
    pub const ALL: [Self; 6] = [
        Self::Essentials,
        Self::Input,
        Self::Navigation,
        Self::Actions,
        Self::Panels,
        Self::Session,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Essentials => "Essentials",
            Self::Input => "Input",
            Self::Navigation => "Conversation Navigation",
            Self::Actions => "Conversation Actions",
            Self::Panels => "Panels",
            Self::Session => "Session",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShortcutRow {
    pub label: &'static str,
    pub keys: &'static str,
    pub section: ShortcutSection,
}

#[derive(Debug, Clone)]
pub struct Shortcuts {
    pub rows: Vec<ShortcutRow>,
    pub filter: String,
    pub selected_section: usize,
    pub expanded: [bool; 6],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Normal,
    Plan,
    AlwaysApprove,
}

impl PermissionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Plan => "Plan",
            Self::AlwaysApprove => "Always-approve",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReasoningEffortEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelReasoning {
    pub efforts: Vec<ReasoningEffortEntry>,
    pub default_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub context_window: Option<u64>,
    pub reasoning: Option<ModelReasoning>,
}

#[derive(Debug, Clone)]
pub struct RewindEntry {
    pub seq: u64,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct RewindPicker {
    pub items: Vec<RewindEntry>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct FilePicker {
    pub files: Vec<String>,
    pub query: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct ModelPicker {
    pub rows: Vec<ModelEntry>,
    pub filter: String,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct EffortPicker {
    pub model: ModelEntry,
    pub rows: Vec<ReasoningEffortEntry>,
    pub selected: usize,
}

impl ModelPicker {
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.rows.len())
            .filter(|i| {
                let r = &self.rows[*i];
                f.is_empty()
                    || r.id.to_lowercase().contains(&f)
                    || r.name.to_lowercase().contains(&f)
                    || r.provider.to_lowercase().contains(&f)
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum Dialog {
    None,
    Approval(ApprovalDialog),
    Ask(AskDialog),
    Resume(ResumePicker),
    Model(ModelPicker),
    Effort(EffortPicker),
    FilePicker(FilePicker),
    Rewind(RewindPicker),
    Palette(Palette),
    Shortcuts(Shortcuts),
    Info(InfoDialog),
    Theme(ThemePicker),
    Subagent(SubagentView),
    Tasks(TasksView),
    Todos(TodosView),
    Block(BlockView),
    History(HistoryView),
    Queue(QueueView),
}

pub struct App {
    pub theme: Theme,
    pub transcript: Transcript,
    /// DSH session projections (docs/04 section 3) — the canonical read model.
    pub projections: crate::projection::Projections,
    pub input: String,
    /// Caret as a byte offset into `input`. Always on a UTF-8 boundary and
    /// never past the end — `clamp_cursor` restores that before every edit,
    /// so a stale write to `input` can never split a codepoint.
    pub cursor: usize,
    /// Snapshots for Ctrl+Z. Consecutive plain inserts coalesce into one entry
    /// so undo steps over a typing burst, not one character.
    undo: Vec<(String, usize)>,
    typing_run: bool,
    confirm: Option<(Confirm, Instant)>,
    /// Is `notice` currently owned by a confirm prompt? Only then may `tick`
    /// clear it, so an unrelated notice is never eaten.
    confirm_notice: bool,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub history_draft: String,
    pub multiline: bool,
    pub focus: Focus,
    pub state: RunState,
    pub esc: EscArm,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<u64>,
    pub reasoning_effort_name: Option<String>,
    pub status: String,
    pub notice: Option<String>,
    pub scroll: usize,
    pub follow_selection: bool,
    pub composer_top: u16,
    pub needs_redraw: bool,
    pub quit: bool,
    pub demo: bool,
    pub dialog: Dialog,
    pub workspace: String,
    pub term_kind: TermKind,
    pub in_tmux: bool,
    at_fragment_start: Option<usize>,
    permission_presets: Vec<String>,
    preset_index: usize,
    pub permission_mode: PermissionMode,
    catalog_for_presets: bool,
    live_ids: HashSet<String>,
    capabilities: HarnessCapabilities,
    harness_commands: Vec<HarnessCommand>,
    catalog_loaded: bool,
    cancel_grace: Option<Instant>,
    pub child_transcripts: HashMap<String, Transcript>,
    pending_resume_file: Option<std::path::PathBuf>,
    queue: Vec<QueueItem>,
    cmd_tx: Sender<Cmd>,
}

impl App {
    pub fn new(
        theme: Theme,
        session_id: String,
        provider: String,
        model: String,
        demo: bool,
        cmd_tx: Sender<Cmd>,
        workspace: String,
    ) -> Self {
        Self {
            theme,
            transcript: Transcript::new(),
            projections: crate::projection::Projections::default(),
            input: String::new(),
            cursor: 0,
            undo: Vec::new(),
            typing_run: false,
            confirm: None,
            confirm_notice: false,
            history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            multiline: false,
            focus: Focus::Prompt,
            state: RunState::Idle,
            esc: EscArm::None,
            session_id,
            provider,
            model,
            reasoning_effort: None,
            context_window: None,
            reasoning_effort_name: None,
            status: String::new(),
            notice: None,
            scroll: 0,
            follow_selection: true,
            composer_top: 0,
            needs_redraw: true,
            quit: false,
            demo,
            dialog: Dialog::None,
            workspace,
            term_kind: TermKind::Plain,
            in_tmux: false,
            at_fragment_start: None,
            permission_presets: Vec::new(),
            preset_index: 0,
            permission_mode: PermissionMode::Normal,
            catalog_for_presets: false,
            capabilities: HarnessCapabilities::default(),
            harness_commands: Vec::new(),
            catalog_loaded: false,
            live_ids: HashSet::new(),
            cancel_grace: None,
            child_transcripts: HashMap::new(),
            pending_resume_file: None,
            queue: Vec::new(),
            cmd_tx,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running | RunState::Starting)
    }

    pub fn queue(&self) -> &[QueueItem] {
        &self.queue
    }
    fn queued_items(&self) -> impl Iterator<Item = &QueueItem> {
        self.queue.iter().filter(|item| item.placement == "queued")
    }

    pub fn queued_count(&self) -> usize {
        self.queued_items().count()
    }

    fn open_queue(&mut self) {
        if self.queued_count() == 0 {
            self.notice = Some("queue empty".into());
            return;
        }
        self.dialog = Dialog::Queue(QueueView {
            selected: 0,
            editing: false,
            draft: String::new(),
        });
    }

    /// The `todos` projection is authoritative when present (docs/04 section 5).
    /// The tool-argument parser in `transcript.rs` stays as the fallback for
    /// harnesses that publish no projection, so this only ever overwrites the
    /// transcript's guess with the real contract.
    fn sync_todos_from_projection(&mut self) {
        if let Some(items) = self.projections.todos.clone() {
            self.transcript.todos = items;
            if let Dialog::Todos(view) = &mut self.dialog {
                view.selected = view
                    .selected
                    .min(self.transcript.todos.len().saturating_sub(1));
            }
            if self.transcript.todos.is_empty() {
                if let Dialog::Todos(_) = self.dialog {
                    self.dialog = Dialog::None;
                    self.notice = Some("task list cleared".into());
                }
            }
        }
    }

    fn open_todos(&mut self) {
        if self.transcript.todos.is_empty() {
            self.notice = Some("no task list yet".into());
            return;
        }
        // Land on whatever the agent is working on right now.
        let selected = self
            .transcript
            .todos
            .iter()
            .position(|item| item.status == crate::transcript::TodoStatus::InProgress)
            .unwrap_or(0);
        self.dialog = Dialog::Todos(TodosView { selected });
    }

    fn queue_action(&mut self, item_id: String, action: Value) {
        if action.get("kind").and_then(Value::as_str) == Some("edit") {
            if let Some(item) = self.queue.iter_mut().find(|item| item.id == item_id) {
                let text = action
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                item.preview = queue_preview(&text);
                item.text = Some(text);
            }
        } else if matches!(
            action.get("kind").and_then(Value::as_str),
            Some("remove" | "steer" | "send-now")
        ) {
            self.queue.retain(|item| item.id != item_id);
        }
        let _ = self.cmd_tx.send(Cmd::UpdateQueue {
            session_id: self.session_id.clone(),
            item_id,
            action,
        });
    }

    fn send_now_item(&mut self, item_id: String) {
        if !self.is_running() {
            return;
        }
        self.queue_action(item_id, json!({ "kind": "send-now" }));
        self.status = "sending queued follow-up now".into();
    }

    fn send_now_text(&mut self, text: String) {
        if text.trim().is_empty() || !self.is_running() {
            return;
        }
        self.history.push(text.clone());
        self.clear_input();
        self.leave_history_navigation();
        let _ = self.cmd_tx.send(Cmd::SendNow {
            session_id: self.session_id.clone(),
            text,
        });
        self.status = "sending follow-up now".into();
    }

    fn replace_queue(&mut self, params: &Value) {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if sid != self.session_id {
            return;
        }
        self.queue = params
            .get("items")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(queue_item_from_value).collect())
            .unwrap_or_default();
        let count = self.queued_count();
        if let Dialog::Queue(view) = &mut self.dialog {
            if count == 0 {
                self.dialog = Dialog::None;
            } else {
                view.selected = view.selected.min(count - 1);
            }
        }
    }


    pub fn has_dialog(&self) -> bool {
        !matches!(self.dialog, Dialog::None)
    }

    fn respond(&mut self, id: String, result: Value) {
        let _ = self.cmd_tx.send(Cmd::Respond { id, result });
    }

    fn send_input(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history.push(text.clone());
        // `text` already came out of `take_input`; this only covers callers
        // that hand us a string built some other way.
        self.input.clear();
        self.cursor = 0;
        self.typing_run = false;
        self.history_cursor = None;
        self.history_draft.clear();
        self.state = RunState::Starting;
        if self.demo {
            self.transcript.push(
                CellKind::Assistant,
                String::new(),
                "(demo) 收到，这里是本地回显。".to_string(),
            );

            self.state = RunState::Idle;
            return;
        }
        let _ = self.cmd_tx.send(Cmd::Prompt {
            session_id: self.session_id.clone(),
            text,
        });
    }

    fn history_matches(&self, query: &str) -> Vec<usize> {
        history_matches(&self.history, query)
    }

    fn open_history(&mut self) {
        if self.history.is_empty() {
            self.notice = Some("prompt history empty".into());
            return;
        }
        let visible = self.history_matches("");
        self.dialog = Dialog::History(HistoryView {
            query: String::new(),
            selected: 0,
            visible,
        });
    }
    // ---- composer editing (docs/01 section 6) ---------------------------
    // `input` is a plain String and the caret lives beside it in `cursor` as a
    // byte offset. Every mutation runs `clamp_cursor` first, so a stale write
    // to `input` (history restore, a test) can never split a codepoint.

    fn clamp_cursor(&mut self) {
        if self.cursor > self.input.len() {
            self.cursor = self.input.len();
            return;
        }
        while self.cursor < self.input.len() && !self.input.is_char_boundary(self.cursor) {
            self.cursor += 1;
        }
    }

    /// Snapshot for Ctrl+Z. `grouped` marks a plain insert or delete, which
    /// coalesces into the run before it so undo steps over a typing burst
    /// rather than one character.
    fn push_undo(&mut self, grouped: bool) {
        if grouped && self.typing_run {
            return;
        }
        if self.undo.last().map(|(text, _)| text.as_str()) != Some(self.input.as_str()) {
            self.undo.push((self.input.clone(), self.cursor));
            if self.undo.len() > 64 {
                self.undo.remove(0);
            }
        }
        self.typing_run = grouped;
    }

    fn undo_input(&mut self) {
        let Some((text, cursor)) = self.undo.pop() else {
            self.notice = Some("nothing to undo".into());
            return;
        };
        self.input = text;
        self.cursor = cursor;
        self.typing_run = false;
        self.clamp_cursor();
        self.leave_history_navigation();
    }

    /// Replace the whole draft and park the caret at the end (history recall,
    /// queue edit, tests).
    pub fn set_input(&mut self, text: impl Into<String>) {
        self.input = text.into();
        self.cursor = self.input.len();
        self.typing_run = false;
    }

    fn take_input(&mut self) -> String {
        self.push_undo(false);
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    fn clear_input(&mut self) {
        if self.input.is_empty() {
            return;
        }
        self.push_undo(false);
        self.input.clear();
        self.cursor = 0;
    }

    fn insert_str(&mut self, text: &str) {
        self.clamp_cursor();
        self.push_undo(true);
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn insert_char(&mut self, c: char) {
        self.clamp_cursor();
        self.push_undo(true);
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn prev_boundary(&self, from: usize) -> usize {
        let mut i = from.saturating_sub(1);
        while i > 0 && !self.input.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_boundary(&self, from: usize) -> usize {
        let mut i = (from + 1).min(self.input.len());
        while i < self.input.len() && !self.input.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// readline word motion: skip the whitespace beside the caret, then the
    /// run of word characters (or the run of punctuation) it lands in.
    fn word_start_before(&self, mut at: usize) -> usize {
        let text = self.input.as_str();
        let prev = |i: usize| text[..i].chars().next_back().map(|c| (i - c.len_utf8(), c));
        while let Some((i, c)) = prev(at) {
            if !c.is_whitespace() {
                break;
            }
            at = i;
        }
        let Some((_, anchor)) = prev(at) else { return at };
        let want_word = Self::is_word_char(anchor);
        while let Some((i, c)) = prev(at) {
            if c.is_whitespace() || Self::is_word_char(c) != want_word {
                break;
            }
            at = i;
        }
        at
    }

    fn word_end_after(&self, mut at: usize) -> usize {
        let text = self.input.as_str();
        let next = |i: usize| text[i..].chars().next().map(|c| (i + c.len_utf8(), c));
        while let Some((i, c)) = next(at) {
            if !c.is_whitespace() {
                break;
            }
            at = i;
        }
        let Some((_, anchor)) = next(at) else { return at };
        let want_word = Self::is_word_char(anchor);
        while let Some((i, c)) = next(at) {
            if c.is_whitespace() || Self::is_word_char(c) != want_word {
                break;
            }
            at = i;
        }
        at
    }

    fn line_start(&self, at: usize) -> usize {
        self.input[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, at: usize) -> usize {
        self.input[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.input.len())
    }

    fn cursor_left(&mut self) {
        self.clamp_cursor();
        self.cursor = self.prev_boundary(self.cursor);
        self.typing_run = false;
    }

    fn cursor_right(&mut self) {
        self.clamp_cursor();
        self.cursor = self.next_boundary(self.cursor);
        self.typing_run = false;
    }

    fn cursor_word_left(&mut self) {
        self.clamp_cursor();
        self.cursor = self.word_start_before(self.cursor);
        self.typing_run = false;
    }

    fn cursor_word_right(&mut self) {
        self.clamp_cursor();
        self.cursor = self.word_end_after(self.cursor);
        self.typing_run = false;
    }

    fn cursor_line_start(&mut self) {
        self.clamp_cursor();
        self.cursor = self.line_start(self.cursor);
        self.typing_run = false;
    }

    fn cursor_line_end(&mut self) {
        self.clamp_cursor();
        self.cursor = self.line_end(self.cursor);
        self.typing_run = false;
    }

    /// Up/Down inside a multi-line draft, holding the column across the move.
    /// Returns false when there is no line to move onto, so the caller can
    /// fall back to history navigation.
    fn move_cursor_line(&mut self, delta: i32) -> bool {
        self.clamp_cursor();
        if !self.input.contains('\n') {
            return false;
        }
        let start = self.line_start(self.cursor);
        let col = self.input[start..self.cursor].chars().count();
        let target_start = if delta < 0 {
            if start == 0 {
                return false;
            }
            self.line_start(start - 1)
        } else {
            let end = self.line_end(self.cursor);
            if end >= self.input.len() {
                return false;
            }
            end + 1
        };
        let target_end = self.line_end(target_start);
        let mut at = target_start;
        for _ in 0..col {
            if at >= target_end {
                break;
            }
            at = self.next_boundary(at);
        }
        self.cursor = at.min(target_end);
        self.typing_run = false;
        true
    }

    fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_boundary(self.cursor);
        self.push_undo(true);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_forward(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        let end = self.next_boundary(self.cursor);
        self.push_undo(true);
        self.input.replace_range(self.cursor..end, "");
    }

    fn delete_word_before(&mut self) {
        self.clamp_cursor();
        let start = self.word_start_before(self.cursor);
        if start == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_word_after(&mut self) {
        self.clamp_cursor();
        let end = self.word_end_after(self.cursor);
        if end == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(self.cursor..end, "");
    }

    fn delete_to_line_start(&mut self) {
        self.clamp_cursor();
        let start = self.line_start(self.cursor);
        if start == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_to_line_end(&mut self) {
        self.clamp_cursor();
        let end = self.line_end(self.cursor);
        if end == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(self.cursor..end, "");
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.history_draft = self.input.clone();
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_cursor = Some(next);
        self.set_input(self.history[next].clone());
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_cursor = Some(next);
            self.set_input(self.history[next].clone());
        } else {
            self.history_cursor = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.set_input(draft);
        }
    }

    fn leave_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft.clear();
    }

    fn set_selected_fold(&mut self, folded: bool) {
        if let Some(index) = self.transcript.selected {
            if let Some(cell) = self.transcript.cells.get_mut(index) {
                cell.folded = folded;
            }
        }
    }

    /// Is this destructive action already armed and still inside the window?
    /// Consumes the arm either way, so a stale arm never fires later.
    fn armed(&mut self, what: Confirm) -> bool {
        let hit = matches!(self.confirm, Some((w, t)) if w == what && t.elapsed().as_millis() <= CONFIRM_MS);
        self.confirm = None;
        self.confirm_notice = false;
        hit
    }

    fn arm(&mut self, what: Confirm) {
        self.confirm = Some((what, Instant::now()));
        self.confirm_notice = true;
    }

    /// Called from the event loop's idle tick. Drops an expired confirm arm and
    /// the prompt that went with it — leaving "press again to quit" on screen
    /// after the window closed would be a lie.
    pub fn tick(&mut self) {
        let expired = matches!(self.confirm, Some((_, t)) if t.elapsed().as_millis() > CONFIRM_MS);
        if expired {
            self.confirm = None;
            if self.confirm_notice {
                self.notice = None;
                self.confirm_notice = false;
                self.needs_redraw = true;
            }
        }
    }

    fn select_first_block(&mut self) {
        self.transcript.selected = (!self.transcript.cells.is_empty()).then_some(0);
        self.scroll = usize::MAX;
    }

    fn select_last_block(&mut self) {
        self.transcript.selected = self.transcript.cells.len().checked_sub(1);
        self.scroll = 0;
    }
    fn toggle_multiline(&mut self) {
        self.multiline = !self.multiline;
        self.notice = Some(if self.multiline {
            "multiline on · Enter newline · Alt+Enter send".into()
        } else {
            "multiline off · Enter send".into()
        });
    }
    fn open_palette(&mut self) {
        if !self.catalog_loaded && !self.demo {
            let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                session_id: self.session_id.clone(),
            });
        }
        let row = |section: &'static str,
                   action: &str,
                   label: &str,
                   shortcut: Option<&str>| PaletteRow {
            label: label.to_string(),
            action: action.to_string(),
            shortcut: shortcut.map(str::to_string),
            section,
        };
        let mut rows = vec![
            row("Session", "New Session", "/new", Some("Ctrl+N")),
            row("Session", "Resume Session", "/resume", Some("Ctrl+S")),
            row("Session", "Session Info", "/session-info", None),
            row("Session", "Copy Last Response", "/copy", None),
            row("Session", "Quit", "/exit", Some("Ctrl+Q")),
            row("Context", "Rewind Conversation", "/rewind", Some("2×Esc")),
            row("Model & Input", "Toggle Multiline", "/multiline", Some("Ctrl+M")),
            row("Model & Input", "Prompt History", "/history", Some("Ctrl+R")),
            row("Appearance", "Switch Theme", "/theme", None),
            row("Appearance", "Keyboard Shortcuts", "/help", Some("Ctrl+X")),
            row("Panels", "Prompt Queue", "/queue", Some("Ctrl+;")),
            row("Panels", "Todos", "/todos", Some("Ctrl+T")),
        ];
        if !self.catalog_loaded || self.capabilities.models {
            rows.push(row("Model & Input", "Switch Model", "/model", Some("Ctrl+M")));
        }
        if !self.catalog_loaded || self.capabilities.compaction {
            rows.push(row("Context", "Compact History", "/compact", None));
        }
        if !self.catalog_loaded || self.capabilities.plan_mode {
            rows.push(row("Model & Input", "Enter Plan Mode", "/plan", None));
        }
        if !self.catalog_loaded || self.capabilities.permissions {
            rows.push(row(
                "Model & Input",
                "Toggle Always Approve",
                "/always-approve",
                Some("Shift+Tab"),
            ));
        }
        if !self.catalog_loaded || self.capabilities.jobs {
            rows.push(row("Panels", "Tasks", "/jobs", Some("Ctrl+G")));
        }
        if self.catalog_loaded {
            let enabled = [
                (self.capabilities.user_questions, "questions"),
                (self.capabilities.session_search, "session search"),
                (self.capabilities.commands, "commands"),
                (self.capabilities.tools, "tools"),
            ]
            .into_iter()
            .filter_map(|(present, label)| present.then_some(label))
            .collect::<Vec<_>>()
            .join(", ");
            if !enabled.is_empty() {
                rows.push(row(
                    "Harness",
                    &format!("Available: {enabled}"),
                    "/session-info",
                    None,
                ));
            }
        }
        rows.extend(self.harness_commands.iter().map(|command| PaletteRow {
            label: format!("/{}", command.name),
            action: command.description.clone(),
            shortcut: command.input_hint.clone(),
            section: "Harness",
        }));
        if self.demo {
            rows.extend([
                row("Demo", "Permission Dialog", "demo-approval", Some("F2")),
                row("Demo", "Question Dialog", "demo-question", Some("F3")),
            ]);
        }
        let visible = (0..rows.len()).collect();
        self.dialog = Dialog::Palette(Palette {
            rows,
            filter: String::new(),
            selected: 0,
            visible,
        });
    }

    fn open_shortcuts(&mut self) {
        use ShortcutSection::{Actions, Essentials, Input, Navigation, Panels, Session};
        let rows = vec![
            ShortcutRow {
                label: "Send",
                keys: "Enter",
                section: Essentials,
            },
            ShortcutRow {
                label: "Focus scrollback",
                keys: "Tab",
                section: Essentials,
            },
            ShortcutRow {
                label: "Cancel turn",
                keys: "Ctrl+C / Esc",
                section: Essentials,
            },
            ShortcutRow {
                label: "Cycle mode",
                keys: "Shift+Tab",
                section: Essentials,
            },
            ShortcutRow {
                label: "Quit",
                keys: "Ctrl+Q ×2 (Ctrl+D in composer)",
                section: Essentials,
            },
            ShortcutRow {
                label: "Command palette",
                keys: "Ctrl+P / ?",
                section: Essentials,
            },
            ShortcutRow {
                label: "Prompt queue",
                keys: "Ctrl+; / Ctrl+'",
                section: Panels,
            },
            ShortcutRow {
                label: "Send now",
                keys: "Ctrl+Enter / Ctrl+I",
                section: Actions,
            },
            ShortcutRow {
                label: "Prompt history",
                keys: "Ctrl+R / /history",
                section: Input,
            },
            ShortcutRow {
                label: "Keyboard shortcuts",
                keys: "Ctrl+X / Ctrl+.",
                section: Essentials,
            },
            ShortcutRow {
                label: "Toggle multiline",
                keys: "Ctrl+M",
                section: Input,
            },
            ShortcutRow {
                label: "Insert newline / send multiline",
                keys: "Shift+Enter / Alt+Enter",
                section: Input,
            },
            ShortcutRow {
                label: "Mention file",
                keys: "@",
                section: Input,
            },
            ShortcutRow {
                label: "Move caret",
                keys: "← / →",
                section: Input,
            },
            ShortcutRow {
                label: "Move caret by word",
                keys: "Alt+← / Alt+→ / Alt+B / Alt+F",
                section: Input,
            },
            ShortcutRow {
                label: "Start / end of line",
                keys: "Ctrl+A / Ctrl+E / Home / End",
                section: Input,
            },
            ShortcutRow {
                label: "Delete word",
                keys: "Ctrl+W / Alt+Backspace / Alt+D",
                section: Input,
            },
            ShortcutRow {
                label: "Delete to line start / end",
                keys: "Ctrl+U / Ctrl+K",
                section: Input,
            },
            ShortcutRow {
                label: "Undo edit",
                keys: "Ctrl+Z",
                section: Input,
            },
            ShortcutRow {
                label: "Prompt history",
                keys: "↑ / ↓",
                section: Input,
            },
            ShortcutRow {
                label: "Select block",
                keys: "↑ / ↓",
                section: Navigation,
            },
            ShortcutRow {
                label: "Previous / next turn",
                keys: "Shift+H / Shift+L",
                section: Navigation,
            },
            ShortcutRow {
                label: "Previous / next reply",
                keys: "Shift+K / Shift+J",
                section: Navigation,
            },
            ShortcutRow {
                label: "Scroll one line",
                keys: "Ctrl+K / Ctrl+J",
                section: Navigation,
            },
            ShortcutRow {
                label: "Scroll half page",
                keys: "Ctrl+U / Ctrl+D",
                section: Navigation,
            },
            ShortcutRow {
                label: "Collapse / expand block",
                keys: "← / → / h / l / e",
                section: Navigation,
            },
            ShortcutRow {
                label: "Collapse / expand all",
                keys: "Shift+E",
                section: Navigation,
            },
            ShortcutRow {
                label: "Open block fullscreen",
                keys: "Enter / Ctrl+F",
                section: Actions,
            },
            ShortcutRow {
                label: "Always-approve",
                keys: "Ctrl+O / Shift+Tab",
                section: Actions,
            },
            ShortcutRow {
                label: "Jump to top / bottom",
                keys: "g / G / Home / End",
                section: Navigation,
            },
            ShortcutRow {
                label: "Page conversation",
                keys: "PageUp / PageDown",
                section: Navigation,
            },
            ShortcutRow {
                label: "Fold all thoughts",
                keys: "Ctrl+E (scrollback)",
                section: Actions,
            },
            ShortcutRow {
                label: "Copy selected block",
                keys: "y",
                section: Actions,
            },
            ShortcutRow {
                label: "Rewind conversation",
                keys: "2×Esc",
                section: Actions,
            },
            ShortcutRow {
                label: "Model selector",
                keys: "Ctrl+M",
                section: Panels,
            },
            ShortcutRow {
                label: "Tasks",
                keys: "Ctrl+G",
                section: Panels,
            },
            ShortcutRow {
                label: "Todos",
                keys: "Ctrl+T",
                section: Panels,
            },
            ShortcutRow {
                label: "Theme",
                keys: "/theme",
                section: Panels,
            },
            ShortcutRow {
                label: "New session",
                keys: "Ctrl+N ×2",
                section: Session,
            },
            ShortcutRow {
                label: "Resume session",
                keys: "Ctrl+S",
                section: Session,
            },
        ];
        self.dialog = Dialog::Shortcuts(Shortcuts {
            rows,
            filter: String::new(),
            selected_section: 0,
            expanded: [true, false, false, false, false, false],
        });
    }

    fn cycle_permission_mode(&mut self) {
        self.permission_mode = match self.permission_mode {
            PermissionMode::Normal => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::AlwaysApprove,
            PermissionMode::AlwaysApprove => PermissionMode::Normal,
        };
        self.notice = Some(format!("mode: {}", self.permission_mode.label()));
        self.apply_permission_mode();
    }

    fn apply_permission_mode(&mut self) {
        if self.permission_presets.is_empty() {
            self.catalog_for_presets = true;
            let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                session_id: self.session_id.clone(),
            });
            return;
        }
        let named = |name: &str| {
            self.permission_presets
                .iter()
                .position(|preset| preset == name)
        };
        let target = match self.permission_mode {
            PermissionMode::Normal => named("workspace-write").or_else(|| {
                self.permission_presets
                    .iter()
                    .position(|preset| !preset.contains("danger") && !preset.contains("approve"))
            }),
            PermissionMode::Plan => named("read-only")
                .or_else(|| named("workspace-write"))
                .or_else(|| {
                    self.permission_presets.iter().position(|preset| {
                        !preset.contains("danger") && !preset.contains("approve")
                    })
                }),
            PermissionMode::AlwaysApprove => named("danger-full-access").or_else(|| {
                self.permission_presets
                    .iter()
                    .position(|preset| preset.contains("danger") || preset.contains("approve"))
            }),
        }
        .unwrap_or(self.preset_index);
        self.preset_index = target;
        let _ = self.cmd_tx.send(Cmd::SetMode {
            session_id: self.session_id.clone(),
            plan: self.permission_mode == PermissionMode::Plan,
            preset: self.permission_presets[target].clone(),
        });
    }

    fn open_rewind(&mut self) {
        let entries: Vec<RewindEntry> = self
            .transcript
            .turns
            .iter()
            .map(|t| {
                let preview = self
                    .transcript
                    .cells
                    .get(t.cell)
                    .map(|c| {
                        let one: String = c.text.replace('\n', " ").chars().take(48).collect();
                        one
                    })
                    .unwrap_or_default();
                RewindEntry {
                    seq: t.seq,
                    preview,
                }
            })
            .collect();
        if entries.is_empty() {
            self.notice = Some("no turns to rewind".into());
        } else {
            self.dialog = Dialog::Rewind(RewindPicker {
                items: entries,
                selected: 0,
            });
        }
    }

    fn copy_text(&mut self, text: String) {
        if text.is_empty() {
            self.notice = Some("nothing to copy".into());
            return;
        }
        let out = clipboard::copy(&text);
        if out.delivered {
            self.notice = Some("copied".into());
        } else {
            self.notice = Some(format!(
                "clipboard unreachable; saved to {}",
                out.backup.display()
            ));
        }
    }

    fn run_command(&mut self, cmd: &str) {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        match name {
            "/resume" => {
                let _ = self.cmd_tx.send(Cmd::ListLive);
                let mut items = list_sessions(&self.workspace, &self.session_id);
                items.retain(|it| !self.live_ids.contains(&it.id));
                if items.is_empty() {
                    self.notice = Some("no sessions found".into());
                } else {
                    self.dialog = Dialog::Resume(ResumePicker { items, selected: 0 });
                }
            }
            "/new" | "/clear" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                self.session_id = format!("dsh-{now}");
                self.transcript = Transcript::new();
                self.queue.clear();
                self.scroll = 0;
                self.status = "new session".into();
            }
            "/exit" | "/quit" => self.quit = true,
            "/copy" => {
                let text = self
                    .transcript
                    .cells
                    .iter()
                    .rev()
                    .find(|c| c.kind == CellKind::Assistant)
                    .map(|c| c.text.clone())
                    .unwrap_or_default();
                self.copy_text(text);
            }
            "/session-info" | "/context" | "/status" | "/info" => {
                let _ = self.cmd_tx.send(Cmd::SessionInfo {
                    session_id: self.session_id.clone(),
                });
            }
            "/theme" => {
                if self.has_dialog() {
                    self.dialog = Dialog::None;
                } else {
                    let original = self.theme;
                    let selected = if self.theme.name == "dark" { 0 } else { 1 };
                    self.dialog = Dialog::Theme(ThemePicker { original, selected });
                }
            }
            "/help" | "/shortcuts" => self.open_shortcuts(),
            "/model" | "/m" => {
                self.catalog_for_presets = false;
                self.status = "loading models".into();
                let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                    session_id: self.session_id.clone(),
                });
            }
            "/compact" => {
                self.status = "compacting".into();
                let _ = self.cmd_tx.send(Cmd::Compact {
                    session_id: self.session_id.clone(),
                });
            }
            "/rewind" | "/undo" => self.open_rewind(),
            "/multiline" | "/ml" => self.toggle_multiline(),
            "/history" => self.open_history(),
            "/queue" => self.open_queue(),
            "/todos" | "/todo" => self.open_todos(),
            "/jobs" | "/tasks" => {
                self.status = "loading tasks".into();
                let _ = self.cmd_tx.send(Cmd::FetchJobs);
            }
            "/plan" => {
                self.permission_mode = PermissionMode::Plan;
                self.notice = Some("mode: Plan".into());
                self.apply_permission_mode();
            }
            "/always-approve" => {
                self.permission_mode = if self.permission_mode == PermissionMode::AlwaysApprove {
                    PermissionMode::Normal
                } else {
                    PermissionMode::AlwaysApprove
                };
                self.notice = Some(format!("mode: {}", self.permission_mode.label()));
                self.apply_permission_mode();
            }
            other if other.starts_with('/') => {
                let _ = self.cmd_tx.send(Cmd::ExecuteCommand {
                    session_id: self.session_id.clone(),
                    line: cmd.to_string(),
                });
                self.status = format!("running Harness command {other}");
            }
            _ => self.notice = Some(format!("unknown command: {name}")),
        }
    }

    fn cancel_now(&mut self) {
        self.esc = EscArm::None;
        // docs/01 section 2.5: for ~1s after an Esc-triggered cancel the idle
        // rewind arm stays suppressed, so mashing Esc cannot open the picker.
        self.cancel_grace = Some(Instant::now());
        if self.demo {
            self.state = RunState::Idle;
            self.status = "cancelled (demo)".into();
            return;
        }
        let _ = self.cmd_tx.send(Cmd::Cancel {
            session_id: self.session_id.clone(),
        });
        self.status = "cancelling".into();
    }

    pub fn handle(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Rpc { method, params } => match method.as_str() {
                "session.event" => {
                    let sid = params
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(event) = params.get("event") {
                        if sid == self.session_id {
                            self.transcript.apply(event);
                        } else {
                            let t = self.child_transcripts.entry(sid.to_string()).or_default();
                            t.apply(event);
                        }
                    }
                }
                "session.projection" => {
                    // One key-agnostic channel for every projection, present and
                    // future (docs/04 section 3.3): a new harness projection
                    // needs no protocol change.
                    let sid = params
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if sid == self.session_id {
                        if let Some(key) = params.get("key").and_then(|v| v.as_str()) {
                            let value = params.get("value").unwrap_or(&Value::Null);
                            let seq = params.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                            self.projections.apply(key, value, seq);
                            self.sync_todos_from_projection();
                        }
                    }
                }
                "subagent.started" => {
                    let parent = params
                        .get("parentSessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let child = params
                        .get("childSessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if parent == self.session_id {
                        let i = self.transcript.push(
                            CellKind::Subagent,
                            "subagent".to_string(),
                            format!("started {child}"),
                        );
                        self.transcript.cells[i].link = Some(child.to_string());
                        self.transcript.selected = Some(i);
                        self.child_transcripts.entry(child.to_string()).or_default();
                    }
                }
                "subagent.finished" => {
                    let child = params
                        .get("childSessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(i) = self
                        .transcript
                        .cells
                        .iter()
                        .position(|c| c.link.as_deref() == Some(child))
                    {
                        let status = params
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("done");
                        let text = params
                            .get("lastAssistantMessage")
                            .and_then(Value::as_array)
                            .and_then(|blocks| content_text(blocks))
                            .unwrap_or_default();
                        self.transcript.cells[i].text = if text.is_empty() {
                            status.to_string()
                        } else {
                            format!("{status}: {text}")
                        };
                        self.transcript.selected = Some(i);
                    }
                }
                "session.status" => {
                    let sid = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if sid == self.session_id {
                        let running =
                            params.get("status").and_then(Value::as_str) == Some("running");
                        self.state = if running {
                            RunState::Running
                        } else {
                            RunState::Idle
                        };
                        self.status = if running {
                            "running".into()
                        } else {
                            "idle".into()
                        };
                    }
                }
                "session.queue" => self.replace_queue(&params),
                "tui/ready" => {
                    if let Some(current) = params
                        .get("server")
                        .and_then(|server| server.get("current"))
                    {
                        if let Some(provider) = current.get("provider").and_then(Value::as_str) {
                            self.provider = provider.to_string();
                        }
                        if let Some(model) = current.get("model").and_then(Value::as_str) {
                            self.model = model.to_string();
                        }
                        self.reasoning_effort = current
                            .get("reasoningEffort")
                            .and_then(Value::as_str)
                            .map(String::from);
                        self.reasoning_effort_name = self.reasoning_effort.clone();
                    }
                    self.status = "runtime ready".into();
                    self.state = RunState::Idle;
                    if !self.demo {
                        let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                            session_id: self.session_id.clone(),
                        });
                    }
                }
                "tui/catalog-result" => {
                    if let Some(names) = params.get("permissionPresets").and_then(Value::as_array) {
                        self.permission_presets = names
                            .iter()
                            .filter_map(|value| value.as_str().map(String::from))
                            .collect();
                    }
                    let current = params.get("current");
                    let current_provider = current
                        .and_then(|value| value.get("provider"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let capabilities = params.get("capabilities");
                    self.capabilities = HarnessCapabilities {
                        models: capability_flag(capabilities, "models"),
                        permissions: capability_flag(capabilities, "permissions"),
                        plan_mode: capability_flag(capabilities, "planMode"),
                        compaction: capability_flag(capabilities, "compaction"),
                        jobs: capability_flag(capabilities, "jobs"),
                        user_questions: capability_flag(capabilities, "userQuestions"),
                        session_search: capability_flag(capabilities, "sessionSearch"),
                        commands: capability_flag(capabilities, "commands"),
                        tools: capability_flag(capabilities, "tools"),
                    };
                    self.harness_commands = params
                        .get("commands")
                        .and_then(Value::as_array)
                        .map(|commands| {
                            commands
                                .iter()
                                .filter_map(|command| Some(HarnessCommand {
                                    name: command.get("name")?.as_str()?.to_string(),
                                    description: command
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    input_hint: command
                                        .get("inputHint")
                                        .and_then(Value::as_str)
                                        .map(String::from),
                                }))
                                .collect()
                        })
                        .unwrap_or_default();
                    self.catalog_loaded = true;
                    if matches!(self.dialog, Dialog::Palette(_)) {
                        self.open_palette();
                    }
                    let current_model = current
                        .and_then(|value| value.get("model"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let current_effort = current
                        .and_then(|value| value.get("reasoningEffort"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let mut rows: Vec<ModelEntry> = params
                        .get("models")
                        .and_then(Value::as_array)
                        .map(|models| {
                            models
                                .iter()
                                .filter_map(|model| {
                                    let reasoning = model
                                        .get("reasoning")
                                        .filter(|value| !value.is_null())
                                        .map(|value| ModelReasoning {
                                            efforts: value
                                                .get("efforts")
                                                .and_then(Value::as_array)
                                                .map(|efforts| {
                                                    efforts
                                                        .iter()
                                                        .filter_map(|effort| {
                                                            Some(ReasoningEffortEntry {
                                                                id: effort
                                                                    .get("id")?
                                                                    .as_str()?
                                                                    .to_string(),
                                                                name: effort
                                                                    .get("name")
                                                                    .and_then(Value::as_str)
                                                                    .unwrap_or("")
                                                                    .to_string(),
                                                                description: effort
                                                                    .get("description")
                                                                    .and_then(Value::as_str)
                                                                    .map(String::from),
                                                            })
                                                        })
                                                        .collect()
                                                })
                                                .unwrap_or_default(),
                                            default_effort: value
                                                .get("defaultEffort")
                                                .and_then(Value::as_str)
                                                .map(String::from),
                                        });
                                    Some(ModelEntry {
                                        provider: model.get("provider")?.as_str()?.to_string(),
                                        id: model.get("id")?.as_str()?.to_string(),
                                        name: model
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        description: model
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .map(String::from),
                                        context_window: model
                                            .get("contextWindow")
                                            .and_then(Value::as_u64),
                                        reasoning,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    rows.sort_by_key(|row| {
                        let current_provider = row.provider == current_provider;
                        let exact = current_provider && row.id == current_model;
                        (
                            if current_provider { 0 } else { 1 },
                            if exact { 0 } else { 1 },
                            row.id.clone(),
                        )
                    });
                    let selected = rows
                        .iter()
                        .position(|row| row.provider == current_provider && row.id == current_model)
                        .unwrap_or(0);
                    if let Some(row) = rows.get(selected) {
                        self.provider = current_provider.clone();
                        self.model = current_model.clone();
                        self.reasoning_effort = current_effort.or_else(|| {
                            row.reasoning
                                .as_ref()
                                .and_then(|reasoning| reasoning.default_effort.clone())
                        });
                        self.reasoning_effort_name = row.reasoning.as_ref().and_then(|reasoning| {
                            let current = self.reasoning_effort.as_deref()?;
                            reasoning
                                .efforts
                                .iter()
                                .find(|effort| effort.id == current)
                                .map(|effort| effort.name.clone())
                        });
                        self.context_window = row.context_window;
                    }
                    let for_presets = self.catalog_for_presets;
                    self.catalog_for_presets = false;
                    if for_presets {
                        self.notice =
                            Some(format!("{} presets loaded", self.permission_presets.len()));
                        self.apply_permission_mode();
                    } else if rows.is_empty() {
                        self.notice = Some("catalog empty".into());
                    } else {
                        self.dialog = Dialog::Model(ModelPicker {
                            rows,
                            filter: String::new(),
                            selected,
                        });
                    }
                }
                "tui/model-set" => {
                    if let Some(current) = params.get("current") {
                        let provider = current
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let model = current.get("model").and_then(Value::as_str).unwrap_or("?");
                        self.provider = provider.to_string();
                        self.model = model.to_string();
                        self.reasoning_effort = current
                            .get("reasoningEffort")
                            .and_then(Value::as_str)
                            .map(String::from);
                        self.reasoning_effort_name = self.reasoning_effort.clone();
                        self.status = format!("model: {provider}/{model}");
                    }
                }
                "tui.capabilities-changed" => {
                    let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                        session_id: self.session_id.clone(),
                    });
                }
                "tui/command-result" => {
                    let kind = params.get("kind").and_then(Value::as_str).unwrap_or("success");
                    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                    self.notice = Some(if text.is_empty() {
                        format!("Harness command {kind}")
                    } else {
                        format!("{kind}: {text}")
                    });
                }
                "tui/jobs-result" => {
                    let mut rows: Vec<TaskRow> = params
                        .get("jobs")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|j| {
                                    Some(TaskRow {
                                        id: j.get("id")?.as_str()?.to_string(),
                                        kind: j.get("kind")?.as_str()?.to_string(),
                                        label: j
                                            .get("label")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        status: j
                                            .get("status")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        detail: j
                                            .get("detail")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // 活跃子代理也列进来（与后台任务同视图）
                    for (child, t) in &self.child_transcripts {
                        if !rows.iter().any(|r| r.id == *child) {
                            let turns = t.cells.iter().filter(|c| c.kind == CellKind::User).count();
                            rows.push(TaskRow {
                                id: child.clone(),
                                kind: "subagent".to_string(),
                                label: format!("live child ({} msgs)", turns),
                                status: "running".to_string(),
                                detail: String::new(),
                            });
                        }
                    }
                    self.dialog = Dialog::Tasks(TasksView { rows, selected: 0 });
                }
                "tui/session-info-result" => {
                    let get = |k: &str| {
                        params
                            .get(k)
                            .map(|v| match v {
                                serde_json::Value::String(sv) => sv.clone(),
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::Null => "-".to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_else(|| "-".to_string())
                    };
                    let stats = self.transcript.stats;
                    let usage = self.transcript.usage;
                    let billed_input = usage
                        .input
                        .saturating_add(usage.cache)
                        .saturating_add(usage.cache_write);
                    let mut rows = vec![
                        ("session".to_string(), get("sessionId")),
                        (
                            "model".to_string(),
                            format!("{}/{}", get("provider"), get("model")),
                        ),
                        ("cwd".to_string(), get("cwd")),
                        (
                            "turns / steps".to_string(),
                            format!("{} / {}", stats.turns, stats.steps),
                        ),
                        (
                            "LLM / tools".to_string(),
                            format!("{} ms / {} ms", stats.llm_ms, stats.tool_ms),
                        ),
                        (
                            "TTFB / TPS".to_string(),
                            format!(
                                "{} ms / {:.1} tok/s",
                                stats.ttft_ms.checked_div(stats.ttft_steps).unwrap_or(0),
                                if stats.decode_ms == 0 {
                                    0.0
                                } else {
                                    stats.decode_tokens as f64 / (stats.decode_ms as f64 / 1000.0)
                                }
                            ),
                        ),
                        (
                            "cache hit".to_string(),
                            format!(
                                "{}%",
                                usage
                                    .cache
                                    .saturating_mul(100)
                                    .checked_div(billed_input)
                                    .unwrap_or(0)
                            ),
                        ),
                        (
                            "tokens".to_string(),
                            format!("in {} · out {}", billed_input, usage.output),
                        ),
                    ];
                    if stats.steps == 0 {
                        rows[3].1 = get("turns");
                    }
                    self.dialog = Dialog::Info(InfoDialog { rows });
                }
                "tui/rewound" => {
                    if let Some(new_id) = params.get("newSessionId").and_then(|v| v.as_str()) {
                        self.session_id = new_id.to_string();
                    }
                    if let Some(b) = params.get("boundary").and_then(|v| v.as_u64()) {
                        if let Some(t) = self.transcript.turns.iter().find(|t| t.seq == b) {
                            let cut = t.cell + 1;
                            self.transcript.cells.truncate(cut);
                            self.transcript.turns.retain(|m| m.seq <= b);
                            self.transcript.selected = None;
                        }
                    }
                    self.queue.clear();
                    self.scroll = 0;
                    self.status = "rewound".into();
                    self.notice = Some("rewound (new session continues here)".into());
                }
                "tui/compacted" => {
                    self.status = "compacted".into();
                    self.notice = Some("history compacted".into());
                }
                "tui/mode-set" => {
                    if let Some(preset) = params.get("applied").and_then(Value::as_str) {
                        if let Some(index) = self
                            .permission_presets
                            .iter()
                            .position(|candidate| candidate == preset)
                        {
                            self.preset_index = index;
                        }
                        let suffix = params
                            .get("planOutcome")
                            .and_then(Value::as_str)
                            .map(|outcome| format!(" · plan {outcome}"))
                            .unwrap_or_default();
                        self.status = format!("mode: {}{suffix}", self.permission_mode.label());
                    }
                }
                "tui/live-list" => {
                    if let Some(ids) = params.get("ids").and_then(|v| v.as_array()) {
                        self.live_ids = ids
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                    }
                    if let Dialog::Resume(p) = &mut self.dialog {
                        p.items.retain(|it| !self.live_ids.contains(&it.id));
                        if !p.items.is_empty() && p.selected >= p.items.len() {
                            p.selected = p.items.len() - 1;
                        }
                    }
                }
                "tui/loaded" => {
                    if let Some(sid) = params.get("sessionId").and_then(|v| v.as_str()) {
                        self.session_id = sid.to_string();
                    }
                    self.status = "resumed".into();
                    if let Some(file) = self.pending_resume_file.take() {
                        if let Ok(events) = read_session_events(&file) {
                            self.transcript = Transcript::new();
                            for ev in &events {
                                self.transcript.apply(ev);
                            }
                            self.scroll = 0;
                            self.notice = Some(format!("replayed {} events", events.len()));
                        }
                    }
                }
                _ => {}
            },
            AppEvent::ServerRequest { id, method, params } => {
                self.open_dialog(id, &method, &params);
            }
            AppEvent::Term(ev) => self.handle_key(ev),
            AppEvent::RuntimeStderr(line) => {
                self.notice = Some(line);
            }
            AppEvent::RuntimeExited(code) => {
                if !self.quit {
                    self.notice = Some(format!("runtime exited: {:?}", code));
                    self.state = RunState::Idle;
                }
            }
        }
        self.needs_redraw = true;
    }

    fn open_dialog(&mut self, id: String, method: &str, params: &Value) {
        match method {
            "ui/approve" => {
                let tool_name = params
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let reason = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = params
                    .get("input")
                    .filter(|v| !v.is_null())
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let options = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["allowed-once".into(), "rejected".into()]);
                // Busy UI fails closed rather than dropping the request.
                if self.has_dialog() {
                    self.respond(id, json!({ "outcome": "unavailable" }));
                    return;
                }
                self.dialog = Dialog::Approval(ApprovalDialog {
                    request_id: id,
                    tool_name,
                    reason,
                    input,
                    call_id,
                    options,
                    selected: 0,
                    parked: false,
                });
            }
            "ui/ask-user" => {
                let questions: Vec<Question> = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|q| {
                                Some(Question {
                                    id: q.get("id")?.as_str()?.to_string(),
                                    question: q
                                        .get("question")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    header: q
                                        .get("header")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    detail: q
                                        .get("detail")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    plan_approve: q
                                        .get("intent")
                                        .and_then(|i| i.get("approve"))
                                        .and_then(Value::as_str)
                                        .map(String::from),
                                    options: q
                                        .get("options")
                                        .and_then(Value::as_array)
                                        .map(|o| {
                                            o.iter()
                                                .filter_map(|v| {
                                                    v.get("label")
                                                        .and_then(Value::as_str)
                                                        .map(String::from)
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                    multi_select: q
                                        .get("multiSelect")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if self.has_dialog() {
                    self.respond(id, json!({ "error": "busy" }));
                    return;
                }
                let n = questions.len();
                self.dialog = Dialog::Ask(AskDialog {
                    request_id: id,
                    questions,
                    current: 0,
                    answers: vec![Vec::new(); n],
                    cursors: vec![0; n],
                    feedback: String::new(),
                    taking_feedback: false,
                    detail_scroll: 0,
                    custom_text: String::new(),
                    taking_text: false,
                    parked: false,
                });
            }
            _ => {
                // Unknown server request: answer null (bridge treats as delegate).
                self.respond(id, Value::Null);
            }
        }
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        if self.has_dialog() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.focus = Focus::Scrollback;
                self.follow_selection = false;
                self.scroll = self.scroll.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                self.focus = Focus::Scrollback;
                self.follow_selection = false;
                self.scroll = self.scroll.saturating_sub(3);
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if mouse.row >= self.composer_top {
                    self.focus = Focus::Prompt;
                    self.scroll = 0;
                } else {
                    self.focus = Focus::Scrollback;
                    self.follow_selection = true;
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, ev: Event) {
        // Bracketed paste: terminals wrap IME commits and Cmd/Ctrl+V paste in
        if let Event::Mouse(mouse) = ev {
            self.handle_mouse(mouse);
            return;
        }
        // ESC[200~ ... ESC[201~. crossterm decodes that to Event::Paste, and
        // dropping it here is what made Chinese input (and any pasted text)
        // never reach the composer on modern terminals.
        if let Event::Paste(text) = ev {
            self.handle_paste(text);
            return;
        }
        let Event::Key(key) = ev else { return };
        if key.kind != KeyEventKind::Press {
            return;
        }

        // ---- global quit (works even while a dialog is open) ----
        // Double-press within 1s, like Grok: a single stray Ctrl+Q used to take
        // the draft down with it. Ctrl+D only quits from the composer — in the
        // scrollback it is half-page-down (see the navigation block below).
        let quit_chord = key.modifiers.contains(KeyModifiers::CONTROL)
            && (key.code == KeyCode::Char('q')
                || key.code == KeyCode::Char('d') && self.focus != Focus::Scrollback);
        if quit_chord {
            if self.armed(Confirm::Quit) {
                self.quit = true;
            } else {
                self.arm(Confirm::Quit);
                self.notice = Some("press again to quit".into());
            }
            return;
        }

        // ---- blocking dialogs take over the keyboard (docs/01 section 2.4) ----
        // Parked approval/question cards stay visible while normal scrollback
        // navigation owns the keyboard. Tab returns focus to the card.
        let parked = matches!(&self.dialog, Dialog::Approval(d) if d.parked)
            || matches!(&self.dialog, Dialog::Ask(d) if d.parked);
        if parked {
            if key.code == KeyCode::Tab {
                match &mut self.dialog {
                    Dialog::Approval(d) => d.parked = false,
                    Dialog::Ask(d) => d.parked = false,
                    _ => {}
                }
                return;
            }
            // fall through to normal scrollback handling below
        } else if self.has_dialog() {
            self.dialog_key(key);
            return;
        }

        // ---- demo-only synthetic dialogs for testing ----
        if self.demo {
            if key.code == KeyCode::F(2) {
                self.open_dialog(
                    "demo-approve".into(),
                    "ui/approve",
                    &json!({ "toolName": "bash", "reason": "shell command", "input": {"command": "cargo test"}, "options": ["allowed-once", "rejected"] }),
                );
                return;
            }
            if key.code == KeyCode::F(3) {
                self.open_dialog(
                    "demo-ask".into(),
                    "ui/ask-user",
                    &json!({ "questions": [
                        { "id": "q1", "question": "选一个颜色？", "header": "主题", "options": [{"label": "蓝色"}, {"label": "绿色"}, {"label": "紫色"}] },
                        { "id": "q2", "question": "多选：喜欢哪些功能？", "options": [{"label": "审批"}, {"label": "子代理"}, {"label": "计划"}], "multiSelect": true }
                    ] }),
                );
                return;
            }
        }

        // ---- Esc semantics (docs/01 section 2.5) ----
        // Rapid ESC ESC collapses to Alt+Esc on most terminals; treat it as
        // the second press so double-Esc works whether spaced or instant.
        if key.code == KeyCode::Esc {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if self.is_running() {
                self.cancel_now();
                return;
            }
            if !self.input.is_empty() {
                if alt {
                    let dropped = self.take_input();
                    self.history.push(dropped);
                    self.leave_history_navigation();
                    self.esc = EscArm::None;
                } else {
                    let now = Instant::now();
                    match self.esc {
                        EscArm::ClearArmed(t)
                            if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS =>
                        {
                            let dropped = self.take_input();
                            self.history.push(dropped);
                            self.leave_history_navigation();
                            self.esc = EscArm::None;
                        }
                        _ => self.esc = EscArm::ClearArmed(now),
                    }
                }
                return;
            }
            if !self.transcript.is_empty() {
                let in_grace = self
                    .cancel_grace
                    .map(|t| t.elapsed().as_millis() < 1000)
                    .unwrap_or(false);
                if in_grace {
                    self.esc = EscArm::None;
                    return;
                }
                if alt {
                    self.esc = EscArm::None;
                    self.open_rewind();
                } else {
                    let now = Instant::now();
                    match self.esc {
                        EscArm::RewindArmed(t)
                            if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS =>
                        {
                            self.esc = EscArm::None;
                            self.open_rewind();
                        }
                        _ => self.esc = EscArm::RewindArmed(now),
                    }
                }
                return;
            }
            self.esc = EscArm::None;
            return;
        }

        // ---- Ctrl+C: clear draft first, then cancel (docs/01 section 2.5) ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.is_running() && !self.input.is_empty() {
                self.clear_input();
                self.leave_history_navigation();
            } else if self.is_running() {
                self.cancel_now();
            } else {
                self.clear_input();
                self.leave_history_navigation();
            }
            return;
        }

        // ---- keyboard shortcuts (Grok Ctrl+X / Ctrl+.) ----
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('.'))
        {
            self.open_shortcuts();
            return;
        }

        // ---- session shortcuts ----
        // /new discards the whole conversation, so Grok gates it the same way
        // as quit: press twice inside a second.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            if self.armed(Confirm::NewSession) {
                self.run_command("/new");
            } else {
                self.arm(Confirm::NewSession);
                self.notice = Some("press again for a new session".into());
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.run_command("/resume");
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.open_history();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(';') | KeyCode::Char('\''))
        {
            self.open_queue();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('4')
            && self.term_kind.is_vscode_family()
        {
            self.open_queue();
            return;
        }

        let send_now = if self.term_kind.is_vscode_family() {
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l')
        } else {
            key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Enter | KeyCode::Char('i'))
                || self.term_kind == TermKind::AppleTerminal
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('o')
        };
        if send_now {
            if !self.is_running() {
                return;
            }
            if self.input.is_empty() {
                let item_id = self.queued_items().next().map(|item| item.id.clone());
                if let Some(item_id) = item_id {
                    self.send_now_item(item_id);
                }
            } else {
                let text = self.take_input();
                self.send_now_text(text);
            }
            return;
        }

        // ---- always-approve toggle (Grok Ctrl+O) ----
        // Deliberately after `send_now`: on Apple Terminal Ctrl+O *is* the
        // send-now chord and has already been consumed above.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.run_command("/always-approve");
            return;
        }

        // ---- Ctrl+M is contextual: multiline in the composer, model picker elsewhere. ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('m') {
            if self.focus == Focus::Prompt {
                self.toggle_multiline();
            } else {
                self.catalog_for_presets = false;
                self.status = "loading models".into();
                let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                    session_id: self.session_id.clone(),
                });
            }
            return;
        }

        // ---- todos pane (Grok Ctrl+T) ----
        // Theme switching used to sit here, which stole Grok's todos binding.
        // It lives on /theme now (that picker has live preview + Esc revert).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.open_todos();
            return;
        }

        // ---- tasks pane (docs/01 section 10) ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            let _ = self.cmd_tx.send(Cmd::FetchJobs);
            return;
        }

        // ---- command palette (docs/01 section 2.6) ----
        // '?' is the palette alt binding only outside the composer, so a
        // question mark in a prompt keeps typing normally.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p')
            || key.code == KeyCode::Char('?') && self.focus == Focus::Scrollback
        {
            self.open_palette();
            return;
        }

        // ---- thinking 全局折叠/展开（Grok Ctrl+E）----
        // Contextual like Grok's Ctrl+M: in the composer Ctrl+E is the readline
        // end-of-line motion, so only the scrollback gets the fold toggle.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('e')
            && self.focus == Focus::Scrollback
        {
            let any_open = self
                .transcript
                .cells
                .iter()
                .any(|c| c.kind == CellKind::Thinking && !c.folded);
            let target = !any_open;
            for c in &mut self.transcript.cells {
                if c.kind == CellKind::Thinking {
                    c.folded = !target;
                }
            }
            self.notice = Some(if target {
                "thinking expanded".into()
            } else {
                "thinking collapsed".into()
            });
            return;
        }

        // ---- Explicit line break works in either composer mode. ----
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.leave_history_navigation();
            self.insert_char('\n');
            return;
        }

        // ---- slash commands (dispatch without a model turn) ----
        if key.code == KeyCode::Enter && !self.input.is_empty() && self.input.starts_with('/') {
            let cmd = self.take_input();
            self.run_command(&cmd);
            return;
        }

        // ---- send / queue / send-now ----
        if key.code == KeyCode::Enter {
            let multiline_send = self.multiline && key.modifiers.contains(KeyModifiers::ALT);
            if self.multiline && !multiline_send {
                if self.input.is_empty() && self.is_running() {
                    let item_id = self.queued_items().next().map(|item| item.id.clone());
                    if let Some(item_id) = item_id {
                        self.send_now_item(item_id);
                        return;
                    }
                }
                self.leave_history_navigation();
                self.insert_char('\n');
                return;
            }
            if self.input.is_empty() {
                if self.is_running() {
                    let item_id = self.queued_items().next().map(|item| item.id.clone());
                    if let Some(item_id) = item_id {
                        self.send_now_item(item_id);
                    }
                }
                return;
            }
            let text = self.take_input();
            self.send_input(text);
            return;
        }

        // ---- mode cycle: Normal → Plan → Always-approve ----
        if key.code == KeyCode::BackTab {
            self.cycle_permission_mode();
            return;
        }

        // ---- focus ----
        if key.code == KeyCode::Tab {
            self.focus = match self.focus {
                Focus::Prompt => {
                    self.follow_selection = true;
                    Focus::Scrollback
                }
                Focus::Scrollback => Focus::Prompt,
            };
            return;
        }

        // ---- scrollback navigation ----
        if self.focus == Focus::Scrollback {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // Viewport-only scrolling: these move the window without touching
            // the selection (Grok Ctrl+J/K by line, Ctrl+U/D by half page).
            if ctrl {
                let step = match key.code {
                    KeyCode::Char('k') => Some(1isize),
                    KeyCode::Char('j') => Some(-1),
                    KeyCode::Char('u') => Some(10),
                    KeyCode::Char('d') => Some(-10),
                    _ => None,
                };
                if let Some(step) = step {
                    self.follow_selection = false;
                    self.scroll = if step > 0 {
                        self.scroll.saturating_add(step as usize)
                    } else {
                        self.scroll.saturating_sub(step.unsigned_abs())
                    };
                    return;
                }
            }
            match key.code {
                // Shift+H/L (and Shift+arrows) walk turns; Shift+J/K walk replies.
                KeyCode::Char('H') | KeyCode::Left
                    if key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.follow_selection = true;
                    self.transcript
                        .move_selection_to_kind(CellKind::User, false);
                }
                KeyCode::Char('L') | KeyCode::Right
                    if key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.follow_selection = true;
                    self.transcript.move_selection_to_kind(CellKind::User, true);
                }
                KeyCode::Char('K') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.follow_selection = true;
                    self.transcript
                        .move_selection_to_kind(CellKind::Assistant, false);
                }
                KeyCode::Char('J') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.follow_selection = true;
                    self.transcript
                        .move_selection_to_kind(CellKind::Assistant, true);
                }
                KeyCode::Char('E') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    let expanded = self.transcript.toggle_all_folds();
                    self.notice = Some(if expanded {
                        "all expanded".into()
                    } else {
                        "all collapsed".into()
                    });
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.follow_selection = true;
                    self.transcript.move_selection(-1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.follow_selection = true;
                    self.transcript.move_selection(1);
                }
                KeyCode::Left | KeyCode::Char('h') => self.set_selected_fold(true),
                KeyCode::Right | KeyCode::Char('l') => self.set_selected_fold(false),
                KeyCode::Char('e') | KeyCode::Char(' ') => {
                    if let Some(i) = self.transcript.selected {
                        self.transcript.toggle_fold(i);
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(i) = self.transcript.selected {
                        self.transcript.toggle_raw(i);
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    self.follow_selection = true;
                    self.select_first_block();
                }
                KeyCode::Char('G') | KeyCode::End => {
                    self.follow_selection = true;
                    self.select_last_block();
                }
                KeyCode::PageUp => {
                    self.follow_selection = false;
                    self.scroll = self.scroll.saturating_add(10);
                }
                KeyCode::PageDown => {
                    self.follow_selection = false;
                    self.scroll = self.scroll.saturating_sub(10);
                }
                // Enter opens the block viewer; Ctrl+F is Grok's alt binding for
                // the same thing. A bare `f` still auto-focuses the composer.
                KeyCode::Enter | KeyCode::Char('f') if key.code == KeyCode::Enter || ctrl => {
                    if let Some(i) = self.transcript.selected {
                        let cell = self.transcript.cells[i].clone();
                        if let Some(link) = cell.link.clone() {
                            self.dialog = Dialog::Subagent(SubagentView {
                                child_id: link,
                                scroll: 0,
                            });
                        } else if matches!(cell.kind, CellKind::Tool | CellKind::ToolResult) {
                            self.dialog = Dialog::Block(BlockView { cell, scroll: 0 });
                        }
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(i) = self.transcript.selected {
                        let text = self.transcript.cells[i].text.clone();
                        self.copy_text(text);
                    }
                }
                KeyCode::Char('Y') => {
                    if let Some(i) = self.transcript.selected {
                        let title = self.transcript.cells[i].title.clone();
                        self.copy_text(title);
                    }
                }
                // Simple-mode auto-focus: a bare letter jumps to the composer
                // and starts typing. Chords must not, or Ctrl+V in the
                // scrollback would insert a literal "v".
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.focus = Focus::Prompt;
                    self.leave_history_navigation();
                    self.insert_char(c);
                }
                _ => {}
            }
            return;
        }

        // ---- prompt editing ----
        // readline motions and kills first, so a Ctrl/Alt chord never falls
        // through to the plain-character arm below.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // kills
            KeyCode::Char('w') if ctrl => {
                self.leave_history_navigation();
                self.delete_word_before();
                return;
            }
            KeyCode::Backspace if alt || ctrl => {
                self.leave_history_navigation();
                self.delete_word_before();
                return;
            }
            KeyCode::Char('d') if alt => {
                self.leave_history_navigation();
                self.delete_word_after();
                return;
            }
            KeyCode::Delete if alt || ctrl => {
                self.leave_history_navigation();
                self.delete_word_after();
                return;
            }
            KeyCode::Char('u') if ctrl => {
                self.leave_history_navigation();
                self.delete_to_line_start();
                return;
            }
            KeyCode::Char('k') if ctrl => {
                self.leave_history_navigation();
                self.delete_to_line_end();
                return;
            }
            KeyCode::Char('z') if ctrl => {
                self.undo_input();
                return;
            }
            // motions
            KeyCode::Char('a') if ctrl => {
                self.cursor_line_start();
                return;
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor_line_end();
                return;
            }
            KeyCode::Home => {
                self.cursor_line_start();
                return;
            }
            KeyCode::End => {
                self.cursor_line_end();
                return;
            }
            KeyCode::Left if alt || ctrl => {
                self.cursor_word_left();
                return;
            }
            KeyCode::Right if alt || ctrl => {
                self.cursor_word_right();
                return;
            }
            KeyCode::Char('b') if alt => {
                self.cursor_word_left();
                return;
            }
            KeyCode::Char('f') if alt => {
                self.cursor_word_right();
                return;
            }
            KeyCode::Left => {
                self.cursor_left();
                return;
            }
            KeyCode::Right => {
                self.cursor_right();
                return;
            }
            KeyCode::Delete => {
                self.leave_history_navigation();
                self.delete_forward();
                return;
            }
            _ => {}
        }
        match key.code {
            KeyCode::Char('/') if self.input.is_empty() => {
                self.leave_history_navigation();
                self.open_palette();
            }
            KeyCode::Char('@') => {
                self.leave_history_navigation();
                let files = list_files(&self.workspace);
                let visible = fuzzy_filter(&files, "");
                self.clamp_cursor();
                self.at_fragment_start = Some(self.cursor);
                self.insert_char('@');
                self.dialog = Dialog::FilePicker(FilePicker {
                    files,
                    query: String::new(),
                    selected: 0,
                    visible,
                });
            }
            KeyCode::Char(c) => {
                self.leave_history_navigation();
                self.insert_char(c);
            }
            KeyCode::Backspace => {
                self.leave_history_navigation();
                self.backspace();
            }
            // Up/Down walk the lines of a multi-line draft; on a single line
            // (or at the first/last line) they fall through to prompt history.
            KeyCode::Up => {
                if !self.move_cursor_line(-1) {
                    self.history_previous();
                }
            }
            KeyCode::Down => {
                if !self.move_cursor_line(1) {
                    self.history_next();
                }
            }
            KeyCode::Enter => {}
            _ => {}
        }
    }

    /// plan-review intent (docs/01 section 3.4): the plan markdown renders
    /// in a scrolled window; a approves, s requests changes with typed
    /// feedback, q abandons, c/y are TODO, arrows scroll the plan.
    fn plan_review_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let (request_id, qid, approve, other, taking, fb, scroll) = {
            let Dialog::Ask(d) = &self.dialog else { return };
            let cur = d.current.min(d.questions.len().saturating_sub(1));
            let q = &d.questions[cur];
            (
                d.request_id.clone(),
                q.id.clone(),
                q.plan_approve.clone().unwrap_or_default(),
                q.options
                    .iter()
                    .find(|o| Some(o.as_str()) != q.plan_approve.as_deref())
                    .cloned()
                    .unwrap_or_default(),
                d.taking_feedback,
                d.feedback.clone(),
                d.detail_scroll,
            )
        };
        if taking {
            match key.code {
                KeyCode::Enter => {
                    self.dialog = Dialog::None;
                    self.respond(
                        request_id,
                        json!({ "answers": [ { "id": qid, "selected": [other], "custom": fb } ] }),
                    );
                }
                KeyCode::Esc => {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.taking_feedback = false;
                    }
                }
                KeyCode::Backspace => {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.feedback.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.feedback.push(c);
                    }
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('a') => {
                self.dialog = Dialog::None;
                self.respond(
                    request_id,
                    json!({ "answers": [ { "id": qid, "selected": [approve] } ] }),
                );
            }
            KeyCode::Char('q') => {
                self.dialog = Dialog::None;
                self.respond(request_id, json!({ "answers": [] }));
            }
            KeyCode::Char('y') => {
                let detail = {
                    let Dialog::Ask(d) = &self.dialog else { return };
                    let cur = d.current.min(d.questions.len().saturating_sub(1));
                    d.questions[cur].detail.clone()
                };
                self.copy_text(detail);
            }
            KeyCode::Char('c') => {
                // Line comment: enter feedback mode prefixed with the line
                // under the detail cursor; comments ride the request-changes
                // (s) answer's custom field.
                let line_no = {
                    let Dialog::Ask(d) = &self.dialog else { return };
                    d.detail_scroll + 1
                };
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.feedback = format!("L{}: ", line_no);
                    d.taking_feedback = true;
                }
            }
            KeyCode::Char('s') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.taking_feedback = true;
                }
            }
            KeyCode::Enter => {}
            KeyCode::Esc => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.parked = true;
                }
                self.focus = Focus::Scrollback;
            }
            KeyCode::PageUp | KeyCode::Up | KeyCode::Char('k') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.detail_scroll =
                        scroll.saturating_sub(if key.code == KeyCode::PageUp { 6 } else { 1 });
                }
            }
            KeyCode::PageDown | KeyCode::Down | KeyCode::Char('j') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.detail_scroll =
                        scroll.saturating_add(if key.code == KeyCode::PageDown { 6 } else { 1 });
                }
            }
            _ => {}
        }
    }

    /// Route bracketed-paste text to the control that currently owns the
    /// keyboard. Outside dialogs this is the prompt composer; inside dialogs
    /// it is the active text field (ask_user free text / plan feedback /
    /// filter boxes). A parked approval card hands the keyboard back to the
    /// scrollback, so paste goes to the composer exactly like typed chars.
    fn handle_paste(&mut self, text: String) {
        // Decide the target before taking a mutable borrow on the dialog, so
        // the composer path can go through `insert_str` and land at the caret.
        let to_composer = match &self.dialog {
            Dialog::None => true,
            Dialog::Approval(d) => d.parked,
            Dialog::Ask(d) => d.parked,
            _ => false,
        };
        if to_composer {
            self.focus = Focus::Prompt;
            self.leave_history_navigation();
            self.insert_str(&text);
            return;
        }
        match &mut self.dialog {
            Dialog::History(d) => {
                d.query.push_str(&text);
                let query = d.query.clone();
                d.visible = history_matches(&self.history, &query);
                d.selected = 0;
            }
            Dialog::Ask(d) => {
                if d.taking_text {
                    d.custom_text.push_str(&text);
                } else if d.taking_feedback {
                    d.feedback.push_str(&text);
                }
            }
            Dialog::Model(d) => d.filter.push_str(&text),
            Dialog::Queue(d) if d.editing => d.draft.push_str(&text),
            Dialog::FilePicker(d) => d.query.push_str(&text),
            Dialog::Palette(d) => d.filter.push_str(&text),
            Dialog::Shortcuts(d) => d.filter.push_str(&text),
            _ => {}
        }
    }

    fn dialog_key(&mut self, key: crossterm::event::KeyEvent) {
        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.dialog {
            Dialog::Approval(d) => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        d.selected = d.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        d.selected = (d.selected + 1).min(d.options.len().saturating_sub(1));
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let n = c.to_digit(10).unwrap_or(10) as usize;
                        if n >= 1 && n <= d.options.len() {
                            d.selected = n - 1;
                        }
                    }
                    KeyCode::Enter => {
                        let chosen = d
                            .options
                            .get(d.selected)
                            .cloned()
                            .unwrap_or_else(|| "cancelled".into());
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        if chosen == "always-allow" {
                            // 本次放行，同时把完整模式切换为 Always-approve。
                            self.permission_mode = PermissionMode::AlwaysApprove;
                            self.apply_permission_mode();
                            self.respond(
                                id,
                                json!({ "outcome": "allowed-once", "remember": "always" }),
                            );
                        } else {
                            self.respond(id, json!({ "outcome": chosen }));
                        }
                    }
                    // docs/01 section 2.4: Esc parks focus in the scrollback
                    // without answering; Tab returns to the card.
                    KeyCode::Esc => {
                        d.parked = true;
                        self.focus = Focus::Scrollback;
                    }
                    KeyCode::Char('c') if has_ctrl => {
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        self.respond(id, json!({ "outcome": "cancelled" }));
                    }
                    _ => {}
                }
            }
            Dialog::Ask(_) => {
                let is_plan = match &self.dialog {
                    Dialog::Ask(d) => {
                        let cur = d.current.min(d.questions.len().saturating_sub(1));
                        d.questions[cur].plan_approve.is_some()
                    }
                    _ => false,
                };
                if is_plan {
                    self.plan_review_key(key);
                    return;
                }
                let d = match &mut self.dialog {
                    Dialog::Ask(d) => d,
                    _ => return,
                };
                let n = d.questions.len();
                let cur = d.current.min(n.saturating_sub(1));
                let opts = d.questions[cur].options.len();
                let multi = d.questions[cur].multi_select;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => {
                        d.current = cur.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(']') => {
                        d.current = (cur + 1).min(n.saturating_sub(1));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        d.cursors[cur] = d.cursors[cur].saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if opts > 0 {
                            d.cursors[cur] = (d.cursors[cur] + 1).min(opts - 1);
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let idx = c.to_digit(10).unwrap_or(10) as usize;
                        if idx >= 1 && idx <= opts {
                            d.cursors[cur] = idx - 1;
                            if multi {
                                let cursor = d.cursors[cur];
                                if d.answers[cur].contains(&cursor) {
                                    d.answers[cur].retain(|i| *i != cursor);
                                } else {
                                    d.answers[cur].push(cursor);
                                }
                            } else {
                                d.answers[cur] = vec![d.cursors[cur]];
                            }
                        }
                    }
                    KeyCode::Char(' ') if multi => {
                        let idx = d.cursors[cur].min(opts.saturating_sub(1));
                        if d.answers[cur].contains(&idx) {
                            d.answers[cur].retain(|i| *i != idx);
                        } else {
                            d.answers[cur].push(idx);
                        }
                    }
                    // Free-text answer (docs/01 section 2.4, z key): custom
                    // overrides the selection for single-select questions.
                    KeyCode::Char('z') => {
                        d.taking_text = true;
                    }
                    KeyCode::Char(c) if d.taking_text => {
                        d.custom_text.push(c);
                    }
                    KeyCode::Backspace => {
                        if d.taking_text {
                            d.custom_text.pop();
                        }
                    }
                    KeyCode::Enter if d.taking_text && d.custom_text.is_empty() => {}
                    KeyCode::Enter => {
                        if d.taking_text {
                            // submit with the typed answer
                            let custom = std::mem::take(&mut d.custom_text);
                            let qid = d.questions[cur].id.clone();
                            let id = d.request_id.clone();
                            self.dialog = Dialog::None;
                            self.respond(
                                id,
                                json!({ "answers": [ { "id": qid, "selected": [], "custom": custom } ] }),
                            );
                        } else if opts > 0 && d.answers[cur].is_empty() {
                            d.answers[cur] = vec![d.cursors[cur].min(opts - 1)];
                            if cur + 1 < n {
                                d.current = cur + 1;
                            } else {
                                let questions = d.questions.clone();
                                let answers: Vec<Value> = questions
                                    .iter()
                                    .enumerate()
                                    .map(|(i, q)| {
                                        let selected: Vec<String> = d.answers[i]
                                            .iter()
                                            .filter_map(|idx| q.options.get(*idx).cloned())
                                            .collect();
                                        json!({ "id": q.id, "selected": selected })
                                    })
                                    .collect();
                                let id = d.request_id.clone();
                                self.dialog = Dialog::None;
                                self.respond(id, json!({ "answers": answers }));
                            }
                        } else if cur + 1 < n {
                            d.current = cur + 1;
                        } else {
                            // Submit: build the canonical answer shape.
                            let questions = d.questions.clone();
                            let answers: Vec<Value> = questions
                                .iter()
                                .enumerate()
                                .map(|(i, q)| {
                                    let selected: Vec<String> = d.answers[i]
                                        .iter()
                                        .filter_map(|idx| q.options.get(*idx).cloned())
                                        .collect();
                                    json!({ "id": q.id, "selected": selected })
                                })
                                .collect();
                            let id = d.request_id.clone();
                            self.dialog = Dialog::None;
                            self.respond(id, json!({ "answers": answers }));
                        }
                    }
                    KeyCode::Esc => {
                        if d.taking_text {
                            d.taking_text = false;
                            d.custom_text.clear();
                        } else if d.has_pending_input() {
                            d.custom_text.clear();
                            d.feedback.clear();
                        } else {
                            d.parked = true;
                            self.focus = Focus::Scrollback;
                        }
                    }
                    _ => {}
                }
                let _ = has_ctrl;
            }
            Dialog::FilePicker(f) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    f.selected = f.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    f.selected = (f.selected + 1).min(f.visible.len().saturating_sub(1));
                }
                KeyCode::Char(c) => {
                    f.query.push(c);
                    f.visible = fuzzy_filter(&f.files, &f.query);
                    f.selected = 0;
                }
                KeyCode::Backspace => {
                    f.query.pop();
                    f.visible = fuzzy_filter(&f.files, &f.query);
                    f.selected = 0;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    if let Some(idx) = f.visible.get(f.selected) {
                        let path = f.files[*idx].clone();
                        if let Some(start) = self.at_fragment_start {
                            // Replace just the "@" we inserted, not everything
                            // after it — the caret can sit mid-draft now.
                            let start = start.min(self.input.len());
                            let end = self.next_boundary(start).min(self.input.len());
                            let insert = format!("{path} ");
                            self.input.replace_range(start..end, &insert);
                            self.cursor = start + insert.len();
                            self.typing_run = false;
                        }
                        self.at_fragment_start = None;
                        self.dialog = Dialog::None;
                        self.focus = Focus::Prompt;
                    }
                }
                KeyCode::Esc => {
                    self.at_fragment_start = None;
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Tasks(t) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    t.selected = t.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    t.selected = (t.selected + 1).min(t.rows.len().saturating_sub(1));
                }
                KeyCode::Char('r') => {
                    let _ = self.cmd_tx.send(Cmd::FetchJobs);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Todos(view) => {
                let last = self.transcript.todos.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        view.selected = (view.selected + 1).min(last);
                    }
                    KeyCode::Char('g') | KeyCode::Home => view.selected = 0,
                    KeyCode::Char('G') | KeyCode::End => view.selected = last,
                    KeyCode::Char('y') => {
                        let text = self
                            .transcript
                            .todos
                            .iter()
                            .map(|item| format!("{} {}", item.status.marker(), item.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.dialog = Dialog::None;
                        self.copy_text(text);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Queue(view) => {
                let queued = self
                    .queue
                    .iter()
                    .filter(|item| item.placement == "queued")
                    .cloned()
                    .collect::<Vec<_>>();
                if queued.is_empty() {
                    self.dialog = Dialog::None;
                    return;
                }
                if view.editing {
                    match key.code {
                        KeyCode::Char(c) => view.draft.push(c),
                        KeyCode::Backspace => {
                            view.draft.pop();
                        }
                        KeyCode::Enter if !view.draft.trim().is_empty() => {
                            let item_id = queued[view.selected].id.clone();
                            let text = std::mem::take(&mut view.draft);
                            view.editing = false;
                            self.queue_action(item_id, json!({ "kind": "edit", "text": text }));
                        }
                        KeyCode::Esc => {
                            view.editing = false;
                            view.draft.clear();
                        }
                        _ => {}
                    }
                    return;
                }
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        view.selected = (view.selected + 1).min(queued.len() - 1);
                    }
                    KeyCode::Char('e') => {
                        if let Some(text) = queued[view.selected].text.clone() {
                            view.editing = true;
                            view.draft = text;
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        let item_id = queued[view.selected].id.clone();
                        self.queue_action(item_id, json!({ "kind": "remove" }));
                    }
                    KeyCode::Char('s') => {
                        let item_id = queued[view.selected].id.clone();
                        self.queue_action(item_id, json!({ "kind": "steer" }));
                    }
                    KeyCode::Enter => {
                        let item_id = queued[view.selected].id.clone();
                        self.dialog = Dialog::None;
                        self.send_now_item(item_id);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Subagent(v) => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => {
                    v.scroll = v.scroll.saturating_add(1);
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown => {
                    v.scroll = v.scroll.saturating_sub(1);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Block(view) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    view.scroll = view.scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    view.scroll = view.scroll.saturating_add(10);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.scroll = view.scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    view.scroll = view.scroll.saturating_sub(10);
                }
                KeyCode::Char('r') => {
                    if !view.cell.raw_text.is_empty() {
                        view.cell.raw = !view.cell.raw;
                        view.scroll = 0;
                    }
                }
                KeyCode::Char('y') => {
                    let text = if view.cell.raw {
                        view.cell.raw_text.clone()
                    } else {
                        view.cell.text.clone()
                    };
                    self.copy_text(text);
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::History(view) => {
                let visible = view.visible.clone();
                match key.code {
                    KeyCode::Up => {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        view.selected = (view.selected + 1).min(visible.len().saturating_sub(1));
                    }
                    KeyCode::Char(c) => {
                        view.query.push(c);
                        let query = view.query.clone();
                        view.visible = history_matches(&self.history, &query);
                        view.selected = 0;
                    }
                    KeyCode::Backspace => {
                        view.query.pop();
                        let query = view.query.clone();
                        view.visible = history_matches(&self.history, &query);
                        view.selected = 0;
                    }
                    KeyCode::Enter | KeyCode::Tab => {
                        if let Some(index) = visible.get(view.selected).copied() {
                            self.set_input(self.history[index].clone());
                            self.history_cursor = Some(index);
                            self.history_draft.clear();
                            self.focus = Focus::Prompt;
                            self.dialog = Dialog::None;
                        }
                    }
                    KeyCode::Delete => {
                        if let Some(index) = visible.get(view.selected).copied() {
                            self.history.remove(index);
                            let query = view.query.clone();
                            view.visible = history_matches(&self.history, &query);
                            view.selected = view.selected.min(view.visible.len().saturating_sub(1));
                            if view.visible.is_empty() {
                                self.dialog = Dialog::None;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Info(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.dialog = Dialog::None;
                }
            }
            Dialog::Theme(t) => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    t.selected = 1 - t.selected;
                    self.theme = if t.selected == 0 {
                        crate::theme::DARK
                    } else {
                        crate::theme::LIGHT
                    };
                }
                KeyCode::Enter => {
                    self.dialog = Dialog::None;
                }
                KeyCode::Esc => {
                    self.theme = t.original;
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Palette(p) => match key.code {
                KeyCode::Up => {
                    p.selected = p.selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    p.selected = (p.selected + 1).min(p.visible.len().saturating_sub(1));
                }
                KeyCode::Char(c) => {
                    p.filter.push(c);
                    p.visible = palette_filter(&p.rows, &p.filter);
                    p.selected = 0;
                }
                KeyCode::Backspace => {
                    p.filter.pop();
                    p.visible = palette_filter(&p.rows, &p.filter);
                    p.selected = 0;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    if let Some(idx) = p.visible.get(p.selected) {
                        let label = p.rows[*idx].label.clone();
                        self.dialog = Dialog::None;
                        match label.as_str() {
                            command if command.starts_with('/') => self.run_command(command),
                            "demo-approval" => self.open_dialog(
                                "demo-approve".into(),
                                "ui/approve",
                                &serde_json::json!({ "toolName": "bash", "reason": "shell command", "input": {"command": "cargo test"}, "options": ["allowed-once", "rejected"] }),
                            ),
                            "demo-question" => self.open_dialog(
                                "demo-ask".into(),
                                "ui/ask-user",
                                &serde_json::json!({ "questions": [
                                    { "id": "q1", "question": "选一个颜色？", "header": "主题", "options": [{"label": "蓝色"}, {"label": "绿色"}] }
                                ] }),
                            ),
                            _ => {}
                        }
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Shortcuts(s) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    s.selected_section = s.selected_section.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    s.selected_section =
                        (s.selected_section + 1).min(ShortcutSection::ALL.len() - 1);
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('e') | KeyCode::Char(' ') => {
                    s.expanded[s.selected_section] = true;
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    s.expanded[s.selected_section] = false;
                }
                KeyCode::Char('/') | KeyCode::Char('f') => {
                    s.filter.clear();
                }
                KeyCode::Char('q') if s.filter.is_empty() => {
                    self.dialog = Dialog::None;
                }
                KeyCode::Char(c) => {
                    s.filter.push(c);
                }
                KeyCode::Backspace => {
                    s.filter.pop();
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Rewind(r) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    r.selected = r.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    r.selected = (r.selected + 1).min(r.items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(item) = r.items.get(r.selected) {
                        let session_id = self.session_id.clone();
                        let boundary = item.seq;
                        self.dialog = Dialog::None;
                        self.status = "rewinding".into();
                        let _ = self.cmd_tx.send(Cmd::Rewind {
                            session_id,
                            boundary,
                        });
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Model(m) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let visible = m.visible();
                    if !visible.is_empty() {
                        let position = visible
                            .iter()
                            .position(|index| *index == m.selected)
                            .unwrap_or(0);
                        m.selected = visible[position.saturating_sub(1)];
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let visible = m.visible();
                    if !visible.is_empty() {
                        let position = visible
                            .iter()
                            .position(|index| *index == m.selected)
                            .unwrap_or(0);
                        m.selected = visible[(position + 1).min(visible.len().saturating_sub(1))];
                    }
                }
                KeyCode::Char(c) => {
                    m.filter.push(c);
                    if let Some(first) = m.visible().first() {
                        m.selected = *first;
                    }
                }
                KeyCode::Backspace => {
                    m.filter.pop();
                    if let Some(first) = m.visible().first() {
                        m.selected = *first;
                    }
                }
                KeyCode::Enter => {
                    if let Some(model) = m.rows.get(m.selected).cloned() {
                        let efforts = model
                            .reasoning
                            .as_ref()
                            .map(|reasoning| reasoning.efforts.clone())
                            .unwrap_or_default();
                        let selected_effort = model.reasoning.as_ref().and_then(|reasoning| {
                            let selected_id =
                                if model.provider == self.provider && model.id == self.model {
                                    self.reasoning_effort.as_deref()
                                } else {
                                    reasoning.default_effort.as_deref()
                                };
                            selected_id
                                .and_then(|id| {
                                    reasoning.efforts.iter().find(|effort| effort.id == id)
                                })
                                .cloned()
                        });
                        if efforts.is_empty() {
                            self.context_window = model.context_window;
                            self.reasoning_effort_name = None;
                            self.dialog = Dialog::None;
                            self.status = "switching model".into();
                            let _ = self.cmd_tx.send(Cmd::SelectModel {
                                session_id: self.session_id.clone(),
                                provider: model.provider,
                                model: model.id,
                                reasoning_effort: None,
                            });
                        } else {
                            let selected = selected_effort
                                .as_ref()
                                .and_then(|selected| {
                                    efforts.iter().position(|effort| effort.id == selected.id)
                                })
                                .unwrap_or(0);
                            self.dialog = Dialog::Effort(EffortPicker {
                                model,
                                rows: efforts,
                                selected,
                            });
                        }
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Effort(effort) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    effort.selected = effort.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    effort.selected =
                        (effort.selected + 1).min(effort.rows.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(selected) = effort.rows.get(effort.selected).cloned() {
                        let model = effort.model.clone();
                        self.context_window = model.context_window;
                        self.reasoning_effort_name = Some(selected.name.clone());
                        self.dialog = Dialog::None;
                        self.status = "switching model".into();
                        let _ = self.cmd_tx.send(Cmd::SelectModel {
                            session_id: self.session_id.clone(),
                            provider: model.provider,
                            model: model.id,
                            reasoning_effort: Some(selected.id),
                        });
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Resume(p) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    p.selected = p.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    p.selected = (p.selected + 1).min(p.items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(item) = p.items.get(p.selected).cloned() {
                        let id = item.id.clone();
                        self.dialog = Dialog::None;
                        self.pending_resume_file = Some(item.file);
                        self.session_id = id.clone();
                        self.status = "resuming".into();
                        let _ = self.cmd_tx.send(Cmd::Load { session_id: id });
                    }
                }

                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::None => {}
        }
    }
}

fn history_matches(history: &[String], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    history
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, prompt)| {
            (query.is_empty() || prompt.to_lowercase().contains(&query)).then_some(index)
        })
        .collect()
}

fn queue_preview(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(200).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn queue_item_from_value(value: &Value) -> Option<QueueItem> {
    let message = value.get("message")?;
    let blocks = message.get("content")?.as_array()?;
    let mut preview_parts = Vec::with_capacity(blocks.len());
    let mut text = String::new();
    let mut text_only = true;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            let part = block.get("text").and_then(Value::as_str).unwrap_or_default();
            text.push_str(part);
            preview_parts.push(part.to_string());
        } else {
            text_only = false;
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("content");
            preview_parts.push(format!("[{kind}]"));
        }
    }
    let preview = queue_preview(&preview_parts.join(" "));
    Some(QueueItem {
        id: value.get("id")?.as_str()?.to_string(),
        placement: value.get("placement")?.as_str()?.to_string(),
        preview,
        text: text_only.then_some(text),
    })
}

fn capability_flag(capabilities: Option<&Value>, key: &str) -> bool {
    capabilities
        .and_then(|value| value.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn palette_filter(rows: &[PaletteRow], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    (0..rows.len())
        .filter(|i| {
            q.is_empty()
                || rows[*i].label.to_lowercase().contains(&q)
                || rows[*i].action.to_lowercase().contains(&q)
                || rows[*i].section.to_lowercase().contains(&q)
                || rows[*i]
                    .shortcut
                    .as_deref()
                    .is_some_and(|shortcut| shortcut.to_lowercase().contains(&q))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_app() -> App {
        let (tx, _rx) = std::sync::mpsc::channel::<Cmd>();
        App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        )
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, mods))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[test]
    fn caret_moves_and_edits_mid_draft() {
        let mut app = test_app();
        typed(&mut app, "cargo tst");
        // walk back over "tst" and repair it in place
        for _ in 0..2 {
            app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        }
        typed(&mut app, "e");
        assert_eq!(app.input, "cargo test");
        assert_eq!(app.cursor, "cargo te".len());

        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.cursor, 0);
        typed(&mut app, "> ");
        assert_eq!(app.input, "> cargo test");
    }

    #[test]
    fn caret_walks_multibyte_text_without_splitting_codepoints() {
        let mut app = test_app();
        typed(&mut app, "你好世界");
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        // two chars back from the end of four 3-byte chars
        assert_eq!(app.cursor, 6);
        typed(&mut app, "X");
        assert_eq!(app.input, "你好X世界");

        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "你好世界");
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "你世界");
    }

    #[test]
    fn readline_kills_and_word_motions() {
        let mut app = test_app();
        typed(&mut app, "fix the auth timeout");

        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "fix the auth ");

        app.handle_key(key(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.cursor, "fix the ".len());
        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "fix the ");

        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn ctrl_z_undoes_a_kill_and_a_typing_burst() {
        let mut app = test_app();
        typed(&mut app, "keep this");
        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());

        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "keep this");
        assert_eq!(app.cursor, "keep this".len());

        // the whole burst is one undo step, not nine
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
    }

    #[test]
    fn ctrl_e_ends_the_line_in_the_composer_but_folds_thinking_in_the_scrollback() {
        let mut app = test_app();
        app.transcript.push(
            CellKind::Thinking,
            "t".to_string(),
            "reasoning".to_string(),
        );
        for cell in &mut app.transcript.cells {
            cell.folded = false;
        }
        typed(&mut app, "abc");
        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.cursor, 3, "Ctrl+E is end-of-line while composing");
        assert!(app
            .transcript
            .cells
            .iter()
            .any(|c| c.kind == CellKind::Thinking && !c.folded));

        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(
            app.transcript
                .cells
                .iter()
                .all(|c| c.kind != CellKind::Thinking || c.folded),
            "Ctrl+E still folds thinking from the scrollback"
        );
    }

    #[test]
    fn scrollback_autofocus_ignores_control_chords() {
        let mut app = test_app();
        app.transcript.push(CellKind::Assistant, String::new(), "hi".to_string());
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty(), "Ctrl+V must not type a literal v");
        assert_eq!(app.focus, Focus::Scrollback);

        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.input, "v", "a bare letter still auto-focuses the composer");
    }

    #[test]
    fn up_down_walk_lines_of_a_multiline_draft_then_fall_back_to_history() {
        let mut app = test_app();
        app.history.push("older".into());
        app.set_input("one\ntwo");

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "one\ntwo", "still editing, not recalling");
        assert_eq!(app.cursor, 3, "column held at end of the shorter first line");

        // already on the first line, so Up now reaches for history
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "older");
    }

    #[test]
    fn ctrl_t_opens_todos_instead_of_flipping_the_theme() {
        let mut app = test_app();
        let before = app.theme.name;

        // no snapshot yet: it must not open an empty pane, and must not
        // silently fall back to the old theme toggle
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(matches!(app.dialog, Dialog::None));
        assert_eq!(app.theme.name, before, "Ctrl+T is no longer a theme toggle");

        app.transcript.apply(&json!({
            "type": "tool/call", "seq": 1, "time": 10,
            "data": {"callId": "c1", "name": "todo_write",
                     "arguments": "{\"todos\":[{\"content\":\"a\",\"status\":\"completed\"},{\"content\":\"b\",\"status\":\"in_progress\"}]}"}
        }));
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let Dialog::Todos(view) = &app.dialog else {
            panic!("expected the todos pane, got {:?}", app.dialog);
        };
        assert_eq!(view.selected, 1, "opens on the in-progress row");
        assert_eq!(app.theme.name, before);

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.dialog, Dialog::None));
    }

    #[test]
    fn shortcut_panel_opens_and_mode_cycles() {
        let mut app = test_app();
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )));
        assert!(matches!(app.dialog, Dialog::Shortcuts(_)));

        app.dialog = Dialog::None;
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.permission_mode, PermissionMode::Plan);
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.permission_mode, PermissionMode::AlwaysApprove);
    }

    #[test]
    fn mode_cycle_drives_plan_state_and_matching_permission_preset() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.permission_presets = vec![
            "read-only".into(),
            "workspace-write".into(),
            "danger-full-access".into(),
        ];

        app.cycle_permission_mode();
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected plan mode command");
        };
        assert!(plan);
        assert_eq!(preset, "read-only");

        app.cycle_permission_mode();
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected always-approve mode command");
        };
        assert!(!plan);
        assert_eq!(preset, "danger-full-access");

        app.cycle_permission_mode();
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected normal mode command");
        };
        assert!(!plan);
        assert_eq!(preset, "workspace-write");
    }

    #[test]
    fn session_shortcuts_execute_without_typing_commands() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Assistant, String::new(), "done");
        // Ctrl+N discards the conversation, so the first press only arms it.
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(
            !app.transcript.is_empty(),
            "a single Ctrl+N must not wipe the session"
        );
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn an_expired_confirm_prompt_clears_itself_on_tick() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.notice.as_deref(), Some("press again to quit"));

        // still inside the window: the prompt stays put
        app.tick();
        assert_eq!(app.notice.as_deref(), Some("press again to quit"));

        app.confirm = Some((
            Confirm::Quit,
            Instant::now() - std::time::Duration::from_millis(CONFIRM_MS as u64 + 50),
        ));
        app.tick();
        assert!(app.notice.is_none(), "stale confirm prompt must disappear");
        assert!(app.confirm.is_none());
        assert!(!app.quit);

        // an unrelated notice must survive a tick
        app.notice = Some("theme: light".into());
        app.tick();
        assert_eq!(app.notice.as_deref(), Some("theme: light"));
    }

    #[test]
    fn quit_needs_a_second_press_and_a_stale_arm_expires() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit, "one Ctrl+Q must not drop the draft on the floor");
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.quit);

        // an arm that timed out must not be cashed in later
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        app.confirm = Some((
            Confirm::Quit,
            Instant::now() - std::time::Duration::from_millis(CONFIRM_MS as u64 + 200),
        ));
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit, "expired arm should re-arm, not quit");

        // and a different action cannot satisfy the other's arm
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl+N's arm must not authorise a quit");
    }

    #[test]
    fn ctrl_d_scrolls_in_the_scrollback_but_quits_from_the_composer() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Assistant, String::new(), "body");
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl+D pages the scrollback, it does not quit");

        app.focus = Focus::Prompt;
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.quit, "from the composer Ctrl+D is the quit alias");
    }

    #[test]
    fn shift_navigation_jumps_between_turns_and_replies() {
        let mut app = test_app();
        for (kind, text) in [
            (CellKind::User, "ask one"),
            (CellKind::Assistant, "reply one"),
            (CellKind::Tool, "tool"),
            (CellKind::User, "ask two"),
            (CellKind::Assistant, "reply two"),
        ] {
            app.transcript.push(kind, String::new(), text.to_string());
        }
        app.focus = Focus::Scrollback;
        app.transcript.selected = Some(4);

        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(3), "previous turn");
        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(0), "the turn before that");
        // no earlier turn: stay put rather than wrap
        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(0));

        app.handle_key(key(KeyCode::Char('J'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(1), "next assistant reply");
        app.handle_key(key(KeyCode::Char('J'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(4));
    }

    #[test]
    fn shift_e_toggles_every_fold_at_once() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Tool, "a".to_string(), "x".to_string());
        app.transcript
            .push(CellKind::Tool, "b".to_string(), "y".to_string());
        app.transcript.cells[0].folded = false;
        app.transcript.cells[1].folded = true;
        app.focus = Focus::Scrollback;

        // something is open, so collapse everything
        app.handle_key(key(KeyCode::Char('E'), KeyModifiers::SHIFT));
        assert!(app.transcript.cells.iter().all(|c| c.folded));
        // now everything is shut, so expand
        app.handle_key(key(KeyCode::Char('E'), KeyModifiers::SHIFT));
        assert!(app.transcript.cells.iter().all(|c| !c.folded));
    }

    #[test]
    fn ctrl_jk_and_ud_scroll_without_moving_the_selection() {
        let mut app = test_app();
        for i in 0..40 {
            app.transcript
                .push(CellKind::Assistant, String::new(), format!("line {i}"));
        }
        app.focus = Focus::Scrollback;
        app.transcript.selected = Some(20);
        app.scroll = 20;

        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 21, "Ctrl+K scrolls up one line");
        assert_eq!(app.transcript.selected, Some(20), "selection unchanged");

        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 20, "Ctrl+J scrolls back down");

        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 30, "Ctrl+U is half a page up");
        assert_eq!(app.transcript.selected, Some(20));
    }

    #[test]
    fn ctrl_f_opens_the_block_viewer_but_a_bare_f_types() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Tool, "bash".to_string(), "output".to_string());
        app.focus = Focus::Scrollback;
        app.transcript.selected = Some(0);

        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(
            matches!(app.dialog, Dialog::Block(_)),
            "Ctrl+F opens the viewer, got {:?}",
            app.dialog
        );

        app.dialog = Dialog::None;
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(app.input, "f", "a bare f still auto-focuses and types");
    }

    #[test]
    fn palette_filters_labels_and_actions() {
        let rows = vec![
            PaletteRow {
                label: "/resume".into(),
                action: "Resume Session".into(),
                shortcut: Some("Ctrl+S".into()),
                section: "Session",
            },
            PaletteRow {
                label: "/model".into(),
                action: "Switch Model".into(),
                shortcut: Some("Ctrl+M".into()),
                section: "Model & Input",
            },
        ];
        assert_eq!(palette_filter(&rows, "model"), vec![1]);
        assert_eq!(palette_filter(&rows, "resume"), vec![0]);
        assert_eq!(palette_filter(&rows, "ctrl+s"), vec![0]);
        assert_eq!(palette_filter(&rows, "session"), vec![0]);
        assert_eq!(palette_filter(&rows, "").len(), 2);
    }

    #[test]
    fn bracketed_paste_lands_in_composer() {
        let mut app = test_app();
        app.handle_key(Event::Paste("你好世界".into()));
        assert_eq!(app.input, "你好世界");
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn child_status_does_not_override_active_session() {
        let mut app = test_app();
        app.handle(AppEvent::Rpc {
            method: "session.status".into(),
            params: json!({"sessionId": "child", "status": "running"}),
        });
        assert_eq!(app.state, RunState::Idle);

        app.handle(AppEvent::Rpc {
            method: "session.status".into(),
            params: json!({"sessionId": "s1", "status": "running"}),
        });
        assert_eq!(app.state, RunState::Running);
    }

    #[test]
    fn subagent_finish_extracts_content_blocks() {
        let mut app = test_app();
        app.handle(AppEvent::Rpc {
            method: "subagent.started".into(),
            params: json!({"parentSessionId": "s1", "childSessionId": "child"}),
        });
        app.handle(AppEvent::Rpc {
            method: "subagent.finished".into(),
            params: json!({
                "childSessionId": "child",
                "status": "ok",
                "lastAssistantMessage": [{"type": "text", "text": "done"}]
            }),
        });

        let cell = app
            .transcript
            .cells
            .iter()
            .find(|cell| cell.link.as_deref() == Some("child"))
            .unwrap();
        assert_eq!(cell.text, "ok: done");
    }
    #[test]
    fn ask_enter_commits_focused_option() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "request-1".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "continue",
                    "question": "是否继续？",
                    "options": [{"label": "继续"}, {"label": "停止"}]
                }]
            }),
        );

        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::Respond { id, result } = rx.recv().unwrap() else {
            panic!("expected a dialog response");
        };
        assert_eq!(id, "request-1");
        assert_eq!(
            result,
            json!({"answers": [{"id": "continue", "selected": ["继续"]}]})
        );
    }
    #[test]
    fn history_search_refills_prompt_for_editing() {
        let mut app = test_app();
        app.history = vec![
            "fix authentication timeout".into(),
            "write release notes".into(),
            "inspect authentication logs".into(),
        ];

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        )));
        for c in "auth".chars() {
            app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        let Dialog::History(view) = &app.dialog else {
            panic!("expected history search");
        };
        assert_eq!(view.visible, vec![2, 0]);

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input, "fix authentication timeout");
        assert_eq!(app.history_cursor, Some(0));
        assert!(matches!(app.dialog, Dialog::None));

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input, "fix authentication timeout!");
        assert_eq!(app.history_cursor, None);
    }


    #[test]
    fn ask_arrow_moves_cursor_before_submit() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "request-2".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "continue",
                    "question": "是否继续？",
                    "options": [{"label": "继续"}, {"label": "停止"}]
                }]
            }),
        );
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::Respond { result, .. } = rx.recv().unwrap() else {
            panic!("expected a dialog response");
        };
        assert_eq!(
            result,
            json!({"answers": [{"id": "continue", "selected": ["停止"]}]})
        );
    }
    #[test]
    fn ask_escape_parks_without_responding_and_tab_restores_focus() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "request-park".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "continue",
                    "question": "是否继续？",
                    "options": [{"label": "继续"}, {"label": "停止"}]
                }]
            }),
        );

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        let Dialog::Ask(dialog) = &app.dialog else {
            panic!("expected parked question card");
        };
        assert!(dialog.parked);
        assert_eq!(app.focus, Focus::Scrollback);
        assert!(rx.try_recv().is_err());

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
        let Dialog::Ask(dialog) = &app.dialog else {
            panic!("expected focused question card");
        };
        assert!(!dialog.parked);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn multiline_mode_uses_enter_for_newline_and_alt_enter_for_send() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.set_input("first");

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL,
        )));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input, "first\ns");

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )));
        let Cmd::Prompt { text, .. } = rx.recv().unwrap() else {
            panic!("expected prompt command");
        };
        assert_eq!(text, "first\ns");
    }

    #[test]
    fn prompt_history_walks_both_directions_and_restores_draft() {
        let mut app = test_app();
        app.history = vec!["one".into(), "two".into()];
        app.set_input("draft");

        for code in [KeyCode::Up, KeyCode::Up, KeyCode::Down, KeyCode::Down] {
            app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
                code,
                KeyModifiers::NONE,
            )));
        }

        assert_eq!(app.input, "draft");
        assert_eq!(app.history_cursor, None);
    }

    #[test]
    fn scrollback_fold_keys_are_directional_and_g_jumps_to_top() {
        let mut app = test_app();
        app.transcript.push(CellKind::Tool, "first", "first output");
        app.transcript
            .push(CellKind::Tool, "second", "second output");
        app.focus = Focus::Scrollback;

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.transcript.selected, Some(0));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert!(!app.transcript.cells[0].folded);
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        assert!(app.transcript.cells[0].folded);
    }

    #[test]
    fn catalog_model_choice_requires_effort_then_submits_complete_selection() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "session".into(),
            "old-provider".into(),
            "old-model".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.handle(AppEvent::Rpc {
            method: "tui/catalog-result".into(),
            params: json!({
                "permissionPresets": [],
                "current": {
                    "provider": "asxs",
                    "model": "gpt-5.6-sol",
                    "reasoningEffort": "low"
                },
                "models": [{
                    "provider": "asxs",
                    "id": "gpt-5.6-sol",
                    "name": "GPT 5.6 Sol",
                    "description": "coding model",
                    "contextWindow": 400000,
                    "reasoning": {
                        "efforts": [
                            {"id": "low", "name": "Low"},
                            {"id": "high", "name": "High"}
                        ],
                        "defaultEffort": "high"
                    }
                }]
            }),
        });

        assert_eq!(app.reasoning_effort_name.as_deref(), Some("Low"));
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        let Dialog::Effort(effort) = &app.dialog else {
            panic!("expected effort picker");
        };
        assert_eq!(effort.selected, 0);
        assert_eq!(effort.rows[effort.selected].id, "low");
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::SelectModel {
            session_id,
            provider,
            model,
            reasoning_effort,
        } = rx.recv().unwrap()
        else {
            panic!("expected model selection");
        };
        assert_eq!(session_id, "session");
        assert_eq!(provider, "asxs");
        assert_eq!(model, "gpt-5.6-sol");
        assert_eq!(reasoning_effort.as_deref(), Some("high"));
        assert_eq!(app.reasoning_effort_name.as_deref(), Some("High"));
        assert_eq!(app.context_window, Some(400000));
    }
    #[test]
    fn plan_review_scrolls_forward_and_feedback_captures_navigation_keys() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "plan-1".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "plan",
                    "question": "Review plan",
                    "detail": "one\ntwo\nthree",
                    "intent": {"approve": "Approve"},
                    "options": [{"label": "Approve"}, {"label": "Request changes"}]
                }]
            }),
        );

        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        let Dialog::Ask(dialog) = &app.dialog else {
            panic!("expected plan review");
        };
        assert_eq!(dialog.detail_scroll, 1);

        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        ));
        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
        ));
        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::Respond { id, result } = rx.recv().unwrap() else {
            panic!("expected plan response");
        };
        assert_eq!(id, "plan-1");
        assert_eq!(
            result,
            json!({
                "answers": [{
                    "id": "plan",
                    "selected": ["Request changes"],
                    "custom": "j"
                }]
            })
        );
    }
    #[test]
    fn slash_commands_expose_rewind_jobs_and_mode_toggles() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.permission_presets = vec![
            "read-only".into(),
            "workspace-write".into(),
            "danger-full-access".into(),
        ];
        app.transcript.push(CellKind::User, String::new(), "prompt");
        app.transcript
            .turns
            .push(crate::transcript::TurnMarker { seq: 7, cell: 0 });

        app.run_command("/rewind");
        assert!(matches!(app.dialog, Dialog::Rewind(_)));
        app.dialog = Dialog::None;

        app.run_command("/jobs");
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchJobs));

        app.run_command("/plan implement auth");
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected plan mode command");
        };
        assert!(plan);
        assert_eq!(preset, "read-only");

        app.run_command("/always-approve");
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected always-approve command");
        };
        assert!(!plan);
        assert_eq!(preset, "danger-full-access");

        app.run_command("/always-approve");
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected normal mode command");
        };
        assert!(!plan);
        assert_eq!(preset, "workspace-write");
    }

    #[test]
    fn catalog_capabilities_drive_palette_and_harness_commands() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "session".into(),
            "provider".into(),
            "model".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.handle(AppEvent::Rpc {
            method: "tui/catalog-result".into(),
            params: json!({
                "permissionPresets": null,
                "capabilities": {
                    "models": false,
                    "permissions": false,
                    "planMode": false,
                    "compaction": false,
                    "jobs": false,
                    "userQuestions": true,
                    "sessionSearch": true,
                    "commands": true,
                    "tools": true
                },
                "commands": [{
                    "name": "goal",
                    "description": "Manage the active goal",
                    "inputHint": "create|get|complete"
                }],
                "current": {"provider": "provider", "model": "model"},
                "models": []
            }),
        });
        app.open_palette();
        let Dialog::Palette(palette) = &app.dialog else {
            panic!("expected capability palette");
        };
        assert!(palette.rows.iter().any(|row| row.label == "/goal"));
        assert!(!palette.rows.iter().any(|row| row.label == "/jobs"));
        assert!(!palette.rows.iter().any(|row| row.label == "/compact"));
        assert!(!palette.rows.iter().any(|row| row.label == "/model"));

        app.dialog = Dialog::None;
        app.run_command("/goal get");
        let Cmd::ExecuteCommand { session_id, line } = rx.recv().unwrap() else {
            panic!("expected Harness command execution");
        };
        assert_eq!(session_id, "session");
        assert_eq!(line, "/goal get");
    }

    #[test]
    fn slash_opens_filtered_command_menu_and_tab_executes_selection() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let Dialog::Palette(palette) = &app.dialog else {
            panic!("expected slash command menu");
        };
        assert!(app.input.is_empty());
        assert_eq!(palette.filter, "");

        for c in "jobs".chars() {
            app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        let Dialog::Palette(palette) = &app.dialog else {
            panic!("expected filtered slash command menu");
        };
        assert_eq!(palette.visible.len(), 1);
        assert_eq!(palette.rows[palette.visible[0]].label, "/jobs");

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchCatalog { .. }));
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchJobs));
        assert!(matches!(app.dialog, Dialog::None));
    }
}
