# spotty-proton-account

Spotty's native Proton account client: sign-in, Proton Drive and Proton
Calendar, and the session forks that give Proton Pass and Proton VPN the same
sign-in. Written in Rust and compiled into Spotty. There is no web view, no
browser engine and no helper process.

| Module | What it does |
| --- | --- |
| `auth` | Proton's SRP sign-in (`core/v4/auth/info` → proof → `core/v4/auth`), the server-proof check, two-factor codes (`auth/v4/2fa`), the optional mailbox password and the key password that unlocks your keys |
| `api` | HTTPS JSON requests with Proton's headers, error format and automatic token refresh |
| `client` | The signed-in account: user and address keys, unlocked in memory only |
| `drive` | "My files": list folders, decrypt names, download and decrypt files (blocks are hash-checked) |
| `calendar` | Calendars and events: calendar keys, event cards, repeating events (`ical`) |
| `pgp` | The few OpenPGP operations needed, over Proton's `proton-crypto` |
| `fork` | Signs Proton Pass's CLI and Proton VPN in with this session through Proton's session fork (no password is shared); only those two child apps are allowed |

SRP and OpenPGP come from Proton's open-source crates `proton-srp` and
`proton-crypto` (MIT, from `rust-registry.proton.me`; the pure-Rust `rustpgp`
back end, so no Go toolchain). The request flow follows Proton's open-source
Drive SDK (`ProtonDriveApps/sdk`, MIT) and its public API shapes.

Spotty identifies itself as `external-drive-spotty@<version>-stable` — a
third-party Drive client — and never claims to be one of Proton's apps.

## Security notes

- Passwords are only used to build the SRP proof and the key password. The login
  password is kept only while a two-factor code or mailbox password is pending.
  The buffers are zeroed afterwards. The session (tokens + key password) is stored
  by Spotty itself, see `src/features/proton_native.rs` in the trigger repo.
- Session forks are checked before anything is sent: only `cli-pass` and
  `linux-vpn-gui` may be forked, always with `Independent: 0`. A fork request
  never carries the login password. Pass's payload is AES-256-GCM, keyed with the
  one-time key from pass-cli's approval link, which only pass-cli can open.
- Unlocked keys and decrypted data are never written to disk by this crate.
- Not implemented yet: OpenPGP signature verification of names and events,
  FIDO2 security keys, Proton's human-verification challenge, shared-with-me /
  photos / trash in Drive, and any write operation.

## Tests

```
cargo test            # unit tests + a pretend-Proton end-to-end test
```

`tests/fake_proton.rs` runs a local server that speaks the API with a real SRP
server, real OpenPGP keys and real ciphertext, so sign-in, two-factor, the key
chain, Drive names and downloads, Calendar events, and the VPN and Pass forks are
exercised end to end without a network. The tests never contact Proton's live
API, and they cannot prove that Proton's live service behaves the same.
