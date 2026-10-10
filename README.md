# Spotty Triggers

Trigger keywords for [Spotty](https://github.com/Aras1907/Spotty), the Raycast-style
launcher for GNOME. Spotty reads [`index.json`](index.json) and the manifests in
[`triggers/`](triggers/) **directly from this repository** — open
**Settings → Trigger → Store** and the list (words, descriptions,
icons, everything) arrives straight from GitHub. You can also download a
manifest file and import it locally (see
[Install a trigger from a file](#install-a-trigger-from-a-file)).

## Install a trigger from a file

1. Open the trigger's file under [`triggers/`](triggers/) on GitHub
   (e.g. [`triggers/dictionary.json`](triggers/dictionary.json)) and download it —
   **Code → Download raw file**, or:
   `curl -LO https://raw.githubusercontent.com/Aras1907/spotty-triggers/main/triggers/dictionary.json`
2. In Spotty: **Settings → Triggers → +** (or **Import Trigger File…** in the
   Triggers window).
3. Pick the downloaded `.json` file — Spotty validates it and installs it
   immediately.

Shell triggers additionally show a confirmation dialog with the exact
command before installing; web and file triggers install directly (they
open HTTP(S) links or filter files). Manifests are validated on installation
and reload. Shell templates remain arbitrary code with your user permissions.
See [PRIVACY_AND_SECURITY.md](PRIVACY_AND_SECURITY.md) for network behavior,
storage protections and remaining risks, and [SECURITY.md](SECURITY.md) to report
a vulnerability.

## Available triggers and results

| Manifest | Default word | What it does |
|---|---|---|
| [`triggers/files.json`](triggers/files.json) | `find` | Search all files and folders |
| [`triggers/clipboard.json`](triggers/clipboard.json) | `clip` | Search clipboard history |
| [`triggers/run.json`](triggers/run.json) | `cmd` | Run a command |
| [`triggers/emoji.json`](triggers/emoji.json) | `emoji` | Search emoji |
| [`triggers/bluetooth.json`](triggers/bluetooth.json) | `bt` | Bluetooth devices |
| [`triggers/apps.json`](triggers/apps.json) | Install from Store | Search installed applications |
| [`triggers/newapps.json`](triggers/newapps.json) | Install from Store | Search apps you can install |
| [`triggers/web.json`](triggers/web.json) | Install from Store | Search the web |
| [`triggers/calc.json`](triggers/calc.json) | Install from Store | Calculate arithmetic |
| [`triggers/convert.json`](triggers/convert.json) | Install from Store | Convert units, currency and number bases |
| [`triggers/updates.json`](triggers/updates.json) | Install from Store | Check and install updates |
| [`triggers/dictionary.json`](triggers/dictionary.json) | `dict` | Look up a word definition |
| [`triggers/translate.json`](triggers/translate.json) | `translate` | Translate text locally — detects the language, targets your system language |
| [`triggers/proton-vpn.json`](triggers/proton-vpn.json) | `vpn` | Proton VPN built in: sign in, pick a country and connect |
| [`triggers/proton-pass.json`](triggers/proton-pass.json) | `pass` | Proton Pass built in: search your vault, copy passwords, usernames and one-time codes |
| [`triggers/proton-calendar.json`](triggers/proton-calendar.json) | `cal` | Proton Calendar built in: sign in natively, see what is coming up, jump to any day |
| [`triggers/proton-drive.json`](triggers/proton-drive.json) | `drive` | Proton Drive built in: sign in natively, search and browse My files, download with Enter |

The six native result providers start uninstalled. Install one from the Store
to add it to regular search. Then set an optional word or shortcut in Spotty
Settings to open its dedicated mode. Native entries require the Spotty version
that supports the repository catalog.

Native result providers are available for users to install from the Store.
The Rust implementations live in [`src/search/`](src/search/) and the
catalog-based defaults in [`src/defaults.rs`](src/defaults.rs). Feature services
(clipboard, file operations, previews, OCR, updates, and operation tracking)
live in [`src/features/`](src/features/), and feature settings dialogs in
[`src/ui/feature_settings.rs`](src/ui/feature_settings.rs). Installing
or removing them preserves customized words, shortcuts, and ordering. App
Management is a core backend and cannot be removed.

## Proton Mail Bridge (optional service)

Proton Mail Bridge is a local background IMAP/SMTP server, not a search
trigger. Click **Install** in the Store's dedicated **Proton** section or in
**Settings → Search → Services and integrations** to enable it. Then click
**Settings** to open its native GTK4/libadwaita window.
No trigger word or shortcut is registered. Existing Proton trigger installs
are migrated automatically without deleting Bridge accounts.

If signed out, enter your Proton username and account password, then complete
any two-factor, mailbox-password or security-key prompts. If already connected,
Settings shows your **generated Bridge password**, mail username and local
IMAP/SMTP host, ports and encryption settings for a new email client.

Use **Sign out** beside an account to disconnect it, then **Sign in** to
connect it again. Closing Settings leaves Bridge running. **Start Bridge at
desktop login** controls automatic reconnection. **Uninstall** removes the optional
service from Spotty and hides its Settings access; saved Bridge accounts and
running mail connections are preserved. Click **Install** to restore access.

The native Cargo package includes Bridge 3.27.0 for Linux x86_64, with no
separate helper installation, system-wide package writes or Flatpak build.
The runtime is extracted into the private Spotty data folder only when needed.
A paid Proton Mail plan and an unlocked Linux keyring are required. Human
verification can use the included official Bridge window.

For packaging, credentials, licensing and verification, see
[the login package](proton-bridge-gui/README.md).

## Proton VPN (built into Spotty)

Install **Proton VPN** from the Store's **Proton** section. Spotty includes
Proton's official Linux VPN client library
([`proton-vpn-embedded/`](proton-vpn-embedded/)), so no separate Proton app or
CLI is needed. Proton VPN has its own sign-in: type your Proton VPN username and
password in the Proton VPN window (two-factor codes included). The password goes
straight to Proton's client, and Spotty never stores it. See
[separate sign-ins](#a-separate-sign-in-for-each-proton-app).

The `vpn` trigger connects and disconnects from search: `vpn on` / `vpn off`,
country suggestions as you type (`vpn ge` → 🇩🇪 Germany, 🇬🇪 Georgia…), cities
from Proton's server list (`vpn zur` → Zurich), servers (`vpn CH#242`) and
`vpn status`. The window also sets a preferred country and Proton's settings
(protocol, NetShield, kill switch, VPN Accelerator, Moderate NAT, port
forwarding, IPv6).

Build Spotty with `--features proton-vpn` to include the client; see
[`proton-vpn-embedded/README.md`](proton-vpn-embedded/README.md).

## A separate sign-in for each Proton app

Every Proton app in Spotty signs in on its own, with its own session. Nothing is
shared: signing out of one app never signs out another, and no app's session is
given to another.

- **Every Proton app** shows the same sign-in page: your Proton email and password,
  then the authenticator code if your account uses one, then the mailbox password
  (or Pass's extra password) if you have one. Enter moves each step on; Back
  cancels the sign-in and clears what was typed. Once signed in, Calendar, Drive,
  Pass and VPN show an **Account** row at the top of their settings with the email
  and **Sign out** (it asks first). Mail Bridge lists its saved accounts instead,
  each with its own **Sign out**.
- **Proton Calendar** and **Proton Drive** each sign in on their own, from their
  settings, the `cal` or `drive` triggers, or the **Sign in** row. Each keeps its
  session in its own folder
  (`~/.local/share/spotty/proton-calendar/` and `~/.local/share/spotty/proton-drive/`).
- **Proton Pass** signs in with its own password, through Proton's official Pass
  client (`pass-cli login --interactive`). Type your Proton email and password in
  the Proton Pass settings. Spotty runs the client on a terminal and answers its
  prompts (password, then a two-factor code or mailbox password when needed).
  The password is never stored and never put on a command line or in the
  environment. Pass keeps its session in `~/.local/share/spotty/proton-pass/`.
- **Proton VPN** signs in with its own username and password, on the same page.
- **Proton Mail Bridge** signs in on the same page, through Bridge's own login.

Each sign-in has been tested only against Spotty's fake Proton server (for
Calendar and Drive) and not against Proton's live service. Proton may refuse or
ask for steps Spotty doesn't support; Spotty then shows the error.

There is no web page, browser engine or helper program for Calendar and Drive:
Spotty speaks to Proton's API itself (the [`proton-account/`](proton-account/)
crate, using Proton's open-source SRP and OpenPGP libraries). See
[how your Proton sign-in is handled](#how-your-proton-sign-in-is-handled).

## Proton Pass (built into Spotty)

Install **Proton Pass** from the Store's **Proton** section. Spotty includes
Proton's official Pass client ([`proton-pass-embedded/`](proton-pass-embedded/)),
so no separate Proton app or CLI is needed.

`pass <title>` searches your vault by item title, like Raycast's password
manager extensions:

- **Enter** on a login copies the **password**; the best match also lists
  *Copy username*, *Copy one-time code* and *Open website*.
- Cards copy the card number (and security code), notes copy their text, and
  other item types open in Proton Pass.
- Secrets are marked as passwords on the clipboard, so Spotty's clipboard
  history skips them, and are cleared after the delay you choose (30 seconds by
  default).
- Vault items never show up in the everyday search.

Spotty loads only item titles (Proton's secret-free listing). Everything else
is read when you open an item or copy from it, and wiped from Spotty's memory
right after.

Open **Open Proton Pass vault** from the `pass` trigger, or **Open vault…** in
Proton Pass's settings, to get a window like the browser extension's: pick a
vault, search, open an item and see its fields. Passwords, card numbers, security
codes, PINs and private keys stay masked until you reveal them (they re-mask after
30 seconds). One-time codes update live with a countdown. Copy buttons put the
value on the clipboard, marked as a password. The window is view and copy only:
it cannot create, edit, trash or generate items.

The Proton Pass settings also hold: how long copied secrets stay on the clipboard
(15 s, 30 s, 1 min, 2 min or 5 min; 30 s by default), whether usernames are shown
under items, the default vault, whether the `pass` trigger searches item titles,
and the PIN lock with its auto-lock time (1 min to 4 hours, or Lock now).

Pass's key lives in your desktop keyring, which must be unlocked. Pass needs a
paid Proton Pass plan.
Build Spotty with `--features proton-pass` to include the client; see
[`proton-pass-embedded/README.md`](proton-pass-embedded/README.md).

## Proton Calendar and Proton Drive (optional integrations)

Install **Proton Calendar** or **Proton Drive** from the Store's **Proton**
section. Each adds a search trigger and a **Settings** button in the Store and
under **Settings → Search → Services and integrations**, where you sign in.

- `cal` lists what is coming up: your next events with time, place and
  calendar. Say when you mean to look: `cal tomorrow`, `cal friday`,
  `cal next week`, `cal +3d`, `cal in 2 weeks`, `cal 24 oct` or
  `cal 2026-10-24`. Words search the next two months of your events
  (`cal dentist`). Enter opens Spotty's agenda window on that day, with a day,
  week or month view (the default is in Settings) and details for each event.
  Repeating events are expanded; calendars you hide in Proton stay hidden.
- `drive <name>` finds files and folders in **My files** by their real,
  decrypted names. Enter on a folder opens Spotty's Drive browser there; Enter
  on a file downloads it to your Downloads folder and opens it. `drive` alone
  opens the browser at the top. Places that exist only in Proton's web app
  (`drive shared`, `with-me`, `photos`, `devices`, `trash`) open in your browser.
  Settings can also point at a local folder you already sync with Proton Drive
  (for example with rclone) so `drive <name>` searches it too.

Signed out, each trigger shows a **Sign in** row that opens that app's own sign-in
window (Calendar and Drive sign in separately).

### How your Proton sign-in is handled

- **Your password** is typed into the sign-in page and its field is emptied at
  once. For Calendar and Drive it is turned into a one-time SRP proof, and Spotty
  checks Proton's proof back, so it never signs in to an impostor. For Pass, VPN
  and Bridge the password goes straight to that app's own client. Spotty keeps no
  password, two-factor code, mailbox password or Pass extra password after the
  sign-in. A step that is still waiting (a code, a mailbox password) holds only
  what it needs, and is wiped when that step ends or the sign-in is cancelled
  (Back, or closing the window).
- **Saved on disk**, one folder per app, readable only by you (mode 0700 for the
  folder, 0600 for the file). Each app's session is its own:
  `~/.local/share/spotty/proton-calendar/session.json` and
  `~/.local/share/spotty/proton-drive/session.json` hold that app's session tokens
  and its account's name and email, and nothing else secret. The files are not
  encrypted; anyone who can read your files as you can use the tokens, like a
  browser's saved session. Spotty ignores a file if group or others can read it.
  An older `proton-account/session.json` from earlier versions is no longer read;
  you can delete it.
- **In the desktop keyring**: the *key password* derived from your password, which
  unlocks your Proton keys, for Calendar and Drive. Spotty stores it through Secret
  Service (GNOME's keyring, for example) and never writes it to a file. Without a
  running keyring Spotty cannot keep a sign-in, so it refuses to sign in and says why.
  Signing out deletes it. An older session file that still held the key password is
  migrated to the keyring at the next start.
- **Pass** keeps its own session in `~/.local/share/spotty/proton-pass/`, with its
  key in your desktop keyring. Its password goes to `pass-cli` only through the
  terminal Spotty opens for it; it is never written to disk, argv or the
  environment.
- **VPN** keeps its session in your desktop keyring, as Proton's own app does.
- **Sign out** in a Calendar or Drive sign-in window ends that app's session at
  Proton and deletes its `session.json`. Signing out in Pass's settings removes
  only Pass's session on this computer. Proton VPN's sign-out ends only VPN's session.
  No sign-out touches another app.
- **Only in memory**: unlocked keys, decrypted file names, calendar events and
  Pass item titles. They are never written to disk and disappear when you sign
  out or quit.
- **Not supported**: accounts protected only by a hardware security key (add an
  authenticator app); Proton's "are you human?" check, which exists only on
  Proton's web page (wait a while and try again); shared-with-me, photos and
  trash in the Drive browser; creating or editing events and files.
- Spotty identifies itself to Proton honestly as a third-party Drive client
  (`external-drive-spotty`); it does not pretend to be one of Proton's apps. The
  Pass and VPN sessions are used by Proton's own Pass and VPN clients. The VPN
  client library identifies itself as Proton's Linux VPN GUI client, because it
  is Proton's own code.
- Signatures on file names and events aren't checked yet (the data is
  authenticated by encryption and each downloaded block by its hash).

## Repository layout

```
index.json           ← searchable Store listing: id, name, word, description,
                        icon, version, author, shortcut and install defaults.
                        Every entry appears in the same Store list.
triggers/<id>.json   ← one manifest per trigger — the full install payload
                        (adds help/help_image and the action)
```

Spotty's trigger repository URL defaults to
`https://raw.githubusercontent.com/Aras1907/spotty-triggers/main`, so any
static host serving this layout works — raw GitHub or your own mirror
(set `trigger_repo_url` in `~/.config/spotty/config.json`; there is no UI
field for it).

## Native trigger source

This repository is the source of truth for all native search triggers and
result providers. Spotty includes it as the pinned `trigger-backends` Git
submodule. `src/search/` supplies search dispatch, result types, and the
providers; `src/defaults.rs` loads native defaults from `index.json`.
`src/features/` owns native feature services and `src/ui/feature_settings.rs`
owns their settings dialogs. These modules compile inside the Spotty crate
and use its shared UI, indexer, configuration, and platform integration
through `crate::` paths. The settings module is a child of Spotty's settings
window module so it can use the shared widget and persistence helpers.

Build through a Spotty checkout with host Cargo:

```sh
git submodule update --init --recursive
cargo build
```

To change a native backend, edit its source in the submodule and commit
and push here first. Then commit and push the updated submodule revision
in Spotty. An ordinary Store manifest cannot introduce a new Rust backend:
new implementations require a Cargo rebuild of Spotty.

## Authoring a trigger

A manifest is a JSON file with the following action types:

### `native` — install a result provider

Native entries have `"native": true` in the index and manifest, and
`"action": {"type": "native"}` in the manifest. `"preinstalled": true` means
a trigger word is enabled by default. Optional result providers use
`"preinstalled": false` and start uninstalled. This field controls fresh
installation defaults only; the Store shows every entry in the same searchable
list without a Built-in section. The `id` identifies a backend already shipped
with Spotty. Store installs enable that provider;
file imports use the same configuration path. They do not run commands or
add duplicate custom trigger files. Unknown backend ids are unavailable.
Words and shortcuts in these manifests document the defaults; reinstalling
preserves the user's existing values. Empty words are valid for native
result types.

### `web` — open a URL with the query

```json
{
  "id": "dictionary",
  "name": "Dictionary",
  "word": "dict",
  "description": "Look up a word definition",
  "icon": "accessories-dictionary-symbolic",
  "shortcut": "",
  "version": "1.0.0",
  "author": "you",
  "action": {
    "type": "web",
    "url": "https://www.dictionary.com/browse/{query}"
  }
}
```

Type `dict word` in Spotty → the URL opens with `word` substituted
(URL-encoded). `icon` is a symbolic icon name from the GNOME/Adwaita icon
theme (test with `gtk4-icon-browser`, or pick from
`/usr/share/icons/Adwaita/symbolic/`); leave it empty for a default.

### `files` — search files by extension

```json
{
  "id": "books",
  "name": "Books",
  "word": "book",
  "description": "Find your e-books",
  "icon": "x-office-document-symbolic",
  "shortcut": "",
  "version": "1.0.0",
  "author": "you",
  "action": {
    "type": "files",
    "extensions": ["epub", "pdf", "mobi"]
  }
}
```

An empty `extensions` list means "all files" (like the built-in `find`).

### `shell` — run a command with the query

```json
{
  "id": "download",
  "name": "Download with yt-dlp",
  "word": "dl",
  "description": "Download a video",
  "icon": "folder-download-symbolic",
  "shortcut": "",
  "version": "1.0.0",
  "author": "you",
  "action": {
    "type": "shell",
    "command": "yt-dlp -o '~/Videos/%(title)s.%(ext)s' {query}"
  }
}
```

**Security:** `{query}` is single-quote-escaped by Spotty — user input can
never inject extra shell commands, only the command template you write runs.
Do NOT wrap `{query}` in your own quotes. Because shell triggers run commands
as the user, Spotty shows the exact command and asks for confirmation at
install time. Users should only install shell triggers they trust.

### `help` and `help_image` (optional)

`help` is usage text shown in the preview panel when the trigger is selected
in the `triggers` search; when omitted, Spotty generates instructions from the
action type. `help_image` is an optional URL of a screenshot rendered below
the text (downloaded once and cached):

```json
{
  "id": "dictionary",
  "name": "Dictionary",
  "word": "dict",
  "description": "Look up a word definition",
  "icon": "accessories-dictionary-symbolic",
  "shortcut": "",
  "version": "1.0.0",
  "author": "you",
  "help": "Type \"dict <word>\" to look up a definition.",
  "help_image": "https://example.com/screenshots/dict.png",
  "action": {
    "type": "web",
    "url": "https://www.dictionary.com/browse/{query}"
  }
}
```

### `shortcut`

Optional GNOME-style global shortcut (e.g. `"Super+Ctrl+D"`) that opens
Spotty directly in the trigger's mode. Registered on install, removed on
uninstall.

### Rules

- `id` must be unique, lowercase, and use only letters, digits, `_`, `-`, `.`
- `word` is the trigger text — it must not collide with a built-in trigger
  (`find`, `clip`, `cmd`, `emoji`, `bt`) or another trigger
- keep `name` short — it's what shows on the trigger row

## Submitting a trigger

1. Write the manifest under `triggers/`
2. Add it to `index.json` and the table above
3. Open a pull request

You (and everyone else) can already install it from your branch with
[Install a trigger from a file](#install-a-trigger-from-a-file) using the
raw file.
