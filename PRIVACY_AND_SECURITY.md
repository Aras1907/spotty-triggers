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
  has its own sign-in: Spotty runs `pass-cli login --interactive <email>` on a
  pseudo-terminal and types the password, two-factor code or mailbox password
  into that terminal when the client asks for it. None of them is put on the
  command line, in the environment or on disk, and no web page opens. The
  terminal has echo off. Spotty keeps only the listing (item titles, vault names
  and item kinds) in memory, never writes it to disk, and forgets it when Pass
  locks. Proton Pass's listing has no usernames or web addresses, so search does
  not match them, and Spotty does not fetch every item to make them searchable.
  When an item is opened, or its row is previewed in the search window, its fields
  are read into memory for as long as it is on screen; switching items, locking,
  hiding the search window or closing wipes them. The search preview fetches an
  item only once the selection has settled for 250 ms and only while it is shown,
  and it shows the password masked, with no reveal.
  The copy-all paste sequence holds the username and password only until each is
  pasted, reads the one-time code when its paste happens, and ends after 60 s, when
  Pass locks, or when anything else is copied. It clears the clipboard when it
  ends. A program that reads the clipboard without pasting (a clipboard manager that
  ignores the password hint) can advance it. Spotty's clipboard history never reads
  these copies.
  Secrets are masked until revealed (re-masked after 30 seconds). Copies are
  marked as passwords on the clipboard, and cleared after the delay chosen in
  settings (15 s to 5 min, 30 s by default). A running desktop can still read the
  clipboard during that time, and GTK keeps a copy of the text until it is
  cleared. The client's dependencies come from Proton's Cargo registry and GitHub,
  pinned by `pass-cli.Cargo.lock`; they are not independently audited here.
- Each Proton app has its own sign-in and session: Calendar and Drive through
  Spotty's native client ([`proton-account/`](proton-account/)), Pass through
  `pass-cli`, VPN through Proton's VPN library, Mail Bridge through its engine.
  No session is forked, copied or shared between them, and no password is reused
  for another app. Every Proton app shows the same sign-in page
  ([`proton-login/`](proton-login/)): you type your email and password into it,
  then a two-factor code or a mailbox password if Proton asks. The entries are
  emptied as soon as they are read, and Enter or Back never keeps them. Calendar and
  Drive turn the password into an SRP proof and verify Proton's proof back; Pass,
  VPN and Mail Bridge pass the password to their own client, which signs in.
  Spotty keeps no password, two-factor code, mailbox password or Pass extra password
  after a sign-in. Each app's session tokens and its account's name and email are
  saved in its own folder (`~/.local/share/spotty/proton-calendar/session.json` or
  `~/.local/share/spotty/proton-drive/session.json`, owner-only, refused if
  group/other-readable). Those files hold no password of any kind. The key password
  that unlocks your Proton keys is kept in the desktop keyring (Secret Service over
  D-Bus), under `com.spotty.Spotty.proton` with the names `proton-calendar` and
  `proton-drive`, and is never written to a file. Without a running keyring Spotty
  cannot keep a sign-in, so it refuses to sign in. Unlocked keys, decrypted file
  names and calendar events live in memory only. Network traffic goes to Proton's
  API and storage hosts, over HTTPS only.
- Not tested against Proton's live service: every sign-in path (Calendar, Drive,
  Pass, VPN and Mail Bridge). Spotty's fake Proton server tests the Calendar and
  Drive protocol and crypto wiring only. Spotty does not yet verify OpenPGP
  signatures on names and events. The live path may meet Proton's anti-abuse
  checks (see the README).
- Signing out of a Calendar or Drive sign-in window ends that app's session at
  Proton, deletes its `session.json` and removes its key password from the
  keyring. Signing out in Pass's settings removes
  only Pass's session on this computer. Proton VPN's sign-out ends only VPN's
  session. No sign-out touches another app. Older versions kept one shared
  session in `~/.local/share/spotty/proton-account/session.json`; it is no
  longer read, and you can delete it. A Calendar or Drive `session.json` from
  before the keyring was used still holds its key password: Spotty moves that
  password into the keyring and rewrites the file without it at the next start.
- Proton Pass can be locked with a PIN. Spotty stores only a random salt and a
  PBKDF2-HMAC-SHA256 hash (`~/.config/spotty/pass-pin.json`, mode 0600), never
  the PIN. While locked, the `pass` trigger lists nothing and copies nothing, the
  vault window is blanked, the search preview is cleared, and the listing is
  forgotten. Auto-lock runs 1 min to 4 hours after the PIN was
  typed (or Lock now). Wrong PINs are slowed down. The PIN gates Spotty's use of
  Proton Pass; it does not encrypt the vault, and someone who can edit your config
  folder can delete it. Pass's other preferences (clipboard delay, usernames,
  default vault, title search) are in `~/.config/spotty/pass-settings.json`
  (mode 0600) and hold no secret.
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
