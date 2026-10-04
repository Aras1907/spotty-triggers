//! What Linux distribution Spotty is running on.
//!
//! Read from `os-release`, which also works from the Flatpak sandbox: the
//! runtime exposes the host's copy at `/run/host/os-release`, so no host command
//! (and no `flatpak-spawn --host which dnf` guessing) is needed. Everything that
//! depends on the distro — which package manager the Install/Update rows speak
//! to, whether system packages are offered at all — asks here.

use std::sync::OnceLock;

/// The package-management family a distro belongs to, as far as Spotty cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Fedora, RHEL and clones, Nobara, Amazon Linux… (rpm + dnf)
    Fedora,
    /// Debian, Ubuntu, Mint, Pop!_OS, Zorin, Raspberry Pi OS… (dpkg + apt)
    Debian,
    /// Arch, Manjaro, EndeavourOS, CachyOS… (pacman)
    Arch,
    /// openSUSE Leap/Tumbleweed/MicroOS, SLE… (rpm + zypper)
    Suse,
    /// Anything else (Alpine, Void, Gentoo, NixOS…): no system package support.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distro {
    /// `ID=` — "fedora", "ubuntu", "arch"…
    pub id: String,
    /// `ID_LIKE=` split into words, nearest relative first.
    pub id_like: Vec<String>,
    /// `NAME=` — "Fedora Linux".
    pub name: String,
    /// `VERSION_ID=` — "44", "24.04", empty on rolling releases.
    pub version: String,
    /// `PRETTY_NAME=` — "Fedora Linux 44 (Workstation Edition)".
    pub pretty: String,
    /// The root filesystem is image-based (rpm-ostree, NixOS, SteamOS…): a
    /// package manager can't change it in place, so system packages are off.
    pub immutable: bool,
    pub family: Family,
}

impl Distro {
    /// Parse the text of an `os-release` file.
    pub fn parse(text: &str) -> Distro {
        let get = |key: &str| -> Option<String> {
            text.lines().find_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix(key)?.strip_prefix('=')?;
                Some(unquote(rest))
            })
        };
        let id = get("ID").unwrap_or_default().to_lowercase();
        let id_like: Vec<String> = get("ID_LIKE")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let name = get("NAME").unwrap_or_default();
        let version = get("VERSION_ID").unwrap_or_default();
        let pretty = get("PRETTY_NAME")
            .or_else(|| (!name.is_empty()).then(|| name.clone()))
            .unwrap_or_default();

        let family = family_of(std::iter::once(id.as_str()).chain(id_like.iter().map(String::as_str)));

        // Image-based systems: rpm-ostree variants carry OSTREE_VERSION (and a
        // VARIANT_ID), NixOS and SteamOS are read-only by design.
        let variant = get("VARIANT_ID").unwrap_or_default().to_lowercase();
        let immutable = text.lines().any(|l| l.trim_start().starts_with("OSTREE_VERSION="))
            || matches!(
                variant.as_str(),
                "silverblue" | "kinoite" | "sericea" | "onyx" | "iot" | "coreos" | "cosmic-atomic"
            )
            || matches!(id.as_str(), "nixos" | "steamos" | "bazzite" | "bluefin" | "aurora")
            || id_like.iter().any(|l| l == "nixos");

        Distro {
            id,
            id_like,
            name,
            version,
            pretty,
            immutable,
            family,
        }
    }

    /// The command-line name of the family's package manager — what the rest
    /// of the app calls a "source" (`dnf`, `apt`, `pacman`, `zypper`).
    pub fn package_manager(&self) -> Option<&'static str> {
        match self.family {
            Family::Fedora => Some("dnf"),
            Family::Debian => Some("apt"),
            Family::Arch => Some("pacman"),
            Family::Suse => Some("zypper"),
            Family::Other => None,
        }
    }

    /// System packages can be searched, installed and updated here: a known
    /// package manager on a system it is allowed to change.
    pub fn supports_system_packages(&self) -> bool {
        self.package_manager().is_some() && !self.immutable
    }

    /// "Fedora Linux 44" — name and version without the edition suffix; falls
    /// back to the pretty name, then the id.
    pub fn display(&self) -> String {
        if !self.name.is_empty() && !self.version.is_empty() {
            format!("{} {}", self.name, self.version)
        } else if !self.pretty.is_empty() {
            self.pretty.clone()
        } else if !self.id.is_empty() {
            self.id.clone()
        } else {
            "Linux".to_string()
        }
    }
}

/// The first recognised id wins, `ID` before the `ID_LIKE` chain — so Mint
/// (`ID_LIKE="ubuntu debian"`) is Debian-family and Nobara (`fedora`) is Fedora.
fn family_of<'a>(ids: impl Iterator<Item = &'a str>) -> Family {
    for id in ids {
        match id {
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "ol" | "amzn" | "nobara" => {
                return Family::Fedora
            }
            "debian" | "ubuntu" | "linuxmint" | "pop" | "zorin" | "raspbian" | "elementary"
            | "kali" | "neon" | "devuan" => return Family::Debian,
            "arch" | "archarm" | "manjaro" | "endeavouros" | "cachyos" | "garuda" | "artix" => {
                return Family::Arch
            }
            "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "opensuse-microos" | "suse"
            | "sles" | "sled" => return Family::Suse,
            _ => {}
        }
    }
    Family::Other
}

