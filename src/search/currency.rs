//! Live currency rates (plus crypto and metals) for the converter.
//!
//! The table comes from the free, keyless `currency-api` project — a
//! USD-based list of 300+ codes updated once a day:
//!
//! * primary: `cdn.jsdelivr.net/npm/@fawazahmed0/currency-api/…`
//! * fallback: `latest.currency-api.pages.dev/…`
//!
//! Rates are fetched only when a query actually mentions money (the
//! dictionary's on-demand pattern), stored in memory for 12h and kept
//! on disk (`~/.cache/spotty/currency_usd.json`) so conversions keep
//! working offline — a disk copy up to 7 days old is used when the
//! network is unreachable. A successful fetch bounces the search
//! window through `app::refresh_search_window`, which re-runs the
//! query that triggered it and reveals the row.
//!
//! The same project publishes the plain name of every code
//! (`currencies.json`: `try` → "Turkish Lira"), cached beside the
//! rates in `~/.cache/spotty/currency_names.json` — natural queries
//! like "10 dollars to turkish lira" resolve through it. It is
//! best-effort: without it the static spellings in `convert.rs`
//! still cover the common currencies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const URLS: [&str; 2] = [
    "https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1/currencies/usd.json",
    "https://latest.currency-api.pages.dev/v1/currencies/usd.json",
];
/// The plain-name list every code carries (`try` → "Turkish Lira") —
/// same project, same fallback shape as [`URLS`].
const NAME_URLS: [&str; 2] = [
    "https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1/currencies.json",
    "https://latest.currency-api.pages.dev/v1/currencies.json",
];
/// Fresh enough to skip the network.
const TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// How long a failed fetch may keep serving the previous disk copy.
const STALE_OK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Never retry more often than this, so a dead network doesn't get a
/// curl attempt per keystroke.
const RETRY: Duration = Duration::from_secs(60);

/// A USD-based rate table: one API payload, parsed.
#[derive(Clone)]
pub struct Rates {
    /// Lowercase code → units per USD (the payload always contains
    /// `usd: 1`, which makes cross rates a single division).
    pub map: HashMap<String, f64>,
    /// The payload's `date` field ("2026-09-28") — shown next to rate
    /// lines so "live" is verifiable.
    pub date: String,
    /// When this snapshot was taken (fetch, or disk load).
    pub at: Instant,
    /// Lowercase plain name ("turkish lira", "us dollar") → code,
    /// built from the API's name list. Empty when it hasn't landed
    /// (offline): the static spellings in `convert.rs` still resolve
    /// the common currencies.
    pub names: HashMap<String, String>,
}

impl Rates {
    /// Units of `to` per one `from` (USD base cancels out), or None if
    /// either side is unknown.
    pub fn cross(&self, from: &str, to: &str) -> Option<f64> {
        let f = self.rate(from)?;
        let t = self.rate(to)?;
        if f > 0.0 {
            Some(t / f)
        } else {
            None
        }
    }

    pub fn rate(&self, code: &str) -> Option<f64> {
        self.map.get(&code.to_lowercase()).copied()
    }
}

static RATES: OnceLock<Mutex<Option<Arc<Rates>>>> = OnceLock::new();
static FETCHING: AtomicBool = AtomicBool::new(false);
static LAST_TRY: Mutex<Option<Instant>> = Mutex::new(None);

/// The current table, if one has landed yet (memory or disk).
pub fn cached() -> Option<Arc<Rates>> {
    RATES
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|g| g.clone())
}

/// Start a fetch when rates are missing or past their TTL. Returns
/// immediately; at most one request is in flight and repeats are
/// rate-limited to one attempt per [`RETRY`]. The search window
/// refreshes itself once new rates arrive.
pub fn ensure_loaded() {
    // A missing name list counts as stale too: an upgrade can find
    // fresh rates but no names yet, and a network that comes back
    // shouldn't leave natural queries waiting for the next 12h —
    // RETRY still caps it at one attempt per minute.
    if cached().is_some_and(|r| r.at.elapsed() < TTL && !r.names.is_empty()) {
        return;
    }
    {
        let mut last = match LAST_TRY.lock() {
            Ok(l) => l,
            Err(_) => return, // poisoned: skip, try again next query
        };
        if last.is_some_and(|t| t.elapsed() < RETRY) {
            return;
        }
        *last = Some(Instant::now());
    }
    if FETCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let _ = fetch_once();
        FETCHING.store(false, Ordering::SeqCst);
    });
}

