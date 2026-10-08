---
name: build-and-install
description: Use after changing Spotty code or trigger manifests in this repo, or when asked to build, install, restart or check the running Spotty. Covers the sync into the Spotty checkout, the host Cargo build, copying the binary to ~/.local/bin, and a safe daemon restart.
---

# Build and install Spotty

Build natively with Cargo on the host. The sandbox cannot build Spotty (GTK dev
files are missing). Never build a Flatpak.

## 1. Sync this repo into the Spotty checkout

```sh
cd /home/aras/development/spotty-triggers && python3 tools/sync_to_spotty.py
```

It prints `synced <path>` per changed file, then `N files updated`. Never copy
into `trigger-backends/` by hand, and do not use `cp -p` or `rsync -a`: old mtimes
can make cargo report "Finished" with a stale binary.

## 2. Build on the host

```sh
flatpak-spawn --host sh -c 'cd /home/aras/development/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo build --release --features proton-vpn,proton-pass'
```

- The build is slow. Run it in the background (Bash `run_in_background`) and read
  the output when it finishes. A `( ... &)` subshell dies with the tool call.
- `OPENSSL_NO_VENDOR=1` makes the `proton-pass` build use the system's OpenSSL 3
  instead of a private copy (see `proton-pass-embedded/README.md`).
- Success: the output ends with `Finished` and has no `error`.

If the build fails, stop. The running daemon is untouched.

## 3. Stop the running daemon (only after a good build)

```sh
pgrep -x spotty
readlink /proc/<pid>/exe
kill <pid>
```

Check each PID's binary with `readlink`, then send SIGTERM with `kill`. This is
the signal `spotty --quit` sends. Run `pgrep -x spotty` again until it prints
nothing. Do not use SIGKILL unless the user asks. Never use `pkill -f`.

## 4. Install and start

Install only after the old process has exited. Writing to a running executable
fails with "Text file busy".

```sh
mkdir -p /home/aras/.local/bin && cp /home/aras/development/spotty-triggers/.integration/Spotty/target/release/spotty /home/aras/.local/bin/spotty
/home/aras/.local/bin/spotty
```

With no arguments, Spotty starts a detached daemon and returns. If a daemon is
already running, it does nothing.

## 5. Verify

```sh
pgrep -x spotty
readlink /proc/<pid>/exe
cmp /home/aras/development/spotty-triggers/.integration/Spotty/target/release/spotty /home/aras/.local/bin/spotty && echo same
```

- `readlink` must print `/home/aras/.local/bin/spotty`. A ` (deleted)` suffix
  means the daemon still runs an old file.
- `cmp` must print `same`.
- `spotty --toggle` reaches only a daemon running the same executable path.

Report the `readlink` and `cmp` results. Do not say "installed" without them. Run
`verify-before-push` first.
