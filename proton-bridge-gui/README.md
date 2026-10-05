# Spotty Proton Mail Bridge login window

An optional native Rust companion launched by Spotty's `proton login` or
`proton gui` trigger. All source, Cargo builds, and checks can stay in this
repository. Embedding the window in Spotty's settings would also require
changes in the main Spotty application.

## Install

Requires Linux, a graphical X11 or Wayland session, Rust, and the native
`protonmail-bridge` executable. Install Bridge using
[Proton's guide](https://proton.me/support/protonmail-bridge-install).
Bridge also needs a paid Proton plan that includes Mail and a working keyring.

```sh
cargo install --path proton-bridge-gui --locked
```

For a build and installation entirely inside this checkout:

```sh
CARGO_HOME="$PWD/.cargo-proton" cargo install --path proton-bridge-gui \
  --locked --root "$PWD/build/proton-bridge-gui"
./build/proton-bridge-gui/bin/spotty-proton-bridge-gui
```

Put the resulting `bin` directory on the `PATH` used to start Spotty to have
the trigger discover this local installation. The ordinary Cargo install is
also discovered in `~/.cargo/bin`. No Flatpak build is used.

## Sign in

1. Install the Proton Mail Bridge trigger from Spotty's Store, then type
   `proton login` and press Enter.
2. If Bridge is stopped, select **Start Bridge**. This starts its local backend
   without the official GUI, then connects the companion.
3. Enter your email/username and account password, then select **Sign in**.
4. Enter the authenticator code, separate mailbox password, or security-key
   PIN when Bridge requests it. Security-key touch prompts are also shown.
5. Enable **Open Bridge at desktop login** if desired. Choose
   **Close and keep Bridge running** when finished.

Saved account names and connection states appear above the form. To configure
Thunderbird or another mail client, select **Open official Bridge window**
and use the local IMAP/SMTP settings and Bridge-generated password there.

The official GUI and this companion cannot own Bridge's event stream at the
same time. An occupied stream produces a clear message and is never stopped
or replaced. Choose **Quit Bridge** in the official GUI first if you want to
use this form. Human verification and keyring setup use the official GUI.
Opening it from an active companion briefly restarts the headless Bridge;
closing the companion normally preserves the running backend.

**Start Bridge** finds the packaged `bridge` backend beside the resolved
`protonmail-bridge` launcher. For a custom installation with a wrapper script
or a different layout, set `SPOTTY_PROTON_BRIDGE_BACKEND` to the backend's
absolute path before starting the companion. The backend is launched with
`--grpc` and the original launcher path for updates and autostart; no
credentials or parent-lifetime flag are passed as process arguments.

## Credential handling and compatibility

The form sends credentials over authenticated TLS to Bridge's local Unix
socket (or its loopback-only TCP port). It verifies Bridge's own certificate
and supplies the local server token. It never calls Proton's cloud API
directly, places credentials in command arguments, logs them, or saves form
values. Secret fields are masked and app-owned buffers are cleared after
submission, errors, cancellation, and closure. Transport and GUI libraries
may retain transient memory copies; this is not a process-wide secure-memory
guarantee. Bridge manages persistent login and credentials using its vault
and Linux keyring.

The client targets the current Bridge 3.x
[local gRPC protocol](https://github.com/ProtonMail/proton-bridge/blob/master/internal/frontend/grpc/bridge.proto).
This is an internal interface and future Bridge releases may change it. The
mock-server checks cover the actual TLS transport and login sequence; a real
account login and a live desktop session are still needed for end-to-end
validation. The companion does not read mail content, and it does not decode
or display the Bridge-generated mail-client passwords included in account
responses.

## Check

```sh
CARGO_HOME="$PWD/.cargo-proton" cargo test --manifest-path proton-bridge-gui/Cargo.toml --locked
CARGO_HOME="$PWD/.cargo-proton" cargo clippy --manifest-path proton-bridge-gui/Cargo.toml --locked --all-targets -- -D warnings
```
