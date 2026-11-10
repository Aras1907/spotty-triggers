# Workspace instructions

- Work only in this opened project folder. Do not modify system/root files or
  files in Documents or Downloads.
- Never install a local Cargo build. Spotty is pushed to GitHub, built as a
  Flatpak by CI (`release.yml`), and installed from the GitHub release with
  `flatpak install --user`. See the `build-and-install` skill.
- Always use Luna (`gpt-6-luna`) for delegated work. Use focused Luna subagents
  for implementation, debugging, and review; the primary agent coordinates,
  verifies, builds, and integrates their changes.

## Before you change anything

1. Read this file, `SOUL.md` and `ARCHITECTURE.md`.
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

- Never add a GNOME Shell extension, with one exception the user approved on
  2026-11-10: the optional Spotty Focus extension
  (`.integration/Spotty/data/gnome-shell-extension/`, `src/focus_ext.rs`).
  The user installs and removes it in Settings → Apps; Spotty must keep
  working without it. This overrides the "no extensions, ever" line in
  `.integration/Spotty/CLAUDE.md` for this extension only.

- Never add a Proton integration (Mail Bridge, Calendar, Drive, Pass or VPN)
  back into Spotty. They were removed on purpose; only add one if the user asks.
- Never store, log or print a password, token or other secret.
- Never add LibreOffice, `soffice` or `libreoffice` conversions.
- Never build a Flatpak locally or copy a Cargo binary into `~/.local/bin`.
  The only installed Spotty is the GitHub release Flatpak.
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
- Never open a PR unless the user asks. Shipping a Spotty change means
  committing and pushing both this repo and `.integration/Spotty`
  (`Aras1907/Spotty`) and tagging a `v*` release; that is the install path.

## How to verify

Run the checks that match your change. The `verify-before-push` skill has the
full list.

- Sync this repo into the Spotty checkout:
  `cd /home/aras/development/Spotty/spotty-triggers && python3 tools/sync_to_spotty.py`
- Spotty unit tests (host, native Cargo):
  `flatpak-spawn --host sh -c 'cd /home/aras/development/Spotty/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo test --bin spotty'`
  Plain `cargo test` fails in the Spotty checkout because of a broken example, so
  always use `--bin spotty`.
- Release build: CI only. Push a `v*` tag; never install a local build.
- Manifest and catalog check, install and restart: see `verify-before-push` and
  `build-and-install`.

## Pointers

- `SOUL.md`: what the project is for, and its non-negotiables.
- `ARCHITECTURE.md`: repo map, the two checkouts, the trigger flow,
  data locations.
- `CLAUDE.md`: the entry point for Claude Code.
- Skills in `.claude/skills/`: `build-and-install`, `add-native-trigger`,
  `verify-before-push`.
