# `ygg-pi-compat`

This directory contains the Node compatibility host used by published `ygg pi`
aggregate plans. It runs Pi extension source through Pi's public loader while
Ygg continues to own the model loop, JSON-RPC transport, trust gates,
persistence, and process cleanup.

## Pinned compatibility profile

The current profile targets exactly
`@earendil-works/pi-coding-agent@0.84.4` on Node 22.19 or newer. The bridge
validates both before importing extension code; it does not silently follow a
newer Pi runtime found on `PATH`. Exact source revision, npm integrity values,
public surface names, and the 78-example corpus live in the machine-readable
[`profiles/0.84.4.json`](profiles/0.84.4.json). The canonical per-surface,
example, and TUI evidence is in
[`profiles/0.84.4.ledger.json`](profiles/0.84.4.ledger.json);
[COMPATIBILITY.md](COMPATIBILITY.md) is its human view.

Use `ygg pi plan`, `ygg pi preflight --plan FILE`, then `ygg pi publish --plan
FILE` to create an aggregate link; `ygg pi install` is the equivalent local
one-command shorthand. Plans are inert and pin source order, source fingerprints,
nearby dependency-lock fingerprints, the canonical selected runtime path, and its
package integrity. Preflight and publish revalidate every pin without importing
source. Schema-v3 generated links and schema-v2 aggregate locks bind those values
plus the manifest path and explicit-enable/explicit-trust requirement into a
link identity. The bridge verifies the identity before the Pi loader runs and
rechecks runtime integrity afterward. `ygg pi list` marks legacy, changed, or
otherwise stale links; it never claims that a link is trusted.

The live Pi process protocol defaults to API `0.2`. Every published aggregate
also has a canonical API `0.3` `pi-runtime-evidence.json` sidecar containing
static selection and integrity evidence for a future runtime manager. That
sidecar alone does not enable API `0.3` lifecycle or dynamic-command support.

Generated links remain inert until separately enabled and trusted. Dependency,
build, and cache directories are excluded from source fingerprints; supported
adjacent dependency locks and the separately reviewed runtime installation are
bound independently.

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

## API 0.3 provider mode

Provider support is an explicit opt-in: run `ygg pi install SOURCE --api-version
0.3`. API `0.2` remains the default, and existing API `0.2` links are never
upgraded in place. An API `0.3` link declares only host-owned `providers` and
one fixed aggregate Pi-tool dispatcher; it omits the legacy command, UI,
context, notification, confirmation, process, and network contributions.

In this mode, bounded secret-free `registerProvider` and `unregisterProvider`
declarations are synchronized to Ygg's API `0.3` catalog. After the bridge's
bounded initial startup collection window closes and it has received every
response for the serialized registration batch, it emits the additive
`providers/complete` notification, including for an empty catalog. Ygg projects
an owner's declarations only after that completion signal; an older API `0.3`
extension that does not negotiate it reaches a bounded timeout with its
incomplete declarations withheld rather than publishing a partial route.
Ygg performs host authorization and retains credentials and authorization
leases. The bridge passes only canonical semantic request JSON, secret-free
catalog metadata, and a generic cancellation signal to the extension's
`yggStream` adapter. It rejects endpoint/base-URL, headers, API keys,
transport, callback, and OAuth payload authority rather than forwarding it.
Safe semantic `before_provider_request` transforms and reduced
`after_provider_response` status hooks are supported;
`before_provider_headers` is explicitly rejected because headers remain
host-owned.

Outside that opt-in mode, provider registration and provider payload hooks fail
explicitly. The bridge does not silently emulate session/tree mutation,
compaction control, root-agent messaging, active-tool policy mutation,
arbitrary Pi components/editors/widgets, or terminal input.

The scanner is pinned to Pi 0.84.4's public event, registration, action, and UI
names. Unknown APIs fail closed instead of being labeled bridge-compatible.

## Tests

```sh
# Hermetic bridge, public-surface, and ledger fixtures.
python3 -m unittest discover -s extensions/ygg-pi-compat/tests -p 'test_*.py'
python3 extensions/ygg-pi-compat/conformance.py --check --json

# Developer diagnosis only; this does not verify npm tarball integrity.
YGG_PI_REAL_PACKAGE=/path/to/@earendil-works/pi-coding-agent \
  python3 -m unittest discover -s extensions/ygg-pi-compat/tests \
  -p 'test_bridge_protocol.py'

YGG_PI_REAL_PACKAGE=/path/to/@earendil-works/pi-coding-agent \
  cargo test -p ygg-coding-agent \
  pi::tests::generated_link_runs_the_pinned_real_pi_hello_example_when_selected --lib

# Full unchanged-source loader gate: local artifacts only, no download,
# fresh HOME/allowlisted environment, and Linux unshare --net required.
python3 extensions/ygg-pi-compat/conformance.py --full --network-isolated \
  --coding-agent-tarball /local/pi-coding-agent-0.84.4.tgz \
  --tui-tarball /local/pi-tui-0.84.4.tgz \
  --pi-package /local/unpacked/pi-coding-agent \
  --source-root /local/pi-source-at-b79e4cc
```

The full gate validates the pinned npm SRI values and matches the selected
coding-agent and Node-resolved Pi TUI package roots against their tarballs before
it loads all 78 unchanged sources through Pi's public loader. It reports failure
rather than treating a fake fixture, a package directory alone, or a smoke test
as real-runtime proof. The real-Pi suite covers the official hello example and
an unchanged `plan-mode` load plus `/todos` smoke. It does not claim plan-mode
behavioral parity: flags, shortcuts, active-tool overlays, session entries,
root messages, editor/widget transport, and durable custom entries remain
explicitly rejected release blockers. API `0.3` provider cases use the
checked-in deterministic fake Pi loader for catalog, authorization, hooks,
streaming, cancellation, mutation, and cleanup assertions; they are fixture
evidence, not a real-runtime provider-parity claim.

The bridge uses the selected Pi package's own loader and does not install npm
dependencies. `ygg pi plan --pi-package DIR` validates and records an exact
nonstandard package location without relying on ambient extension environment
inheritance. `ygg pi rollback NAME` removes only a validated generated package
from discovery by renaming it into a local rollback directory; it does not delete
reviewed sources or modify trust policy. Package code still runs with the
launching user's operating-system authority under Ygg's executable-extension
trust model.
