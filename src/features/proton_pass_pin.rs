//! A PIN in front of Proton Pass in Spotty.
//!
//! With a PIN set, the `pass` trigger shows nothing but a locked row until the
//! PIN is typed, and no secret can be copied while it is locked. The unlock
//! lasts a few minutes (your choice) and ends sooner with "Lock now".
//!
//! What this protects: someone sitting at your signed-in computer cannot use
//! Spotty to read your vault. What it does not do: encrypt anything. The vault
//! stays protected by Proton Pass's own key; the PIN only gates Spotty's use of
//! it, so it is a lock on the door, not a safe.
//!
//! The PIN is never stored. Only a random salt and a PBKDF2-HMAC-SHA256 hash of
//! it are, in `~/.config/spotty/pass-pin.json` (readable by you only). Wrong
//! PINs are slowed down: every third miss in a row locks the prompt for longer.

use crate::i18n::gettext;
use adw::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ITERATIONS: u32 = 210_000;
const MIN_LEN: usize = 4;
const MAX_LEN: usize = 64;
/// "Lock again after" choices, in minutes.
const TIMEOUTS: [u32; 4] = [1, 5, 15, 60];
const DEFAULT_MINUTES: u32 = 5;

#[derive(Serialize, Deserialize, Clone)]
struct Stored {
    salt: String,
    iterations: u32,
    hash: String,
    minutes: u32,
}

#[derive(Default)]
struct Session {
    unlocked_until: Option<Instant>,
    misses: u32,
    blocked_until: Option<Instant>,
}

fn session() -> &'static Mutex<Session> {
    static SESSION: Mutex<Session> = Mutex::new(Session { unlocked_until: None, misses: 0, blocked_until: None });
    &SESSION
}

fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("spotty").join("pass-pin.json")
}

fn load() -> Option<Stored> {
    serde_json::from_slice(&std::fs::read(path()).ok()?).ok()
}

fn save(stored: &Stored) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let target = path();
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = target.with_extension("json.new");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temp)?;
    file.write_all(&serde_json::to_vec(stored).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(temp, target)
}

