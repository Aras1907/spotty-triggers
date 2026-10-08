//! Signing Proton's other official clients in with this account. Proton Pass's
//! CLI and Proton VPN run as their own apps, but Spotty already holds the
//! session, so it asks Proton for a child session (Proton's "session fork",
//! what account.proton.me does for desktop apps) and hands that to the other
//! app. No password is shared. `Independent: 0` ties the child to this
//! session, so signing this session out revokes the child as well.

use crate::client::Client;
use crate::error::{Error, Result};
use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

/// The child client id of Proton Pass's CLI (`pass-cli`).
pub const PASS_CLIENT_ID: &str = "cli-pass";
/// The child client id of Proton VPN's Linux GUI.
pub const VPN_CLIENT_ID: &str = "linux-vpn-gui";

/// Only these children may be forked. Checked before any request is sent, so a
/// bug elsewhere can't mint a session for some other Proton app.
const ALLOWED_CHILDREN: [&str; 2] = [PASS_CLIENT_ID, VPN_CLIENT_ID];

/// Where `pass-cli login` sends the user. Proton fixes this shape; anything else
/// is refused rather than guessed at.
const PASS_LOGIN_ORIGIN: &str = "https://account.proton.me";
const PASS_LOGIN_PATH: &str = "/desktop/login";
const PASS_LOGIN_QUERY: &str = "app=pass";
/// pass-cli reads a 16-byte nonce from the front of the payload (the usual AES-GCM size is 12).
const NONCE_LEN: usize = 16;

/// AES-256-GCM with the 16-byte nonce pass-cli expects.
type PassCipher = aes_gcm::AesGcm<aes_gcm::aes::Aes256, U16>;
type PassNonce = aes_gcm::Nonce<U16>;

/// What a session fork asks Proton for.
pub struct ForkRequest {
    /// Which app the child session is for. Must be one of the constants above.
    pub child_client_id: &'static str,
    /// The code `pass-cli` printed, which lets Proton match the approval to it.
    pub user_code: Option<String>,
    /// Encrypted data for the child (see `pass_fork_payload`).
    pub payload: Option<String>,
}

/// A `pass-cli login` link that passed every check.
pub struct PassLogin {
    /// The code `pass-cli` printed, sent back to Proton with the approval.
    pub user_code: String,
    /// The 32-byte key `pass-cli` made for this login. Never printed.
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for PassLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PassLogin(..)")
    }
}

impl Client {
    /// Ask Proton for a child session of this one and return its selector, which
    /// the child app exchanges for its own tokens.
    pub fn fork_session(&self, request: &ForkRequest) -> Result<String> {
        if !ALLOWED_CHILDREN.contains(&request.child_client_id) {
            return Err(Error::Unsupported("Spotty only signs in Proton Pass and Proton VPN this way".into()));
        }
        let mut body = json!({ "ChildClientID": request.child_client_id, "Independent": 0 });
        if let Some(code) = &request.user_code {
            body["UserCode"] = json!(code);
        }
        if let Some(payload) = &request.payload {
            body["Payload"] = json!(payload);
        }
        let answer = self.api.post("auth/v4/sessions/forks", &body)?;
        answer
            .get("Selector")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol("session fork answer has no selector".into()))
    }

    /// Approve a `pass-cli login` link. The key password goes to Proton only
    /// inside a payload that pass-cli alone can open.
    pub fn approve_pass_login(&self, login: &PassLogin) -> Result<()> {
        let payload = pass_fork_payload(login, &self.key_password)?;
        self.fork_session(&ForkRequest {
            child_client_id: PASS_CLIENT_ID,
            user_code: Some(login.user_code.clone()),
            payload: Some(payload),
        })?;
        Ok(())
    }

    /// A session for Proton VPN. VPN needs no key password, so there is no
    /// code or payload.
    pub fn fork_for_vpn(&self) -> Result<String> {
        self.fork_session(&ForkRequest { child_client_id: VPN_CLIENT_ID, user_code: None, payload: None })
    }
}