/// Strip the optional quotes around an os-release value.
fn unquote(v: &str) -> String {
    let v = v.trim();
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v);
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// Where this process finds the host's os-release: the sandbox exposes it
/// under `/run/host`, a native run reads the system's own.
fn os_release_paths() -> &'static [&'static str] {
    if crate::app::is_flatpak() {
        &["/run/host/os-release", "/run/host/etc/os-release"]
    } else {
        &["/etc/os-release", "/usr/lib/os-release"]
    }
}

/// The distro this process runs on. Read once; an unreadable file yields an
/// `Other` distro (system packages off) rather than a guess.
pub fn current() -> &'static Distro {
    static CURRENT: OnceLock<Distro> = OnceLock::new();
    CURRENT.get_or_init(|| {
        let text = os_release_paths()
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let d = Distro::parse(&text);
        log::info!(
            "distro: {} (id={} like={:?} family={:?} immutable={})",
            d.display(),
            d.id,
            d.id_like,
            d.family,
            d.immutable
        );
        d
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(id: &str, like: &str, extra: &str) -> Distro {
        Distro::parse(&format!(
            "NAME=\"Test\"\nID={id}\nID_LIKE=\"{like}\"\nVERSION_ID=\"1\"\n{extra}\n"
        ))
    }

    #[test]
    fn os_release_fields_are_read_and_unquoted() {
        let d = Distro::parse(
            "NAME=\"Fedora Linux\"\nVERSION=\"44 (Workstation Edition)\"\nID=fedora\n\
             VERSION_ID=44\nPRETTY_NAME=\"Fedora Linux 44 (Workstation Edition)\"\n",
        );
        assert_eq!(d.id, "fedora");
        assert_eq!(d.name, "Fedora Linux");
        assert_eq!(d.version, "44");
        assert_eq!(d.family, Family::Fedora);
        assert_eq!(d.package_manager(), Some("dnf"));
        assert_eq!(d.display(), "Fedora Linux 44");
        assert!(d.supports_system_packages());
        // A rolling release has no VERSION_ID: the pretty name stands in.
        let arch = Distro::parse("NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n");
        assert_eq!(arch.display(), "Arch Linux");
    }

    #[test]
    fn derivatives_follow_their_parent_family() {
        // Own id first, then the ID_LIKE chain.
        assert_eq!(parse("ubuntu", "debian", "").family, Family::Debian);
        assert_eq!(parse("linuxmint", "ubuntu debian", "").family, Family::Debian);
        assert_eq!(parse("pop", "ubuntu debian", "").family, Family::Debian);
        assert_eq!(parse("manjaro", "arch", "").family, Family::Arch);
        assert_eq!(parse("endeavouros", "arch", "").family, Family::Arch);
        assert_eq!(parse("rocky", "rhel centos fedora", "").family, Family::Fedora);
        assert_eq!(parse("nobara", "rhel centos fedora", "").family, Family::Fedora);
        assert_eq!(
            parse("opensuse-tumbleweed", "opensuse suse", "").family,
            Family::Suse
        );
        // An unknown distro with a known relative is that relative.
        assert_eq!(parse("mydistro", "arch", "").family, Family::Arch);
        // Package manager per family.
        assert_eq!(parse("ubuntu", "debian", "").package_manager(), Some("apt"));
        assert_eq!(parse("arch", "", "").package_manager(), Some("pacman"));
        assert_eq!(parse("opensuse-leap", "suse", "").package_manager(), Some("zypper"));
    }

    #[test]
    fn unknown_distros_get_no_system_packages() {
        for id in ["alpine", "void", "gentoo", "slackware"] {
            let d = parse(id, "", "");
            assert_eq!(d.family, Family::Other, "{id}");
            assert_eq!(d.package_manager(), None, "{id}");
            assert!(!d.supports_system_packages(), "{id}");
        }
        // Nothing readable at all: the same, never a guess.
        let none = Distro::parse("");
        assert_eq!(none.family, Family::Other);
        assert!(!none.supports_system_packages());
        assert_eq!(none.display(), "Linux");
    }

    #[test]
    fn image_based_systems_are_recognised_and_left_alone() {
        // rpm-ostree desktops are Fedora-family but a package manager can't
        // change them: no system package rows, Flatpak only.
        let silverblue = parse("fedora", "", "VARIANT_ID=silverblue\nOSTREE_VERSION='44.20260101.0'");
        assert_eq!(silverblue.family, Family::Fedora);
        assert!(silverblue.immutable);
        assert!(!silverblue.supports_system_packages());
        let bazzite = parse("bazzite", "fedora", "OSTREE_VERSION=44");
        assert!(bazzite.immutable);
        assert!(parse("nixos", "", "").immutable);
        assert!(parse("steamos", "arch", "").immutable);
        // Regular installs are not.
        assert!(!parse("fedora", "", "VARIANT_ID=workstation").immutable);
        assert!(!parse("ubuntu", "debian", "").immutable);
    }

    /// Reads this machine's real os-release.
    #[test]
    #[ignore = "reads the host's os-release"]
    fn this_system_is_recognised() {
        let d = current();
        println!("{} → {:?} (pm {:?}, immutable {})", d.display(), d.family, d.package_manager(), d.immutable);
        assert_ne!(d.family, Family::Other, "{d:?}");
    }
}
