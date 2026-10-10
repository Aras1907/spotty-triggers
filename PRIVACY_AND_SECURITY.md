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
  environment, its own session folder (`~/.local/share/spotty/proton-pass`), its
  key in the desktop keyring and its update check and telemetry disabled. Pass
  signs in with a child session forked from Spotty's Proton session. `pass-cli`
  prints an approval link; Spotty reads it and approves it natively, and no web
  page opens. For that fork Spotty sends Proton only the user code, the client
  name and an AES-256-GCM payload that holds the key password. The 32-byte key
  that opens the payload is in the link and is never sent. Spotty keeps only item titles in memory and never
  writes them to disk. Passwords, usernames, one-time codes and notes are read
  when copied, marked as passwords on the clipboard, wiped from Spotty's buffers
  and cleared from the clipboard after 30 seconds. A running desktop can still
  read the clipboard during those 30 seconds, and GTK keeps a copy of the text
  until it is cleared. The client's dependencies come from Proton's Cargo
  registry and GitHub, pinned by `pass-cli.Cargo.lock`; they are not
  independently audited here.
- The Proton sign-in (Calendar, Drive, Pass and VPN) talks to Proton's API from
  inside Spotty (no web view; [`proton-account/`](proton-account/)). You type
  your password into a Spotty field; it is turned into an SRP proof, the server's
  proof is verified, and the field is cleared at once. The password is kept in
  memory only while a two-factor code or mailbox password is pending. Spotty
  saves Proton's session tokens and the key password derived from your password
  in `~/.local/share/spotty/proton-account/session.json` (owner-only, refused if
  group/other-readable). This file is not encrypted, so anyone who can read your
  files as you can use that session until you sign out. Unlocked keys, decrypted
  file names and calendar events live in memory only. Network traffic goes to
  Proton's API and storage hosts, over HTTPS only.
- Pass and VPN each get a session forked from Spotty's Proton session (Proton's
  session fork, requested with `Independent: 0`, which is meant to tie them to
  Spotty's session; not tested live). The fork
  requests are checked before they are sent: only Proton Pass's CLI (`cli-pass`)
  and Proton VPN's Linux client (`linux-vpn-gui`) may be forked. No login
  password is sent with a fork. VPN's session is kept in the desktop keyring by
  Proton's client library, and the fork selector is never stored or logged. The
  VPN window's own username and password form is the fallback. There the
  password goes to Proton's client, and Spotty does not store it.
- Not tested against Proton's live service: the Pass and VPN forks, and the
  whole sign-in path. Whether Proton accepts forks from Spotty's third-party
  session is unknown. Spotty does not yet verify OpenPGP signatures on names and
  events. The live path may meet Proton's anti-abuse checks (see the README).
- Sign out in the account window ends Spotty's Proton session at Proton and
  deletes `session.json`. It also signs out Pass and VPN on this computer and
  erases any Proton web profile left by older versions. Pass's local sign-out runs in the background,
  and Spotty does not wait for it. Signing out in Pass's settings removes only
  Pass's session on this computer.
- Proton Pass can be locked with a PIN. Spotty stores only a random salt and a
  PBKDF2-HMAC-SHA256 hash (`~/.config/spotty/pass-pin.json`, mode 0600), never
  the PIN. While locked, the `pass` trigger lists nothing and copies nothing.
  Wrong PINs are slowed down. The PIN gates Spotty's use of Proton Pass; it does
  not encrypt the vault, and someone who can edit your config folder can delete
  it.
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
