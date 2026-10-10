//! Proton Calendar and Proton Drive inside Spotty, each with its own sign-in.
//!
//! Spotty talks to Proton's API itself (see `spotty-proton-account`): there is
//! no web page, no browser engine and no helper program involved. You type
//! your Proton email and password into a Spotty window; the password is turned
//! into a one-time cryptographic proof, checked against Proton and then
//! forgotten (it is held in memory only while a two-factor code or mailbox
//! password is still pending).
//!
//! Calendar and Drive are separate sign-ins: each has its own saved session,
//! its own in-memory client and its own caches. Signing out of one never signs
//! out of the other, and neither session is ever shared with another app.
//! What Spotty keeps afterwards, per app:
//!
//! - in `~/.local/share/spotty/proton-calendar/session.json` or
//!   `~/.local/share/spotty/proton-drive/session.json` (folder 0700, file 0600,
//!   readable only by you): that app's session tokens, its account's name and
//!   email. No password of any kind.
//! - in the desktop keyring (Secret Service): that app's *key password*, the
//!   value that unlocks your Proton keys. It is never written to a file.
//!   Sign-out deletes it.
//! - in memory only: unlocked keys, decrypted Drive names and calendar events.
//!   None of that is ever written to disk, and it is dropped on sign-out.
//!
//! Every function here may block on the network or on the keyring: call them
//! off the GTK thread (the UI uses `gio::spawn_blocking`). The only call the UI
//! makes on the GTK thread, `signed_in`, never blocks.

use spotty_proton_account as pa;
use spotty_proton_account::KeyStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub use pa::{Event, Node};

/// The Proton apps that have a native sign-in of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Calendar,
    Drive,
}

impl Service {
    /// The folder under `~/.local/share/spotty/` that holds this app's session.
    fn folder(self) -> &'static str {
        match self {
            Service::Calendar => "proton-calendar",
            Service::Drive => "proton-drive",
        }
    }

    /// The app's name for log lines.
    fn name(self) -> &'static str {
        match self {
            Service::Calendar => "Proton Calendar",
            Service::Drive => "Proton Drive",
        }
    }
}

const DRIVE: Service = Service::Drive;
const CALENDAR: Service = Service::Calendar;

/// What to show after a sign-in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Ask for the authenticator code.
    TwoFactor,
    /// Ask for the second (mailbox) password.
    MailboxPassword,
    Failed(String),
}

#[derive(Default)]
struct State {
    client: Option<Arc<pa::Client>>,
    /// Loaded the saved session from disk already.
    loaded: bool,
    pending: Option<pa::Pending>,
    /// The half-finished sign-in's own session (not the saved one).
    pending_api: Option<Arc<pa::Api>>,
    /// Bumped on every sign-in/out so stale background work can tell.
    generation: u64,
    drive: DriveIndex,
    calendar: CalendarCache,
}

fn state(service: Service) -> MutexGuard<'static, State> {
    static CALENDAR_STATE: OnceLock<Mutex<State>> = OnceLock::new();
    static DRIVE_STATE: OnceLock<Mutex<State>> = OnceLock::new();
    let cell = match service {
        Service::Calendar => &CALENDAR_STATE,
        Service::Drive => &DRIVE_STATE,
    };
    cell.get_or_init(|| Mutex::new(State::default())).lock().unwrap_or_else(|e| e.into_inner())
}

pub fn available() -> bool {
    true
}

// ── Saved session ───────────────────────────────────────────────────────────

/// The keyring name of an app's key password.
fn key_name(service: Service) -> &'static str {
    service.folder()
}

fn store_dir(service: Service) -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("spotty").join(service.folder()))
}

fn session_file(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

/// The keyring that holds the key passwords.
static KEYS: spotty_proton_account::SecretService = spotty_proton_account::SecretService;

/// What a session file holds. `key_password` is read only so that an old file
/// (from before the keyring was used) can be migrated; it is never written.
#[derive(serde::Deserialize)]
struct Stored {
    tokens: pa::Tokens,
    email: String,
    name: String,
    #[serde(default)]
    key_password: String,
}

fn write_private(dir: &Path, saved: &pa::SavedSession) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let temp = dir.join("session.json.new");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(&serde_json::to_vec(saved).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&temp, session_file(dir))
}

