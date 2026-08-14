//! Clipboard: native tool -> tmux buffer -> OSC 52 (docs/01 section 11).
//! Every copy also lands in a backup file; the notice names it whenever
//! delivery is unverified (SSH / tmux / headless).

use std::io::Write;
use base64::Engine;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub struct CopyOutcome {
    pub delivered: bool,
    pub backup: PathBuf,
}

fn in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

fn is_remote() -> bool {
    std::env::var("SSH_TTY").is_ok() || std::env::var("SSH_CONNECTION").is_ok()
}

fn backup_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".dsh").join("last-copy.txt")
}

fn pipe_cmd(cmd: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let ok = child
        .stdin
        .as_mut()
        .map(|stdin| stdin.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    let _ = child.wait();
    ok
}

#[cfg(target_os = "macos")]
fn native_copy(text: &str) -> bool {
    pipe_cmd("pbcopy", &[], text)
}

#[cfg(target_os = "linux")]
fn native_copy(text: &str) -> bool {
    for (cmd, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ] {
        if pipe_cmd(cmd, args, text) {
            return true;
        }
    }
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_copy(_text: &str) -> bool {
    false
}

fn osc52_copy(text: &str) -> bool {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > 100_000 {
        return false;
    }
    let esc = "\x1b";
    let seq = format!("{}]52;c;{}{}", esc, encoded, "\x07");
    let framed = if in_tmux() {
        let inner = seq.replace(esc, &format!("{esc}{esc}"));
        format!("{}Ptmux;{}{}", esc, inner, "\x5c")
    } else {
        seq
    };
    let mut stdout = std::io::stdout();
    stdout.write_all(framed.as_bytes()).is_ok() && stdout.flush().is_ok()
}

/// Copy text through native -> tmux -> OSC 52, always writing a backup.
pub fn copy(text: &str) -> CopyOutcome {
    let native_ok = native_copy(text);
    let mut delivered = native_ok;
    if in_tmux() {
        delivered |= pipe_cmd("tmux", &["load-buffer", "-"], text);
    }
    if !native_ok || in_tmux() || is_remote() {
        delivered |= osc52_copy(text);
    }
    let backup = backup_path();
    if let Some(parent) = backup.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&backup, text);
    CopyOutcome { delivered, backup }
}
