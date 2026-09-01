# Pi TUI behavioral parity

Pi TUI `0.84.4` is the normative renderer for Pi extension components.

- Repository: `https://github.com/earendil-works/pi.git`
- Revision: `b79e4cc834970cca69daebffab7df1da7d1e52c4`
- Release: `v0.84.4` (`@earendil-works/pi-tui@0.84.4`)
- npm integrity: `sha512-nPUnwDkLtupPXnZQYrCwPFcuTydCDqTY6ZbFqhsL4S4kVq0AT418kPa/6uXwtaCD+MjBNBltb7ScTYX65yeE1w==`
- Source root: `packages/tui`
- Machine ledger: [`upstream/pi-tui-0.84.4.json`](upstream/pi-tui-0.84.4.json)
  (33 required `*.test.ts` files; `release_status: complete`)

Validate the ledgers with:

```sh
python3 scripts/verify-pi-parity-profile.py
python3 scripts/verify-pi-parity-profile.py --pi-source /path/to/pi-at-b79e4cc
cargo test -p sexy-tui-rs --tests
```

## Audit result

All 33 upstream test files were audited at the pinned revision. The split is
intentional:

1. Terminal-owned behavior that Ygg implements in Rust is directly ported and
   marked `passing`: ANSI width/wrap/truncation, regional indicators, tab
   accounting, terminal input/color parsing, terminal image cleanup, retained
   frame rendering/shrink, Markdown/LaTeX rendering, and overlay style safety.
2. Pi component internals that remain inside the trusted compatibility process
   are marked `approved_divergence`: editor/history, keybinding manager,
   autocomplete/fuzzy matching, list widgets, generic layout/overlay focus, and
   alternate-screen orchestration. Ygg does not reimplement those classes.
   Instead, the exact pinned Pi implementation renders them to bounded semantic
   frames, and `remote_component.rs` validates identity, generation, revision,
   width, text/style/link safety, cursor geometry, and resource limits before
   terminal-owned code can display them.

This is an architectural divergence, not an unsupported call: extension-facing
Pi components still execute unchanged. Only terminal ownership, input routing,
persistence, and process supervision remain in Ygg.

## Evidence by area

| Area | Pinned Pi tests | Rust evidence | Result |
|---|---|---|---|
| ANSI width/wrap/truncate/tabs/flags | `wrap-ansi`, `truncate-to-width`, `tab-width`, regional-indicator regression | `src/utils.rs` | Passing |
| Terminal input and colors | `stdin-buffer`, `terminal`, `terminal-colors` | `src/terminal.rs`, `src/terminal_colors.rs` | Passing |
| Retained frame, shrink, images, style isolation | `tui-render`, `tui-shrink`, terminal-image regressions, overlay-style-leak | `tests/pi_tui_render.rs`, `tests/remote_component.rs` | Passing |
| Markdown and LaTeX | `markdown`, `latex` | `src/rich_text/markdown.rs`, `tests/rich_rendering.rs` | Passing |
| Pi editor, input, history, keys and keybindings | editor/input/key files | `tests/remote_component.rs` | Approved remote-component divergence |
| Autocomplete and widgets | autocomplete/fuzzy/select/settings/truncated-text | `tests/remote_component.rs` | Approved remote-component divergence |
| Layout, overlays, alternate screen and cell queries | layout/overlay/TUI files | `tests/pi_tui_render.rs`, `tests/remote_component.rs` | Approved remote-component divergence |
| Native helper lookup | `native-module-path.test.ts` | `tests/remote_component.rs` | Approved: helper resolution remains in the digest-bound Pi process |

A closed machine-ledger row names the concrete crate-relative evidence file.
The parity verifier rejects a closed row without such evidence and rejects a
`complete` TUI ledger while any required row remains open.
