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
    /// Chosen option indices per question (empty until answered).
    pub answers: Vec<Vec<usize>>,
    /// plan-review: free-form feedback typed after pressing s.
    pub feedback: String,
    pub taking_feedback: bool,
    pub detail_scroll: usize,
    pub custom_text: String,
    pub taking_text: bool,
}

#[derive(Debug, Clone)]
pub struct ResumePicker {
    pub items: Vec<SessionSummary>,
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
pub struct ThemePicker {
    pub original: crate::theme::Theme,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct PaletteRow {
    pub label: String,
    pub action: String,
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub rows: Vec<PaletteRow>,
    pub filter: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
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
    FilePicker(FilePicker),
    Rewind(RewindPicker),
    Palette(Palette),
    Info(InfoDialog),
    Theme(ThemePicker),
    Subagent(SubagentView),
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
    at_fragment_start: Option<usize>,
    permission_presets: Vec<String>,
    preset_index: usize,
    catalog_for_presets: bool,
    live_ids: HashSet<String>,
    cancel_grace: Option<Instant>,
    pub child_transcripts: HashMap<String, Transcript>,
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
            at_fragment_start: None,
            permission_presets: Vec::new(),
            preset_index: 0,
            catalog_for_presets: false,
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

    fn open_palette(&mut self) {
        let rows = vec![
            ("/resume", "恢复会话"),
            ("/new", "新会话"),
            ("/model", "切换模型"),
            ("/compact", "压缩历史"),
            ("/copy", "复制最近回复"),
            ("/help", "帮助"),
            ("/exit", "退出"),
            ("rewind 2×Esc", "回滚到早前消息"),
            ("theme Ctrl+T", "切换深浅主题"),
            ("approve dialog F2 (demo)", "演示审批弹窗"),
            ("ask dialog F3 (demo)", "演示问题卡"),
        ];
        let rows: Vec<PaletteRow> = rows
            .into_iter()
            .map(|(label, action)| PaletteRow {
                label: label.to_string(),
                action: action.to_string(),
            })
            .collect();
        let visible = (0..rows.len()).collect();
        self.dialog = Dialog::Palette(Palette {
            rows,
            filter: String::new(),
            selected: 0,
            visible,
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
        let name = cmd.split_whitespace().next().unwrap_or("");
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
            "/help" => {
                self.notice = Some("/resume /new /exit /help ready · /model /compact TODO".into())
            }
            "/model" => {
                self.catalog_for_presets = false;
                self.status = "loading models".into();
                let _ = self.cmd_tx.send(Cmd::FetchCatalog);
            }
            "/compact" => {
                self.status = "compacting".into();
                let _ = self.cmd_tx.send(Cmd::Compact {
                    session_id: self.session_id.clone(),
                });
            }
            other => self.notice = Some(format!("unknown command {other}")),
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
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        self.transcript.cells[i].text = format!("{status}: {text}");
                        self.transcript.selected = Some(i);
                    }
                }
                "session.status" => {
                    let running = params.get("status").and_then(|s| s.as_str()) == Some("running");
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
                "tui/ready" => {
                    self.status = "runtime ready".into();
                    self.state = RunState::Idle;
                }
                "tui/catalog-result" => {
                    if let Some(names) = params.get("permissionPresets").and_then(|v| v.as_array())
                    {
                        self.permission_presets = names
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                    }
                    let current_provider = params
                        .get("current")
                        .and_then(|v| v.get("provider"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let current_model = params
                        .get("current")
                        .and_then(|v| v.get("model"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let mut rows: Vec<ModelEntry> = params
                        .get("models")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|m| {
                                    Some(ModelEntry {
                                        provider: m.get("provider")?.as_str()?.to_string(),
                                        id: m.get("id")?.as_str()?.to_string(),
                                        name: m
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Current provider's models first; preselect the current model.
                    rows.sort_by_key(|r| {
                        let cur = r.provider == current_provider;
                        let exact = cur && r.id == current_model;
                        (
                            if cur { 0 } else { 1 },
                            if exact { 0 } else { 1 },
                            r.id.clone(),
                        )
                    });
                    let selected = rows
                        .iter()
                        .position(|r| r.provider == current_provider && r.id == current_model)
                        .unwrap_or(0);
                    let for_presets = self.catalog_for_presets;
                    self.catalog_for_presets = false;
                    if for_presets {
                        self.notice =
                            Some(format!("{} presets loaded", self.permission_presets.len()));
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
                    if let Some(cur) = params.get("current") {
                        let provider = cur.get("provider").and_then(|v| v.as_str()).unwrap_or("?");
                        let model = cur.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                        self.model = model.to_string();
                        self.status = format!("model: {provider}/{model}");
                    }
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
                    let mut rows = vec![
                        ("session".to_string(), get("sessionId")),
                        (
                            "model".to_string(),
                            format!("{}/{}", get("provider"), get("model")),
                        ),
                        ("cwd".to_string(), get("cwd")),
                        ("turns".to_string(), get("turns")),
                    ];
                    if let Some(u) = params.get("usage") {
                        let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                        rows.push((
                            "usage".to_string(),
                            format!(
                                "in {} · out {} · cache {}",
                                g("inputTokens"),
                                g("outputTokens"),
                                g("cacheReadTokens")
                            ),
                        ));
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
                "tui/permission-set" => {
                    if let Some(p) = params.get("applied").and_then(|v| v.as_str()) {
                        self.status = format!("preset: {p}");
                        if let Some(i) = self.permission_presets.iter().position(|x| x == p) {
                            self.preset_index = i;
                        }
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
                    feedback: String::new(),
                    taking_feedback: false,
                    detail_scroll: 0,
                    custom_text: String::new(),
                    taking_text: false,
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
        // A parked approval card keeps the dialog visible but hands the
        // keyboard to the scrollback; Tab returns to the card.
        let parked = matches!(&self.dialog, Dialog::Approval(d) if d.parked);
        if parked {
            if key.code == KeyCode::Tab {
                if let Dialog::Approval(d) = &mut self.dialog {
                    d.parked = false;
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
                    self.history.push(std::mem::take(&mut self.input));
                    self.esc = EscArm::None;
                } else {
                    let now = Instant::now();
                    match self.esc {
                        EscArm::ClearArmed(t)
                            if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS =>
                        {
                            self.history.push(std::mem::take(&mut self.input));
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

        // ---- command palette (docs/01 section 2.6) ----
        // '?' is the palette alt binding only outside the composer, so a
        // question mark in a prompt keeps typing normally.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p')
            || key.code == KeyCode::Char('?') && self.focus == Focus::Scrollback
        {
            self.open_palette();
            return;
        }

        // ---- model picker ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('m') {
            if self.has_dialog() {
                self.dialog = Dialog::None;
            } else {
                self.catalog_for_presets = false;
                self.status = "loading models".into();
                let _ = self.cmd_tx.send(Cmd::FetchCatalog);
            }
            return;
        }

        // ---- thinking expand placeholder ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.notice = Some("thinking fold: TODO".into());
            return;
        }

        // ---- slash commands (dispatch without a model turn) ----
        if key.code == KeyCode::Enter && !self.input.is_empty() && self.input.starts_with('/') {
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

        // ---- permission preset cycle: Shift+Tab (docs/01 section 3.1) ----
        if key.code == KeyCode::BackTab {
            if self.permission_presets.is_empty() {
                self.catalog_for_presets = true;
                let _ = self.cmd_tx.send(Cmd::FetchCatalog);
                self.notice = Some("loading presets".into());
            } else {
                self.preset_index = (self.preset_index + 1) % self.permission_presets.len();
                let preset = self.permission_presets[self.preset_index].clone();
                self.status = format!("preset: {preset} (switching)");
                let _ = self.cmd_tx.send(Cmd::SetPermission {
                    session_id: self.session_id.clone(),
                    preset,
                });
            }
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
                KeyCode::Enter => {
                    if let Some(i) = self.transcript.selected {
                        if let Some(link) = self.transcript.cells[i].link.clone() {
                            self.dialog = Dialog::Subagent(SubagentView {
                                child_id: link,
                                scroll: 0,
                            });
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
            KeyCode::Char('@') => {
                let files = list_files(&self.workspace);
                let visible = fuzzy_filter(&files, "");
                self.at_fragment_start = Some(self.input.len());
                self.input.push('@');
                self.dialog = Dialog::FilePicker(FilePicker {
                    files,
                    query: String::new(),
                    selected: 0,
                    visible,
                });
            }
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
            KeyCode::Enter => {
                if taking {
                    self.dialog = Dialog::None;
                    self.respond(
                        request_id,
                        json!({ "answers": [ { "id": qid, "selected": [other], "custom": fb } ] }),
                    );
                }
            }
            KeyCode::Esc => {
                if taking {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.taking_feedback = false;
                    }
                } else {
                    self.dialog = Dialog::None;
                    self.respond(request_id, json!({ "answers": [] }));
                }
            }
            KeyCode::PageUp | KeyCode::Up | KeyCode::Char('k') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.detail_scroll =
                        scroll.saturating_add(if key.code == KeyCode::PageUp { 6 } else { 1 });
                }
            }
            KeyCode::PageDown | KeyCode::Down | KeyCode::Char('j') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.detail_scroll =
                        scroll.saturating_sub(if key.code == KeyCode::PageDown { 6 } else { 1 });
                }
            }
            KeyCode::Backspace => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    if d.taking_feedback {
                        d.feedback.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    if d.taking_feedback {
                        d.feedback.push(c);
                    }
                }
            }
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
                            // 切到免审批 preset（本次仍按允许一次放行）
                            let session_id = self.session_id.clone();
                            let _ = self.cmd_tx.send(Cmd::SetPermission {
                                session_id,
                                preset: "danger-full-access".to_string(),
                            });
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
                        if !d.answers[cur].is_empty() {
                            let v = d.answers[cur][0];
                            d.answers[cur] = if multi {
                                vec![v]
                            } else {
                                vec![v.saturating_sub(1)]
                            };
                        } else if opts > 0 {
                            d.answers[cur] = vec![0];
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !d.answers[cur].is_empty() {
                            let v = d.answers[cur][0];
                            let nv = if multi {
                                v
                            } else {
                                (v + 1).min(opts.saturating_sub(1))
                            };
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
                        } else {
                            // Skip the whole batch (agent continues without answers).
                            let id = d.request_id.clone();
                            self.dialog = Dialog::None;
                            self.respond(id, json!({ "answers": [] }));
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
                            self.input.truncate(start);
                            self.input.push_str(&path);
                            self.input.push(' ');
                        }
                        self.at_fragment_start = None;
                        self.dialog = Dialog::None;
                        self.focus = Focus::Prompt;
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
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
                KeyCode::Up | KeyCode::Char('k') => {
                    p.selected = p.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
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
                KeyCode::Enter => {
                    if let Some(idx) = p.visible.get(p.selected) {
                        let label = p.rows[*idx].label.clone();
                        self.dialog = Dialog::None;
                        if label.starts_with('/') {
                            self.run_command(&label);
                        } else if label.starts_with("rewind") {
                            self.open_rewind();
                        } else if label.starts_with("theme") {
                            self.theme = if self.theme.name == "dark" {
                                crate::theme::LIGHT
                            } else {
                                crate::theme::DARK
                            };
                        } else if label.contains("F2") {
                            self.open_dialog(
                                    "demo-approve".into(),
                                    "ui/approve",
                                    &serde_json::json!({ "toolName": "bash", "reason": "shell command", "input": {"command": "cargo test"}, "options": ["allowed-once", "rejected"] }),
                                );
                        } else if label.contains("F3") {
                            self.open_dialog(
                                    "demo-ask".into(),
                                    "ui/ask-user",
                                    &serde_json::json!({ "questions": [
                                        { "id": "q1", "question": "选一个颜色？", "header": "主题", "options": [{"label": "蓝色"}, {"label": "绿色"}] }
                                    ] }),
                                );
                        }
                    }
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
                    m.selected = m.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let vis = m.visible();
                    if !vis.is_empty() {
                        let pos = vis.iter().position(|i| *i == m.selected).unwrap_or(0);
                        let next = (pos + 1).min(vis.len().saturating_sub(1));
                        m.selected = vis[next];
                    }
                }
                KeyCode::Char(c) => {
                    m.filter.push(c);
                    let vis = m.visible();
                    if !vis.is_empty() {
                        m.selected = vis[0];
                    }
                }
                KeyCode::Backspace => {
                    m.filter.pop();
                    let vis = m.visible();
                    if !vis.is_empty() {
                        m.selected = vis[0];
                    }
                }
                KeyCode::Enter => {
                    if let Some(row) = m.rows.get(m.selected) {
                        let provider = row.provider.clone();
                        let model = row.id.clone();
                        self.dialog = Dialog::None;
                        self.status = "switching model".into();
                        let _ = self.cmd_tx.send(Cmd::SelectModel { provider, model });
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

fn palette_filter(rows: &[PaletteRow], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    (0..rows.len())
        .filter(|i| {
            q.is_empty()
                || rows[*i].label.to_lowercase().contains(&q)
                || rows[*i].action.to_lowercase().contains(&q)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_filters_labels_and_actions() {
        let rows = vec![
            PaletteRow {
                label: "/resume".into(),
                action: "恢复会话".into(),
            },
            PaletteRow {
                label: "/model".into(),
                action: "切换模型".into(),
            },
        ];
        assert_eq!(palette_filter(&rows, "model"), vec![1]);
        assert_eq!(palette_filter(&rows, "恢复"), vec![0]);
        assert_eq!(palette_filter(&rows, "").len(), 2);
    }
}
