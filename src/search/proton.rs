//! Proton VPN, Proton Calendar and Proton Drive triggers. VPN drives Proton's
//! official Linux client library built into Spotty. Proton ships no Linux
//! desktop client for Calendar or Drive, so those open Proton's own web apps
//! inside Spotty (signed in once in Spotty's Proton window); Drive can also
//! search a local folder the user already syncs (for example with rclone).
//! Spotty never sees Proton passwords: sign-in happens on Proton's own page.
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

/// Places in Proton Drive's web app a few letters away: (word, title, path).
pub const DRIVE_PLACES: [(&str, &str, &str); 5] = [
    ("shared", "Shared by me", "shared-urls"),
    ("with-me", "Shared with me", "shared-with-me"),
    ("photos", "Photos", "photos"),
    ("devices", "Computers", "devices"),
    ("trash", "Trash", "trash"),
];

pub fn drive_place_url(slot: u32, path: &str) -> String {
    format!("https://drive.proton.me/u/{}/{path}", account(slot))
}

fn today() -> Option<glib::DateTime> {
    glib::DateTime::now_local().ok()
}

fn ymd(date: &glib::DateTime) -> (i32, u32, u32) {
    (date.year(), date.month() as u32, date.day_of_month() as u32)
}

/// Where a calendar query points: a day, and optionally a view of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct When {
    pub date: (i32, u32, u32),
    pub view: Option<&'static str>,
}

const WEEKDAYS: [(&str, &str); 7] = [
    ("monday", "mon"), ("tuesday", "tue"), ("wednesday", "wed"), ("thursday", "thu"),
    ("friday", "fri"), ("saturday", "sat"), ("sunday", "sun"),
];

const MONTHS: [(&str, &str); 12] = [
    ("january", "jan"), ("february", "feb"), ("march", "mar"), ("april", "apr"),
    ("may", "may"), ("june", "jun"), ("july", "jul"), ("august", "aug"),
    ("september", "sep"), ("october", "oct"), ("november", "nov"), ("december", "dec"),
];

/// Words `cal` completes as you type (the ghost text).
pub const CALENDAR_WORDS: [&str; 13] = [
    "today", "tomorrow", "yesterday", "monday", "tuesday", "wednesday", "thursday", "friday",
    "saturday", "sunday", "week", "month", "day",
];

fn weekday_number(word: &str) -> Option<i32> {
    WEEKDAYS
        .iter()
        .position(|(long, short)| word == *long || word == *short)
        .map(|index| index as i32 + 1)
}

fn month_number(word: &str) -> Option<i32> {
    MONTHS
        .iter()
        .position(|(long, short)| word == *long || word == *short || (word.len() >= 3 && long.starts_with(word)))
        .map(|index| index as i32 + 1)
}

/// "+3", "3d", "in 3 days", "2 weeks" → a day offset.
fn parse_offset(words: &[&str]) -> Option<i64> {
    let words: Vec<&str> = if words.first() == Some(&"in") { words[1..].to_vec() } else { words.to_vec() };
    let (number, unit) = match words.as_slice() {
        [single] => match single.find(|c: char| !(c.is_ascii_digit() || c == '+' || c == '-')) {
            Some(split) => (&single[..split], &single[split..]),
            // "+3" alone means days.
            None if single.starts_with(['+', '-']) => (*single, "d"),
            None => return None,
        },
        [number, unit] => (*number, *unit),
        _ => return None,
    };
    let count: i64 = number.trim_start_matches('+').parse().ok()?;
    if count.abs() > 3650 {
        return None;
    }
    match unit {
        "d" | "day" | "days" => Some(count),
        "w" | "wk" | "week" | "weeks" => Some(count * 7),
        _ => None,
    }
}

