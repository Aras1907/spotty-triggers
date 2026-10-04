// Background package operations (install / uninstall).
//
// Operations run on detached threads and record their state in a global
// registry, so they keep running even when the search window is hidden. While
// an operation is running it shows up as a live progress row (a loading bar
// with a percentage when the tool reports one). When it finishes it briefly
// shows a "Completed"/"Failed" state and is then removed automatically.

use crate::search::{Action, ResultKind, SearchResult};
use gtk::glib;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use crate::i18n::gettext;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone)]
struct Operation {
    id: u64,
    /// Human label, e.g. "Installing Firefox".
    title: String,
    /// Where the package comes from, e.g. "Flatpak".
    source: String,
    /// Icon name / app-id to show on the row.
    icon: String,
    /// Most recent output line, shown as live status.
    status: String,
    /// Reported completion fraction (0.0–1.0), when the tool emits one.
    progress: Option<f64>,
    state: State,
    /// PID of the spawned process (the immediate child, e.g. `flatpak` or
    /// `pkexec`), so the operation can be cancelled from the UI.
    pid: Option<u32>,
    /// Full argv (program + args), kept so a cancelled operation can be
    /// restarted from scratch.
    args: Vec<String>,
    /// When set, the row is hidden behind a reversible swipe-dismiss state.
    dismissed_dir: Option<f64>,
    /// Deadline for auto-closing a dismissed item if it is not restored.
    pending_commit_at: Option<Instant>,
}

/// Grace period a finished operation lingers so the user sees it completed.
const DONE_GRACE: Duration = Duration::from_millis(2200);
/// How long a cancelled operation stays visible (with a redo button) so the
/// user has time to press Enter again to restart it.
const CANCEL_GRACE: Duration = Duration::from_secs(30);
const DISMISS_GRACE: Duration = Duration::from_secs(4);
const PENDING_SWEEP: Duration = Duration::from_millis(250);

fn registry() -> &'static Mutex<Vec<Operation>> {
    static C: OnceLock<Mutex<Vec<Operation>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Vec::new()))
}

/// A finished operation kept for the Operations history popover.
#[derive(Clone)]
struct HistoryEntry {
    id: u64,
    title: String,
    source: String,
    icon: String,
    state: State,
    at: Instant,
    dismissed_dir: Option<f64>,
    pending_commit_at: Option<Instant>,
}

/// Past operations (installs/uninstalls/commands), newest first.
fn history() -> &'static Mutex<Vec<HistoryEntry>> {
    static C: OnceLock<Mutex<Vec<HistoryEntry>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Vec::new()))
}

/// How many past operations to keep around.
const HISTORY_CAP: usize = 50;

fn history_next_id() -> u64 {
    static H: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    H.fetch_add(1, Ordering::Relaxed)
}

fn push_history(
    title: String,
    source: String,
    icon: String,
    state: State,
    dismissed_dir: Option<f64>,
    pending_commit_at: Option<Instant>,
) {
    let mut h = history().lock().unwrap();
    h.insert(
        0,
        HistoryEntry {
            id: history_next_id(),
            title,
            source,
            icon,
            state,
            at: Instant::now(),
            dismissed_dir,
            pending_commit_at,
        },
    );
    h.truncate(HISTORY_CAP);
}

/// Remove a history entry by id (swipe-to-delete in the Operations popover).
pub fn remove_history(id: u64) {
    history().lock().unwrap().retain(|e| e.id != id);
    nudge_ui();
}

/// Record a free-form command (run via the in-window runner, not the registry)
/// in the Operations history so it shows up alongside installs/uninstalls.
pub fn record_command(command: &str, ok: bool) {
    push_history(
        gettext("Run: {command}").replace("{command}", command),
        "Command".into(),
        "utilities-terminal-symbolic".into(),
        if ok { State::Done } else { State::Failed },
        None,
        None,
    );
    nudge_ui();
}

/// One item shown in the Operations popover (ongoing + past).
#[derive(Clone)]
pub struct OpItem {
    pub title: String,
    pub detail: String,
    /// "running" | "done" | "failed" | "cancelled"
    pub state: &'static str,
    pub icon: String,
    /// Deterministic progress fraction (0..1) for running operations.
    pub progress: Option<f64>,
    /// Set for running operations: swiping calls `cancel(op_id)`.
    pub op_id: Option<u64>,
    /// Set for history entries: swiping calls `remove_history(hist_id)`.
    pub hist_id: Option<u64>,
    /// Swipe direction used to hide this item pending restore/commit.
    pub dismissed_dir: Option<f64>,
}

fn relative(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    if secs < 60 {
        gettext("just now")
    } else if secs < 3600 {
        gettext("{n}m ago").replace("{n}", &(secs / 60).to_string())
    } else if secs < 86_400 {
        gettext("{n}h ago").replace("{n}", &(secs / 3600).to_string())
    } else {
        gettext("{n}d ago").replace("{n}", &(secs / 86_400).to_string())
    }
}

