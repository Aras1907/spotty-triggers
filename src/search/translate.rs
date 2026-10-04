//! Translate trigger — inline, live translation through a **local**
//! LibreTranslate endpoint. The query text is only ever POSTed to the
//! endpoint in `config.translate_endpoint` (127.0.0.1:5000 by default),
//! so translations are 100% private: no Google, no cloud, no telemetry.
//!
//! Behaviour (Raycast-style, all inside the launcher):
//!  * **auto mode**: the source language is detected and the text is
//!    translated to the system language 3 s after typing stops;
//!  * **expanded options**: with the trigger active every language the
//!    engine supports is listed — pick one, or type a language after your
//!    text ("hello po") and Enter on the suggestion ("po" → Portuguese);
//!  * **Enter on the shown translation copies it.**

use crate::config::Config;
use crate::i18n::gettext;
use crate::search::{Action, ResultKind, SearchResult};
use gtk::glib;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

/// How long typing must pause before the auto-translation fires.
const AUTO_DELAY: Duration = Duration::from_secs(3);
/// After a failed call, don't hammer a stopped engine on every keystroke
/// (the wait folds into the debounce delay instead of blocking it).
const FAIL_BACKOFF: Duration = Duration::from_secs(10);
/// …same for the supported-language list.
const LANGS_BACKOFF: Duration = Duration::from_secs(15);
/// Call budget for one translation: the very first request can stall while
/// the engine warms its models, later calls are quick.
const TRANSLATE_TIMEOUT: u64 = 30;
/// The command offered when the engine isn't running.
const SETUP_CMD: &str = "pip install libretranslate";

/// A language the endpoint can translate into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangInfo {
    pub code: String,
    pub name: String,
}

/// One finished translation: what came back + the detected source.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    translated: String,
    src: Option<String>,
}

/// A failed attempt; `connect` marks "the engine couldn't be reached".
#[derive(Debug, Clone)]
struct FetchErr {
    msg: String,
    connect: bool,
}

/// Fallback picker used until `/languages` answers — LibreTranslate's
/// standard set, so the UI is usable before the engine has ever replied.
const DEFAULT_LANGS: &[(&str, &str)] = &[
    ("ar", "Arabic"), ("az", "Azerbaijani"), ("bg", "Bulgarian"),
    ("bn", "Bengali"), ("ca", "Catalan"), ("cs", "Czech"), ("cy", "Welsh"),
    ("da", "Danish"), ("de", "German"), ("el", "Greek"), ("en", "English"),
    ("eo", "Esperanto"), ("es", "Spanish"), ("et", "Estonian"),
    ("fa", "Persian"), ("fi", "Finnish"), ("fr", "French"), ("ga", "Irish"),
    ("gl", "Galician"), ("he", "Hebrew"), ("hi", "Hindi"), ("hu", "Hungarian"),
    ("id", "Indonesian"), ("it", "Italian"), ("ja", "Japanese"),
    ("kk", "Kazakh"), ("ko", "Korean"), ("lt", "Lithuanian"),
    ("lv", "Latvian"), ("ms", "Malay"), ("nb", "Norwegian"),
    ("ne", "Nepali"), ("nl", "Dutch"), ("nn", "Nynorsk"), ("pl", "Polish"),
    ("pt", "Portuguese"), ("ro", "Romanian"), ("ru", "Russian"),
    ("sk", "Slovak"), ("sl", "Slovenian"), ("sq", "Albanian"),
    ("sv", "Swedish"), ("sw", "Swahili"), ("th", "Thai"), ("tr", "Turkish"),
    ("uk", "Ukrainian"), ("ur", "Urdu"), ("vi", "Vietnamese"),
    ("zh", "Chinese"),
];

/// POSIX locale base → engine code aliases (GLib reports the traditional
/// codes; Argos/LibreTranslate uses the modern ones).
const LANG_ALIASES: &[(&str, &str)] = &[("no", "nb"), ("iw", "he"), ("in", "id")];

static LANGS: LazyLock<RwLock<Vec<LangInfo>>> = LazyLock::new(|| RwLock::new(Vec::new()));
/// 0 = needs fetching, 1 = in flight, 2 = loaded.
static LANGS_STATE: AtomicU8 = AtomicU8::new(0);
static LANGS_RETRY_AT: LazyLock<Mutex<Option<Instant>>> =
    LazyLock::new(|| Mutex::new(None));
/// (text, source, target) → result; keyed so a stale answer (or one for a
/// different language direction) can never show up under a newer query.
static CACHE: LazyLock<RwLock<HashMap<(String, String, String), Result<Cached, String>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
/// The last failure was a connection failure → offer the setup row.
static ENGINE_DOWN: AtomicBool = AtomicBool::new(false);

/// Which picker is expanded next to the typed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picker {
    Target,
    Source,
}

/// Session state, shared by **all** threads: the search runs on a
/// background job thread while Enter/`activate()` runs on the main thread,
/// so anything either side writes must be visible to the other. (It used to
/// be `thread_local!` + a glib timer, which panicked on the job thread —
/// "default main context already acquired by another thread" — leaving the
/// UI stuck on the previous rows.)
#[derive(Default)]
struct Session {
    /// Explicit target picked this session; None = auto (system language).
    target: Option<String>,
    /// Explicit source picked this session; None = auto-detect.
    source: Option<String>,
    /// Which language list is expanded next to the typed text.
    expanded: Option<Picker>,
    /// (text, source, target) currently being fetched.
    pending: Option<(String, String, String)>,
    /// Last failed attempt (drives the backoff).
    last_fail: Option<Instant>,
    /// Endpoint + API key as last seen by [`results`] (for TranslateNow).
    endpoint: String,
    api_key: String,
}

