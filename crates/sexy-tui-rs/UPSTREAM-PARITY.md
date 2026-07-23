# Pi TUI behavioral parity

Pi TUI is the normative implementation for this crate's core behavior.

- Repository: `https://github.com/earendil-works/pi.git`
- Revision: `20be4b18d4c57487f8993d2762bace129f0cf7c6`
- Release: `v0.81.1` (`@earendil-works/pi-tui@0.81.1`)
- Source root: `packages/tui`

To inspect the exact source and tests:

```sh
git clone --filter=blob:none --no-checkout https://github.com/earendil-works/pi.git /tmp/pi
cd /tmp/pi
git sparse-checkout init --cone
git sparse-checkout set packages/tui
git checkout 20be4b18d4c57487f8993d2762bace129f0cf7c6
```

## Port gate

A module is not marked ported merely because its API compiles or selected
regressions pass. Every test in the corresponding pinned Pi test files must
have a named Rust behavioral equivalent. Deviations require an explicit
compatibility-layer API and must not alter the Pi-equivalent core.

Port order:

1. `utils`, `keys`, `stdin-buffer`, `terminal`
2. `tui`, including frame, focus, overlay, cursor, shrink and resize state
3. editor/input, autocomplete and widgets
4. terminal image and Markdown compatibility
5. Rust rich rendering and Ygg native-scrollback extensions

## Current status

| Area | Pinned Pi tests | Status |
|---|---|---|
| ANSI width/wrap/truncate/slicing | `wrap-ansi.test.ts`, `truncate-to-width.test.ts`, `tab-width.test.ts` | In progress; core algorithms and overlay segment primitives ported |
| Keys | `keys.test.ts` | In progress; Kitty, alternate-layout, keypad, modifyOtherKeys and legacy families ported |
| Stdin buffering | `stdin-buffer.test.ts` | In progress; sequence splitting, semantic paste, timeout polling and Kitty duplicate suppression ported |
| Terminal | `terminal.test.ts` | In progress; negotiation parser/lifecycle sequences and Apple Return normalization ported |
| TUI | overlay tests plus `tui-*.test.ts` | In progress; options now drive layout and segment composition; full frame/focus parity remains |
| Editor/autocomplete/widgets | corresponding pinned tests | Not ported |
| Image/Markdown | corresponding pinned tests | Not ported |

The existing `rich_text`, capability and inline/native-scrollback tests cover
additive Rust behavior; they do not count as evidence that a Pi core module is
ported.
