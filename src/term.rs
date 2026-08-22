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

pub fn detect() -> TermKind {
    let prog = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    match prog.as_str() {
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
    }
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

/// OSC52 写系统剪贴板：`\x1b]52;c;<base64>\x07`。tmux 里也发裸序列——
/// tmux 会消费它并在 `set-clipboard external/on`（默认 external）时存入
/// 自己的 buffer 并转发给外层终端。DCS 包裹（`\x1bPtmux;…`）是旧方案，
/// tmux ≥3.3 默认 `allow-passthrough off`，会被静默丢弃。
pub(crate) fn osc52_sequence(text: &str) -> String {
    let payload = base64_encode(text.as_bytes());
    format!("\x1b]52;c;{payload}\x07")
}

pub fn osc52_copy(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "{}", osc52_sequence(text));
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

    #[test]
    fn osc52_sequence_is_raw_osc52() {
        // tmux 外：终端直接放行；tmux 内：tmux 消费后按 set-clipboard 转发。
        assert_eq!(osc52_sequence("foo"), "\x1b]52;c;Zm9v\x07");
    }
}
