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
}