/// Ongoing operations (newest first) followed by past ones, for the popover.
pub fn popover_items() -> Vec<OpItem> {
    let mut out = Vec::new();
    {
        let reg = registry().lock().unwrap();
        for op in reg.iter().rev().filter(|o| o.state == State::Running) {
            out.push(OpItem {
                title: op.title.clone(),
                detail: gettext("{source} · {status}").replace("{source}", &op.source).replace("{status}", &op.status),
                state: "running",
                icon: op.icon.clone(),
                progress: op.progress,
                op_id: Some(op.id),
                hist_id: None,
                dismissed_dir: op.dismissed_dir,
            });
        }
    }
    let h = history().lock().unwrap();
    for e in h.iter() {
        let state = match e.state {
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Running => "running",
        };
        out.push(OpItem {
            title: e.title.clone(),
            detail: gettext("{source} · {status}").replace("{source}", &e.source).replace("{status}", &relative(e.at)),
            state,
            icon: e.icon.clone(),
            progress: None,
            op_id: None,
            hist_id: Some(e.id),
            dismissed_dir: e.dismissed_dir,
        });
    }
    out
}

fn next_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn nudge_ui() {
    // Always defer to an idle tick rather than `MainContext::invoke`, which runs
    // synchronously when called from the main thread. A cancel/start triggered by
    // a UI callback would otherwise re-enter `refresh_search_window` and rebuild
    // the very popover/rows we're inside — freezing the UI. Deferring lets the
    // current callback unwind first.
    glib::idle_add_once(crate::app::refresh_search_window);
}

/// After an operation finishes or is cancelled, clear the installed-package
/// caches and re-enumerate desktop apps so the results list updates immediately.
fn post_op_refresh() {
    crate::search::cmd::invalidate_installed_caches();
    // Re-enumerate apps on a background thread to avoid stalling the UI, then
    // hand the result to the main thread. App state is a thread-local, so it
    // can only be touched there — reaching for it from this thread used to
    // panic with "not init" and silently drop the refresh.
    std::thread::spawn(|| {
        let apps = crate::index::enum_apps();
        glib::MainContext::default().invoke(move || {
            crate::app::with_state(|st| {
                st.indexer.snapshot().write().unwrap().apps = apps;
            });
            nudge_ui();
        });
    });
}

fn ensure_pending_sweeper() {
    static START: OnceLock<()> = OnceLock::new();
    START.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(PENDING_SWEEP);
            let now = Instant::now();
            let op_ids: Vec<u64> = {
                let reg = registry().lock().unwrap();
                reg.iter()
                    .filter(|o| {
                        o.dismissed_dir.is_some() && o.pending_commit_at.is_some_and(|at| at <= now)
                    })
                    .map(|o| o.id)
                    .collect()
            };
            let hist_ids: Vec<u64> = {
                let h = history().lock().unwrap();
                h.iter()
                    .filter(|e| {
                        e.dismissed_dir.is_some() && e.pending_commit_at.is_some_and(|at| at <= now)
                    })
                    .map(|e| e.id)
                    .collect()
            };
            for id in op_ids {
                cancel_silently(id);
            }
            for id in hist_ids {
                remove_history(id);
            }
        });
    });
}

/// Progress/status handle for an operation that runs *inside* Spotty rather
/// than as a child process (see [`start`]'s [`crate::dnf5daemon`] argv).
///
/// Held by the worker thread, so every method is safe to call from there.
#[derive(Clone, Copy)]
pub struct TaskHandle {
    id: u64,
}

impl TaskHandle {
    /// Publish the current status line (shown next to the title).
    pub fn status(&self, status: &str) {
        update(self.id, status, None);
        nudge_ui();
    }

    /// Publish progress (0..=1, when known) and, if given, a new status line.
    pub fn report(&self, fraction: Option<f64>, status: Option<&str>) {
        update(self.id, status.unwrap_or(""), fraction);
        nudge_ui();
    }

    /// True once the user cancelled this operation — long calls poll this.
    pub fn cancelled(&self) -> bool {
        is_cancelled(self.id)
    }
}

/// Start a background operation running `args` (argv; `args[0]` is the program).
///
/// Two `args[0]` are special: [`crate::dnf5daemon::ARGV0`] and
/// [`crate::packagekit::ARGV0`] mean the work is done in-process over D-Bus
/// (a distro install or update driven by the service the Software store uses)
/// rather than by spawning a program, so it runs on a
/// plain thread and reports through a [`TaskHandle`]. Everything else — the
/// registry, cancel, restart, undo, history — is identical either way.
pub fn start(title: String, source: String, icon: String, args: Vec<String>) {
    let id = next_id();
    registry().lock().unwrap().push(Operation {
        id,
        title,
        source,
        icon,
        status: "Starting…".into(),
        progress: None,
        state: State::Running,
        pid: None,
        args: args.clone(),
        dismissed_dir: None,
        pending_commit_at: None,
    });
    nudge_ui();
    run(id, args);
}

