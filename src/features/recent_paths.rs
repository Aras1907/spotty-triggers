use crate::search::{Action, ResultKind, SearchResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_RECENT_PATHS: usize = 20;

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentPaths {
    paths: Vec<PathBuf>,
}

impl RecentPaths {
    fn path() -> PathBuf {
        dirs::config_dir().unwrap().join("spotty/recent_paths.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(path, json);
        }
    }

    fn record(&mut self, path: &Path) {
        if !path.exists() {
            return;
        }
        self.paths.retain(|p| p != path);
        self.paths.insert(0, path.to_path_buf());
        self.paths.truncate(MAX_RECENT_PATHS);
        self.save();
    }

    fn remove(&mut self, path: &Path) {
        self.paths.retain(|p| p != path);
        self.save();
    }
}

static RECENT: OnceLock<Mutex<RecentPaths>> = OnceLock::new();

fn instance() -> &'static Mutex<RecentPaths> {
    RECENT.get_or_init(|| Mutex::new(RecentPaths::load()))
}

pub fn record(path: &Path) {
    if let Ok(mut recent) = instance().lock() {
        recent.record(path);
    }
}

pub fn remove(path: &Path) {
    if let Ok(mut recent) = instance().lock() {
        recent.remove(path);
    }
}

pub fn results() -> Vec<SearchResult> {
    let Ok(recent) = instance().lock() else {
        return Vec::new();
    };
    recent
        .paths
        .iter()
        .filter(|path| path.exists())
        .take(MAX_RECENT_PATHS)
        .map(|path| {
            let is_dir = path.is_dir();
            SearchResult {
                kind: if is_dir {
                    ResultKind::Folder
                } else {
                    ResultKind::File
                },
                title: path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| path.display().to_string()),
                subtitle: Some(format!(
                    "Recent {} - {}",
                    if is_dir { "folder" } else { "file" },
                    path.display()
                )),
                icon: Some(if is_dir {
                    "folder-symbolic".into()
                } else {
                    "text-x-generic-symbolic".into()
                }),
                action: if is_dir {
                    Action::BrowseInto(path.clone())
                } else {
                    Action::OpenPath(path.clone())
                },
                score: 20_000,
            }
        })
        .collect()
}
