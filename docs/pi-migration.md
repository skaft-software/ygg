# Migrating from Pi

Ygg can inventory an installed Pi setup without running package code or invoking
a model:

```console
ygg migrate pi --dry-run
```

The same deterministic scanner can write a reviewable, content-digested plan
for one inert aggregate and apply only that saved plan in a separate step. This
is not a source-compatibility or full-parity promise.

## Current command

The scanner reads Pi's user settings at `~/.pi/agent/settings.json` and the
selected project's `.pi/settings.json`. `PI_CODING_AGENT_DIR` or `--pi-home`
can select another user directory, and `--project` can select another project:

```console
ygg migrate pi --dry-run --project /path/to/project
ygg migrate pi --dry-run --json > pi-inventory.json
```

With neither `--plan-out` nor `--apply`, the command is an inventory-only dry
run; `--dry-run` makes that intent explicit. To stage an aggregate migration:

```console
ygg migrate pi --plan-out ./pi-plan.json \
  --pi-package /absolute/path/to/pi-coding-agent \
  --extension-root /absolute/path/to/ygg/extensions \
  --name pi-compat-0-84-4
# Review the JSON plan and its ordered, fingerprinted sources.
ygg migrate pi --apply ./pi-plan.json
# Non-interactive automation must opt in explicitly:
ygg migrate pi --apply ./pi-plan.json --yes
```

Planning scans the setup and writes a private schema-2 plan but does not publish
an extension. Apply revalidates the plan and operation digests, every locked
source and pinned Pi package precondition, and the expected destination state
before asking for confirmation. It then atomically publishes only
`bridge.mjs`, `extension.toml`, and `pi-lock.json` into one disabled, untrusted
Ygg extension directory. The generated manifest carries the lock's bridge
SHA-256, so Ygg verifies and stages those exact script bytes before process
execution. A changed source/package, tampered plan, symlinked or conflicting
destination, or non-interactive apply without `--yes` fails closed.

Inventory, planning, and apply all exit before normal Ygg configuration,
provider discovery, session startup, extension startup, or model bootstrap.
They consume zero model tokens and never import Pi package code. Apply is the
only form that writes migration output; it does not modify the Pi setup, install
dependencies, import credentials, enable the extension, or grant trust.

## Install a compatible aggregate

Once a local Pi extension or installed Pi package has been reviewed, create an
inert Ygg aggregate without running its code:

```console
ygg pi install ./path/to/extension.ts
ygg pi install ./path/to/pi-package
# Preserve Pi load order and shared state in one process:
ygg pi install ./first.ts --with ./second-package --with ./third.ts \
  --name pi-compat-0-84-4
ygg pi list
```

The aggregate lives under `~/.ygg/extensions/` by default, records the
existing source set, and embeds the pinned `ygg-pi-compat` bridge. It does not
install npm dependencies, run lifecycle scripts, copy the Pi package or source,
or enable/trust the resulting Ygg extension. The schema-2 aggregate lock records
the exact bridge profile, Pi/Ygg versions, pinned Pi package identity, ordered
sources, and bounded source fingerprints. `ygg pi list` marks legacy or changed
installations stale without asserting trust, and the bridge rejects lock,
package, or source drift before importing extension code. Source fingerprints
exclude `.git`, `node_modules`, `target`, and recognized cache directories;
dependency/runtime changes remain bound separately by the pinned Pi package
metadata. Start a current aggregate explicitly after verifying its source:

```console
ygg --enable-extension pi-extension-name --trust-extension pi-extension-name
```

