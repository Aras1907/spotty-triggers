use crate::config::Config;
use crate::index::FileEntry;
use crate::search::{Action, ResultKind, SearchResult};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use crate::i18n::gettext;

const QUERY_RESULT_LIMIT: usize = 40;
const QUERY_REFRESH_EVERY_HITS: usize = 4;
const MAX_DIRECT_SCAN_DOCS: usize = 12_000;
const MAX_PRIORITY_SCAN_DOCS: usize = 800;
const CONTENT_CANDIDATE_CACHE_SECS: u64 = 5;
const QUERY_MISS_CACHE_SECS: u64 = 10;
const MAX_CACHED_TEXT_CHARS: usize = 200_000;
const MAX_COMPACT_SNIPPET_CHARS: usize = MAX_CACHED_TEXT_CHARS;
const MAX_PDF_BYTES: u64 = 15_000_000;
const MAX_PDF_STREAMS: usize = 1_024;
const MAX_PDF_STREAM_BYTES: usize = 2_000_000;
const MAX_PDF_TOTAL_STREAM_BYTES: usize = 20_000_000;
const MAX_TEXT_FILE_BYTES: u64 = 1_500_000;
const MAX_ZIP_ENTRY_BYTES: usize = 5_000_000;
const MAX_BINARY_BYTES: u64 = 2_000_000;

pub fn search_find(query: &str, snapshot: &Arc<RwLock<crate::index::Snapshot>>) -> Vec<SearchResult> {
    let query = query.trim();
    let mut results = {
        let g = snapshot.read().unwrap();
        search_names(query, &g.files)
    };
    if query.chars().count() < 3 {
        results.truncate(20);
        return results;
    }

    let ql = normalize_query_text(query).to_lowercase();
    set_active_content_query(&ql);

    let mut content_hits = Vec::new();
    {
        let g = snapshot.read().unwrap();
        add_query_content_hits(&ql, &g.files, &mut content_hits);
    }
    start_content_query_search(ql, snapshot.clone());

    // Name matches always rank first; content hits fill the remaining slots.
    let mut out = results;
    let mut seen: HashSet<PathBuf> = out
        .iter()
        .filter_map(|r| match &r.action {
            Action::OpenPath(p) | Action::BrowseInto(p) | Action::OpenInFileManager(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    for hit in content_hits {
        if out.len() >= 20 {
            break;
        }
        match &hit.action {
            Action::OpenPath(p) | Action::BrowseInto(p) | Action::OpenInFileManager(p) => {
                if seen.insert(p.clone()) {
                    out.push(hit);
                }
            }
            _ => {
                out.push(hit);
            }
        }
    }
    out.truncate(20);
    out
}

fn dedup_sort_truncate(mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    results.sort_by(|a, b| b.score.cmp(&a.score));
    let mut seen = HashSet::new();
    results.retain(|r| match &r.action {
        Action::OpenPath(p) | Action::BrowseInto(p) | Action::OpenInFileManager(p) => {
            seen.insert(p.clone())
        }
        _ => true,
    });
    results.truncate(20);
    results
}

fn content_cache() -> &'static Mutex<HashMap<PathBuf, Option<String>>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn content_inflight() -> &'static Mutex<HashSet<PathBuf>> {
    static C: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashSet::new()))
}

fn query_hits() -> &'static Mutex<HashMap<String, HashMap<PathBuf, (FileEntry, String)>>> {
    static C: OnceLock<Mutex<HashMap<String, HashMap<PathBuf, (FileEntry, String)>>>> =
        OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn query_inflight() -> &'static Mutex<HashSet<String>> {
    static C: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashSet::new()))
}

fn direct_candidate_cache() -> &'static Mutex<Option<(Instant, Vec<FileEntry>)>> {
    static C: OnceLock<Mutex<Option<(Instant, Vec<FileEntry>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn query_miss_cache() -> &'static Mutex<HashMap<String, HashMap<PathBuf, Instant>>> {
    static C: OnceLock<Mutex<HashMap<String, HashMap<PathBuf, Instant>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clear all content-search caches. Called on index rebuild so newly
/// downloaded files are re-scanned instead of hidden behind stale entries.
pub fn clear_content_caches() {
    content_cache().lock().unwrap().clear();
    direct_candidate_cache().lock().unwrap().take();
    query_miss_cache().lock().unwrap().clear();
    query_hits().lock().unwrap().clear();
    content_inflight().lock().unwrap().clear();
    query_inflight().lock().unwrap().clear();
    active_content_query().lock().unwrap().clear();
}

fn active_content_query() -> &'static Mutex<String> {
    static C: OnceLock<Mutex<String>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(String::new()))
}

fn set_active_content_query(query: &str) {
    *active_content_query().lock().unwrap() = query.to_string();
}

fn is_active_content_query(query: &str) -> bool {
    active_content_query().lock().unwrap().as_str() == query
}

fn add_query_content_hits(query: &str, files: &[FileEntry], out: &mut Vec<SearchResult>) {
    let hits = query_hits().lock().unwrap();
    let Some(hits) = hits.get(query) else {
        return;
    };
    if hits.is_empty() {
        return;
    }
    // Only build the rank map if we actually have content hits to look up.
    // This avoids an O(n) HashMap allocation over all indexed files on every
    // keystroke when no content search has completed yet.
    let indexed_rank: HashMap<&Path, usize> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.path.as_path(), i))
        .collect();
    let mut rows: Vec<(i32, &FileEntry, &String)> = hits
        .values()
        .map(|(file, snippet)| {
            let is_exact = snippet_is_exact_word_match(snippet, query);
            let score = if is_exact {
                8_000
            } else {
                let rank = indexed_rank
                    .get(file.path.as_path())
                    .copied()
                    .unwrap_or_else(|| 2_000 + content_scan_rank(&file.path));
                4_000 - (rank as i32).min(1_800)
            };
            (score, file, snippet)
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    for (score, file, snippet) in rows {
        if out.len() >= 20 {
            break;
        }
        push_content_hit(out, file, score, snippet.clone());
    }
}

fn push_content_hit(out: &mut Vec<SearchResult>, f: &FileEntry, score: i32, snippet: String) {
    let mut r = mk(f, score);
    r.subtitle = Some(gettext("Text match: {snippet}").replace("{snippet}", &snippet));
    out.push(r);
}

/// Check whether the snippet's `<b>…</b>` matched text forms a standalone
/// word (bounded by non-alphanumeric chars) — used to boost exact content
/// matches above substring-only matches.
fn snippet_is_exact_word_match(snippet: &str, query: &str) -> bool {
    let tag_start = match snippet.find("<b>") {
        Some(p) => p,
        None => return false,
    };
    let inner_start = tag_start + 3;
    let tag_end = match snippet[inner_start..].find("</b>") {
        Some(p) => p,
        None => return false,
    };
    let inner = &snippet[inner_start..inner_start + tag_end];
    if inner != query {
        return false;
    }
    // Word boundary before match
    if tag_start > 0 {
        let prev = snippet.as_bytes()[tag_start - 1];
        if prev.is_ascii_alphanumeric() {
            return false;
        }
    }
    // Word boundary after match
    let after = inner_start + tag_end + 4; // skip </b>
    if after < snippet.len() {
        let next = snippet.as_bytes()[after];
        if next.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

fn start_content_query_search(query: String, snapshot: Arc<RwLock<crate::index::Snapshot>>) {
    {
        let mut inflight = query_inflight().lock().unwrap();
        if !inflight.insert(query.clone()) {
            return;
        }
    }

    std::thread::spawn(move || {
        // Guard: always remove query from inflight on exit, even on panic.
        struct InflightGuard(String);
        impl Drop for InflightGuard {
            fn drop(&mut self) {
                query_inflight().lock().unwrap().remove(&self.0);
            }
        }
        let _guard = InflightGuard(query.clone());

        let mut hits = 0usize;
        let mut since_refresh = 0usize;

        // Do the expensive candidate collection inside the thread so
        // the main thread stays responsive while typing.
        let (priority_docs, indexed_files) = {
            let Ok(g) = snapshot.read() else {
                return;
            };
            let priority_docs = collect_indexed_priority_content_candidates(&g.files, &query);
            let indexed_files: Vec<FileEntry> = g
                .files
                .iter()
                .filter(|f| !f.is_dir && should_search_file_content(&f.path))
                .take(MAX_DIRECT_SCAN_DOCS)
                .cloned()
                .collect();
            (priority_docs, indexed_files)
        };

        scan_content_docs(&query, priority_docs, &mut hits, &mut since_refresh);
        if is_active_content_query(&query) && hits > 0 {
            refresh_search_results();
        }

        if is_active_content_query(&query) && hits < QUERY_RESULT_LIMIT {
            let mut docs = collect_content_candidates(&indexed_files, &query);
            if !is_active_content_query(&query) {
                return;
            }
            sort_candidates_for_query(&mut docs, &query);
            scan_content_docs(&query, docs, &mut hits, &mut since_refresh);
        }

        if is_active_content_query(&query) {
            refresh_search_results();
        }
    });
}

fn scan_content_docs(
    query: &str,
    docs: Vec<FileEntry>,
    hits: &mut usize,
    since_refresh: &mut usize,
) {
    for file in docs {
        if !is_active_content_query(query) {
            break;
        }
        let is_ocr = is_image_file(&file.path) || is_pdf_file(&file.path);
        if *hits >= QUERY_RESULT_LIMIT && (!is_ocr || *hits >= QUERY_RESULT_LIMIT + 5) {
            break;
        }
        if query_miss_cached(query, &file.path) {
            continue;
        }
        if query_hit_cached(query, &file.path) {
            continue;
        }
        let Some(snippet) = content_snippet_for_file(&file, query) else {
            remember_query_miss(query, &file.path);
            continue;
        };
        query_hits()
            .lock()
            .unwrap()
            .entry(query.to_string())
            .or_default()
            .insert(file.path.clone(), (file.clone(), snippet));
        *hits += 1;
        *since_refresh += 1;
        // Refresh on the very first hit as well: a single early match must
        // reach the UI immediately instead of waiting for the whole scan
        // phase (hundreds of candidates, some needing slow PDF OCR) to end.
        // Subsequent refreshes are throttled to ≥250ms to coalesce rebuilds.
        if *hits == 1 {
            refresh_search_results();
            *since_refresh = 0;
        } else if *since_refresh >= QUERY_REFRESH_EVERY_HITS {
            refresh_search_results_throttled();
            *since_refresh = 0;
        }
    }
}

fn query_hit_cached(query: &str, path: &Path) -> bool {
    query_hits()
        .lock()
        .unwrap()
        .get(query)
        .is_some_and(|hits| hits.contains_key(path))
}

fn query_miss_cached(query: &str, path: &Path) -> bool {
    let mut cache = query_miss_cache().lock().unwrap();
    let Some(paths) = cache.get_mut(query) else {
        return false;
    };
    let Some(when) = paths.get(path).copied() else {
        return false;
    };
    if when.elapsed() < Duration::from_secs(QUERY_MISS_CACHE_SECS) {
        true
    } else {
        paths.remove(path);
        false
    }
}

fn remember_query_miss(query: &str, path: &Path) {
    query_miss_cache()
        .lock()
        .unwrap()
        .entry(query.to_string())
        .or_default()
        .insert(path.to_path_buf(), Instant::now());
}

fn content_snippet_for_file(file: &FileEntry, query: &str) -> Option<String> {
    if is_pdf_file(&file.path) {
        return pdf_content_snippet_for_query(file, query);
    }
    cached_or_extract_content(&file.path)
        .and_then(|text| content_snippet_markup(&text, query))
}

fn pdf_content_snippet_for_query(file: &FileEntry, query: &str) -> Option<String> {
    if let Some(Some(text)) = cached_content_text(&file.path) {
        if let Some(snippet) = content_snippet_markup(&text, query) {
            return Some(snippet);
        }
    }
    // Poppler pdftotext (fast, accurate) → page OCR fallback → pure-Rust parser.
    // All paths persist in ocr.tsv so repeat queries are instant.
    crate::ocr::pdf_text_for(&file.path)
        .or_else(|| extract_pdf_text(&file.path))
        .and_then(|text| content_snippet_markup(&text, query))
}

fn cached_content_text(path: &Path) -> Option<Option<String>> {
    content_cache().lock().unwrap().get(path).cloned()
}

fn cached_or_extract_content(path: &Path) -> Option<String> {
    {
        let cache = content_cache().lock().unwrap();
        if let Some(text) = cache.get(path) {
            return text.clone();
        }
    }
    {
        let cache = content_cache().lock().unwrap();
        let mut inflight = content_inflight().lock().unwrap();
        if let Some(text) = cache.get(path) {
            return text.clone();
        }
        if !inflight.insert(path.to_path_buf()) {
            return None;
        }
    }

    let text = extract_search_text(path).map(normalize_content_text);
    {
        // ponytail: only cache successful extractions. Failed OCR (None) is not
        // cached so a later query can retry (e.g. after a buggy decoder is fixed
        // or the file is re-downloaded).
        if text.is_some() {
            content_cache()
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), text.clone());
        }
        content_inflight().lock().unwrap().remove(path);
    }
    text
}

fn content_search_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Downloads"),
        home.join("Documents"),
        home.join("Desktop"),
        home.join("Pictures"),
        home.join("Music"),
        home.join("Videos"),
        home.to_path_buf(),
    ]
}

