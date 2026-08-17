//! dsh-whale-tui — grok-style terminal UI for DeepSeek Harness.
//!
//! Architecture: docs/02-openma-teardown.md (same shape, bidirectional
//! protocol). Interaction spec: docs/01-grok-tui-spec.md.

mod app;
mod bus;
mod clipboard;
mod demo;
mod files;
mod highlight;
mod markdown;
mod projection;
mod proto;
mod resume;
mod term;
mod theme;
mod toolcard;
mod transcript;
mod ui;

use std::io::Write;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use serde_json::json;

use crate::app::App;
use crate::bus::{AppEvent, Cmd};
use crate::proto::Runtime;

const HELP: &str = "dsh-whale-tui — grok-style terminal UI for DeepSeek Harness

USAGE:
  dsh-whale-tui [OPTIONS]

OPTIONS:
  --demo               scripted demo turn (no runtime / API key)
  --dump-frame <WxH>   print one deterministic demo frame and exit
  --attach-fds         plugin mode: JSON-RPC over inherited fds 3/4 (unix)
  -w, --workspace <d>  agent workspace (default: cwd)
  --session-id <id>    session id (default: generated)
  --provider <id>      provider route (default: dsh-whale-tui settings block)
  --model <id>         model id (default: dsh-whale-tui settings block)
  --theme <d|l>        dark (default) | light
  -V, --version        print version
  -h, --help           this help
";