/// Run an operation's work: in-process for the daemon sentinel, a child
/// process otherwise.
fn run(id: u64, args: Vec<String>) {
    if is_in_process(&args) {
        run_task(id, &args);
    } else {
        run_process(id, args);
    }
}

/// True for an argv that Spotty runs itself over D-Bus: dnf5daemon (Fedora's
/// store daemon) or PackageKit (every other distro).
fn is_in_process(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some(a) if a == crate::dnf5daemon::ARGV0 || a == crate::packagekit::ARGV0
    )
}

/// Run an in-process operation on its own thread.
fn run_task(id: u64, args: &[String]) {
    let packagekit = args.first().map(String::as_str) == Some(crate::packagekit::ARGV0);
    let rest = args[1..].to_vec();
    std::thread::spawn(move || {
        let task = TaskHandle { id };
        let result = if packagekit {
            crate::packagekit::run_task(&rest, &task)
        } else {
            crate::dnf5daemon::run_task(&rest, &task)
        };
        match result {
            Ok(()) => finish(id, State::Done),
            // A cancellation is not a failure: the row already says so, and
            // `finish` would overwrite it with a "Failed" state.
            Err(e) => {
                if is_cancelled(id) {
                    log::info!("task cancelled: {e}");
                } else {
                    log::info!("task failed: {e}");
                    finish(id, State::Failed);
                }
            }
        }
    });
}

/// Restart a cancelled operation from scratch, reusing its original argv.
pub fn restart(id: u64) {
    let args = {
        let mut reg = registry().lock().unwrap();
        match reg.iter_mut().find(|o| o.id == id) {
            Some(op) if op.state == State::Cancelled => {
                op.state = State::Running;
                op.status = "Starting…".into();
                op.progress = None;
                op.pid = None;
                op.dismissed_dir = None;
                op.pending_commit_at = None;
                op.args.clone()
            }
            _ => return,
        }
    };
    nudge_ui();
    run(id, args);
}

fn run_process(id: u64, mut args: Vec<String>) {
    if args.is_empty() {
        finish(id, State::Failed);
        return;
    }
    let program = args.remove(0);

    // flatpak consults $BROWSER for webflow auth.  Override it so that
    // authenticated remotes never open a browser on the host — they fail
    // cleanly instead (same as --noninteractive did).
    let is_flatpak = program == "flatpak"
        || (program == "flatpak-spawn" && args.first().map(|s| s.as_str()) == Some("--host"));

    std::thread::spawn(move || {
        // Phase-aware tracker: knows how the running tool (flatpak, dnf, …)
        // splits its output into real units of work, so metadata `100%` lines
        // can no longer pin the orb before anything has been installed.
        let argv: Vec<String> = std::iter::once(program.clone())
            .chain(args.iter().cloned())
            .collect();
        let mut tracker = crate::opprogress::Tracker::new(&argv);

        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if is_flatpak {
            cmd.env("BROWSER", "true");
        }
        let child = cmd.spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(_) => {
                finish(id, State::Failed);
                return;
            }
        };
        {
            let mut reg = registry().lock().unwrap();
            if let Some(op) = reg.iter_mut().find(|o| o.id == id) {
                if op.state == State::Cancelled {
                    drop(reg);
                    let _ = child.kill();
                    return;
                }
                op.pid = Some(child.id());
            }
        }

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, tx.clone());
        }
        drop(tx);

        // Tools like flatpak emit progress via carriage-return updates many
        // times per second. Writing each one into the shared registry would
        // hammer its mutex in a tight loop and can starve the GTK main thread of
        // the same lock — making the window unresponsive (e.g. when cancelling).
        // So we keep only the latest line and flush it (registry write + UI
        // nudge) at most every ~120ms.
        let mut last_flush = Instant::now() - Duration::from_secs(1);
        let mut pending: Option<(String, Option<f64>)> = None;
        for line in rx {
            let line = line.trim().to_string();
            if !line.is_empty() {
                // Abort immediately on webflow — prevents browser popup for
                // authenticated remotes (BROWSER=true already prevents the
                // launch, but this avoids a hang if BROWSER is overridden
                // elsewhere).
                if is_flatpak && line.contains("Waiting for browser") {
                    update(id, "Authentication required (remote login unsupported)", None);
                    let _ = child.kill();
                    break;
                }
                tracker.feed(&line);
                let frac = tracker.fraction();
                if crate::opprogress::is_part_marker(&line) {
                    // Internal `__spotty_part_…__` markers from chained update
                    // commands move progress on but never clobber the status.
                    match &mut pending {
                        Some(p) => p.1 = frac,
                        None => pending = Some((String::new(), frac)),
                    }
                } else {
                    pending = Some((clean_status(&line), frac));
                }
            }
            // Touch the shared registry only on the throttle tick (not per line):
            // here we both check for cancellation (bail out + kill so a cancelled
            // install stops promptly) and flush the latest progress + UI nudge.
            if last_flush.elapsed() >= Duration::from_millis(120) {
                if is_cancelled(id) {
                    let _ = child.kill();
                    break;
                }
                if let Some((l, p)) = pending.take() {
                    update(id, &l, p);
                }
                nudge_ui();
                last_flush = Instant::now();
            }
        }
        // Flush the final status line.
        if let Some((l, p)) = pending.take() {
            update(id, &l, p);
        }

        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        finish(id, if ok { State::Done } else { State::Failed });
    });
}

