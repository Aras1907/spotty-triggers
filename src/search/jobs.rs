use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::config::Config;
use crate::index::Snapshot;
use crate::search::browse;
use crate::search::inline_file_mode;
use crate::search::SearchResult;

static PENDING: OnceLock<Mutex<Option<(u64, String, Vec<SearchResult>)>>> = OnceLock::new();
static GEN: AtomicU64 = AtomicU64::new(0);
static INFLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn pending() -> &'static Mutex<Option<(u64, String, Vec<SearchResult>)>> {
    PENDING.get_or_init(|| Mutex::new(None))
}

fn inflight_set() -> &'static Mutex<HashSet<String>> {
    INFLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Build a key from mode + query so different modes with the same text
/// don't collide and the pending match is exact.
pub fn key(mode: Option<&str>, query: &str) -> String {
    match mode {
        Some(m) => format!("{}\u{1}{}", m, query),
        None => query.to_string(),
    }
}

pub fn job_inflight(k: &str) -> bool {
    inflight_set().lock().unwrap().contains(k)
}

pub fn has_pending(k: &str) -> bool {
    pending().lock().unwrap().as_ref().map_or(false, |(_, q, _)| q == k)
}

pub fn take_pending(k: &str) -> Option<Vec<SearchResult>> {
    let mut g = pending().lock().ok()?;
    if let Some((_, q, _)) = g.as_ref() {
        if q == k {
            return g.take().map(|(_, _, r)| r);
        }
    }
    None
}

/// Spawn a search job for the given key/mode/query. The worker computes
/// results on a background thread; when done it stores them in the pending
/// slot and invokes `refresh_search_window`.
pub fn spawn(
    k: String,
    query: String,
    mode: Option<String>,
    config: Config,
    snap: Arc<RwLock<crate::index::Snapshot>>,
) -> u64 {
    inflight_set().lock().unwrap().insert(k.clone());
    let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        // Guard: always remove from inflight on exit, even on panic.
        struct InflightGuard(String);
        impl Drop for InflightGuard {
            fn drop(&mut self) {
                inflight_set().lock().unwrap().remove(&self.0);
            }
        }
        let _guard = InflightGuard(k.clone());
        let results = compute(&query, mode.as_deref(), &config, &snap);
        if GEN.load(Ordering::SeqCst) == gen {
            *pending().lock().unwrap() = Some((gen, k, results));
            glib::MainContext::default().invoke(crate::app::refresh_search_window);
        }
    });
    gen
}

