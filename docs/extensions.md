# Executable extensions

> **Protocol reference:** [`docs/extensions/PROTOCOL-REFERENCE.md`](extensions/PROTOCOL-REFERENCE.md)
> contains the complete specification of every JSON-RPC method, request/response
> shape, type reference, and lifecycle timing.

Ygg supports trusted local extension processes alongside native Rust
`Extension` implementations. Process extensions use JSON-RPC 2.0 messages,
one compact JSON object per line, over stdin/stdout. They may be written in any
language that can read and write JSON lines.

## Kernel boundary

Ygg is a small agent kernel and JSON-RPC bus. The host owns only services that
an extension must already have in order to exist safely:

- starting, supervising, stopping, and force-killing extension process groups;
- transporting bounded JSON-RPC messages;
- running model conversations;
- persisting sessions, tool calls, and tool results;
- enforcing user permissions and approvals; and
- owning resource-limit policy for memory, messages, concurrent work,
  artifacts, and child processes.

For explicit capability ownership (search vs browser vs computer use, hosted vs in-harness delegated execution, trust propagation, and non-goals), see [`docs/design/extension-capability-and-orchestration-boundaries.md`](design/extension-capability-and-orchestration-boundaries.md).

Product capabilities are subprocess extensions. MCP bridging, web search,
browser use, computer use, memory, LSP, subagent orchestration, and caffeinate
do not belong in the host. A host service such as artifact ingestion or child
model-session creation is a generic kernel primitive; the extension still owns
the domain behavior and presents its tools to the model.

This is the architectural boundary, not a claim that first-party extensions
for every listed capability already ship. The implementation-status notes below
distinguish the working transport/kernel pieces from capability packages still
to be built or migrated. Current runtime enforcement covers message, queue,
request, concurrency, artifact, shutdown, and process-tree cleanup bounds. OS
CPU/RSS/FD/PID isolation is not implemented; trusted extensions still run with
the current user's operating-system authority.

For example, one lightweight `ygg-mcp` process can supervise any number of MCP
servers and publish their changing catalogs without teaching the Ygg host MCP:

```text
Ygg <- JSON-RPC -> ygg-mcp <- MCP -> Ableton MCP
```

The extra local hop is intentional. It preserves language neutrality,
replaceability, failure isolation, and a kernel that does not grow a special
manager for every external protocol.

Two exact manifest-selected protocol versions are implemented:

- API `0.1` is frozen for existing trusted, bounded, text-oriented
  extensions. Its initialization wire remains unchanged. It does not inherit
  API `0.2` cancellation, progress, structured/media retention, correlation,
  or terminal lifecycle guarantees; optional `metadata` is accepted but
  discarded by the native subprocess adapter.
- API `0.2` is the current stateful foundation. It negotiates cooperative
  cancellation and typed content as required features, plus scoped progress,
  artifacts, lifecycle observations, policy intents, and live tool catalogs as
  optional features. Bounded child model sessions, single-use approval
  capabilities, and owner-scoped secret lookup are offered conditionally;
  parent-correlated ephemeral input is part of the base `0.2` contract.

API `0.2` supplies the stateful transport foundation for trusted daily use
within those boundaries. It does not add an operating-system sandbox or move a
domain capability into the host.

Executable extensions are intentionally a local tinkerer feature. Capability
declarations are visible consent metadata, not an operating-system sandbox.
Discovery remains available under every effect policy, but process startup
requires all three independent gates: enablement, an exact trust grant, and the
default full-access policy. `--safe-mode` never starts an executable extension,
even when the process/shell sandbox flags are enabled, and reports that blocked
startup in `/extensions`. An admitted extension runs as the current user, so use
full-access mode only inside separate OS-level isolation.

## Layout and discovery

Each direct child directory contains one file named `extension.toml`:

```text
.ygg/extensions/git-tools/extension.toml
~/.ygg/extensions/git-tools/extension.toml
```

Precedence is global, then trusted project, then explicit directories in
command-line order; later definitions win by extension directory name. Project
extensions are ignored until the workspace is trusted. Discovery alone never
executes code: enablement and executable trust are independent, explicit
decisions bound to the selected manifest name and source.

