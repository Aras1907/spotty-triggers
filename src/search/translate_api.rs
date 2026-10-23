//! Translation services the translate trigger can use besides Google's free
//! endpoint (that one lives in `translate.rs`).
//!
//! Each service lists whether it needs an API key. Keys come from the
//! desktop keyring ([`crate::keyring`]) and are sent to curl over stdin
//! ([`crate::security::curl_with_headers`]), never in argv or a URL.

use crate::i18n::gettext;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Whether a service works without an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyNeed {
    None,
    Required,
}

/// One selectable translation service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// Stored in `config.translate_provider`.
    pub id: &'static str,
    pub name: &'static str,
    pub key: KeyNeed,
    /// What the key field is called for this service.
    pub key_label: &'static str,
    /// Where the typed text goes.
    pub host: &'static str,
    /// Where to get a key, or other setup advice.
    pub hint: &'static str,
}

pub const GOOGLE: &str = "google";

/// Every service, in the order the settings list them: no key first.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: GOOGLE,
        name: "Google Translate (free)",
        key: KeyNeed::None,
        key_label: "",
        host: "translate.googleapis.com",
        hint: "Free web service, no account needed.",
    },
    Provider {
        id: "mymemory",
        name: "MyMemory",
        key: KeyNeed::None,
        key_label: "",
        host: "api.mymemory.translated.net",
        hint: "Free, no account needed. Up to 500 bytes per text and a daily limit.",
    },
    Provider {
        id: "deepl",
        name: "DeepL",
        key: KeyNeed::Required,
        key_label: "API key",
        host: "api.deepl.com",
        hint: "Free or Pro key from deepl.com/your-account/keys. Free keys end in \":fx\".",
    },
    Provider {
        id: "google-cloud",
        name: "Google Cloud Translation",
        key: KeyNeed::Required,
        key_label: "API key",
        host: "translation.googleapis.com",
        hint: "API key from console.cloud.google.com with the Cloud Translation API enabled.",
    },
    Provider {
        id: "azure",
        name: "Microsoft Translator (Azure)",
        key: KeyNeed::Required,
        key_label: "API key",
        host: "api.cognitive.microsofttranslator.com",
        hint: "Key of an Azure Translator resource. Set the region unless the resource is global.",
    },
    Provider {
        id: "yandex",
        name: "Yandex Translate",
        key: KeyNeed::Required,
        key_label: "API key",
        host: "translate.api.cloud.yandex.net",
        hint: "API key of a Yandex Cloud service account with the translate role.",
    },
    Provider {
        id: "kagi",
        name: "Kagi Translate",
        key: KeyNeed::Required,
        key_label: "Session token",
        host: "translate.kagi.com",
        hint: "Needs a Kagi subscription. Paste the token from your Kagi session link \
               (kagi.com → Settings → Account → Session Link, the part after \"token=\"). \
               Uses Kagi's web API, which Kagi may change.",
    },
];

pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// The keyring account a service's key is stored under.
pub fn key_account(id: &str) -> String {
    format!("translate:{id}")
}

/// "API key required" / "No API key needed".
pub fn key_need_label(need: KeyNeed) -> String {
    match need {
        KeyNeed::None => gettext("No API key needed"),
        KeyNeed::Required => gettext("API key required"),
    }
}

/// DeepL's target languages (codes in the picker's scheme).
const DEEPL_LANGS: &[(&str, &str)] = &[
    ("ar", "Arabic"), ("bg", "Bulgarian"), ("zh-cn", "Chinese (Simplified)"),
    ("zh-tw", "Chinese (Traditional)"), ("cs", "Czech"), ("da", "Danish"),
    ("nl", "Dutch"), ("en", "English"), ("et", "Estonian"), ("fi", "Finnish"),
    ("fr", "French"), ("de", "German"), ("el", "Greek"), ("hu", "Hungarian"),
    ("id", "Indonesian"), ("it", "Italian"), ("ja", "Japanese"), ("ko", "Korean"),
    ("lv", "Latvian"), ("lt", "Lithuanian"), ("nb", "Norwegian"), ("pl", "Polish"),
    ("pt", "Portuguese"), ("ro", "Romanian"), ("ru", "Russian"), ("sk", "Slovak"),
    ("sl", "Slovenian"), ("es", "Spanish"), ("sv", "Swedish"), ("tr", "Turkish"),
    ("uk", "Ukrainian"),
];

/// A service's own language list; None = Google's list.
pub fn langs(id: &str) -> Option<&'static [(&'static str, &'static str)]> {
    (id == "deepl").then_some(DEEPL_LANGS)
}