// Read `r` line by line, splitting on BOTH '\n' and '\r' so carriage-return
// progress updates (as flatpak emits) are captured incrementally.
fn spawn_reader<R: Read + Send + 'static>(r: R, tx: std::sync::mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(r);
        let mut buf: Vec<u8> = Vec::with_capacity(128);
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\n' || byte[0] == b'\r' {
                        if !buf.is_empty() {
                            let line = String::from_utf8_lossy(&buf).into_owned();
                            if tx.send(line).is_err() {
                                return;
                            }
                            buf.clear();
                        }
                    } else {
                        buf.push(byte[0]);
                    }
                }
                Err(_) => break,
            }
        }
        if !buf.is_empty() {
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        }
    });
}

// Pull a percentage (e.g. the "57" in "57%") out of a status line, if present.
// Also understands package-manager download counters as a fraction, since
// piped tools suppress their "%" progress bar entirely:
//   dnf4: "(2/5): package.rpm  12 MB/s | 5.2 MB  00:00"
//   dnf5: "[91/130] proj-data-ar-0.9.8.1-1.fc44.n | 2.5 MiB/s | 6.1 MiB | 00m02s"
pub(crate) fn parse_percent(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'%' {
            let mut j = i;
            while j > 0 && bytes[j - 1].is_ascii_digit() {
                j -= 1;
            }
            if j < i {
                if let Ok(n) = s[j..i].parse::<f64>() {
                    return Some((n / 100.0).clamp(0.0, 1.0));
                }
            }
        }
    }
    parse_counter(s)
}

// Scan a status line for an `(N/M)` or `[N/M]` counter token — anywhere in
// the line, not just at its start: dnf5 prefixes its progress with
// "[91/130] pkg…" and only shows the counter when piped (no "%" bar), so
// this is the only progress signal a distro-package install gives us.
fn parse_counter(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    for (i, &open) in bytes.iter().enumerate() {
        let close = match open {
            b'(' => b')',
            b'[' => b']',
            _ => continue,
        };
        // Counters are short ("[91/130]") — don't scan the whole line.
        let limit = (i + 24).min(bytes.len());
        let Some(rel) = bytes[i + 1..limit].iter().position(|&c| c == close) else {
            continue;
        };
        let inner = &s[i + 1..i + 1 + rel];
        if let Some((a, b)) = inner.split_once('/') {
            if let (Ok(n), Ok(m)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
                if n.is_finite() && m.is_finite() && m > 0.0 {
                    return Some((n / m).clamp(0.0, 1.0));
                }
            }
        }
    }
    None
}

// Strip ANSI escapes, block-progress bar glyphs, and collapse whitespace from
// flatpak's CLI output so the status text reads cleanly in the UI.
pub(crate) fn clean_status(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for ch in s.chars() {
        if ch == '\x1b' {
            in_escape = true;
            continue;
        }
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        // flatpak uses these Unicode block chars in its progress bar
        if matches!(
            ch,
            '\u{2580}' | '\u{2581}' | '\u{2582}' | '\u{2583}' | '\u{2584}'
                | '\u{2585}' | '\u{2586}' | '\u{2587}' | '\u{2588}' | '\u{2589}'
                | '\u{258a}' | '\u{258b}' | '\u{258c}' | '\u{258d}' | '\u{258e}'
                | '\u{258f}' | '\u{2590}' | '\u{2591}' | '\u{2592}' | '\u{2593}'
                | '\u{2594}' | '\u{2595}' | '\u{2596}' | '\u{2597}' | '\u{2598}'
                | '\u{2599}' | '\u{259a}' | '\u{259b}' | '\u{259c}' | '\u{259d}'
                | '\u{259e}' | '\u{259f}' | '\u{2500}' | '\u{2501}'
        ) {
            continue;
        }
        out.push(ch);
    }
    // collapse multiple spaces and strip NN% tokens (leave parse_percent to
    // read the raw line before this step, so the orb still works).
    let mut prev = ' ';
    let mut digits = String::new();
    let mut collapsed = String::with_capacity(out.len());
    for ch in out.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            digits.push(ch);
            continue;
        }
        if ch == '%' && !digits.is_empty() {
            digits.clear();
            continue;
        }
        if !digits.is_empty() {
            collapsed.push_str(&digits);
            prev = digits.chars().last().unwrap();
            digits.clear();
        }
        if ch.is_whitespace() && prev.is_whitespace() {
            continue;
        }
        collapsed.push(ch);
        prev = ch;
    }
    if !digits.is_empty() {
        collapsed.push_str(&digits);
    }
    collapsed.trim().to_string()
}