/// Compute search results. Mode routing mirrors the old synchronous
/// `search::search` / `search_mode` dispatch.
fn compute(
    query: &str,
    mode: Option<&str>,
    config: &Config,
    snap: &Arc<RwLock<crate::index::Snapshot>>,
) -> Vec<SearchResult> {
    // 1. Active mode → dispatch directly (handles empty query too).
    if let Some(m) = mode {
        return crate::search::search_mode(m, query, config, snap);
    }

    let query = query.trim();
    if query.is_empty() {
        return vec![];
    }

    // 2. Inline "find foo" etc.
    if let Some((mode_word, rest)) = inline_file_mode(query, config) {
        return crate::search::search_mode(&mode_word, &rest, config, snap);
    }

    // 3. Universal search.
    // Path browsing
    if config.enable_root_browsing
        && (query.starts_with('/') || query.starts_with("~/") || query == "~")
    {
        return browse::browse(query);
    }
    // Ranked by the order of result types and triggers set in Settings.
    crate::search::universal_results(query, config, snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::FileEntry;

    fn snap_with(files: Vec<FileEntry>) -> Arc<RwLock<crate::index::Snapshot>> {
        Arc::new(RwLock::new(crate::index::Snapshot { apps: vec![], files }))
    }

    #[test]
    fn key_distinguishes_modes() {
        assert_ne!(key(Some("find"), "x"), key(None, "x"));
        assert_ne!(key(Some("find"), "x"), key(Some("app"), "x"));
        assert_eq!(key(Some("find"), "x"), key(Some("find"), "x"));
    }

    #[test]
    fn results_appear_in_the_regular_search_without_any_trigger_word() {
        let snap = snap_with(vec![]);
        let cfg = Config::default();
        // A result type has no word out of the box — yet the regular search
        // (the production worker path) still produces its results.
        for id in crate::config::RESULT_IDS {
            let kw = cfg.command_keywords.iter().find(|k| k.id == id).expect(id);
            assert!(kw.word.is_empty(), "{id} has no word");
        }
        let calc = compute("2+3", None, &cfg, &snap);
        assert!(
            calc.iter().any(|r| r.title.contains('5')),
            "calculator: {:?}",
            calc.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
        let web = compute("rust lifetimes", None, &cfg, &snap);
        assert!(
            web.iter().any(|r| matches!(r.action, crate::search::Action::OpenUrl(_))),
            "web: {:?}",
            web.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
        // Giving a result type a word adds a mode; it takes nothing away.
        let mut with_word = Config::default();
        if let Some(k) = with_word.command_keywords.iter_mut().find(|k| k.id == "calc") {
            k.word = "calc".into();
        }
        let again = compute("2+3", None, &with_word, &snap);
        assert!(again.iter().any(|r| r.title.contains('5')), "still found in the regular search");
    }

    #[test]
    fn the_regular_search_switch_hides_results_only_for_a_type_with_a_word() {
        let snap = snap_with(vec![]);
        let found = |cfg: &Config, q: &str| compute(q, None, cfg, &snap).iter().any(|r| r.title.contains('5'));
        let mut cfg = Config::default();
        assert!(found(&cfg, "2+3"));
        // Off, but no word: nothing else reaches it, so it still shows.
        cfg.regular_search_off.push("calc".into());
        assert!(found(&cfg, "2+3"), "wordless types always show");
        // Off with a word: gone from the regular search…
        cfg.set_result_word("calc", "calc");
        cfg.set_in_regular_search("calc", false);
        assert!(!found(&cfg, "2+3"));
        // …but still there in its own mode.
        let snap2 = snap_with(vec![]);
        let in_mode = compute("2+3", Some("calc"), &cfg, &snap2);
        assert!(in_mode.iter().any(|r| r.title.contains('5')), "mode still computes");
        // Back on.
        cfg.set_in_regular_search("calc", true);
        assert!(found(&cfg, "2+3"));
    }

    #[test]
    fn the_calculator_and_the_converter_are_separate_results() {
        let snap = snap_with(vec![]);
        let has = |cfg: &Config, q: &str, needle: &str| {
            compute(q, None, cfg, &snap).iter().any(|r| r.title.contains(needle))
        };
        let both = Config::default();
        assert!(has(&both, "2+3", "5"));
        assert!(has(&both, "10 km to mi", "6.21"));
        let mut calc_only = Config::default();
        calc_only.set_result_enabled("convert", false);
        assert!(has(&calc_only, "2+3", "5"));
        assert!(!has(&calc_only, "10 km to mi", "6.21"), "converter off");
        let mut convert_only = Config::default();
        convert_only.set_result_enabled("calc", false);
        assert!(!has(&convert_only, "2+3", "5"), "calculator off");
        assert!(has(&convert_only, "10 km to mi", "6.21"));
        // Each has its own mode, answering only its own kind of query.
        let calc_mode = compute("10 km to mi", Some("calc"), &both, &snap);
        assert!(calc_mode.is_empty(), "{:?}", calc_mode.iter().map(|r| &r.title).collect::<Vec<_>>());
        assert!(!compute("10 km to mi", Some("convert"), &both, &snap).is_empty());
    }

    #[test]
    fn a_trigger_adds_its_results_to_the_regular_search_only_when_it_opts_in() {
        let snap = snap_with(vec![]);
        let emoji_rows = |cfg: &Config| -> Vec<crate::search::SearchResult> {
            let want: Vec<String> = crate::search::emoji::search("smile").into_iter().map(|r| r.title).collect();
            assert!(!want.is_empty(), "the emoji trigger finds something for 'smile'");
            compute("smile", None, cfg, &snap)
                .into_iter()
                .filter(|r| want.contains(&r.title))
                .collect()
        };
        let mut cfg = Config::default();
        assert!(emoji_rows(&cfg).is_empty(), "reached by its word, not by default");

        cfg.set_in_regular_search("emoji", true);
        let rows = emoji_rows(&cfg);
        assert!(!rows.is_empty() && rows.len() <= 3, "a few rows, not the whole list: {}", rows.len());
        // Ranked by Emoji's place in the order: above the web fallback, which
        // comes after it by default.
        let all = compute("smile", None, &cfg, &snap);
        let web_title = crate::search::web::result("smile", &cfg).title;
        let web = all.iter().position(|r| r.title == web_title).expect("web row");
        let first_emoji = all.iter().position(|r| rows.iter().any(|e| e.title == r.title)).unwrap();
        assert!(first_emoji < web, "emoji {first_emoji} before web {web}");

        // Paused or uninstalled: opted in, but not offered.
        let mut paused = cfg.clone();
        paused.command_keywords.iter_mut().find(|k| k.id == "emoji").unwrap().enabled = false;
        assert!(emoji_rows(&paused).is_empty(), "paused");
        let mut gone = cfg.clone();
        gone.uninstall_builtin("emoji");
        assert!(emoji_rows(&gone).is_empty(), "uninstalled");

        // One typed character is too little to bring a trigger in.
        assert!(crate::search::regular_trigger_results("s", &cfg, &snap).is_empty());
    }

    #[test]
    fn the_order_set_in_settings_decides_what_ranks_first() {
        let snap = snap_with(vec![]);
        let web_title = |cfg: &Config| crate::search::web::result("2+3", cfg).title;
        // Running-operation rows (other tests share that registry) always sit
        // on top by design; the order is about everything under them.
        let ranked = |cfg: &Config| -> Vec<crate::search::SearchResult> {
            compute("2+3", None, cfg, &snap)
                .into_iter()
                .filter(|r| r.icon.as_deref() != Some("op-progress"))
                .collect()
        };
        let top = |cfg: &Config| ranked(cfg)[0].title.clone();
        let mut cfg = Config::default();
        // By default the calculator's answer leads…
        assert!(top(&cfg).contains('5'), "calculator first: {}", top(&cfg));
        // …move Web Search to the top and it leads instead…
        cfg.move_in_order_with("web", 0, &[]);
        assert_eq!(top(&cfg), web_title(&cfg), "web first now");
        // …and the calculator row is still there, right under it.
        let rows = ranked(&cfg);
        assert!(rows[1].title.contains('5'), "{:?}", rows.iter().map(|r| &r.title).collect::<Vec<_>>());
    }

    #[test]
    fn a_command_runner_in_the_regular_search_never_becomes_the_top_row() {
        let snap = snap_with(vec![]);
        let mut cfg = Config::default();
        cfg.set_in_regular_search("run", true);
        let rows = compute("zzqqxx", None, &cfg, &snap);
        let run = rows.iter().find(|r| matches!(r.action, crate::search::Action::RunWithProgress { .. }));
        let run = run.expect("the Run trigger added its row");
        let web_title = crate::search::web::result("zzqqxx", &cfg).title;
        let web = rows.iter().find(|r| r.title == web_title).expect("web fallback");
        assert!(run.score < web.score, "run {} must sit below the web fallback {}", run.score, web.score);
        let top = rows.iter().find(|r| r.icon.as_deref() != Some("op-progress")).unwrap();
        assert_ne!(top.title, run.title, "Enter must not run a command by accident");
    }

    #[test]
    fn typoed_trigger_word_reaches_the_live_search() {
        let snap = snap_with(vec![]);
        let cfg = Config::default();
        // The worker path (the one that runs in production) must surface
        // fuzzy trigger suggestions too — it used to be prefix-only.
        let rows = compute("fnd", None, &cfg, &snap);
        assert!(
            rows.iter()
                .any(|r| matches!(&r.action, crate::search::Action::EnterMode(w) if w == "find")),
            "fnd: {:?}",
            rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn find_mode_routes_to_file_search() {
        let dir = std::env::temp_dir().join(format!("spotty_jobs_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("report_q4.txt");
        std::fs::write(&path, "quarter four report").unwrap();

        let entry = FileEntry {
            name: "report_q4.txt".into(),
            name_lower: "report_q4.txt".into(),
            path: path.clone(),
            is_dir: false,
        };
        let snap = snap_with(vec![entry]);
        let cfg = Config::default();

        let results = compute("report_q4", Some("find"), &cfg, &snap);
        assert!(
            results.iter().any(|r| r.title == "report_q4.txt"),
            "find mode should return the file, got: {:?}",
            results.iter().map(|r| r.title.clone()).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_routing_happens_before_empty_query_return() {
        let snap = snap_with(vec![]);
        let cfg = Config::default();
        let results = compute("", Some("emoji"), &cfg, &snap);
        assert!(
            !results.is_empty(),
            "an empty query in a mode must still be routed to that mode"
        );
    }
}
