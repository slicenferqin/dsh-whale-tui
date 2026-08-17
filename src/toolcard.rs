//! Per-tool card rendering (docs/01 section 4).
//!
//! Grok renders each tool call as a card whose header, fold preview, and body
//! all depend on what kind of tool it was: an execute block shows the command
//! and head/tail of output, an edit block shows the path and a +N/-M diff stat.
//! We classify on the *stem* of the tool name rather than a fixed vocabulary,
//! because the harness (and any MCP server behind it) is free to name tools
//! whatever it likes.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Execute,
    Edit,
    Read,
    Search,
    Web,
    Todo,
    Other,
}

impl ToolKind {
    pub fn classify(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|needle| n.contains(needle));
        // Order matters: `web_search` must land on Web, not Search.
        if crate::transcript::is_todo_tool(&n) {
            Self::Todo
        } else if has(&["web", "browse", "http", "fetch_url", "url_fetch"]) {
            Self::Web
        } else if has(&["bash", "shell", "exec", "terminal", "command", "run_"]) || n == "run" {
            Self::Execute
        } else if has(&["edit", "write", "patch", "apply", "replace", "create_file", "insert"]) {
            Self::Edit
        } else if has(&["grep", "search", "glob", "find", "list_dir", "ls", "ripgrep", "rg"]) {
            Self::Search
        } else if has(&["read", "cat", "view", "open", "load_file"]) {
            Self::Read
        } else {
            Self::Other
        }
    }

    /// Argument keys that make the best card header for this kind, most
    /// specific first.
    fn header_keys(self) -> &'static [&'static str] {
        match self {
            Self::Execute => &["command", "cmd", "script", "shell_command"],
            Self::Edit => &["file_path", "path", "file", "filename", "target"],
            Self::Read => &["file_path", "path", "file", "filename", "url"],
            Self::Search => &["pattern", "regex", "query", "glob", "path", "dir"],
            Self::Web => &["query", "url", "q"],
            Self::Todo | Self::Other => &[],
        }
    }

    /// Prefix shown before the header, the way Grok labels its blocks.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Execute => "Run",
            Self::Edit => "Edit",
            Self::Read => "Read",
            Self::Search => "Search",
            Self::Web => "Web",
            Self::Todo => "Tasks",
            Self::Other => "",
        }
    }
}

/// Build the card header for a tool call: `Run cargo test`, `Edit src/app.rs`,
/// `Search "fn main"`. Falls back to the bare tool name when the arguments
/// carry nothing recognizable, so an unknown tool never renders headerless.
pub fn header(name: &str, arguments: &str) -> String {
    let kind = ToolKind::classify(name);
    let Some(detail) = header_detail(kind, arguments) else {
        return name.to_string();
    };
    let label = kind.label();
    if label.is_empty() {
        format!("{name} {detail}")
    } else {
        format!("{label} {detail}")
    }
}

fn header_detail(kind: ToolKind, arguments: &str) -> Option<String> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let object = value.as_object()?;
    for key in kind.header_keys() {
        if let Some(raw) = object.get(*key).and_then(Value::as_str) {
            let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let flat = flat.trim();
            if !flat.is_empty() {
                return Some(flat.to_string());
            }
        }
    }
    None
}

/// `+N -M` line counts when `body` looks like a unified diff. Returns None for
/// anything that is not diff-shaped, so callers can fall back to a text preview.
pub fn diff_stat(body: &str) -> Option<(usize, usize)> {
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut saw_marker = false;
    for line in body.lines() {
        if line.starts_with("@@") || line.starts_with("diff --git") {
            saw_marker = true;
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            // A bare "+" column with content; ignore "++" continuation noise.
            if !rest.starts_with('+') {
                added += 1;
            }
        } else if let Some(rest) = line.strip_prefix('-') {
            if !rest.starts_with('-') {
                removed += 1;
            }
        }
    }
    // Require either a real hunk marker or enough +/- lines that this cannot be
    // prose that happens to start with a dash.
    if saw_marker || added + removed >= 2 {
        Some((added, removed))
    } else {
        None
    }
}

