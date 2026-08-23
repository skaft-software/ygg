# Ygg Rust 1.86 compatibility patch

This directory is the crates.io `android-native-keyring-store` 1.0.0 source
(checksum `48c6349ddff23194f8fdce2ea8849380f5a4868c1648965b70e801e104cba9b3`).

Ygg carries only these Rust 1.86 compatibility changes:

- lower `rust-version` from 1.88 to 1.86 in `Cargo.toml` and
  `Cargo.toml.orig`; and
- rewrite the two let-chain expressions in `src/by_store/vault.rs` as
  equivalent Rust 1.86 control flow.

The published MSRV and let chains otherwise prevent the independently
buildable companion crate from compiling its Android secure-storage backend
with the repository's Rust 1.86 compatibility toolchain.

To reproduce the patch, unpack the exact 1.0.0 crate, copy its manifests,
licenses, README, and `src/` tree here, then apply only the edits listed above.
Validate the vendored crate with Rust 1.86 and compare it against the published
crate before reviewing or upgrading. Remove this patch when the mobile
toolchain baseline or upstream backend makes it unnecessary.
