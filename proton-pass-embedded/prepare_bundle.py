#!/usr/bin/env python3
"""Build the Proton Pass client that Spotty embeds.

Run by build.rs only when the `bundled` feature is enabled:

    prepare_bundle.py <OUT_DIR> <CACHE_DIR>

1. Downloads Proton's pass-cli source pinned in sources.json and accepts it
   only if its SHA-256 matches.
2. Builds it with `cargo build --release --locked -p pass-cli`, using the
   dependency versions pinned in pass-cli.Cargo.lock. Nothing is installed.
3. Compresses the binary into <OUT_DIR>/pass-cli.gz and prints
   "<bundle id> <binary size>" for build.rs.

Results are cached in CACHE_DIR, keyed by every input, so rebuilding Spotty
doesn't rebuild the client.
"""
import gzip
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
    with tarfile.open(archive) as tar:
        top = tar.getmembers()[0].name.split("/")[0]
        tar.extractall(destination.parent, filter="data")
    if destination.exists():
        shutil.rmtree(destination)
    (destination.parent / top).rename(destination)


def clean_env():
    """The outer cargo's build-script variables must not leak into the inner build."""
    env = {k: v for k, v in os.environ.items()
           if not (k.startswith("CARGO_") and k not in ("CARGO_HOME",))
           and k not in ("RUSTFLAGS", "RUSTC", "RUSTDOC", "RUSTC_WRAPPER",
                         "RUSTC_WORKSPACE_WRAPPER", "TARGET", "HOST", "OUT_DIR",
                         "OPT_LEVEL", "PROFILE", "DEBUG", "NUM_JOBS")}
    return env


def main():
    out_dir, cache = Path(sys.argv[1]), Path(sys.argv[2])
    source = json.loads((HERE / "sources.json").read_text())["source"]
    lock = HERE / "pass-cli.Cargo.lock"

    key = hashlib.sha256()
    for path in (HERE / "sources.json", lock, Path(__file__)):
        key.update(path.read_bytes())
    bundle_id = key.hexdigest()[:16]
    cache.mkdir(parents=True, exist_ok=True)
    cached = cache / f"pass-cli-{bundle_id}.gz"
    sizes = cache / f"pass-cli-{bundle_id}.size"

    if not cached.exists() or not sizes.exists():
        archives = cache / "archives"
        archives.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=cache) as temp:
            work = Path(temp)
            src = work / "src"
            extract(fetch(source, archives), src)
            shutil.copy(lock, src / "Cargo.lock")

            env = clean_env()
            # Link the system's OpenSSL (for the encrypted session database)
            # instead of compiling a private copy, which needs Perl modules.
            env.update(OPENSSL_NO_VENDOR="1", CARGO_TARGET_DIR=str(cache / "cargo-target"),
                       CARGO_PROFILE_RELEASE_STRIP="symbols")
            subprocess.run(["cargo", "build", "--release", "--locked", "-p", "pass-cli"],
                           cwd=src, env=env, check=True)
            binary = cache / "cargo-target" / "release" / "pass-cli"

            partial = cached.with_suffix(".part")
            with open(binary, "rb") as raw, gzip.GzipFile(partial, "wb", compresslevel=9, mtime=0) as out:
                shutil.copyfileobj(raw, out)
            partial.rename(cached)
            sizes.write_text(str(binary.stat().st_size))

    shutil.copy(cached, out_dir / "pass-cli.gz")
    print(f"{bundle_id} {sizes.read_text().strip()}")


if __name__ == "__main__":
    main()
