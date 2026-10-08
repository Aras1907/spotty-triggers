---
name: proton-integration
description: Use before any change that touches Proton, including sign-in, saved sessions, the session fork, Calendar, Drive, Pass, VPN, Bridge, Proton data and Proton logging. Lists the security rules, where state lives, how to test with the fake Proton server, and how to report what is untested.
---

# Proton integration

## Rules (no exceptions)

1. Spotty never stores a Proton password. A password goes into the SRP proof (or
   to Proton's client). The login password is kept only while a two-factor code
   or mailbox password is still pending, and it is then wiped.
2. Spotty's own saved Proton secret is `~/.local/share/spotty/proton-account/session.json`
   (tokens and the key password, not encrypted). Its folder is 0700 and the file
   0600. The reader ignores a file that others can read. Keep that check. Pass's
   forked session lives in pass-cli's folder (key in the keyring), and VPN's in
   the desktop keyring. Do not copy any of them elsewhere. The Proton web profile
   is not a session store.
3. Unlocked keys, decrypted names, calendar events and vault contents stay in
   memory. Never write them to disk or log them.
4. Never log passwords, 2FA or mailbox codes, key passwords, tokens, fork
   selectors, decrypted names, event titles or file contents. Log errors without
   secret fields.
5. Sharing one sign-in with another Proton client goes only through the session
   fork in `proton-account/src/fork.rs`. `fork_session` allows only `cli-pass` and
   `linux-vpn-gui`, always sends `Independent: 0`, and never sends the login
   password. Pass's fork carries the key password only inside the AES-256-GCM
   payload. No other route: not copying `session.json`, not passing tokens, not
   reusing cookies. A new client id needs a test and a review.
6. No web views or third-party CLIs for Proton sign-ins. Pass's approval link is
   read from pass-cli's output and approved natively. Do not open that link in a
   browser or a web view.
7. Proton's official clients (pass-cli, the VPN library) are used as they are,
   built from pinned sources with SHA-256 checks. Do not patch them.
8. Identify honestly as `external-drive-spotty@<version>-stable`. Never pose as a
   Proton app. The VPN library identifies itself as Proton's Linux GUI client; see
   the "Gaps and stale spots" section of `ARCHITECTURE.md`.
9. Run network work off the GTK thread (`gio::spawn_blocking`, as in `run()` in
   `src/ui/proton_native_ui.rs`).
10. Secrets copied to the clipboard carry the password-manager hint and are cleared
    after 30 seconds (Pass).

## Where state lives

See `ARCHITECTURE.md`, "Data locations". Main places: `session.json` (Spotty's
Proton session; the Pass and VPN sessions are forked from it),
`~/.local/share/spotty/proton-pass/` (pass-cli session; key in the keyring), the
desktop keyring (VPN session, Pass key), `~/.local/share/spotty/proton-web/` (shows
Proton web pages; never signs in), and `~/.cache/spotty/proton-*/` (unpacked
binaries).

## Checklist

1. Read `ARCHITECTURE.md`, "The Proton stack", "Session fork" and "Gaps and stale
   spots". Run `git status`: the Proton files may have uncommitted changes, so
   check the current code before you rely on a path.
2. Identify the layer: crate (`proton-account/src`), Spotty feature
   (`src/features/proton_*.rs`), UI (`src/ui/proton_native_ui.rs`), search
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
- The live sign-in path is untested, and so are the Pass and VPN forks. Do not
  sign in to a real account unless the user asks. List the live steps you could
  not run.
- Never add a test or check that contacts Proton's live API; use the pretend
  server in `tests/fake_proton.rs`.
- Not supported; say so when relevant: FIDO2-only accounts, Proton's
  human-verification check, OpenPGP signature checks on names and events (not
  done yet), shared-with-me, photos and trash in Drive, and all write operations.
