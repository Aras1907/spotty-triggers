# CLAUDE.md

Read `AGENTS.md`, `SOUL.md` and `ARCHITECTURE.md` first. `AGENTS.md` has the
workspace rules and the "Never" list. `SOUL.md` has the project's character.

Project skills live in `.claude/skills/<name>/SKILL.md`. Claude Code finds them
there. Invoke one by name with the Skill tool or `/<name>`:

- `build-and-install`: sync, push to GitHub, let CI build the Flatpak, install
  the release Flatpak from GitHub and restart Spotty.
- `add-native-trigger`: add a trigger, from manifest to tests and version bump.
- `verify-before-push`: the full check list and the push rule.

## Install rule

Never install a local `cargo build`. Every change ships the same way: sync,
commit and push to GitHub (this repo and `Aras1907/Spotty`), tag a `v*` release
so `release.yml` builds the Flatpak, then install that Flatpak from the GitHub
release with `flatpak install --user`. Local `cargo test`/`cargo check` are for
checking code only, never for what runs on the machine.

Spotty's own rules are in `.integration/Spotty/CLAUDE.md`. Read them, do not edit
them. For this workspace, `AGENTS.md` takes precedence.