static SESSION: LazyLock<Mutex<Session>> = LazyLock::new(|| Mutex::new(Session::default()));
/// Debounce generation: every schedule bumps it, and a woken sleeper only
/// fetches if it is still the newest. Replaces the glib timer — this must
/// work from the search worker thread.
static DEBOUNCE_GEN: AtomicU64 = AtomicU64::new(0);
/// System locale base, resolved once (the locale can't change under a
/// running app, and this is read on every search).
static SYSTEM_BASE: LazyLock<String> = LazyLock::new(|| {
    let name = glib::language_names().first().map(|s| s.to_string()).unwrap_or_default();
    name.split(['_', '.', '@']).next().unwrap_or("").to_ascii_lowercase()
});

/// Run `f` with the session. A poisoned lock (a panic elsewhere) must not
/// brick translations, so recover instead of propagating.
fn session<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    let mut guard = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut guard)
}

/// Make the endpoint usable as a base URL: default when empty, scheme when
/// missing, no trailing slash.
fn normalize_endpoint(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    if s.is_empty() {
        s = Config::default().translate_endpoint;
    }
    if !s.contains("://") {
        s = format!("http://{s}");
    }
    s
}

/// Base language of the current system locale (`es_ES.UTF-8` → `es`).
fn system_base() -> String {
    SYSTEM_BASE.clone()
}

/// Map a locale base onto a language the engine actually offers:
/// alias → exact code → code/name prefix → English → whatever exists.
fn map_to_supported(base: &str, langs: &[LangInfo]) -> String {
    let base = base.to_ascii_lowercase();
    let base = LANG_ALIASES
        .iter()
        .find(|(from, _)| *from == base)
        .map(|(_, to)| to.to_string())
        .unwrap_or(base);
    if base.is_empty() {
        return langs.first().map(|l| l.code.clone()).unwrap_or_else(|| "en".into());
    }
    if langs.iter().any(|l| l.code == base) {
        return base;
    }
    if let Some(l) = langs.iter().find(|l| l.code.starts_with(&base)) {
        return l.code.clone();
    }
    if langs.iter().any(|l| l.code == "en") {
        return "en".into();
    }
    langs.first().map(|l| l.code.clone()).unwrap_or_else(|| "en".into())
}

/// Default target for this lookup: the `translate_target` config override
/// when set, otherwise the system language mapped onto the engine's set.
pub fn target_code(config: &Config) -> String {
    let explicit = config.translate_target.trim();
    if !explicit.is_empty() {
        return explicit.to_ascii_lowercase();
    }
    map_to_supported(&system_base(), &langs())
}

/// Target for the current render: an explicitly picked language wins over
/// the default.
fn active_target(config: &Config) -> String {
    session(|s| s.target.clone()).unwrap_or_else(|| target_code(config))
}

/// Source for the next request: an explicitly picked language, else
/// `auto` — this is the lever that turns auto-detection off by hand.
fn active_source() -> String {
    session(|s| s.source.clone()).unwrap_or_else(|| "auto".into())
}

/// Pick a target language for the session; empty = back to auto.
pub fn set_target(code: &str) {
    session(|s| {
        s.target = if code.is_empty() {
            None
        } else {
            Some(code.to_ascii_lowercase())
        };
        s.expanded = None;
    });
}

/// Pick a source language for the session (empty = automatic detection).
/// Overrides the engine's auto-detection when it guesses wrong.
pub fn set_source(code: &str) {
    session(|s| {
        s.source = if code.is_empty() {
            None
        } else {
            Some(code.to_ascii_lowercase())
        };
        s.expanded = None;
    });
}

/// Expand one of the two language pickers (`source` = source picker).
pub fn expand(source: bool) {
    session(|s| s.expanded = Some(if source { Picker::Source } else { Picker::Target }));
}

/// Supported languages: whatever the engine reported, else the fallback set.
fn langs() -> Vec<LangInfo> {
    let mut list = match LANGS.read() {
        Ok(g) if !g.is_empty() => g.clone(),
        _ => DEFAULT_LANGS
            .iter()
            .map(|(c, n)| LangInfo { code: (*c).into(), name: (*n).into() })
            .collect(),
    };
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn lang_name(langs: &[LangInfo], code: &str) -> String {
    langs
        .iter()
        .find(|l| l.code == code)
        .map(|l| l.name.clone())
        .unwrap_or_else(|| code.to_string())
}

/// Fetch the engine's supported languages once (retries with backoff while
/// it's down) so the picker reflects what this instance can really do.
fn ensure_langs(endpoint: &str) {
    if LANGS_STATE.load(Ordering::SeqCst) == 2 {
        return;
    }
    if LANGS_STATE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return;
    }
    if let Ok(g) = LANGS_RETRY_AT.lock() {
        if let Some(at) = *g {
            if at.elapsed() < LANGS_BACKOFF {
                LANGS_STATE.store(0, Ordering::SeqCst);
                return;
            }
        }
    }
    let endpoint = endpoint.to_string();
    std::thread::spawn(move || {
        let got = crate::triggers::fetch_text(&format!("{endpoint}/languages"))
            .ok()
            .and_then(|body| parse_languages(&body).ok())
            .filter(|l| !l.is_empty());
        match got {
            Some(list) => {
                if let Ok(mut g) = LANGS.write() {
                    *g = list;
                }
                LANGS_STATE.store(2, Ordering::SeqCst);
                glib::MainContext::default().invoke(|| crate::app::refresh_search_window());
            }
            None => {
                LANGS_STATE.store(0, Ordering::SeqCst);
                if let Ok(mut g) = LANGS_RETRY_AT.lock() {
                    *g = Some(Instant::now());
                }
            }
        }
    });
}