struct Args {
    demo: bool,
    dump_frame: Option<(u16, u16)>,
    attach_fds: bool,
    workspace: Option<String>,
    theme: String,
    session_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        demo: false,
        dump_frame: None,
        attach_fds: false,
        workspace: None,
        theme: "dark".into(),
        session_id: None,
        provider: None,
        model: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> Result<String> {
            it.next().with_context(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--demo" => args.demo = true,
            "--dump-frame" => args.dump_frame = Some(parse_frame_size(&take("--dump-frame")?)?),
            "--attach-fds" => args.attach_fds = true,
            "-w" | "--workspace" => args.workspace = Some(take("--workspace")?),
            "--theme" => args.theme = take("--theme")?,
            "--session-id" => args.session_id = Some(take("--session-id")?),
            "--provider" => args.provider = Some(take("--provider")?),
            "--model" => args.model = Some(take("--model")?),
            "-V" | "--version" => {
                println!("dsh-whale-tui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other} (see --help)"),
        }
    }
    Ok(args)
}

fn parse_frame_size(raw: &str) -> Result<(u16, u16)> {
    let (width, height) = raw
        .split_once('x')
        .or_else(|| raw.split_once('X'))
        .with_context(|| format!("invalid frame size {raw:?}; expected WIDTHxHEIGHT"))?;
    let width = width
        .parse::<u16>()
        .with_context(|| format!("invalid frame width {width:?}"))?;
    let height = height
        .parse::<u16>()
        .with_context(|| format!("invalid frame height {height:?}"))?;
    if width < 40 || height < 12 {
        bail!("frame must be at least 40x12");
    }
    Ok((width, height))
}

struct RuntimeCfg {
    cwd: String,
    provider: String,
    model: String,
}

fn controller_loop(
    _demo: bool,
    rt: Option<Arc<Runtime>>,
    rx: mpsc::Receiver<Cmd>,
    bus: mpsc::Sender<AppEvent>,
    cfg: RuntimeCfg,
) {
    let Some(rt) = rt else {
        // demo: no runtime; drain commands until shutdown
        for cmd in rx {
            if let Cmd::Shutdown { ack } = cmd {
                let _ = ack.send(());
                break;
            }
        }
        return;
    };
    let init = json!({ "cwd": cfg.cwd, "provider": cfg.provider, "model": cfg.model });
    match rt.request("initialize", Some(init), Duration::from_secs(60)) {
        Ok(res) => {
            let _ = bus.send(AppEvent::Rpc {
                method: "tui/ready".to_string(),
                params: json!({ "server": res }),
            });
        }
        Err(e) => {
            let _ = bus.send(AppEvent::RuntimeStderr(format!("initialize failed: {e}")));
        }
    }
    for cmd in rx {
        match cmd {
            Cmd::Prompt { session_id, text } => {
                let params = json!({
                    "sessionId": session_id,
                    "contentBlocks": [ { "type": "text", "text": text } ]
                });
                if let Err(e) = rt.request("session/prompt", Some(params), Duration::from_secs(30))
                {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("prompt failed: {e}")));
                }
            }
            Cmd::SendNow { session_id, text } => {
                let params = json!({
                    "sessionId": session_id,
                    "contentBlocks": [ { "type": "text", "text": text } ]
                });
                if let Err(e) =
                    rt.request("session/send-now", Some(params), Duration::from_secs(30))
                {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("send-now failed: {e}")));
                }
            }
            Cmd::UpdateQueue {
                session_id,
                item_id,
                action,
            } => {
                let params = json!({
                    "sessionId": session_id,
                    "itemId": item_id,
                    "action": action,
                });
                if let Err(e) =
                    rt.request("session/update-queue", Some(params), Duration::from_secs(10))
                {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!(
                        "queue update failed: {e}"
                    )));
                }
            }
            Cmd::ExecuteCommand { session_id, line } => {
                let params = json!({ "sessionId": session_id, "line": line });
                match rt.request("tui/execute-command", Some(params), Duration::from_secs(60)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/command-result".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ = bus.send(AppEvent::RuntimeStderr(format!(
                            "command execution failed: {e}"
                        )));
                    }
                }
            }
            Cmd::Cancel { session_id } => {
                let params = json!({ "sessionId": session_id });
                if let Err(e) = rt.request("session/cancel", Some(params), Duration::from_secs(10))
                {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("cancel failed: {e}")));
                }
            }
            Cmd::Load { session_id } => {
                let params = json!({ "sessionId": session_id });
                match rt.request("session/load", Some(params), Duration::from_secs(30)) {
                    Ok(_res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/loaded".to_string(),
                            params: json!({ "sessionId": session_id }),
                        });
                    }
                    Err(e) => {
                        let _ = bus.send(AppEvent::RuntimeStderr(format!("load failed: {e}")));
                    }
                }
            }
            Cmd::FetchCatalog { session_id } => {
                let params = json!({ "sessionId": session_id });
                match rt.request("tui/catalog", Some(params), Duration::from_secs(30)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/catalog-result".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ = bus.send(AppEvent::RuntimeStderr(format!("catalog failed: {e}")));
                    }
                }
            }
            Cmd::SelectModel {
                session_id,
                provider,
                model,
                reasoning_effort,
            } => {
                let params = json!({
                    "sessionId": session_id,
                    "provider": provider,
                    "model": model,
                    "reasoningEffort": reasoning_effort,
                });
                match rt.request("tui/select-model", Some(params), Duration::from_secs(30)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/model-set".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ =
                            bus.send(AppEvent::RuntimeStderr(format!("select-model failed: {e}")));
                    }
                }
            }
            Cmd::SetMode {
                session_id,
                plan,
                preset,
            } => {
                let params = json!({ "sessionId": session_id, "plan": plan, "preset": preset });
                match rt.request("tui/mode", Some(params), Duration::from_secs(15)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/mode-set".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ =
                            bus.send(AppEvent::RuntimeStderr(format!("mode switch failed: {e}")));
                    }
                }
            }
            Cmd::Compact { session_id } => {
                let params = json!({ "sessionId": session_id });
                match rt.request("tui/compact", Some(params), Duration::from_secs(310)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/compacted".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ = bus.send(AppEvent::RuntimeStderr(format!("compact failed: {e}")));
                    }
                }
            }
            Cmd::Rewind {
                session_id,
                boundary,
            } => {
                let params = json!({ "sessionId": session_id, "boundary": boundary });
                match rt.request("tui/rewind", Some(params), Duration::from_secs(30)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/rewound".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ = bus.send(AppEvent::RuntimeStderr(format!("rewind failed: {e}")));
                    }
                }
            }
            Cmd::SessionInfo { session_id } => {
                let params = json!({ "sessionId": session_id });
                match rt.request("tui/session-info", Some(params), Duration::from_secs(15)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/session-info-result".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ =
                            bus.send(AppEvent::RuntimeStderr(format!("session-info failed: {e}")));
                    }
                }
            }
            Cmd::FetchJobs => match rt.request("tui/jobs", None, Duration::from_secs(15)) {
                Ok(res) => {
                    let _ = bus.send(AppEvent::Rpc {
                        method: "tui/jobs-result".to_string(),
                        params: res,
                    });
                }
                Err(e) => {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("jobs failed: {e}")));
                }
            },
            Cmd::ListLive => match rt.request("tui/live-sessions", None, Duration::from_secs(10)) {
                Ok(res) => {
                    let _ = bus.send(AppEvent::Rpc {
                        method: "tui/live-list".to_string(),
                        params: res,
                    });
                }
                Err(_e) => {}
            },
            Cmd::Respond { id, result } => {
                if let Err(e) = rt.respond(&id, result) {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("respond failed: {e}")));
                }
            }
            Cmd::Shutdown { ack } => {
                rt.shutdown();
                let _ = ack.send(());
                break;
            }
        }
    }
}

