# Packaged Proton Mail Bridge

This Rust package supplies Spotty's optional login window and embeds Proton
Bridge 3.27.0 for native Linux x86_64. Spotty links its GUI into the main
executable and starts a separate process with `--proton-bridge-gui` when the
Store's **Install** button is clicked. Users need no second installation.

The login window uses GTK4 and libadwaita, matching Spotty's native GNOME
interface and system light/dark preference. Adaptive forms, animated login
steps, saved-account cards, copy buttons and a password visibility control
cover sign-in and mail-client setup. Closing the window detaches its local
session without stopping Bridge.

## User flow

1. Click **Install** for Proton Mail Bridge in Spotty's Store.
2. Bridge starts automatically. Enter your Proton username and account password.
3. Complete any two-factor, mailbox-password or security-key prompts.
4. The window shows the generated Bridge password, mail username, local IMAP
   and SMTP host, ports and encryption settings. Copy these into your mail client.
5. Optionally enable **Start Bridge at desktop login**, then close the window.
   Bridge keeps running independently of Spotty.

Use `proton settings` to reopen the window and **Saved accounts → Mail settings**
to retrieve the password again. The generated password is displayed by default
and can be hidden with the password visibility button. Copying deliberately places it
on the desktop clipboard, which may be recorded by a clipboard manager.

A paid Proton Mail plan and a working unlocked Linux keyring are required.
Only one frontend can use Bridge's login stream at a time. An occupied stream
is never stopped or replaced. Human verification and keyring setup can use
**Open official Bridge window**, which launches the included Qt GUI. This
handoff briefly restarts a headless Bridge. Normal closure keeps it running.

## Native Cargo packaging

The default `bundled-bridge` feature embeds the official Linux x86_64 runtime
and its corresponding source archive. Cargo's build script uses Python 3's
standard library to fetch the pinned release and checks committed SHA-256
hashes before accepting either download. Outputs stay in Cargo's build folder.
GTK4 4.12+ and libadwaita 1.6+ are shared with Spotty. Standalone compilation
needs their development packages, in addition to Python 3 and a C toolchain.
The binary requires no download at user installation time. Its first activation
extracts the runtime into the user's private Spotty data folder, atomically,
without a package manager or system-wide writes. The bundle also supplies the FIDO2/CBOR libraries needed by the native backend,
with their pinned hashes, source archives and licence notices. These libraries
are used only by the packaged Bridge process. The included Qt fallback adds
package size but is not loaded by the Rust login window.

An existing `protonmail-bridge` on PATH is preferred. For a custom native layout,
`SPOTTY_PROTON_BRIDGE_BACKEND` may identify an absolute backend path. The backend
runs with `--grpc` and the original launcher path for updates and autostart;
no credentials or parent-lifetime flag are passed as arguments.

For standalone development inside this checkout:

```sh
CARGO_HOME="$PWD/.cargo-proton" cargo install --path proton-bridge-gui \
  --locked --root "$PWD/build/proton-bridge-gui"
./build/proton-bridge-gui/bin/spotty-proton-bridge-gui
```

Use `--no-default-features` to build a window for an existing native Bridge,
without bundling the x86_64 payload. The `SPOTTY_PROTON_BUNDLE_CACHE` build
variable can point to an absolute folder containing `bridge.deb` and
`source.tar.gz`; their hashes are still checked. Do not use Flatpak.

## Credentials and protocol

Credentials travel to the local Unix socket or loopback TCP endpoint over TLS
pinned to Bridge's own certificate and authenticated with its local token.
The form never calls Proton's cloud API directly. Login fields are masked and
cleared after submission, errors, cancellation and closure. Bridge manages
saved login in its vault and Linux keyring. Mail-client passwords are retrieved
only for connected accounts, held in zeroizing app-owned buffers and discarded
on account changes and closure. Desktop startup initializes the saved accounts without a window, then detaches
the login stream so it remains available for the next settings window.

No passwords are logged, passed through shell
commands or written by this window. GUI/transport libraries can retain transient
copies, so this is not a process-wide secure-memory guarantee.

The wire protocol is pinned to [upstream Bridge 3.27.0](https://github.com/ProtonMail/proton-bridge/blob/v3.27.0/internal/frontend/grpc/bridge.proto).
Its `User.password` is already the mail-client password: it must **not** be
base64-decoded or replaced with the Proton account password. TLS mock-server
tests exercise multi-stage login, generated credentials and mail settings,
locked accounts, startup controls, occupied frontends and graceful disconnect.
A real Proton account is needed to validate actual cloud authentication.

## Checks

```sh
CARGO_HOME="$PWD/.cargo-proton" cargo test --manifest-path proton-bridge-gui/Cargo.toml --locked
CARGO_HOME="$PWD/.cargo-proton" cargo clippy --manifest-path proton-bridge-gui/Cargo.toml --locked --all-targets -- -D warnings
```