fn collect_indexed_priority_content_candidates(files: &[FileEntry], query: &str) -> Vec<FileEntry> {
    let mut out: Vec<FileEntry> = files
        .iter()
        .filter(|f| f.is_dir == false && should_search_file_content(&f.path))
        .filter(|f| {
            is_pdf_file(&f.path)
                || is_image_file(&f.path)
                || content_query_rank_key(&f.path, query).0 <= 2
        })
        .take(MAX_DIRECT_SCAN_DOCS)
        .cloned()
        .collect();
    sort_candidates_for_query(&mut out, query);
    out.truncate(MAX_PRIORITY_SCAN_DOCS);
    out
}

fn collect_content_candidates(files: &[FileEntry], query: &str) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let Some(home) = dirs::home_dir() else {
        return indexed_content_candidates(files, &mut seen, MAX_DIRECT_SCAN_DOCS);
    };
    if !is_active_content_query(query) {
        return out;
    }

    if let Some((created, cached)) = direct_candidate_cache().lock().unwrap().as_ref() {
        if created.elapsed() < Duration::from_secs(CONTENT_CANDIDATE_CACHE_SECS) {
            out = cached.clone();
            seen.extend(out.iter().map(|f| f.path.clone()));
            if is_active_content_query(query) && out.len() < MAX_DIRECT_SCAN_DOCS {
                out.extend(indexed_content_candidates(
                    files,
                    &mut seen,
                    MAX_DIRECT_SCAN_DOCS - out.len(),
                ));
            }
            return out;
        }
    }

    let roots = content_search_roots(&home);
    for pdf_only in [true, false] {
        for root in &roots {
            if !is_active_content_query(query) || out.len() >= MAX_DIRECT_SCAN_DOCS {
                break;
            }
            collect_content_candidates_from_root(
                root,
                &home,
                query,
                &mut seen,
                &mut out,
                pdf_only,
                MAX_DIRECT_SCAN_DOCS,
                None,
            );
        }
    }

    if find_debug_enabled() {
        let pdfs = out.iter().filter(|f| is_pdf_file(&f.path)).count();
        find_debug_log(format_args!(
            "content candidates: total={} pdfs={} query={}",
            out.len(),
            pdfs,
            query
        ));
    }

    *direct_candidate_cache().lock().unwrap() = Some((Instant::now(), out.clone()));

    if is_active_content_query(query) && out.len() < MAX_DIRECT_SCAN_DOCS {
        out.extend(indexed_content_candidates(
            files,
            &mut seen,
            MAX_DIRECT_SCAN_DOCS - out.len(),
        ));
    }
    out
}

