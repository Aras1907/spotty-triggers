# CLAUDE.md

Read `AGENTS.md`, `SOUL.md` and `ARCHITECTURE.md` first. `AGENTS.md` has the
workspace rules and the "Never" list. `SOUL.md` has the project's character.

Project skills live in `.claude/skills/<name>/SKILL.md`. Claude Code finds them
there. Invoke one by name with the Skill tool or `/<name>`:

- `build-and-install`: sync, build on the host, install and restart Spotty.
- `add-native-trigger`: add a trigger, from manifest to tests and version bump.
- `proton-integration`: rules and checks for anything that touches Proton.
- `verify-before-push`: the full check list and the push rule.

Spotty's own rules are in `.integration/Spotty/CLAUDE.md`. Read them, do not edit
them. For this workspace, `AGENTS.md` takes precedence.
