//! Settings for the optional local Proton mail-server service.

pub fn supported() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}
