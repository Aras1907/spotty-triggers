---
name: add-native-trigger
description: Use when adding a trigger or result type to Spotty, or changing an existing native trigger's manifest, catalog entry, defaults or search routing. Lists every file to touch in order, with tests, version bumps and docs.
---

# Add a trigger

Decide the kind first:

- **Manifest-only** (`web`, `files`, `shell`): change `triggers/<id>.json` and
  `index.json` only. No Rust, no Spotty rebuild.
- **Native** (`"action": {"type": "native"}`): Rust in this repo, then a Spotty
  rebuild. Follow every step below.
- **Proton** (`proton-*` id): also follow the `proton-integration` skill.

## 1. Manifest and catalog

Create `triggers/<id>.json`, copying the shape of `triggers/calc.json` (a result
type) or `triggers/proton-pass.json` (a service, with `"category": "service"`).
Fields: `id` (lowercase letters, digits, `_`, `-`, `.`), `name`, `word` (must not
collide with a built-in word or another trigger), `description`, `icon`
(symbolic), `shortcut`, `version`, `author`, `native`, `preinstalled`, `help`,
`action`.

Add the same entry to `index.json`, with the same `version`, `native` and
`preinstalled`, and without `help` or `action`.

## 2. Defaults (`src/defaults.rs`)

- Proton integration: add a row to `PROTON_TRIGGERS`, a `display_name` arm, and
  make `supports_native` return true for it.
- Other native trigger: `supports_native` and `command_keyword` already read the
  catalog. Add a `display_name` arm, or the name shows as "Trigger".
- Result type: also add the id to `RESULT_IDS`, `DEFAULT_ORDER`, `result_title`
  and `result_blurb`.

## 3. Search routing (`src/search/mod.rs`)

- Add `pub mod <name>;` to the module list (lines 11-34).
- Add a route in `search_mode()`, beside the others:
  `if kw.id == "<id>" { return <module>::search(rest); }`. Result types go in
  `result_mode()` instead.
- Add an `Action` variant only if no existing one fits (the enum starts at
  line 71). Never put a secret in an `Action`.

## 4. Spotty checkout (local only)

- Each new `Action` variant needs an arm in the `match` in
  `.integration/Spotty/src/ui/search_window.rs` (around line 6360). The compiler
  lists the missing arms.
- A new feature module needs a `#[path]` include in `.integration/Spotty/src/main.rs`.
- Do not commit the Spotty checkout.

## 5. Tests, versions, docs

- Unit tests go in the provider module (`#[cfg(test)]`). For a Proton trigger,
  add a case to `service_tests` in `src/defaults.rs`: `supports_native`, the word,
  and not preinstalled.
- Bump `version` in the manifest and in `index.json` together for any behaviour
  change.
- Add a row to the trigger table in `README.md` for a user-facing trigger. Update
  `PRIVACY_AND_SECURITY.md` if stored data or network traffic changes.

## 6. Verify and report

- Run the manifest and catalog check, the tests and the build in
  `verify-before-push`. Install with `build-and-install` and try the trigger.
- The Store reads `index.json` from GitHub main, so a new or changed entry appears
  there only after the user pushes. Say so in the report.