/// What `query` asks the calendar for, relative to `now`: "today",
/// "tomorrow", "friday", "next week", "in 3 days", "+2w", "month",
/// "24 oct", "oct 24 2027" or an ISO date (2026-10-06).
pub fn parse_when(query: &str, now: &glib::DateTime) -> Option<When> {
    let lower = query.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    let words: Vec<&str> = lower.split_whitespace().collect();
    let day = |offset: i64| now.add_days(offset as i32).ok().map(|d| ymd(&d));
    let at = |date: Option<(i32, u32, u32)>, view| date.map(|date| When { date, view });

    match words.as_slice() {
        ["today" | "now"] => return at(day(0), None),
        ["tomorrow" | "tmrw" | "tom"] => return at(day(1), None),
        ["yesterday"] => return at(day(-1), None),
        ["day"] => return at(day(0), Some("day")),
        ["week"] | ["this", "week"] => return at(day(0), Some("week")),
        ["month"] | ["this", "month"] => return at(day(0), Some("month")),
        ["next", "week"] => return at(day(7), Some("week")),
        ["last" | "previous", "week"] => return at(day(-7), Some("week")),
        ["next", "month"] | ["last" | "previous", "month"] => {
            let forward = words[0] == "next";
            let first = glib::DateTime::from_local(now.year(), now.month(), 1, 0, 0, 0.0).ok()?;
            let moved = first.add_months(if forward { 1 } else { -1 }).ok()?;
            return Some(When { date: ymd(&moved), view: Some("month") });
        }
        _ => {}
    }

    // "friday", "next friday", "this friday", "on fri".
    let weekday_words: &[&str] = match words.as_slice() {
        ["next" | "this" | "on", rest @ ..] if rest.len() == 1 => rest,
        other => other,
    };
    if let [word] = weekday_words {
        if let Some(target) = weekday_number(word) {
            let today = now.day_of_week();
            let mut ahead = (target - today).rem_euclid(7) as i64;
            // A bare weekday means the coming one; "this friday" may be today.
            if ahead == 0 && words[0] != "this" {
                ahead = 7;
            }
            return at(day(ahead), None);
        }
    }

    if let Some(offset) = parse_offset(&words) {
        return at(day(offset), None);
    }

    // ISO date.
    if words.len() == 1 && lower.contains('-') {
        let mut parts = lower.split('-');
        let y = parts.next()?.parse::<i32>().ok()?;
        let m = parts.next()?.parse::<i32>().ok()?;
        let d = parts.next()?.parse::<i32>().ok()?;
        if parts.next().is_some() || !(1..=9999).contains(&y) {
            return None;
        }
        // glib validates the day against the month (and leap years).
        return glib::DateTime::from_local(y, m, d, 0, 0, 0.0).ok().map(|date| When { date: ymd(&date), view: None });
    }

    // "24 oct", "24 oct 2027", "oct 24", "oct 24 2027".
    // "24" or "1st": a day number, optionally with an ordinal suffix.
    let day_like = |word: &str| {
        let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let suffix = &word[digits.len()..];
        !digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit())
            && matches!(suffix, "" | "st" | "nd" | "rd" | "th")
    };
    let (day_word, month_word, year_word) = match words.as_slice() {
        [a, b] if day_like(a) => (*a, *b, None),
        [a, b] if day_like(b) => (*b, *a, None),
        [a, b, y] if day_like(a) => (*a, *b, Some(*y)),
        [a, b, y] if day_like(b) => (*b, *a, Some(*y)),
        _ => return None,
    };
    let month = month_number(month_word)?;
    let day_of_month = day_word.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse::<i32>().ok()?;
    let year = match year_word {
        Some(y) => y.parse::<i32>().ok().filter(|y| (1..=9999).contains(y))?,
        None => now.year(),
    };
    glib::DateTime::from_local(year, month, day_of_month, 0, 0, 0.0)
        .ok()
        .map(|date| When { date: ymd(&date), view: None })
}

fn calendar_row(title: String, subtitle: String, url: String, score: i32) -> SearchResult {
    SearchResult {
        kind: ResultKind::Web,
        title,
        subtitle: Some(subtitle),
        icon: Some("x-office-calendar-symbolic".into()),
        action: Action::OpenProtonWeb(url),
        score,
    }
}