fn collect_content_candidates_from_root(
    root: &Path,
    home: &Path,
    query: &str,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<FileEntry>,
    pdf_only: bool,
    limit: usize,
    max_depth_override: Option<usize>,
) {
    if out.len() >= limit || !root.exists() {
        return;
    }
    let max_depth = max_depth_override.or(if root == home { Some(5) } else { Some(12) });
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(max_depth)
        .filter_entry(|entry| !is_heavy_content_component(&entry.file_name().to_string_lossy()))
        .build()
        .flatten()
    {
        if !is_active_content_query(query) || out.len() >= limit {
            break;
        }
        let path = entry.path().to_path_buf();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            || !should_search_file_content(&path)
            || pdf_only != is_pdf_file(&path)
            || !seen.insert(path.clone())
        {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        out.push(FileEntry {
            path,
            name: name.clone(),
            name_lower: name.to_ascii_lowercase(),
            is_dir: false,
        });
    }
}

fn indexed_content_candidates(
    files: &[FileEntry],
    seen: &mut HashSet<PathBuf>,
    limit: usize,
) -> Vec<FileEntry> {
    let mut out = Vec::new();
    for f in files {
        if out.len() >= limit {
            break;
        }
        if f.is_dir || !should_search_file_content(&f.path) || !seen.insert(f.path.clone()) {
            continue;
        }
        out.push(f.clone());
    }
    out
}

/// How expensive it is to extract searchable text from a candidate:
/// 0 — cheap (plain text/docs, or an image whose OCR text is cached);
/// 1 — an image needing on-demand OCR (~0.5s); 2 — a PDF (full byte scan
/// and possibly page OCR, seconds). Cheap candidates are scanned first so
/// early hits reach the UI quickly instead of waiting behind slow ones.
fn candidate_cost_class(path: &Path) -> u8 {
    if is_pdf_file(path) {
        2
    } else if is_image_file(path) {
        if crate::ocr::cached_text_for(path).is_some() {
            0
        } else {
            1
        }
    } else {
        0
    }
}

fn sort_candidates_for_query(files: &mut [FileEntry], query: &str) {
    // Precompute keys once per candidate: the cost-class lookup touches the
    // OCR cache (lock + file stat), so it must not run inside the comparator.
    let mut keyed: Vec<((usize, u8, usize), FileEntry)> = files
        .iter()
        .map(|f| {
            let (hint, len) = content_query_rank_key(&f.path, query);
            ((hint, candidate_cost_class(&f.path), len), f.clone())
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    for (i, (_, f)) in keyed.into_iter().enumerate() {
        files[i] = f;
    }
}

fn content_query_rank_key(path: &Path, query: &str) -> (usize, usize) {
    let text = path.to_string_lossy().to_lowercase();
    let compact_path = compact_alnum(&text);
    let compact_query = compact_alnum(query);
    let query_hint = if !query.is_empty() && text.contains(query) {
        0
    } else if compact_query.chars().count() >= 3 && compact_path.contains(&compact_query) {
        1
    } else if content_query_terms(query)
        .iter()
        .any(|term| term.chars().count() >= 3 && text.contains(term))
    {
        2
    } else {
        3
    };
    let (_, _, len_rank) = content_scan_rank_key(path);
    (query_hint, len_rank)
}

fn content_scan_rank(path: &Path) -> usize {
    content_scan_rank_key(path).0
}

fn content_scan_rank_key(path: &Path) -> (usize, usize, usize) {
    let s = path.to_string_lossy();
    let root_rank = if s.contains("/Downloads/") {
        0
    } else if s.contains("/Documents/") || s.contains("/Desktop/") {
        1
    } else if s.contains("/Pictures/") || s.contains("/Music/") || s.contains("/Videos/") {
        2
    } else {
        3
    };
    let type_rank = 0;
    (root_rank, type_rank, s.len())
}

fn refresh_search_results() {
    gtk::glib::MainContext::default().invoke(crate::app::refresh_search_window);
}

/// Throttled version: only refreshes if at least 250ms have passed since the
/// last throttled refresh. Used during content-scan loops to coalesce rebuilds.
static LAST_THROTTLED_REFRESH: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn refresh_search_results_throttled() {
    let mut last = LAST_THROTTLED_REFRESH.lock().unwrap();
    let now = std::time::Instant::now();
    if let Some(prev) = *last {
        if now.duration_since(prev).as_millis() < 350 {
            return;
        }
    }
    *last = Some(now);
    drop(last);
    refresh_search_results();
}

fn should_search_file_content(path: &Path) -> bool {
    is_content_searchable(path) && !has_heavy_content_component(path)
}

fn has_heavy_content_component(path: &Path) -> bool {
    path.components().any(|component| {
        let Some(name) = component.as_os_str().to_str() else {
            return false;
        };
        is_heavy_content_component(name)
    })
}

fn is_heavy_content_component(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".cache"
            | "cache"
            | "Cache"
            | "target"
            | "node_modules"
            | "tmp"
            | "temp"
            | "__pycache__"
            | "venv"
            | ".venv"
            | ".npm"
            | ".cargo"
            | ".rustup"
            | ".gradle"
            | ".m2"
            | "Trash"
            | "GPUCache"
            | "Code Cache"
            | "Service Worker"
            | "storage"
    )
}

fn is_content_searchable(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| {
            matches!(
                &*ext.to_ascii_lowercase(),
                "txt"
                    | "md"
                    | "markdown"
                    | "rst"
                    | "log"
                    | "csv"
                    | "json"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "rs"
                    | "py"
                    | "js"
                    | "ts"
                    | "html"
                    | "htm"
                    | "css"
                    | "xml"
                    | "pdf"
                    | "rtf"
                    | "docx"
                    | "docm"
                    | "dotx"
                    | "pptx"
                    | "pptm"
                    | "pps"
                    | "ppsx"
                    | "potx"
                    | "xlsx"
                    | "xlsm"
                    | "xltx"
                    | "odt"
                    | "ott"
                    | "fodt"
                    | "odp"
                    | "otp"
                    | "fodp"
                    | "ods"
                    | "ots"
                    | "fods"
                    | "doc"
                    | "ppt"
                    | "xls"
                    | "png"
                    | "jpg"
                    | "jpeg"
                    | "webp"
                    | "tif"
                    | "tiff"
                    | "bmp"
                    | "avif"
                    | "gif"
                    | "ico"
                    | "pnm"
                    | "pgm"
                    | "ppm"
                    | "pbm"
                    | "qoi"
                    | "tga"
                    | "heic"
                    | "heif"
                    | "svg"
            )
        })
        .unwrap_or(false)
}

fn is_pdf_file(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| {
            matches!(
                &*ext.to_ascii_lowercase(),
                "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff" | "bmp" | "avif"
                    | "gif" | "ico" | "pnm" | "pgm" | "ppm" | "pbm" | "qoi" | "tga"
                    | "heic" | "heif" | "svg"
            )
        })
        .unwrap_or(false)
}

fn extract_search_text(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "md" | "markdown" | "rst" | "log" | "csv" | "json" | "toml" | "yaml" | "yml"
        | "rs" | "py" | "js" | "ts" | "html" | "htm" | "css" | "xml" => read_small_text(path),
        "pdf" => extract_pdf_text(path).or_else(|| extract_binary_strings(path)),
        "docx" | "docm" | "dotx" | "pptx" | "pptm" | "ppsx" | "potx" | "xlsx" | "xlsm" | "xltx" => {
            extract_ooxml_text(path, &ext)
        }
        "odt" | "ott" | "odp" | "otp" | "ods" | "ots" => extract_odf_text(path),
        "fodt" | "fodp" | "fods" => extract_flat_xml_text(path),
        "doc" | "ppt" | "pps" | "xls" | "rtf" => extract_binary_strings(path),
        "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff" | "bmp" | "avif" | "gif"
        | "ico" | "pnm" | "pgm" | "ppm" | "pbm" | "qoi" | "tga" | "heic" | "heif"
        | "svg" => crate::ocr::text_for(path),
        _ => None,
    }
}

fn read_small_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_TEXT_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn find_debug_enabled() -> bool {
    std::env::var_os("SPOTTY_FIND_DEBUG").is_some()
}

fn find_debug_log(args: std::fmt::Arguments<'_>) {
    if find_debug_enabled() {
        eprintln!("[spotty-find] {args}");
    }
}

fn extract_pdf_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_PDF_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let streams = extract_pdf_streams(&bytes);
    let cmap = extract_pdf_cmap(&streams);
    let cmap_maps = extract_pdf_cmap_maps(&streams);
    let difference_maps = extract_pdf_difference_maps(&bytes, &streams);
    let mut parts = Vec::new();

    for stream in &streams {
        if is_pdf_cmap_stream(stream) {
            continue;
        }
        parts.extend(extract_pdf_strings(stream, &cmap));
        parts.extend(extract_pdf_cmap_strings(stream, &cmap_maps));
        parts.extend(extract_pdf_difference_strings(stream, &difference_maps));
    }

    if parts.is_empty() {
        parts.extend(extract_pdf_strings(&bytes, &cmap));
    }
    parts.extend(extract_pdf_cmap_strings(&bytes, &cmap_maps));
    parts.extend(extract_pdf_difference_strings(&bytes, &difference_maps));

    // Receipts and forms often keep visible text in fallback encodings or
    // fragmented object data. Keep this bounded, but always add it to the parsed
    // stream text instead of using it only when PDF parsing returns nothing.
    parts.extend(extract_ascii_strings(&bytes, 4).into_iter().take(800));
    parts.extend(extract_utf16be_strings(&bytes, 4).into_iter().take(800));
    parts.extend(extract_utf16le_strings(&bytes, 4).into_iter().take(800));

    if !parts.is_empty() {
        let joined = parts.join("");
        if has_searchable_text(&joined) {
            parts.push(joined);
        }
    }
    let text = parts.join("\n");
    has_searchable_text(&text).then_some(text)
}

fn pdf_stream_dict(bytes: &[u8], stream_kw: usize) -> &[u8] {
    let before = &bytes[..stream_kw];
    let obj_start = rfind_bytes(before, b"obj")
        .map(|pos| pos + b"obj".len())
        .unwrap_or(0);
    let scope = &bytes[obj_start..stream_kw];
    if let Some(start) = find_bytes(scope, b"<<") {
        return &scope[start..];
    }
    rfind_bytes(before, b"<<")
        .map(|start| &bytes[start..stream_kw])
        .unwrap_or(&[])
}

fn extract_pdf_cmap_maps(streams: &[Vec<u8>]) -> Vec<HashMap<Vec<u8>, String>> {
    let mut maps = Vec::new();
    for stream in streams {
        if !is_pdf_cmap_stream(stream) {
            continue;
        }
        let text = String::from_utf8_lossy(stream);
        let mut map = HashMap::new();
        parse_cmap_bfchar(&text, &mut map);
        parse_cmap_bfrange(&text, &mut map);
        if !map.is_empty() {
            maps.push(map);
        }
    }
    maps.truncate(32);
    maps
}

fn extract_pdf_cmap_strings(bytes: &[u8], maps: &[HashMap<Vec<u8>, String>]) -> Vec<String> {
    if maps.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for raw in extract_pdf_raw_strings(bytes).into_iter().take(20_000) {
        for map in maps {
            let text = decode_pdf_text_bytes(&raw, map);
            if is_useful_pdf_text(&text) {
                out.push(text);
            }
        }
    }
    out
}

