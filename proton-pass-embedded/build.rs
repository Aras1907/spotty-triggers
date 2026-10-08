fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=prepare_bundle.py");
    println!("cargo:rerun-if-changed=sources.json");
    println!("cargo:rerun-if-changed=pass-cli.Cargo.lock");
    println!("cargo:rerun-if-env-changed=SPOTTY_PROTON_PASS_CACHE");
    if std::env::var_os("CARGO_FEATURE_BUNDLED").is_none() {
        return Ok(());
    }
    if std::env::var("CARGO_CFG_TARGET_OS")? != "linux"
        || std::env::var("CARGO_CFG_TARGET_ARCH")? != "x86_64"
    {
        return Err("The embedded Proton Pass client supports Linux x86_64 only.".into());
    }
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("Missing OUT_DIR")?);
    // Shared across Spotty rebuilds, next to Cargo's target directory.
    let cache = match std::env::var_os("SPOTTY_PROTON_PASS_CACHE") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => out_dir
            .ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == "target"))
            .and_then(|target| target.parent())
            .ok_or("Cannot locate Cargo's target directory")?
            .join("build")
            .join("spotty-proton-pass-cache"),
    };
    let output = std::process::Command::new("python3")
        .arg("prepare_bundle.py")
        .arg(&out_dir)
        .arg(&cache)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Cannot prepare the Proton Pass client:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let mut fields = stdout.lines().last().unwrap_or_default().split_whitespace();
    let id = fields.next().ok_or("prepare_bundle.py printed no bundle id")?;
    let size = fields.next().ok_or("prepare_bundle.py printed no binary size")?;
    println!("cargo:rustc-env=SPOTTY_PROTON_PASS_ID={id}");
    println!("cargo:rustc-env=SPOTTY_PROTON_PASS_SIZE={size}");
    Ok(())
}