/// The session file's bytes, when the file exists and only you can read it.
/// A file others can read is not trusted and is deleted: it may hold a key
/// password from an older version.
fn read_private(dir: &Path) -> Option<spotty_proton_account::Zeroizing<Vec<u8>>> {
    use std::os::unix::fs::MetadataExt;
    let path = session_file(dir);
    let meta = std::fs::metadata(&path).ok()?;
    if meta.mode() & 0o077 != 0 {
        log::warn!("ignoring a session file other users can read; removing it");
        let _ = std::fs::remove_file(&path);
        return None;
    }
    std::fs::read(path).ok().map(spotty_proton_account::Zeroizing::new)
}

fn forget_file(dir: &Path) {
    let _ = std::fs::remove_file(session_file(dir));
}

/// End a session that Spotty can't keep (no keyring, or its key is gone): the
/// tokens are revoked at Proton in the background.
fn revoke_in_background(tokens: pa::Tokens) {
    std::thread::spawn(move || {
        if let Ok(api) = pa::Api::new() {
            pa::revoke(&api, &tokens);
        }
    });
}

/// Load an app's saved session. The account is usable only when its key
/// password is in the keyring. An old file that still holds the key password
/// has it moved to the keyring and is rewritten without it.
fn load_saved(dir: &Path, name: &str, keys: &dyn KeyStore) -> Option<pa::Account> {
    let bytes = read_private(dir)?;
    let mut stored: Stored = serde_json::from_slice(&bytes).ok()?;
    drop(bytes);
    let legacy_key = spotty_proton_account::Zeroizing::new(std::mem::take(&mut stored.key_password));
    let saved = pa::SavedSession { tokens: stored.tokens.clone(), email: stored.email, name: stored.name };
    if !legacy_key.is_empty() {
        return match keys.save(name, &legacy_key) {
            Ok(()) => {
                if write_private(dir, &saved).is_err() {
                    // Never leave the key password in the file.
                    forget_file(dir);
                }
                Some(pa::Account::from_saved(saved, legacy_key))
            }
            Err(error) => {
                log::warn!("couldn't move a saved sign-in to the keyring, so it is ended: {error}");
                forget_file(dir);
                revoke_in_background(stored.tokens);
                None
            }
        };
    }
    match keys.load(name) {
        Ok(Some(key)) => Some(pa::Account::from_saved(saved, key)),
        Ok(None) => {
            // The keyring has no key for this session, so it can't unlock anything.
            forget_file(dir);
            revoke_in_background(stored.tokens);
            None
        }
        // Keep the file: a keyring that is only locked for now may open later.
        Err(error) => {
            log::warn!("the keyring couldn't return a saved sign-in: {error}");
            None
        }
    }
}

/// Save a new sign-in. Fails, and keeps nothing, when the keyring refuses.
fn save_account(dir: &Path, name: &str, keys: &dyn KeyStore, account: &pa::Account) -> Result<(), String> {
    keys.save(name, &account.key()).map_err(|e| e.to_string())?;
    if let Err(error) = write_private(dir, &account.saved()) {
        let _ = keys.forget(name);
        return Err(format!("Spotty couldn't write its session file: {error}"));
    }
    Ok(())
}

/// Forget an app's saved session on this computer: its file and its key password.
fn forget_saved_with(dir: &Path, name: &str, keys: &dyn KeyStore) {
    forget_file(dir);
    if let Err(error) = keys.forget(name) {
        log::warn!("couldn't remove a key password from the keyring: {error}");
    }
}

