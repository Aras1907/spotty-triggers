# ARCHITECTURE.md

How this repository plugs into Spotty, what lives where, and how data moves.
Paths are relative to this repository unless they start with `~` or
`.integration/`. Items marked **(Spotty checkout)** are not in this repo.

## Two checkouts

| Where | What | Git |
|---|---|---|
| `/home/aras/development/spotty-triggers` (this repo) | Trigger manifests, Store catalog, native backends | Remote: GitHub `Aras1907/spotty-triggers`, branch `main` |
| `.integration/Spotty` (**Spotty checkout**) | Spotty's application code, `Cargo.toml`, GTK UI, daemon | Separate repo; ignored by this repo (`/.integration/` in `.gitignore`) |
| `.integration/Spotty/trigger-backends/` (**Spotty checkout**) | A copy of this repo's files | Its own `.git` directory, not a link |

The copy is refreshed with `python3 tools/sync_to_spotty.py` from this repo. The
script copies every tracked or non-ignored file, skipping `.integration/`,
`build/` and `.cargo-proton/`. It also removes stale files under `triggers/`
and `src/` in the copy. Never edit the copy by hand.

Spotty loads this repo's Rust files with `#[path = "../trigger-backends/src/..."]`
attributes. The `README.md` calls `trigger-backends` a pinned submodule; the working setup is
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
                        version, author, shortcut, native, preinstalled, and the
                        Store's detail fields: long_description, examples,
                        network (empty = runs here) and homepage. Missing or
                        malformed detail fields hide their section only.
                        Spotty fetches it from GitHub main, so changes go live
                        only after a push.
triggers/<id>.json      One manifest per trigger: the install payload, with
                        help text, help_image and the action.
src/defaults.rs         Loads index.json (include_str!) for native defaults.
                        supports_native, command_keyword(s), DEFAULT_ORDER,
                        RESULT_IDS, display and result titles.
src/search/             Search dispatch and result providers.
  mod.rs                SearchResult, the Action enum, search_mode() routing
                        by keyword id, trigger_suggestions(), result_mode()
                        for result types, and web/shell manifest handling.
  calculator.rs, convert.rs, web.rs, apps.rs, ...   Other providers.
src/features/           Feature services.
  clipboard.rs, fileops.rs, ocr.rs, packagekit.rs, ...   Other services.
src/ui/                 Shared UI pieces that Spotty also compiles.
  feature_settings.rs   Feature settings dialogs (included in settings_window).
src/security.rs         Private storage helpers and bounded HTTP downloads.
tools/sync_to_spotty.py Copies this repo into the Spotty checkout.
build/, .cargo-proton/  Local caches. Gitignored. Not part of the source.
.claude/skills/         Project skills for Claude Code.
```

Spotty-side files (**Spotty checkout**, local only):

- `Cargo.toml`: Spotty's own dependencies. It has no path crates from this repo.
- `src/main.rs`: `#[path]` includes of the repo's modules (about lines 26-77),
  daemon start, the PID file and `--toggle`/`--quit` handling.
- `src/ui/settings_window.rs`: `#[path]` include of `feature_settings.rs`.
- `src/ui/search_window.rs`: the `match` on `Action` that runs each action.
- `src/triggers.rs`: `RepoTrigger` (a catalog entry, with the Store's detail fields) and `TriggerAction`.
- `src/app.rs`: daemon, SIGUSR1 handler, keybinding registration.

## Trigger flow

Manifest to result:

1. **Store or import.** The Store reads `index.json` from GitHub. A manifest
   file can also be imported locally. Installing a trigger stores its word,
   shortcut and enabled flag in Spotty's config. Native entries (`"action": {"type": "native"}`)
   only enable a backend that is already compiled in. They cannot add code.
2. **Keyword lookup.** The user types `dict word`. Spotty finds the keyword
   for the word `dict` (`config.keyword_for_word`).
3. **Routing.** `src/search/mod.rs` `search_mode()` matches on `kw.id`:
   `bluetooth` goes to `bluetooth::search`, `emoji` to `emoji::search`, `run` to
   `run::search`. Result types (`apps`, `calc`, `convert`, `web`, ...)
   go through `result_mode()`. Web and shell manifests go through the
   `TriggerAction` branch in the same file.
4. **Result rows.** Each provider returns `SearchResult { kind, title, subtitle,
   icon, action, score }`. The `Action` enum (top of `src/search/mod.rs`) is the
   contract with Spotty. Examples: `OpenUrl`, `RunCommand`, `EnterMode`,
   `Bluetooth { op, mac }`.
5. **Dispatch.** Spotty's `search_window.rs` matches the `Action` and calls the
   feature. Slow actions run on background threads. An `Action` never carries a
   secret.

A new Action variant must be handled in Spotty's `search_window.rs` match, so
the compiler points at every place that needs it.

Native backend versus manifest-only: web, files and shell manifests need no
Rust and no rebuild of Spotty. Anything with `"action": {"type": "native"}`
needs a Rust provider in this repo and a rebuild of Spotty.

## Data locations

| Path | Holds | Written by |
|---|---|---|
| `~/.config/spotty/config.json` | Spotty settings, including `trigger_repo_url` (no UI field) | Spotty |
| `~/.config/spotty/spotty.pid` | PID of the running daemon | Spotty |
| `~/.config/spotty/spotty_keyword.txt` | Keyword passed to a summoned daemon | Spotty |

