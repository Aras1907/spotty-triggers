---
name: proton-integration
description: Use before any change that touches Proton, including sign-in, saved sessions, Calendar, Drive, Pass, VPN, Bridge, Proton data and Proton logging. Lists the security rules, where state lives, how to test with the fake Proton server, and how to report what is untested.
---

# Proton integration

## Rules (no exceptions)

1. Spotty never stores a Proton password, two-factor code, mailbox password or Pass extra password, not even after a sign-in. A password goes into the SRP proof (Calendar and Drive), to Proton Pass's client through its terminal (Pass), to Proton VPN's client (one request line, wiped after it is written) or to Bridge's login stream. The login password is kept only while a two-factor code or mailbox password is still pending, and it is then wiped.
2. Each Proton app has its own sign-in and its own session, and nothing is shared between apps. Calendar's session is `~/.local/share/spotty/proton-calendar/session.json` and Drive's is `~/.local/share/spotty/proton-drive/session.json`: folder 0700, file 0600, and the reader ignores (and removes) a file that others can read. Keep that check. The file holds the tokens, the account's name and email, and no key password. Calendar's and Drive's *key password* (it unlocks the account's keys) is in the desktop keyring, service `com.spotty.Spotty.proton`, names `proton-calendar` and `proton-drive` (`proton-account/src/secrets.rs`). Sign-out deletes the file and the keyring entry. An old session file that still holds `key_password` is migrated at the next start. Pass's session lives in pass-cli's folder (key in the keyring), VPN's in the desktop keyring. The Pass PIN file (`~/.config/spotty/pass-pin.json`) holds a salted hash only, never the PIN. Pass's preferences (`~/.config/spotty/pass-settings.json`) hold no secret. The old `proton-account/session.json` is no longer read.
3. Unlocked keys, decrypted names, calendar events and vault contents stay in memory. Never write them to disk or log them.
4. Never log passwords, 2FA or mailbox codes, key passwords, tokens, decrypted names, event titles, vault fields or file contents. Log errors without secret fields.
5. Never give one app's session or password to another app: no session fork, no copying `session.json`, no passing tokens, no reusing cookies, no prefilling one app's password. There is no sharing path. Prefilling the email of an account picked from Bridge's own list is the only exception, and it is not a password.
6. Pass signs in with `pass-cli login --interactive <account>`. Spotty runs it on a pseudo-terminal (`src/features/proton_pass_login.rs`), answers the password, 2FA and mailbox prompts by writing to that terminal, and never puts a secret in argv or the environment. No web view, no browser, no third-party CLI for any Proton sign-in.
7. Proton's official clients (pass-cli, the VPN library) are used as they are, built from pinned sources with SHA-256 checks. Do not patch them.
8. Identify honestly as `external-drive-spotty@<version>-stable`. Never pose as a Proton app. The VPN library identifies itself as Proton's Linux GUI client; see the "Gaps and stale spots" section of `ARCHITECTURE.md`.
9. Run network work off the GTK thread (`gio::spawn_blocking`, as in `run()` in `src/ui/proton_native_ui.rs`). Pass's terminal reads run on their own thread.
10. Secrets copied to the clipboard carry the password-manager hint and are cleared after the delay chosen in Pass's settings (15 s to 5 min, 30 s by default).
11. The Pass vault window is view and copy only. It needs the PIN (`proton_pass_pin::unlocked()`) to show anything, re-checks it on every copy and reveal, blanks itself when the lock runs out, and wipes an item's fields when it is switched, locked or closed.
12. Every Proton sign-in uses the one shared page (`proton-login/`, `spotty_proton_login::LoginPage`). Do not write a second sign-in form: add a `Backend` for the app (`begin`, `submit_code`, `submit_second`, `cancel`, all blocking) and map its answers onto `Step`. While signed out an app's settings show only that page; once signed in they start with the Account group (email and Sign out).

## Where state lives

See `ARCHITECTURE.md`, "Data locations". Main places: `~/.local/share/spotty/proton-calendar/session.json`
and `~/.local/share/spotty/proton-drive/session.json` (each app's own session),
`~/.local/share/spotty/proton-pass/` (pass-cli session; key in the keyring), the
desktop keyring (VPN session, Pass key, and the Calendar and Drive key passwords), `~/.config/spotty/pass-pin.json` (Pass PIN
hash), `~/.config/spotty/pass-settings.json` (Pass preferences), and
`~/.cache/spotty/proton-*/` (unpacked binaries).

## Checklist

1. Read `ARCHITECTURE.md`, "The Proton stack" and "Gaps and stale
   spots". Run `git status`: the Proton files may have uncommitted changes, so
   check the current code before you rely on a path.
2. Identify the layer: crate (`proton-account/src`), Spotty feature
   (`src/features/proton_*.rs`), UI (`src/ui/proton_native_ui.rs`, `src/ui/proton_pass_ui.rs`), search
   (`src/search/proton.rs`, `src/search/pass.rs`), or Spotty-side glue.
3. Check logging:
   `grep -rn "log::\|eprintln!\|println!" src/features/proton_*.rs src/ui/proton_native_ui.rs proton-account/src`
   and confirm no secret reaches a log line.
4. Add the behaviour to the fake server first. `proton-account/tests/fake_proton.rs`
   runs a local server with real SRP, real keys and real ciphertext. Add a test
   there. Existing tests: `sign_in_then_browse_drive_and_read_the_calendar`,
   `two_factor_step_is_required_and_wrong_codes_can_be_retried`,
   `vpn_and_pass_get_their_own_child_sessions_and_other_apps_do_not`.
5. Run `cd /home/aras/development/spotty-triggers/proton-account && cargo test`
   (on the host if the sandbox cannot build it, as in `verify-before-push`).
6. Run the Spotty Proton tests, as in `verify-before-push`. The session-file
   permission test is `saved_session_is_private_and_round_trips` in
   `src/features/proton_native.rs`.
7. Sync and build with `build-and-install`.

## Reporting

- The fake server proves the protocol and crypto wiring, not that Proton's live
  service accepts the requests. Say so every time.
- The live sign-in path is untested for every app. Do not
  sign in to a real account unless the user asks. List the live steps you could
  not run.
- Never add a test or check that contacts Proton's live API; use the pretend
  server in `tests/fake_proton.rs`.
- Pass's interactive sign-in and the prompt handling are not run against Proton's live service; say so.
- Not supported; say so when relevant: FIDO2-only accounts, Proton's
  human-verification check, OpenPGP signature checks on names and events (not
  done yet), shared-with-me, photos and trash in Drive, and all write operations.
