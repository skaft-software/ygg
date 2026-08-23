# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-05-22

### Added

- **`WriteAccessibility`** (iOS/macOS): configure Data Protection for new writes; default on iOS is **`AfterFirstUnlock`** via `access-policy` modifiers on `set_password` / `set_bytes`.
- **`Builder::ios_write_accessibility`** to set the write policy at plugin init.
- **`Error::KeychainLocked`**: distinct from generic `Keyring(String)` when the protected store is unavailable (device locked, `errSecNotAvailable`, etc.).
- **`KeyringAvailability`** and **`KeyringStore::availability()`** — probe-read without UIKit in the host app.
- **Background helpers**: `get_password_for_background`, `get_bytes_for_background`, `exists_nonempty_for_background` return `None` / `false` when locked instead of surfacing `KeychainLocked`.

### Changed

- iOS writes use `Entry::new_with_modifiers` with `after-first-unlock` by default; reads still use `Entry::new` (same service/account).
- **npm** [`tauri-plugin-keyring-store-api`](https://www.npmjs.com/package/tauri-plugin-keyring-store-api) bumped to **0.2.0** (aligned with the Rust crate; guest-js API unchanged).

### Notes

- Credentials written before 0.2.0 keep their previous accessibility until re-saved (`set_password` / `set_bytes`). No automatic migration command in this release.
- Downstream apps with exhaustive `match` on `Error` must handle `KeychainLocked` (minor breaking within 0.x).

## [0.1.5] - 2026-05-13

### Changed

- **README:** Recommend `pnpm run tauri add keyring-store` (and npm/yarn/cargo equivalents) instead of a bare `cargo add` “automatic” block; clarify **snapshot path** for all OSes; document guest-js API with examples for `ping`, session/client/store/vault, `Location`, naming helpers, and direct account backup helpers.

## [0.1.4] - 2026-05-13

### Fixed

- **docs.rs:** Drop `package.metadata.docs.rs` `rustc-args = ["--cfg", "docsrs"]`. That flag was applied to the whole dependency graph and broke serde / serde_json / derives on nightly (see [serde-rs/serde#2994](https://github.com/serde-rs/serde/issues/2994)). Skip the Linux D-Bus dependency using **`cfg(doc)`** in `Cargo.toml` and `backend.rs` instead of `cfg(docsrs)`, so normal builds still link dbus while `cargo rustdoc` / docs.rs do not.

## [0.1.3] - 2026-05-13

### Fixed

- **docs.rs:** Pin `generic-array` to **0.14.9** so `cargo rustdoc` under nightly `--cfg docsrs` does not compile **0.14.7** (still uses removed `doc_auto_cfg`). Keeps `default-target = "x86_64-apple-darwin"` from 0.1.2.

## [0.1.2] - 2026-05-13

### Fixed

- **docs.rs:** Set `package.metadata.docs.rs` `default-target = "x86_64-apple-darwin"` so documentation builds do not use the default Linux graph (GTK/WebKit → old `winnow`/`toml` crates failing under nightly `--cfg docsrs`). Linux dbus stub + `not(docsrs)` deps remain for developers running `cargo doc` on Linux.

## [0.1.1] - 2026-05-13

### Fixed

- **docs.rs:** Skip linking `dbus-secret-service-keyring-store` when `cfg(docsrs)` so documentation builds on Linux without host GLib/pkg-config (0.1.0 rustdoc build failed for this reason).

## [0.1.0] - 2026-05-13

### Changed

- **Breaking:** Renamed distribution packages to avoid collisions on crates.io/npm: Rust crate **`tauri-plugin-keyring-store`** (import `tauri_plugin_keyring_store`), npm **`tauri-plugin-keyring-store-api`**. Tauri plugin id is **`keyring-store`** — IPC `plugin:keyring-store|<command>`, capability prefix **`keyring-store:`** (e.g. `keyring-store:default`).

### Added

- Initial release: OS keyring backend (`keyring-core` + platform stores).
- Stronghold-shaped IPC and guest-js API (`KeyringSession`, store, vault).
- Optional `crypto` feature: SLIP10 / BIP39 / Ed25519 procedures via `iota-crypto`.
- Encrypted backup (Argon2id + ChaCha20-Poly1305) is always available; it does not depend on the `crypto` feature.
- Rust `KeyringExt` / `KeyringPlugin` for direct store access from backend code.
- Permissions autogeneration and default capability set.
- Direct account IPC: `get_passwords`, `set_passwords`, `delete_passwords`, `password_exists` (max 256 items per bulk call).
- Plaintext backup: `export_passwords_plain` / `import_passwords_plain` (secrets cross IPC in the clear — see README).
- Encrypted backup: `export_passwords_encrypted` / `import_passwords_encrypted`.
- Rust [`naming`](https://docs.rs/tauri-plugin-keyring-store/latest/tauri_plugin_keyring_store/naming/index.html) helpers (`join_prefix`, `split_prefixed`) and guest-js `joinKeyPrefix` / `splitKeyPrefix`.
- Guest-js: `getPasswords`, `setPasswords`, `deletePasswords`, `passwordExists`, `exportPasswordsPlain`, `importPasswordsPlain`, `exportPasswordsEncrypted`, `importPasswordsEncrypted`.

[0.2.0]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.2.0
[0.1.5]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.1.5
[0.1.4]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.1.4
[0.1.3]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.1.3
[0.1.2]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.1.2
[0.1.1]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.1.1
[0.1.0]: https://github.com/s00d/tauri-plugin-keyring-store/releases/tag/v0.1.0
