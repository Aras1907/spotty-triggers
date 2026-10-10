//! One Proton sign-in for the Proton integrations that can share it.
//!
//! Spotty signs in to Proton once, natively, from the Proton account window
//! (`crate::ui::proton_native_ui::open_account_window`). Calendar and Drive use
//! that session directly. Proton Pass and Proton VPN are Proton's own clients,
//! so each gets a session of its own: a child session forked from the native
//! one (Proton's "session fork"), handed to the client. No password is shared,
//! and signing out of the native session ends those children as well. Proton
//! Mail Bridge keeps its own sign-in; Spotty never keeps a Proton password to
//! sign it in for you.
//!
//! Everything here that talks to Proton blocks: the entry points only start
//! background work, except `hand_to_vpn`, which the caller runs off the GTK
//! thread.

use crate::i18n::gettext;
use crate::proton_native as native;
use crate::proton_pass::State;
use crate::proton_vpn::LoginStep;
use spotty_proton_account::Zeroizing;
use std::sync::Mutex;

/// The integrations that take a session forked from the native one.
const SHARING: [&str; 2] = ["proton-pass", "proton-vpn"];

/// Called right after the Store installs a Proton integration. Returns true
/// when the integration is using the Proton sign-in that already exists.
pub fn after_install(id: &str) -> bool {
    match id {
        // Same native session: already signed in.
        "proton-calendar" | "proton-drive" => native::signed_in(),
        "proton-pass" if native::signed_in() && crate::proton_pass::available() => {
            crate::proton_pass::sign_in();
            true
        }
        "proton-vpn" if native::signed_in() && crate::proton_vpn::available() => {
            share_with_vpn();
            true
        }
        _ => false,
    }
}

/// After a native sign-in: give every installed Proton app that is not signed
/// in yet the same sign-in. Calendar and Drive need nothing more.
pub fn share_sign_in() {
    let Some(config) = crate::app::shared_config() else { return };
    let installed: Vec<&str> = {
        let config = config.borrow();
        SHARING.iter().copied().filter(|id| config.proton_service_enabled(id)).collect()
    };
    for id in installed {
        if !app_signed_in(id) && !signing_in(id) {
            share_with(id);
        }
    }
}

/// Automatic sharing: once per app per run, so signing out of one app on
/// purpose is never undone behind the user's back.
static AUTO_TRIED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

fn first_auto_try(id: &'static str) -> bool {
    let mut tried = AUTO_TRIED.lock().unwrap_or_else(|p| p.into_inner());
    if tried.contains(&id) {
        return false;
    }
    tried.push(id);
    true
}

/// Run on the GTK thread when Spotty starts (and again when an app reports it
/// is signed out): if Spotty is signed in to Proton, give that sign-in to every
/// installed app that turns out not to be signed in, without any click.
pub fn share_on_start() {
    if !native::signed_in() {
        return;
    }
    let Some(config) = crate::app::shared_config() else { return };
    let (pass, vpn) = {
        let config = config.borrow();
        (config.proton_service_enabled("proton-pass"), config.proton_service_enabled("proton-vpn"))
    };
    if pass && crate::proton_pass::available() {
        // Pass reports "signed out" once it has looked; `share_pass_if_signed_out` then acts.
        crate::proton_pass::ensure_loaded();
    }
    if vpn && crate::proton_vpn::available() && first_auto_try("proton-vpn") && !signing_in("proton-vpn") {
        std::thread::spawn(|| {
            // Ask the client; only a definite "not signed in" gets the shared sign-in.
            if crate::proton_vpn::status().is_ok_and(|s| !s.logged_in) {
                match hand_to_vpn() {
                    Ok(()) => notify(&gettext("Proton VPN"), &gettext("Proton VPN is signed in with your Proton account.")),
                    Err(error) => log::warn!("Proton VPN couldn't use the shared sign-in: {error}"),
                }
            }
        });
    }
}

/// Called when Proton Pass has looked and is signed out.
pub fn share_pass_if_signed_out() {
    if native::signed_in() && first_auto_try("proton-pass") && !signing_in("proton-pass") {
        glib::MainContext::default().invoke(|| crate::proton_pass::sign_in());
    }
}

