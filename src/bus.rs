//! Unified event bus for the single-threaded UI loop.

use serde_json::Value;

/// Everything the app loop can receive.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Terminal input.
    Term(crossterm::event::Event),
    /// JSON-RPC notification from the harness runtime.
    Rpc { method: String, params: Value },
    /// One line of runtime stderr (kept for diagnostics).
    RuntimeStderr(String),
    /// Server-initiated request from the bridge (approval / ask_user dialogs).
    ServerRequest {
        id: String,
        method: String,
        params: Value,
    },
    /// Runtime subprocess exited.
    RuntimeExited(Option<i32>),
}

/// UI -> controller commands.
#[derive(Debug, Clone)]
pub enum Cmd {
    Prompt {
        session_id: String,
        text: String,
    },
    SendNow {
        session_id: String,
        text: String,
    },
    /// Apply one mutation to a Harness-owned pending queue item.
    UpdateQueue {
        session_id: String,
        item_id: String,
        action: Value,
    },
    /// Our protocol extension: hard-cancel the running turn (agent.cancel).
    Cancel {
        session_id: String,
    },
    /// Graceful quit; the controller answers the shutdown RPC then acks.
    Shutdown {
        ack: std::sync::mpsc::Sender<()>,
    },
    /// Resume a persisted session through the bridge (agents.resume).
    Load {
        session_id: String,
    },
    /// Fetch the provider/model catalog for the current session.
    FetchCatalog {
        session_id: String,
    },
    /// Switch the current session's model route and optional reasoning effort.
    SelectModel {
        session_id: String,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    },
    /// Ask the bridge which sessions are already live in this host.
    ListLive,
    /// 原子切换计划协作状态与权限预设。
    SetMode {
        session_id: String,
        plan: bool,
        preset: String,
    },
    /// Manual compaction (the agent must be idle).
    Compact {
        session_id: String,
    },
    /// Rewind: fork the session at a turn boundary.
    Rewind {
        session_id: String,
        boundary: u64,
    },
    /// /session-info: session facts for the info dialog.
    SessionInfo {
        session_id: String,
    },
    /// Background jobs for the tasks pane (Ctrl+G).
    FetchJobs,
    /// Execute one command from the Harness command registry.
    ExecuteCommand {
        session_id: String,
        line: String,
    },
    /// Answer a server-initiated request (approval / ask_user dialog).
    Respond {
        id: String,
        result: Value,
    },
}
