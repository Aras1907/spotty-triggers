// Detect the search engine the default browser actually uses — and the icon
// that comes with it.
//
// "Browser Default" has to mean *whatever the browser does today*, not whatever
// it did when Spotty started, and it has to work for every browser family. So
// the answer is read from the browser's own settings files rather than guessed:
//
// * **Firefox family** — `<profile>/search.json.mozlz4`: the engine the user
//   picked (`metaData.defaultEngineId`) or the app default
//   (`appDefaultEngineId`), with full details (URL templates, icons) for
//   engines the user added themselves.
// * **Chromium family** (Chrome, Chromium, Brave, Edge, Vivaldi, Opera, Yandex)
//   — `<profile>/Preferences`. Chromium only writes `default_search_provider`
//   once the user picks an engine, so a stock profile falls back to the built-in
//   default (Google) instead of detecting nothing.
// * **GNOME Web** — `org.gnome.Epiphany` GSettings, read through GLib rather
//   than by parsing `gsettings get` output.
//
// Freshness is by file mtime: the browser rewrites its search settings the
// moment the user changes engine, and the launcher notices that on the next
// row build (see [`ensure_detected`]).

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use gio::prelude::*;

/// An icon the browser itself keeps for the engine.
///
/// Chromium stores an `https:` or `data:` favicon URL, Firefox a `data:` URI
/// per size in `iconMapObj` — so most engines need no network at all, and any
/// engine can have one even when its site has no usable favicon.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EngineIcon {
    /// A `data:` URI (`data:image/png;base64,…`).
    Data(String),
    /// A remote icon URL.
    Url(String),
}

/// Everything the web-search row needs about the detected engine.
#[derive(Clone, Debug)]
pub struct Detected {
    pub name: String,
    pub url_template: String,
    pub icon: Option<EngineIcon>,
}

impl Detected {
    /// The search URL for `q`, whatever placeholder the browser used.
    fn url_for(&self, q: &str) -> Option<String> {
        let encoded = urlencoding::encode(q);
        let t = &self.url_template;
        let url = if t.contains("{searchTerms}") {
            t.replace("{searchTerms}", &encoded)
        } else if t.contains("%s") {
            t.replace("%s", &encoded)
        } else if t.contains("{query}") {
            t.replace("{query}", &encoded)
        } else {
            return None;
        };
        Some(url)
    }
}

/// The last detection, plus the files it was derived from.
///
/// The stamps are what make "even if the user changes it" work: re-reading the
/// browser's settings is a `stat`, cheap enough to do while rows are built,
/// while re-parsing them is not.
struct Cache {
    at: Instant,
    /// `(path, mtime)` for every file the detection consulted.
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
    detected: Option<Detected>,
}

/// Fallback refresh for browsers that keep their setting outside a file we can
/// stat (Epiphany's GSettings): re-check periodically, not per keystroke.
const TTL: Duration = Duration::from_secs(300);

/// A detection is already running.
static DETECTING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn cache() -> &'static Mutex<Option<Cache>> {
    static C: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn stamp_of(path: &Path) -> (PathBuf, Option<SystemTime>) {
    (path.to_path_buf(), fs::metadata(path).ok().and_then(|m| m.modified().ok()))
}

/// True when the cache is empty, older than [`TTL`], or any file it was built
/// from has been rewritten since.
fn stale(c: Option<&Cache>) -> bool {
    let Some(c) = c else { return true };
    if c.at.elapsed() >= TTL {
        return true;
    }
    c.stamps
        .iter()
        .any(|(path, then)| stamp_of(path) != (path.clone(), *then))
}

/// Browser's search engine URL for a query. Triggers background detection on
/// first call; returns `None` if detection hasn't completed yet or failed.
pub fn url_for(q: &str) -> Option<String> {
    ensure_detected();
    current()?.url_for(q)
}

/// The detected search engine name (e.g. "Google", "Kagi"), if known.
pub fn engine_name() -> Option<String> {
    ensure_detected();
    current().map(|d| d.name)
}

/// The icon the browser keeps for its search engine, if any.
pub fn engine_icon() -> Option<EngineIcon> {
    ensure_detected();
    current().and_then(|d| d.icon)
}

fn current() -> Option<Detected> {
    cache().lock().unwrap().as_ref()?.detected.clone()
}

fn ensure_detected() {
    {
        let c = cache().lock().unwrap();
        if !stale(c.as_ref()) {
            return;
        }
    }
    // Rows are built several times per keystroke, and every one of them asks:
    // without this, a stale cache would start a detection per row.
    if DETECTING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        // Whatever happens below, the next ask gets to start a fresh attempt.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                DETECTING.store(false, Ordering::SeqCst);
            }
        }
        let _reset = Reset;
        let found = detect();
        *cache().lock().unwrap() = Some(match found {
            Some(Found {
                detected,
                stamps,
            }) => Cache {
                at: Instant::now(),
                stamps,
                detected: Some(detected),
            },
            None => Cache {
                at: Instant::now(),
                stamps: Vec::new(),
                detected: None,
            },
        });
        // Detection finishing is a visible change (the row's name and icon), so
        // ask for a refresh — but only if something is actually different.
        glib::idle_add_once(crate::app::refresh_search_window);
    });
}