// Turn a running-operation title like "Installing Firefox" or "Removing Foo"
// into a finished-state notification message.
fn notification_text(title: &str, state: State) -> String {
    let suffix = if state == State::Failed {
        "failed"
    } else {
        "is finished"
    };
    for (prefix, noun) in [
        ("Installing ", "Installation of"),
        ("Uninstalling ", "Uninstallation of"),
        ("Removing ", "Removal of"),
        ("Updating ", "Update of"),
    ] {
        if let Some(target) = title.strip_prefix(prefix) {
            return gettext("{noun} {target} {suffix}").replace("{noun}", &noun).replace("{target}", &target).replace("{suffix}", &suffix);
        }
    }
    gettext("{title} {suffix}").replace("{title}", &title).replace("{suffix}", &suffix)
}

fn is_cancelled(id: u64) -> bool {
    registry()
        .lock()
        .unwrap()
        .iter()
        .any(|o| o.id == id && o.state == State::Cancelled)
}

fn update(id: u64, status: &str, progress: Option<f64>) {
    let mut reg = registry().lock().unwrap();
    if let Some(op) = reg.iter_mut().find(|o| o.id == id) {
        // An empty status means "progress only" — used for internal marker
        // lines, which must keep whatever the tool last actually said.
        if !status.is_empty() {
            op.status = status.to_string();
        }
        // Progress only ever moves forward. A tool restarting its counter on
        // a phase change (dnf download → transaction) must not yank the orb
        // backwards mid-install.
        if let Some(p) = progress.filter(|p| p.is_finite()) {
            if op.progress.map_or(true, |cur| p > cur) {
                op.progress = Some(p);
            }
        }
    }
}

pub fn dismiss_item(op_id: Option<u64>, hist_id: Option<u64>, dir: f64) {
    let mut changed = false;
    let deadline = Instant::now() + DISMISS_GRACE;
    if let Some(id) = op_id {
        let mut reg = registry().lock().unwrap();
        if let Some(op) = reg
            .iter_mut()
            .find(|o| o.id == id && o.state == State::Running)
        {
            op.dismissed_dir = Some(dir);
            op.pending_commit_at = Some(deadline);
            changed = true;
        }
    } else if let Some(id) = hist_id {
        let mut h = history().lock().unwrap();
        if let Some(entry) = h.iter_mut().find(|e| e.id == id) {
            entry.dismissed_dir = Some(dir);
            entry.pending_commit_at = Some(deadline);
            changed = true;
        }
    }
    if changed {
        ensure_pending_sweeper();
        nudge_ui();
    }
}

pub fn restore_item(op_id: Option<u64>, hist_id: Option<u64>) -> bool {
    let mut restored = false;
    if let Some(id) = op_id {
        let mut reg = registry().lock().unwrap();
        if let Some(op) = reg
            .iter_mut()
            .find(|o| o.id == id && o.dismissed_dir.is_some())
        {
            op.dismissed_dir = None;
            op.pending_commit_at = None;
            restored = true;
        }
    } else if let Some(id) = hist_id {
        let mut h = history().lock().unwrap();
        if let Some(entry) = h
            .iter_mut()
            .find(|e| e.id == id && e.dismissed_dir.is_some())
        {
            entry.dismissed_dir = None;
            entry.pending_commit_at = None;
            restored = true;
        }
    }
    if restored {
        nudge_ui();
    }
    restored
}

/// Key of the most recently dismissed item that is still inside its undo grace
/// window: `(op_id, hist_id)`. Once the grace lapses the sweeper commits the
/// dismissal and there is nothing left to bring back.
pub fn latest_dismissed() -> Option<(Option<u64>, Option<u64>)> {
    // The most recent dismissal is the one with the latest commit deadline.
    let reg_latest = {
        let reg = registry().lock().unwrap();
        reg.iter()
            .filter(|o| o.dismissed_dir.is_some() && o.pending_commit_at.is_some())
            .max_by_key(|o| o.pending_commit_at)
            .map(|o| (o.id, o.pending_commit_at))
    };
    let hist_latest = {
        let h = history().lock().unwrap();
        h.iter()
            .filter(|e| e.dismissed_dir.is_some() && e.pending_commit_at.is_some())
            .max_by_key(|e| e.pending_commit_at)
            .map(|e| (e.id, e.pending_commit_at))
    };
    match (reg_latest, hist_latest) {
        (Some((a, ta)), Some((b, tb))) => {
            if ta >= tb {
                Some((Some(a), None))
            } else {
                Some((None, Some(b)))
            }
        }
        (Some((a, _)), None) => Some((Some(a), None)),
        (None, Some((b, _))) => Some((None, Some(b))),
        (None, None) => None,
    }
}