/// Parse `GET /languages` → sorted picker entries.
fn parse_languages(body: &str) -> Result<Vec<LangInfo>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| "not json".to_string())?;
    let arr = v.as_array().ok_or_else(|| "not an array".to_string())?;
    let mut out = Vec::new();
    for item in arr {
        let code = item.get("code").and_then(|c| c.as_str()).unwrap_or("");
        if code.is_empty() {
            continue;
        }
        let name =
            item.get("name").and_then(|n| n.as_str()).unwrap_or(code).to_string();
        out.push(LangInfo { code: code.to_string(), name });
    }
    if out.is_empty() {
        return Err("no languages".into());
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Parse `POST /translate` → translation + detected source.
fn parse_translation(body: &str) -> Result<Cached, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| gettext("empty response"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    let translated = v
        .get("translatedText")
        .and_then(|t| t.as_str())
        .ok_or_else(|| gettext("empty response"))?
        .to_string();
    let src = v
        .get("detectedLanguage")
        .and_then(|d| d.get("language"))
        .and_then(|l| l.as_str())
        .map(|s| s.to_string());
    Ok(Cached { translated, src })
}

/// One local POST; `connect` flags "the engine isn't running" so the UI can
/// say how to start it instead of showing a bare failure.
fn post_translate(
    endpoint: &str,
    text: &str,
    source: &str,
    target: &str,
    api_key: &str,
) -> Result<Cached, FetchErr> {
    let mut obj = serde_json::Map::new();
    obj.insert("q".into(), serde_json::Value::String(text.to_string()));
    // "auto" = the engine detects the source; an explicit code overrides
    // auto-detection for this session.
    obj.insert("source".into(), serde_json::Value::String(source.to_string()));
    obj.insert("target".into(), serde_json::Value::String(target.to_string()));
    obj.insert("format".into(), serde_json::json!("text"));
    if !api_key.is_empty() {
        obj.insert("api_key".into(), serde_json::Value::String(api_key.to_string()));
    }
    let body = serde_json::Value::Object(obj).to_string();
    let url = format!("{endpoint}/translate");
    let out = crate::security::curl_request(&url, Some(body.as_bytes()), 1_048_576,
        TRANSLATE_TIMEOUT, true)
        .map_err(|e| FetchErr { msg: e.to_string(), connect: true })?;
    if !out.status.success() {
        let code = out.status.code();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim().to_string();
        return Err(FetchErr {
            msg: if stderr.is_empty() {
                format!("curl ({code:?})")
            } else {
                stderr
            },
            // 6 = couldn't resolve, 7 = refused, 28 = timeout, 52/56 = early
            // close — i.e. nothing usable is listening.
            connect: matches!(code, Some(6 | 7 | 28 | 52 | 56)),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_translation(&text).map_err(|msg| FetchErr { msg, connect: false })
}

type CacheKey = (String, String, String);

fn cache_get(key: &CacheKey) -> Option<Result<Cached, String>> {
    CACHE.read().ok()?.get(key).cloned()
}

fn cache_put(key: CacheKey, v: Result<Cached, String>) {
    if let Ok(mut m) = CACHE.write() {
        if m.len() > 400 {
            m.clear();
        }
        m.insert(key, v);
    }
}

/// Queue the debounced auto-translate: 3 s after the last change, folded
/// together with the failure backoff so nothing hammers a stopped engine.
///
/// A plain sleeping thread, not a glib timer: `schedule_fetch` runs on the
/// background search worker, and `glib::timeout_add_local` panics off the
/// main thread ("default main context already acquired by another thread").
/// Cancelling means bumping [`DEBOUNCE_GEN`] — older sleepers wake, see
/// they are stale, and drop their fetch.
fn schedule_fetch(
    text: &str,
    source: &str,
    target: &str,
    endpoint: &str,
    api_key: &str,
) {
    let mut delay = AUTO_DELAY;
    if let Some(at) = session(|s| s.last_fail) {
        let remaining = FAIL_BACKOFF.saturating_sub(at.elapsed());
        if remaining > delay {
            delay = remaining;
        }
    }
    let gen = DEBOUNCE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let text = text.to_string();
    let source = source.to_string();
    let target = target.to_string();
    let endpoint = endpoint.to_string();
    let api_key = api_key.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        if DEBOUNCE_GEN.load(Ordering::SeqCst) == gen {
            start_fetch(&text, &source, &target, &endpoint, &api_key);
        }
    });
}

/// Fire immediately (Enter on the "Translating…" / failed row).
pub fn translate_now(text: &str, target: &str) {
    // Invalidate whatever the debounce would have fired later.
    DEBOUNCE_GEN.fetch_add(1, Ordering::SeqCst);
    let (endpoint, api_key) = session(|s| (s.endpoint.clone(), s.api_key.clone()));
    if endpoint.is_empty() {
        return;
    }
    start_fetch(text, &active_source(), target, &endpoint, &api_key);
}

fn start_fetch(text: &str, source: &str, target: &str, endpoint: &str, api_key: &str) {
    let key = (text.to_string(), source.to_string(), target.to_string());
    if cache_get(&key).is_some() {
        return;
    }
    let claimed = session(|s| {
        if s.pending.as_ref() == Some(&key) {
            return false;
        }
        s.pending = Some(key.clone());
        true
    });
    if !claimed {
        return;
    }
    let text = text.to_string();
    let source = source.to_string();
    let target = target.to_string();
    let endpoint = endpoint.to_string();
    let api_key = api_key.to_string();
    std::thread::spawn(move || {
        let res = post_translate(&endpoint, &text, &source, &target, &api_key);
        glib::MainContext::default().invoke(move || {
            session(|s| {
                match &res {
                    Ok(_) => ENGINE_DOWN.store(false, Ordering::SeqCst),
                    Err(e) => {
                        if e.connect {
                            ENGINE_DOWN.store(true, Ordering::SeqCst);
                        }
                        s.last_fail = Some(Instant::now());
                    }
                }
                if s.pending.as_ref() == Some(&key) {
                    s.pending = None;
                }
            });
            cache_put(key, res.map_err(|e| e.msg));
            crate::app::refresh_search_window();
        });
    });
}

/// Last whitespace token of the query — the language filter the user may
/// be typing ("hello po" → "po").
fn trailing_token(rest: &str) -> Option<String> {
    let t = rest.trim_end();
    if t.is_empty() {
        return None;
    }
    let tok = t.rsplit(char::is_whitespace).next().unwrap_or("");
    if tok.chars().count() < 2 {
        return None;
    }
    Some(tok.to_string())
}

/// Remove that trailing token again (used when its suggestion is picked).
pub fn strip_tail_token(query: &str, token: &str) -> Option<String> {
    let t = query.trim_end();
    let head = t.strip_suffix(token)?.trim_end();
    if head.is_empty() {
        None
    } else {
        Some(head.to_string())
    }
}

/// Languages matching a typed token: exact code first, then name/code
/// prefixes ("po" → Portuguese). Punctuation is ignored for matching.
pub fn match_langs<'a>(langs: &'a [LangInfo], token: &str) -> Vec<&'a LangInfo> {
    let raw = token
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_ascii_lowercase();
    if raw.chars().count() < 2 {
        return vec![];
    }
    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    for l in langs {
        if l.code == raw {
            exact.push(l);
        } else if l.code.starts_with(&raw) || l.name.to_lowercase().starts_with(&raw) {
            prefix.push(l);
        }
    }
    exact.extend(prefix);
    exact.truncate(6);
    exact
}

