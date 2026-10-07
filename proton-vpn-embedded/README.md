# Embedded Proton VPN client

Spotty's Proton VPN integration runs **Proton's official Linux client
library** inside Spotty, so users need no separate Proton VPN app or CLI.
Sign-in, connecting and Proton's settings happen in Spotty's own
GTK4/libadwaita window and through the `vpn` search trigger.

## How it works

- `prepare_bundle.py` (run by `build.rs` when the `bundled` feature is on)
  downloads Proton's sources pinned in `sources.json` and accepts them only if
  their SHA-256 matches:
  [python-proton-core](https://github.com/ProtonVPN/python-proton-core),
  [python-proton-keyring-linux](https://github.com/ProtonVPN/python-proton-keyring-linux),
  [python-proton-vpn-api-core](https://github.com/ProtonVPN/python-proton-vpn-api-core)
  and [protun](https://github.com/ProtonVPN/protun) (needed only so Cargo can
  read the core's manifest; the protun feature isn't built).
- It compiles Proton's Rust platform module (`proton-vpn-platform`, PyO3 abi3)
  with the `python,core,kill_switch,local_agent,telemetry` features.
- It installs the third-party wheels from `requirements.lock` with
  `pip --require-hashes`, and Proton's packages with their entry points.
- The result is compressed into Spotty's binary. On first use Spotty unpacks it
  into `~/.cache/spotty/proton-vpn/<bundle id>/`.
- `helper/spotty_vpn_helper.py` drives Proton's API and talks JSON lines with
  Spotty (`src/proton_vpn.rs` in Spotty).

Connections are NetworkManager connections using its built-in WireGuard (or
OpenVPN when its NetworkManager plugin is installed), exactly as in Proton's
app. Proton's library keeps the session in the desktop keyring. Spotty never
stores the password or 2FA code.

## Building

    cargo build --features proton-vpn          # in Spotty

Needs network access on first build, Python 3 with pip and setuptools, Cargo,
and the runtime `libmnl`/`libnftnl` libraries (no `-devel` packages needed).
At runtime it needs the same Python version the bundle was built with,
PyGObject, and NetworkManager's GObject bindings (all standard on Fedora
Workstation). Results are cached in `build/spotty-proton-vpn-cache/` (or
`$SPOTTY_PROTON_VPN_CACHE`).

Without the feature Spotty still builds; the Proton VPN window then explains
how to include the client.

## Updating the pins

Update the commit URLs and SHA-256 values in `sources.json`. Regenerate
`requirements.lock` by resolving Proton's dependencies with pip for the target
Python, downloading the wheels and recording `name==version --hash=sha256:…`.

## Not supported

- Proton's "protun" protocol needs Proton's system NetworkManager plugin;
  Proton's library then uses WireGuard instead.
- Security-key (FIDO2) two-factor sign-in; use an authenticator code.