/// Tokens the app renewed: update the saved file (it never holds a key password).
fn tokens_renewed(service: Service, tokens: &pa::Tokens) {
    let Some(dir) = store_dir(service) else { return };
    let Some(bytes) = read_private(&dir) else { return };
    let Ok(stored) = serde_json::from_slice::<Stored>(&bytes) else { return };
    let saved = pa::SavedSession { tokens: tokens.clone(), email: stored.email, name: stored.name };
    if let Err(error) = write_private(&dir, &saved) {
        log::warn!("couldn't save the {} session: {error}", service.name());
    }
}

fn make_client(service: Service, account: pa::Account) -> Result<Arc<pa::Client>, String> {
    pa::Client::new(account, move |tokens| tokens_renewed(service, tokens)).map(Arc::new).map_err(|e| e.to_string())
}

/// Serialises loading an app's saved session. The keyring may prompt or be
/// slow, so loading never holds the state lock.
fn loading_lock(service: Service) -> MutexGuard<'static, ()> {
    static CALENDAR_LOADING: Mutex<()> = Mutex::new(());
    static DRIVE_LOADING: Mutex<()> = Mutex::new(());
    let cell = match service {
        Service::Calendar => &CALENDAR_LOADING,
        Service::Drive => &DRIVE_LOADING,
    };
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

/// The app's signed-in client, loading its saved session on first use. This
/// may read the keyring: call it off the GTK thread.
pub fn client(service: Service) -> Option<Arc<pa::Client>> {
    let _loading = loading_lock(service);
    {
        let state = state(service);
        if state.loaded {
            return state.client.clone();
        }
    }
    let client = store_dir(service)
        .and_then(|dir| load_saved(&dir, key_name(service), &KEYS))
        .and_then(|account| match make_client(service, account) {
            Ok(client) => Some(client),
            Err(error) => {
                log::warn!("{} session not usable: {error}", service.name());
                None
            }
        });
    let mut state = state(service);
    state.loaded = true;
    state.client = client.clone();
    client
}

/// Forget this app's saved session on this computer: its file and key password.
fn forget_saved(service: Service) {
    if let Some(dir) = store_dir(service) {
        forget_saved_with(&dir, key_name(service), &KEYS);
    }
}

/// Start loading the saved session on a worker thread, if it isn't loaded yet.
fn load_in_background(service: Service) {
    if !state(service).loaded {
        std::thread::spawn(move || {
            let _ = client(service);
        });
    }
}

/// Whether the app is signed in. Never blocks: until the saved session has
/// loaded (on a worker thread) the app counts as signed out.
pub fn signed_in(service: Service) -> bool {
    let state = state(service);
    if state.loaded {
        return state.client.is_some();
    }
    drop(state);
    load_in_background(service);
    false
}

/// Whether the saved session has been looked at (it loads on a worker thread).
pub fn loaded(service: Service) -> bool {
    state(service).loaded
}

/// "Name · email" of the account signed in to this app. Reads the loaded
/// session only; empty until it has loaded.
pub fn account_label(service: Service) -> String {
    let client = state(service).client.clone();
    client
        .map(|c| if c.name.is_empty() || c.name == c.email { c.email.clone() } else { format!("{} · {}", c.name, c.email) })
        .unwrap_or_default()
}

// ── Signing in and out ──────────────────────────────────────────────────────

fn api() -> Result<pa::Api, String> {
    pa::Api::new().map_err(|e| e.to_string())
}

/// Check the password with Proton. The password isn't kept anywhere beyond
/// the one second-step (the two-factor code) that needs it, and is zeroed
/// when that step is over.
pub fn sign_in(service: Service, username: &str, password: &str) -> Outcome {
    cancel_sign_in(service);
    let api = match api() {
        Ok(api) => api,
        Err(error) => return Outcome::Failed(error),
    };
    let result = pa::begin(&api, username, password);
    step(service, api, result)
}

