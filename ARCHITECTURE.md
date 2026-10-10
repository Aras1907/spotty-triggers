# ARCHITECTURE.md

How this repository plugs into Spotty, what lives where, and how data moves.
Paths are relative to this repository unless they start with `~` or
`.integration/`. Items marked **(Spotty checkout)** are not in this repo.

## Two checkouts

| Where | What | Git |
|---|---|---|
| `/home/aras/development/spotty-triggers` (this repo) | Trigger manifests, Store catalog, native backends, Proton crates | Remote: GitHub `Aras1907/spotty-triggers`, branch `main` |
| `.integration/Spotty` (**Spotty checkout**) | Spotty's application code, `Cargo.toml`, GTK UI, daemon | Separate repo; ignored by this repo (`/.integration/` in `.gitignore`) |
| `.integration/Spotty/trigger-backends/` (**Spotty checkout**) | A copy of this repo's files | Its own `.git` directory, not a link |

The copy is refreshed with `python3 tools/sync_to_spotty.py` from this repo. The
script copies every tracked or non-ignored file, skipping `.integration/`,
`build/` and `.cargo-proton/`. It also removes stale files under `triggers/`
and `src/` in the copy. Never edit the copy by hand.

Spotty loads this repo's Rust files with `#[path = "../trigger-backends/src/..."]`
attributes and depends on the path crates (`proton-account`,
`proton-pass-embedded`, `proton-vpn-embedded`, `proton-bridge-gui`). The
`README.md` calls `trigger-backends` a pinned submodule; the working setup is
the copy described above. Check with `git -C .integration/Spotty/trigger-backends status`
before assuming either.

Spotty's own `CLAUDE.md` (in the Spotty checkout, read-only for us) is partly
out of date. It describes a Flatpak-first build and says there is no test
suite. For this workspace, `AGENTS.md` wins: native Cargo builds and the tests
listed in `AGENTS.md`. Its rules on LibreOffice, the single GTK main thread and
`futures` (not tokio) still apply. The glib signal-handler rule (always return
`Some(bool)`) and the gsettings rule (quote string values) come from this
project's brief, not from that file; they are kept as rules in `AGENTS.md`.

## Repository map

```
index.json              Store catalog: id, name, word, description, icon,
                        version, author, shortcut, native, preinstalled.
                        Spotty fetches it from GitHub main, so changes go live
                        only after a push.
triggers/<id>.json      One manifest per trigger: the install payload, with
                        help text, help_image and the action.
src/defaults.rs         Loads index.json (include_str!) for native defaults.
                        Proton trigger table PROTON_TRIGGERS, supports_native,
                        command_keyword(s), DEFAULT_ORDER, RESULT_IDS, display
                        and result titles.
src/search/             Search dispatch and result providers.
  mod.rs                SearchResult, the Action enum, search_mode() routing
                        by keyword id, trigger_suggestions(), result_mode()
                        for result types, and web/shell manifest handling.
  proton.rs             VPN, Calendar and Drive search rows, and the synced-folder
                        search for Drive.
  pass.rs               Proton Pass search rows: titles, vault names and item kinds
                        (usernames and web addresses aren't in the listing).
  calculator.rs, convert.rs, web.rs, apps.rs, ...   Other providers.
src/features/           Feature services.
  proton_native.rs      Spotty's Proton sign-in state and saved session
                        (session.json; the key password in the keyring), Drive and
                        Calendar calls (blocking; run off the GTK thread).
  proton_session.rs     Which Proton apps are signed in or signing in (the
                        "signing in" marker the UI shows). No sharing.
  proton_pass_login.rs  Proton Pass's own sign-in: `pass-cli login --interactive`
                        on a pseudo-terminal; the password, 2FA code and mailbox
                        password are typed into it.
  proton_pass_sequence.rs  The copy-all plan (which values, in what order, when a
                        paste counts). Pure, unit-tested; the clipboard side is
                        `ui/proton_pass_paste.rs`.
  proton_pass_item.rs   Parses `pass-cli item view --output json` into the fields
                        the vault window shows (pure functions, unit-tested).
  proton_pass_settings.rs  Pass preferences in `pass-settings.json` (clipboard
                        delay, usernames, default vault, title search).
  proton_pass_pin.rs    The optional PIN in front of Proton Pass: salted
                        PBKDF2 hash, timed unlock, the prompt and its settings.
  proton_pass.rs        Drives the embedded pass-cli child process: listings,
                        field reads, TOTP, copies, sign-out.
  proton_bridge.rs      Whether Mail Bridge is supported on this platform.
  clipboard.rs, fileops.rs, ocr.rs, packagekit.rs, ...   Other services.
src/ui/                 Shared UI pieces that Spotty also compiles.
  proton_native_ui.rs   The Calendar and Drive sign-in windows (one per app), their
                        settings wrap, the Drive browser and the Calendar agenda.
                        Its sign-in is the shared page (below).
  proton_pass_ui.rs     The Proton Pass vault window (view and copy only), Pass's
                        sign-in backend for the shared page, and the Pass settings
                        groups.
  feature_settings.rs   Feature settings dialogs (included in settings_window).
src/security.rs         Private storage helpers and bounded HTTP downloads.
proton-account/         Rust crate: Proton API client and sign-in for Calendar and
                        Drive (see Proton stack), and the keyring store for the key
                        passwords (`secrets.rs`).
proton-login/           Rust crate `spotty-proton-login`: the one sign-in page every
                        Proton app shows (GTK/libadwaita), and its `Backend` trait.
                        Used by Spotty and by `proton-bridge-gui`.
proton-pass-embedded/   build.rs + prepare_bundle.py + sources.json +
                        pass-cli.Cargo.lock: builds Proton's pass-cli.
proton-vpn-embedded/    build.rs + prepare_bundle.py + sources.json +
                        requirements.lock + helper/spotty_vpn_helper.py: Proton's
                        VPN library, pinned and hashed.
proton-bridge-gui/      Crate for the optional Proton Mail Bridge service.
tools/sync_to_spotty.py Copies this repo into the Spotty checkout.
build/, .cargo-proton/  Local caches. Gitignored. Not part of the source.
.claude/skills/         Project skills for Claude Code.
```