/// Read a `pass-cli login` link. Strict on purpose: the link arrives by paste,
/// so only Proton's exact shape is accepted. Surrounding whitespace is not
/// trimmed; the caller does that.
pub fn parse_pass_login_url(url: &str) -> Option<PassLogin> {
    let (before, fragment) = url.split_once('#')?;
    let (base, query) = before.split_once('?')?;
    if query != PASS_LOGIN_QUERY || base.strip_prefix(PASS_LOGIN_ORIGIN)? != PASS_LOGIN_PATH {
        return None;
    }
    // The payload lives in the fragment, so it never reaches a web server.
    let encoded = fragment.strip_prefix("payload=")?;
    let decoded = Zeroizing::new(percent_decode(encoded)?);
    let fields: Vec<&str> = decoded.split(':').collect();
    let [version, user_code, key, client] = fields.as_slice() else { return None };
    if *version != "0" || *client != PASS_CLIENT_ID {
        return None;
    }
    let code_ok = (1..=64).contains(&user_code.len()) && user_code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !code_ok {
        return None;
    }
    let key_bytes = Zeroizing::new(STANDARD.decode(key).ok()?);
    let mut login_key = Zeroizing::new([0u8; 32]);
    if key_bytes.len() != login_key.len() {
        return None;
    }
    login_key.copy_from_slice(&key_bytes);
    Some(PassLogin { user_code: (*user_code).to_owned(), key: login_key })
}

