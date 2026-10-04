use crate::clipboard::{ClipboardEntry, ClipboardHistory};
use crate::search::{Action, ResultKind, SearchResult};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use std::path::Path;

/// Check whether every whitespace-separated token in `query` appears as a
/// substring in `haystack`.  Returns true when `query` is empty.
fn tokens_match(query_lower: &str, haystack: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    query_lower
        .split_whitespace()
        .all(|tok| haystack.contains(tok))
}

/// Build a subtitle string with image details.
/// e.g. "PNG · 1920×1080 · 1.2 MB · 16:9"
fn image_subtitle(path: &Path) -> String {
    match crate::imageinfo::info(path) {
        Some(info) => format!(
            "{} · {}×{} · {} · {}",
            info.format,
            info.width,
            info.height,
            crate::imageinfo::human_size(info.bytes),
            crate::imageinfo::aspect_ratio(info.width, info.height),
        ),
        None => "Image".into(),
    }
}

/// Match a clipboard image against the query.
/// - `None` — no match.
/// - `Some(None)` — matched by empty query, keyword, or image details.
/// - `Some(Some(snippet))` — matched by recognized (OCR) text; `snippet` is
///   the find-mode style highlighted fragment (exact → compact →
///   keyboard-aware fuzzy fallback, same as file content search).
fn image_match(path: &Path, query_lower: &str) -> Option<Option<String>> {
    if query_lower.is_empty() {
        return Some(None);
    }
    // Static keyword matches (fast substring)
    if "image".contains(query_lower)
        || "picture".contains(query_lower)
        || "screenshot".contains(query_lower)
    {
        return Some(None);
    }
    // Token-based match against image details ("1920", "png", "16:9", ...)
    let mut haystack = String::from("image picture screenshot");
    if let Some(info) = crate::imageinfo::info(path) {
        haystack.push(' ');
        haystack.push_str(&crate::imageinfo::search_tokens(&info));
    }
    if tokens_match(query_lower, &haystack) {
        return Some(None);
    }
    // Recognized text: same matcher as find-mode content hits, so OCR
    // misreads still match and the returned snippet highlights the hit.
    if let Some(ocr) = crate::ocr::cached_text_for(path) {
        if let Some(snippet) =
            crate::search::files::content_snippet_markup(&ocr, query_lower)
        {
            return Some(Some(snippet));
        }
    }
    None
}

fn pinned_result(
    text: &str,
    query_lower: &str,
    matcher: &mut Matcher,
    pattern: &Pattern,
) -> Option<SearchResult> {
    if !query_lower.is_empty() {
        let tl = text.to_lowercase();
        if !tl.contains(query_lower) {
            let snippet: String = text.chars().take(300).collect();
            let fs = pattern.score(Utf32String::from(snippet.as_str()).slice(..), matcher);
            if fs
                .map(|s| s < (query_lower.len() as u32) * 20)
                .unwrap_or(true)
            {
                return None;
            }
        }
    }
    let preview: String = text
        .trim()
        .replace(['\n', '\t'], " ")
        .chars()
        .take(120)
        .collect();
    Some(SearchResult {
        kind: ResultKind::Clipboard,
        title: preview,
        subtitle: Some("Pinned — Enter to copy to front".into()),
        icon: Some("view-pin-symbolic".into()),
        action: Action::CopyToClipboard(text.to_string()),
        score: 10_000,
    })
}

fn pinned_image_result(path: &Path, query_lower: &str) -> Option<SearchResult> {
    if !path.exists() {
        return None;
    }
    let ocr_snippet = image_match(path, query_lower)?;
    let details = image_subtitle(path);
    // "Text match: " prefix + <b> keeps markup rendering (see result_row.rs).
    let subtitle = match ocr_snippet {
        Some(snippet) => format!("Text match: {snippet} — {details}"),
        None => format!("{details} — Pinned · Enter to copy back"),
    };
    Some(SearchResult {
        kind: ResultKind::Clipboard,
        title: "Image".into(),
        subtitle: Some(subtitle),
        icon: Some(path.display().to_string()),
        action: Action::CopyImageToClipboard(path.to_path_buf()),
        score: 10_000,
    })
}

fn pinned_file_result(path: &Path, query_lower: &str) -> Option<SearchResult> {
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    if !query_lower.is_empty() && !fname.to_lowercase().contains(query_lower) {
        return None;
    }
    if !path.exists() {
        return None;
    }
    let is_dir = path.is_dir();
    Some(SearchResult {
        kind: ResultKind::Clipboard,
        title: fname.to_string(),
        subtitle: Some(if is_dir {
            "Pinned folder — Enter to copy back".into()
        } else {
            "Pinned file — Enter to copy back".into()
        }),
        icon: Some(if is_dir {
            "folder-symbolic".into()
        } else {
            path.display().to_string()
        }),
        action: Action::CopyFileToClipboard(path.to_path_buf()),
        score: 10_000,
    })
}

