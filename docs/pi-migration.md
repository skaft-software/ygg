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

The scanner invocation is always a dry run; `--dry-run` makes that intent
explicit. It exits before normal Ygg configuration, provider discovery, session
startup, extension startup, or model bootstrap. It therefore consumes zero
model tokens.

## Import portable setup data

A separate, opt-in command imports the portable subset of a Pi setup. It does
not change the existing scanner behavior:

```console
ygg migrate import pi --dry-run
ygg migrate import pi --source /path/to/pi/agent --dry-run --json
ygg migrate import pi --source /path/to/pi/agent
ygg migrate import pi --source /path/to/pi/agent --yes
```

Without `--source`, the importer checks `PI_CODING_AGENT_DIR`, then the standard
Pi agent locations. It runs Ygg's built-in, read-only API 0.3 adapter rather
than executing Pi packages or a user-selected adapter command. The host owns
all destination decisions and writes.

The importer can select a model already known to Ygg, copy portable skills into
`~/.ygg/skills/`, and add local stdio MCP declarations to `~/.ygg/mcp.json`.
A Pi provider/API-model pair is selected only when it has exactly one match in
Ygg's built-in catalog; Ygg persists that catalog entry's canonical ID. Custom,
unknown, and ambiguous provider/model values are skipped rather than guessed.
Every imported skill is wrapped in host-authored frontmatter with
`disable-model-invocation: true`; every imported MCP server has `enabled: false`
and `required: false`. Review and explicitly enable either resource only after
inspecting it.

Credentials, MCP environment values, headers, working directories, and Pi
permission decisions are never copied. Unsupported models and transports are
reported as skipped; model skips include bounded details in the text report and
JSON `model_diagnostics`. The command never writes the Pi source setup, contacts
a network service, starts an imported MCP server, starts an extension, or invokes
a model.

Imports track the hashes they own in `~/.ygg/migrations/pi-state.json`. A
changed destination is a conflict and requires an interactive confirmation or
`--yes`; `--dry-run` performs the same validation without writing anything.
Before an import changes a destination, it creates a private backup under
`~/.ygg/backups/migrate/` and prints its path. Restore it only when the current
destination still matches the import:

```console
ygg migrate restore ~/.ygg/backups/migrate/IMPORT-DIRECTORY
# Explicitly overwrite a destination changed after import:
ygg migrate restore ~/.ygg/backups/migrate/IMPORT-DIRECTORY --yes
```

## Plan, preflight, and publish a compatible extension

Once a local Pi extension or installed Pi package has been reviewed, compile an
inert aggregate plan. `--with` is ordered: the first source loads first and all
sources share one Pi process, event bus, `globalThis`, and registry set.

```console
ygg pi plan ./first.ts --with ./second-package --with ./third.ts \
  --name pi-compat-0-84-4 --pi-package /reviewed/pi-coding-agent \
  --output /private/review/pi-aggregate-plan.json
ygg pi preflight --plan /private/review/pi-aggregate-plan.json
ygg pi publish --plan /private/review/pi-aggregate-plan.json
ygg pi list
```

`ygg pi install ...` remains a shorthand for compile, preflight, and publish in
one local command. It is useful for a reviewed one-off source; the explicit
three-step form leaves an auditable handoff between review and publication.
`--output` requires an existing non-symlink parent and a new file, so a plan is
never silently replaced. Without `--output`, stdout is only canonical JSON (the
inertness note is written to stderr), so it can be redirected into a plan file.
Compilation requires exactly
`@earendil-works/pi-coding-agent@0.84.4`, either selected with `--pi-package`
or found by the bounded local discovery rules. It never downloads, installs, or
executes a package. Prefer `--pi-package` in automation so the selected runtime
is unambiguous.

The canonical plan pins, in order:

- every canonical source path, bounded source SHA-256, and supported adjacent
  dependency-lock SHA-256 (`package-lock.json`, npm shrinkwrap, pnpm, Yarn, or
  Bun lock files);
- the canonical Pi package root and a package-integrity SHA-256 over its exact
  `package.json` bytes and reviewed `dist/` tree;
- the bridge, Pi, and Ygg versions, the `pi_aggregate` lifecycle profile, and
  the explicit-enable/explicit-trust requirement.

