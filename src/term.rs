//! Terminal detection (docs/01 section 11): identifies the hosting
//! terminal so keyboard hints can adapt (VS Code family captures Ctrl+Q,
//! tmux changes paste behavior, etc.).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Vscode,
    Cursor,
    Windsurf,
    Zed,
    AppleTerminal,
    Kitty,
    WezTerm,
    Ghostty,
    Alacritty,
    Iterm2,
    Plain,
}

impl TermKind {
    /// VS Code / Cursor / Windsurf / Zed capture Ctrl+Q in their
    /// integrated terminals; these hosts advertise Ctrl+D as the quit key.
    pub fn is_vscode_family(self) -> bool {
        matches!(
            self,
            TermKind::Vscode | TermKind::Cursor | TermKind::Windsurf | TermKind::Zed
        )
    }
}

pub fn detect() -> (TermKind, bool) {
    let prog = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    let kind = match prog.as_str() {
        "vscode" => TermKind::Vscode,
        "cursor" => TermKind::Cursor,
        "windsurf" => TermKind::Windsurf,
        "zed" => TermKind::Zed,
        "apple_terminal" => TermKind::AppleTerminal,
        "kitty" => TermKind::Kitty,
        "wezterm" => TermKind::WezTerm,
        "ghostty" => TermKind::Ghostty,
        "alacritty" => TermKind::Alacritty,
        "iterm.app" | "iterm2" => TermKind::Iterm2,
        _ => TermKind::Plain,
    };
    let in_tmux = std::env::var("TMUX").is_ok();
    (kind, in_tmux)
}

/// 标准 base64（OSC52 载荷要求）。无依赖实现，输入是任意 UTF-8 字节流。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// OSC52 写系统剪贴板：`\x1b]52;c;<base64>\x07`。tmux 里需要 DCS 包装且
/// 服务端 `set-clipboard on` 才放行，否则序列被 tmux 吃掉（无害但无效）。
pub fn osc52_copy(text: &str, in_tmux: bool) {
    use std::io::Write;
    let payload = base64_encode(text.as_bytes());
    let mut out = std::io::stdout();
    if in_tmux {
        let _ = write!(out, "\x1bPtmux;\x1b\x1b]52;c;{payload}\x07\x1b\\");
    } else {
        let _ = write!(out, "\x1b]52;c;{payload}\x07");
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_family_marks_quit_alt() {
        assert!(TermKind::Vscode.is_vscode_family());
        assert!(TermKind::Cursor.is_vscode_family());
        assert!(!TermKind::Kitty.is_vscode_family());
    }

    #[test]
    fn detect_runs() {
        let _ = detect();
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode("你好".as_bytes()), "5L2g5aW9");
    }
}
