// Bluetooth search (bt trigger): list connected / available devices, connect,
// disconnect, pair, scan, and toggle controller power — all via `bluetoothctl`
// on the host (bluez). The trigger itself ships as a manifest in the
// triggers repository; this module
// powers the dynamic device list shown once the `bt` mode is entered.

use super::{Action, ResultKind, SearchResult};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Eq)]
struct Device {
    mac: String,
    name: String,
}

#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    powered: bool,
    available: bool,
    connected: Vec<Device>,
    paired: Vec<Device>,
    others: Vec<Device>,
}

/// Enumeration results are cached for a few seconds so per-keystroke searches
/// don't re-spawn bluetoothctl processes; `invalidate()` forces a refresh
/// after any action completes.
static CACHE: OnceLock<Mutex<Option<(Instant, Snapshot)>>> = OnceLock::new();

/// Whether a continuous scan session is active.
static SCANNING: AtomicBool = AtomicBool::new(false);

/// Whether we powered the adapter on for the scan (to restore off on cancel).
static RESTORE_OFF: AtomicBool = AtomicBool::new(false);

fn run(cmd: &str) -> Option<String> {
    crate::app::run_host_shell_command(cmd)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            if it.next() == Some("Device") {
                let mac = it.next()?.to_string();
                let name = it.next().unwrap_or("").trim().to_string();
                Some(Device { mac, name })
            } else {
                None
            }
        })
        .collect()
}

fn snapshot() -> Snapshot {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    if let Some((at, snap)) = guard.as_ref() {
        if SCANNING.load(Ordering::Relaxed) || at.elapsed() < Duration::from_secs(4) {
            return snap.clone();
        }
    }
    let snap = enumerate();
    *guard = Some((Instant::now(), snap.clone()));
    snap
}

fn update_cache(new: Snapshot) {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    let changed = guard.as_ref().map_or(true, |(_, old)| old != &new);
    *guard = Some((Instant::now(), new));
    if changed {
        // Marshal to the main thread — STATE.with() is thread-local.
        gtk::glib::idle_add_once(crate::app::refresh_search_window);
    }
}

pub(crate) fn invalidate_cache() {
    if let Some(c) = CACHE.get() {
        *c.lock().unwrap() = None;
    }
}

fn cached_powered() -> bool {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    cache
        .lock()
        .unwrap()
        .as_ref()
        .map_or(false, |(_, s)| s.powered)
}

/// Shell command prefix that reaches `bluetoothctl` on the host even inside a
/// Flatpak sandbox (used by the progress-pane runner, which spawns argv
/// directly rather than through a shell).
fn shell_cmd(cmd: &str) -> Vec<String> {
    if crate::app::is_flatpak() {
        vec![
            "flatpak-spawn".into(),
            "--host".into(),
            "sh".into(),
            "-lc".into(),
            cmd.into(),
        ]
    } else {
        vec!["sh".into(), "-lc".into(), cmd.into()]
    }
}

// ── Power state re-check ─────────────────────────────────────────────────

/// At most one power-state probe can run at a time.
static POWER_CHECK_INFLIGHT: AtomicBool = AtomicBool::new(false);

/// Timestamp of the last successful power probe.  Used to skip redundant
/// probes (at most one per ~2 s).
static POWER_CHECK_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// Background: re-probe `bluetoothctl show` and update the cached powered
/// flag if it changed.  Runs at most once every ~2 s.
fn refresh_power_state() {
    if POWER_CHECK_INFLIGHT.swap(true, Ordering::Relaxed) {
        return; // already in flight
    }
    {
        let guard = POWER_CHECK_AT.lock().unwrap();
        if guard.as_ref().is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
            POWER_CHECK_INFLIGHT.store(false, Ordering::Relaxed);
            return;
        }
    }
    std::thread::spawn(|| {
        let powered = run_direct(&["bluetoothctl", "show"])
            .map(|o| o.lines().any(|l| l.contains("Powered: yes")))
            .unwrap_or(true); // probe failure → keep last known value
        *POWER_CHECK_AT.lock().unwrap() = Some(Instant::now());
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let mut guard = cache.lock().unwrap();
        let changed = guard.as_ref().map_or(false, |(_, s)| s.powered != powered);
        if let Some((_, snap)) = guard.as_mut() {
            snap.powered = powered;
        }
        if changed || guard.as_ref().map_or(true, |(_, s)| !s.available) {
            drop(guard);
            nudge_ui();
        }
        POWER_CHECK_INFLIGHT.store(false, Ordering::Relaxed);
    });
}

// ── Scan session ──────────────────────────────────────────────────────────

