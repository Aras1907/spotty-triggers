// CMD trigger mode: kill running processes, install/uninstall/search Flatpak+distro apps.
// Suggestions are built from async-fetched caches so the UI never blocks.

use crate::config::{AppSources, Config};
use crate::index::AppEntry;
use crate::search::{Action, ResultKind, SearchResult};
use crate::i18n::gettext;
use gtk::glib;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::collections::HashMap;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct UpdateInfo {
    source: String,
    app_id: Option<String>,
    name: String,
    /// The concrete packages behind this row (used by the preview pane):
    /// "name  version" lines per source, app names for flatpak.
    details: Vec<String>,
}

#[derive(Clone)]
struct FlatpakApp {
    app_id: String,
    name: String,
    description: String,
    app_id_lc: String,
    name_lc: String,
    description_lc: String,
}

impl FlatpakApp {
    fn new(app_id: String, name: String, description: String) -> Self {
        let app_id_lc = app_id.to_lowercase();
        let name_lc = name.to_lowercase();
        let description_lc = description.to_lowercase();
        Self {
            app_id,
            name,
            description,
            app_id_lc,
            name_lc,
            description_lc,
        }
    }
}

#[derive(Clone)]
struct DistroPackage {
    name: String,
    description: String,
    name_lc: String,
    description_lc: String,
}

impl DistroPackage {
    fn new(name: String, description: String) -> Self {
        let name_lc = name.to_lowercase();
        let description_lc = description.to_lowercase();
        Self {
            name,
            description,
            name_lc,
            description_lc,
        }
    }
}

// ── Thread-safe caches ────────────────────────────────────────────────────────

fn process_cache() -> &'static Mutex<Option<(Instant, Vec<String>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<String>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn process_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Running Flatpak app-ids (from `flatpak ps`), refreshed alongside `ps`.
fn flatpak_running_cache() -> &'static Mutex<Option<(Instant, Vec<String>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<String>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn flatpak_running_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

fn installed_cache() -> &'static Mutex<Option<(Instant, Vec<FlatpakApp>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<FlatpakApp>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn installed_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

fn flatpak_catalog_cache() -> &'static Mutex<Option<(Instant, Vec<FlatpakApp>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<FlatpakApp>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn flatpak_catalog_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Distro PM: full available-package catalog, fetched once and fuzzy-matched
// in-memory (same approach as the Flatpak catalog) so typing feels instant
// instead of shelling out to the package manager on every keystroke.
fn distro_catalog_cache() -> &'static Mutex<Option<(Instant, String, Arc<Vec<DistroPackage>>)>> {
    static C: OnceLock<Mutex<Option<(Instant, String, Arc<Vec<DistroPackage>>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn distro_catalog_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Background fuzzy search over the distro catalog: keeps the UI thread from
// scanning the (potentially tens-of-thousands-of-entries) catalog on every
// keystroke. `ensure_distro_search` kicks off a background match for the
// current query; `distro_search_cache` holds the latest completed matches as
// raw `DistroPackage`s — the `SearchResult` rows (and the already-installed
// filter) are built at read time, so an installed list that warms up after
// the match still takes effect. The generation counter ensures only the most
// recent query's results win, even if an older search thread finishes after a
// newer one.
fn distro_search_generation() -> &'static AtomicU64 {
    static C: OnceLock<AtomicU64> = OnceLock::new();
    C.get_or_init(|| AtomicU64::new(0))
}
fn distro_search_cache() -> &'static Mutex<Option<(String, Vec<DistroPackage>)>> {
    static C: OnceLock<Mutex<Option<(String, Vec<DistroPackage>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

// Distro PM: installed packages cache
fn distro_installed_cache() -> &'static Mutex<Option<(Instant, Vec<DistroPackage>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<DistroPackage>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn distro_installed_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Snap: like distro, an async presence check — None = not yet checked,
// Some(true/false) = snapd available on the host.
fn snap_available_cache() -> &'static Mutex<Option<bool>> {
    static C: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn snap_available_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Flatpak presence — same shape as the snap one, so the Settings source
// switches can hide themselves on systems without flatpak.
fn flatpak_available_cache() -> &'static Mutex<Option<bool>> {
    static C: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn flatpak_available_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Snap installed-package cache (`snap list`), same shape as the distro one.
fn snap_installed_cache() -> &'static Mutex<Option<(Instant, Vec<DistroPackage>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<DistroPackage>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn snap_installed_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// Snap find is a live, server-side search (there's no local catalog dump),
// so results are cached per-query instead of against a preloaded catalog.
// Generation counter keeps only the newest query's results winning.
fn snap_search_generation() -> &'static AtomicU64 {
    static C: OnceLock<AtomicU64> = OnceLock::new();
    C.get_or_init(|| AtomicU64::new(0))
}
fn snap_search_cache() -> &'static Mutex<Option<(String, Vec<SearchResult>)>> {
    static C: OnceLock<Mutex<Option<(String, Vec<SearchResult>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

// Software update cache (5-minute TTL — updates change faster than catalogs).
fn update_cache() -> &'static Mutex<Option<(Instant, Vec<UpdateInfo>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<UpdateInfo>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}
fn update_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

/// Clear all installed-package caches so the next search re-queries flatpak/dnf/snap.
/// Called after an operation finishes to ensure removed apps vanish from results.
pub fn invalidate_installed_caches() {
    *installed_cache().lock().unwrap() = None;
    *distro_installed_cache().lock().unwrap() = None;
    *snap_installed_cache().lock().unwrap() = None;
}

// ── Host-aware command helpers ────────────────────────────────────────────────

fn is_sandbox() -> bool {
    std::env::var("FLATPAK_ID").is_ok()
}

// Build a Command that runs `prog` on the host (via flatpak-spawn when sandboxed).
fn host_command(prog: &str) -> std::process::Command {
    if is_sandbox() {
        let mut c = std::process::Command::new("flatpak-spawn");
        c.args(["--host", prog]);
        c
    } else {
        std::process::Command::new(prog)
    }
}

// Build the argv for a flatpak sub-command, prefixing with flatpak-spawn when needed.
fn flatpak_cmd_args(subargs: &[&str]) -> Vec<String> {
    if is_sandbox() {
        let mut v = vec![
            "flatpak-spawn".to_string(),
            "--host".to_string(),
            "flatpak".to_string(),
        ];
        v.extend(subargs.iter().map(|s| s.to_string()));
        v
    } else {
        let mut v = vec!["flatpak".to_string()];
        v.extend(subargs.iter().map(|s| s.to_string()));
        v
    }
}

// ── Distro and system packages ────────────────────────────────────────────────

/// The package manager of the distro Spotty runs on — read from os-release
/// (see [`crate::distro`]), not guessed from which binaries are on the PATH,
/// so it is the same in the Flatpak sandbox and on the host. `None` where
/// system packages are off: an unsupported distro or an image-based one.
fn get_detected_pm() -> Option<String> {
    let d = crate::distro::current();
    if d.supports_system_packages() {
        d.package_manager().map(str::to_string)
    } else {
        None
    }
}

/// What a distro-package source has to work with on this system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemPackages {
    /// Searching, installing and updating all work; `via` names the service.
    Ready { distro: String, pm: String, via: String },
    /// An image-based system: packages are managed as a whole image.
    Immutable { distro: String },
    /// No package manager Spotty speaks to (Alpine, Void, NixOS, …).
    Unsupported { distro: String },
    /// A known distro, but nothing to apply changes with: PackageKit (or
    /// dnf5daemon) isn't running, and a password prompt of our own is not an
    /// acceptable substitute.
    NoService { distro: String, pm: String },
}

pub(crate) fn system_packages() -> SystemPackages {
    let d = crate::distro::current();
    let distro = d.display();
    if d.immutable {
        return SystemPackages::Immutable { distro };
    }
    let Some(pm) = d.package_manager() else {
        return SystemPackages::Unsupported { distro };
    };
    let pm = pm.to_string();
    if crate::packagekit::available() {
        let via = match crate::packagekit::backend_name() {
            Some(b) => format!("PackageKit · {b}"),
            None => "PackageKit".to_string(),
        };
        SystemPackages::Ready { distro, pm, via }
    } else if system_door() == Some(Door::Dnf5Daemon) {
        SystemPackages::Ready { distro, pm, via: "dnf5daemon".to_string() }
    } else {
        SystemPackages::NoService { distro, pm }
    }
}

/// The service that applies distro updates here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Door {
    /// Fedora's store daemon: downloads, then the restart installs.
    Dnf5Daemon,
    /// PackageKit: every other distro (and Fedora without the daemon).
    PackageKit,
}

/// Which service applies distro updates, if any. `None` means distro updates
/// aren't offered at all — never a fallback that asks for a root password.
fn system_door() -> Option<Door> {
    let d = crate::distro::current();
    if !d.supports_system_packages() {
        return None;
    }
    if d.family == crate::distro::Family::Fedora && crate::dnf5daemon::available() {
        Some(Door::Dnf5Daemon)
    } else if crate::packagekit::available() {
        Some(Door::PackageKit)
    } else {
        None
    }
}

/// True when an update is downloaded and armed for the next restart, whichever
/// service (or store) armed it.
fn any_offline_armed() -> bool {
    crate::dnf5daemon::offline_armed() || crate::packagekit::offline_armed()
}

/// A package-manager name as the update cache labels distro packages.
fn is_distro_source(source: &str) -> bool {
    matches!(source, "dnf" | "apt" | "pacman" | "zypper")
}

// ── Snap presence ─────────────────────────────────────────────────────────────

