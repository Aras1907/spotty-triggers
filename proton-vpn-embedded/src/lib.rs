//! Proton VPN's official Linux client library (proton-vpn-api-core and
//! friends, GPL-3.0), embedded in Spotty so no separate Proton app or CLI is
//! needed. See README.md.
//!
//! With the `bundled` feature the client is compiled into the binary as a
//! compressed bundle; [`install`] unpacks it once into a private cache and
//! returns what Spotty needs to start the helper. Without the feature,
//! [`install`] explains how to build it in.

use std::path::{Path, PathBuf};

/// The JSON-lines helper that drives Proton's library (see `helper/`).
pub const HELPER: &str = include_str!("../helper/spotty_vpn_helper.py");

#[cfg(feature = "bundled")]
const BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/proton-vpn-bundle.tar.gz"));
#[cfg(feature = "bundled")]
const BUNDLE_ID: &str = env!("SPOTTY_PROTON_VPN_BUNDLE_ID");
#[cfg(feature = "bundled")]
const PYTHON: &str = env!("SPOTTY_PROTON_VPN_PYTHON");

/// An unpacked client, ready to run.
#[derive(Debug, Clone)]
pub struct Installed {
    /// Directory holding `site/` (wheels) and `proton/` (Proton's packages).
    pub bundle: PathBuf,
    /// The helper script.
    pub helper: PathBuf,
    /// `python3.X` the bundle's compiled wheels were built for.
    pub python: String,
}

/// True when this build carries the embedded client.
pub const fn bundled() -> bool {
    cfg!(feature = "bundled")
}

/// Unpack the client under `cache_root` (once per bundle version) and write
/// the helper next to it.
#[cfg(feature = "bundled")]
pub fn install(cache_root: &Path) -> Result<Installed, String> {
    let bundle = cache_root.join(BUNDLE_ID);
    if !bundle.join("BUNDLE.json").exists() {
        std::fs::create_dir_all(cache_root).map_err(|e| e.to_string())?;
        let partial = cache_root.join(format!("{BUNDLE_ID}.partial"));
        let _ = std::fs::remove_dir_all(&partial);
        let decoder = flate2::read::GzDecoder::new(BUNDLE);
        tar::Archive::new(decoder)
            .unpack(&partial)
            .map_err(|e| format!("Couldn't unpack the Proton VPN client: {e}"))?;
        let _ = std::fs::remove_dir_all(&bundle);
        std::fs::rename(&partial, &bundle).map_err(|e| e.to_string())?;
        // Older bundle versions are no longer used.
        if let Ok(entries) = std::fs::read_dir(cache_root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.len() == BUNDLE_ID.len() && name != BUNDLE_ID && entry.path().join("BUNDLE.json").exists() {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }
    let helper = cache_root.join("spotty_vpn_helper.py");
    if std::fs::read_to_string(&helper).ok().as_deref() != Some(HELPER) {
        std::fs::write(&helper, HELPER).map_err(|e| e.to_string())?;
    }
    Ok(Installed { bundle, helper, python: format!("python{PYTHON}") })
}

#[cfg(not(feature = "bundled"))]
pub fn install(_cache_root: &Path) -> Result<Installed, String> {
    Err("This Spotty build doesn't include the Proton VPN client. Rebuild Spotty with `--features proton-vpn`.".into())
}