fn extract_pdf_difference_maps(raw_pdf: &[u8], streams: &[Vec<u8>]) -> Vec<Vec<Option<char>>> {
    let mut maps = Vec::new();
    add_difference_maps_from_text(&String::from_utf8_lossy(raw_pdf), &mut maps);
    for stream in streams {
        add_difference_maps_from_text(&String::from_utf8_lossy(stream), &mut maps);
    }
    maps.truncate(32);
    maps
}

fn add_difference_maps_from_text(text: &str, maps: &mut Vec<Vec<Option<char>>>) {
    let mut rest = text;
    while let Some(pos) = rest.find("/Differences") {
        rest = &rest[pos + "/Differences".len()..];
        let Some(open) = rest.find('[') else {
            break;
        };
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find(']') else {
            break;
        };
        let body = &after_open[..close];
        let mut map = vec![None; 256];
        let mut code: Option<usize> = None;
        for token in body.split_whitespace() {
            if let Ok(n) = token.parse::<usize>() {
                if n < 256 {
                    code = Some(n);
                }
                continue;
            }
            let Some(name) = token.strip_prefix('/') else {
                continue;
            };
            if let Some(idx) = code {
                if idx < 256 {
                    map[idx] = pdf_glyph_name_to_char(name);
                    code = Some(idx + 1);
                }
            }
        }
        if map.iter().any(Option::is_some) {
            maps.push(map);
        }
        rest = &after_open[close + 1..];
    }
}

fn extract_pdf_difference_strings(bytes: &[u8], maps: &[Vec<Option<char>>]) -> Vec<String> {
    if maps.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for raw in extract_pdf_raw_strings(bytes).into_iter().take(20_000) {
        for map in maps {
            let text: String = raw
                .iter()
                .filter_map(|b| map.get(*b as usize).and_then(|ch| *ch))
                .collect();
            if is_useful_pdf_text(&text) {
                out.push(text);
            }
        }
    }
    out
}

