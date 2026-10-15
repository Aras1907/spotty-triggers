---
name: verify-before-push
description: Use before reporting a change as done, before any commit, and before pushing the trigger repo. Gives the full verification list (diff review, manifest and version check, crate tests, Spotty tests, release build, docs) and the push rule.
---

# Verify before you report or push

Run the checks that match the change.

## 1. Review the diff

```sh
cd /home/aras/development/spotty-triggers && git status --short && git diff --stat && git diff --check
```

`git diff --check` must print nothing. Read the diff: only what the task asked for,
no reverted unrelated work, no token or real account data. The tree often mixes
several people's changes, so confirm every modified or new file is intended.

## 2. Manifest and catalog check

Run from the repo root. Each `triggers/*.json` must match its `index.json` entry,
and each catalog id needs a manifest. It ends with `problems: 0` when clean.
The script cannot tell whether behaviour changed; if it did, bump `version` in both files.

```sh
cd /home/aras/development/spotty-triggers && python3 - <<'EOF'
import json, glob, os
ix = {e["id"]: e for e in json.load(open("index.json"))}
bad = 0
for p in sorted(glob.glob("triggers/*.json")):
    m = json.load(open(p)); e = ix.get(m["id"])
    if e is None: print("NOT IN index.json:", p); bad += 1; continue
    for k in e:
        if k in m and k not in ("help", "help_image", "action", "category") and e[k] != m[k]:
            print("MISMATCH", p, k, e[k], m[k]); bad += 1
    print("ok", p, m["version"])
for i in ix:
    if not os.path.exists(f"triggers/{i}.json"): print("NO MANIFEST:", i); bad += 1
print("problems:", bad)
EOF
```

## 3. Sync and Spotty tests

```sh
cd /home/aras/development/spotty-triggers && python3 tools/sync_to_spotty.py
flatpak-spawn --host sh -c 'cd /home/aras/development/spotty-triggers/.integration/Spotty && OPENSSL_NO_VENDOR=1 cargo test --bin spotty'
```

Never run plain `cargo test` in the Spotty checkout; a broken example makes it fail.

## 4. Release build and docs

- Run the build step in `build-and-install`. Only a build ending with `Finished` passes.
- `README.md`: the trigger table matches the manifests.
- `PRIVACY_AND_SECURITY.md`: updated if stored data, network traffic or a secret path changed.
- `ARCHITECTURE.md` and `AGENTS.md`: updated if a path, file or rule changed.

## 5. Report what you did not run

Name each check you could not run and why, and what is untested:
on-screen GTK behaviour, and the Store until the user pushes.

## Push rule

- Commit and push only when the user asks.
- Push only this trigger repo, `/home/aras/development/spotty-triggers`. Do not
  push the Spotty checkout unless told to.
- End commit messages and PR descriptions with the attribution lines in the
  session's system reminder, when there is one.
- Pushing the catalog makes new `index.json` entries appear in the Store. Tell the user.
