# Startup frame PTY lane

`startup-frame-pty` is a small Unix regression lane for Ygg's primary-screen
startup boundary. It runs the compiled `ygg` binary under a controlled PTY and
checks the bytes and emulated terminal state rather than relying on a human
terminal or a live provider.

Run it from the repository root:

```bash
scripts/test-startup-frame-pty.sh
```

The lane uses Cargo's existing target directory. Set `CARGO_TARGET_DIR` before
running it when a shared build cache is required.

## What it covers

The real-binary contract runs twice, with `--mouse auto` and `--mouse app`:

- a 96x18 PTY is seeded with two `YGG_PTY_STALE_STARTUP_*` rows before Ygg
  starts;
- the initial splash frame and the first ready frame are parsed through
  `vt100`; the visible stale rows must be gone;
- a controlled resize to 64x12 must produce a synchronized full redraw with
  `CSI 2J` and `CSI 3J`;
- Ctrl-D is the only supplied input. It must exit successfully and restore
  cursor visibility, bracketed paste, mouse modes, and termios state;
- no primary-screen scenario may enter an alternate screen. App mouse capture
  is required only for `--mouse app`.

The separate `legacy-inline` test drives the explicit `sexy-tui-rs` inline
scrollback compatibility path through the same PTY capture. It confirms that
its first paint preserves the pre-existing viewport in native history rather
than clearing saved lines, that shortening the transient fixture removes its
visible rows, and that it also avoids alternate-screen mode.

The expected normalized contracts and row fixtures are in
`crates/ygg-coding-agent/tests/fixtures/startup-frame-pty/`. The harness is
`crates/ygg-coding-agent/tests/startup_frame_pty.rs`.

## Isolation and safety

Each real-binary run creates a disposable HOME, workspace, and session store.
It writes only an inert custom-provider fixture with an empty API key,
`auto_discover: false`, and `http://127.0.0.1:9/v1/` as its unreachable base
URL. The process is invoked with `--offline`, `--no-context-files`, and
`--no-tools`; no prompt is submitted. Consequently it neither reads a user's
credentials nor contacts a provider.

The lane requires Unix `openpty` support. It gives the child a controlling TTY
(`setsid` plus `TIOCSCTTY`) so the resize signal and terminal-size handling
match an interactive shell.

## Optional v0.6.7 comparison

An explicitly selected local v0.6.7 binary can be compared without making it a
default test dependency:

```bash
YGG_STARTUP_FRAME_BASELINE=/absolute/path/to/ygg-v0.6.7 \
  scripts/test-startup-frame-pty.sh --nocapture
```

The harness first verifies that the selected binary reports `0.6.7`. It then
prints the normalized byte/frame delta and requires lifecycle behavior that is
unrelated to the startup fix (synchronized resize replay, restoration,
alternate-screen policy, and mouse policy) to remain compatible. The expected
startup clear/stale-row correction is intentionally allowed to differ.