The command-line trust above is invocation-scoped. Persistent trust for a locked
aggregate must use the digest-qualified grant printed by `/extensions status`;
changing its ordered source set or lock invalidates that grant. See
[Executable extensions](extensions.md#layout-and-discovery).

Bridge profile `0.3.0` targets exactly
`@earendil-works/pi-coding-agent@0.84.4` and Node 22.19 or newer. The bridge
validates that profile before importing extension code instead of silently using
a newer runtime found on `PATH`. Pass `ygg pi install --pi-package DIR ...` when
the package is not in a conventional location; the generated inert aggregate
records and forwards that exact path across Ygg's sanitized subprocess
environment. The exhaustive per-event/API/UI ledger and completion gates are
maintained in
[`extensions/ygg-pi-compat/COMPATIBILITY.md`](../extensions/ygg-pi-compat/COMPATIBILITY.md).

The API `0.3` bridge publishes transactional tools, commands, flags, shortcuts,
all ordered events, renderers, providers, and roles. Session/model/tool state,
custom entries and messages, active-tool overlays, semantic remote UI frames,
provider streaming, and OAuth callbacks cross bounded owner-fenced host
contracts. Product-mode reductions are explicit approved divergences; unavailable
operations return errors rather than becoming no-ops.

The scanner:

1. reads bounded user and project Pi settings;
2. resolves configured local, managed npm, and managed git package locations
   without installing missing packages;
3. applies Pi package manifests, conventional resource directories, package
   filters, and top-level resource overrides;
4. records installed package names and versions;
5. hashes bounded package source/configuration and lockfiles separately;
6. parses JavaScript, TypeScript, and TSX with tree-sitter;
7. follows bounded relative source imports inside each package;
8. inventories Pi event subscriptions, registrations, UI calls, mutations, and
   runtime imports; and
9. derives conservative filesystem, process, network, secret, native-module,
   and dynamic-import signals.

Malformed, missing, oversized, linked, or unsupported inputs become report
diagnostics. One bad package does not prevent the rest of the inventory.

### Classification

The human and JSON reports use migration-path classifications:

| Path | Meaning |
| --- | --- |
| `direct` | Pi skill or Markdown prompt content has a deterministic Ygg resource path. The dry run does not copy it. |
| `replace` | Reserved for an exact package/version/source-hash recipe that selects a Ygg-native replacement. No replacement recipes ship in this first scanner slice. |
| `bridge` | The extension uses only surfaces implemented by the pinned compatibility process. A generated aggregate still needs a successful runtime handshake before it is known compatible. |
| `native_port` | The extension uses a private/runtime-specific behavior outside the pinned public profile and needs an explicit Ygg-native port. |
| `manual` | The extension depends on a private Pi ABI, an unbounded native module, or a product-specific authority that cannot cross the compatibility contract; redesign is required. |
| `blocked` | The package could not be resolved/read or parsed completely, or it uses names outside the pinned Pi 0.84.4 public compatibility profile. |

`bridge` describes a migration candidate, not current runtime availability or
exact behavioral fidelity. The report never silently treats an unsupported call
as a no-op.

### Machine-readable report

`--json` emits schema version `1`. Its top-level safety fields are explicit:

```json
{
  "schema_version": 1,
  "source": "pi",
  "mode": "dry_run",
  "model_usage": "disabled",
  "package_code_executed": false
}
```

Package entries include the configured source and scope, resolved root, package
name/version when available, source and lock hashes, discovered resources,
extension analyses, analyzed file/byte/node counts, unresolved internal imports,
and diagnostics. Resource paths include whether Pi's current
filters enable them. The source hash covers the package manifest, discovered
resources, reachable relative modules, and bounded source/configuration files;
the lock hash covers supported npm/pnpm/Yarn lockfiles. A hash is omitted when
its complete selected input cannot be read within the bounds. A future recipe
must key on package identity, version, source hash, and lock hash rather than
package name alone.

A file written with `--plan-out` is a separate schema-2 contract. It records
absolute owner/destination paths, the expected absent-or-identical destination
state, one `publish_aggregate_lock` operation, the complete aggregate lock, and
both operation and whole-plan SHA-256 digests. Those digests detect edits; they
are not a signature or proof of who authored the plan. Apply therefore also
revalidates every filesystem/package/source precondition and requires explicit
human or `--yes` authorization.

## Safety and bounds

The scanner does not:

- execute or import a Pi extension;
- run an npm lifecycle script or install a missing package;
- trust and start a Ygg executable extension;
- send source, settings, or the report to a model or network service;
- copy, rewrite, or delete Pi or Ygg files; or
- read Pi authentication/model credential stores.

Plan generation has the same no-execution boundary and writes only the selected
private plan file. Apply reads that bounded regular file and publishes only the
three generated aggregate files under its exact destination. Publication uses a
same-filesystem private staging directory and atomic rename, refuses destination
extras or drift, and never rewrites an existing non-identical directory.

Settings, package manifests, source files, lockfiles, resource counts, relative
import closure, and aggregate hashing all have fixed limits. Selected files use
Ygg's descriptor-bound, no-follow regular-file reader. Symlinked package roots
and resources are rejected. `--npm-root` only adds an explicitly selected legacy
`node_modules` search root; the scanner never executes a configured
`npmCommand` from Pi settings.

Static authority signals are conservative inventory, not proof that a package
will or will not exercise an effect at runtime. The compatibility host runs
third-party npm code with the launching user's operating-system authority under
Ygg's executable-extension trust model. See [Executable
extensions](extensions.md#kernel-boundary) and the [security
policy](../SECURITY.md).

