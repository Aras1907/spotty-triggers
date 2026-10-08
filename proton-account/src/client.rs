//! A signed-in Proton account in memory: the session plus the account's
//! unlocked keys. Unlocked keys never leave this process and are never
//! written to disk.

use crate::api::{Api, Tokens};
use crate::auth::Account;
use crate::error::{Error, Result};
use crate::pgp::{self, Key, Pgp};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct AddressKeys {
    pub id: String,
    pub email: String,
    pub keys: Vec<Key>,
}

pub struct Keyring {
    pub user: Vec<Key>,
    pub addresses: Vec<AddressKeys>,
}

impl Keyring {
    pub fn address(&self, id: &str) -> Option<&AddressKeys> {
        self.addresses.iter().find(|a| a.id == id)
    }

    pub fn all_address_keys(&self) -> Vec<Key> {
        self.addresses.iter().flat_map(|a| a.keys.iter().cloned()).collect()
    }
}

pub struct Client {
    pub(crate) api: Arc<Api>,
    pub(crate) pgp: Box<dyn Pgp>,
    pub email: String,
    pub name: String,
    pub(crate) key_password: zeroize::Zeroizing<String>,
    keyring: Mutex<Option<Arc<Keyring>>>,
    /// Unlocked Drive node keys by link id, so browsing doesn't redo work.
    pub(crate) node_keys: Mutex<HashMap<String, Key>>,
    /// Unlocked calendar keys by calendar id.
    pub(crate) calendar_keys: Mutex<HashMap<String, Arc<Vec<Key>>>>,
}

impl Client {
    /// `save` is called with fresh tokens whenever Proton renews the session.
    pub fn new(account: Account, save: impl Fn(&Tokens) + Send + Sync + 'static) -> Result<Self> {
        Self::with_api(Api::new()?, account, save)
    }

    /// Like `new`, with an explicit API (tests point it at a local server).
    pub fn with_api(api: Api, account: Account, save: impl Fn(&Tokens) + Send + Sync + 'static) -> Result<Self> {
        api.set_tokens(Some(account.tokens.clone()));
        api.on_refresh(save);
        Ok(Self {
            api: Arc::new(api),
            pgp: pgp::provider(),
            email: account.email.clone(),
            name: account.name.clone(),
            key_password: zeroize::Zeroizing::new(account.key_password.clone()),
            keyring: Mutex::new(None),
            node_keys: Mutex::new(HashMap::new()),
            calendar_keys: Mutex::new(HashMap::new()),
        })
    }

    pub fn api(&self) -> &Api {
        &self.api
    }

    /// The signed-in account as it should be saved.
    pub fn account(&self) -> Option<Account> {
        Some(Account {
            tokens: self.api.tokens()?,
            key_password: self.key_password.to_string(),
            email: self.email.clone(),
            name: self.name.clone(),
        })
    }

    /// End the session on Proton's side.
    pub fn sign_out(&self) {
        if let Some(tokens) = self.api.tokens() {
            crate::auth::revoke(&self.api, &tokens);
        }
        self.api.set_tokens(None);
    }

    /// The account's unlocked user and address keys (loaded once).
    pub fn keyring(&self) -> Result<Arc<Keyring>> {
        if let Some(ring) = self.keyring.lock().unwrap().as_ref() {
            return Ok(ring.clone());
        }
        let ring = Arc::new(self.load_keyring()?);
        *self.keyring.lock().unwrap() = Some(ring.clone());
        Ok(ring)
    }

    fn load_keyring(&self) -> Result<Keyring> {
        let user = self.api.get("core/v4/users", &[])?;
        let mut user_keys = Vec::new();
        for key in array(&user, "/User/Keys") {
            if let Some(armored) = key.get("PrivateKey").and_then(Value::as_str) {
                if let Ok(unlocked) = self.pgp.unlock(armored, self.key_password.as_bytes()) {
                    user_keys.push(unlocked);
                }
            }
        }
        if user_keys.is_empty() {
            return Err(Error::Crypto("none of your account keys could be unlocked".into()));
        }
        let addresses = self.api.get("core/v4/addresses", &[])?;
        let mut out = Vec::new();
        for address in array(&addresses, "/Addresses") {
            let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            let mut keys = Vec::new();
            for key in address.get("Keys").and_then(Value::as_array).into_iter().flatten() {
                if let Ok(unlocked) = self.unlock_address_key(key, &user_keys) {
                    keys.push(unlocked);
                }
            }
            out.push(AddressKeys { id: text(address, "ID"), email: text(address, "Email"), keys });
        }
        Ok(Keyring { user: user_keys, addresses: out })
    }

    fn unlock_address_key(&self, key: &Value, user_keys: &[Key]) -> Result<Key> {
        let armored = key.get("PrivateKey").and_then(Value::as_str).unwrap_or_default();
        match key.get("Token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            // Current accounts: a random passphrase encrypted to the user key.
            Some(token) => {
                let passphrase = self.pgp.decrypt_armored(token, user_keys)?;
                self.pgp.unlock(armored, &passphrase)
            }
            // Legacy accounts: the key password itself.
            None => self.pgp.unlock(armored, self.key_password.as_bytes()),
        }
    }
}

pub(crate) fn array<'a>(value: &'a Value, pointer: &str) -> Vec<&'a Value> {
    value.pointer(pointer).and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
}

pub(crate) fn b64(data: &str) -> Result<Vec<u8>> {
    STANDARD.decode(data.trim()).map_err(|_| Error::Protocol("expected base64".into()))
}
