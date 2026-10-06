//! Private runtime files needed by the in-process Bridge Go shared library.
//! No Proton executable or Qt payload is extracted or launched.

use std::path::{Path, PathBuf};

#[cfg(feature = "bundled-bridge")]
const PACKAGE_ID: &str = concat!("3.27.0-inprocess-", env!("SPOTTY_BRIDGE_LIBRARY_SHA256"));
#[cfg(feature = "bundled-bridge")]
const LEGACY_PACKAGE_ID: &str = "3.27.0-4ae1f9a38392379a-fido1.15-startup1";

#[cfg(feature = "bundled-bridge")]
fn data_dir() -> Result<PathBuf, String> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(value) if PathBuf::from(&value).is_absolute() => Ok(PathBuf::from(value)),
        _ => std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".local/share"))
            .ok_or_else(|| "Cannot locate the user data folder.".into()),
    }
}

/// Location used by the previous detached backend package, for the explicit
/// one-time migration command only.
pub(crate) fn legacy_runtime_root() -> Result<PathBuf, String> {
    #[cfg(feature = "bundled-bridge")]
    {
        return Ok(data_dir()?
            .join("spotty/proton-bridge")
            .join(LEGACY_PACKAGE_ID));
    }
    #[cfg(not(feature = "bundled-bridge"))]
    Err("This build cannot identify the old bundled Bridge runtime.".into())
}

/// Return the dynamically loaded Go adapter from Spotty's owner-private cache.
pub fn inprocess_library() -> Result<PathBuf, String> {
    #[cfg(feature = "bundled-bridge")]
    {
        let root = install_in(&data_dir()?.join("spotty/proton-bridge"))?;
        return Ok(root.join("libspotty_proton_bridge.so"));
    }
    #[cfg(not(feature = "bundled-bridge"))]
    Err("This build does not include the in-process Bridge runtime.".into())
}

/// The Go adapter uses this path as the desktop-login entry point. It only
/// starts Spotty's daemon mode, which loads Bridge in-process.
pub fn launcher_argument() -> Result<PathBuf, String> {
    #[cfg(feature = "bundled-bridge")]
    {
        let root = install_in(&data_dir()?.join("spotty/proton-bridge"))?;
        write_startup_wrapper(&root)?;
        return Ok(root.join("spotty-bridge-launcher"));
    }
    #[cfg(not(feature = "bundled-bridge"))]
    Err("This build does not include the in-process Bridge runtime.".into())
}

#[cfg(feature = "bundled-bridge")]
fn write_startup_wrapper(root: &Path) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let spotty = std::env::current_exe()
        .map_err(|_| "Cannot locate Spotty for Bridge desktop startup.")?
        .to_string_lossy()
        .replace('\'', "'\\''");
    let command = std::env::var("FLATPAK_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .map(|id| {
            let id = id.replace('\'', "'\\''");
            format!("exec flatpak run --command=spotty '{id}' --daemon")
        })
        .unwrap_or_else(|| format!("exec '{spotty}' --daemon"));
    let script = format!("#!/bin/sh\n{command}\n");
    let mut file = tempfile::NamedTempFile::new_in(root)
        .map_err(|_| "Cannot create the Bridge desktop-startup wrapper.")?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Cannot secure the Bridge desktop-startup wrapper.")?;
    file.write_all(script.as_bytes())
        .map_err(|_| "Cannot write the Bridge desktop-startup wrapper.")?;
    file.persist(root.join("spotty-bridge-launcher"))
        .map_err(|_| "Cannot install the Bridge desktop-startup wrapper.")?;
    Ok(())
}

