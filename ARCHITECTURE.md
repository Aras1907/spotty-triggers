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
  pass.rs               Proton Pass search rows (item titles only).
  calculator.rs, convert.rs, web.rs, apps.rs, ...   Other providers.
src/features/           Feature services.
  proton_native.rs      Spotty's Proton sign-in state and saved session
                        (session.json), Drive and Calendar calls (blocking; run
                        off the GTK thread).
  proton_session.rs     The one Proton sign-in: after-install hook, sharing it
                        with Pass and VPN, and the VPN fork hand-off.
  proton_web.rs         WebKitGTK window for Proton web pages (for example Open
                        Proton Pass). Not used to sign in.
  proton_pass.rs        Drives the embedded pass-cli child process, including
                        its sign-in approval.
  proton_bridge.rs      Whether Mail Bridge is supported on this platform.
  clipboard.rs, fileops.rs, ocr.rs, packagekit.rs, ...   Other services.
src/ui/                 Shared UI pieces that Spotty also compiles.
  proton_native_ui.rs   The Proton account window, the sign-in row of the
                        Calendar and Drive settings, the Drive browser and the
                        Calendar agenda.
  feature_settings.rs   Feature settings dialogs (included in settings_window).
src/security.rs         Private storage helpers and bounded HTTP downloads.
proton-account/         Rust crate: Proton API client, sign-in and session forks
                        (see Proton stack).
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
- `src/ui/mod.rs`: `#[path]` include of `proton_native_ui.rs`.
  `src/ui/settings_window.rs`: `#[path]` include of `feature_settings.rs`; it also
  builds the Proton VPN window (with its "Your Proton account" section), the
  Proton Pass settings and the Calendar and Drive settings.
- `src/ui/search_window.rs`: the `match` on `Action` that runs each action
  (Proton arms around lines 6360-6380).
- `src/proton_vpn.rs`: Spotty's client for the VPN helper (JSON lines), including
  `import_fork`.
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

Four Proton integrations share one sign-in. Calendar and Drive use it directly.
Pass and VPN each get a session forked from it.