pub fn commit_item(op_id: Option<u64>, hist_id: Option<u64>) {
    if let Some(id) = op_id {
        cancel_silently(id);
    } else if let Some(id) = hist_id {
        remove_history(id);
    }
}

fn cancel_silently(id: u64) {
    let pid = {
        let mut reg = registry().lock().unwrap();
        match reg.iter_mut().find(|o| o.id == id) {
            Some(op) if op.state == State::Running || op.state == State::Cancelled => {
                op.state = State::Cancelled;
                op.status = "Cancelled".into();
                op.dismissed_dir = None;
                op.pending_commit_at = None;
                op.pid
            }
            _ => return,
        }
    };
    if let Some(pid) = pid {
        std::thread::spawn(move || {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output();
        });
    }
    nudge_ui();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        let mut reg = registry().lock().unwrap();
        reg.retain(|o| o.id != id || o.state != State::Cancelled);
        drop(reg);
        nudge_ui();
    });
}

/// Cancel a running operation: kill the spawned process and mark it cancelled.
/// If the process hasn't been spawned yet, mark it so `start`'s thread kills
/// it as soon as it appears.
pub fn cancel(id: u64) {
    let (pid, hist) = {
        let mut reg = registry().lock().unwrap();
        match reg.iter_mut().find(|o| o.id == id) {
            Some(op) if op.state == State::Running => {
                op.state = State::Cancelled;
                op.status = "Cancelled".into();
                (
                    op.pid,
                    Some((
                        op.title.clone(),
                        op.source.clone(),
                        op.icon.clone(),
                        op.dismissed_dir,
                        op.pending_commit_at,
                    )),
                )
            }
            _ => return,
        }
    };
    if let Some((title, source, icon, dismissed_dir, pending_commit_at)) = hist {
        push_history(
            title,
            source,
            icon,
            State::Cancelled,
            dismissed_dir,
            pending_commit_at,
        );
    }
    if let Some(pid) = pid {
        // Kill off the main thread: spawning + reaping a process synchronously in
        // a UI callback can stall the window.
        std::thread::spawn(move || {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output();
        });
    }
    nudge_ui();
    post_op_refresh();
    std::thread::spawn(move || {
        std::thread::sleep(CANCEL_GRACE);
        // If the user restarted it in the meantime, leave it running.
        let mut reg = registry().lock().unwrap();
        reg.retain(|o| o.id != id || o.state != State::Cancelled);
        drop(reg);
        nudge_ui();
    });
}

fn finish(id: u64, state: State) {
    let mut notify_title: Option<String> = None;
    let mut hist: Option<(String, String, String, Option<f64>, Option<Instant>)> = None;
    let mut was_update = false;
    let mut armed_update = false;
    {
        let mut reg = registry().lock().unwrap();
        if let Some(op) = reg.iter_mut().find(|o| o.id == id) {
            if op.state == State::Cancelled {
                return;
            }
            op.state = state;
            let done = matches!(state, State::Done);
            was_update = done && crate::search::cmd::is_update_op(&op.args);
            // Arming an already-downloaded update changes what the reboot row
            // has to say ("this restart installs them"), so it needs the same
            // re-probe an update run gets — without pretending to be one.
            armed_update = done
                && (crate::dnf5daemon::is_schedule_task(&op.args)
                    || crate::packagekit::is_schedule_task(&op.args));
            op.progress = Some(1.0);
            op.status = match state {
                State::Done => "Completed".into(),
                State::Failed => "Failed".into(),
                State::Cancelled => "Cancelled".into(),
                State::Running => op.status.clone(),
            };
            if matches!(state, State::Done | State::Failed) {
                notify_title = Some(notification_text(&op.title, state));
                hist = Some((
                    op.title.clone(),
                    op.source.clone(),
                    op.icon.clone(),
                    op.dismissed_dir,
                    op.pending_commit_at,
                ));
            }
        }
    }
    if let Some((title, source, icon, dismissed_dir, pending_commit_at)) = hist {
        push_history(title, source, icon, state, dismissed_dir, pending_commit_at);
    }
    if let Some(text) = notify_title {
        glib::MainContext::default().invoke(move || {
            if crate::app::is_search_window_hidden() {
                crate::app::send_desktop_notification("Spotty", &text);
            }
        });
    }
    // After a system update, re-evaluate whether a reboot is pending
    // (off-thread; the live state clears itself after the user reboots).
    if was_update || armed_update {
        crate::search::cmd::refresh_reboot_state();
    }
    if was_update {
        // The pending list is stale now — drop it and re-check, so the
        // rows/badge go away once the run really applied everything.
        crate::search::cmd::refresh_updates_after_run();
    }
    nudge_ui();
    post_op_refresh();
    // Linger briefly so the completed/failed state is visible, then drop it.
    std::thread::spawn(move || {
        std::thread::sleep(DONE_GRACE);
        registry().lock().unwrap().retain(|o| o.id != id);
        nudge_ui();
    });
}

