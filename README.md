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
| [`triggers/cmd.json`](triggers/cmd.json) | `app` | Install, uninstall, and manage apps |
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
| [`triggers/proton-vpn.json`](triggers/proton-vpn.json) | Install from Store | Optional Proton VPN controls |

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
trigger. Click **Install** in **Settings → Search → Server-side installations**, or
click **Install** in the Store's separate **Server-side installations**
section. Then click **Settings** to open its native GTK4/libadwaita window.
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

## Proton VPN controls (optional integration)

Install **Proton VPN** from Spotty's Trigger Store to add a native
GTK4/libadwaita connection popup under **Settings → Search → Services and integrations**.
It uses the official Proton VPN Linux CLI installed on the host for status,
connect, disconnect and sign-out. Sign-in stays with Proton's CLI so its
password, two-factor and security-key prompts remain under Proton's control.
Spotty does not collect VPN credentials, implement a tunnel, or install system
packages.

Install Proton's official Linux CLI separately using its
[Linux installation guide](https://protonvpn.com/support/download-and-installation/linux).
Proton officially supports the CLI on Fedora GNOME and says its CLI and GUI
apps cannot run at the same time. In a Flatpak build, Spotty calls the host
CLI through its existing host-command bridge. The CLI and host VPN services
must be installed and authorized by the user. Uninstalling the Spotty
integration leaves the Proton VPN package, account and active connection
alone.

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
  (`find`, `clip`, `app`, `cmd`, `emoji`, `bt`) or another trigger
- keep `name` short — it's what shows on the trigger row

## Submitting a trigger

1. Write the manifest under `triggers/`
2. Add it to `index.json` and the table above
3. Open a pull request

You (and everyone else) can already install it from your branch with
[Install a trigger from a file](#install-a-trigger-from-a-file) using the
raw file.
