// Installed AppImages: discovery, launch/remove rows, and updates.
//
// AppImages are portable binaries, not packages — there is no store to
// search and nothing Spotty can reliably install from. What gets managed is
// what's already on the system: the files in the usual install dirs (plus
// the desktop-integrated ones AppImageLauncher/appimaged create), offered as
// rows to run or remove, and — when `appimageupdatetool`/`appimageupdate` is
// installed and an AppImage embeds update information — checked for and
// updated in place.
//
// Everything shell-ish runs on the host (`find`, the updater CLI), so this
// behaves the same inside the Flatpak sandbox.

use crate::i18n::gettext;
use crate::search::{Action, ResultKind, SearchResult};
use gtk::glib;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── Data ─────────────────────────────────────────────────────────────────────

/// One AppImage found on the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppImage {
    pub path: PathBuf,
    /// Display name: the integrating `.desktop` file's `Name` when there is
    /// one, otherwise derived from the filename ("Krita-5.2.6-x86_64" →
    /// "Krita").
    pub name: String,
    /// Version from `X-AppImage-Version` or the filename; empty if unknown.
    pub version: String,
    /// The `.desktop` entry that integrates this AppImage, when one exists
    /// (such AppImages are already searchable as normal apps).
    pub desktop: Option<PathBuf>,
}

impl AppImage {
    /// Desktop-integrated AppImages already show up in the app index, so
    /// they get no extra launch row of their own.
    fn is_integrated(&self) -> bool {
        self.desktop.is_some()
    }
}

/// How long a discovery result is served before re-scanning. Short enough
/// that a just-added or just-removed AppImage shows up quickly, long enough
/// that typing doesn't shell out `find` per keystroke.
const TTL: Duration = Duration::from_secs(20);

fn discovery() -> &'static Mutex<Option<(Instant, Vec<AppImage>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<AppImage>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn discovery_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

/// `None` = not probed yet, `Some(None)` = probed and no updater exists,
/// `Some(Some(cmd))` = the updater command (PATH name or absolute path).
fn updater() -> &'static Mutex<Option<Option<String>>> {
    static C: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn updater_fetching() -> &'static Mutex<bool> {
    static C: OnceLock<Mutex<bool>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(false))
}

// ── Host plumbing ────────────────────────────────────────────────────────────

fn is_sandbox() -> bool {
    std::env::var("FLATPAK_ID").is_ok()
}

fn host_command(prog: &str) -> std::process::Command {
    if is_sandbox() {
        let mut c = std::process::Command::new("flatpak-spawn");
        c.args(["--host", prog]);
        c
    } else {
        std::process::Command::new(prog)
    }
}

fn host_prefix() -> Vec<String> {
    if is_sandbox() {
        vec!["flatpak-spawn".into(), "--host".into()]
    } else {
        vec![]
    }
}

/// Single-quote a path for a `sh -c` string (`it's` → `'it'\''s'`).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A path with the home directory folded to `~`, for row subtitles.
pub fn short_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// The name a `RemoveAppImage` row was titled with: the filename
/// derivation, which is what discovery uses for exactly these rows
/// (integrated ones carry a desktop entry and don't take this action).
pub fn display_name(path: &Path) -> String {
    name_version_from_filename(path).0
}

/// Whether the file is still there. Checked directly only for paths that
/// are readable from inside the sandbox (home is mounted; /opt is not), so
/// a just-removed AppImage drops out of the rows immediately and a host
/// path we can't stat never produces a false negative.
fn alive(path: &Path) -> bool {
    if !is_sandbox() {
        return path.exists();
    }
    match dirs::home_dir() {
        Some(h) if path.starts_with(&h) => path.exists(),
        _ => true,
    }
}

// ── Filename / desktop parsing ───────────────────────────────────────────────

