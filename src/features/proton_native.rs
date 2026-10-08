//! Proton Calendar and Proton Drive inside Spotty, signed in natively.
//!
//! Spotty talks to Proton's API itself (see `spotty-proton-account`): there is
//! no web page, no browser engine and no helper program involved. You type
//! your Proton email and password into a Spotty window; the password is turned
//! into a one-time cryptographic proof, checked against Proton and then
//! forgotten (it is held in memory only while a two-factor code or mailbox
//! password is still pending). What Spotty keeps afterwards:
//!
//! - on disk (`~/.local/share/spotty/proton-account/session.json`, readable
//!   only by you): Proton's session tokens and the *key password* derived from
//!   your password, which unlocks your Proton keys. Signing out ends the
//!   session at Proton and deletes the file.
//! - in memory only: unlocked keys, decrypted Drive names and calendar events.
//!   None of that is ever written to disk, and it is dropped on sign-out.
//!
//! Every function here may block on the network: call them off the GTK thread
//! (the UI uses `gio::spawn_blocking`).

use spotty_proton_account as pa;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub use pa::{Event, Node};

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
    /// Bumped on every sign-in/out so stale background work can tell.
    generation: u64,
    drive: DriveIndex,
    calendar: CalendarCache,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default())).lock().unwrap_or_else(|e| e.into_inner())
}

pub fn available() -> bool {
    true
}

// ── Saved session ───────────────────────────────────────────────────────────

fn store_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("spotty").join("proton-account"))
}

fn session_file(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

fn write_private(dir: &Path, account: &pa::Account) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let temp = dir.join("session.json.new");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(&serde_json::to_vec(account).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&temp, session_file(dir))
}

