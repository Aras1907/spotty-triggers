//! Proton's HTTP API: JSON requests with the session headers, automatic token
//! refresh and Proton's error format. Blocking; call it from a worker thread.

use crate::error::{Error, Result};
use reqwest::Method;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

/// Identifies Spotty to Proton honestly. Proton's own apps use their own ids;
/// this one is for third-party Drive clients.
pub const APP_VERSION: &str = concat!("external-drive-spotty@", env!("CARGO_PKG_VERSION"), "-stable");
const BASE_URL: &str = "https://drive-api.proton.me";

/// The three values that make up a Proton session.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tokens {
    pub uid: String,
    pub access: String,
    pub refresh: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens(..)")
    }
}

type TokenSink = Box<dyn Fn(&Tokens) + Send + Sync>;

pub struct Api {
    http: Client,
    base: String,
    tokens: Mutex<Option<Tokens>>,
    refresh: Mutex<()>,
    on_refresh: Mutex<Option<TokenSink>>,
}

impl Api {
    pub fn new() -> Result<Self> {
        Self::with_base(BASE_URL)
    }

    pub fn with_base(base: &str) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("Spotty/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .https_only(base.starts_with("https://"))
            .build()
            .map_err(|e| Error::Network(e.to_string()))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            tokens: Mutex::new(None),
            refresh: Mutex::new(()),
            on_refresh: Mutex::new(None),
        })
    }

    pub fn set_tokens(&self, tokens: Option<Tokens>) {
        *self.tokens.lock().unwrap() = tokens;
    }

    pub fn tokens(&self) -> Option<Tokens> {
        self.tokens.lock().unwrap().clone()
    }

    /// Called whenever Proton hands out fresh tokens, so they can be saved.
    pub fn on_refresh(&self, sink: impl Fn(&Tokens) + Send + Sync + 'static) {
        *self.on_refresh.lock().unwrap() = Some(Box::new(sink));
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(Method::GET, path, query, None, true)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(Method::POST, path, &[], Some(body), true)
    }

    /// A request that carries no session (sign-in itself).
    pub fn anonymous_post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(Method::POST, path, &[], Some(body), false)
    }

    /// A request made with explicit tokens, e.g. the half-finished session
    /// between the password and the two-factor code.
    pub fn send_as(&self, tokens: &Tokens, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        parse(self.request(method, path, &[], body, Some(tokens)).send().map_err(network)?)
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        tokens: Option<&Tokens>,
    ) -> reqwest::blocking::RequestBuilder {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut request = self
            .http
            .request(method, url)
            .header("x-pm-appversion", APP_VERSION)
            .header("Accept", "application/vnd.protonmail.v1+json");
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(tokens) = tokens {
            request = request.header("x-pm-uid", &tokens.uid).bearer_auth(&tokens.access);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        request
    }

    pub fn send(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, auth: bool) -> Result<Value> {
        let tokens = if auth { Some(self.tokens().ok_or(Error::SignedOut)?) } else { None };
        let response = self.request(method.clone(), path, query, body, tokens.as_ref()).send().map_err(network)?;
        if auth && response.status().as_u16() == 401 {
            let rejected = tokens.expect("authenticated request has tokens");
            self.refresh_session(&rejected)?;
            let fresh = self.tokens().ok_or(Error::SignedOut)?;
            return parse(self.request(method, path, query, body, Some(&fresh)).send().map_err(network)?);
        }
        parse(response)
    }

    /// Swap the refresh token for a new access token. Only one thread does it;
    /// the others find the work already done.
    fn refresh_session(&self, rejected: &Tokens) -> Result<()> {
        let _guard = self.refresh.lock().unwrap();
        let current = self.tokens().ok_or(Error::SignedOut)?;
        if current.access != rejected.access {
            return Ok(());
        }
        let body = json!({
            "ResponseType": "token",
            "GrantType": "refresh_token",
            "RefreshToken": current.refresh,
        });
        let response = self
            .request(Method::POST, "auth/v4/refresh", &[], Some(&body), Some(&current))
            .send()
            .map_err(network)?;
        let status = response.status().as_u16();
        match parse(response) {
            Ok(value) => {
                let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
                let access = text("AccessToken").ok_or_else(|| Error::Protocol("refresh without a token".into()))?;
                let fresh = Tokens {
                    uid: text("UID").unwrap_or(current.uid),
                    access,
                    refresh: text("RefreshToken").unwrap_or(current.refresh),
                };
                self.set_tokens(Some(fresh.clone()));
                if let Some(sink) = self.on_refresh.lock().unwrap().as_ref() {
                    sink(&fresh);
                }
                Ok(())
            }
            Err(error) => {
                // A refusal (not a hiccup) means Proton ended the session.
                if (400..500).contains(&status) && status != 429 {
                    self.set_tokens(None);
                    return Err(Error::SignedOut);
                }
                Err(error)
            }
        }
    }

    /// Fetch an encrypted Drive block from Proton's storage.
    pub fn download(&self, url: &str, token: &str) -> Result<Vec<u8>> {
        // Storage is always https; only a test server on the API's own host may be plain.
        if !url.starts_with("https://") && !(!self.base.starts_with("https://") && url.starts_with(&self.base)) {
            return Err(Error::Protocol("storage address isn't https".into()));
        }
        let response = self
            .http
            .get(url)
            .header("x-pm-appversion", APP_VERSION)
            .header("pm-storage-token", token)
            .send()
            .map_err(network)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Api { status: status.as_u16(), code: 0, message: format!("storage returned HTTP {status}") });
        }
        response.bytes().map(|b| b.to_vec()).map_err(network)
    }
}