/// Everything [`render`] needs — a plain snapshot so tests can drive the
/// rows without config, timers or the network.
struct Snapshot<'a> {
    rest: &'a str,
    target: &'a str,
    /// Target still comes from the system/config default (nothing picked).
    auto: bool,
    /// Explicit source pick; None = auto-detect.
    source: Option<String>,
    langs: &'a [LangInfo],
    expanded: Option<Picker>,
    /// None = not asked yet, Some(Ok) = translation, Some(Err) = failed.
    cached: Option<Result<Cached, String>>,
    /// The endpoint could not be reached last time.
    engine_down: bool,
}

fn translation_row(s: &Snapshot, icon: &Option<String>) -> SearchResult {
    let tgt_name = lang_name(s.langs, s.target);
    match &s.cached {
        Some(Ok(c)) => {
            // Detected source == target (English typed on an English
            // system): there is nothing to translate — say so instead of
            // echoing the text back as a fake translation.
            if c.src.as_deref() == Some(s.target) || s.source.as_deref() == Some(s.target) {
                return SearchResult {
                    kind: ResultKind::Translate,
                    title: s.rest.to_string(),
                    subtitle: Some(
                        gettext("Already {language}").replace("{language}", &tgt_name),
                    ),
                    icon: icon.clone(),
                    action: Action::CopyToClipboard(s.rest.to_string()),
                    score: 100_000,
                };
            }
            let src = match (&c.src, &s.source) {
                (Some(code), _) => lang_name(s.langs, code),
                (None, Some(code)) => lang_name(s.langs, code),
                _ => gettext("Auto"),
            };
            SearchResult {
                kind: ResultKind::Translate,
                title: c.translated.clone(),
                subtitle: Some(
                    gettext("{src} → {tgt}")
                        .replace("{src}", &src)
                        .replace("{tgt}", &tgt_name),
                ),
                icon: icon.clone(),
                action: Action::CopyToClipboard(c.translated.clone()),
                score: 100_000,
            }
        }
        Some(Err(e)) => {
            let short: String = e.chars().take(80).collect();
            SearchResult {
                kind: ResultKind::Translate,
                title: s.rest.to_string(),
                subtitle: Some(format!("{} — {short}", gettext("Translation unavailable"))),
                icon: icon.clone(),
                action: Action::TranslateNow {
                    text: s.rest.to_string(),
                    target: s.target.to_string(),
                },
                score: 100_000,
            }
        }
        None => SearchResult {
            kind: ResultKind::Translate,
            title: s.rest.to_string(),
            subtitle: Some(gettext("Translating…")),
            icon: icon.clone(),
            action: Action::TranslateNow {
                text: s.rest.to_string(),
                target: s.target.to_string(),
            },
            score: 100_000,
        },
    }
}