fn fetch_once() -> Option<()> {
    // A fresh disk copy (from this or a previous run) beats the wire.
    let mut r = if let Some(r) = load_disk(TTL) {
        r
    } else {
        let mut body = None;
        for url in URLS {
            if let Ok(text) = crate::triggers::fetch_text(url) {
                if parse(&text).is_some() {
                    body = Some(text);
                    break;
                }
            }
        }
        let fresh = body.and_then(|text| {
            let r = parse(&text);
            if r.is_some() {
                save_disk(&text);
            }
            r
        });
        // Offline: fall back to yesterday's disk copy rather than nothing.
        match fresh.or_else(|| load_disk(STALE_OK)) {
            Some(r) => r,
            None => return None,
        }
    };

    // The plain-name list rides on its own request: publish the rates
    // first — the row needs them more — then publish again once the
    // names land, so a slow or missing list never delays the row.
    if let Some(n) = load_names(TTL) {
        r.names = n;
    }
    let arc = commit(r);
    if arc.names.is_empty() {
        let n = fetch_names();
        if !n.is_empty() {
            let mut r = (*arc).clone();
            r.names = n;
            commit(r);
        }
    }
    Some(())
}

/// Publish the table on the main thread and re-run the pending query.
/// Returns the shared snapshot so a follow-up commit (names landing
/// after the rates) can build on it.
fn commit(r: Rates) -> Arc<Rates> {
    let arc = Arc::new(r);
    if let Ok(mut g) = RATES.get_or_init(|| Mutex::new(None)).lock() {
        *g = Some(arc.clone());
    }
    glib::MainContext::default().invoke(|| crate::app::refresh_search_window());
    arc
}

fn parse(text: &str) -> Option<Rates> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let date = v.get("date")?.as_str()?.to_string();
    let usd = v.get("usd")?.as_object()?;
    let map: HashMap<String, f64> = usd
        .iter()
        .filter_map(|(k, v)| v.as_f64().filter(|r| *r > 0.0).map(|r| (k.clone(), r)))
        .collect();
    if map.is_empty() {
        return None;
    }
    Some(Rates {
        map,
        date,
        at: Instant::now(),
        names: HashMap::new(),
    })
}

/// `{"eur":"Euro","try":"Turkish Lira",…}` → lowercase name → code,
/// plus the plural people type ("turkish liras" — only for names not
/// already ending in `s`). The names are the API's, so all ~340
/// currencies resolve without a hand-maintained table.
pub(crate) fn parse_names(text: &str) -> HashMap<String, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return HashMap::new();
    };
    let Some(obj) = v.as_object() else {
        return HashMap::new();
    };
    let mut out: HashMap<String, String> = HashMap::new();
    for (code, name) in obj {
        let Some(name) = name.as_str() else {
            continue;
        };
        if code.is_empty() || name.is_empty() {
            continue;
        }
        let key = name.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let code = code.to_lowercase();
        out.entry(key.clone()).or_insert_with(|| code.clone());
        if !key.ends_with('s') {
            out.entry(format!("{key}s")).or_insert(code);
        }
    }
    out
}

fn cache_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("spotty").join("currency_usd.json"))
}

/// Disk copy, but only when its mtime is within `max_age`.
fn load_disk(max_age: Duration) -> Option<Rates> {
    let path = cache_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let age = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::MAX);
    if age > max_age {
        return None;
    }
    parse(&text)
}

fn save_disk(text: &str) {
    if let Some(path) = cache_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}