fn step(service: Service, api: pa::Api, result: pa::Result<pa::Step>) -> Outcome {
    match result {
        Ok(pa::Step::Done(account)) => finish(service, account),
        Ok(pa::Step::TwoFactor(pending)) => {
            let mut state = state(service);
            state.pending_api = Some(Arc::new(api));
            state.pending = Some(pending);
            Outcome::TwoFactor
        }
        Ok(pa::Step::MailboxPassword(pending)) => {
            let mut state = state(service);
            state.pending_api = Some(Arc::new(api));
            state.pending = Some(pending);
            Outcome::MailboxPassword
        }
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

/// Message for a sign-in the keyring refused to keep.
const KEYRING_REFUSED: &str = "Spotty couldn't keep this sign-in in your desktop keyring, so it can't stay signed in. Start your keyring (GNOME's keyring, for example) and sign in again.";

fn finish(service: Service, account: pa::Account) -> Outcome {
    let Some(dir) = store_dir(service) else {
        return Outcome::Failed("Spotty has no data folder to keep the sign-in in".into());
    };
    if let Err(error) = save_account(&dir, key_name(service), &KEYS, &account) {
        log::warn!("{}: {error}", service.name());
        revoke_in_background(account.tokens.clone());
        return Outcome::Failed(KEYRING_REFUSED.into());
    }
    match make_client(service, account) {
        Ok(client) => {
            let mut state = state(service);
            state.generation += 1;
            state.loaded = true;
            state.client = Some(client);
            state.pending = None;
            state.pending_api = None;
            state.drive = DriveIndex::default();
            state.calendar = CalendarCache::default();
            Outcome::Done
        }
        Err(error) => Outcome::Failed(error),
    }
}

pub fn submit_two_factor(service: Service, code: &str) -> Outcome {
    let (pending, api) = {
        let mut state = state(service);
        (state.pending.take(), state.pending_api.clone())
    };
    let (Some(pending), Some(api)) = (pending, api) else {
        return Outcome::Failed("The sign-in expired. Enter your password again.".into());
    };
    match pending.submit_two_factor(&api, code) {
        Ok(next) => match next {
            pa::Step::Done(account) => finish(service, account),
            pa::Step::MailboxPassword(pending) => {
                state(service).pending = Some(pending);
                Outcome::MailboxPassword
            }
            pa::Step::TwoFactor(pending) => {
                state(service).pending = Some(pending);
                Outcome::TwoFactor
            }
        },
        // A wrong code: keep the sign-in so another can be typed.
        Err(pa::Retry::Again(error, pending)) => {
            state(service).pending = Some(pending);
            Outcome::Failed(error.to_string())
        }
        Err(pa::Retry::Failed(error)) => {
            state(service).pending_api = None;
            Outcome::Failed(error.to_string())
        }
    }
}

pub fn submit_mailbox_password(service: Service, password: &str) -> Outcome {
    let (pending, api) = {
        let mut state = state(service);
        (state.pending.take(), state.pending_api.clone())
    };
    let (Some(pending), Some(api)) = (pending, api) else {
        return Outcome::Failed("The sign-in expired. Enter your password again.".into());
    };
    match pending.submit_mailbox_password(&api, password) {
        Ok(pa::Step::Done(account)) => finish(service, account),
        Ok(_) => Outcome::Failed("Proton asked for another step Spotty can't do".into()),
        Err(error) => {
            state(service).pending_api = None;
            Outcome::Failed(error.to_string())
        }
    }
}

/// Drop a half-finished sign-in and end its session at Proton.
pub fn cancel_sign_in(service: Service) {
    let (pending, api) = {
        let mut state = state(service);
        (state.pending.take(), state.pending_api.take())
    };
    if let (Some(pending), Some(api)) = (pending, api) {
        pending.cancel(&api);
    }
}

/// End this app's session at Proton and forget everything on this computer:
/// its session file and its key password in the keyring. The other Proton apps
/// keep their own sign-ins.
pub fn sign_out(service: Service) {
    cancel_sign_in(service);
    let client = {
        let mut state = state(service);
        state.generation += 1;
        state.loaded = true;
        state.drive = DriveIndex::default();
        state.calendar = CalendarCache::default();
        state.client.take()
    };
    forget_saved(service);
    if let Some(client) = client {
        client.sign_out();
    }
}

/// Proton ended this app's session on its side.
fn session_lost(service: Service, error: &pa::Error) {
    if error.is_signed_out() {
        let mut state = state(service);
        state.client = None;
        state.generation += 1;
        drop(state);
        forget_saved(service);
    }
}

fn describe(service: Service, error: pa::Error) -> String {
    session_lost(service, &error);
    if error.is_signed_out() { "Your Proton session ended. Sign in again in Settings.".into() } else { error.to_string() }
}

const NOT_SIGNED_IN: &str = "Sign in to Proton first";

// ── Drive ───────────────────────────────────────────────────────────────────

#[derive(Default, PartialEq, Eq, Clone, Copy, Debug)]
pub enum Progress {
    #[default]
    Idle,
    Working,
    Ready,
    Failed,
}

#[derive(Default)]
struct DriveIndex {
    progress: Progress,
    root: Option<Node>,
    by_id: HashMap<String, Node>,
    built: Option<Instant>,
    error: String,
}

const INDEX_LIMIT: usize = 20_000;
const INDEX_TTL: Duration = Duration::from_secs(15 * 60);

pub fn drive_root() -> Result<Node, String> {
    if let Some(root) = state(DRIVE).drive.root.clone() {
        return Ok(root);
    }
    let client = client(DRIVE).ok_or(NOT_SIGNED_IN)?;
    let root = client.drive_root().map_err(|e| describe(DRIVE, e))?;
    state(DRIVE).drive.root = Some(root.clone());
    Ok(root)
}

pub fn drive_children(folder: &Node) -> Result<Vec<Node>, String> {
    let client = client(DRIVE).ok_or(NOT_SIGNED_IN)?;
    let children = client.drive_children(folder).map_err(|e| describe(DRIVE, e))?;
    let mut state = state(DRIVE);
    for node in &children {
        state.drive.by_id.entry(node.id.clone()).or_insert_with(|| node.clone());
    }
    Ok(children)
}

/// Download into the user's Downloads folder; returns the saved path.
pub fn drive_download(file: &Node) -> Result<PathBuf, String> {
    let client = client(DRIVE).ok_or(NOT_SIGNED_IN)?;
    let dir = dirs::download_dir().or_else(dirs::home_dir).ok_or("No Downloads folder")?;
    client.drive_download(file, &dir, &mut |_| {}).map_err(|e| describe(DRIVE, e))
}

/// Start (or refresh, when old) the in-memory name index of "My files".
/// Cheap to call on every keystroke.
pub fn drive_ensure_index() {
    let generation = {
        let mut state = state(DRIVE);
        if !state.loaded {
            drop(state);
            load_in_background(DRIVE);
            return;
        }
        let fresh = state.drive.built.is_some_and(|at| at.elapsed() < INDEX_TTL);
        if state.client.is_none() || state.drive.progress == Progress::Working || (fresh && state.drive.progress == Progress::Ready) {
            return;
        }
        if state.drive.progress == Progress::Failed && state.drive.built.is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        state.drive.progress = Progress::Working;
        state.generation
    };
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let client = client(DRIVE).ok_or(NOT_SIGNED_IN)?;
            let root = drive_root()?;
            client
                .drive_walk(&root, INDEX_LIMIT, &mut |nodes| {
                    let mut state = state(DRIVE);
                    if state.generation != generation {
                        return false;
                    }
                    for node in nodes {
                        state.drive.by_id.insert(node.id.clone(), node.clone());
                    }
                    true
                })
                .map_err(|e| describe(DRIVE, e))
        })();
        let mut state = state(DRIVE);
        if state.generation != generation {
            return;
        }
        state.drive.built = Some(Instant::now());
        match result {
            Ok(()) => state.drive.progress = Progress::Ready,
            Err(error) => {
                state.drive.progress = Progress::Failed;
                state.drive.error = error;
            }
        }
        drop(state);
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

pub fn drive_progress() -> (Progress, usize, String) {
    let state = state(DRIVE);
    (state.drive.progress, state.drive.by_id.len(), state.drive.error.clone())
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub node: Node,
    /// "Folder/Sub" the node sits in.
    pub location: String,
}

/// Indexed names containing `query`, best first.
pub fn drive_search(query: &str, limit: usize) -> Vec<Hit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let state = state(DRIVE);
    let mut scored: Vec<(i32, &Node)> = state
        .drive
        .by_id
        .values()
        .filter_map(|node| {
            let name = node.name.to_lowercase();
            let pos = name.find(&needle)?;
            let score = if name == needle { 3 } else if pos == 0 { 2 } else { 1 };
            Some((score, node))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase())));
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(_, node)| Hit { node: node.clone(), location: location_of(&state.drive.by_id, state.drive.root.as_ref(), node) })
        .collect()
}

