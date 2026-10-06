//! Proton Calendar and Proton Drive triggers. Proton ships neither as a Linux
//! desktop client, so both open Proton's own web apps. Drive can also search a
//! local folder the user already syncs (for example with rclone). Spotty never
//! sees Proton credentials: sign-in stays in the browser.
use super::{Action, ResultKind, SearchResult};
use crate::config::Config;
use crate::i18n::gettext;
use std::path::{Path, PathBuf};

/// Calendar views Proton's web app routes to, in Settings order.
pub const CALENDAR_VIEWS: [&str; 3] = ["day", "week", "month"];

fn account(slot: u32) -> u32 {
    slot.min(9)
}

pub fn calendar_view(config: &Config) -> &'static str {
    CALENDAR_VIEWS
        .iter()
        .copied()
        .find(|view| *view == config.proton_calendar_view)
        .unwrap_or("week")
}

/// `https://calendar.proton.me/u/{slot}/{view}/{y}/{m}/{d}`.
pub fn calendar_url(slot: u32, view: &str, date: Option<(i32, u32, u32)>) -> String {
    let base = format!("https://calendar.proton.me/u/{}/{view}", account(slot));
    match date {
        Some((y, m, d)) => format!("{base}/{y}/{m}/{d}"),
        None => base,
    }
}

pub fn drive_url(slot: u32) -> String {
    format!("https://drive.proton.me/u/{}/", account(slot))
}

fn today() -> Option<glib::DateTime> {
    glib::DateTime::now_local().ok()
}

fn ymd(date: &glib::DateTime) -> (i32, u32, u32) {
    (date.year(), date.month() as u32, date.day_of_month() as u32)
}

/// "today", "tomorrow", "yesterday" or an ISO date (2026-10-06).
fn parse_date(query: &str) -> Option<(i32, u32, u32)> {
    let q = query.trim().to_lowercase();
    let offset = match q.as_str() {
        "today" => Some(0),
        "tomorrow" => Some(1),
        "yesterday" => Some(-1),
        _ => None,
    };
    if let Some(days) = offset {
        return today().and_then(|now| now.add_days(days).ok()).map(|date| ymd(&date));
    }
    let mut parts = q.split('-');
    let y = parts.next()?.parse::<i32>().ok()?;
    let m = parts.next()?.parse::<i32>().ok()?;
    let d = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some() || !(1..=9999).contains(&y) {
        return None;
    }
    // glib validates the day against the month (and leap years).
    glib::DateTime::from_local(y, m, d, 0, 0, 0.0).ok().map(|date| ymd(&date))
}

pub fn calendar_search(query: &str, config: &Config) -> Vec<SearchResult> {
    let slot = config.proton_calendar_account;
    let view = calendar_view(config);
    let mut rows = Vec::new();
    if let Some(date) = parse_date(query) {
        rows.push(SearchResult {
            kind: ResultKind::Web,
            title: gettext("Open {date} in Proton Calendar")
                .replace("{date}", &format!("{:04}-{:02}-{:02}", date.0, date.1, date.2)),
            subtitle: Some(gettext("calendar.proton.me · {view} view").replace("{view}", view)),
            icon: Some("x-office-calendar-symbolic".into()),
            action: Action::OpenUrl(calendar_url(slot, view, Some(date))),
            score: 100_001,
        });
    }
    rows.push(SearchResult {
        kind: ResultKind::Web,
        title: gettext("Open Proton Calendar"),
        subtitle: Some(if query.trim().is_empty() {
            gettext("Today in the {view} view · type a date, today or tomorrow to jump").replace("{view}", view)
        } else {
            gettext("Today in the {view} view").replace("{view}", view)
        }),
        icon: Some("x-office-calendar-symbolic".into()),
        action: Action::OpenUrl(calendar_url(slot, view, today().map(|now| ymd(&now)))),
        score: 100_000,
    });
    rows
}

