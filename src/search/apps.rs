use crate::index::AppEntry;
use crate::search::{Action, ResultKind, SearchResult};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};

/// Minimum keyboard-similarity for a typo match: the very bottom of
/// `keyboard_similarity`'s range, i.e. "the whole edit budget was spent".
/// Accepting it is what finds `gotod` → Godot, `blndr` → Blender.
const TYPO_FLOOR: u32 = 300;
/// A 1–3 char query only gets a 2-edit budget, so at the bottom of the range
/// it matches nearly anything (`gam` ≈ `geary`, `ton` ≈ `text`). Short queries
/// therefore have to look confident, not merely inside budget.
const TYPO_FLOOR_SHORT: u32 = 450;

fn typo_floor(query: &str) -> u32 {
    if query.chars().count() >= 4 {
        TYPO_FLOOR
    } else {
        TYPO_FLOOR_SHORT
    }
}

/// Map a typo similarity onto 1000..=1400 — above the catalogue's
/// `Install: …` rows (`universal_install` scores those 800…770), so an app
/// you already have leads its own install suggestion, but still below keyword
/// (1500) and fuzzy (≤1800) matches so stronger hits keep their rank.
fn typo_score(sim: u32, floor: u32) -> i32 {
    let span = (900 - floor) as i32;
    1000 + (sim.min(900).saturating_sub(floor) as i32 * 400) / span
}

/// Best keyboard-typo score for `query` against an app named `name`
/// (lowercase), or None if nothing is close enough to be a typo.
///
/// Both the whole-name and the per-word comparison are anchored on the
/// query's first letter (see the Tier 6 comment at the call site).
fn typoscore(query: &str, name: &str) -> Option<i32> {
    let first = query.chars().next()?;
    let whole = name
        .starts_with(first)
        .then(|| crate::search::typo::keyboard_similarity(query, name))
        .flatten();
    let floor = typo_floor(query);
    whole.into_iter()
        .chain(crate::search::typo::best_word_similarity(query, name))
        .max()
        .filter(|s| *s >= floor)
        .map(|s| typo_score(s, floor))
}

pub fn search(query: &str, apps: &[AppEntry]) -> Vec<SearchResult> {
    if apps.is_empty() || query.is_empty() {
        return vec![];
    }
    let ql = query.to_lowercase();
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut results = Vec::new();

    for app in apps {
        // Use pre-computed lowercase name — avoids a String allocation per app
        // per keystroke (the hottest path in app search).
        let nl = app.name_lower.as_str();
        let mut score: i32 = 0;

        // Tier 1: exact name match
        if nl == ql {
            score = 10000;
        }
        // Tier 2: name starts with query
        else if nl.starts_with(&ql) {
            score = 5000;
        }
        // Tier 3: name contains query as substring
        else if nl.contains(&ql) {
            // Shorter names rank higher (less noise)
            score = 2500 - (app.name.len() as i32).min(500);
        }
        // Tier 4: keyword match (from .desktop file Keywords field)
        else if app.keywords.iter().any(|k| {
            k.eq_ignore_ascii_case(&ql) || k.to_ascii_lowercase().starts_with(&ql)
        }) {
            score = 1500;
        }
        // Tier 5: stronger fuzzy on the NAME only (no description/comment).
        else if ql.len() >= 2 {
            let hay = Utf32String::from(app.name.as_str());
            if let Some(fuzzy) = pattern.score(hay.slice(..), &mut matcher) {
                // Lower floor to tolerate more typos and partial matches.
                if fuzzy >= (ql.len() as u32) * 25 {
                    score = (fuzzy as i32).min(1800);
                }
            }
            // Tier 6: keyboard-layout typo detection (e.g. "frefox" → "Firefox").
            // Compare the whole name AND each word of it: a multi-word name
            // like "Godot Engine" is far too long for the whole-string check
            // to ever see a typo of its leading word — which is how a typo'd
            // query found the install row for a package you already had but
            // not the app itself.
            //
            // Run even when Tier 5 already matched and keep the better score:
            // a deletion typo like "blendr" IS a subsequence of "Blender", so
            // nucleo finds it but scores it ~150 — below the catalogue's
            // install rows. Both paths are anchored on the query's first
            // letter too: first-letter typos are rare, and unanchored short
            // queries match almost anything ("ton" → "Fonts" via the t↔f
            // keyboard adjacency).
            if let Some(sim) = typoscore(&ql, nl) {
                score = score.max(sim);
            }
        }

        if score == 0 {
            continue;
        }
        results.push(mk(app, score));
    }

    // Apply frequency bonus from history
    for r in &mut results {
        r.score += crate::history::frequency_bonus_for(query, &r.title);
    }

    results.sort_by(|a, b| b.score.cmp(&a.score));
    results
}

