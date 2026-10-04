// Local prediction system: tracks how often the user runs each
// (query → action-title) pair and suggests autocompletion.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// Maps lowercase query → vec of (selected title, count) ordered by count desc.
    pub entries: HashMap<String, Vec<(String, u32)>>,
}

impl History {
    fn path() -> PathBuf {
        dirs::config_dir().unwrap().join("spotty/history.json")
    }
    pub fn load() -> Self {
        if !crate::security::search_history_enabled() { return Self::default(); }
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            let _ = crate::security::private_dir(parent);
        }
        if let Ok(s) = serde_json::to_string(self) {
            let _ = crate::security::write_private(p, s);
        }
    }
    pub fn record(&mut self, query: &str, title: &str) {
        // Bound persistence and do not record clipboard/translation content
        // unless the user explicitly enables history.
        if !crate::security::search_history_enabled() || query.len() > 4096 || title.len() > 4096 {
            return;
        }
        if self.entries.len() >= 1000 && !self.entries.contains_key(&query.trim().to_lowercase()) {
            self.entries.clear();
        }
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        let entry = self.entries.entry(q).or_default();
        if let Some(item) = entry.iter_mut().find(|(t, _)| t == title) {
            item.1 = item.1.saturating_add(1);
        } else {
            entry.push((title.to_string(), 1));
        }
        entry.sort_by(|a, b| b.1.cmp(&a.1));
        entry.truncate(5);
    }
}

static HISTORY: OnceLock<Mutex<History>> = OnceLock::new();

pub fn instance() -> &'static Mutex<History> {
    HISTORY.get_or_init(|| Mutex::new(History::load()))
}

pub fn record(query: &str, title: &str) {
    if !crate::security::search_history_enabled() { return; }
    if let Ok(mut h) = instance().lock() {
        h.record(query, title);
        h.save();
    }
}

/// Bonus score to apply for results matching frequent past selections.
pub fn frequency_bonus_for(query: &str, title: &str) -> i32 {
    if !crate::security::search_history_enabled() { return 0; }
    if let Ok(h) = instance().lock() {
        if let Some(items) = h.entries.get(&query.to_lowercase()) {
            if let Some((_, c)) = items.iter().find(|(t, _)| t == title) {
                return (*c).min(1000) as i32 * 50;
            }
        }
    }
    0
}