Spotty-side files (**Spotty checkout**, local only):

- `Cargo.toml`: path dependencies on the four Proton crates; features
  `proton-vpn` and `proton-pass`.
- `src/main.rs`: `#[path]` includes of the repo's modules (about lines 26-77),
  `mod proton_vpn;`, daemon start, the PID file and `--toggle`/`--quit` handling.
- `src/ui/mod.rs`: `#[path]` includes of `proton_native_ui.rs`, `proton_pass_ui.rs`,
  `proton_pass_preview.rs` and `proton_pass_paste.rs`.
  `src/ui/settings_window.rs`: `#[path]` include of `feature_settings.rs`; it also
  builds the Proton VPN window (with its "Your Proton account" section), the
  Proton Pass settings and the Calendar and Drive settings.
- `src/ui/search_window.rs`: the `match` on `Action` that runs each action
  (Proton arms around lines 6360-6380).
- `src/proton_vpn.rs`: Spotty's client for the VPN helper (JSON lines).
- `src/triggers.rs`: `RepoTrigger`, `TriggerAction` and `is_service()`.
- `src/app.rs`: daemon, SIGUSR1 handler, keybinding registration.

## Trigger flow

Manifest to result:

1. **Store or import.** The Store reads `index.json` from GitHub. A manifest
   file can also be imported locally. Installing a trigger stores its word,
   shortcut and enabled flag in Spotty's config. Native entries (`"action": {"type": "native"}`)
   only enable a backend that is already compiled in. They cannot add code.
2. **Keyword lookup.** The user types `cal tomorrow`. Spotty finds the keyword
   for the word `cal` (`config.keyword_for_word`).
3. **Routing.** `src/search/mod.rs` `search_mode()` matches on `kw.id`:
   `proton-vpn` goes to `proton::vpn_search`, `proton-pass` to `pass::search`,
   `proton-calendar` to `proton::calendar_search`, `proton-drive` to
   `proton::drive_search`. Result types (`apps`, `calc`, `convert`, `web`, ...)
   go through `result_mode()`. Web and shell manifests go through the
   `TriggerAction` branch in the same file.
4. **Result rows.** Each provider returns `SearchResult { kind, title, subtitle,
   icon, action, score }`. The `Action` enum (top of `src/search/mod.rs`) is the
   contract with Spotty. Examples: `OpenUrl`, `RunCommand`, `EnterMode`,
   `OpenProtonVpn`, `ProtonVpn { op, target }`, `ProtonPass { op, target }`,
   `ProtonNative { op, target }`, `Bluetooth { op, mac }`.
