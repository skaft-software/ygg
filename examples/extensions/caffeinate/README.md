# caffeinate executable extension

This API `0.2` Python extension keeps a Mac awake while Ygg owns one or more
active turns. Sleep inhibition is domain behavior, so it lives here rather
than in the agent kernel. The extension observes `turn/started`,
`turn/settled`, and `session/settled`, reference-counts overlapping turns, and
runs one bounded `/usr/bin/caffeinate -i -t 1800` subprocess until the last
observed turn settles.

The `-i` assertion prevents idle system sleep without forcing the display to
stay on or overriding explicit sleep choices. The `-t 1800` argument bounds the
assertion to 30 minutes if Ygg cannot deliver a cleanup boundary. This example
does not pass `-w`, so it does not bind `caffeinate` to the extension PID.
`/caffeinate` reports whether the inhibitor is active, and the interactive TUI
shows an `awake` status contribution while it is running. Unsupported systems
remain usable and receive a diagnostic when a turn starts.

Install the SDK before copying the directory:

```console
python3 -m pip install ./sdk/python
```

Copy the directory to `.ygg/extensions/caffeinate/`, then explicitly enable and
trust it. Executable-extension startup requires the default full-access policy.
For a project extension, one invocation is:

```console
ygg --workspace-trusted \
    --enable-extension caffeinate \
    --trust-extension caffeinate
```

Full-access mode uses the Ygg process's ambient operating-system authority; run
this example only from an appropriately isolated, trusted environment.

The extension requires macOS and `/usr/bin/caffeinate`. It reads no files and
uses no network. Its declared `process = true` capability is visible consent
metadata for launching the sleep inhibitor; it is not an operating-system
sandbox.

API `0.2` emits `turn/settled` for completed, failed, interrupted, and cancelled
root turns, so each terminal path releases its reference. `session/settled`
cleans up any remaining references for that session. Extension shutdown and
the top-level protocol cleanup explicitly terminate the child, while the
30-minute subprocess timeout is a final fail-safe. The `-i` assertion prevents
idle system sleep only; it does not prevent display sleep or override an
explicit user sleep request.

Run the example's dependency-free tests from the repository root with:

```console
python3 examples/extensions/caffeinate/test_extension.py
```