| Integration | Trigger | Sign-in | Runs |
|---|---|---|---|
| Calendar, Drive | `cal`, `drive` | The shared sign-in, used directly. SRP sign-in in Spotty's Proton account window | `proton-account` crate, in process |
| Pass | `pass` | The shared sign-in. Spotty forks a `cli-pass` child session and approves the link that `pass-cli login` prints | `pass-cli` child process |
| VPN | `vpn` | The shared sign-in. Spotty forks a `linux-vpn-gui` child session that the helper imports. VPN's own username and password form is the fallback | Python helper process (JSON lines) |
| Mail Bridge | none (service) | Bridge's own window | Bridge engine (`proton-bridge-gui`) |

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
- `fork.rs`: session forks for Proton's other official clients (see below).
- `pgp.rs`: the OpenPGP operations used (Proton's `proton-crypto`, rustpgp).
- `error.rs`: error type, including `is_signed_out()`.
- `tests/fake_proton.rs`: a local fake Proton server with real SRP, real keys
  and real ciphertext. It covers sign-in, two-factor, key chain, Drive names and
  downloads, Calendar events, and the VPN and Pass forks (including a refused
  fork for another app). It cannot prove that Proton's live service behaves the
  same.

### Session fork (shared sign-in with Pass and VPN)

Proton lets one session create a child session for another of Proton's clients.
Spotty asks for that child (`POST auth/v4/sessions/forks`) and hands it to the
other app. No login password is shared.

- Whitelist: `ALLOWED_CHILDREN` in `fork.rs` allows exactly `cli-pass`
  (`PASS_CLIENT_ID`, Proton Pass CLI) and `linux-vpn-gui` (`VPN_CLIENT_ID`,
  Proton VPN). `fork_session` refuses any other id before it sends a request.
  Adding a client needs a code change, a test and a review.
- `Independent: 0` ties the child to Spotty's session, so signing that session
  out should revoke the child too. The code relies on this; it is not tested
  live.
- `fork_for_vpn()` returns a selector for VPN. It is called by
  `proton_session::hand_to_vpn`. The VPN helper's `import_fork` command takes the
  selector and the account email over stdin and exchanges the selector for a
  session. The selector is never logged or saved.
- `approve_pass_login(&PassLogin)` approves a `pass-cli login` link. It is called
  by `proton_pass::sign_in`. Only the exact shape
  `https://account.proton.me/desktop/login?app=pass#payload=...` is accepted
  (`parse_pass_login_url`). The key password goes to Proton only inside an
  AES-256-GCM payload, keyed with the one-time login key from that link
  (`pass_fork_payload`). Spotty sends the user code, the client id and that
  payload. It never sends the login key.
- Callers: `grep -rn "fork_for_vpn\|approve_pass_login\|import_fork" --include=*.rs --include=*.py .`
- Status: both forks are wired into the app. VPN starts from the account
  window's Sign in button, the VPN window's account button, a native sign-in
  (`share_sign_in`) and installing VPN while signed in (`after_install`). Pass
  starts from the Pass settings' sign-in row, the `pass` "Sign in" row, the
  account window's Sign in button, a native sign-in and installing Pass while
  signed in. Neither fork has been tried against Proton's live service.

### Native state and windows (Calendar, Drive and the shared sign-in)

- `src/features/proton_native.rs`: state lives in a process-wide
  `OnceLock<Mutex<State>>` holding the signed-in client, a pending sign-in step
  (`Outcome`: `Done`, `TwoFactor`, `MailboxPassword`, `Failed`), a generation
  counter (bumped on each sign-in or out so stale work can tell), and in-memory
  Drive and Calendar caches. Saving and loading `session.json` is in the same
  file. Sign-out revokes the session at Proton and deletes the file.
- `src/ui/proton_native_ui.rs`:
  - `open_account_window(parent)`: the Proton account window. It has the
    sign-in form (Proton email, password, two-factor code, mailbox password), a
    signed-in page with the **Proton apps** list (each app's state, and a
    **Sign in** button for Pass and VPN), and **Sign out**. Sign-out asks first,
    then signs out of everything (`AccountWindow::sign_out_everywhere`): Pass,
    VPN, the native session and the web profile. Closing the window during a
    sign-in cancels that sign-in at Proton.
  - `add_account_row(page)`: the "Proton account" group in the Calendar and Drive
    settings, a signed-in row with **Manage…** or a "Sign in to Proton" row. Both
    open the account window. Pass and VPN have their own account sections in
    `settings_window.rs`.
  - `handle(parent, op, target)` routes the `ProtonNative` actions. `open_drive`
    and `open_agenda` build the Drive browser and the Calendar agenda. A small
    `run()` helper moves blocking work to `gio::spawn_blocking` and returns to the
    GTK thread.
- `src/search/proton.rs`: search rows for `cal`, `drive` and `vpn`, including the
  synced-folder search for `drive` (for example a local rclone folder).
- `src/features/proton_session.rs`:
  - `after_install(id)`: called when a Proton integration is installed in the
    Store. Calendar and Drive report signed in when a native session exists. Pass
    and VPN sign in with the shared sign-in when there is one.
  - `share_sign_in()`: after a native sign-in, gives each installed Pass or VPN
    that is not signed in yet the shared sign-in.
  - `share_with(id)`: gives one app the shared sign-in (the account window's Sign
    in button).
  - `hand_to_vpn()`: forks a VPN session and imports it. Runs off the GTK thread.
  - `app_signed_in(id)`, `signing_in(id)`: what the UI shows for each app.

### Proton Pass

- `src/features/proton_pass.rs` runs `pass-cli` as a short-lived child with a
  cleared environment, its own session folder, the desktop keyring, and the
  update check and telemetry disabled. Listings use `item list --output json`
  without `--show-secrets`. Secrets are read with `item view --field` when the
  user picks an action, then cleared from the clipboard after 30 seconds.
- Sign-in (`sign_in`): runs `pass-cli login` and reads the approval link from its
  output. The link is parsed and approved with the native session
  (`approve_pass_login`). Without a native session, the account window opens
  instead, and Pass signs in once Spotty is signed in to Proton (`share_sign_in`).
  A sign-in that nobody finishes is stopped after 10 minutes.
- `src/features/proton_web.rs` provides the WebKitGTK window used for
  `OpenProtonWeb` pages (for example "Open Proton Pass"). It does not sign in.
  Without WebKitGTK 6 links open in the default browser.
- `proton-pass-embedded/build.rs` (with the `bundled` feature) runs
  `prepare_bundle.py`, which fetches the source pinned in `sources.json`, checks
  its SHA-256, and builds `pass-cli --locked`. The binary is compressed into Spotty.

### Proton VPN

- `proton-vpn-embedded/` pins Proton's Python core packages and their
  dependencies. `prepare_bundle.py` checks each source hash and installs wheels
  with `pip --require-hashes`. The bundle is compressed into Spotty.
- `helper/spotty_vpn_helper.py` runs Proton's library and talks JSON lines with
  Spotty. The password, 2FA code and fork selector go straight to the library
  over stdin. The library identifies itself as Proton's Linux GUI client
  (`ClientTypeMetadata(type="gui")`), not as `external-drive-spotty`.
- Spotty's `src/proton_vpn.rs` (**Spotty checkout**) owns the helper process.
  Every call blocks, so it runs off the GTK thread.

### Proton Mail Bridge

A service, not a search trigger (`category: service`). `proton-bridge-gui/`
holds the login companion and the bundled Bridge engine. Its own README covers
packaging and verification. `src/features/proton_bridge.rs` only reports whether
Bridge is supported on this platform.

## Data locations

| Path | Holds | Written by |
|---|---|---|
| `~/.config/spotty/config.json` | Spotty settings, including `trigger_repo_url` (no UI field) | Spotty |
| `~/.config/spotty/spotty.pid` | PID of the running daemon | Spotty |
| `~/.config/spotty/spotty_keyword.txt` | Keyword passed to a summoned daemon | Spotty |
| `~/.local/share/spotty/proton-account/` | Folder, mode 0700 | `proton_native.rs` |
| `~/.local/share/spotty/proton-account/session.json` | Spotty's Proton session: tokens and key password, not encrypted. Mode 0600. Ignored (treated as signed out) if group or other can read it. Sign-out deletes it. The Pass and VPN sessions are forked from it | `proton_native.rs` |
| `~/.local/share/spotty/proton-web/` | Private WebKit profile for Proton web pages. Not used to sign in. Sign-out erases it | `proton_web.rs` |
| `~/.local/share/spotty/proton-web/signed-in` | Empty marker, written when a Proton web page with an account path loads. Nothing reads it | `proton_web.rs` |
| `~/.local/share/spotty/proton-pass/` | `pass-cli`'s own session folder (Pass's forked session); key in the desktop keyring | `pass-cli` |
| `~/.cache/spotty/proton-web/` | WebKit cache for the Proton web window | `proton_web.rs` |
| `~/.cache/spotty/proton-pass/<bundle id>/` | Unpacked `pass-cli` (owner-only folder) | `proton_pass.rs` |
| `~/.cache/spotty/proton-vpn/<bundle id>/` | Unpacked VPN bundle and helper | `proton_vpn.rs` (**Spotty checkout**) |
| Desktop keyring | Pass's session key, VPN library session | Proton's clients |
| `build/spotty-proton-pass-cache/`, `build/spotty-proton-vpn-cache/` | Build caches (gitignored). Overridden by `SPOTTY_PROTON_PASS_CACHE` and `SPOTTY_PROTON_VPN_CACHE` | `prepare_bundle.py` |

