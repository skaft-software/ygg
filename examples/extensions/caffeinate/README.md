# caffeinate executable extension

This Python example uses the dependency-free `ygg-extension-sdk` package to
keep a Mac awake while Ygg is processing a prompt. It starts
`/usr/bin/caffeinate -i -w <extension-pid>` at the `before_prompt` boundary,
releases it after a complete assistant response, and cleans it up when the
extension shuts down or loses its protocol stream.

The `-i` assertion prevents idle system sleep without forcing the display to
stay on or overriding explicit sleep choices. Tying the assertion to the
extension PID also lets macOS release it if the extension exits unexpectedly.
`/caffeinate` reports whether the inhibitor is active, and the interactive TUI
shows an `awake` status contribution while it is running. Unsupported systems
remain usable and receive a warning when a prompt starts.

Install the SDK before copying the directory:

```console
python3 -m pip install ./sdk/python
```

Copy the directory to `.ygg/extensions/caffeinate/`, then explicitly enable and
trust it. For a project extension, one invocation is:

```console
ygg --workspace-trusted \
    --enable-extension caffeinate \
    --trust-extension caffeinate
```

The extension requires macOS and `/usr/bin/caffeinate`. It reads no files and
uses no network. Its declared `process = true` capability is visible consent
metadata for launching the sleep inhibitor; it is not an operating-system
sandbox.

Ygg's current lifecycle API calls `after_response` only for completed runs. If
a run fails or is aborted, the inhibitor remains active until the next
completed response, extension reload, or Ygg shutdown. The `-w` parent binding
still prevents an orphaned assertion after the extension exits.

Run the example's dependency-free tests from the repository root with:

```console
python3 examples/extensions/caffeinate/test_extension.py
```