/// A finished translation from one of these services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translated {
    pub text: String,
    /// Detected source, in the picker's code scheme.
    pub src: Option<String>,
}

/// A failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiErr {
    pub msg: String,
}

fn err(msg: impl Into<String>) -> ApiErr {
    ApiErr { msg: msg.into() }
}

const TIMEOUT: u64 = 15;
const LIMIT: usize = 2_097_152;
const MYMEMORY_MAX_BYTES: usize = 500;
const KAGI_SESSION_TTL: Duration = Duration::from_secs(10 * 60);

/// Translate with service `id`. `source` is "auto" or a picker code.
pub fn translate(
    id: &str,
    key: Option<&str>,
    region: &str,
    text: &str,
    source: &str,
    target: &str,
) -> Result<Translated, ApiErr> {
    let p = provider(id).ok_or_else(|| err(gettext("unknown translation service")))?;
    let key = key.map(str::trim).filter(|k| !k.is_empty());
    if p.key == KeyNeed::Required && key.is_none() {
        return Err(err(gettext("{service} needs an API key — add it in Settings → Translate")
            .replace("{service}", p.name)));
    }
    let key = key.unwrap_or("");
    match id {
        "mymemory" => mymemory(text, source, target),
        "deepl" => deepl(key, text, source, target),
        "google-cloud" => google_cloud(key, text, source, target),
        "azure" => azure(key, region, text, source, target),
        "yandex" => yandex(key, text, source, target),
        "kagi" => kagi(key, text, source, target),
        _ => Err(err(gettext("unknown translation service"))),
    }
}

/// One request; HTTP errors become readable messages.
fn send(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, &str)>,
) -> Result<String, ApiErr> {
    let reply = crate::security::curl_with_headers(url, headers, body, LIMIT, TIMEOUT)
        .map_err(|e| err(e.to_string()))?;
    if let Some(code) = reply.curl_exit {
        if reply.status == 0 {
            return Err(err(if matches!(code, 6 | 7 | 28 | 35 | 52 | 56) {
                gettext("no connection to the translation service")
            } else {
                format!("curl ({code})")
            }));
        }
    }
    let body = String::from_utf8_lossy(&reply.body).into_owned();
    match reply.status {
        200..=299 => Ok(body),
        401 | 403 => Err(err(gettext("the service refused the API key"))),
        429 => Err(err(gettext("too many requests — try again shortly"))),
        456 => Err(err(gettext("the translation quota is used up"))),
        s => Err(err(format!("HTTP {s}{}", service_message(&body)))),
    }
}