fn names_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("spotty").join("currency_names.json"))
}

/// Disk name list, but only when its mtime is within `max_age`.
fn load_names(max_age: Duration) -> Option<HashMap<String, String>> {
    let path = names_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let age = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::MAX);
    if age > max_age {
        return None;
    }
    let names = parse_names(&text);
    (!names.is_empty()).then_some(names)
}

fn save_names(text: &str) {
    if let Some(path) = names_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}

/// Wire, then yesterday's disk copy. Empty when nothing lands —
/// [`load_names`]'s callers treat the list as a completeness bonus,
/// never a requirement.
fn fetch_names() -> HashMap<String, String> {
    for url in NAME_URLS {
        if let Ok(text) = crate::triggers::fetch_text(url) {
            let names = parse_names(&text);
            if !names.is_empty() {
                save_names(&text);
                return names;
            }
        }
    }
    load_names(STALE_OK).unwrap_or_default()
}

/// Test seam: publish a fixture table the way a fetch would — the
/// shared table is the only channel `convert()` reads.
#[cfg(test)]
pub(crate) fn commit_for_test(r: Rates) {
    let _ = commit(r);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        r#"{"date":"2026-09-28","usd":{"eur":0.875,"gbp":0.75,"usd":1,"jpy":150.5,"zero":0}}"#;

    #[test]
    fn parses_the_usd_table_and_ignores_zero_rates() {
        let r = parse(SAMPLE).expect("sample parses");
        assert_eq!(r.date, "2026-09-28");
        assert_eq!(r.rate("EUR"), Some(0.875));
        assert_eq!(r.rate("usd"), Some(1.0));
        assert_eq!(r.rate("JPY"), Some(150.5));
        assert_eq!(r.rate("zero"), None, "zero rates never divide");
        assert_eq!(r.rate("chf"), None, "absent codes stay absent");
    }

    #[test]
    fn cross_rates_divide_through_the_usd_base() {
        let r = parse(SAMPLE).unwrap();
        assert_eq!(r.cross("usd", "eur"), Some(0.875));
        assert_eq!(r.cross("eur", "usd"), Some(1.0 / 0.875));
        // 100 EUR in USD terms: 1/0.875 * 0.875… — cross-check gbp→usd.
        assert_eq!(r.cross("gbp", "usd"), Some(1.0 / 0.75));
        assert_eq!(r.cross("usd", "chf"), None);
        assert_eq!(r.cross("eur", "jpy"), Some(150.5 / 0.875));
    }

    #[test]
    fn rejects_payloads_without_a_usd_table() {
        assert!(parse(r#"{"date":"2026-09-28"}"#).is_none());
        assert!(parse(r#"{"date":"x","usd":{}}"#).is_none());
        assert!(parse("not json").is_none());
    }

    #[test]
    fn parses_the_name_list_into_natural_lookup_keys() {
        const SAMPLE: &str = r#"{"eur":"Euro","try":"Turkish Lira","usd":"US Dollar","xau":"Gold Ounce","ltl":"Lithuanian Litas"}"#;
        let names = parse_names(SAMPLE);
        assert_eq!(names.get("turkish lira").map(String::as_str), Some("try"));
        assert_eq!(names.get("us dollar").map(String::as_str), Some("usd"));
        // The plural people type rides along…
        assert_eq!(names.get("turkish liras").map(String::as_str), Some("try"));
        assert_eq!(names.get("euros").map(String::as_str), Some("eur"));
        // …but a name already ending in `s` never doubles it.
        assert_eq!(names.get("gold ounces").map(String::as_str), Some("xau"));
        assert_eq!(
            names.get("lithuanian litas").map(String::as_str),
            Some("ltl")
        );
        assert!(!names.contains_key("lithuanian litass"));
        // Malformed payloads resolve nothing rather than garbage.
        assert!(parse_names("not json").is_empty());
        assert!(parse_names(r#"{"eur":5}"#).is_empty());
        assert!(parse_names(r#"{"eur":"  "}"#).is_empty());
    }
}
