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
    pub fn name(self) -> &'static str {
        match self {
            TermKind::Vscode => "vscode",
            TermKind::Cursor => "cursor",
            TermKind::Windsurf => "windsurf",
            TermKind::Zed => "zed",
            TermKind::AppleTerminal => "apple-terminal",
            TermKind::Kitty => "kitty",
            TermKind::WezTerm => "wezterm",
            TermKind::Ghostty => "ghostty",
            TermKind::Alacritty => "alacritty",
            TermKind::Iterm2 => "iterm2",
            TermKind::Plain => "plain",
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_family_marks_quit_alt() {
        assert!(TermKind::Vscode.is_vscode_family());
        assert!(TermKind::Cursor.is_vscode_family());
        assert!(!TermKind::Kitty.is_vscode_family());
        assert_eq!(TermKind::WezTerm.name(), "wezterm");
    }

    #[test]
    fn detect_runs() {
        let _ = detect();
    }
}