/// Read provider/model defaults from the local dsh install's settings.yaml
/// (agent-default-model block). Best effort; returns None when absent.
/// Read provider/model defaults from the local dsh install's settings.yaml.
/// Priority: the dsh-whale-tui block (this TUI's own default), then the
/// shared agent-default-model block. Best effort; None when absent.
fn local_defaults() -> (Option<String>, Option<String>) {
    let root = std::env::var("DSH_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.dsh")));
    let Some(root) = root else {
        return (None, None);
    };
    let path = std::path::Path::new(&root).join("settings.yaml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let mut block: Option<&str> = None;
    let mut whale = (None, None);
    let mut agent = (None, None);
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) {
            let head = line.trim_end();
            if head == "dsh-whale-tui:" {
                block = Some("whale");
            } else if head == "agent-default-model:" {
                block = Some("agent");
            } else {
                block = None;
            }
            continue;
        }
        let Some(b) = block else { continue };
        let Some((k, v)) = line.trim().split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches(|c| c == '\'' || c == '"').trim();
        if v.is_empty() {
            continue;
        }
        let slot = if b == "whale" { &mut whale } else { &mut agent };
        match k.trim() {
            "provider" => slot.0 = Some(v.to_string()),
            "model" => slot.1 = Some(v.to_string()),
            _ => {}
        }
    }
    if whale.0.is_some() || whale.1.is_some() {
        whale
    } else {
        agent
    }
}

fn dump_demo_frame(
    size: (u16, u16),
    theme: theme::Theme,
    session_id: String,
    provider: String,
    model: String,
    cmd_tx: mpsc::Sender<Cmd>,
    cwd: String,
) -> Result<()> {
    let backend = ratatui::backend::TestBackend::new(size.0, size.1);
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut app = App::new(theme, session_id, provider, model, true, cmd_tx, cwd);
    demo::seed(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app))?;

    let mut stdout = std::io::stdout().lock();
    let buffer = terminal.backend().buffer();
    for y in 0..size.1 {
        let mut row = String::new();
        let mut x = 0;
        while x < size.0 {
            if let Some(cell) = buffer.cell((x, y)) {
                let symbol = cell.symbol();
                row.push_str(symbol);
                let width = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
                x = x.saturating_add(width);
            } else {
                x += 1;
            }
        }
        writeln!(stdout, "{}", row.trim_end())?;
    }
    Ok(())
}