/// rfkill soft blocks (airplane mode / Fn key / ideapad platform switch) make
/// bluez refuse `power on` with org.bluez.Error.Failed — clear it first.
/// The settle + retry covers the bluez/rfkill race.
const POWER_ON: &str =
    "rfkill unblock bluetooth 2>/dev/null; sleep 0.3; bluetoothctl power on \
     || { sleep 1; bluetoothctl power on; }";

/// Persistent bluetoothctl child that holds discovery alive.
static SCAN_CHILD: std::sync::Mutex<Option<std::process::Child>> =
    std::sync::Mutex::new(None);

/// Pre-scan baseline of known MACs; devices found during the scan that
/// are *not* in this set count as "new" and are shown in the scan list.
static SCAN_BASELINE: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

/// Coalesced nudge: at most one idle callback per main-loop turn.
static NUDGE_PENDING: AtomicBool = AtomicBool::new(false);

/// Coalesced UI nudge (safe from any thread). Avoids a shower of rebuilds
/// when many discovery events arrive in a burst.
fn nudge_ui() {
    if !NUDGE_PENDING.swap(true, Ordering::Relaxed) {
        gtk::glib::idle_add_once(|| {
            NUDGE_PENDING.store(false, Ordering::Relaxed);
            crate::app::refresh_search_window();
        });
    }
}

/// Drop a reader into /dev/null so the pipe never fills.
fn drain<R: std::io::Read + Send + 'static>(mut r: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut r, &mut std::io::sink());
    });
}

/// Spawn the long-lived discovery holder with line-buffered stdout so
/// `[NEW]/[CHG]/[DEL]` events arrive promptly via the event reader.
fn start_discovery() -> bool {
    // stdbuf -oL forces line buffering via LD_PRELOAD so bluetoothctl
    // events aren't held up by block-buffered stdio.
    let mut args: Vec<String> = vec!["--timeout".into(), "1800".into(), "scan".into(), "on".into()];
    let program: String;
    if crate::app::is_flatpak() {
        program = "flatpak-spawn".into();
        args.insert(0, "--host".into());
        args.insert(1, "stdbuf".into());
        args.insert(2, "-oL".into());
    } else {
        program = "stdbuf".into();
        args.insert(0, "-oL".into());
        args.insert(1, "bluetoothctl".into());
    }
    match std::process::Command::new(&program)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            // Parse discovery events from stdout for instant updates.
            if let Some(out) = child.stdout.take() {
                        std::thread::spawn(move || event_reader(std::io::BufReader::new(out)));
            }
            // Drain stderr (errors/prompts).
            if let Some(err) = child.stderr.take() {
                drain(err);
            }
            let mut guard = SCAN_CHILD.lock().unwrap();
            *guard = Some(child);
            true
        }
        Err(e) => {
            // stdbuf may not be installed — fall back to plain bluetoothctl.
            let mut args2: Vec<String> = vec![
                "--timeout".into(), "1800".into(), "scan".into(), "on".into(),
            ];
            let program2: String;
            if crate::app::is_flatpak() {
                program2 = "flatpak-spawn".into();
                args2.insert(0, "--host".into());
            } else {
                program2 = "bluetoothctl".into();
            }
            match std::process::Command::new(&program2)
                .args(&args2)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                std::thread::spawn(move || event_reader(std::io::BufReader::new(out)));
                    }
                    if let Some(err) = child.stderr.take() {
                        drain(err);
                    }
                    *SCAN_CHILD.lock().unwrap() = Some(child);
                    true
                }
                Err(e2) => {
                    log::warn!("bluetoothctl scan on failed (stdbuf: {e}, plain: {e2})");
                    false
                }
            }
        }
    }
}

/// Kill the persistent discovery holder (if any).
fn kill_discovery() {
    if let Some(mut child) = SCAN_CHILD.lock().unwrap().take() {
        let _ = child.kill();
    }
}

/// True if the persistent discovery child is still running.
fn discovery_alive() -> bool {
    SCAN_CHILD
        .lock()
        .unwrap()
        .as_mut()
        .map_or(false, |c| c.try_wait().ok().flatten().is_none())
}

// ── Discovery event reader ────────────────────────────────────────────────

/// Parse `[NEW]`, `[DEL]`, and `[CHG ... Name:]` events from the
/// discovery child's stdout and update the cache incrementally.
/// Keeps the "others" list current; connected/paired are not tracked
/// here (the safety-net poll refreshes them when the scan stops).
fn event_reader(reader: impl std::io::BufRead) {
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.starts_with("[NEW] Device ") || line.starts_with("[CHG] Device ") {
            // "[NEW] Device A0:5A:5F:31:B2:76 Wireless Controller"
            // "[CHG] Device ... Name: Wireless Controller"
            let rest = if line.starts_with("[NEW] Device ") {
                &line[13..]
            } else {
                &line[13..]
            };
            let (mac, name) = parse_event_device(rest);
            if !mac.is_empty() {
                cache_upsert_other(&mac, &name);
            }
        } else if line.starts_with("[DEL] Device ") {
            let mac = line[13..].split_whitespace().next().unwrap_or("");
            if !mac.is_empty() {
                cache_remove_mac(mac);
            }
        } else if line.contains("Discovering: no") {
            // Discovery ended — the session may have ended (child exit or
            // explicit stop). The poller/self-healing will handle it.
        } else if line.contains("Powered: yes") {
            cache_set_powered(true);
        } else if line.contains("Powered: no") {
            cache_set_powered(false);
        }
    }
}