pub fn calendar_search(query: &str, config: &Config) -> Vec<SearchResult> {
    let slot = config.proton_calendar_account;
    let default_view = calendar_view(config);
    let now = today();
    let mut rows = Vec::new();
    if let Some(when) = now.as_ref().and_then(|now| parse_when(query, now)) {
        let view = when.view.unwrap_or(default_view);
        let (y, m, d) = when.date;
        let name = glib::DateTime::from_local(y, m as i32, d as i32, 0, 0, 0.0)
            .ok()
            .and_then(|date| date.format("%A %-d %B %Y").ok())
            .map(|text| text.to_string())
            .unwrap_or_else(|| format!("{y:04}-{m:02}-{d:02}"));
        rows.push(calendar_row(
            gettext("Open {date} in Proton Calendar").replace("{date}", &name),
            gettext("Proton Calendar in Spotty · {view} view").replace("{view}", view),
            calendar_url(slot, view, Some(when.date)),
            100_001,
        ));
    }
    let today_date = now.as_ref().map(ymd);
    rows.push(calendar_row(
        gettext("Open Proton Calendar"),
        if query.trim().is_empty() {
            gettext("Today in the {view} view · try tomorrow, friday, next week, 24 oct or +3d").replace("{view}", default_view)
        } else {
            gettext("Today in the {view} view").replace("{view}", default_view)
        },
        calendar_url(slot, default_view, today_date),
        100_000,
    ));
    if query.trim().is_empty() {
        for (offset, view) in CALENDAR_VIEWS.iter().filter(|view| **view != default_view).enumerate() {
            let label = match *view {
                "day" => gettext("Today's agenda (day view)"),
                "week" => gettext("This week"),
                _ => gettext("This month"),
            };
            rows.push(calendar_row(label, gettext("Proton Calendar in Spotty"), calendar_url(slot, view, today_date), 99_990 - offset as i32));
        }
    }
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
    let slot = config.proton_drive_account;
    let mut rows = vec![SearchResult {
        kind: ResultKind::Web,
        title: gettext("Open Proton Drive"),
        subtitle: Some(gettext("Your files, in Spotty · try trash, shared, photos")),
        icon: Some("folder-remote-symbolic".into()),
        action: Action::OpenProtonWeb(drive_url(slot)),
        score: 100_000,
    }];
    // Places in Drive, offered when nothing is typed or what is typed starts one.
    let lower = query.trim().to_lowercase();
    for (rank, (word, title, path)) in DRIVE_PLACES.iter().enumerate() {
        let named = !lower.is_empty() && (word.starts_with(&lower) || title.to_lowercase().starts_with(&lower));
        if lower.is_empty() || named {
            rows.push(SearchResult {
                kind: ResultKind::Web,
                title: gettext(title),
                subtitle: Some(gettext("Proton Drive in Spotty")),
                icon: Some("folder-remote-symbolic".into()),
                action: Action::OpenProtonWeb(drive_place_url(slot, path)),
                score: if named { 100_050 - rank as i32 } else { 99_900 - rank as i32 },
            });
        }
    }
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

// ── Proton VPN ────────────────────────────────────────────────────────────
// The `vpn` trigger drives Spotty's in-app Proton VPN client (Proton's
// official library, see crate::proton_vpn): connect to the fastest server, a
// country, a city or a server, and disconnect. Sign-in, status and Proton's
// settings live in the Proton VPN window.

/// Countries Proton VPN serves, as (ISO code, English name).
pub const VPN_COUNTRIES: &[(&str, &str)] = &[
    ("AL", "Albania"), ("DZ", "Algeria"), ("AO", "Angola"), ("AR", "Argentina"),
    ("AM", "Armenia"), ("AU", "Australia"), ("AT", "Austria"), ("AZ", "Azerbaijan"),
    ("BH", "Bahrain"), ("BD", "Bangladesh"), ("BE", "Belgium"), ("BA", "Bosnia and Herzegovina"),
    ("BR", "Brazil"), ("BG", "Bulgaria"), ("KH", "Cambodia"), ("CA", "Canada"),
    ("CL", "Chile"), ("CO", "Colombia"), ("CR", "Costa Rica"), ("HR", "Croatia"),
    ("CY", "Cyprus"), ("CZ", "Czechia"), ("DK", "Denmark"), ("EC", "Ecuador"),
    ("EG", "Egypt"), ("EE", "Estonia"), ("FI", "Finland"), ("FR", "France"),
    ("GE", "Georgia"), ("DE", "Germany"), ("GH", "Ghana"), ("GR", "Greece"),
    ("HK", "Hong Kong"), ("HU", "Hungary"), ("IS", "Iceland"), ("IN", "India"),
    ("ID", "Indonesia"), ("IE", "Ireland"), ("IL", "Israel"), ("IT", "Italy"),
    ("JP", "Japan"), ("JO", "Jordan"), ("KZ", "Kazakhstan"), ("KE", "Kenya"),
    ("KR", "South Korea"), ("KW", "Kuwait"), ("LV", "Latvia"), ("LB", "Lebanon"),
    ("LT", "Lithuania"), ("LU", "Luxembourg"), ("MY", "Malaysia"), ("MT", "Malta"),
    ("MX", "Mexico"), ("MD", "Moldova"), ("MA", "Morocco"), ("NL", "Netherlands"),
    ("NZ", "New Zealand"), ("NG", "Nigeria"), ("MK", "North Macedonia"), ("NO", "Norway"),
    ("PK", "Pakistan"), ("PE", "Peru"), ("PH", "Philippines"), ("PL", "Poland"),
    ("PT", "Portugal"), ("PR", "Puerto Rico"), ("RO", "Romania"), ("RS", "Serbia"),
    ("SA", "Saudi Arabia"), ("SG", "Singapore"), ("SK", "Slovakia"), ("SI", "Slovenia"),
    ("ZA", "South Africa"), ("ES", "Spain"), ("LK", "Sri Lanka"), ("SE", "Sweden"),
    ("CH", "Switzerland"), ("TW", "Taiwan"), ("TH", "Thailand"), ("TR", "Türkiye"),
    ("UA", "Ukraine"), ("AE", "United Arab Emirates"), ("GB", "United Kingdom"),
    ("US", "United States"), ("UY", "Uruguay"), ("UZ", "Uzbekistan"), ("VE", "Venezuela"),
    ("VN", "Vietnam"),
];

/// What a free-text connect target names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VpnTarget {
    Fastest,
    Country(String),
    City(String),
    Server(String),
}

impl VpnTarget {
    /// A compact form for search actions: `country:CH`, `city:Zurich`,
    /// `server:CH#242` or `fastest`.
    pub fn encode(&self) -> String {
        match self {
            VpnTarget::Fastest => "fastest".into(),
            VpnTarget::Country(code) => format!("country:{code}"),
            VpnTarget::City(city) => format!("city:{city}"),
            VpnTarget::Server(name) => format!("server:{name}"),
        }
    }

    pub fn decode(text: &str) -> VpnTarget {
        match text.split_once(':') {
            Some(("country", code)) => VpnTarget::Country(code.to_owned()),
            Some(("city", city)) => VpnTarget::City(city.to_owned()),
            Some(("server", name)) => VpnTarget::Server(name.to_owned()),
            _ => VpnTarget::Fastest,
        }
    }

    pub fn label(&self) -> String {
        match self {
            VpnTarget::Fastest => gettext("the fastest server"),
            VpnTarget::Country(code) => country_name(code)
                .map(|name| format!("{name} ({code})"))
                .unwrap_or_else(|| code.clone()),
            VpnTarget::City(city) => city.clone(),
            VpnTarget::Server(name) => name.clone(),
        }
    }
}

