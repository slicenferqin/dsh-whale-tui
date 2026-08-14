//! dsh-whale-tui — grok-style terminal UI for DeepSeek Harness (skeleton).
//!
//! Architecture: docs/02-openma-teardown.md (same shape, bidirectional
//! protocol). Interaction spec: docs/01-grok-tui-spec.md.

mod app;
mod bus;
mod demo;
mod proto;
mod theme;
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
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use serde_json::json;

use crate::app::App;
use crate::bus::{AppEvent, Cmd};
use crate::proto::Runtime;

const HELP: &str = "dsh-whale-tui — grok-style terminal UI for DeepSeek Harness

USAGE:
  dsh-whale-tui [OPTIONS]

OPTIONS:
  --demo               scripted demo turn (no runtime / API key)
  --attach-fds         plugin mode: JSON-RPC over inherited fds 3/4 (unix)
  -w, --workspace <d>  agent workspace (default: cwd)
  --session-id <id>    session id (default: generated)
  --provider <id>      provider route (default: dsh settings agent-default-model)
  --model <id>         model id (default: dsh settings agent-default-model)
  --theme <d|l>        dark (default) | light
  -V, --version        print version
  -h, --help           this help
";

struct Args {
    demo: bool,
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
                if let Err(e) = rt.request("session/prompt", Some(params), Duration::from_secs(30)) {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("prompt failed: {e}")));
                }
            }
            Cmd::Cancel { session_id } => {
                let params = json!({ "sessionId": session_id });
                if let Err(e) = rt.request("session/cancel", Some(params), Duration::from_secs(10)) {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("cancel failed: {e}")));
                }
            }
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
fn local_defaults() -> (Option<String>, Option<String>) {
    let root = std::env::var("DSH_HOME").ok().or_else(|| {
        std::env::var("HOME").ok().map(|h| format!("{h}/.dsh"))
    });
    let Some(root) = root else { return (None, None) };
    let path = std::path::Path::new(&root).join("settings.yaml");
    let Ok(text) = std::fs::read_to_string(path) else { return (None, None) };
    let mut in_block = false;
    let mut provider = None;
    let mut model = None;
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) {
            in_block = line.trim_end() == "agent-default-model:";
            continue;
        }
        if !in_block {
            continue;
        }
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        let v = v.trim().trim_matches(|c| c == '\'' || c == '"').trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "provider" => provider = Some(v.to_string()),
            "model" => model = Some(v.to_string()),
            _ => {}
        }
    }
    (provider, model)
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
    let session_id = args
        .session_id
        .clone()
        .unwrap_or_else(|| format!("dsh-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()));

    let (bus_tx, bus_rx) = mpsc::channel::<AppEvent>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();

    let runtime: Option<Arc<Runtime>> = if args.demo {
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
        bail!("skeleton: standalone spawn not wired yet — use --demo or --attach-fds")
    };

    let (provider, model) = {
        let local = local_defaults();
        (
            args.provider.clone().or(local.0).unwrap_or_else(|| "deepseek-official".into()),
            args.model.clone().or(local.1).unwrap_or_else(|| "deepseek-v4-flash".into()),
        )
    };
    {
        let tx = bus_tx.clone();
        let cfg = RuntimeCfg {
            cwd,
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
            .spawn(move || loop {
                match crossterm::event::read() {
                    Ok(ev) => {
                        if tx.send(AppEvent::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            })
            .expect("spawn input thread");
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;

    let theme = theme::theme_for(&args.theme);
    let mut app = App::new(theme, session_id, model, args.demo, cmd_tx.clone());
    if args.demo {
        demo::seed(&mut app);
    }

    loop {
        if app.needs_redraw {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            app.needs_redraw = false;
        }
        match bus_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ev) => {
                app.handle(ev);
                while let Ok(ev) = bus_rx.try_recv() {
                    app.handle(ev);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
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