fn extract_pdf_raw_strings(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() && out.len() < 25_000 {
        match bytes[i] {
            b'(' => {
                if let Some((raw, next)) = parse_pdf_literal(bytes, i + 1) {
                    if !raw.is_empty() {
                        out.push(raw);
                    }
                    i = next;
                    continue;
                }
            }
            b'<' if bytes.get(i + 1) != Some(&b'<') => {
                if let Some((raw, next)) = parse_pdf_hex(bytes, i + 1) {
                    if !raw.is_empty() {
                        out.push(raw);
                    }
                    i = next;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn pdf_glyph_name_to_char(name: &str) -> Option<char> {
    let clean = name
        .split(['.', '_'])
        .next()
        .unwrap_or(name)
        .trim_start_matches("uni");
    if clean.len() == 1 {
        return clean.chars().next();
    }
    if clean.len() == 4 && clean.chars().all(|ch| ch.is_ascii_hexdigit()) {
        if let Ok(value) = u16::from_str_radix(clean, 16) {
            return char::from_u32(value as u32);
        }
    }
    match clean {
        "space" => Some(' '),
        "exclam" => Some('!'),
        "quotedbl" => Some('"'),
        "numbersign" => Some('#'),
        "dollar" => Some('$'),
        "percent" => Some('%'),
        "ampersand" => Some('&'),
        "quotesingle" | "quoteright" | "quoteleft" => Some('\''),
        "parenleft" => Some('('),
        "parenright" => Some(')'),
        "asterisk" => Some('*'),
        "plus" => Some('+'),
        "comma" => Some(','),
        "hyphen" | "minus" => Some('-'),
        "period" => Some('.'),
        "slash" => Some('/'),
        "zero" => Some('0'),
        "one" => Some('1'),
        "two" => Some('2'),
        "three" => Some('3'),
        "four" => Some('4'),
        "five" => Some('5'),
        "six" => Some('6'),
        "seven" => Some('7'),
        "eight" => Some('8'),
        "nine" => Some('9'),
        "colon" => Some(':'),
        "semicolon" => Some(';'),
        "less" => Some('<'),
        "equal" => Some('='),
        "greater" => Some('>'),
        "question" => Some('?'),
        "at" => Some('@'),
        "bracketleft" => Some('['),
        "backslash" => Some('\\'),
        "bracketright" => Some(']'),
        "asciicircum" => Some('^'),
        "underscore" => Some('_'),
        "grave" => Some('`'),
        "braceleft" => Some('{'),
        "bar" => Some('|'),
        "braceright" => Some('}'),
        "asciitilde" => Some('~'),
        "Aacute" | "aacute" => Some('a'),
        "Agrave" | "agrave" => Some('a'),
        "Acircumflex" | "acircumflex" => Some('a'),
        "Atilde" | "atilde" => Some('a'),
        "Ccedilla" | "ccedilla" => Some('c'),
        "Eacute" | "eacute" => Some('e'),
        "Egrave" | "egrave" => Some('e'),
        "Ecircumflex" | "ecircumflex" => Some('e'),
        "Iacute" | "iacute" => Some('i'),
        "Igrave" | "igrave" => Some('i'),
        "Icircumflex" | "icircumflex" => Some('i'),
        "Oacute" | "oacute" => Some('o'),
        "Ograve" | "ograve" => Some('o'),
        "Ocircumflex" | "ocircumflex" => Some('o'),
        "Otilde" | "otilde" => Some('o'),
        "Uacute" | "uacute" => Some('u'),
        "Ugrave" | "ugrave" => Some('u'),
        "Ucircumflex" | "ucircumflex" => Some('u'),
        _ => None,
    }
}

fn extract_pdf_streams(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut streams = Vec::new();
    let mut total_stream_bytes = 0usize;
    let mut pos = 0usize;
    while streams.len() < MAX_PDF_STREAMS {
        let Some(rel) = find_bytes(&bytes[pos..], b"stream") else {
            break;
        };
        let stream_kw = pos + rel;
        let mut data_start = stream_kw + b"stream".len();
        if bytes.get(data_start) == Some(&b'\r') && bytes.get(data_start + 1) == Some(&b'\n') {
            data_start += 2;
        } else if matches!(bytes.get(data_start), Some(b'\n' | b'\r')) {
            data_start += 1;
        }

        let Some(end_rel) = find_bytes(&bytes[data_start..], b"endstream") else {
            break;
        };
        let data_end = data_start + end_rel;
        let dict = pdf_stream_dict(bytes, stream_kw);
        let data = &bytes[data_start..data_end];

        if let Some(decoded) = decode_pdf_stream_data(dict, data) {
            total_stream_bytes += decoded.len();
            if total_stream_bytes <= MAX_PDF_TOTAL_STREAM_BYTES {
                streams.push(decoded);
            }
        }
        if total_stream_bytes >= MAX_PDF_TOTAL_STREAM_BYTES {
            break;
        }
        pos = data_end + b"endstream".len();
    }
    streams
}

fn decode_pdf_stream_data(dict: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut out = data[..data.len().min(MAX_PDF_STREAM_BYTES)].to_vec();
    let filters = pdf_stream_filters(dict);
    if filters.is_empty() {
        if out.first() == Some(&0x78) {
            if let Some(inflated) = inflate_pdf_stream(&out) {
                return Some(inflated);
            }
        }
        return Some(out);
    }
    for filter in filters {
        out = match filter.as_str() {
            "ASCIIHexDecode" | "AHx" => decode_pdf_ascii_hex(&out)?,
            "ASCII85Decode" | "A85" => decode_pdf_ascii85(&out)?,
            "FlateDecode" | "Fl" => inflate_pdf_stream(&out)?,
            // Image-only and uncommon compression filters are intentionally skipped
            // for text extraction; OCR handles supported embedded image streams.
            _ => return None,
        };
        if out.len() > MAX_PDF_STREAM_BYTES {
            return None;
        }
    }
    Some(out)
}

fn pdf_stream_filters(dict: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(dict);
    let Some(pos) = text.find("/Filter") else {
        return Vec::new();
    };
    let trimmed = text[pos + "/Filter".len()..].trim_start();
    if let Some(array) = trimmed.strip_prefix('[') {
        let array = array.find(']').map(|end| &array[..end]).unwrap_or(array);
        let mut filters = Vec::new();
        let mut rest = array;
        while let Some(pos) = rest.find('/') {
            rest = &rest[pos + 1..];
            if let Some((name, next)) = pdf_name_token(rest) {
                filters.push(name.to_string());
                rest = &rest[next..];
            } else {
                break;
            }
        }
        filters
    } else if let Some(name) = trimmed.strip_prefix('/') {
        pdf_name_token(name)
            .map(|(name, _)| vec![name.to_string()])
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn pdf_name_token(text: &str) -> Option<(&str, usize)> {
    let end = text
        .char_indices()
        .find_map(|(i, ch)| {
            (ch.is_whitespace() || matches!(ch, '/' | '[' | ']' | '<' | '>' | '(' | ')'))
                .then_some(i)
        })
        .unwrap_or(text.len());
    (end > 0).then_some((&text[..end], end))
}

fn decode_pdf_ascii_hex(data: &[u8]) -> Option<Vec<u8>> {
    let mut nibbles = Vec::new();
    for &b in data {
        match b {
            b'>' => break,
            b if b.is_ascii_hexdigit() => nibbles.push(b),
            b if b.is_ascii_whitespace() => {}
            _ => return None,
        }
    }
    if nibbles.len() % 2 == 1 {
        nibbles.push(b'0');
    }
    let mut out = Vec::with_capacity(nibbles.len() / 2);
    for pair in nibbles.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

fn decode_pdf_ascii85(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut group = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'~' && data.get(i + 1) == Some(&b'>') {
            break;
        }
        if b == b'z' && group.is_empty() {
            out.extend_from_slice(&[0, 0, 0, 0]);
            i += 1;
            continue;
        }
        if !(b'!'..=b'u').contains(&b) {
            return None;
        }
        group.push(b - b'!');
        if group.len() == 5 {
            let mut value = 0u32;
            for digit in &group {
                value = value.checked_mul(85)?.checked_add(*digit as u32)?;
            }
            out.extend_from_slice(&value.to_be_bytes());
            group.clear();
        }
        i += 1;
    }
    if !group.is_empty() {
        let original = group.len();
        while group.len() < 5 {
            group.push(84);
        }
        let mut value = 0u32;
        for digit in &group {
            value = value.checked_mul(85)?.checked_add(*digit as u32)?;
        }
        out.extend_from_slice(&value.to_be_bytes()[..original.saturating_sub(1)]);
    }
    Some(out)
}

fn inflate_pdf_stream(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(MAX_PDF_STREAM_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() <= MAX_PDF_STREAM_BYTES).then_some(out)
}

fn extract_pdf_cmap(streams: &[Vec<u8>]) -> HashMap<Vec<u8>, String> {
    let mut map = HashMap::new();
    for stream in streams {
        if !is_pdf_cmap_stream(stream) {
            continue;
        }
        let text = String::from_utf8_lossy(stream);
        parse_cmap_bfchar(&text, &mut map);
        parse_cmap_bfrange(&text, &mut map);
    }
    map
}

fn is_pdf_cmap_stream(bytes: &[u8]) -> bool {
    bytes
        .windows(b"beginbfchar".len())
        .any(|w| w == b"beginbfchar")
        || bytes
            .windows(b"beginbfrange".len())
            .any(|w| w == b"beginbfrange")
}

fn parse_cmap_bfchar(text: &str, map: &mut HashMap<Vec<u8>, String>) {
    let mut rest = text;
    while let Some(start) = rest.find("beginbfchar") {
        let block_start = start + "beginbfchar".len();
        let Some(end) = rest[block_start..].find("endbfchar") else {
            break;
        };
        let block = &rest[block_start..block_start + end];
        let tokens = hex_tokens(block);
        for pair in tokens.chunks_exact(2) {
            if let Some(value) = decode_cmap_unicode(&pair[1]) {
                map.insert(pair[0].clone(), value);
            }
        }
        rest = &rest[block_start + end + "endbfchar".len()..];
    }
}

fn parse_cmap_bfrange(text: &str, map: &mut HashMap<Vec<u8>, String>) {
    let mut rest = text;
    while let Some(start) = rest.find("beginbfrange") {
        let block_start = start + "beginbfrange".len();
        let Some(end) = rest[block_start..].find("endbfrange") else {
            break;
        };
        let block = &rest[block_start..block_start + end];
        for line in block.lines() {
            let tokens = hex_tokens(line);
            if tokens.len() < 3 {
                continue;
            }
            let start_code = be_bytes_to_u32(&tokens[0]);
            let end_code = be_bytes_to_u32(&tokens[1]);
            let Some((from, to)) = start_code.zip(end_code) else {
                continue;
            };
            if from > to || to - from > 4096 {
                continue;
            }
            if line.contains('[') {
                for (offset, dst) in tokens.iter().skip(2).enumerate() {
                    let code = from + offset as u32;
                    if code > to {
                        break;
                    }
                    if let Some(value) = decode_cmap_unicode(dst) {
                        map.insert(code_to_bytes(code, tokens[0].len()), value);
                    }
                }
            } else if let Some(dst_start) = be_bytes_to_u32(&tokens[2]) {
                let dst_len = tokens[2].len();
                for code in from..=to {
                    let dst = dst_start + (code - from);
                    if let Some(value) = decode_cmap_unicode(&code_to_bytes(dst, dst_len)) {
                        map.insert(code_to_bytes(code, tokens[0].len()), value);
                    }
                }
            }
        }
        rest = &rest[block_start + end + "endbfrange".len()..];
    }
}

fn hex_tokens(text: &str) -> Vec<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' && bytes.get(i + 1) != Some(&b'<') {
            if let Some((raw, next)) = parse_pdf_hex(bytes, i + 1) {
                out.push(raw);
                i = next;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn be_bytes_to_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    let mut n = 0u32;
    for b in bytes {
        n = (n << 8) | *b as u32;
    }
    Some(n)
}

fn code_to_bytes(code: u32, len: usize) -> Vec<u8> {
    (0..len)
        .rev()
        .map(|shift| ((code >> (shift * 8)) & 0xff) as u8)
        .collect()
}

fn decode_cmap_unicode(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return None;
    }
    let text: String = bytes
        .chunks_exact(2)
        .filter_map(|pair| char::from_u32(u16::from_be_bytes([pair[0], pair[1]]) as u32))
        .collect();
    (!text.is_empty()).then_some(text)
}

fn extract_pdf_strings(bytes: &[u8], cmap: &HashMap<Vec<u8>, String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                if let Some((raw, next)) = parse_pdf_literal(bytes, i + 1) {
                    let text = decode_pdf_text_bytes(&raw, cmap);
                    if is_useful_pdf_text(&text) {
                        out.push(text);
                    }
                    i = next;
                    continue;
                }
            }
            b'<' if bytes.get(i + 1) != Some(&b'<') => {
                if let Some((raw, next)) = parse_pdf_hex(bytes, i + 1) {
                    let text = decode_pdf_text_bytes(&raw, cmap);
                    if is_useful_pdf_text(&text) {
                        out.push(text);
                    }
                    i = next;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn parse_pdf_literal(bytes: &[u8], mut i: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut depth = 1usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i += 1;
            let esc = *bytes.get(i)?;
            match esc {
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'(' | b')' | b'\\' => out.push(esc),
                b'\r' | b'\n' => {
                    if esc == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                        i += 1;
                    }
                }
                b'0'..=b'7' => {
                    let mut val = (esc - b'0') as u16;
                    for _ in 0..2 {
                        if let Some(next @ b'0'..=b'7') = bytes.get(i + 1).copied() {
                            i += 1;
                            val = val * 8 + (next - b'0') as u16;
                        } else {
                            break;
                        }
                    }
                    out.push((val & 0xff) as u8);
                }
                other => out.push(other),
            }
        } else if b == b'(' {
            depth += 1;
            out.push(b);
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some((out, i + 1));
            }
            out.push(b);
        } else {
            out.push(b);
        }
        i += 1;
    }
    None
}

fn parse_pdf_hex(bytes: &[u8], mut i: usize) -> Option<(Vec<u8>, usize)> {
    let mut nibbles = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'>' => {
                if nibbles.len() % 2 == 1 {
                    nibbles.push(b'0');
                }
                let mut out = Vec::new();
                for pair in nibbles.chunks_exact(2) {
                    let hi = (pair[0] as char).to_digit(16)?;
                    let lo = (pair[1] as char).to_digit(16)?;
                    out.push(((hi << 4) | lo) as u8);
                }
                return Some((out, i + 1));
            }
            b if b.is_ascii_hexdigit() => nibbles.push(b),
            b if b.is_ascii_whitespace() => {}
            _ => return None,
        }
        i += 1;
    }
    None
}

fn decode_pdf_text_bytes(bytes: &[u8], cmap: &HashMap<Vec<u8>, String>) -> String {
    if !cmap.is_empty() {
        let decoded = decode_with_cmap(bytes, cmap);
        if is_useful_pdf_text(&decoded) {
            return decoded;
        }
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return bytes[2..]
            .chunks_exact(2)
            .filter_map(|pair| char::from_u32(u16::from_be_bytes([pair[0], pair[1]]) as u32))
            .collect();
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return bytes[2..]
            .chunks_exact(2)
            .filter_map(|pair| char::from_u32(u16::from_le_bytes([pair[0], pair[1]]) as u32))
            .collect();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_with_cmap(bytes: &[u8], cmap: &HashMap<Vec<u8>, String>) -> String {
    let max_key = cmap.keys().map(Vec::len).max().unwrap_or(1).min(4);
    let mut out = String::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let mut matched = false;
        for len in (1..=max_key).rev() {
            if i + len > bytes.len() {
                continue;
            }
            if let Some(text) = cmap.get(&bytes[i..i + len]) {
                out.push_str(text);
                i += len;
                matched = true;
                break;
            }
        }
        if !matched {
            let b = bytes[i];
            if b.is_ascii_graphic() || b == b' ' {
                out.push(b as char);
            }
            i += 1;
        }
    }
    out
}

fn is_useful_pdf_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.chars().any(|ch| ch.is_alphanumeric())
}

fn has_searchable_text(text: &str) -> bool {
    text.chars().filter(|ch| ch.is_alphanumeric()).count() >= 2
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}


fn extract_ooxml_text(path: &Path, ext: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    let mut parts = Vec::new();
    match ext {
        "docx" | "docm" | "dotx" => {
            if let Some(xml) = read_zip_entry(&mut zip, "word/document.xml") {
                parts.extend(extract_xml_text_runs(&xml, "w:t"));
            }
        }
        "pptx" | "pptm" | "ppsx" | "potx" => {
            let mut slides: Vec<String> = names
                .into_iter()
                .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
                .collect();
            slides.sort();
            for name in slides {
                if let Some(xml) = read_zip_entry(&mut zip, &name) {
                    parts.extend(extract_xml_text_runs(&xml, "a:t"));
                }
            }
        }
        "xlsx" | "xlsm" | "xltx" => {
            let shared = read_zip_entry(&mut zip, "xl/sharedStrings.xml")
                .map(|xml| extract_shared_strings(&xml))
                .unwrap_or_default();
            let mut sheets: Vec<String> = names
                .into_iter()
                .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
                .collect();
            sheets.sort();
            for name in sheets {
                if let Some(xml) = read_zip_entry(&mut zip, &name) {
                    parts.extend(extract_sheet_text(&xml, &shared));
                }
            }
        }
        _ => return None,
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    use std::io::Read;
    let entry = zip.by_name(name).ok()?;
    if entry.size() > MAX_ZIP_ENTRY_BYTES as u64 {
        return None;
    }
    let mut buf = String::new();
    entry
        .take(MAX_ZIP_ENTRY_BYTES as u64 + 1)
        .read_to_string(&mut buf)
        .ok()?;
    (buf.len() <= MAX_ZIP_ENTRY_BYTES).then_some(buf)
}

fn extract_odf_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let xml = read_zip_entry(&mut zip, "content.xml")?;
    let text = extract_plain_xml_text(&xml);
    (!text.trim().is_empty()).then_some(text)
}

fn extract_flat_xml_text(path: &Path) -> Option<String> {
    read_small_text(path)
        .map(|xml| extract_plain_xml_text(&xml))
        .filter(|text| !text.trim().is_empty())
}

fn extract_plain_xml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_xml_entities(&out)
}

fn extract_xml_text_runs(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = xml[pos..].find(&open) {
        let abs = pos + start;
        let after = xml[abs + open.len()..].chars().next();
        if !matches!(
            after,
            Some('>') | Some(' ') | Some('\t') | Some('\r') | Some('\n') | Some('/')
        ) {
            pos = abs + open.len();
            continue;
        }
        let Some(gt) = xml[abs..].find('>') else {
            break;
        };
        if xml[abs..abs + gt].ends_with('/') {
            pos = abs + gt + 1;
            continue;
        }
        let text_start = abs + gt + 1;
        let Some(end_rel) = xml[text_start..].find(&close) else {
            break;
        };
        let decoded = decode_xml_entities(&xml[text_start..text_start + end_rel]);
        if !decoded.trim().is_empty() {
            out.push(decoded);
        }
        pos = text_start + end_rel + close.len();
    }
    out
}

fn extract_shared_strings(xml: &str) -> Vec<String> {
    xml.split("</si>")
        .filter_map(|si| {
            if !si.contains("<si") && !si.contains("<t") {
                return None;
            }
            let runs = extract_xml_text_runs(si, "t");
            Some(runs.join(""))
        })
        .collect()
}

fn extract_sheet_text(xml: &str, shared: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for row in xml.split("</row>").take(500) {
        if !row.contains("<c") {
            continue;
        }
        let mut pos = 0;
        while let Some(rel) = row[pos..].find("<c") {
            let abs = pos + rel;
            let Some(tag) = tag_substr(&row[abs..], "<c") else {
                break;
            };
            let after_tag = abs + tag.len();
            let cell_end = row[after_tag..]
                .find("<c")
                .map(|x| after_tag + x)
                .unwrap_or(row.len());
            let cell_body = &row[after_tag..cell_end];
            let text = match attr_value(&tag, "t").as_deref() {
                Some("s") => inner_text(cell_body, "<v>", "</v>")
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .and_then(|i| shared.get(i).cloned())
                    .unwrap_or_default(),
                Some("inlineStr") => extract_xml_text_runs(cell_body, "t").join(""),
                Some("b") => inner_text(cell_body, "<v>", "</v>")
                    .map(|v| if v.trim() == "1" { "TRUE" } else { "FALSE" }.to_string())
                    .unwrap_or_default(),
                _ => inner_text(cell_body, "<v>", "</v>").unwrap_or_default(),
            };
            let text = decode_xml_entities(text.trim());
            if !text.is_empty() {
                out.push(text);
            }
            pos = cell_end;
            if out.len() >= 5000 {
                return out;
            }
        }
    }
    out
}

fn tag_substr(s: &str, open: &str) -> Option<String> {
    let start = s.find(open)?;
    let end = s[start..].find('>')? + start + 1;
    Some(s[start..end].to_string())
}

fn attr_value(tag: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = tag.find(&pat)? + pat.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn inner_text(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

fn decode_xml_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn extract_binary_strings(path: &Path) -> Option<String> {
    use std::io::Read;
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_BINARY_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_BINARY_BYTES {
        return None;
    }
    let mut out = Vec::new();
    out.extend(extract_ascii_strings(&bytes, 4));
    out.extend(extract_utf16le_strings(&bytes, 4));
    out.sort();
    out.dedup();
    if out.is_empty() {
        None
    } else {
        Some(out.into_iter().take(400).collect::<Vec<_>>().join("\n"))
    }
}

fn extract_utf16be_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for pair in bytes.chunks_exact(2).take(1_000_000) {
        let u = u16::from_be_bytes([pair[0], pair[1]]);
        let ch = char::from_u32(u as u32).unwrap_or('\0');
        if (ch.is_ascii_graphic() || ch == ' ') && ch != '\0' {
            cur.push(ch);
        } else {
            if cur.len() >= min_len {
                out.push(cur.iter().collect::<String>().trim().to_string());
            }
            cur.clear();
        }
    }
    if cur.len() >= min_len {
        out.push(cur.iter().collect::<String>().trim().to_string());
    }
    out
}

fn extract_ascii_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &b in bytes.iter().take(2_000_000) {
        if b.is_ascii_graphic() || b == b' ' {
            cur.push(b);
        } else {
            if cur.len() >= min_len {
                out.push(String::from_utf8_lossy(&cur).trim().to_string());
            }
            cur.clear();
        }
    }
    if cur.len() >= min_len {
        out.push(String::from_utf8_lossy(&cur).trim().to_string());
    }
    out
}

fn extract_utf16le_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for pair in bytes.chunks_exact(2).take(1_000_000) {
        let u = u16::from_le_bytes([pair[0], pair[1]]);
        let ch = char::from_u32(u as u32).unwrap_or('\0');
        if (ch.is_ascii_graphic() || ch == ' ') && ch != '\0' {
            cur.push(ch);
        } else {
            if cur.len() >= min_len {
                out.push(cur.iter().collect::<String>().trim().to_string());
            }
            cur.clear();
        }
    }
    if cur.len() >= min_len {
        out.push(cur.iter().collect::<String>().trim().to_string());
    }
    out
}