pub fn country_name(code: &str) -> Option<&'static str> {
    VPN_COUNTRIES.iter().find(|(c, _)| c.eq_ignore_ascii_case(code)).map(|(_, name)| *name)
}

/// Reject text that could be read as a CLI option or carries control
/// characters. Arguments never pass through a shell either way.
fn clean_target(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()
        && text.len() <= 64
        && !text.starts_with('-')
        && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

/// Server names look like `CH#242` or `US-NY#10`.
fn is_server_name(text: &str) -> bool {
    let Some((prefix, number)) = text.split_once('#') else { return false };
    !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && prefix.split('-').all(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Names people commonly type for a country, mapped to its code.
const COUNTRY_ALIASES: &[(&str, &str)] = &[
    ("uk", "GB"), ("england", "GB"), ("britain", "GB"), ("great britain", "GB"), ("scotland", "GB"),
    ("usa", "US"), ("america", "US"), ("united states of america", "US"),
    ("holland", "NL"), ("the netherlands", "NL"), ("korea", "KR"), ("uae", "AE"), ("emirates", "AE"),
    ("turkey", "TR"), ("czech republic", "CZ"), ("swiss", "CH"), ("deutschland", "DE"),
    ("espana", "ES"), ("españa", "ES"), ("brasil", "BR"), ("bosnia", "BA"), ("macedonia", "MK"),
];

/// The country's flag as an emoji (two regional indicator symbols).
pub fn country_flag(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphabetic)
        .filter_map(|c| char::from_u32(0x1F1E6 + (c.to_ascii_uppercase() as u32 - 'A' as u32)))
        .collect()
}

/// How well `needle` (lower case) names a country: lower is better.
fn country_rank(needle: &str, code: &str, name: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let aliases = || COUNTRY_ALIASES.iter().filter(move |(_, c)| *c == code).map(|(alias, _)| *alias);
    if code.eq_ignore_ascii_case(needle) || name == needle || aliases().any(|a| a == needle) {
        Some(0)
    } else if name.starts_with(needle) || aliases().any(|a| a.starts_with(needle)) {
        Some(1)
    } else if name.split([' ', '-']).skip(1).any(|word| word.starts_with(needle)) {
        Some(2)
    } else if needle.chars().count() >= 3 && name.contains(needle) {
        Some(3)
    } else {
        None
    }
}

/// Countries matching what was typed so far, best first: exact code, name
/// or alias, then name prefixes, word prefixes ("kingdom") and substrings.
pub fn matching_countries(text: &str) -> Vec<(&'static str, &'static str)> {
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<_> = VPN_COUNTRIES
        .iter()
        .filter_map(|&(code, name)| country_rank(&needle, code, name).map(|rank| (rank, code, name)))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(b.2)));
    found.into_iter().map(|(_, code, name)| (code, name)).collect()
}

/// Resolve what the user typed into one connect target: a server name, a
/// country (code or full name), otherwise a city.
pub fn parse_vpn_target(text: &str) -> Option<VpnTarget> {
    let text = clean_target(text)?;
    if is_server_name(&text) {
        return Some(VpnTarget::Server(text.to_uppercase()));
    }
    let lower = text.to_lowercase();
    if let Some((code, _)) = matching_countries(&lower)
        .into_iter()
        .find(|(code, name)| country_rank(&lower, code, name) == Some(0))
    {
        return Some(VpnTarget::Country(code.to_owned()));
    }
    Some(VpnTarget::City(text))
}

/// The configured preferred country, used by `vpn on` and quick connect.
pub fn preferred_vpn_target(config: &Config) -> VpnTarget {
    match parse_vpn_target(&config.proton_vpn_country) {
        Some(target @ VpnTarget::Country(_)) => target,
        _ => VpnTarget::Fastest,
    }
}

fn vpn_row(title: String, subtitle: String, icon: &str, action: Action, score: i32) -> SearchResult {
    SearchResult {
        kind: ResultKind::System,
        title,
        subtitle: Some(subtitle),
        icon: Some(icon.into()),
        action,
        score,
    }
}

fn vpn_action(op: &str, target: &VpnTarget) -> Action {
    Action::ProtonVpn { op: op.into(), target: target.encode() }
}

/// What Proton's server list says about a country, when it has been fetched.
fn country_note(code: &str, name: &str, countries: &[crate::proton_vpn::Country]) -> String {
    match countries.iter().find(|c| c.code.eq_ignore_ascii_case(code)) {
        Some(country) if !country.available => {
            gettext("{country} needs a paid Proton VPN plan").replace("{country}", name)
        }
        Some(country) => gettext("Fastest server in {country} · {count} servers")
            .replace("{country}", name)
            .replace("{count}", &country.servers.to_string()),
        None => gettext("Connect to the fastest server in {country}").replace("{country}", name),
    }
}

