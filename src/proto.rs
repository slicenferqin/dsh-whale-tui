//! NDJSON JSON-RPC 2.0 transport. Two peers:
//! - attach: inherited pipe fds (plugin mode, fd 3 server->tui, fd 4 tui->server)
//! - spawn: an owned runtime subprocess (standalone mode, later)
//!
//! Client requests: initialize / session/prompt / session/cancel / shutdown.
//! Server notifications: session.event / session.status / subagent.*.
//! Server->client requests: approval and ask_user dialogs, answered over the
//! same transport so tool execution stays blocked until the human responds.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::bus::AppEvent;

type SharedWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;
type Pending = Arc<Mutex<HashMap<String, mpsc::SyncSender<Result<Value, String>>>>>;

pub struct Runtime {
    child: Arc<Mutex<Option<Child>>>,
    stdin: SharedWriter,
    pending: Pending,
    next_id: AtomicU64,
    stderr_tail: Arc<Mutex<Vec<String>>>,
}

impl Runtime {
    /// Spawn a standalone SDK runtime subprocess.
    #[allow(dead_code)] // standalone mode (Windows / SDK runtime), planned
    pub fn spawn(
        bin: &str,
        envs: &[(String, String)],
        cwd: &str,
        bus: mpsc::Sender<AppEvent>,
    ) -> Result<Self> {
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(cwd);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn harness runtime: {bin}"))?;
        let stdin = child.stdin.take().context("runtime stdin unavailable")?;
        let stdout = child.stdout.take().context("runtime stdout unavailable")?;
        let stderr = child.stderr.take().context("runtime stderr unavailable")?;

        let rt = Runtime {
            child: Arc::new(Mutex::new(Some(child))),
            stdin: Arc::new(Mutex::new(Some(Box::new(stdin) as Box<dyn Write + Send>))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
        };
        rt.start_reader(stdout, bus.clone());
        rt.start_stderr(stderr, bus);
        Ok(rt)
    }

    /// Attach to a host dsh process over inherited pipe fds (plugin mode).
    pub fn attach(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        bus: mpsc::Sender<AppEvent>,
    ) -> Self {
        let rt = Runtime {
            child: Arc::new(Mutex::new(None)),
            stdin: Arc::new(Mutex::new(Some(Box::new(writer) as Box<dyn Write + Send>))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
        };
        rt.start_reader(reader, bus);
        rt
    }

    fn start_reader(&self, stream: impl Read + Send + 'static, bus: mpsc::Sender<AppEvent>) {
        let pending = Arc::clone(&self.pending);
        let stdin_slot = Arc::clone(&self.stdin);
        let child_slot = Arc::clone(&self.child);
        std::thread::Builder::new()
            .name("dsh-frames".into())
            .spawn(move || {
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    route(&msg, &pending, &stdin_slot, &bus);
                }
                let waiters: Vec<_> = pending.lock().unwrap().drain().collect();
                for (_, tx) in waiters {
                    let _ = tx.try_send(Err("harness runtime closed".into()));
                }
                let code = child_slot
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|c| c.wait().ok())
                    .and_then(|s| s.code());
                let _ = bus.send(AppEvent::RuntimeExited(code));
            })
            .expect("spawn frame reader");
    }

    #[allow(dead_code)] // standalone-mode diagnostics pump
    fn start_stderr(&self, stderr: impl Read + Send + 'static, bus: mpsc::Sender<AppEvent>) {
        let tail = Arc::clone(&self.stderr_tail);
        std::thread::Builder::new()
            .name("dsh-stderr".into())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    {
                        let mut t = tail.lock().unwrap();
                        t.push(line.clone());
                        let len = t.len();
                        if len > 200 {
                            t.drain(0..len - 200);
                        }
                    }
                    let _ = bus.send(AppEvent::RuntimeStderr(line));
                }
            })
            .expect("spawn stderr reader");
    }

    fn write_line(&self, value: &Value) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard.as_mut().context("runtime stdin closed")?;
        let mut payload = serde_json::to_vec(value)?;
        payload.push(b'\n');
        stdin.write_all(&payload)?;
        stdin.flush()?;
        Ok(())
    }

    /// Blocking JSON-RPC request. Call off the UI thread.
    pub fn request(&self, method: &str, params: Option<Value>, timeout: Duration) -> Result<Value> {
        let id = format!("dsb-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id.clone(), tx);

        let mut msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(p) = params {
            msg["params"] = p;
        }
        if let Err(err) = self.write_line(&msg) {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }

        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(failure)) => Err(anyhow!(failure)),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                let tail = self.stderr_snapshot(6);
                if tail.is_empty() {
                    bail!("{method} timed out waiting for harness runtime")
                }
                bail!(
                    "{method} timed out: {}",
                    tail.join(
                        "
"
                    )
                )
            }
        }
    }

    #[allow(dead_code)] // client->server notifications (planned: usage/telemetry)
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Answer a server-initiated request from the bridge.
    pub fn respond(&self, id: &str, result: Value) -> Result<()> {
        self.write_line(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    pub fn stderr_snapshot(&self, n: usize) -> Vec<String> {
        let tail = self.stderr_tail.lock().unwrap();
        tail.iter().rev().take(n).rev().cloned().collect()
    }

    /// Hard interrupt: SIGKILL the owned runtime (standalone only).
    /// Durable JSONL survives for the next spawn.
    pub fn kill(&self) {
        {
            let mut stdin = self.stdin.lock().unwrap();
            *stdin = None;
        }
        let mut guard = self.child.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *guard = None;
    }

    /// Polite shutdown; falls back to kill.
    pub fn shutdown(&self) {
        let _ = self.request("shutdown", None, Duration::from_millis(1200));
        self.kill();
    }
}

fn route(msg: &Value, pending: &Pending, stdin_slot: &SharedWriter, bus: &mpsc::Sender<AppEvent>) {
    let id = msg.get("id");
    let method = msg.get("method").and_then(Value::as_str);
    match (id, method) {
        // Server-initiated request: the approval / ask_user bridge. Dispatch
        // to the UI thread; if the UI is gone, fail the request so the peer
        // never deadlocks waiting for an answer.
        (Some(id), Some(method)) => {
            let rid = id
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.to_string());
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let sent = bus.send(AppEvent::ServerRequest {
                id: rid.clone(),
                method: method.to_string(),
                params,
            });
            if sent.is_err() {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": rid,
                    "error": { "code": -32601, "message": "dsh-whale-tui: no UI to answer" }
                });
                if let Some(stdin) = stdin_slot.lock().unwrap().as_mut() {
                    if let Ok(mut payload) = serde_json::to_vec(&reply) {
                        payload.push(b'\n');
                        let _ = stdin.write_all(&payload);
                        let _ = stdin.flush();
                    }
                }
            }
        }
        // Response to one of our requests.
        (Some(id), None) => {
            let key = id
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.to_string());
            let waiter = pending.lock().unwrap().remove(&key);
            if let Some(tx) = waiter {
                let outcome = if let Some(err) = msg.get("error") {
                    Err(err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("JSON-RPC error")
                        .to_string())
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.try_send(outcome);
            }
        }
        // Notification.
        (None, Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let _ = bus.send(AppEvent::Rpc {
                method: method.to_string(),
                params,
            });
        }
        _ => {}
    }
}
