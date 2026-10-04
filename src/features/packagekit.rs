//! System packages through **PackageKit**, the distro-agnostic service GNOME
//! Software drives (apt on Debian/Ubuntu, zypp on openSUSE, alpm on Arch, dnf on
//! Fedora). One D-Bus conversation with `org.freedesktop.PackageKit`:
//!
//! * **No `pkexec`, no root password of our own.** PackageKit is a root service;
//!   whether a request is allowed is polkit's call. Trusted installs and updates
//!   are `package-install` / `system-update`, which distros open to the active
//!   session of an admin (Fedora and Debian ship rules for `wheel`/`sudo`) and
//!   otherwise answer with the usual desktop authentication dialog — the same
//!   prompt the Software store shows, not a root terminal prompt.
//! * **Works from the Flatpak sandbox.** It is a system-bus name
//!   (`--system-talk-name=org.freedesktop.PackageKit`), so the app never has to
//!   escape to the host to change packages.
//! * **Updates apply at the next restart**, as in the store: the packages are
//!   downloaded (`only-download`), then the offline update is armed
//!   (`Offline.Trigger`) and the reboot installs them. A backend that can't
//!   download-only (Arch's) updates in place instead.
//!
//! PackageKit answers asynchronously: a method call only *starts* a
//! transaction, and the packages, the errors and the end all arrive as signals
//! on the transaction object. [`transact`] subscribes before it calls, then
//! pumps a private main context until `Finished`, so each entry point here is
//! an ordinary blocking function — and, like the dnf5daemon ones, must run off
//! the GTK main thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use glib::prelude::ToVariant;
use glib::Variant;

use crate::operations::TaskHandle;

const DEST: &str = "org.freedesktop.PackageKit";
const ROOT: &str = "/org/freedesktop/PackageKit";
const MANAGER: &str = "org.freedesktop.PackageKit";
const TRANSACTION: &str = "org.freedesktop.PackageKit.Transaction";
const OFFLINE: &str = "org.freedesktop.PackageKit.Offline";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Quick calls (a property, a new transaction).
const CALL_TIMEOUT_MS: i32 = 30_000;
/// Starting a transaction can sit on an authentication dialog.
const START_TIMEOUT_MS: i32 = 10 * 60 * 1000;
/// Nothing here legitimately runs longer: a download-and-update of a big
/// system on a slow link, with room to spare.
const TRANSACTION_LIMIT: Duration = Duration::from_secs(3 * 60 * 60);
/// How long an [`available`] verdict is reused (see the dnf5daemon twin).
const PROBE_TTL: Duration = Duration::from_secs(30);

/// Argv[0] that means "Spotty runs this itself" (see [`crate::dnf5daemon::ARGV0`]).
pub const ARGV0: &str = "spotty-packagekit";

/// Task verbs, as `ARGV0`'s first argument.
pub const VERB_UPDATE_ALL: &str = "update-all";
pub const VERB_UPDATE_PKG: &str = "update-pkg";
pub const VERB_INSTALL: &str = "install";
pub const VERB_SCHEDULE: &str = "schedule";
pub const VERB_ALL: &str = "all";

// PackageKit passes enums as bitfields, one bit per enum *value*
// (`pk_bitfield_value(e) == 1 << e`); the values are the ones PackageKit 1.x
// defines in pk-enum.h (read back from its own library, not remembered).
const fn bit(value: u32) -> u64 {
    1u64 << value
}
/// `PK_TRANSACTION_FLAG_ENUM_ONLY_TRUSTED` — without it the *untrusted*
/// polkit actions apply, which always ask for an admin password.
const FLAG_ONLY_TRUSTED: u64 = bit(1);
const FLAG_SIMULATE: u64 = bit(2);
const FLAG_ONLY_DOWNLOAD: u64 = bit(3);
const FILTER_NONE: u64 = bit(1);
const FILTER_NOT_INSTALLED: u64 = bit(3);
const FILTER_NEWEST: u64 = bit(16);
const FILTER_ARCH: u64 = bit(18);