fn main() -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = parse_args()?;
    let cwd = match &args.workspace {
        Some(w) => std::fs::canonicalize(w)
            .with_context(|| format!("workspace not found: {w}"))?
            .to_string_lossy()
            .into_owned(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    let session_id = args.session_id.clone().unwrap_or_else(|| {
        if args.dump_frame.is_some() {
            "demo-session".into()
        } else {
            format!(
                "dsh-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            )
        }
    });

    let (bus_tx, bus_rx) = mpsc::channel::<AppEvent>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

    let runtime: Option<Arc<Runtime>> = if args.demo || args.dump_frame.is_some() {
        None
    } else if args.attach_fds {
        #[cfg(unix)]
        {
            use std::os::unix::io::FromRawFd;
            let reader = unsafe { std::fs::File::from_raw_fd(3) };
            let writer = unsafe { std::fs::File::from_raw_fd(4) };
            Some(Arc::new(Runtime::attach(reader, writer, bus_tx.clone())))
        }
        #[cfg(not(unix))]
        {
            bail!("--attach-fds requires a unix platform")
        }
    } else {
        bail!("standalone mode is not implemented — use --demo or --attach-fds")
    };

    let (provider, model) = {
        let local = if args.dump_frame.is_some() {
            (None, None)
        } else {
            local_defaults()
        };
        (
            args.provider
                .clone()
                .or(local.0)
                .unwrap_or_else(|| "deepseek-official".into()),
            args.model
                .clone()
                .or(local.1)
                .unwrap_or_else(|| "deepseek-v4-flash".into()),
        )
    };
    if let Some(size) = args.dump_frame {
        return dump_demo_frame(
            size,
            theme::theme_for(&args.theme),
            session_id,
            provider,
            model,
            cmd_tx,
            cwd,
        );
    }

    {
        let tx = bus_tx.clone();
        let cfg = RuntimeCfg {
            cwd: cwd.clone(),
            provider: provider.clone(),
            model: model.clone(),
        };
        std::thread::Builder::new()
            .name("controller".into())
            .spawn(move || controller_loop(args.demo, runtime, cmd_rx, tx, cfg))
            .expect("spawn controller");
    }

    {
        let tx = bus_tx.clone();
        std::thread::Builder::new()
            .name("input".into())
            .spawn(move || {
                while let Ok(ev) = crossterm::event::read() {
                    if tx.send(AppEvent::Term(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn input thread");
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;

    let theme = theme::theme_for(&args.theme);
    let (term_kind, in_tmux) = term::detect();
    let mut app = App::new(
        theme,
        session_id,
        provider,
        model,
        args.demo,
        cmd_tx.clone(),
        cwd,
    );
    app.term_kind = term_kind;
    app.in_tmux = in_tmux;
    if args.demo {
        demo::seed(&mut app);
    }

    loop {
        if app.needs_redraw {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            app.needs_redraw = false;
        }
        match bus_rx.recv_timeout(Duration::from_millis(80)) {
            Ok(ev) => {
                app.handle(ev);
                while let Ok(ev) = bus_rx.try_recv() {
                    app.handle(ev);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                app.tick();
                if app.is_running() {
                    app.needs_redraw = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if app.quit {
            break;
        }
    }
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    let _ = cmd_tx.send(Cmd::Shutdown { ack: ack_tx });
    // Wait for the controller's shutdown RPC (bounded), then leave.
    let _ = ack_rx.recv_timeout(Duration::from_secs(3));
    restore_terminal();
    Ok(())
}

fn restore_terminal() {
    let mut stdout = std::io::stdout();
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_rejects_dimensions_that_cannot_render_the_shell() {
        assert!(parse_frame_size("39x12").is_err());
        assert!(parse_frame_size("40x11").is_err());
        assert_eq!(parse_frame_size("40x12").unwrap(), (40, 12));
    }
}