/// Give one installed integration the Proton sign-in that already exists. Returns
/// at once; the work runs in the background and reports its own outcome.
pub fn share_with(id: &str) {
    match id {
        "proton-pass" => crate::proton_pass::sign_in(),
        "proton-vpn" => share_with_vpn(),
        _ => {}
    }
}

/// Sign Proton VPN in with the native session, in the background, and tell the
/// user how it went.
pub fn share_with_vpn() {
    if !crate::proton_vpn::available() || !native::signed_in() {
        return;
    }
    std::thread::spawn(|| match hand_to_vpn() {
        Ok(()) => notify(&gettext("Proton VPN"), &gettext("Proton VPN is signed in with your Proton account.")),
        Err(error) => notify(
            &gettext("Proton VPN"),
            &gettext("Proton VPN couldn't use your Proton sign-in: {error}. Sign in from the Proton VPN window.")
                .replace("{error}", error.trim_end_matches('.')),
        ),
    });
}

/// Fork a Proton VPN session from the native one and sign the VPN client in
/// with it. Blocks on the network: run it off the GTK thread. The selector is a
/// one-time handle; it is never stored or logged.
pub fn hand_to_vpn() -> Result<(), String> {
    let _signing = SigningIn::start("proton-vpn");
    let client = native::client().ok_or_else(|| gettext("Sign in to Proton first."))?;
    let selector = Zeroizing::new(client.fork_for_vpn().map_err(|e| e.to_string())?);
    match crate::proton_vpn::import_fork(&selector, &client.email)? {
        LoginStep::Done => Ok(()),
        LoginStep::TwoFactor => Err(gettext("it asked for a two-factor code")),
        LoginStep::Failed(message) => Err(message),
    }
}

/// Whether an integration already has a working Proton sign-in. Cheap: it reads
/// what the integration last reported and never starts a client.
pub fn app_signed_in(id: &str) -> bool {
    match id {
        "proton-pass" => {
            let state = crate::proton_pass::state();
            matches!(state, State::Ready) || (matches!(state, State::Loading) && !crate::proton_pass::account().is_empty())
        }
        "proton-vpn" => crate::proton_vpn::cached_status().is_some_and(|s| s.logged_in),
        _ => native::signed_in(),
    }
}

// ── Signing in, as seen by the UI ───────────────────────────────────────────

static SIGNING_IN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Marks an integration as signing in for as long as the guard lives.
pub(crate) struct SigningIn(&'static str);

impl SigningIn {
    pub(crate) fn start(id: &'static str) -> Self {
        SIGNING_IN.lock().unwrap_or_else(|p| p.into_inner()).push(id);
        Self(id)
    }
}

impl Drop for SigningIn {
    fn drop(&mut self) {
        let mut busy = SIGNING_IN.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(at) = busy.iter().position(|id| *id == self.0) {
            busy.remove(at);
        }
    }
}

/// Whether `id` is signing in right now.
pub fn signing_in(id: &str) -> bool {
    SIGNING_IN.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|busy| *busy == id)
}

fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_owned(), body.to_owned());
    glib::MainContext::default().invoke(move || {
        if let Some(app) = gio::Application::default() {
            let notification = gio::Notification::new(&title);
            notification.set_body(Some(&body));
            gio::prelude::ApplicationExt::send_notification(&app, Some("proton-vpn"), &notification);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses ids of its own: the tests run in parallel and share the list.
    #[test]
    fn signing_in_is_marked_for_the_life_of_the_guard() {
        assert!(!signing_in("test-one"));
        let guard = SigningIn::start("test-one");
        assert!(signing_in("test-one"));
        drop(guard);
        assert!(!signing_in("test-one"));
    }

    #[test]
    fn overlapping_sign_ins_count_separately() {
        let first = SigningIn::start("test-two");
        let second = SigningIn::start("test-two");
        drop(first);
        assert!(signing_in("test-two"), "the second sign-in is still running");
        drop(second);
        assert!(!signing_in("test-two"));
    }
}
