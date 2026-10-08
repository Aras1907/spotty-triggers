//! Proton's official Pass client (`pass-cli`, GPL-3.0), embedded in Spotty so
//! no separate Proton app or CLI has to be installed. See README.md.
//!
//! With the `bundled` feature the client is compiled into the binary as a
//! compressed executable; [`install`] unpacks it once into a private cache and
//! returns its path. Without the feature, [`install`] explains how to build it
//! in.

use std::path::{Path, PathBuf};

#[cfg(feature = "bundled")]
const BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pass-cli.gz"));
#[cfg(feature = "bundled")]
const BUNDLE_ID: &str = env!("SPOTTY_PROTON_PASS_ID");
#[cfg(feature = "bundled")]
const BINARY_SIZE: &str = env!("SPOTTY_PROTON_PASS_SIZE");

/// True when this build carries the embedded client.
pub const fn bundled() -> bool {
    cfg!(feature = "bundled")
}

/// Unpack the client under `cache_root` (once per bundle version) and return
/// the path of the executable. The directory is private to the user.
#[cfg(feature = "bundled")]
pub fn install(cache_root: &Path) -> Result<PathBuf, String> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

    let expected: u64 = BINARY_SIZE.parse().map_err(|_| "Invalid embedded client size".to_owned())?;
    let private_dir = |path: &Path| {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path).map_err(|e| e.to_string())
    };
    private_dir(cache_root)?;
    let dir = cache_root.join(BUNDLE_ID);
    let binary = dir.join("pass-cli");
    // Trust a previous extraction only when it still looks untouched: right
    // size, owned by this user and not writable by anyone else.
    let intact = std::fs::metadata(&binary).is_ok_and(|meta| {
        meta.len() == expected
            && meta.mode() & 0o022 == 0
            && meta.mode() & 0o100 != 0
            && meta.uid() == unsafe { libc_geteuid() }
    });
    if intact {
        return Ok(binary);
    }

    private_dir(&dir)?;
    let partial = dir.join("pass-cli.partial");
    let _ = std::fs::remove_file(&partial);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&partial)
        .map_err(|e| format!("Couldn't unpack the Proton Pass client: {e}"))?;
    let mut decoder = flate2::read::GzDecoder::new(BUNDLE);
    let mut buffer = vec![0u8; 1 << 16];
    let mut written = 0u64;
    loop {
        let n = decoder.read(&mut buffer).map_err(|e| format!("Couldn't unpack the Proton Pass client: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        written += n as u64;
    }
    out.sync_all().map_err(|e| e.to_string())?;
    drop(out);
    if written != expected {
        let _ = std::fs::remove_file(&partial);
        return Err("The embedded Proton Pass client is damaged.".into());
    }
    std::fs::rename(&partial, &binary).map_err(|e| e.to_string())?;
    // Older versions are no longer used.
    if let Ok(entries) = std::fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            if entry.file_name() != BUNDLE_ID && entry.path().join("pass-cli").is_file() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    Ok(binary)
}

#[cfg(feature = "bundled")]
unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[cfg(not(feature = "bundled"))]
pub fn install(_cache_root: &Path) -> Result<PathBuf, String> {
    Err("This Spotty build doesn't include the Proton Pass client. Rebuild Spotty with `--features proton-pass`.".into())
}

#[cfg(all(test, feature = "bundled"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unpacks_a_private_runnable_client_once() {
        let root = std::env::temp_dir().join(format!("spotty-pass-embedded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let binary = super::install(&root).expect("unpack the embedded client");
        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&binary), 0o700);
        assert_eq!(std::fs::metadata(&binary).unwrap().len().to_string(), super::BINARY_SIZE);
        // A second call reuses it.
        let modified = std::fs::metadata(&binary).unwrap().modified().unwrap();
        assert_eq!(super::install(&root).unwrap(), binary);
        assert_eq!(std::fs::metadata(&binary).unwrap().modified().unwrap(), modified);

        let output = std::process::Command::new(&binary)
            .arg("--version")
            .env_clear()
            .env("PROTON_PASS_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run the client");
        let _ = std::fs::remove_dir_all(&root);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("2.4.2"));
    }
}