fn connect_row(target: &VpnTarget, countries: &[crate::proton_vpn::Country], score: i32) -> SearchResult {
    let (title, subtitle, icon) = match target {
        VpnTarget::Country(code) => {
            let name = countries
                .iter()
                .find(|c| c.code.eq_ignore_ascii_case(code))
                .map(|c| c.name.clone())
                .or_else(|| country_name(code).map(str::to_owned))
                .unwrap_or_else(|| code.clone());
            (
                format!("{} {name}", country_flag(code)),
                country_note(code, &name, countries),
                "network-vpn-symbolic",
            )
        }
        VpnTarget::City(city) => (
            format!("📍 {city}"),
            gettext("Connect to the fastest server in {city}").replace("{city}", city),
            "find-location-symbolic",
        ),
        VpnTarget::Server(name) => (
            gettext("Connect to {target}").replace("{target}", name),
            gettext("A specific Proton VPN server"),
            "network-vpn-symbolic",
        ),
        VpnTarget::Fastest => (
            gettext("Connect to the fastest server"),
            gettext("Proton VPN picks the best server for you"),
            "network-vpn-symbolic",
        ),
    };
    vpn_row(title, subtitle, icon, vpn_action("connect", target), score)
}

fn disconnect_row(score: i32) -> SearchResult {
    vpn_row(
        gettext("Disconnect Proton VPN"),
        gettext("End the current VPN connection"),
        "network-vpn-disabled-symbolic",
        vpn_action("disconnect", &VpnTarget::Fastest),
        score,
    )
}

fn controls_row(title: String, score: i32) -> SearchResult {
    vpn_row(
        title,
        gettext("Sign-in, status, preferred country and Proton VPN settings"),
        "emblem-system-symbolic",
        Action::OpenProtonVpn,
        score,
    )
}

/// Countries ranked for what was typed: Proton's own list when the client
/// has fetched it (only countries it serves), otherwise the built-in table.
fn ranked_countries(text: &str, countries: &[crate::proton_vpn::Country]) -> Vec<String> {
    if countries.is_empty() {
        return matching_countries(text).into_iter().map(|(code, _)| code.to_owned()).collect();
    }
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<_> = countries
        .iter()
        .filter_map(|c| country_rank(&needle, &c.code, &c.name).map(|rank| (rank, !c.available, c)))
        .collect();
    found.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| a.2.name.cmp(&b.2.name)));
    found.into_iter().map(|(_, _, c)| c.code.clone()).collect()
}

/// Proton cities starting with what was typed, as (city, country code).
fn matching_cities(text: &str, countries: &[crate::proton_vpn::Country]) -> Vec<(String, String)> {
    let needle = text.trim().to_lowercase();
    if needle.chars().count() < 2 {
        return Vec::new();
    }
    let mut found: Vec<_> = countries
        .iter()
        .flat_map(|c| c.cities.iter().map(move |city| (city.clone(), c.code.clone())))
        .filter(|(city, _)| city.to_lowercase().starts_with(&needle))
        .collect();
    found.sort();
    found
}

const ON_WORDS: [&str; 6] = ["on", "connect", "up", "start", "fastest", "quick"];
const OFF_WORDS: [&str; 5] = ["off", "disconnect", "down", "stop", "disable"];
const CONTROL_WORDS: [&str; 9] =
    ["status", "settings", "signin", "signout", "sign", "login", "logout", "account", "options"];

/// Ghost text for Proton trigger rows: the word or place a row stands for,
/// continuing what was typed — "ger" → "Germany", "connect ge" → "connect
/// Germany", "zur" → "Zurich", "of" → "off", "st" → "status",
/// "tom" → "tomorrow". `None` means the row offers no completion.
pub fn ghost_for(res: &SearchResult, typed: &str) -> Option<String> {
    let lower = typed.to_lowercase();
    // A leading connector word stays as typed: "connect ge" → "connect Germany".
    let prefix_len = match lower.split_once(' ') {
        Some((first, _)) if matches!(first, "connect" | "to" | "in") => first.len() + 1,
        _ => 0,
    };
    let (prefix, rest) = typed.split_at(prefix_len);
    if rest.is_empty() || rest.starts_with(' ') {
        return None;
    }
    let complete = |word: &str| {
        let lower_word = word.to_lowercase();
        (lower_word.starts_with(&rest.to_lowercase()) && lower_word != rest.to_lowercase())
            .then(|| format!("{prefix}{word}"))
    };
    let first_word = |words: &[&str]| words.iter().find_map(|word| complete(word));
    // Country and city rows are titled "<flag> Name" / "📍 Name".
    let place = || res.title.split_once(' ').map(|(_, name)| name).unwrap_or(&res.title);
    match &res.action {
        Action::ProtonVpn { op, target } if op == "disconnect" => {
            if prefix.is_empty() { first_word(&OFF_WORDS) } else { None }
        }
        Action::ProtonVpn { target, .. } => match VpnTarget::decode(target) {
            VpnTarget::Country(_) | VpnTarget::City(_) => complete(place()),
            VpnTarget::Server(name) => complete(&name),
            VpnTarget::Fastest if prefix.is_empty() => first_word(&ON_WORDS),
            VpnTarget::Fastest => None,
        },
        Action::OpenProtonVpn if prefix.is_empty() => first_word(&CONTROL_WORDS),
        Action::OpenProtonWeb(url) if url.starts_with("https://calendar.proton.me/") && prefix.is_empty() => {
            first_word(&CALENDAR_WORDS)
        }
        Action::OpenProtonWeb(url) if url.starts_with("https://drive.proton.me/") && prefix.is_empty() => {
            first_word(&DRIVE_PLACES.map(|(word, ..)| word))
        }
        _ => None,
    }
}

