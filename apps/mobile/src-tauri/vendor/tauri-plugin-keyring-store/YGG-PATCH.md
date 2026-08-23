# Ygg compatibility patch

This directory is the crates.io `tauri-plugin-keyring-store` 0.2.0 source
(checksum `31bcea08aef26f1378f8b82004b434698c203f757e86b21346f7a38f8ae558dc`).
Ygg carries three focused changes:

- The plugin's `sha2 = "0.11"` selects stable `sha2` 0.11.0, while pinned
  Iroh 0.95.1 requires `sha2 = 0.11.0-rc.2` through
  `ed25519-dalek` 3.0.0-pre.1. Cargo cannot resolve those versions together,
  so this copy uses API-compatible `sha2` 0.10 for its account hashing.
- Base64 strings that temporarily contain credential bytes are wrapped in
  `Zeroizing<String>` in `src/store.rs`.
- The Android backend is pinned to `=1.0.0`, the exact source carried in the
  adjacent Rust 1.86 compatibility patch, so a lockfile refresh cannot silently
  bypass the reviewed backend.

Recreate the directory from the exact 0.2.0 crate before reviewing or
upgrading it. Remove the `sha2` patch when the pinned Iroh graph no longer
requires the conflicting prerelease, and upstream the temporary-buffer
zeroization where practical. Keep backend upgrades explicit and review the
adjacent compatibility patch with them.
