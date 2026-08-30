# `ygg-pi-compat`

This directory contains the Node compatibility host used by `ygg pi install`.
It runs Pi extension source through Pi's public loader while Ygg continues to
own the model loop, JSON-RPC transport, trust gates, persistence, and process
cleanup.

## Pinned compatibility profile

The current profile targets exactly
`@earendil-works/pi-coding-agent@0.84.4` on Node 22.19 or newer. The bridge
validates both before importing extension code; it does not silently follow a
newer Pi runtime found on `PATH`. Exact source revision, npm integrity values,
public surface names, and the 78-example corpus live in the machine-readable
[`profiles/0.84.4.json`](profiles/0.84.4.json); the human status ledger and
completion gates live in [COMPATIBILITY.md](COMPATIBILITY.md).

Generated links are inert until separately enabled and trusted. Schema-v2 link
metadata records the bridge profile, Pi and Ygg versions, and a bounded source
fingerprint. `ygg pi list` marks legacy, changed, or otherwise stale links, and
generated links re-verify that fingerprint before Pi imports extension code.
Dependency/build/cache directories are excluded from this source digest and
remain part of the separately reviewed runtime installation.

## Current supported surface

- Pi tools with text/image output, cancellation, bounded progress, argument
  preparation, transformed result details/error/usage, and live tool catalogs;
- initialization-time Pi command discovery as native Ygg slash commands when
  the host negotiates `runtime_commands`, with the generated multiplexed route
  retained only as a compatibility fallback;
- notifications, confirmations, text input, and a plain-text compatibility
  theme;
- basic lifecycle events, prompt/context contributions, and local Pi event-bus
  behavior; and
- host session-name and reasoning snapshots where Ygg already supplies them.

Unsupported APIs fail explicitly. The bridge does not silently emulate provider
registration, session/tree mutation, compaction control, root-agent messaging,
active-tool policy mutation, arbitrary Pi components/editors/widgets, terminal
input, or provider payload hooks.

The scanner is pinned to Pi 0.84.4's public event, registration, action, and UI
names. Unknown APIs fail closed instead of being labeled bridge-compatible.

## Tests

```sh
python3 -m unittest extensions.ygg-pi-compat.tests.test_bridge_protocol

YGG_PI_REAL_PACKAGE=/path/to/@earendil-works/pi-coding-agent \
  python3 -m unittest extensions.ygg-pi-compat.tests.test_bridge_protocol

YGG_PI_REAL_PACKAGE=/path/to/@earendil-works/pi-coding-agent \
  cargo test -p ygg-coding-agent \
  pi::tests::generated_link_runs_the_pinned_real_pi_hello_example_when_selected --lib
```

The real-Pi suite covers the official hello example and an unchanged
`plan-mode` load plus `/todos` smoke. It does not claim plan-mode behavioral
parity: flags, shortcuts, active-tool overlays, widgets, and durable custom
entries remain release blockers.

The bridge uses the selected Pi package's own loader and does not install npm
dependencies. `ygg pi install --pi-package DIR` validates, records, and forwards
an exact nonstandard package location without relying on ambient extension
environment inheritance. Package code still runs with the launching user's
operating-system authority under Ygg's executable-extension trust model.
