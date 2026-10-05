//! The native runtime is embedded at build time, activated only on opt-in.
//! No writes to /usr, no package manager, and no credentials in this cache.
use std::path::PathBuf;

pub const VERSION: &str = "3.27.0";

#[cfg(feature = "bundled-bridge")]
const PACKAGE_ID: &str = "3.27.0-4ae1f9a38392379a-fido1.15-startup1";

pub fn autostart_launcher(launcher: &std::path::Path) -> PathBuf {
    #[cfg(feature = "bundled-bridge")]
    if let Some(root) = launcher
        .parent()
        .and_then(|directory| directory.ancestors().nth(4))
        && std::fs::read_to_string(root.join(".ready")).ok().as_deref() == Some(PACKAGE_ID)
    {
        return root.join("spotty-bridge-launcher");
    }
    launcher.into()
}

#[cfg(feature = "bundled-bridge")]
fn write_startup_launcher(root: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let spotty = std::env::current_exe()?
        .to_string_lossy()
        .replace('\'', "'\\''");
    let background_start = std::env::var("FLATPAK_ID")
        .ok()
        .filter(|app_id| !app_id.is_empty())
        .map(|app_id| {
            let app_id = app_id.replace('\'', "'\\''");
            format!("exec flatpak run --command=spotty '{app_id}' --proton-bridge-gui --background")
        })
        .unwrap_or_else(|| format!("exec '{spotty}' --proton-bridge-gui --background"));
    // Keep the bundled libraries available on desktop login. Initialize and
    // detach the frontend in the background so it can be reopened later.
    let script = format!(
        r#"#!/bin/sh
bridge_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
export LD_LIBRARY_PATH="${{bridge_root}}/runtime-libs${{LD_LIBRARY_PATH:+:${{LD_LIBRARY_PATH}}}}"
case "$1" in
  --no-window) {background_start} ;;
esac
exec "${{bridge_root}}/usr/lib/protonmail/bridge/proton-bridge" --launcher "${{bridge_root}}/spotty-bridge-launcher" "$@"
"#
    );
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    file.write_all(script.as_bytes())?;
    file.persist(root.join("spotty-bridge-launcher"))?;
    Ok(())
}

pub fn configure_libraries(launcher: &std::path::Path, command: &mut std::process::Command) {
    #[cfg(feature = "bundled-bridge")]
    if let Some(root) = launcher
        .parent()
        .and_then(|directory| directory.ancestors().nth(4))
        && std::fs::read_to_string(root.join(".ready")).ok().as_deref() == Some(PACKAGE_ID)
    {
        let mut paths = vec![root.join("runtime-libs")];
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(paths) = std::env::join_paths(paths) {
            command.env("LD_LIBRARY_PATH", paths);
        }
    }
    #[cfg(not(feature = "bundled-bridge"))]
    let _ = (launcher, command);
}

pub fn launcher() -> Result<PathBuf, String> {
    #[cfg(feature = "bundled-bridge")]
    {
        let data = match std::env::var_os("XDG_DATA_HOME") {
            Some(value) if PathBuf::from(&value).is_absolute() => PathBuf::from(value),
            _ => PathBuf::from(
                std::env::var_os("HOME").ok_or("Cannot locate the user data folder.")?,
            )
            .join(".local/share"),
        };
        install_in(&data.join("spotty/proton-bridge"))
    }
    #[cfg(not(feature = "bundled-bridge"))]
    Err("This build does not include the optional native Bridge runtime.".into())
}

