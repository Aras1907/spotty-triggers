//! Which Proton apps are signing in, and whether each one is signed in.
//!
//! Every Proton app in Spotty has a sign-in of its own: Calendar and Drive
//! through Spotty's native client (`crate::proton_native`), Pass through
//! Proton's own `pass-cli`, VPN through Proton's VPN library and Mail Bridge
//! through its engine. No session is shared between them, no password is
//! reused for another app, and signing out of one never touches another.

use crate::proton_native as native;
use std::sync::Mutex;

/// Called right after the Store installs a Proton integration. Returns true
/// when the integration already has a working sign-in of its own.
pub fn after_install(id: &str) -> bool {
    app_signed_in(id)
}

/// Whether an integration already has a working sign-in. Cheap: it reads what
/// the integration last reported and never starts a client.
pub fn app_signed_in(id: &str) -> bool {
    match id {
        "proton-pass" => crate::proton_pass::state() == crate::proton_pass::State::Ready,
        "proton-vpn" => crate::proton_vpn::cached_status().is_some_and(|s| s.logged_in),
        "proton-calendar" => native::signed_in(native::Service::Calendar),
        "proton-drive" => native::signed_in(native::Service::Drive),
        _ => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses ids of its own: the tests run in parallel and share the list.
    fn busy(id: &str) -> usize {
        SIGNING_IN.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|busy| **busy == id).count()
    }

    #[test]
    fn signing_in_is_marked_for_the_life_of_the_guard() {
        assert_eq!(busy("test-one"), 0);
        let guard = SigningIn::start("test-one");
        assert_eq!(busy("test-one"), 1);
        drop(guard);
        assert_eq!(busy("test-one"), 0);
    }

    #[test]
    fn overlapping_sign_ins_count_separately() {
        let first = SigningIn::start("test-two");
        let second = SigningIn::start("test-two");
        drop(first);
        assert_eq!(busy("test-two"), 1, "the second sign-in is still running");
        drop(second);
        assert_eq!(busy("test-two"), 0);
    }
}
