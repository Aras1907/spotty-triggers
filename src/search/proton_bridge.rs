//! Optional packaged Bridge login. Account secrets never enter search queries.
use super::{Action, ResultKind, SearchResult};
use std::os::unix::process::CommandExt;

pub fn open() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|_| "Cannot locate the Spotty package.")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--proton-bridge-gui")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot open the packaged Proton Bridge login window.")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn search(query: &str) -> Vec<SearchResult> {
    if !matches!(
        query.to_lowercase().as_str(),
        "" | "login" | "gui" | "open" | "install" | "start" | "settings" | "password"
    ) {
        return vec![SearchResult {
            kind: ResultKind::System,
            title: "Enter credentials in the Proton Bridge window".into(),
            subtitle: Some("Use proton login or proton settings".into()),
            icon: Some("mail-send-receive-symbolic".into()),
            action: Action::Noop,
            score: 100,
        }];
    }
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return vec![],
    };
    // Existing launch action accepts shell syntax. Quote the package path;
    // never substitute query text or account information in this command.
    let quoted = executable.to_string_lossy().replace('\'', "'\\''");
    vec![SearchResult {
        kind: ResultKind::System,
        title: "Proton Mail Bridge".into(),
        subtitle: Some("Sign in or view your Bridge password and mail settings".into()),
        icon: Some("mail-send-receive-symbolic".into()),
        action: Action::RunCommand(format!("'{quoted}' --proton-bridge-gui")),
        score: 1000,
    }]
}
