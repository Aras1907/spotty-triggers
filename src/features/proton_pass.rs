//! Proton Pass inside Spotty. Proton's official Pass client (`pass-cli`,
//! embedded by `trigger-backends/proton-pass-embedded`) runs as a short-lived
//! child process; nothing has to be installed separately.
//!
//! Privacy and security rules this module keeps:
//! - The vault listing Spotty keeps in memory is Proton's *summary*: item
//!   titles, types and ids. Usernames, passwords, one-time codes and notes are
//!   fetched only when you pick that action, are never stored, and are wiped
//!   from Spotty's buffers right after they reach the clipboard.
//! - The clipboard copy carries `x-kde-passwordManagerHint`, so Spotty's own
//!   clipboard history (and other managers that honour it) skip it, and Spotty
//!   clears it again after [`CLEAR_CLIPBOARD_SECS`] if it is still there.
//! - Sign-in uses Spotty's Proton account: the client asks for a session fork
//!   and Spotty approves it natively (`crate::proton_session`), so no password
//!   is typed here and no web page opens.
//! - The client runs with a cleared environment, its own session folder and
//!   key in the desktop keyring, and with its update check and telemetry off.
//!
//! Every call that talks to the client blocks: run them off the GTK thread.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Seconds before a copied secret is removed from the clipboard again.
pub const CLEAR_CLIPBOARD_SECS: u32 = 30;
const REFRESH_AFTER: Duration = Duration::from_secs(180);
const RETRY_AFTER: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_secs(30);
const SIGN_IN_WAIT: Duration = Duration::from_secs(600);

pub fn available() -> bool {
    spotty_proton_pass_embedded::bundled()
}

// ── What Spotty knows about the vault ───────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Login,
    Note,
    Alias,
    CreditCard,
    Identity,
    SshKey,
    Wifi,
    Custom,
}

impl Kind {
    fn parse(text: &str) -> Kind {
        match text {
            "login" => Kind::Login,
            "note" => Kind::Note,
            "alias" => Kind::Alias,
            "credit_card" => Kind::CreditCard,
            "identity" => Kind::Identity,
            "ssh_key" => Kind::SshKey,
            "wifi" => Kind::Wifi,
            _ => Kind::Custom,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Kind::Login => "dialog-password-symbolic",
            Kind::Note => "text-x-generic-symbolic",
            Kind::Alias => "mail-send-symbolic",
            Kind::CreditCard => "wallet-symbolic",
            Kind::Identity => "avatar-default-symbolic",
            Kind::SshKey => "dialog-password-symbolic",
            Kind::Wifi => "network-wireless-symbolic",
            Kind::Custom => "dialog-password-symbolic",
        }
    }
}

/// One vault item as Proton's secret-free listing describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub share_id: String,
    pub item_id: String,
    pub title: String,
    pub kind: Kind,
    pub vault: String,
}