/// Arch/platform tokens appended to AppImage filenames that carry no name
/// information ("VSCode-linux-x64" → "VSCode").
const JUNK_SUFFIXES: &[&str] = &[
    "x86_64",
    "amd64",
    "x64",
    "x86",
    "aarch64",
    "arm64",
    "armhf",
    "i386",
    "i686",
    "linux",
    "gnu",
    "appimage",
];

/// Drop trailing junk tokens from a filename stem.
fn strip_junk(stem: &str) -> String {
    let mut name = stem.trim_end_matches(['-', '_', '.', ' ']);
    loop {
        let lower = name.to_ascii_lowercase();
        let cut = JUNK_SUFFIXES.iter().find_map(|j| {
            let head = lower.strip_suffix(j)?;
            let head = head.trim_end_matches(['-', '_', '.', ' ']);
            // Nothing left ("linux" itself) → not a suffix to strip.
            (!head.is_empty()).then_some(head.len())
        });
        match cut {
            Some(len) => name = &name[..len],
            None => break,
        }
    }
    name.to_string()
}

/// Byte range of the first `d.d[.d…]` run in `s` — the filename's version.
/// Digit runs without a dot ("x86_64") are not versions.
fn find_version(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            let mut end = i;
            while end > start && b[end - 1] == b'.' {
                end -= 1;
            }
            let parts: Vec<&str> = s[start..end].split('.').collect();
            if parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
            {
                return Some((start, end));
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Display name + version derived from the filename alone:
/// "Krita-5.2.6-x86_64.AppImage" → ("Krita", "5.2.6"),
/// "balenaEtcher-x86_64.AppImage" → ("balenaEtcher", "").
pub fn name_version_from_filename(path: &Path) -> (String, String) {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match find_version(&stem) {
        Some((start, end)) => {
            let version = stem[start..end].to_string();
            let mut name = strip_junk(&stem[..start]);
            // "v1.2.3-app" prefixes leave a lone "v" behind.
            if matches!(name.as_str(), "v" | "V") {
                name.clear();
            }
            if name.is_empty() {
                (stem, version)
            } else {
                (name, version)
            }
        }
        None => (strip_junk(&stem), String::new()),
    }
}

fn is_appimage(s: &str) -> bool {
    Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".appimage"))
}

/// The AppImage an `Exec=` line launches, if any: field codes (%f, %U…)
/// stripped, quotes respected, and unquoted paths with spaces kept whole.
fn exec_appimage_path(exec: &str) -> Option<PathBuf> {
    let trimmed = exec.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        let p = &rest[..end];
        return is_appimage(p).then(|| PathBuf::from(p));
    }
    // Unquoted: drop trailing/inline field codes; the rest is the command
    // line — usually the path itself, possibly containing spaces.
    let mut words: Vec<&str> = exec
        .split_whitespace()
        .filter(|t| !(t.len() == 2 && t.starts_with('%')))
        .collect();
    if let Some(last) = words.last_mut() {
        if let Some((head, code)) = last.split_once('%') {
            if code.len() == 1 {
                *last = head;
            }
        }
    }
    let joined = words.join(" ");
    if is_appimage(&joined) {
        return Some(PathBuf::from(joined));
    }
    // A command prefix (env VAR=…): the token that is the AppImage itself.
    words.iter().find(|w| is_appimage(w)).map(|w| PathBuf::from(w))
}

/// The identity keys of a `.desktop` file that integrates an AppImage.
/// Returns `(name, version, exec_path)` when `Exec` launches an AppImage.
pub fn parse_appimage_desktop(content: &str) -> Option<(String, String, PathBuf)> {
    let mut group = String::new();
    let mut name = String::new();
    let mut version = String::new();
    let mut exec: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            group = rest.strip_suffix(']').unwrap_or(rest).to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if group == "Desktop Entry" {
            match k {
                "Name" if name.is_empty() => name = v.to_string(),
                "X-AppImage-Version" if version.is_empty() => version = v.to_string(),
                "Exec" if exec.is_none() => exec = Some(v.to_string()),
                _ => {}
            }
        } else if group.starts_with("Desktop Action") && k == "Exec" && exec.is_none() {
            exec = Some(v.to_string());
        }
    }
    let path = exec_appimage_path(&exec?)?;
    // Filename identity fills in whatever the entry didn't carry — many
    // generated entries have no Name= or X-AppImage-Version= at all.
    let (file_name, file_version) = name_version_from_filename(&path);
    if name.is_empty() {
        name = file_name;
    }
    if version.is_empty() {
        version = file_version;
    }
    Some((name, version, path))
}