/// Extract (mac, name) from an event line like
/// "A0:5A:5F:31:B2:76 Wireless Controller".
/// For CHG events the name may be "Name: Wireless Controller".
fn parse_event_device(rest: &str) -> (String, String) {
    let rest = rest.trim();
    let parts: Vec<&str> = rest.splitn(2, " ").collect();
    if parts.len() < 2 {
        return (parts[0].to_string(), String::new());
    }
    let mac = parts[0].to_string();
    let mut name = parts[1].trim().to_string();
    // CHG events use "Name: ..." or "Alias: ..." after the MAC.
    if let Some(stripped) = name.strip_prefix("Name: ") {
        name = stripped.trim().to_string();
    } else if let Some(stripped) = name.strip_prefix("Alias: ") {
        name = stripped.trim().to_string();
    }
    (mac, name)
}

/// Insert/update a device in the `others` list. Connected/paired are skipped.
/// Nudges the UI if the cache changed.
fn cache_upsert_other(mac: &str, name: &str) {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    let snap = guard.as_mut().map(|(_, s)| s);
    let snap = match snap {
        Some(s) => s,
        None => {
            let new_snap = Snapshot { powered: true, available: true, connected: Vec::new(), paired: Vec::new(), others: vec![Device { mac: mac.into(), name: name.into() }] };
            *guard = Some((Instant::now(), new_snap));
            drop(guard);
            nudge_ui();
            return;
        }
    };
    // Don't add to others if it's already connected or paired.
    if snap.connected.iter().any(|d| d.mac == mac) || snap.paired.iter().any(|d| d.mac == mac) {
        return;
    }
    if let Some(d) = snap.others.iter_mut().find(|d| d.mac == mac) {
        if d.name != name && !name.is_empty() {
            d.name = name.to_string();
            drop(guard);
            nudge_ui();
        }
    } else {
        snap.others.push(Device { mac: mac.into(), name: name.into() });
        drop(guard);
        nudge_ui();
    }
}

/// Remove a device from all lists.
fn cache_remove_mac(mac: &str) {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    if let Some((_, snap)) = guard.as_mut() {
        let before = snap.others.len() + snap.connected.len() + snap.paired.len();
        snap.others.retain(|d| d.mac != mac);
        snap.connected.retain(|d| d.mac != mac);
        snap.paired.retain(|d| d.mac != mac);
        if snap.others.len() + snap.connected.len() + snap.paired.len() != before {
            drop(guard);
            nudge_ui();
        }
    }
}

/// Update the powered flag from a discovery event.
fn cache_set_powered(powered: bool) {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    if let Some((_, snap)) = guard.as_mut() {
        if snap.powered != powered {
            snap.powered = powered;
            drop(guard);
            nudge_ui();
        }
    }
}

// ── Lightweight poll (direct argv, no sh -lc) ────────────────────────────

/// Poll `bluetoothctl devices` directly (no shell wrapper) — much cheaper
/// than the full `enumerate()` which spawns 4 sh+bluetoothctl pairs.
/// Only used during the scan for the discovery list.
fn poll_devices_only() -> Vec<Device> {
    let out = if crate::app::is_flatpak() {
        run_direct(&["flatpak-spawn", "--host", "bluetoothctl", "devices"])
    } else {
        run_direct(&["bluetoothctl", "devices"])
    };
    out.map(|o| parse_devices(&o)).unwrap_or_default()
}