fn lang_row(
    l: &LangInfo,
    current: &str,
    strip: Option<String>,
    icon: &Option<String>,
    score: i32,
) -> SearchResult {
    let is_current = l.code == current;
    SearchResult {
        kind: ResultKind::Translate,
        title: gettext("Translate to {name}").replace("{name}", &l.name),
        subtitle: Some(format!("{} · {}", l.name, l.code)),
        icon: if is_current {
            Some("emblem-ok-symbolic".into())
        } else {
            icon.clone()
        },
        action: Action::SetTranslateTarget {
            code: l.code.clone(),
            strip: strip.unwrap_or_default(),
        },
        score,
    }
}

/// The source row: "Source: Auto" opens the source picker, an explicit
/// source can be cleared back to automatic detection. This is the manual
/// override for when auto-detection guesses wrong.
fn push_source_rows(
    out: &mut Vec<SearchResult>,
    s: &Snapshot,
    icon: &Option<String>,
    score: i32,
) {
    let (title, subtitle, action) = match &s.source {
        Some(code) => (
            gettext("Source: {name}").replace("{name}", &lang_name(s.langs, code)),
            gettext("Press Enter to detect automatically"),
            Action::SetTranslateSource {
                code: String::new(),
            },
        ),
        None => (
            gettext("Source: {name}").replace("{name}", &gettext("Auto")),
            gettext("Detected automatically — press Enter to choose"),
            Action::TranslateExpand { source: true },
        ),
    };
    out.push(SearchResult {
        kind: ResultKind::Translate,
        title,
        subtitle: Some(subtitle),
        icon: icon.clone(),
        action,
        score,
    });
}

/// Every language as a *source* pick (source-language picker expanded).
fn push_source_lang_rows(
    out: &mut Vec<SearchResult>,
    s: &Snapshot,
    icon: &Option<String>,
    base: i32,
) {
    for (idx, l) in s.langs.iter().enumerate() {
        let current = s.source.as_deref() == Some(l.code.as_str());
        out.push(SearchResult {
            kind: ResultKind::Translate,
            title: gettext("Source: {name}").replace("{name}", &l.name),
            subtitle: Some(format!("{} · {}", l.name, l.code)),
            icon: if current {
                Some("emblem-ok-symbolic".into())
            } else {
                icon.clone()
            },
            action: Action::SetTranslateSource {
                code: l.code.clone(),
            },
            score: base - idx as i32,
        });
    }
}

/// "Back to Auto" + (optionally) the expander row.
fn push_target_rows(
    out: &mut Vec<SearchResult>,
    s: &Snapshot,
    icon: &Option<String>,
    back_score: i32,
    expander_score: i32,
) {
    let tgt_name = lang_name(s.langs, s.target);
    if !s.auto {
        out.push(SearchResult {
            kind: ResultKind::Translate,
            title: gettext("Back to Auto (system language)"),
            subtitle: Some(
                gettext("{src} → {tgt}")
                    .replace("{src}", &gettext("Auto"))
                    .replace("{tgt}", &tgt_name),
            ),
            icon: icon.clone(),
            action: Action::SetTranslateTarget {
                code: String::new(),
                strip: String::new(),
            },
            score: back_score,
        });
    }
    if expander_score > 0 {
        out.push(SearchResult {
            kind: ResultKind::Translate,
            title: gettext("Target: {name}").replace("{name}", &tgt_name),
            subtitle: Some(gettext("Press Enter to choose another language")),
            icon: icon.clone(),
            action: Action::TranslateExpand { source: false },
            score: expander_score,
        });
    }
}

/// The supported languages (skipping `skip`, which the suggestions show).
fn push_lang_rows(
    out: &mut Vec<SearchResult>,
    s: &Snapshot,
    icon: &Option<String>,
    skip: &[String],
    base: i32,
) {
    for (idx, l) in s.langs.iter().enumerate() {
        if skip.iter().any(|c| c == &l.code) {
            continue;
        }
        out.push(lang_row(l, s.target, None, icon, base - idx as i32));
    }
}