`preflight` re-reads all of those inputs without importing a source. `publish`
runs that same preflight immediately before creating a discoverable package and
rolls back a partial package on write failure. A changed source, lock, package,
plan digest, or selected runtime is rejected with a replacement-plan action.
Generated schema-v3 link records and aggregate-lock schema-v2 records bind the
source order, package integrity, manifest path, and explicit trust requirement
through a link identity. The bridge checks those values before and after its Pi
loader imports source, and rejects a startup whose source/runtime changed during
that interval.

The generated wrapper lives under `~/.ygg/extensions/`, points at existing
sources, and does not install npm dependencies, run lifecycle scripts, copy the
Pi package, or enable/trust itself. It remains disabled and untrusted until the
user makes both decisions:

```console
ygg --enable-extension pi-extension-name --trust-extension pi-extension-name
```

`ygg pi list` reports metadata freshness only; it deliberately does **not** claim
that the user has enabled or trusted a link. To remove a generated link from
discovery without deleting the reviewed package, use the reversible local
rollback action:

```console
ygg pi rollback pi-extension-name
```

The command moves only a validated generated package into a private rollback
directory beside the extension root and leaves Ygg's enable/trust policy intact.
Review its records before manually restoring it.

Bridge profile `0.3.0` targets exactly Pi `0.84.4` and Node 22.19 or newer. Its
live Pi protocol remains API `0.2`: API `0.3` currently has no available
lifecycle-event or dynamic-command surface for this bridge. Publication also
writes a canonical `pi-runtime-evidence.json` sidecar using the generated API
`0.3` canonical JSON helper. That small, static selection/evidence seam is for
the future runtime manager; it is **not** a claim that Pi lifecycle behavior has
been upgraded to API `0.3`.

The exhaustive per-event/API/UI ledger and completion gates are maintained in
[`extensions/ygg-pi-compat/COMPATIBILITY.md`](../extensions/ygg-pi-compat/COMPATIBILITY.md).
Its canonical machine-readable form is
[`0.84.4.ledger.json`](../extensions/ygg-pi-compat/profiles/0.84.4.ledger.json).
`python3 extensions/ygg-pi-compat/conformance.py --check --json` validates its
118 public surfaces, 78 official examples, 33 TUI audit rows, six plan-mode
journeys, fixture links, and profile digest without claiming that a real Pi
package was run. The separate full gate accepts only local integrity-verified
tarballs, a clean pinned Pi checkout, a fresh allowlisted environment, and Linux
network isolation.

It supports Pi tools, transformed result details/error/usage, live tool catalogs,
notifications, confirmations, text input, basic lifecycle/context events, and
local Pi event-bus behavior. On a Ygg host negotiating `runtime_commands`, Pi's
initial command catalog is exposed under its native slash names; the generated
`/<name> COMMAND ...` route remains only as a fallback for older hosts.
Unsupported TUI, provider, session, compaction, agent-control, and mutation
surfaces remain explicit migration diagnostics rather than silent no-ops. Pi
`registerFlag` is also diagnosed: its runtime registration cannot safely become
a Ygg API `0.3` manifest flag without running the source before trust and CLI
construction.

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

The shipped dry run is the inventory front end for this stage. `ygg pi plan`
compiles an inert source/lock/runtime-integrity aggregate; `preflight` verifies
it and `publish` creates the generated wrapper only after that verification.
`ygg pi install` is the one-command shorthand. None installs dependencies or
executes package code. Future recipes can copy compatible skills/prompts,
transform known configuration, and cache intermediate results by source and lock
hash. Those operations should remain deterministic and model-free.

### Compatibility process

A generated Pi aggregate hosts a deliberately bounded subset of Pi's
`ExtensionAPI` through one persistent `ygg-pi-compat` process. Ordered `--with`
sources are compiled into a source/lock/runtime-integrity plan, preflighted, and
published as one aggregate lock. The one real `ExtensionRunner` preserves their
local event bus, `globalThis`, and shared registries. Runtime-manager-owned lazy
activation, workspace sharing, and hot reload remain future work; the published
sidecar is only a narrow API `0.3` evidence seam for that manager.

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

Today the zero-token scanner, limited host-owned portable import, and explicitly
trusted, pinned compatibility links are implemented. The bridge runs a tested
subset of Pi 0.84.4 tools, commands, dialogs, context, and lifecycle behavior;
explicit ordered source sets can share one locked runtime. Exact replacement
recipes, automatic whole-setup selection, session/provider mutation, and
arbitrary Pi component parity remain unfinished and are reported rather than
silently emulated.
