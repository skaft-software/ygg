# Vendored provenance

This crate is vendored into the Ygg workspace so a fresh clone builds the exact
terminal renderer used by `ygg-coding-agent`.

- Upstream: <https://github.com/achuthanmukundan00/sexy-tui-rs>
- Upstream base revision: `7770c3ef52d1df5b554f597f77d9e85803d8976d`
- Imported version: `0.2.0`
- License: MIT (`LICENSE`)

The vendored source includes Ygg integration changes that were present in the
reviewed local checkout but not committed at the upstream base revision. Future
updates should be imported deliberately and validated with the full Ygg
workspace test, formatting, and lint gates.
