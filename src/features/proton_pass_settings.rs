//! Proton Pass's settings in Spotty: how long copied secrets stay on the
//! clipboard, what the vault window shows, which vault it opens on, and whether
//! the `pass` trigger lists item titles.
//!
//! Saved in `~/.config/spotty/pass-settings.json` (readable by you only). The
//! file holds preferences, never a secret. The PIN lock keeps its own file,
//! `pass-pin.json` (see `proton_pass_pin`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// The clipboard delays offered, in seconds.
pub const CLIPBOARD_CHOICES: [u32; 5] = [15, 30, 60, 120, 300];
const DEFAULT_CLIPBOARD_SECS: u32 = 30;
const MAX_VAULT_NAME: usize = 200;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    /// Seconds a copied secret stays on the clipboard. One of [`CLIPBOARD_CHOICES`].
    pub clipboard_secs: u32,
    /// Show the username or email under each item in the vault window.
    pub show_username: bool,
    /// The vault the window opens on, by name. Empty: all vaults.
    pub default_vault: String,
    /// Whether the `pass` trigger lists item titles. Off: only the sign-in,
    /// vault and PIN rows are shown.
    pub search_titles: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { clipboard_secs: DEFAULT_CLIPBOARD_SECS, show_username: true, default_vault: String::new(), search_titles: true }
    }
}

impl Settings {
    /// Put every value into the range the app accepts.
    pub fn sanitized(mut self) -> Self {
        if !CLIPBOARD_CHOICES.contains(&self.clipboard_secs) {
            self.clipboard_secs = DEFAULT_CLIPBOARD_SECS;
        }
        let vault: String = self.default_vault.trim().chars().filter(|c| !c.is_control()).take(MAX_VAULT_NAME).collect();
        self.default_vault = vault;
        self
    }
}

/// Read settings from their text. Anything unreadable gives the defaults.
pub fn parse(text: &str) -> Settings {
    serde_json::from_str::<Settings>(text).unwrap_or_default().sanitized()
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("spotty").join("pass-settings.json"))
}

fn cache() -> std::sync::MutexGuard<'static, Option<Settings>> {
    static CACHE: Mutex<Option<Settings>> = Mutex::new(None);
    CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// The current settings. Read from disk once, then kept in memory.
pub fn get() -> Settings {
    let mut cache = cache();
    if let Some(settings) = cache.as_ref() {
        return settings.clone();
    }
    let settings = path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default();
    *cache = Some(settings.clone());
    settings
}

/// Change the settings and save them. Returns the saved values.
pub fn update(change: impl FnOnce(&mut Settings)) -> std::io::Result<Settings> {
    let mut settings = get();
    change(&mut settings);
    let settings = settings.sanitized();
    save(&settings)?;
    *cache() = Some(settings.clone());
    Ok(settings)
}

fn save(settings: &Settings) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let target = path().ok_or_else(|| std::io::Error::other("no config folder"))?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = target.with_extension("json.new");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(temp, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_private_and_safe() {
        let s = Settings::default();
        assert_eq!(s.clipboard_secs, 30);
        assert!(s.show_username && s.search_titles);
        assert!(s.default_vault.is_empty());
    }

    #[test]
    fn unreadable_or_partial_text_gives_defaults_for_the_rest() {
        assert_eq!(parse("not json"), Settings::default());
        let s = parse(r#"{"clipboard_secs":120,"unknown":true}"#);
        assert_eq!(s.clipboard_secs, 120);
        assert!(s.show_username);
    }

    #[test]
    fn a_clipboard_delay_outside_the_choices_falls_back() {
        assert_eq!(parse(r#"{"clipboard_secs":3}"#).clipboard_secs, 30);
        assert_eq!(parse(r#"{"clipboard_secs":86400}"#).clipboard_secs, 30);
        for choice in CLIPBOARD_CHOICES {
            assert_eq!(parse(&format!(r#"{{"clipboard_secs":{choice}}}"#)).clipboard_secs, choice);
        }
    }

    #[test]
    fn vault_names_lose_control_characters_and_length_is_capped() {
        let s = parse("{\"default_vault\":\"  Work\\u0007 \"}");
        assert_eq!(s.default_vault, "Work");
        let long = format!(r#"{{"default_vault":"{}"}}"#, "v".repeat(500));
        assert_eq!(parse(&long).default_vault.chars().count(), MAX_VAULT_NAME);
    }

    #[test]
    fn settings_round_trip_through_their_text() {
        let s = Settings { clipboard_secs: 300, show_username: false, default_vault: "Personal".into(), search_titles: false };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(parse(&text), s);
    }
}