impl Item {
    /// `share_id/item_id`, the form search actions carry.
    pub fn target(&self) -> String {
        format!("{}/{}", self.share_id, self.item_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Nothing asked yet.
    Idle,
    Loading,
    SignedOut,
    Ready,
    Failed(String),
}

struct Cache {
    state: State,
    items: Vec<Item>,
    /// How many vaults the last listing covered.
    vaults: usize,
    account: String,
    fetched: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    state: State::Idle,
    items: Vec::new(),
    vaults: 0,
    account: String::new(),
    fetched: None,
});

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn state() -> State {
    cache().state.clone()
}

/// The signed-in account's email, once known.
pub fn account() -> String {
    cache().account.clone()
}

pub fn vault_count() -> usize {
    cache().vaults
}

/// Look at the cached vault without copying it.
pub fn with_items<R>(f: impl FnOnce(&State, &[Item]) -> R) -> R {
    let cache = cache();
    f(&cache.state, &cache.items)
}

pub fn find(target: &str) -> Option<Item> {
    with_items(|_, items| items.iter().find(|item| item.target() == target).cloned())
}

/// Ask the UI to redraw anything that shows Pass state.
fn notify_ui() {
    glib::MainContext::default().invoke(crate::app::refresh_search_window);
}

/// Start loading (or reloading) the vault listing in the background when it
/// is missing or stale. Cheap to call on every keystroke.
pub fn ensure_loaded() {
    if !available() {
        return;
    }
    let due = {
        let cache = cache();
        match (&cache.state, cache.fetched) {
            (State::Loading | State::SignedOut, _) => false,
            (State::Idle, _) => true,
            (State::Ready, Some(at)) => at.elapsed() > REFRESH_AFTER,
            (State::Failed(_), Some(at)) => at.elapsed() > RETRY_AFTER,
            _ => true,
        }
    };
    if due {
        refresh();
    }
}

/// Reload the vault listing now.
pub fn refresh() {
    if !available() {
        return;
    }
    {
        let mut cache = cache();
        if cache.state == State::Loading {
            return;
        }
        cache.state = State::Loading;
    }
    std::thread::spawn(|| {
        let result = fetch_listing();
        let mut signed_out = false;
        {
            let mut cache = cache();
            cache.fetched = Some(Instant::now());
            match result {
                Ok((items, vaults)) => {
                    cache.items = items;
                    cache.vaults = vaults;
                    cache.state = State::Ready;
                }
                Err(Failure::SignedOut) => {
                    cache.items.clear();
                    cache.vaults = 0;
                    cache.account.clear();
                    cache.state = State::SignedOut;
                    signed_out = true;
                }
                Err(Failure::Other(message)) => cache.state = State::Failed(message),
            }
        }
        notify_ui();
        if signed_out {
            crate::proton_session::share_pass_if_signed_out();
        }
    });
}

fn fetch_listing() -> Result<(Vec<Item>, usize), Failure> {
    let vaults = parse_vaults(&run(&["vault", "list", "--output", "json"], SHORT)?.text())?;
    let lists: Vec<Result<Vec<Item>, Failure>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = vaults
            .iter()
            .map(|(share_id, name)| {
                scope.spawn(move || {
                    let share = format!("--share-id={share_id}");
                    let out = run(
                        &["item", "list", &share, "--filter-state", "active", "--output", "json"],
                        SHORT,
                    )?;
                    parse_items(&out.text(), name)
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().unwrap_or_else(|_| Err(Failure::Other("The Proton Pass client crashed.".into()))))
            .collect()
    });
    let mut items = Vec::new();
    for list in lists {
        items.extend(list?);
    }
    items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    Ok((items, vaults.len()))
}

/// `{"vaults":[{"name","vault_id","share_id"}]}` → (share id, name).
fn parse_vaults(text: &str) -> Result<Vec<(String, String)>, Failure> {
    let value: Value = serde_json::from_str(text).map_err(|_| Failure::Other("Unexpected answer from Proton Pass.".into()))?;
    let list = value.get("vaults").and_then(Value::as_array).ok_or_else(|| Failure::Other("Unexpected answer from Proton Pass.".into()))?;
    Ok(list
        .iter()
        .filter_map(|vault| {
            let share = vault.get("share_id")?.as_str()?;
            valid_id(share).then(|| (share.to_owned(), vault.get("name").and_then(Value::as_str).unwrap_or("").to_owned()))
        })
        .collect())
}

/// `{"items":[{"id","share_id","title","item_type","state"}]}`. Only these
/// secret-free fields are read, whatever else the client sends.
fn parse_items(text: &str, vault: &str) -> Result<Vec<Item>, Failure> {
    let value: Value = serde_json::from_str(text).map_err(|_| Failure::Other("Unexpected answer from Proton Pass.".into()))?;
    let list = value.get("items").and_then(Value::as_array).ok_or_else(|| Failure::Other("Unexpected answer from Proton Pass.".into()))?;
    Ok(list
        .iter()
        .filter_map(|item| {
            let item_id = item.get("id")?.as_str()?;
            let share_id = item.get("share_id")?.as_str()?;
            if !valid_id(item_id) || !valid_id(share_id) {
                return None;
            }
            if item.get("state").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("trashed")) {
                return None;
            }
            Some(Item {
                share_id: share_id.to_owned(),
                item_id: item_id.to_owned(),
                title: item.get("title").and_then(Value::as_str).unwrap_or("").trim().to_owned(),
                kind: Kind::parse(item.get("item_type").and_then(Value::as_str).unwrap_or("")),
                vault: vault.to_owned(),
            })
        })
        .collect())
}

/// Proton's ids are URL-safe base64; refuse anything else before it reaches a
/// command line.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '.' | '+'))
}