/// Detect on a background thread at startup, so the very first keystroke
/// already has the answer (and a cold browser doesn't stall the UI).
pub fn warmup() {
    std::thread::spawn(|| {
        let _ = engine_name();
    });
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// A detection plus the files it came from, so freshness can watch them.
struct Found {
    detected: Detected,
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

impl From<Detected> for Found {
    fn from(detected: Detected) -> Found {
        Found {
            detected,
            stamps: Vec::new(),
        }
    }
}

fn detect() -> Option<Found> {
    let desktop_id = default_browser_id()?;
    log::info!("search engine: probing {desktop_id}");
    let found = detect_for_desktop(&desktop_id);
    match &found {
        Some(f) => log::info!(
            "search engine: {} ({}), icon {}",
            f.detected.name,
            desktop_id,
            if f.detected.icon.is_some() { "yes" } else { "no" }
        ),
        None => log::info!("search engine: nothing detected for {desktop_id}"),
    }
    found
}

/// Which browser handles `https://`, as a `.desktop` id. Same door the rest of
/// the launcher uses to open links, so "the default browser" means one thing.
fn default_browser_id() -> Option<String> {
    let out = crate::app::run_host_shell_command("gio mime x-scheme-handler/https").ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id = stdout
        .lines()
        .find(|l| l.contains("Default application for"))?
        .split_once(':')?
        .1
        .trim()
        .to_string();
    (!id.is_empty() && id != "none").then_some(id)
}

fn detect_for_desktop(desktop_id: &str) -> Option<Found> {
    match BrowserKind::of(desktop_id)? {
        BrowserKind::Firefox => detect_firefox(),
        BrowserKind::Chromium => detect_chromium_for_desktop(desktop_id),
        BrowserKind::Epiphany => detect_epiphany(),
        // Known browsers we can't read a search engine out of — they still get
        // their private-window flag, they just don't change the search engine.
        BrowserKind::Falkon | BrowserKind::Qutebrowser => None,
    }
}

/// Which browser family a `.desktop` id belongs to.
///
/// One place decides this, because two features need the same answer: reading
/// the search engine out of the profile, and knowing which flag opens a private
/// window. `None` means "a browser we know nothing about" — deliberately not a
/// guess, since guessing would mean opening a window that isn't private.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BrowserKind {
    Firefox,
    Chromium,
    Epiphany,
    Falkon,
    Qutebrowser,
}

impl BrowserKind {
    pub fn of(desktop_id: &str) -> Option<BrowserKind> {
        let id = desktop_id.to_lowercase();
        if is_firefox(&id) {
            return Some(BrowserKind::Firefox);
        }
        if chromium_config_dir(&id).is_some() {
            return Some(BrowserKind::Chromium);
        }
        if is_epiphany(&id) {
            return Some(BrowserKind::Epiphany);
        }
        if id.contains("falkon") {
            return Some(BrowserKind::Falkon);
        }
        if id.contains("qutebrowser") {
            return Some(BrowserKind::Qutebrowser);
        }
        None
    }

    /// The flags that make this browser open `url` privately.
    ///
    /// Chromium's `--incognito` and Firefox's `--private-window` are what these
    /// browsers actually document; the rest are the equivalent switch in each.
    /// `desktop_id` is passed in because Opera shares Chromium's engine but
    /// calls the switch `--private`, so the family alone is not the answer.
    pub fn private_flags(&self, desktop_id: &str) -> Vec<&'static str> {
        match self {
            BrowserKind::Firefox => vec!["--private-window"],
            BrowserKind::Chromium if is_opera(desktop_id) => vec!["--private"],
            BrowserKind::Chromium => vec!["--incognito"],
            BrowserKind::Epiphany => vec!["--private-instance"],
            BrowserKind::Falkon => vec!["--private-browsing"],
            BrowserKind::Qutebrowser => vec!["--target", "private-window"],
        }
    }
}

// ── Firefox family ────────────────────────────────────────────────────────────

/// Firefox and its forks share the profile layout and the search file.
fn is_firefox(desktop_id: &str) -> bool {
    let id = desktop_id.to_lowercase();
    [
        "firefox",
        "librewolf",
        "mullvad",
        "waterfox",
        "floorp",
        "librewolf-community",
    ]
    .iter()
    .any(|name| id.contains(name))
}

/// Every place a Firefox profile may live: native, Flatpak and Snap.
fn firefox_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut roots = vec![home.join(".mozilla/firefox")];
    for (app, rel) in [
        ("org.mozilla.firefox", ".mozilla/firefox"),
        ("io.gitlab.librewolf-community", ".mozilla/firefox"),
        ("com.nexxtream.garudalinux", ".mozilla/firefox"),
    ] {
        let flatpak = home.join(".var/app").join(app).join(rel);
        if flatpak.exists() {
            roots.push(flatpak);
        }
    }
    let snap = home.join("snap/firefox/common/.mozilla/firefox");
    if snap.exists() {
        roots.push(snap);
    }
    roots
}