/// Spawn a command directly (no sh -lc) and return its stdout.
fn run_direct(args: &[&str]) -> Option<String> {
    std::process::Command::new(args[0])
        .args(&args[1..])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Full enumerate (sh -lc; used when NOT scanning or on scan stop).
fn enumerate() -> Snapshot {
    let all = run("bluetoothctl devices");
    let Some(all) = all else {
        return Snapshot {
            powered: false,
            available: false,
            connected: Vec::new(),
            paired: Vec::new(),
            others: Vec::new(),
        };
    };
    let connected = parse_devices(&run("bluetoothctl devices Connected").unwrap_or_default());
    let paired = parse_devices(&run("bluetoothctl devices Paired").unwrap_or_default());
    let powered = run("bluetoothctl show")
        .map(|s| s.lines().any(|l| l.trim().starts_with("Powered: yes")))
        .unwrap_or(false);
    let known = parse_devices(&all);
    let con: HashSet<&str> = connected.iter().map(|d| d.mac.as_str()).collect();
    let pair: HashSet<&str> = paired.iter().map(|d| d.mac.as_str()).collect();
    let others = known
        .into_iter()
        .filter(|d| !con.contains(d.mac.as_str()) && !pair.contains(d.mac.as_str()))
        .collect();
    Snapshot {
        powered,
        available: true,
        connected,
        paired,
        others,
    }
}

/// Background scan thread: powers on, starts discovery, polls for new devices,
/// and handles child exit.
fn scan_thread() {
    // Power on if the adapter was off before the scan started.
    if RESTORE_OFF.load(Ordering::Relaxed) {
        let _ = run(POWER_ON);
    }
    if !start_discovery() {
        SCANNING.store(false, Ordering::Relaxed);
        nudge_ui();
        return;
    }
    // Initial baseline snapshot (light poll — only `devices`).
    let initial_devices = poll_devices_only();
    {
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let snap = Snapshot {
            powered: true,
            available: true,
            connected: Vec::new(),
            paired: Vec::new(),
            others: initial_devices,
        };
        *cache.lock().unwrap() = Some((Instant::now(), snap));
    }
    nudge_ui();
    // Poll loop: lightweight `devices`-only poll every ~2 s (safety net
    // for missed events). The event_reader handles instant updates.
    while SCANNING.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(2000));
        if !SCANNING.load(Ordering::Relaxed) {
            break;
        }
        if !discovery_alive() {
            log::info!("bluetooth: scan session ended (child exited)");
            SCANNING.store(false, Ordering::Relaxed);
            invalidate_cache();
            nudge_ui();
            return;
        }
        let devices = poll_devices_only();
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let mut guard = cache.lock().unwrap();
        let snap = guard.as_mut().map(|(_, s)| s);
        if let Some(snap) = snap {
            // Merge: add new devices to others, update names.
            for d in &devices {
                if snap.connected.iter().any(|x| x.mac == d.mac)
                    || snap.paired.iter().any(|x| x.mac == d.mac)
                {
                    continue;
                }
                if let Some(existing) = snap.others.iter_mut().find(|x| x.mac == d.mac) {
                    if !d.name.is_empty() && existing.name != d.name {
                        existing.name = d.name.clone();
                    }
                } else {
                    snap.others.push(d.clone());
                }
            }
            snap.others.retain(|d| devices.iter().any(|x| x.mac == d.mac));
            nudge_ui();
        } else {
            *guard = Some((Instant::now(), Snapshot {
                powered: true,
                available: true,
                connected: Vec::new(),
                paired: Vec::new(),
                others: devices,
            }));
            nudge_ui();
        }
    }
}

/// Start a continuous Bluetooth scan session.
pub fn start_scan() {
    if SCANNING.swap(true, Ordering::Relaxed) {
        return;
    }
    // Capture baseline of known MACs at scan start (no main-thread spawns).
    // If the adapter was off pre-scan, baseline is empty (every device is "new").
    if cached_powered() {
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let guard = cache.lock().unwrap();
        let baseline = guard.as_ref().map_or_else(HashSet::new, |(_, s)| {
            let mut m = HashSet::new();
            for d in &s.connected { m.insert(d.mac.clone()); }
            for d in &s.paired { m.insert(d.mac.clone()); }
            for d in &s.others { m.insert(d.mac.clone()); }
            m
        });
        *SCAN_BASELINE.lock().unwrap() = Some(baseline);
    } else {
        *SCAN_BASELINE.lock().unwrap() = Some(HashSet::new());
    }
    RESTORE_OFF.store(!cached_powered(), Ordering::Relaxed);
    invalidate_cache();
    nudge_ui();
    std::thread::spawn(|| scan_thread());
}

pub enum ScanStop {
    DeviceChosen,
    Cancelled,
}

pub fn stop_scan(reason: ScanStop) {
    if !SCANNING.swap(false, Ordering::Relaxed) {
        return;
    }
    kill_discovery();
    let _ = run("bluetoothctl scan off");
    let restore =
        matches!(reason, ScanStop::Cancelled) && RESTORE_OFF.swap(false, Ordering::Relaxed);
    if restore {
        // Spawn and wait for the power-off to complete (with a short poll
        // to ensure the adapter reports Powered: no) so the cache can
        // never be populated with a stale "on" value.
        std::thread::spawn(|| {
            let _ = run("bluetoothctl power off");
            // Poll until Powered: no (or timeout after ~2 s).
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                let powered = run_direct(&["bluetoothctl", "show"])
                    .map(|o| o.lines().any(|l| l.contains("Powered: yes")))
                    .unwrap_or(false);
                if !powered {
                    break;
                }
            }
            invalidate_cache();
            nudge_ui();
        });
    } else {
        invalidate_cache();
        crate::app::refresh_search_window();
    }
    *SCAN_BASELINE.lock().unwrap() = None;
}