pub fn valid_target(target: &str) -> Option<(&str, &str)> {
    let (share, item) = target.split_once('/')?;
    (valid_id(share) && valid_id(item)).then_some((share, item))
}

// ── Running the client ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    SignedOut,
    Other(String),
}

impl From<Failure> for String {
    fn from(failure: Failure) -> String {
        match failure {
            Failure::SignedOut => "Sign in to Proton Pass first.".into(),
            Failure::Other(message) => message,
        }
    }
}

/// Bytes that may be secret: wiped when dropped.
struct Output(Vec<u8>);

impl Output {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// The first line, without its line ending.
    fn first_line(&self) -> String {
        String::from_utf8_lossy(&self.0).lines().next().unwrap_or("").to_owned()
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // Volatile so the compiler cannot drop the write as dead.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

fn session_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("spotty").join("proton-pass")
}

fn cache_root() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("spotty").join("proton-pass")
}

fn binary() -> Result<PathBuf, String> {
    static BINARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BINARY.get_or_init(|| spotty_proton_pass_embedded::install(&cache_root())).clone()
}

fn command() -> Result<Command, String> {
    let binary = binary()?;
    let session = session_dir();
    crate::security::private_dir(&session).map_err(|e| e.to_string())?;
    let mut command = Command::new(binary);
    command.env_clear();
    // Only what the client needs to find the session and the desktop keyring.
    for key in [
        "HOME",
        "PATH",
        "LANG",
        "LC_ALL",
        "XDG_RUNTIME_DIR",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("PROTON_PASS_SESSION_DIR", &session)
        .env("PROTON_PASS_KEY_PROVIDER", "keyring")
        // The kernel keyring forgets the key at every reboot; the desktop
        // keyring keeps you signed in, as Bridge and Proton VPN do.
        .env("PROTON_PASS_LINUX_KEYRING", "dbus")
        .env("PROTON_PASS_NO_UPDATE_CHECK", "1")
        .env("PROTON_PASS_DISABLE_TELEMETRY", "1")
        .env("PASS_LOG_LEVEL", "off")
        .stdin(Stdio::null());
    Ok(command)
}

fn classify(stderr: &str) -> Failure {
    let lower = stderr.to_lowercase();
    if lower.contains("requires an authenticated client")
        || lower.contains("log in again")
        || lower.contains("session has been invalidated")
        || lower.contains("not logged in")
    {
        return Failure::SignedOut;
    }
    let line = stderr
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or("The Proton Pass client failed.");
    Failure::Other(line.strip_prefix("Error: ").unwrap_or(line).chars().take(300).collect())
}

/// Run the client with `args` and return what it printed. Stops it after
/// `timeout`.
fn run(args: &[&str], timeout: Duration) -> Result<Output, Failure> {
    let mut command = command().map_err(Failure::Other)?;
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| Failure::Other(format!("Couldn't start the Proton Pass client: {e}")))?;
    let mut stdout = child.stdout.take().ok_or_else(|| Failure::Other("No output from Proton Pass.".into()))?;
    let mut stderr = child.stderr.take().ok_or_else(|| Failure::Other("No output from Proton Pass.".into()))?;
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout).take(8 * 1024 * 1024).read_to_end(&mut buffer);
        Output(buffer)
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buffer);
        Output(buffer)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = out_reader.join().unwrap_or(Output(Vec::new()));
    let err = err_reader.join().unwrap_or(Output(Vec::new()));
    match status {
        Some(status) if status.success() => Ok(out),
        Some(_) => Err(classify(&err.text())),
        None => Err(Failure::Other("Proton Pass took too long to answer.".into())),
    }
}

// ── Reading one secret ──────────────────────────────────────────────────────

/// What a search row can hand over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Password,
    Username,
    Totp,
    CardNumber,
    Cvv,
    Note,
}

impl Field {
    pub fn parse(op: &str) -> Option<Field> {
        Some(match op {
            "password" => Field::Password,
            "username" => Field::Username,
            "totp" => Field::Totp,
            "card" => Field::CardNumber,
            "cvv" => Field::Cvv,
            "note" => Field::Note,
            _ => return None,
        })
    }

