//! Translate trigger — live translation inside the launcher, modelled on
//! Raycast's Translate command.
//!
//! Engine:
//!  * **default**: Google Translate's free web endpoint
//!    (`translate.googleapis.com`, no account or key). The typed text is
//!    sent there over HTTPS, in the request body (never in argv/URLs);
//!  * **picked in Settings** (`config.translate_provider`): MyMemory,
//!    LibreTranslate, DeepL, Google Cloud, Azure, Yandex or Kagi — see
//!    `translate_api.rs`. Older configs without a pick fall back to
//!    LibreTranslate when `config.translate_endpoint` was changed.
//!
//! Behaviour (Raycast-style, all inside the launcher):
//!  * **live**: the source language is detected and the text is translated
//!    a moment after typing pauses;
//!  * **two-way**: text that already is in the target language is
//!    translated to the second language instead (system language ↔
//!    English, or the picked target ↔ the system language);
//!  * **dictionary**: single words also list alternative translations;
//!  * **pickers**: every language can be picked as target or source — or
//!    type a language after your text ("hello po") and Enter on the
//!    suggestion;
//!  * **Enter on a translation copies it.**

use crate::config::Config;
use crate::i18n::gettext;
use crate::search::translate_api;
use crate::search::{Action, ResultKind, SearchResult};
use gtk::glib;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

/// How long typing must pause before the live translation fires.
const AUTO_DELAY: Duration = Duration::from_millis(400);
/// After a failed call, don't hammer the engine on every keystroke
/// (the wait folds into the debounce delay instead of blocking it).
const FAIL_BACKOFF: Duration = Duration::from_secs(4);
/// …same for the supported-language list of a LibreTranslate server.
const LANGS_BACKOFF: Duration = Duration::from_secs(15);
/// Call budget for one translation: a local engine can stall on its very
/// first request while it warms its models; later calls are quick.
const TRANSLATE_TIMEOUT: u64 = 30;
/// The free endpoint answers quickly or not at all.
const GOOGLE_TIMEOUT: u64 = 10;
/// Google's free endpoint rejects longer text.
const GOOGLE_MAX_CHARS: usize = 5000;
const GOOGLE_URL: &str = "https://translate.googleapis.com/translate_a/single";
/// The command offered when a configured local LibreTranslate isn't running.
const SETUP_CMD: &str = "pip install libretranslate";
/// Dictionary alternatives shown under a single-word translation.
const MAX_ALTS: usize = 6;

/// A language the engine can translate into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LangInfo {
    pub code: String,
    pub name: String,
}

/// One dictionary alternative ("dog" → "Rüde", noun, "male, dog").
#[derive(Debug, Clone, PartialEq, Eq)]
struct Alt {
    word: String,
    pos: String,
    article: Option<String>,
    reverse: Vec<String>,
}

/// One finished translation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Cached {
    translated: String,
    /// Detected (or picked) source language.
    src: Option<String>,
    /// The language actually translated into — differs from the requested
    /// target when the text already was in it (two-way fallback).
    target: String,
    /// Pronunciation of the translation in Latin script ("привет" → privet).
    translit: Option<String>,
    alts: Vec<Alt>,
}

/// A failed attempt; `connect` marks "the engine couldn't be reached".
#[derive(Debug, Clone)]
struct FetchErr {
    msg: String,
    connect: bool,
}

/// Where translations come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    /// Google's free endpoint (the default).
    Google,
    /// A LibreTranslate server the user configured. `api_key` is the old
    /// key from config.json; a key in the keyring takes over when it is empty.
    Libre { endpoint: String, api_key: String },
    /// Another service from [`translate_api::PROVIDERS`] (DeepL, Kagi, …).
    /// Its key is read from the keyring when a request is made.
    Api { id: String, region: String },
}

impl Backend {
    fn translate(&self, text: &str, source: &str, target: &str) -> Result<Cached, FetchErr> {
        let mut c = match self {
            Backend::Google => post_google(text, source, target)?,
            Backend::Libre { endpoint, api_key } => {
                let stored;
                let key = if api_key.is_empty() {
                    stored = stored_key(translate_api::LIBRE).unwrap_or_default();
                    stored.as_str()
                } else {
                    api_key.as_str()
                };
                post_translate(endpoint, text, source, target, key)?
            }
            Backend::Api { id, region } => {
                let key = stored_key(id);
                let t = translate_api::translate(id, key.as_deref(), region, text, source, target)
                    .map_err(|e| FetchErr { msg: e.msg, connect: e.connect })?;
                Cached { translated: t.text, src: t.src, ..Default::default() }
            }
        };
        c.target = target.to_string();
        if c.src.is_none() && source != "auto" {
            c.src = Some(source.to_string());
        }
        Ok(c)
    }
}

/// A service's key from the keyring (cached in memory after the first read).
fn stored_key(id: &str) -> Option<String> {
    match crate::keyring::get(&translate_api::key_account(id)) {
        Ok(key) => key,
        Err(e) => {
            log::warn!("translate: keyring: {e}");
            None
        }
    }
}

