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
mod settings;
mod term;
mod theme;
mod toolcard;
mod transcript;
mod ui;

use std::io::Write;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
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
    theme: Option<String>,
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
        theme: None,
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
            "--theme" => args.theme = Some(take("--theme")?),
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
    /// 仅命令行显式传入的 --provider/--model 会发给桥的 initialize；
    /// 不传时桥用 agentDefaultModel 里持久化的选择（README 的优先级约定：
    /// 命令行 > dsh-whale-tui 块 > agent-default-model > stock）。
    cli_provider: Option<String>,
    cli_model: Option<String>,
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
    let mut init = json!({ "cwd": cfg.cwd });
    if let Some(provider) = &cfg.cli_provider {
        init["provider"] = json!(provider);
    }
    if let Some(model) = &cfg.cli_model {
        init["model"] = json!(model);
    }
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
                if let Err(e) = rt.request(
                    "session/update-queue",
                    Some(params),
                    Duration::from_secs(10),
                ) {
                    let _ = bus.send(AppEvent::RuntimeStderr(format!("queue update failed: {e}")));
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
            Cmd::ListProviders => {
                match rt.request("tui/list-providers", None, Duration::from_secs(30)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/providers".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        let _ = bus.send(AppEvent::RuntimeStderr(format!(
                            "list-providers failed: {e}"
                        )));
                    }
                }
            }
            Cmd::SaveProvider { draft } => {
                match rt.request("tui/save-provider", Some(draft), Duration::from_secs(30)) {
                    Ok(res) => {
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/provider-saved".to_string(),
                            params: res,
                        });
                    }
                    Err(e) => {
                        // 桥端校验失败（id 重复、schema 拒绝等）也走同一事件，
                        // 由 app 侧统一以通知呈现。
                        let _ = bus.send(AppEvent::Rpc {
                            method: "tui/provider-saved".to_string(),
                            params: json!({ "ok": false, "error": e.to_string() }),
                        });
                    }
                }
            }
            Cmd::FetchModels {
                api,
                base_url,
                api_key,
            } => {
                let params = json!({ "api": api, "baseURL": base_url, "apiKey": api_key });
                let event = match rt.request(
                    "tui/fetch-models",
                    Some(params),
                    Duration::from_secs(30),
                ) {
                    Ok(res) => {
                        json!({ "ok": true, "models": res.get("models").cloned().unwrap_or(json!([])) })
                    }
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                };
                let _ = bus.send(AppEvent::Rpc {
                    method: "tui/models-fetched".to_string(),
                    params: event,
                });
            }
            Cmd::RemoveProvider { id } => {
                let event = match rt.request(
                    "tui/remove-provider",
                    Some(json!({ "id": id })),
                    Duration::from_secs(30),
                ) {
                    Ok(_) => json!({ "ok": true, "id": id }),
                    Err(e) => json!({ "ok": false, "id": id, "error": e.to_string() }),
                };
                let _ = bus.send(AppEvent::Rpc {
                    method: "tui/provider-removed".to_string(),
                    params: event,
                });
            }
            Cmd::SetProviderKey { id, api_key } => {
                let event = match rt.request(
                    "tui/set-provider-key",
                    Some(json!({ "id": id, "apiKey": api_key })),
                    Duration::from_secs(30),
                ) {
                    Ok(_) => json!({ "ok": true, "id": id }),
                    Err(e) => json!({ "ok": false, "id": id, "error": e.to_string() }),
                };
                let _ = bus.send(AppEvent::Rpc {
                    method: "tui/key-saved".to_string(),
                    params: event,
                });
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

    let local = if args.dump_frame.is_some() {
        (None, None, None)
    } else {
        settings::read_defaults()
    };
    let (provider, model) = (
        args.provider
            .clone()
            .or(local.0)
            .unwrap_or_else(|| "deepseek-official".into()),
        args.model
            .clone()
            .or(local.1)
            .unwrap_or_else(|| "deepseek-v4-flash".into()),
    );
    // --theme flag > dsh-whale-tui.theme in settings.yaml > dark.
    let theme_name = args
        .theme
        .as_deref()
        .or(local.2.as_deref())
        .unwrap_or("dark")
        .to_string();
    if let Some(size) = args.dump_frame {
        return dump_demo_frame(
            size,
            theme::theme_for(&theme_name),
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
            cli_provider: args.provider.clone(),
            cli_model: args.model.clone(),
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
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    // Mouse reporting is applied from app state on the first loop pass, so the
    // default lives in one place (App::new) rather than being duplicated here.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;

    let theme = theme::theme_for(&theme_name);
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

    // Frame budget. A busy session can emit many events per visible change
    // (session.event plus one notification per changed projection), and drawing
    // once per event floods the terminal with escape sequences it then has to
    // parse — which shows up as input lag, because keystrokes queue on the same
    // bus behind that work. Cap the draw rate and let the coalesced state catch
    // up in one frame instead.
    const FRAME: Duration = Duration::from_millis(16);
    const IDLE_WAIT: Duration = Duration::from_millis(80);
    let mut last_draw = std::time::Instant::now() - FRAME;

    loop {
        if app.mouse_capture_dirty {
            set_mouse_reporting(app.mouse_capture);
            app.mouse_capture_dirty = false;
        }
        // Wait only as long as the pending frame allows, so a deferred draw is
        // never delayed by the full idle timeout.
        let wait = if app.needs_redraw {
            let since = last_draw.elapsed();
            if since >= FRAME {
                terminal.draw(|f| ui::draw(f, &mut app))?;
                app.needs_redraw = false;
                last_draw = std::time::Instant::now();
                IDLE_WAIT
            } else {
                FRAME - since
            }
        } else {
            IDLE_WAIT
        };
        match bus_rx.recv_timeout(wait) {
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

/// Mouse reporting, narrowed to what we actually use.
///
/// crossterm's `EnableMouseCapture` also turns on `?1003h` (any-event tracking),
/// which reports **every pointer movement** — not just drags. Each of those woke
/// a full redraw, so simply moving the mouse across the TUI made the screen
/// judder. We only need button press/release (`?1000h`, which also carries wheel
/// events) plus SGR coordinates (`?1006h`) for terminals wider than 223 columns.
///
/// Any tracking mode suppresses the terminal's own text selection. Most
/// terminals let you hold Shift to get it back; `/mouse` turns reporting off
/// entirely for the ones that do not.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

fn set_mouse_reporting(enabled: bool) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(if enabled { MOUSE_ON } else { MOUSE_OFF }.as_bytes());
    let _ = stdout.flush();
}

fn restore_terminal() {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(MOUSE_OFF.as_bytes());
    let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
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