// `PkInfoEnum` — only the ones read below.
const INFO_AVAILABLE: u32 = 2;
const INFO_LOW: u32 = 3;
const INFO_SECURITY: u32 = 8;
// `PkExitEnum`.
const EXIT_SUCCESS: u32 = 1;
const EXIT_CANCELLED: u32 = 3;
const EXIT_KEY_REQUIRED: u32 = 4;
const EXIT_EULA_REQUIRED: u32 = 5;
// `PkErrorEnum`.
const ERROR_NO_NETWORK: u32 = 2;
const ERROR_NOT_SUPPORTED: u32 = 3;
const ERROR_PACKAGE_NOT_FOUND: u32 = 8;
const ERROR_PACKAGE_ALREADY_INSTALLED: u32 = 9;
const ERROR_DEP_RESOLUTION_FAILED: u32 = 13;
const ERROR_CANNOT_GET_LOCK: u32 = 26;
const ERROR_NO_SPACE_ON_DEVICE: u32 = 46;
const ERROR_NOT_AUTHORIZED: u32 = 48;

// ── Describing what PackageKit reports ───────────────────────────────────────

/// One pending update, as `GetUpdates` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// `name;version;arch;data` — what `UpdatePackages` wants back.
    pub id: String,
    pub name: String,
    pub version: String,
}

impl Update {
    pub fn from_id(id: &str) -> Option<Update> {
        let mut parts = id.split(';');
        let name = parts.next().filter(|n| !n.is_empty())?;
        let version = parts.next().unwrap_or("");
        Some(Update {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    /// The "name  version" line every update source shows.
    pub fn line(&self) -> String {
        if self.version.is_empty() {
            self.name.clone()
        } else {
            format!("{}  {}", self.name, self.version)
        }
    }

    /// Whether a row's target ("vim", or dnf-style "vim.x86_64") names this one.
    pub fn matches(&self, target: &str) -> bool {
        target == self.name
            || target
                .rsplit_once('.')
                .is_some_and(|(n, _arch)| n == self.name)
    }
}

/// Words for PackageKit's status enum, shown next to the operation.
fn status_text(status: u32) -> Option<&'static str> {
    Some(match status {
        1 => "Waiting for the package manager",
        2 => "Preparing",
        4 => "Searching",
        7 => "Refreshing the package lists",
        8 => "Downloading",
        9 => "Installing",
        10 => "Updating",
        13 => "Resolving dependencies",
        15 => "Testing the changes",
        16 => "Applying the changes",
        31 => "Waiting for authorisation",
        _ => return None,
    })
}

/// A failure, with PackageKit's error code when it gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: Option<u32>,
    pub message: String,
}

impl Failure {
    fn plain(message: impl Into<String>) -> Failure {
        Failure {
            code: None,
            message: message.into(),
        }
    }