Unlocked keys, decrypted names and calendar events are in memory only. A Drive
file saved by the user goes to the Downloads folder as a plain file.

## Gaps and stale spots (known, not fixed here)

- **The Pass and VPN forks have not been tried against Proton's live service.**
  Proton may refuse a fork from Spotty's third-party session. VPN then falls back
  to its own form. Pass has no separate form.
- **Pass's sign-out is not awaited.** `sign_out_everywhere` (`proton_native_ui.rs`)
  calls `proton_pass::sign_out()`, which runs on a background thread. Spotty then
  signs out of Proton without waiting for it. The code comment's "Pass and VPN
  first" order is therefore not guaranteed.
- `src/features/proton_pass.rs`: the module header (lines 10-14) still says Pass
  sign-in is Proton's web sign-in. That is stale.
- `src/features/proton_native.rs`: the header says the password is "forgotten"
  after the proof. It is kept while a two-factor code or mailbox password is
  pending.
- `src/features/proton_web.rs`: `sign_in()` opens Proton's sign-in page, and
  nothing calls it. `signed_in()` reads the marker file, and nothing calls that
  either. Both are dead code.
- `triggers/proton-calendar.json` and `triggers/proton-drive.json` say to sign in
  "in the settings, in Spotty's own form". The sign-in is now the Proton account
  window. `triggers/proton-vpn.json` says to sign in in the VPN window with a
  username and password. That is now the fallback. These manifests were not
  updated in this pass.
- The VPN client library identifies itself as Proton's Linux GUI client, not as
  `external-drive-spotty`. SOUL.md, rule 4, says Spotty never poses as one of
  Proton's apps. Whether this is acceptable is for the maintainer to decide.
- In this repo only `proton_native.rs` reads or writes `session.json`.
- OpenPGP signature checks on Drive names and calendar events are not done yet.
- FIDO2-only accounts, Proton's human-verification check, shared-with-me, photos
  and trash in Drive, and all write operations are not supported.
- The live sign-in path, including the Pass and VPN forks, has not been run
  against Proton's live service. The fake server is the only test of the sign-in
  chain.