/// Newest running operation's (title, fraction), if any.
pub fn active_op_progress() -> Option<(String, Option<f64>)> {
    let reg = registry().lock().unwrap();
    reg.iter()
        .rev()
        .find(|o| o.state == State::Running)
        .map(|o| (o.title.clone(), o.progress))
}

/// Newest running operation's (title, "source · status") — the inline orb
/// pill in the search bar shows this when clicked open.
pub fn active_op_detail() -> Option<(String, String)> {
    let reg = registry().lock().unwrap();
    reg.iter()
        .rev()
        .find(|o| o.state == State::Running)
        .map(|o| {
            let detail = match (o.source.is_empty(), o.status.is_empty()) {
                (false, false) => gettext("{source} · {status}").replace("{source}", &o.source).replace("{status}", &o.status),
                (true, false) => o.status.clone(),
                (false, true) => o.source.clone(),
                (true, true) => String::new(),
            };
            (o.title.clone(), detail)
        })
}

/// Newest operation still inside its completion linger (finished, not
/// cancelled — `finish()` drops it after `DONE_GRACE`), as `(title, failed)`.
/// Lets the search-bar orb flash a full completed ring in the same window the
/// popover shows the finished row, instead of the orb just vanishing.
pub fn just_finished_op() -> Option<(String, bool)> {
    let reg = registry().lock().unwrap();
    reg.iter()
        .rev()
        .find(|o| matches!(o.state, State::Done | State::Failed))
        .map(|o| (o.title.clone(), o.state == State::Failed))
}

/// Live (subtitle, indeterminate) for a still-running op, keyed by id.
/// Returns `None` once the op is gone or no longer running.
pub fn op_row_update(id: u64) -> Option<(String, bool)> {
    let reg = registry().lock().unwrap();
    let op = reg.iter().find(|o| o.id == id && o.state == State::Running)?;
    Some((gettext("{source} · {status}").replace("{source}", &op.source).replace("{status}", &op.status), op.progress.is_none()))
}