    fn from_code(code: u32, details: &str) -> Failure {
        let message = match code {
            ERROR_NOT_AUTHORIZED => "The system refused the request (authorisation required)".to_string(),
            ERROR_NO_NETWORK => "No network connection".to_string(),
            ERROR_PACKAGE_NOT_FOUND => "Package not found in the software sources".to_string(),
            ERROR_PACKAGE_ALREADY_INSTALLED => "Already installed".to_string(),
            ERROR_CANNOT_GET_LOCK => "Another package manager is running — try again in a moment".to_string(),
            ERROR_NO_SPACE_ON_DEVICE => "Not enough disk space".to_string(),
            ERROR_DEP_RESOLUTION_FAILED if details.trim().is_empty() => {
                "The dependencies can't be resolved".to_string()
            }
            _ if !details.trim().is_empty() => details.trim().to_string(),
            _ => format!("PackageKit error {code}"),
        };
        Failure {
            code: Some(code),
            message,
        }
    }
}

impl From<Failure> for String {
    fn from(f: Failure) -> String {
        f.message
    }
}

/// What a transaction reported by the time it ended.
#[derive(Default, Debug)]
struct Txn {
    /// `Package` signals: (info, package_id).
    packages: Vec<(u32, String)>,
    error: Option<(u32, String)>,
    exit: Option<u32>,
    percentage: u32,
    status: u32,
}

impl Txn {
    /// `Ok` only for a clean `Finished(success)`; otherwise the best wording.
    fn outcome(self) -> Result<Txn, Failure> {
        match self.exit {
            Some(EXIT_SUCCESS) => Ok(self),
            Some(EXIT_CANCELLED) => Err(Failure::plain("Cancelled")),
            Some(EXIT_KEY_REQUIRED) => Err(Failure::plain(
                "A signing key has to be accepted first — use the system software centre",
            )),
            Some(EXIT_EULA_REQUIRED) => Err(Failure::plain(
                "A licence has to be accepted first — use the system software centre",
            )),
            _ => Err(match self.error {
                Some((code, details)) => Failure::from_code(code, &details),
                None => Failure::plain("The package manager did not finish"),
            }),
        }
    }
}

// ── The transaction machinery ────────────────────────────────────────────────

/// One synchronous D-Bus call on the system bus.
fn call(
    conn: &gio::DBusConnection,
    path: &str,
    iface: &str,
    method: &str,
    params: Option<Variant>,
    timeout: i32,
) -> Result<Variant, Failure> {
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

/// A GDBus failure as wording worth showing (polkit refusals especially).
fn describe(error: &glib::Error) -> Failure {
    let message = error.message().to_string();
    if message.contains("Not authorized") || message.contains("not authorized") {
        return Failure::from_code(ERROR_NOT_AUTHORIZED, "");
    }
    Failure::plain(message)
}

fn tuple<const N: usize>(items: [Variant; N]) -> Variant {
    Variant::tuple_from_iter(items)
}

/// Run one transaction to its end and hand back what it reported.
///
/// `method` is a `Transaction` method, `args` its argument tuple. `task`, when
/// given, receives progress and is polled for cancellation.
fn transact(
    method: &str,
    args: Variant,
    interactive: bool,
    task: Option<&TaskHandle>,
) -> Result<Txn, Failure> {
    // Signals are delivered to the main context that is thread-default when
    // the subscription is made. This thread has none running, so it brings its
    // own and turns the crank itself.
    let ctx = glib::MainContext::new();
    ctx.with_thread_default(|| run_transaction(&ctx, method, args, interactive, task))
        .map_err(|e| Failure::plain(format!("main context: {e}")))?
}

fn run_transaction(
    ctx: &glib::MainContext,
    method: &str,
    args: Variant,
    interactive: bool,
    task: Option<&TaskHandle>,
) -> Result<Txn, Failure> {
    let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
        .map_err(|e| Failure::plain(format!("system bus: {e}")))?;
    let path = call(&conn, ROOT, MANAGER, "CreateTransaction", None, CALL_TIMEOUT_MS)?
        .child_value(0)
        .str()
        .map(String::from)
        .ok_or_else(|| Failure::plain("CreateTransaction: no object path"))?;

    let state = Rc::new(RefCell::new(Txn::default()));
    // Subscribe *before* starting: the transaction begins the moment the method
    // call is accepted, and a fast one can finish before we'd have listened.
    let on_signal = {
        let state = state.clone();
        conn.signal_subscribe(
            Some(DEST),
            Some(TRANSACTION),
            None,
            Some(&path),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, member, params| {
                let mut s = state.borrow_mut();
                match member {
                    "Package" => {
                        if let Some((info, id, _summary)) = params.get::<(u32, String, String)>() {
                            s.packages.push((info, id));
                        }
                    }
                    "ErrorCode" => {
                        if let Some((code, details)) = params.get::<(u32, String)>() {
                            s.error = Some((code, details));
                        }
                    }
                    "Finished" => {
                        if let Some((exit, _runtime)) = params.get::<(u32, u32)>() {
                            s.exit = Some(exit);
                        }
                    }
                    _ => {}
                }
            },
        )
    };
    // Percentage and Status are properties of the transaction object.
    let on_props = {
        let state = state.clone();
        conn.signal_subscribe(
            Some(DEST),
            Some(PROPERTIES),
            Some("PropertiesChanged"),
            Some(&path),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                let Some((_iface, changed, _invalid)) =
                    params.get::<(String, std::collections::HashMap<String, Variant>, Vec<String>)>()
                else {
                    return;
                };
                let mut s = state.borrow_mut();
                if let Some(p) = changed.get("Percentage").and_then(|v| v.get::<u32>()) {
                    s.percentage = p;
                }
                if let Some(st) = changed.get("Status").and_then(|v| v.get::<u32>()) {
                    s.status = st;
                }
            },
        )
    };
    let unsubscribe = |conn: &gio::DBusConnection| {
        conn.signal_unsubscribe(on_signal);
        conn.signal_unsubscribe(on_props);
    };

    let hints: Vec<String> = if interactive {
        vec!["interactive=true".into()]
    } else {
        vec!["background=true".into(), "interactive=false".into()]
    };
    let started = call(
        &conn,
        &path,
        TRANSACTION,
        "SetHints",
        Some(tuple([hints.to_variant()])),
        CALL_TIMEOUT_MS,
    )
    .and_then(|_| call(&conn, &path, TRANSACTION, method, Some(args), START_TIMEOUT_MS));
    if let Err(e) = started {
        unsubscribe(&conn);
        return Err(e);
    }

    // Wake up regularly even when PackageKit is quiet, to honour a cancel.
    let tick = glib::timeout_source_new(
        Duration::from_millis(250),
        None,
        glib::Priority::DEFAULT,
        || glib::ControlFlow::Continue,
    );
    tick.attach(Some(ctx));

    let began = Instant::now();
    let mut cancel_sent = false;
    let mut last_report = (u32::MAX, u32::MAX);
    let mut ticks = 0u32;
    let result = loop {
        if state.borrow().exit.is_some() {
            break Ok(());
        }
        if began.elapsed() > TRANSACTION_LIMIT {
            break Err(Failure::plain("The package manager took too long"));
        }
        ticks += 1;
        // A crashed PackageKit never says `Finished`: notice the object is gone.
        if ticks % 40 == 0
            && call(
                &conn,
                &path,
                PROPERTIES,
                "Get",
                Some(tuple([TRANSACTION.to_variant(), "Status".to_variant()])),
                CALL_TIMEOUT_MS,
            )
            .is_err()
        {
            break Err(Failure::plain("PackageKit stopped answering"));
        }
        if let Some(task) = task {
            if task.cancelled() && !cancel_sent {
                cancel_sent = true;
                let _ = call(&conn, &path, TRANSACTION, "Cancel", None, CALL_TIMEOUT_MS);
            }
            let (pct, status) = {
                let s = state.borrow();
                (s.percentage, s.status)
            };
            if (pct, status) != last_report {
                last_report = (pct, status);
                let fraction = (pct <= 100).then(|| f64::from(pct) / 100.0);
                task.report(fraction, status_text(status));
            }
        }
        ctx.iteration(true);
    };
    tick.destroy();
    unsubscribe(&conn);
    result?;
    // Take the state out of its Rc: the subscriptions are gone, so this is the
    // only owner left.
    let txn = std::mem::take(&mut *state.borrow_mut());
    txn.outcome()
}

// ── Availability ─────────────────────────────────────────────────────────────

static PROBE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

/// PackageKit's backend name ("dnf5", "aptcc", "zypp", "alpm"…), if it answers.
fn probe() -> Option<String> {
    let mut guard = PROBE.lock().unwrap();
    if let Some((at, verdict)) = guard.as_ref() {
        if at.elapsed() < PROBE_TTL {
            return verdict.clone();
        }
    }
    let verdict = read_backend();
    *guard = Some((Instant::now(), verdict.clone()));
    verdict
}

/// Ask for `BackendName`, which is also what activates the service when it
/// isn't running yet. The "dummy" backend is PackageKit with nothing behind it.
fn read_backend() -> Option<String> {
    let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;
    let reply = call(
        &conn,
        ROOT,
        PROPERTIES,
        "Get",
        Some(tuple([MANAGER.to_variant(), "BackendName".to_variant()])),
        CALL_TIMEOUT_MS,
    )
    .ok()?;
    let name = reply.child_value(0).as_variant()?.get::<String>()?;
    (!name.is_empty() && name != "dummy").then_some(name)
}

/// True when PackageKit is reachable and has a real backend.
pub fn available() -> bool {
    probe().is_some()
}

/// The backend PackageKit uses on this system (`aptcc`, `zypp`, `alpm`, `dnf5`…).
pub fn backend_name() -> Option<String> {
    probe()
}

/// Fill the [`available`] cache from a background thread, at startup.
pub fn warmup() {
    std::thread::spawn(|| {
        let _ = available();
    });
}

// ── Queries ──────────────────────────────────────────────────────────────────

/// The pending distro updates (read-only; needs no authorisation).
pub fn get_updates() -> Result<Vec<Update>, Failure> {
    let txn = transact("GetUpdates", tuple([FILTER_NONE.to_variant()]), false, None)?;
    let mut seen = std::collections::HashSet::new();
    Ok(txn
        .packages
        .iter()
        // BLOCKED (held-back) updates can't be applied, so aren't offered.
        .filter(|(info, _)| (INFO_LOW..=INFO_SECURITY).contains(info))
        .filter(|(_, id)| seen.insert(id.clone()))
        .filter_map(|(_, id)| Update::from_id(id))
        .collect())
}

/// "name  version" lines for the update list; empty when PackageKit is away.
pub fn update_lines() -> Vec<String> {
    if !available() {
        return Vec::new();
    }
    match get_updates() {
        Ok(list) => list.iter().map(Update::line).collect(),
        Err(e) => {
            log::info!("packagekit: update check failed: {}", e.message);
            Vec::new()
        }
    }
}

/// The newest installable package id for `name`, if the sources have one.
fn resolve_installable(name: &str) -> Result<Option<String>, Failure> {
    let filter = FILTER_NOT_INSTALLED | FILTER_NEWEST | FILTER_ARCH;
    let names: Vec<String> = vec![name.to_string()];
    let txn = transact(
        "Resolve",
        tuple([filter.to_variant(), names.to_variant()]),
        false,
        None,
    )?;
    Ok(txn
        .packages
        .into_iter()
        .find(|(info, _)| *info == INFO_AVAILABLE)
        .map(|(_, id)| id))
}

/// True when an update is downloaded and armed for the next restart.
///
/// Those packages still show up in `GetUpdates` until the restart installs
/// them, so the update check asks here and leaves them out.
pub fn offline_armed() -> bool {
    if !available() {
        return false;
    }
    let Ok(conn) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) else {
        return false;
    };
    call(
        &conn,
        ROOT,
        PROPERTIES,
        "Get",
        Some(tuple([OFFLINE.to_variant(), "UpdateTriggered".to_variant()])),
        CALL_TIMEOUT_MS,
    )
    .ok()
    .and_then(|r| r.child_value(0).as_variant())
    .and_then(|v| v.get::<bool>())
    .unwrap_or(false)
}

