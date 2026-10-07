#!/usr/bin/env python3
"""Build the Proton VPN client bundle that Spotty embeds.

Run by build.rs only when the `bundled` feature is enabled:

    prepare_bundle.py <OUT_DIR> <CACHE_DIR>

1. Downloads Proton's sources listed in sources.json and accepts them only if
   their SHA-256 matches.
2. Compiles Proton's Rust platform module (PyO3, abi3) with cargo. Its
   nftables bindings link against the system's runtime libmnl/libnftnl; when
   their -devel symlinks are missing, private link symlinks are used instead.
3. Installs the third-party wheels from requirements.lock with
   `pip --require-hashes`, and Proton's packages (with their entry points).
4. Packs everything into <OUT_DIR>/proton-vpn-bundle.tar.gz and prints the
   bundle id for build.rs.

Nothing is installed system-wide. Results are cached in CACHE_DIR, keyed by
every input, so rebuilding Spotty doesn't rebuild the bundle.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(source, archives):
    target = archives / f"{source['sha256']}.tar.gz"
    if target.exists() and sha256(target) == source["sha256"]:
        return target
    partial = target.with_suffix(".part")
    with urllib.request.urlopen(source["url"], timeout=120) as response, open(partial, "wb") as out:
        shutil.copyfileobj(response, out)
    actual = sha256(partial)
    if actual != source["sha256"]:
        partial.unlink()
        sys.exit(f"{source['name']}: SHA-256 mismatch ({actual}); refusing the download")
    partial.rename(target)
    return target


def extract(archive, destination):
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as tar:
        top = tar.getmembers()[0].name.split("/")[0]
        tar.extractall(destination.parent, filter="data")
    shutil.rmtree(destination)
    (destination.parent / top).rename(destination)


def link_dir(work):
    """Private `libmnl.so`/`libnftnl.so` link names for the runtime libraries."""
    libs = work / "linklibs"
    libs.mkdir(exist_ok=True)
    for name, soname in (("mnl", "libmnl.so.0"), ("nftnl", "libnftnl.so.11")):
        for directory in ("/usr/lib64", "/usr/lib/x86_64-linux-gnu", "/usr/lib"):
            runtime = Path(directory) / soname
            if runtime.exists():
                link = libs / f"lib{name}.so"
                if not link.exists():
                    link.symlink_to(runtime)
                break
        else:
            sys.exit(f"{soname} not found: install your distribution's lib{name} package")
    return libs


def clean_env():
    """The outer cargo's build-script variables must not leak into the inner build."""
    env = {k: v for k, v in os.environ.items()
           if not (k.startswith("CARGO_") and k != "CARGO_HOME")
           and k not in ("RUSTFLAGS", "RUSTC", "RUSTDOC", "RUSTC_WRAPPER",
                         "RUSTC_WORKSPACE_WRAPPER", "TARGET", "HOST", "OUT_DIR",
                         "OPT_LEVEL", "PROFILE", "DEBUG", "NUM_JOBS")}
    return env


def main():
    out_dir, cache = Path(sys.argv[1]), Path(sys.argv[2])
    config = json.loads((HERE / "sources.json").read_text())
    lock = HERE / "requirements.lock"
    python = f"{sys.version_info.major}.{sys.version_info.minor}"

    key = hashlib.sha256()
    for path in (HERE / "sources.json", lock, Path(__file__)):
        key.update(path.read_bytes())
    key.update(python.encode())
    bundle_id = key.hexdigest()[:16]
    cache.mkdir(parents=True, exist_ok=True)
    cached = cache / f"proton-vpn-bundle-{bundle_id}.tar.gz"
    result = out_dir / "proton-vpn-bundle.tar.gz"

    if not cached.exists():
        archives = cache / "archives"
        archives.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=cache) as temp:
            work = Path(temp)
            src = work / "src"
            for source in config["sources"]:
                archive = fetch(source, archives)
                extract(archive, src / source.get("into", source["name"]))

            core = src / "python-proton-vpn-api-core"
            env = clean_env()
            libs = link_dir(work)
            env.update(LIBMNL_LIB_DIR=str(libs), LIBNFTNL_LIB_DIR=str(libs),
                       CARGO_TARGET_DIR=str(cache / "cargo-target"))
            subprocess.run(["cargo", "build", "--release", "--lib", "--no-default-features",
                            "--features", config["platform_features"]],
                           cwd=core, env=env, check=True)
            shutil.copy(cache / "cargo-target" / "release" / "libproton_vpn_platform.so",
                        core / "proton" / "vpn" / "platform.abi3.so")

            stage = work / "stage"
            pip = [sys.executable, "-m", "pip", "install", "--quiet", "--disable-pip-version-check",
                   "--no-input"]
            subprocess.run(pip + ["--target", str(stage / "site"), "--require-hashes", "--no-deps",
                                  "--only-binary=:all:", "-r", str(lock)], check=True)
            subprocess.run(pip + ["--target", str(stage / "proton"), "--no-deps",
                                  "--no-build-isolation",
                                  str(src / "python-proton-core"),
                                  str(src / "python-proton-keyring-linux"), str(core)],
                           check=True)
            (stage / "BUNDLE.json").write_text(json.dumps({"id": bundle_id, "python": python}))

            partial = cached.with_suffix(".part")
            with tarfile.open(partial, "w:gz") as tar:
                for child in sorted(stage.iterdir()):
                    tar.add(child, arcname=child.name)
            partial.rename(cached)

    shutil.copy(cached, result)
    print(f"{bundle_id} {python}")


if __name__ == "__main__":
    main()