/// What a folded card shows. Grok collapses an execute block to its first two
/// and last three output lines, summarizes a diff as `+N -M`, and reduces
/// everything else to one truncated line.
pub fn fold_preview(kind: ToolKind, body: &str) -> Vec<String> {
    let body = body.trim_end();
    if body.is_empty() {
        return Vec::new();
    }
    if let Some((added, removed)) = diff_stat(body) {
        return vec![format!("+{added} -{removed}")];
    }
    if kind == ToolKind::Execute {
        const HEAD: usize = 2;
        const TAIL: usize = 3;
        let lines: Vec<&str> = body.lines().collect();
        if lines.len() <= HEAD + TAIL + 1 {
            return lines.iter().map(|l| l.to_string()).collect();
        }
        let hidden = lines.len() - HEAD - TAIL;
        let mut out: Vec<String> = lines[..HEAD].iter().map(|l| l.to_string()).collect();
        out.push(format!("… {hidden} more lines"));
        out.extend(lines[lines.len() - TAIL..].iter().map(|l| l.to_string()));
        return out;
    }
    vec![body.replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ")]
}

/// Style one body line. Diffs use the dedicated `diff_*` theme slots; execute
/// output stays neutral so real stderr colouring is not competed with.
pub fn body_line(raw: &str, is_diff: bool, theme: &Theme, indent: &str) -> Line<'static> {
    let (color, modifier) = if !is_diff {
        (theme.text_secondary, Modifier::empty())
    } else if raw.starts_with("@@") {
        (theme.accent_plan, Modifier::BOLD)
    } else if raw.starts_with("diff --git") || raw.starts_with("index ") {
        (theme.gray_dim, Modifier::empty())
    } else if raw.starts_with("+++") || raw.starts_with("---") {
        (theme.gray, Modifier::empty())
    } else if raw.starts_with('+') {
        (theme.diff_insert_fg, Modifier::empty())
    } else if raw.starts_with('-') {
        (theme.diff_delete_fg, Modifier::empty())
    } else {
        (theme.text_secondary, Modifier::empty())
    };
    Line::from(Span::styled(
        format!("{indent}{raw}"),
        Style::default().fg(color).add_modifier(modifier),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tool_names_across_vocabularies() {
        use ToolKind::*;
        for (name, want) in [
            ("bash", Execute),
            ("shell_exec", Execute),
            ("run_command", Execute),
            ("edit", Edit),
            ("write", Edit),
            ("str_replace_editor", Edit),
            ("read", Read),
            ("read_file", Read),
            ("grep", Search),
            ("glob", Search),
            ("grep_search", Search),
            ("web_search", Web),
            ("web_fetch", Web),
            ("todo_write", Todo),
            ("goal", Other),
        ] {
            assert_eq!(ToolKind::classify(name), want, "{name}");
        }
    }

    #[test]
    fn web_search_is_web_not_search() {
        assert_eq!(ToolKind::classify("web_search"), ToolKind::Web);
    }

    #[test]
    fn header_uses_the_argument_that_identifies_the_call() {
        assert_eq!(
            header("bash", r#"{"command":"cargo test --all"}"#),
            "Run cargo test --all"
        );
        assert_eq!(
            header("edit", r#"{"file_path":"src/app.rs","old":"a","new":"b"}"#),
            "Edit src/app.rs"
        );
        assert_eq!(header("grep", r#"{"pattern":"fn main"}"#), "Search fn main");
        assert_eq!(
            header("web_search", r#"{"query":"ratatui cursor"}"#),
            "Web ratatui cursor"
        );
        // multi-line commands collapse onto one header line
        assert_eq!(
            header("bash", "{\"command\":\"set -e\\nmake build\"}"),
            "Run set -e make build"
        );
        // unknown tool / unusable args keep the bare name
        assert_eq!(header("goal", r#"{"note":"x"}"#), "goal");
        assert_eq!(header("bash", "not json"), "bash");
    }

    #[test]
    fn diff_stat_counts_hunks_and_ignores_prose() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n ctx\n-old\n+new\n+extra\n";
        assert_eq!(diff_stat(diff), Some((2, 1)));

        // prose with a single leading dash is not a diff
        assert_eq!(diff_stat("- just a bullet\nplain text"), None);
        assert_eq!(diff_stat("tests passed (2 suites)"), None);
    }

    #[test]
    fn folded_execute_output_keeps_head_and_tail() {
        let body = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = fold_preview(ToolKind::Execute, &body);
        assert_eq!(preview[0], "line 1");
        assert_eq!(preview[1], "line 2");
        assert_eq!(preview[2], "… 7 more lines");
        assert_eq!(preview[3], "line 10");
        assert_eq!(preview[5], "line 12");

        // short output is shown whole, with no ellipsis row
        let short = fold_preview(ToolKind::Execute, "a\nb\nc");
        assert_eq!(short, vec!["a", "b", "c"]);
    }

    #[test]
    fn folded_diff_summarizes_as_line_counts() {
        let diff = "@@ -1 +1,2 @@\n-a\n+b\n+c\n";
        assert_eq!(fold_preview(ToolKind::Edit, diff), vec!["+2 -1"]);
    }

    #[test]
    fn folded_other_collapses_to_one_line() {
        let preview = fold_preview(ToolKind::Other, "some\nmulti\nline   text");
        assert_eq!(preview, vec!["some multi line text"]);
    }

    #[test]
    fn diff_bodies_use_the_dedicated_diff_theme_slots() {
        let theme = crate::theme::DARK;
        let plus = body_line("+added", true, &theme, "  ");
        assert_eq!(plus.spans[0].style.fg, Some(theme.diff_insert_fg));
        let minus = body_line("-gone", true, &theme, "  ");
        assert_eq!(minus.spans[0].style.fg, Some(theme.diff_delete_fg));
        let hunk = body_line("@@ -1 +1 @@", true, &theme, "  ");
        assert_eq!(hunk.spans[0].style.fg, Some(theme.accent_plan));

        // non-diff output must not be painted green/red by a leading dash
        let output = body_line("-rw-r--r-- 1 me me", false, &theme, "  ");
        assert_eq!(output.spans[0].style.fg, Some(theme.text_secondary));
    }
}
