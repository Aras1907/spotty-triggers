//! API keys in the desktop keyring (GNOME Keyring, KWallet — anything that
//! speaks the freedesktop Secret Service over D-Bus).
//!
//! Keys never go into `config.json`, logs or argv: they are written to the
//! keyring, read back on first use and then kept in memory for the session.
//! Every call here blocks on D-Bus, so call it from a worker thread
//! (`gio::spawn_blocking`), never from the GTK main thread.

use gtk::{gio, glib};
use glib::variant::{ObjectPath, ToVariant};
use glib::Variant;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const DEST: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";
const SCHEMA: &str = "io.github.spotty.ApiKey";
const CALL_TIMEOUT_MS: i32 = 10_000;
/// How long an unlock prompt may stay open.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// account → key (None = looked up, nothing stored). Memory only.
static CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache() -> std::sync::MutexGuard<'static, HashMap<String, Option<String>>> {
    CACHE.lock().unwrap_or_else(|p| p.into_inner())
}

/// The key stored for `account` (e.g. "translate:deepl"), if any.
pub fn get(account: &str) -> Result<Option<String>, String> {
    if let Some(hit) = cache().get(account) {
        return Ok(hit.clone());
    }
    let found = with_service(|conn, session| lookup(conn, session, account))?;
    cache().insert(account.to_string(), found.clone());
    Ok(found)
}

/// Whether a key is stored — without keeping the key itself around.
pub fn has(account: &str) -> Result<bool, String> {
    get(account).map(|k| k.is_some())
}

/// Store (or replace) the key for `account`. `label` is what the keyring
/// app shows ("Spotty: DeepL API key").
pub fn set(account: &str, label: &str, secret: &str) -> Result<(), String> {
    with_service(|conn, session| store(conn, session, account, label, secret))?;
    cache().insert(account.to_string(), Some(secret.to_string()));
    Ok(())
}

/// Remove the key for `account` (no error when there is none).
pub fn clear(account: &str) -> Result<(), String> {
    with_service(|conn, _| {
        let (unlocked, locked) = search(conn, account)?;
        for path in unlocked.iter().chain(locked.iter()) {
            let reply = call(conn, path, ITEM, "Delete", None)?;
            prompt_if_needed(conn, &reply.child_value(0))?;
        }
        Ok(())
    })?;
    cache().insert(account.to_string(), None);
    Ok(())
}

fn attributes(account: &str) -> HashMap<String, String> {
    HashMap::from([
        ("xdg:schema".to_string(), SCHEMA.to_string()),
        ("application".to_string(), "spotty".to_string()),
        ("account".to_string(), account.to_string()),
    ])
}

/// Open the session bus and a "plain" Secret Service session (the secret
/// crosses the local session bus only), then run `f`. Signals (unlock
/// prompts) need a main context, so this thread brings its own.
fn with_service<R>(
    f: impl FnOnce(&gio::DBusConnection, &ObjectPath) -> Result<R, String>,
) -> Result<R, String> {
    let ctx = glib::MainContext::new();
    ctx.with_thread_default(|| {
        let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .map_err(|e| format!("session bus: {}", e.message()))?;
        let reply = call(
            &conn,
            SERVICE_PATH,
            SERVICE,
            "OpenSession",
            Some(("plain", "".to_variant()).to_variant()),
        )
        .map_err(|e| {
            if e.contains("ServiceUnknown") || e.contains("not provided") {
                "no keyring service is running (install GNOME Keyring or KWallet)".to_string()
            } else {
                e
            }
        })?;
        let session = object_path(&reply.child_value(1))?;
        let out = f(&conn, &session);
        let _ = call(&conn, session.as_str(), "org.freedesktop.Secret.Session", "Close", None);
        out
    })
    .map_err(|e| format!("main context: {e}"))?
}

fn call(
    conn: &gio::DBusConnection,
    path: &str,
    iface: &str,
    method: &str,
    params: Option<Variant>,
) -> Result<Variant, String> {
    conn.call_sync(
        Some(DEST),
        path,
        iface,
        method,
        params.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
    )
    .map_err(|e| e.message().to_string())
}

fn object_path(v: &Variant) -> Result<ObjectPath, String> {
    v.str()
        .and_then(|s| ObjectPath::try_from(s.to_string()).ok())
        .ok_or_else(|| "keyring: unexpected reply".to_string())
}

fn paths(v: &Variant) -> Vec<String> {
    v.iter().filter_map(|p| p.str().map(String::from)).collect()
}

/// (unlocked, locked) items for `account`.
fn search(conn: &gio::DBusConnection, account: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let reply = call(
        conn,
        SERVICE_PATH,
        SERVICE,
        "SearchItems",
        Some((attributes(account),).to_variant()),
    )?;
    Ok((paths(&reply.child_value(0)), paths(&reply.child_value(1))))
}