/// One live progress result row per operation, newest first. Empty when nothing
/// is active, so indicators vanish on completion.
pub fn running_result_rows() -> Vec<SearchResult> {
    let reg = registry().lock().unwrap();
    let mut score: i32 = 80_000;
    reg.iter()
        .rev()
        .map(|op| {
            let state_str = match op.state {
                State::Running => "running",
                State::Done => "done",
                State::Failed => "failed",
                State::Cancelled => "cancelled",
            };
            let frac_str = match op.state {
                State::Running => op.progress.map(|f| format!("{:.4}", f)).unwrap_or_default(),
                _ => "1".into(),
            };
            let status_text = match op.state {
                State::Done => format!("{} · Completed ✓", op.source),
                State::Failed => format!("{} · Failed", op.source),
                State::Cancelled => format!("{} · Cancelled", op.source),
                State::Running => gettext("{source} · {status}").replace("{source}", &op.source).replace("{status}", &op.status),
            };
            // The action string is a sentinel carrying render hints:
            //   "__op__\x1f<fraction>\x1f<state>\x1f<icon>\x1f<id>"
            let action = Action::EnterMode(format!(
                "__op__\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                frac_str, state_str, op.icon, op.id
            ));
            let row = SearchResult {
                kind: ResultKind::System,
                title: op.title.clone(),
                subtitle: Some(status_text),
                icon: Some("op-progress".into()),
                action,
                score,
            };
            score -= 1;
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── end-to-end wiring ────────────────────────────────────────────────────

    /// Drive the real `run_process` pipeline (reader threads → phase tracker →
    /// 120 ms-throttled registry flush) with a chained-update-shaped script:
    /// a part marker, dnf download bars, then transaction bars. The internal
    /// marker must never reach the visible status, and the registry must show
    /// live intermediate fractions — not an instant 100% from a metadata line.
    #[test]
    fn run_process_tracks_phases_and_hides_part_markers() {
        let title = "__op_progress_test__".to_string();
        let script = concat!(
            "echo __spotty_part_1_2_dnf__ && ",
            "echo '[1/4] pkg-a-1.0-1.fc44.x86_64   100% | 1.0 MiB/s | 1.0 MiB | 00m01s' && ",
            "sleep 0.4 && ",
            "echo '[2/4] pkg-b-1.0-1.fc44.x86_64   100% | 1.0 MiB/s | 1.0 MiB | 00m01s' && ",
            "sleep 0.4 && ",
            "echo 'Running transaction' && ",
            "echo '[1/1] Installing pkg-a-1.0-1.fc44 100% | 1.0 MiB/s | 1.0 MiB | 00m01s' && ",
            "i=0; while [ $i -lt 15 ]; do echo tick; sleep 0.2; i=$((i+1)); done",
        );
        let args: Vec<String> = ["sh", "-c", script].iter().map(|s| s.to_string()).collect();

        let id = next_id();
        registry().lock().unwrap().push(Operation {
            id,
            title: title.clone(),
            source: "Test".into(),
            icon: "test".into(),
            status: "Starting…".into(),
            progress: None,
            state: State::Running,
            pid: None,
            args: args.clone(),
            dismissed_dir: None,
            pending_commit_at: None,
        });
        run_process(id, args);

        // Record registry snapshots; cancel the fake op as soon as a live
        // intermediate fraction shows up (or after 1.5 s, always before the
        // script can finish). Cancelling directly in the registry avoids the
        // history/notification side effects of `cancel()`.
        let started = Instant::now();
        let mut seen: Vec<(String, Option<f64>)> = Vec::new();
        let mut did_cancel = false;
        while started.elapsed() < Duration::from_secs(4) {
            let snap = {
                let reg = registry().lock().unwrap();
                reg.iter()
                    .find(|o| o.id == id)
                    .map(|o| (o.status.clone(), o.progress))
            };
            let Some(cur) = snap else { break };
            let mid = cur.1.is_some_and(|p| p > 0.01 && p < 0.99);
            if seen.last() != Some(&cur) {
                seen.push(cur);
            }
            if mid || started.elapsed() > Duration::from_millis(1500) {
                if let Some(op) = registry().lock().unwrap().iter_mut().find(|o| o.id == id) {
                    op.state = State::Cancelled;
                }
                did_cancel = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Give the reader loop time to notice the cancel and kill the child,
        // then drop the fake op. Any history entry with our title (only
        // possible if the script died early) goes too — history is in-memory.
        std::thread::sleep(Duration::from_millis(700));
        registry().lock().unwrap().retain(|o| o.id != id);
        history().lock().unwrap().retain(|e| e.title != title);

        assert!(did_cancel, "never observed a cancel point: {seen:?}");
        assert!(!seen.is_empty(), "no registry snapshots recorded");
        assert!(
            seen.iter().all(|(s, _)| !s.contains("spotty_part")),
            "part marker leaked into status: {seen:?}"
        );
        assert!(
            seen.iter().any(|(_, p)| p.is_some_and(|p| p > 0.01 && p < 0.99)),
            "no intermediate fraction recorded (metadata 100% regression?): {seen:?}"
        );
        let mut last = 0.0f64;
        for (_, p) in &seen {
            if let Some(v) = p {
                assert!(*v >= last - 1e-9, "progress went backwards: {seen:?}");
                last = *v;
            }
        }
    }

    #[test]
    fn parse_percent_various() {
        assert_eq!(parse_percent("45% done"), Some(0.45));
        assert_eq!(parse_percent("  0%  "), Some(0.0));
        assert_eq!(parse_percent("Installing ████░░ 50%"), Some(0.50));
        assert_eq!(parse_percent("100%"), Some(1.0));
        assert_eq!(parse_percent("no number here"), None);
        assert_eq!(parse_percent("(3/7): foo.rpm"), Some(3.0 / 7.0));
        assert_eq!(parse_percent("(0/5): bar.rpm"), Some(0.0));
        assert_eq!(parse_percent("done!"), None);
    }

    #[test]
    fn parse_percent_dnf5_bracket_counter() {
        // dnf5's piped download progress: no "%" bar, counter mid-line.
        assert_eq!(
            parse_percent(
                "[91/130] proj-data-ar-0.9.8.1-1.fc44.n | 2.5 MiB/s | 6.1 MiB | 00m02s"
            ),
            Some(91.0 / 130.0)
        );
        assert_eq!(parse_percent("[ 3/ 7] foo.rpm"), Some(3.0 / 7.0));
        assert_eq!(parse_percent("progress (2/5): bar.rpm"), Some(2.0 / 5.0));
        // Not a counter: no digits around the slash / unmatched bracket.
        assert_eq!(parse_percent("[ok] package.rpm"), None);
        assert_eq!(parse_percent("(12/5/2025) log"), None);
        assert_eq!(parse_percent("100/100 no brackets"), None);
    }

    #[test]
    fn clean_status_strips_ansi() {
        let raw = "\x1b[1mInstalling\x1b[0m firefox";
        assert_eq!(clean_status(raw), "Installing firefox");
    }

    #[test]
    fn clean_status_strips_block_bar() {
        let raw = "Installing… ████████░░░░ 45%  1.2 MB/s";
        assert_eq!(clean_status(raw), "Installing… 1.2 MB/s");
    }

    #[test]
    fn clean_status_collapses_whitespace() {
        let raw = "  hello   world  ";
        assert_eq!(clean_status(raw), "hello world");
    }

    #[test]
    fn clean_status_plain_text_unchanged() {
        assert_eq!(clean_status("hello"), "hello");
        assert_eq!(clean_status(""), "");
    }

    #[test]
    fn clean_status_strips_percent_tokens() {
        assert_eq!(clean_status("45% done"), "done");
        assert_eq!(clean_status("Installing… 100% complete"), "Installing… complete");
        assert_eq!(clean_status("3.2% I/O"), "I/O");
        assert_eq!(clean_status("no percent here"), "no percent here");
    }
}