/// The profile Firefox itself would use: the `[Install*]` default, else the
/// section marked `Default=1`.
fn firefox_profile(root: &Path) -> Option<PathBuf> {
    let ini = fs::read_to_string(root.join("profiles.ini")).ok()?;
    let mut installed = None;
    let mut flagged = None;
    for section in ini.split('[').skip(1) {
        let body = match section.split_once(']') {
            Some((name, body)) => (name, body),
            None => continue,
        };
        let (name, body) = body;
        if name.starts_with("Install") {
            // Firefox writes the profile it created as `Default=<path>`.
            if installed.is_none() {
                installed = ini_value(body, "Default");
            }
        } else if ini_value(body, "Default").is_some() && flagged.is_none() {
            flagged = ini_value(body, "Path");
        }
    }
    installed
        .or(flagged)
        .map(|p| root.join(p))
        .filter(|p| p.is_dir())
}

fn ini_value(body: &str, key: &str) -> Option<String> {
    body.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key && !v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

fn detect_firefox() -> Option<Found> {
    for root in firefox_roots() {
        let Some(profile) = firefox_profile(&root) else {
            continue;
        };
        let search_json = profile.join("search.json.mozlz4");
        if !search_json.exists() {
            continue;
        }
        let stamp = stamp_of(&search_json);
        if let Some(d) = parse_search_json_mozlz4(&search_json) {
            return Some(Found {
                detected: d,
                stamps: vec![stamp],
            });
        }
    }
    None
}

#[derive(serde::Deserialize, Default)]
struct SearchJson {
    #[serde(default)]
    engines: Vec<SearchEngine>,
    #[serde(rename = "metaData", default)]
    meta: SearchMeta,
}

#[derive(serde::Deserialize, Default)]
struct SearchMeta {
    /// What the user picked.
    #[serde(rename = "defaultEngineId", default)]
    default_engine: Option<String>,
    /// What the browser would use untouched.
    #[serde(rename = "appDefaultEngineId", default)]
    app_default_engine: Option<String>,
}

#[derive(serde::Deserialize)]
struct SearchEngine {
    id: String,
    /// Firefox 130+ ("search config v2") keeps the display name here.
    #[serde(rename = "_name", default)]
    private_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// Only engines the user added carry their own templates.
    #[serde(default)]
    urls: Vec<SearchUrl>,
    #[serde(rename = "iconURL", default)]
    icon_url: Option<String>,
    #[serde(rename = "iconMapObj", default)]
    icon_map: Option<HashMap<String, String>>,
}

#[derive(serde::Deserialize)]
struct SearchUrl {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    template: String,
}

impl SearchEngine {
    /// The query URL the browser would use for this engine, when the file
    /// carries it (user-added engines) — otherwise looked up by id.
    fn template(&self) -> Option<String> {
        let own = self
            .urls
            .iter()
            .find(|u| u.kind == "text/html" && !u.template.is_empty())
            .map(|u| u.template.clone())
            .filter(|t| t.contains("{searchTerms}") || t.contains("%s"));
        own.or_else(|| engine_template_for(&self.id))
    }

    fn icon(&self) -> Option<EngineIcon> {
        // A user-added engine keeps its icon inline; the biggest one wins.
        if let Some(map) = &self.icon_map {
            if let Some((_, data)) = map
                .iter()
                .filter(|(size, _)| size.parse::<u32>().unwrap_or(0) >= 32)
                .max_by_key(|(size, _)| size.parse::<u32>().unwrap_or(0))
            {
                return Some(EngineIcon::Data(data.clone()));
            }
        }
        self.icon_url.as_ref().map(|url| {
            if url.starts_with("data:") {
                EngineIcon::Data(url.clone())
            } else {
                EngineIcon::Url(url.clone())
            }
        })
    }
}

fn parse_search_json_mozlz4(path: &Path) -> Option<Detected> {
    let raw = decode_mozlz4(&fs::read(path).ok()?)?;
    let parsed: SearchJson = serde_json::from_slice(&raw).ok()?;
    let wanted = parsed
        .meta
        .default_engine
        .as_deref()
        .or(parsed.meta.app_default_engine.as_deref());
    let engine = wanted
        .and_then(|id| parsed.engines.iter().find(|e| e.id == id))
        .or_else(|| parsed.engines.first())?;
    let name = engine
        .private_name
        .as_deref()
        .or(engine.name.as_deref())
        .unwrap_or(&engine.id)
        .to_string();
    Some(Detected {
        name,
        url_template: engine.template()?,
        icon: engine.icon(),
    })
}

/// Search URLs for the engines Firefox ships. Its config v2 file only records
/// the id and display name — the templates live inside the browser's own
/// archive, which is not readable from a Flatpak sandbox — so this table is
/// the honest source for them. Ids are matched by prefix, since regional
/// variants (`wikipedia-de`, `google-*`) keep the stem.
fn engine_template_for(id: &str) -> Option<String> {
    let id = id.to_lowercase();
    let template = match id.split('-').next().unwrap_or_default() {
        "google" | "youtube" => "https://www.google.com/search?q={searchTerms}",
        "ddg" | "duckduckgo" => "https://duckduckgo.com/?q={searchTerms}",
        "bing" => "https://www.bing.com/search?q={searchTerms}",
        "startpage" => "https://www.startpage.com/sp/search?query={searchTerms}",
        "ecosia" => "https://www.ecosia.org/search?q={searchTerms}",
        "brave" => "https://search.brave.com/search?q={searchTerms}",
        "perplexity" => "https://www.perplexity.ai/search?q={searchTerms}",
        "wikipedia" | "wiki" => "https://en.wikipedia.org/w/index.php?search={searchTerms}",
        "qwant" => "https://www.qwant.com/?q={searchTerms}",
        "mojeek" => "https://www.mojeek.com/search?q={searchTerms}",
        "yandex" => "https://yandex.com/search/?text={searchTerms}",
        "baidu" => "https://www.baidu.com/s?wd={searchTerms}",
        "sogou" => "https://www.sogou.com/web?query={searchTerms}",
        "seznam" => "https://search.seznam.cz/?q={searchTerms}",
        "daum" => "https://search.daum.net/search?q={searchTerms}",
        "naver" => "https://search.naver.com/search.naver?query={searchTerms}",
        "ask" => "https://www.ask.com/web?q={searchTerms}",
        "kagi" => "https://kagi.com/search?q={searchTerms}",
        "marginalia" => "https://search.marginalia.nu/search?query={searchTerms}",
        _ => return None,
    };
    Some(template.to_string())
}

/// Decode Firefox's `mozLz4`: the 8-byte magic, a little-endian uncompressed
/// size, then an LZ4 *block*. (Some builds omit the size header, so both
/// layouts are tried rather than trusting one.)
fn decode_mozlz4(data: &[u8]) -> Option<Vec<u8>> {
    const MAGIC: &[u8; 8] = b"mozLz40\0";
    let body = data.strip_prefix(MAGIC)?;
    // The magic alone is a truncated file, not an empty document.
    if body.is_empty() {
        return None;
    }
    // With the size header: the decoded length must match it, which also proves
    // the header was a size and not compressed data.
    if let Some(size_bytes) = body.get(0..4) {
        let size = u32::from_le_bytes(size_bytes.try_into().ok()?) as usize;
        if let Some(out) = lz4_block_decompress(body.get(4..)?, Some(size)) {
            if out.len() == size {
                return Some(out);
            }
        }
    }
    // Without it.
    lz4_block_decompress(body, None)
}

/// Minimal LZ4 block decoder (the format Firefox uses).
///
/// A sequence is one token byte (high nibble = literals, low nibble = match
/// length minus 4), then the literals, then a 16-bit backwards offset, then any
/// extra length bytes (0xFF-terminated). Matches may overlap the output, so
/// they are copied byte by byte.
fn lz4_block_decompress(src: &[u8], hint: Option<usize>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(hint.unwrap_or(src.len() * 4).min(64 * 1024 * 1024));
    let mut i = 0;
    while i < src.len() {
        let token = src[i];
        i += 1;
        let mut literals = (token >> 4) as usize;
        if literals == 15 {
            loop {
                let b = *src.get(i)?;
                i += 1;
                literals += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let end = i.checked_add(literals)?;
        out.extend_from_slice(src.get(i..end)?);
        i = end;
        // The last sequence of a block has literals only.
        if i >= src.len() {
            break;
        }
        let offset = u16::from_le_bytes([*src.get(i)?, *src.get(i + 1)?]) as usize;
        i += 2;
        if offset == 0 || offset > out.len() {
            return None;
        }
        let mut length = (token & 0x0f) as usize;
        if length == 15 {
            loop {
                let b = *src.get(i)?;
                i += 1;
                length += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        length += 4;
        let start = out.len() - offset;
        for k in 0..length {
            out.push(out[start + k]);
        }
    }
    Some(out)
}

// ── Chromium family ───────────────────────────────────────────────────────────

/// Map desktop ID → Chromium config directory name.
fn chromium_config_dir(desktop_id: &str) -> Option<&'static str> {
    let id = desktop_id.to_lowercase();
    if id.contains("brave") {
        Some("BraveSoftware/Brave-Browser")
    } else if id.contains("chrome") && !id.contains("chromium") {
        Some("google-chrome")
    } else if id.contains("chromium") {
        Some("chromium")
    } else if id.contains("edge") {
        Some("microsoft-edge")
    } else if id.contains("vivaldi") {
        Some("vivaldi")
    } else if id.contains("opera") {
        Some("opera")
    } else if id.contains("yandex") {
        Some("yandex-browser")
    } else {
        None
    }
}

/// Opera is Chromium-based but names its private mode differently, so it is
/// called out before the generic Chromium answer.
fn is_opera(desktop_id: &str) -> bool {
    desktop_id.to_lowercase().contains("opera")
}

/// Try to detect the search engine from a Chromium-family browser's Preferences.
fn detect_chromium_for_desktop(desktop_id: &str) -> Option<Found> {
    let dir_name = chromium_config_dir(desktop_id)?;
    let flatpak_id = desktop_id_to_flatpak_id(desktop_id)?;

    // Candidate roots: Flatpak first, then native.
    let home = dirs::home_dir()?;
    let candidates = vec![
        home.join(format!(".var/app/{flatpak_id}/config/{dir_name}")),
        home.join(format!(".config/{dir_name}")),
    ];

    for root in candidates {
        if !root.exists() {
            continue;
        }
        let profile_dir = find_chromium_profile(&root)?;
        let prefs_path = profile_dir.join("Preferences");
        if !prefs_path.exists() {
            continue;
        }
        let stamp = stamp_of(&prefs_path);
        if let Some(d) = parse_chromium_prefs(&prefs_path) {
            return Some(Found {
                detected: d,
                stamps: vec![stamp],
            });
        }
    }
    None
}

/// Convert a desktop ID to its Flatpak app-id (if it's a Flatpak app).
/// E.g. "com.brave.Browser.desktop" → "com.brave.Browser"
fn desktop_id_to_flatpak_id(desktop_id: &str) -> Option<String> {
    let id = desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id);
    // Flatpak IDs always contain at least one dot
    if id.contains('.') {
        Some(id.to_string())
    } else {
        None
    }
}

/// Find the active Chromium profile directory by reading `Local State`.
fn find_chromium_profile(chromium_root: &Path) -> Option<PathBuf> {
    let local_state_path = chromium_root.join("Local State");
    if local_state_path.exists() {
        if let Ok(data) = fs::read_to_string(&local_state_path) {
            let v: Value = serde_json::from_str(&data).ok()?;
            // Try last_active_profiles first (Brave uses this)
            if let Some(profiles) = v.pointer("/profile/last_active_profiles") {
                if let Some(arr) = profiles.as_array() {
                    if let Some(name) = arr.first().and_then(|v| v.as_str()) {
                        let p = chromium_root.join(name);
                        if p.is_dir() {
                            return Some(p);
                        }
                    }
                }
            }
            // Try last_used
            if let Some(name) = v.pointer("/profile/last_used").and_then(|v| v.as_str()) {
                let p = chromium_root.join(name);
                if p.is_dir() {
                    return Some(p);
                }
            }
        }
    }
    // Fallback: Default profile
    let default = chromium_root.join("Default");
    if default.is_dir() {
        return Some(default);
    }
    // Fallback: first Profile_* directory
    let entries = fs::read_dir(chromium_root).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("Profile ") {
            return Some(entry.path());
        }
    }
    None
}

/// Parse a Chromium Preferences JSON for the default search engine template.
///
/// Chromium only *writes* `default_search_provider` once the user picks an
/// engine — a profile that never changed it has no entry at all, and treating
/// that as "unknown" is how a stock profile used to end up on DuckDuckGo. The
/// built-in default is Google, so that is what an unwritten profile gets.
fn parse_chromium_prefs(prefs_path: &Path) -> Option<Detected> {
    let data = fs::read_to_string(prefs_path).ok()?;
    let v: Value = serde_json::from_str(&data).ok()?;
    let icon = v
        .pointer("/default_search_provider/favicon_url")
        .or_else(|| {
            v.pointer("/default_search_provider_data/template_url_data/icon_url")
        })
        .and_then(|v| v.as_str())
        .map(|url| {
            if url.starts_with("data:") {
                EngineIcon::Data(url.to_string())
            } else {
                EngineIcon::Url(url.to_string())
            }
        });

    // Modern path: default_search_provider_data.template_url_data
    if let Some(tud) = v.pointer("/default_search_provider_data/template_url_data") {
        if let Some(url) = tud.get("url").and_then(|v| v.as_str()) {
            if is_search_template(url) {
                let name = tud
                    .get("short_name")
                    .or_else(|| tud.get("keyword"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Google")
                    .to_string();
                return Some(Detected {
                    name,
                    url_template: url.to_string(),
                    icon,
                });
            }
        }
    }

    // Legacy path: default_search_provider.search_url
    if let Some(search_url) = v
        .pointer("/default_search_provider/search_url")
        .and_then(|v| v.as_str())
    {
        if is_search_template(search_url) {
            let name = v
                .pointer("/default_search_provider/name")
                .and_then(|v| v.as_str())
                .unwrap_or("Google")
                .to_string();
            return Some(Detected {
                name,
                url_template: search_url.to_string(),
                icon,
            });
        }
    }

    // No engine was ever chosen: the browser's own default is Google.
    Some(Detected {
        name: "Google".into(),
        url_template: "https://www.google.com/search?q={searchTerms}".into(),
        icon,
    })
}

fn is_search_template(url: &str) -> bool {
    url.contains("{searchTerms}") || url.contains("%s")
}

// ── GNOME Web ─────────────────────────────────────────────────────────────────

fn is_epiphany(desktop_id: &str) -> bool {
    let id = desktop_id.to_lowercase();
    id.contains("epiphany")
}

/// GNOME Web keeps the selected engine by name and the whole provider list in
/// GSettings, so the URL comes from matching the two — read through GLib so the
/// GVariant is typed rather than scraped out of `gsettings get` output.
fn detect_epiphany() -> Option<Found> {
    let schema = gio::SettingsSchemaSource::default()?
        .lookup("org.gnome.Epiphany", true)?;
    let settings = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None);
    let wanted = settings.string("default-search-engine").to_string();
    let providers = settings.value("search-engine-providers");
    for entry in providers.iter() {
        let name = provider_field(&entry, "name");
        let url = provider_field(&entry, "url");
        if let (Some(name), Some(url)) = (name, url) {
            if name == wanted && is_search_template(&url) {
                return Some(Found::from(Detected {
                    name,
                    url_template: url,
                    icon: None,
                }));
            }
        }
    }
    None
}

/// Pull one string field out of an `a{sv}` GVariant value.
fn provider_field(entry: &glib::Variant, key: &str) -> Option<String> {
    entry
        .child_value(1)
        .child_value(0)
        .iter()
        .find_map(|kv| {
            let kv_key = kv.child_value(0).get::<String>()?;
            (kv_key == key)
                .then(|| kv.child_value(1).get::<String>())
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literals-only LZ4 block — enough to hand-build a mozLz4 fixture.
    /// Lengths past 15 continue in bytes of 255, which is why this can't be a
    /// single byte.
    fn lz4_literals(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x0f << 4];
        let mut rest = data.len() - 15;
        while rest >= 255 {
            out.push(255);
            rest -= 255;
        }
        out.push(rest as u8);
        out.extend_from_slice(data);
        out
    }

    fn mozlz4(data: &[u8]) -> Vec<u8> {
        let mut file = b"mozLz40\0".to_vec();
        file.extend_from_slice(&(data.len() as u32).to_le_bytes());
        file.extend_from_slice(&lz4_literals(data));
        file
    }

    fn detected(name: &str, url: &str) -> Detected {
        Detected {
            name: name.into(),
            url_template: url.into(),
            icon: None,
        }
    }

    #[test]
    fn substitutes_every_placeholder_a_browser_uses() {
        let encoded = urlencoding::encode("hello world").into_owned();
        for template in [
            "https://kagi.com/search?q={searchTerms}",
            "https://example.com/search?q=%s",
            "https://example.com/?q={query}",
        ] {
            let d = detected("Kagi", template);
            let url = d.url_for("hello world").expect("a search URL");
            assert!(url.contains(&encoded), "{template} → {url}");
        }
        // A template with no placeholder can't carry a query — better to report
        // "unknown" than to open a page that ignores the search.
        assert!(detected("X", "https://example.com/").url_for("q").is_none());
    }

    #[test]
    fn recognises_the_firefox_family() {
        for id in [
            "firefox.desktop",
            "firefox_fedora.desktop",
            "org.mozilla.firefox.desktop",
            "io.gitlab.librewolf-community.desktop",
        ] {
            assert!(is_firefox(id), "{id} should be a Firefox profile");
        }
        assert!(!is_firefox("com.google.Chrome.desktop"));
        assert!(!is_firefox("org.gnome.Epiphany.desktop"));
    }

    #[test]
    fn maps_chromium_desktop_ids() {
        assert_eq!(
            chromium_config_dir("com.brave.Browser.desktop"),
            Some("BraveSoftware/Brave-Browser")
        );
        assert_eq!(
            chromium_config_dir("com.google.Chrome.desktop"),
            Some("google-chrome")
        );
        assert_eq!(
            chromium_config_dir("org.chromium.Chromium.desktop"),
            Some("chromium")
        );
        assert_eq!(
            chromium_config_dir("com.microsoft.Edge.desktop"),
            Some("microsoft-edge")
        );
        assert_eq!(
            chromium_config_dir("com.vivaldi.Vivaldi.desktop"),
            Some("vivaldi")
        );
        assert_eq!(
            chromium_config_dir("com.opera.Opera.desktop"),
            Some("opera")
        );
        assert_eq!(
            chromium_config_dir("ru.yandex.Browser.desktop"),
            Some("yandex-browser")
        );
        // Non-Chromium browsers
        assert_eq!(chromium_config_dir("org.mozilla.firefox.desktop"), None);
        assert_eq!(
            chromium_config_dir("io.gitlab.librewolf-community.desktop"),
            None
        );
    }

    #[test]
    fn chromium_prefs_give_the_chosen_engine_and_its_icon() {
        let json = r#"{
            "default_search_provider_data": {
                "template_url_data": {
                    "url": "https://kagi.com/search?q={searchTerms}",
                    "short_name": "Kagi",
                    "icon_url": "data:image/png;base64,iVBORw0KGgo="
                }
            }
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let url = v
            .pointer("/default_search_provider_data/template_url_data/url")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(is_search_template(url));
        assert_eq!(
            v.pointer("/default_search_provider_data/template_url_data/short_name")
                .unwrap()
                .as_str(),
            Some("Kagi")
        );
    }

    #[test]
    fn a_stock_chromium_profile_gets_the_browsers_own_default() {
        // This is a real Chrome profile with no `default_search_provider`:
        // detection used to give up here and silently fall back to DuckDuckGo.
        let json = r#"{
            "default_search_provider": {
                "choice_screen_completion_program": "Omnibox.Suggestions"
            }
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        assert!(v.pointer("/default_search_provider/search_url").is_none());
        assert!(engine_template_for("google").is_some());
    }

    #[test]
    fn firefox_engines_resolve_to_a_real_search_url() {
        // Shipped engines carry only an id + name, so the URL comes from the
        // table — including regional variants, which keep the stem.
        for (id, host) in [
            ("google", "google.com"),
            ("ddg", "duckduckgo.com"),
            ("bing", "bing.com"),
            ("startpage", "startpage.com"),
            ("ecosia", "ecosia.org"),
            ("brave", "search.brave.com"),
            ("perplexity", "perplexity.ai"),
            ("wikipedia", "wikipedia.org"),
            ("qwant", "qwant.com"),
            ("mojeek", "mojeek.com"),
            ("yandex", "yandex.com"),
            ("kagi", "kagi.com"),
        ] {
            let template = engine_template_for(id)
                .unwrap_or_else(|| panic!("{id} has no search URL"));
            assert!(template.contains(host), "{id} → {template}");
            assert!(is_search_template(&template), "{id} → {template}");
        }
        // Regional and unknown ids.
        assert!(engine_template_for("wikipedia-de").is_some());
        assert!(engine_template_for("some-engine-nobody-knows").is_none());
    }

    #[test]
    fn the_users_choice_beats_the_apps_default() {
        // The real shape of Firefox's search.json.mozlz4 (config v2): shipped
        // engines are id + display name only, and the selection lives in
        // metaData.
        let json = br#"{
            "version": 13,
            "engines": [
                {"id": "google", "_name": "Google", "_isConfigEngine": true, "_metaData": {}},
                {"id": "ddg", "_name": "DuckDuckGo", "_isConfigEngine": true, "_metaData": {}},
                {"id": "kagi",
                 "name": "Kagi",
                 "urls": [{"type": "text/html", "template": "https://kagi.com/search?q={searchTerms}"},
                          {"type": "application/x-suggestions+json", "template": "https://kagi.com/api/x"}],
                 "iconMapObj": {"32": "data:image/png;base64,iVBORw0KGgo=",
                                "16": "data:image/png;base64,AA=="}}
            ],
            "metaData": {"locale": "en-US", "appDefaultEngineId": "google", "defaultEngineId": "kagi"}
        }"#;
        let parsed: SearchJson = serde_json::from_slice(json).unwrap();
        let engine = parsed
            .meta
            .default_engine
            .as_deref()
            .and_then(|id| parsed.engines.iter().find(|e| e.id == id))
            .expect("the chosen engine");
        assert_eq!(
            engine.private_name.clone().or_else(|| engine.name.clone()),
            Some("Kagi".into()),
            "the display name, whichever key this engine uses"
        );
        assert_eq!(
            engine.template().unwrap(),
            "https://kagi.com/search?q={searchTerms}"
        );
        // The inline icon is used as-is — no network needed.
        assert_eq!(
            engine.icon(),
            Some(EngineIcon::Data("data:image/png;base64,iVBORw0KGgo=".into()))
        );

        // With no user choice, the app default is what the browser searches with.
        let json = br#"{
            "engines": [{"id": "ddg", "_name": "DuckDuckGo", "_isConfigEngine": true, "_metaData": {}}],
            "metaData": {"appDefaultEngineId": "ddg"}
        }"#;
        let parsed: SearchJson = serde_json::from_slice(json).unwrap();
        let id = parsed
            .meta
            .default_engine
            .as_deref()
            .or(parsed.meta.app_default_engine.as_deref())
            .unwrap();
        assert_eq!(id, "ddg");
        let engine = &parsed.engines[0];
        assert_eq!(engine.private_name.as_deref(), Some("DuckDuckGo"));
        assert!(engine.template().unwrap().contains("duckduckgo.com"));
        assert!(engine.icon().is_none());
    }

    /// A whole Firefox profile file, end to end: mozLz4 framing, JSON, the
    /// engine the user picked, and the icon stored inline beside it.
    #[test]
    fn reads_a_whole_firefox_search_file() {
        let json = br#"{
            "version": 13,
            "engines": [
                {"id": "google", "_name": "Google", "_isConfigEngine": true, "_metaData": {}},
                {"id": "myengine",
                 "name": "My Engine",
                 "urls": [{"type": "text/html", "template": "https://search.example.org/find?q={searchTerms}"}],
                 "iconURL": "data:image/png;base64,iVBORw0KGgo="}
            ],
            "metaData": {"appDefaultEngineId": "google", "defaultEngineId": "myengine"}
        }"#;
        let dir = std::env::temp_dir().join(format!("spotty-ffjson-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("search.json.mozlz4");
        // Literals-only LZ4 block, which is all a hand-built fixture needs.
        fs::write(&path, mozlz4(json)).unwrap();

        let detected = parse_search_json_mozlz4(&path).expect("the user's engine");
        assert_eq!(detected.name, "My Engine");
        assert_eq!(
            detected.url_for("hello world").as_deref(),
            Some("https://search.example.org/find?q=hello%20world")
        );
        assert_eq!(
            detected.icon,
            Some(EngineIcon::Data("data:image/png;base64,iVBORw0KGgo=".into()))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decodes_mozlz4_the_way_firefox_writes_it() {
        // mozLz40\0 + LE size + an LZ4 *block*. Built by hand, since the format
        // only exists inside the browser's profile: one block with a long run of
        // literals (the 0xFF-terminated length extension) and one with a
        // back-reference that overlaps its own output.
        let json = br#"{"metaData":{"defaultEngineId":"google"},"engines":[]}"#;

        // Long literals: token nibble 15 means "length continues in the bytes
        // that follow", each adding 255 while it is 0xFF.
        assert_eq!(decode_mozlz4(&mozlz4(json)).as_deref(), Some(json.as_slice()));

        // A match: "ab", then 8 more bytes copied from offset 2 — the copy runs
        // past what was written, which is exactly what makes "ab" repeat.
        let mut block = vec![(2 << 4) | (8 - 4)];
        block.extend_from_slice(b"ab");
        block.extend_from_slice(&2u16.to_le_bytes());
        let mut file = b"mozLz40\0".to_vec();
        file.extend_from_slice(&10u32.to_le_bytes());
        file.extend_from_slice(&block);
        assert_eq!(decode_mozlz4(&file).as_deref(), Some(&b"ababababab"[..]));

        // The same block without the size header (older layout).
        let mut file = b"mozLz40\0".to_vec();
        file.extend_from_slice(&block);
        assert_eq!(decode_mozlz4(&file).as_deref(), Some(&b"ababababab"[..]));

        // Wrong magic, a corrupt offset and a truncated block all fail cleanly
        // instead of panicking — this runs on the state behind every keystroke.
        assert!(decode_mozlz4(b"not-moz-lz4-at-all").is_none());
        assert!(decode_mozlz4(b"mozLz40\0").is_none());
        let mut bad = b"mozLz40\0".to_vec();
        bad.extend_from_slice(&10u32.to_le_bytes());
        bad.extend_from_slice(&[0x10, b'a', 0xff, 0x00]); // offset past the output
        assert!(decode_mozlz4(&bad).is_none());
    }

    #[test]
    fn a_changed_profile_is_stale_and_an_untouched_one_is_not() {
        // "Even if the user changes it": the cache is invalidated by the file
        // being rewritten, not by a timer.
        let dir = std::env::temp_dir().join(format!("spotty-stale-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let prefs = dir.join("Preferences");
        fs::write(&prefs, "{}").unwrap();
        let fresh = stamp_of(&prefs);

        let cache = Cache {
            at: Instant::now(),
            stamps: vec![fresh.clone()],
            detected: Some(detected("Google", "https://google.com/?q=%s")),
        };
        assert!(!stale(Some(&cache)), "nothing changed, no re-detection");

        // The browser rewrites its search settings → a newer mtime → the cache
        // (which still holds the old stamp) goes stale.
        fs::File::options()
            .write(true)
            .open(&prefs)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        assert_ne!(stamp_of(&prefs), fresh, "the test needs a newer mtime");
        assert!(stale(Some(&cache)), "a rewritten profile must be read again");

        // …and an old cache is re-read even when the files are untouched.
        assert!(stale(Some(&Cache {
            at: Instant::now() - TTL - Duration::from_secs(1),
            ..cache
        })));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_firefox_profile_the_browser_itself_would_pick() {
        let dir = std::env::temp_dir().join(format!("spotty-ff-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join("t089ycsz.default"));
        fs::write(
            dir.join("profiles.ini"),
            "[Profile1]\nName=default\nIsRelative=1\nPath=t089ycsz.default\nDefault=1\n\n\
             [Profile0]\nName=default-release\nIsRelative=1\nPath=other.default-release\n",
        )
        .unwrap();
        // The `Default=1` section wins…
        assert_eq!(firefox_profile(&dir), Some(dir.join("t089ycsz.default")));
        // …and an `[Install*]` entry wins over it, as Firefox writes on install.
        let _ = fs::create_dir_all(dir.join("Profiles/inst.default"));
        fs::write(
            dir.join("profiles.ini"),
            "[Install4F96D1932A9F858E]\nDefault=Profiles/inst.default\nLocked=1\n\n\
             [Profile1]\nName=default\nIsRelative=1\nPath=t089ycsz.default\nDefault=1\n",
        )
        .unwrap();
        assert_eq!(firefox_profile(&dir), Some(dir.join("Profiles/inst.default")));
        let _ = fs::remove_dir_all(&dir);
    }
}
#[cfg(test)]
mod live {
    //! Read-only checks against the *system* browser. Ignored by default:
    //! `cargo test -- --ignored --nocapture`.
    use super::*;

    #[test]
    #[ignore = "reads the system default browser's settings"]
    fn detects_this_machines_browser() {
        let desktop = default_browser_id().expect("a default browser");
        println!("default browser: {desktop}");
        let found = detect_for_desktop(&desktop);
        match &found {
            Some(f) => {
                println!("engine: {}", f.detected.name);
                println!("template: {}", f.detected.url_template);
                println!("icon: {:?}", f.detected.icon);
                println!(
                    "url: {}",
                    f.detected.url_for("spotty").unwrap_or_default()
                );
                assert!(f.detected.url_for("spotty").is_some());
            }
            None => println!("nothing detected"),
        }
        // Whatever it found must survive the freshness check as "not stale"
        // only while nothing is rewritten — the stamps are the point.
        if let Some(f) = &found {
            let cache = Cache {
                at: Instant::now(),
                stamps: f.stamps.clone(),
                detected: Some(f.detected.clone()),
            };
            assert!(!stale(Some(&cache)), "a fresh read must not look stale");
        }
    }
}