fn location_of(by_id: &HashMap<String, Node>, root: Option<&Node>, node: &Node) -> String {
    let mut parts = Vec::new();
    let mut at = node.parent.clone();
    for _ in 0..32 {
        let Some(id) = at else { break };
        if root.is_some_and(|r| r.id == id) {
            break;
        }
        let Some(parent) = by_id.get(&id) else { break };
        parts.push(parent.name.clone());
        at = parent.parent.clone();
    }
    parts.reverse();
    if parts.is_empty() { "My files".to_owned() } else { format!("My files / {}", parts.join(" / ")) }
}

/// A node seen earlier (from the index or a listing).
pub fn drive_node(id: &str) -> Option<Node> {
    let state = state(DRIVE);
    state.drive.by_id.get(id).cloned().or_else(|| state.drive.root.as_ref().filter(|r| r.id == id).cloned())
}

// ── Calendar ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct CalendarCache {
    progress: Progress,
    from: i64,
    to: i64,
    events: Vec<Event>,
    fetched: Option<Instant>,
    error: String,
}

const CALENDAR_TTL: Duration = Duration::from_secs(3 * 60);
/// How far the search cache reaches around today.
pub const CACHE_DAYS_BACK: i64 = 1;
pub const CACHE_DAYS_AHEAD: i64 = 60;

