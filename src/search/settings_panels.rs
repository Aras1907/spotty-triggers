// GNOME Control Center panels searchable through Spotty.

use crate::search::{Action, ResultKind, SearchResult};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use crate::i18n::gettext;

struct Panel {
    title: &'static str,
    panel: &'static str,
    icon: &'static str,
    keywords: &'static [&'static str],
}

const PANELS: &[Panel] = &[
    Panel {
        title: "Wi-Fi",
        panel: "wifi",
        icon: "network-wireless-symbolic",
        keywords: &["wifi", "wireless", "internet"],
    },
    Panel {
        title: "Network",
        panel: "network",
        icon: "network-wired-symbolic",
        keywords: &["network", "ethernet", "wired", "vpn"],
    },
    Panel {
        title: "Bluetooth",
        panel: "bluetooth",
        icon: "bluetooth-symbolic",
        keywords: &["bluetooth"],
    },
    Panel {
        title: "Background",
        panel: "background",
        icon: "preferences-desktop-wallpaper-symbolic",
        keywords: &["wallpaper", "background"],
    },
    Panel {
        title: "Appearance",
        panel: "background",
        icon: "applications-graphics-symbolic",
        keywords: &["theme", "appearance"],
    },
    Panel {
        title: "Notifications",
        panel: "notifications",
        icon: "preferences-system-notifications-symbolic",
        keywords: &["notifications", "alerts"],
    },
    Panel {
        title: "Search Settings",
        panel: "search",
        icon: "system-search-symbolic",
        keywords: &["indexing"],
    },
    Panel {
        title: "Applications",
        panel: "applications",
        icon: "preferences-other-symbolic",
        keywords: &["applications", "permissions"],
    },
    Panel {
        title: "Privacy",
        panel: "privacy",
        icon: "preferences-system-privacy-symbolic",
        keywords: &["privacy", "camera", "microphone"],
    },
    Panel {
        title: "Online Accounts",
        panel: "online-accounts",
        icon: "goa-panel-symbolic",
        keywords: &["accounts", "google", "microsoft"],
    },
    Panel {
        title: "Sharing",
        panel: "sharing",
        icon: "preferences-system-sharing-symbolic",
        keywords: &["sharing", "remote"],
    },
    Panel {
        title: "Sound",
        panel: "sound",
        icon: "multimedia-volume-control-symbolic",
        keywords: &["sound", "audio", "volume", "speaker", "microphone"],
    },
    Panel {
        title: "Power",
        panel: "power",
        icon: "battery-symbolic",
        keywords: &["power", "battery", "suspend"],
    },
    Panel {
        title: "Displays",
        panel: "display",
        icon: "preferences-desktop-display-symbolic",
        keywords: &["display", "monitor", "resolution", "brightness"],
    },
    Panel {
        title: "Mouse & Touchpad",
        panel: "mouse",
        icon: "input-mouse-symbolic",
        keywords: &["mouse", "touchpad", "trackpad"],
    },
    Panel {
        title: "Keyboard",
        panel: "keyboard",
        icon: "input-keyboard-symbolic",
        keywords: &["keyboard", "shortcuts"],
    },
    Panel {
        title: "Printers",
        panel: "printers",
        icon: "printer-symbolic",
        keywords: &["printer", "print", "scan"],
    },
    Panel {
        title: "Color",
        panel: "color",
        icon: "preferences-color-symbolic",
        keywords: &["calibration"],
    },
    Panel {
        title: "Region & Language",
        panel: "system region",
        icon: "preferences-desktop-locale-symbolic",
        keywords: &["language", "region", "locale"],
    },
    Panel {
        title: "Accessibility",
        panel: "universal-access",
        icon: "preferences-desktop-accessibility-symbolic",
        keywords: &["accessibility", "a11y"],
    },
    Panel {
        title: "Users",
        panel: "system users",
        icon: "system-users-symbolic",
        keywords: &["users", "password"],
    },
    Panel {
        title: "Default Applications",
        panel: "applications default-apps",
        icon: "preferences-other-symbolic",
        keywords: &["default"],
    },
    Panel {
        title: "Date & Time",
        panel: "system datetime",
        icon: "preferences-system-time-symbolic",
        keywords: &["date", "time", "clock", "timezone"],
    },
    Panel {
        title: "About",
        panel: "system about",
        icon: "help-about-symbolic",
        keywords: &["about", "version"],
    },
];

pub fn search(query: &str) -> Vec<SearchResult> {
    let ql = query.trim().to_lowercase();
    if ql.len() < 2 {
        return vec![];
    }

    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);

    let mut results = Vec::new();
    for p in PANELS {
        let title_lower = p.title.to_lowercase();
        let mut score = 0;

        if title_lower == ql {
            score = 3000;
        } else if title_lower.starts_with(&ql) {
            score = 2000;
        } else if p.keywords.iter().any(|k| k.eq_ignore_ascii_case(&ql)) {
            score = 1500;
        } else if ql.len() >= 3 && p.keywords.iter().any(|k| k.starts_with(&ql)) {
            score = 1200;
        } else if ql.len() >= 3
            && (title_lower.contains(&ql) || p.keywords.iter().any(|k| k.contains(&*ql)))
        {
            score = 900;
        } else if ql.len() >= 2 {
            // Fuzzy fallback: match on title + all keywords concatenated
            let haystack = format!("{} {}", p.title, p.keywords.join(" "));
            if let Some(fs) =
                pattern.score(Utf32String::from(haystack.as_str()).slice(..), &mut matcher)
            {
                if fs >= (ql.len() as u32) * 20 {
                    score = 600;
                }
            }
            // Typo fallback: keyboard-layout typo on title
            if score == 0 {
                if let Some(ts) = crate::search::typo::keyboard_similarity(&ql, &title_lower) {
                    score = (ts as i32 + 300).min(600);
                }
            }
        }

        if score > 0 {
            results.push(SearchResult {
                kind: ResultKind::System,
                title: gettext("Settings: {title}").replace("{title}", &p.title),
                subtitle: Some(gettext("Open in GNOME Settings").into()),
                icon: Some(p.icon.into()),
                action: Action::RunCommand(format!("gnome-control-center {}", p.panel)),
                score,
            });
        }
    }
    results.sort_by(|a, b| b.score.cmp(&a.score));
    results.truncate(2);
    results
}
