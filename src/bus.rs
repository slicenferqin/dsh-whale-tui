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
    ServerRequest { id: String, method: String, params: Value },
    /// Runtime subprocess exited.
    RuntimeExited(Option<i32>),
}

/// UI -> controller commands.
#[derive(Debug, Clone)]
pub enum Cmd {
    Prompt { session_id: String, text: String },
    /// Our protocol extension: hard-cancel the running turn (agent.cancel).
    Cancel { session_id: String },
    /// Graceful quit; the controller answers the shutdown RPC then acks.
    Shutdown { ack: std::sync::mpsc::Sender<()> },
    /// Resume a persisted session through the bridge (agents.resume).
    Load { session_id: String },
    /// Fetch the provider/model catalog for the picker.
    FetchCatalog,
    /// Switch the model route (applies to future sessions).
    SelectModel { provider: String, model: String },
    /// Ask the bridge which sessions are already live in this host.
    ListLive,
    /// Switch the live session's permission preset.
    SetPermission { session_id: String, preset: String },
    /// Manual compaction (the agent must be idle).
    Compact { session_id: String },
    /// Answer a server-initiated request (approval / ask_user dialog).
    Respond { id: String, result: Value },
}
