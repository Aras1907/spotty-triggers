use crate::config::Config;
use crate::de::Desktop;
use crate::search::{Action, ResultKind, SearchResult};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use std::sync::OnceLock;
use crate::i18n::gettext;

struct Cmd {
    keywords: &'static [&'static str],
    title: &'static str,
    subtitle: &'static str,
    icon: &'static str,
    confirm: bool,
    /// Returns the DE-specific command string.
    command: fn(Desktop) -> String,
}

fn cmd_shutdown(de: Desktop) -> String {
    match de {
        Desktop::Kde => format!(
            "qdbus6 org.kde.Shutdown /Shutdown logoutAndShutdown 2>/dev/null \
             || qdbus org.kde.Shutdown /Shutdown logoutAndShutdown 2>/dev/null \
             || systemctl poweroff || loginctl poweroff"
        ),
        _ => "systemctl poweroff || loginctl poweroff".into(),
    }
}

/// The reboot itself, portable across init systems: systemd →
/// logind/elogind (Artix, Void, Devuan, …) → OpenRC → sysvinit/BusyBox.
/// The last three need root, which the launcher's privileged paths give us.
pub(crate) fn reboot_chain() -> &'static str {
    "systemctl reboot || loginctl reboot || openrc-shutdown -r now \
     || shutdown -r now || reboot"
}

fn cmd_reboot(de: Desktop) -> String {
    match de {
        Desktop::Kde => format!(
            "qdbus6 org.kde.Shutdown /Shutdown logoutAndReboot 2>/dev/null \
             || qdbus org.kde.Shutdown /Shutdown logoutAndReboot 2>/dev/null \
             || {}",
            reboot_chain()
        ),
        _ => reboot_chain().into(),
    }
}

fn cmd_suspend(_de: Desktop) -> String {
    "systemctl suspend || loginctl suspend".into()
}

fn cmd_hibernate(_de: Desktop) -> String {
    "systemctl hibernate || loginctl hibernate".into()
}

fn cmd_lock(de: Desktop) -> String {
    match de {
        Desktop::Gnome | Desktop::Budgie | Desktop::Cosmic => {
            "loginctl lock-session".into()
        }
        Desktop::Kde => format!(
            "loginctl lock-session 2>/dev/null \
             || qdbus org.kde.screensaver /ScreenSaver org.freedesktop.ScreenSaver.Lock 2>/dev/null"
        ),
        Desktop::Cinnamon => {
            "cinnamon-screensaver-command -l 2>/dev/null || loginctl lock-session".into()
        }
        Desktop::Mate => {
            "mate-screensaver-command -l 2>/dev/null || loginctl lock-session".into()
        }
        Desktop::Xfce => "xflock4 2>/dev/null || loginctl lock-session".into(),
        Desktop::Pantheon => format!(
            "dbus-send --session --dest=org.gnome.ScreenSaver --type=method_call \
             /org/gnome/ScreenSaver org.gnome.ScreenSaver.Lock 2>/dev/null \
             || loginctl lock-session"
        ),
        _ => format!(
            "dbus-send --session --dest=org.freedesktop.ScreenSaver --type=method_call \
             /ScreenSaver org.freedesktop.ScreenSaver.Lock 2>/dev/null \
             || loginctl lock-session"
        ),
    }
}

fn cmd_logout(de: Desktop) -> String {
    match de {
        Desktop::Gnome | Desktop::Budgie | Desktop::Cosmic | Desktop::Pantheon => {
            "gnome-session-quit --no-prompt".into()
        }
        Desktop::Kde => format!(
            "qdbus6 org.kde.Shutdown /Shutdown logout 2>/dev/null \
             || qdbus org.kde.Shutdown /Shutdown logout 2>/dev/null \
             || qdbus org.kde.ksmserver /KSMServer logout 0 0 0 2>/dev/null"
        ),
        Desktop::Cinnamon => "cinnamon-session-quit --logout --no-prompt".into(),
        Desktop::Mate => "mate-session-save --force-logout".into(),
        Desktop::Xfce => "xfce4-session-logout --logout --fast".into(),
        _ => format!(
            "gnome-session-quit --no-prompt 2>/dev/null \
             || loginctl terminate-user \"$(id -un)\""
        ),
    }
}

fn hibernate_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        std::fs::read_to_string("/sys/power/state")
            .map(|s| s.contains("disk"))
            .unwrap_or(false)
    })
}

