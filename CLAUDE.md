# CLAUDE.md

The rules for this workspace. Read `SOUL.md` (the project's character) and
`ARCHITECTURE.md` (repo map, the two checkouts, the trigger flow) too.
This file wins over `.integration/Spotty/CLAUDE.md` where they disagree.

Work only in this project folder. Do not modify system/root files or files in
Documents or Downloads.

## Skills

Project skills live in `.claude/skills/<name>/SKILL.md`. Invoke one with the
Skill tool or `/<name>`:

- `build-and-install`: sync, push to GitHub, let CI build the Flatpak, install
  the release Flatpak from GitHub and restart Spotty.
- `add-native-trigger`: add a trigger, from manifest to tests and version bump.
- `verify-before-push`: the full check list and the push rule.

## Install rule

Never install a local `cargo build`. Every change ships the same way: sync,
commit and push to GitHub (this repo and `Aras1907/Spotty`), tag a `v*` release
so `release.yml` builds the Flatpak, then install that Flatpak from the GitHub
release with `flatpak install --user`. Local `cargo test`/`cargo check` are for
checking code only, never for what runs on the machine.

## Before you change anything

1. Run `git status` and `git diff --stat`. The working tree may hold someone's
   unfinished work. Do not revert, reset, clean, stash or stage files you did
   not change.
2. Find the boundary: this repo, the Spotty checkout (`.integration/Spotty`), or
   both. `ARCHITECTURE.md` maps them.
3. Decide manifest-only or native. Web, files and shell manifests need no Rust
   and no Spotty rebuild. A native backend needs a Rust provider here and a
   release. The `add-native-trigger` skill lists the steps.
4. If behaviour changes, bump the `version` in the manifest and in its
   `index.json` entry, to the same new value.
5. Decide how you will verify the change before you edit.

## GNOME Shell extensions

No GNOME Shell extensions and no third-party CLI tools, with one exception the
user approved on 2026-11-10: the optional **Spotty Focus** extension
(`.integration/Spotty/data/gnome-shell-extension/`, `src/focus_ext.rs`). It
lets Spotty focus an app that is already open. The user installs and removes it
in Settings → Apps; Spotty must keep working without it. Do not add any other
extension or rely on a third-party one.

## Never

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
- Never stop the daemon with `pkill -f`. Use `flatpak kill com.spotty.Spotty`.
- Never change a manifest's behaviour without bumping its `version`, or leave
  `index.json` and a manifest out of step.
- Never open a PR unless the user asks. Shipping a Spotty change means
  committing and pushing both this repo and `.integration/Spotty`
  (`Aras1907/Spotty`) and tagging a `v*` release; that is the install path.

## How to verify

The `verify-before-push` skill has the full list.

- Sync this repo into the Spotty checkout:
  `cd /home/aras/development/Spotty/spotty-triggers && python3 tools/sync_to_spotty.py`
- Spotty unit tests (check only):
  `flatpak-spawn --host sh -c 'cd /home/aras/development/Spotty/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo test --bin spotty'`
  Always use `--bin spotty`; a broken example makes plain `cargo test` fail.
- `cargo check --release --bin spotty` before tagging: CI builds the release.
- Release build: CI only. Push a `v*` tag; never install a local build.
