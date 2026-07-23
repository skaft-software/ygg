# Vendored provenance

This crate is vendored into the Ygg workspace so a fresh clone builds the exact
terminal renderer used by `ygg-coding-agent`.

- Upstream: <https://github.com/achuthanmukundan00/sexy-tui-rs>
- Upstream base revision: `7770c3ef52d1df5b554f597f77d9e85803d8976d`
- Imported version: `0.2.0`
- License: MIT (`LICENSE`)

The behavioral source of truth is a pinned Pi TUI release, not `main` and not
this crate's earlier independently evolved behavior:

- Pi source: <https://github.com/earendil-works/pi/tree/20be4b18d4c57487f8993d2762bace129f0cf7c6/packages/tui>
- Pi tag/package: `v0.81.1` / `@earendil-works/pi-tui@0.81.1`
- Pi revision: `20be4b18d4c57487f8993d2762bace129f0cf7c6`
- Pi copyright: Copyright (c) 2025 Mario Zechner
- Pi license: MIT; the upstream notice is preserved in this crate's `LICENSE`
  and in the workspace `THIRD_PARTY_NOTICES.md`.

Core ports must cite and reproduce the pinned Pi tests. Rust-only rich rendering
and Ygg native-scrollback behavior are additive layers and must not redefine
core Pi APIs or semantics. See `UPSTREAM-PARITY.md` for the port gate and order.

The vendored source includes Ygg-specific integration changes maintained in
this workspace. Future updates should be imported deliberately and validated
with the full Ygg workspace test, formatting, and lint gates.