// ── Discovery ────────────────────────────────────────────────────────────────

/// `find` one set of dirs for AppImages (host-side, so unmounted host paths
/// like /opt work from the sandbox too).
fn find_appimages(dirs: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    if dirs.is_empty() {
        return Vec::new();
    }
    let paths: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
    let out = host_command("find")
        .args(&paths)
        .args([
            "-maxdepth",
            &max_depth.to_string(),
            "-type",
            "f",
            "-iname",
            "*.AppImage",
        ])
        .output();
    let mut v: Vec<PathBuf> = out
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// `.desktop` entries in the user's application dir whose `Exec` points at
/// an AppImage — these carry the real name/icon identity of integrated
/// AppImages and may reference files outside the usual install dirs.
fn integrated_appimages() -> Vec<(PathBuf, String, String, PathBuf)> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(home.join(".local/share/applications")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Some((name, version, exec_path)) = parse_appimage_desktop(&content) {
            out.push((exec_path, name, version, p));
        }
    }
    out
}

/// Scan everything once: AppImage files in the usual install dirs, plus the
/// ones desktop integration points at, merged with their entry metadata.
fn discover() -> Vec<AppImage> {
    let home = dirs::home_dir();
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(h) = &home {
        files.extend(find_appimages(
            &[
                h.join("Applications"),
                h.join("AppImages"),
                h.join("bin"),
                h.join(".local/bin"),
            ],
            1,
        ));
    }
    files.extend(find_appimages(&[PathBuf::from("/opt"), PathBuf::from("/usr/local/bin")], 2));

    let metadata: Vec<(PathBuf, String, String, PathBuf)> = integrated_appimages();
    for (path, ..) in &metadata {
        if !files.contains(path) {
            files.push(path.clone());
        }
    }

    files
        .into_iter()
        .map(|path| match metadata.iter().find(|(p, ..)| *p == path) {
            Some((_, name, version, desktop)) => AppImage {
                path,
                name: name.clone(),
                version: version.clone(),
                desktop: Some(desktop.clone()),
            },
            None => {
                let (name, version) = name_version_from_filename(&path);
                AppImage {
                    path,
                    name,
                    version,
                    desktop: None,
                }
            }
        })
        .collect()
}