// ── Hashing ─────────────────────────────────────────────────────────────────

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// PBKDF2-HMAC-SHA256, one 32-byte block.
fn pbkdf2(pin: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut first = salt.to_vec();
    first.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(pin, &first);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(pin, &u);
        for (o, b) in out.iter_mut().zip(u) {
            *o ^= b;
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn matches(stored: &Stored, pin: &str) -> bool {
    let (Some(salt), Some(want)) = (unhex(&stored.salt), unhex(&stored.hash)) else { return false };
    same(&pbkdf2(pin.as_bytes(), &salt, stored.iterations), &want)
}

// ── State ───────────────────────────────────────────────────────────────────

/// Whether a PIN is set.
pub fn enabled() -> bool {
    load().is_some()
}

/// Whether secrets may be used right now: no PIN set, or unlocked recently.
pub fn unlocked() -> bool {
    !enabled() || session().lock().unwrap().unlocked_until.is_some_and(|until| until > Instant::now())
}

pub fn lock_now() {
    session().lock().unwrap().unlocked_until = None;
}

fn unlock_for(minutes: u32) {
    let mut session = session().lock().unwrap();
    session.unlocked_until = Some(Instant::now() + Duration::from_secs(u64::from(minutes) * 60));
    session.misses = 0;
    session.blocked_until = None;
}

pub fn timeout_minutes() -> u32 {
    load().map(|s| s.minutes).filter(|m| TIMEOUTS.contains(m)).unwrap_or(DEFAULT_MINUTES)
}

fn set_timeout_minutes(minutes: u32) {
    if let Some(mut stored) = load() {
        stored.minutes = minutes;
        let _ = save(&stored);
    }
}

fn check_format(pin: &str) -> Result<(), String> {
    let length = pin.chars().count();
    if length < MIN_LEN {
        Err(gettext("Use at least {n} characters.").replace("{n}", &MIN_LEN.to_string()))
    } else if length > MAX_LEN {
        Err(gettext("That PIN is too long."))
    } else {
        Ok(())
    }
}

fn set_pin(pin: &str, minutes: u32) -> Result<(), String> {
    check_format(pin)?;
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|_| gettext("Couldn't get randomness to protect the PIN."))?;
    let stored = Stored {
        salt: hex(&salt),
        iterations: ITERATIONS,
        hash: hex(&pbkdf2(pin.as_bytes(), &salt, ITERATIONS)),
        minutes,
    };
    save(&stored).map_err(|e| gettext("Couldn't save the PIN: {error}").replace("{error}", &e.to_string()))?;
    unlock_for(minutes);
    Ok(())
}

fn remove_pin() {
    let _ = std::fs::remove_file(path());
    lock_now();
}

/// Check a typed PIN, slowing guessing down. `Ok` unlocks.
fn try_pin(pin: &str) -> Result<(), String> {
    let Some(stored) = load() else { return Ok(()) };
    {
        let session = session().lock().unwrap();
        if let Some(until) = session.blocked_until.filter(|u| *u > Instant::now()) {
            let seconds = until.saturating_duration_since(Instant::now()).as_secs() + 1;
            return Err(gettext("Too many wrong PINs. Try again in {seconds} s.").replace("{seconds}", &seconds.to_string()));
        }
    }
    if matches(&stored, pin) {
        unlock_for(stored.minutes);
        return Ok(());
    }
    let mut session = session().lock().unwrap();
    session.misses += 1;
    if session.misses % 3 == 0 {
        let batch = session.misses / 3;
        let wait = 30u64.saturating_mul(1 << (batch - 1).min(5)).min(900);
        session.blocked_until = Some(Instant::now() + Duration::from_secs(wait));
        return Err(gettext("Wrong PIN. Try again in {seconds} s.").replace("{seconds}", &wait.to_string()));
    }
    Err(gettext("Wrong PIN."))
}

// ── Dialogs ─────────────────────────────────────────────────────────────────

/// A small window with password fields. `submit` gets the typed values and
/// either closes the window (`Ok`) or shows its message and stays (`Err`).
fn ask(
    parent: Option<&gtk::Window>,
    title: &str,
    description: &str,
    fields: &[String],
    confirm: &str,
    submit: impl Fn(&[String]) -> Result<(), String> + 'static,
) {
    let window = adw::Window::builder().title(title).default_width(380).resizable(false).build();
    if let Some(parent) = parent.filter(|p| p.is_visible()) {
        window.set_transient_for(Some(parent));
    }
    gtk::WindowGroup::new().add_window(&window);

    let header = adw::HeaderBar::builder().show_end_title_buttons(true).build();
    let group = adw::PreferencesGroup::builder().description(description).build();
    let rows: Vec<adw::PasswordEntryRow> =
        fields.iter().map(|title| adw::PasswordEntryRow::builder().title(title.as_str()).build()).collect();
    for row in &rows {
        group.add(row);
    }
    let error = gtk::Label::builder().wrap(true).xalign(0.0).visible(false).css_classes(["error"]).build();
    let cancel = gtk::Button::builder().label(gettext("Cancel")).build();
    let ok = gtk::Button::builder().label(confirm).css_classes(["suggested-action"]).build();
    let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&ok);
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    content.append(&group);
    content.append(&error);
    content.append(&buttons);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    let rows = Rc::new(rows);
    let submit = Rc::new(submit);
    let run = {
        let (window, rows, error) = (window.downgrade(), rows.clone(), error.clone());
        move || {
            let values: Vec<String> = rows.iter().map(|row| row.text().to_string()).collect();
            match submit(&values) {
                Ok(()) => {
                    if let Some(window) = window.upgrade() {
                        window.close();
                    }
                }
                Err(message) => {
                    error.set_label(&message);
                    error.set_visible(true);
                    for row in rows.iter() {
                        row.set_text("");
                    }
                    if let Some(first) = rows.first() {
                        first.grab_focus();
                    }
                }
            }
        }
    };
    let run = Rc::new(run);
    {
        let run = run.clone();
        ok.connect_clicked(move |_| run());
    }
    if let Some(last) = rows.last() {
        let run = run.clone();
        last.connect_entry_activated(move |_| run());
    }
    for pair in rows.windows(2) {
        let next = pair[1].clone();
        pair[0].connect_entry_activated(move |_| {
            next.grab_focus();
        });
    }
    {
        let window = window.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
    let escape = gtk::EventControllerKey::new();
    {
        let window = window.downgrade();
        escape.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(escape);
    window.present();
    if let Some(first) = rows.first() {
        first.grab_focus();
    }
}

/// Ask for the PIN, then run `then`. Without a PIN (or already unlocked) it
/// just runs. GTK main thread only.
pub fn unlock_then(parent: Option<&gtk::Window>, then: impl FnOnce() + 'static) {
    if unlocked() {
        then();
        return;
    }
    let then = Rc::new(RefCell::new(Some(then)));
    ask(
        parent,
        &gettext("Proton Pass is locked"),
        &gettext("Type your PIN to use Proton Pass in Spotty."),
        &[gettext("PIN")],
        &gettext("Unlock"),
        move |values| {
            try_pin(&values[0])?;
            if let Some(then) = then.borrow_mut().take() {
                then();
            }
            Ok(())
        },
    );
}

// ── Settings ────────────────────────────────────────────────────────────────

/// The PIN group of Proton Pass's settings. `parent` is the settings window.
pub fn add_settings_group(page: &adw::PreferencesPage, parent: &adw::Window) {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("PIN lock"))
        .description(gettext("Asks for a PIN before Spotty shows your vault or copies anything from it. It locks Spotty's use of Proton Pass; it doesn't change how Proton protects your vault."))
        .build();
    let status = adw::ActionRow::builder().title(gettext("PIN")).use_markup(false).build();
    let set = gtk::Button::builder().valign(gtk::Align::Center).build();
    let remove = gtk::Button::builder().label(gettext("Remove")).valign(gtk::Align::Center).css_classes(["destructive-action"]).build();
    status.add_suffix(&set);
    status.add_suffix(&remove);
    let timeout = adw::ComboRow::builder()
        .title(gettext("Lock again after"))
        .subtitle(gettext("How long Proton Pass stays open once you have typed the PIN"))
        .model(&gtk::StringList::new(&[]))
        .build();
    let labels: Vec<String> = TIMEOUTS
        .iter()
        .map(|m| if *m == 60 { gettext("1 hour") } else { gettext("{n} min").replace("{n}", &m.to_string()) })
        .collect();
    timeout.set_model(Some(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())));
    let lock = adw::ActionRow::builder().title(gettext("Lock now")).subtitle(gettext("Ask for the PIN again next time")).build();
    let lock_button = gtk::Button::builder().label(gettext("Lock")).valign(gtk::Align::Center).build();
    lock.add_suffix(&lock_button);
    group.add(&status);
    group.add(&timeout);
    group.add(&lock);
    page.add(&group);

    let refresh = {
        let (status, set, remove, timeout, lock) = (status.clone(), set.clone(), remove.clone(), timeout.clone(), lock.clone());
        move || {
            let on = enabled();
            status.set_subtitle(&if on {
                if unlocked() { gettext("On — unlocked") } else { gettext("On — locked") }
            } else {
                gettext("Off — anyone using this session can copy your passwords")
            });
            set.set_label(&if on { gettext("Change…") } else { gettext("Set PIN…") });
            remove.set_visible(on);
            timeout.set_visible(on);
            lock.set_visible(on && unlocked());
            if on {
                let position = TIMEOUTS.iter().position(|m| *m == timeout_minutes()).unwrap_or(1);
                timeout.set_selected(position as u32);
            }
        }
    };
    refresh();
    let refresh = Rc::new(refresh);

    {
        let (parent, refresh) = (parent.downgrade(), refresh.clone());
        set.connect_clicked(move |_| {
            let Some(parent) = parent.upgrade() else { return };
            let refresh = refresh.clone();
            if enabled() {
                let after = refresh.clone();
                ask(
                    Some(parent.upcast_ref()),
                    &gettext("Change PIN"),
                    &gettext("Type your current PIN, then the new one."),
                    &[gettext("Current PIN"), gettext("New PIN"), gettext("Repeat new PIN")],
                    &gettext("Change PIN"),
                    move |v| {
                        if v[1] != v[2] {
                            return Err(gettext("The new PINs don't match."));
                        }
                        check_format(&v[1])?;
                        try_pin(&v[0])?;
                        set_pin(&v[1], timeout_minutes())?;
                        after();
                        Ok(())
                    },
                );
            } else {
                ask(
                    Some(parent.upcast_ref()),
                    &gettext("Set a PIN"),
                    &gettext("Use at least 4 characters. If you forget it, remove pass-pin.json from Spotty's config folder to turn the lock off."),
                    &[gettext("New PIN"), gettext("Repeat PIN")],
                    &gettext("Set PIN"),
                    move |v| {
                        if v[0] != v[1] {
                            return Err(gettext("The PINs don't match."));
                        }
                        set_pin(&v[0], DEFAULT_MINUTES)?;
                        refresh();
                        Ok(())
                    },
                );
            }
        });
    }
    {
        let (parent, refresh) = (parent.downgrade(), refresh.clone());
        remove.connect_clicked(move |_| {
            let Some(parent) = parent.upgrade() else { return };
            let refresh = refresh.clone();
            ask(
                Some(parent.upcast_ref()),
                &gettext("Remove PIN"),
                &gettext("Type your PIN to turn the lock off."),
                &[gettext("PIN")],
                &gettext("Remove PIN"),
                move |v| {
                    try_pin(&v[0])?;
                    remove_pin();
                    refresh();
                    Ok(())
                },
            );
        });
    }
    {
        let refresh = refresh.clone();
        timeout.connect_selected_notify(move |row| {
            if let Some(minutes) = TIMEOUTS.get(row.selected() as usize) {
                if enabled() && *minutes != timeout_minutes() {
                    set_timeout_minutes(*minutes);
                    refresh();
                }
            }
        });
    }
    {
        let refresh = refresh.clone();
        lock_button.connect_clicked(move |_| {
            lock_now();
            refresh();
        });
    }
    // Follow the unlock running out while this window is open.
    let weak = parent.downgrade();
    glib::timeout_add_seconds_local(2, move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        refresh();
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_the_rfc_vector() {
        // RFC 7914 §11: PBKDF2-HMAC-SHA-256, P="passwd", S="salt", c=1.
        let out = pbkdf2(b"passwd", b"salt", 1);
        assert_eq!(hex(&out), "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc");
    }

    #[test]
    fn hash_round_trips_and_rejects_other_pins() {
        let salt = [7u8; 16];
        let stored = Stored { salt: hex(&salt), iterations: 10, hash: hex(&pbkdf2(b"4242", &salt, 10)), minutes: 5 };
        assert!(matches(&stored, "4242"));
        assert!(!matches(&stored, "4243"));
    }

    #[test]
    fn pin_length_is_checked() {
        assert!(check_format("123").is_err());
        assert!(check_format("1234").is_ok());
        assert!(check_format(&"x".repeat(65)).is_err());
    }
}
