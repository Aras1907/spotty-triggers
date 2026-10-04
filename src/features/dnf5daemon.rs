//! Updates through **dnf5daemon**, the system service the Software store uses.
//!
//! Everything here is one D-Bus conversation with `org.rpm.dnf.v0`, a root
//! service started on demand. Why that and not `pkexec dnf upgrade`:
//!
//! * **No password.** Arming an update checks the polkit action
//!   `org.rpm.dnf.v0.rpm.execute_trusted_transaction`, and Fedora ships
//!   `/usr/share/polkit-1/rules.d/org.rpm.dnf.v0.rules` — for an active local
//!   session on a `wheel` user the answer is `YES`. `pkexec` instead goes
//!   through `org.freedesktop.policykit.exec`, which has no such rule, so it
//!   always asks. That asymmetry is why the store needs no sudo and a shell
//!   `dnf upgrade` does.
//! * **It actually applies.** The daemon writes an offline transaction
//!   (packages downloaded, dependency-solved, rpmdb-tested), creates the
//!   `/system-update` symlink and sets the state to `ready`; the next boot
//!   installs it. `store_transaction_offline()` in dnf5daemon's `session.cpp`
//!   is that code, and we are calling the same door it calls.
//!
//! The session object is bound to the D-Bus *connection* that opened it, so
//! the whole sequence has to share one connection ([`Session`] does).
//!
//! Every entry point here runs off the GTK main thread — the transaction call
//! downloads packages and blocks for as long as that takes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glib::variant::DictEntry;
use glib::Variant;

use crate::operations::TaskHandle;

const DEST: &str = "org.rpm.dnf.v0";
const ROOT_PATH: &str = "/org/rpm/dnf/v0";
const SESSION_MANAGER: &str = "org.rpm.dnf.v0.SessionManager";
const OFFLINE: &str = "org.rpm.dnf.v0.Offline";
const GOAL: &str = "org.rpm.dnf.v0.Goal";
const RPM: &str = "org.rpm.dnf.v0.rpm.Rpm";

/// Plain calls (a session, a status read) are quick; give them seconds, not
/// minutes, so an unresponsive daemon can't wedge an operation.
const CALL_TIMEOUT_MS: i32 = 30_000;

/// Downloading + testing a transaction is a long call: a big update over a
/// slow link is easily tens of minutes.
const TRANSACTION_TIMEOUT_MS: i32 = 60 * 60 * 1000;

/// How long an [`available`] verdict is reused. Short enough that a daemon
/// which appears (or dies) after startup is picked up, long enough that
/// building the update rows on every keystroke stays free.
const PROBE_TTL: Duration = Duration::from_secs(30);

/// Argv[0] that means "Spotty runs this itself instead of spawning a program".
///
/// The update rows, the history, restart-from-cancelled and the update-run
/// detection all speak argv, so an in-process update has to enter the
/// operations registry like any other command. This name is the marker;
/// [`crate::operations`] routes it to [`run_task`] rather than to `exec`.
pub const ARGV0: &str = "spotty-dnf5daemon";

/// Task verbs, as `ARGV0`'s first argument.
pub const VERB_UPGRADE_ALL: &str = "upgrade-all";
pub const VERB_UPGRADE_PKG: &str = "upgrade-pkg";
pub const VERB_SCHEDULE: &str = "schedule";
pub const VERB_ALL: &str = "all";

/// What a pending offline transaction is waiting for. Read straight from the
/// daemon, so it is accurate where a file-based probe would have to guess.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OfflineStatus {
    /// Nothing pending.
    None,
    /// Downloading, or a previous download failed — the transaction is not
    /// usable yet, so the update has to be run again.
    Downloading,
    /// Everything is downloaded and tested, waiting to be armed for the next
    /// boot. A restart alone would *not* install this one.
    Downloaded,
    /// Armed: `/system-update` exists, so the next boot installs it.
    Armed,
    /// Some other state (e.g. `transaction-incomplete`) — kept as-is so the
    /// UI can be honest about it.
    Other(String),
}

impl OfflineStatus {
    /// Map what `get_status` answered: `armed` is the daemon's own "this will
    /// be installed by the next boot" flag, `status` its state string.
    pub fn from_parts(armed: bool, status: &str) -> OfflineStatus {
        if armed || status == "ready" {
            return OfflineStatus::Armed;
        }
        match status {
            "" => OfflineStatus::None,
            "download-incomplete" => OfflineStatus::Downloading,
            "download-complete" => OfflineStatus::Downloaded,
            other => OfflineStatus::Other(other.to_string()),
        }
    }
}

