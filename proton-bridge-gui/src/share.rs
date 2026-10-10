//! Lets the app that embeds Bridge follow a Bridge sign-in with a sign-in of
//! its own, from the same details the user typed. Bridge itself never hands out
//! its session; this only repeats what the user entered, as it is entered.
//!
//! Secrets are passed by reference for the length of the call and never logged
//! here. `Finished` comes once Bridge's own sign-in has succeeded, so an
//! observer that reuses a one-time code can never break Bridge's sign-in.
use crate::rpc::LoginMethod;
use std::sync::OnceLock;

pub enum LoginEvent<'a> {
    /// The user entered a sign-in detail for Bridge.
    Entered { method: LoginMethod, username: &'a str, secret: &'a str },
    /// Bridge is signed in.
    Finished,
    /// The sign-in was cancelled or its window closed.
    Abandoned,
}

type Observer = Box<dyn Fn(LoginEvent<'_>) + Send + Sync>;

static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Set once, at startup. Later calls are ignored.
pub fn set_login_observer(observer: impl Fn(LoginEvent<'_>) + Send + Sync + 'static) {
    let _ = OBSERVER.set(Box::new(observer));
}

pub(crate) fn notify(event: LoginEvent<'_>) {
    if let Some(observer) = OBSERVER.get() {
        observer(event);
    }
}

static ACCOUNT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The address of the account Bridge last showed as signed in (memory only).
pub fn account_email() -> Option<String> {
    ACCOUNT.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub(crate) fn remember_account(settings: &crate::session::MailSettings) {
    let email = settings.addresses.first().cloned().unwrap_or_else(|| settings.username.clone());
    if !email.is_empty() {
        *ACCOUNT.lock().unwrap_or_else(|p| p.into_inner()) = Some(email);
    }
}
