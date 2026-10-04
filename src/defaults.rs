//! Native trigger defaults, owned by the spotty-triggers repository.
//! Compiled as a module of Spotty so backends share its host services.
use crate::config::CommandKeyword;
use crate::i18n::gettext;
use crate::triggers::RepoTrigger;
use std::sync::LazyLock;

static CATALOG: LazyLock<Vec<RepoTrigger>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../index.json"))
        .expect("the pinned spotty-triggers catalog must be valid JSON")
});

pub fn supports_builtin(id: &str) -> bool {
    CATALOG.iter().any(|entry| entry.builtin && entry.id == id)
}

pub fn command_keywords() -> Vec<CommandKeyword> {
    CATALOG.iter().filter(|entry| entry.builtin).map(|entry| CommandKeyword {
        id: entry.id.clone(),
        word: entry.word.clone(),
        description: gettext(&entry.description),
        icon: entry.icon.clone(),
        extensions: vec![],
        all_files: entry.id == "files",
        shortcut: entry.shortcut.clone(),
        enabled: true,
    }).collect()
}

/// Spotty's own software-store glyph (a bag with a download arrow), shipped in
/// `data/icons` — Adwaita's `system-software-install-symbolic` is a legacy
/// icon, and GNOME Software's isn't there when GNOME Software isn't.
pub const STORE_ICON: &str = "spotty-store-symbolic";

/// The ids of the result types (in list order). Each one is a keyword with an
/// empty word and shortcut by default, and its switch is a `Config` flag.
pub const RESULT_IDS: [&str; 6] = ["apps", "newapps", "web", "calc", "convert", "updates"];

/// The out-of-the-box order of result types and built-in triggers — higher
/// ranks first in the regular search. Answers that only appear for their own
/// kind of query (a sum, a conversion) lead; the web search, the fallback for
/// everything, comes last. Installed triggers follow after it.
pub const DEFAULT_ORDER: [&str; 11] = [
    "calc", "convert", "apps", "updates", "files", "clipboard", "run", "emoji", "bluetooth",
    "newapps", "web",
];


pub fn display_name(id: &str) -> &'static str {
        match id {
            "files" => "Find",
            "clipboard" => "Clip",
            "cmd" => "App",
            "run" => "Cmd",
            "emoji" => "Emoji",
            "music" => "Music",
            "translate" => "Translate",
            "apps" => "Apps",
            "newapps" => "New Apps",
            "web" => "Web",
            "calc" => "Calc",
            "convert" => "Convert",
            "updates" => "Updates",
            _ => "Trigger",
        }
    }


/// A result type's name in the list and the Store.
pub fn result_title(id: &str) -> String {
    match id {
        "apps" => gettext("Applications"),
        "newapps" => gettext("Search for new Apps"),
        "web" => gettext("Web Search"),
        "calc" => gettext("Calculator"),
        "convert" => gettext("Converter"),
        "updates" => gettext("Updates"),
        _ => crate::search::capitalize(id),
    }
}

/// What a result type adds to the universal search.
pub fn result_blurb(id: &str) -> String {
    match id {
        "apps" => gettext("System, Flatpak, Snap and AppImage apps"),
        "newapps" => gettext("Also suggest installable apps as you type"),
        "web" => gettext("Always show web search row"),
        "calc" => gettext("Arithmetic as you type"),
        "convert" => gettext("Units, currency and number bases"),
        "updates" => gettext("Flatpak, system, Snap and AppImage updates"),
        _ => String::new(),
    }
}

