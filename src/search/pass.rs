//! The `pass` trigger: search your Proton Pass vault by item title and copy a
//! password, username, one-time code or card detail without leaving the
//! launcher. The listing comes from the Proton Pass client built into Spotty
//! (see `crate::proton_pass`); secrets are fetched only when you pick a row.
use super::{Action, ResultKind, SearchResult};
use crate::i18n::gettext;
use crate::proton_pass::{self, Item, Kind, State};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};

/// Most vault matches shown at once; the best one also lists its other actions.
const MAX_ITEMS: usize = 8;
const TOP: i32 = 200_000;

pub fn item_url(item: &Item) -> String {
    format!("https://pass.proton.me/u/0/share/{}/item/{}", item.share_id, item.item_id)
}

fn pass_row(title: String, subtitle: String, icon: &str, op: &str, target: &str, score: i32) -> SearchResult {
    SearchResult {
        kind: ResultKind::System,
        title,
        subtitle: Some(subtitle),
        icon: Some(icon.into()),
        action: Action::ProtonPass { op: op.into(), target: target.into() },
        score,
    }
}

fn open_pass_row(score: i32) -> SearchResult {
    SearchResult {
        kind: ResultKind::Web,
        title: gettext("Open Proton Pass"),
        subtitle: Some(gettext("Your vault in Spotty's Proton window")),
        icon: Some("dialog-password-symbolic".into()),
        action: Action::OpenProtonWeb("https://pass.proton.me/".into()),
        score,
    }
}

/// How well `item` matches the lower-cased `needle`: higher is better, 0 none.
fn rank(needle: &str, pattern: &Pattern, matcher: &mut Matcher, item: &Item) -> i32 {
    let title = item.title.to_lowercase();
    if title == needle {
        return 10_000;
    }
    if title.starts_with(needle) {
        return 5_000 - (title.len() as i32).min(500);
    }
    if title.contains(needle) {
        return 2_500 - (title.len() as i32).min(500);
    }
    if needle.chars().count() >= 2 {
        let haystack = Utf32String::from(item.title.as_str());
        if let Some(fuzzy) = pattern.score(haystack.slice(..), matcher) {
            if fuzzy >= (needle.len() as u32) * 25 {
                return (fuzzy as i32).min(1_800);
            }
        }
    }
    0
}

fn item_subtitle(item: &Item, hint: &str) -> String {
    let kind = match item.kind {
        Kind::Login => gettext("Login"),
        Kind::Note => gettext("Note"),
        Kind::Alias => gettext("Alias"),
        Kind::CreditCard => gettext("Card"),
        Kind::Identity => gettext("Identity"),
        Kind::SshKey => gettext("SSH key"),
        Kind::Wifi => gettext("Wi-Fi"),
        Kind::Custom => gettext("Item"),
    };
    let place = if item.vault.is_empty() { kind } else { format!("{kind} · {}", item.vault) };
    if hint.is_empty() { place } else { format!("{place} — {hint}") }
}

/// The rows for one vault item. `expand` adds its other actions.
fn item_rows(item: &Item, score: i32, expand: bool) -> Vec<SearchResult> {
    let target = item.target();
    let title = if item.title.is_empty() { gettext("(untitled)") } else { item.title.clone() };
    let icon = item.kind.icon();
    let mut rows = Vec::new();
    match item.kind {
        Kind::Login => {
            rows.push(pass_row(title.clone(), item_subtitle(item, &gettext("Enter copies the password")), icon, "password", &target, score));
            if expand {
                rows.push(pass_row(gettext("Copy username"), title.clone(), "avatar-default-symbolic", "username", &target, score - 1));
                rows.push(pass_row(gettext("Copy one-time code"), title.clone(), "appointment-soon-symbolic", "totp", &target, score - 2));
                rows.push(pass_row(gettext("Open website"), title, "web-browser-symbolic", "website", &target, score - 3));
            }
        }
        Kind::CreditCard => {
            rows.push(pass_row(title.clone(), item_subtitle(item, &gettext("Enter copies the card number")), icon, "card", &target, score));
            if expand {
                rows.push(pass_row(gettext("Copy security code"), title, icon, "cvv", &target, score - 1));
            }
        }
        Kind::Note => {
            rows.push(pass_row(title, item_subtitle(item, &gettext("Enter copies the note")), icon, "note", &target, score));
        }
        _ => rows.push(SearchResult {
            kind: ResultKind::Web,
            title,
            subtitle: Some(item_subtitle(item, &gettext("Enter opens it in Proton Pass"))),
            icon: Some(icon.into()),
            action: Action::OpenProtonWeb(item_url(item)),
            score,
        }),
    }
    rows
}

fn message_row(title: String, subtitle: String, op: &str, score: i32) -> SearchResult {
    if op.is_empty() {
        SearchResult {
            kind: ResultKind::System,
            title,
            subtitle: Some(subtitle),
            icon: Some("dialog-password-symbolic".into()),
            action: Action::Noop,
            score,
        }
    } else {
        pass_row(title, subtitle, "dialog-password-symbolic", op, "", score)
    }
}