pub fn is_scanning() -> bool {
    SCANNING.load(Ordering::Relaxed)
}

// ── Background Bluetooth action runner ────────────────────────────────────

/// Whether a Bluetooth action is currently running in the background.
static BT_BUSY: AtomicBool = AtomicBool::new(false);

/// Label shown in the tooltip while a BT action is running.
static BT_TASK: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Result of the last completed BT action: (text, ok).
static BT_RESULT: std::sync::Mutex<Option<(String, bool)>> = std::sync::Mutex::new(None);

/// Single source of truth for the shell command per BT action.
fn bt_command(op: &str, mac: &str) -> Option<String> {
    match op {
        "power_on" => Some(POWER_ON.to_string()),
        "power_off" => Some("bluetoothctl power off".to_string()),
        "connect" => Some(format!("{POWER_ON}; timeout 25 bluetoothctl connect {mac}")),
        "disconnect" => Some(format!("bluetoothctl disconnect {mac}")),
        "pair" => Some(format!(
            "{POWER_ON}; timeout 25 bluetoothctl pair {mac} && timeout 25 bluetoothctl connect {mac}"
        )),
        _ => None,
    }
}

pub fn is_busy() -> bool {
    BT_BUSY.load(Ordering::Relaxed)
}

pub fn busy_label() -> String {
    BT_TASK
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default()
}

pub fn take_result() -> Option<(String, bool)> {
    BT_RESULT.lock().unwrap().take()
}

/// The label the toast should show for a successful action.
fn toast_label(op: &str, device_name: &str, mac: &str) -> String {
    match op {
        "power_on" => "Bluetooth is on".into(),
        "power_off" => "Bluetooth is off".into(),
        "connect" => {
            if device_name.is_empty() { format!("Connected to {mac}") }
            else { format!("Connected to {device_name}") }
        }
        "pair" => {
            if device_name.is_empty() { format!("Paired and connected to {mac}") }
            else { format!("Paired and connected to {device_name}") }
        }
        "disconnect" => {
            if device_name.is_empty() { "Disconnected".into() }
            else { format!("Disconnected from {device_name}") }
        }
        _ => "Done".into(),
    }
}

/// The label for a failed action.
fn fail_label(op: &str) -> String {
    match op {
        "power_on" => "Couldn't turn on Bluetooth".into(),
        "power_off" => "Couldn't turn off Bluetooth".into(),
        "connect" => "Couldn't connect".into(),
        "pair" => "Couldn't pair".into(),
        "disconnect" => "Couldn't disconnect".into(),
        _ => "Bluetooth operation failed".into(),
    }
}

/// Expected `Powered:` value after a power transition.
fn expected_powered(op: &str) -> bool {
    matches!(op, "power_on")
}

/// Run a Bluetooth action in the background. Stores a toast result on
/// completion so the window can show it (no progress pane).
pub fn run_action(op: &str, mac: &str, device_name: &str) {
    let Some(cmd) = bt_command(op, mac) else {
        return;
    };
    let toast_text = toast_label(op, device_name, mac);
    let fail_text = fail_label(op);
    let task = match op {
        "power_on" => "Turning on Bluetooth…".into(),
        "power_off" => "Turning off Bluetooth…".into(),
        "connect" => "Connecting…".into(),
        "pair" => "Pairing…".into(),
        "disconnect" => "Disconnecting…".into(),
        _ => "Working…".into(),
    };
    BT_BUSY.store(true, Ordering::Relaxed);
    *BT_TASK.lock().unwrap() = Some(task);
    nudge_ui();
    let op_owned = op.to_string();
    std::thread::spawn(move || {
        let ok = if crate::app::is_flatpak() {
            crate::app::run_host_shell_command(&cmd)
                .ok()
                .map(|o| o.status.success())
                .unwrap_or(false)
        } else {
            std::process::Command::new("sh")
                .args(["-lc", &cmd])
                .output()
                .ok()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        // Poll until the expected end state is reflected in the daemon,
        // or give up after a short timeout.
        let mut verified = ok;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            let current = run_direct(&["bluetoothctl", "show"])
                .map(|o| o.lines().any(|l| l.contains("Powered: yes")))
                .unwrap_or(!expected_powered(&op_owned));
            if expected_powered(&op_owned) == current {
                verified = true;
                break;
            }
            if op_owned == "connect" || op_owned == "pair" || op_owned == "disconnect" {
                // For connect/pair/disconnect, a single poll is enough — if
                // the command failed, no point waiting for the state to change.
                break;
            }
            verified = false;
        }
        let result = if verified { toast_text } else { fail_text };
        *BT_RESULT.lock().unwrap() = Some((result, verified));
        BT_BUSY.store(false, Ordering::Relaxed);
        *BT_TASK.lock().unwrap() = None;
        invalidate_cache();
        nudge_ui();
    });
}