#[cfg(feature = "bundled-bridge")]
pub fn install_in(parent: &Path) -> Result<PathBuf, String> {
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
        if root.exists() {
            let metadata = std::fs::symlink_metadata(&root)?;
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || std::fs::read_to_string(root.join(".ready"))? != PACKAGE_ID
                || !root.join("libspotty_proton_bridge.so").is_file()
                || !root.join("runtime-libs").is_dir()
            {
                return Err("Incomplete or unsafe in-process Bridge runtime".into());
            }
            return Ok(root);
        }

        let temporary = tempfile::Builder::new()
            .prefix(".install-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        std::fs::create_dir_all(temporary.path().join("runtime-libs"))?;
        let library = include_bytes!(concat!(env!("OUT_DIR"), "/libspotty_proton_bridge.so"));
        std::fs::write(temporary.path().join("libspotty_proton_bridge.so"), library)?;
        std::fs::set_permissions(
            temporary.path().join("libspotty_proton_bridge.so"),
            std::fs::Permissions::from_mode(0o600),
        )?;

        // Preserve the pinned source archives and licence notices from the
        // verified dependency archive. Only runtime libraries are loaded.
        let dependencies = include_bytes!(concat!(env!("OUT_DIR"), "/bridge-dependencies.tar.gz"));
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&dependencies[..]));
        for entry in archive.entries()? {
            if !entry?.unpack_in(temporary.path())? {
                return Err("Invalid path in Bridge runtime dependencies".into());
            }
        }
        let sources = temporary.path().join("source");
        std::fs::create_dir_all(&sources)?;
        std::fs::write(
            sources.join("proton-bridge-3.27.0.tar.gz"),
            include_bytes!(concat!(env!("OUT_DIR"), "/bridge-source.tar.gz")),
        )?;
        let adapter = sources.join("spotty-inprocess");
        std::fs::create_dir_all(&adapter)?;
        for (name, contents) in [
            (
                "main.go",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/inprocess/main.go"))
                    .as_slice(),
            ),
            (
                "main_test.go",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/inprocess/main_test.go"
                ))
                .as_slice(),
            ),
            (
                "embedded_app.go",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/inprocess/embedded_app.go"
                ))
                .as_slice(),
            ),
            (
                "build_inprocess.py",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/build_inprocess.py"))
                    .as_slice(),
            ),
            (
                "prepare_bundle.py",
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/prepare_bundle.py"))
                    .as_slice(),
            ),
            (
                "native_dependencies.json",
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/native_dependencies.json"
                ))
                .as_slice(),
            ),
        ] {
            std::fs::write(adapter.join(name), contents)?;
        }
        // Carry forward Proton's documentation and licence notices while
        // omitting the executable and Qt trees from the runtime cache.
        let payload = include_bytes!(concat!(env!("OUT_DIR"), "/bridge-payload.tar.gz"));
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&payload[..]));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()?.starts_with("usr/share/doc") && !entry.unpack_in(temporary.path())? {
                return Err("Invalid path in Bridge documentation archive".into());
            }
        }
        std::fs::write(temporary.path().join(".ready"), PACKAGE_ID)?;
        match std::fs::rename(temporary.path(), &root) {
            Ok(()) => {}
            Err(_) if root.join(".ready").is_file() => {}
            Err(error) => return Err(error.into()),
        }
        Ok(root)
    };
    install().map_err(|_| {
        "Cannot activate the in-process Bridge runtime in your private data folder. Check its permissions and free disk space.".into()
    })
}

#[cfg(all(test, feature = "bundled-bridge"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn installs_only_inprocess_library_and_runtime_dependencies_privately() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = install_in(directory.path()).unwrap();
        assert!(root.join("libspotty_proton_bridge.so").is_file());
        assert!(root.join("runtime-libs").is_dir());
        assert!(root.join("source/proton-bridge-3.27.0.tar.gz").is_file());
        assert!(
            root.join("source/spotty-inprocess/build_inprocess.py")
                .is_file()
        );
        assert!(root.join("runtime-licences").is_dir());
        assert!(
            !root
                .join("usr/lib/protonmail/bridge/proton-bridge")
                .exists()
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        write_startup_wrapper(&root).unwrap();
        let wrapper = root.join("spotty-bridge-launcher");
        assert!(wrapper.is_file());
        let text = std::fs::read_to_string(wrapper).unwrap();
        assert!(text.contains("--daemon"));
        assert!(!text.contains("proton-bridge\""));
    }
}