pub fn day_start(now: i64) -> i64 {
    now - now.rem_euclid(86_400)
}

/// Events overlapping `[from, to)`. Blocks on the network.
pub fn calendar_events(from: i64, to: i64) -> Result<Vec<Event>, String> {
    let client = client(CALENDAR).ok_or(NOT_SIGNED_IN)?;
    client.calendar_events(from, to).map_err(|e| describe(CALENDAR, e))
}

/// Load (or refresh) the events around today in the background, for search.
pub fn calendar_ensure_cache(now: i64, local_midnight: i64) {
    let generation = {
        let mut state = state(CALENDAR);
        if !state.loaded {
            drop(state);
            load_in_background(CALENDAR);
            return;
        }
        let cache = &state.calendar;
        let fresh = cache.fetched.is_some_and(|at| at.elapsed() < CALENDAR_TTL) && cache.from <= local_midnight;
        if state.client.is_none() || cache.progress == Progress::Working || (fresh && cache.progress == Progress::Ready) {
            return;
        }
        if cache.progress == Progress::Failed && cache.fetched.is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        state.calendar.progress = Progress::Working;
        state.generation
    };
    let _ = now;
    let (from, to) = (local_midnight - CACHE_DAYS_BACK * 86_400, local_midnight + CACHE_DAYS_AHEAD * 86_400);
    std::thread::spawn(move || {
        let result = calendar_events(from, to);
        let mut state = state(CALENDAR);
        if state.generation != generation {
            return;
        }
        state.calendar.fetched = Some(Instant::now());
        state.calendar.from = from;
        state.calendar.to = to;
        match result {
            Ok(events) => {
                state.calendar.events = events;
                state.calendar.progress = Progress::Ready;
            }
            Err(error) => {
                state.calendar.progress = Progress::Failed;
                state.calendar.error = error;
            }
        }
        drop(state);
        glib::MainContext::default().invoke(crate::app::refresh_search_window);
    });
}

