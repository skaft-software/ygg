# Ygg MSRV patch

This is the crates.io `lopdf` 0.42.0 source (checksum
`25aab26d99567469098e64a02f42679f8965c6401263eefa31d8f2dcc37a221c`),
retained because it fixes RUSTSEC-2026-0187. The upstream release declares
Rust 1.85 support but contains one `let`-chain in `src/object.rs`, which does
not compile on Ygg's Rust 1.86 MSRV. Ygg rewrites that expression as nested
`if let` statements without changing behavior.

The companion graph also pins Iroh 0.95.1, whose crypto dependencies require
`chacha20 = 0.10.0-rc.2`. Upstream `lopdf` selects `rand` 0.10 and therefore
stable `chacha20` 0.10.1, which Cargo cannot resolve alongside that exact
prerelease. This patch uses API-compatible `rand` 0.9 and updates only the
renamed `RngExt` imports.

Recreate this directory from the exact `lopdf` 0.42.0 crate before reviewing
or upgrading the patch. Remove the syntax patch when upstream ships a
compatible release or raises its declared MSRV in a coordinated Ygg release;
remove the `rand` patch when the pinned Iroh graph no longer requires the
conflicting `chacha20` prerelease.
