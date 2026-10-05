use crate::clipboard::ClipboardHistory;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use crate::config::Config;
use crate::index::Indexer;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use crate::i18n::gettext;

pub mod apps;
pub mod appimage;
pub mod bluetooth;
pub mod browse;
pub mod browser_engine;
pub mod browser_launch;
pub mod calculator;
pub mod clipboard;
pub mod cmd;
pub mod convert;
pub mod currency;
pub mod dictionary;
pub mod emoji;
pub mod files;
pub mod jobs;
pub mod proton_bridge;
pub mod run;
pub mod settings_panels;
pub mod system;
pub mod translate;
pub mod typo;
pub mod uninstall;
pub mod web;

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResultKind {
    App,
    File,
    Folder,
    Web,
    Clipboard,
    Calculator,
    System,
    /// An emoji result: `icon` carries the literal emoji glyph, rendered as
    /// large text instead of an icon.
    Emoji,
    /// A translation row: Enter copies the text (never auto-pastes).
    Translate,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub kind: ResultKind,
    pub title: String,
    pub subtitle: Option<String>,
    pub icon: Option<String>,
    pub action: Action,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Action {
    LaunchDesktopFile(std::path::PathBuf),
    /// Run an installed AppImage (a portable file, not a desktop entry —
    /// it may have no integration entry at all).
    LaunchAppImage(std::path::PathBuf),
    /// Trash an installed AppImage (confirmed first; see the search window).
    RemoveAppImage(std::path::PathBuf),
    OpenPath(std::path::PathBuf),
    OpenInFileManager(std::path::PathBuf),
    BrowseInto(std::path::PathBuf),
    OpenUrl(String),
    /// Open a URL in a *private* window of the default browser (incognito /
    /// private window). Used by the "search privately" shortcut, so a web
    /// search never leaves traces in the normal session.
    OpenUrlPrivate(String),
    CopyToClipboard(String),
    CopyImageToClipboard(std::path::PathBuf),
    /// Re-copy a file/folder onto the clipboard (from the clipboard manager).
    CopyFileToClipboard(std::path::PathBuf),
    InsertCalculatorResult(String),
    RunCommand(String),
    /// Run a command with a confirmation dialog first (for destructive actions
    /// like shutdown/reboot/logout/suspend).
    ConfirmRunCommand(String),
    /// Run a command inside a terminal emulator window (for interactive use).
    RunInTerminal(String),
    /// Run a command and show streaming progress inline in the search window.
    /// `title` is the header label; `args[0]` is the program, rest are argv.
    RunWithProgress {
        title: String,
        args: Vec<String>,
    },
    /// Enter a trigger mode (carries the trigger word, e.g. "files", "pdf").
    EnterMode(String),
    /// Remove an installed trigger by id.
    UninstallTrigger(String),
    /// Start a long-running package operation in the background. It keeps
    /// running even if the search window is hidden. `args[0]` is the program.
    StartOperation {
        title: String,
        source: String,
        /// Icon name / app-id shown on the operation's progress row.
        icon: String,
        args: Vec<String>,
    },
    /// Bluetooth control (bt trigger). `op` ∈ connect/disconnect/pair/scan/
    /// power_on/power_off; `mac` is empty for the scan/power/check actions.
    Bluetooth {
        op: String,
        mac: String,
    },
    /// Pick a target language for the translate trigger; `strip` is the
    /// trailing token of the query that named it ("hello po" → "po") and is
    /// removed from the entry when set.
    SetTranslateTarget {
        code: String,
        strip: String,
    },
    /// Expand a language list next to the typed text: the target picker
    /// (`source == false`) or the source picker (manual override of
    /// auto-detection).
    TranslateExpand {
        source: bool,
    },
    /// Pick the source language for this session; empty = auto-detect.
    SetTranslateSource {
        code: String,
    },
    /// Translate right now, skipping the 3 s auto-translate delay (also the
    /// retry for a failed attempt).
    TranslateNow {
        text: String,
        target: String,
    },
    /// Switch the background update feature on/off (from search or the
    /// Settings row).
    ToggleUpdates,
    /// "Remind tomorrow": snooze the update notice for 24 hours.
    SnoozeUpdates,
    /// "Dismiss update notice": hide this update set until a new one
    /// appears (carries its signature).
    DismissUpdates(String),
    /// Re-run the update check right now. Enter on "No updates available"
    /// (or on the "Checking for updates" row) checks again instead of
    /// doing nothing.
    CheckUpdates,
    /// A row that only exists to be displayed (e.g. "No updates
    /// available") — Enter does nothing.
    Noop,
}

/// Fuzzy match for typed keywords: nucleo scores `text` against `query`
/// above a length-scaled threshold — that catches dropped letters ("sytem"
/// → system, "fltak" → flatpak). nucleo only accepts subsequences, so a
/// second check covers what subsequence scoring rejects: swapped,
/// substituted or doubled letters ("udpat", "sistem", "aall") within two
/// edits of a short candidate. Unrelated words never hit: "upload" is four
/// edits away from "update". Queries shorter than two characters stay with
/// the explicit prefix logic.
pub fn fuzzy_match(query: &str, text: &str) -> bool {
    let len = query.chars().count();
    if len < 2 {
        return false;
    }
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    if pattern
        .score(Utf32String::from(text).slice(..), &mut matcher)
        .map(|s| s >= (len as u32) * 20)
        .unwrap_or(false)
    {
        return true;
    }
    len >= 3
        && edit_distance(&query.to_lowercase(), &text.to_lowercase()) <= 2
}

/// Classic Levenshtein distance over chars — the words compared here are
/// short keywords, candidates and verbs.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Universally pinned results (any kind) whose title/subtitle matches `query_lower`.
/// Matches always score above normal results so they sort to the top.
pub fn pinned_matches(query_lower: &str, pinned: &[SearchResult]) -> Vec<SearchResult> {
    if pinned.is_empty() || query_lower.is_empty() {
        return vec![];
    }
    pinned
        .iter()
        .filter(|p| {
            let title = p.title.to_lowercase();
            let sub = p.subtitle.as_deref().map(str::to_lowercase);
            // Substring first; fuzzy below it so a typo'd pinned query
            // ("setings") still surfaces its pinned row.
            title.contains(query_lower)
                || fuzzy_match(query_lower, &title)
                || sub
                    .as_deref()
                    .map(|s| s.contains(query_lower) || fuzzy_match(query_lower, s))
                    .unwrap_or(false)
        })
        .cloned()
        .map(|mut p| {
            p.score = 1_000_000;
            p
        })
        .collect()
}

/// Prepend matching pinned results to `results`, removing any duplicates
/// (same action) that already appear naturally.
pub fn merge_pinned(
    mut results: Vec<SearchResult>,
    query_lower: &str,
    pinned: &[SearchResult],
) -> Vec<SearchResult> {
    let pins = pinned_matches(query_lower, pinned);
    if pins.is_empty() {
        return results;
    }
    results.retain(|r| !pins.iter().any(|p| p.action == r.action));
    let mut out = pins;
    out.extend(results);
    out
}

fn inline_file_mode(query: &str, config: &Config) -> Option<(String, String)> {
    let mut parts = query.splitn(2, char::is_whitespace);
    let word = parts.next()?.trim();
    let rest = parts.next()?.trim();
    if word.is_empty() {
        return None;
    }
    let kw = inline_keyword(word, config)?;
    // All-files keywords inline-route ("find foo"); the dictionary and the
    // translate trigger route the same way so `dict word` / `translate text`
    // work in one go, without a separate mode-entry step first.
    (kw.all_files || matches!(kw.id.as_str(), "dictionary" | "translate"))
        .then(|| (kw.word.clone(), rest.to_string()))
}

/// The inline trigger behind a typed word: exact first, then a bounded
/// fuzzy pass (3–12 chars) so a typo'd inline trigger still routes —
/// "fnd report" searches files — while short or long words, which is where
/// unrelated queries live, are never guessed.
fn inline_keyword(word: &str, config: &Config) -> Option<crate::config::CommandKeyword> {
    if let Some(kw) = config.keyword_for_word(word) {
        return Some(kw);
    }
    let wl = word.to_lowercase();
    if !(3..=12).contains(&wl.chars().count()) {
        return None;
    }
    let inline_capable = |kw: &crate::config::CommandKeyword| {
        kw.all_files || matches!(kw.id.as_str(), "dictionary" | "translate")
    };
    let owned = crate::triggers::keywords();
    config
        .command_keywords
        .iter()
        .chain(owned.iter())
        .filter(|kw| kw.enabled && inline_capable(kw))
        .find(|kw| fuzzy_match(&wl, &kw.word))
        .cloned()
}

/// Trigger-word suggestions for the universal search (Raycast-style):
/// prefix matches at any length — so the fully-typed word `bluetooth` or
/// `translate` still matches — plus a fuzzy tier for typos ("fils" → Find)
/// that only runs for short queries, so long searches never fuzzy-match a
/// trigger. Pressing Enter on a suggestion enters that mode. Shared with the
/// worker (`jobs::compute`), which is the path that runs in production.
pub fn trigger_suggestions(query: &str, config: &Config) -> Vec<SearchResult> {
    let ql = query.trim().to_lowercase();
    if ql.is_empty() || !ql.chars().all(|c| c.is_alphabetic()) {
        return Vec::new();
    }
    let trigger_kws = crate::triggers::keywords();
    let kw_iter = config
        .command_keywords
        .iter()
        .chain(trigger_kws.iter());
    let mut r = Vec::new();
    for kw in kw_iter {
        // Nothing to type for a word-less keyword, and nothing to enter for
        // one that is paused, uninstalled or a switched-off result type.
        if kw.word.is_empty() || !config.keyword_usable(kw) {
            continue;
        }
        let dn = kw.display_name().to_lowercase();
        let word_match = kw.word.starts_with(&ql);
        let name_match = dn.starts_with(&ql);
        // Fuzzy last: a typo'd trigger word ("fils" → find) still suggests
        // its mode, ranked below every real prefix hit. Bounded to short
        // queries so a long search never fuzzy-hits a trigger by accident.
        let fuzzy = !word_match
            && !name_match
            && ql.chars().count() <= 12
            && (fuzzy_match(&ql, &kw.word) || fuzzy_match(&ql, &dn));
        if !(word_match || name_match || fuzzy) {
            continue;
        }
        // Exact match scores highest; display-name prefix a bit lower than
        // word prefix; fuzzy below both.
        let score = if kw.word == ql || dn == ql {
            100_000
        } else if word_match {
            50_000
        } else if name_match {
            45_000
        } else {
            40_000
        };
        r.push(SearchResult {
            kind: ResultKind::System,
            title: capitalize(&kw.word),
            subtitle: Some(kw.description.clone()),
            icon: Some(if kw.icon.is_empty() {
                "folder-symbolic".into()
            } else {
                kw.icon.clone()
            }),
            action: Action::EnterMode(kw.word.clone()),
            score,
        });
    }
    r
}

pub fn search(
    query: &str,
    config: &Config,
    snap_lock: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<SearchResult> {
    let query = query.trim();
    if query.is_empty() {
        return vec![];
    }
    if let Some((mode_word, rest)) = inline_file_mode(query, config) {
        return search_mode(&mode_word, &rest, config, snap_lock);
    }
    // Path browsing mode
    if config.enable_root_browsing
        && (query.starts_with('/') || query.starts_with("~/") || query == "~")
    {
        return browse::browse(query);
    }
    universal_results(query, config, snap_lock)
}

/// Fewest typed characters before triggers join the regular search — a single
/// letter would only bring noise (every emoji, every file).
const REGULAR_MIN_CHARS: usize = 2;
/// Rows one trigger may add to the regular search.
const REGULAR_ROWS: usize = 3;

/// Where a trigger's rows sit in the regular search: just above the web
/// fallback (score 100), so they show without outranking real matches. A
/// trigger that *runs commands* goes below it instead — with nothing else
/// matching, the top row is what Enter does, and a command runner must never
/// be that by accident.
fn regular_base_score(runs_commands: bool) -> i32 {
    if runs_commands {
        40
    } else {
        150
    }
}

/// Take a trigger's rows for the regular search: its real results only (not
/// its "type something" hints), few of them, ranked under the main results.
fn regular_rows(rows: Vec<SearchResult>, runs_commands: bool) -> Vec<SearchResult> {
    let base = regular_base_score(runs_commands);
    rows.into_iter()
        .filter(|r| !matches!(r.action, Action::EnterMode(_)))
        .take(REGULAR_ROWS)
        .enumerate()
        .map(|(i, mut r)| {
            r.score = base + (REGULAR_ROWS - i) as i32;
            r
        })
        .collect()
}

/// What the triggers that opted into the regular search add for `query`.
///
/// Safe on the search worker. Clipboard and Dictionary are left out: they read
/// state that only the main thread owns, so the window adds them itself (see
/// [`main_thread_trigger_results`]).
pub fn regular_trigger_results(
    query: &str,
    config: &Config,
    snap_lock: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<SearchResult> {
    regular_trigger_sources(query, config, snap_lock)
        .into_iter()
        .flat_map(|(_, rows, _)| rows)
        .collect()
}

/// [`regular_trigger_results`] per trigger: (id, rows, runs commands).
fn regular_trigger_sources(
    query: &str,
    config: &Config,
    snap_lock: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<(String, Vec<SearchResult>, bool)> {
    let q = query.trim();
    if q.chars().count() < REGULAR_MIN_CHARS {
        return Vec::new();
    }
    let owned = crate::triggers::keywords();
    let mut out = Vec::new();
    for kw in config.command_keywords.iter().chain(owned.iter()) {
        if kw.is_result()
            || kw.id == "cmd"
            || kw.word.is_empty()
            || matches!(kw.id.as_str(), "clipboard" | "dictionary")
            || !config.keyword_usable(kw)
            || !config.in_regular_search(&kw.id)
        {
            continue;
        }
        let runs_commands = matches!(kw.id.as_str(), "run" | "proton-bridge")
            || matches!(
                crate::triggers::by_id(&kw.id).map(|m| m.action),
                Some(crate::triggers::TriggerAction::Shell { .. })
            );
        out.push((
            kw.id.clone(),
            regular_rows(search_mode(&kw.word, q, config, snap_lock), runs_commands),
            runs_commands,
        ));
    }
    out
}

/// The same for the triggers that need the main thread: Clipboard (its history
/// lives there) and Dictionary (its lookup throttle does).
pub fn main_thread_trigger_results(
    query: &str,
    config: &Config,
    clipboard: &crate::clipboard::ClipboardHistory,
) -> Vec<SearchResult> {
    let q = query.trim();
    if q.chars().count() < REGULAR_MIN_CHARS {
        return Vec::new();
    }
    let mut out = Vec::new();
    for id in ["clipboard", "dictionary"] {
        let usable = config.keyword_for_id(id).is_some_and(|kw| !kw.word.is_empty());
        if !usable || !config.in_regular_search(id) {
            continue;
        }
        let rows = if id == "clipboard" {
            clipboard::all_or_filtered(
                q,
                clipboard,
                &config.pinned_clipboard,
                &config.pinned_clipboard_images,
                &config.pinned_clipboard_files,
            )
        } else {
            dictionary::results(q)
        };
        let rows = regular_rows(rows, false);
        let pos = config.order_position(id).unwrap_or(ORDER_SLOTS);
        out.extend(rows.into_iter().enumerate().map(|(i, mut r)| {
            r.score = ordered_score(pos, i);
            r
        }));
    }
    out
}

/// Scores that carry the user's order: every source gets a band, highest
/// first, and a row's place inside its band keeps the source's own ranking.
/// Pins (1 000 000) stay above all of them; operations sit right under pins.
const ORDER_TOP: i32 = 900_000;
const ORDER_BAND: i32 = 1_000;
const ORDER_SLOTS: usize = 800;

fn ordered_score(position: usize, index: usize) -> i32 {
    let position = position.min(ORDER_SLOTS) as i32;
    ORDER_TOP - position * ORDER_BAND - (index as i32).min(ORDER_BAND - 1)
}

/// The regular search: every source's rows, ranked by the order the user gave
/// result types and triggers in Settings (highest first), and by each source's
/// own relevance within it. Pins lead, running operations follow them, and a
/// trigger that runs commands always comes last — whatever its place — so
/// Enter can't run one by accident.
pub fn universal_results(
    query: &str,
    config: &Config,
    snap_lock: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<SearchResult> {
    let ql = query.to_lowercase();
    let mut sources: std::collections::HashMap<String, Vec<SearchResult>> =
        std::collections::HashMap::new();
    let mut update_verb_rows = Vec::<(String, Action)>::new();
    let mut put = |id: &str, rows: Vec<SearchResult>| {
        if !rows.is_empty() {
            sources.entry(id.to_string()).or_default().extend(rows);
        }
    };

    // A suggestion to enter a trigger's mode ranks with that trigger.
    for row in trigger_suggestions(query, config) {
        let id = match &row.action {
            Action::EnterMode(w) => config.keyword_for_word(w).map(|k| k.id),
            _ => None,
        };
        put(id.as_deref().unwrap_or("apps"), vec![row]);
    }
    if config.result_enabled("updates") && config.in_regular_search("updates") {
        if let Some(rows) = cmd::update_verb_rows(query, config) {
            update_verb_rows.extend(
                rows.iter()
                    .map(|row| (row.title.clone(), row.action.clone())),
            );
            put("updates", rows);
        }
    }
    // System actions and Settings panels are things to launch, like apps.
    put("apps", system::search(query, config));
    put("apps", settings_panels::search(query));
    if config.result_enabled("calc") && config.in_regular_search("calc") {
        put("calc", calculator::evaluate(query, config).into_iter().collect());
    }
    if config.result_enabled("convert") && config.in_regular_search("convert") {
        put("convert", convert::convert(query, config).into_iter().collect());
    }
    if config.result_enabled("apps") && config.in_regular_search("apps") {
        let snap_guard = snap_lock.read().unwrap();
        put("apps", apps::search(query, &snap_guard.apps));
        // Portable AppImages: launch rows for files that have no desktop
        // entry of their own (integrated ones are already indexed apps).
        if config.app_sources().appimage {
            put("apps", appimage::search(query, 3));
        }
    }
    if config.result_enabled("newapps") && config.in_regular_search("newapps") && query.chars().count() >= 3 {
        put("newapps", cmd::universal_install(query, config.app_sources(), 4));
    }
    if config.result_enabled("web") && config.in_regular_search("web") {
        put("web", vec![web::result(query, config)]);
    }
    let mut last: Vec<SearchResult> = Vec::new();
    for (id, rows, runs_commands) in regular_trigger_sources(query, config, snap_lock) {
        if runs_commands {
            last.extend(rows);
        } else {
            put(&id, rows);
        }
    }

    // Bands in the user's order; anything unlisted after them.
    let mut ranked: Vec<SearchResult> = Vec::new();
    let order = config.ordered_ids();
    let mut place = |rows: &mut Vec<SearchResult>, position: usize, ranked: &mut Vec<SearchResult>| {
        rows.sort_by(|a, b| b.score.cmp(&a.score));
        for (i, mut r) in rows.drain(..).enumerate() {
            r.score = ordered_score(position, i);
            ranked.push(r);
        }
    };
    for (position, id) in order.iter().enumerate() {
        if let Some(mut rows) = sources.remove(id) {
            place(&mut rows, position, &mut ranked);
        }
    }
    let mut rest: Vec<SearchResult> = sources.into_values().flatten().collect();
    place(&mut rest, ORDER_SLOTS, &mut ranked);
    for (i, mut r) in last.into_iter().enumerate() {
        r.score = 1_000 - i as i32;
        ranked.push(r);
    }

    // Above the bands (below pins and operations): a row titled exactly what
    // was typed, then rows the user has picked before for this query, most
    // often picked first. Ties keep the band order.
    let qt = ql.trim();
    for r in ranked.iter_mut() {
        let learned = crate::history::frequency_bonus_for(qt, &r.title);
        if let Some(position) = update_verb_rows
            .iter()
            .position(|(title, action)| title == &r.title && action == &r.action)
        {
            // A query routed through the update verb is an update request;
            // keep its update actions ahead of unrelated app/setting matches.
            r.score = 990_000 - position as i32;
            continue;
        }
        // In the universal search, a direct prefix match on a trigger's
        // configured word or display name should offer that mode first.
        // Fuzzy typo suggestions keep their normal source ordering.
        let trigger_match = match &r.action {
            Action::EnterMode(word) => config.keyword_for_word(word).filter(|kw| {
                kw.word.to_lowercase().starts_with(qt)
                    || kw.display_name().to_lowercase().starts_with(qt)
            }),
            _ => None,
        };
        if r.title.eq_ignore_ascii_case(qt) {
            // A result whose visible name is exactly what the user typed
            // leads every partial trigger suggestion, regardless of source.
            r.score = 980_000 + learned.min(10_000);
        } else if let Some(kw) = trigger_match {
            let exact = kw.word.eq_ignore_ascii_case(qt)
                || kw.display_name().eq_ignore_ascii_case(qt);
            r.score = 960_000 + i32::from(exact);
        } else if learned > 0 {
            r.score = r.score.max(920_000 + learned);
        }
    }

    let mut r = merge_pinned(ranked, &ql, &config.pinned_results);
    let ops: Vec<SearchResult> = crate::operations::running_result_rows()
        .into_iter()
        .enumerate()
        .map(|(i, mut op)| {
            op.score = 950_000 - i as i32;
            op
        })
        .collect();
    r.extend(ops);
    r.sort_by(|a, b| b.score.cmp(&a.score));
    r.truncate(20);
    r
}

/// A result type's own mode: only that kind of result, for the typed text.
fn result_mode(
    id: &str,
    rest: &str,
    config: &Config,
    snap_lock: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<SearchResult> {
    let mut r = match id {
        "apps" => {
            let snap_guard = snap_lock.read().unwrap();
            let mut r = apps::search(rest, &snap_guard.apps);
            if config.app_sources().appimage {
                r.extend(appimage::search(rest, 5));
            }
            r
        }
        "newapps" => cmd::universal_install(rest, config.app_sources(), 20),
        "web" if !rest.is_empty() => vec![web::result(rest, config)],
        "calc" => calculator::evaluate(rest, config).into_iter().collect(),
        "convert" => convert::convert(rest, config).into_iter().collect(),
        "updates" => cmd::update_results(rest, config),
        _ => Vec::new(),
    };
    r.sort_by(|a, b| b.score.cmp(&a.score));
    r.truncate(20);
    r
}

pub fn search_mode(
    mode_word: &str,
    query: &str,
    config: &Config,
    snap_lock: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<SearchResult> {
    // A mode is named by its word — or, for a keyword without one (a result
    // type reached through its shortcut), by its id.
    let kw = match config
        .keyword_for_word(mode_word)
        .or_else(|| config.keyword_for_id(mode_word))
    {
        Some(kw) => kw,
        None => return vec![],
    };

    if kw.all_files || !kw.extensions.is_empty() {
        // ensure_files_indexed() must be called by the caller (main thread)
        // before invoking this function.
    }

    let rest = query.trim();

    if kw.id == "proton-bridge" {
        return proton_bridge::search(rest);
    }

    if kw.id == "clipboard" {
        // Clipboard mode not used in worker (it uses Rc<ClipboardHistory> on main).
        return vec![];
    }
    let rl = rest.to_lowercase();
    let pinned = &config.pinned_results;
    if kw.id == "emoji" {
        return emoji::search(rest);
    }
    if kw.id == "bluetooth" {
        return bluetooth::search(rest);
    }
    if kw.is_result() {
        return result_mode(&kw.id, rest, config, snap_lock);
    }
    if kw.id == "run" {
        return run::search(rest);
    }
    if kw.id == "cmd" {
        let snap_guard = snap_lock.read().unwrap();
        return merge_pinned(
            cmd::search(rest, config, &snap_guard.apps),
            &rl,
            pinned,
        );
    }
    if kw.id == "translate" {
        // The translate trigger: live, local translation — target follows
        // the system language unless a language was picked.
        return translate::results(rest, config);
    }
    if kw.all_files {
        // The dedicated "Find" trigger always supports path browsing,
        // independent of the general "Root Path Browsing" toggle (which only
        // governs typing `/` directly into the universal search box).
        if rest.starts_with('/') || rest.starts_with("~/") || rest == "~" {
            return browse::browse(rest);
        }
        if rest.is_empty() {
            return if config.show_recent_file_searches {
                crate::recent_paths::results()
            } else {
                vec![]
            };
        }
        let files = files::search_find(rest, &snap_lock);
        return merge_pinned(files, &rl, pinned);
    }
    if !kw.extensions.is_empty() {
        let query = if rest.is_empty() {
            kw.word.clone()
        } else {
            format!("{} {}", kw.word, rest)
        };
        let mut files = {
            let snap_guard = snap_lock.read().unwrap();
            files::search(&query, &snap_guard.files, config)
        };
        files.truncate(20);
        return merge_pinned(files, &rl, pinned);
    }
    // Installed trigger actions. Files-type triggers were already handled by the
    // all_files/extensions branches above (they convert to ordinary keywords);
    // only web and shell actions reach this branch.
    if let Some(trigger) = crate::triggers::by_id(&kw.id) {
        // The dictionary trigger does live lookups (word autocomplete +
        // definition) instead of a plain web search.
        if kw.id == "dictionary" {
            return dictionary::results(rest);
        }
        let icon = Some(if trigger.icon.is_empty() {
            "folder-symbolic".into()
        } else {
            trigger.icon.clone()
        });
        return match &trigger.action {
            crate::triggers::TriggerAction::Web { url } => {
                if rest.is_empty() {
                    return vec![SearchResult {
                        kind: ResultKind::System,
                        title: gettext("Type something to search with {name}").replace("{name}", &trigger.name),
                        subtitle: Some(trigger.description.clone()),
                        icon,
                        action: Action::EnterMode(kw.word.clone()),
                        score: 1000,
                    }];
                }
                let encoded = urlencoding::encode(rest);
                let url = url.replace("{query}", &encoded);
                vec![SearchResult {
                    kind: ResultKind::Web,
                    title: gettext("{name}: {query}").replace("{name}", &trigger.name).replace("{query}", rest),
                    subtitle: Some(url.clone()),
                    icon,
                    action: Action::OpenUrl(url),
                    score: 100_000,
                }]
            }
            crate::triggers::TriggerAction::Shell { command } => {
                if rest.is_empty() {
                    return vec![SearchResult {
                        kind: ResultKind::System,
                        title: gettext("Type a command to run with {name}").replace("{name}", &trigger.name),
                        subtitle: Some(trigger.description.clone()),
                        icon,
                        action: Action::EnterMode(kw.word.clone()),
                        score: 1000,
                    }];
                }
                // {query} is single-quote-escaped: user input can't inject
                // extra shell commands, only the template author's command runs.
                let rendered = command.replace("{query}", &crate::triggers::shell_escape(rest));
                vec![SearchResult {
                    kind: ResultKind::System,
                    title: gettext("Run {name}").replace("{name}", &trigger.name),
                    subtitle: Some(rendered.clone()),
                    icon,
                    action: Action::RunWithProgress {
                        title: gettext("{name}: {query}").replace("{name}", &trigger.name).replace("{query}", rest),
                        args: run::command_argv(&rendered),
                    },
                    score: 100_000,
                }]
            }
            crate::triggers::TriggerAction::Files { .. }
            | crate::triggers::TriggerAction::Native => vec![],
        };
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_type_mode_searches_only_its_results() {
        let mut cfg = Config::default();
        cfg.install_builtin("calc");
        cfg.install_builtin("web");
        let snap = Arc::new(RwLock::new(crate::index::Snapshot::default()));
        // No word yet: the mode is reached by id, as its shortcut does.
        let calc = search_mode("calc", "2+3", &cfg, &snap);
        assert!(!calc.is_empty(), "calc mode computes");
        assert!(calc.iter().all(|r| r.title.contains('5') || r.subtitle.as_deref().unwrap_or("").contains('5')), "{calc:?}");
        let web = search_mode("web", "rust", &cfg, &snap);
        assert_eq!(web.len(), 1);
        assert!(matches!(web[0].action, Action::OpenUrl(_)), "{web:?}");
        // A word-less keyword is never offered as something to type.
        let suggested = trigger_suggestions("ca", &cfg);
        assert!(
            !suggested.iter().any(|r| matches!(&r.action, Action::EnterMode(w) if w.is_empty())),
            "{suggested:?}"
        );
    }

    /// A config carrying the dictionary keyword (store-installed trigger).
    fn config_with_dictionary() -> Config {
        let mut cfg = Config::default();
        cfg.command_keywords.push(crate::config::CommandKeyword {
            id: "dictionary".into(),
            word: "dict".into(),
            description: String::new(),
            extensions: vec![],
            icon: String::new(),
            all_files: false,
            shortcut: String::new(),
            enabled: true,
        });
        cfg
    }

    #[test]
    fn fuzzy_match_catches_typos_but_not_unrelated_words() {
        // Mid-word typos hit their target…
        assert!(fuzzy_match("sytem", "system"));
        assert!(fuzzy_match("fltak", "flatpak"));
        assert!(fuzzy_match("updte", "update"));
        assert!(fuzzy_match("shutdn", "Shutdown"));
        // …including classes nucleo's subsequence scoring rejects:
        assert!(fuzzy_match("sistem", "system"), "substituted letter");
        assert!(fuzzy_match("updaet", "update"), "swapped letters");
        assert!(fuzzy_match("aall", "all"), "doubled letter");
        // …short queries stay with the explicit prefix logic…
        assert!(!fuzzy_match("a", "anything"));
        // …and words that merely share letters never match.
        assert!(!fuzzy_match("upload", "update"));
        assert!(!fuzzy_match("zyx", "system"));
    }

    #[test]
    fn typoed_trigger_word_still_suggests_its_mode() {
        let snap = crate::index::Snapshot {
            apps: Vec::new(),
            files: Vec::new(),
        };
        let lock = std::sync::Arc::new(std::sync::RwLock::new(snap));
        let cfg = Config::default();

        // "find" with a dropped letter still suggests entering Find mode…
        let rows = super::search("fnd", &cfg, &lock);
        assert!(
            rows.iter()
                .any(|r| matches!(&r.action, Action::EnterMode(w) if w == "find")),
            "fnd: {:?}",
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
        // …an unrelated word never suggests a mode. (Operation rows encode
        // their state in EnterMode too, so only real keyword suggestions
        // count here.)
        let rows = super::search("xyzzy", &cfg, &lock);
        assert!(
            !rows.iter().any(|r| match &r.action {
                Action::EnterMode(w) => cfg.keyword_for_word(w).is_some(),
                _ => false,
            }),
            "xyzzy: {:?}",
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn typoed_inline_trigger_still_routes() {
        let cfg = Config::default();
        // A dropped letter routes "fnd report" into the file search…
        assert_eq!(
            super::inline_file_mode("fnd report", &cfg),
            Some(("find".to_string(), "report".to_string()))
        );
        // …while short words, where unrelated queries live, never hijack.
        assert_eq!(super::inline_file_mode("san francisco", &cfg), None);
    }

    #[test]
    fn trigger_suggestions_match_typos_and_fully_typed_long_words() {
        let mut cfg = Config::default();
        cfg.command_keywords.push(crate::config::CommandKeyword {
            id: "superlong".into(),
            word: "superlongword".into(),
            description: "Test trigger".into(),
            extensions: vec![],
            icon: String::new(),
            all_files: false,
            shortcut: String::new(),
            enabled: true,
        });
        // The fully-typed 13-char word matches — the old <=8 gate hid it.
        let rows = super::trigger_suggestions("superlongword", &cfg);
        assert!(
            rows.iter()
                .any(|r| matches!(&r.action, Action::EnterMode(w) if w == "superlongword")),
            "long word: {:?}",
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
        // A typo'd default keyword still suggests its mode…
        let rows = super::trigger_suggestions("fnd", &cfg);
        assert!(
            rows.iter()
                .any(|r| matches!(&r.action, Action::EnterMode(w) if w == "find")),
            "fnd: {:?}",
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
        // …and non-words never match anything.
        assert!(super::trigger_suggestions("xyzzy123", &cfg).is_empty());
    }

    #[test]
    fn dict_word_inline_routes_to_the_dictionary() {
        let cfg = config_with_dictionary();
        // The whole point: `dict <word>` shows the meaning directly, without
        // a separate mode-entry step first.
        assert_eq!(
            inline_file_mode("dict serendipity", &cfg),
            Some(("dict".to_string(), "serendipity".to_string()))
        );
        assert_eq!(
            inline_file_mode("dict  spaced rest", &cfg),
            Some(("dict".to_string(), "spaced rest".to_string()))
        );
        // A lone keyword (no rest) is not an inline route — it stays the
        // mode suggestion the user enters with.
        assert_eq!(inline_file_mode("dict", &cfg), None);
        // Unknown words never route.
        assert_eq!(inline_file_mode("nope x", &cfg), None);

        // Regression guard: the all-files keyword still routes inline.
        let files = Config::default();
        assert_eq!(
            inline_file_mode("find notes.txt", &files),
            Some(("find".to_string(), "notes.txt".to_string()))
        );
    }
}