fn ensure_snap_available() {
    {
        let c = snap_available_cache().lock().unwrap();
        if c.is_some() {
            return;
        }
    }
    {
        let mut f = snap_available_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(|| {
        let available = host_command("snap")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        *snap_available_cache().lock().unwrap() = Some(available);
        *snap_available_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

pub(crate) fn snap_is_available() -> Option<bool> {
    snap_available_cache().lock().ok().and_then(|g| *g)
}

fn ensure_flatpak_available() {
    {
        let c = flatpak_available_cache().lock().unwrap();
        if c.is_some() {
            return;
        }
    }
    {
        let mut f = flatpak_available_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(|| {
        let available = host_command("flatpak")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        *flatpak_available_cache().lock().unwrap() = Some(available);
        *flatpak_available_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

/// `None` while the probe is still in flight (Settings hides the switch
/// until the answer is known, like the snap option).
pub(crate) fn flatpak_is_available() -> Option<bool> {
    flatpak_available_cache().lock().ok().and_then(|g| *g)
}

/// The detected distro package manager name ("dnf", "apt", …), if any —
/// gates the "System packages" source switch in Settings.
pub(crate) fn detected_distro_pm() -> Option<String> {
    get_detected_pm()
}

// ── Background fetchers ───────────────────────────────────────────────────────

pub fn prewarm_update_cache() {
    ensure_updates_checked();
}

pub fn preload_install_cache_async(sources: AppSources) {
    // Source presence is always probed (not just when the saved switches use
    // them) so the Settings source switches can show/hide based on it.
    ensure_snap_available();
    ensure_flatpak_available();
    // AppImage discovery + updater probe, same reason.
    crate::search::appimage::preload();
    if sources.flatpak {
        ensure_flatpak_catalog();
    }
    if sources.distro {
        // Warm the (cached) full package catalog so it's ready before the user
        // finishes typing "install ".
        if let Some(pm_name) = get_detected_pm() {
            ensure_distro_catalog(pm_name);
        }
    }
}

fn ensure_processes() {
    {
        let c = process_cache().lock().unwrap();
        if let Some((t, _)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(3) {
                return;
            }
        }
    }
    {
        let mut f = process_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    {
        let mut f = flatpak_running_fetching().lock().unwrap();
        *f = true;
    }
    std::thread::spawn(|| {
        let raw = host_command("ps")
            .args(["-eo", "comm", "--no-headers"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let mut names: Vec<String> = raw
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|s| !s.is_empty() && s != "ps" && s != "flatpak-spawn")
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        *process_cache().lock().unwrap() = Some((Instant::now(), names));
        *process_fetching().lock().unwrap() = false;

        let raw = host_command("flatpak")
            .args(["ps", "--columns=application"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let ids: Vec<String> = raw
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        *flatpak_running_cache().lock().unwrap() = Some((Instant::now(), ids));
        *flatpak_running_fetching().lock().unwrap() = false;

        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

fn ensure_installed() {
    {
        let c = installed_cache().lock().unwrap();
        if let Some((t, _)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(30) {
                return;
            }
        }
    }
    {
        let mut f = installed_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(|| {
        let raw = host_command("flatpak")
            .args(["list", "--app", "--columns=application,name"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let apps: Vec<FlatpakApp> = raw
            .lines()
            .filter_map(|line| {
                let mut p = line.splitn(2, '\t');
                let app_id = p.next()?.trim().to_string();
                if app_id.is_empty() {
                    return None;
                }
                let name = p.next().unwrap_or(&app_id).trim().to_string();
                Some(FlatpakApp::new(app_id, name, String::new()))
            })
            .collect();
        *installed_cache().lock().unwrap() = Some((Instant::now(), apps));
        *installed_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

// ── Update checking ───────────────────────────────────────────────────────────

/// The universal update verb: `update`, `updates`, `upd`, `upgrade`,
/// `upg`, optionally followed by a target ("update flatpak", "update all",
/// "update firefox"). Updates are not a trigger, so this runs in plain
/// universal search. A still-incomplete verb ("up", "updat") shows the very
/// same rows as the complete verb — one update row in every state, with
/// ghost text completing the verb itself (see [`verb_completion`]).
pub fn update_verb_rows(query: &str, config: &Config) -> Option<Vec<SearchResult>> {
    let q = query.trim();
    let (first, rest) = match q.find(char::is_whitespace) {
        Some(i) => (&q[..i], q[i..].trim()),
        None => (q, ""),
    };
    let verb = first.to_ascii_lowercase();
    let exact = verb == "update"
        || verb == "updates"
        || verb == "upd"
        || verb.starts_with("upg");
    let prefix = rest.is_empty() && verb_completion(q).is_some();
    // A typo'd verb ("updte", "udpat") still opens the same list; two
    // edits is strict enough that "upload" never counts as "update".
    let typo = ["update", "updates", "upgrade"]
        .iter()
        .any(|v| crate::search::fuzzy_match(&verb, v));
    if !exact && !prefix && !typo {
        return None;
    }
    let score = if rest.is_empty() { 100_000 } else { 50_000 };
    let mut rows = update_results(rest, config);
    // Boost the actual update rows above incidental matches; leave the
    // "disable update checks" tail row where it belongs.
    for r in &mut rows {
        if r.score >= 1000 {
            r.score = r.score.max(score);
        }
    }
    Some(rows)
}

/// The ghost completion for a still-incomplete update verb — "upd" →
/// "update ", "upg" → "upgrade " — `None` once the verb is complete or the
/// text is shorter than two characters (so "u" stays available for other
/// words like "uninstall").
pub fn verb_completion(user_text: &str) -> Option<String> {
    if user_text.chars().count() < 2 || user_text.contains(char::is_whitespace) {
        return None;
    }
    let t = user_text.to_ascii_lowercase();
    // A complete verb stays as it is ("update" must not grow into
    // "updates"), even though the longer one starts with it.
    if ["update", "updates", "upgrade"].contains(&t.as_str()) {
        return None;
    }
    ["update", "updates", "upgrade"]
        .iter()
        .find(|v| v.starts_with(&t))
        .map(|v| format!("{v} "))
}

pub fn ensure_updates_checked() {
    ensure_updates_checked_age(Duration::from_secs(5 * 60));
}

/// Refresh the cached update list once it is older than `max_age`. The
/// background scheduler passes the user-configured interval, searches pass
/// a short one so the list in the results stays fresh.
/// Milliseconds of the last finished update run: a check that started
/// before it can still contain the packages that run just installed, so its
/// result is discarded instead of cached.
static LAST_UPDATE_DONE: AtomicU64 = AtomicU64::new(0);

/// True when the check started at `started` (ms) predates the last
/// finished update run.
fn check_is_stale(started: u64) -> bool {
    started < LAST_UPDATE_DONE.load(Ordering::SeqCst)
}

/// Drop the cached pending list and remember when — see [`check_is_stale`].
fn invalidate_update_cache() {
    LAST_UPDATE_DONE.store(now_millis(), Ordering::SeqCst);
    *update_cache().lock().unwrap() = None;
}

/// An update run finished: forget the stale pending list and check again
/// right away, so the rows (and the badge) show what is actually left.
pub fn refresh_updates_after_run() {
    invalidate_update_cache();
    ensure_updates_checked_age(Duration::ZERO);
}

pub fn ensure_updates_checked_age(max_age: Duration) {
    {
        let c = update_cache().lock().unwrap();
        if let Some((t, _)) = c.as_ref() {
            if t.elapsed() < max_age {
                return;
            }
        }
    }
    {
        let mut f = update_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    let started = now_millis();
    std::thread::spawn(move || {
        let updates = fetch_updates();
        // A run finished while this check was in flight: its list can still
        // contain the updated packages — drop it and check once more.
        let stale = check_is_stale(started);
        if !stale {
            *update_cache().lock().unwrap() = Some((Instant::now(), updates.clone()));
        }
        *update_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(move || {
            crate::app::refresh_search_window();
            if !stale {
                notify_if_new(&updates);
            }
        });
        if stale {
            ensure_updates_checked_age(Duration::ZERO);
        }
    });
}

/// Startup / periodic tick: respects the feature switch and the
/// user-configured interval (Settings → Updates).
pub fn periodic_update_check(config: &Config) {
    if !config.result_enabled("updates") {
        return;
    }
    let interval =
        Duration::from_secs(config.update_check_interval_hours.max(1) as u64 * 3600);
    ensure_updates_checked_age(interval);
}

/// "Check now" in Settings → Updates: ignore the freshness window.
pub fn check_updates_now() {
    ensure_updates_checked_age(Duration::ZERO);
}

fn update_cmd_args(source: &str, app_id: Option<&str>) -> Vec<String> {
    match source {
        "flatpak" if app_id.is_some() => {
            flatpak_cmd_args(&["update", "--assumeyes", app_id.unwrap()])
        }
        "flatpak" => flatpak_cmd_args(&["update", "--assumeyes"]),
        "dnf" | "apt" | "pacman" | "zypper" => distro_update_args(None),
        "snap" => pkexec_cmd_args(vec!["snap".into(), "refresh".into()]),
        // One file per run — the classic tool takes a single path, so the
        // scope row chains them (markers keep the progress tracker aware).
        "appimage" if app_id.is_some() => {
            crate::search::appimage::update_args(std::path::Path::new(app_id.unwrap()))
        }
        "appimage" => {
            let mut parts: Vec<(&str, String)> = Vec::new();
            {
                let cache = update_cache().lock().unwrap();
                if let Some((_, entries)) = cache.as_ref() {
                    for u in entries.iter().filter(|u| u.source == "appimage") {
                        if let Some(p) = &u.app_id {
                            if let Some(cmd) =
                                crate::search::appimage::update_shell_cmd(std::path::Path::new(p))
                            {
                                parts.push(("appimage", cmd));
                            }
                        }
                    }
                }
            }
            if parts.is_empty() {
                no_updates_cmd()
            } else {
                chained_script(&parts)
            }
        }
        "all" => {
            let mut parts: Vec<(&str, String)> = Vec::new();
            let mut has_distro = false;
            let cache = update_cache().lock().unwrap();
            if let Some((_, entries)) = cache.as_ref() {
                let has_fp = entries
                    .iter()
                    .any(|u| u.source == "flatpak");
                if has_fp {
                    parts.push(("flatpak", "flatpak update --assumeyes".into()));
                }
                // Distro packages go through a system service only — an
                // in-process D-Bus task, not a shell command, so it can't join
                // the chain below.
                has_distro = entries.iter().any(|u| is_distro_source(&u.source));
                if entries.iter().any(|u| u.source == "snap") {
                    parts.push(("snap", "pkexec snap refresh".into()));
                }
                // AppImage updates always come last: they run unprivileged
                // and in place (no pkexec involved).
                for u in entries.iter().filter(|u| u.source == "appimage") {
                    if let Some(p) = &u.app_id {
                        if let Some(cmd) =
                            crate::search::appimage::update_shell_cmd(std::path::Path::new(p))
                        {
                            parts.push(("appimage", cmd));
                        }
                    }
                }
            }
            drop(cache);
            // The daemon half runs first (it installs at the next restart,
            // the rest install immediately), so the chain follows it as the
            // task's script argument.
            let door = if has_distro { system_door() } else { None };
            let via_daemon = door.is_some();
            if parts.is_empty() && !via_daemon {
                no_updates_cmd()
            } else if let Some(door) = door {
                let script = chained_script_text(&parts);
                match door {
                    Door::Dnf5Daemon => crate::dnf5daemon::all_args(&script),
                    Door::PackageKit => crate::packagekit::all_args(&script),
                }
            } else {
                chained_script(&parts)
            }
        }
        _ => no_updates_cmd(),
    }
}

/// `sh -c <script>` argv that runs the script **on the host**. Inside the
/// Flatpak sandbox a plain `sh` has no `dnf`, `pkexec` or `flatpak` CLI —
/// an update chain written for the host must be handed to the host whole,
/// reboot suffix included.
fn host_shell_argv(script: &str) -> Vec<String> {
    if is_sandbox() {
        vec![
            "flatpak-spawn".to_string(),
            "--host".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ]
    } else {
        vec!["sh".to_string(), "-c".to_string(), script.to_string()]
    }
}

/// The argv for an update action when there is nothing to update: fails
/// loudly (stderr + exit 1). The old `true` completed silently — and inside
/// "Update & Restart" that meant rebooting without installing anything.
fn no_updates_cmd() -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "echo {} >&2; exit 1",
            crate::search::appimage::shell_quote(&gettext("No updates available"))
        ),
    ]
}

/// Build the shell argv for a chained multi-source update run. Each part is
/// prefixed with an `__spotty_part_k_m_tool__` echo marker so the progress
/// tracker knows which tool is running and what share of the whole update it
/// owns (see `opprogress`). Every source is attempted even when an earlier
/// one fails (`;` + a status accumulator instead of `&&` — a failing flatpak
/// update used to skip the distro upgrade behind it), and the script exits
/// non-zero when any part failed, so the "… && reboot" suffix of
/// "Update & Restart" only reboots after a clean run.
fn chained_script(parts: &[(&str, String)]) -> Vec<String> {
    host_shell_argv(&chained_script_text(parts))
}

/// The chain as one shell script: each part announced by a progress marker, all
/// of them attempted, and a non-zero exit only if something actually failed.
fn chained_script_text(parts: &[(&str, String)]) -> String {
    let total = parts.len();
    let mut script = String::from("st=0; ");
    for (i, (tool, cmd)) in parts.iter().enumerate() {
        script.push_str(&format!(
            "echo __spotty_part_{}_{}_{}__ && {{ {} || st=1; }}; ",
            i + 1,
            total,
            tool,
            cmd
        ));
    }
    script.push_str("[ $st -eq 0 ]");
    script
}
fn fetch_updates() -> Vec<UpdateInfo> {
    // Every source check runs in parallel: the wait is the slowest single
    // check (dnf/flatpak metadata) instead of the sum of all of them. Nothing
    // is cached here — each command still queries live state. Each check
    // returns the *package list*, so "has updates" is just "non-empty" and
    // the preview can show what exactly wants updating. AppImages are their
    // own source: one `-j` network check per installed file (empty without
    // an updater tool — gated in `appimage::pending_updates`).
    let (fp_updates, distro, snap, appimages) = std::thread::scope(|s| {
        let fp = s.spawn(|| fetch_flatpak_updates());
        let distro = s.spawn(|| distro_updates());
        let snap = s.spawn(|| snap_updates());
        let appimage = s.spawn(|| crate::search::appimage::pending_updates());
        (
            fp.join().unwrap_or_default(),
            distro.join().unwrap_or_default(),
            snap.join().unwrap_or_default(),
            appimage.join().unwrap_or_default(),
        )
    });
    let pm = get_detected_pm().unwrap_or_else(|| "dnf".to_string());
    assemble_updates(fp_updates, (&pm, distro), snap, appimages)
}

/// Turn the per-source package lists into the update cache: one entry per
/// real package (a flatpak app, a distro package, a snap, an AppImage).
/// Counts, previews and the "update <package>" rows all derive from these
/// entries, so aggregates can never inflate the "N updates available" badge.
fn assemble_updates(
    fp_updates: Vec<(String, String)>,
    (pm, distro): (&str, Vec<String>),
    snap: Vec<String>,
    appimages: Vec<(String, String)>,
) -> Vec<UpdateInfo> {
    let mut out = Vec::new();
    // Flatpak: one entry per app (name = display name, app_id = command).
    for (app_id, name) in &fp_updates {
        out.push(UpdateInfo {
            source: "flatpak".into(),
            app_id: Some(app_id.clone()),
            name: name.clone(),
            details: vec![name.clone()],
        });
    }
    // Distro packages: one entry per package, labelled with the distro's
    // package manager; the sources return "name  version" lines, which is exactly what the preview shows.
    for pkg in distro {
        out.push(UpdateInfo {
            source: pm.into(),
            app_id: None,
            name: pkg.clone(),
            details: vec![pkg],
        });
    }
    // Snap: one entry per package.
    for pkg in snap {
        out.push(UpdateInfo {
            source: "snap".into(),
            app_id: None,
            name: pkg.clone(),
            details: vec![pkg.clone()],
        });
    }
    // AppImages: one entry per file; `app_id` carries the path the updater
    // runs against (and names may contain spaces, so they're never split).
    for (name, path) in appimages {
        out.push(UpdateInfo {
            source: "appimage".into(),
            app_id: Some(path.clone()),
            name: name.clone(),
            details: vec![format!("{name} — {}", crate::search::appimage::short_path(std::path::Path::new(&path)))],
        });
    }
    out
}

fn fetch_flatpak_updates() -> Vec<(String, String)> {
    // --user and --system in parallel too.
    let raws: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = ["--user", "--system"]
            .iter()
            .copied()
            .map(|scope| {
                s.spawn(move || {
                    host_command("flatpak")
                        .args([
                            scope,
                            "remote-ls",
                            "--updates",
                            "--app",
                            "--columns=application,name",
                        ])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        .unwrap_or_default()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let mut all = Vec::new();
    for raw in raws {
        for line in raw.lines() {
            let mut p = line.splitn(2, '\t');
            let app_id = p.next().unwrap_or("").trim().to_string();
            if app_id.is_empty() || !app_id.contains('.') {
                continue;
            }
            let name = p.next().unwrap_or(&app_id).trim().to_string();
            all.push((app_id, name));
        }
    }
    all
}

/// Pure parsers: command output → "name  version" lines for the preview.

fn parse_dnf_updates(out: &str) -> Vec<String> {
    // `dnf -q check-update`: "name.arch  version  repo" per line.
    out.lines()
        .filter_map(|l| {
            let toks: Vec<&str> = l.split_whitespace().collect();
            if toks.len() < 3 {
                return None;
            }
            Some(format!("{}  {}", toks[0], toks[1]))
        })
        .collect()
}

fn parse_snap_updates(out: &str) -> Vec<String> {
    // `snap refresh --list`: a "Name Version …" table after a header line.
    let mut seen_header = false;
    out.lines()
        .filter_map(|l| {
            let t = l.trim_start();
            if !seen_header {
                if t.starts_with("Name") {
                    seen_header = true;
                }
                return None;
            }
            if t.starts_with('=') || t.is_empty() {
                return None;
            }
            let mut it = t.split_whitespace();
            let name = it.next()?;
            let ver = it.next()?;
            Some(format!("{name}  {ver}"))
        })
        .collect()
}

/// The pending distro package updates, as "name  version" lines.
///
/// Only where a service can apply them (see [`system_door`]) — never list what
/// we couldn't install. Already downloaded and armed for the next restart:
/// nothing left to offer, the restart row owns that state.
fn distro_updates() -> Vec<String> {
    let Some(door) = system_door() else {
        return Vec::new();
    };
    if any_offline_armed() {
        return Vec::new();
    }
    match door {
        // dnf refreshes its own metadata, which PackageKit's cache lags behind.
        Door::Dnf5Daemon => host_command("dnf")
            .args(["check-update", "-q"])
            .output()
            .map(|o| parse_dnf_updates(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default(),
        Door::PackageKit => crate::packagekit::update_lines(),
    }
}

fn snap_updates() -> Vec<String> {
    host_command("snap")
        .args(["refresh", "--list"])
        .output()
        .map(|o| parse_snap_updates(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}


fn ensure_flatpak_catalog() {
    {
        let cache = flatpak_catalog_cache().lock().unwrap();
        if let Some((updated, apps)) = cache.as_ref() {
            if updated.elapsed() < Duration::from_secs(6 * 60 * 60) && !apps.is_empty() {
                return;
            }
        }
    }
    {
        let mut fetching = flatpak_catalog_fetching().lock().unwrap();
        if *fetching {
            return;
        }
        *fetching = true;
    }
    std::thread::spawn(|| {
        let apps = fetch_flatpak_catalog();
        *flatpak_catalog_cache().lock().unwrap() = Some((Instant::now(), apps));
        *flatpak_catalog_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

/// Dependency bases (`org.mozilla.firefox.BaseApp`) come through
/// `flatpak remote-ls --app` but aren't apps a user installs: they carry no
/// appstream or CDN icon and only ever rendered a generic box — hidden from
/// the install catalog.
fn is_catalog_app(app_id: &str) -> bool {
    !app_id.ends_with(".BaseApp")
}

fn fetch_flatpak_catalog() -> Vec<FlatpakApp> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for scope in ["--user", "--system"] {
        let raw = host_command("flatpak")
            .args([
                scope,
                "remote-ls",
                "--cached",
                "--app",
                "--columns=application,name,description",
                "flathub",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        for app in parse_flatpak_apps(&raw) {
            if is_catalog_app(&app.app_id) && seen.insert(app.app_id.clone()) {
                out.push(app);
            }
        }
    }
    out
}

fn parse_flatpak_apps(raw: &str) -> Vec<FlatpakApp> {
    raw.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            let app_id = parts.first()?.trim().to_string();
            if app_id.is_empty() {
                return None;
            }
            let name = parts
                .get(1)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| app_id.clone());
            let description = parts
                .get(2)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            Some(FlatpakApp::new(app_id, name, description))
        })
        .collect()
}

fn ensure_distro_catalog(pm: String) {
    {
        let c = distro_catalog_cache().lock().unwrap();
        if let Some((updated, cpm, _)) = c.as_ref() {
            if *cpm == pm && updated.elapsed() < Duration::from_secs(3600) {
                return;
            }
        }
    }
    {
        let mut f = distro_catalog_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(move || {
        let pkgs = fetch_distro_catalog(&pm);
        *distro_catalog_cache().lock().unwrap() = Some((Instant::now(), pm, Arc::new(pkgs)));
        *distro_catalog_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

// Kick off a background fuzzy match of `query` against the distro catalog (if
// not already cached/in-flight), so the UI thread never scans the full
// (tens-of-thousands-entry) package list directly. Matches land in
// `distro_search_cache` and trigger a refresh when ready.
fn ensure_distro_search(query: String, catalog: Arc<Vec<DistroPackage>>) {
    {
        let c = distro_search_cache().lock().unwrap();
        if let Some((cq, _)) = c.as_ref() {
            if *cq == query {
                return;
            }
        }
    }
    let generation = distro_search_generation().fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let matches: Vec<DistroPackage> = fuzzy_distro(&query, &catalog)
            .into_iter()
            .map(|(pkg, _)| pkg.clone())
            .collect();
        if distro_search_generation().load(Ordering::SeqCst) != generation {
            return;
        }
        *distro_search_cache().lock().unwrap() = Some((query, matches));
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

fn ensure_distro_installed(pm: String) {
    {
        let c = distro_installed_cache().lock().unwrap();
        if let Some((t, _)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(30) {
                return;
            }
        }
    }
    {
        let mut f = distro_installed_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(move || {
        let pkgs = fetch_distro_installed(&pm);
        *distro_installed_cache().lock().unwrap() = Some((Instant::now(), pkgs));
        *distro_installed_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

/// Lowercased names of the distro packages already installed on the host.
/// Empty while the background fetch is still warming — early searches then
/// show install rows until it lands, and the next search drops them.
fn distro_installed_names() -> std::collections::HashSet<String> {
    distro_installed_cache()
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, pkgs)| pkgs.iter().map(|p| p.name_lc.clone()).collect())
        .unwrap_or_default()
}

// ── Distro PM fetch + parse ───────────────────────────────────────────────────

// Fetch the FULL list of available packages once (like the Flatpak catalog),
// so subsequent searches are instant fuzzy lookups in memory instead of a
// fresh package-manager invocation per keystroke.
fn fetch_distro_catalog(pm: &str) -> Vec<DistroPackage> {
    let output = match pm {
        "apt" => host_command("apt-cache").args(["search", "."]).output(),
        "dnf" => host_command("dnf")
            .args([
                "repoquery",
                "--available",
                "--cacheonly",
                "--queryformat",
                "%{name}\t%{summary}\n",
            ])
            .output(),
        "pacman" => host_command("pacman").args(["-Ss", "."]).output(),
        "zypper" => host_command("zypper")
            .args(["--no-refresh", "search"])
            .output(),
        _ => return vec![],
    };
    let raw = output
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut pkgs = parse_distro_search(pm, &raw);
    let mut seen = std::collections::HashSet::new();
    pkgs.retain(|p| seen.insert(p.name_lc.clone()));
    pkgs
}

fn parse_distro_search(pm: &str, raw: &str) -> Vec<DistroPackage> {
    match pm {
        "apt" => raw
            .lines()
            .filter_map(|line| {
                let (name, desc) = line.split_once(" - ")?;
                let name = name.trim().to_string();
                if name.is_empty() {
                    return None;
                }
                Some(DistroPackage::new(name, desc.trim().to_string()))
            })
            .collect(),
        "dnf" => {
            let mut pkgs = Vec::new();
            for line in raw.lines() {
                let t = line.trim();
                if t.is_empty() || t.starts_with('=') || t.starts_with("Matched fields") {
                    continue;
                }
                // dnf5 search: " name.arch\tdescription"; dnf4 search:
                // "name.arch : description"; repoquery (catalog): "name\tdescription"
                // (no arch suffix). Only strip a trailing ".<arch>" component, since
                // package names themselves can legitimately contain dots.
                let parsed = t.split_once('\t').or_else(|| t.split_once(" : "));
                if let Some((name_arch, desc)) = parsed {
                    let name_arch = name_arch.trim();
                    let name = match name_arch.rsplit_once('.') {
                        Some((base, arch))
                            if matches!(
                                arch,
                                "x86_64"
                                    | "i686"
                                    | "noarch"
                                    | "aarch64"
                                    | "armv7hl"
                                    | "s390x"
                                    | "ppc64le"
                            ) =>
                        {
                            base.to_string()
                        }
                        _ => name_arch.to_string(),
                    };
                    if !name.is_empty() && !name.starts_with('=') {
                        pkgs.push(DistroPackage::new(name, desc.trim().to_string()));
                    }
                }
            }
            pkgs
        }
        "pacman" => {
            let lines: Vec<&str> = raw.lines().collect();
            let mut pkgs = Vec::new();
            let mut i = 0;
            while i < lines.len() {
                let line = lines[i].trim();
                if let Some(slash) = line.find('/') {
                    let after = &line[slash + 1..];
                    let name = after.split_whitespace().next().unwrap_or("").to_string();
                    let desc = lines
                        .get(i + 1)
                        .map(|l| l.trim().to_string())
                        .unwrap_or_default();
                    if !name.is_empty() {
                        pkgs.push(DistroPackage::new(name, desc));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            pkgs
        }
        "zypper" => {
            let mut pkgs = Vec::new();
            for line in raw.lines() {
                if !line.contains('|') {
                    continue;
                }
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 3 {
                    let name = parts[1].trim().to_string();
                    let desc = parts[2].trim().to_string();
                    if !name.is_empty() && name != "Name" && !name.starts_with('-') {
                        pkgs.push(DistroPackage::new(name, desc));
                    }
                }
            }
            pkgs
        }
        _ => vec![],
    }
}

fn fetch_distro_installed(pm: &str) -> Vec<DistroPackage> {
    let output = match pm {
        "apt" => host_command("dpkg-query")
            .args(["-W", "-f=${Package}\t${binary:Summary}\n"])
            .output(),
        "dnf" => host_command("rpm")
            .args(["-qa", "--queryformat", "%{NAME}\t%{SUMMARY}\n"])
            .output(),
        "pacman" => host_command("pacman").args(["-Q"]).output(),
        "zypper" => host_command("zypper")
            .args(["packages", "--installed-only"])
            .output(),
        _ => return vec![],
    };
    let raw = output
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    parse_distro_installed(pm, &raw)
}

fn parse_distro_installed(pm: &str, raw: &str) -> Vec<DistroPackage> {
    match pm {
        "apt" | "dnf" => raw
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '\t');
                let name = parts.next()?.trim().to_string();
                let desc = parts.next().unwrap_or("").trim().to_string();
                if name.is_empty() {
                    return None;
                }
                Some(DistroPackage::new(name, desc))
            })
            .collect(),
        "pacman" => raw
            .lines()
            .filter_map(|line| {
                let name = line.split_whitespace().next()?.to_string();
                if name.is_empty() {
                    return None;
                }
                Some(DistroPackage::new(name, String::new()))
            })
            .collect(),
        "zypper" => {
            let mut pkgs = Vec::new();
            for line in raw.lines() {
                if !line.contains('|') {
                    continue;
                }
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 5 {
                    let name = parts[2].trim().to_string();
                    if !name.is_empty() && name != "Name" && !name.starts_with('-') {
                        pkgs.push(DistroPackage::new(name, String::new()));
                    }
                }
            }
            pkgs
        }
        _ => vec![],
    }
}

// ── Snap fetch + parse ────────────────────────────────────────────────────────

// Snap has no local package catalog — `snap find` is a live store search, so
// we fetch per query (instead of preloading a full catalog like distro) and
// cache the results keyed by query string.
fn ensure_snap_search(query: String) {
    {
        let c = snap_search_cache().lock().unwrap();
        if let Some((q, _)) = c.as_ref() {
            if *q == query {
                return;
            }
        }
    }
    // Ensure we know whether installed snaps shadow the candidates.
    ensure_snap_installed();
    let generation = snap_search_generation().fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let results = fetch_snap_search(&query);
        if snap_search_generation().load(Ordering::SeqCst) != generation {
            return;
        }
        *snap_search_cache().lock().unwrap() = Some((query, results));
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

fn fetch_snap_search(query: &str) -> Vec<SearchResult> {
    let raw = host_command("snap")
        .args(["find", "--limit=20", query])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let installed: std::collections::HashSet<String> = snap_installed_cache()
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, pkgs)| pkgs.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default();
    parse_snap_find(&raw)
        .into_iter()
        .filter(|pkg| !installed.contains(&pkg.name))
        .map(|pkg| snap_install_result(pkg))
        .collect()
}

// `snap find` output: Name  Version  Publisher  Notes  Summary
fn parse_snap_find(raw: &str) -> Vec<DistroPackage> {
    raw.lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.is_empty() || t.starts_with("Name") {
                return None;
            }
            let name = t.split_whitespace().next()?.to_string();
            if name.is_empty() {
                return None;
            }
            Some(DistroPackage::new(name, String::new()))
        })
        .collect()
}

fn ensure_snap_installed() {
    {
        let c = snap_installed_cache().lock().unwrap();
        if let Some((t, _)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(30) {
                return;
            }
        }
    }
    {
        let mut f = snap_installed_fetching().lock().unwrap();
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(|| {
        let pkgs = fetch_snap_installed();
        *snap_installed_cache().lock().unwrap() = Some((Instant::now(), pkgs));
        *snap_installed_fetching().lock().unwrap() = false;
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

// `snap list` columns: Name Version Rev Tracking Publisher Notes
fn fetch_snap_installed() -> Vec<DistroPackage> {
    let raw = host_command("snap")
        .args(["list"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    raw.lines()
        .skip(1)
        .filter_map(|line| {
            let name = line.split_whitespace().next()?.to_string();
            if name.is_empty() {
                return None;
            }
            Some(DistroPackage::new(name, String::new()))
        })
        .collect()
}

// ── Fuzzy helpers ─────────────────────────────────────────────────────────────

/// Cap the number of items scanned by the fuzzy/typo pass in catalog searches.
/// Keeps worst-case latency under ~20ms on the UI thread for large Flatpak
/// catalogs while still catching most plausible typos.
const FUZZY_SCAN_CAP: usize = 4000;

fn fuzzy_strings<'a>(query: &str, items: &'a [String]) -> Vec<(&'a String, u32)> {
    if query.is_empty() {
        return items.iter().take(10).map(|s| (s, 1000)).collect();
    }
    let ql = query.to_lowercase();
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);
    let mut scored: Vec<(&String, u32)> = items
        .iter()
        .filter_map(|s| {
            let sl = s.to_lowercase();
            let score = if sl == ql {
                100_000
            } else if sl.starts_with(&ql) {
                50_000
            } else if sl.contains(&ql) {
                30_000
            } else {
                let fs = pattern.score(Utf32String::from(s.as_str()).slice(..), &mut matcher)?;
                let threshold = (ql.len() as u32).saturating_mul(20);
                if fs >= threshold {
                    fs
                } else {
                    return None;
                }
            };
            Some((s, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(20);
    scored
}

fn fuzzy_apps<'a>(query: &str, items: &'a [FlatpakApp]) -> Vec<(&'a FlatpakApp, u32)> {
    let ql = query.trim().to_lowercase();
    if ql.is_empty() {
        return items.iter().take(8).map(|app| (app, 1_000)).collect();
    }

    let ql_len = ql.len();
    let mut best: Vec<(&FlatpakApp, u32)> = Vec::with_capacity(8);
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);
    for (i, app) in items.iter().enumerate() {
        let score = if app.name_lc == ql || app.app_id_lc == ql {
            100_000
        } else if app.name_lc.starts_with(&ql) {
            80_000u32.saturating_sub(app.name_lc.len() as u32)
        } else if app.app_id_lc.starts_with(&ql) {
            70_000u32.saturating_sub(app.app_id_lc.len() as u32)
        } else if word_starts_with(&app.name_lc, &ql) {
            60_000u32.saturating_sub(app.name_lc.len() as u32)
        } else if app.name_lc.contains(&ql) {
            40_000u32.saturating_sub(app.name_lc.len() as u32)
        } else if app.app_id_lc.contains(&ql) {
            30_000u32.saturating_sub(app.app_id_lc.len() as u32)
        } else if ql_len >= 4 && app.description_lc.contains(&ql) {
            10_000
        } else if i < FUZZY_SCAN_CAP && ql_len >= 3 {
            // Nucleo fuzzy fallback for typos (e.g. "blendr" → "Blender")
            if let Some(fs) = pattern.score(
                Utf32String::from(app.name.as_str()).slice(..),
                &mut matcher,
            ) {
                if fs >= (ql_len as u32).saturating_mul(25) {
                    fs / 4 // scale to ~25000 range, below name-contains (40k)
                } else {
                    // Keyboard-layout typo pass (e.g. "frefox" → "Firefox")
                    if let Some(ts) = crate::search::typo::keyboard_similarity(&ql, &app.name_lc) {
                        ts / 2 // scale to ~450 range
                    } else {
                        continue;
                    }
                }
            } else if let Some(ts) = crate::search::typo::keyboard_similarity(&ql, &app.name_lc) {
                ts / 2
            } else {
                continue;
            }
        } else {
            continue;
        };
        insert_top_match(&mut best, app, score);
    }
    best
}

fn word_starts_with(text: &str, query: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(query))
}

fn insert_top_match<'a, T>(best: &mut Vec<(&'a T, u32)>, item: &'a T, score: u32) {
    let pos = best
        .iter()
        .position(|(_, existing)| score > *existing)
        .unwrap_or(best.len());
    if pos < 8 {
        best.insert(pos, (item, score));
        if best.len() > 8 {
            best.pop();
        }
    }
}

fn fuzzy_distro<'a>(query: &str, items: &'a [DistroPackage]) -> Vec<(&'a DistroPackage, u32)> {
    let ql = query.trim().to_lowercase();
    if ql.is_empty() {
        return items.iter().take(8).map(|pkg| (pkg, 1_000)).collect();
    }

    let ql_len = ql.len();
    let mut best: Vec<(&DistroPackage, u32)> = Vec::with_capacity(8);
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);
    for (i, pkg) in items.iter().enumerate() {
        let score = if pkg.name_lc == ql {
            100_000
        } else if pkg.name_lc.starts_with(&ql) {
            60_000u32.saturating_sub(pkg.name_lc.len() as u32)
        } else if pkg.name_lc.contains(&ql) {
            40_000u32.saturating_sub(pkg.name_lc.len() as u32)
        } else if ql_len >= 4 && pkg.description_lc.contains(&ql) {
            10_000
        } else if i < FUZZY_SCAN_CAP && ql_len >= 3 {
            // Nucleo fuzzy fallback for typos (e.g. "chrom" → "chromium")
            if let Some(fs) = pattern.score(
                Utf32String::from(pkg.name.as_str()).slice(..),
                &mut matcher,
            ) {
                if fs >= (ql_len as u32).saturating_mul(25) {
                    fs / 4
                } else if let Some(ts) = crate::search::typo::keyboard_similarity(&ql, &pkg.name_lc) {
                    ts / 2
                } else {
                    continue;
                }
            } else if let Some(ts) = crate::search::typo::keyboard_similarity(&ql, &pkg.name_lc) {
                ts / 2
            } else {
                continue;
            }
        } else {
            continue;
        };
        insert_top_match(&mut best, pkg, score);
    }
    best
}

// ── Template list ─────────────────────────────────────────────────────────────

fn templates() -> Vec<SearchResult> {
    // Running operations (live loading bars) sit at the very top.
    let mut v = crate::operations::running_result_rows();
    v.extend([
        SearchResult {
            kind: ResultKind::System,
            title: gettext("Kill").into(),
            subtitle: Some(gettext("kill <app-name>  —  stop a running process").into()),
            icon: Some("process-stop-symbolic".into()),
            action: Action::EnterMode("cmd".into()),
            score: 1000,
        },
        SearchResult {
            kind: ResultKind::System,
            title: gettext("Install").into(),
            subtitle: Some(gettext("install <app>  —  install from Flatpak, distro, or Snap").into()),
            icon: Some("package-x-generic-symbolic".into()),
            action: Action::EnterMode("cmd".into()),
            score: 999,
        },
        SearchResult {
            kind: ResultKind::System,
            title: gettext("Uninstall").into(),
            subtitle: Some(gettext("uninstall <app>  —  remove an installed app").into()),
            icon: Some("edit-delete-symbolic".into()),
            action: Action::EnterMode("cmd".into()),
            score: 998,
        },
        SearchResult {
            kind: ResultKind::System,
            title: gettext("Update").into(),
            subtitle: Some(gettext("update  —  update installed packages").into()),
            icon: Some("software-update-available-symbolic".into()),
            action: Action::EnterMode("cmd".into()),
            score: 997,
        },
    ]);
    v
}

fn searching_placeholder(label: &str) -> Vec<SearchResult> {
    vec![SearchResult {
        kind: ResultKind::System,
        title: label.into(),
        subtitle: Some(gettext("Please wait…").into()),
        icon: Some("emblem-synchronizing-symbolic".into()),
        action: Action::EnterMode("cmd".into()),
        score: 1000,
    }]
}

// ── Main search ───────────────────────────────────────────────────────────────

pub fn search(query: &str, config: &Config, apps: &[AppEntry]) -> Vec<SearchResult> {
    let sources = config.app_sources();
    // Kick off distro PM detection early so it's ready when needed

    let q = query.trim();
    if q.is_empty() {
        return templates();
    }
    let ql = q.to_lowercase();

    let (verb, rest) = match ql.find(' ') {
        Some(i) => (ql[..i].to_string(), ql[i + 1..].trim().to_string()),
        None => (ql.clone(), String::new()),
    };

    // ── Synonym suggestions ───────────────────────────────────────────────────
    // Words like "add"/"remove"/"delete"/"stop" mean the same thing as
    // install/uninstall/kill but aren't recognized verbs themselves. Suggest
    // the matching action template so the user can press Enter to autofill
    // the canonical action word and continue typing the app name.
    if rest.is_empty() {
        let suggestion = match verb.as_str() {
            "add" | "get" | "download" => Some((
                "Install",
                "install <app-name>  —  searches Flatpak, distro and Snap",
                "package-x-generic-symbolic",
            )),
            "remove" | "delete" | "del" | "erase" | "rem" => Some((
                "Uninstall",
                "uninstall <app-name>  —  remove an installed app",
                "edit-delete-symbolic",
            )),
            "stop" | "end" | "terminate" | "close" => Some((
                "Kill",
                "kill <app-name>  —  stop a running process",
                "process-stop-symbolic",
            )),
            "upgrade" | "up" => Some((
                "Update",
                "update  —  check for & install package updates",
                "software-update-available-symbolic",
            )),
            _ => None,
        };
        if let Some((title, sub, icon)) = suggestion {
            let mut results = vec![SearchResult {
                kind: ResultKind::System,
                title: title.into(),
                subtitle: Some(sub.into()),
                icon: Some(icon.into()),
                action: Action::EnterMode("cmd".into()),
                score: 1000,
            }];
            results.extend(crate::operations::running_result_rows());
            return results;
        }
    }

    // ── Kill ──────────────────────────────────────────────────────────────────
    // Restricted to running processes that correspond to indexed (visible) apps —
    // never arbitrary system processes.
    // Only act on these verbs once the action word has been fully typed
    // ("kill"/"install"/"uninstall") or accepted via autocomplete (which always
    // expands to the canonical word + a space). A bare alias with nothing after
    // it (e.g. just "k") falls through to the template list instead of jumping
    // straight to recommendations.
    if verb == "kill" || (matches_verb(&verb, &["kill", "k", "stop"]) && !rest.is_empty()) {
        ensure_processes();
        let cache = process_cache().lock().unwrap();
        let Some((_, names)) = cache.as_ref() else {
            drop(cache);
            return searching_placeholder(&gettext("Scanning running processes…"));
        };
        let names = names.clone();
        drop(cache);
        let flatpak_ids = flatpak_running_cache()
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default();
        let running = running_apps(&names, &flatpak_ids, apps);
        if rest.is_empty() {
            return running
                .iter()
                .take(10)
                .map(|(name, target)| kill_result(name, target))
                .collect();
        }
        let display_names: Vec<String> = running.iter().map(|(name, _)| name.clone()).collect();
        let matches = fuzzy_strings(&rest, &display_names);
        if matches.is_empty() {
            return vec![SearchResult {
                kind: ResultKind::System,
                title: gettext("No running app matching \"{query}\"").replace("{query}", &rest),
                subtitle: Some(gettext("Only running apps can be killed").into()),
                icon: Some("process-stop-symbolic".into()),
                action: Action::EnterMode("cmd".into()),
                score: 0,
            }];
        }
        return matches
            .iter()
            .filter_map(|(name, _)| {
                running
                    .iter()
                    .find(|(n, _)| n == *name)
                    .map(|(n, p)| kill_result(n, p))
            })
            .collect();
    }

    // ── Uninstall (fully-spelled verb) ────────────────────────────────────
    // Matched before Install on purpose: matches_verb() fuzzy-rates
    // edit_distance("uninstall", "install") == 2 as a match, so the install
    // branch checked first would steal the canonical "uninstall <app>" query
    // and offer to install the app the user is trying to remove.
    if verb == "uninstall" {
        // Surface any running operation's live loading bar above the results.
        let mut results = search_uninstall(&rest, sources.appimage);
        results.extend(crate::operations::running_result_rows());
        return results;
    }

    // ── Install ───────────────────────────────────────────────────────────────
    if verb == "install"
        || (matches_verb(&verb, &["install", "ins", "add", "i"]) && !rest.is_empty())
    {
        // While an install/uninstall is in flight, surface its live loading bar
        // above the search results so the user sees it's already running.
        let mut results = search_install(&rest, sources);
        results.extend(crate::operations::running_result_rows());
        return results;
    }

    // ── Uninstall (fuzzy verbs: "remove", "rem", "del", typos …) ──────────
    if matches_verb(&verb, &["uninstall", "remove", "rem", "uninst", "del"])
        && !rest.is_empty()
    {
        // Surface any running operation's live loading bar above the results.
        let mut results = search_uninstall(&rest, sources.appimage);
        results.extend(crate::operations::running_result_rows());
        return results;
    }

    // ── Update ─────────────────────────────────────────────────────────────────
    if verb == "update"
        || (matches_verb(&verb, &["update", "upgrade", "up"]) && !rest.is_empty())
    {
        let mut results = search_updates(&rest, config);
        results.extend(crate::operations::running_result_rows());
        return results;
    }

    // ── Fuzzy fallback across template titles ─────────────────────────────────
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);
    let mut out: Vec<SearchResult> = templates()
        .into_iter()
        .filter(|r| {
            let tl = r.title.to_lowercase();
            tl.contains(&ql)
                || pattern
                    .score(Utf32String::from(r.title.as_str()).slice(..), &mut matcher)
                    .map(|s| s >= (ql.len() as u32).saturating_mul(25))
                    .unwrap_or(false)
        })
        .collect();
    out.truncate(4);
    out
}

// Search for apps to install across Flatpak and/or distro PM.
fn search_install(query: &str, sources: AppSources) -> Vec<SearchResult> {
    let detected = get_detected_pm();
    let use_flatpak = sources.flatpak;
    // Distro rows need a service to install with: no PackageKit, no rows (a
    // pkexec install would just ask for a root password).
    let use_distro = sources.distro && detected.is_some() && crate::packagekit::available();
    let use_snap = sources.snap && snap_is_available() == Some(true);

    let mut flatpak_results: Vec<SearchResult> = Vec::new();
    let mut distro_results: Vec<SearchResult> = Vec::new();
    let mut snap_results: Vec<SearchResult> = Vec::new();
    let mut loading = false;

    if use_flatpak {
        ensure_flatpak_catalog();
        ensure_installed();
        let installed: std::collections::HashSet<String> = installed_cache()
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, apps)| apps.iter().map(|a| a.app_id.clone()).collect())
            .unwrap_or_default();
        match flatpak_catalog_cache().try_lock() {
            Ok(catalog) => match catalog.as_ref() {
                Some((_, apps)) if !apps.is_empty() => {
                    let matches = fuzzy_apps(query, apps);
                    let mut seen = std::collections::HashSet::new();
                    flatpak_results.extend(
                        matches
                            .iter()
                            .filter(|(app, _)| seen.insert(app.name.to_lowercase()))
                            .filter(|(app, _)| !installed.contains(&app.app_id))
                            .map(|(app, _)| install_result(app)),
                    );
                }
                _ => loading = true,
            },
            Err(_) => loading = true,
        }
    }

    if use_distro {
        let pm_name = detected.as_deref().unwrap_or("");
        ensure_distro_catalog(pm_name.to_string());
        // Warm the installed-package list so we don't offer "Install: x" for
        // something you already have (Flatpak and Snap already filter this).
        ensure_distro_installed(pm_name.to_string());
        let installed = distro_installed_names();
        match distro_catalog_cache().try_lock() {
            Ok(cache) => match cache.as_ref() {
                Some((_, cpm, pkgs)) if cpm == pm_name && !pkgs.is_empty() => {
                    ensure_distro_search(query.to_string(), pkgs.clone());
                    let sc = distro_search_cache().lock().unwrap();
                    match sc.as_ref() {
                        Some((q, matches)) if q == query => {
                            distro_results.extend(
                                matches
                                    .iter()
                                    .filter(|p| !installed.contains(&p.name_lc))
                                    .map(|p| distro_install_result(p, pm_name)),
                            );
                        }
                        _ => {
                            if flatpak_results.is_empty() {
                                loading = true;
                            }
                        }
                    }
                }
                _ => {
                    loading = true;
                }
            },
            Err(_) => {
                loading = true;
            }
        }
    }

    if use_snap {
        ensure_snap_search(query.to_string());
        let sc = snap_search_cache().lock().unwrap();
        match sc.as_ref() {
            Some((q, res)) if q == query => {
                snap_results.extend(res.iter().cloned());
            }
            _ => {
                if flatpak_results.is_empty() && distro_results.is_empty() {
                    loading = true;
                }
            }
        }
    }

    if flatpak_results.is_empty() && distro_results.is_empty() && snap_results.is_empty() && loading {
        return searching_placeholder(&gettext("Searching for \"{query}\"…").replace("{query}", query));
    }

    if flatpak_results.is_empty() && distro_results.is_empty() && snap_results.is_empty() {
        return vec![SearchResult {
            kind: ResultKind::System,
            title: gettext("No apps found for \"{query}\"").replace("{query}", query),
            subtitle: Some(gettext("Check spelling or try a different name").into()),
            icon: Some("package-x-generic-symbolic".into()),
            action: Action::EnterMode("cmd".into()),
            score: 0,
        }];
    }

    // In combined modes, interleave the active sources (each already ordered
    // by relevance) so the best matches from EACH package manager appear at
    // the top — e.g. searching "firefox" surfaces the Flatpak, distro, and
    // Snap builds of Firefox as the first results — instead of listing every
    // match from one source before any from another.
    let sources = [use_flatpak, use_distro, use_snap];
    let mut results: Vec<SearchResult> = Vec::new();
    if sources.iter().filter(|&&s| s).count() > 1 {
        let mut fi = flatpak_results.into_iter();
        let mut di = distro_results.into_iter();
        let mut si = snap_results.into_iter();
        loop {
            let f = fi.next();
            let d = di.next();
            let s = si.next();
            if f.is_none() && d.is_none() && s.is_none() {
                break;
            }
            if let Some(f) = f {
                results.push(f);
            }
            if let Some(d) = d {
                results.push(d);
            }
            if let Some(s) = s {
                results.push(s);
            }
        }
    } else {
        results.extend(flatpak_results);
        results.extend(distro_results);
        results.extend(snap_results);
    }

    results.truncate(20);
    results
}

/// Install suggestions for the default (universal) search.
///
/// Reuses the catalog-backed `search_install`, but keeps only real install
/// actions — dropping the "Searching…"/"No apps found" placeholder rows that
/// would be noise outside the dedicated App mode — caps the count, and re-scores
/// them to sit just below locally-installed apps (which score ≥1500) yet above
/// web (100). Returns empty while catalogs are still warming or nothing matches.
pub fn universal_install(query: &str, sources: AppSources, limit: usize) -> Vec<SearchResult> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<SearchResult> = search_install(q, sources)
        .into_iter()
        .filter(|r| matches!(r.action, Action::StartOperation { .. }))
        .take(limit)
        .collect();
    for (i, r) in out.iter_mut().enumerate() {
        r.score = 800 - i as i32 * 10;
    }
    out
}

// Search for installed apps to uninstall across the package sources.
// Unlike installing, the package managers always both get consulted — the
// user can remove anything actually installed — but AppImage removal
// respects its source switch like the launch rows do (it's a search
// feature, not a system inventory).
fn search_uninstall(query: &str, appimage: bool) -> Vec<SearchResult> {
    let detected = get_detected_pm();
    let use_flatpak = true;
    let use_distro = detected.is_some();
    let use_snap = snap_is_available() == Some(true);

    let mut results: Vec<SearchResult> = Vec::new();
    let mut loading = false;

    if use_flatpak {
        ensure_installed();
        let cache = installed_cache().lock().unwrap();
        match cache.as_ref() {
            None => {
                loading = true;
            }
            Some((_, apps)) => {
                if query.is_empty() {
                    results.extend(apps.iter().take(8).map(|app| uninstall_result(app)));
                } else {
                    let matches = fuzzy_apps(query, apps);
                    results.extend(matches.iter().map(|(app, _)| uninstall_result(app)));
                }
            }
        }
    }

    if use_distro {
        let pm_name = detected.as_deref().unwrap_or("");
        ensure_distro_installed(pm_name.to_string());
        let cache = distro_installed_cache().lock().unwrap();
        match cache.as_ref() {
            None => {
                loading = true;
            }
            Some((_, pkgs)) => {
                if query.is_empty() {
                    results.extend(
                        pkgs.iter()
                            .take(4)
                            .map(|pkg| distro_uninstall_result(pkg, pm_name)),
                    );
                } else {
                    let matches = fuzzy_distro(query, pkgs);
                    results.extend(
                        matches
                            .iter()
                            .map(|(pkg, _)| distro_uninstall_result(pkg, pm_name)),
                    );
                }
            }
        }
    }

    if use_snap {
        ensure_snap_installed();
        let cache = snap_installed_cache().lock().unwrap();
        match cache.as_ref() {
            None => {
                loading = true;
            }
            Some((_, pkgs)) => {
                if query.is_empty() {
                    results.extend(pkgs.iter().take(4).map(snap_uninstall_result));
                } else {
                    let matches = fuzzy_distro(query, pkgs);
                    results.extend(
                        matches.iter().map(|(pkg, _)| snap_uninstall_result(pkg)),
                    );
                }
            }
        }
    }

    if appimage {
        if crate::search::appimage::discovery_pending() {
            loading = true;
        }
        results.extend(crate::search::appimage::remove_rows(query));
    }

    if results.is_empty() && loading {
        return searching_placeholder(&gettext("Scanning installed apps…"));
    }

    if results.is_empty() {
        return vec![SearchResult {
            kind: ResultKind::System,
            title: gettext("No installed app matching \"{query}\"").replace("{query}", query),
            subtitle: Some(gettext("Check spelling or try fewer characters").into()),
            icon: Some("edit-delete-symbolic".into()),
            action: Action::EnterMode("cmd".into()),
            score: 0,
        }];
    }

    results.sort_by(|a, b| b.score.cmp(&a.score));
    results.truncate(20);
    results
}

// ── Software update search ────────────────────────────────────────────────────

pub fn update_results(query: &str, config: &Config) -> Vec<SearchResult> {
    search_updates(query, config)
}

fn search_updates(query: &str, config: &Config) -> Vec<SearchResult> {
    // Master switch off: the verb only offers to turn the feature back on
    // (the badge and the background checks are gated elsewhere).
    if !config.result_enabled("updates") {
        return vec![update_toggle_row(config)];
    }
    ensure_updates_checked();
    // Clone updates out of the cache so the lock is released before the
    // row builders touch it (std::sync::Mutex is not reentrant — holding
    // the lock across those calls deadlocks).
    let updates: Option<Vec<UpdateInfo>> = update_cache()
        .try_lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|(_, v)| v);
    let mut out: Vec<SearchResult> = Vec::new();
    let rest = query.trim();
    // The restart row sits on top whenever the probe confirms a reboot is
    // pending — including right after a finished update asked for one. It is
    // never offered just because updates exist: those are installed first (see
    // the scope/package rows below), and only a finished run can put the
    // machine in the state that needs a restart. Targeted subqueries
    // ("update firefox") keep their own rows first.
    if reboot_pending() {
        out.push(restart_required_row());
    }
    // A downloaded-but-unarmed offline transaction: a restart alone wouldn't
    // install it, so offer to arm it — through the daemon, the only door.
    if offline_update_staged() && system_door().is_some() {
        out.push(staged_update_now_row());
    }
    let rl = rest.to_lowercase();
    if *update_fetching().lock().unwrap() || updates.is_none() {
        out.push(check_now_row(
            gettext("Checking for updates…"),
            gettext("Press Enter to check again"),
            "emblem-synchronizing-symbolic",
        ));
    } else {
        {
            let list: &[UpdateInfo] = updates.as_deref().unwrap_or_default();
            // 1. The scoped targets — the obvious things to type after
            //    "update " ("update all", "update flatpak", "update system
            //    packages", "update snap") — always offered, so they read
            //    as recommendations even with nothing pending (idle rows
            //    re-check on Enter instead of firing an empty run).
            let scopes = scopes_from(list);
            let mut matched = false;
            let mut flatpak_scope = false;
            let mut distro_scope = false;
            let mut snap_scope = false;
            let mut appimage_scope = false;
            for sc in &scopes {
                if rest.is_empty() || sc.matches(&rl) {
                    out.push(sc.row());
                    matched = true;
                    match sc.key {
                        "flatpak" => flatpak_scope = true,
                        "distro" => distro_scope = true,
                        "snap" => snap_scope = true,
                        "appimage" => appimage_scope = true,
                        _ => {} // "all" already covers every source
                    }
                }
            }
            // 2. One row per pending package ("update firefox"). Picking a
            //    source scope also lists its packages, so a single one can
            //    still be chosen ("update flatpak" -> Firefox…).
            let in_scope = |u: &UpdateInfo| match u.source.as_str() {
                "flatpak" => flatpak_scope,
                "snap" => snap_scope,
                "appimage" => appimage_scope,
                _ => distro_scope,
            };
            let mut n = 0;
            for u in list {
                if n >= 10 {
                    break;
                }
                if rest.is_empty()
                    || in_scope(u)
                    || u.name.to_lowercase().contains(&rl)
                    || package_display(u).to_lowercase().contains(&rl)
                    || crate::search::fuzzy_match(&rl, &package_display(u))
                {
                    out.push(package_row(u));
                    n += 1;
                    matched = true;
                }
            }
            if !matched {
                out.push(SearchResult {
                    kind: ResultKind::System,
                    title: gettext("No matching updates").into(),
                    subtitle: Some(
                        gettext("Type all, flatpak, distro or a package name").into(),
                    ),
                    icon: Some("edit-find-symbolic".into()),
                    action: Action::Noop,
                    score: 1000,
                });
            }
        }
    }
    // Always offer the switch, from search as well as from Settings.
    out.push(update_toggle_row(config));
    // Notice controls live in the list as well: options only appear once
    // the user types "update".
    if let Some(list) = &updates {
        if !list.is_empty() {
            let sig = update_signature(list);
            out.push(SearchResult {
                kind: ResultKind::System,
                title: gettext("Remind tomorrow").into(),
                subtitle: Some(gettext("Hide the update notice for 24 hours").into()),
                icon: Some("document-open-recent-symbolic".into()),
                action: Action::SnoozeUpdates,
                score: 400,
            });
            out.push(SearchResult {
                kind: ResultKind::System,
                title: gettext("Dismiss update notice").into(),
                subtitle: Some(gettext("Hidden until a new update appears").into()),
                icon: Some("window-close-symbolic".into()),
                action: Action::DismissUpdates(sig),
                score: 300,
            });
        }
    }
    out
}

/// Row that owns the restart decision, on top of the update list: a reboot is
/// pending, so offer it — and nothing else.
///
/// Updates are a deliberately separate action: an update run always completes
/// first, and only once `operations::finish`'s post-update probe confirms that a
/// reboot is needed does this row appear. It never chains an update into the
/// reboot, which is what used to reboot the machine mid-update. The system
/// session rows share it, so a pending reboot replaces the plain "Restart" there.
pub(crate) fn restart_required_row() -> SearchResult {
    restart_required_row_with(offline_update_armed())
}

/// The two things a pending reboot can mean, in words.
///
/// An *armed* update (the daemon wrote `/system-update`) is installed by this
/// restart; an already-*installed* update is only started being used by it.
/// The row, the settings group and the badge all say the same thing, so they
/// share this — a promise the restart can't keep must not be made anywhere.
pub(crate) fn reboot_notice_text(armed: bool) -> (String, String) {
    if armed {
        (
            gettext("Restart to install the updates"),
            gettext("Downloaded and ready — they install with the restart"),
        )
    } else {
        (
            gettext("Restart required to finish the update"),
            gettext("Installed — restart to start using them"),
        )
    }
}

/// [`reboot_notice_text`] for the state the probe last confirmed.
pub fn reboot_notice() -> (String, String) {
    reboot_notice_text(offline_update_armed())
}

/// [`restart_required_row`] with "the pending reboot installs a downloaded
/// update" passed in — the caller decides, so the wording is unit-testable
/// without globals.
///
/// A downloaded-but-unarmed transaction has its own row
/// ([`staged_update_now_row`]) instead: restarting can't install that one.
pub(crate) fn restart_required_row_with(armed: bool) -> SearchResult {
    let (title, subtitle) = reboot_notice_text(armed);
    SearchResult {
        kind: ResultKind::System,
        title,
        subtitle: Some(subtitle),
        icon: Some("system-reboot-symbolic".into()),
        action: Action::ConfirmRunCommand(crate::search::system::reboot_command()),
        score: 100_000,
    }
}

/// Top row while updates sit downloaded but not yet armed: arm them.
///
/// This is what gnome-software's background download leaves behind, and a
/// restart alone would not install it — the service has to mark it for the
/// next boot first (dnf5daemon's `Offline.schedule_for_next_boot`, PackageKit's
/// `Offline.Trigger`: the store's own calls, no password of ours). Only
/// offered while a service is reachable.
fn staged_update_now_row() -> SearchResult {
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Update now"),
        subtitle: Some(gettext("Downloaded — restart to install them")),
        icon: Some("software-update-available-symbolic".into()),
        action: Action::StartOperation {
            title: gettext("Update all packages"),
            source: "System Update".into(),
            icon: "software-update-available-symbolic".into(),
            // Arm it with whichever service holds the download.
            args: if staged_via_packagekit() {
                crate::packagekit::schedule_args()
            } else {
                crate::dnf5daemon::schedule_args()
            },
        },
        score: 100_000,
    }
}

/// On/off switch for the whole update feature, reachable from search.
fn update_toggle_row(config: &Config) -> SearchResult {
    let on = config.enable_updates;
    SearchResult {
        kind: ResultKind::System,
        title: if on {
            gettext("Disable update checks")
        } else {
            gettext("Enable update checks")
        }
        .into(),
        subtitle: None,
        icon: Some(if on {
            "changes-prevent-symbolic"
        } else {
            "emblem-ok-symbolic"
        }
        .into()),
        action: Action::ToggleUpdates,
        score: 500,
    }
}

/// Split a parser's "name  version" line into (name, version).
fn split_pkg(raw: &str) -> (&str, &str) {
    match raw.find(char::is_whitespace) {
        Some(i) => (raw[..i].trim(), raw[i..].trim()),
        None => (raw, ""),
    }
}

/// The name a package row shows: flatpak keeps its display name, the
/// distro parsers carry "name  version" and the version goes to the
/// subtitle instead.
fn package_display(u: &UpdateInfo) -> String {
    if u.source == "flatpak" || u.source == "appimage" {
        // Display names, not "name version" package lines — no splitting.
        u.name.clone()
    } else {
        split_pkg(&u.name).0.to_string()
    }
}

/// Argv that installs distro updates through the system's own service:
/// dnf5daemon where it exists (Fedora), PackageKit everywhere else. Both are
/// the Software store's doors — no password of Spotty's own, and reachable from
/// the Flatpak sandbox over D-Bus. There is deliberately no other door: a
/// pkexec fallback would ask for a root password, and a sandboxed build
/// couldn't run it without escaping to the host.
fn distro_update_args(package: Option<&str>) -> Vec<String> {
    match system_door() {
        Some(Door::Dnf5Daemon) => match package {
            Some(p) => vec![
                crate::dnf5daemon::ARGV0.into(),
                crate::dnf5daemon::VERB_UPGRADE_PKG.into(),
                p.into(),
            ],
            None => crate::dnf5daemon::upgrade_all_args(),
        },
        Some(Door::PackageKit) => crate::packagekit::update_args(package),
        None => no_updates_cmd(),
    }
}

/// Args to update exactly one package of `source`.
fn package_args(source: &str, target: &str) -> Vec<String> {
    let t = target.to_string();
    match source {
        "flatpak" => flatpak_cmd_args(&["update", "--assumeyes", &t]),
        "dnf" | "apt" | "pacman" | "zypper" => distro_update_args(Some(&t)),
        "snap" => pkexec_cmd_args(vec!["snap".into(), "refresh".into(), t]),
        // `target` is the AppImage path (names may contain spaces — it's
        // passed as a single argv element, never split).
        "appimage" => crate::search::appimage::update_args(std::path::Path::new(&t)),
        _ => no_updates_cmd(),
    }
}

/// One row per pending package: "Update: firefox" — Enter asks once and
/// then upgrades exactly that package.
fn package_row(u: &UpdateInfo) -> SearchResult {
    let display = package_display(u);
    let (sub, icon, target) = if u.source == "flatpak" {
        let id = u.app_id.clone().unwrap_or_else(|| u.name.clone());
        (format!("flatpak — {id}"), id.clone(), id)
    } else if u.source == "appimage" {
        // `app_id` is the file path; no distro icon to borrow, so the
        // executable glyph stands in for the AppImage itself.
        let path = u.app_id.clone().unwrap_or_else(|| u.name.clone());
        let short = crate::search::appimage::short_path(std::path::Path::new(&path));
        (
            format!("appimage — {short}"),
            "application-x-executable-symbolic".to_string(),
            path,
        )
    } else {
        let (_, ver) = split_pkg(&u.name);
        let sub = if ver.is_empty() {
            u.source.clone()
        } else {
            format!("{} — {}", u.source, ver)
        };
        (
            sub,
            "software-update-available-symbolic".to_string(),
            display.clone(),
        )
    };
    let title = gettext("Update: {name}").replace("{name}", &display);
    remember_details(&title, u.details.clone());
    remember_query(&title, format!("update {display}"));
    let args = package_args(&u.source, &target);
    SearchResult {
        kind: ResultKind::System,
        title: title.clone(),
        subtitle: Some(sub),
        icon: Some(icon.clone()),
        action: Action::StartOperation {
            title,
            source: gettext("{source} Update").replace("{source}", &u.source),
            icon,
            args,
        },
        score: 1000,
    }
}

/// A target the user can type after the verb: "update all", "update
/// flatpak", "update system" (the detected PM), "update snap". The scopes
/// are always offered — the obvious targets after "update " — with the
/// subtitle telling whether that target has anything pending.
#[derive(Debug)]
struct Scope {
    /// What the user types: "all" | "flatpak" | "distro" | "snap".
    key: &'static str,
    /// Source name `update_cmd_args` understands ("distro" -> detected PM).
    cmd: String,
    /// Typing keywords that select this scope.
    kws: Vec<String>,
    /// The canonical typed target ("system" for the distro scope) — what
    /// ghost text completes the query to.
    typed: String,
    /// Row title: "Update all packages", "Update flatpak packages"...
    title: String,
    /// Pending packages in this scope (0 = idle, Enter re-checks).
    count: usize,
    /// Sources, shown in the subtitle while pending.
    sources: String,
    /// Package lines this scope would upgrade (the preview list).
    details: Vec<String>,
}

impl Scope {
    /// True when the typed target ("update f", "update sytem") picks this
    /// scope — prefix first, fuzzy for typos.
    fn matches(&self, rl: &str) -> bool {
        self.kws
            .iter()
            .any(|k| k.starts_with(rl) || crate::search::fuzzy_match(rl, k))
    }

    /// The result row for this scope.
    fn row(&self) -> SearchResult {
        let title = self.title.clone();
        remember_details(&title, self.details.clone());
        remember_query(&title, format!("update {}", self.typed));
        // Pending rows advertise the update; idle ones show the checkmark —
        // there is nothing to do for that target.
        let icon = if self.count > 0 {
            "software-update-available-symbolic"
        } else {
            "object-select-symbolic"
        };
        let (subtitle, action) = if self.count > 0 {
            (
                format!("{} — update available", self.sources),
                Action::StartOperation {
                    title: title.clone(),
                    source: gettext("{source} Update").replace("{source}", &self.cmd),
                    icon: icon.to_string(),
                    args: update_cmd_args(&self.cmd, None),
                },
            )
        } else {
            // Nothing pending for this target: the recommendation still
            // shows, and Enter re-checks instead of firing an empty run.
            let state = if self.key == "all" {
                gettext("No updates available")
            } else {
                gettext("Nothing pending")
            };
            (
                format!("{} · {}", state, gettext("Press Enter to check again")),
                Action::CheckUpdates,
            )
        };
        SearchResult {
            kind: ResultKind::System,
            title,
            subtitle: Some(subtitle),
            icon: Some(icon.to_string()),
            action,
            score: match self.key {
                "all" => 2000,
                "flatpak" => 1900,
                "distro" => 1850,
                "appimage" => 1760,
                _ => 1800,
            },
        }
    }
}

/// (package count, package lines, sources) for every entry whose source
/// passes `pred`.
fn scope_group(
    list: &[UpdateInfo],
    pred: impl Fn(&str) -> bool,
) -> (usize, Vec<String>, Vec<String>) {
    let mut details = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    for u in list.iter().filter(|u| pred(&u.source)) {
        details.extend(u.details.iter().cloned());
        if !sources.contains(&u.source) {
            sources.push(u.source.clone());
        }
    }
    (details.len(), details, sources)
}

/// The scopes offered for `list`, in typing order: all, flatpak, system,
/// snap — always shown (pending or not) so they read as recommendations.
fn scopes_from(list: &[UpdateInfo]) -> Vec<Scope> {
    ensure_snap_available();
    let mut out = Vec::new();
    let pm = list
        .iter()
        .map(|u| u.source.as_str())
        .find(|s| is_distro_source(s))
        .map(str::to_string);

    let (count, details, sources) = scope_group(list, |_| true);
    out.push(Scope {
        key: "all",
        cmd: "all".into(),
        kws: vec!["all".into(), "everything".into()],
        typed: "all".into(),
        title: gettext("Update all packages"),
        count,
        sources: sources.join(", "),
        details,
    });

    let (count, details, sources) = scope_group(list, |s| s == "flatpak");
    out.push(Scope {
        key: "flatpak",
        cmd: "flatpak".into(),
        kws: vec!["flatpak".into(), "flathub".into()],
        typed: "flatpak".into(),
        title: gettext("Update flatpak packages"),
        count,
        sources: sources.join(", "),
        details,
    });

    let (count, details, sources) = scope_group(list, |s| is_distro_source(s));
    let mut kws = vec![
        "distro".into(),
        "distribution".into(),
        "system".into(),
        "sys".into(),
    ];
    if let Some(pm) = &pm {
        kws.push(pm.clone());
    }
    // Only where a service can apply them (see `distro_update_args`).
    if count > 0 || system_door().is_some() {
        out.push(Scope {
            key: "distro",
            cmd: pm.unwrap_or_default(),
            kws,
            typed: "system".into(),
            title: gettext("Update system packages"),
            count,
            sources: sources.join(", "),
            details,
        });
    }

    let (count, details, sources) = scope_group(list, |s| s == "snap");
    if count > 0 || snap_is_available() == Some(true) {
        out.push(Scope {
            key: "snap",
            cmd: "snap".into(),
            kws: vec!["snap".into(), "snapd".into()],
            typed: "snap".into(),
            title: gettext("Update snap packages"),
            count,
            sources: sources.join(", "),
            details,
        });
    }

    let (count, details, sources) = scope_group(list, |s| s == "appimage");
    if count > 0 || crate::search::appimage::is_supported() == Some(true) {
        out.push(Scope {
            key: "appimage",
            cmd: "appimage".into(),
            kws: vec!["appimage".into(), "appimages".into()],
            typed: "appimage".into(),
            title: gettext("Update AppImages"),
            count,
            sources: sources.join(", "),
            details,
        });
    }
    out
}

/// The "Checking for updates..." row: Enter re-runs the check.
fn check_now_row(title: String, subtitle: String, icon: &str) -> SearchResult {
    SearchResult {
        kind: ResultKind::System,
        title,
        subtitle: Some(subtitle),
        icon: Some(icon.to_string()),
        action: Action::CheckUpdates,
        score: 1000,
    }
}

// ── Result builders ───────────────────────────────────────────────────────────

// What to kill: a regular host process (matched by name via pkill), or a
// running Flatpak app instance (killed by app-id via `flatpak kill`).
#[derive(Clone)]
enum KillTarget {
    Process(String),
    Flatpak(String),
}

// Cross-reference running processes and Flatpak instances against indexed
// apps, returning (app display name, kill target) pairs for apps currently
// running. Flatpak matches take priority since `flatpak kill` is more
// reliable than `pkill` for sandboxed apps (whose host process name may not
// match the app's display name).
fn running_apps(
    proc_names: &[String],
    flatpak_ids: &[String],
    apps: &[AppEntry],
) -> Vec<(String, KillTarget)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for app in apps {
        for id in flatpak_ids {
            let il = id.to_lowercase();
            if app.keywords.iter().any(|k| k.to_lowercase() == il) && seen.insert(app.name.clone())
            {
                out.push((app.name.clone(), KillTarget::Flatpak(id.clone())));
                break;
            }
        }
    }
    for proc in proc_names {
        let pl = proc.to_lowercase();
        for app in apps {
            let nl = app.name.to_lowercase();
            let nl_compact = nl.replace(' ', "");
            let matches =
                nl == pl || nl_compact == pl || app.keywords.iter().any(|k| k.to_lowercase() == pl);
            if matches && seen.insert(app.name.clone()) {
                out.push((app.name.clone(), KillTarget::Process(proc.clone())));
                break;
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn kill_result(name: &str, target: &KillTarget) -> SearchResult {
    let subtitle = match target {
        KillTarget::Process(proc) => format!("pkill -i {}", proc),
        KillTarget::Flatpak(id) => format!("flatpak kill {}", id),
    };
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Kill: {name}").replace("{name}", &name.to_string()),
        subtitle: Some(subtitle),
        icon: Some("process-stop-symbolic".into()),
        action: kill_action(target),
        score: 1000,
    }
}

fn kill_action(target: &KillTarget) -> Action {
    let cmd = match target {
        KillTarget::Process(proc) => {
            let safe = shell_safe(proc);
            if is_sandbox() {
                format!("flatpak-spawn --host pkill -i {}", safe)
            } else {
                format!("pkill -i {}", safe)
            }
        }
        KillTarget::Flatpak(id) => {
            let safe = shell_safe(id);
            if is_sandbox() {
                format!("flatpak-spawn --host flatpak kill {}", safe)
            } else {
                format!("flatpak kill {}", safe)
            }
        }
    };
    Action::RunCommand(cmd)
}

fn install_result(app: &FlatpakApp) -> SearchResult {
    let sub = if app.description.is_empty() {
        gettext("via Flatpak ({app})").replace("{app}", &app.app_id)
    } else {
        gettext("{description} — via Flatpak ({app})").replace("{description}", &app.description).replace("{app}", &app.app_id)
    };
    let args = {
        let mut a = flatpak_cmd_args(&["install", "--user", "--assumeyes"]);
        a.push(app.app_id.clone());
        a
    };
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Install: {name}").replace("{name}", &app.name),
        subtitle: Some(sub),
        // The app-id doubles as an icon name so the row shows the real app icon
        // when available (result_row falls back to a package icon otherwise).
        icon: Some(app.app_id.clone()),
        action: Action::StartOperation {
            title: gettext("Installing {name}").replace("{name}", &app.name),
            source: "Flatpak".into(),
            icon: app.app_id.clone(),
            args,
        },
        score: 1000,
    }
}

fn uninstall_result(app: &FlatpakApp) -> SearchResult {
    let args = {
        let mut a = flatpak_cmd_args(&["uninstall", "--assumeyes"]);
        a.push(app.app_id.clone());
        a
    };
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Uninstall: {name}").replace("{name}", &app.name),
        subtitle: Some(gettext("via Flatpak ({app})").replace("{app}", &app.app_id)),
        icon: Some(app.app_id.clone()),
        action: Action::StartOperation {
            title: gettext("Uninstalling {name}").replace("{name}", &app.name),
            source: "Flatpak".into(),
            icon: app.app_id.clone(),
            args,
        },
        score: 1000,
    }
}

// Build argv for `subargs` run as root via `pkexec`, prefixed with
// `flatpak-spawn --host` when sandboxed. `pkexec` shows a PolicyKit GUI prompt
// asking the user for their password before running the command.
fn pkexec_cmd_args(subargs: Vec<String>) -> Vec<String> {
    let mut v = if is_sandbox() {
        vec![
            "flatpak-spawn".to_string(),
            "--host".to_string(),
            "pkexec".to_string(),
        ]
    } else {
        vec!["pkexec".to_string()]
    };
    v.extend(subargs);
    v
}

fn distro_install_result(pkg: &DistroPackage, pm: &str) -> SearchResult {
    // Installs go through PackageKit — the Software store's door: polkit, not
    // a root password prompt of our own, and reachable from the Flatpak sandbox.
    let label = match pm {
        "apt" => "apt",
        "dnf" => "dnf",
        "pacman" => "pacman",
        "zypper" => "zypper",
        _ => {
            return SearchResult {
                kind: ResultKind::System,
                title: gettext("Install: {name}").replace("{name}", &pkg.name),
                subtitle: None,
                icon: Some("package-x-generic-symbolic".into()),
                action: Action::EnterMode("cmd".into()),
                score: 900,
            }
        }
    };
    let sub = if pkg.description.is_empty() {
        format!("via {}", label)
    } else {
        format!("{} — via {}", pkg.description, label)
    };
    // "pkg:<name>:<fallback>" — result_row tries to resolve a real app icon
    // matching the package name (theme + host icon dirs), falling back to
    // a generic package icon if none is found.
    let icon = format!("pkg:{}:package-x-generic-symbolic", pkg.name);
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Install: {name}").replace("{name}", &pkg.name),
        subtitle: Some(sub),
        icon: Some(icon.clone()),
        action: Action::StartOperation {
            title: format!("Installing {}", pkg.name),
            source: label.into(),
            icon,
            args: crate::packagekit::install_args(&pkg.name),
        },
        score: 900,
    }
}

fn distro_uninstall_result(pkg: &DistroPackage, pm: &str) -> SearchResult {
    let (inner, label): (Vec<String>, &str) = match pm {
        "apt" => (
            vec![
                "apt-get".into(),
                "remove".into(),
                "-y".into(),
                pkg.name.clone(),
            ],
            "apt",
        ),
        "dnf" => (
            vec!["dnf".into(), "remove".into(), "-y".into(), pkg.name.clone()],
            "dnf",
        ),
        "pacman" => (
            vec![
                "pacman".into(),
                "-R".into(),
                "--noconfirm".into(),
                pkg.name.clone(),
            ],
            "pacman",
        ),
        "zypper" => (
            vec![
                "zypper".into(),
                "--non-interactive".into(),
                "remove".into(),
                pkg.name.clone(),
            ],
            "zypper",
        ),
        _ => {
            return SearchResult {
                kind: ResultKind::System,
                title: format!("Uninstall: {}", pkg.name),
                subtitle: None,
                icon: Some("edit-delete-symbolic".into()),
                action: Action::EnterMode("cmd".into()),
                score: 900,
            }
        }
    };
    let sub = if pkg.description.is_empty() {
        format!("via {}", label)
    } else {
        format!("{} — via {}", pkg.description, label)
    };
    let icon = format!("pkg:{}:edit-delete-symbolic", pkg.name);
    SearchResult {
        kind: ResultKind::System,
        title: format!("Uninstall: {}", pkg.name),
        subtitle: Some(sub),
        icon: Some(icon.clone()),
        action: Action::StartOperation {
            title: format!("Uninstalling {}", pkg.name),
            source: label.into(),
            icon,
            args: pkexec_cmd_args(inner),
        },
        score: 900,
    }
}

fn snap_install_result(pkg: DistroPackage) -> SearchResult {
    let sub = if pkg.description.is_empty() {
        "via Snap".into()
    } else {
        format!("{} — via snap", pkg.description)
    };
    let icon = format!("pkg:{}:package-x-generic-symbolic", pkg.name);
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Install: {name}").replace("{name}", &pkg.name),
        subtitle: Some(sub),
        icon: Some(icon.clone()),
        action: Action::StartOperation {
            title: format!("Installing {}", pkg.name),
            source: "snap".into(),
            icon,
            args: pkexec_cmd_args(vec![
                "snap".into(),
                "install".into(),
                pkg.name.clone(),
            ]),
        },
        score: 900,
    }
}

fn snap_uninstall_result(pkg: &DistroPackage) -> SearchResult {
    let icon = format!("pkg:{}:edit-delete-symbolic", pkg.name);
    SearchResult {
        kind: ResultKind::System,
        title: format!("Uninstall: {}", pkg.name),
        subtitle: Some(gettext("via snap").into()),
        icon: Some(icon.clone()),
        action: Action::StartOperation {
            title: format!("Uninstalling {}", pkg.name),
            source: "snap".into(),
            icon,
            args: pkexec_cmd_args(vec!["snap".into(), "remove".into(), pkg.name.clone()]),
        },
        score: 900,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn matches_verb(verb: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|c| {
        *c == verb
            || c.starts_with(verb)
            || verb.starts_with(c)
            || crate::search::fuzzy_match(verb, c)
    })
}

fn shell_safe(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::chained_script;

    /// Fill the cache so `update_verb_rows` doesn't kick off a live
    /// dnf/flatpak check inside the test.
    fn prime_update_cache() {
        let mut c = super::update_cache().lock().unwrap();
        if c.is_none() {
            *c = Some((std::time::Instant::now(), Vec::new()));
        }
    }

    #[test]
    fn update_verb_is_recognised_in_plain_search() {
        let _g = cache_guard();
        prime_update_cache();
        let cfg = crate::config::Config::default();
        for q in ["update", "updates", "upd", "upgrade", "upg", "update flatpak"] {
            assert!(super::update_verb_rows(q, &cfg).is_some(), "should match {q:?}");
        }
        for q in ["", "install firefox", "daily", "sup", "updateing", "u"] {
            assert!(super::update_verb_rows(q, &cfg).is_none(), "should NOT match {q:?}");
        }
        // A still-incomplete verb shows the very same rows as the complete
        // verb — one update row in every state, no separate suggestion row.
        let titles_of = |q: &str| -> Vec<String> {
            super::update_verb_rows(q, &cfg)
                .expect("rows")
                .into_iter()
                .map(|r| r.title)
                .collect()
        };
        let full = titles_of("update");
        for q in ["up", "upda", "updat", "upgrad"] {
            assert_eq!(
                titles_of(q),
                full,
                "{q:?} must show the same rows as \"update\""
            );
        }
    }

    #[test]
    fn verb_completion_only_completes_incomplete_verbs() {
        assert_eq!(super::verb_completion("up").as_deref(), Some("update "));
        assert_eq!(super::verb_completion("upd").as_deref(), Some("update "));
        assert_eq!(super::verb_completion("updat").as_deref(), Some("update "));
        assert_eq!(super::verb_completion("upg").as_deref(), Some("upgrade "));
        assert_eq!(super::verb_completion("upgrad").as_deref(), Some("upgrade "));
        assert_eq!(super::verb_completion("update"), None);
        assert_eq!(super::verb_completion("updates"), None);
        assert_eq!(super::verb_completion("upgrade"), None);
        assert_eq!(super::verb_completion("update f"), None);
        // Below two characters other words are at play ("u" → uninstall).
        assert_eq!(super::verb_completion("u"), None);
        assert_eq!(super::verb_completion(""), None);
    }

    #[test]
    fn one_cache_entry_per_package_and_scopes_on_top() {
        // The cache holds one entry per *real* package: aggregates are
        // built for display only, so they can never inflate the
        // "N updates available" badge.
        let solo = super::assemble_updates(
            vec![],
            ("dnf", vec!["vim.x86_64  2:9.2.1129-1.fc44".to_string()]),
            vec![],
            vec![],
        );
        assert_eq!(solo.len(), 1, "solo list: {solo:?}");
        assert_eq!(solo[0].source, "dnf");
        // The entry carries its package line for the preview.
        assert_eq!(solo[0].details, vec!["vim.x86_64  2:9.2.1129-1.fc44"]);

        // Two sources: still two entries, and the derived scopes cover
        // every package exactly once.
        let duo = super::assemble_updates(
            vec![],
            ("dnf", vec!["vim.x86_64  9.2".to_string()]),
            vec!["core22  2024".to_string()],
            vec![],
        );
        assert_eq!(duo.len(), 2, "dnf + snap: {duo:?}");
        let scopes = super::scopes_from(&duo);
        let keys: Vec<&str> = scopes.iter().map(|s| s.key).collect();
        // The recommendations are always offered, in typing order.
        assert_eq!(keys[..3], ["all", "flatpak", "distro"], "{scopes:?}");
        assert_eq!(scopes[0].details.len(), 2, "all scope merges both");
        assert_eq!(scopes[2].cmd, "dnf", "system scope runs the detected PM");
        assert!(scopes[0].matches("a"));
        assert!(scopes[1].matches("f") && scopes[1].matches("flatpak"));
        assert!(scopes[2].matches("d") && scopes[2].matches("system"));
        // …even for a source with nothing pending.
        assert_eq!(scopes[1].count, 0, "flatpak idle but still shown");
        assert_eq!(scopes[2].count, 1, "one distro package pending");
    }

    #[test]
    fn typoed_update_search_still_finds_everything() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![
                    super::UpdateInfo {
                        source: "flatpak".into(),
                        app_id: Some("org.mozilla.firefox".into()),
                        name: "Firefox".into(),
                        details: vec!["Firefox".into()],
                    },
                    super::UpdateInfo {
                        source: "dnf".into(),
                        app_id: None,
                        name: "vim.x86_64  2:9.2.1129-1.fc44".into(),
                        details: vec!["vim.x86_64  2:9.2.1129-1.fc44".into()],
                    },
                ],
            ));
        }
        let cfg = crate::config::Config::default();

        // A typo'd verb still opens the list — only two edits away.
        assert!(super::update_verb_rows("updte", &cfg).is_some(), "typo verb");
        assert!(super::update_verb_rows("Updte flatpak", &cfg).is_some(), "typo verb + target");
        // …unrelated words stay out.
        assert!(super::update_verb_rows("upload", &cfg).is_none(), "upload");
        assert!(super::update_verb_rows("updateing", &cfg).is_none(), "updateing");

        // Typo'd targets pick their scope and package.
        for (q, want) in [
            ("sytem", "Update system packages"),
            ("flk", "Update flatpak packages"),
            ("aall", "Update all packages"),
        ] {
            let rows = super::update_results(q, &cfg);
            assert!(
                rows.iter().any(|r| r.title == want),
                "{q:?} should match {want:?}: {:?}",
                rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
            );
        }
        let rows = super::update_results("firfox", &cfg);
        assert!(
            rows.iter().any(|r| r.title == "Update: Firefox"),
            "firfox should match the package: {:?}",
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn update_rows_offer_scopes_packages_and_ghost_queries() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![
                    super::UpdateInfo {
                        source: "flatpak".into(),
                        app_id: Some("org.mozilla.firefox".into()),
                        name: "Firefox".into(),
                        details: vec!["Firefox".into()],
                    },
                    super::UpdateInfo {
                        source: "dnf".into(),
                        app_id: None,
                        name: "vim.x86_64  2:9.2.1129-1.fc44".into(),
                        details: vec!["vim.x86_64  2:9.2.1129-1.fc44".into()],
                    },
                ],
            ));
        }
        let cfg = crate::config::Config::default();

        // "update" → scopes first, then one row per pending package.
        let rows = super::update_results("", &cfg);
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"Update all packages"), "{titles:?}");
        assert!(titles.contains(&"Update flatpak packages"), "{titles:?}");
        assert!(titles.contains(&"Update system packages"), "{titles:?}");
        assert!(titles.contains(&"Update: Firefox"), "{titles:?}");
        assert!(titles.contains(&"Update: vim.x86_64"), "{titles:?}");
        let all_i = titles.iter().position(|t| *t == "Update all packages").unwrap();
        let pkg_i = titles.iter().position(|t| *t == "Update: Firefox").unwrap();
        assert!(all_i < pkg_i, "scope rows rank above packages: {titles:?}");

        // The ghost suggestions each row completes to.
        assert_eq!(super::query_for("Update all packages").as_deref(), Some("update all"));
        assert_eq!(
            super::query_for("Update flatpak packages").as_deref(),
            Some("update flatpak")
        );
        assert_eq!(
            super::query_for("Update system packages").as_deref(),
            Some("update system")
        );
        assert_eq!(super::query_for("Update: Firefox").as_deref(), Some("update Firefox"));
        assert_eq!(super::query_for("no such row"), None);

        // "update flatpak" → the flatpak scope + its package, nothing system.
        let rows = super::update_results("flatpak", &cfg);
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"Update flatpak packages"), "{titles:?}");
        assert!(titles.contains(&"Update: Firefox"), "{titles:?}");
        assert!(!titles.contains(&"Update system packages"), "{titles:?}");

        // "update d|di|distro|dnf|system" all pick the system scope.
        for q in ["d", "di", "distro", "dnf", "system"] {
            let rows = super::update_results(q, &cfg);
            assert!(
                rows.iter().any(|r| r.title == "Update system packages"),
                "{q:?} should select the system scope: {:?}",
                rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
            );
        }

        // "update fire" → the package row itself.
        let rows = super::update_results("fire", &cfg);
        assert_eq!(rows[0].title, "Update: Firefox");
        // …and an unknown target says so instead of showing nothing.
        let rows = super::update_results("zzz", &cfg);
        assert!(rows.iter().any(|r| r.title.contains("No matching updates")), "{rows:?}");
    }

    #[test]
    fn idle_scopes_stay_visible_and_recheck_on_enter() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((std::time::Instant::now(), Vec::new()));
        }
        let cfg = crate::config::Config::default();
        let rows = super::update_results("", &cfg);
        // The three recommendations show even with nothing pending, and
        // Enter re-checks instead of firing an empty run.
        for (title, state) in [
            ("Update all packages", crate::i18n::gettext("No updates available")),
            ("Update flatpak packages", crate::i18n::gettext("Nothing pending")),
            ("Update system packages", crate::i18n::gettext("Nothing pending")),
        ] {
            let row = rows
                .iter()
                .find(|r| r.title == title)
                .unwrap_or_else(|| panic!("{title} missing: {:?}", rows.iter().map(|r| &r.title).collect::<Vec<_>>()));
            assert!(
                matches!(row.action, crate::search::Action::CheckUpdates),
                "{title}: {:?}",
                row.action
            );
            let sub = row.subtitle.as_deref().unwrap_or_default();
            assert!(sub.contains(&state), "{title}: {sub}");
            assert!(sub.contains("Press Enter to check again"), "{title}: {sub}");
            // …and still complete their typed command for ghost text.
            assert!(super::query_for(&row.title).is_some(), "{title}");
            // Nothing to update → the checkmark, not the update glyph.
            assert_eq!(row.icon.as_deref(), Some("object-select-symbolic"), "{title}");
        }
    }

    #[test]
    fn package_rows_build_per_source_commands() {
        // A distro package goes to this system's service (an in-process D-Bus
        // task, no sudo) or nowhere — there is no pkexec fallback. Which
        // service is the system's call, not the package manager label's.
        let joined = |v: Vec<String>| v.join(" ");
        let expected = match super::system_door() {
            Some(super::Door::Dnf5Daemon) => vec![
                crate::dnf5daemon::ARGV0.to_string(),
                crate::dnf5daemon::VERB_UPGRADE_PKG.to_string(),
                "vim.x86_64".to_string(),
            ],
            Some(super::Door::PackageKit) => crate::packagekit::update_args(Some("vim.x86_64")),
            None => super::no_updates_cmd(),
        };
        for pm in ["dnf", "apt", "pacman", "zypper"] {
            let args = super::package_args(pm, "vim.x86_64");
            assert_eq!(args, expected, "{pm}");
            // No pkexec anywhere in the store's path — that is the point.
            assert!(!joined(args).contains("pkexec"), "{pm}");
        }
        // Updating everything goes through the same door.
        assert!(!joined(super::update_cmd_args("dnf", None)).contains("pkexec"));
        let fp = super::package_args("flatpak", "org.mozilla.firefox");
        assert!(fp.contains(&"org.mozilla.firefox".to_string()), "{fp:?}");
        assert!(fp.contains(&"update".to_string()), "{fp:?}");
        let snap = super::package_args("snap", "core22");
        assert!(snap.contains(&"refresh".to_string()), "{snap:?}");

        // The daemon task is an update run too, though its argv is our own.
        assert!(super::is_update_run(
            "Update all packages",
            &crate::dnf5daemon::upgrade_all_args()
        ));
    }

    #[test]
    fn distro_installs_go_through_packagekit_never_pkexec() {
        // One install path for every distro: a PackageKit task naming the
        // package. A root password prompt of our own is not an alternative.
        for pm in ["apt", "dnf", "pacman", "zypper"] {
            let pkg = super::DistroPackage::new("htop".into(), "Interactive process viewer".into());
            let row = super::distro_install_result(&pkg, pm);
            let crate::search::Action::StartOperation { args, source, .. } = &row.action else {
                panic!("{pm}: expected a runnable install, got {:?}", row.action);
            };
            assert_eq!(args, &crate::packagekit::install_args("htop"), "{pm}");
            assert_eq!(source, pm, "the row says which package manager it is");
            assert!(!args.join(" ").contains("pkexec"), "{pm}");
            // An install is never mistaken for an update run.
            assert!(!super::is_update_run("Installing htop", args), "{pm}");
        }
    }

    #[test]
    fn plain_update_lists_the_updates_without_a_restart_row() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![super::UpdateInfo {
                    source: "dnf".into(),
                    app_id: None,
                    name: "vim.x86_64  2:9.2.1129-1.fc44".into(),
                    details: vec!["vim.x86_64  2:9.2.1129-1.fc44".into()],
                }],
            ));
        }
        let cfg = crate::config::Config::default();
        // Pending updates offer the update itself — never a restart chained
        // onto it, and never a restart row before a run has left the machine
        // actually needing one.
        let rows = super::update_results("", &cfg);
        assert!(
            !rows.iter().any(|r| r.title == "Update & Restart"),
            "{rows:?}"
        );
        let aggregate = rows
            .iter()
            .find(|r| r.title == crate::i18n::gettext("Update all packages"))
            .expect("the aggregate update row is offered");
        let args = match &aggregate.action {
            crate::search::Action::StartOperation { args, .. } => args.clone(),
            other => panic!("expected a runnable operation, got {other:?}"),
        };
        // The aggregate covers dnf, either as the daemon task (preferred) or as
        // the shell fallback where there is no daemon.
        let aggregate_text = args.join(" ");
        assert!(
            crate::dnf5daemon::is_update_task(&args)
                || args.iter().any(|a| a.contains("dnf upgrade")),
            "{args:?}"
        );
        assert!(!aggregate_text.contains("reboot"), "{args:?}");
        // No update argv anywhere may carry a reboot: a run has to finish
        // before the restart step is ever offered.
        for r in &rows {
            if let crate::search::Action::StartOperation { args, .. } = &r.action {
                assert!(
                    !args.iter().any(|a| a.contains("systemctl reboot")
                        || a.contains("reboot_chain")
                        || a.contains("loginctl reboot")),
                    "restart chained into {:?}: {args:?}",
                    r.title
                );
            }
        }
        // Targeted subqueries keep their own rows first.
        let rows = super::update_results("vim", &cfg);
        assert_eq!(rows[0].title, "Update: vim.x86_64");
    }

    #[test]
    fn an_empty_update_chain_fails_instead_of_succeeding_silently() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((std::time::Instant::now(), Vec::new()));
        }
        // Nothing in the cache → the update action must fail loudly. The
        // old `true` completed silently, and "Update & Restart" built on it
        // rebooted without having installed anything.
        for source in ["all", "appimage"] {
            let args = super::update_cmd_args(source, None);
            assert_eq!(
                &args[..2],
                &["sh".to_string(), "-c".to_string()],
                "{source}: {args:?}"
            );
            assert!(args[2].contains("exit 1"), "{source}: {args:?}");
            assert_ne!(args[2], "true", "{source}");
        }
        // …the restart chain built on it can't reboot either (exit 1
        // terminates the shell before the `&& ( reboot … )` part).
        let args = super::package_args("nonsense", "x");
        assert!(args[2].contains("exit 1"), "{args:?}");
    }

    #[test]
    fn the_restart_row_only_restarts_never_chains_an_update() {
        // Updates must be able to run to completion on their own; the row that
        // offers the restart is only ever a plain (confirmed) reboot.
        for row in [
            super::restart_required_row_with(false),
            super::restart_required_row_with(true),
        ] {
            assert!(matches!(
                row.action,
                crate::search::Action::ConfirmRunCommand(_)
            ));
        }
        // A staged download has its own row rather than pretending a reboot installs it.
        let plain = super::restart_required_row_with(false);
        assert_eq!(
            plain.title,
            crate::i18n::gettext("Restart required to finish the update")
        );
    }

    #[test]
    fn a_staged_download_is_applied_not_merely_restarted() {
        // A downloaded-but-unarmed transaction is not installed by a restart, so
        // its row has to do the arming: the store's own `schedule_for_next_boot`
        // call.
        let row = super::staged_update_now_row();
        assert_eq!(row.title, crate::i18n::gettext("Update now"));
        let args = match &row.action {
            crate::search::Action::StartOperation { args, .. } => args.clone(),
            other => panic!("expected a runnable operation, got {other:?}"),
        };
        assert_eq!(args, crate::dnf5daemon::schedule_args(), "{args:?}");
        // The restart row never lies about what it does: with the update armed
        // it *installs* it, otherwise it only starts using what is installed.
        let armed = super::restart_required_row_with(true);
        assert_eq!(
            armed.title,
            crate::i18n::gettext("Restart to install the updates"),
            "an armed update is installed by this restart"
        );
        let installed = super::restart_required_row_with(false);
        assert_eq!(
            installed.title,
            crate::i18n::gettext("Restart required to finish the update")
        );
        for flavour in [armed, installed] {
            assert!(matches!(
                flavour.action,
                crate::search::Action::ConfirmRunCommand(_)
            ));
        }
    }

    #[test]
    fn the_probe_recognises_a_staged_dnf5_offline_update() {
        // The offline transaction is what makes a staged update installable on
        // the next boot — the probe has to see it, or the restart row stays the
        // plain "restart to start using them" wording. It has to tell *armed*
        // (the daemon's /system-update symlink: this reboot installs it) from
        // merely downloaded, or the row promises something a restart can't do.
        let probe = super::REBOOT_PROBE;
        assert!(probe.contains("offline-transaction-state.toml"), "{probe}");
        assert!(probe.contains("download-complete"), "{probe}");
        assert!(probe.contains("/system-update"), "{probe}");
        assert_eq!(super::REASON_OFFLINE_ARMED, "dnf-offline-armed");
        // …and it stays ahead of the checks that would otherwise claim the
        // reboot first (a staged update is the more accurate reason).
        let staged = probe.find("dnf-offline-staged").unwrap_or(0);
        assert!(staged > 0);
        let armed = probe.find(super::REASON_OFFLINE_ARMED).unwrap_or(0);
        assert!(armed > 0 && armed < staged, "armed is the more precise state");
        for earlier in [
            "needs-restarting",
            "ostree-booted",
            "rpm-core",
            "pipewire",
            "core-libs",
        ] {
            let at = probe.find(earlier).unwrap_or_else(|| probe.len());
            assert!(at > armed, "{earlier} is checked before the offline state");
        }
    }

    #[test]
    fn update_operations_are_recognised() {
        assert!(super::is_update_op(&[
            "pkexec".into(),
            "dnf".into(),
            "upgrade".into(),
            "-y".into()
        ]));
        assert!(super::is_update_op(&["flatpak".into(), "update".into()]));
        assert!(super::is_update_op(&["pacman".into(), "-Syu".into()]));
        assert!(super::is_update_op(&["pkexec".into(), "snap".into(), "refresh".into()]));
        assert!(!super::is_update_op(&["flatpak".into(), "install".into(), "org.x.Y".into()]));
    }

    // "uninstall" sits at edit distance 2 from "install" — matches_verb()
    // fuzzy-rates the pair as a match, so the install branch (checked first)
    // used to steal the canonical uninstall query and offer to INSTALL the
    // app you asked to remove.
    #[test]
    fn base_apps_are_filtered_from_the_install_catalog() {
        // Dependency bases have no icon anywhere — they must not surface
        // as install suggestions ("Install: firefox application base").
        assert!(!super::is_catalog_app("org.mozilla.firefox.BaseApp"));
        assert!(!super::is_catalog_app("com.system76.Cosmic.BaseApp"));
        assert!(super::is_catalog_app("org.mozilla.firefox"));
        assert!(super::is_catalog_app("app.gummi.gummi"));
    }

    #[test]
    fn uninstall_verb_wins_over_the_install_fuzzy_match() {
        let cfg = crate::config::Config::default();
        let rows = super::search("uninstall zzz_no_such_app_zzz", &cfg, &[]);
        assert!(!rows.is_empty());
        let title = &rows[0].title;
        assert!(
            title.contains("Scanning installed apps")
                || title.contains("No installed app matching"),
            "uninstall query left the uninstall branch: {title}"
        );
        // …and the install verb still routes to install.
        let rows = super::search("install zzz_no_such_app_zzz", &cfg, &[]);
        assert!(!rows.is_empty());
        let title = &rows[0].title;
        assert!(
            title.starts_with("Searching for") || title.starts_with("No apps found"),
            "install query left the install branch: {title}"
        );
    }

    #[test]
    fn appimage_updates_flow_through_rows_scopes_and_commands() {
        let _g = cache_guard();
        // Pin the updater probe: the real one shells out to `command -v`
        // and would make every assertion here machine-dependent.
        crate::search::appimage::set_updater_for_test("/usr/bin/appimageupdatetool");
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![super::UpdateInfo {
                    source: "appimage".into(),
                    app_id: Some("/home/u/Applications/Krita-5.2.6-x86_64.AppImage".into()),
                    name: "Krita".into(),
                    details: vec!["Krita — ~/Applications/Krita-5.2.6-x86_64.AppImage".into()],
                }],
            ));
        }
        let cfg = crate::config::Config::default();

        // "update" → the appimage scope plus the per-file row, the display
        // name carried through unsplitted.
        let rows = super::update_results("", &cfg);
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"Update all packages"), "{titles:?}");
        assert!(titles.contains(&"Update AppImages"), "{titles:?}");
        assert!(titles.contains(&"Update: Krita"), "{titles:?}");
        let row = rows.iter().find(|r| r.title == "Update: Krita").unwrap();
        assert!(
            row.subtitle.as_deref().unwrap_or_default().contains("appimage"),
            "{row:?}"
        );

        // "update appimage" selects the scope and its package.
        let rows = super::update_results("appimage", &cfg);
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"Update AppImages"), "{titles:?}");
        assert!(titles.contains(&"Update: Krita"), "{titles:?}");
        assert_eq!(
            super::query_for("Update AppImages").as_deref(),
            Some("update appimage")
        );

        // Single file: plain argv — no shell, so the spaced path stays one
        // element and needs no quoting.
        for args in [
            super::update_cmd_args("appimage", Some("/home/u/My App.AppImage")),
            super::package_args("appimage", "/home/u/My App.AppImage"),
        ] {
            assert_eq!(
                &args[args.len() - 3..],
                [
                    "/usr/bin/appimageupdatetool",
                    "--overwrite",
                    "/home/u/My App.AppImage"
                ],
                "{args:?}"
            );
        }

        // The scope and "update all" chain pending files behind part markers.
        for source in ["appimage", "all"] {
            let chain = super::update_cmd_args(source, None);
            assert_eq!(chain[0], "sh", "{source}: {chain:?}");
            assert_eq!(chain[1], "-c", "{source}: {chain:?}");
            assert!(
                chain[2].contains("__spotty_part_1_1_appimage__"),
                "{source}: {:?}",
                chain[2]
            );
            assert!(
                chain[2]
                    .contains("--overwrite '/home/u/Applications/Krita-5.2.6-x86_64.AppImage'"),
                "{source}: {:?}",
                chain[2]
            );
        }

        // The updater counts as an update operation (post-run refresh +
        // reboot probe) — a plain file removal does not.
        assert!(super::is_update_op(&[
            "flatpak-spawn".into(),
            "--host".into(),
            "appimageupdatetool".into(),
            "--overwrite".into(),
            "/x.AppImage".into()
        ]));
        assert!(!super::is_update_op(&[
            "gio".into(),
            "trash".into(),
            "/x.AppImage".into()
        ]));
    }

    /// Serialize the tests that seed the shared update cache (tests run on
    /// parallel threads and the cache is one static slot).
    fn cache_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    fn seed_updates() {
        let mut c = super::update_cache().lock().unwrap();
        *c = Some((
            std::time::Instant::now(),
            vec![super::UpdateInfo {
                source: "dnf".into(),
                app_id: None,
                name: "vim.x86_64  2:9.2.1129-1.fc44".to_string(),
                details: vec!["vim.x86_64  2:9.2.1129-1.fc44".to_string()],
            }],
        ));
    }

    #[test]
    fn the_combined_update_runs_the_daemon_first_and_the_rest_afterwards() {
        // "Update all" covers several sources; only the distro half can be an
        // in-process D-Bus task, so it becomes the task and everything else
        // rides along as the script it runs afterwards.
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![
                    super::UpdateInfo {
                        source: "dnf".into(),
                        app_id: None,
                        name: "vim.x86_64  2:9.2.1129-1.fc44".into(),
                        details: vec![],
                    },
                    super::UpdateInfo {
                        source: "flatpak".into(),
                        app_id: Some("org.mozilla.firefox".into()),
                        name: "Firefox".into(),
                        details: vec![],
                    },
                ],
            ));
        }
        let args = super::update_cmd_args("all", None);
        if crate::dnf5daemon::available() {
            assert_eq!(args[0], crate::dnf5daemon::ARGV0, "{args:?}");
            assert_eq!(args[1], crate::dnf5daemon::VERB_ALL, "{args:?}");
            let script = args.last().expect("the rest rides along as a script");
            assert!(script.contains("flatpak update"), "{script}");
            // dnf is the daemon's job — it must not also be a shell command,
            // or the same packages would be handled twice.
            assert!(!script.contains("dnf"), "{script}");
        } else {
            assert!(
                args.iter().any(|a| a.contains("dnf upgrade")),
                "{args:?}"
            );
        }
    }

    #[test]
    fn package_list_parsers_extract_name_and_version() {
        assert_eq!(
            super::parse_dnf_updates(
                "vim.x86_64      2:9.2.1129-1.fc44   updates\nmanifold.x86_64 3.5.3-1.fc44 updates\n\n"
            ),
            vec![
                "vim.x86_64  2:9.2.1129-1.fc44",
                "manifold.x86_64  3.5.3-1.fc44"
            ]
        );
        assert_eq!(
            super::parse_snap_updates(
                "Name    Version  Rev  Tracking  Notes\ncore22  2024     1234 latest    -\n========\n"
            ),
            vec!["core22  2024"]
        );
        // Command missing / no output → no rows (not an error).
        assert!(super::parse_dnf_updates("").is_empty());
    }

    #[test]
    fn update_list_carries_notice_controls_and_status() {
        let _g = cache_guard();
        seed_updates();
        let cfg = crate::config::Config::default();
        let rows = super::update_results("", &cfg);
        assert!(
            rows.iter().any(|r| matches!(r.action, crate::search::Action::SnoozeUpdates)),
            "Remind tomorrow row"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r.action, crate::search::Action::DismissUpdates(_))),
            "Dismiss update notice row"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r.action, crate::search::Action::ToggleUpdates)),
            "enable/disable row"
        );
        assert!(super::updates_pending());
        assert_eq!(
            super::update_status_text(true),
            crate::i18n::gettext("One update available")
        );
    }

    #[test]
    fn a_finished_run_forgets_the_stale_pending_list() {
        use std::sync::atomic::Ordering;
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![super::UpdateInfo {
                    source: "dnf".into(),
                    app_id: None,
                    name: "vim.x86_64  2:9.2".into(),
                    details: vec!["vim.x86_64  2:9.2".into()],
                }],
            ));
        }

        super::invalidate_update_cache();
        assert!(
            super::update_cache().lock().unwrap().is_none(),
            "the stale pending list is dropped"
        );
        assert!(
            super::LAST_UPDATE_DONE.load(Ordering::SeqCst) > 0,
            "the run moment is remembered"
        );

        // A check that started after the run is trusted…
        assert!(!super::check_is_stale(super::now_millis() + 60_000));
        // …one that started before it is not.
        assert!(super::check_is_stale(1));

        super::LAST_UPDATE_DONE.store(0, Ordering::SeqCst);
    }

    #[test]
    fn icons_tell_pending_from_nothing_to_update() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![super::UpdateInfo {
                    source: "dnf".into(),
                    app_id: None,
                    name: "vim.x86_64  2:9.2".into(),
                    details: vec!["vim.x86_64  2:9.2".into()],
                }],
            ));
        }
        let cfg = crate::config::Config::default();
        let rows = super::update_results("", &cfg);
        let pending = rows
            .iter()
            .find(|r| r.title == "Update system packages")
            .expect("pending system scope");
        assert_eq!(
            pending.icon.as_deref(),
            Some("software-update-available-symbolic"),
            "pending keeps the update glyph"
        );
        let idle = rows
            .iter()
            .find(|r| r.title == "Update flatpak packages")
            .expect("idle flatpak scope");
        assert_eq!(
            idle.icon.as_deref(),
            Some("object-select-symbolic"),
            "idle shows the checkmark: {:?}",
            idle.icon
        );
    }

    #[test]
    fn disabled_updates_offer_only_the_enable_row() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((
                std::time::Instant::now(),
                vec![super::UpdateInfo {
                    source: "dnf".into(),
                    app_id: None,
                    name: "vim.x86_64  2:9.2".into(),
                    details: vec!["vim.x86_64  2:9.2".into()],
                }],
            ));
        }
        let mut cfg = crate::config::Config::default();
        cfg.enable_updates = false;

        // Everything else disappears — even with updates pending…
        for rows in [
            super::update_results("", &cfg),
            super::update_verb_rows("update", &cfg).expect("verb rows"),
        ] {
            assert_eq!(
                rows.len(),
                1,
                "{:?}",
                rows.iter().map(|r| &r.title).collect::<Vec<_>>()
            );
            assert_eq!(rows[0].title, crate::i18n::gettext("Enable update checks"));
            assert!(
                matches!(rows[0].action, crate::search::Action::ToggleUpdates),
                "{:?}",
                rows[0].action
            );
        }
    }

    #[test]
    fn status_reports_no_updates_for_an_empty_list() {
        let _g = cache_guard();
        {
            let mut c = super::update_cache().lock().unwrap();
            *c = Some((std::time::Instant::now(), Vec::new()));
        }
        // The Settings status row says "No updates available" — never
        // "0 updates available".
        assert_eq!(
            super::update_status_text(true),
            crate::i18n::gettext("No updates available")
        );
        assert!(!super::updates_checking());
        // Switch off: the status says so instead of counting anything.
        assert_eq!(
            super::update_status_text(false),
            crate::i18n::gettext("Update checks are off")
        );
    }

    #[test]
    fn no_update_chain_ever_carries_a_reboot() {
        // The regression this guards: "Update & Restart" ran `update && reboot`,
        // so the machine rebooted as part of the update — before the run could
        // report that it finished. Updates are their own action now, and the
        // restart is a separate row the user picks afterwards.
        for source in ["all", "flatpak", "dnf", "apt", "pacman", "zypper", "snap"] {
            for args in [
                super::update_cmd_args(source, None),
                super::package_args(source, "vim"),
            ] {
                let joined = args.join(" ");
                for needle in [
                    "systemctl reboot",
                    "loginctl reboot",
                    "openrc-shutdown",
                    "shutdown -r now",
                    "reboot_chain",
                ] {
                    assert!(
                        !joined.contains(needle),
                        "{source}: reboot ({needle}) chained into an update: {joined}"
                    );
                }
            }
        }
    }

    #[test]
    fn reboot_probe_covers_every_distro_family() {
        let probe = super::REBOOT_PROBE;
        // Canonical flag files: Debian/Ubuntu + openSUSE/SLE/MicroOS.
        assert!(probe.contains("/run/reboot-required"), "{probe}");
        assert!(probe.contains("/run/reboot-needed"), "{probe}");
        // Atomic images (Silverblue/Kinoite/Bazzite/CoreOS).
        assert!(probe.contains("/run/ostree-booted"), "{probe}");
        assert!(probe.contains("bootc status"), "{probe}");
        // Per-distro tools and generations.
        assert!(probe.contains("needs-restarting"), "{probe}");
        // …including the dnf subcommand form (Fedora ships it as a plugin).
        assert!(probe.contains("dnf needs-restarting"), "{probe}");
        // FCC 38.106: the audio stack needs a restart after an update, and
        // its module layout is distro-specific.
        assert!(probe.contains("pipewire"), "{probe}");
        assert!(probe.contains("/usr/lib64/pipewire-0.3"), "{probe}");
        assert!(probe.contains("/usr/lib/x86_64-linux-gnu/pipewire-0.3"), "{probe}");
        assert!(probe.contains("/nix/var/nix/profiles/system"), "{probe}");
        // Package-manager fallbacks: rpm (Fedora/RHEL/SUSE/Mageia/…) and
        // dpkg (Debian family without the flag file).
        assert!(probe.contains("INSTALLTIME"), "{probe}");
        assert!(probe.contains("/var/lib/dpkg/info"), "{probe}");
        // Generic fallbacks for every remaining distro.
        assert!(probe.contains("/usr/lib/modules"), "{probe}");
        assert!(probe.contains("libc.so.6"), "{probe}");
        assert!(probe.contains("/usr/lib/systemd/systemd"), "{probe}");
        // Always ends with an explicit "no reboot" line, never silence.
        assert!(probe.trim_end().ends_with(r#"echo "0 none""#), "{probe}");
    }

    #[test]
    fn preview_details_round_trip() {
        super::remember_details(
            "Update: system packages (dnf)",
            vec!["vim.x86_64  2:9.2.1129-1.fc44".to_string()],
        );
        assert_eq!(
            super::details_for("Update: system packages (dnf)"),
            vec!["vim.x86_64  2:9.2.1129-1.fc44".to_string()]
        );
        assert!(super::details_for("nonsense row").is_empty());
    }

    #[test]
    fn update_signature_ignores_order() {
        let a = super::UpdateInfo {
            source: "dnf".into(),
            app_id: None,
            name: "system packages (dnf)".into(),
            details: Vec::new(),
        };
        let b = super::UpdateInfo {
            source: "flatpak".into(),
            app_id: Some("org.x.Y".into()),
            name: "Y".into(),
            details: Vec::new(),
        };
        assert_eq!(
            super::update_signature(&[a.clone(), b.clone()]),
            super::update_signature(&[b, a])
        );
    }

    #[test]
    fn chained_update_script_carries_progress_markers() {
        let script = chained_script(&[
            ("flatpak", "flatpak update --assumeyes".to_string()),
            ("dnf", "pkexec dnf upgrade -y".to_string()),
            ("snap", "pkexec snap refresh".to_string()),
        ]);
        assert_eq!(&script[..2], &["sh".to_string(), "-c".to_string()]);
        // Every part runs even when an earlier one fails (`;` + status
        // accumulator), and the script fails if any part failed.
        assert_eq!(
            script[2],
            "st=0; echo __spotty_part_1_3_flatpak__ && { flatpak update --assumeyes || st=1; }; \
             echo __spotty_part_2_3_dnf__ && { pkexec dnf upgrade -y || st=1; }; \
             echo __spotty_part_3_3_snap__ && { pkexec snap refresh || st=1; }; \
             [ $st -eq 0 ]"
        );
        // Markers must be standalone echo arguments (no quoting needed).
        assert!(script[2].starts_with("st=0; echo __spotty_part_1_3_flatpak__ && "));
    }
}

// ── Update notice + reboot state (badge/banner near the orb) ─────────────────

/// Milliseconds since the epoch — finer than [`now_epoch`] so a check can
/// be ordered precisely against the moment an update run finished.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Stable signature of an update set: dismissing hides exactly this set —
/// a *new* update re-raises the notice.
fn update_signature(updates: &[UpdateInfo]) -> String {
    let mut keys: Vec<String> = updates
        .iter()
        .map(|u| format!("{}:{}:{}", u.source, u.app_id.as_deref().unwrap_or(""), u.name))
        .collect();
    keys.sort();
    keys.join("|")
}

/// The update notice as (count, signature): None when there is nothing to
/// show — no updates, the feature or its notification is off, the user
/// snoozed it, or this exact set was dismissed.
pub fn update_notice(cfg: &Config) -> Option<(usize, String)> {
    if !cfg.result_enabled("updates") || !cfg.update_notification {
        return None;
    }
    if cfg.update_snooze_until > now_epoch() {
        return None;
    }
    let updates = {
        let g = update_cache().lock().ok()?;
        match g.as_ref() {
            Some((_, v)) => v.clone(),
            None => return None,
        }
    };
    if updates.is_empty() {
        return None;
    }
    let sig = update_signature(&updates);
    if cfg.update_dismissed_sig == sig {
        return None;
    }
    Some((updates.len(), sig))
}

/// Host-side probe: layered reboot-required signals so every distro family
/// is covered, cheapest first. Emits `1 <reason>` or `0 none` (the reason
/// is logged) and stops at the first signal that fires:
///
/// 1. `/run/reboot-required` — Debian/Ubuntu family (kernel/libc6/dbus/
///    systemd postinsts touch it), plus derivatives that ship the flag.
/// 2. `/run/reboot-needed` — openSUSE/SLE/MicroOS (zypp-boot-plugin).
/// 3. ostree/bootc staged deployment — Silverblue, Kinoite, Bazzite,
///    Aurora, Bluefin, Fedora CoreOS.
/// 4. `needs-restarting -r` (exit 1 = reboot) — dnf/openSUSE plugin, when
///    installed.
/// 5. NixOS: a new generation built but not activated yet.
/// 6. rpm INSTALLTIME of dnf's own NEED_REBOOT list (+ SUSE/Mageia kernel
///    flavours) newer than the boot — every rpm distro (Fedora, RHEL,
///    openSUSE, Mageia, OpenMandriva, PCLinuxOS, …).
/// 7. dpkg install stamps of kernel/libc6/systemd/dbus newer than the boot
///    — Debian family when the flag file is absent.
/// 8. a non-running kernel in /usr/lib/modules|/lib/modules installed after
///    boot — Arch family, Alpine, Void, Gentoo, Slackware, Solus, Clear, …
/// 9. glibc/systemd themselves replaced after boot — anywhere else.
///
/// Live detection — evaluated at startup and after every update operation,
/// kept in memory only. Deliberately NOT persisted: comparing package
/// install times with the boot time means the notice clears itself after
/// the user actually reboots.
const REBOOT_PROBE: &str = r##"
# Boot time: /proc/stat btime (dnf's own source), else PID 1, else uptime.
boot=$(awk '/^btime /{print $2}' /proc/stat 2>/dev/null)
[ -n "$boot" ] || boot=$(stat -c %Y /proc/1 2>/dev/null)
[ -n "$boot" ] || boot=$(($(date +%s) - $(cut -d. -f1 /proc/uptime)))
[ -n "$boot" ] || boot=0

# 1. Debian/Ubuntu family: the canonical flag file.
if [ -e /run/reboot-required ] || [ -e /var/run/reboot-required ]; then
    echo "1 debian-flag"; exit 0
fi

# 2. openSUSE/SLE/MicroOS: zypp-boot-plugin (reboot|kexec|soft-reboot).
if [ -e /run/reboot-needed ]; then
    echo "1 suse-flag"; exit 0
fi

# 2b. Offline (system) update: the packages are already downloaded, and the
#     /system-update symlink is what makes the next boot install them — the flow
#     GNOME Software drives, through dnf5daemon or PackageKit (either arms the
#     same symlink). Two states, and the wording must differ: armed (the reboot
#     applies it) or merely downloaded (it doesn't yet). Kept early so the
#     wording can be the accurate one. The reason names predate PackageKit.
state=/usr/lib/sysimage/libdnf5/offline/offline-transaction-state.toml
if [ -L /system-update ]; then
    echo "1 dnf-offline-armed"; exit 0
fi
if [ -r "$state" ] \
   && grep -qE '^status = "(ready|download-complete)"' "$state" 2>/dev/null; then
    echo "1 dnf-offline-staged"; exit 0
fi
if [ -r /var/lib/PackageKit/prepared-update ]; then
    echo "1 packagekit-offline-staged"; exit 0
fi

# 3. Atomic images: a deployment is staged, so the booted one is no longer
#    the default for the next boot.
if [ -e /run/ostree-booted ]; then
    if command -v ostree >/dev/null 2>&1 \
       && [ "$(ostree admin status -D 2>/dev/null)" = "not-default" ]; then
        echo "1 ostree-staged"; exit 0
    fi
    if command -v bootc >/dev/null 2>&1 \
       && bootc status --json 2>/dev/null | grep -Eq '"staged"[[:space:]]*:[[:space:]]*\{'; then
        echo "1 bootc-staged"; exit 0
    fi
fi

# 4. dnf/openSUSE plugin, when installed: exit 1 = a full reboot is required.
#    Fedora ships it as a dnf subcommand rather than a standalone binary, so
#    fall back to `dnf needs-restarting` when the command isn't on PATH.
if command -v needs-restarting >/dev/null 2>&1; then
    needs-restarting -r >/dev/null 2>&1
    [ $? -eq 1 ] && { echo "1 needs-restarting"; exit 0; }
elif command -v dnf >/dev/null 2>&1; then
    dnf needs-restarting -r >/dev/null 2>&1
    [ $? -eq 1 ] && { echo "1 dnf-needs-restarting"; exit 0; }
fi

# 5. NixOS: current system != next-boot profile.
if [ -e /nix/var/nix/profiles/system ] && [ -L /run/current-system ]; then
    if [ "$(readlink -f /run/current-system)" != "$(readlink -f /nix/var/nix/profiles/system)" ]; then
        echo "1 nixos-generation"; exit 0
    fi
fi

# 6. rpm family: mirror dnf's NEED_REBOOT list, plus the kernel flavours
#    other rpm distros use — anything installed after the current boot.
#    pipewire/wireplumber are in the list for FCC 38.106: replacing the audio
#    stack needs the services restarted, which in practice means a reboot.
if command -v rpm >/dev/null 2>&1; then
    latest=$(rpm -q --qf '%{INSTALLTIME}\n' \
        kernel kernel-core kernel-rt kernel-default kernel-default-base \
        kernel-desktop kernel-server kernel-uek \
        glibc linux-firmware systemd dbus dbus-broker dbus-daemon \
        microcode_ctl intel-microcode amd-ucode-firmware ucode-intel ucode-amd \
        pipewire wireplumber gstreamer1 alsa-lib \
        2>/dev/null | grep -E '^[0-9]+$' | sort -n | tail -1)
    if [ -n "$latest" ] && [ "$latest" -gt "$boot" ]; then
        echo "1 rpm-core"; exit 0
    fi
fi

# 7. dpkg family: same idea via package install stamps, for systems where
#    the flag file above is missing (stock Debian, sysvinit setups).
if command -v dpkg-query >/dev/null 2>&1; then
    for f in /var/lib/dpkg/info/linux-image-*.list /var/lib/dpkg/info/libc6*.list \
             /var/lib/dpkg/info/systemd.list /var/lib/dpkg/info/dbus*.list \
             /var/lib/dpkg/info/pipewire*.list /var/lib/dpkg/info/wireplumber*.list; do
        [ -e "$f" ] || continue
        if [ "$(stat -c %Y "$f" 2>/dev/null || echo 0)" -gt "$boot" ]; then
            echo "1 dpkg-core"; exit 0
        fi
    done
fi

# 8. Any other distro: a kernel other than the running one, installed after
#    the current boot (Arch/Alpine/Void/Gentoo/Slackware/Solus/Clear/…).
for d in /usr/lib/modules/*/ /lib/modules/*/; do
    [ -d "$d" ] || continue
    [ "$(basename "$d")" = "$(uname -r)" ] && continue
    if [ "$(stat -c %Y "$d" 2>/dev/null || echo 0)" -gt "$boot" ]; then
        echo "1 kernel-update"; exit 0
    fi
done

# 9. FCC 38.106 audio stack replaced after boot (pipewire/wireplumber
#    modules, or their configuration). Module layouts differ per distro —
#    Fedora keeps them flat in /usr/lib64/pipewire-0.3/, Debian nests them
#    under pipewire-0.3/pipewire/ — so probe every layout we know.
for d in /usr/lib64/pipewire-0.3 /usr/lib/pipewire-0.3 \
         /usr/lib/pipewire-0.3/pipewire /usr/lib64/pipewire-0.3/pipewire \
         /usr/lib/x86_64-linux-gnu/pipewire-0.3 /usr/lib/x86_64-linux-gnu/pipewire-0.3/pipewire; do
    [ -d "$d" ] || continue
    for f in "$d"/*.so; do
        [ -e "$f" ] || continue
        if [ "$(stat -c %Y "$f" 2>/dev/null || echo 0)" -gt "$boot" ]; then
            echo "1 pipewire"; exit 0
        fi
    done
done
for d in /usr/share/pipewire/pipewire.conf.d /usr/share/pipewire/pipewire-pulse.conf.d \
         /usr/share/pipewire/client.conf.d /usr/share/wireplumber/wireplumber.conf.d; do
    [ -d "$d" ] || continue
    for f in "$d"/*.conf; do
        [ -e "$f" ] || continue
        if [ "$(stat -c %Y "$f" 2>/dev/null || echo 0)" -gt "$boot" ]; then
            echo "1 pipewire-config"; exit 0
        fi
    done
done

# 10. Last resort everywhere: glibc/systemd replaced after boot.
for f in /usr/lib/libc.so.6 /usr/lib64/libc.so.6 /lib/libc.so.6 /lib64/libc.so.6 \
         /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc.so.6 \
         /lib/aarch64-linux-gnu/libc.so.6 /usr/lib/aarch64-linux-gnu/libc.so.6 \
         /usr/lib/systemd/systemd /lib/systemd/systemd /usr/lib64/systemd/systemd; do
    [ -e "$f" ] || continue
    if [ "$(stat -c %Y "$f" 2>/dev/null || echo 0)" -gt "$boot" ]; then
        echo "1 core-libs"; exit 0
    fi
done

echo "0 none"
"##;

/// Why the probe says a reboot is needed, when it does.
pub(crate) const REASON_OFFLINE_STAGED: &str = "dnf-offline-staged";
pub(crate) const REASON_PK_OFFLINE_STAGED: &str = "packagekit-offline-staged";
pub(crate) const REASON_OFFLINE_ARMED: &str = "dnf-offline-armed";

fn detect_reboot() -> u8 {
    let Ok(out) = crate::app::run_host_shell_command(REBOOT_PROBE) else {
        return REBOOT_NONE;
    };
    // Take the last 0/1 line: a login shell may print profile noise above
    // the probe's own output.
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| l.starts_with('0') || l.starts_with('1'))
        .unwrap_or("");
    if !line.starts_with('1') {
        log::info!("reboot check: none");
        return REBOOT_NONE;
    }
    let token = line.split_whitespace().nth(1).unwrap_or("?");
    log::info!("reboot check: {token}");
    match token {
        // Armed by the daemon: the next boot installs these packages.
        REASON_OFFLINE_ARMED => REBOOT_OFFLINE_ARMED,
        // Downloaded only — a restart would install nothing, so the update row
        // has to arm it instead.
        REASON_OFFLINE_STAGED => REBOOT_OFFLINE_STAGED,
        REASON_PK_OFFLINE_STAGED => REBOOT_PK_OFFLINE_STAGED,
        _ => REBOOT_REQUIRED,
    }
}

/// Re-evaluate the reboot notice (off-thread: rpm queries take a moment).
///
/// When the probe flips from "no reboot needed" to "reboot needed", the update
/// that just finished only takes effect after a restart — say so, so the offer
/// isn't missed with the launcher closed. Updates never reboot by themselves;
/// this is the user-facing half of "update first, then restart".
pub fn refresh_reboot_state() {
    std::thread::spawn(|| {
        let reason = detect_reboot();
        let was = REBOOT_REASON.swap(reason, Ordering::SeqCst);
        glib::MainContext::default().invoke(move || {
            // Only an installed update is worth a notification: a downloaded
            // one still needs applying, which the row itself says.
            if reason == REBOOT_REQUIRED && was == REBOOT_NONE {
                let text = gettext("Restart required to finish the update");
                if crate::app::is_search_window_hidden() {
                    crate::app::send_desktop_notification("Spotty", &text);
                }
            }
            crate::app::refresh_search_window();
        });
    });
}

/// Probe outcomes: nothing pending, a plain reboot, or a dnf5 offline update
/// downloaded and waiting for the restart that installs it.
pub(crate) const REBOOT_NONE: u8 = 0;
pub(crate) const REBOOT_REQUIRED: u8 = 1;
pub(crate) const REBOOT_OFFLINE_STAGED: u8 = 2;
/// The same, but PackageKit holds the download (GNOME Software's background
/// download, or a PackageKit update that was never armed).
pub(crate) const REBOOT_PK_OFFLINE_STAGED: u8 = 4;
/// A downloaded update the daemon has armed (`/system-update`): the pending
/// reboot installs it, so the row can say exactly that.
pub(crate) const REBOOT_OFFLINE_ARMED: u8 = 3;

static REBOOT_REASON: AtomicU8 = AtomicU8::new(REBOOT_NONE);

/// True when a finished update still needs a reboot to take full effect.
pub fn reboot_pending() -> bool {
    REBOOT_REASON.load(Ordering::SeqCst) != REBOOT_NONE
}

/// True when the pending reboot is a dnf5 *offline* update that is downloaded
/// but not armed yet — restarting installs nothing, so the update row offers to
/// arm it (`Offline.schedule_for_next_boot`).
pub fn offline_update_staged() -> bool {
    matches!(
        REBOOT_REASON.load(Ordering::SeqCst),
        REBOOT_OFFLINE_STAGED | REBOOT_PK_OFFLINE_STAGED
    )
}

/// True when the staged download belongs to PackageKit, so PackageKit is the
/// service that has to arm it.
fn staged_via_packagekit() -> bool {
    REBOOT_REASON.load(Ordering::SeqCst) == REBOOT_PK_OFFLINE_STAGED
}

/// True when the pending reboot installs a downloaded update: the daemon has
/// marked it for the next boot, so this restart is what applies the packages.
pub fn offline_update_armed() -> bool {
    REBOOT_REASON.load(Ordering::SeqCst) == REBOOT_OFFLINE_ARMED
}

/// True when these operation args are an update run (dnf upgrade, flatpak
/// update, pacman -Syu, zypper update, snap refresh) — used to decide
/// whether to run the reboot check afterwards.
pub fn is_update_op(args: &[String]) -> bool {
    if crate::dnf5daemon::is_update_task(args) || crate::packagekit::is_update_task(args) {
        return true;
    }
    args.iter().any(|a| {
        a == "upgrade"
            || a == "update"
            || a == "-Syu"
            || a == "refresh"
            // `sh -c` chains (e.g. "pkexec dnf upgrade -y && reboot")
            || a.contains("dnf upgrade")
            || a.contains("apt-get upgrade")
            || a.contains("flatpak update")
            || a.contains("pacman -Syu")
            || a.contains("zypper update")
            || a.contains("snap refresh")
            // appimageupdatetool / the appimageupdate fork (path or argv)
            || a.contains("appimageupdate")
    })
}

/// True when a whole operation is a package update: recognised by its argv
/// or by its title ("Update: ..."). The argv of a single-package pacman run
/// (`pacman -S pkg`) is identical to an install, so the title is what
/// disambiguates it.
pub fn is_update_run(title: &str, args: &[String]) -> bool {
    // An install is never an update, however its argv reads. Update runs carry
    // the package manager's upgrade flags (`-Syu`, `upgrade`, `refresh`), so
    // without this an "Install: …" row would look like an update run.
    if title.starts_with("Install") {
        return false;
    }
    title.starts_with("Update") || is_update_op(args)
}

static NOTIFIED_SIG: OnceLock<Mutex<String>> = OnceLock::new();

fn notified_sig() -> &'static Mutex<String> {
    NOTIFIED_SIG.get_or_init(|| Mutex::new(String::new()))
}

/// Desktop notification when Spotty is hidden and this update set is new —
/// same eligibility rules as the in-app notice, plus "not notified yet".
fn notify_if_new(updates: &[UpdateInfo]) {
    if updates.is_empty() || !crate::app::is_search_window_hidden() {
        return;
    }
    let sig = update_signature(updates);
    crate::app::with_state(|st| {
        let cfg = st.config.borrow();
        if update_notice(&cfg).map(|(_, s)| s) != Some(sig.clone()) {
            return;
        }
        let mut seen = notified_sig().lock().unwrap();
        if *seen == sig {
            return;
        }
        *seen = sig.clone();
        crate::app::send_desktop_notification(
            "Spotty",
            &gettext("{n} updates available").replace("{n}", &updates.len().to_string()),
        );
    });
}


/// "Remind tomorrow": hide the update notice for 24 hours.
pub fn snooze_update_notice() {
    crate::app::with_state(|st| {
        let mut c = st.config.borrow_mut();
        c.update_snooze_until = now_epoch() + 24 * 3600;
        c.save();
    });
    crate::app::refresh_search_window();
}

/// "Dismiss": hide exactly this update set — a *new* update re-raises it.
pub fn dismiss_update_notice(sig: &str) {
    crate::app::with_state(|st| {
        let mut c = st.config.borrow_mut();
        c.update_dismissed_sig = sig.to_string();
        c.save();
    });
    crate::app::refresh_search_window();
}

// ── Preview details + Settings helpers ───────────────────────────────────────

static DETAILS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();

fn details_store() -> &'static Mutex<HashMap<String, Vec<String>>> {
    DETAILS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Row title → package list, recorded as update rows are built (the
/// preview pane looks the list up by the title it sees on the row).
fn remember_details(title: &str, details: Vec<String>) {
    if let Ok(mut g) = details_store().lock() {
        if g.len() > 200 {
            g.clear();
        }
        g.insert(title.to_string(), details);
    }
}

/// Package list for a row title (empty when unknown / no details).
pub fn details_for(title: &str) -> Vec<String> {
    details_store()
        .lock()
        .ok()
        .and_then(|g| g.get(title).cloned())
        .unwrap_or_default()
}

/// Row title -> the exact query the row runs, so ghost text can complete
/// "update f" to "update flatpak" while typing (see `candidate_for`).
static QUERIES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn query_store() -> &'static Mutex<HashMap<String, String>> {
    QUERIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the canonical query for an update row title (pair with
/// `remember_details`).
pub fn remember_query(title: &str, query: String) {
    if let Ok(mut g) = query_store().lock() {
        if g.len() > 200 {
            g.clear();
        }
        g.insert(title.to_string(), query);
    }
}

/// The canonical query for a row title, when it is an update row.
pub fn query_for(title: &str) -> Option<String> {
    query_store()
        .lock()
        .ok()
        .and_then(|g| g.get(title).cloned())
}

/// One-line status for Settings → Updates; with the feature off it says
/// so instead of a stale count.
pub fn update_status_text(enabled: bool) -> String {
    if !enabled {
        return gettext("Update checks are off");
    }
    if updates_checking() {
        return gettext("Checking for updates…");
    }
    match update_cache()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|(_, v)| v.len()))
    {
        Some(0) => gettext("No updates available"),
        Some(1) => gettext("One update available"),
        Some(n) => gettext("{n} updates available").replace("{n}", &n.to_string()),
        None => gettext("Checking for updates…"),
    }
}

/// True while a background update check is running — Settings' refresh
/// button shows itself as busy for that time.
pub fn updates_checking() -> bool {
    *update_fetching().lock().unwrap()
}

/// Signature of the currently cached update set, if any — what Settings'
/// "Dismiss update notice" hides.
pub fn dismiss_current_update_notice() {
    let sig = update_cache()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|(_, v)| update_signature(v)));
    if let Some(sig) = sig {
        dismiss_update_notice(&sig);
    }
}

/// True when the cached update list has at least one entry — gates the
/// notice rows in Settings → Updates.
pub fn updates_pending() -> bool {
    update_cache()
        .lock()
        .ok()
        .map(|g| g.as_ref().is_some_and(|(_, v)| !v.is_empty()))
        .unwrap_or(false)
}
