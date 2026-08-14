//! App state machine: run state, focus, Esc semantics (docs/01 section 2.5),
//! follow-up queue, selection and scroll.

use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::bus::{AppEvent, Cmd};
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

    fn send_input(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history.push(text.clone());
        self.input.clear();
        self.state = RunState::Starting;
        if self.demo {
            // Scripted local echo: no runtime behind demo mode.
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
                _ => {}
            },
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

    fn handle_key(&mut self, ev: Event) {
        let Event::Key(key) = ev else { return };
        if key.kind != KeyEventKind::Press {
            return;
        }

        // ---- Esc semantics (spec section 2.5) ----
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

        // ---- Ctrl+C: clear draft first, then cancel (spec section 2.5) ----
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

        // ---- global quit ----
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL)
            || key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.quit = true;
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

        // ---- send / queue / send-now ----
        if key.code == KeyCode::Enter {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if alt {
                // send-now: cancel current turn, run this message next
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
            KeyCode::Enter => {} // handled above
            _ => {}
        }
    }
}
