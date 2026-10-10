//! Where a Proton app's key password lives: the desktop keyring (Secret
//! Service over D-Bus), never a file.
//!
//! The key password unlocks the account's private keys, so it is as sensitive
//! as the password itself. It is stored under a name per app (for example
//! `proton-calendar`), read back once when the app starts, and removed on
//! sign-out. Spotty never encrypts it with its own key: the keyring does that.

use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use zeroize::Zeroizing;

/// The keyring "service" every Proton app in Spotty stores its key under.
pub const KEYRING_SERVICE: &str = "com.spotty.Spotty.proton";

/// A place to keep one key password per app.
pub trait KeyStore: Send + Sync {
    /// Store `key_password` under `name`, replacing any earlier value.
    fn save(&self, name: &str, key_password: &str) -> Result<()>;
    /// The value stored under `name`, or `None` when there is none.
    fn load(&self, name: &str) -> Result<Option<Zeroizing<String>>>;
    /// Remove what is stored under `name`. Nothing stored is not an error.
    fn forget(&self, name: &str) -> Result<()>;
}

/// The desktop keyring, through Secret Service.
pub struct SecretService;

/// Connects to the session bus once and makes the keyring the default store.
static DEFAULT_STORE: OnceLock<std::result::Result<(), String>> = OnceLock::new();

fn ensure_default_store() -> Result<()> {
    DEFAULT_STORE
        .get_or_init(|| match zbus_secret_service_keyring_store::Store::new() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        })
        .clone()
        .map_err(Error::Keyring)
}

fn entry(name: &str) -> Result<keyring_core::Entry> {
    ensure_default_store()?;
    keyring_core::Entry::new(KEYRING_SERVICE, name).map_err(|error| Error::Keyring(error.to_string()))
}

impl KeyStore for SecretService {
    fn save(&self, name: &str, key_password: &str) -> Result<()> {
        entry(name)?.set_password(key_password).map_err(|error| Error::Keyring(error.to_string()))
    }

    fn load(&self, name: &str) -> Result<Option<Zeroizing<String>>> {
        match entry(name)?.get_password() {
            Ok(password) => Ok(Some(Zeroizing::new(password))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(Error::Keyring(error.to_string())),
        }
    }

    fn forget(&self, name: &str) -> Result<()> {
        match entry(name)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(Error::Keyring(error.to_string())),
        }
    }
}

/// A store in memory, for tests. Nothing is written anywhere.
#[derive(Default)]
pub struct MemoryKeyStore {
    values: Mutex<HashMap<String, String>>,
}

impl MemoryKeyStore {
    /// How many values are stored (for tests).
    pub fn len(&self) -> usize {
        self.values.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl KeyStore for MemoryKeyStore {
    fn save(&self, name: &str, key_password: &str) -> Result<()> {
        self.values.lock().unwrap_or_else(|p| p.into_inner()).insert(name.to_owned(), key_password.to_owned());
        Ok(())
    }

    fn load(&self, name: &str) -> Result<Option<Zeroizing<String>>> {
        Ok(self.values.lock().unwrap_or_else(|p| p.into_inner()).get(name).map(|v| Zeroizing::new(v.clone())))
    }

    fn forget(&self, name: &str) -> Result<()> {
        self.values.lock().unwrap_or_else(|p| p.into_inner()).remove(name);
        Ok(())
    }
}

/// A store that is always unavailable, as on a computer with no keyring
/// running. For tests of the paths that must cope with that.
pub struct UnavailableKeyStore;

impl KeyStore for UnavailableKeyStore {
    fn save(&self, _: &str, _: &str) -> Result<()> {
        Err(Error::Keyring("no keyring is running".into()))
    }

    fn load(&self, _: &str) -> Result<Option<Zeroizing<String>>> {
        Err(Error::Keyring("no keyring is running".into()))
    }

    fn forget(&self, _: &str) -> Result<()> {
        Err(Error::Keyring("no keyring is running".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_saves_loads_and_forgets_per_name() {
        let store = MemoryKeyStore::default();
        assert!(store.load("proton-calendar").unwrap().is_none());
        store.save("proton-calendar", "one").unwrap();
        store.save("proton-drive", "two").unwrap();
        assert_eq!(store.load("proton-calendar").unwrap().unwrap().as_str(), "one");
        assert_eq!(store.load("proton-drive").unwrap().unwrap().as_str(), "two");
        store.forget("proton-calendar").unwrap();
        assert!(store.load("proton-calendar").unwrap().is_none());
        assert_eq!(store.len(), 1, "forgetting one app leaves the other");
        store.forget("proton-calendar").unwrap();
        assert_eq!(store.len(), 1, "forgetting nothing is not an error");
    }

    #[test]
    fn an_unavailable_keyring_says_so_and_stores_nothing() {
        assert!(matches!(UnavailableKeyStore.save("x", "y"), Err(Error::Keyring(_))));
        assert!(matches!(UnavailableKeyStore.load("x"), Err(Error::Keyring(_))));
        assert!(matches!(UnavailableKeyStore.forget("x"), Err(Error::Keyring(_))));
    }
}
