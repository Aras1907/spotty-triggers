//! Settings for the optional local Proton mail-server service.
use std::os::unix::process::CommandExt;

pub fn supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

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