/// One conversation with the daemon: a system-bus connection plus the session
/// object opened on it.
pub struct Session {
    conn: gio::DBusConnection,
    path: String,
    /// Released on drop so a panic can't leave a session behind.
    closer: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Open a session, which is what makes the rest of the API — including
    /// [`OfflineStatus`] — reachable: the services live on the session object.
    pub fn open() -> Result<Session, String> {
        let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
            .map_err(|e| format!("system bus: {e}"))?;
        let reply = dbus_call(
            &conn,
            ROOT_PATH,
            SESSION_MANAGER,
            "open_session",
            Some(options_arg([])),
            CALL_TIMEOUT_MS,
        )?;
        let path = reply
            .child_value(0)
            .get::<String>()
            .ok_or_else(|| "open_session: no session path".to_string())?;
        let closer = {
            let conn = conn.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                let _ = dbus_call(
                    &conn,
                    ROOT_PATH,
                    SESSION_MANAGER,
                    "close_session",
                    Some(Variant::tuple_from_iter([Variant::from(path.as_str())])),
                    CALL_TIMEOUT_MS,
                );
            })
        };
        Ok(Session {
            conn,
            path,
            closer: Some(closer),
        })
    }

    fn call(
        &self,
        iface: &str,
        method: &str,
        params: Option<Variant>,
        timeout: i32,
    ) -> Result<Variant, String> {
        dbus_call(&self.conn, &self.path, iface, method, params, timeout)
    }

    /// `Offline.get_status`: `(pending, {status, cachedir, verb, …})`.
    ///
    /// The `pending` flag is only true once the transaction is *armed* — the
    /// daemon checks the `/system-update` symlink and the rpmdb cookie — so it
    /// is the difference between "a restart installs this" and "this is only
    /// downloaded".
    pub fn offline_status(&self) -> Result<OfflineStatus, String> {
        let reply = self.call(OFFLINE, "get_status", None, CALL_TIMEOUT_MS)?;
        let armed = reply.child_value(0).get::<bool>().unwrap_or(false);
        let status = dict_string(&reply.child_value(1), "status").unwrap_or_default();
        Ok(OfflineStatus::from_parts(armed, &status))
    }

    /// Arm the downloaded transaction for the next boot: the store's own
    /// "restart & update" call. The reboot afterwards is a plain reboot.
    pub fn schedule(&self) -> Result<(), String> {
        let reply = self.call(
            OFFLINE,
            "schedule_for_next_boot",
            Some(options_arg([])),
            CALL_TIMEOUT_MS,
        )?;
        // (success, error_msg) — the daemon answers over polkit, so a refusal
        // carries its own wording instead of us inventing one.
        if reply.child_value(0).get::<bool>().unwrap_or(false) {
            Ok(())
        } else {
            Err(message_of(&reply, 1))
        }
    }

    /// Download, test and arm the update, the way the store does it.
    ///
    /// `specs` empty means every available upgrade (the daemon's own
    /// `add_rpm_upgrade()`), which is what `dnf upgrade` means; a single
    /// package passes just that spec. Nothing is installed here — the reboot
    /// does that, so an interrupted download can't leave a half-updated
    /// system.
    fn upgrade_offline(&self, specs: &[String], task: &TaskHandle) -> Result<(), String> {
        if task.cancelled() {
            return Err("Cancelled".into());
        }
        self.call(
            RPM,
            "upgrade",
            Some(Variant::tuple_from_iter([
                Variant::array_from_iter::<String>(specs.iter().map(|s| Variant::from(s.as_str()))),
                dict([]),
            ])),
            TRANSACTION_TIMEOUT_MS,
        )
        .map_err(|e| format!("Upgrade request failed: {e}"))?;

        // resolve() answers with the transaction *and* a result code; a
        // non-zero one means nothing is installable, and the reason is a
        // separate call rather than an error on this one.
        task.status("Resolving dependencies");
        let resolved = self.call(GOAL, "resolve", Some(options_arg([])), TRANSACTION_TIMEOUT_MS)?;
        let result = resolved.child_value(1).get::<u32>().unwrap_or(0);
        if result != 0 {
            let problems = self
                .call(GOAL, "get_transaction_problems_string", None, CALL_TIMEOUT_MS)
                .map(|r| problems_of(&r))
                .unwrap_or_default();
            let problems = problems.trim().to_string();
            return Err(if problems.is_empty() {
                format!("Nothing to install ({result})")
            } else {
                problems
            });
        }
        // Nothing resolved means nothing to download: say so instead of
        // spending minutes on a transaction with no packages in it.
        let count = resolved.child_value(0).n_children();
        if count == 0 {
            task.status("Nothing to install");
            return Ok(());
        }

        task.status(&format!("Downloading {count} updates"));
        // `offline: true` is the whole point: the daemon stores the
        // transaction instead of running rpm, then arms /system-update for the
        // next boot. Nothing here reboots.
        let watch = CancelWatch::start(self, *task);
        let outcome = self.call(
            GOAL,
            "do_transaction",
            Some(options_arg([DictEntry::new("offline", Variant::from(true))])),
            TRANSACTION_TIMEOUT_MS,
        );
        watch.stop();
        outcome.map_err(|e| format!("Update failed: {e}"))?;

        task.status("Update ready for restart");
        match self.offline_status()? {
            OfflineStatus::Armed => Ok(()),
            other => Err(format!(
                "Update stored but not armed for the next boot ({other:?})"
            )),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(handle) = self.closer.take() {
            let _ = handle.join();
        }
    }
}

