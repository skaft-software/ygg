# Pi TUI behavioral parity

Pi TUI is the normative implementation for this crate's core behavior.

- Repository: `https://github.com/earendil-works/pi.git`
- Revision: `b79e4cc834970cca69daebffab7df1da7d1e52c4`
- Release: `v0.84.4` (`@earendil-works/pi-tui@0.84.4`)
- npm integrity: `sha512-nPUnwDkLtupPXnZQYrCwPFcuTydCDqTY6ZbFqhsL4S4kVq0AT418kPa/6uXwtaCD+MjBNBltb7ScTYX65yeE1w==`
- Source root: `packages/tui`
- Normative test inventory: [`upstream/pi-tui-0.84.4.json`](upstream/pi-tui-0.84.4.json)
  (33 `*.test.ts` files; `release_status` remains `in_progress`)

Validate the local ledgers with `python3 scripts/verify-pi-parity-profile.py`
from the workspace root. Pass `--pi-source DIR` to additionally inspect the
exact commit object and lightweight tag in an existing Pi worktree. The verifier
reads pinned blobs and trees from Git directly; it neither switches nor depends
on that worktree's current checkout.

To inspect the exact source and tests:

```sh
git clone --filter=blob:none --no-checkout https://github.com/earendil-works/pi.git /tmp/pi
cd /tmp/pi
git sparse-checkout init --cone
git sparse-checkout set packages/tui
git checkout b79e4cc834970cca69daebffab7df1da7d1e52c4
```

## Port gate

A module is not marked ported merely because its API compiles or selected
regressions pass. Every pinned row is `required`; `requires_0.84.4_audit` and
`requires_0.84.4_port` are open statuses. A `passing` or
`approved_divergence` row must name at least one existing, crate-relative Rust
behavioral equivalent. Deviations require an explicit compatibility-layer API
and must not alter the Pi-equivalent core. The verifier blocks
`release_status: complete` while any required row remains open.

Port order:

1. `utils`, `keys`, `stdin-buffer`, `terminal`
2. `tui`, including frame, focus, overlay, cursor, shrink and resize state
3. editor/input, autocomplete and widgets
4. terminal image and Markdown compatibility
5. Rust rich rendering and Ygg native-scrollback extensions

## Current status

Every prior port is being re-audited against the 0.84.4 file and case inventory;
passing an older 0.81.1 equivalent is not sufficient. The JSON ledger is the
machine-readable source of truth while this table remains the compact human view.

| Area | Pinned Pi tests | Status |
|---|---|---|
| ANSI width/wrap/truncate/slicing | `wrap-ansi.test.ts`, `truncate-to-width.test.ts`, `tab-width.test.ts`, regional-indicator regression | In progress; existing algorithms require the 0.84.4 case audit |
| Keys/keybindings/history | `keys.test.ts`, `keybindings.test.ts`, `editor-history-keybindings.test.ts` | In progress; the history/keybinding file is new since the old pin |
| Stdin buffering | `stdin-buffer.test.ts` | In progress; existing sequence/paste behavior requires the 0.84.4 case audit |
| Terminal/colors/native module | `terminal.test.ts`, `terminal-colors.test.ts`, `native-module-path.test.ts` | In progress; native-module-path is new since the old pin |
| Pi retained-frame rendering | `tui-render.test.ts`, `tui-shrink.test.ts` | Existing named ports in `tests/pi_tui_render.rs`; 0.84.4 re-audit required |
| Main/alternate TUI and layout | `tui-cell-size-input.test.ts`, `tui-alt-screen.test.ts`, `layout.test.ts` | In progress; alternate-screen and layout files are new since the old pin |
| Overlay/focus orchestration | all overlay tests and style/CJK regressions | In progress; complete generic overlay/focus API remains |
| Input/editor/navigation | `input.test.ts`, `editor.test.ts`, `word-navigation.test.ts` | In progress; complete named case parity remains |
| Autocomplete/lists/widgets | autocomplete, fuzzy, select/settings/truncated-text tests | Not fully ported; settings-list is new since the old pin |
| Image/Markdown/LaTeX | terminal-image, Markdown, image regression, and `latex.test.ts` | Not fully ported; LaTeX is new since the old pin |

Existing rich-text, capability, and opt-in inline/native-scrollback tests remain
additive Rust behavior and do not count as evidence for a pinned Pi test unless the
ledger names them explicitly.
