//! Signing in to a Proton account natively: Proton's SRP login (the password
//! never leaves this process, only a proof of it), the optional two-factor
//! code and the second "mailbox" password, then the key password that
//! unlocks your end-to-end encrypted data.

use crate::api::{Api, Tokens};
use crate::error::{Error, Result};
use crate::pgp;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use proton_srp::{SRPAuth, SRPProofB64, SrpHashVersion, mailbox_password_hash};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::Zeroizing;

/// A signed-in account: what Spotty keeps between runs (see `store`).
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub tokens: Tokens,
    /// Unlocks the account's private keys. Derived from your password, which
    /// itself is never stored.
    pub key_password: String,
    pub email: String,
    pub name: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("email", &self.email).finish_non_exhaustive()
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.key_password);
    }
}

/// What Proton still needs after a step.
pub enum Step {
    Done(Account),
    /// Enter the authenticator code (or a recovery code).
    TwoFactor(Pending),
    /// The account has a separate mailbox password.
    MailboxPassword(Pending),
}

pub struct Pending {
    tokens: Tokens,
    username: String,
    password: Zeroizing<String>,
    /// How the account is protected, from Proton's login answer.
    two_factor: u8,
    two_passwords: bool,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pending(..)")
    }
}

/// Check the username and password with Proton.
pub fn begin(api: &Api, username: &str, password: &str) -> Result<Step> {
    let username = username.trim();
    if username.is_empty() || password.is_empty() {
        return Err(Error::Unsupported("Enter your Proton email and password".into()));
    }
    let info = api.anonymous_post("core/v4/auth/info", &json!({ "Intent": "Proton", "Username": username }))?;
    let field = |name: &str| -> Result<&str> {
        info.get(name).and_then(Value::as_str).ok_or_else(|| Error::Protocol(format!("sign-in answer misses {name}")))
    };
    let version = info
        .get("Version")
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| Error::Protocol("sign-in answer misses Version".into()))?;
    let version = SrpHashVersion::try_from(version).map_err(|e| Error::Protocol(e.to_string()))?;
    let srp = SRPAuth::with_pgp(
        Some(username),
        password,
        version,
        // Accounts without a salt (very old ones) sign in with an empty one.
        info.get("Salt").and_then(Value::as_str).unwrap_or_default(),
        field("Modulus")?,
        field("ServerEphemeral")?,
    )
    .map_err(|e| Error::Crypto(format!("sign-in proof failed: {e}")))?;
    let proof: SRPProofB64 = srp.generate_proofs().map_err(|e| Error::Crypto(e.to_string()))?.into();

    let answer = api.anonymous_post(
        "core/v4/auth",
        &json!({
            "Username": username,
            "ClientEphemeral": proof.client_ephemeral,
            "ClientProof": proof.client_proof,
            "SRPSession": field("SRPSession")?,
            "PersistentCookies": 0,
            "Payload": {},
        }),
    )?;
    // Proton proves it knows the verifier too; anything else isn't Proton.
    let server_proof = answer.get("ServerProof").and_then(Value::as_str).unwrap_or_default();
    if !proof.compare_server_proof(server_proof) {
        return Err(Error::Crypto("Proton's answer couldn't be verified; not signing in".into()));
    }
    let text = |key: &str| answer.get(key).and_then(Value::as_str).map(str::to_owned);
    let tokens = match (text("UID"), text("AccessToken"), text("RefreshToken")) {
        (Some(uid), Some(access), Some(refresh)) => Tokens { uid, access, refresh },
        _ => return Err(Error::Protocol("sign-in answer has no session".into())),
    };
    let two_factor = answer
        .pointer("/2FA/Enabled")
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(0);
    let two_passwords = answer.get("PasswordMode").and_then(Value::as_u64) == Some(2);
    let pending = Pending { tokens, username: username.to_owned(), password: Zeroizing::new(password.to_owned()), two_factor, two_passwords };
    if two_factor == 0 {
        return pending.next(api);
    }
    if two_factor & 1 == 0 {
        revoke(api, &pending.tokens);
        return Err(Error::Unsupported(
            "This account is protected by a security key only. Add an authenticator app in Proton's security settings to sign in here.".into(),
        ));
    }
    Ok(Step::TwoFactor(pending))
}

/// A failed second step: either try the same code box again, or start over.
pub enum Retry {
    /// Wrong code, say, keep the half-finished sign-in and ask again.
    Again(Error, Pending),
    Failed(Error),
}