/// Return the expected `Powered` state after a power transition.
pub fn expected_power(op: &str) -> bool {
    matches!(op, "power_on")
}

// ── Search results ────────────────────────────────────────────────────────

pub fn search(query: &str) -> Vec<SearchResult> {
    let snap = snapshot();
    if !snap.available {
        return vec![SearchResult {
            kind: ResultKind::System,
            title: "bluetoothctl not found".into(),
            subtitle: Some("Install bluez to control Bluetooth devices".into()),
            icon: Some("bluetooth-disabled-symbolic".into()),
            action: Action::Bluetooth {
                op: "check".into(),
                mac: String::new(),
            },
            score: 100_000,
        }];
    }

    let ql = query.trim().to_lowercase();
    let mut results = Vec::new();
    let scanning = SCANNING.load(Ordering::Relaxed);

    if scanning {
        // Scan mode: only devices newly discovered since the scan started.
        let baseline = SCAN_BASELINE.lock().unwrap();
        let mut idx = 0;
        for d in &snap.others {
            if baseline.as_ref().map_or(false, |b| b.contains(&d.mac)) {
                continue; // known pre-scan — not "new"
            }
            let name = if d.name.is_empty() {
                d.mac.clone()
            } else {
                d.name.clone()
            };
            if !ql.is_empty()
                && !name.to_lowercase().contains(&ql)
                && !d.mac.to_lowercase().contains(&ql)
            {
                continue;
            }
            results.push(SearchResult {
                kind: ResultKind::System,
                title: name,
                subtitle: Some(format!("{} — Enter to pair and connect", d.mac)),
                icon: Some("bluetooth-symbolic".into()),
                action: Action::Bluetooth {
                    op: "pair".into(),
                    mac: d.mac.clone(),
                },
                score: 95_000 - idx,
            });
            idx += 1;
        }
        results.sort_by(|a, b| b.score.cmp(&a.score));
        return results;
    }

    // Normal (non-scan) view.
    if ql.is_empty() {
        // Refresh the power state in the background so the row is accurate
        // (catches external toggles; at most once per ~2 s).
        refresh_power_state();
        // Power row: only the usable action is shown.
        if snap.powered {
            results.push(SearchResult {
                kind: ResultKind::System,
                title: "Turn off Bluetooth".into(),
                subtitle: Some("Bluetooth is on".into()),
                icon: Some("bluetooth-active-symbolic".into()),
                action: Action::Bluetooth {
                    op: "power_off".into(),
                    mac: String::new(),
                },
                score: 100_000,
            });
        } else {
            results.push(SearchResult {
                kind: ResultKind::System,
                title: "Turn on Bluetooth".into(),
                subtitle: Some("Bluetooth is off".into()),
                icon: Some("bluetooth-disabled-symbolic".into()),
                action: Action::Bluetooth {
                    op: "power_on".into(),
                    mac: String::new(),
                },
                score: 100_000,
            });
        }
        // Scan row.
        results.push(SearchResult {
            kind: ResultKind::System,
            title: "Scan for devices".into(),
            subtitle: Some("Discover new Bluetooth devices".into()),
            icon: Some("system-search-symbolic".into()),
            action: Action::Bluetooth {
                op: "scan".into(),
                mac: String::new(),
            },
            score: 99_000,
        });
    } else {
        // Non-empty query: deterministic match for the power row (no fuzzy/typo)
        // so the non-usable option can never appear.
        let (power_title, power_subtitle, power_icon, power_op, power_kw): (&str, &str, &str, &str, &[&str]) =
            if snap.powered {
                ("Turn off Bluetooth", "Bluetooth is on", "bluetooth-active-symbolic", "power_off",
                 &["turn off", "off", "disable", "power off"])
            } else {
                ("Turn on Bluetooth", "Bluetooth is off", "bluetooth-disabled-symbolic", "power_on",
                 &["turn on", "on", "enable", "power on"])
            };

        if let Some(score) = deterministic_score(&ql, power_kw) {
            results.push(SearchResult {
                kind: ResultKind::System,
                title: power_title.into(),
                subtitle: Some(power_subtitle.into()),
                icon: Some(power_icon.into()),
                action: Action::Bluetooth {
                    op: power_op.into(),
                    mac: String::new(),
                },
                score,
            });
        }

        let scan_keywords = [
            "scan",
            "scan for devices",
            "discover",
            "search",
            "find devices",
            "pair new",
        ];
        if let Some(score) = match_score(&ql, &scan_keywords) {
            results.push(SearchResult {
                kind: ResultKind::System,
                title: "Scan for devices".into(),
                subtitle: Some("Discover new Bluetooth devices".into()),
                icon: Some("system-search-symbolic".into()),
                action: Action::Bluetooth {
                    op: "scan".into(),
                    mac: String::new(),
                },
                score,
            });
        }
    }

    // ── Devices ──
    let mut idx = 0;
    for (devices, connected, label, op) in [
        (&snap.connected, true, "Connected", "disconnect"),
        (&snap.paired, false, "Paired", "connect"),
        (&snap.others, false, "", "pair"),
    ] {
        for d in devices {
            let name = if d.name.is_empty() {
                d.mac.clone()
            } else {
                d.name.clone()
            };
            let score = if ql.is_empty() {
                95_000 - idx
            } else if let Some(ms) = device_score(&ql, &name, &d.mac) {
                let section_bonus = if connected {
                    3_000
                } else if label == "Paired" {
                    2_000
                } else {
                    1_000
                };
                ms + section_bonus
            } else {
                idx += 1;
                continue;
            };
            let icon = if connected {
                "bluetooth-active-symbolic"
            } else {
                "bluetooth-symbolic"
            };
            let sub = if connected {
                format!("{label} · {} — Enter to disconnect", d.mac)
            } else if label.is_empty() {
                format!("{} — Enter to pair and connect", d.mac)
            } else {
                format!("{label} · {} — Enter to connect", d.mac)
            };
            results.push(SearchResult {
                kind: ResultKind::System,
                title: name,
                subtitle: Some(sub),
                icon: Some(icon.into()),
                action: Action::Bluetooth {
                    op: op.into(),
                    mac: d.mac.clone(),
                },
                score,
            });
            idx += 1;
        }
    }

    // Fallback: nothing matched → offer scan.
    if !ql.is_empty() && results.is_empty() {
        results.push(SearchResult {
            kind: ResultKind::System,
            title: "Scan for devices".into(),
            subtitle: Some("No matching device — scan for new devices".into()),
            icon: Some("system-search-symbolic".into()),
            action: Action::Bluetooth {
                op: "scan".into(),
                mac: String::new(),
            },
            score: 60_000,
        });
    }

    results.sort_by(|a, b| b.score.cmp(&a.score));
    results
}