fn entry_to_result(
    e: &ClipboardEntry,
    query_lower: &str,
    matcher: &mut Matcher,
    pattern: &Pattern,
) -> Option<SearchResult> {
    match e {
        ClipboardEntry::Text(t) => {
            if !query_lower.is_empty() {
                let tl = t.to_lowercase();
                if !tl.contains(query_lower) {
                    // Fuzzy fallback for text entries
                    let snippet: String = t.chars().take(300).collect();
                    let fs = pattern.score(Utf32String::from(snippet.as_str()).slice(..), matcher);
                    if fs
                        .map(|s| s < (query_lower.len() as u32) * 20)
                        .unwrap_or(true)
                    {
                        return None;
                    }
                }
            }
            let preview: String = t
                .trim()
                .replace(['\n', '\t'], " ")
                .chars()
                .take(120)
                .collect();
            Some(SearchResult {
                kind: ResultKind::Clipboard,
                title: preview,
                subtitle: Some("Text snippet — Enter to copy to front".into()),
                icon: Some("edit-paste-symbolic".into()),
                action: Action::CopyToClipboard(t.clone()),
                score: 100,
            })
        }
        ClipboardEntry::Image(path) => {
            if !path.exists() {
                return None;
            }
            let ocr_snippet = image_match(path, query_lower)?;
            let details = image_subtitle(path);
            // "Text match: " prefix + <b> keeps markup rendering (see result_row.rs).
            let subtitle = match ocr_snippet {
                Some(snippet) => format!("Text match: {snippet} — {details}"),
                None => format!("{details} — Enter to copy back"),
            };
            Some(SearchResult {
                kind: ResultKind::Clipboard,
                title: "Image".into(),
                subtitle: Some(subtitle),
                // Use the actual image path as the icon so a thumbnail renders
                icon: Some(path.display().to_string()),
                action: Action::CopyImageToClipboard(path.clone()),
                score: 100,
            })
        }
        ClipboardEntry::File(path) => {
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
            if !query_lower.is_empty() {
                let fname_lower = fname.to_lowercase();
                if !fname_lower.contains(query_lower) {
                    let fs = pattern.score(Utf32String::from(fname).slice(..), matcher);
                    if fs
                        .map(|s| s < (query_lower.len() as u32) * 20)
                        .unwrap_or(true)
                    {
                        return None;
                    }
                }
            }
            let is_dir = path.is_dir();
            Some(SearchResult {
                kind: ResultKind::Clipboard,
                title: fname.to_string(),
                subtitle: Some(if is_dir {
                    "Copied folder - Enter to copy back".into()
                } else {
                    "Copied file - Enter to copy back".into()
                }),
                icon: Some(if is_dir {
                    "folder-symbolic".into()
                } else {
                    path.display().to_string()
                }),
                action: Action::CopyFileToClipboard(path.clone()),
                score: 100,
            })
        }
    }
}

/// Return all clipboard entries as results (for clipboard-only mode).
/// When `query` is empty, returns everything; otherwise filters.
/// Pinned entries (from config) always appear first with a pin icon.
pub fn all_or_filtered(
    query: &str,
    history: &ClipboardHistory,
    pinned: &[String],
    pinned_images: &[String],
    pinned_files: &[String],
) -> Vec<SearchResult> {
    let ql = query.to_lowercase();
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);

    // Pinned items first (always shown, softly filtered by query)
    let mut results: Vec<SearchResult> = pinned
        .iter()
        .filter_map(|t| pinned_result(t, &ql, &mut matcher, &pattern))
        .collect();
    results.extend(
        pinned_images
            .iter()
            .filter_map(|p| pinned_image_result(Path::new(p), &ql)),
    );
    results.extend(
        pinned_files
            .iter()
            .filter_map(|p| pinned_file_result(Path::new(p), &ql)),
    );

    if history.is_empty() && results.is_empty() {
        return vec![];
    }

    // History entries (skip any that are already pinned)
    let pinned_set: std::collections::HashSet<&str> = pinned.iter().map(|s| s.as_str()).collect();
    let pinned_img_set: std::collections::HashSet<&str> =
        pinned_images.iter().map(|s| s.as_str()).collect();
    let pinned_file_set: std::collections::HashSet<&str> =
        pinned_files.iter().map(|s| s.as_str()).collect();
    let history_results: Vec<SearchResult> = history
        .entries()
        .iter()
        .filter(|e| match e {
            ClipboardEntry::Text(t) => !pinned_set.contains(t.as_str()),
            ClipboardEntry::Image(p) => !pinned_img_set.contains(p.display().to_string().as_str()),
            ClipboardEntry::File(p) => !pinned_file_set.contains(p.display().to_string().as_str()),
        })
        .filter_map(|e| entry_to_result(e, &ql, &mut matcher, &pattern))
        .take(40)
        .collect();

    results.extend(history_results);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_IMG_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn write_test_png(w: u32, h: u32) -> std::path::PathBuf {
        let n = TEST_IMG_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "spotty-clip-test-{}-{}-{}x{}.png",
            std::process::id(),
            n,
            w,
            h
        ));
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn image_match_empty_query_matches_without_snippet() {
        let p = write_test_png(64, 48);
        assert_eq!(image_match(&p, ""), Some(None));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn image_match_keyword_matches_without_snippet() {
        let p = write_test_png(64, 48);
        assert_eq!(image_match(&p, "screenshot"), Some(None));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn image_match_details_tokens() {
        let p = write_test_png(320, 200);
        assert_eq!(image_match(&p, "320"), Some(None));
        assert_eq!(image_match(&p, "png"), Some(None));
        assert_eq!(image_match(&p, "320x200"), Some(None));
        assert_eq!(image_match(&p, "landscape"), Some(None));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn image_match_ocr_text_with_snippet() {
        let p = write_test_png(96, 64);
        crate::ocr::seed_test_cache(&p, "Fitting Room quota exceeded");
        let m = image_match(&p, "fitting");
        assert!(matches!(m, Some(Some(ref s)) if s.contains("<b>")));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn image_match_ocr_fuzzy_misread() {
        let p = write_test_png(96, 64);
        crate::ocr::seed_test_cache(&p, "invo1ce total");
        let m = image_match(&p, "invoice");
        assert!(matches!(m, Some(Some(_))));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn image_match_no_match() {
        let p = write_test_png(64, 48);
        // No OCR seeded, so image text can't match.
        assert_eq!(image_match(&p, "zzzzqqqq"), None);
        let _ = std::fs::remove_file(&p);
    }
}
