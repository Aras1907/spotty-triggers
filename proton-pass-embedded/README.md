# Embedded Proton Pass client

Spotty's Proton Pass integration runs **Proton's official Pass client**
([`pass-cli`](https://github.com/protonpass/pass-cli), GPL-3.0) inside Spotty,
so users need no separate Proton app or CLI. Search, sign-in and settings
happen in Spotty's `pass` trigger and its Settings window.

## How it works

- `prepare_bundle.py` (run by `build.rs` when the `bundled` feature is on)
  downloads the `pass-cli` source pinned in `sources.json` (tag 2.4.2) and
  accepts it only if its SHA-256 matches.
- It builds the client with `cargo build --release --locked -p pass-cli`
  against the dependency versions in `pass-cli.Cargo.lock`. Proton's own
  crates come from Proton's public Cargo registry
  (`rust-registry.proton.me`, configured by the source tree) and a few from
  GitHub, all pinned by that lock file. `OPENSSL_NO_VENDOR=1` makes the
  client's encrypted session database use the system's OpenSSL 3 instead of a
  private copy.
- The binary is stripped, gzip-compressed and embedded in Spotty. On first use
  Spotty unpacks it into `~/.cache/spotty/proton-pass/<bundle id>/` (owner-only
  folder, executable only by the user) and runs it per request.
- `src/features/proton_pass.rs` in this repository drives the client.

## What Spotty runs, and what it never does

- The client runs with a **cleared environment** (only the variables it needs
  to find the desktop keyring), its **own session folder**
  (`~/.local/share/spotty/proton-pass`), its key in the **desktop keyring**,
  and with `PROTON_PASS_NO_UPDATE_CHECK` and `PROTON_PASS_DISABLE_TELEMETRY`
  set. Spotty never runs `pass-cli update`.
- Sign-in is the client's **web sign-in**: the client prints a Proton address,
  Spotty opens it in its Proton window (which already holds the Proton session
  of Calendar and Drive), and the client finishes by itself. No password, 2FA
  code or recovery secret ever passes through Spotty.
- Listings use `item list --output json` **without** `--show-secrets`, which is
  Proton's secret-free summary (title, type, ids). Passwords, usernames,
  one-time codes and notes are read with `item view --field …` only when a row
  is picked, copied with the password-manager hint (so clipboard history
  skips them), wiped from Spotty's buffers, and cleared from the clipboard
  after 30 seconds.
- Item ids are validated and passed as `--share-id=…` / `--item-id=…`, never
  through a shell.

## Building

    cargo build --features proton-pass          # in Spotty

Needs network access on first build, Python 3, Cargo with a recent stable
Rust (edition 2024), a C compiler, and OpenSSL's development files
(`openssl-devel`). Results are cached in `build/spotty-proton-pass-cache/` (or
`$SPOTTY_PROTON_PASS_CACHE`). At runtime it needs OpenSSL 3 and an unlocked
desktop keyring (GNOME Keyring or another Secret Service).

Without the feature Spotty still builds; the `pass` trigger then explains how
to include the client.

## Updating the pin

Update the commit URL and SHA-256 in `sources.json`, then regenerate
`pass-cli.Cargo.lock`: unpack the new source, run `cargo generate-lockfile`
(or `cargo update` against the previous lock), check that
`cargo build --release --locked -p pass-cli` succeeds, and copy `Cargo.lock`
over `pass-cli.Cargo.lock`. Review the dependency changes.

## Not supported

- Linux x86_64 only.
- Proton Pass CLI needs a paid Proton Pass plan; the client refuses sign-in for
  accounts that may not use it, and Spotty shows Proton's message.
- Creating or editing items; use Proton Pass in the Proton window for that.