// ── Doing things ─────────────────────────────────────────────────────────────

/// Arm the downloaded update for the next boot — the store's "restart & update"
/// call, minus the restart.
fn trigger_offline() -> Result<(), Failure> {
    let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
        .map_err(|e| Failure::plain(format!("system bus: {e}")))?;
    call(
        &conn,
        ROOT,
        OFFLINE,
        "Trigger",
        Some(tuple(["reboot".to_variant()])),
        START_TIMEOUT_MS,
    )
    .map(|_| ())
}

/// Download the updates and arm them for the next restart; update in place on a
/// backend that can't download-only. `only` limits it to one package.
fn update_system(only: Option<&str>, task: &TaskHandle) -> Result<(), Failure> {
    task.status("Looking for updates");
    let wanted: Vec<Update> = get_updates()?
        .into_iter()
        .filter(|u| only.map_or(true, |p| u.matches(p)))
        .collect();
    if wanted.is_empty() {
        task.status("Nothing to update");
        return Ok(());
    }
    let ids: Vec<String> = wanted.iter().map(|u| u.id.clone()).collect();
    if task.cancelled() {
        return Err(Failure::plain("Cancelled"));
    }

    task.status(&format!("Downloading {} updates", ids.len()));
    let download = transact(
        "UpdatePackages",
        tuple([(FLAG_ONLY_TRUSTED | FLAG_ONLY_DOWNLOAD).to_variant(), ids.to_variant()]),
        true,
        Some(task),
    );
    match download {
        Ok(_) => {
            trigger_offline().map_err(|e| Failure {
                code: e.code,
                message: format!("Downloaded, but not scheduled for the restart: {}", e.message),
            })?;
            task.status("Update ready for restart");
            Ok(())
        }
        // The backend has no download-only mode: install now, as it always did.
        Err(e) if e.code == Some(ERROR_NOT_SUPPORTED) => {
            task.status("Updating");
            transact(
                "UpdatePackages",
                tuple([FLAG_ONLY_TRUSTED.to_variant(), ids.to_variant()]),
                true,
                Some(task),
            )?;
            task.status("Updated");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Install one package by name.
fn install(name: &str, task: &TaskHandle) -> Result<(), Failure> {
    task.status("Looking for the package");
    let id = resolve_installable(name)?.ok_or_else(|| {
        Failure::plain(format!("{name} isn't available to install (or is already installed)"))
    })?;
    if task.cancelled() {
        return Err(Failure::plain("Cancelled"));
    }
    let ids: Vec<String> = vec![id];
    transact(
        "InstallPackages",
        tuple([FLAG_ONLY_TRUSTED.to_variant(), ids.to_variant()]),
        true,
        Some(task),
    )?;
    task.status("Installed");
    Ok(())
}

/// Run an in-process operation. Called by [`crate::operations`] for every
/// operation whose argv starts with [`ARGV0`]. The `Err` is user-facing wording.
pub fn run_task(args: &[String], task: &TaskHandle) -> Result<(), String> {
    let verb = args.first().map(String::as_str).unwrap_or("");
    let spec = args.get(1).map(String::as_str);
    match verb {
        VERB_UPDATE_ALL => update_system(None, task).map_err(Into::into),
        VERB_UPDATE_PKG => update_system(spec, task).map_err(Into::into),
        VERB_INSTALL => install(spec.unwrap_or_default(), task).map_err(Into::into),
        VERB_SCHEDULE => {
            task.status("Scheduling updates for the next restart");
            trigger_offline().map_err(Into::into)
        }
        // The distro half, then the shell chain for flatpak/snap/AppImage.
        VERB_ALL => {
            let distro = update_system(None, task);
            let others = spec.map(|s| crate::dnf5daemon::run_script(s, task));
            distro.map_err(String::from)?;
            match others {
                Some(false) => Err("Some of the updates did not install".into()),
                _ => Ok(()),
            }
        }
        other => Err(format!("Unknown PackageKit task \"{other}\"")),
    }
}

// ── Argv builders (the operation registry speaks argv) ───────────────────────

/// "Update every available package" (or one, by name).
pub fn update_args(package: Option<&str>) -> Vec<String> {
    match package {
        Some(p) => vec![ARGV0.into(), VERB_UPDATE_PKG.into(), p.into()],
        None => vec![ARGV0.into(), VERB_UPDATE_ALL.into()],
    }
}

pub fn install_args(name: &str) -> Vec<String> {
    vec![ARGV0.into(), VERB_INSTALL.into(), name.into()]
}

/// Arm an already-downloaded update.
pub fn schedule_args() -> Vec<String> {
    vec![ARGV0.into(), VERB_SCHEDULE.into()]
}

/// "Update all sources": the distro half first, then a shell script.
pub fn all_args(script: &str) -> Vec<String> {
    vec![ARGV0.into(), VERB_ALL.into(), script.to_string()]
}

/// True when this argv is an in-process run that changes installed packages
/// by updating (downloads and arms, or updates in place).
pub fn is_update_task(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some(ARGV0)
        && matches!(
            args.get(1).map(String::as_str),
            Some(VERB_UPDATE_ALL | VERB_UPDATE_PKG | VERB_ALL)
        )
}

/// True when this argv only arms what is already downloaded.
pub fn is_schedule_task(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some(ARGV0)
        && args.get(1).map(String::as_str) == Some(VERB_SCHEDULE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_ids_become_update_lines() {
        let u = Update::from_id("firefox;130.0-1.fc44;x86_64;updates").unwrap();
        assert_eq!(u.name, "firefox");
        assert_eq!(u.version, "130.0-1.fc44");
        assert_eq!(u.line(), "firefox  130.0-1.fc44");
        // A version-less id still names the package.
        assert_eq!(Update::from_id("vim").unwrap().line(), "vim");
        assert!(Update::from_id("").is_none());
        assert!(Update::from_id(";1.0;x86_64;repo").is_none());
    }

    #[test]
    fn a_row_target_finds_its_update() {
        let u = Update::from_id("vim;9.2;x86_64;repo").unwrap();
        assert!(u.matches("vim"));
        // dnf-style "name.arch", as the update rows pass them.
        assert!(u.matches("vim.x86_64"));
        assert!(!u.matches("vi"));
        assert!(!u.matches("vimx"));
        // A dotted name is not mistaken for name.arch.
        let lib = Update::from_id("libfoo.so;1;x86_64;repo").unwrap();
        assert!(lib.matches("libfoo.so"));
    }

    #[test]
    fn flags_and_filters_are_one_bit_per_enum_value() {
        // The values come from PackageKit's pk-enum.h: ONLY_TRUSTED=1,
        // SIMULATE=2, ONLY_DOWNLOAD=3; filters NONE=1, NOT_INSTALLED=3,
        // NEWEST=16, ARCH=18 — each sent as `1 << value`.
        assert_eq!(FLAG_ONLY_TRUSTED, 0b10);
        assert_eq!(FLAG_SIMULATE, 0b100);
        assert_eq!(FLAG_ONLY_DOWNLOAD, 0b1000);
        assert_eq!(FILTER_NONE, 0b10);
        assert_eq!(FILTER_NOT_INSTALLED, 1 << 3);
        assert_eq!(FILTER_NEWEST, 1 << 16);
        assert_eq!(FILTER_ARCH, 1 << 18);
        // Distinct, so combining them can never alias one into another.
        let all = [FLAG_ONLY_TRUSTED, FLAG_SIMULATE, FLAG_ONLY_DOWNLOAD];
        assert_eq!(all.iter().fold(0, |a, b| a | b).count_ones(), 3);
    }

    #[test]
    fn a_transaction_outcome_is_only_ok_when_it_succeeded() {
        let ok = Txn {
            exit: Some(EXIT_SUCCESS),
            ..Txn::default()
        };
        assert!(ok.outcome().is_ok());

        // An error code carries its own wording…
        let failed = Txn {
            exit: Some(2),
            error: Some((ERROR_NO_NETWORK, "dns".into())),
            ..Txn::default()
        };
        let e = failed.outcome().unwrap_err();
        assert_eq!(e.code, Some(ERROR_NO_NETWORK));
        assert_eq!(e.message, "No network connection");

        // …an unknown one falls back to PackageKit's own details…
        let odd = Txn {
            exit: Some(2),
            error: Some((999, " disk exploded ".into())),
            ..Txn::default()
        };
        assert_eq!(odd.outcome().unwrap_err().message, "disk exploded");

        // …a cancel is not a failure message…
        let cancelled = Txn {
            exit: Some(EXIT_CANCELLED),
            ..Txn::default()
        };
        assert_eq!(cancelled.outcome().unwrap_err().message, "Cancelled");

        // …and never having finished is never success.
        assert!(Txn::default().outcome().is_err());
    }

    #[test]
    fn polkit_refusals_read_as_authorisation_problems() {
        let e = Failure::from_code(ERROR_NOT_AUTHORIZED, "");
        assert!(e.message.contains("authorisation"), "{e:?}");
        let lock = Failure::from_code(ERROR_CANNOT_GET_LOCK, "");
        assert!(lock.message.contains("Another package manager"), "{lock:?}");
        // Download-only being unsupported is how the in-place fallback triggers.
        assert_eq!(Failure::from_code(ERROR_NOT_SUPPORTED, "").code, Some(ERROR_NOT_SUPPORTED));
    }

    #[test]
    fn task_argv_is_recognisable() {
        assert!(is_update_task(&update_args(None)));
        assert!(is_update_task(&update_args(Some("vim"))));
        assert!(is_update_task(&all_args("true")));
        // Installing and arming are not update runs: they don't need the
        // post-update re-check, and arming has its own re-probe.
        assert!(!is_update_task(&install_args("vim")));
        assert!(!is_update_task(&schedule_args()));
        assert!(is_schedule_task(&schedule_args()));
        assert!(!is_schedule_task(&update_args(None)));
        // Not ours.
        assert!(!is_update_task(&["flatpak".into(), VERB_UPDATE_ALL.into()]));
        assert_eq!(update_args(Some("vim")), vec![ARGV0, VERB_UPDATE_PKG, "vim"]);
        assert_eq!(install_args("vim"), vec![ARGV0, VERB_INSTALL, "vim"]);
    }

    #[test]
    fn status_words_cover_the_phases_a_user_waits_through() {
        for s in [8, 9, 10, 13, 16, 31] {
            assert!(status_text(s).is_some(), "status {s}");
        }
        // Housekeeping states say nothing rather than something wrong.
        assert!(status_text(0).is_none());
        assert!(status_text(18).is_none());
    }

    /// Read-only: the system PackageKit answers and has a real backend.
    #[test]
    #[ignore = "needs a running PackageKit"]
    fn the_system_packagekit_answers() {
        let backend = backend_name();
        println!("backend: {backend:?} offline armed: {}", offline_armed());
        assert!(backend.is_some());
    }

    /// Read-only: `GetUpdates` runs through the signal machinery end to end.
    #[test]
    #[ignore = "needs a running PackageKit"]
    fn the_system_packagekit_lists_updates() {
        let updates = get_updates().expect("GetUpdates");
        println!("{} updates", updates.len());
        for u in updates.iter().take(5) {
            println!("  {}", u.line());
        }
    }

    /// Resolves a package and *simulates* installing it: PackageKit works out
    /// what would change and reports it, installing nothing.
    #[test]
    #[ignore = "needs a running PackageKit and package metadata"]
    fn the_system_packagekit_simulates_an_install() {
        let id = resolve_installable("cowsay").expect("Resolve");
        println!("resolved: {id:?}");
        let Some(id) = id else { return };
        let ids: Vec<String> = vec![id];
        let txn = transact(
            "InstallPackages",
            tuple([(FLAG_ONLY_TRUSTED | FLAG_SIMULATE).to_variant(), ids.to_variant()]),
            false,
            None,
        )
        .expect("simulated InstallPackages");
        println!("would change: {:?}", txn.packages);
        assert!(!txn.packages.is_empty());
    }
}
