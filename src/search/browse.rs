use crate::search::{Action, ResultKind, SearchResult};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_DIRECT_RESULTS: usize = 50;
const MAX_RECURSIVE_RESULTS: usize = 30;
const MAX_RECURSIVE_VISITS: usize = 3_000;
const MAX_RECURSIVE_DEPTH: usize = 5;

pub fn browse(query: &str) -> Vec<SearchResult> {
    let expanded = expand(query);
    let browsing_dir = expanded.ends_with('/');
    let (dir, prefix): (PathBuf, String) = if browsing_dir {
        (PathBuf::from(&expanded), String::new())
    } else {
        let p = PathBuf::from(&expanded);
        let d = p
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let pfx = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        (d, pfx)
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        return vec![];
    };

    let mut entries: Vec<(PathBuf, String, bool)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some((e.path(), name, is_dir))
        })
        .collect();

    let mut results: Vec<SearchResult> = if prefix.is_empty() {
        entries.sort_by(|a, b| match (a.2, b.2) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
        });
        entries
            .into_iter()
            .take(MAX_DIRECT_RESULTS)
            .enumerate()
            .map(|(i, (p, n, d))| mk(p, n, d, 1000 - i as i32))
            .collect()
    } else {
        let pl = prefix.to_lowercase();
        let mut scored: Vec<_> = entries
            .into_iter()
            .map(|(p, n, d)| {
                let nl = n.to_lowercase();
                let score = if nl == pl {
                    1_500
                } else if nl.starts_with(&pl) {
                    1_000
                } else if nl.contains(&pl) {
                    500
                } else {
                    0
                };
                (p, n, d, score)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.3.cmp(&a.3).then_with(|| match (a.2, b.2) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
            })
        });
        let mut out = scored
            .into_iter()
            .take(MAX_DIRECT_RESULTS)
            .map(|(p, n, d, s)| mk(p, n, d, s))
            .collect::<Vec<_>>();
        if out.len() < 12 {
            out.extend(recursive_matches(&dir, &pl, &out));
        }
        out
    };

    add_full_path_result(&expanded, &mut results);

    // In an exact directory scope (`/path/to/folder/`), the list should reflect
    // only the folder's contents. Outside that scope, keep the current
    // directory available as a path-level action.
    if dir.exists() && !browsing_dir {
        results.insert(
            0,
            SearchResult {
                kind: ResultKind::Folder,
                title: dir.display().to_string(),
                subtitle: Some("Enter: open in Files  •  Tab: complete path".into()),
                icon: Some("folder-open-symbolic".into()),
                action: Action::OpenInFileManager(dir),
                score: i32::MAX,
            },
        );
    }
    results
}

fn recursive_matches(dir: &Path, needle: &str, existing: &[SearchResult]) -> Vec<SearchResult> {
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    let mut scored = Vec::new();

    while let Some((current, depth)) = stack.pop() {
        if visited >= MAX_RECURSIVE_VISITS || depth >= MAX_RECURSIVE_DEPTH {
            continue;
        }
        let Ok(rd) = fs::read_dir(&current) else {
            continue;
        };
        for entry in rd.flatten() {
            visited += 1;
            if visited >= MAX_RECURSIVE_VISITS {
                break;
            }
            let path = entry.path();
            if is_existing_result(&path, existing) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                stack.push((path.clone(), depth + 1));
            }
            let name_l = name.to_lowercase();
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .display()
                .to_string()
                .to_lowercase();
            let score = if name_l.starts_with(needle) {
                850
            } else if name_l.contains(needle) {
                650
            } else if rel.contains(needle) {
                450
            } else {
                continue;
            };
            scored.push((path, name, is_dir, score));
        }
    }

    scored.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    scored
        .into_iter()
        .take(MAX_RECURSIVE_RESULTS)
        .map(|(p, n, d, s)| {
            let mut result = mk(p.clone(), n, d, s);
            result.subtitle = Some(format!("Inside {}", p.display()));
            result
        })
        .collect()
}

fn add_full_path_result(expanded: &str, results: &mut Vec<SearchResult>) {
    if expanded.ends_with('/') {
        return;
    }
    let path = PathBuf::from(expanded);
    if !path.exists() || is_existing_result(&path, results) {
        return;
    }
    let is_dir = path.is_dir();
    let mut result = mk(
        path.clone(),
        path.display().to_string(),
        is_dir,
        i32::MAX - 1,
    );
    result.subtitle = Some("Full path".into());
    results.insert(0, result);
}

fn is_existing_result(path: &Path, results: &[SearchResult]) -> bool {
    results.iter().any(|result| match &result.action {
        Action::OpenPath(p) | Action::OpenInFileManager(p) | Action::BrowseInto(p) => p == path,
        _ => false,
    })
}

fn mk(path: PathBuf, name: String, is_dir: bool, score: i32) -> SearchResult {
    let (kind, icon, action, subtitle) = if is_dir {
        (
            ResultKind::Folder,
            "folder-symbolic",
            Action::BrowseInto(path.clone()),
            format!("Enter: open  •  Tab: complete path"),
        )
    } else {
        (
            ResultKind::File,
            "text-x-generic-symbolic",
            Action::OpenPath(path.clone()),
            path.display().to_string(),
        )
    };
    SearchResult {
        kind,
        title: name,
        subtitle: Some(subtitle),
        icon: Some(icon.into()),
        action,
        score,
    }
}

fn expand(s: &str) -> String {
    if let Some(r) = s.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(r).display().to_string();
        }
    }
    if s == "~" {
        if let Some(h) = dirs::home_dir() {
            return h.display().to_string();
        }
    }
    s.to_string()
}