/// The configured synced folder, if it exists.
pub fn drive_folder(config: &Config) -> Option<PathBuf> {
    let raw = config.proton_drive_folder.trim();
    if raw.is_empty() {
        return None;
    }
    let path = match raw.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(raw),
    };
    path.is_dir().then_some(path)
}

const MAX_VISITED: usize = 20_000;
const MAX_DEPTH: usize = 8;
const MAX_MATCHES: usize = 20;

/// Name matches under `root`, best first: exact, prefix, then substring.
fn find_in_folder(root: &Path, query: &str) -> Vec<(i32, PathBuf)> {
    let needle = query.to_lowercase();
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_VISITED {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Never follow symlinks out of the synced folder.
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if let Some(pos) = name.find(&needle) {
                let score = if name == needle { 3 } else if pos == 0 { 2 } else { 1 };
                found.push((score, path.clone()));
            }
            if is_dir && depth + 1 < MAX_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.truncate(MAX_MATCHES);
    found
}

pub fn drive_search(query: &str, config: &Config) -> Vec<SearchResult> {
    let mut rows = vec![SearchResult {
        kind: ResultKind::Web,
        title: gettext("Open Proton Drive"),
        subtitle: Some(gettext("drive.proton.me in your browser")),
        icon: Some("folder-remote-symbolic".into()),
        action: Action::OpenUrl(drive_url(config.proton_drive_account)),
        score: 100_000,
    }];
    let Some(folder) = drive_folder(config) else { return rows };
    rows.push(SearchResult {
        kind: ResultKind::Folder,
        title: gettext("Open synced Proton Drive folder"),
        subtitle: Some(folder.display().to_string()),
        icon: Some("folder-symbolic".into()),
        action: Action::OpenPath(folder.clone()),
        score: 99_999,
    });
    let query = query.trim();
    if query.is_empty() {
        return rows;
    }
    for (rank, (score, path)) in find_in_folder(&folder, query).into_iter().enumerate() {
        let is_dir = path.is_dir();
        let relative = path.strip_prefix(&folder).unwrap_or(&path);
        rows.push(SearchResult {
            kind: if is_dir { ResultKind::Folder } else { ResultKind::File },
            title: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            subtitle: Some(relative.display().to_string()),
            icon: Some(if is_dir { "folder-symbolic" } else { "text-x-generic-symbolic" }.into()),
            action: Action::OpenPath(path),
            score: 100_100 + score * 100 - rank as i32,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_urls_use_the_account_slot_view_and_date() {
        assert_eq!(
            calendar_url(1, "month", Some((2026, 10, 6))),
            "https://calendar.proton.me/u/1/month/2026/10/6"
        );
        assert_eq!(calendar_url(42, "week", None), "https://calendar.proton.me/u/9/week");
        assert_eq!(drive_url(0), "https://drive.proton.me/u/0/");
    }

    #[test]
    fn dates_parse_iso_and_reject_invalid_days() {
        assert_eq!(parse_date("2026-02-28"), Some((2026, 2, 28)));
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("meeting"), None);
        assert!(parse_date("today").is_some());
    }

    #[test]
    fn unknown_calendar_view_falls_back_to_week() {
        let mut config = Config::default();
        config.proton_calendar_view = "agenda".into();
        assert_eq!(calendar_view(&config), "week");
        config.proton_calendar_view = "day".into();
        assert_eq!(calendar_view(&config), "day");
    }

    #[test]
    fn drive_search_finds_names_in_the_synced_folder() {
        let root = std::env::temp_dir().join(format!("spotty-drive-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Taxes/2026")).unwrap();
        std::fs::write(root.join("Taxes/2026/receipt.pdf"), b"").unwrap();
        std::fs::write(root.join(".hidden-receipt"), b"").unwrap();
        let mut config = Config::default();
        config.proton_drive_folder = root.display().to_string();

        let rows = drive_search("receipt", &config);
        let _ = std::fs::remove_dir_all(&root);
        assert!(matches!(rows[0].action, Action::OpenUrl(_)));
        let files: Vec<_> = rows.iter().filter(|r| r.kind == ResultKind::File).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "receipt.pdf");
    }
}
