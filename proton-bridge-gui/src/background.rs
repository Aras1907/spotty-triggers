//! Flatpak desktop-login integration through the XDG Background portal.
//! Native Bridge autostart remains managed by Bridge's own RPC.

use gtk::gio;
use gtk::glib::{self, VariantDict, variant::ToVariant};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const BACKGROUND_IFACE: &str = "org.freedesktop.portal.Background";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

pub fn is_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").is_file()
}

/// The marker records an accepted portal grant; it contains no account data.
/// Spotty's hidden startup path uses this to decide whether to start Bridge.
pub fn autostart_enabled() -> bool {
    marker_path()
        .and_then(|path| std::fs::read(path).map_err(|error| error.to_string()))
        .is_ok_and(|contents| contents == b"enabled\n")
}

pub fn set_autostart_marker(enabled: bool) -> Result<(), String> {
    let path = marker_path()?;
    if !enabled {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Cannot save the Bridge desktop-login setting.".into()),
        };
    }

    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let parent = path
        .parent()
        .ok_or("Cannot locate the Bridge settings folder.")?;
    std::fs::create_dir_all(parent).map_err(|_| "Cannot save the Bridge desktop-login setting.")?;
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|_| "Cannot save the Bridge desktop-login setting.")?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err("Cannot save the Bridge desktop-login setting safely.".into());
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Cannot save the Bridge desktop-login setting.")?;

    let mut file = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(parent)
        .map_err(|_| "Cannot save the Bridge desktop-login setting.")?;
    std::io::Write::write_all(&mut file, b"enabled\n")
        .map_err(|_| "Cannot save the Bridge desktop-login setting.")?;
    file.persist(path)
        .map(|_| ())
        .map_err(|_| "Cannot save the Bridge desktop-login setting.".into())
}

/// Ask the desktop to grant or revoke background and login-start permission.
/// The caller updates the marker only after this request succeeds.
pub async fn request_autostart(enabled: bool) -> Result<(), String> {
    if !is_flatpak() {
        return Err("The desktop background portal is only available in Flatpak.".into());
    }
    tokio::task::spawn_blocking(move || request_autostart_sync(enabled))
        .await
        .map_err(|_| "The desktop background request could not be completed.".to_owned())?
}

fn marker_path() -> Result<PathBuf, String> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) if PathBuf::from(&path).is_absolute() => PathBuf::from(path),
        Some(_) => return Err("Cannot locate the Bridge settings folder.".into()),
        None => PathBuf::from(
            std::env::var_os("HOME").ok_or("Cannot locate the Bridge settings folder.")?,
        )
        .join(".config"),
    };
    Ok(config.join("spotty/bridge-autostart"))
}

fn request_autostart_sync(enabled: bool) -> Result<(), String> {
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| request_autostart_on_context(enabled, &context))
        .map_err(|_| "Cannot start the desktop background request.".to_owned())?
}

fn request_autostart_on_context(enabled: bool, context: &glib::MainContext) -> Result<(), String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|_| "Cannot reach the desktop background portal.".to_owned())?;
    let (response_sender, response_receiver) = mpsc::channel();
    let subscription = connection.signal_subscribe(
        Some(PORTAL_NAME),
        Some(REQUEST_IFACE),
        Some("Response"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, object_path, _, _, parameters| {
            let _ = response_sender.send((object_path.to_owned(), parameters.to_owned()));
        },
    );

    let result = (|| {
        let parameters = request_parameters(enabled);
        let reply = connection
            .call_sync(
                Some(PORTAL_NAME),
                PORTAL_PATH,
                BACKGROUND_IFACE,
                "RequestBackground",
                Some(&parameters),
                None,
                gio::DBusCallFlags::NONE,
                30_000,
                None::<&gio::Cancellable>,
            )
            .map_err(|_| "Cannot request desktop-login access for Bridge.".to_owned())?;
        let handle = reply
            .child_value(0)
            .get::<glib::variant::ObjectPath>()
            .map(|path| path.as_str().to_owned())
            .ok_or("The desktop returned an invalid background request.")?;

        let deadline = std::time::Instant::now() + Duration::from_secs(300);
        loop {
            while let Ok((path, response)) = response_receiver.try_recv() {
                if path != handle {
                    continue;
                }
                return parse_response(&response, enabled);
            }
            if std::time::Instant::now() >= deadline {
                let _ = connection.call_sync(
                    Some(PORTAL_NAME),
                    &handle,
                    REQUEST_IFACE,
                    "Close",
                    None,
                    None,
                    gio::DBusCallFlags::NONE,
                    1_000,
                    None::<&gio::Cancellable>,
                );
                return Err(
                    "The desktop background request timed out. Bridge startup was not changed."
                        .into(),
                );
            }
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(20));
        }
    })();

    connection.signal_unsubscribe(subscription);
    result
}

fn request_parameters(enabled: bool) -> glib::Variant {
    let token = format!("spotty_{}_{}", std::process::id(), request_token());
    let options = VariantDict::new(None);
    options.insert("handle_token", token);
    options.insert(
        "reason",
        "Keep Proton Mail Bridge available for mail clients.",
    );
    options.insert("autostart", enabled);
    options.insert("commandline", flatpak_commandline());
    options.insert("dbus-activatable", false);
    glib::Variant::tuple_from_iter(["".to_variant(), options.end()])
}

fn parse_response(response: &glib::Variant, enabled: bool) -> Result<(), String> {
    let status = response.child_value(0).get::<u32>().unwrap_or(2);
    if status != 0 {
        return Err(if status == 1 {
            "Desktop-login access was cancelled. Bridge startup was not changed.".into()
        } else {
            "The desktop did not grant Bridge desktop-login access.".into()
        });
    }
    let results = VariantDict::new(Some(&response.child_value(1)));
    let background = results
        .lookup::<bool>("background")
        .ok()
        .flatten()
        .unwrap_or(false);
    let autostart = results
        .lookup::<bool>("autostart")
        .ok()
        .flatten()
        .unwrap_or(false);
    if enabled && (!background || !autostart) {
        return Err("The desktop did not allow Bridge to run in the background at login.".into());
    }
    if !enabled && autostart {
        return Err("The desktop did not disable Bridge startup at login.".into());
    }
    Ok(())
}

fn flatpak_commandline() -> Vec<String> {
    ["spotty", "--proton-bridge-gui", "--background"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn request_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_request_uses_background_protocol_tuple_and_flatpak_command() {
        let request = request_parameters(true);
        assert_eq!(request.type_().as_str(), "(sa{sv})");
        assert_eq!(
            flatpak_commandline(),
            [
                "spotty".to_owned(),
                "--proton-bridge-gui".to_owned(),
                "--background".to_owned()
            ]
        );
    }

    #[test]
    fn portal_acceptance_requires_background_and_autostart() {
        let accepted = response_variant(0, true, true);
        assert!(parse_response(&accepted, true).is_ok());

        let background_only = response_variant(0, true, false);
        assert!(parse_response(&background_only, true).is_err());

        let denied = response_variant(1, false, false);
        assert!(
            parse_response(&denied, true)
                .unwrap_err()
                .contains("cancelled")
        );

        let disabled = response_variant(0, false, false);
        assert!(parse_response(&disabled, false).is_ok());
    }

    fn response_variant(status: u32, background: bool, autostart: bool) -> glib::Variant {
        let results = VariantDict::new(None);
        results.insert("background", background);
        results.insert("autostart", autostart);
        glib::Variant::tuple_from_iter([status.to_variant(), results.end()])
    }
}