/// Fallback picker for LibreTranslate until `/languages` answers — its
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

/// Languages of the free Google endpoint (modern codes; the legacy ones it
/// reports on detection are folded in by [`normalize_code`]).
const GOOGLE_LANGS: &[(&str, &str)] = &[
    ("af", "Afrikaans"), ("sq", "Albanian"), ("am", "Amharic"), ("ar", "Arabic"),
    ("hy", "Armenian"), ("az", "Azerbaijani"), ("eu", "Basque"),
    ("be", "Belarusian"), ("bn", "Bengali"), ("bs", "Bosnian"),
    ("bg", "Bulgarian"), ("ca", "Catalan"), ("zh-cn", "Chinese (Simplified)"),
    ("zh-tw", "Chinese (Traditional)"), ("hr", "Croatian"), ("cs", "Czech"),
    ("da", "Danish"), ("nl", "Dutch"), ("en", "English"), ("eo", "Esperanto"),
    ("et", "Estonian"), ("tl", "Filipino"), ("fi", "Finnish"), ("fr", "French"),
    ("gl", "Galician"), ("ka", "Georgian"), ("de", "German"), ("el", "Greek"),
    ("gu", "Gujarati"), ("ht", "Haitian Creole"), ("ha", "Hausa"),
    ("he", "Hebrew"), ("hi", "Hindi"), ("hu", "Hungarian"), ("is", "Icelandic"),
    ("ig", "Igbo"), ("id", "Indonesian"), ("ga", "Irish"), ("it", "Italian"),
    ("ja", "Japanese"), ("jv", "Javanese"), ("kn", "Kannada"), ("kk", "Kazakh"),
    ("km", "Khmer"), ("ko", "Korean"), ("ku", "Kurdish"), ("ky", "Kyrgyz"),
    ("lo", "Lao"), ("la", "Latin"), ("lv", "Latvian"), ("lt", "Lithuanian"),
    ("lb", "Luxembourgish"), ("mk", "Macedonian"), ("mg", "Malagasy"),
    ("ms", "Malay"), ("ml", "Malayalam"), ("mt", "Maltese"), ("mi", "Maori"),
    ("mr", "Marathi"), ("mn", "Mongolian"), ("my", "Myanmar (Burmese)"),
    ("ne", "Nepali"), ("nb", "Norwegian"), ("ps", "Pashto"), ("fa", "Persian"),
    ("pl", "Polish"), ("pt", "Portuguese"), ("pa", "Punjabi"),
    ("ro", "Romanian"), ("ru", "Russian"), ("sr", "Serbian"), ("si", "Sinhala"),
    ("sk", "Slovak"), ("sl", "Slovenian"), ("so", "Somali"), ("es", "Spanish"),
    ("sw", "Swahili"), ("sv", "Swedish"), ("tg", "Tajik"), ("ta", "Tamil"),
    ("tt", "Tatar"), ("te", "Telugu"), ("th", "Thai"), ("tr", "Turkish"),
    ("uk", "Ukrainian"), ("ur", "Urdu"), ("uz", "Uzbek"), ("vi", "Vietnamese"),
    ("cy", "Welsh"), ("xh", "Xhosa"), ("yi", "Yiddish"), ("yo", "Yoruba"),
    ("zu", "Zulu"),
];

/// Legacy/POSIX code → modern code (GLib and Google report the traditional
/// codes; the pickers use the modern ones).
const LANG_ALIASES: &[(&str, &str)] =
    &[("no", "nb"), ("iw", "he"), ("in", "id"), ("jw", "jv"), ("fil", "tl")];

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
    /// Engine as last seen by [`results`] (for TranslateNow).
    backend: Option<Backend>,
    /// Default target as last seen by [`results`] (for the two-way fallback).
    default_target: String,
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
    locale_base(&name)
});

/// `es_ES.UTF-8` → `es`; Chinese keeps its script (`zh_TW` → `zh-tw`).
fn locale_base(name: &str) -> String {
    let name = name.split(['.', '@']).next().unwrap_or("");
    let mut parts = name.split(['_', '-']);
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    if lang == "zh" {
        let region = parts.next().unwrap_or("").to_ascii_uppercase();
        return if matches!(region.as_str(), "TW" | "HK" | "MO") { "zh-tw" } else { "zh-cn" }
            .into();
    }
    lang
}

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

/// The service picked in Settings. With none picked (older configs): the
/// free Google endpoint, unless a LibreTranslate server was configured (an
/// endpoint other than the untouched default).
fn backend_for(config: &Config) -> Backend {
    let raw = config.translate_endpoint.trim();
    let libre = || Backend::Libre {
        endpoint: normalize_endpoint(raw),
        api_key: config.translate_api_key.clone(),
    };
    match config.translate_provider.trim() {
        "" => {
            let default = Config::default().translate_endpoint;
            if raw.is_empty() || normalize_endpoint(raw) == normalize_endpoint(&default) {
                Backend::Google
            } else {
                libre()
            }
        }
        translate_api::GOOGLE => Backend::Google,
        translate_api::LIBRE => libre(),
        id if translate_api::provider(id).is_some() => Backend::Api {
            id: id.to_string(),
            region: config.translate_region.trim().to_string(),
        },
        _ => Backend::Google,
    }
}

