# Ygg MSRV patch

This is `lopdf` 0.42.0, retained because it fixes RUSTSEC-2026-0187. The
upstream release declares Rust 1.85 support but contains one `let`-chain in
`src/object.rs`, which does not compile on Ygg's Rust 1.86 MSRV. Ygg rewrites
that expression as nested `if let` statements without changing behavior.

Recreate this directory from the exact `lopdf` 0.42.0 crate before reviewing
or upgrading the patch. Remove the patch when upstream ships a compatible
release or raises its declared MSRV in a coordinated Ygg release.
