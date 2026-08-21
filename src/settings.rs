//! ~/.dsh/settings.yaml 里 `dsh-whale-tui:` 块的读写。
//!
//! 读取侧与 main.rs 原来的 local_defaults 一致：whale 块优先，
//! `agent-default-model:` 块兜底。写入侧做行级 YAML 编辑——只动 whale 块里的
//! provider/model/theme 三个键，其余内容逐行保留，避免引入完整 YAML 解析器
//! 来改写用户手维护的文件。

use std::path::PathBuf;

/// settings.yaml 的路径：DSH_HOME 优先，其次 ~/.dsh。
pub fn settings_path() -> Option<PathBuf> {
    let root = std::env::var("DSH_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.dsh")))?;
    Some(PathBuf::from(root).join("settings.yaml"))
}

/// (provider, model, theme)。provider/model 允许 agent-default-model 兜底，
/// theme 只认 whale 块——它是本 TUI 自己的偏好，与 dsh 宿主无关。
pub fn read_defaults() -> (Option<String>, Option<String>, Option<String>) {
    let Some(path) = settings_path() else {
        return (None, None, None);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None, None);
    };
    let mut block: Option<&str> = None;
    let mut whale = (None, None, None);
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
        match (b, k.trim()) {
            ("whale", "provider") => whale.0 = Some(v.to_string()),
            ("whale", "model") => whale.1 = Some(v.to_string()),
            ("whale", "theme") => whale.2 = Some(v.to_string()),
            ("agent", "provider") => agent.0 = Some(v.to_string()),
            ("agent", "model") => agent.1 = Some(v.to_string()),
            _ => {}
        }
    }
    (whale.0.or(agent.0), whale.1.or(agent.1), whale.2)
}

/// 在 whale 块里更新/插入键，返回新文件全文。纯函数，便于测试。
///
/// 键已存在则原位替换（保留原缩进），不存在则紧跟块头插入；whale 块不存在
/// 时在文件末尾新建。其余行（包括别的顶层块和注释）原样保留。
pub fn render_updates(existing: &str, updates: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let header = lines
        .iter()
        .position(|line| !line.starts_with([' ', '\t']) && line.trim_end() == "dsh-whale-tui:");
    let Some(header) = header else {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("dsh-whale-tui:".to_string());
        for (k, v) in updates {
            lines.push(format!("  {k}: {v}"));
        }
        return lines.join("\n") + "\n";
    };
    // 块体 = 块头之后到下一个顶格行（或文件尾）之间的所有行。
    let body_end = lines[header + 1..]
        .iter()
        .position(|line| !line.starts_with([' ', '\t']) && !line.trim().is_empty())
        .map(|i| header + 1 + i)
        .unwrap_or(lines.len());
    let mut insert_at = header + 1;
    for (k, v) in updates {
        let found = lines[header + 1..body_end].iter().position(|line| {
            line.starts_with([' ', '\t'])
                && line
                    .trim()
                    .split_once(':')
                    .is_some_and(|(key, _)| key.trim() == *k)
        });
        if let Some(i) = found {
            let i = header + 1 + i;
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            lines[i] = format!("{indent}{k}: {v}");
        } else {
            lines.insert(insert_at, format!("  {k}: {v}"));
            insert_at += 1;
        }
    }
    lines.join("\n") + "\n"
}

/// 落盘：读-改-写，tmp + rename 避免写一半留下残文件。
pub fn update(updates: &[(&str, &str)]) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no DSH_HOME or HOME for settings.yaml",
        ));
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let rendered = render_updates(&existing, updates);
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, rendered)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_existing_keys_in_place_and_keeps_other_blocks() {
        let before = "dsh-whale-tui:\n  provider: old\n  model: old-m\nui-onboarding:\n  x: 1\n";
        let after = render_updates(before, &[("provider", "new"), ("model", "new-m")]);
        assert_eq!(
            after,
            "dsh-whale-tui:\n  provider: new\n  model: new-m\nui-onboarding:\n  x: 1\n"
        );
    }

    #[test]
    fn inserts_missing_keys_after_the_block_header() {
        let before = "dsh-whale-tui:\n  provider: p\nother:\n  z: 2\n";
        let after = render_updates(before, &[("theme", "light")]);
        assert_eq!(
            after,
            "dsh-whale-tui:\n  theme: light\n  provider: p\nother:\n  z: 2\n"
        );
    }

    #[test]
    fn creates_the_block_when_absent() {
        let after = render_updates("permission:\n  defaultPreset: x\n", &[("model", "m")]);
        assert_eq!(
            after,
            "permission:\n  defaultPreset: x\n\ndsh-whale-tui:\n  model: m\n"
        );
        let empty = render_updates("", &[("theme", "dark")]);
        assert_eq!(empty, "dsh-whale-tui:\n  theme: dark\n");
    }

    #[test]
    fn preserves_indent_and_comment_lines_outside_the_block() {
        let before = "# top\ndsh-whale-tui:\n    model: a\n# tail\n";
        let after = render_updates(before, &[("model", "b")]);
        assert_eq!(after, "# top\ndsh-whale-tui:\n    model: b\n# tail\n");
    }
}