#[cfg(feature = "bundled-bridge")]
pub fn install_in(parent: &std::path::Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let install = || -> Result<PathBuf, Box<dyn std::error::Error>> {
        std::fs::create_dir_all(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err("Unsafe Bridge runtime folder".into());
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        let root = parent.join(PACKAGE_ID);
        let executable = root.join("usr/lib/protonmail/bridge/proton-bridge");
        if root.exists() {
            let metadata = std::fs::symlink_metadata(&root)?;
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || std::fs::read_to_string(root.join(".ready"))? != PACKAGE_ID
                || !executable.is_file()
            {
                return Err("Incomplete or unsafe Bridge runtime".into());
            }
            write_startup_launcher(&root)?;
            return Ok(executable);
        }
        let temporary = tempfile::Builder::new()
            .prefix(".install-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let payload = include_bytes!(concat!(env!("OUT_DIR"), "/bridge-payload.tar.gz"));
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&payload[..]));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            // Keep Proton's runtime, Qt fallback and licence notices. Do not
            // install desktop entries, system-wide symlinks, or autostart.
            let path = path.strip_prefix(".").unwrap_or(&path);
            if (path.starts_with("usr/lib/protonmail/bridge") || path.starts_with("usr/share/doc"))
                && !entry.unpack_in(temporary.path())?
            {
                return Err("Invalid path in Bridge bundle".into());
            }
        }
        let sources = temporary.path().join("source");
        std::fs::create_dir(&sources)?;
        std::fs::write(
            sources.join("proton-bridge-3.27.0.tar.gz"),
            include_bytes!(concat!(env!("OUT_DIR"), "/bridge-source.tar.gz")),
        )?;
        let dependencies = include_bytes!(concat!(env!("OUT_DIR"), "/bridge-dependencies.tar.gz"));
        tar::Archive::new(flate2::read::GzDecoder::new(&dependencies[..]))
            .unpack(temporary.path())?;
        write_startup_launcher(temporary.path())?;
        std::fs::write(temporary.path().join(".ready"), PACKAGE_ID)?;
        let backend = temporary.path().join("usr/lib/protonmail/bridge/bridge");
        let launcher = temporary
            .path()
            .join("usr/lib/protonmail/bridge/proton-bridge");
        if !backend.is_file() || !launcher.is_file() {
            return Err("Bridge bundle is incomplete".into());
        }
        // Atomic activation; concurrent installers may have completed first.
        match std::fs::rename(temporary.path(), &root) {
            Ok(()) => {}
            Err(_)
                if std::fs::read_to_string(root.join(".ready")).ok().as_deref()
                    == Some(PACKAGE_ID) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(executable)
    };
    install().map_err(|_| {
         "Cannot activate the bundled Bridge in your private data folder. Check its permissions and free disk space.".into() })
}

#[cfg(all(test, feature = "bundled-bridge"))]
mod tests {
    #[test]
    fn installs_real_bundle_once_in_private_folder_with_sources_and_licence() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let launcher = super::install_in(directory.path()).unwrap();
        assert!(launcher.is_file());
        assert!(launcher.parent().unwrap().join("bridge").is_file());
        // Exercise the actual packaged executable without account/keyring access.
        let mut process = std::process::Command::new(launcher.parent().unwrap().join("bridge"));
        super::configure_libraries(&launcher, &mut process);
        let version = process
            .arg("--version")
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/no-bus", directory.path().display()),
            )
            .output()
            .unwrap();
        assert!(
            version.status.success(),
            "Native Bridge version check failed: {}",
            String::from_utf8_lossy(&version.stderr)
        );
        assert!(String::from_utf8_lossy(&version.stdout).contains(super::VERSION));
        let root = directory.path().join(super::PACKAGE_ID);
        assert!(root.join("source/proton-bridge-3.27.0.tar.gz").is_file());
        let wrapper = super::autostart_launcher(&launcher);
        assert!(
            std::fs::read_to_string(&wrapper)
                .unwrap()
                .contains("--background")
        );
        assert!(
            std::process::Command::new("sh")
                .arg("-n")
                .arg(&wrapper)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            root.join("usr/share/doc/protonmail/bridge/LICENSE")
                .is_file()
        );
        assert_eq!(super::install_in(directory.path()).unwrap(), launcher);
    }
}
