// Launching the default browser ourselves — for a private/incognito window.
//
// `gio::AppInfo::launch_default_for_uri()` cannot pass a flag, so a private
// window means reading the browser's own `.desktop` entry and running its
// `Exec=` line with the flag and the URL appended. That is also the only way to
// honour a Flatpak browser (`flatpak run … org.mozilla.firefox @@u %u @@`), so
// field codes and flatpak's `@@` markers are stripped rather than guessed at.

use std::path::{Path, PathBuf};

use crate::search::browser_engine::BrowserKind;

/// The default browser's `.desktop` id (`gio mime x-scheme-handler/https`).
fn default_browser_id() -> Option<String> {
    let out = crate::app::run_host_shell_command("gio mime x-scheme-handler/https").ok()?;
    let id = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("Default application for"))?
        .split_once(':')?
        .1
        .trim()
        .to_string();
    (!id.is_empty() && id != "none").then_some(id)
}

/// Find a desktop entry by id, in the same order the desktop does: the user's
/// own first, then the system and Flatpak export directories.
fn find_desktop_file(desktop_id: &str) -> Option<PathBuf> {
    let file = if desktop_id.ends_with(".desktop") {
        desktop_id.to_string()
    } else {
        format!("{desktop_id}.desktop")
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/share/applications"));
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
        // Per-app Flatpak exports (a sideloaded browser).
        if let Ok(apps) = fs_read_dir(&home.join(".var/app")) {
            dirs.extend(apps.into_iter().map(|a| a.join("exports/share/applications")));
        }
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs.push(PathBuf::from("/usr/local/share/applications"));
    dirs.push(PathBuf::from("/usr/share/applications"));
    dirs.into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

fn fs_read_dir(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(std::fs::read_dir(path)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect())
}

/// The `Exec=` line of a desktop entry.
fn desktop_exec(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(exec) = line.strip_prefix("Exec=") {
            return Some(exec.trim().to_string());
        }
    }
    None
}

/// Turn a desktop `Exec=` line into argv.
///
/// Desktop entry field codes (`%u`, `%U`, `%f`, …) are placeholders for the
/// files/URLs we are about to append ourselves, and flatpak's
/// `--file-forwarding` wraps them in `@@ … @@`; leaving either in place would
/// pass a literal `%u` to the browser.
fn exec_to_argv(exec: &str) -> Vec<String> {
    exec.split_whitespace()
        .filter(|tok| !is_field_code(tok) && !is_flatpak_marker(tok))
        .map(|tok| tok.trim_matches('"').to_string())
        .collect()
}

/// flatpak's file-forwarding wrapper: `@@ … @@`, whose first token also carries
/// the forwarding mode (`@@u`).
fn is_flatpak_marker(token: &str) -> bool {
    token.starts_with("@@")
}

fn is_field_code(token: &str) -> bool {
    // %u %U %f %F %i %c %k %% — and never a lone "%" by accident.
    matches!(token, "%u" | "%U" | "%f" | "%F" | "%i" | "%c" | "%k" | "%%")
}

/// The argv that opens `url` privately in the default browser, or `None` when
/// the browser's private mode is unknown — in which case the caller opens
/// nothing at all rather than opening a window that isn't private.
pub fn private_window_argv(url: &str) -> Option<Vec<String>> {
    let desktop_id = default_browser_id()?;
    let kind = BrowserKind::of(&desktop_id)?;
    let flags = kind.private_flags(&desktop_id);
    let entry = find_desktop_file(&desktop_id)?;
    let exec = desktop_exec(&entry)?;
    let mut argv = exec_to_argv(&exec);
    argv.extend(flags.into_iter().map(str::to_string));
    argv.push(url.to_string());
    log::debug!("private window: launching browser {}", desktop_id);
    Some(argv)
}

/// Run `argv` on the host, detached: from inside the sandbox the browser lives
/// on the host, and a launcher must not wait for it.
pub fn spawn_private_window(argv: &[String]) {
    if argv.is_empty() {
        return;
    }
    let quoted = shell_quote_all(argv);
    let script = format!("{} >/dev/null 2>&1 &", quoted.join(" "));
    let _ = crate::app::spawn_host_shell_command(&script);
}

fn shell_quote_all(argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
                && !a.is_empty()
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_native_desktop_exec() {
        assert_eq!(
            exec_to_argv("/usr/bin/google-chrome-stable %U"),
            vec!["/usr/bin/google-chrome-stable"]
        );
        assert_eq!(
            exec_to_argv("/usr/bin/firefox --new-tab %u"),
            vec!["/usr/bin/firefox", "--new-tab"]
        );
    }

    #[test]
    fn parses_the_flatpak_exec_this_machine_uses() {
        // Taken verbatim from the exported Firefox entry — the `@@u %u @@`
        // wrapper belongs to `--file-forwarding` and must not reach the app.
        let argv = exec_to_argv(
            "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=firefox \
             --file-forwarding org.mozilla.firefox @@u %u @@",
        );
        assert_eq!(
            argv,
            vec![
                "/usr/bin/flatpak",
                "run",
                "--branch=stable",
                "--arch=x86_64",
                "--command=firefox",
                "--file-forwarding",
                "org.mozilla.firefox",
            ]
        );
    }

    #[test]
    fn every_family_has_a_private_flag_and_nothing_else_does() {
        // The gesture is "private or nothing", so an unknown browser must not
        // quietly open a normal window.
        for (id, expected) in [
            ("org.mozilla.firefox.desktop", vec!["--private-window"]),
            ("io.gitlab.librewolf-community.desktop", vec!["--private-window"]),
            ("com.google.Chrome.desktop", vec!["--incognito"]),
            ("org.chromium.Chromium.desktop", vec!["--incognito"]),
            ("com.brave.Browser.desktop", vec!["--incognito"]),
            ("com.microsoft.Edge.desktop", vec!["--incognito"]),
            ("com.vivaldi.Vivaldi.desktop", vec!["--incognito"]),
            ("com.opera.Opera.desktop", vec!["--private"]),
            ("org.gnome.Epiphany.desktop", vec!["--private-instance"]),
            ("org.kde.falkon.desktop", vec!["--private-browsing"]),
            (
                "org.qutebrowser.qutebrowser.desktop",
                vec!["--target", "private-window"],
            ),
        ] {
            let kind = BrowserKind::of(id).unwrap_or_else(|| panic!("{id}"));
            assert_eq!(kind.private_flags(id), expected, "{id}");
        }
        // Browsers we have no private flag for: nothing is offered.
        assert_eq!(BrowserKind::of("com.example.Nonesuch.desktop"), None);
        assert_eq!(BrowserKind::of(""), None);
    }

    #[test]
    fn quotes_arguments_a_shell_would_otherwise_eat() {
        let quoted = shell_quote_all(&[
            "flatpak".into(),
            "run".into(),
            "--query".into(),
            "https://example.org/?q=a b&x=1".into(),
            "it's".into(),
        ]);
        assert_eq!(quoted[3], "'https://example.org/?q=a b&x=1'");
        assert_eq!(quoted[4], "'it'\\''s'");
        assert_eq!(quoted[0], "flatpak", "safe words stay unquoted");
    }
}
#[cfg(test)]
mod live {
    //! Read-only checks against the *system* browser. Ignored by default:
    //! `cargo test -- --ignored --nocapture`.
    use super::*;

    #[test]
    #[ignore = "reads the system default browser's desktop entry"]
    fn resolves_this_machines_private_window() {
        let desktop_id = default_browser_id().expect("a default browser");
        println!("default browser: {desktop_id}");
        let kind = BrowserKind::of(&desktop_id);
        println!("family: {kind:?}");
        let entry = find_desktop_file(&desktop_id).expect("a desktop entry");
        println!("entry: {}", entry.display());
        println!("exec: {}", desktop_exec(&entry).unwrap_or_default());
        let argv = private_window_argv("https://kagi.com/?q=spotty").expect("private argv");
        println!("argv: {}", argv.join(" "));
        // The URL must survive the round trip, whatever the Exec line looks like.
        assert_eq!(argv.last().unwrap(), "https://kagi.com/?q=spotty");
        assert!(!argv.iter().any(|a| a.starts_with("%") || a.starts_with("@@")));
    }
}
