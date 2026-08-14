//! App state machine: run state, focus, Esc semantics (docs/01 section 2.5),
//! follow-up queue, selection/scroll, and the blocking dialogs (approval
//! prompt + ask_user_question card, docs/01 section 2.4).

use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};

use crate::bus::{AppEvent, Cmd};
use crate::resume::{list_sessions, read_session_events, SessionSummary};
use crate::theme::Theme;
use crate::transcript::{CellKind, Transcript};

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

/// Permission prompt (docs/01 section 3.3). Options come from the bridge:
/// allowed-once / rejected (+ always-* rows later).
#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub request_id: String,
    pub tool_name: String,
    pub reason: String,
    pub input: String,
    pub options: Vec<String>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub question: String,
    pub header: String,
    pub options: Vec<String>,
    pub multi_select: bool,
}

/// ask_user_question card (docs/01 section 2.4).
#[derive(Debug, Clone)]
pub struct AskDialog {
    pub request_id: String,
    pub questions: Vec<Question>,
    pub current: usize,
    /// Chosen option indices per question (empty until answered).
    pub answers: Vec<Vec<usize>>,
}

#[derive(Debug, Clone)]
pub struct ResumePicker {
    pub items: Vec<SessionSummary>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub enum Dialog {
    None,
    Approval(ApprovalDialog),
    Ask(AskDialog),
    Resume(ResumePicker),
}

pub struct App {
    pub theme: Theme,
    pub transcript: Transcript,
    pub input: String,
    pub history: Vec<String>,
    pub focus: Focus,
    pub state: RunState,
    pub esc: EscArm,
    pub session_id: String,
    pub model: String,
    pub status: String,
    pub notice: Option<String>,
    pub scroll: usize,
    pub needs_redraw: bool,
    pub quit: bool,
    pub demo: bool,
    pub dialog: Dialog,
    pub workspace: String,
    pending_resume_file: Option<std::path::PathBuf>,
    queue: Vec<String>,
    cmd_tx: Sender<Cmd>,
}

impl App {
    pub fn new(
        theme: Theme,
        session_id: String,
        model: String,
        demo: bool,
        cmd_tx: Sender<Cmd>,
        workspace: String,
    ) -> Self {
        Self {
            theme,
            transcript: Transcript::new(),
            input: String::new(),
            history: Vec::new(),
            focus: Focus::Prompt,
            state: RunState::Idle,
            esc: EscArm::None,
            session_id,
            model,
            status: String::new(),
            notice: None,
            scroll: 0,
            needs_redraw: true,
            quit: false,
            demo,
            dialog: Dialog::None,
            workspace,
            pending_resume_file: None,
            queue: Vec::new(),
            cmd_tx,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running | RunState::Starting)
    }

    pub fn queue(&self) -> &[String] {
        &self.queue
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
        self.input.clear();
        self.state = RunState::Starting;
        if self.demo {
            self.transcript
                .push(CellKind::Assistant, String::new(), "(demo) 收到，这里是本地回显。".to_string());
            self.state = RunState::Idle;
            return;
        }
        let _ = self.cmd_tx.send(Cmd::Prompt {
            session_id: self.session_id.clone(),
            text,
        });
    }

    fn run_command(&mut self, cmd: &str) {
        let name = cmd.split_whitespace().next().unwrap_or("");
        match name {
            "/resume" => {
                let items = list_sessions(&self.workspace, &self.session_id);
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
            "/help" => {
                self.notice =
                    Some("/resume /new /exit /help ready · /model /compact TODO".into())
            }
            "/model" => self.notice = Some("model picker: TODO".into()),
            "/compact" => self.notice = Some("compact: TODO".into()),
            other => self.notice = Some(format!("unknown command {other}")),
        }
    }

    fn cancel_now(&mut self) {
        self.esc = EscArm::None;
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
                    if let Some(event) = params.get("event") {
                        self.transcript.apply(event);
                    }
                }
                "session.status" => {
                    let running = params.get("status").and_then(|s| s.as_str()) == Some("running");
                    self.state = if running { RunState::Running } else { RunState::Idle };
                    self.status = if running { "running".into() } else { "idle".into() };
                }
                "tui/ready" => {
                    self.status = "runtime ready".into();
                    self.state = RunState::Idle;
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
                    options,
                    selected: 0,
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
                                    options: q
                                        .get("options")
                                        .and_then(Value::as_array)
                                        .map(|o| {
                                            o.iter()
                                                .filter_map(|v| {
                                                    v.get("label").and_then(Value::as_str).map(String::from)
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
                });
            }
            _ => {
                // Unknown server request: answer null (bridge treats as delegate).
                self.respond(id, Value::Null);
            }
        }
    }

    fn handle_key(&mut self, ev: Event) {
        let Event::Key(key) = ev else { return };
        if key.kind != KeyEventKind::Press {
            return;
        }

        // ---- global quit (works even while a dialog is open) ----
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL)
            || key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.quit = true;
            return;
        }

