# Migrating from Pi

Ygg can inventory an installed Pi setup without running package code or invoking
a model:

```console
ygg migrate pi --dry-run
```

This is the first, deterministic stage of Pi migration. It is an inspection and
planning command, not a source-compatibility promise and not yet an apply
command.

## Current command

The scanner reads Pi's user settings at `~/.pi/agent/settings.json` and the
selected project's `.pi/settings.json`. `PI_CODING_AGENT_DIR` or `--pi-home`
can select another user directory, and `--project` can select another project:

```console
ygg migrate pi --dry-run --project /path/to/project
ygg migrate pi --dry-run --json > pi-migration.json
```

All current invocations are dry runs; `--dry-run` makes that intent explicit and
keeps scripts compatible with a future separately authorized apply stage. The
command exits before normal Ygg configuration, provider discovery, session
startup, extension startup, or model bootstrap. It therefore consumes zero
model tokens.

## Link a compatible extension

Once a local Pi extension or installed Pi package has been reviewed, create an
inert Ygg wrapper without running its code:

```console
ygg pi install ./path/to/extension.ts
ygg pi install ./path/to/pi-package
# Preserve Pi load order and shared state in one process:
ygg pi install ./first.ts --with ./second-package --with ./third.ts \
  --name pi-compat-0-84-4
ygg pi list
```

The wrapper lives under `~/.ygg/extensions/`, points at the existing source, and
uses the persistent `ygg-pi-compat` host. It does not install npm dependencies,
run lifecycle scripts, copy the Pi package, or enable/trust the resulting Ygg
extension. Schema-v2 link metadata records the bridge profile, exact Pi and Ygg
versions, and a bounded source fingerprint; `ygg pi list` marks legacy or changed
links stale without asserting trust, and a generated bridge rejects a fingerprint
mismatch before importing extension code. The source digest excludes `.git`,
`node_modules`, `target`, and recognized cache directories; dependency/runtime
changes remain part of the separately reviewed Pi installation rather than the
source-link digest. Start a current link explicitly after
verifying its source:

```console
ygg --enable-extension pi-extension-name --trust-extension pi-extension-name
```

Bridge profile `0.2.0` targets exactly
`@earendil-works/pi-coding-agent@0.84.4` and Node 22.19 or newer. The bridge
validates that profile before importing extension code instead of silently using
a newer runtime found on `PATH`. Pass `ygg pi install --pi-package DIR ...` when
the package is not in a conventional location; the generated inert link records
and forwards that exact path across Ygg's sanitized subprocess environment.
The exhaustive per-event/API/UI ledger and completion gates are maintained in
[`extensions/ygg-pi-compat/COMPATIBILITY.md`](../extensions/ygg-pi-compat/COMPATIBILITY.md).
It supports Pi tools, transformed result details/error/usage, live tool catalogs,
notifications, confirmations, text input, basic lifecycle/context events, and
local Pi event-bus behavior. On a Ygg host negotiating `runtime_commands`, Pi's
initial command catalog is exposed under its native slash names; the generated
`/<name> COMMAND ...` route remains only as a fallback for older hosts.
Unsupported TUI, provider, session, compaction, agent-control, and mutation
surfaces remain explicit migration diagnostics rather than silent no-ops.

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
| `bridge` | The extension uses only surfaces implemented by the pinned compatibility process. A generated link still needs a successful runtime handshake before it is known compatible. |
| `native_port` | The extension uses a known Pi 0.84.4 mutation or registration that needs an explicit Ygg-native port or a future bounded host primitive. |
| `manual` | The extension depends on arbitrary Pi TUI/editor components, custom providers, or deep session/compaction internals; redesign is required. Pi JSON themes are also manual because Ygg themes use a different semantic schema. |
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

## Safety and bounds

The scanner does not:

- execute or import a Pi extension;
- run an npm lifecycle script or install a missing package;
- trust and start a Ygg executable extension;
- send source, settings, or the report to a model or network service;
- copy, rewrite, or delete Pi or Ygg files; or
- read Pi authentication/model credential stores.

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

Universal Pi source compatibility is deliberately not the goal. Pi extensions
can mutate in-process agent, provider, session, and TUI state that Ygg keeps
behind a language-neutral subprocess boundary. Recreating that ABI in the Ygg
kernel would compromise the boundary rather than improve migration.

The intended staged system is:

```text
scanner/compiler
  + exact package recipes
  + one persistent Pi compatibility process
  + explicitly selected agentic fallback
```

### Deterministic scanner/compiler

The shipped dry run is the inventory front end for this stage. `ygg pi install`
now creates an inert generated wrapper for an existing local source; it does not
install dependencies or execute package code. Future recipes can copy compatible
skills/prompts, transform known configuration, and cache intermediate results by
source and lock hash. Those operations should remain deterministic and
model-free.

### Compatibility process

The generated `ygg pi install` link hosts a deliberately bounded subset of Pi's
`ExtensionAPI` through the persistent `ygg-pi-compat` process. Repeated `--with`
arguments record an ordered, source-fingerprinted set in one aggregate lock and
load those sources through one real `ExtensionRunner`, preserving their local
event bus, `globalThis`, and shared registries. Automatically selecting and
locking an entire reviewed Pi setup remains future migration work.

Unsupported Pi APIs must raise a clear compatibility error. The bridge must not
silently discard a policy, mutation, lifecycle, or UI call. Static and runtime
tools use API `0.2` live tool catalogs. Negotiated `runtime_commands` makes the
command set discovered during Pi initialization authoritative without requiring
those names in the generated manifest; command registration after initialization
still needs a live command-catalog protocol.

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
request is made. Arbitrary TUI frontends and custom provider transports should
report manual redesign rather than trigger an unbounded automatic port.

## Deliberate compatibility boundary

Capability migration is often practical even when exact UX is not:

- basic tools, commands, notifications, and local event-bus behavior are strong
  bridge candidates;
- MCP, search, browser, LSP, memory, and subagent behavior belong in replaceable
  Ygg extension processes, not the kernel;
- Pi input transforms, safe tool-argument replacement with host revalidation,
  pre-persistence tool-result transforms, extension-scoped durable state, and
  per-turn tool-policy overlays are evidence for possible narrow future APIs;
- custom provider/OAuth/stream handlers, mutable session-tree/compaction hooks,
  and arbitrary editor/header/footer/widget components are not transparent
  bridge targets; and
- Ygg should evolve semantic, frontend-neutral UI contributions rather than an
  arbitrary component ABI.

None of those possible protocol additions is implied by the current dry-run
command. They should be introduced only with a concrete migrated package,
wire-level tests, bounded failure semantics, and no cost on the no-extension
path.

## Product promise

The intended promise is:

> Ygg can inspect a Pi setup, migrate portable resources without model tokens,
> replace known infrastructure with exact Ygg-native recipes, run a bounded
> compatible subset through an explicitly trusted bridge, and identify exactly
> what still requires a port.

Today the zero-token scanner and explicitly trusted, pinned compatibility links
are implemented. The bridge runs a tested subset of Pi 0.84.4 tools, commands,
dialogs, context, and lifecycle behavior; explicit ordered source sets can share
one locked runtime. Deterministic resource apply, exact replacement recipes,
automatic whole-setup selection, session/provider mutation, and arbitrary Pi
component parity remain unfinished and are reported rather than silently
emulated.