impl Pending {
    /// Send the authenticator (or recovery) code.
    pub fn submit_two_factor(mut self, api: &Api, code: &str) -> std::result::Result<Step, Retry> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.is_empty() {
            return Err(Retry::Again(Error::Unsupported("Enter the code from your authenticator app".into()), self));
        }
        if let Err(error) = api.send_as(&self.tokens, reqwest::Method::POST, "auth/v4/2fa", Some(&json!({ "TwoFactorCode": code }))) {
            return Err(Retry::Again(error, self));
        }
        self.two_factor = 0;
        self.next(api).map_err(Retry::Failed)
    }

    /// Continue with the mailbox password. On a wrong one the sign-in ends.
    pub fn submit_mailbox_password(self, api: &Api, mailbox: &str) -> Result<Step> {
        if mailbox.is_empty() {
            return Err(Error::Unsupported("Enter your mailbox password".into()));
        }
        let account = finish(api, &self.tokens, &self.username, mailbox, true)?;
        Ok(Step::Done(account))
    }

    /// Give up: end the half-open session on Proton's side.
    pub fn cancel(self, api: &Api) {
        revoke(api, &self.tokens);
    }

    fn next(self, api: &Api) -> Result<Step> {
        if self.two_passwords {
            return Ok(Step::MailboxPassword(self));
        }
        let account = finish(api, &self.tokens, &self.username, &self.password, false)?;
        Ok(Step::Done(account))
    }
}

/// Fetch the user, derive the key password and prove it unlocks the keys.
fn finish(api: &Api, tokens: &Tokens, username: &str, key_source: &str, mailbox: bool) -> Result<Account> {
    api.set_tokens(Some(tokens.clone()));
    let result = (|| {
        let user = api.get("core/v4/users", &[])?;
        let user = user.get("User").ok_or_else(|| Error::Protocol("no user in answer".into()))?;
        let keys = user.get("Keys").and_then(Value::as_array).cloned().unwrap_or_default();
        let salts = api.get("core/v4/keys/salts", &[])?;
        let salts = salts.get("KeySalts").and_then(Value::as_array).cloned().unwrap_or_default();
        let key_password = unlock_primary(&keys, &salts, key_source, mailbox)?;
        let pick = |names: &[&str]| {
            names
                .iter()
                .filter_map(|n| user.get(*n).and_then(Value::as_str))
                .find(|s| !s.is_empty())
                .map(str::to_owned)
        };
        Ok(Account {
            tokens: tokens.clone(),
            key_password,
            email: pick(&["Email", "Name"]).unwrap_or_else(|| username.to_owned()),
            name: pick(&["DisplayName", "Name", "Email"]).unwrap_or_else(|| username.to_owned()),
        })
    })();
    if result.is_err() {
        revoke(api, tokens);
        api.set_tokens(None);
    }
    result
}

/// Derive the key password from `password` for the primary user key and check
/// it really opens that key.
fn unlock_primary(keys: &[Value], salts: &[Value], password: &str, mailbox: bool) -> Result<String> {
    let primary = keys
        .iter()
        .find(|k| k.get("Primary").and_then(Value::as_u64) == Some(1))
        .or_else(|| keys.first())
        .ok_or_else(|| Error::Unsupported("This Proton account has no encryption keys yet. Open Proton once in a browser to set it up.".into()))?;
    let id = primary.get("ID").and_then(Value::as_str).unwrap_or_default();
    let salt = salts
        .iter()
        .find(|s| s.get("ID").and_then(Value::as_str) == Some(id))
        .and_then(|s| s.get("KeySalt").and_then(Value::as_str))
        .ok_or_else(|| Error::Protocol("no key salt for your primary key".into()))?;
    let salt = STANDARD.decode(salt).map_err(|_| Error::Protocol("key salt isn't base64".into()))?;
    let key_password = derive_key_password(password, &salt)?;
    let armored = primary.get("PrivateKey").and_then(Value::as_str).unwrap_or_default();
    pgp::provider().unlock(armored, key_password.as_bytes()).map_err(|_| {
        if mailbox {
            Error::Unsupported("That mailbox password doesn't open your Proton keys".into())
        } else {
            Error::Crypto("your password doesn't open your Proton keys".into())
        }
    })?;
    Ok(key_password.to_string())
}

/// Proton's key password: the bcrypt hash of the password with the account's
/// key salt, without the bcrypt prefix.
pub fn derive_key_password(password: &str, salt: &[u8]) -> Result<Zeroizing<String>> {
    let hash = mailbox_password_hash(password, salt).map_err(|e| Error::Crypto(format!("key derivation failed: {e}")))?;
    String::from_utf8(hash.hashed_password().to_vec())
        .map(Zeroizing::new)
        .map_err(|_| Error::Crypto("key derivation produced invalid text".into()))
}

/// End a session on Proton's side. Best effort.
pub fn revoke(api: &Api, tokens: &Tokens) {
    let _ = api.send_as(tokens, reqwest::Method::DELETE, "auth/v4", None);
}
