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

/// Score tiers: a title match ranks above a username match, which ranks above a
/// vault or item-kind match, which ranks above words spread over several fields.
/// Inside the title tier the title's own score decides.
const TITLE_TIER: i32 = 3_000_000;
const USERNAME_TIER: i32 = 2_000_000;
const META_TIER: i32 = 1_000_000;
const SPREAD_TIER: i32 = 100_000;

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

/// How well the item's title matches the lower-cased `needle`: 0 none.
fn title_rank(needle: &str, pattern: &Pattern, matcher: &mut Matcher, item: &Item) -> i32 {
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

/// The item's kind as words to match: its English and translated names.
fn kind_words(kind: Kind) -> String {
    let english = match kind {
        Kind::Login => "login password",
        Kind::Note => "note",
        Kind::Alias => "alias email",
        Kind::CreditCard => "card credit",
        Kind::Identity => "identity",
        Kind::SshKey => "ssh key",
        Kind::Wifi => "wifi wi-fi network",
        Kind::Custom => "item",
    };
    format!("{english} {}", kind_name(kind)).to_lowercase()
}

/// How well `item` matches the lower-cased `needle`, across its title, username,
/// vault and kind. Every word of the search must appear in the same field
/// group (or, for the last tier, anywhere in the item). Higher is better; 0 none.
fn rank(needle: &str, pattern: &Pattern, matcher: &mut Matcher, item: &Item) -> i32 {
    let title = title_rank(needle, pattern, matcher, item);
    if title > 0 {
        return TITLE_TIER + title;
    }
    let words: Vec<&str> = needle.split_whitespace().collect();
    if words.is_empty() {
        return 0;
    }
    let contains_all = |haystack: &str| words.iter().all(|word| haystack.contains(*word));
    let username = item.username.to_lowercase();
    if !username.is_empty() && contains_all(&username) {
        return USERNAME_TIER + 1_000;
    }
    let meta = format!("{} {}", item.vault, kind_words(item.kind)).to_lowercase();
    if contains_all(&meta) {
        return META_TIER;
    }
    let everything = format!("{} {} {meta}", item.title, username).to_lowercase();
    if contains_all(&everything) {
        return SPREAD_TIER;
    }
    0
}

/// Rows for the vault window, shown with the trigger: opens it.
fn open_row(score: i32) -> SearchResult {
    pass_row(
        gettext("Open Proton Pass vault"),
        gettext("Browse, view and copy items in a window"),
        "dialog-password-symbolic",
        "open",
        "",
        score,
    )
}

fn kind_name(kind: Kind) -> String {
    match kind {
        Kind::Login => gettext("Login"),
        Kind::Note => gettext("Note"),
        Kind::Alias => gettext("Alias"),
        Kind::CreditCard => gettext("Card"),
        Kind::Identity => gettext("Identity"),
        Kind::SshKey => gettext("SSH key"),
        Kind::Wifi => gettext("Wi-Fi"),
        Kind::Custom => gettext("Item"),
    }
}

fn item_subtitle(item: &Item, hint: &str) -> String {
    let kind = kind_name(item.kind);
    let mut place = if item.vault.is_empty() { kind } else { format!("{kind} · {}", item.vault) };
    if crate::proton_pass_settings::get().show_username && !item.username.is_empty() {
        place = format!("{} · {place}", item.username);
    }
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
                rows.push(pass_row(gettext("Open website"), title.clone(), "web-browser-symbolic", "website", &target, score - 3));
                rows.push(pass_row(
                    gettext("Copy username, password and code"),
                    format!("{title} · {}", gettext("paste one after another")),
                    "edit-copy-symbolic",
                    "sequence",
                    &target,
                    score - 4,
                ));
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
            kind: ResultKind::System,
            title,
            subtitle: Some(item_subtitle(item, &gettext("Spotty can't copy from this kind of item"))),
            icon: Some(icon.into()),
            action: Action::Noop,
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
    if !crate::proton_pass_pin::unlocked() {
        // Nothing of the vault, not even titles, until the PIN is typed.
        return vec![message_row(
            gettext("Proton Pass is locked"),
            gettext("Press Enter and type your PIN"),
            "unlock",
            TOP,
        )];
    }
    proton_pass::ensure_loaded();
    let needle = query.trim().to_lowercase();
    proton_pass::with_items(|state, items| match state {
        State::SignedOut => vec![
            message_row(gettext("Sign in to Proton Pass"), gettext("Type your Proton email and password in the Proton Pass settings"), "signin", TOP),
            open_row(TOP - 1),
        ],
        State::Failed(message) => vec![
            message_row(gettext("Proton Pass isn't available right now"), message.clone(), "", TOP),
            message_row(gettext("Try again"), gettext("Reload your vault"), "refresh", TOP - 1),
        ],
        State::Idle | State::Loading => vec![message_row(
            gettext("Loading your vault…"),
            gettext("Only item titles are loaded; secrets stay in Proton Pass until you copy one"),
            "",
            TOP,
        )],
        State::Ready => ready_rows(&needle, items),
    })
}

fn ready_rows(needle: &str, items: &[Item]) -> Vec<SearchResult> {
    let settings = crate::proton_pass_settings::get();
    let mut rows = Vec::new();
    if !settings.search_titles {
        // Titles are not searched at all; the window is the way in.
        rows.push(message_row(
            gettext("Item titles aren't searched"),
            gettext("Turn this on in the Proton Pass settings, or open the vault window"),
            "",
            TOP,
        ));
        rows.push(open_row(TOP - 1));
    } else if needle.is_empty() {
        rows.push(message_row(
            gettext("Search your Proton Pass vault"),
            gettext("{count} items — type a title; Enter copies the password, which clears after {seconds} s")
                .replace("{count}", &items.len().to_string())
                .replace("{seconds}", &proton_pass::clear_after_secs().to_string()),
            "",
            TOP,
        ));
        rows.push(open_row(TOP - 1));
        for (i, item) in items.iter().take(MAX_ITEMS).enumerate() {
            rows.extend(item_rows(item, TOP - 100 * (i as i32 + 2), false));
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
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str, kind: Kind) -> Item {
        Item { share_id: "s1".into(), item_id: id.into(), title: title.into(), kind, vault: "Personal".into(), username: String::new() }
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
        assert!(matches!(&alias[0].action, Action::Noop));
    }

    fn item_with(id: &str, title: &str, kind: Kind, vault: &str, username: &str) -> Item {
        Item { share_id: "s1".into(), item_id: id.into(), title: title.into(), kind, vault: vault.into(), username: username.into() }
    }

    #[test]
    fn proton_pass_search_ranks_title_then_username_then_vault() {
        let items = [
            item_with("1", "Bank", Kind::Login, "Personal", ""),
            item_with("2", "Shop", Kind::Login, "Work", "personal@example.test"),
            item_with("3", "Personal log", Kind::Login, "Work", ""),
        ];
        let rows = ready_rows("personal", &items);
        let logins: Vec<String> = rows
            .iter()
            .filter(|row| matches!(&row.action, Action::ProtonPass { op, .. } if op == "password"))
            .map(|row| row.title.clone())
            .collect();
        assert_eq!(logins, vec!["Personal log", "Shop", "Bank"]);
    }

    #[test]
    fn proton_pass_search_matches_vault_kind_and_words_across_fields() {
        let items = [
            item_with("1", "Visa", Kind::CreditCard, "Finance", ""),
            item_with("2", "GitHub", Kind::Login, "Personal", ""),
        ];
        // The kind's name and the vault's name are searchable.
        assert_eq!(ready_rows("card", &items)[0].title, "Visa");
        assert_eq!(ready_rows("finance", &items)[0].title, "Visa");
        // Words may come from different fields, as long as each word is found.
        assert_eq!(ready_rows("github personal", &items)[0].title, "GitHub");
        assert!(ready_rows("github finance", &items).iter().all(|row| row.title != "GitHub"));
    }

    #[test]
    fn proton_pass_login_rows_offer_copy_all_for_the_best_match() {
        let items = [item("1", "GitHub", Kind::Login)];
        let rows = ready_rows("github", &items);
        assert!(rows.iter().any(|row| matches!(&row.action, Action::ProtonPass { op, target } if op == "sequence" && target == "s1/1")));
        // Only the best match offers it; the bare title list does not.
        let plain = ready_rows("", &items);
        assert!(!plain.iter().any(|row| matches!(&row.action, Action::ProtonPass { op, .. } if op == "sequence")));
    }

    #[test]
    fn empty_query_lists_a_few_titles_never_secrets() {
        let items: Vec<Item> = (0..20).map(|n| item(&n.to_string(), &format!("Site {n}"), Kind::Login)).collect();
        let rows = ready_rows("", &items);
        // hint, open row, then 8 items
        assert_eq!(rows.len(), 2 + MAX_ITEMS);
        assert!(rows[0].subtitle.as_deref().unwrap_or("").contains("20 items"));
    }
}