The direct child directory name must exactly match `name` in its manifest.
This makes the shared resolver's later-wins precedence authoritative for both
discovery and trust; aliases are rejected with a diagnostic.

Use repeatable command-line options for one-off tinkering:

```console
ygg \
    --extension-dir ./my-extensions \
    --enable-extension hello-world \
    --trust-extension hello-world
```

Or persist activation in the user config:

```toml
# Unsafe: the default full-access mode is intended only inside separate
# OS-level isolation. Use --safe-mode when approval is required.
enabled_extensions = ["hello-world"]
trusted_extensions = ["hello-world"]
```

A bare persistent trust name applies only to the matching extension under
`~/.ygg/extensions`. It never transfers to a same-named project or explicit
extension. Persist trust for either of those sources with its exact absolute
manifest path:

```toml
enabled_extensions = ["git-tools"]
trusted_extensions = [
  "git-tools@/absolute/project/.ygg/extensions/git-tools/extension.toml",
]
```

`--trust-extension git-tools` is deliberately different: it trusts the
currently selected `git-tools` source for this process invocation only and is
never written back as a persistent name grant.

A trusted project config may suggest `enabled_extensions`, but it cannot grant
itself executable trust. The default full-access policy permits a fully enabled
and trusted extension to start; `--safe-mode` keeps it stopped. Persistent trust
must come from the user config or environment (`YGG_TRUSTED_EXTENSIONS`);
one-shot trust comes from `--trust-extension`.

The agent crate exposes both pieces of the boundary:

- `discover_extension_manifests` scans conventional direct-child layouts.
- `ExtensionCatalog::load_resolved` accepts already resolved manifest paths in
  authoritative precedence order, retaining diagnostics instead of making one
  bad extension disable the catalog.

Manifest reads are bounded. Selected files must be regular, non-symlink files;
malformed or shadowed resources produce inspectable diagnostics without
preventing the core binary from starting.

## Manifest

```toml
name = "git-tools"
version = "0.2.0"
api_version = "0.2"
description = "Small local git helpers"

[entrypoint]
command = "git-tools"
args = ["--stdio"]

[capabilities]
filesystem = "workspace" # none, workspace, or unrestricted
process = true
network = false
secrets = ["git.provider_token"] # exact logical names; empty by default

[contributes]
tools = ["git_status"]
commands = ["checkpoint"]
hooks = ["after_tool_call"]
ui = ["status"] # status, header, or footer
context = true
tool_renderers = ["git_status"]
notifications = true
confirmations = true
```

Bare entrypoint commands are first resolved beside the manifest, then through
`PATH`. Arguments are passed directly without a shell. The child working
directory is the active workspace. Ygg supplies `YGG_EXTENSION_API_VERSION`,
`YGG_EXTENSION_NAME`, `YGG_EXTENSION_DIR`, `YGG_EXTENSION_MANIFEST`, and
`YGG_WORKSPACE`. Every generation also receives `YGG_EXTENSION_SCRATCH` for
host-verified artifact publication. To keep an existing extension on the
frozen wire, leave `api_version = "0.1"` in its manifest.

`[capabilities].secrets` is a duplicate-free allowlist, not a request to copy
credentials into the launch environment. Each name is at most 64 ASCII bytes,
starts with a letter or underscore, and thereafter uses letters, digits,
underscore, hyphen, or dot. A non-empty list makes `secrets` eligible for
negotiation only when the host also configured a broker; it never makes an
undeclared name readable.

## Transport contract

Stdout is protocol-only. Human diagnostics belong on stderr, which Ygg drains
and exposes as bounded diagnostic events. The default maximum JSON line is 1
MiB, the default in-flight request cap is 64, and ordinary requests time out
after 30 seconds. A bounded dedicated writer serializes complete frames; API
`0.2` then schedules requests behind the negotiated concurrency limit.