    fn names(self) -> &'static [&'static str] {
        match self {
            Field::Password => &["password"],
            // Many logins keep only an email address.
            Field::Username => &["username", "email"],
            Field::Totp => &["totp"],
            Field::CardNumber => &["number"],
            Field::Cvv => &["cvv"],
            Field::Note => &["note"],
        }
    }

    fn label(self) -> String {
        use crate::i18n::gettext;
        match self {
            Field::Password => gettext("Password"),
            Field::Username => gettext("Username"),
            Field::Totp => gettext("One-time code"),
            Field::CardNumber => gettext("Card number"),
            Field::Cvv => gettext("Security code"),
            Field::Note => gettext("Note"),
        }
    }
}

fn read_field(target: &str, names: &[&str]) -> Result<Output, Failure> {
    let (share, item) = valid_target(target).ok_or_else(|| Failure::Other("That Proton Pass item is no longer valid.".into()))?;
    let share = format!("--share-id={share}");
    let item = format!("--item-id={item}");
    let mut last = Failure::Other("The item has no such field.".into());
    for name in names {
        let field = format!("--field={name}");
        match run(&["item", "view", &share, &item, &field, "--output", "human"], SHORT) {
            Ok(out) if !out.first_line().is_empty() => return Ok(out),
            Ok(_) => {}
            Err(Failure::SignedOut) => return Err(Failure::SignedOut),
            Err(other) => last = other,
        }
    }
    Err(last)
}

static COPY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Put `secret` on the clipboard marked as a password, and clear it again
/// after [`CLEAR_CLIPBOARD_SECS`] if it is still there. GTK main thread only.
fn copy_secret(secret: &str) {
    use gtk::prelude::*;
    let Some(display) = gdk::Display::default() else { return };
    let clipboard = display.clipboard();
    let text = gdk::ContentProvider::for_value(&glib::Value::from(secret));
    let hint = gdk::ContentProvider::for_bytes("x-kde-passwordManagerHint", &glib::Bytes::from_static(b"secret"));
    let both = gdk::ContentProvider::new_union(&[text, hint]);
    if clipboard.set_content(Some(&both)).is_err() {
        return;
    }
    let generation = COPY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    glib::timeout_add_seconds_local_once(CLEAR_CLIPBOARD_SECS, move || {
        // A later copy (ours or anyone's) must not be wiped.
        if COPY_GENERATION.load(Ordering::SeqCst) == generation && clipboard.is_local() {
            let _ = clipboard.set_content(None::<&gdk::ContentProvider>);
        }
    });
}

fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_owned(), body.to_owned());
    glib::MainContext::default().invoke(move || {
        if let Some(app) = gio::Application::default() {
            let notification = gio::Notification::new(&title);
            if !body.is_empty() {
                notification.set_body(Some(&body));
            }
            gio::prelude::ApplicationExt::send_notification(&app, Some("proton-pass"), &notification);
        }
    });
}

fn title_of(target: &str) -> String {
    find(target).map(|item| item.title).filter(|t| !t.is_empty()).unwrap_or_else(|| crate::i18n::gettext("this item"))
}

/// Copy one field of a vault item. Runs in the background; the outcome is a
/// notification that never contains the secret. Signed out, it starts the
/// sign-in instead.
pub fn copy_field(field: Field, target: &str) {
    let target = target.to_owned();
    std::thread::spawn(move || {
        use crate::i18n::gettext;
        let title = title_of(&target);
        match read_field(&target, field.names()) {
            Ok(out) => {
                let value = out.first_line();
                drop(out);
                let mut bytes = value.into_bytes();
                // The clipboard needs one copy; the buffers here are wiped.
                let secret = String::from_utf8_lossy(&bytes).into_owned();
                wipe(&mut bytes);
                glib::MainContext::default().invoke(move || {
                    copy_secret(&secret);
                    drop(secret);
                });
                notify(
                    &gettext("{field} copied").replace("{field}", &field.label()),
                    &gettext("{title} · clears from the clipboard in {seconds} s")
                        .replace("{title}", &title)
                        .replace("{seconds}", &CLEAR_CLIPBOARD_SECS.to_string()),
                );
            }
            Err(Failure::SignedOut) => {
                mark_signed_out();
                notify(&gettext("Proton Pass"), &gettext("Sign in to Proton Pass first. Opening the sign-in…"));
                sign_in();
            }
            Err(Failure::Other(message)) => notify(&gettext("Proton Pass"), &format!("{title}: {message}")),
        }
    });
}

