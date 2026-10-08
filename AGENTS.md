# Workspace instructions

- Work only in this opened project folder. Do not modify system/root files or
  files in Documents or Downloads.
- Build and install natively with Cargo. Do not use a Flatpak build.
- Always use Luna (`gpt-6-luna`) for delegated work. Use focused Luna subagents
  for implementation, debugging, and review; the primary agent coordinates,
  verifies, builds, and integrates their changes.

## Before you change anything

1. Read this file, `SOUL.md` and `ARCHITECTURE.md`. For anything that touches
   Proton, also read the `proton-integration` skill in `.claude/skills/`.
2. Run `git status` and `git diff --stat`. The working tree may hold someone's
   unfinished work. Do not revert, reset, clean, stash or stage files you did
   not change.
3. Find the boundary: this repo, the Spotty checkout (`.integration/Spotty`, local
   only), or both. `ARCHITECTURE.md` maps them.
4. Decide manifest-only or native. Web, files and shell manifests need no Rust
   and no Spotty rebuild. A native backend needs a Rust provider here and a
   rebuild. The `add-native-trigger` skill lists the steps.
5. If behaviour changes, bump the `version` in the manifest and in its
   `index.json` entry, to the same new value.
6. Decide how you will verify the change (see "How to verify") before you edit.

## Never

- Never store, log or print a Proton password, 2FA code, mailbox password, key
  password or session token.
- Never write unlocked keys, decrypted names, calendar events or vault contents
  to disk.
- Never run a Proton sign-in in a web view or through a third-party CLI.
- Never share a Proton session except through the session-fork whitelist in
  `proton-account/src/fork.rs`. Never add a client id to that whitelist without
  a test and a review.
- Never add LibreOffice, `soffice` or `libreoffice` conversions.
- Never build or run a Flatpak.
- Never edit `trigger-backends/` inside `.integration/Spotty` by hand. Change this
  repo, then run `python3 tools/sync_to_spotty.py`.
- Never run blocking network or disk work on the GTK main thread. Async work uses
  `futures`, not tokio.
- Always return `Some(bool)` from a glib boolean signal handler. Returning `None`
  aborts the app.
- Never write a gsettings string unquoted. gsettings values are GVariant text.
- Never put quotes around `{query}` in a shell manifest. Spotty quotes it.
- Never stop the daemon with `pkill -f`. Find it with `pgrep -x spotty` and check
  the binary with `readlink /proc/<pid>/exe`.
- Never change a manifest's behaviour without bumping its `version`, or leave
  `index.json` and a manifest out of step.
- Never commit, push or open a PR unless the user asks. When asked, push only this
  trigger repo.
- Never claim a live Proton login works, including the Pass and VPN forks. The
  tests use a fake Proton server; say so whenever you report on the sign-in path.

## How to verify

Run the checks that match your change. The `verify-before-push` skill has the
full list.

- Proton account crate:
  `cd /home/aras/development/spotty-triggers/proton-account && cargo test`
  If the sandbox cannot build it, run it on the host:
  `flatpak-spawn --host sh -c 'cd /home/aras/development/spotty-triggers/proton-account && cargo test'`
- Sync this repo into the Spotty checkout:
  `cd /home/aras/development/spotty-triggers && python3 tools/sync_to_spotty.py`
- Spotty unit tests (host, native Cargo):
  `flatpak-spawn --host sh -c 'cd /home/aras/development/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo test --bin spotty --features proton-vpn,proton-pass -- proton'`
  Plain `cargo test` fails in the Spotty checkout because of a broken example, so
  always use `--bin spotty`.
- Release build (host, native Cargo):
  `flatpak-spawn --host sh -c 'cd /home/aras/development/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo build --release --features proton-vpn,proton-pass'`
- Manifest and catalog check, install and restart: see `verify-before-push` and
  `build-and-install`.

## Pointers

- `SOUL.md`: what the project is for, and its non-negotiables.
- `ARCHITECTURE.md`: repo map, the two checkouts, the trigger flow, the Proton
  stack, data locations, and known gaps.
- `CLAUDE.md`: the entry point for Claude Code.
- Skills in `.claude/skills/`: `build-and-install`, `add-native-trigger`,
  `proton-integration`, `verify-before-push`.
