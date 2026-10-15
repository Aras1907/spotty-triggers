# Privacy and security — Spotty triggers

Review date: **2026-10-04**. The complete application policy and review findings
are in [Spotty's PRIVACY_AND_SECURITY.md](https://github.com/Aras1907/Spotty/blob/main/PRIVACY_AND_SECURITY.md).
When this repository is the `trigger-backends` submodule, the same document is
available at `../PRIVACY_AND_SECURITY.md`.

## Repository-specific boundaries

- Native Rust backends are compiled into Spotty. Their manifest enables a
  shipped feature; it does not install executable code dynamically.
- Shell manifests are arbitrary user-level executable code. Installation must
  show the exact command and require approval. Use `{query}` unquoted as a shell
  word; Spotty supplies quoting. Do not re-interpret query data through `eval`
  or another interpreter. Quoting cannot secure an untrusted template.
- Web manifests support HTTP(S) links. Activating one sends the query to its
  website. Translation uses the configured LibreTranslate endpoint, which is
  local by default but can be remote. Dictionary/currency backends can also
  make network requests; local processing is not a universal privacy promise.
- The mutable repository/catalog is trusted and is not independently signed.
  Maintainer account compromise can affect users installing manifests. Protect
  publishing credentials and review every manifest/backend contribution.
- Clipboard, search history, OCR text and document previews can contain secrets.
  Protected state writes create owner-only files/directories but do not encrypt
  them. Disabling Clipboard stops new capture and does not erase existing data.
- [`src/security.rs`](src/security.rs) implements shared private storage and
  bounded HTTP downloads. Host applications compiling these modules must wire
  up startup directory protection and privacy preferences, as Spotty does.

Preview parsers still have no complete process sandbox. Maintenance advisories
in the app's dependency chains and other residual risks are described in the
main report. The backend is verified through Spotty's native `cargo build
--locked`; there is no separate standalone Cargo package in this repository.