/// Lower-case a code and fold legacy codes into the modern ones.
fn normalize_code(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    LANG_ALIASES
        .iter()
        .find(|(from, _)| *from == c)
        .map(|(_, to)| to.to_string())
        .unwrap_or(c)
}

fn same_lang(a: &str, b: &str) -> bool {
    normalize_code(a) == normalize_code(b)
}

/// Base language of the current system locale (`es_ES.UTF-8` → `es`).
fn system_base() -> String {
    SYSTEM_BASE.clone()
}

/// Map a locale base onto a language the engine actually offers:
/// exact/alias code → primary subtag → code prefix → English → whatever
/// exists.
fn map_to_supported(base: &str, langs: &[LangInfo]) -> String {
    let base = base.to_ascii_lowercase();
    if base.is_empty() {
        return langs.first().map(|l| l.code.clone()).unwrap_or_else(|| "en".into());
    }
    let primary = base.split('-').next().unwrap_or("").to_string();
    let mut cands = vec![base.clone(), primary.clone()];
    for (from, to) in LANG_ALIASES {
        if *from == primary {
            cands.push(to.to_string());
        }
        if *to == primary {
            cands.push(from.to_string());
        }
    }
    for c in &cands {
        if let Some(l) = langs.iter().find(|l| l.code.eq_ignore_ascii_case(c)) {
            return l.code.clone();
        }
    }
    for c in &cands {
        if let Some(l) = langs.iter().find(|l| l.code.to_ascii_lowercase().starts_with(c.as_str())) {
            return l.code.clone();
        }
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

/// Raycast-style second language: when the text already is in `target`,
/// translate it here instead. A picked target falls back to the default
/// (system) language; the default falls back to English.
fn fallback_target(target: &str, default: &str) -> Option<String> {
    if !default.is_empty() && !same_lang(target, default) {
        Some(default.to_string())
    } else if !same_lang(target, "en") {
        Some("en".into())
    } else {
        None
    }
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

fn to_langs(list: &[(&str, &str)]) -> Vec<LangInfo> {
    list.iter()
        .map(|(c, n)| LangInfo { code: (*c).into(), name: (*n).into() })
        .collect()
}

/// Supported languages of the active engine: Google's fixed set, or what
/// the LibreTranslate server reported (else its standard set).
fn langs() -> Vec<LangInfo> {
    let backend = session(|s| s.backend.clone());
    let mut list = match backend {
        Some(Backend::Libre { .. }) => match LANGS.read() {
            Ok(g) if !g.is_empty() => g.clone(),
            _ => to_langs(DEFAULT_LANGS),
        },
        Some(Backend::Api { id, .. }) => {
            to_langs(translate_api::langs(&id).unwrap_or(GOOGLE_LANGS))
        }
        _ => to_langs(GOOGLE_LANGS),
    };
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn lang_name(langs: &[LangInfo], code: &str) -> String {
    langs
        .iter()
        .find(|l| same_lang(&l.code, code))
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

/// Parse LibreTranslate's `POST /translate` → translation + detected source.
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
    Ok(Cached { translated, src, ..Default::default() })
}

/// Parse the free Google endpoint's `dj=1` answer: the sentences joined,
/// the detected source, the pronunciation and dictionary alternatives.
fn parse_google(body: &str) -> Result<Cached, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| gettext("empty response"))?;
    let mut translated = String::new();
    let mut translit = None;
    for s in v.get("sentences").and_then(|s| s.as_array()).into_iter().flatten() {
        if let Some(t) = s.get("trans").and_then(|t| t.as_str()) {
            translated.push_str(t);
        }
        if let Some(t) = s.get("translit").and_then(|t| t.as_str()) {
            if !t.trim().is_empty() {
                translit = Some(t.trim().to_string());
            }
        }
    }
    let translated = translated.trim().to_string();
    if translated.is_empty() {
        return Err(gettext("empty response"));
    }
    let src = v.get("src").and_then(|s| s.as_str()).map(normalize_code);
    let mut alts = Vec::new();
    for group in v.get("dict").and_then(|d| d.as_array()).into_iter().flatten() {
        let pos = group.get("pos").and_then(|p| p.as_str()).unwrap_or("").to_string();
        for e in group.get("entry").and_then(|e| e.as_array()).into_iter().flatten() {
            let Some(word) = e.get("word").and_then(|w| w.as_str()) else { continue };
            let reverse = e
                .get("reverse_translation")
                .and_then(|r| r.as_array())
                .map(|r| {
                    r.iter().filter_map(|x| x.as_str()).take(4).map(String::from).collect()
                })
                .unwrap_or_default();
            let article = e
                .get("previous_word")
                .and_then(|a| a.as_str())
                .filter(|a| !a.is_empty())
                .map(String::from);
            alts.push(Alt { word: word.to_string(), pos: pos.clone(), article, reverse });
        }
    }
    Ok(Cached { translated, src, translit, alts, ..Default::default() })
}

/// The request body for the free endpoint: just the text, form-encoded.
fn google_form(text: &str) -> String {
    format!("q={}", urlencoding::encode(text))
}

/// The free endpoint's URL — languages only; the text goes in the body.
fn google_url(source: &str, target: &str) -> String {
    let hl = match system_base().as_str() {
        "" => "en".to_string(),
        b => b.to_string(),
    };
    format!(
        "{GOOGLE_URL}?client=gtx&sl={}&tl={}&hl={}&dt=t&dt=bd&dt=rm&dj=1",
        urlencoding::encode(source),
        urlencoding::encode(target),
        urlencoding::encode(&hl),
    )
}

/// One HTTPS POST to Google's free endpoint (text over stdin, never argv).
fn post_google(text: &str, source: &str, target: &str) -> Result<Cached, FetchErr> {
    if text.chars().count() > GOOGLE_MAX_CHARS {
        return Err(FetchErr {
            msg: gettext("text is longer than {n} characters")
                .replace("{n}", &GOOGLE_MAX_CHARS.to_string()),
            connect: false,
        });
    }
    let body = google_form(text);
    let out = crate::security::curl_post_form(
        &google_url(source, target),
        body.as_bytes(),
        2_097_152,
        GOOGLE_TIMEOUT,
        true,
    )
    .map_err(|e| FetchErr { msg: e.to_string(), connect: true })?;
    if !out.status.success() {
        let code = out.status.code();
        let connect = matches!(code, Some(6 | 7 | 28 | 35 | 52 | 56));
        return Err(FetchErr {
            msg: match code {
                Some(22) => gettext("the service refused the request — try again shortly"),
                _ if connect => gettext("no connection to translate.googleapis.com"),
                _ => format!("curl ({code:?})"),
            },
            connect,
        });
    }
    parse_google(&String::from_utf8_lossy(&out.stdout))
        .map_err(|msg| FetchErr { msg, connect: false })
}

/// One POST to LibreTranslate; `connect` flags "the engine isn't running"
/// so the UI can say how to start it instead of showing a bare failure.
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

/// One request for a fetch: what to translate and where to fall back to.
#[derive(Clone)]
struct Job {
    text: String,
    source: String,
    target: String,
    fallback: Option<String>,
    backend: Backend,
}

/// Queue the debounced live translation: shortly after the last change,
/// folded together with the failure backoff so nothing hammers the engine.
///
/// A plain sleeping thread, not a glib timer: `schedule_fetch` runs on the
/// background search worker, and `glib::timeout_add_local` panics off the
/// main thread ("default main context already acquired by another thread").
/// Cancelling means bumping [`DEBOUNCE_GEN`] — older sleepers wake, see
/// they are stale, and drop their fetch.
fn schedule_fetch(job: Job) {
    let mut delay = AUTO_DELAY;
    if let Some(at) = session(|s| s.last_fail) {
        let remaining = FAIL_BACKOFF.saturating_sub(at.elapsed());
        if remaining > delay {
            delay = remaining;
        }
    }
    let gen = DEBOUNCE_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        if DEBOUNCE_GEN.load(Ordering::SeqCst) == gen {
            start_fetch(job);
        }
    });
}

