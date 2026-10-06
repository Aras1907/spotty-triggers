# Packaged Proton Mail Bridge

This Rust package supplies Proton Bridge settings inside Spotty and embeds
Proton Bridge 3.27.0 for native Linux x86_64. Spotty links the GTK interface
into its main executable and shows the Bridge page inside the app's Settings
window. Users need no second installation.

The login window uses GTK4 and libadwaita, matching Spotty's native GNOME
interface and system light/dark preference. Adaptive forms, animated login
steps, saved-account cards, copy buttons and a password visibility control
cover sign-in and mail-client setup. Closing the window detaches its local
session without stopping Bridge.

## User flow

1. In the Store, find **Server-side installations** and click **Install** for
   Proton Mail Bridge. This enables the local mail server service; it adds no
   search keyword or shortcut.
2. Open Bridge's popup from Spotty's Settings window. If signed out, enter your Proton account details and
   complete any two-factor, mailbox-password or security-key prompts.
3. If already connected, Settings automatically retrieves the generated
   Bridge password, mail username, IMAP/SMTP host, ports and encryption.
   Copy them into your new mail client.
4. Use **Sign out** beside an account to disconnect it. After confirmation,
   displayed secrets are cleared immediately, and the interface waits for
   Bridge to confirm sign-out. Use **Sign in** to connect again.
5. Optionally enable **Start Bridge at desktop login**, then close Settings.
   Spotty keeps the Bridge service available in its own process.

Use **Uninstall** in the Store to remove the service and dismiss its popup from
Spotty. This preserves saved accounts but stops the mail server and disconnects
mail clients, preventing desktop-login startup from reactivating the removed service.
**Install** restores access without adding a trigger word.

Reopen the service's **Settings** button whenever you need mail credentials.
The generated password is hidden by default and can be revealed using its
visibility button. Copying marks it as sensitive, and Spotty excludes marked
content from its clipboard history. Other clipboard managers may ignore the
marker and retain copied content.

A paid Proton Mail plan and a working unlocked Linux keyring are required.
Only one frontend can use Bridge's login stream at a time. An occupied stream
is never stopped or replaced. Unlock the keyring in your desktop's keyring
settings if needed. Closing the login view detaches its stream while the
Bridge service remains available in Spotty.

## Native Cargo packaging

Native builds require GTK4, libadwaita and OpenSSL development libraries.

The default `bundled-bridge` feature builds Proton Bridge as a Go shared library
for Spotty's process and embeds that library with its corresponding source.
Cargo's build script uses Python 3's standard library to fetch pinned upstream
archives and checks committed SHA-256 hashes before accepting downloads. It
builds the Go adapter in Cargo's output folder. GTK4 4.12+ and libadwaita 1.6+
are shared with Spotty. Native builds need their development packages, Go 1.26
with CGO, Python 3 and a C toolchain. The binary requires no download at user
installation time. Its first activation copies the shared library and the
pinned FIDO2/CBOR runtime dependencies into a private Spotty cache. It does not
extract or launch Proton's executable or Qt interface, and makes no system-wide
writes. Cache identity includes the compiled adapter hash, so an upgraded
Spotty build activates its matching library.

The shared library acquires Bridge's single-instance lock before Spotty treats
the service as started. Bridge server goroutines run inside Spotty and stop
when Spotty shuts down or the service is uninstalled. Desktop autostart starts
Spotty in daemon mode; it does not start a separate Bridge process. No
credentials are passed in process arguments.

For standalone development inside this checkout:

```sh
CARGO_HOME="$PWD/.cargo-proton" cargo install --path proton-bridge-gui \
  --locked --root "$PWD/build/proton-bridge-gui"
./build/proton-bridge-gui/bin/spotty-proton-bridge-gui
```

The `SPOTTY_PROTON_BUNDLE_CACHE` build variable can point to an absolute folder containing `bridge.deb`,
`source.tar.gz`, and every archive named in `native_dependencies.json`; every
archive is SHA-256 checked. Set `SPOTTY_PROTON_BUNDLE_OFFLINE=1` to make a build
fail if any pinned archive is absent from that cache. Flatpak packages grant
network access for Bridge and Secret Service access for its Linux keyring. The
desktop-login portal re-enters Spotty through `flatpak run` so the in-process
service starts inside those permissions. Do not use a Flatpak build for native
development.

## Credentials and protocol

Credentials travel to the local Unix socket or loopback TCP endpoint over TLS
trusted only through Bridge's own certificate and authenticated with its local token.
The native TLS connector validates the hostname and certificate, requires TLS 1.2
or newer and negotiates HTTP/2. It supports Bridge's self-signed CA certificate
without enabling system trust roots or bypassing verification.
The form never calls Proton's cloud API directly. Login fields are masked and
cleared after submission, errors, cancellation and closure. Bridge manages
saved login in its vault and Linux keyring. Mail-client passwords are retrieved
only for connected accounts, held in zeroizing app-owned buffers and discarded
on account changes and closure. Desktop startup initializes the saved accounts
without a window, then detaches the login stream so it remains available for
the next settings window.

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