/// The `Payload` for a Pass approval, as pass-cli opens it: base64 of a 16-byte
/// random nonce followed by AES-256-GCM output (ciphertext and tag, no
/// additional data) of `{"keyPassword": ...}`, keyed with the login key.
pub fn pass_fork_payload(login: &PassLogin, key_password: &str) -> Result<String> {
    #[derive(Serialize)]
    struct Plain<'a> {
        #[serde(rename = "keyPassword")]
        key_password: &'a str,
    }
    let cipher = PassCipher::new_from_slice(login.key.as_slice()).map_err(|_| Error::Crypto("bad login key".into()))?;
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|e| Error::Crypto(format!("no randomness for the payload: {e}")))?;
    // The plaintext holds the key password, so its buffer is wiped on drop.
    let plain = Zeroizing::new(serde_json::to_vec(&Plain { key_password })?);
    let sealed = cipher
        .encrypt(PassNonce::from_slice(&nonce), plain.as_slice())
        .map_err(|_| Error::Crypto("couldn't seal the pass payload".into()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(STANDARD.encode(out))
}

/// Decode `%XX` escapes and nothing else. A `+` stays a plus sign, as it does in
/// a URL fragment. A malformed escape or non-UTF-8 result gives None.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex_digit(*bytes.get(i + 1)?)?;
            let low = hex_digit(*bytes.get(i + 2)?)?;
            out.push((high << 4) | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::str::from_utf8(&out).ok().map(str::to_owned)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [0xfb; 32];

    /// `version:code:key:client`, the text Proton percent-encodes into the link.
    fn plain(version: &str, code: &str, key: &[u8], client: &str) -> String {
        format!("{version}:{code}:{}:{client}", STANDARD.encode(key))
    }

    /// Percent-encodes everything but unreserved characters, as the link does.
    fn encode(text: &str) -> String {
        text.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") })
            .collect()
    }

    fn link(origin: &str, path: &str, query: &str, payload: &str) -> String {
        format!("{origin}{path}?{query}#payload={}", encode(payload))
    }

    fn real_link(payload: &str) -> String {
        link("https://account.proton.me", "/desktop/login", "app=pass", payload)
    }

    #[test]
    fn accepts_the_link_pass_cli_prints() {
        // The key's base64 has '+', so the escaped form is really exercised.
        assert!(STANDARD.encode(KEY).contains('+'));
        let login = parse_pass_login_url(&real_link(&plain("0", "AB-12cd", &KEY, "cli-pass"))).expect("real link");
        assert_eq!(login.user_code, "AB-12cd");
        assert_eq!(*login.key, KEY);
        assert_eq!(format!("{login:?}"), "PassLogin(..)");
    }

    #[test]
    fn accepts_a_user_code_of_64_characters_but_not_65() {
        let longest = "a".repeat(64);
        assert!(parse_pass_login_url(&real_link(&plain("0", &longest, &KEY, "cli-pass"))).is_some());
        let too_long = "a".repeat(65);
        assert!(parse_pass_login_url(&real_link(&plain("0", &too_long, &KEY, "cli-pass"))).is_none());
    }

    #[test]
    fn refuses_anything_but_pass_cli_login_links() {
        let good = plain("0", "CODE", &KEY, "cli-pass");
        let refused = [
            ("http scheme", link("http://account.proton.me", "/desktop/login", "app=pass", &good)),
            ("lookalike host", link("https://account.proton.me.evil.com", "/desktop/login", "app=pass", &good)),
            ("user info trick", link("https://account.proton.me@evil.com", "/desktop/login", "app=pass", &good)),
            ("explicit port", link("https://account.proton.me:444", "/desktop/login", "app=pass", &good)),
            ("other path", link("https://account.proton.me", "/desktop/other", "app=pass", &good)),
            ("trailing slash", link("https://account.proton.me", "/desktop/login/", "app=pass", &good)),
            ("other app", link("https://account.proton.me", "/desktop/login", "app=drive", &good)),
            ("extra query", link("https://account.proton.me", "/desktop/login", "app=pass&x=1", &good)),
            ("missing fragment", "https://account.proton.me/desktop/login?app=pass".to_owned()),
            ("other fragment", format!("https://account.proton.me/desktop/login?app=pass#token={}", encode(&good))),
            ("malformed escape", "https://account.proton.me/desktop/login?app=pass#payload=%zz".to_owned()),
            ("truncated escape", "https://account.proton.me/desktop/login?app=pass#payload=%4".to_owned()),
            ("other client", real_link(&plain("0", "CODE", &KEY, "cli-drive"))),
            ("version 1", real_link(&plain("1", "CODE", &KEY, "cli-pass"))),
            ("key too short", real_link(&plain("0", "CODE", &KEY[..31], "cli-pass"))),
            ("key too long", real_link(&plain("0", "CODE", &[0xfb; 33], "cli-pass"))),
            ("empty code", real_link(&plain("0", "", &KEY, "cli-pass"))),
            ("space in code", real_link(&plain("0", "AB CD", &KEY, "cli-pass"))),
            ("slash in code", real_link(&plain("0", "AB/CD", &KEY, "cli-pass"))),
            ("non-ascii code", real_link(&plain("0", "ÄB", &KEY, "cli-pass"))),
            ("extra field", real_link(&format!("{}:extra", plain("0", "CODE", &KEY, "cli-pass")))),
        ];
        for (why, url) in refused {
            assert!(parse_pass_login_url(&url).is_none(), "accepted: {why}");
        }
    }

    #[test]
    fn plus_signs_stay_plus_signs_when_decoding() {
        assert_eq!(percent_decode("a+b%2Bc").as_deref(), Some("a+b+c"));
        assert_eq!(percent_decode("%3A%2f%3D").as_deref(), Some(":/="));
        assert!(percent_decode("%FF").is_none(), "not UTF-8");
    }

    #[test]
    fn payload_opens_the_way_pass_cli_opens_it() {
        let login = PassLogin { user_code: "CODE".into(), key: Zeroizing::new(KEY) };
        let password = "key password with spaces and ü";
        let payload = pass_fork_payload(&login, password).unwrap();

        // pass-cli: base64, then a 16-byte nonce, then AES-256-GCM with no AAD.
        let raw = STANDARD.decode(&payload).unwrap();
        let (nonce, sealed) = raw.split_at(16);
        let cipher = aes_gcm::AesGcm::<aes_gcm::aes::Aes256, U16>::new_from_slice(&KEY).unwrap();
        let opened = cipher.decrypt(aes_gcm::Nonce::<U16>::from_slice(nonce), sealed).expect("pass-cli can open it");
        let json: Value = serde_json::from_slice(&opened).unwrap();
        assert_eq!(json, json!({ "keyPassword": password }));

        // A fresh nonce every time, and nobody else's key opens it.
        assert_ne!(payload, pass_fork_payload(&login, password).unwrap());
        let other = aes_gcm::AesGcm::<aes_gcm::aes::Aes256, U16>::new_from_slice(&[1u8; 32]).unwrap();
        assert!(other.decrypt(aes_gcm::Nonce::<U16>::from_slice(nonce), sealed).is_err());
    }
}
