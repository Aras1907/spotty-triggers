use crate::i18n::gettext;

// "cmd" trigger mode: run an arbitrary shell command live inside Spotty.
//
// The typed text is treated as a shell command line. Pressing Enter runs it
// through `sh -lc` (on the host via `flatpak-spawn --host` when sandboxed) and
// streams its output into the in-window progress pane. Recently run commands
// are remembered so they can be re-run from the suggestion list.

use super::{Action, ResultKind, SearchResult};
use std::sync::{Mutex, OnceLock};

/// Most-recently-run commands, newest first (capped). Kept in memory for the
/// lifetime of the daemon so the user can quickly re-run things they like.
fn recent() -> &'static Mutex<Vec<String>> {
    static R: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// Same expansion limit as the rest of the result list.
const LIMIT: usize = 5;

/// Record a command the user just ran, so it shows up as a re-run suggestion.
pub fn record(command: &str) {
    let cmd = command.trim();
    if cmd.is_empty() {
        return;
    }
    let mut r = recent().lock().unwrap();
    r.retain(|c| c != cmd);
    r.insert(0, cmd.to_string());
    r.truncate(20);
}

pub(crate) fn is_sandbox() -> bool {
    std::env::var("FLATPAK_ID").is_ok()
}

/// Build the argv that runs `cmd` through a login shell, on the host when
/// we're sandboxed (so it sees the real system, not the Flatpak runtime).
pub fn command_argv(cmd: &str) -> Vec<String> {
    if is_sandbox() {
        vec![
            "flatpak-spawn".into(),
            "--host".into(),
            "sh".into(),
            "-lc".into(),
            cmd.to_string(),
        ]
    } else {
        vec!["sh".into(), "-lc".into(), cmd.to_string()]
    }
}

fn run_result(cmd: &str, subtitle: &str, score: i32) -> SearchResult {
    SearchResult {
        kind: ResultKind::System,
        title: format!("Run: {}", cmd),
        subtitle: Some(subtitle.into()),
        icon: Some("utilities-terminal-symbolic".into()),
        action: Action::RunWithProgress {
            title: cmd.to_string(),
            args: command_argv(cmd),
        },
        score,
    }
}

pub fn search(query: &str) -> Vec<SearchResult> {
    let q = query.trim();

    if q.is_empty() {
        let r = recent().lock().unwrap();
        if r.is_empty() {
            return vec![SearchResult {
                kind: ResultKind::System,
                title: gettext("Type a command to run").into(),
                subtitle: Some(gettext("Runs through your shell, output shown here").into()),
                icon: Some("utilities-terminal-symbolic".into()),
                action: Action::EnterMode("cmd".into()),
                score: 1000,
            }];
        }
        return r
            .iter()
            .take(LIMIT)
            .enumerate()
            .map(|(i, c)| run_result(c, "Recent · Enter to run", 1000 - i as i32))
            .collect();
    }

    // Top result runs exactly what was typed; recent commands that contain the
    // query fill out the rest of the (5-row) list as quick re-run suggestions.
    let mut out = vec![run_result(q, "Enter to run in Spotty", 100_000)];
    let ql = q.to_lowercase();
    let r = recent().lock().unwrap();
    for (i, c) in r
        .iter()
        .filter(|c| {
            if c.as_str() == q {
                return false;
            }
            let lc = c.to_lowercase();
            // Recent commands: substring first, then a fuzzy pass so a
            // typo'd command ("dcoker ps") still resurfaces.
            lc.contains(&ql) || crate::search::fuzzy_match(&ql, &lc)
        })
        .enumerate()
    {
        if out.len() >= LIMIT {
            break;
        }
        out.push(run_result(c, "Recent · Enter to run", 1000 - i as i32));
    }
    out
}
