fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    tonic_prost_build::configure().compile_with_config(prost, &["bridge.proto"], &["."])?;
    println!("cargo:rerun-if-changed=bridge.proto");
    println!("cargo:rerun-if-changed=prepare_bundle.py");
    println!("cargo:rerun-if-changed=build_inprocess.py");
    println!("cargo:rerun-if-changed=inprocess/main.go");
    println!("cargo:rerun-if-changed=inprocess/main_test.go");
    println!("cargo:rerun-if-changed=inprocess/embedded_app.go");
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
        let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("Missing OUT_DIR")?);
        let target_root = out_dir
            .ancestors()
            .find(|directory| {
                directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "target" || name.ends_with("-target"))
            })
            .ok_or("Cannot locate Cargo target directory for Go cache")?;
        let cache_parent = if target_root.file_name().is_some_and(|name| name == "target") {
            target_root
                .parent()
                .ok_or("Cargo target directory has no project parent")?
                .join("build")
        } else {
            target_root
                .parent()
                .ok_or("Cargo target directory has no build parent")?
                .to_path_buf()
        };
        let go_cache = cache_parent.join("spotty-go-cache");
        let status = std::process::Command::new("python3")
            .arg("build_inprocess.py")
            .arg(std::env::var_os("OUT_DIR").ok_or("Missing OUT_DIR")?)
            .env("SPOTTY_PROTON_GO_CACHE", go_cache)
            .status()?;
        if !status.success() {
            return Err("Cannot build the in-process Proton Bridge library".into());
        }
        let library =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("Missing OUT_DIR")?)
                .join("libspotty_proton_bridge.so");
        let hash = std::process::Command::new("sha256sum")
            .arg(&library)
            .output()?;
        if !hash.status.success() {
            return Err("Cannot identify the generated Proton Bridge library".into());
        }
        let output = String::from_utf8(hash.stdout)?;
        let digest = output
            .split_whitespace()
            .next()
            .ok_or("sha256sum returned no digest")?;
        println!("cargo:rustc-env=SPOTTY_BRIDGE_LIBRARY_SHA256={digest}");
    }
    Ok(())
}
