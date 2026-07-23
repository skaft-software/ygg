# Vendored provenance

This crate is vendored into the Ygg workspace so a fresh clone builds the exact
terminal renderer used by `ygg-coding-agent`.

- Upstream: <https://github.com/achuthanmukundan00/sexy-tui-rs>
- Upstream base revision: `7770c3ef52d1df5b554f597f77d9e85803d8976d`
- Imported version: `0.2.0`
- License: MIT (`LICENSE`)

The renderer is a Rust port of Pi's TUI architecture:

- Pi source: <https://github.com/earendil-works/pi/tree/main/packages/tui>
- Pi copyright: Copyright (c) 2025 Mario Zechner
- Pi license: MIT; the upstream notice is preserved in this crate's `LICENSE`
  and in the workspace `THIRD_PARTY_NOTICES.md`.

The vendored source includes Ygg-specific integration changes maintained in
this workspace. Future updates should be imported deliberately and validated
with the full Ygg workspace test, formatting, and lint gates.