/// The error text a service put in its JSON answer, if any (" — …").
fn service_message(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let msg = v
        .pointer("/error/message")
        .or_else(|| v.get("message"))
        .or_else(|| v.get("error"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let msg: String = msg.chars().take(120).collect();
    if msg.is_empty() { String::new() } else { format!(" — {msg}") }
}

fn json(body: &str) -> Result<serde_json::Value, ApiErr> {
    serde_json::from_str(body).map_err(|_| err(gettext("empty response")))
}

fn non_empty(text: Option<&str>) -> Result<String, ApiErr> {
    text.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .ok_or_else(|| err(gettext("empty response")))
}

/// A service's code back to the picker's scheme ("ZH-HANS" → "zh-cn").
fn from_code(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase().replace('_', "-");
    match c.as_str() {
        "zh" | "zh-hans" | "zh-cn" => "zh-cn".into(),
        "zh-hant" | "zh-tw" => "zh-tw".into(),
        "fil" => "tl".into(),
        "iw" => "he".into(),
        "no" => "nb".into(),
        _ => c,
    }
}

// ── MyMemory ────────────────────────────────────────────────────────────

fn mymemory_code(code: &str) -> String {
    match code {
        "auto" => "Autodetect".into(),
        "zh-cn" => "zh-CN".into(),
        "zh-tw" => "zh-TW".into(),
        c => c.into(),
    }
}

fn mymemory(text: &str, source: &str, target: &str) -> Result<Translated, ApiErr> {
    if text.len() > MYMEMORY_MAX_BYTES {
        return Err(err(gettext("MyMemory translates at most 500 bytes at a time")));
    }
    let form = format!(
        "q={}&langpair={}",
        urlencoding::encode(text),
        urlencoding::encode(&format!("{}|{}", mymemory_code(source), mymemory_code(target))),
    );
    let body = send(
        "https://api.mymemory.translated.net/get",
        &[],
        Some(("application/x-www-form-urlencoded", &form)),
    )?;
    match parse_mymemory(&body) {
        // Text already in the target language: MyMemory refuses the pair
        // instead of detecting it. Report it as such, so the caller's
        // two-way fallback translates it the other way.
        Err(e) if source == "auto" && e.msg.to_ascii_uppercase().contains("DISTINCT LANGUAGES") => {
            Ok(Translated { text: text.to_string(), src: Some(target.to_string()) })
        }
        other => other,
    }
}

fn parse_mymemory(body: &str) -> Result<Translated, ApiErr> {
    let v = json(body)?;
    let status = v.get("responseStatus").and_then(|s| {
        s.as_u64().or_else(|| s.as_str().and_then(|x| x.parse().ok()))
    });
    if status != Some(200) {
        let detail = v.get("responseDetails").and_then(|d| d.as_str()).unwrap_or("");
        return Err(err(if detail.is_empty() { gettext("the service refused the request") } else {
            detail.chars().take(120).collect()
        }));
    }
    let data = v.get("responseData");
    Ok(Translated {
        text: non_empty(data.and_then(|d| d.get("translatedText")).and_then(|t| t.as_str()))?,
        src: data
            .and_then(|d| d.get("detectedLanguage"))
            .and_then(|l| l.as_str())
            .map(from_code),
    })
}

// ── DeepL ───────────────────────────────────────────────────────────────

/// Free-plan keys end in ":fx" and use their own host.
fn deepl_url(key: &str) -> &'static str {
    if key.ends_with(":fx") {
        "https://api-free.deepl.com/v2/translate"
    } else {
        "https://api.deepl.com/v2/translate"
    }
}

fn deepl_target(code: &str) -> String {
    match code {
        "en" => "EN-US".into(),
        "pt" => "PT-BR".into(),
        "zh-cn" => "ZH-HANS".into(),
        "zh-tw" => "ZH-HANT".into(),
        c => c.to_ascii_uppercase(),
    }
}

fn deepl_source(code: &str) -> String {
    code.split('-').next().unwrap_or(code).to_ascii_uppercase()
}

fn deepl_body(text: &str, source: &str, target: &str) -> String {
    let mut body = serde_json::json!({ "text": [text], "target_lang": deepl_target(target) });
    if source != "auto" {
        body["source_lang"] = deepl_source(source).into();
    }
    body.to_string()
}

fn deepl(key: &str, text: &str, source: &str, target: &str) -> Result<Translated, ApiErr> {
    let auth = format!("DeepL-Auth-Key {key}");
    let body = send(
        deepl_url(key),
        &[("Authorization", &auth)],
        Some(("application/json", &deepl_body(text, source, target))),
    )?;
    parse_deepl(&body)
}

fn parse_deepl(body: &str) -> Result<Translated, ApiErr> {
    let v = json(body)?;
    let t = v.pointer("/translations/0");
    Ok(Translated {
        text: non_empty(t.and_then(|t| t.get("text")).and_then(|t| t.as_str()))?,
        src: t
            .and_then(|t| t.get("detected_source_language"))
            .and_then(|l| l.as_str())
            .map(from_code),
    })
}

// ── Google Cloud Translation (v2) ───────────────────────────────────────

fn google_cloud_code(code: &str) -> String {
    match code {
        "zh-cn" => "zh-CN".into(),
        "zh-tw" => "zh-TW".into(),
        c => c.into(),
    }
}

fn google_cloud(key: &str, text: &str, source: &str, target: &str) -> Result<Translated, ApiErr> {
    let mut req = serde_json::json!({
        "q": text, "target": google_cloud_code(target), "format": "text",
    });
    if source != "auto" {
        req["source"] = google_cloud_code(source).into();
    }
    let body = send(
        "https://translation.googleapis.com/language/translate/v2",
        &[("X-goog-api-key", key)],
        Some(("application/json", &req.to_string())),
    )?;
    let v = json(&body)?;
    let t = v.pointer("/data/translations/0");
    Ok(Translated {
        text: non_empty(t.and_then(|t| t.get("translatedText")).and_then(|t| t.as_str()))?,
        src: t
            .and_then(|t| t.get("detectedSourceLanguage"))
            .and_then(|l| l.as_str())
            .map(from_code),
    })
}

// ── Microsoft Translator (Azure) ────────────────────────────────────────

fn azure_code(code: &str) -> String {
    match code {
        "zh-cn" => "zh-Hans".into(),
        "zh-tw" => "zh-Hant".into(),
        "tl" => "fil".into(),
        c => c.into(),
    }
}

fn azure_url(source: &str, target: &str) -> String {
    let mut url = format!(
        "https://api.cognitive.microsofttranslator.com/translate?api-version=3.0&to={}",
        urlencoding::encode(&azure_code(target)),
    );
    if source != "auto" {
        url.push_str(&format!("&from={}", urlencoding::encode(&azure_code(source))));
    }
    url
}

fn azure(key: &str, region: &str, text: &str, source: &str, target: &str) -> Result<Translated, ApiErr> {
    let mut headers = vec![("Ocp-Apim-Subscription-Key", key)];
    let region = region.trim();
    if !region.is_empty() && region != "global" {
        headers.push(("Ocp-Apim-Subscription-Region", region));
    }
    let req = serde_json::json!([{ "Text": text }]).to_string();
    let body = send(&azure_url(source, target), &headers, Some(("application/json", &req)))?;
    parse_azure(&body)
}

fn parse_azure(body: &str) -> Result<Translated, ApiErr> {
    let v = json(body)?;
    Ok(Translated {
        text: non_empty(v.pointer("/0/translations/0/text").and_then(|t| t.as_str()))?,
        src: v.pointer("/0/detectedLanguage/language").and_then(|l| l.as_str()).map(from_code),
    })
}

// ── Yandex Translate (Yandex Cloud) ─────────────────────────────────────

fn yandex_code(code: &str) -> String {
    match code {
        "zh-cn" | "zh-tw" => "zh".into(),
        c => c.into(),
    }
}

fn yandex(key: &str, text: &str, source: &str, target: &str) -> Result<Translated, ApiErr> {
    let mut req = serde_json::json!({
        "texts": [text], "targetLanguageCode": yandex_code(target),
    });
    if source != "auto" {
        req["sourceLanguageCode"] = yandex_code(source).into();
    }
    let auth = format!("Api-Key {key}");
    let body = send(
        "https://translate.api.cloud.yandex.net/translate/v2/translate",
        &[("Authorization", &auth)],
        Some(("application/json", &req.to_string())),
    )?;
    let v = json(&body)?;
    let t = v.pointer("/translations/0");
    Ok(Translated {
        text: non_empty(t.and_then(|t| t.get("text")).and_then(|t| t.as_str()))?,
        src: t
            .and_then(|t| t.get("detectedLanguageCode"))
            .and_then(|l| l.as_str())
            .map(from_code),
    })
}

// ── Kagi Translate ──────────────────────────────────────────────────────

/// (user token it was made for, short-lived translate token, made at).
static KAGI_SESSION: LazyLock<Mutex<Option<(String, String, Instant)>>> =
    LazyLock::new(|| Mutex::new(None));

fn kagi_code(code: &str) -> String {
    match code {
        "zh-cn" => "zh".into(),
        "zh-tw" => "zh-TW".into(),
        c => c.into(),
    }
}

/// Exchange the user's Kagi session token for the translate token.
fn kagi_auth(cookie: &str) -> Result<String, ApiErr> {
    let mut slot = KAGI_SESSION.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((owner, token, at)) = slot.as_ref() {
        if owner == cookie && at.elapsed() < KAGI_SESSION_TTL {
            return Ok(token.clone());
        }
    }
    let header = format!("kagi_session={cookie}");
    let body = send(
        "https://translate.kagi.com/api/auth",
        &[("Cookie", &header), ("Referer", "https://translate.kagi.com/")],
        None,
    )?;
    let v = json(&body)?;
    let token = v
        .get("token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| err(gettext("Kagi did not accept the session token")))?
        .to_string();
    *slot = Some((cookie.to_string(), token.clone(), Instant::now()));
    Ok(token)
}

fn kagi(key: &str, text: &str, source: &str, target: &str) -> Result<Translated, ApiErr> {
    let session = kagi_auth(key)?;
    let to = kagi_code(target);
    let req = serde_json::json!({
        "text": text,
        "from": if source == "auto" { "auto".to_string() } else { kagi_code(source) },
        "to": to,
        "stream": false,
        "formality": "default",
        "speaker_gender": "unknown",
        "addressee_gender": "unknown",
        "language_complexity": "standard",
        "translation_style": "natural",
        "context": "",
        "model": "standard",
        "session_token": session,
        "dictionary_language": to,
        "use_definition_context": false,
        "enable_language_features": false,
    })
    .to_string();
    let cookie = format!("kagi_session={key}");
    let body = send(
        "https://translate.kagi.com/api/translate",
        &[("Cookie", &cookie), ("Referer", "https://translate.kagi.com/")],
        Some(("application/json", &req)),
    )?;
    parse_kagi(&body)
}

fn parse_kagi(body: &str) -> Result<Translated, ApiErr> {
    let v = json(body)?;
    Ok(Translated {
        text: non_empty(v.get("translation").and_then(|t| t.as_str()))?,
        src: v.pointer("/detected_language/iso").and_then(|l| l.as_str()).map(from_code),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_says_whether_it_needs_a_key() {
        assert_eq!(provider(GOOGLE).unwrap().key, KeyNeed::None);
        assert_eq!(provider("mymemory").unwrap().key, KeyNeed::None);
        assert!(provider("libretranslate").is_none());
        for id in ["deepl", "google-cloud", "azure", "yandex", "kagi"] {
            assert_eq!(provider(id).unwrap().key, KeyNeed::Required, "{id}");
        }
        let mut ids: Vec<_> = PROVIDERS.iter().map(|p| p.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), PROVIDERS.len());
    }

    #[test]
    fn a_missing_required_key_is_reported_without_a_request() {
        let e = translate("deepl", None, "", "hello", "auto", "de").unwrap_err();
        assert!(e.msg.contains("DeepL needs an API key"));
        let e = translate("kagi", Some("  "), "", "hello", "auto", "de").unwrap_err();
        assert!(e.msg.contains("Kagi Translate needs an API key"));
    }

    #[test]
    fn deepl_codes_and_hosts() {
        assert_eq!(deepl_url("abc:fx"), "https://api-free.deepl.com/v2/translate");
        assert_eq!(deepl_url("abc"), "https://api.deepl.com/v2/translate");
        assert_eq!(deepl_target("en"), "EN-US");
        assert_eq!(deepl_target("zh-tw"), "ZH-HANT");
        assert_eq!(deepl_source("zh-cn"), "ZH");
        let b: serde_json::Value = serde_json::from_str(&deepl_body("hi", "auto", "de")).unwrap();
        assert_eq!(b["text"][0], "hi");
        assert_eq!(b["target_lang"], "DE");
        assert!(b.get("source_lang").is_none());
        let t = parse_deepl(
            r#"{"translations":[{"detected_source_language":"JA","text":"The translator"}]}"#,
        )
        .unwrap();
        assert_eq!(t, Translated { text: "The translator".into(), src: Some("ja".into()) });
    }

    #[test]
    fn azure_and_mymemory_answers_parse() {
        assert_eq!(
            azure_url("auto", "zh-cn"),
            "https://api.cognitive.microsofttranslator.com/translate?api-version=3.0&to=zh-Hans"
        );
        let t = parse_azure(
            r#"[{"detectedLanguage":{"language":"zh-Hans","score":1.0},"translations":[{"text":"hi","to":"en"}]}]"#,
        )
        .unwrap();
        assert_eq!(t.src.as_deref(), Some("zh-cn"));
        let t = parse_mymemory(
            r#"{"responseData":{"translatedText":"Hello","detectedLanguage":"ja"},"responseStatus":200}"#,
        )
        .unwrap();
        assert_eq!(t, Translated { text: "Hello".into(), src: Some("ja".into()) });
        let e = parse_mymemory(r#"{"responseData":{"translatedText":"x"},"responseStatus":"403","responseDetails":"INVALID LANGUAGE PAIR"}"#)
            .unwrap_err();
        assert_eq!(e.msg, "INVALID LANGUAGE PAIR");
        assert!(mymemory(&"a".repeat(501), "auto", "en").is_err());
    }

    #[test]
    fn kagi_answer_parses() {
        let t = parse_kagi(r#"{"translation":"me llamo","detected_language":{"iso":"es","label":"Spanish"}}"#)
            .unwrap();
        assert_eq!(t, Translated { text: "me llamo".into(), src: Some("es".into()) });
    }

    #[test]
    #[ignore = "network: MyMemory's free service"]
    fn live_mymemory_translates_and_detects() {
        let t = translate("mymemory", None, "", "翻訳者は言葉の魔術師である", "auto", "en").unwrap();
        assert!(t.text.to_lowercase().contains("magician"));
        assert_eq!(t.src.as_deref(), Some("ja"));
        // Already English: reported as such, so the two-way fallback runs.
        let t = translate("mymemory", None, "", "hello", "auto", "en").unwrap();
        assert_eq!(t.src.as_deref(), Some("en"));
    }
}