5. **Dispatch.** Spotty's `search_window.rs` matches the `Action` and calls the
   feature. Proton actions run on background threads. Their results come back
   as notifications or windows. An `Action` never carries a secret; secrets are
   fetched at the moment they are copied.

A new Action variant must be handled in Spotty's `search_window.rs` match, so
the compiler points at every place that needs it.

Native backend versus manifest-only: web, files and shell manifests need no
Rust and no rebuild of Spotty. Anything with `"action": {"type": "native"}`
needs a Rust provider in this repo and a rebuild of Spotty.

## The Proton stack

Every Proton app in Spotty has a sign-in of its own, with its own session. No
session is forked, copied or shared, and no password is reused for another app.
Signing out of one app never touches another.

| Integration | Trigger | Sign-in | Runs |
|---|---|---|---|
| Calendar | `cal` | Its own SRP sign-in, on the shared sign-in page, in the Proton Calendar sign-in window or settings | `proton-account` crate, in process |
| Drive | `drive` | Its own SRP sign-in, on the shared sign-in page, in the Proton Drive sign-in window or settings | `proton-account` crate, in process |
| Pass | `pass` | Its own password sign-in on the shared sign-in page (settings and vault window): `pass-cli login --interactive`, answered on a pseudo-terminal | `pass-cli` child process |
| VPN | `vpn` | Its own username and password sign-in on the shared sign-in page (two-factor included) | Python helper process (JSON lines) |
| Mail Bridge | none (service) | Its own sign-in on the shared sign-in page in Bridge's window, driving Bridge's login stream | Bridge engine (`proton-bridge-gui`) |

### The shared sign-in page (`proton-login/`)

Every Proton app shows the same page, `spotty_proton_login::LoginPage`, built
from one libadwaita layout: the app's icon and "Sign in to Proton <App>", then
a boxed list with "Email or username" and "Password", a **Sign in** pill (with a
spinner while busy) and a "Create a free Proton account" link. Errors go to one
banner. Proton can ask for more: the same page then swaps in a "Two-factor code"
step, or a second-password step (mailbox password, or Pass's extra password), each
with Back and Continue. Enter submits a step. Back cancels the sign-in at the
backend and wipes the fields. Closing the window does the same.

An app plugs its own sign-in in through the `Backend` trait: `begin(email,
password) -> Step`, `submit_code`, `submit_second`, `cancel`, with `Step` =
`Done | TwoFactor | SecondPassword{title, label} | Failed(message)`. Each call
blocks, so the page runs it with `gio::spawn_blocking`. The backend of each app is
in its own module: `proton_native_ui.rs` (Calendar, Drive), `proton_pass_ui.rs`
(Pass), `settings_window.rs` (`VpnLogin`, VPN) and `proton-bridge-gui/src/gui.rs`
(`BridgeLogin`, through `BridgeLink`, which passes Bridge's login stream answers to
the waiting call).

`sign_in_stack(login, page, session)` shows the page while signed out, the app's
settings once signed in, and a "checking" page until the saved session has loaded.
Signed in, the settings start with an **Account** group: the email and **Sign out**
(a confirmation `adw::AlertDialog`). Signing out returns to the sign-in page. Mail
Bridge does not use this stack: its window keeps Bridge's own accounts list (each
account with Settings and Sign out) and shows the sign-in page while no account is
connected.

### proton-account crate (`proton-account/src/`)

- `auth.rs`: SRP sign-in (`core/v4/auth/info`, proof, `core/v4/auth`), server
  proof check, two-factor (`auth/v4/2fa`), mailbox password and the key
  password that unlocks keys. `derive_key_password`, `revoke`. The login
  password is held (`Pending`) only while a two-factor code or mailbox password
  is still to be entered.
- `api.rs`: HTTPS JSON requests with Proton's headers and error format, token
  refresh (`on_refresh` callback). `APP_VERSION` is
  `external-drive-spotty@<version>-stable`.
- `client.rs`: a signed-in `Client`: user and address keyrings, unlocked in
  memory, plus caches of unlocked keys.
- `drive.rs`: list folders, decrypt names, download and decrypt files (block
  hashes checked). `safe_file_name`.