fn normalize_content_text(text: String) -> String {
    // Strip NULs (from UTF-16 files etc.) to prevent glib GStrInteriorNulError panics.
    normalize_query_text(&text)
        .replace('\0', "")
        .to_lowercase()
        .chars()
        .take(MAX_CACHED_TEXT_CHARS)
        .collect()
}

fn normalize_query_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}


pub(crate) fn content_snippet_markup(text: &str, query_lower: &str) -> Option<String> {
    let query = normalize_query_text(query_lower).to_lowercase();
    if query.is_empty() {
        return None;
    }

    let lower = text;
    if let Some(idx) = lower.find(&query) {
        let match_char = lower[..idx].chars().count();
        return Some(snippet_around_markup(
            text,
            match_char,
            query.chars().count(),
        ));
    }

    let terms = content_query_terms(&query);
    if !terms.is_empty() && terms.iter().all(|term| lower.contains(term)) {
        let (idx, term) = terms
            .iter()
            .filter_map(|term| lower.find(term).map(|idx| (idx, term)))
            .min_by_key(|(idx, _)| *idx)?;
        let match_char = lower[..idx].chars().count();
        return Some(snippet_around_markup(
            text,
            match_char,
            term.chars().count(),
        ));
    }

    let compact_query = compact_alnum(&query);
    if compact_query.chars().count() >= 3 && lower.chars().count() <= MAX_COMPACT_SNIPPET_CHARS {
        let (compact_text, char_map) = compact_alnum_with_char_map(lower);
        if let Some(idx) = compact_text.find(&compact_query) {
            return char_map.get(idx).copied().map(|match_char| {
                snippet_around_markup(text, match_char, compact_query.chars().count())
            });
        }
        if !terms.is_empty()
            && terms
                .iter()
                .map(|term| compact_alnum(term))
                .filter(|term| term.chars().count() >= 2)
                .all(|term| compact_text.contains(&term))
        {
            if let Some(first) = terms.first().map(|term| compact_alnum(term)) {
                if let Some(idx) = compact_text.find(&first) {
                    return char_map.get(idx).copied().map(|match_char| {
                        snippet_around_markup(text, match_char, first.chars().count())
                    });
                }
            }
        }
    }

    // Fuzzy fallback: OCR text is frequently misread (e.g. "screcnshot" for
    // "screenshot"), so a strict substring/term match misses real hits. Run
    // keyboard-aware similarity per line — it handles substitutions (1→l,
    // 0→o), insertions and deletions, which is what OCR noise looks like.
    if let Some(fuzzy_snippet) = fuzzy_content_snippet(text, &query) {
        return Some(fuzzy_snippet);
    }
    None
}

