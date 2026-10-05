fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    tonic_prost_build::configure().compile_with_config(prost, &["bridge.proto"], &["."])?;
    println!("cargo:rerun-if-changed=bridge.proto");
    println!("cargo:rerun-if-changed=prepare_bundle.py");
    println!("cargo:rerun-if-changed=native_dependencies.json");
    println!("cargo:rerun-if-env-changed=SPOTTY_PROTON_BUNDLE_CACHE");
    println!("cargo:rerun-if-env-changed=SPOTTY_PROTON_BUNDLE_OFFLINE");
    if std::env::var_os("CARGO_FEATURE_BUNDLED_BRIDGE").is_some() {
        if std::env::var("CARGO_CFG_TARGET_OS")? != "linux"
            || std::env::var("CARGO_CFG_TARGET_ARCH")? != "x86_64"
        {
            return Err("The bundled native Proton Bridge currently supports Linux x86_64 only. Build with --no-default-features to use an existing native Bridge on other architectures.".into());
        }
        let status = std::process::Command::new("python3")
            .arg("prepare_bundle.py")
            .arg(std::env::var_os("OUT_DIR").ok_or("Missing OUT_DIR")?)
            .status()?;
        if !status.success() {
            return Err("Cannot prepare the verified native Proton Bridge bundle".into());
        }
    }
    Ok(())
}