- `calendar.rs`, `ical.rs`: calendars, events, repeating events.
- `pgp.rs`: the OpenPGP operations used (Proton's `proton-crypto`, rustpgp).
- `error.rs`: error type, including `is_signed_out()`.
- `tests/fake_proton.rs`: a local fake Proton server with real SRP, real keys
  and real ciphertext. It covers sign-in, two-factor, key chain, Drive names and
  downloads, and Calendar events. It cannot prove that Proton's live service
  behaves the same.

### Native state and windows (Calendar and Drive)

- `src/features/proton_native.rs`: one `Service` per app (`Calendar`, `Drive`).
  Each has its own state (a `OnceLock<Mutex<State>>`): the signed-in client, the
  pending sign-in step and its own half-finished session, a generation counter,
  and the Drive index or Calendar cache. Each saves its session in its own folder,
  `~/.local/share/spotty/proton-calendar/` or `proton-drive/` (mode 0700; the
  file 0600, ignored if group or other can read it). The file holds the tokens and
  the account's name and email. The key password goes to the desktop keyring
  (`spotty_proton_account::SecretService`, service `com.spotty.Spotty.proton`,
  names `proton-calendar` and `proton-drive`). Sign-out revokes that app's
  session at Proton, deletes its file and its keyring entry.
- Loading the saved session (`client`, `load_saved`) reads the keyring, so it runs
  on a worker thread (`load_in_background`). `signed_in` never blocks: until the
  session has loaded it reports "checking". An old file that still holds
  `key_password` is migrated into the keyring and rewritten without it. A file
  whose key the keyring can't return is treated as signed out and deleted. If the
  keyring refuses a new sign-in (not running), the sign-in is refused and its tokens
  are revoked in the background.
- `src/ui/proton_native_ui.rs`:
  - `open_account_window(parent, service)`: the sign-in window of one app. Signed
    out it shows the shared sign-in page; signed in, the Account group and a note.
    Closing it during a sign-in cancels that sign-in at Proton.
  - `add_account_group` and `SettingsSignIn::wrap`: the Calendar and Drive settings
    popups. Signed out, only the sign-in page; signed in, the settings with the
    Account group first.
  - `handle(parent, op, target)` routes the `ProtonNative` actions. `open_drive`
    and `open_agenda` build the Drive browser and the Calendar agenda. A small
    `run()` helper moves blocking work to `gio::spawn_blocking` and returns to the
    GTK thread.
- `src/search/proton.rs`: search rows for `cal` and `drive`, including the
  synced-folder search for `drive` (for example a local rclone folder).
- `src/features/proton_session.rs`: `app_signed_in(id)`, `after_install(id)` (the
  Store's hook: reports whether the app already has its own working sign-in),
  and `SigningIn`, the marker the UI reads while an app is signing in.

### Proton Pass

- `src/features/proton_pass.rs` runs `pass-cli` as a short-lived child with a
  cleared environment (`command()`), its own session folder, the desktop keyring,
  and the update check and telemetry off. Listings use `item list --output json`
  without `--show-secrets`. Fields are read only when needed: `item view
  --output json` for the vault window (`view_item`), `item view --field` for the
  search rows, `item totp --output json` for the live one-time code (`totp_now`).
  Copies go through `copy_secret`, marked as passwords, cleared after the delay
  in the settings (`clear_after_secs`).
- Sign-in (`src/features/proton_pass_login.rs`): `begin(email, password)` starts
  `pass-cli login --interactive <email>` on a new pseudo-terminal (`posix_openpt`,
  made the controlling terminal with `setsid` and `TIOCSCTTY`, echo off on the
  slave). `pass-cli` reads its password from `/dev/tty` (rpassword), so Spotty writes
  the password, then any two-factor code or mailbox password, to the terminal when
  each prompt appears. A prompt is the last unfinished line ending in a colon. The
  client's "Successfully logged in" line or exit status 0 means done. Secrets are
  never on argv or in the environment.
- `src/features/proton_pass_item.rs` parses the item JSON (the field names follow
  Proton's item model in `proton-pass-types`). It is tolerant about nesting; the
  exact JSON from `pass-cli` has not been checked against a live account.
- `src/ui/proton_pass_ui.rs`: the vault window (`open_vault`), an `adw` window
  opened from the **Open vault…** settings row or the **Open Proton Pass vault**
  trigger row. It is not modal and has its own window group. The item list holds
  titles and usernames only; an item's fields are held while its detail is shown
  and wiped on switch, lock and close. Revealed secrets re-mask after 30 s. The
  window follows the PIN lock every second and blanks itself when it locks.
  `settings_sign_in` and `SettingsSignIn::wrap` are the Pass settings (signed out:
  only the sign-in page; signed in: the Account group, the PIN and the settings),
  `PassLogin` is Pass's backend for the shared page, and `add_settings_group`
  holds the clipboard delay, usernames, default vault and title-search settings.
- Search (`src/search/pass.rs`): the listing has titles, vault names, item kinds
  and ids (pass-cli 2.4.2's `item list --output json` has no username, email, URL
  or secret). Ranking: title matches first, then username, then vault or kind, then
  words spread over fields. The `username` field of `Item` is read if a listing ever
  carries it; with the pinned client it stays empty.
- `src/ui/proton_pass_preview.rs` (`PassPreview`, one per search window): the item
  the selected `pass` row names, with its username, masked password, one-time code
  and website, each with a copy button, and "copy all" for a login. It is fetched
  250 ms after the selection settles, only while its page is mapped and Pass is
  unlocked; a different selection, an unmap or a lock drops its fields.
  `features/preview.rs` holds it as the `pass` page of `PreviewPane`.
- Copy all (`src/features/proton_pass_sequence.rs`, `src/ui/proton_pass_paste.rs`):
  a `gdk::ContentProvider` subclass offers the username, then the password, then
  the one-time code (read at its paste). A text read by another app (a write made
  while no Spotty window has focus), after 300 ms of quiet, offers the next step.
  The provider carries the password hint and has no storable formats. It ends after
  60 s, on a lock, or when another copy replaces it, and then clears the clipboard
  if the sequence still holds it. `clipboard.rs` checks the hint before it reads.
- `src/features/proton_pass_settings.rs` stores those settings in
  `~/.config/spotty/pass-settings.json` (mode 0600). They hold no secret.
- `src/features/proton_pass_pin.rs` holds the PIN lock. With a PIN set, the
  `pass` trigger shows one locked row, the vault window shows its lock page, and
  `run_search_action` and every copy ask for the PIN before a secret is read. Only
  a salt and a PBKDF2-HMAC-SHA256 hash are stored. Auto-lock choices: 1, 2, 5, 10,
  15, 30, 60 and 240 minutes, or Lock now. It is a lock on Spotty's use of Pass,
  not encryption of the vault.
- `proton-pass-embedded/build.rs` (with the `bundled` feature) runs
  `prepare_bundle.py`, which fetches the source pinned in `sources.json`, checks
  its SHA-256, and builds `pass-cli --locked`. The binary is compressed into Spotty.

### Proton VPN

- `proton-vpn-embedded/` pins Proton's Python core packages and their
  dependencies. `prepare_bundle.py` checks each source hash and installs wheels
  with `pip --require-hashes`. The bundle is compressed into Spotty.
- `helper/spotty_vpn_helper.py` runs Proton's library and talks JSON lines with
  Spotty. The username, password and 2FA code go straight to the library over
  stdin. The library identifies itself as Proton's Linux GUI client
  (`ClientTypeMetadata(type="gui")`), not as `external-drive-spotty`.
- Spotty's `src/proton_vpn.rs` (**Spotty checkout**) owns the helper process.
  Every call blocks, so it runs off the GTK thread.

### Proton Mail Bridge

A service, not a search trigger (`category: service`). `proton-bridge-gui/`
holds the login companion and the bundled Bridge engine. Its own README covers
packaging and verification. Its sign-in is its own; Spotty does not follow it or
reuse its details. `src/features/proton_bridge.rs` only reports whether Bridge is
supported on this platform.

## Data locations

| Path | Holds | Written by |
|---|---|---|
| `~/.config/spotty/config.json` | Spotty settings, including `trigger_repo_url` (no UI field) | Spotty |
| `~/.config/spotty/spotty.pid` | PID of the running daemon | Spotty |
| `~/.config/spotty/spotty_keyword.txt` | Keyword passed to a summoned daemon | Spotty |
| `~/.local/share/spotty/proton-calendar/session.json` | Calendar's session: tokens, account name and email. No password and no key password. Not encrypted. Folder 0700, file 0600. A file others can read is removed. Sign-out deletes it | `proton_native.rs` |
| `~/.local/share/spotty/proton-drive/session.json` | Drive's session, same rules as Calendar's | `proton_native.rs` |
| Desktop keyring, service `com.spotty.Spotty.proton`, names `proton-calendar` and `proton-drive` | The key password (it unlocks the account's keys) of Calendar and Drive. Sign-out deletes it | `proton-account/src/secrets.rs` |
| `~/.local/share/spotty/proton-account/session.json` | Old shared session from earlier versions. No longer read or written; can be deleted by hand | (none) |
| `~/.config/spotty/pass-settings.json` | Proton Pass preferences: clipboard delay, usernames, default vault, title search. Mode 0600. No secret | `proton_pass_settings.rs` |
| `~/.config/spotty/pass-pin.json` | Salt, PBKDF2 hash and lock timeout of the Proton Pass PIN. Mode 0600. Removing the PIN deletes it | `proton_pass_pin.rs` |
| `~/.local/share/spotty/proton-pass/` | `pass-cli`'s own session folder (Pass's own session, from its own sign-in); key in the desktop keyring | `pass-cli` |
| `~/.cache/spotty/proton-pass/<bundle id>/` | Unpacked `pass-cli` (owner-only folder) | `proton_pass.rs` |
| `~/.cache/spotty/proton-vpn/<bundle id>/` | Unpacked VPN bundle and helper | `proton_vpn.rs` (**Spotty checkout**) |
| Desktop keyring | Pass's session key, VPN library session | Proton's clients |
| Bridge's vault and keyring entries | Bridge's saved accounts and logins (the README of `proton-bridge-gui` describes them) | Bridge engine. Spotty keeps no Bridge password |
| `build/spotty-proton-pass-cache/`, `build/spotty-proton-vpn-cache/` | Build caches (gitignored). Overridden by `SPOTTY_PROTON_PASS_CACHE` and `SPOTTY_PROTON_VPN_CACHE` | `prepare_bundle.py` |

Unlocked keys, decrypted names and calendar events are in memory only. A Drive
file saved by the user goes to the Downloads folder as a plain file.

## Gaps and stale spots (known, not fixed here)

- **No Proton sign-in has been tried against Proton's live service.** The fake
  server covers Calendar and Drive (protocol and crypto). Pass's interactive
  login has not been run against a live account, and neither has VPN's sign-in
  on the shared page.
- **Pass's login prompts are matched by text.** `proton_pass_login` answers a
  prompt that contains "password" or a code word ("code", "2fa", "totp",
  "authenticator"). If `pass-cli` words a prompt differently, the sign-in stops
  with an error rather than guessing.
- **Pass's JSON shape is assumed.** `proton_pass_item` follows Proton's item model
  and looks keys up by name. The exact output of `pass-cli item view --output json`
  and `item totp --output json` has not been checked against a live account.
- **Pass search doesn't match usernames or web addresses.** The listing lacks
  them, and reading each item to find them would read every item (`item view` is
  one fetch of the whole item). Vault names, kinds and titles are searched.
- **The copy-all paste sequence can be advanced by any reader.** An app that reads
  the clipboard without pasting (a clipboard manager that ignores the hint) counts
  as a paste. Spotty can't tell the readers apart; it only ignores reads made while
  one of its own windows has focus.
- **Pass's one-time codes use a 30-second period.** The countdown assumes 30 s.
  Extra TOTP fields (not the login's own code) are shown as text, not as live codes.
- **Pass's sign-out is not awaited.** `sign_out_everywhere` was removed with the
  shared sign-in; the Pass settings' sign-out runs `pass-cli logout` on a background
  thread and does not wait for it.
- `src/features/proton_native.rs`: the header says the password is "forgotten"
  after the proof. It is kept while a two-factor code or mailbox password is
  pending.
- The VPN client library identifies itself as Proton's Linux GUI client, not as
  `external-drive-spotty`. SOUL.md, rule 4, says Spotty never poses as one of
  Proton's apps. Whether this is acceptable is for the maintainer to decide.
- OpenPGP signature checks on Drive names and calendar events are not done yet.
- FIDO2-only accounts, Proton's human-verification check, shared-with-me, photos
  and trash in Drive, and all write operations are not supported. The Pass window
  is view and copy only.
- The Pass favicon feature is not implemented. It would fetch websites, so it was
  left out rather than offered as a switch that does nothing.