/// Open the first web address stored in a login.
pub fn open_website(target: &str) {
    let target = target.to_owned();
    std::thread::spawn(move || {
        use crate::i18n::gettext;
        match read_field(&target, &["urls"]) {
            Ok(out) => {
                let urls = out.first_line();
                let url = urls
                    .split(',')
                    .map(str::trim)
                    .find(|url| crate::security::http_uri(url).is_ok())
                    .map(str::to_owned);
                match url {
                    Some(url) => glib::MainContext::default().invoke(move || {
                        let _ = gio::AppInfo::launch_default_for_uri(&url, gio::AppLaunchContext::NONE);
                    }),
                    None => notify(&gettext("Proton Pass"), &gettext("{title} has no web address.").replace("{title}", &title_of(&target))),
                }
            }
            Err(Failure::SignedOut) => {
                mark_signed_out();
                sign_in();
            }
            Err(Failure::Other(message)) => notify(&gettext("Proton Pass"), &message),
        }
    });
}

// ── Signing in and out ──────────────────────────────────────────────────────

static SIGN_IN: Mutex<Option<Child>> = Mutex::new(None);

fn mark_signed_out() {
    let mut cache = cache();
    cache.state = State::SignedOut;
    cache.items.clear();
    cache.vaults = 0;
    cache.account.clear();
    drop(cache);
    notify_ui();
}

/// Sign in to Proton Pass. The client prints a Proton approval link; Spotty
/// approves it with the native Proton session, and the client finishes by
/// itself. Without a native session the link is not opened: the Proton account
/// window opens instead, and Pass signs in once that sign-in is done.
pub fn sign_in() {
    if !available() {
        return;
    }
    std::thread::spawn(|| {
        use crate::i18n::gettext;
        let _signing = crate::proton_session::SigningIn::start("proton-pass");
        if let Some(mut earlier) = SIGN_IN.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = earlier.kill();
            let _ = earlier.wait();
        }
        let spawned = command().and_then(|mut command| {
            command.arg("login").stdout(Stdio::piped()).stderr(Stdio::piped());
            command.spawn().map_err(|e| format!("Couldn't start the Proton Pass client: {e}"))
        });
        let mut child = match spawned {
            Ok(child) => child,
            Err(message) => return notify(&gettext("Proton Pass"), &message),
        };
        let Some(stdout) = child.stdout.take() else { return };
        if let Some(mut stderr) = child.stderr.take() {
            // Drain it so the client never blocks on a full pipe.
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            });
        }
        *SIGN_IN.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);

        // Watchdog: a sign-in nobody completes must not linger.
        std::thread::spawn(|| {
            std::thread::sleep(SIGN_IN_WAIT);
            if let Some(mut stale) = SIGN_IN.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = stale.kill();
                let _ = stale.wait();
            }
        });

        // Ends this sign-in: nothing more can come of it.
        let stop = || {
            let child = SIGN_IN.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some(mut child) = child {
                let _ = child.kill();
                let _ = child.wait();
            }
        };
        let mut opened = false;
        let mut success = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let line = line.trim();
            if !opened && line.starts_with("https://") {
                opened = true;
                // The link carries a key for the client: it is read here and
                // never shown or logged.
                let Some(login) = spotty_proton_account::parse_pass_login_url(line) else {
                    notify(&gettext("Proton Pass"), &gettext("Proton Pass asked to sign in in a form Spotty doesn't know. Try again."));
                    stop();
                    break;
                };
                match crate::proton_native::client() {
                    Some(client) => {
                        if let Err(error) = client.approve_pass_login(&login) {
                            notify(&gettext("Proton Pass"), &gettext("Couldn't sign in to Proton Pass: {error}").replace("{error}", &error.to_string()));
                            stop();
                            break;
                        }
                    }
                    None => {
                        // Not signed in to Proton in Spotty: sign in there first.
                        // Pass starts again once that is done (share_sign_in).
                        stop();
                        glib::MainContext::default().invoke(|| crate::ui::proton_native_ui::open_account_window(None));
                        break;
                    }
                }
            } else if line.starts_with("Successfully logged in") {
                success = true;
            }
        }
        if let Some(mut done) = SIGN_IN.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = done.wait();
        }
        if success {
            notify(&gettext("Proton Pass"), &gettext("Signed in. Your vault is ready to search."));
            refresh();
            load_account();
        }
    });
}