/// OCR-noise-tolerant match: compare the query against each word of `text`
/// with keyboard-aware similarity, returning a snippet of the best-matching
/// line. Returns None when nothing clears the similarity floor.
fn fuzzy_content_snippet(text: &str, query: &str) -> Option<String> {
    let qlen = query.chars().count();
    if qlen < 3 || text.chars().count() > MAX_CACHED_TEXT_CHARS {
        return None;
    }
    // OCR keeps a word's length roughly intact, so only consider words whose
    // length is near the query's (a 7-char query can't match a 60-char word).
    let min_len = qlen.saturating_sub(2);
    let max_len = qlen + 3;
    const SIM_FLOOR: u32 = 550;

    let mut best: Option<(u32, String, usize, usize)> = None; // (sim, line, line_idx, word_char_idx)
    for (i, line) in text.lines().enumerate() {
        for (word_idx, word) in line.split_whitespace().enumerate() {
            let wlen = word.chars().count();
            if wlen < min_len || wlen > max_len {
                continue;
            }
            if let Some(sim) =
                crate::search::typo::keyboard_similarity(query, &word.to_lowercase())
            {
                if sim >= SIM_FLOOR && best.as_ref().map(|(s, _, _, _)| sim > *s).unwrap_or(true) {
                    let char_idx = line
                        .split_whitespace()
                        .take(word_idx)
                        .map(|w| w.chars().count() + 1)
                        .sum::<usize>();
                    best = Some((sim, line.to_string(), i, char_idx));
                }
            }
        }
    }
    let (_, line, line_idx, word_char_idx) = best?;

    // Highlight the matched word (or query substring within it) so the hit is
    // visible in the snippet.
    let mut out = String::new();
    if line_idx > 0 {
        out.push_str("...");
    }
    // word_char_idx is a char index; compute word_len in char space (no byte slicing).
    let word_len = line.chars().count().saturating_sub(word_char_idx);
    let matched_len = word_len.min(qlen + 1);
    let before: String = line.chars().take(word_char_idx).collect();
    let matched: String = line
        .chars()
        .skip(word_char_idx)
        .take(matched_len)
        .collect();
    let after: String = line.chars().skip(word_char_idx + matched_len).collect();
    out.push_str(&markup_escape_inline(&before));
    out.push_str("<b>");
    out.push_str(&markup_escape_inline(&matched));
    out.push_str("</b>");
    out.push_str(&markup_escape_inline(&after));
    out.push_str("...");
    Some(out)
}

fn content_query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|term| term.chars().count() >= 2)
        .map(str::to_string)
        .collect()
}

fn compact_alnum(text: &str) -> String {
    text.chars().filter(|ch| ch.is_alphanumeric()).collect()
}

fn compact_alnum_with_char_map(text: &str) -> (String, Vec<usize>) {
    let mut compact = String::new();
    let mut char_map = Vec::new();
    for (char_idx, ch) in text.chars().enumerate() {
        if ch.is_alphanumeric() {
            compact.push(ch);
            char_map.push(char_idx);
        }
    }
    (compact, char_map)
}

fn snippet_around_markup(text: &str, match_char: usize, match_len: usize) -> String {
    let start_char = match_char.saturating_sub(30);
    let take = match_char - start_char + match_len + 80;
    let total_chars = text.chars().count();
    let snippet_chars: Vec<char> = text.chars().skip(start_char).take(take).collect();
    let rel_start = match_char - start_char;
    let rel_end = (rel_start + match_len).min(snippet_chars.len());

    let before: String = snippet_chars[..rel_start.min(snippet_chars.len())]
        .iter()
        .collect();
    let matched: String = snippet_chars[rel_start.min(snippet_chars.len())..rel_end]
        .iter()
        .collect();
    let after: String = snippet_chars[rel_end..].iter().collect();

    let mut out = String::new();
    if start_char > 0 {
        out.push_str("...");
    }
    out.push_str(&markup_escape_inline(&before.replace('\n', " ")));
    out.push_str("<b>");
    out.push_str(&markup_escape_inline(&matched.replace('\n', " ")));
    out.push_str("</b>");
    out.push_str(&markup_escape_inline(&after.replace('\n', " ")));
    if start_char + take < total_chars {
        out.push_str("...");
    }
    out.trim().to_string()
}

fn markup_escape_inline(text: &str) -> String {
    // Strip NULs (from UTF-16 / binary-decoded text) to prevent
    // glib GStrInteriorNulError panics in debug builds.
    let text = text.replace('\0', "");
    gtk::glib::markup_escape_text(&text).to_string()
}

pub fn search(query: &str, files: &[FileEntry], cfg: &Config) -> Vec<SearchResult> {
    if files.is_empty() || query.is_empty() {
        return vec![];
    }

    // Type-filter mode: if the first word is a type keyword (image, pdf, txt...),
    // restrict results to those extensions. The rest of the query (if any) is a
    // name filter. "image" alone lists all images; "image vacation" filters by name.
    let mut words = query.splitn(2, char::is_whitespace);
    let first = words.next().unwrap_or("").to_lowercase();
    let name_filter = words.next().unwrap_or("").trim().to_lowercase();
    if let Some(exts) = cfg.extensions_for_word(&first) {
        // Defensive fallback: older/custom configs may accidentally miss "ppt"
        // while still containing "pptx". Ensure presentation mode includes .ppt.
        if first == "ppt" {
            let mut merged = exts.clone();
            for ext in ["ppt", "pptx", "odp", "key"] {
                if !merged.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                    merged.push(ext.to_string());
                }
            }
            return type_filtered(files, &merged, &name_filter);
        }
        return type_filtered(files, exts, &name_filter);
    }
    if first == "ppt" {
        let fallback = vec![
            "ppt".to_string(),
            "pptx".to_string(),
            "odp".to_string(),
            "key".to_string(),
        ];
        return type_filtered(files, &fallback, &name_filter);
    }

    // Normal name search
    search_names(query, files)
}

/// List files whose extension matches `exts`, optionally filtered by name.
fn type_filtered(files: &[FileEntry], exts: &[String], name_filter: &str) -> Vec<SearchResult> {
    let mut hits: Vec<(i32, usize)> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if f.is_dir {
            continue;
        }
        let Some(ext) = f.path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !exts.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            continue;
        }

        let score = if name_filter.is_empty() {
            // No name filter: rank by recency-ish (shorter path first as a proxy)
            2000 - (f.path.as_os_str().len() as i32).min(1500)
        } else if f.name.eq_ignore_ascii_case(name_filter) {
            10000
        } else if starts_with_ci(&f.name, name_filter) {
            5000
        } else if contains_ci(&f.name, name_filter) {
            2000
        } else {
            continue;
        };

        hits.push((score, i));
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.truncate(20);
    hits.into_iter()
        .map(|(score, i)| mk(&files[i], score))
        .collect()
}

