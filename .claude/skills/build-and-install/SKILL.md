---
name: build-and-install
description: Use after changing Spotty code or trigger manifests in this repo, or when asked to build, install, restart or check the running Spotty. Covers the sync into the Spotty checkout, pushing to GitHub, the CI Flatpak release, and installing that Flatpak from GitHub.
---

# Ship and install Spotty

Never install a local Cargo build and never build a Flatpak locally. GitHub CI
(`.github/workflows/release.yml` in `Aras1907/Spotty`) builds the Flatpak; the
machine runs the Flatpak from the GitHub release (`com.spotty.Spotty`, `--user`).

## 1. Sync this repo into the Spotty checkout

```sh
cd /home/aras/development/Spotty/spotty-triggers && python3 tools/sync_to_spotty.py
```

Never copy into `trigger-backends/` by hand.

## 2. Check the code (optional, never installed)

```sh
flatpak-spawn --host sh -c 'cd /home/aras/development/Spotty/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo test --bin spotty'
```

Compare failures with the known baseline. This is a check only; the binary is
not installed.

## 3. Bump the version and release notes

In `.integration/Spotty`: bump `version` in `Cargo.toml` (and `Cargo.lock`,
plus any metainfo release entry the previous release commit touched), and
update "What's new" in `packaging/release-notes.md`.

## 4. Commit and push both repos, then tag

```sh
cd /home/aras/development/Spotty/spotty-triggers && git add -A <changed files> && git commit && git push
cd .integration/Spotty && git add <changed files> && git commit -m "Spotty X.Y.Z: ..." && git push
git tag vX.Y.Z && git push origin vX.Y.Z
```

Stage only files you changed. The tag starts the release workflow.

## 5. Wait for CI

```sh
gh run list -R Aras1907/Spotty --limit 3
gh run watch -R Aras1907/Spotty <run-id> --exit-status
```

Run the watch in the background. If CI fails, stop and report; nothing is
published and the installed Flatpak is untouched.

## 6. Install the release Flatpak

```sh
d=$(mktemp -d) && gh release download vX.Y.Z -R Aras1907/Spotty -p '*x86_64*.flatpak' -D "$d"
flatpak-spawn --host flatpak install --user -y --noninteractive "$d"/*.flatpak
```

## 7. Restart and verify

```sh
flatpak-spawn --host flatpak kill com.spotty.Spotty
flatpak-spawn --host flatpak run com.spotty.Spotty &
flatpak-spawn --host flatpak info --user com.spotty.Spotty | grep -i version
```

Report the installed version. Do not say "installed" without it.
