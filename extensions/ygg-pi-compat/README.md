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

Generated aggregates are inert until separately enabled and trusted. Their
schema-2 lock records the bridge profile, exact Pi/Ygg versions, pinned Pi
package identity, ordered sources, and bounded source fingerprints.
`ygg pi list` marks legacy, changed, or otherwise stale installations, and the
bridge re-verifies the lock, package, and every source before importing
extension code.
Dependency/build/cache directories are excluded from source fingerprints and
remain bound by the separately reviewed pinned runtime installation. The same
aggregate output can be produced from a reviewed scanner result through the
separate `ygg migrate pi --plan-out` and `--apply` flow.

## Supported API `0.3` surface

The bridge negotiates the complete mandatory API `0.3` feature set. It publishes
one revisioned catalog for tools, commands, flags, shortcuts, all 36 events,
renderers, providers, and roles; later registration changes use atomic
`catalog/replace`.

Pi tools and commands return operation-bound effect journals. Ygg validates and
commits session custom entries/messages/labels, session names, active-tool
policy, model/reasoning requests, owner-scoped messaging, and semantic UI state
at product boundaries. Large ordered-event payloads use immutable pull-based
documents. Session/model/tool snapshots and reverse host calls remain bounded by
the host-issued operation token.

The pinned Pi UI implementation continues to construct editors, autocomplete,
widgets, headers, footers, overlays, renderers, themes, and custom components.
Only validated semantic rows cross into Ygg's terminal owner. Provider catalogs
include public model configuration and opaque callback handles; custom streams,
refresh, OAuth login/refresh/key projection, and provider interception execute
inside the supervised bridge while credentials and user prompts remain
host-mediated.

Calls that cannot be represented in a product mode return an explicit error.
They are never accepted and silently discarded. Reviewed differences from Pi's
in-process host are recorded as `approved safe divergence` in
[COMPATIBILITY.md](COMPATIBILITY.md).

## Tests

```sh
python3 -m unittest extensions.ygg-pi-compat.tests.test_bridge_protocol

YGG_PI_REAL_PACKAGE=/path/to/@earendil-works/pi-coding-agent \
  python3 -m unittest extensions.ygg-pi-compat.tests.test_bridge_protocol

YGG_PI_REAL_PACKAGE=/path/to/@earendil-works/pi-coding-agent \
  cargo test -p ygg-coding-agent \
  pi::tests::generated_link_runs_the_pinned_real_pi_hello_example_when_selected --lib
```

The real-Pi suite executes all 78 official examples unchanged, runs the real
`hello.ts` tool, and verifies unchanged plan-mode effects plus destructive-bash
blocking. The hermetic suite covers catalog replacement, effect identity,
ordered events/documents, remote UI state, custom provider streaming, and the
complete OAuth login/refresh/key callback cycle.

The bridge uses the selected Pi package's own loader and does not install npm
dependencies. `ygg pi install --pi-package DIR` validates, records, and forwards
an exact nonstandard package location without relying on ambient extension
environment inheritance. Package code still runs with the launching user's
operating-system authority under Ygg's executable-extension trust model.