/// Fire immediately (Enter on the "Translating…" / failed row).
pub fn translate_now(text: &str, target: &str) {
    // Invalidate whatever the debounce would have fired later.
    DEBOUNCE_GEN.fetch_add(1, Ordering::SeqCst);
    let (backend, default) = session(|s| (s.backend.clone(), s.default_target.clone()));
    let Some(backend) = backend else { return };
    // A retry must not be swallowed by the cached failure.
    let source = active_source();
    if let Ok(mut m) = CACHE.write() {
        let key = (text.to_string(), source.clone(), target.to_string());
        if matches!(m.get(&key), Some(Err(_))) {
            m.remove(&key);
        }
    }
    session(|s| s.last_fail = None);
    start_fetch(Job {
        text: text.to_string(),
        fallback: if source == "auto" { fallback_target(target, &default) } else { None },
        source,
        target: target.to_string(),
        backend,
    });
}

fn start_fetch(job: Job) {
    let key = (job.text.clone(), job.source.clone(), job.target.clone());
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
    std::thread::spawn(move || {
        let mut res = job.backend.translate(&job.text, &job.source, &job.target);
        // Already in the target language → translate the other way instead.
        if let (Ok(c), Some(fb)) = (&res, &job.fallback) {
            if c.src.as_deref().is_some_and(|s| same_lang(s, &job.target)) {
                if let Ok(second) = job.backend.translate(&job.text, &job.source, fb) {
                    res = Ok(second);
                }
            }
        }
        glib::MainContext::default().invoke(move || {
            session(|s| {
                match &res {
                    Ok(_) => {
                        ENGINE_DOWN.store(false, Ordering::SeqCst);
                        s.last_fail = None;
                    }
                    Err(e) => {
                        ENGINE_DOWN.store(e.connect, Ordering::SeqCst);
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

/// Quote pairs that may wrap the text in `"<text>" in <language>`.
const QUOTES: &[(char, char)] =
    &[('"', '"'), ('\'', '\''), ('“', '”'), ('„', '“'), ('«', '»'), ('「', '」'), ('『', '』')];

/// Raycast-style inline target: `"<text>" in english`, `<text> to french`,
/// `<text> into ja`. Returns the text (quotes removed) and the language
/// code. A full language name always counts; a bare code ("in it") only
/// after quoted text, so ordinary sentences are not cut short.
fn inline_target(rest: &str, langs: &[LangInfo]) -> Option<(String, String)> {
    inline_target_with(rest, langs, &[" in ", " to ", " into "])
}

/// [`inline_target`] limited to the given separators.
fn inline_target_with(rest: &str, langs: &[LangInfo], seps: &[&str]) -> Option<(String, String)> {
    // ASCII lowering keeps byte offsets valid for slicing `rest`.
    let lower = rest.to_ascii_lowercase();
    for sep in seps {
        let Some(at) = lower.rfind(sep) else { continue };
        let lang = rest[at + sep.len()..].trim().trim_end_matches(['.', '!', '?']).to_lowercase();
        let head = rest[..at].trim();
        let unquoted = QUOTES.iter().find_map(|(open, close)| {
            head.strip_prefix(*open)?.strip_suffix(*close).map(str::trim)
        });
        let by_name = langs.iter().find(|l| {
            let name = l.name.to_lowercase();
            name == lang || name.split(" (").next() == Some(lang.as_str())
        });
        let found = by_name.or_else(|| {
            unquoted.is_some().then(|| langs.iter().find(|l| same_lang(&l.code, &lang))).flatten()
        });
        let text = unquoted.unwrap_or(head);
        if let Some(l) = found.filter(|_| !text.is_empty()) {
            return Some((text.to_string(), l.code.clone()));
        }
    }
    None
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
    /// A LibreTranslate server is configured (enables the setup hint).
    local_engine: bool,
    /// The target was typed inline ("… in english"): no suggestions for it.
    inline: bool,
}

/// Icon sentinel for the translation card: Spotty draws the first row as a
/// two-pane card (source text → translation, language chips underneath)
/// instead of a plain row. Fields follow, separated by U+001F:
/// source text, source language, target language, state
/// (`ok`/`same`/`pending`/`error`) and a note (transliteration or error).
pub const CARD_ICON: &str = "translate-card";

fn card_icon(text: &str, src: &str, tgt: &str, state: &str, note: &str) -> Option<String> {
    let clean = |v: &str| v.replace('\u{1f}', " ");
    Some(
        [CARD_ICON, &clean(text), &clean(src), &clean(tgt), state, &clean(note)]
            .join("\u{1f}"),
    )
}

fn translation_row(s: &Snapshot) -> SearchResult {
    let picked_src = || match &s.source {
        Some(code) => lang_name(s.langs, code),
        None => gettext("Auto"),
    };
    match &s.cached {
        Some(Ok(c)) => {
            let target = if c.target.is_empty() { s.target } else { c.target.as_str() };
            let tgt_name = lang_name(s.langs, target);
            // Detected source == target and there was no other way to go
            // (English typed on an English system): say so instead of
            // echoing the text back as a fake translation.
            let src_is_target = c.src.as_deref().is_some_and(|x| same_lang(x, target))
                || s.source.as_deref().is_some_and(|x| same_lang(x, target));
            if src_is_target {
                let note = gettext("Already {language}").replace("{language}", &tgt_name);
                return SearchResult {
                    kind: ResultKind::Translate,
                    title: s.rest.to_string(),
                    subtitle: Some(note.clone()),
                    icon: card_icon(s.rest, &tgt_name, &tgt_name, "same", &note),
                    action: Action::CopyToClipboard(s.rest.to_string()),
                    score: 100_000,
                };
            }
            let src = match &c.src {
                Some(code) => lang_name(s.langs, code),
                None => picked_src(),
            };
            let mut subtitle = gettext("{src} → {tgt}")
                .replace("{src}", &src)
                .replace("{tgt}", &tgt_name);
            let translit = c.translit.as_deref().filter(|t| *t != c.translated).unwrap_or("");
            if !translit.is_empty() {
                subtitle.push_str(" · ");
                subtitle.push_str(translit);
            }
            SearchResult {
                kind: ResultKind::Translate,
                title: c.translated.clone(),
                subtitle: Some(subtitle),
                icon: card_icon(s.rest, &src, &tgt_name, "ok", translit),
                action: Action::CopyToClipboard(c.translated.clone()),
                score: 100_000,
            }
        }
        Some(Err(e)) => {
            let short: String = e.chars().take(80).collect();
            SearchResult {
                kind: ResultKind::Translate,
                title: s.rest.to_string(),
                subtitle: Some(format!(
                    "{} — {short} · {}",
                    gettext("Translation unavailable"),
                    gettext("Enter to retry")
                )),
                icon: card_icon(
                    s.rest,
                    &picked_src(),
                    &lang_name(s.langs, s.target),
                    "error",
                    &short,
                ),
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
            icon: card_icon(s.rest, &picked_src(), &lang_name(s.langs, s.target), "pending", ""),
            action: Action::TranslateNow {
                text: s.rest.to_string(),
                target: s.target.to_string(),
            },
            score: 100_000,
        },
    }
}

/// Dictionary alternatives under a single-word translation (Enter copies
/// the word). The main translation itself is not repeated.
fn push_alt_rows(out: &mut Vec<SearchResult>, s: &Snapshot, icon: &Option<String>) {
    let Some(Ok(c)) = &s.cached else { return };
    let alts = c
        .alts
        .iter()
        .filter(|a| !a.word.eq_ignore_ascii_case(&c.translated))
        .take(MAX_ALTS);
    for (idx, a) in alts.enumerate() {
        let mut parts = Vec::new();
        match (&a.article, a.pos.is_empty()) {
            (Some(art), false) => parts.push(format!("{} ({art})", a.pos)),
            (Some(art), true) => parts.push(art.clone()),
            (None, false) => parts.push(a.pos.clone()),
            (None, true) => {}
        }
        if !a.reverse.is_empty() {
            parts.push(a.reverse.join(", "));
        }
        out.push(SearchResult {
            kind: ResultKind::Translate,
            title: a.word.clone(),
            subtitle: (!parts.is_empty()).then(|| parts.join(" · ")),
            icon: icon.clone(),
            action: Action::CopyToClipboard(a.word.clone()),
            score: 99_000 - idx as i32,
        });
    }
}

fn lang_row(
    l: &LangInfo,
    current: &str,
    strip: Option<String>,
    icon: &Option<String>,
    score: i32,
) -> SearchResult {
    let is_current = same_lang(&l.code, current);
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
        let current = s.source.as_deref().is_some_and(|x| same_lang(x, &l.code));
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

    // The translation itself — first row, always — then its alternatives.
    out.push(translation_row(s));
    push_alt_rows(&mut out, s, &icon);
    // Configured local engine unreachable → say how to start it.
    if s.local_engine && s.engine_down && matches!(s.cached, Some(Err(_))) {
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
    let token = if s.inline { None } else { trailing_token(s.rest) };
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

/// Results for the translate trigger (`query` = the text being translated).
///
/// Safe from any thread: the search calls this on its background job
/// worker, while [`set_target`]/[`set_source`]/[`expand`] run on the main
/// thread — all shared state goes through [`session`], and nothing here
/// touches glib timers.
pub fn results(query: &str, config: &Config) -> Vec<SearchResult> {
    results_with(query, config, &[" in ", " to ", " into "], false)
}

/// The translation card for the main search (no trigger word): only for
/// `<text> in <language>`, so nothing else typed there is ever sent to the
/// translation service. Empty when the query does not have that form.
pub fn regular_results(query: &str, config: &Config) -> Vec<SearchResult> {
    results_with(query, config, &[" in "], true).into_iter().take(1).collect()
}

fn results_with(
    query: &str,
    config: &Config,
    seps: &[&str],
    require_inline: bool,
) -> Vec<SearchResult> {
    let backend = backend_for(config);
    let switched = session(|s| {
        let changed = s.backend.as_ref().is_some_and(|b| *b != backend);
        s.backend = Some(backend.clone());
        changed
    });
    if switched {
        // Another service (or server): earlier answers and failures are
        // not this one's.
        if let Ok(mut m) = CACHE.write() {
            m.clear();
        }
        ENGINE_DOWN.store(false, Ordering::SeqCst);
        session(|s| s.last_fail = None);
    }
    if let Backend::Libre { endpoint, .. } = &backend {
        ensure_langs(endpoint);
    }
    let default = target_code(config);
    session(|s| s.default_target = default.clone());
    let (auto, source_pick, expanded) =
        session(|s| (s.target.is_none(), s.source.clone(), s.expanded));
    let source = source_pick.clone().unwrap_or_else(|| "auto".into());
    let langs = langs();
    // `"text" in english` names the target right in the query.
    let inline = inline_target_with(query.trim(), &langs, seps);
    if require_inline && inline.is_none() {
        return Vec::new();
    }
    let (rest, target) = match &inline {
        Some((text, code)) => (text.as_str(), code.clone()),
        None => (query.trim(), active_target(config)),
    };
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
        local_engine: matches!(backend, Backend::Libre { .. }),
        inline: inline.is_some(),
    };
    let rows = render(&snap);
    if !rest.is_empty() && cached.is_none() {
        schedule_fetch(Job {
            text: rest.to_string(),
            // An inline target is explicit: never swap it for the second language.
            fallback: if source == "auto" && inline.is_none() {
                fallback_target(&target, &default)
            } else {
                None
            },
            source,
            target,
            backend,
        });
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
            local_engine: false,
            inline: false,
        }
    }

    #[test]
    fn inline_target_reads_quoted_text_and_language() {
        let l = sample_langs();
        let q = "\"翻訳者は言葉の魔術師である\" in english";
        assert_eq!(
            inline_target(q, &l),
            Some(("翻訳者は言葉の魔術師である".into(), "en".into()))
        );
        assert_eq!(inline_target("good morning to French", &l), Some(("good morning".into(), "fr".into())));
        assert_eq!(inline_target("“bonjour” into en", &l), Some(("bonjour".into(), "en".into())));
        // A bare code only counts after quoted text.
        assert_eq!(inline_target("I'm in pt", &l), None);
        assert_eq!(inline_target("going to the park", &l), None);
        assert_eq!(inline_target("\"\" in english", &l), None);
        // Multi-byte text before an ASCII separator slices cleanly.
        assert_eq!(inline_target("İstanbul in german", &l), Some(("İstanbul".into(), "de".into())));
    }

    #[test]
    fn main_search_translates_only_text_in_a_language() {
        let l = sample_langs();
        let main = [" in "];
        assert!(inline_target_with("hello", &l, &main).is_none());
        assert!(inline_target_with("good morning to french", &l, &main).is_none());
        assert!(inline_target_with("living in paris", &l, &main).is_none());
        assert_eq!(
            inline_target_with("good morning in french", &l, &main),
            Some(("good morning".into(), "fr".into()))
        );
        let config = Config::default();
        assert!(regular_results("firefox", &config).is_empty());
        assert!(regular_results("hello to french", &config).is_empty());
    }

    #[test]
    fn inline_target_hides_language_suggestions() {
        let l = sample_langs();
        let mut s = snap("hello", "fr", &l);
        s.inline = true;
        let rows = render(&s);
        assert!(!rows.iter().any(|r| r.title == "Translate to French"));
    }

    #[test]
    fn translation_row_carries_the_card_fields() {
        let l = sample_langs();
        let mut s = snap("bonjour", "en", &l);
        s.cached = Some(Ok(Cached {
            translated: "hello".into(),
            src: Some("fr".into()),
            target: "en".into(),
            ..Default::default()
        }));
        let rows = render(&s);
        let icon = rows[0].icon.as_deref().unwrap();
        let fields: Vec<&str> = icon.split('\u{1f}').collect();
        assert_eq!(fields, [CARD_ICON, "bonjour", "French", "English", "ok", ""]);
        assert_eq!(rows[0].title, "hello");

        let pending = render(&snap("bon\u{1f}jour", "en", &l));
        let fields: Vec<&str> = pending[0].icon.as_deref().unwrap().split('\u{1f}').collect();
        assert_eq!(fields[1], "bon jour");
        assert_eq!(fields[4], "pending");
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
            target: "en".into(),
            ..Default::default()
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

        // Google failing: no LibreTranslate setup hint.
        s.engine_down = true;
        let rows = render(&s);
        assert!(!rows.iter().any(|r| r.title == "Local translation engine not reachable"));

        s.local_engine = true;
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
            target: "en".into(),
            ..Default::default()
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
            target: "en".into(),
            ..Default::default()
        }));
        let rows = render(&s);
        assert!(rows[0].subtitle.as_deref().unwrap().contains("Already English"));
    }

    #[test]
    fn default_endpoint_uses_the_free_google_backend() {
        let mut cfg = Config::default();
        assert_eq!(backend_for(&cfg), Backend::Google);
        cfg.translate_endpoint = String::new();
        assert_eq!(backend_for(&cfg), Backend::Google);
        cfg.translate_endpoint = "https://mt.example.org/".into();
        cfg.translate_api_key = "k".into();
        assert_eq!(
            backend_for(&cfg),
            Backend::Libre { endpoint: "https://mt.example.org".into(), api_key: "k".into() }
        );
    }

    #[test]
    fn locale_base_keeps_the_chinese_script() {
        assert_eq!(locale_base("es_ES.UTF-8"), "es");
        assert_eq!(locale_base("zh_TW.UTF-8"), "zh-tw");
        assert_eq!(locale_base("zh_CN"), "zh-cn");
        assert_eq!(locale_base("C"), "c");
        let g = to_langs(GOOGLE_LANGS);
        assert_eq!(map_to_supported("zh-tw", &g), "zh-tw");
        assert_eq!(map_to_supported("no", &g), "nb");
        // LibreTranslate only has plain "zh".
        assert_eq!(map_to_supported("zh-tw", &to_langs(DEFAULT_LANGS)), "zh");
    }

    #[test]
    fn two_way_fallback_targets() {
        // Picked German, typed German → system language.
        assert_eq!(fallback_target("de", "es").as_deref(), Some("es"));
        // System language typed → English.
        assert_eq!(fallback_target("es", "es").as_deref(), Some("en"));
        // English everywhere → nowhere to go.
        assert_eq!(fallback_target("en", "en"), None);
        assert_eq!(fallback_target("he", "iw").as_deref(), Some("en"));
    }

    #[test]
    fn parse_google_reads_sentences_translit_and_dictionary() {
        let body = r#"{"sentences":[{"trans":"Hallo Welt. ","orig":"Hello world. "},
            {"trans":"Wie geht's?","orig":"How are you?"},{"translit":"privet","src_translit":"x"}],
            "dict":[{"pos":"noun","terms":["Hund","Rüde"],"entry":[
              {"word":"Hund","reverse_translation":["dog","hound"],"previous_word":"der"},
              {"word":"Rüde","reverse_translation":["male","dog"],"previous_word":"der"}]}],
            "src":"iw","confidence":1.0}"#;
        let c = parse_google(body).unwrap();
        assert_eq!(c.translated, "Hallo Welt. Wie geht's?");
        assert_eq!(c.src.as_deref(), Some("he")); // legacy code folded
        assert_eq!(c.translit.as_deref(), Some("privet"));
        assert_eq!(c.alts.len(), 2);
        assert_eq!(c.alts[1].word, "Rüde");
        assert_eq!(c.alts[1].article.as_deref(), Some("der"));
        assert!(parse_google("{}").is_err());
        assert!(parse_google("<html>").is_err());
    }

    #[test]
    fn google_request_keeps_the_text_out_of_the_url() {
        let url = google_url("auto", "zh-tw");
        assert!(url.starts_with("https://translate.googleapis.com/"));
        assert!(url.contains("sl=auto") && url.contains("tl=zh-tw") && url.contains("dj=1"));
        assert_eq!(google_form("a b&c=ü"), "q=a%20b%26c%3D%C3%BC");
        let long = "x".repeat(GOOGLE_MAX_CHARS + 1);
        assert!(post_google(&long, "auto", "en").is_err());
    }

    #[test]
    fn two_way_result_names_the_language_it_went_to() {
        let l = sample_langs();
        // Asked for English, text was English → went to French instead.
        let mut s = snap("good morning", "en", &l);
        s.cached = Some(Ok(Cached {
            translated: "bonjour".into(),
            src: Some("en".into()),
            target: "fr".into(),
            ..Default::default()
        }));
        let rows = render(&s);
        assert_eq!(rows[0].title, "bonjour");
        assert!(rows[0].subtitle.as_deref().unwrap().contains("English → French"));
    }

    #[test]
    fn dictionary_alternatives_follow_the_translation() {
        let l = sample_langs();
        let mut s = snap("dog", "de", &l);
        s.cached = Some(Ok(Cached {
            translated: "Hund".into(),
            src: Some("en".into()),
            target: "de".into(),
            translit: None,
            alts: vec![
                Alt { word: "Hund".into(), pos: "noun".into(), article: Some("der".into()),
                      reverse: vec!["dog".into()] },
                Alt { word: "Rüde".into(), pos: "noun".into(), article: Some("der".into()),
                      reverse: vec!["male".into(), "dog".into()] },
            ],
        }));
        let rows = render(&s);
        assert_eq!(rows[0].title, "Hund");
        // The main translation is not repeated as an alternative.
        assert_eq!(rows.iter().filter(|r| r.title == "Hund").count(), 1);
        assert_eq!(rows[1].title, "Rüde");
        assert_eq!(rows[1].subtitle.as_deref(), Some("noun (der) · male, dog"));
        assert_eq!(rows[1].action, Action::CopyToClipboard("Rüde".into()));
    }

    #[test]
    fn translit_is_shown_with_the_translation() {
        let l = sample_langs();
        let mut s = snap("hello", "ja", &l);
        s.cached = Some(Ok(Cached {
            translated: "こんにちは".into(),
            src: Some("en".into()),
            target: "ja".into(),
            translit: Some("Kon'nichiwa".into()),
            ..Default::default()
        }));
        let rows = render(&s);
        assert_eq!(rows[0].subtitle.as_deref(), Some("English → Japanese · Kon'nichiwa"));
    }

    /// Hits the real free endpoint: `cargo test --bin spotty -- --ignored live_google`.
    #[test]
    #[ignore = "network"]
    fn live_google_translates_and_detects() {
        let c = Backend::Google.translate("hola mundo", "auto", "en").expect("live call");
        assert_eq!(c.translated.to_lowercase(), "hello world");
        assert_eq!(c.src.as_deref(), Some("es"));
        let c = Backend::Google.translate("dog", "auto", "de").expect("live call");
        assert!(!c.alts.is_empty(), "single words carry dictionary alternatives");
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