Every request and response uses the standard JSON-RPC envelope:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":1,"result":{}}
{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid params"}}
```

The initial host request is always `initialize`. Its parameters include API and
Ygg versions, extension identity and source, the workspace, capability and
contribution declarations, and inspectable session/model/reasoning/active-skill
state. The response must use the same API version and provide complete schemas
for exactly the tools and commands declared in the manifest. API `0.2` adds
this exact negotiation while keeping those top-level fields:

```json
{
  "api_version": "0.2",
  "tools": [{
    "name": "git_status",
    "description": "Show compact workspace status",
    "parameters": {"type":"object","properties":{}},
    "output_schema": {"type":"object","properties":{"branch":{"type":"string"}},"required":["branch"]}
  }],
  "commands": [{
    "name": "checkpoint",
    "description": "Record a local checkpoint",
    "usage": "/checkpoint [label]"
  }],
  "protocol": {
    "version": "0.2",
    "features": [
      "request_cancellation",
      "content_parts",
      "request_progress",
      "artifacts",
      "lifecycle_events",
      "dynamic_tools"
    ],
    "limits": {"max_concurrent_requests": 4},
    "lifecycle_events": ["turn/started", "turn/settled"]
  }
}
```

The host request names required `request_cancellation` and `content_parts` and
optional `request_progress`, `artifacts`, `lifecycle_events`, `policy_intents`,
and `dynamic_tools`. It conditionally appends `agent_sessions`, `approvals`, and
`secrets` only when the corresponding host service is configured;
`approvals` also requires negotiated `policy_intents`, while `secrets` requires
a configured broker and a non-empty manifest allowlist. Missing required,
unknown, or duplicate response features, an API/version mismatch, or a zero
concurrency limit rejects the process. The host caps the accepted limit.
Omitting the lifecycle subscription list while negotiating that feature
subscribes to all six session/turn/tool start/settle notifications. API `0.1`
omits `protocol` entirely and forbids `output_schema`.

When bounded V2 delegation is enabled for the selected model/reasoning mode,
the working-tree host also offers `agent_sessions`. It is not advertised when
that service is unavailable, and an extension must not return a feature the
host did not offer.

Host-to-extension methods are typed in
`ygg_agent::extension_process::methods`:

| Method | Result |
| --- | --- |
| `tool/call` | API `0.1`: string `content`; API `0.2`: typed `content` parts plus validated `structured_content`, `is_error`, and retained metadata |
| `command/execute` | `{text, notifications, context}` |
| `hook/run` | `{disposition, context, notifications}` |
| `context/collect` | array of `{label, content, placement}` |
| `status/collect` | a semantic status contribution or `null` |
| `tool/render` | ordered `{segments: [{text, style_role}]}` |
| `shutdown` | any JSON result, followed by process exit |

API `0.2` tool results require a compact text part. Image parts use optional
`alt`; audio parts use optional `transcript`. Media parts require a verified,
same-owner, same-generation artifact and exact MIME match.
`structured_content` is required exactly when the tool declared
`output_schema` and is retained with bounded metadata as non-model-visible
native details.

Extensions may send these process-to-host messages at any time after
initialization:

| Method | Envelope |
| --- | --- |
| `notification` | notification; no `id` |
| `confirmation/request` | request with string or numeric `id`; Ygg answers that `id` |
| `context/contribution` | unsolicited context notification |
| `status/contribution` | unsolicited semantic TUI notification |
| `$/progress` | API `0.2` notification with `{request_id, sequence, event}` |
| `input/request` | API `0.2` correlated ephemeral text/secret input request |
| `artifact/publish` | API `0.2` correlated request; inline base64 or relative scratch path |
| `policy/evaluate` | API `0.2` correlated structured action intent |
| `secret/get` | API `0.2` correlated lookup of one manifest-allowlisted logical secret |
| `tools/register` | API `0.2` uncorrelated request that transactionally adds or replaces live tool definitions |
| `tools/unregister` | API `0.2` uncorrelated request that transactionally removes live tool names |
| `agent/spawn` | API `0.2` correlated request for one bounded child model session |
| `agent/message` | API `0.2` correlated steering message to an owned child |
| `agent/follow_up` | API `0.2` correlated follow-up run for an owned child |
| `agent/list` | API `0.2` correlated snapshot of owned child-session trees |
| `agent/wait` | API `0.2` correlated bounded wait for owned child state |
| `agent/interrupt` | API `0.2` correlated interrupt of an owned child tree |

### Live tool catalogs

An API `0.2` extension that negotiates `dynamic_tools` may change its tool
catalog without restarting Ygg. Initialization still returns exactly the tools
declared in `extension.toml`; dynamic mutations begin only after initialization
and host registration are complete. That initialize response is authoritative
catalog epoch `0` and is the only deterministic catalog for the first model
request. If a tool must be available on turn one, declare it in the manifest
and return it from `initialize`.

`tools/register` accepts `{ "tools": [ToolDefinition, ...] }` and adds or
replaces the named tools. `tools/unregister` accepts `{ "names": [string, ...]
}`; names that are already absent are harmless. Requests and complete catalogs
are capped at 256 tools. A successful response is:

```json
{"revision":1,"tools":["existing_tool","new_tool"]}
```

`revision` is a monotonic per-process catalog epoch. It starts at `0` after
initialization or reload and increments once for every accepted mutation. The
returned names are the complete catalog the host actually published, after
host tool policy is applied; an extension must not assume every requested name
was accepted.

Publication is transactional. Ygg validates the complete prospective catalog,
reserves names against core tools and other extensions, applies policy, and
only then swaps the extension-owned group. With a parseable request ID,
conflicts or malformed parameters return `-32602` and leave the previous catalog
visible. If Ygg cannot deliver an acknowledgement after publication, it removes
the group and terminates that process generation instead of allowing host and
child catalogs to diverge.

Each model request uses one frozen schema-and-implementation snapshot. An
accepted post-initialize mutation becomes visible at the next model-request
boundary after publication; it never changes the tools halfway through a
provider request. Ygg does not guess when a provider has finished its startup
registrations or wait for an implicit catalog-quiescence period, so a mutation
sent immediately after `initialize` is not guaranteed to enter the first
request's snapshot. Reload follows the same rule: the replacement's initialize
catalog is epoch `0`, and its later mutations appear at subsequent request
boundaries. `tool/call` includes the snapshot's `catalog_revision`, so a call
emitted by an older in-flight model turn reaches the handler version the model
saw even if the extension has since replaced or removed that tool. Extensions
should retain bounded recent catalog snapshots; the Python SDK retains eight
and rejects an unknown or retired revision with `-32602`.

### Child model-session service

The working-tree `agent_sessions` feature is the narrow reverse service a
subagent-orchestrator extension needs. Every request includes the active
`parent_request_id`; Ygg derives the durable resource owner from that host tool
call rather than accepting one from child parameters. Calls without a bound
owner/service fail with `-32002`.

`agent/spawn` takes `{parent_request_id, task_name, message,
idempotency_key}`. The key is scoped to the extension principal and resource
owner: retrying the same input returns the same child, while reuse with different
input fails. Malformed parameters with a parseable request ID return `-32602`;
service, ownership, delegation-limit, persistence, and operation failures return
`-32002`. Message, follow-up, list, wait, and interrupt accept only IDs or paths
in that principal's owned child trees. `agent/wait` defaults to 30 seconds and
is capped at 60 seconds. Parent settlement cancels outstanding reverse requests;
extension shutdown stops its owned child trees. The service deliberately keys
trees by extension principal plus the durable session owner, not by process
generation: a supervised restart or reload can resume the same trees, while a
full process-host rebuild creates a new service boundary.

The kernel owns the actual model conversations, persistence, permission
inheritance, and team resource limits. The extension owns orchestration policy.
Hosted-agent services remain separate: `agent_sessions` creates in-harness Ygg
children, not remote hosted agents. Child state is observed through
`agent/list` and `agent/wait`: delegated child turns do not currently fan out
as extension `session/*` or `turn/*` lifecycle notifications, which remain the
owning/root product-session stream.

### Optional kernel services

The API keeps generic brokers separate from capability protocols:

- `artifact/publish` returns verified media IDs bound to the active
  host-derived session owner and process generation. Publication without that
  owner fails with `-32002`; resolution from another owner or generation sees
  an unavailable artifact.
- `policy/evaluate` carries a host-classified intent. When the conditional
  `approvals` feature is offered and negotiated, an approved `ask` returns a
  short-lived opaque token. The extension must retry the same intent under the
  same still-active owner request with that token. Redemption atomically
  consumes it; expiry, reuse, a different intent, generation, or parent all
  return `deny` and invalidate any presented live token. Tokens are valid only
  on an `ask` response; `allow` and `deny` omit them. `approvals` is not implied
  by `policy_intents`.
- `secret/get` is available only when the conditional `secrets` feature was
  offered and negotiated. The manifest must declare every exact logical name
  in `[capabilities].secrets`; the host passes the extension principal, full
  host-derived resource-owner triple, active parent request, and name to its
  broker. An absent active owner/service fails with `-32002`, undeclared names
  fail with `-32602`, and a broker returning no value or failing is collapsed
  to the generic `-32004` `secret is unavailable` response.

The coding product currently constructs extension runtimes with approvals off,
no secret broker, and a policy supervisor that answers generic intents with
`deny`. It therefore does not offer `approvals` or `secrets` even though the
API `0.2` host services and Python SDK helpers are implemented.

These are additive host services, not reasons to move browser, MCP, search, or
another domain into the kernel.

API `0.2` confirmation, input, artifact, policy, secret, and agent-session
requests include `parent_request_id` for an active host request. Parent
settlement cancels every unresolved child. Progress sequence numbers are
strictly monotonic per parent; inactive or stale events are ignored and
progress remains ephemeral. Artifact ingestion verifies size, SHA-256,
supported MIME signature, path containment, owner, and generation before
returning an opaque ID.

API `0.2` model-tool and tool-hook contexts also carry a host-derived
`resource_owner`:

```json
{
  "session_id": "durable-session-owner",
  "extension_instance_id": "host-created-instance-fence",
  "process_generation": 3
}
```

Extensions must namespace browser tabs, MCP connections, LSP documents,
memory handles, and comparable state by this three-part owner rather than
trusting a model-supplied identifier. `session_id` is a SHA-256-derived
namespace that remains stable when the same persisted Ygg session is reopened
at the same canonical path. `extension_instance_id` changes when the process
host is rebuilt, including when generation numbering starts over, while
`process_generation` fences stale handles after an extension reload or
automatic restart within one host instance. Frozen API `0.1` omits the field.
Non-tool contribution requests do not currently carry an operation owner and
must not create session-owned resource handles.

`input/request` sends `{parent_request_id, prompt, secret}` and receives
`{value: string|null}`. Prompts are non-whitespace and at most 16 KiB UTF-8;
answers are at most 256 KiB. `null` is cancellation. Secret input uses the
private frontend reply channel and never enters diagnostics, progress, session
state, or persistence; headless or unavailable input is cancelled.

`policy/evaluate` answers `{decision: "allow"|"ask"|"deny",
approval_token?: string}`. Hints cannot lower host policy. A token is a
64-character lowercase hexadecimal, single-use retry capability, not a durable
permission: it is bound to the canonical original intent, process generation,
active parent/owner, and bounded expiry. `confirmation/request` remains
cooperative UI rather than an enforcement boundary.

`secret/get` sends `{parent_request_id, name}` and receives `{value: string}`.
Names are exact, duplicate-free manifest identifiers of at most 64 ASCII bytes;
values are UTF-8 and capped at 64 KiB. The host does not persist or log values
and best-effort wipes its broker value and serialized writer buffers after
delivery. The extension still receives an ordinary process-memory string, so
this is not end-to-end zeroization; it must keep the value short-lived and out
of results, progress, diagnostics, and storage.

All TUI contributions contain plain text and optional semantic style roles.
Raw terminal escape sequences are not part of the extension API.
Tool-renderer segments are accepted and retained as internal extension
provenance, but are never rendered in the TUI or exposed through Ctrl+O,
`/verbose`, transcript selection, or copy. The original tool result
remains immutable evidence for the agent's required protocol result,
session persistence, and export redaction policy; it is not a presentation
surface. Extension header, status, footer, notification, and confirmation
features remain separate Ygg UI surfaces.

In the interactive frontend, confirmation requests made while an extension
tool or command is running open a typed allow/deny panel. Dropping the request
or using a non-interactive frontend denies it. Requests that arrive outside an
active confirmation boundary are also denied; they are never implicitly
accepted.

API `0.2` cancellation is cooperative and request-scoped. Before the writer
starts a queued frame, cancellation skips it; once a write starts, the frame
finishes and the host sends at most one `$/cancelRequest` with the original ID.
The SDK exposes an ambient token and returns JSON-RPC `-32800` when cooperative
cancellation wins. The host drops the waiter, tombstones late replies without
damaging unrelated calls, cancels correlated child requests, and terminates a
generation that remains non-cooperative after the bounded grace period.
Cancellation never claims rollback and unsafe ambiguous work is not replayed.

Use `/extensions` to inspect discovered, enabled, trusted, and running state.
Each entry includes the selected manifest path. An enabled-but-untrusted entry
reports the exact copyable persistent grant as well as the one-shot CLI form.
Use `/extensions reload` to replace each running process after a successful
handshake. The general `/reload` command re-runs resource discovery and rebuilds
the product boundary.

## Python SDK

Ygg ships a dependency-free Python package for extensions that use the stdio
protocol. Install it from a checkout before copying an example or building an
extension:

```console
python3 -m pip install ./sdk/python
```

The package is named `ygg-extension-sdk` and exposes `ygg_extension.Extension`.
Decorate tools and commands with their manifest metadata, then call
`Extension.run()`; the SDK owns JSON-RPC 2.0 JSON-lines framing, one serialized
stdout writer, bounded concurrent dispatch, API negotiation, ambient
cancellation, scoped progress, ephemeral input, correlated host requests, and
bounded shutdown/drain. It keeps structured logs on stderr and exits on
shutdown or stdin close. API `0.1` manifests keep their legacy wire behavior.

```python
from ygg_extension import Extension

ext = Extension(api_version="0.2")

@ext.tool(
    name="hello_world",
    description="Greet someone",
    output_schema={"type": "object", "properties": {"greeting": {"type": "string"}}, "required": ["greeting"]},
)
def hello(args):
    return {
        "content": [{"type": "text", "text": "Hello!"}],
        "structured_content": {"greeting": "Hello!"},
    }

ext.run()
```

Bootstrap tool and command names must exactly match the manifest. Tools
published after initialization through negotiated `dynamic_tools` need not be
manifest entries. The SDK also provides decorators for hooks, prompt context,
status surfaces, and tool renderers, plus `notify()` and correlated `confirm()`
helpers for process-to-host messages.
See [`sdk/python/README.md`](../sdk/python/README.md) for the complete API and
the three runnable examples below.

## Lifecycle and reload

`ExtensionProcess` implements the existing native `Extension` trait, so its
negotiated tools register through `ExtensionHost` and retain the agent's normal
duplicate detection and non-replayable safety default. Product frontends call
the typed command, hook, context, status, renderer, notification, confirmation,
and API `0.2` lifecycle APIs at their corresponding semantic boundaries.

The optional lifecycle feature subscribes to exact `session/started`,
`session/settled`, `turn/started`, `turn/settled`, `tool/started`, and
`tool/settled` method names. Settled outcomes cover completion, failure,
cancellation, interruption, frontend disconnection, shutdown, and limits. The
shared interactive/plain/print/RPC/native-host/Serve terminal boundary settles
each admitted turn. Notifications are best effort; host cleanup and persistence
remain authoritative. Frozen API `0.1` keeps `after_response` success-only.

For an admitted, running full-access extension, reload starts and fully
initializes a replacement while the existing process remains ready. Launch,
handshake, or contribution mismatch leaves the existing process active. A
process negotiating `dynamic_tools` may replace its tool catalog during reload;
static tool catalogs and all command/hook/UI
contributions must remain compatible or return a clear "re-registration
required" error so the frontend can rebuild intentionally.
On an accepted reload, the old generation stops admission, drains to a bounded
deadline, cancels the remainder, emits terminal lifecycle observations, and
reaches its shutdown acknowledgement or timeout. Ygg then seeds replacement
lifecycle state and atomically cuts new calls over to it. Pending
confirmations, progress, artifacts, approvals, secret lookups, and other child
operations cannot cross generations.

Shutdown is requested gracefully and bounded by a short timeout. A process
that does not exit is killed, and dropping the last runtime handle also uses
kill-on-drop cleanup. `/extensions` shows the process generation, API,
negotiated features, pending count, health, and last bounded error.

The supervisor watches every successfully initialized resident process. On an
unexpected exit or terminal transport failure it removes the dead tool group,
enters `backoff`, and attempts candidate-first reload with full-jitter
exponential delay (250 ms base, 30 s cap). It parks after eight failed restarts
or a permanent manifest/version/re-registration error; 30 seconds continuously
ready resets the retry budget. Shutdown cancels supervision, manual reload and
supervisor cutover share one generation-checked lock, and unsafe interrupted
calls are not replayed. Initial launch/handshake failures remain parked
discovery entries; supervision begins only after one successful initialization.
There is no independent heartbeat for an otherwise live process. A full product
rebuild creates a new extension instance and supervisor, resetting in-memory
backoff/parked history rather than resuming the old watcher.

Sleep inhibition is a capability, not a kernel prerequisite. Ygg therefore has
no core sleep inhibitor. The `caffeinate` example is a supervised API `0.2`,
version `0.2.0` extension: it reference-counts owning/root `turn/started` and
`turn/settled` observations, clears remaining state on `session/settled`, and
runs one bounded macOS `/usr/bin/caffeinate` helper. Extension shutdown and host
process-group cleanup provide the final crash/shutdown fence.

The SDK-backed examples remain small and copyable:

- [`hello-world`](../examples/extensions/hello-world) demonstrates the minimum
  process and protocol handshake.
- [`caffeinate`](../examples/extensions/caffeinate) demonstrates API `0.2`
  terminal lifecycle ownership, overlapping-turn reference counting, bounded
  macOS sleep inhibition, status contribution, and explicit shutdown cleanup.
- [`git-tools`](../examples/extensions/git-tools) contributes a bounded custom
  tool, command, and semantic renderer.
- [`local-model-workflow`](../examples/extensions/local-model-workflow)
  demonstrates prompt hooks, deterministic context, status, and notifications.

## Capability boundaries

These extension families are deliberately separate even when a product may
compose them:

- **Web search** retrieves and ranks web results. It does not own an interactive
  tab or gain browser-control authority.
- **Browser use** owns page/tab state and semantic web interaction inside its
  extension resource owner. It is not general operating-system control.
- **Computer use** drives the desktop or application UI and needs its own
  approval and observation boundary.
- **Hosted agents** are remote provider services reached by an extension. They
  are not Ygg child conversations.
- **In-harness subagents** are child Ygg model sessions created through the
  host's bounded agent-session service and orchestrated by an extension.

MCP, LSP, memory, and caffeinate remain separate extension domains too. Sharing
the JSON-RPC transport does not merge their permissions, resource ownership,
failure policy, or user-facing tool semantics.

## First-party application packages

Application packages are separate from the executable-extension protocol above.
They distribute a complete first-party application runtime rather than JSON-RPC
tools or hooks, use `package.toml` instead of `extension.toml`, and are never
loaded during ordinary agent startup. The `0.5.0` package manager supports
only the official `ygg-serve` package and local copies of that release archive.
It is intentionally not a general package registry.

```console
ygg extension install ygg-serve
ygg extension list
ygg extension update ygg-serve
ygg extension remove ygg-serve
ygg serve
```

Packages are installed under `~/.ygg/extensions/ygg-serve/`:

```text
package.toml
bin/ygg-serve-runtime
install.json
```

The manifest is schema-versioned and declares the package ID and version, an
exact required Ygg version, target triple, entrypoint arguments and SHA-256,
and loopback/process/workspace capabilities. Official installs download the
matching release archive and `SHA256SUMS` over HTTPS. Local archives use:

```console
ygg extension install --path ./ygg-serve-0.5.0-TARGET.tar.gz
```

Installation validates the bounded archive and embedded executable checksum,
rejects links and unexpected paths, and publishes the package with an atomic
same-filesystem rename. `ygg serve` revalidates compatibility and the executable
checksum before replacing the launcher process with the installed runtime. As a
first-party replacement Ygg process, that runtime inherits the launcher's
configuration and provider environment; the sanitized child environment used
for model-controlled tools and executable extensions does not apply. Removal
deletes only package files; sessions, project metadata, and other user data
remain outside the package directory.