// ── Scoring helpers ───────────────────────────────────────────────────────

/// Deterministic exact/prefix/contains match — no fuzzy/typo tiers.
/// Used for the power row so the non-usable option can never be surfaced
/// by an accidental near-match.
fn deterministic_score(ql: &str, candidates: &[&str]) -> Option<i32> {
    if ql.is_empty() {
        return None;
    }
    for &c in candidates {
        let cl = c.to_lowercase();
        if cl == ql {
            return Some(100_000);
        }
    }
    for &c in candidates {
        let cl = c.to_lowercase();
        if cl.starts_with(ql) {
            return Some(90_000);
        }
    }
    for &c in candidates {
        let cl = c.to_lowercase();
        if cl.contains(ql) {
            return Some(80_000);
        }
    }
    None
}

fn match_score(ql: &str, candidates: &[&str]) -> Option<i32> {
    if ql.is_empty() {
        return None;
    }
    for &c in candidates {
        let cl = c.to_lowercase();
        if cl == ql {
            return Some(100_000);
        }
    }
    for &c in candidates {
        let cl = c.to_lowercase();
        if cl.starts_with(ql) {
            return Some(90_000);
        }
    }
    for &c in candidates {
        let cl = c.to_lowercase();
        if cl.contains(ql) {
            return Some(80_000);
        }
    }
    // Fuzzy match on concatenated candidates
    if ql.len() >= 2 {
        let haystack: String = candidates
            .iter()
            .map(|c| c.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        let mut matcher = Matcher::default();
        let pattern = Pattern::parse(ql, CaseMatching::Ignore, Normalization::Smart);
        if let Some(fs) =
            pattern.score(Utf32String::from(haystack.as_str()).slice(..), &mut matcher)
        {
            if fs >= (ql.len() as u32) * 20 {
                return Some(70_000);
            }
        }
    }
    // Typo match on the first candidate (keyboard-layout typo)
    if ql.len() >= 3 {
        for &c in candidates {
            let cl = c.to_lowercase();
            if let Some(ts) = crate::search::typo::keyboard_similarity(ql, &cl) {
                if ts > 700 {
                    return Some(60_000 + ts as i32);
                }
            }
        }
    }
    None
}

fn device_score(ql: &str, name: &str, mac: &str) -> Option<i32> {
    if ql.is_empty() {
        return None;
    }
    let nl = name.to_lowercase();
    let ml = mac.to_lowercase();
    if nl == ql {
        return Some(40_000);
    }
    if nl.starts_with(ql) || ml.starts_with(ql) {
        return Some(35_000);
    }
    if nl.contains(ql) || ml.contains(ql) {
        return Some(30_000);
    }
    if ql.len() >= 2 {
        let mut matcher = Matcher::default();
        let pattern = Pattern::parse(ql, CaseMatching::Ignore, Normalization::Smart);
        if let Some(fs) = pattern.score(Utf32String::from(name).slice(..), &mut matcher) {
            if fs >= (ql.len() as u32) * 15 {
                return Some(20_000);
            }
        }
        if let Some(ts) = crate::search::typo::keyboard_similarity(ql, &nl) {
            return Some(18_000 + ts as i32);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bluetoothctl_devices_output() {
        let out = "Device A0:5A:5F:31:B2:76 Wireless Controller\nDevice 84:AC:60:E8:96:F4 QCY MeloBuds Pro\n";
        let devices = parse_devices(out);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].mac, "A0:5A:5F:31:B2:76");
        assert_eq!(devices[0].name, "Wireless Controller");
        assert_eq!(devices[1].name, "QCY MeloBuds Pro");
        assert!(parse_devices("garbage\n").is_empty());
    }

    #[test]
    fn match_score_exact() {
        assert_eq!(
            match_score("scan", &["scan", "discover"]),
            Some(100_000)
        );
    }

    #[test]
    fn match_score_prefix() {
        assert_eq!(match_score("sca", &["scan for devices"]), Some(90_000));
    }

    #[test]
    fn match_score_contains() {
        assert_eq!(
            match_score("devices", &["scan for devices"]),
            Some(80_000)
        );
    }

    #[test]
    fn match_score_empty_query() {
        assert_eq!(match_score("", &["scan"]), None);
    }

    #[test]
    fn match_score_no_match() {
        assert_eq!(match_score("xyz", &["scan", "power"]), None);
    }

    #[test]
    fn device_score_exact_name() {
        assert_eq!(
            device_score("keyboard", "Keyboard", "AA:BB:CC:DD:EE:FF"),
            Some(40_000)
        );
    }

    #[test]
    fn device_score_prefix_name() {
        assert_eq!(device_score("key", "Keyboard", "AA:BB"), Some(35_000));
    }

    #[test]
    fn device_score_contains_name() {
        assert_eq!(device_score("board", "Keyboard", "AA:BB"), Some(30_000));
    }

    #[test]
    fn device_score_mac_prefix() {
        assert_eq!(
            device_score("aa:bb", "My Device", "AA:BB:CC:DD:EE:FF"),
            Some(35_000)
        );
    }

    #[test]
    fn device_score_empty_query() {
        assert_eq!(device_score("", "Keyboard", "AA:BB"), None);
    }

    #[test]
    fn deterministic_score_exact() {
        assert_eq!(deterministic_score("off", &["turn off", "off", "disable"]), Some(100_000));
    }

    #[test]
    fn deterministic_score_contains() {
        // "off" is contained in "turn off" → contains match.
        assert_eq!(deterministic_score("off", &["turn off"]), Some(80_000));
    }

    #[test]
    fn deterministic_score_prefix() {
        // "turn" is a prefix of "turn off".
        assert_eq!(deterministic_score("turn", &["turn off"]), Some(90_000));
    }

    #[test]
    fn deterministic_score_no_match_for_opposite() {
        // When ON, keywords are ["turn off", "off", "disable", "power off"];
        // typing "on" should not match.
        assert_eq!(deterministic_score("on", &["turn off", "off", "disable", "power off"]), None);
    }

    #[test]
    fn deterministic_score_empty() {
        assert_eq!(deterministic_score("", &["off"]), None);
    }

    #[test]
    fn power_on_chain_includes_rfkill_unblock() {
        assert!(POWER_ON.contains("rfkill unblock bluetooth"));
    }

    #[test]
    fn bt_command_power_on_includes_rfkill() {
        let cmd = bt_command("power_on", "").unwrap();
        assert!(cmd.contains("rfkill unblock bluetooth"));
    }

    #[test]
    fn bt_command_connect_includes_rfkill() {
        let cmd = bt_command("connect", "AA:BB:CC:DD:EE:FF").unwrap();
        assert!(cmd.contains("rfkill unblock bluetooth"));
    }

    #[test]
    fn bt_command_pair_includes_rfkill() {
        let cmd = bt_command("pair", "AA:BB:CC:DD:EE:FF").unwrap();
        assert!(cmd.contains("rfkill unblock bluetooth"));
    }

    #[test]
    fn bt_command_disconnect_no_rfkill() {
        let cmd = bt_command("disconnect", "AA:BB:CC:DD:EE:FF").unwrap();
        assert!(!cmd.contains("rfkill"));
    }

    #[test]
    fn bt_command_unknown_op() {
        assert!(bt_command("unknown_op", "").is_none());
    }
}