fn render(s: &Snapshot) -> Vec<SearchResult> {
    let icon = Some("tools-check-spelling-symbolic".into());
    let locale_icon = Some("preferences-desktop-locale-symbolic".into());
    let mut out: Vec<SearchResult> = Vec::new();

    if s.rest.is_empty() {
        // Trigger active, nothing typed yet: hint + the expanded picker.
        out.push(SearchResult {
            kind: ResultKind::Translate,
            title: gettext("Type text to translate"),
            subtitle: Some(
                gettext("{src} → {tgt}")
                    .replace("{src}", &gettext("Auto"))
                    .replace("{tgt}", &lang_name(s.langs, s.target)),
            ),
            icon: icon.clone(),
            action: Action::EnterMode("translate".into()),
            score: 100_000,
        });
        push_target_rows(&mut out, s, &locale_icon, 95_000, 0);
        push_lang_rows(&mut out, s, &locale_icon, &[], 90_000);
        return out;
    }

    // The translation itself — first row, always.
    out.push(translation_row(s, &icon));
    // Engine unreachable → say how to start it.
    if s.engine_down && matches!(s.cached, Some(Err(_))) {
        out.push(SearchResult {
            kind: ResultKind::Translate,
            title: gettext("Local translation engine not reachable"),
            subtitle: Some(SETUP_CMD.into()),
            icon: locale_icon.clone(),
            action: Action::CopyToClipboard(SETUP_CMD.into()),
            score: 96_000,
        });
    }
    // Typing a language after the text ("hello po") offers it as target.
    let token = trailing_token(s.rest);
    let suggestions: Vec<&LangInfo> = token
        .as_deref()
        .map(|t| match_langs(s.langs, t))
        .unwrap_or_default();
    let sug_codes: Vec<String> = suggestions.iter().map(|l| l.code.clone()).collect();
    for (idx, l) in suggestions.iter().enumerate() {
        out.push(lang_row(l, s.target, token.clone(), &locale_icon, 94_000 - idx as i32));
    }
    let show_expander = suggestions.is_empty() && s.expanded.is_none();
    push_target_rows(&mut out, s, &locale_icon, 93_000, if show_expander { 92_000 } else { 0 });
    // The source row is the manual override for auto-detection; opening its
    // picker turns it into the full list.
    if s.expanded != Some(Picker::Source) {
        push_source_rows(&mut out, s, &locale_icon, 91_000);
    }
    match s.expanded {
        Some(Picker::Target) => {
            push_lang_rows(&mut out, s, &locale_icon, &sug_codes, 75_000);
        }
        Some(Picker::Source) => push_source_lang_rows(&mut out, s, &locale_icon, 74_000),
        None => {}
    }
    out
}