fn action_command(title: &str, de: Desktop) -> String {
    match title {
        "Shut Down" => cmd_shutdown(de),
        "Restart" => cmd_reboot(de),
        "Suspend" => cmd_suspend(de),
        "Hibernate" => cmd_hibernate(de),
        "Lock Screen" => cmd_lock(de),
        "Log Out" => cmd_logout(de),
        _ => unreachable!(),
    }
}

const BUILTIN: &[Cmd] = &[
    Cmd {
        keywords: &["shutdown", "power off", "poweroff", "turn off"],
        title: "Shut Down",
        subtitle: "Power off the computer",
        icon: "system-shutdown-symbolic",
        confirm: true,
        command: cmd_shutdown,
    },
    Cmd {
        keywords: &["reboot", "restart"],
        title: "Restart",
        subtitle: "Reboot the computer",
        icon: "system-reboot-symbolic",
        confirm: true,
        command: cmd_reboot,
    },
    Cmd {
        keywords: &["suspend", "sleep"],
        title: "Suspend",
        subtitle: "Sleep the computer",
        icon: "weather-clear-night-symbolic",
        confirm: true,
        command: cmd_suspend,
    },
    Cmd {
        keywords: &["hibernate"],
        title: "Hibernate",
        subtitle: "Hibernate the computer",
        icon: "weather-clear-night-symbolic",
        confirm: false,
        command: cmd_hibernate,
    },
    Cmd {
        keywords: &["lock", "lock screen"],
        title: "Lock Screen",
        subtitle: "Lock the session",
        icon: "system-lock-screen-symbolic",
        confirm: false,
        command: cmd_lock,
    },
    Cmd {
        keywords: &["logout", "log out", "sign out"],
        title: "Log Out",
        subtitle: "End current session",
        icon: "system-log-out-symbolic",
        confirm: true,
        command: cmd_logout,
    },
];

/// Command that restarts the machine — used by the "restart required to
/// finish the update" notice after a system update.
pub fn reboot_command() -> String {
    action_command("Restart", crate::de::detect())
}

pub fn search(query: &str, _cfg: &Config) -> Vec<SearchResult> {
    search_with_state(query, crate::search::cmd::reboot_pending())
}

