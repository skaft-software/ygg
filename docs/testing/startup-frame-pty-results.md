# Startup frame PTY lane results

## Focused result

The focused PTY lane passed with the current binary and an explicitly selected
local `ygg 0.6.7` baseline:

```text
2 passed; 0 failed
```

The incremental run took about 0.6 seconds for test execution. It covered both
real-binary mouse modes plus the legacy-inline compatibility path.

## Observed byte/frame delta from v0.6.7

The baseline comparison reported this normalized delta:

```text
--mouse auto
- startup.clear_screen=false
+ startup.clear_screen=true
- startup.first_full_frame.stale_rows=true
+ startup.first_full_frame.stale_rows=false
- startup.ready_frame.stale_rows=true
+ startup.ready_frame.stale_rows=false

--mouse app
- startup.clear_screen=false
+ startup.clear_screen=true
```

The concrete startup change is `CSI 2J` followed by cursor home before the
first synchronized primary-screen frame. It deliberately does not emit `CSI
3J`, so prior terminal history remains available. In `auto` mode this removes
the two pre-seeded stale rows from both the splash and ready screens. In `app`
mode the baseline already scrolled those rows out of the visible viewport, but
the current binary now starts from the same explicit clean viewport.

The comparison found no difference in synchronized resize replay, cursor and
bracketed-paste restoration, terminal-mode restoration, alternate-screen
policy, or mouse-capture policy.

## Contract observations

| Scenario | Observed contract |
| --- | --- |
| Primary `--mouse auto` | 96x18 startup has no visible stale rows; first/ready frames are synchronized; no alternate screen or mouse capture. |
| Primary `--mouse app` | Same clean startup and synchronized frames; app mouse capture is enabled and restored. |
| Resize | 96x18 to 64x12 emits a synchronized redraw containing `CSI 2J` and `CSI 3J`; stale rows are absent afterward. |
| Ctrl-D shutdown | Exit status is zero; cursor is visible, bracketed paste and mouse modes are disabled, and termios matches its pre-start state. |
| Legacy inline | Pre-existing rows are moved to native history without `CSI 3J`; shortening the startup fixture removes its visible transient rows; no alternate screen is used. |

See [the lane guide](startup-frame-pty.md) for the reproducible command,
isolation details, and optional baseline setup.
