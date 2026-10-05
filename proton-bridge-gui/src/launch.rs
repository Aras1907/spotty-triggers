use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub struct BackendLaunch {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn backend_launch() -> Result<BackendLaunch, String> {
    let launcher = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join("protonmail-bridge"))
        .find(|path| executable(path))
        .ok_or(
            "Install Proton's native Linux package and make sure protonmail-bridge is on PATH.",
        )?;
    backend_beside(
        &launcher,
        std::env::var_os("SPOTTY_PROTON_BRIDGE_BACKEND").map(PathBuf::from),
    )
}

fn backend_beside(
    launcher: &Path,
    override_path: Option<PathBuf>,
) -> Result<BackendLaunch, String> {
    let launcher = launcher
        .canonicalize()
        .map_err(|_| "Cannot locate the native Bridge launcher.")?;
    let directory = launcher
        .parent()
        .ok_or("Cannot locate Bridge's installation folder.")?;
    // Proton's launcher selects its GUI unless --cli/--noninteractive is used.
    // Its native backend accepts --grpc; the packaged backend is beside the
    // launcher. Keep the launcher path for Bridge's updates and autostart.
    let backend = override_path.or_else(|| {
        ["bridge", "proton-bridge"].into_iter().map(|name| directory.join(name)).find(|path| executable(path))
    }).ok_or("Cannot find the packaged Bridge backend beside its launcher. Open the official GUI, or set SPOTTY_PROTON_BRIDGE_BACKEND to the native backend's absolute path.")?;
    if !backend.is_absolute() || !executable(&backend) {
        return Err("The Bridge backend must be an absolute path to an executable file.".into());
    }
    if backend.canonicalize().ok().as_ref() == Some(&launcher) {
        return Err("Select Bridge's backend executable, not its GUI launcher.".into());
    }
    Ok(BackendLaunch {
        executable: backend,
        arguments: vec![
            "--grpc".into(),
            "--launcher".into(),
            launcher.into_os_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_packaged_backend_without_launching_gui_or_inheriting_parent_lifetime() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let launcher = directory.path().join("launcher");
        let backend = directory.path().join("bridge");
        for path in [&launcher, &backend] {
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let link = directory.path().join("protonmail-bridge");
        std::os::unix::fs::symlink(&launcher, &link).unwrap();
        let plan = backend_beside(&link, None).unwrap();
        assert_eq!(plan.executable, backend);
        assert_eq!(
            plan.arguments,
            [
                OsString::from("--grpc"),
                OsString::from("--launcher"),
                launcher.into_os_string()
            ]
        );
        assert!(backend_beside(&link, Some(link.clone())).is_err());
        assert!(backend_beside(&link, Some("relative".into())).is_err());
    }
}