pub fn search(query: &str) -> Vec<SearchResult> {
    if !proton_pass::available() {
        return vec![message_row(
            gettext("Proton Pass isn't built into this Spotty"),
            gettext("Rebuild Spotty with the proton-pass feature to include Proton's official Pass client."),
            "",
            TOP,
        )];
    }
    proton_pass::ensure_loaded();
    let needle = query.trim().to_lowercase();
    proton_pass::with_items(|state, items| match state {
        State::SignedOut => {
            let subtitle = if crate::proton_native::signed_in() {
                gettext("Uses your Proton account in Spotty — nothing to type")
            } else {
                gettext("Sign in once with your Proton account")
            };
            vec![
                message_row(gettext("Sign in to Proton Pass"), subtitle, "signin", TOP),
                open_pass_row(TOP - 10),
            ]
        }
        State::Failed(message) => vec![
            message_row(gettext("Proton Pass isn't available right now"), message.clone(), "", TOP),
            message_row(gettext("Try again"), gettext("Reload your vault"), "refresh", TOP - 1),
            open_pass_row(TOP - 10),
        ],
        State::Idle | State::Loading => vec![
            message_row(gettext("Loading your vault…"), gettext("Only item titles are loaded; secrets stay in Proton Pass until you copy one"), "", TOP),
            open_pass_row(TOP - 10),
        ],
        State::Ready => ready_rows(&needle, items),
    })
}

fn ready_rows(needle: &str, items: &[Item]) -> Vec<SearchResult> {
    let mut rows = Vec::new();
    if needle.is_empty() {
        rows.push(message_row(
            gettext("Search your Proton Pass vault"),
            gettext("{count} items — type a title; Enter copies the password, which clears after {seconds} s")
                .replace("{count}", &items.len().to_string())
                .replace("{seconds}", &proton_pass::CLEAR_CLIPBOARD_SECS.to_string()),
            "",
            TOP,
        ));
        for (i, item) in items.iter().take(MAX_ITEMS).enumerate() {
            rows.extend(item_rows(item, TOP - 100 * (i as i32 + 1), false));
        }
    } else {
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(needle, CaseMatching::Ignore, Normalization::Smart);
        let mut ranked: Vec<(i32, &Item)> = items
            .iter()
            .filter_map(|item| {
                let score = rank(needle, &pattern, &mut matcher, item);
                (score > 0).then_some((score, item))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.to_lowercase().cmp(&b.1.title.to_lowercase())));
        for (i, (_, item)) in ranked.into_iter().take(MAX_ITEMS).enumerate() {
            rows.extend(item_rows(item, TOP - 100 * i as i32, i == 0));
        }
        if rows.is_empty() {
            rows.push(message_row(
                gettext("No matching item"),
                gettext("Nothing in your Proton Pass vault is titled like that"),
                "",
                TOP,
            ));
        }
    }
    rows.push(open_pass_row(TOP - 100 * (MAX_ITEMS as i32 + 2)));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str, kind: Kind) -> Item {
        Item { share_id: "s1".into(), item_id: id.into(), title: title.into(), kind, vault: "Personal".into() }
    }

    fn titles(rows: &[SearchResult]) -> Vec<String> {
        rows.iter().map(|row| row.title.clone()).collect()
    }

    #[test]
    fn best_login_match_lists_its_other_actions() {
        let items = [item("1", "GitHub", Kind::Login), item("2", "GitLab", Kind::Login), item("3", "Bank card", Kind::CreditCard)];
        let rows = ready_rows("github", &items);
        let names = titles(&rows);
        assert_eq!(names[0], "GitHub");
        assert!(names.contains(&"Copy username".to_owned()));
        assert!(names.contains(&"Copy one-time code".to_owned()));
        assert!(names.contains(&"Open website".to_owned()));
        assert!(matches!(&rows[0].action, Action::ProtonPass { op, target } if op == "password" && target == "s1/1"));
        // Rows keep their order by score.
        assert!(rows.windows(2).all(|pair| pair[0].score >= pair[1].score));
    }

    #[test]
    fn fuzzy_and_prefix_matches_rank_sensibly() {
        let items = [item("1", "Netflix", Kind::Login), item("2", "Nextcloud", Kind::Login)];
        let rows = ready_rows("nex", &items);
        assert_eq!(rows[0].title, "Nextcloud");
        assert!(ready_rows("zzzz", &items).iter().any(|row| row.title == "No matching item"));
    }

    #[test]
    fn cards_notes_and_other_items_get_matching_actions() {
        let card = item_rows(&item("3", "Visa", Kind::CreditCard), 100, true);
        assert!(matches!(&card[0].action, Action::ProtonPass { op, .. } if op == "card"));
        assert!(matches!(&card[1].action, Action::ProtonPass { op, .. } if op == "cvv"));
        let note = item_rows(&item("4", "Wifi code", Kind::Note), 100, true);
        assert!(matches!(&note[0].action, Action::ProtonPass { op, .. } if op == "note"));
        let alias = item_rows(&item("5", "News alias", Kind::Alias), 100, true);
        assert!(matches!(&alias[0].action, Action::OpenProtonWeb(url) if url == "https://pass.proton.me/u/0/share/s1/item/5"));
    }

    #[test]
    fn empty_query_lists_a_few_titles_never_secrets() {
        let items: Vec<Item> = (0..20).map(|n| item(&n.to_string(), &format!("Site {n}"), Kind::Login)).collect();
        let rows = ready_rows("", &items);
        // hint + 8 items + "Open Proton Pass"
        assert_eq!(rows.len(), 1 + MAX_ITEMS + 1);
        assert!(rows[0].subtitle.as_deref().unwrap_or("").contains("20 items"));
    }
}