/// True for rows whose ghost text [`ghost_for`] decides.
pub fn owns_ghost(res: &SearchResult) -> bool {
    match &res.action {
        Action::ProtonVpn { .. } | Action::OpenProtonVpn => true,
        Action::OpenProtonWeb(url) => {
            url.starts_with("https://calendar.proton.me/") || url.starts_with("https://drive.proton.me/")
        }
        _ => false,
    }
}

pub fn vpn_search(query: &str, config: &Config) -> Vec<SearchResult> {
    if !crate::proton_vpn::available() {
        return vec![controls_row(gettext("Proton VPN isn't built into this Spotty"), 100_003)];
    }
    let logged_in = crate::proton_vpn::cached_status().map(|status| status.logged_in);
    vpn_search_with(query, config, &crate::proton_vpn::cached_countries(), logged_in)
}

fn vpn_search_with(
    query: &str,
    config: &Config,
    countries: &[crate::proton_vpn::Country],
    logged_in: Option<bool>,
) -> Vec<SearchResult> {
    let query = query.trim();
    let lower = query.to_lowercase();
    let preferred = preferred_vpn_target(config);
    let mut rows = Vec::new();
    // Signed out, every action opens sign-in first; say so up front.
    if logged_in == Some(false) {
        rows.push(controls_row(gettext("Sign in to Proton VPN"), 100_010));
    }

    if lower.is_empty() {
        rows.push(connect_row(&preferred, countries, 100_003));
        if preferred != VpnTarget::Fastest {
            rows.push(connect_row(&VpnTarget::Fastest, countries, 100_002));
        }
        rows.push(disconnect_row(100_001));
        rows.push(controls_row(gettext("Proton VPN status and settings"), 100_000));
        return rows;
    }

    let (first, rest) = match lower.split_once(char::is_whitespace) {
        Some((first, _)) => (first.to_owned(), query[first.len()..].trim()),
        None => (lower.clone(), ""),
    };
    if ON_WORDS.contains(&first.as_str()) && rest.is_empty() {
        rows.push(connect_row(&preferred, countries, 100_003));
        if preferred != VpnTarget::Fastest {
            rows.push(connect_row(&VpnTarget::Fastest, countries, 100_002));
        }
        return rows;
    }
    if OFF_WORDS.contains(&first.as_str()) {
        rows.push(disconnect_row(100_003));
        return rows;
    }
    if CONTROL_WORDS.contains(&first.as_str()) && rest.is_empty() {
        rows.push(controls_row(gettext("Open Proton VPN status and settings"), 100_003));
        return rows;
    }
    // "st", "sett"…: still suggest countries, with the controls as one more row.
    let control_prefix = rest.is_empty()
        && first.chars().count() >= 2
        && CONTROL_WORDS.iter().any(|word| word.starts_with(first.as_str()));

    // "connect germany", "to zurich" or just "germany".
    let target_text = if matches!(first.as_str(), "connect" | "to" | "in") { rest } else { query };
    let Some(target) = parse_vpn_target(target_text) else {
        rows.push(controls_row(gettext("Open Proton VPN status and settings"), 100_000));
        return rows;
    };
    let mut score = 100_003;
    if let VpnTarget::Server(_) = target {
        rows.push(connect_row(&target, countries, score));
        return rows;
    }
    // Country suggestions as the user types: "g" → Georgia, Germany, Ghana…
    let suggestions = ranked_countries(target_text, countries);
    for code in suggestions.iter().take(8) {
        rows.push(connect_row(&VpnTarget::Country(code.clone()), countries, score));
        score -= 1;
    }
    // Cities from Proton's server list ("zur" → Zurich); without the list, a
    // free-text city once the text isn't a country and is long enough.
    let cities = matching_cities(target_text, countries);
    if !cities.is_empty() {
        for (city, _) in cities.iter().take(4) {
            rows.push(connect_row(&VpnTarget::City(city.clone()), countries, score));
            score -= 1;
        }
    } else if countries.is_empty()
        && matches!(target, VpnTarget::City(_))
        && (suggestions.is_empty() || target_text.trim().chars().count() >= 3)
    {
        let city = VpnTarget::City(clean_target(target_text).unwrap_or_default());
        rows.push(connect_row(&city, countries, score));
        score -= 1;
    }
    // Part of a command word ("o", "of", "disc"): offer it too, so the ghost
    // text can complete it.
    let word_prefix = |words: &[&str]| rest.is_empty() && words.iter().any(|w| w.starts_with(first.as_str()));
    if control_prefix {
        score -= 1;
        rows.push(controls_row(gettext("Open Proton VPN status and settings"), score));
    }
    if word_prefix(&OFF_WORDS) {
        score -= 1;
        rows.push(disconnect_row(score));
    }
    if word_prefix(&ON_WORDS) {
        score -= 1;
        rows.push(connect_row(&preferred, countries, score));
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

    /// Thursday 8 October 2026.
    fn thursday() -> glib::DateTime {
        glib::DateTime::from_local(2026, 10, 8, 12, 0, 0.0).unwrap()
    }

    fn when(query: &str) -> Option<((i32, u32, u32), Option<&'static str>)> {
        parse_when(query, &thursday()).map(|w| (w.date, w.view))
    }

    #[test]
    fn dates_parse_iso_and_reject_invalid_days() {
        assert_eq!(when("2026-02-28"), Some(((2026, 2, 28), None)));
        assert_eq!(when("2026-02-30"), None);
        assert_eq!(when("meeting"), None);
        assert_eq!(when(""), None);
        assert_eq!(when("today"), Some(((2026, 10, 8), None)));
    }

    #[test]
    fn calendar_understands_everyday_phrases() {
        assert_eq!(when("tomorrow"), Some(((2026, 10, 9), None)));
        assert_eq!(when("yesterday"), Some(((2026, 10, 7), None)));
        // The coming weekday; a bare weekday never means today.
        assert_eq!(when("friday"), Some(((2026, 10, 9), None)));
        assert_eq!(when("mon"), Some(((2026, 10, 12), None)));
        assert_eq!(when("thursday"), Some(((2026, 10, 15), None)));
        assert_eq!(when("this thursday"), Some(((2026, 10, 8), None)));
        assert_eq!(when("next sunday"), Some(((2026, 10, 11), None)));
        assert_eq!(when("week"), Some(((2026, 10, 8), Some("week"))));
        assert_eq!(when("next week"), Some(((2026, 10, 15), Some("week"))));
        assert_eq!(when("last week"), Some(((2026, 10, 1), Some("week"))));
        assert_eq!(when("month"), Some(((2026, 10, 8), Some("month"))));
        assert_eq!(when("next month"), Some(((2026, 11, 1), Some("month"))));
        assert_eq!(when("last month"), Some(((2026, 9, 1), Some("month"))));
        assert_eq!(when("+3"), Some(((2026, 10, 11), None)));
        assert_eq!(when("3d"), Some(((2026, 10, 11), None)));
        assert_eq!(when("-2d"), Some(((2026, 10, 6), None)));
        assert_eq!(when("in 2 weeks"), Some(((2026, 10, 22), None)));
        assert_eq!(when("2 days"), Some(((2026, 10, 10), None)));
        assert_eq!(when("24 oct"), Some(((2026, 10, 24), None)));
        assert_eq!(when("oct 24"), Some(((2026, 10, 24), None)));
        assert_eq!(when("1st jan 2027"), Some(((2027, 1, 1), None)));
        assert_eq!(when("31 apr"), None);
        assert_eq!(when("99999d"), None);
    }

    #[test]
    fn calendar_rows_use_the_phrase_view_and_default_the_rest() {
        let mut config = Config::default();
        config.proton_calendar_view = "day".into();
        let rows = calendar_search("next month", &config);
        assert!(matches!(&rows[0].action, Action::OpenProtonWeb(url) if url.contains("/month/")));
        assert!(matches!(&rows.last().unwrap().action, Action::OpenProtonWeb(url) if url.contains("/day/")));
        // Nothing typed also offers the other views.
        assert_eq!(calendar_search("", &config).len(), 3);
    }

    #[test]
    fn drive_places_have_stable_web_paths() {
        assert_eq!(drive_place_url(0, "trash"), "https://drive.proton.me/u/0/trash");
        assert!(DRIVE_PLACES.iter().all(|(word, _, path)| !word.is_empty() && !path.contains("..")));
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
        assert!(matches!(rows[0].action, Action::OpenProtonWeb(_)));
        let files: Vec<_> = rows.iter().filter(|r| r.kind == ResultKind::File).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "receipt.pdf");
    }

    #[test]
    fn vpn_targets_resolve_servers_countries_and_cities() {
        assert_eq!(parse_vpn_target("ch#242"), Some(VpnTarget::Server("CH#242".into())));
        assert_eq!(parse_vpn_target("US-NY#10"), Some(VpnTarget::Server("US-NY#10".into())));
        assert_eq!(parse_vpn_target("germany"), Some(VpnTarget::Country("DE".into())));
        assert_eq!(parse_vpn_target("de"), Some(VpnTarget::Country("DE".into())));
        assert_eq!(parse_vpn_target("Zurich"), Some(VpnTarget::City("Zurich".into())));
        assert_eq!(parse_vpn_target("--help"), None);
        assert_eq!(parse_vpn_target("a\nb"), None);
        for target in [
            VpnTarget::Fastest,
            VpnTarget::Country("DE".into()),
            VpnTarget::City("New York".into()),
            VpnTarget::Server("US-NY#10".into()),
        ] {
            assert_eq!(VpnTarget::decode(&target.encode()), target);
        }
    }

    #[test]
    fn country_suggestions_follow_what_is_typed() {
        let codes = |text: &str| matching_countries(text).into_iter().map(|(c, _)| c).collect::<Vec<_>>();
        assert_eq!(codes("ger")[0], "DE");
        assert_eq!(codes("de")[0], "DE");
        assert!(codes("de").contains(&"DK"));
        assert_eq!(codes("uk")[0], "GB");
        assert_eq!(codes("kingdom"), vec!["GB"]);
        assert_eq!(codes("usa")[0], "US");
        let g = codes("g");
        for code in ["GE", "DE", "GH", "GR"] {
            assert!(g.contains(&code), "{code} in {g:?}");
        }
        assert!(codes("xyzq").is_empty());
        assert_eq!(parse_vpn_target("holland"), Some(VpnTarget::Country("NL".into())));
        assert_eq!(country_flag("de"), "🇩🇪");
    }

    #[test]
    fn typing_a_partial_country_suggests_countries_not_a_city() {
        let config = Config::default();
        let rows = search("swi", &config);
        assert!(rows[0].title.contains("Switzerland"), "{:?}", rows[0].title);
        let rows = search("s", &config);
        assert!(rows.iter().any(|row| row.title.contains("Saudi Arabia")));
        assert!(search("swe", &config)[0].title.contains("Sweden"));
        assert!(rows.iter().any(|row| row.title.contains("Spain")));
        let rows = search("st", &config);
        assert!(rows.iter().any(|row| matches!(row.action, Action::OpenProtonVpn)));
        let rows = search("g", &config);
        assert!(rows.len() >= 4);
        assert!(rows.iter().all(|row| !row.title.contains("city")));
    }

    fn search(query: &str, config: &Config) -> Vec<SearchResult> {
        vpn_search_with(query, config, &[], None)
    }

    fn target_of(row: &SearchResult) -> (String, VpnTarget) {
        match &row.action {
            Action::ProtonVpn { op, target } => (op.clone(), VpnTarget::decode(target)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn vpn_trigger_words_map_to_client_actions() {
        let mut config = Config::default();
        let connect = |target: VpnTarget| ("connect".to_owned(), target);
        assert_eq!(target_of(&search("on", &config)[0]), connect(VpnTarget::Fastest));
        assert_eq!(target_of(&search("off", &config)[0]).0, "disconnect");
        assert!(matches!(search("status", &config)[0].action, Action::OpenProtonVpn));
        assert_eq!(target_of(&search("connect ch", &config)[0]), connect(VpnTarget::Country("CH".into())));
        assert_eq!(target_of(&search("CH#242", &config)[0]), connect(VpnTarget::Server("CH#242".into())));
        let city = search("zurich", &config);
        assert_eq!(target_of(city.last().unwrap()), connect(VpnTarget::City("zurich".into())));

        config.proton_vpn_country = "NL".into();
        assert_eq!(target_of(&search("on", &config)[0]), connect(VpnTarget::Country("NL".into())));
        assert_eq!(target_of(&search("", &config)[1]), connect(VpnTarget::Fastest));
    }

    fn proton_list() -> Vec<crate::proton_vpn::Country> {
        let country = |code: &str, name: &str, cities: &[&str], available: bool| crate::proton_vpn::Country {
            code: code.into(),
            name: name.into(),
            cities: cities.iter().map(|c| (*c).to_owned()).collect(),
            servers: 12,
            available,
        };
        vec![
            country("CH", "Switzerland", &["Zurich", "Geneva"], true),
            country("DE", "Germany", &["Frankfurt", "Berlin"], true),
            country("GH", "Ghana", &[], false),
        ]
    }

    #[test]
    fn protons_own_list_drives_suggestions_and_cities() {
        let config = Config::default();
        let list = proton_list();
        let rows = vpn_search_with("g", &config, &list, Some(true));
        let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        // Only countries Proton serves; ones on the plan first.
        assert_eq!(titles, vec!["🇩🇪 Germany", "🇬🇭 Ghana"]);
        assert!(rows[1].subtitle.as_deref().unwrap().contains("paid"));
        // Cities join in from the second letter.
        let rows = vpn_search_with("ge", &config, &list, Some(true));
        let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["🇩🇪 Germany", "📍 Geneva"]);
        let rows = vpn_search_with("zur", &config, &list, Some(true));
        assert_eq!(target_of(&rows[0]), ("connect".into(), VpnTarget::City("Zurich".into())));
        let rows = vpn_search_with("", &config, &list, Some(false));
        assert!(matches!(rows[0].action, Action::OpenProtonVpn));
        assert!(rows[0].title.contains("Sign in"));
    }

    #[test]
    fn ghost_text_completes_what_rows_stand_for() {
        let config = Config::default();
        let list = proton_list();
        let ghost = |query: &str| {
            vpn_search_with(query, &config, &list, Some(true))
                .iter()
                .find_map(|row| ghost_for(row, query))
        };
        assert_eq!(ghost("ger").as_deref(), Some("Germany"));
        assert_eq!(ghost("Swi").as_deref(), Some("Switzerland"));
        assert_eq!(ghost("connect ge").as_deref(), Some("connect Germany"));
        assert_eq!(ghost("zur").as_deref(), Some("Zurich"));
        assert_eq!(ghost("of").as_deref(), Some("off"));
        assert_eq!(ghost("sta").as_deref(), Some("status"));
        assert_eq!(ghost("ch#2").as_deref(), None);
        assert_eq!(ghost("Germany"), None);
        let calendar = calendar_search("tom", &config);
        assert_eq!(calendar.iter().find_map(|row| ghost_for(row, "tom")).as_deref(), Some("tomorrow"));
        assert!(calendar.iter().all(owns_ghost));
    }
}