fn lookup(
    conn: &gio::DBusConnection,
    session: &ObjectPath,
    account: &str,
) -> Result<Option<String>, String> {
    let (mut unlocked, locked) = search(conn, account)?;
    if unlocked.is_empty() && !locked.is_empty() {
        unlock(conn, &locked)?;
        unlocked = search(conn, account)?.0;
    }
    let Some(first) = unlocked.first() else { return Ok(None) };
    let items = vec![ObjectPath::try_from(first.clone()).map_err(|_| "keyring: bad item")?];
    let reply = call(
        conn,
        SERVICE_PATH,
        SERVICE,
        "GetSecrets",
        Some((items, session.clone()).to_variant()),
    )?;
    // a{o(oayays)}: the secret's value is the third field.
    let secrets = reply.child_value(0);
    let Some(entry) = secrets.iter().next() else { return Ok(None) };
    let value: Vec<u8> = entry.child_value(1).child_value(2).get().unwrap_or_default();
    let key = String::from_utf8(value).map_err(|_| "keyring: the stored key is not text")?;
    Ok((!key.is_empty()).then_some(key))
}

fn store(
    conn: &gio::DBusConnection,
    session: &ObjectPath,
    account: &str,
    label: &str,
    secret: &str,
) -> Result<(), String> {
    let props: HashMap<String, Variant> = HashMap::from([
        ("org.freedesktop.Secret.Item.Label".to_string(), label.to_variant()),
        (
            "org.freedesktop.Secret.Item.Attributes".to_string(),
            attributes(account).to_variant(),
        ),
    ]);
    let secret = (
        session.clone(),
        Vec::<u8>::new(),
        secret.as_bytes().to_vec(),
        "text/plain".to_string(),
    );
    let params = create_item_params(props, secret.to_variant());
    let reply = match call(conn, DEFAULT_COLLECTION, COLLECTION, "CreateItem", Some(params.clone())) {
        Ok(r) => r,
        Err(e) if e.contains("IsLocked") || e.contains("locked") => {
            unlock(conn, &[DEFAULT_COLLECTION.to_string()])?;
            call(conn, DEFAULT_COLLECTION, COLLECTION, "CreateItem", Some(params))?
        }
        Err(e) => return Err(e),
    };
    prompt_if_needed(conn, &reply.child_value(1))
}

/// CreateItem's `(a{sv}(oayays)b)`. Built from parts: a `Variant` inside a
/// Rust tuple would be boxed as `v`.
fn create_item_params(props: HashMap<String, Variant>, secret: Variant) -> Variant {
    Variant::tuple_from_iter([props.to_variant(), secret, true.to_variant()])
}

fn unlock(conn: &gio::DBusConnection, objects: &[String]) -> Result<(), String> {
    let objects: Vec<ObjectPath> =
        objects.iter().filter_map(|p| ObjectPath::try_from(p.clone()).ok()).collect();
    let reply = call(conn, SERVICE_PATH, SERVICE, "Unlock", Some((objects,).to_variant()))?;
    prompt_if_needed(conn, &reply.child_value(1))
}

/// A Secret Service call may answer with a prompt ("/" = none): show it (the
/// keyring asks for its password) and wait until the user answers.
fn prompt_if_needed(conn: &gio::DBusConnection, prompt: &Variant) -> Result<(), String> {
    let path = prompt.str().unwrap_or("/").to_string();
    if path == "/" {
        return Ok(());
    }
    // None = waiting, Some(true) = dismissed, Some(false) = done.
    let outcome: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
    let sub = {
        let outcome = outcome.clone();
        conn.signal_subscribe(
            Some(DEST),
            Some(PROMPT),
            Some("Completed"),
            Some(&path),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                let dismissed = params.child_value(0).get::<bool>().unwrap_or(true);
                outcome.set(Some(dismissed));
            },
        )
    };
    let result = call(conn, &path, PROMPT, "Prompt", Some(("",).to_variant())).and_then(|_| {
        let ctx = glib::MainContext::ref_thread_default();
        let start = std::time::Instant::now();
        while outcome.get().is_none() && start.elapsed() < PROMPT_TIMEOUT {
            if !ctx.iteration(false) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        match outcome.get() {
            Some(false) => Ok(()),
            Some(true) => Err("the keyring stayed locked".to_string()),
            None => Err("the keyring did not answer".to_string()),
        }
    });
    #[allow(deprecated)]
    conn.signal_unsubscribe(sub);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_name_the_account_and_never_the_key() {
        let a = attributes("translate:deepl");
        assert_eq!(a["account"], "translate:deepl");
        assert_eq!(a["xdg:schema"], SCHEMA);
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn secret_struct_has_the_secret_service_signature() {
        let session = ObjectPath::try_from("/s".to_string()).unwrap();
        let v = (session, Vec::<u8>::new(), b"k".to_vec(), "text/plain".to_string()).to_variant();
        assert_eq!(v.type_().as_str(), "(oayays)");
        let props: HashMap<String, Variant> =
            HashMap::from([("x".to_string(), attributes("a").to_variant())]);
        assert_eq!(create_item_params(props, v).type_().as_str(), "(a{sv}(oayays)b)");
    }

    #[test]
    #[ignore = "uses the desktop keyring"]
    fn round_trip_through_the_desktop_keyring() {
        let account = "selftest:spotty";
        set(account, "Spotty self-test (safe to delete)", "s3cr3t").unwrap();
        cache().clear();
        assert_eq!(get(account).unwrap().as_deref(), Some("s3cr3t"));
        clear(account).unwrap();
        cache().clear();
        assert_eq!(get(account).unwrap(), None);
    }
}
