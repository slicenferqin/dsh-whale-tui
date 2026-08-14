//! @file autocomplete: workspace file discovery + fuzzy substring filter.
//! Best effort; skips common heavy directories. Relative paths only.

use std::path::{Path, PathBuf};

const SKIP_DIRS: [&str; 6] = [".git", "target", "node_modules", "dist", ".venv", "vendor"];

pub fn list_files(workspace: &str) -> Vec<String> {
    let root = PathBuf::from(workspace);
    let mut out = Vec::new();
    walk(&root, &root, 0, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 6 || out.len() >= 2000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') && name != "." {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, depth + 1, out);
        } else {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel = rel.to_string_lossy().into_owned();
                if rel.starts_with(".") {
                    continue;
                }
                out.push(rel);
            }
        }
        if out.len() >= 2000 {
            return;
        }
    }
}

pub fn fuzzy_filter(files: &[String], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    (0..files.len())
        .filter(|i| {
            let f = &files[*i];
            q.is_empty() || f.to_lowercase().contains(&q)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_substrings() {
        let files = vec![
            "src/main.rs".to_string(),
            "src/app.rs".to_string(),
            "README.md".to_string(),
        ];
        let hits = fuzzy_filter(&files, "main");
        assert_eq!(hits, vec![0]);
        let all = fuzzy_filter(&files, "");
        assert_eq!(all.len(), 3);
        let none = fuzzy_filter(&files, "zzz");
        assert!(none.is_empty());
    }
}