/// Stops a running transaction when the user cancels the operation.
///
/// The blocking `do_transaction` call can't be interrupted from its own
/// thread, but the daemon can be told to — from a second thread on the same
/// connection, against the same session object.
struct CancelWatch {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CancelWatch {
    fn start(session: &Session, task: TaskHandle) -> CancelWatch {
        // Only armed for the long call: earlier steps are quick enough that a
        // second thread would cost more than it saves.
        let conn = session.conn.clone();
        let path = session.path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(300));
                if task.cancelled() {
                    let _ = dbus_call(&conn, &path, GOAL, "cancel", None, CALL_TIMEOUT_MS);
                    return;
                }
            }
        });
        CancelWatch {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One synchronous D-Bus call on the system bus.
fn dbus_call(
    conn: &gio::DBusConnection,
    path: &str,
    iface: &str,
    method: &str,
    params: Option<Variant>,
    timeout: i32,
) -> Result<Variant, String> {
    conn.call_sync(
        Some(DEST),
        path,
        iface,
        method,
        params.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        timeout,
        gio::Cancellable::NONE,
    )
    .map_err(|e| describe(&e))
}

/// Turn a GDBus failure into wording worth showing: the daemon's own message
/// (transaction problems, polkit refusals) instead of GDBus' generic one.
fn describe(error: &glib::Error) -> String {
    let message = error.message().to_string();
    if message.contains("Not authorized") {
        return "The system refused the update (authorisation required)".into();
    }
    message
}

/// Argv[0] of an in-process "update every available package" task.
pub fn upgrade_all_args() -> Vec<String> {
    vec![ARGV0.into(), VERB_UPGRADE_ALL.into()]
}

/// Argv for arming an already-downloaded transaction.
pub fn schedule_args() -> Vec<String> {
    vec![ARGV0.into(), VERB_SCHEDULE.into()]
}

/// Argv for "update all sources": the daemon upgrade first, then a shell
/// script (flatpak, pacman, snap, AppImage — all still child processes).
pub fn all_args(script: &str) -> Vec<String> {
    vec![ARGV0.into(), VERB_ALL.into(), script.to_string()]
}

/// True when the daemon is reachable and speaks the API we need.
///
/// Probed by opening a session, which is both the cheapest real check and the
/// only one that activates the service if it isn't running yet.
pub fn available() -> bool {
    static PROBE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut guard = PROBE.lock().unwrap();
    if let Some((at, verdict)) = *guard {
        if at.elapsed() < PROBE_TTL {
            return verdict;
        }
    }
    let verdict = Session::open().is_ok();
    *guard = Some((Instant::now(), verdict));
    verdict
}

/// True when the daemon holds an update that the next restart installs.
///
/// Those packages are queued, not installed: `dnf check-update` keeps listing
/// them until the restart, so the update check asks here and leaves them out —
/// otherwise the list, the badge and the "Update all" rows keep offering what
/// is already on its way. Read from the daemon itself (no host command, so it
/// works from the sandbox) and never from a timing-dependent file probe.
pub fn offline_armed() -> bool {
    available()
        && Session::open()
            .and_then(|s| s.offline_status())
            .map(|st| st == OfflineStatus::Armed)
            .unwrap_or(false)
}

/// Fill the [`available`] cache from a background thread, at startup.
///
/// The update rows are built while the user types, so the first one must not be
/// the thing that pays for a D-Bus round trip — and a cold daemon (D-Bus
/// activation) makes that round trip the slowest one of all.
pub fn warmup() {
    std::thread::spawn(|| {
        let _ = available();
    });
}

/// True when this argv is an in-process update that actually installs
/// something — the verbs that download and arm a transaction.
///
/// `schedule` is deliberately not one of them: it only marks an
/// already-downloaded transaction for the next boot, so it must not count as a
/// run that needs a reboot probe of its own.
pub fn is_update_task(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some(ARGV0)
        && matches!(
            args.get(1).map(String::as_str),
            Some(VERB_UPGRADE_ALL | VERB_UPGRADE_PKG | VERB_ALL)
        )
}

/// True when this argv only arms what is already downloaded.
pub fn is_schedule_task(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some(ARGV0)
        && args.get(1).map(String::as_str) == Some(VERB_SCHEDULE)
}

/// Run an in-process update. Called by [`crate::operations`] for every
/// operation whose argv starts with [`ARGV0`].
///
/// The `Err(String)` is user-facing wording — the daemon's, or polkit's — so
/// the failure surfaces in the row instead of only in the log.
pub fn run_task(args: &[String], task: &TaskHandle) -> Result<(), String> {
    let verb = args.get(1).map(String::as_str).unwrap_or("");
    let session = Session::open()?;
    match verb {
        VERB_UPGRADE_ALL => session.upgrade_offline(&[], task),
        VERB_UPGRADE_PKG => {
            let spec = args.get(2).cloned().unwrap_or_default();
            session.upgrade_offline(&[spec], task)
        }
        VERB_SCHEDULE => {
            task.status("Scheduling updates for the next restart");
            session.schedule()
        }
        // The daemon does the distro half, then a shell script runs the rest:
        // flatpak/pacman/snap/AppImage are all still separate programs. Both
        // halves are attempted, and a failure in either is reported.
        VERB_ALL => {
            let script = args.get(2);
            let distro = session.upgrade_offline(&[], task);
            let others = script.map(|s| run_script(s, task));
            distro?;
            match others {
                Some(false) => Err("Some of the updates did not install".into()),
                _ => Ok(()),
            }
        }
        other => Err(format!("Unknown dnf5daemon task \"{other}\"")),
    }
}

/// Run a shell script, forwarding its output as the operation status. Returns
/// whether it succeeded.
pub(crate) fn run_script(script: &str, task: &TaskHandle) -> bool {
    let mut cmd = if crate::app::is_flatpak() {
        let mut c = std::process::Command::new("flatpak-spawn");
        c.args(["--host", "sh", "-c", script]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", script]);
        c
    };
    let mut child = match cmd.stdout(std::process::Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            log::info!("update script: {e}");
            return false;
        }
    };
    if let Some(out) = child.stdout.take() {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            // Chain markers are progress bookkeeping, not status.
            if line.trim().is_empty() || crate::opprogress::is_part_marker(&line) {
                continue;
            }
            let line = crate::operations::clean_status(line.trim());
            if !line.is_empty() {
                task.status(&line);
            }
        }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// The `a{sv}` options every one of these methods takes, as GDBus wants them:
/// arguments are always a *tuple*, so the single options map is `(a{sv})`.
/// Passing the bare `a{sv}` is rejected by `g_dbus_connection_call_sync`.
fn options_arg(entries: impl IntoIterator<Item = DictEntry<&'static str, Variant>>) -> Variant {
    Variant::tuple_from_iter([dict(entries)])
}

/// An `a{sv}` from dict entries. The element type is stated explicitly, so an
/// empty one is still `a{sv}` and not an untyped array.
fn dict(entries: impl IntoIterator<Item = DictEntry<&'static str, Variant>>) -> Variant {
    // The value has to be boxed as a variant: with plain values the array comes
    // out as `a{sb}`, and the daemon — which asks for `a{sv}` — rejects that.
    // Collecting entries (rather than building a typed array) is also what makes
    // the empty map `a{sv}` instead of an untyped array.
    entries
        .into_iter()
        .map(|entry| DictEntry::new(*entry.key(), Variant::from_variant(entry.value())))
        .collect()
}

/// Read a string out of an `a{sv}` reply. Its children are `(sv)` entries.
fn dict_string(dict: &Variant, key: &str) -> Option<String> {
    dict.iter().find_map(|entry| {
        let name = entry.child_value(0).get::<String>()?;
        (name == key)
            .then(|| entry.child_value(1).get::<String>())
            .flatten()
    })
}

/// The daemon's own message from a `(success, error_msg)` reply.
fn message_of(reply: &Variant, index: usize) -> String {
    let message = reply.child_value(index).get::<String>().unwrap_or_default();
    if message.trim().is_empty() {
        "The system declined the request".into()
    } else {
        message
    }
}

/// The reasons a resolved transaction can't be installed. The daemon answers
/// with a *list* of them, so they are joined into one readable line.
fn problems_of(reply: &Variant) -> String {
    reply
        .child_value(0)
        .iter()
        .filter_map(|p| p.get::<String>())
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_read_from_the_daemons_own_wording() {
        // get_status answers (armed, {status: …}). The armed flag is what the
        // daemon reports once /system-update exists; the status string covers
        // the downloaded-but-unarmed case, where a restart would install
        // nothing.
        use OfflineStatus::*;
        assert_eq!(OfflineStatus::from_parts(true, "ready"), Armed);
        assert_eq!(OfflineStatus::from_parts(false, "ready"), Armed);
        assert_eq!(
            OfflineStatus::from_parts(false, "download-complete"),
            Downloaded
        );
        assert_eq!(
            OfflineStatus::from_parts(false, "download-incomplete"),
            Downloading
        );
        assert_eq!(
            OfflineStatus::from_parts(false, "transaction-incomplete"),
            Other("transaction-incomplete".into())
        );
        // An empty dict must not read as armed — nothing is waiting.
        assert_eq!(OfflineStatus::from_parts(false, ""), None);
    }

    #[test]
    fn option_dicts_have_the_type_the_daemon_expects() {
        // sdbus-c++ is strict about the signature: every one of these methods
        // takes `a{sv}`, so an `a{ss}` or a `s` here fails the call outright.
        // An empty map must still be a{sv} — an untyped empty array is
        // rejected by the daemon's marshaller.
        assert_eq!(dict([]).type_().as_str(), "a{sv}");
        assert_eq!(
            dict([DictEntry::new("offline", Variant::from(true))])
                .type_()
                .as_str(),
            "a{sv}"
        );
        // …and it arrives as the (a{sv}) tuple GDBus insists on.
        assert_eq!(options_arg([]).type_().as_str(), "(a{sv})");
        assert_eq!(
            Variant::array_from_iter::<String>(["vim.x86_64"].map(Variant::from))
                .type_()
                .as_str(),
            "as"
        );
    }

    #[test]
    fn task_argv_is_recognisable_as_an_update_run() {
        // The operations registry keys restart, undo and history off argv, so
        // these have to look like update runs.
        let all = upgrade_all_args();
        assert_eq!(all[0], ARGV0);
        assert!(crate::search::cmd::is_update_op(&all), "{all:?}");
        // Arming on its own installs nothing, so it isn't an update run — it
        // must not trigger a second reboot probe as if it were.
        let scheduled = schedule_args();
        assert!(
            !crate::search::cmd::is_update_op(&scheduled),
            "arming alone is not an update run: {scheduled:?}"
        );
    }

    /// Talks to the *system* dnf5daemon. Read-only: opens a session, reads the
    /// offline status, closes it — no transaction is created or touched.
    #[test]
    #[ignore = "needs a running dnf5daemon (dnf5 systems)"]
    fn the_system_daemon_answers() {
        let status = Session::open().and_then(|s| s.offline_status());
        println!("offline status: {status:?}");
        assert!(status.is_ok(), "{status:?}");
    }

    /// Reads the daemon's own view of the pending upgrade — goal + resolve
    /// only. No download, no transaction, nothing armed: the goal lives in the
    /// session and dies with it. This is the part that proves the tuple shapes
    /// we send are the ones sdbus-c++ accepts.
    #[test]
    #[ignore = "needs a running dnf5daemon and network metadata"]
    fn the_system_daemon_resolves_the_upgrade() {
        let session = Session::open().expect("open session");
        session
            .call(
                RPM,
                "upgrade",
                Some(Variant::tuple_from_iter([
                    Variant::array_from_iter::<String>(std::iter::empty()),
                    dict([]),
                ])),
                TRANSACTION_TIMEOUT_MS,
            )
            .expect("Rpm.upgrade");
        let resolved = session
            .call(GOAL, "resolve", Some(options_arg([])), TRANSACTION_TIMEOUT_MS)
            .expect("Goal.resolve");
        let items = resolved.child_value(0).n_children();
        let result = resolved.child_value(1).get::<u32>().unwrap_or(0);
        println!("resolved items={items} result={result}");
        let problems = session
            .call(GOAL, "get_transaction_problems_string", None, CALL_TIMEOUT_MS)
            .map(|r| problems_of(&r))
            .unwrap_or_default();
        println!("problems: {problems}");
        assert_eq!(result, 0, "{problems}");
    }
}
