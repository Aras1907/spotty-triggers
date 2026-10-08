//! One Proton sign-in for every Proton integration in Spotty.
//!
//! Calendar, Drive and Pass's web sign-in share the private Proton web profile
//! (`crate::proton_web`), so once you are signed in to any of them a newly
//! installed one needs no second password: the web apps simply find the
//! session, and Proton Pass's client signs in through that same window.
//! Proton VPN and Proton Mail Bridge are separate clients with their own
//! sign-in; Spotty never keeps a Proton password to sign them in for you.

/// Called right after the Store installs a Proton integration. Returns true
/// when the integration is using the Proton sign-in that already exists.
pub fn after_install(id: &str) -> bool {
    if !crate::proton_web::signed_in() {
        return false;
    }
    match id {
        // Same web profile: already signed in.
        "proton-calendar" | "proton-drive" => true,
        "proton-pass" if crate::proton_pass::available() => {
            crate::proton_pass::sign_in();
            true
        }
        _ => false,
    }
}