## Migration architecture

Recreating Pi's mutable in-process JavaScript object ABI is deliberately not
the goal. The target is semantic compatibility with Pi 0.84.4's public extension
surface through language-neutral, typed subprocess contracts. Pi extensions can
mutate agent, provider, session, and TUI state directly; Ygg keeps those objects
behind an authority boundary and represents compatible mutations as validated
host effects instead.

The intended staged system is:

```text
scanner/compiler
  + exact package recipes
  + one persistent Pi compatibility process
  + explicitly selected agentic fallback
```

### Deterministic scanner/compiler

The shipped scanner is the inventory front end for this stage. `ygg pi install`
and the explicit migration plan/apply flow create the same inert aggregate form
for reviewed sources; neither installs dependencies nor executes package code.
No direct skill/prompt copy or replacement recipe is applied yet. Future recipes
can copy compatible skills/prompts, transform known configuration, and cache
intermediate results by source and lock hash. Those operations should remain
deterministic and model-free.

### Compatibility process

The generated `ygg pi install` aggregate hosts Pi's pinned public
`ExtensionAPI` through the persistent `ygg-pi-compat` process. Repeated
`--with` arguments record an ordered, source-fingerprinted set in one aggregate
lock and load those sources through one real `ExtensionRunner`, preserving their
local event bus, `globalThis`, and shared registries. `ygg migrate pi --plan-out`
selects the scanner's enabled extension sources in Pi load order and locks that
whole reviewed set; `--apply` publishes it only after revalidation and explicit
authorization.

Unsupported or unavailable product-mode calls must raise a clear compatibility
error. The bridge never silently discards a policy, mutation, lifecycle, or UI
call. API `0.3` publishes the complete initial catalog and uses atomic catalog
replacement for later changes; every mutating handler returns an operation-bound
effect journal.

### Exact recipes

A recipe may replace an implementation with the capability it provides—for
example, importing MCP configuration into a future `ygg-mcp` package instead of
porting a Pi-specific MCP UI. Recipe lookup must require the package identity,
exact installed version, source hash, and lock hash. Name-only recipes are not
safe enough to apply automatically.

### Agentic fallback

Model-assisted porting remains opt-in and receives the scanner's structured
residual, relevant source functions, target API contract, and tests—not an
entire setup by default. The command must show where model use begins before any
request is made. Private Pi internals and provider transports outside the pinned
public configuration/callback contract should report manual redesign rather than
trigger an unbounded automatic port.

## Deliberate compatibility boundary

Capability migration is often practical even when exact UX is not:

- basic tools, commands, notifications, and local event-bus behavior are strong
  bridge candidates;
- MCP, search, browser, LSP, memory, and subagent behavior belong in replaceable
  Ygg extension processes, not the kernel;
- Pi input transforms, tool interception, durable extension state,
  per-turn tool-policy overlays, provider callbacks, and remote components use
  narrow API `0.3` events/effects rather than direct access to Ygg objects;
- custom provider OAuth/stream handlers and editor/header/footer/widget
  components execute in the pinned process while credentials, persistence,
  terminal input, and final rendering remain host-owned; and
- future UI additions should extend semantic, frontend-neutral frames rather
  than grant arbitrary terminal ownership.

New protocol additions remain tied to concrete migrated packages, wire-level
tests, bounded failure semantics, and no cost on the no-extension path.

## Product promise

The intended promise is:

> Ygg can inspect a Pi setup, migrate portable resources without model tokens,
> replace known infrastructure with exact Ygg-native recipes, run the pinned
> public Pi extension surface through an explicitly trusted bridge, and identify
> private or product-specific behavior that still requires a port.

Today the zero-token scanner, content-digested aggregate plan/apply flow, and
explicitly trusted pinned compatibility aggregates are implemented. API `0.3`
passes the six pinned release gates: unchanged plan mode, all 78 official
examples, all 33 TUI audit rows, zero silent unsupported calls, one aggregate
process, and provider/OAuth callbacks. Direct portable-resource apply and exact
replacement recipes remain separate future migration features rather than Pi
extension-parity blockers.