fn network(error: reqwest::Error) -> Error {
    // reqwest's text can include the URL; never include query strings.
    Error::Network(error.without_url().to_string())
}

fn parse(response: reqwest::blocking::Response) -> Result<Value> {
    let status = response.status().as_u16();
    let body = response.text().map_err(network)?;
    parse_body(status, &body)
}

/// Turn a status and body into the JSON payload or Proton's error.
pub(crate) fn parse_body(status: u16, body: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(body).map_err(|_| {
        if (200..300).contains(&status) {
            Error::Protocol("answer wasn't JSON".into())
        } else {
            Error::Api { status, code: 0, message: format!("Proton returned HTTP {status}") }
        }
    })?;
    let code = value.get("Code").and_then(Value::as_i64).unwrap_or(0);
    if (200..300).contains(&status) && matches!(code, 1000 | 1001) {
        return Ok(value);
    }
    let message = value.get("Error").and_then(Value::as_str).unwrap_or_default().to_owned();
    Err(Error::Api { status, code, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_error_bodies_are_told_apart() {
        let ok = parse_body(200, r#"{"Code":1000,"Volumes":[]}"#).unwrap();
        assert!(ok.get("Volumes").is_some());
        let multi = parse_body(200, r#"{"Code":1001,"Responses":[]}"#);
        assert!(multi.is_ok());

        match parse_body(422, r#"{"Code":8002,"Error":"Incorrect login credentials. Please try again.","Details":{}}"#) {
            Err(Error::Api { status: 422, code: 8002, message }) => assert!(message.contains("Incorrect")),
            other => panic!("{other:?}"),
        }
        assert!(parse_body(401, r#"{"Code":401,"Error":"Invalid access token"}"#).unwrap_err().is_signed_out());
        assert!(matches!(parse_body(502, "<html>"), Err(Error::Api { status: 502, .. })));
        assert!(matches!(parse_body(200, "<html>"), Err(Error::Protocol(_))));
    }

    #[test]
    fn tokens_never_print_their_values() {
        let tokens = Tokens { uid: "u".into(), access: "secret-a".into(), refresh: "secret-r".into() };
        assert!(!format!("{tokens:?}").contains("secret"));
    }
}