fn read_saved(dir: &Path) -> Option<pa::Account> {
    use std::os::unix::fs::MetadataExt;
    let path = session_file(dir);
    let meta = std::fs::metadata(&path).ok()?;
    // Refuse a file anyone else could have written or read.
    if meta.mode() & 0o077 != 0 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save(account: &pa::Account) {
    if let Some(dir) = store_dir() {
        if let Err(error) = write_private(&dir, account) {
            log::warn!("couldn't save the Proton session: {error}");
        }
    }
}

fn forget_saved() {
    if let Some(dir) = store_dir() {
        let _ = std::fs::remove_file(session_file(&dir));
    }
}

/// Tokens Proton renewed: update the saved copy (keeping the key password).
fn tokens_renewed(tokens: &pa::Tokens) {
    let Some(dir) = store_dir() else { return };
    if let Some(mut account) = read_saved(&dir) {
        account.tokens = tokens.clone();
        save(&account);
    }
}

fn make_client(account: pa::Account) -> Result<Arc<pa::Client>, String> {
    pa::Client::new(account, tokens_renewed).map(Arc::new).map_err(|e| e.to_string())
}

/// The signed-in client, loading the saved session on first use.
pub fn client() -> Option<Arc<pa::Client>> {
    let mut state = state();
    if !state.loaded {
        state.loaded = true;
        if let Some(account) = store_dir().and_then(|dir| read_saved(&dir)) {
            match make_client(account) {
                Ok(client) => state.client = Some(client),
                Err(error) => log::warn!("Proton session not usable: {error}"),
            }
        }
    }
    state.client.clone()
}

pub fn signed_in() -> bool {
    client().is_some()
}

/// "Name · email" of the signed-in account.
pub fn account_label() -> String {
    client()
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
pub fn sign_in(username: &str, password: &str) -> Outcome {
    cancel_sign_in();
    let api = match api() {
        Ok(api) => api,
        Err(error) => return Outcome::Failed(error),
    };
    let result = pa::begin(&api, username, password);
    step(api, result)
}

/// Where a half-finished sign-in lives (its own session, not the saved one).
fn pending_api() -> &'static Mutex<Option<Arc<pa::Api>>> {
    static API: OnceLock<Mutex<Option<Arc<pa::Api>>>> = OnceLock::new();
    API.get_or_init(|| Mutex::new(None))
}

fn step(api: pa::Api, result: pa::Result<pa::Step>) -> Outcome {
    match result {
        Ok(pa::Step::Done(account)) => finish(account),
        Ok(pa::Step::TwoFactor(pending)) => {
            *pending_api().lock().unwrap() = Some(Arc::new(api));
            state().pending = Some(pending);
            Outcome::TwoFactor
        }
        Ok(pa::Step::MailboxPassword(pending)) => {
            *pending_api().lock().unwrap() = Some(Arc::new(api));
            state().pending = Some(pending);
            Outcome::MailboxPassword
        }
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

fn finish(account: pa::Account) -> Outcome {
    save(&account);
    match make_client(account) {
        Ok(client) => {
            let mut state = state();
            state.generation += 1;
            state.loaded = true;
            state.client = Some(client);
            state.pending = None;
            state.drive = DriveIndex::default();
            state.calendar = CalendarCache::default();
            drop(state);
            *pending_api().lock().unwrap() = None;
            Outcome::Done
        }
        Err(error) => Outcome::Failed(error),
    }
}

pub fn submit_two_factor(code: &str) -> Outcome {
    let (Some(pending), Some(api)) = (state().pending.take(), pending_api().lock().unwrap().clone()) else {
        return Outcome::Failed("The sign-in expired. Enter your password again.".into());
    };
    match pending.submit_two_factor(&api, code) {
        Ok(next) => match next {
            pa::Step::Done(account) => finish(account),
            pa::Step::MailboxPassword(pending) => {
                state().pending = Some(pending);
                Outcome::MailboxPassword
            }
            pa::Step::TwoFactor(pending) => {
                state().pending = Some(pending);
                Outcome::TwoFactor
            }
        },
        // A wrong code: keep the sign-in so another can be typed.
        Err(pa::Retry::Again(error, pending)) => {
            state().pending = Some(pending);
            Outcome::Failed(error.to_string())
        }
        Err(pa::Retry::Failed(error)) => {
            *pending_api().lock().unwrap() = None;
            Outcome::Failed(error.to_string())
        }
    }
}

pub fn submit_mailbox_password(password: &str) -> Outcome {
    let (Some(pending), Some(api)) = (state().pending.take(), pending_api().lock().unwrap().clone()) else {
        return Outcome::Failed("The sign-in expired. Enter your password again.".into());
    };
    match pending.submit_mailbox_password(&api, password) {
        Ok(pa::Step::Done(account)) => finish(account),
        Ok(_) => Outcome::Failed("Proton asked for another step Spotty can't do".into()),
        Err(error) => {
            *pending_api().lock().unwrap() = None;
            Outcome::Failed(error.to_string())
        }
    }
}

/// Drop a half-finished sign-in and end its session at Proton.
pub fn cancel_sign_in() {
    let pending = state().pending.take();
    let api = pending_api().lock().unwrap().take();
    if let (Some(pending), Some(api)) = (pending, api) {
        pending.cancel(&api);
    }
}

/// End the session at Proton and forget everything on this computer.
pub fn sign_out() {
    cancel_sign_in();
    let client = {
        let mut state = state();
        state.generation += 1;
        state.loaded = true;
        state.drive = DriveIndex::default();
        state.calendar = CalendarCache::default();
        state.client.take()
    };
    forget_saved();
    if let Some(client) = client {
        client.sign_out();
    }
}

/// Proton ended the session on its side.
fn session_lost(error: &pa::Error) {
    if error.is_signed_out() {
        let mut state = state();
        state.client = None;
        state.generation += 1;
        drop(state);
        forget_saved();
    }
}

fn describe(error: pa::Error) -> String {
    session_lost(&error);
    if error.is_signed_out() { "Your Proton session ended. Sign in again in Settings.".into() } else { error.to_string() }
}

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
    if let Some(root) = state().drive.root.clone() {
        return Ok(root);
    }
    let client = client().ok_or("Sign in to Proton first")?;
    let root = client.drive_root().map_err(describe)?;
    state().drive.root = Some(root.clone());
    Ok(root)
}

pub fn drive_children(folder: &Node) -> Result<Vec<Node>, String> {
    let client = client().ok_or("Sign in to Proton first")?;
    let children = client.drive_children(folder).map_err(describe)?;
    let mut state = state();
    for node in &children {
        state.drive.by_id.entry(node.id.clone()).or_insert_with(|| node.clone());
    }
    Ok(children)
}

/// Download into the user's Downloads folder; returns the saved path.
pub fn drive_download(file: &Node) -> Result<PathBuf, String> {
    let client = client().ok_or("Sign in to Proton first")?;
    let dir = dirs::download_dir().or_else(dirs::home_dir).ok_or("No Downloads folder")?;
    client.drive_download(file, &dir, &mut |_| {}).map_err(describe)
}

/// Start (or refresh, when old) the in-memory name index of "My files".
/// Cheap to call on every keystroke.
pub fn drive_ensure_index() {
    let generation = {
        let mut state = state();
        if state.client.is_none() && !state.loaded {
            drop(state);
            let _ = client();
            state = self::state();
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
            let client = client().ok_or("Sign in to Proton first")?;
            let root = drive_root()?;
            let mut count = 0usize;
            client
                .drive_walk(&root, INDEX_LIMIT, &mut |nodes| {
                    let mut state = state();
                    if state.generation != generation {
                        return false;
                    }
                    for node in nodes {
                        state.drive.by_id.insert(node.id.clone(), node.clone());
                    }
                    count += nodes.len();
                    true
                })
                .map_err(describe)
        })();
        let mut state = state();
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
    let state = state();
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
    let state = state();
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
    let state = state();
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
    let client = client().ok_or("Sign in to Proton first")?;
    client.calendar_events(from, to).map_err(describe)
}

/// Load (or refresh) the events around today in the background, for search.
pub fn calendar_ensure_cache(now: i64, local_midnight: i64) {
    let generation = {
        let mut state = state();
        if state.client.is_none() && !state.loaded {
            drop(state);
            let _ = client();
            state = self::state();
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
        let mut state = state();
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
    let state = state();
    let cache = &state.calendar;
    let events = cache.events.iter().filter(|e| e.end > from && e.start < to).cloned().collect();
    (cache.progress, events, cache.error.clone())
}

/// Whether `[from, to)` lies inside what the cache covers.
pub fn calendar_covers(from: i64, to: i64) -> bool {
    let state = state();
    state.calendar.progress == Progress::Ready && state.calendar.from <= from && to <= state.calendar.to
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn account() -> pa::Account {
        pa::Account {
            tokens: pa::Tokens { uid: "u".into(), access: "a".into(), refresh: "r".into() },
            key_password: "kp".into(),
            email: "sam@proton.test".into(),
            name: "Sam".into(),
        }
    }

    #[test]
    fn saved_session_is_private_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("spotty-native-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_private(&dir, &account()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&session_file(&dir)), 0o600);
        assert_eq!(read_saved(&dir).unwrap().email, "sam@proton.test");

        // A session file others can read is not trusted.
        std::fs::set_permissions(session_file(&dir), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_saved(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