/// The session rows for `query`. While a reboot is pending to finish an update,
/// the plain "Restart" row is replaced by the notice that says the updates are
/// waiting for a restart ("Restart required to finish the update") — a bare
/// "Restart" would restart without saying what it applies. The flag is a
/// parameter so the swap is unit-testable without touching globals.
fn search_with_state(query: &str, reboot_needed: bool) -> Vec<SearchResult> {
    let ql = query.to_lowercase();
    if ql.is_empty() {
        return vec![];
    }

    let de = crate::de::detect();
    let mut results = Vec::new();
    let mut matcher = Matcher::default();
    let pattern = Pattern::parse(&ql, CaseMatching::Ignore, Normalization::Smart);

    for cmd in BUILTIN {
        // Skip hibernate when unsupported.
        if cmd.title == "Hibernate" && !hibernate_supported() {
            continue;
        }

        let title_lower = cmd.title.to_lowercase().replace(' ', "");
        let title_spaced = cmd.title.to_lowercase();
        // Tier 1: keyword substring / prefix
        let kw_match = cmd
            .keywords
            .iter()
            .any(|&k| k.contains(&*ql) || ql.contains(k));
        // Tier 2: title contains or starts with query
        let title_match = title_spaced.contains(&*ql) || title_lower.contains(&*ql);
        // Tier 3: nucleo fuzzy on title
        let fuzzy_score = if !kw_match && !title_match && ql.len() >= 2 {
            pattern.score(Utf32String::from(cmd.title).slice(..), &mut matcher)
        } else {
            None
        };
        // Tier 4: keyboard-layout typo detection
        let typo_score = if !kw_match && !title_match {
            crate::search::typo::keyboard_similarity(&ql, &title_lower)
        } else {
            None
        };
        let score = if kw_match || title_match {
            9000
        } else if let Some(fs) = fuzzy_score {
            if fs >= (ql.len() as u32) * 25 {
                7500
            } else {
                continue;
            }
        } else if let Some(ts) = typo_score {
            (ts as i32 + 5000).min(7500)
        } else {
            continue;
        };

        // A pending reboot turns the query's plain Restart row into the
        // update-aware notice (the same row the "update" verb shows on top).
        if cmd.title == "Restart" && reboot_needed {
            results.push(crate::search::cmd::restart_required_row());
            continue;
        }

        let command_str = action_command(cmd.title, de);
        results.push(SearchResult {
            kind: ResultKind::System,
            title: gettext(cmd.title),
            subtitle: Some(gettext(cmd.subtitle)),
            icon: Some(cmd.icon.into()),
            action: if cmd.confirm {
                Action::ConfirmRunCommand(command_str)
            } else {
                Action::RunCommand(command_str)
            },
            score,
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::de::classify;

    #[test]
    fn command_shutdown_kde_uses_graceful() {
        let cmd = cmd_shutdown(classify("KDE"));
        assert!(cmd.contains("logoutAndShutdown"));
    }

    #[test]
    fn command_shutdown_gnome_uses_systemctl() {
        let cmd = cmd_shutdown(classify("GNOME"));
        assert!(cmd.contains("systemctl poweroff"));
        assert!(!cmd.contains("qdbus"));
    }

    #[test]
    fn command_lock_cinnamon_uses_cinnamon_screensaver() {
        let cmd = cmd_lock(classify("X-Cinnamon"));
        assert!(cmd.contains("cinnamon-screensaver-command"));
    }

    #[test]
    fn command_lock_xfce_uses_xflock4() {
        let cmd = cmd_lock(classify("XFCE"));
        assert!(cmd.contains("xflock4"));
    }

    #[test]
    fn command_logout_kde_uses_qdbus() {
        let cmd = cmd_logout(classify("KDE"));
        assert!(cmd.contains("org.kde.Shutdown"));
    }

    #[test]
    fn command_logout_cinnamon() {
        let cmd = cmd_logout(classify("X-Cinnamon"));
        assert!(cmd.contains("cinnamon-session-quit"));
    }

    #[test]
    fn command_logout_mate() {
        let cmd = cmd_logout(classify("MATE"));
        assert!(cmd.contains("mate-session-save"));
    }

    #[test]
    fn command_logout_xfce() {
        let cmd = cmd_logout(classify("XFCE"));
        assert!(cmd.contains("xfce4-session-logout"));
    }

    #[test]
    fn hibernate_hidden_when_unsupported() {
        let supported = hibernate_supported();
        // On this test machine we just check the function runs without panic.
        // The actual filtering is done in search().
        let _ = supported;
    }

    #[test]
    fn action_command_dispatches() {
        let shutdown = action_command("Shut Down", classify("GNOME"));
        assert!(shutdown.contains("poweroff"));
        let reboot = action_command("Restart", classify("GNOME"));
        assert!(reboot.contains("reboot"));
        // …and it still reboots where systemd isn't the init system:
        // elogind, OpenRC, sysvinit/BusyBox fallbacks, in that order.
        assert!(reboot.contains("loginctl reboot"), "{reboot}");
        assert!(reboot.contains("openrc-shutdown -r now"), "{reboot}");
        assert!(reboot.contains("shutdown -r now"), "{reboot}");
        assert!(reboot.ends_with("|| reboot"), "{reboot}");
        let lock = action_command("Lock Screen", classify("GNOME"));
        assert!(lock.contains("lock-session"));
        let logout = action_command("Log Out", classify("GNOME"));
        assert!(logout.contains("gnome-session-quit"));
    }

    #[test]
    fn reboot_kde_keeps_graceful_logout_then_the_generic_chain() {
        let cmd = action_command("Restart", classify("KDE"));
        assert!(cmd.contains("logoutAndReboot"), "{cmd}");
        assert!(cmd.ends_with("|| reboot"), "{cmd}");
    }

    #[test]
    fn pending_reboot_replaces_the_plain_restart_row() {
        let titles = |q: &str, reboot: bool| -> Vec<String> {
            search_with_state(q, reboot)
                .into_iter()
                .map(|r| r.title)
                .collect()
        };
        // Nothing pending: the plain Restart row as always.
        assert!(titles("restart", false).contains(&"Restart".to_string()));
        // Reboot pending → the update-aware notice instead of plain Restart,
        // and no chained update anywhere.
        let t = titles("restart", true);
        assert!(!t.contains(&"Restart".to_string()), "{t:?}");
        assert!(
            t.contains(&crate::i18n::gettext("Restart required to finish the update")),
            "{t:?}"
        );
        assert!(!t.iter().any(|x| x.contains("Update & Restart")), "{t:?}");
        // "reboot" is the same row.
        assert!(titles("reboot", true).iter().any(|x| x.contains("Restart required")));
        // Other session rows stay untouched.
        assert!(titles("shutdown", true).contains(&"Shut Down".to_string()));
        // Unrelated queries gain nothing.
        assert!(titles("zzz", true).is_empty());
    }
}