fn mk(app: &AppEntry, score: i32) -> SearchResult {
    SearchResult {
        kind: ResultKind::App,
        title: app.name.clone(),
        // Keep subtitle for display only - NOT used in matching
        subtitle: app.comment.clone().or_else(|| app.generic_name.clone()),
        icon: app.icon.clone(),
        action: Action::LaunchDesktopFile(app.desktop_file.clone()),
        score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn app(name: &str) -> AppEntry {
        AppEntry {
            name_lower: name.to_lowercase(),
            name: name.into(),
            generic_name: None,
            comment: None,
            keywords: vec![],
            icon: None,
            desktop_file: PathBuf::from("/tmp/test.desktop"),
        }
    }

    fn titles(query: &str, apps: &[AppEntry]) -> Vec<String> {
        search(query, apps).into_iter().map(|r| r.title).collect()
    }

    #[test]
    fn typo_of_multiword_name_beats_the_install_row() {
        // Before, only the whole display name was typo-checked, so "godto"
        // never got to compare against the word "Godot" (the whole-name
        // check bails when the name is much longer than the query) and the
        // installed app scored 0 — while the catalogue's `Install: godot` row
        // (scored 800) happily showed up.
        let apps = [app("Godot Engine"), app("Video Player"), app("Fonts")];
        let hits = search("godto", &apps);
        assert_eq!(
            hits.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
            ["Godot Engine"]
        );
        // Universal install rows score 800…770: an app we already have leads.
        assert!(hits[0].score > 800, "score {}", hits[0].score);
        assert!(hits[0].score <= 1400, "score {}", hits[0].score);
    }

    #[test]
    fn deletion_typo_outranks_nucleos_weak_score() {
        // "blendr" IS a subsequence of "Blender", so the fuzzy tier finds it
        // first but scores it ~150 — below the install rows. The typo tier
        // must be allowed to lift it.
        let apps = [app("Blender")];
        let hits = search("blendr", &apps);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 800, "score {}", hits[0].score);
    }

    #[test]
    fn short_queries_only_match_near_the_confident_end() {
        // A 3-char query gets a 2-edit budget, so at budget depth it matches
        // almost any name ("gam" ≈ "geary"); keep it strict.
        let apps = [app("Geary"), app("Godot Engine"), app("Heroic Games Launcher")];
        assert_eq!(titles("gam", &apps), ["Heroic Games Launcher"]);
    }

    #[test]
    fn typo_matches_are_anchored_on_the_first_letter() {
        // Unanchored, "ton" is one adjacent-key swap from "Fonts" (t↔f) and
        // two edits from "Text" — neither is what the user meant.
        let apps = [app("Fonts"), app("Text Editor")];
        assert!(titles("ton", &apps).is_empty());
        // Same first letter, transposed middle: still matches.
        assert_eq!(titles("eidtor", &apps), ["Text Editor"]);
    }

    #[test]
    fn exact_and_prefix_tiers_are_untouched() {
        let apps = [app("Godot Engine")];
        // Scores here may carry a history bonus, so assert the tier minimum.
        assert!(search("godot engine", &apps)[0].score >= 10000);
        assert!(search("god", &apps)[0].score >= 5000);
        assert!(search("godot", &apps)[0].score >= 2500 - "Godot Engine".len() as i32);
    }
}