/// Results for the translate trigger (`rest` = the text being translated).
/// Results for the translate trigger (`query` = the text being translated).
///
/// Safe from any thread: the search calls this on its background job
/// worker, while [`set_target`]/[`set_source`]/[`expand`] run on the main
/// thread —
/// all shared state goes through [`session`], and nothing here touches
/// glib timers.
pub fn results(query: &str, config: &Config) -> Vec<SearchResult> {
    let endpoint = normalize_endpoint(&config.translate_endpoint);
    let api_key = config.translate_api_key.clone();
    session(|s| {
        s.endpoint = endpoint.clone();
        s.api_key = api_key.clone();
    });
    ensure_langs(&endpoint);
    let rest = query.trim();
    let target = active_target(config);
    let (auto, source_pick, expanded) =
        session(|s| (s.target.is_none(), s.source.clone(), s.expanded));
    let source = source_pick.clone().unwrap_or_else(|| "auto".into());
    let langs = langs();
    let cached = if rest.is_empty() {
        None
    } else {
        cache_get(&(rest.to_string(), source.clone(), target.clone()))
    };
    let snap = Snapshot {
        rest,
        target: &target,
        auto,
        source: source_pick,
        langs: &langs,
        expanded,
        cached: cached.clone(),
        engine_down: ENGINE_DOWN.load(Ordering::SeqCst),
    };
    let rows = render(&snap);
    if !rest.is_empty() && cached.is_none() {
        schedule_fetch(rest, &source, &target, &endpoint, &api_key);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_langs() -> Vec<LangInfo> {
        [("en", "English"), ("fr", "French"), ("de", "German"),
         ("ja", "Japanese"), ("nb", "Norwegian"), ("pt", "Portuguese")]
            .iter()
            .map(|(c, n)| LangInfo { code: (*c).into(), name: (*n).into() })
            .collect()
    }

    fn snap<'a>(rest: &'a str, target: &'a str, langs: &'a [LangInfo]) -> Snapshot<'a> {
        Snapshot {
            rest,
            target,
            auto: true,
            source: None,
            langs,
            expanded: None,
            cached: None,
            engine_down: false,
        }
    }

    #[test]
    fn endpoint_defaults_to_the_local_instance() {
        assert_eq!(normalize_endpoint(""), "http://localhost:5000");
        assert_eq!(normalize_endpoint("localhost:5000"), "http://localhost:5000");
        assert_eq!(normalize_endpoint("http://127.0.0.1:5000/"), "http://127.0.0.1:5000");
        assert_eq!(normalize_endpoint("https://mt.local"), "https://mt.local");
    }

    #[test]
    fn system_locale_maps_to_a_supported_code() {
        let l = sample_langs();
        assert_eq!(map_to_supported("es", &l), "en"); // not offered → English
        assert_eq!(map_to_supported("pt", &l), "pt");
        assert_eq!(map_to_supported("fr", &l), "fr");
        assert_eq!(map_to_supported("no", &l), "nb"); // alias
        assert_eq!(map_to_supported("", &l), "en");
        assert_eq!(map_to_supported("xy", &l), "en");
    }

    #[test]
    fn explicit_target_overrides_the_system_language() {
        let mut cfg = Config::default();
        cfg.translate_target = String::new();
        // System language drives the default — never empty.
        assert!(!target_code(&cfg).is_empty());
        cfg.translate_target = "DE".into();
        assert_eq!(target_code(&cfg), "de");
    }

    #[test]
    fn parse_translation_reads_text_and_detection() {
        let body = r#"{"detectedLanguage":{"confidence":97.0,"language":"fr"},"translatedText":"Hello!"}"#;
        let c = parse_translation(body).unwrap();
        assert_eq!(c.translated, "Hello!");
        assert_eq!(c.src.as_deref(), Some("fr"));
        // Engine-level error surfaces as an error, not as a translation.
        assert!(parse_translation(r#"{"error":"Invalid target language"}"#).is_err());
        assert!(parse_translation("not json").is_err());
        assert!(parse_translation("{}").is_err());
    }

    #[test]
    fn parse_languages_is_sorted_and_named() {
        let body = r#"[{"code":"pt","name":"Portuguese","targets":["en"]},
                       {"code":"en","name":"English","targets":["es"]},
                       {"code":"de","name":"German","targets":["en"]}]"#;
        let list = parse_languages(body).unwrap();
        let names: Vec<_> = list.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["English", "German", "Portuguese"]);
        assert!(parse_languages("[]").is_err());
        assert!(parse_languages("{}").is_err());
    }

    #[test]
    fn trailing_token_offers_languages_and_strips_them() {
        let l = sample_langs();
        assert_eq!(trailing_token("hello everyone po").as_deref(), Some("po"));
        assert_eq!(trailing_token("   ").is_none(), true);
        assert_eq!(trailing_token("x").is_none(), true); // too short to be one

        let hits = match_langs(&l, "po");
        assert_eq!(hits[0].code, "pt");
        let hits = match_langs(&l, "fr.");
        assert_eq!(hits[0].code, "fr");
        assert!(match_langs(&l, "hello").is_empty());

        assert_eq!(
            strip_tail_token("translate hello po", "po").as_deref(),
            Some("translate hello")
        );
        assert_eq!(strip_tail_token("translate hello", "po"), None);
        assert_eq!(strip_tail_token("po", "po"), None);
    }

    #[test]
    fn empty_query_shows_hint_and_the_expanded_picker() {
        let l = sample_langs();
        let rows = render(&snap("", "en", &l));
        assert_eq!(rows[0].title, "Type text to translate");
        assert!(rows.iter().any(|r| r.title == "Translate to Portuguese"));
        assert!(rows.iter().any(|r| r.title == "Translate to French"));
        // Nothing picked → no "back to Auto" noise.
        assert!(!rows.iter().any(|r| r.title == "Back to Auto (system language)"));
    }

    #[test]
    fn waiting_row_offers_enter_to_translate_now() {
        let l = sample_langs();
        let mut s = snap("hola", "en", &l);
        s.cached = None;
        let rows = render(&s);
        assert_eq!(rows[0].title, "hola");
        assert_eq!(rows[0].subtitle.as_deref(), Some("Translating…"));
        assert!(matches!(
            rows[0].action,
            Action::TranslateNow { .. }
        ));
    }

    #[test]
    fn translated_row_copies_the_translation() {
        let l = sample_langs();
        let mut s = snap("hola", "en", &l);
        s.cached = Some(Ok(Cached {
            translated: "hello".into(),
            src: Some("fr".into()),
        }));
        let rows = render(&s);
        assert_eq!(rows[0].title, "hello");
        assert!(rows[0].subtitle.as_deref().unwrap().contains("French → English"));
        assert_eq!(rows[0].action, Action::CopyToClipboard("hello".into()));
        assert_eq!(rows[0].kind, ResultKind::Translate);
    }

    #[test]
    fn failed_row_offers_retry_and_setup_when_engine_is_down() {
        let l = sample_langs();
        let mut s = snap("hola", "en", &l);
        s.cached = Some(Err("curl (7)".into()));
        let rows = render(&s);
        assert!(rows[0].subtitle.as_deref().unwrap().contains("Translation unavailable"));
        assert!(matches!(rows[0].action, Action::TranslateNow { .. }));

        s.engine_down = true;
        let rows = render(&s);
        let setup = rows
            .iter()
            .find(|r| r.title == "Local translation engine not reachable")
            .expect("setup row while the engine is down");
        assert_eq!(
            setup.action,
            Action::CopyToClipboard(SETUP_CMD.into())
        );
    }

    #[test]
    fn language_suggestions_carry_the_token_to_strip() {
        let l = sample_langs();
        let s = snap("hello everyone po", "en", &l);
        let rows = render(&s);
        let sug = rows
            .iter()
            .find(|r| r.title == "Translate to Portuguese")
            .expect("suggestion row");
        assert_eq!(
            sug.action,
            Action::SetTranslateTarget {
                code: "pt".into(),
                strip: "po".into(),
            }
        );
        // …and the translation row stays on top.
        assert_eq!(rows[0].subtitle.as_deref(), Some("Translating…"));
    }

    #[test]
    fn picked_target_shows_back_to_auto_and_expands_the_list() {
        let l = sample_langs();
        let mut s = snap("hola", "pt", &l);
        s.auto = false;
        let rows = render(&s);
        assert!(rows.iter().any(|r| r.title == "Back to Auto (system language)"));
        // Not expanded, no suggestions → expander row offers the full list.
        assert!(rows
            .iter()
            .any(|r| r.title.starts_with("Target: ") && matches!(r.action, Action::TranslateExpand { source: false })));
        assert!(!rows.iter().any(|r| r.title == "Translate to Japanese"));

        s.expanded = Some(Picker::Target);
        let rows = render(&s);
        assert!(rows.iter().any(|r| r.title == "Translate to Japanese"));
        // Current target is marked with the ok icon.
        let cur = rows
            .iter()
            .find(|r| r.title == "Translate to Portuguese")
            .unwrap();
        assert_eq!(cur.icon.as_deref(), Some("emblem-ok-symbolic"));
    }