        // ---- blocking dialogs take over the keyboard (docs/01 section 2.4) ----
        if self.has_dialog() {
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
        if key.code == KeyCode::Esc {
            if self.is_running() {
                self.cancel_now();
                return;
            }
            if !self.input.is_empty() {
                let now = Instant::now();
                match self.esc {
                    EscArm::ClearArmed(t) if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS => {
                        self.history.push(std::mem::take(&mut self.input));
                        self.esc = EscArm::None;
                    }
                    _ => self.esc = EscArm::ClearArmed(now),
                }
                return;
            }
            if !self.transcript.is_empty() {
                let now = Instant::now();
                match self.esc {
                    EscArm::RewindArmed(t) if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS => {
                        self.notice = Some("rewind: TODO (ctx.sessions.fork + replay)".into());
                        self.esc = EscArm::None;
                    }
                    _ => self.esc = EscArm::RewindArmed(now),
                }
                return;
            }
            self.esc = EscArm::None;
            return;
        }

        // ---- Ctrl+C: clear draft first, then cancel (docs/01 section 2.5) ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.is_running() && !self.input.is_empty() {
                self.input.clear();
            } else if self.is_running() {
                self.cancel_now();
            } else {
                self.input.clear();
            }
            return;
        }

        // ---- theme toggle ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.theme = if self.theme.name == "dark" {
                crate::theme::LIGHT
            } else {
                crate::theme::DARK
            };
            return;
        }

        // ---- model picker placeholder ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('m') {
            self.notice = Some("model picker: TODO (tui/catalog)".into());
            return;
        }

        // ---- thinking expand placeholder ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.notice = Some("thinking fold: TODO".into());
            return;
        }

        // ---- slash commands (dispatch without a model turn) ----
        if key.code == KeyCode::Enter
            && !self.input.is_empty()
            && self.input.starts_with('/')
        {
            let cmd = std::mem::take(&mut self.input);
            self.run_command(&cmd);
            return;
        }

        // ---- send / queue / send-now ----
        if key.code == KeyCode::Enter {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if alt {
                if self.is_running() {
                    self.cancel_now();
                }
                let text = std::mem::take(&mut self.input);
                self.send_input(text);
                return;
            }
            if self.input.is_empty() {
                if let Some(top) = self.queue.first().cloned() {
                    self.queue.remove(0);
                    self.send_input(top);
                }
                return;
            }
            if self.is_running() {
                self.queue.push(std::mem::take(&mut self.input));
                return;
            }
            let text = std::mem::take(&mut self.input);
            self.send_input(text);
            return;
        }

        // ---- focus ----
        if key.code == KeyCode::Tab {
            self.focus = match self.focus {
                Focus::Prompt => Focus::Scrollback,
                Focus::Scrollback => Focus::Prompt,
            };
            return;
        }

        // ---- scrollback navigation ----
        if self.focus == Focus::Scrollback {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.transcript.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.transcript.move_selection(1),
                KeyCode::Left | KeyCode::Char('h') => {
                    if let Some(i) = self.transcript.selected {
                        self.transcript.toggle_fold(i);
                    }
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(i) = self.transcript.selected {
                        self.transcript.toggle_fold(i);
                    }
                }
                KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
                KeyCode::Char(c) => {
                    self.focus = Focus::Prompt;
                    self.input.push(c);
                }
                _ => {}
            }
            return;
        }

        // ---- prompt editing ----
        match key.code {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Up => {
                if let Some(h) = self.history.last() {
                    self.input = h.clone();
                }
            }
            KeyCode::Down => {
                self.input.clear();
            }
            KeyCode::Enter => {}
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
                        let outcome = d
                            .options
                            .get(d.selected)
                            .cloned()
                            .unwrap_or_else(|| "cancelled".into());
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        self.respond(id, json!({ "outcome": outcome }));
                    }
                    // Esc parks in grok; skeleton cancels the request instead.
                    KeyCode::Esc => {
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        self.respond(id, json!({ "outcome": "cancelled" }));
                    }
                    _ => {}
                }
            }
            Dialog::Ask(d) => {
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
                        if !d.answers[cur].is_empty() {
                            let v = d.answers[cur][0];
                            d.answers[cur] = if multi { vec![v] } else { vec![v.saturating_sub(1)] };
                        } else if opts > 0 {
                            d.answers[cur] = vec![0];
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !d.answers[cur].is_empty() {
                            let v = d.answers[cur][0];
                            let nv = if multi { v } else { (v + 1).min(opts.saturating_sub(1)) };
                            d.answers[cur] = vec![nv];
                        } else if opts > 0 {
                            d.answers[cur] = vec![0];
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let idx = c.to_digit(10).unwrap_or(10) as usize;
                        if idx >= 1 && idx <= opts {
                            d.answers[cur] = vec![idx - 1];
                        }
                    }
                    KeyCode::Char(' ') if multi => {
                        let idx = d.answers[cur].first().copied().unwrap_or(0);
                        if d.answers[cur].contains(&idx) {
                            d.answers[cur].retain(|i| *i != idx);
                        } else {
                            d.answers[cur].push(idx);
                        }
                    }
                    KeyCode::Enter => {
                        if cur + 1 < n {
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
                        // Skip the whole batch (agent continues without answers).
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        self.respond(id, json!({ "answers": [] }));
                    }
                    _ => {}
                }
                let _ = has_ctrl;
            }
            Dialog::Resume(p) => {
                match key.code {
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
                }
            }
            Dialog::None => {}
        }
    }
}