/// Read the signed-in account's email from `info`.
fn load_account() {
    std::thread::spawn(|| {
        if let Ok(out) = run(&["info"], SHORT) {
            let email = out
                .text()
                .lines()
                .find_map(|line| line.trim().trim_start_matches("- ").strip_prefix("Email:").map(|e| e.trim().to_owned()))
                .unwrap_or_default();
            cache().account = email;
            notify_ui();
        }
    });
}

/// Sign out and forget the vault listing and the local session.
pub fn sign_out() {
    if !available() {
        return;
    }
    std::thread::spawn(|| {
        if let Some(mut pending) = SIGN_IN.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = pending.kill();
            let _ = pending.wait();
        }
        // Never signed in here: nothing to remove, and no reason to unpack the
        // client just to say so.
        if session_dir().exists() {
            let _ = run(&["logout", "--force"], SHORT);
        }
        mark_signed_out();
    });
}

/// Run a `pass` trigger row. Everything happens in the background; the
/// outcome is a notification that never contains a secret.
pub fn run_search_action(op: &str, target: &str) {
    match op {
        "signin" => sign_in(),
        "signout" => sign_out(),
        "refresh" => refresh(),
        "website" => open_website(target),
        other => {
            if let Some(field) = Field::parse(other) {
                copy_field(field, target);
            }
        }
    }
}

/// Learn whether a session exists (for the settings window) without listing.
pub fn check_session() {
    if !available() {
        return;
    }
    std::thread::spawn(|| match run(&["info"], SHORT) {
        Ok(_) => {
            load_account();
            ensure_loaded();
        }
        Err(Failure::SignedOut) => mark_signed_out(),
        Err(Failure::Other(message)) => {
            cache().state = State::Failed(message);
            notify_ui();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vaults_and_items_and_skips_trash() {
        let vaults = parse_vaults(r#"{"vaults":[{"name":"Personal","vault_id":"v1","share_id":"s-1"},{"name":"bad","share_id":"a b"}]}"#).unwrap();
        assert_eq!(vaults, vec![("s-1".to_owned(), "Personal".to_owned())]);

        let items = parse_items(
            r#"{"items":[
                {"id":"i1","share_id":"s-1","title":"GitHub","item_type":"login","state":"Active","password":"never read"},
                {"id":"i2","share_id":"s-1","title":"Old","item_type":"note","state":"Trashed"},
                {"id":"i3","share_id":"s-1","title":"Card","item_type":"credit_card","state":"Active"},
                {"id":"../x","share_id":"s-1","title":"Bad","item_type":"login","state":"Active"}
            ]}"#,
            "Personal",
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].title.as_str(), items[0].kind), ("GitHub", Kind::Login));
        assert_eq!(items[1].kind, Kind::CreditCard);
        assert_eq!(items[0].target(), "s-1/i1");
    }

    #[test]
    fn rejects_unexpected_listings() {
        assert!(parse_vaults("not json").is_err());
        assert!(parse_items(r#"{"nothing":[]}"#, "x").is_err());
    }

    #[test]
    fn ids_stay_out_of_option_syntax() {
        assert!(valid_id("abc-DEF_12=="));
        assert!(!valid_id(""));
        assert!(!valid_id("a b"));
        assert!(!valid_id("a;b"));
        assert!(valid_target("s-1/i1").is_some());
        assert!(valid_target("only-one").is_none());
        assert!(valid_target("a/b/c").is_none());
    }

    #[test]
    fn signed_out_errors_are_recognised() {
        assert_eq!(classify("Error: This operation requires an authenticated client\n"), Failure::SignedOut);
        assert_eq!(classify("Your session has been invalidated and you have been logged out automatically."), Failure::SignedOut);
        assert_eq!(classify("x\nError: network unreachable\n"), Failure::Other("network unreachable".into()));
    }

    #[test]
    fn output_is_wiped_on_drop() {
        let mut bytes = b"secret".to_vec();
        wipe(&mut bytes);
        assert!(bytes.iter().all(|b| *b == 0));
        assert_eq!(Output(b"line one\nline two\n".to_vec()).first_line(), "line one");
    }

    #[test]
    fn fields_map_to_client_field_names() {
        assert_eq!(Field::parse("password"), Some(Field::Password));
        assert_eq!(Field::parse("rm -rf"), None);
        assert_eq!(Field::Username.names(), &["username", "email"]);
    }
}