#[test]
    fn echo_is_never_shown_as_a_translation() {
        let l = sample_langs();
        let mut s = snap("hello everyone", "en", &l);
        s.cached = Some(Ok(Cached {
            translated: "hello everyone".into(),
            src: Some("en".into()),
        }));
        let rows = render(&s);
        // Detected source == target: say it's already that language.
        assert_eq!(rows[0].title, "hello everyone");
        assert!(rows[0].subtitle.as_deref().unwrap().contains("Already English"));
        assert_eq!(rows[0].action, Action::CopyToClipboard("hello everyone".into()));
    }

    #[test]
    fn source_picker_overrides_autodetect() {
        let l = sample_langs();
        let mut s = snap("hola mundo", "en", &l);

        // Automatic → the source row offers the picker.
        let rows = render(&s);
        let auto = rows
            .iter()
            .find(|r| r.title.starts_with("Source: "))
            .expect("source row while auto-detecting");
        assert_eq!(auto.title, "Source: Auto");
        assert_eq!(auto.action, Action::TranslateExpand { source: true });

        // Picked → back-to-auto row carrying the chosen language.
        s.source = Some("fr".into());
        let rows = render(&s);
        let picked = rows
            .iter()
            .find(|r| r.title.starts_with("Source: "))
            .expect("source row after picking");
        assert_eq!(picked.title, "Source: French");
        assert_eq!(
            picked.action,
            Action::SetTranslateSource {
                code: String::new()
            }
        );

        // Expanded → one selectable row per language, plus the subtitle of
        // the translation uses the picked source.
        s.expanded = Some(Picker::Source);
        let rows = render(&s);
        let src_rows = rows
            .iter()
            .filter(|r| r.title.starts_with("Source: ") || r.action == (Action::SetTranslateSource { code: "fr".into() }))
            .count();
        assert!(src_rows > 3, "the full source list should be shown, got {src_rows}");
        assert!(rows
            .iter()
            .any(|r| r.action == Action::SetTranslateSource { code: "ja".into() }));

        // Explicit source that equals the target → no fake translation.
        s.expanded = None;
        s.source = Some("en".into());
        s.cached = Some(Ok(Cached {
            translated: "hola mundo".into(),
            src: Some("es".into()),
        }));
        let rows = render(&s);
        assert!(rows[0].subtitle.as_deref().unwrap().contains("Already English"));
    }

    #[test]
    fn setup_command_is_offered_as_plain_text() {
        assert_eq!(SETUP_CMD, "pip install libretranslate");
    }

    /// The search calls `results()` on a background job thread. It used to
    /// panic there (glib timer off the main thread), so the job delivered
    /// nothing and the UI stayed stuck on the previous rows — the exact
    /// bug behind "translation doesn't work while typing".
    #[test]
    fn results_survive_a_search_worker_thread() {
        let cfg = Config::default();
        let rows = std::thread::spawn(move || results("hola mundo", &cfg))
            .join()
            .expect("results() must not panic off the main thread");
        assert!(rows.iter().any(|r| r.kind == ResultKind::Translate));
        assert_eq!(rows[0].subtitle.as_deref(), Some("Translating…"));
    }

    /// Real end-to-end path — curl → a local HTTP server → parse. Bound to
    /// 127.0.0.1 only: this exercises the whole request/quote/parse chain
    /// without touching anything external.
    #[test]
    fn post_translate_round_trips_against_a_local_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut data: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let Ok(n) = sock.read(&mut buf) else { break };
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                    let cl = head
                        .lines()
                        .find(|l| l.starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= pos + 4 + cl {
                        break;
                    }
                }
            }
            let _ = tx.send(data);
            let body = r#"{"detectedLanguage":{"confidence":97.0,"language":"es"},"translatedText":"hello"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        });

        let got = post_translate(&format!("http://{addr}"), "hola mundo", "auto", "en", "")
            .expect("local round trip");
        assert_eq!(got.translated, "hello");
        assert_eq!(got.src.as_deref(), Some("es"));

        let req = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the request should reach the local server");
        let req = String::from_utf8_lossy(&req);
        assert!(req.starts_with("POST /translate"), "{req}");
        assert!(req.contains(r#""source":"auto""#), "{req}");
        assert!(req.contains(r#""target":"en""#), "{req}");
        assert!(req.contains("hola mundo"), "{req}");
        assert!(!req.contains("api_key"), "{req}");
    }
}
