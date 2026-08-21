//! /resume: client-side session discovery. The harness persists one JSONL
//! log per session; the picker reads it directly (zstd multi-frame aware)
//! and /resume asks the bridge to agents.resume the chosen session id.
//! Layout: <root>/<workspace-slug>/<session-id>/session.jsonl[.zstd].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub file: PathBuf,
    pub id: String,
    pub turns: usize,
    pub preview: String,
    pub modified: SystemTime,
}

/// Encode a workspace exactly like dsh-session-persistence-jsonl's
/// projectKey(): separator runs become '-', unsafe UTF-16 code units become
/// ~XXXX, and the readable component is bounded to filesystem limits.
pub fn workspace_slug(workspace: &str) -> String {
    assert!(!workspace.is_empty(), "cannot encode an empty project path");
    let mut readable = String::new();
    let mut separator_run = false;

    for code in workspace.encode_utf16() {
        let is_separator =
            code == u16::from(b'/') || code == u16::from(b'\\') || code == u16::from(b':');
        let ascii = u8::try_from(code).ok();
        let is_safe = ascii.is_some_and(|byte| {
            byte != b'~' && (byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
        if is_separator {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else if is_safe {
            readable.push(char::from(ascii.expect("safe code unit is ASCII")));
            separator_run = false;
        } else {
            readable.push_str(&format!("~{code:04X}"));
            separator_run = false;
        }
    }

    let readable = readable.trim_start_matches('-');
    let readable = if readable.is_empty() {
        "root"
    } else {
        readable
    };
    let bounded: String = readable.chars().take(251).collect();
    format!("--{bounded}--")
}

fn session_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("DSH_HOME") {
        roots.push(PathBuf::from(&home).join("sessions"));
    }
    if let Ok(home) = std::env::var("HOME") {
        roots.push(Path::new(&home).join(".dsh").join("sessions"));
    }
    if let Ok(root) = std::env::var("DSH_SESSION_ROOT") {
        roots.push(PathBuf::from(root));
    }
    roots.sort();
    roots.dedup();
    roots.retain(|r| r.is_dir());
    roots
}

fn session_file(dir: &Path) -> Option<PathBuf> {
    for name in ["session.jsonl", "session.jsonl.zstd"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// List resumable sessions for the workspace, newest first, excluding
/// skip_id. Best effort: unreadable files are skipped, never an error.
pub fn list_sessions(workspace: &str, skip_id: &str) -> Vec<SessionSummary> {
    let slug = workspace_slug(workspace);
    // 先 stat 后解析：候选文件全部读 mtime（便宜），按新到旧排序后只
    // 全量解析排在前面的。原来对每个会话都解压+解析整个 JSONL（可能
    // 数 MB），会话一多 /resume 就把 UI 线程卡死。
    let mut candidates: Vec<(PathBuf, SystemTime)> = Vec::new();
    for root in session_roots() {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root.join(&slug)) {
            dirs.extend(entries.flatten().map(|e| e.path()));
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            dirs.extend(entries.flatten().map(|e| e.path()));
        }
        for dir in dirs {
            let Some(file) = session_file(&dir) else {
                continue;
            };
            let modified = std::fs::metadata(&file)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if candidates.iter().any(|(f, _)| *f == file) {
                continue;
            }
            candidates.push((file, modified));
        }
    }
    candidates.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    let mut out: Vec<SessionSummary> = Vec::new();
    for (file, _) in candidates {
        if out.len() >= 50 {
            break;
        }
        let Some(summary) = summarize(&file) else {
            continue;
        };
        if summary.id == skip_id || out.iter().any(|s| s.id == summary.id) {
            continue;
        }
        out.push(summary);
    }
    out
}

/// Read and parse every JSONL event of one session (zstd concatenated-frame
/// aware: each flush appends a frame, so keep decoding until EOF).
pub fn read_session_events(file: &Path) -> Result<Vec<Value>> {
    let text = read_session_text(file)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect())
}

fn read_session_text(file: &Path) -> Result<String> {
    let raw = std::fs::File::open(file).with_context(|| format!("open {}", file.display()))?;
    let mut text = String::new();
    if file.extension().is_some_and(|e| e == "zstd") {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(raw);
        while !reader
            .fill_buf()
            .with_context(|| format!("read {}", file.display()))?
            .is_empty()
        {
            let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut reader)
                .with_context(|| format!("zstd frame of {}", file.display()))?;
            dec.read_to_string(&mut text)
                .with_context(|| format!("decompress {}", file.display()))?;
        }
    } else {
        let mut raw = raw;
        raw.read_to_string(&mut text)
            .with_context(|| format!("read {}", file.display()))?;
    }
    Ok(text)
}

fn user_text(event: &Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str) != Some("user/message") {
        return None;
    }
    let data = event.get("data")?;
    if data.pointer("/source/kind").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let mut out = String::new();
    for block in data.get("content")?.as_array()? {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn summarize(file: &Path) -> Option<SessionSummary> {
    let events = read_session_events(file).ok()?;
    let header = events.first()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let id = header.get("id").and_then(Value::as_str)?.to_string();
    let turns = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("turn/start"))
        .count();
    let preview = events
        .iter()
        .find_map(user_text)
        .map(|t| {
            let one_line = t.replace('\n', " ");
            let mut p: String = one_line.chars().take(40).collect();
            if one_line.chars().count() > 40 {
                p.push('…');
            }
            p
        })
        .unwrap_or_default();
    let modified = std::fs::metadata(file)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Some(SessionSummary {
        file: file.to_path_buf(),
        id,
        turns,
        preview,
        modified,
    })
}

pub fn age_label(modified: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slug_matches_host_layout() {
        assert_eq!(
            workspace_slug("/Users/sanjiu/proj"),
            "--Users-sanjiu-proj--"
        );
        assert_eq!(
            workspace_slug("/Users/三九/my projects"),
            "--Users-~4E09~4E5D-my~0020projects--"
        );
        assert_eq!(workspace_slug(r"C:\work"), "--C-work--");
        assert_eq!(workspace_slug("/"), "--root--");
    }

    #[test]
    fn reads_concatenated_zstd_frames() {
        let tmp = std::env::temp_dir().join(format!("dsh-zstd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("session.jsonl.zstd");
        let header = json!({"type":"session","id":"x"}).to_string()
            + "
";
        let body = json!({"type":"turn/start"}).to_string()
            + "
";
        let mut frames = Vec::new();
        for chunk in [header.as_bytes(), body.as_bytes()] {
            frames.extend(ruzstd::encoding::compress_to_vec(
                chunk,
                ruzstd::encoding::CompressionLevel::Fastest,
            ));
        }
        std::fs::write(&file, frames).unwrap();
        let events = read_session_events(&file).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"].as_str(), Some("session"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
