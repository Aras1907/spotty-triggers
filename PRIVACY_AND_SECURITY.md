# Privacy and security — Spotty triggers

Review date: **2026-10-04**. The complete application policy and review findings
are in [Spotty's PRIVACY_AND_SECURITY.md](https://github.com/Aras1907/Spotty/blob/main/PRIVACY_AND_SECURITY.md).
When this repository is the `trigger-backends` submodule, the same document is
available at `../PRIVACY_AND_SECURITY.md`.

## Repository-specific boundaries

- Native Rust backends are compiled into Spotty. Their manifest enables a
  shipped feature; it does not install executable code dynamically.
- Shell manifests are arbitrary user-level executable code. Installation must
  show the exact command and require approval. Use `{query}` unquoted as a shell
  word; Spotty supplies quoting. Do not re-interpret query data through `eval`
  or another interpreter. Quoting cannot secure an untrusted template.
- The optional [Proton login companion](proton-bridge-gui/README.md) sends
  sign-in details only to the local Bridge API over authenticated, verified
  TLS. It does not persist form credentials. Bridge owns the saved account,
  encrypted vault, keyring, and network connection to Proton. The companion
  uses Bridge's internal 3.x API; live account login remains unverified.
- Proton Pass runs Proton's official `pass-cli`, built from a source archive
  pinned by commit and SHA-256 and embedded in Spotty
  ([details](proton-pass-embedded/README.md)). It runs with a cleared
  environment, its own session folder, its key in the desktop keyring and its
  update check and telemetry disabled. Spotty keeps only item titles in memory
  and never writes them to disk; passwords, usernames, one-time codes and notes
  are read when copied, marked as passwords on the clipboard, wiped from
  Spotty's buffers and cleared from the clipboard after 30 seconds. Sign-in is
  Proton's web sign-in in Spotty's Proton window; Spotty never sees the
  password. A running desktop can still read the clipboard during those 30
  seconds, and GTK keeps a copy of the text until it is cleared. The client's
  dependencies come from Proton's Cargo registry and GitHub, pinned by
  `pass-cli.Cargo.lock`; they are not independently audited here.
- Spotty remembers that a Proton web session exists with an empty marker file
  so a newly installed Proton integration can use it; the session itself stays
  in the private WebKit profile (`~/.local/share/spotty/proton-web`).
- Web manifests support HTTP(S) links. Activating one sends the query to its
  website. Translation uses the configured LibreTranslate endpoint, which is
  local by default but can be remote. Dictionary/currency backends can also
  make network requests; local processing is not a universal privacy promise.
- The mutable repository/catalog is trusted and is not independently signed.
  Maintainer account compromise can affect users installing manifests. Protect
  publishing credentials and review every manifest/backend contribution.
- Clipboard, search history, OCR text and document previews can contain secrets.
  Protected state writes create owner-only files/directories but do not encrypt
  them. Disabling Clipboard stops new capture and does not erase existing data.
- [`src/security.rs`](src/security.rs) implements shared private storage and
  bounded HTTP downloads. Host applications compiling these modules must wire
  up startup directory protection and privacy preferences, as Spotty does.

Preview parsers still have no complete process sandbox. Maintenance advisories
in the app's dependency chains and other residual risks are described in the
main report. The backend is verified through Spotty's native `cargo build
--locked`; there is no separate standalone Cargo package in this repository.