/// Standard fuzzy/substring name search across all indexed files.
pub fn search_names(query: &str, files: &[FileEntry]) -> Vec<SearchResult> {
    let ql = query.to_ascii_lowercase();
    let mut hits: Vec<(i32, usize)> = Vec::new();

    for (i, f) in files.iter().enumerate() {
        let nl = f.name_lower.as_str();
        let score: i32 = if nl == ql {
            10000
        } else if nl.starts_with(&ql) {
            5000
        } else if nl.contains(&ql) {
            2000 - (f.name.len() as i32).min(500)
        } else {
            0
        };
        if score != 0 {
            hits.push((score, i));
        }
    }

    // PASS 2 — fuzzy + keyboard-typo fallback. This is ONLY for short,
    // typo-like, SINGLE-word queries. Fuzzy subsequence matching is garbage for
    // long or multi-word phrases: "home designing" would otherwise match
    // "Using-the-Dart-Developer/docs/engine" because those letters can be found
    // scattered across the long path. So we gate it hard:
    //   • query must be a single word (no spaces) — phrases use substring only
    //   • query length 3..=15
    //   • we only keep matches that score well ABOVE a length-scaled floor
    let is_single_word = !ql.trim().contains(char::is_whitespace);
    if hits.len() < 5 && ql.len() >= 3 && ql.len() <= 15 && is_single_word {
        let qbytes = ql.as_bytes();
        let qfirst = qbytes[0];
        let qlen = ql.len();
        let typo_enabled = qlen <= 12;

        let mut matcher = Matcher::default();
        let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);
        // Require a strong fuzzy score. nucleo gives higher scores for tighter,
        // earlier, contiguous matches; scattered subsequence matches score low.
        // A high floor keeps only genuinely close matches (real typos / prefixes).
        let fuzzy_floor = (qlen as u32) * 60;
        const FUZZY_SCAN_CAP: usize = 4000;
        let mut inspected = 0usize;

        for (i, f) in files.iter().enumerate() {
            if inspected >= FUZZY_SCAN_CAP {
                break;
            }
            let name = f.name.as_str();
            let nb = name.as_bytes();

            // GUARD 1 — name must contain the query's first char.
            if !nb.iter().any(|b| b.eq_ignore_ascii_case(&qfirst)) {
                continue;
            }
            // GUARD 2 — name must be at least as long as the query.
            if nb.len() < qlen {
                continue;
            }
            // GUARD 3 — name must not be wildly longer than the query. A 9-char
            // query matching a 60-char path is almost always a junk subsequence,
            // so cap the haystack length relative to the query.
            if nb.len() > qlen * 4 + 12 {
                continue;
            }
            inspected += 1;

            let hay = Utf32String::from(name);
            let mut score = 0i32;
            if let Some(fuzzy) = pattern.score(hay.slice(..), &mut matcher) {
                if fuzzy >= fuzzy_floor {
                    score = (fuzzy as i32).min(1500);
                }
            }
            // GUARD 4 — typo DP only for similar-length names.
            if score == 0 && typo_enabled {
                let ndiff = (nb.len() as i32 - qlen as i32).unsigned_abs() as usize;
                if ndiff <= 3 {
                    let nl = name.to_lowercase();
                    if let Some(sim) = crate::search::typo::keyboard_similarity(&ql, &nl) {
                        score = (sim as i32).min(1200);
                    }
                }
            }
            if score != 0 {
                hits.push((score, i));
            }
        }
    }

    for (score, i) in &mut hits {
        *score += crate::history::frequency_bonus_for(query, &files[*i].name);
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.truncate(20);
    hits.into_iter()
        .map(|(score, i)| mk(&files[i], score))
        .collect()
}

/// Case-insensitive starts_with without allocating.
fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    h[..n.len()].eq_ignore_ascii_case(n)
}

/// Case-insensitive substring search without allocating a lowercased copy.
/// Falls back to a simple byte scan; good enough for filenames.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    let first = n[0].to_ascii_lowercase();
    for i in 0..=h.len() - n.len() {
        if h[i].to_ascii_lowercase() == first && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return true;
        }
    }
    false
}

fn mk(f: &FileEntry, score: i32) -> SearchResult {
    let action = if f.is_dir {
        Action::BrowseInto(f.path.clone())
    } else {
        Action::OpenPath(f.path.clone())
    };
    SearchResult {
        kind: if f.is_dir {
            ResultKind::Folder
        } else {
            ResultKind::File
        },
        title: f.name.clone(),
        subtitle: f.path.parent().map(|p| p.display().to_string()),
        icon: Some(icon_for_path(&f.path, f.is_dir)),
        action,
        score,
    }
}

/// Choose an icon for a file result. This runs for EVERY result on EVERY
/// keystroke, so it must be cheap: no filesystem access, no hashing. We return a
/// type-appropriate symbolic icon name. The real visual thumbnail (PDF first
/// page, etc.) is shown lazily in the preview pane for the SELECTED item only.
fn icon_for_path(p: &std::path::Path, is_dir: bool) -> String {
    icon_for(p, is_dir).to_string()
}

fn icon_for(p: &std::path::Path, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder-symbolic";
    }
    // NOTE: only the "*-generic-symbolic" icons and a few basics are guaranteed
    // present across icon themes. Modern Adwaita ships no PDF icon at all and
    // no script icon, so those fall back to the generic document/text icons;
    // everything here picks symbolic variants so rows never render blank.
    match p
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some(
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" | "bmp" | "tiff" | "tif" | "ico"
            | "heic" | "avif",
        ) => "image-x-generic-symbolic",
        Some("mp3" | "flac" | "ogg" | "wav" | "m4a" | "opus" | "aac" | "wma") => {
            "audio-x-generic-symbolic"
        }
        Some("mp4" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "m4v" | "mpeg" | "mpg") => {
            "video-x-generic-symbolic"
        }
        Some(
            "rs" | "py" | "js" | "ts" | "c" | "cpp" | "h" | "go" | "java" | "rb" | "sh" | "lua"
            | "sql" | "json" | "toml" | "yaml" | "yml",
        ) => "text-x-generic-symbolic",
        Some("zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" | "zst") => {
            "package-x-generic-symbolic"
        }
        Some("pdf") => "x-office-document-symbolic",
        Some(
            "ppt" | "pptx" | "pptm" | "ppsm" | "potx" | "potm" | "pps" | "ppsx" | "odp" | "otp"
            | "fodp" | "key",
        ) => {
            "x-office-presentation-symbolic"
        }
        Some("doc" | "docx" | "odt" | "rtf" | "ott" | "fodt" | "wps" | "pages") => {
            "x-office-document-symbolic"
        }
        Some("xls" | "xlsx" | "ods" | "ots" | "fods" | "csv" | "numbers") => "x-office-spreadsheet-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

pub fn is_content_search_inflight(query: &str) -> bool {
    let ql = normalize_query_text(query).to_lowercase();
    query_inflight().lock().unwrap().contains(&ql)
}





#[cfg(test)]
mod tests {
    use super::{candidate_cost_class, content_snippet_markup, sort_candidates_for_query, FileEntry};
    use std::path::PathBuf;

    #[test]
    fn fuzzy_matches_ocr_misreads() {
        // Content text is lowercased by normalize_content_text before this fn.
        // Exact substring still works.
        assert!(content_snippet_markup("invoice 2023 total 500", "invoice").is_some());
        // OCR noise: 1→l, 0→o, missing letters — keyboard_similarity catches.
        assert!(content_snippet_markup("invo1ce 2023", "invoice").is_some());
        assert!(content_snippet_markup("screcnshot taken at noon", "screenshot").is_some());
        assert!(content_snippet_markup("tota1 amt", "total").is_some());
        // Junk must not match.
        assert!(content_snippet_markup("zzzzzzzzz qqqqq", "invoice").is_none());
        // Fuzzy path is skipped for short queries, but a genuine substring
        // still matches.
        assert!(content_snippet_markup("invo1ce", "inv").is_some());
        assert!(content_snippet_markup("zzzzzz", "inv").is_none());
    }

    #[test]
    fn fuzzy_does_not_match_wildly_different_lines() {
        // Same length but unrelated words.
        assert!(content_snippet_markup("qwertyuiop asdfghjkl", "invoice").is_none());
    }

    #[test]
    fn cost_class_ordering() {
        let tmp = std::env::temp_dir().join("spotty_cost_class_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let cached_png = tmp.join("cached.png");
        let uncached_png = tmp.join("uncached.png");
        let a_pdf = tmp.join("a.pdf");

        // Create minimal files — seed_test_cache only stores the text in the
        // in-memory map; cached_text_for still stats the file to check freshness.
        std::fs::write(&cached_png, b"fake png").unwrap();
        std::fs::write(&uncached_png, b"fake png").unwrap();
        std::fs::write(&a_pdf, b"fake pdf").unwrap();

        // Seed the OCR cache for the cached image only.
        crate::ocr::seed_test_cache(&cached_png, "hello world");

        let to_entry = |p: PathBuf| FileEntry {
            name: p.file_name().unwrap().to_string_lossy().to_string(),
            name_lower: p.file_name().unwrap().to_string_lossy().to_lowercase(),
            path: p,
            is_dir: false,
        };

        let mut files = vec![
            to_entry(a_pdf.clone()),
            to_entry(uncached_png.clone()),
            to_entry(cached_png.clone()),
        ];

        sort_candidates_for_query(&mut files, "hello");

        let paths: Vec<_> = files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().to_string()).collect();

        // Cached image (class 0) must appear before uncached image (class 1)
        // and before the PDF (class 2).
        assert_eq!(paths[0], "cached.png");
        assert_eq!(paths[1], "uncached.png");
        assert_eq!(paths[2], "a.pdf");

        // Verify individual cost classes directly.
        assert_eq!(candidate_cost_class(&cached_png), 0);
        assert_eq!(candidate_cost_class(&uncached_png), 1);
        assert_eq!(candidate_cost_class(&a_pdf), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fuzzy_snippet_em_dash_no_panic() {
        // Lines with multi-byte chars (em dash) before the matched word must not
        // cause a char-boundary panic.
        let text = "previously the user pressed Down to navigate — Mom told me not to touch it";
        // Should not panic and should return a snippet.
        let result = super::fuzzy_content_snippet(text, "mom");
        assert!(result.is_some());
        let snippet = result.unwrap();
        assert!(snippet.contains("<b>"));
    }

    #[test]
    fn fuzzy_snippet_nul_text() {
        // Text with NUL bytes should not panic.
        let text = "hello\x00world";
        let result = super::fuzzy_content_snippet(text, "hello");
        // Should not panic; may return None if NUL confuses matching, but no crash.
        let _ = result;
    }
}
