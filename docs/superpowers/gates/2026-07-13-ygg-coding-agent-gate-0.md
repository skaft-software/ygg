# Gate 0 — sexy-tui integration notes

**Date:** 2026-07-13
**Terminal:** WezTerm 0-unstable-2025-05-18

The spike was exercised in a real terminal after buffering diagnostics so they
would not interfere with sexy-tui's differential renderer.

Verified:

- typing updates the TUI;
- the ticker can request renders without input;
- resize events were received and the view reflowed;
- Ctrl+S arrived as `KeyEvent { code: Char('s'), modifiers: CONTROL }`;
- Ctrl+C arrived as `KeyEvent { code: Char('c'), modifiers: CONTROL }`;
- normal exit restores the terminal.

Caveat recorded for v1 key handling:

- WezTerm's default `Alt+Enter` binding is `ToggleFullScreen`, so the chord is
  intercepted by the terminal and no crossterm event reaches the application.
- Sending Escape followed by Enter produces two events:
  `KeyEvent { code: Esc, modifiers: NONE }` and
  `KeyEvent { code: Enter, modifiers: NONE }`; it is not an Alt+Enter event.
- A WezTerm binding can send `CSI 13;3u` (`\x1b[13;3u`), which crossterm parses
  as `Enter` with the `ALT` modifier. The application keymap must also keep a
  compatibility path for terminals that encode Alt+Enter differently.

The requested decision was to note this terminal-specific caveat and continue;
the Alt+Enter criterion is therefore not marked as universally passed.