/// Kick the discovery scan if there is none yet (or the cached one went
/// stale). Never blocks: callers get whatever is cached right now.
fn ensure_discovered() {
    let stale = match discovery().lock() {
        Ok(c) => match c.as_ref() {
            None => true,
            Some((t, _)) => t.elapsed() > TTL,
        },
        Err(_) => return,
    };
    if !stale {
        return;
    }
    {
        let Ok(mut f) = discovery_fetching().lock() else {
            return;
        };
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(|| {
        let list = discover();
        // The updater probe needs the file list for its AppImage-shipped
        // fallback; run it here so one warm-up pass answers both questions.
        if let Ok(mut u) = updater().lock() {
            if u.is_none() {
                *u = Some(probe_updater(&list));
            }
        }
        if let Ok(mut c) = discovery().lock() {
            *c = Some((Instant::now(), list));
        }
        if let Ok(mut f) = discovery_fetching().lock() {
            *f = false;
        }
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

/// Whatever is cached (possibly stale — served while the refresh runs),
/// or `None` while the very first scan is still in flight.
fn try_discovered() -> Option<Vec<AppImage>> {
    ensure_discovered();
    discovery()
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|(_, list)| list.clone()))
}

/// Blocking variant for worker threads (the update check): discovers right
/// away when cold instead of returning `None`.
fn discovered_blocking() -> Vec<AppImage> {
    if let Some(list) = try_discovered() {
        return list;
    }
    let list = discover();
    if let Ok(mut c) = discovery().lock() {
        *c = Some((Instant::now(), list.clone()));
    }
    list
}

// ── Updater tool ─────────────────────────────────────────────────────────────

/// Locate the updater: `appimageupdatetool` or the newer `appimageupdate`
/// fork on PATH first, then shipped as an AppImage itself (their usual
/// distribution form) among the discovered files.
fn probe_updater(list: &[AppImage]) -> Option<String> {
    let out = host_command("sh")
        .args([
            "-lc",
            "command -v appimageupdatetool || command -v appimageupdate",
        ])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !path.is_empty() {
        return Some(path);
    }
    let tool = list.iter().find(|ai| {
        ai.path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| {
                let n = n.to_ascii_lowercase();
                n.starts_with("appimageupdatetool") || n.starts_with("appimageupdate")
            })
    })?;
    // Downloaded AppImages rarely carry the executable bit.
    let _ = host_command("chmod")
        .args(["+x", &tool.path.to_string_lossy()])
        .status();
    Some(tool.path.to_string_lossy().into_owned())
}

/// The cached probe result; `None` means "no updater installed" (rows and
/// update checks simply don't happen then) — as opposed to `updater()`'s
/// "not probed yet", which only the gating helpers care about.
fn updater_cmd() -> Option<String> {
    updater().lock().ok().and_then(|u| u.as_ref().and_then(|o| o.clone()))
}

/// Blocking probe for worker threads (the update check may beat the warm-up).
fn updater_blocking() -> Option<String> {
    if let Ok(u) = updater().lock() {
        if let Some(cached) = u.as_ref() {
            return cached.clone();
        }
    }
    let list = discovered_blocking();
    let found = probe_updater(&list);
    if let Ok(mut u) = updater().lock() {
        if u.is_none() {
            *u = Some(found.clone());
        }
        return u.as_ref().and_then(|o| o.clone());
    }
    found
}

fn ensure_probed() {
    ensure_discovered();
    let ready = discovery().lock().is_ok_and(|c| c.is_some());
    let unprobed = updater().lock().is_ok_and(|u| u.is_none());
    if ready && unprobed {
        probe_updater_async();
    }
}

fn probe_updater_async() {
    {
        let Ok(mut f) = updater_fetching().lock() else {
            return;
        };
        if *f {
            return;
        }
        *f = true;
    }
    std::thread::spawn(|| {
        let list = discovery()
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|(_, l)| l.clone()))
            .unwrap_or_default();
        let found = probe_updater(&list);
        if let Ok(mut u) = updater().lock() {
            if u.is_none() {
                *u = Some(found);
            }
        }
        if let Ok(mut f) = updater_fetching().lock() {
            *f = false;
        }
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

/// Warm discovery + updater probe (startup, before anyone opens Settings).
pub fn preload() {
    ensure_probed();
}

/// Whether AppImage support means anything on this machine — an updater is
/// installed or AppImages were found. `None` while the probes are still in
/// flight (Settings hides the switch until the answer is known, the same
/// way Snap options wait for the snapd probe).
pub fn is_supported() -> Option<bool> {
    ensure_probed();
    let upd = updater()
        .lock()
        .ok()
        .and_then(|u| u.as_ref().map(|o| o.is_some()));
    let found = discovery()
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|(_, l)| !l.is_empty()));
    match (upd, found) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

// ── Rows ─────────────────────────────────────────────────────────────────────

/// Match an AppImage list against the query, scored into 1250..=1450: above
/// the install suggestions (800…770) so an app you already have leads them,
/// below indexed apps (≥1500) which stay the top hits.
fn score_matches<'a>(query: &str, list: &'a [AppImage], skip_integrated: bool) -> Vec<(&'a AppImage, i32)> {
    let ql = query.to_lowercase();
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);
    let mut out: Vec<(&AppImage, i32)> = list
        .iter()
        .filter(|ai| alive(&ai.path))
        .filter(|ai| !(skip_integrated && ai.is_integrated()))
        .filter_map(|ai| {
            let nl = ai.name.to_lowercase();
            let score = if nl == ql {
                1450
            } else if nl.starts_with(&ql) {
                1400
            } else if nl.contains(&ql) {
                1350
            } else if ai.path.to_string_lossy().to_lowercase().contains(&ql) {
                1300
            } else {
                let fs = pattern.score(Utf32String::from(ai.name.as_str()).slice(..), &mut matcher)?;
                if fs >= (ql.len() as u32).saturating_mul(25) {
                    1250
                } else {
                    return None;
                }
            };
            Some((ai, score))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

fn appimage_subtitle(ai: &AppImage) -> String {
    let path = short_path(&ai.path);
    if ai.version.is_empty() {
        gettext("AppImage — {path}").replace("{path}", &path)
    } else {
        gettext("AppImage · {version} — {path}")
            .replace("{version}", &ai.version)
            .replace("{path}", &path)
    }
}

/// Launch rows for the universal search: Enter runs the AppImage, the
/// uninstall shortcut removes it (wired in the search window).
pub fn search(query: &str, limit: usize) -> Vec<SearchResult> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let Some(list) = try_discovered() else {
        return Vec::new();
    };
    score_matches(q, &list, true)
        .into_iter()
        .take(limit)
        .map(|(ai, score)| SearchResult {
            kind: ResultKind::App,
            title: ai.name.clone(),
            subtitle: Some(appimage_subtitle(ai)),
            icon: None,
            action: Action::LaunchAppImage(ai.path.clone()),
            score,
        })
        .collect()
}

/// Whether the very first discovery scan is still in flight — the cmd
/// trigger shows its "Scanning…" placeholder instead of "no matches" for
/// sources that haven't answered yet.
pub fn discovery_pending() -> bool {
    try_discovered().is_none()
}

/// "Remove AppImage: …" rows for the cmd trigger's `uninstall` verb —
/// integrated ones included, since their desktop entry dies with them.
pub fn remove_rows(query: &str) -> Vec<SearchResult> {
    let Some(list) = try_discovered() else {
        return Vec::new();
    };
    let q = query.trim();
    let matches: Vec<(&AppImage, i32)> = if q.is_empty() {
        list.iter()
            .filter(|ai| alive(&ai.path))
            .take(4)
            .map(|ai| (ai, 1000))
            .collect()
    } else {
        score_matches(q, &list, false)
    };
    matches.into_iter().map(|(ai, _)| remove_row(ai)).collect()
}

fn remove_row(ai: &AppImage) -> SearchResult {
    let name = ai.name.clone();
    let icon = "application-x-executable-symbolic";
    // Plain files delete through the local trash dialog; an integrated
    // AppImage also loses its generated desktop entry, which needs the
    // two-step shell command — run as a (fast) operation instead.
    let action = if ai.desktop.is_some() {
        Action::StartOperation {
            title: gettext("Uninstalling {name}").replace("{name}", &name),
            source: "AppImage".into(),
            icon: icon.into(),
            args: remove_args(&ai.path, ai.desktop.as_deref()),
        }
    } else {
        Action::RemoveAppImage(ai.path.clone())
    };
    SearchResult {
        kind: ResultKind::System,
        title: gettext("Remove AppImage: {name}").replace("{name}", &name),
        subtitle: Some(appimage_subtitle(ai)),
        icon: Some(icon.into()),
        action,
        score: 1000,
    }
}

// ── Actions ──────────────────────────────────────────────────────────────────

/// Run an AppImage, detached so the search window can close. Downloads
/// often lack the executable bit (chmod first), and the runtime's
/// `--appimage-extract-and-run` is the fallback on systems without FUSE.
pub fn launch(path: &Path) {
    let q = shell_quote(&path.to_string_lossy());
    let _ = crate::app::spawn_host_shell_command(&format!(
        "chmod +x {q} 2>/dev/null; {q} >/dev/null 2>&1 || {q} --appimage-extract-and-run >/dev/null 2>&1 &"
    ));
}

/// argv that moves an AppImage to the trash (recoverable — a plain `rm`
/// on someone's downloaded app would be rude). A desktop-integrated
/// AppImage loses its generated `.desktop` entry too (also trashed, not
/// deleted), or the app index would keep offering a launcher for a file
/// that is gone.
pub fn remove_args(path: &Path, desktop: Option<&Path>) -> Vec<String> {
    let mut v = host_prefix();
    let quoted = shell_quote(&path.to_string_lossy());
    match desktop {
        Some(d) => v.extend([
            "sh".into(),
            "-c".into(),
            format!(
                "gio trash {quoted} && gio trash {}",
                shell_quote(&d.to_string_lossy())
            ),
        ]),
        None => v.extend([
            "gio".into(),
            "trash".into(),
            path.to_string_lossy().into_owned(),
        ]),
    }
    v
}

/// argv to update one AppImage in place (`--overwrite` keeps the original
/// path, which the desktop entry points at). No updater → a no-op.
pub fn update_args(path: &Path) -> Vec<String> {
    let Some(tool) = updater_cmd() else {
        return vec!["true".into()];
    };
    let mut v = host_prefix();
    v.push(tool);
    v.push("--overwrite".into());
    v.push(path.to_string_lossy().into_owned());
    v
}

/// `appimageupdatetool --overwrite '<path>'` for the "update all" shell
/// chain; `None` when no updater is installed.
pub fn update_shell_cmd(path: &Path) -> Option<String> {
    let tool = updater_cmd()?;
    Some(format!(
        "{} --overwrite {}",
        shell_quote(&tool),
        shell_quote(&path.to_string_lossy())
    ))
}

/// Recheck every discovered AppImage during an all-source update, rather
/// than limiting the run to files in the earlier pending-update preview.
/// The updater's check returns 1 for an available update; other files may
/// have no embedded update information and are left alone.
pub fn all_update_shell_cmds() -> Vec<String> {
    let Some(tool) = updater_cmd() else {
        return Vec::new();
    };
    let tool_path = PathBuf::from(&tool);
    let quoted_tool = shell_quote(&tool);
    try_discovered().unwrap_or_default().iter()
        .filter(|ai| ai.path != tool_path && alive(&ai.path))
        .map(|ai| {
            let path = shell_quote(&ai.path.to_string_lossy());
            format!("{quoted_tool} -j {path}; ai_status=$?; if [ \"$ai_status\" -eq 1 ]; then {quoted_tool} --overwrite {path}; fi")
        })
        .collect()
}

// ── Update checks ────────────────────────────────────────────────────────────

/// `-j` exits 1 when an update is available, 0 when current; any other
/// code means "no update information" / a broken source → not updatable.
fn check_update(tool: &str, path: &Path) -> bool {
    host_command(tool)
        .args(["-j", &path.to_string_lossy()])
        .output()
        .map(|o| o.status.code() == Some(1))
        .unwrap_or(false)
}

/// Pin the updater probe result from tests (the real probe shells out to
/// `command -v` and would make assertions machine-dependent).
#[cfg(test)]
pub(crate) fn set_updater_for_test(cmd: &str) {
    *updater().lock().unwrap() = Some(Some(cmd.to_string()));
}

/// `(name, path)` for every AppImage with a release update out there.
/// Each check is a network round trip, so they run in small parallel
/// waves — this only ever runs on the update-check thread, never per
/// keystroke.
pub fn pending_updates() -> Vec<(String, String)> {
    let list = discovered_blocking();
    let Some(tool) = updater_blocking() else {
        return Vec::new();
    };
    let tool_path = PathBuf::from(&tool);
    let candidates: Vec<&AppImage> = list
        .iter()
        .filter(|ai| ai.path != tool_path && alive(&ai.path))
        .collect();
    let mut out = Vec::new();
    for chunk in candidates.chunks(8) {
        let checked = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(i, ai)| {
                    let tool = tool.clone();
                    s.spawn(move || (i, check_update(&tool, &ai.path)))
                })
                .collect();
            let mut flags = vec![false; chunk.len()];
            for h in handles {
                if let Ok((i, upd)) = h.join() {
                    flags[i] = upd;
                }
            }
            flags
        });
        for (ai, upd) in chunk.iter().zip(checked) {
            if upd {
                out.push((ai.name.clone(), ai.path.to_string_lossy().into_owned()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn filename_yields_name_and_version() {
        assert_eq!(
            name_version_from_filename(&p("/home/u/Applications/Krita-5.2.6-x86_64.AppImage")),
            ("Krita".into(), "5.2.6".into())
        );
        assert_eq!(
            name_version_from_filename(&p("/home/u/bin/balenaEtcher-x86_64.AppImage")),
            ("balenaEtcher".into(), String::new())
        );
        assert_eq!(
            name_version_from_filename(&p("/opt/VSCode-linux-x64.AppImage")),
            ("VSCode".into(), String::new())
        );
        assert_eq!(
            name_version_from_filename(&p("/opt/NotePad.AppImage")),
            ("NotePad".into(), String::new())
        );
        // Digit runs without a dot are not versions ("x86_64" above, "2" here).
        assert_eq!(
            name_version_from_filename(&p("/opt/Program-2.AppImage")),
            ("Program-2".into(), String::new())
        );
        // A lone "v" left by "v1.2.3" doesn't become the name.
        assert_eq!(
            name_version_from_filename(&p("/opt/v5.2.6-x86_64.AppImage")),
            ("v5.2.6-x86_64".into(), "5.2.6".into())
        );
        // The whole "linux" word is never junk-stripped.
        assert_eq!(
            name_version_from_filename(&p("/opt/Linux.AppImage")),
            ("Linux".into(), String::new())
        );
    }

    #[test]
    fn desktop_entry_with_quoted_exec_and_version() {
        let content = "\
[Desktop Entry]
Type=Application
Name=Krita
Comment=painting
Exec=\"/home/u/Applications/Krita-5.2.6-x86_64.AppImage\" %U
Icon=krita
X-AppImage-Version=5.2.6
X-AppImage-Arch=x86_64
";
        let (name, version, path) = parse_appimage_desktop(content).unwrap();
        assert_eq!(name, "Krita");
        assert_eq!(version, "5.2.6");
        assert_eq!(path, p("/home/u/Applications/Krita-5.2.6-x86_64.AppImage"));
    }

    #[test]
    fn desktop_entry_unquoted_exec_keeps_spaces_and_drops_field_codes() {
        let content = "\
[Desktop Entry]
Type=Application
Name=My Tool
Exec=/home/u/My Apps/Tool-1.0-x86_64.AppImage %F
";
        let (_, _, path) = parse_appimage_desktop(content).unwrap();
        assert_eq!(path, p("/home/u/My Apps/Tool-1.0-x86_64.AppImage"));
    }

    #[test]
    fn desktop_entry_without_appimage_exec_is_ignored() {
        assert!(parse_appimage_desktop(
            "[Desktop Entry]\nName=Firefox\nExec=firefox %u\n"
        )
        .is_none());
        // Action groups are only a fallback, and only when they launch an
        // AppImage themselves.
        assert!(parse_appimage_desktop(
            "[Desktop Entry]\nName=X\nExec=sh -c 'foo'\n\n[Desktop Action run]\nExec=sh -c 'bar'\n"
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_falls_back_to_filename_identity() {
        let content = "[Desktop Entry]\nExec=/home/u/Apps/SomeApp-3.1-x86_64.AppImage %U\n";
        let (name, version, _) = parse_appimage_desktop(content).unwrap();
        assert_eq!(name, "SomeApp");
        assert_eq!(version, "3.1");
    }

    #[test]
    fn shell_quote_survives_quotes_and_spaces() {
        assert_eq!(shell_quote("/home/u/My App.AppImage"), "'/home/u/My App.AppImage'");
        assert_eq!(shell_quote("/home/u/it's.AppImage"), "'/home/u/it'\\''s.AppImage'");
    }

    #[test]
    fn remove_and_update_args_shape() {
        // Native tests have no sandbox prefix; assert the meaningful tail so
        // the test also holds when FLATPAK_ID leaks into the environment.
        let args = remove_args(&p("/home/u/Applications/Foo.AppImage"), None);
        assert_eq!(
            &args[args.len() - 3..],
            ["gio", "trash", "/home/u/Applications/Foo.AppImage"]
        );

        // Integrated: the entry goes with the file (single shell string).
        let args = remove_args(
            &p("/home/u/Applications/Foo.AppImage"),
            Some(&p("/home/u/.local/share/applications/foo.desktop")),
        );
        let sh = &args[args.len() - 1];
        assert!(sh.starts_with("gio trash "), "{sh}");
        assert!(sh.contains("&& gio trash "), "{sh}");
        assert!(sh.contains("foo.desktop"), "{sh}");
    }

    #[test]
    fn short_path_folds_home() {
        if let Some(home) = dirs::home_dir() {
            let deep = home.join("Applications").join("Foo.AppImage");
            assert_eq!(short_path(&deep), "~/Applications/Foo.AppImage");
        }
        assert_eq!(short_path(Path::new("/opt/Foo.AppImage")), "/opt/Foo.AppImage");
    }

    #[test]
    fn exec_parsing_edge_cases() {
        // env prefix
        assert_eq!(
            exec_appimage_path("env FOO=1 /opt/Bar.AppImage --flag"),
            Some(p("/opt/Bar.AppImage"))
        );
        // no AppImage at all
        assert_eq!(exec_appimage_path("/usr/bin/foo %U"), None);
        // field code glued to the path
        assert_eq!(
            exec_appimage_path("/opt/Bar.AppImage%U"),
            Some(p("/opt/Bar.AppImage"))
        );
    }

    #[test]
    fn update_check_only_counts_exit_code_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("spotty-ai-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("updater.sh");
        for code in [1u8, 0, 2] {
            std::fs::write(&script, format!("#!/bin/sh\nexit {code}\n")).unwrap();
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
            let got = check_update(&script.to_string_lossy(), &p("/nonexistent.AppImage"));
            assert_eq!(got, code == 1, "exit {code} → updatable = {}", got);
        }
        // A missing tool never counts as an update.
        assert!(!check_update("/nonexistent-updater-binary", &p("/x")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_chmods_and_runs_the_appimage() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("spotty-ai-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let app = dir.join("Demo-1.0-x86_64.AppImage");
        let marker = dir.join("ran.log");
        // Deliberately not executable: launch() must chmod +x it first.
        // The script exits 0, so the --appimage-extract-and-run fallback
        // stays out of the run and the marker proves the direct path.
        std::fs::write(&app, format!("#!/bin/sh\necho ran >> '{}'\n", marker.display())).unwrap();
        launch(&app);
        let mut ran = false;
        for _ in 0..50 {
            if marker.exists() {
                ran = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ran, "the AppImage script never ran");
        let mode = std::fs::metadata(&app).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "launch() must chmod +x the file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