/// Cached events overlapping `[from, to)`, plus the cache's state.
pub fn calendar_cached(from: i64, to: i64) -> (Progress, Vec<Event>, String) {
    let state = state(CALENDAR);
    let cache = &state.calendar;
    let events = cache.events.iter().filter(|e| e.end > from && e.start < to).cloned().collect();
    (cache.progress, events, cache.error.clone())
}

/// Whether `[from, to)` lies inside what the cache covers.
pub fn calendar_covers(from: i64, to: i64) -> bool {
    let state = state(CALENDAR);
    state.calendar.progress == Progress::Ready && state.calendar.from <= from && to <= state.calendar.to
}

#[cfg(test)]
mod tests {
    use super::*;
    use spotty_proton_account::{MemoryKeyStore, UnavailableKeyStore};
    use std::os::unix::fs::PermissionsExt;

    const KEY: &str = "key-password-secret";

    fn account() -> pa::Account {
        pa::Account {
            tokens: pa::Tokens { uid: "u".into(), access: "a".into(), refresh: "r".into() },
            key_password: KEY.into(),
            email: "sam@proton.test".into(),
            name: "Sam".into(),
        }
    }

    /// A fresh folder for one test (tests run in parallel).
    fn folder(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spotty-native-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn file_text(dir: &Path) -> String {
        std::fs::read_to_string(session_file(dir)).unwrap()
    }

    #[test]
    fn saved_session_is_private_and_round_trips() {
        let dir = folder("round-trip");
        let keys = MemoryKeyStore::default();
        save_account(&dir, "proton-calendar", &keys, &account()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&session_file(&dir)), 0o600);
        let loaded = load_saved(&dir, "proton-calendar", &keys).expect("the session loads");
        assert_eq!(loaded.email, "sam@proton.test");
        assert_eq!(loaded.key_password, KEY, "the key password comes back from the keyring");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_session_file_never_holds_the_key_password() {
        let dir = folder("no-key-in-file");
        let keys = MemoryKeyStore::default();
        save_account(&dir, "proton-drive", &keys, &account()).unwrap();
        let text = file_text(&dir);
        assert!(!text.contains(KEY), "the key password must not reach the file");
        assert!(!text.contains("key_password"), "the file has no key_password field");
        assert!(text.contains("sam@proton.test") && text.contains("\"uid\""), "tokens and email stay in the file");
        assert_eq!(keys.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_others_can_read_is_not_trusted_and_is_removed() {
        let dir = folder("open-file");
        let keys = MemoryKeyStore::default();
        save_account(&dir, "proton-calendar", &keys, &account()).unwrap();
        std::fs::set_permissions(session_file(&dir), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_saved(&dir, "proton-calendar", &keys).is_none());
        assert!(!session_file(&dir).exists(), "the untrusted file is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_session_with_a_key_password_is_moved_to_the_keyring() {
        let dir = folder("migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let old = serde_json::json!({
            "tokens": {"uid": "u", "access": "a", "refresh": "r"},
            "key_password": KEY,
            "email": "sam@proton.test",
            "name": "Sam",
        });
        std::fs::write(session_file(&dir), old.to_string()).unwrap();
        std::fs::set_permissions(session_file(&dir), std::fs::Permissions::from_mode(0o600)).unwrap();
        let keys = MemoryKeyStore::default();

        let loaded = load_saved(&dir, "proton-drive", &keys).expect("the old session still signs in");
        assert_eq!(loaded.key_password, KEY);
        assert_eq!(keys.load("proton-drive").unwrap().unwrap().as_str(), KEY, "the key is now in the keyring");
        let text = file_text(&dir);
        assert!(!text.contains(KEY) && !text.contains("key_password"), "the file is rewritten without it");
        assert!(text.contains("\"uid\""), "the tokens are kept");

        // A second start reads the keyring, not the file.
        let again = load_saved(&dir, "proton-drive", &keys).expect("signs in again");
        assert_eq!(again.key_password, KEY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_session_is_ended_when_the_keyring_refuses_the_key() {
        let dir = folder("migrate-refused");
        std::fs::create_dir_all(&dir).unwrap();
        let old = serde_json::json!({"tokens": {"uid": "u", "access": "a", "refresh": "r"}, "key_password": KEY, "email": "e", "name": "n"});
        std::fs::write(session_file(&dir), old.to_string()).unwrap();
        std::fs::set_permissions(session_file(&dir), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_saved(&dir, "proton-drive", &UnavailableKeyStore).is_none());
        assert!(!session_file(&dir).exists(), "the key password must not stay in a file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_without_its_keyring_entry_is_signed_out_and_removed() {
        let dir = folder("no-entry");
        save_account(&dir, "proton-calendar", &MemoryKeyStore::default(), &account()).unwrap();
        let empty = MemoryKeyStore::default();
        assert!(load_saved(&dir, "proton-calendar", &empty).is_none());
        assert!(!session_file(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_locked_or_missing_keyring_at_start_keeps_the_file_for_later() {
        let dir = folder("keyring-down");
        save_account(&dir, "proton-calendar", &MemoryKeyStore::default(), &account()).unwrap();
        assert!(load_saved(&dir, "proton-calendar", &UnavailableKeyStore).is_none(), "not signed in for now");
        assert!(session_file(&dir).exists(), "the saved session is kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sign_in_the_keyring_refuses_is_not_kept_at_all() {
        let dir = folder("refused");
        assert!(save_account(&dir, "proton-pass", &UnavailableKeyStore, &account()).is_err());
        assert!(!session_file(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_out_removes_the_file_and_the_key_password() {
        let dir = folder("sign-out");
        let keys = MemoryKeyStore::default();
        save_account(&dir, "proton-calendar", &keys, &account()).unwrap();
        save_account(&dir, "proton-drive", &keys, &account()).unwrap();
        forget_saved_with(&dir, "proton-calendar", &keys);
        assert!(!session_file(&dir).exists());
        assert!(keys.load("proton-calendar").unwrap().is_none(), "this app's key is gone");
        assert!(keys.load("proton-drive").unwrap().is_some(), "the other app's key stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renewed_token_keeps_the_file_free_of_the_key() {
        let dir = folder("renew");
        let keys = MemoryKeyStore::default();
        save_account(&dir, "proton-calendar", &keys, &account()).unwrap();
        let bytes = read_private(&dir).unwrap();
        let mut stored: Stored = serde_json::from_slice(&bytes).unwrap();
        stored.tokens.access = "renewed".into();
        let saved = pa::SavedSession { tokens: stored.tokens, email: stored.email, name: stored.name };
        write_private(&dir, &saved).unwrap();
        let text = file_text(&dir);
        assert!(text.contains("renewed") && !text.contains(KEY));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_app_keeps_its_session_in_its_own_folder() {
        let calendar = store_dir(Service::Calendar).unwrap();
        let drive = store_dir(Service::Drive).unwrap();
        assert!(calendar.ends_with("spotty/proton-calendar"));
        assert!(drive.ends_with("spotty/proton-drive"));
        assert_ne!(calendar, drive);
        assert!(!calendar.ends_with("proton-account"), "the old shared folder is not used");
        assert_ne!(key_name(Service::Calendar), key_name(Service::Drive), "each app has its own keyring entry");
    }

    #[test]
    fn locations_follow_parents_up_to_my_files() {
        let node = |id: &str, parent: Option<&str>, name: &str| Node::for_tests(id, parent, name);
        let root = node("root", None, "My files");
        let mut by_id = HashMap::new();
        for n in [node("a", Some("root"), "Tax"), node("b", Some("a"), "2026"), node("c", Some("b"), "invoice.pdf")] {
            by_id.insert(n.id.clone(), n);
        }
        let file = by_id["c"].clone();
        assert_eq!(location_of(&by_id, Some(&root), &file), "My files / Tax / 2026");
        assert_eq!(location_of(&by_id, Some(&root), &by_id["a"].clone()), "My files");
    }

    #[test]
    fn day_start_rounds_to_utc_midnight() {
        assert_eq!(day_start(1_781_085_600), 1_781_049_600);
    }
}
