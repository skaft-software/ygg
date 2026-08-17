# Executable Extensions Spike: Daily-Driver Capabilities

> **Status:** research evidence retained; architecture superseded by the
> tiny-kernel decision below
> **Observed:** 2026-08-16
> **Ygg target:** workspace version `0.4.0`, repository rooted at
> `84c2fb8b654b107e869ed9b8add29b3a50043e60`
> **Capabilities:** WebSearch, BrowserUse, Caffeinate, Subagents, MCP,
> Language Server Protocol (LSP), and Memory

## Executive decision

This section supersedes the original accepted recommendation in this spike.
The cross-product evidence remains useful, but the former proposal for
host-owned WebSearch, Browser, MCP, LSP, memory, delegation, and caffeinate
managers is no longer Ygg's architecture.

The central architectural conclusion is:

> **Ygg is a tiny agent kernel. Everything interesting is a subprocess
> extension speaking JSON-RPC.**

Recommended ownership:

| Ygg host/kernel owns | Subprocess extensions own |
| --- | --- |
| model conversations and bounded child model sessions | MCP bridging and MCP server lifecycle |
| JSON-RPC transport and process supervision/cleanup | web search and result/citation behavior |
| session, tool-call, and tool-result persistence | browser tabs, contexts, and interaction semantics |
| user permission and approval enforcement | computer-use observation and actions |
| generic secrets and artifact brokers | memory retrieval, consolidation, and storage policy |
| memory/message/concurrency/process limits | LSP clients, documents, and diagnostics |
| stable session/process resource ownership | subagent orchestration over host-created sessions |
| | caffeinate/sleep-inhibition behavior |

The host may expose generic services—child model sessions, secrets, artifacts,
and approvals—because extensions cannot bootstrap or enforce those services
themselves. A generic service is not a domain manager. `ygg-mcp`, for example,
is one long-lived Rust extension that speaks JSON-RPC to Ygg, speaks MCP to its
servers, and publishes live tools with `tools/register` and
`tools/unregister`.

Web search, browser use, computer use, hosted agents, and in-harness subagents
remain separate capabilities. Search is retrieval; browser use owns web-page
state; computer use controls the OS UI; hosted agents are remote provider
services; in-harness subagents are bounded child Ygg model sessions requested
through a host service.

### Implementation status (2026-08-16)

The current checkout implements the API `0.2` transport foundation:

- exact dual-version initialization, required `request_cancellation` and
  `content_parts`, optional `request_progress`, `artifacts`,
  `lifecycle_events`, `policy_intents`, and `dynamic_tools`, conditionally
  offered `agent_sessions`, `approvals`, and `secrets`, plus host-capped
  concurrency;
- a bounded serialized writer, cooperative cancellation, late-response
  tombstones, parent-correlated confirmation/input/artifact/policy/secret
  requests, ephemeral secret input, and request-scoped progress;
- typed text/image/audio results, output schemas, validated structured content,
  retained metadata, and owner-and-generation-scoped verified artifact
  publication and resolution;
- best-effort session/turn/tool lifecycle notifications backed by one shared
  terminal outcome boundary across every product frontend;
- structured policy intents, an optional original-intent/active-owner-bound
  single-use approval retry, and an optional manifest-allowlisted,
  owner-scoped secret broker. The coding product leaves approvals off,
  configures no secret broker, and defaults generic actions to `deny`;
- transactional post-initialize tool registration/removal, per-process catalog
  epochs, and schema/implementation snapshots frozen at model-request
  boundaries;
- host-derived resource owners combining durable session identity with
  process-host-instance and process-generation fences; and
- inspectable health, explicit drain, candidate-first atomic reload, and
  automatic post-initialization restart/backoff supervision.

Automatic supervision is implemented after one successful initialization; it
has no independent heartbeat, does not retry initial launch/handshake failure,
and its in-memory retry state resets with a full product rebuild.
Extension-to-host agent-session services remain working-tree functionality
until their product gates pass. Optional artifact, policy, approval, and secret
host services are implemented at the kernel boundary; the latter two remain
disabled/unconfigured in the coding product. The sleep-inhibitor migration is
complete: no core inhibitor remains, and the example is a supervised API `0.2`,
version `0.2.0` extension. Protocol/queue/artifact/process-tree bounds are
implemented; OS CPU/RSS/FD/PID quotas remain kernel work.

## Scope and evidence standard

This spike answers four questions:

1. What product behavior makes each capability useful every day?
2. Which process, state, security, and failure semantics are required?
3. Which semantics must Ygg own, and which are safely replaceable?
4. What must change in the executable-extension protocol first?

It does **not** propose a general extension marketplace, claim an OS sandbox for
trusted local code, standardize every provider-specific option, or reproduce
every feature found in the references.

Evidence labels used below:

- **OSS** — inspected open-source implementation at the cited commit.
- **PACKAGE** — inspected distributed schemas, manifests, or documentation.
- **CLI** — observed command help or command behavior.
- **STATIC** — inferred from symbols/strings/static analysis of a proprietary
  binary. This is evidence of packaged code or configuration, not source proof
  of every reachable runtime path.
- **NEGATIVE** — no comparable implementation was found in the searched
  snapshot; this is not proof that no implementation exists elsewhere.

Exact snapshots and source paths are catalogued in [Evidence](#evidence).
Claude Code and Google Antigravity are proprietary; all conclusions about their
internals remain explicitly qualified.

## Current Ygg baseline

### What API `0.1` already gets right

Ygg has a strong narrow base:

- JSON-RPC 2.0 over JSON Lines is language-neutral and easy to implement.
- Discovery, enablement, workspace trust, and executable trust are separate.
- Extension stdout is protocol-only; stderr is bounded diagnostic data.
- Child environments are cleared and rebuilt from a non-secret allowlist plus
  explicit manifest environment values.
- Capability declarations are honestly documented as consent metadata, not a
  sandbox.
- Manifest contributions are checked against the initialization response.
- Messages, pending requests, and ordinary request duration are bounded: 1 MiB,
  64, and 30 seconds by default.
- Reload initializes a candidate generation before atomically replacing the
  active connection, and stale confirmation generations are rejected.
- Child process groups receive graceful shutdown and forceful cleanup paths.
- Native tools already have bounded ephemeral progress, cooperative
  `CancellationToken`s, and `ToolOutput` media for image/audio URLs, inline
  bytes, and provider references.

The relevant implementation is primarily
`crates/ygg-agent/src/extension_process.rs`, `crates/ygg-agent/src/tool.rs`, and
`crates/ygg-coding-agent/src/extensions.rs`.

### API `0.1` limits that motivated `0.2`

| Gap | Observed API `0.1` behavior | Consequence |
| --- | --- | --- |
| Operation cancellation | The shared host transport safely drops and tombstones a cancelled waiter, but the frozen wire has no `$/cancelRequest` feature | API `0.1` work or side effects may continue after the host stops waiting |
| Framed-write cancellation | The current bounded serialized writer emits complete frames for both versions | Host-side frame safety is fixed without changing `0.1`, but only `0.2` can cooperatively cancel admitted work |
| Late replies | The host tombstones dropped request IDs and ignores their late replies | The connection stays healthy, but an API `0.1` child receives no cancellation acknowledgement |
| Correlated progress | Native progress exists, but extensions can only emit general notifications/status events | Concurrent calls cannot reliably attribute progress or prompts to the initiating operation |
| Result fidelity | `ToolCallOutput` accepts string `content`, `is_error`, and `metadata`; `ProcessTool` converts success to `ToolOutput::new(content)` | Metadata is discarded and executable extensions cannot return native image/audio media |
| Terminal lifecycle | Hooks are only `before_prompt`, `after_response`, `before_tool_call`, and `after_tool_call`; product paths call `after_response` after successful complete responses | Failure, cancellation, interruption, frontend loss, and shutdown are not terminal hook outcomes |
| Policy enforcement | An extension can request a generic confirmation, but it runs with the user's privileges and may bypass that request | Confirmation is cooperative UX, not a security boundary |
| Service health | The frozen wire has no health negotiation; the host supplies ready/degraded/crashed/drain state plus post-initialization backoff/parked supervision | Capability extensions share one inspectable host lifecycle without moving their domain managers into core |
| Reload drain | A candidate is initialized, then the old connection stops admission and receives a bounded host-side drain/shutdown before cutover; contribution changes are rejected | API `0.1` has no cooperative cancellation protocol, so unfinished child work cannot acknowledge drain, and schema changes require a larger rebuild |
| Process lifetime | API `0.1` has one resident contact policy: enabled, trusted processes start during product construction; the host supervisor replaces a generation after an unexpected exit | On-demand and per-call contact policies remain unexpressed, while crash restart stays a generic host concern |

The former `before_prompt`/`after_response` Caffeinate prototype exposed the
terminal-lifecycle gap: an aborted or failed run could leave its bounded helper
active without a matching success hook. The current API `0.2`, version `0.2.0`
extension is the replacement. It uses `turn/started`, `turn/settled`, and
`session/settled`, reference-counts overlapping owning/root turns, and cleans up
on extension shutdown. No sleep-inhibitor path remains in the kernel.

## Cross-product comparison

Cells summarize the inspected snapshot, not an evergreen product claim.

| Capability | Current Ygg | OpenAI Codex | Claude Code | Google Antigravity | Hermes Agent |
| --- | --- | --- | --- | --- | --- |
| **WebSearch** | No first-party search manager; API `0.2` can carry structured/media provider results but does not supply search policy, citations, or cache | Open-source `web.run` extension covers search/image search/open/click/find/screenshot and vertical data commands, with typed begin/end items and result payloads (**OSS**) | Packaged `WebSearch` and `WebFetch` schemas expose domain filtering, URL fetch, processed text, and structured hit URLs/titles (**PACKAGE**) | `NewSearchWebTool` and related symbols indicate an integrated search tool (**STATIC**) | Brave, DDGS, SearXNG, Exa, Parallel, Tavily, and Firecrawl adapters; extraction/cache limits, secret checks, DNS-aware SSRF checks, pinned-IP transport (**OSS**) |
| **BrowserUse** | No browser manager; API `0.2` bridges verified owner-and-generation-scoped screenshots/audio, but not browser sessions or action policy | Bundled Browser plugin `26.803.41515` documents persistent tabs/REPL handles, semantic DOM interaction, post-action checks, screenshots, scoped CDP, untrusted-page rules, and action-time confirmation (**PACKAGE**) | No equivalent native persistent browser was established; official marketplace distributes Playwright as an external MCP server (**PACKAGE**) | Browser tools and `BrowserSubagent` symbols indicate integrated browsing/subagent paths (**STATIC**) | Local/cloud providers, CDP and Browser Use, semantic accessibility snapshots, task-isolated persistent sessions, reaping, dialogs, frames/OOPIF, redaction, and network policy (**OSS**) |
| **Caffeinate** | API `0.2` version `0.2.0` extension: terminal lifecycle observations, overlapping-turn reference counting, bounded macOS helper, status, and shutdown cleanup; no core inhibitor | Core cross-platform `SleepInhibitor`: macOS IOKit assertion, Linux helper backends with parent-death handling, Windows power request, drop cleanup (**OSS**) | Binary strings indicate macOS `caffeinate`, Linux `systemd-inhibit`, restart/spawn-error/explicit-stop paths (**STATIC**) | No sleep-inhibitor symbols found in the inspected binary (**NEGATIVE/STATIC**) | No sleep-inhibitor implementation found in the inspected tree (**NEGATIVE**) |
| **Subagents** | V2 harness orchestration exists; the extension host-service bridge is working-tree implementation so orchestrators can remain subprocesses | Hierarchical registry, roles/paths, optional turn forking, follow-ups, messaging, waits, interrupts, shared depth/concurrency controls (**OSS**) | Packaged `Agent` schema and `claude agents` expose background agents, models/effort/permissions, addressable names, worktree/remote isolation, output and stop controls (**PACKAGE/CLI**) | Agent derivation, cancellation, workspace isolation, and subagent-management symbols are present (**STATIC**) | Isolated child conversations, summary return, parallel/background/nested agents, steering/stopping, limits, stalls/timeouts, worktrees, cost rollups, lifecycle plugins, durable async delivery (**OSS**) |
| **MCP** | No first-party bridge yet; `dynamic_tools` now supplies the catalog seam for a `ygg-mcp` extension | Stdio and Streamable HTTP, OAuth/config/env/headers, parallel/deferred startup, reusable connections, required/optional servers, cached revisioned catalogs, resources, cancellation, elicitation, approval policy (**OSS**) | `mcp add/get/list/remove/login/logout`, stdio/HTTP, headers/env, user/local/project scopes, project-config approval, health and OAuth login (**CLI/PACKAGE**) | MCP manager/call symbols plus tools/prompts/resources/progress-related symbols indicate broad support (**STATIC**) | Stdio, Streamable HTTP and SSE; reuse, keepalive, reconnect/backoff/parking/revival, pagination/refresh, tools/resources/prompts, sampling, elicitation, structured/media content, cancellation and cleanup (**OSS**) |
| **LSP** | No host LSP manager | No comparable native LSP subsystem found in the examined Rust sources (**NEGATIVE**) | Generated schemas and official plugins describe definition/references/hover/workspace/document symbols, server commands, language maps, timeouts, transport and diagnostics (**PACKAGE/STATIC**) | In-process `language_server/lsp/lsp.Serve` and related symbols indicate integrated language-server support (**STATIC**) | Lazy long-lived clients per server/workspace, background loop, git-root gating, document versions, push/pull diagnostics, baseline deltas, cancellation, idle reap and graceful shutdown (**OSS**) |
| **Memory** | Session/context primitives exist, but no scoped memory product or provenance/retrieval lifecycle | Asynchronous two-phase root-session extraction/consolidation, filesystem artifacts, citations/pruning/telemetry; replaceable `MemoriesBackend` tools, while core owns startup orchestration; stable feature default-disabled (**OSS**) | Packaged/static evidence identifies auto-memory settings, project-scoped directory, `MEMORY.md`, pause/resume, disable env var, and provenance tags (**PACKAGE/STATIC**) | Layered memory/retrieval, summary-store, SQLite/WAL, trajectories/watchers/indexed search symbols indicate a broad integrated subsystem (**STATIC**) | Bounded profile-scoped `MEMORY.md`/`USER.md`, frozen session snapshot, locked atomic edits, drift backups, provider isolation, background review, plus SQLite FTS5 session search (**OSS**) |

### Reference-specific cautions

- **Codex:** WebSearch, subagents, MCP, sleep inhibition, and memory are
  source-backed at the cited commit. The browser conclusions come from a
  separately versioned proprietary plugin package. MCP progress notifications
  were primarily logged in the examined client handler rather than fully
  surfaced to model/UI state, and no prompt-client path or native LSP subsystem
  was found in the scoped search.
- **Claude Code:** tool types, CLI surfaces, marketplace data, and binary strings
  establish packaged behavior and configuration, not implementation ownership.
  The official Playwright and LSP entries are especially useful evidence that a
  feature can be package-installed while lifecycle remains a host concern.
- **Antigravity:** symbols strongly suggest broad integrated managers, but they
  do not prove reachability, policy ordering, persistence semantics, or remote
  backend behavior. Recommendations do not depend on those inferences alone.
- **Hermes:** the open tree provides the clearest provider/manager examples and
  durable background delivery design. Its MCP `trust` setting defaults to
  `full`; unknown values normalize to `untrusted`, and only an annotation whose
  `readOnlyHint` is exactly true bypasses the untrusted-server gate. No use of
  Hermes's DNS-aware URL-safety module was found for remote MCP endpoints, so
  that omission should not be copied.

## Patterns worth carrying into Ygg

### 1. Extension managers own domain invariants; the kernel owns enforcement

Hermes's web, browser, and memory provider APIs and Codex's memory backend are
useful extension seams. In Ygg, the long-lived extension manager owns its
domain state, caching, protocol lifecycle, and backend variation. The kernel
owns only enforceable permissions, generic approvals/artifacts/secrets, durable
session identity, model-session creation, process cleanup, and resource bounds.

### 2. Search and browsing are different products

Search is mostly stateless retrieval with stable citations and aggressive
normalization. Browsing owns mutable tabs, cookies, dialogs, downloads, stale
element references, screenshots, and consequential actions. Combining both
behind one undifferentiated extension tool makes safety and cleanup worse.

### 3. Long-lived services need explicit health states

Codex MCP and Hermes MCP distinguish optional startup failure, permanent
configuration/auth failure, transient transport failure, reconnect, and
refresh. Hermes similarly reaps browser and LSP state. A single generic
"process is running" bit is not enough for MCP, browser, LSP, or provider
adapters.

### 4. Live progress is ephemeral; terminal results are durable

Ygg's native `ToolProgress` already has the right persistence rule. Codex emits
start/completion items; Hermes forwards progress and keeps final structured
results. Extension progress should be bounded and disposable, while exactly one
terminal result is persisted and sent to the model.

### 5. Background completion must re-enter legally

Hermes delivers a background completion as a new turn instead of mutating an
already completed conversation prefix. Its claim/ack persistence also avoids
losing completion during a crash. Ygg's subagent-orchestrator extension should
use the same principle while the host persists and runs the child model
sessions it creates.

### 6. Prompt-affecting memory should be frozen

Hermes makes writes durable immediately but freezes the system-prompt memory
snapshot for the session. Codex limits automatic startup memory work to eligible
root sessions. Both avoid silently changing the established prompt prefix.

### 7. Trust metadata is a hint, not authority

MCP annotations, extension capability declarations, and provider labels are
useful inputs. They must not be able to mark their own operation safe. Codex's
conservative MCP defaults and Hermes's exact `readOnlyHint is True` rule are
safer than treating missing metadata as read-only.

## Recommended Ygg architecture

```text
Ygg host / kernel
├── model loop and child model sessions
├── bounded JSON-RPC bus
├── sessions, tool calls, and tool-result persistence
├── permissions and approvals
├── extension process supervision and cleanup
├── stable session/process ownership
└── memory, message, concurrency, artifact, and process limits

Subprocess extensions
├── ygg-mcp ───────────── MCP servers and live tool catalogs
├── web search ────────── retrieval, normalization, citations
├── browser use ───────── tabs, page state, web actions
├── computer use ──────── desktop observation and actions
├── memory ────────────── retrieval, consolidation, storage
├── LSP ───────────────── clients, documents, diagnostics
├── subagent orchestrator child sessions through host service
└── caffeinate ────────── platform sleep-inhibition behavior
```

A common kernel supervisor provides process groups, generation IDs,
startup/shutdown deadlines, health, restart/backoff, drain, and diagnostics.
It never needs to understand MCP, LSP, CDP, a memory schema, or a search
provider. Each extension speaks its domain protocol on the far side of the Ygg
JSON-RPC boundary.

### Core-versus-extension decision rule

Keep a responsibility in the host only when an extension logically depends on
it to run or when only the host can enforce it across extensions: model
conversations, transport, persistence, permissions/approvals, process cleanup,
stable ownership, and resource limits. Long-lived state, cross-call
multiplexing, or terminal cleanup do not by themselves make a capability core;
the supervisor, ownership token, and process-group fence let an extension own
those safely.

Everything else uses the language-neutral subprocess seam. Trusted local code
may still have authority the host cannot technically sandbox; capability
metadata remains visible consent, not a security claim.

### Registration shape

An extension manifest declares the trusted executable and its bootstrap
contributions. Initialization exactly matches that declaration. An API `0.2`
extension may then negotiate `dynamic_tools` and publish its live catalog with
transactional `tools/register` and `tools/unregister` requests. Per-process
catalog epochs pin calls from an in-flight model turn to the schema and handler
the model saw; a new process generation starts again at epoch zero. The
initialize catalog is the only deterministic turn-one catalog. Post-initialize
mutations appear at the next model-request boundary after publication, with no
implicit startup-quiescence heuristic.

Stateful extensions namespace handles by the host-derived
`(session_id, extension_instance_id, process_generation)` resource owner. A
subagent orchestrator uses the agent-session host service rather than receiving
direct access to the kernel's conversation registry. Secrets, artifacts, and
approvals likewise use optional generic host services instead of
domain-specific host managers. Secret names must also appear exactly in the
manifest's `[capabilities].secrets` allowlist; negotiation alone never widens
that set.

## Capability recommendations

The domain requirements below survive the superseded architecture, but their
owner is the corresponding long-lived extension. References to host artifacts,
secrets, approvals, model sessions, or process cleanup mean generic kernel
services; they do not create a host-side search, browser, MCP, LSP, memory, or
caffeinate manager.

### WebSearch

#### Product contract

The web-search extension should publish a search surface with at least:

- query (including a bounded batch of queries);
- domain allow/block filters;
- open/fetch by stable result reference or explicit URL;
- find within a fetched document;
- optional image search after media output lands;
- normalized title, URL, snippet/content, publication time when known, and a
  stable citation ID.

Codex's broad `web.run` command union is a useful model-facing interface, while
Hermes demonstrates the backend portability and network defenses. Ygg need not
ship finance/weather/sports/time commands in the first slice; those are product
breadth, not architectural prerequisites.

#### Ownership

The extension owns provider translation, normalization, result references,
citations, caches, truncation, retries, cancellation behavior, and
billing-aware limits. It declares the authentication and network authority it
needs. The host supplies only generic secret access, permission/approval
decisions, artifacts, transport cancellation, and resource ceilings.

Provider credentials should come from a host secret broker or scoped launch
environment, never an ambient dotenv inherited by every extension. For network
policy to be enforceable, a future optional generic egress broker may perform
HTTP for the extension. An extension that opens its own sockets is trusted
local code; Ygg may validate declared intent and results, but cannot claim to
constrain malicious code without an OS sandbox.

#### Network boundary

For host-mediated fetches:

1. permit only expected schemes;
2. reject embedded credentials and suspicious secret-bearing URLs;
3. normalize hostnames/IDNs and resolve DNS;
4. reject loopback, private, link-local, multicast, and cloud-metadata ranges
   for every resolved IPv4/IPv6 address unless an explicit local-network policy
   allows them;
5. connect to a vetted/pinned address while preserving TLS SNI and HTTP host;
6. revalidate every redirect;
7. bound redirects, bytes, decompression, content type, and time;
8. account for proxies explicitly, because local DNS pinning is not effective
   if an untrusted proxy performs resolution.

Retrieved pages are untrusted data. They must be delimited from system
instructions and retain source provenance.

#### Retry and UX

Search/fetch can be marked semantically replay-safe only after policy permits
it and the adapter contract guarantees no mutation; retries still need billing
and rate limits. Show correlated status (provider, query count, fetch stage),
but persist only the final citations/result. Cache hits should remain
inspectable rather than silently look like a new network call.

### BrowserUse

#### Product contract

BrowserUse should be a persistent, session-aware service rather than a fresh
process per tool call. Minimum operations:

- create/close context; list/open/close tabs;
- navigate and report URL/title;
- semantic accessibility/DOM snapshot;
- click, fill, select, press, hover, scroll, and wait;
- upload/download through host-mediated artifact paths;
- screenshot as native image media;
- restricted script/CDP escape hatch with stronger policy.

Prefer semantic role/name/text/test-ID locators. Snapshot element references
must be opaque and generation-scoped so stale references fail explicitly.
After each action, return a cheap observation: URL/title, changed semantic
nodes, dialogs, downloads, or other state needed to determine whether the
operation worked. Use screenshots selectively, and reserve coordinate/pixel
control for an explicit fallback.

#### State and isolation

- Scope browser contexts to `(Ygg session, task/subagent)`.
- Default to an ephemeral profile. Attaching to a personal profile or reusing
  cookies requires explicit user choice.
- Give each subagent isolated tabs/context unless sharing is intentional.
- Tag tab/context handles with the provider generation; invalidate them after
  restart rather than accidentally targeting a new tab.
- Reap idle contexts and close all owned browser descendants at session/host
  shutdown.
- Quarantine downloads in a host-owned directory and ingest them as bounded
  artifacts.

#### Safety

Page text, accessibility names, script output, downloads, and tooltips are
untrusted. Browser content cannot grant permission or change policy.

Ask at the last responsible moment for consequential external actions such as
sending a message, submitting a form with sensitive data, publishing,
purchasing, deleting, uploading, changing access, or entering credentials.
Ordinary reading and navigation should not prompt. The host derives the risk
from the action/target and user intent; an adapter's `read_only` or
`destructive` flag is only a hint.

Secrets should be fetched only by exact manifest-allowlisted name from the
owner-scoped host broker and should not be returned in snapshots, logs,
screenshots, or model-visible text. Raw CDP access needs a small allowed domain
set or a separate high-risk capability.
Main-frame URL checks are insufficient: redirects, subresources, popups,
downloads, service workers, and browser-originated fetches all need policy.
Prefer a policy-enforcing egress proxy plus browser interception; without an
OS/network sandbox, the browser sidecar remains trusted local code.

#### Failure and cancellation

Cancellation first cancels the in-flight browser command. If it does not settle
within a grace period, close the affected page/context; if the transport is
wedged, restart the provider and invalidate its handles. A disconnect after a
consequential click is an ambiguous outcome and must never be retried
automatically.

The browser extension owns these domain rules and requests host approval at the
last responsible moment. The host owns the final allow/ask/deny decision and
process/resource fences, not tabs, locators, or browser action semantics.

### Caffeinate

Sleep inhibition is implemented as the long-lived API `0.2`, version `0.2.0`
`caffeinate` example extension:

- acquire and release leases from complete turn/task lifecycle observations;
- share one platform assertion while at least one observed owner is active;
- treat acquisition failure as non-fatal and expose bounded status;
- keep helper processes in the extension process group so host cleanup is the
  final crash/shutdown fence; and
- discard all leases when the owning process generation changes.

It prevents idle **system** sleep, not display sleep or explicit user sleep.
The current implementation is intentionally narrow: on macOS it runs one
`/usr/bin/caffeinate -i -t 1800` helper while at least one owning/root turn is
active; unsupported systems or a missing executable produce a bounded,
non-fatal diagnostic. Linux and Windows backends, if added, belong in this
extension rather than the kernel. No core sleep inhibitor remains.

### Subagents

Subagent orchestration belongs in an extension. The kernel host service creates
and runs bounded child model sessions, persists their conversations/results,
enforces model/token/time/depth/concurrency and inherited permissions, and
provides scoped spawn, follow-up, message, wait, list, and interrupt operations.
The orchestrator extension owns task decomposition, routing, roles, completion
policy, and how child results re-enter the parent workflow.

The tracked working-tree V2 harness already uses the right control vocabulary:
`spawn_agent`, `followup_task`, `send_message`, `wait_agent`, `list_agents`, and
`interrupt_agent`. The host-service bridge exposes that bounded machinery to a
process-scoped orchestrator without giving it the global conversation registry.

Background delivery should use a durable state progression such as:

```text
queued -> running -> completed|failed|cancelled -> claimed -> acknowledged
```

After restart, an unacknowledged completion can be claimed again. Delivery
enters the parent as a new legal turn/event; it does not splice a message into
an already completed prefix. Child agents should return a concise summary plus
references to durable artifacts/worktree changes rather than copying their full
conversation into the parent.

The extension never supplies an arbitrary session owner or widens its
permissions. Its process principal scopes the child sessions it can address;
the host owns the conversation records and execution lifecycle. Child trees are
keyed by extension principal plus durable session owner rather than process
generation, so a supervised extension restart/reload can resume them; explicit
extension shutdown stops the owned trees.

In the current working-tree bridge, orchestrators observe child state through
`agent/list` and `agent/wait`. Delegated child turns do not fan out through the
extension `session/*` or `turn/*` lifecycle stream; those notifications cover
the owning/root product session. Child lifecycle fan-out would be an additive
host-service behavior, not a reason to move orchestration into the kernel.

### MCP

Build MCP as a long-lived `ygg-mcp` extension. It speaks MCP directly to its
servers, translates their tool catalogs onto Ygg's JSON-RPC bus, and publishes
changes through `tools/register` and `tools/unregister`. The additional local
hop is cheap beside inference and external execution; replaceability and a
small host are worth it. MCP catalog revisions, annotations, progress,
cancellation, sampling, elicitation, resources, and transport health remain
inside the bridge rather than being flattened into host concepts.

#### Required lifecycle

- stdio and Streamable HTTP; retain legacy SSE only where compatibility needs it;
- global/user, trusted-project, and invocation scopes;
- pending approval for project-supplied server configuration;
- parallel startup with required versus optional servers;
- deferred startup for cold or rarely used servers;
- reusable sessions, keepalives, idle/max-age recycling;
- transient reconnect with exponential backoff/jitter;
- permanent parking for authentication, invalid configuration, executable-not-
  found, or clearly non-MCP endpoints until a user/config refresh;
- bounded pagination and cached, revisioned catalogs;
- explicit refresh and observable per-server health;
- graceful shutdown and process-tree cleanup.

Support tools first without designing out resources and prompts. Sampling and
elicitation are server-to-host authority requests and require explicit host
policy/UI; they must not silently invoke a model or obtain secrets. Trust to
launch a local stdio server is separate from approval to call one of its tools:
the server itself is arbitrary code running as the user, so per-call approval
does not sandbox a malicious server.

#### Approval policy

Codex exposes `Auto`, `Prompt`, `Writes`, and `Approve` modes and defaults
conservatively for destructive/open-world tools unless a tool is read-only.
Hermes places its trust gate before transport and requires exact positive
read-only annotation for an untrusted server to bypass approval. Ygg should
retain that conservative direction without copying Hermes's `trust = full`
default.

Central policy returns `allow`, `ask`, or `deny`. Inputs include server identity
and fingerprint, configuration scope, tool/schema revision, actual target and
arguments, current workspace/session, user rules, and MCP annotations.

Treat annotations conservatively:

- only `readOnlyHint === true` is evidence toward read-only;
- missing or malformed metadata is unknown, not safe;
- destructive/open-world hints increase risk;
- server trust does not imply every tool call is approved;
- remembered approval is scoped to server identity, tool schema revision,
  workspace/session as appropriate, and a bounded action pattern;
- invalidate remembered decisions when server identity or catalog revision
  changes.

Never automatically replay a tool call after an ambiguous disconnect unless the
host has positive idempotence evidence and an explicit retry policy. Catalog
listing and other protocol reads can retry independently.

Remote MCP URLs use the same SSRF/redirect protections as WebSearch. Stdio
servers receive a minimal environment plus explicitly scoped secrets. Store
OAuth refresh/access tokens in a host credential store, persist only references
in configuration, inject them only into the intended server connection, and
redact them from diagnostics. MCP structured content and image/audio content
lower through the same artifact/media bridge as executable tools.

### Language servers

An LSP extension should directly manage language-server processes from
descriptors such as:

- server ID, command and direct args;
- extension-to-language mapping;
- workspace/root markers;
- stdio or supported socket transport;
- initialization options/settings;
- startup/shutdown and idle timeouts;
- whether diagnostics are enabled.

Key a reusable client by server descriptor/config fingerprint and canonical
workspace root. Start lazily only in a trusted project with a matching file.
Resolve the executable deterministically and show actionable install guidance;
do not silently download or execute a new project command.

The LSP extension owns:

- `initialize`/`initialized` and graceful `shutdown`/`exit`;
- `didOpen`, monotonic-version `didChange`, `didSave`, and `didClose`;
- freshness after Ygg or external filesystem edits;
- `$/cancelRequest` and progress forwarding;
- push and pull diagnostics;
- idle reaping, crash restart, and bounded logs.

Initial model-facing operations should be definition, references, hover,
workspace symbol, document symbol, and diagnostics. Return compact file/range
and snippet data, and reject stale responses whose document version no longer
matches. Capture a diagnostic baseline before a host write and report a bounded
delta afterward; diagnostics are useful fail-soft feedback, not a reason to
roll back a successful edit automatically.

Code actions, rename, `workspace/executeCommand`, or server-initiated edits are
mutating operations and need a separate policy and optimistic file checks. A
one-process-per-tool-call tool would lose indexes, document versions, and
diagnostics, so the LSP extension must be long-lived.

### Memory

Memory needs explicit layers instead of one unscoped text blob:

| Layer | Typical scope | Purpose | Injection policy |
| --- | --- | --- | --- |
| User rules/preferences | user/profile | durable user choices and constraints | bounded frozen session snapshot |
| Project knowledge | canonical project | repository facts not already obvious from files | bounded frozen session snapshot with citations |
| Episodic summaries | session/task/rollout | what happened and why | retrieved or compacted, not globally injected |
| Transient retrieval | current turn | relevant prior sessions/artifacts | query-time, bounded, provenance-preserving |
| Procedural skills | installed/project resource | reusable workflows | keep in the existing skill system, not memory |

The memory extension owns scope semantics, provenance, retrieval, write policy,
editing/deletion, retention, and consolidation timing. It publishes bounded
tools/context and uses durable artifacts or its own explicitly authorized
storage. The host owns the model context budget, session persistence, and final
permission boundary; an extension cannot grant itself cross-scope authority by
returning instructions as data.

#### Snapshot and write semantics

At session start, take a bounded immutable snapshot of memory selected for the
system prompt. Mid-session writes are immediately durable and visible through
memory tools, but do not mutate the established prompt prefix. An explicit
reload/new prompt epoch may refresh it; otherwise it takes effect next session.
This preserves provider prefix caching and makes the context inspectable.

Use locked, no-follow, atomic writes; reject unreadable source files rather than
overwriting them; retain a bounded drift backup when the on-disk file changes
outside Ygg. Every item or generated summary carries source scope, file/session/
turn identity, timestamp/version, and trust classification. Retrieved memory is
data, never an instruction that can grant authority.

Expose at least list, read, search, add/update, and delete operations. Users need
to inspect why a memory was retrieved and remove incorrect or sensitive data.
Do not store credentials. External embedding/consolidation providers are
opt-in, receive only the selected scope, and cannot cause cross-provider data
leakage by default.

Automatic extraction/consolidation should run asynchronously only for eligible
root, non-ephemeral sessions. Subagents inherit a bounded frozen snapshot and
should propose durable memories to the parent rather than writing global memory
directly. Start with manual curated memory and local session search; add
automatic extraction only after provenance, deletion, and evaluation are in
place.

## Executable-extension protocol `0.2`

API `0.1` remains frozen for existing simple extensions. API `0.2` implements
the breaking result/lifecycle semantics below. The manifest selects the exact
version, then `initialize` negotiates additive features; support is never
inferred from extension version strings.

The host request is:

```json
{
  "protocol": {
    "version": "0.2",
    "required_features": ["request_cancellation", "content_parts"],
    "optional_features": [
      "request_progress",
      "artifacts",
      "lifecycle_events",
      "policy_intents",
      "dynamic_tools"
    ],
    "limits": {"max_concurrent_requests": 64}
  }
}
```

The initialization response returns `protocol.version`, the supported feature
subset, and `limits.max_concurrent_requests`; the host caps the limit. Missing
required, unknown, or duplicate features and mismatched versions reject the
candidate before registration. If `lifecycle_events` is negotiated, the
response may include an exact subscription subset; omission subscribes all six
events. The host conditionally appends `agent_sessions`, `approvals`, and
`secrets` only when it has the corresponding service. `approvals` also requires
`policy_intents`; `secrets` additionally requires a configured broker and a
non-empty exact manifest allowlist. The SDK reader loop never executes a handler
inline: it keeps reading control frames and schedules handlers behind the
negotiated concurrency semaphore, which lets queued or running work observe
cancellation. Domain provider contracts retain their own version so a
browser-provider change does not require changing the base JSON-RPC framing
version.

### 1. Real request cancellation

Use the originating host JSON-RPC ID as the cancellation target, following the
LSP convention:

```json
{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":42,"reason":"user"}}
```

Implemented semantics:

1. Before the writer starts a frame, cancellation removes the request from the
   queue and sends nothing.
2. Once a frame write starts, the writer completes that frame without
   cancellation; the host then sends one idempotent cancellation notification.
3. The SDK exposes an ambient cancellation token/event to the active handler.
4. A cooperative extension completes the original request with a cancellation
   error (use `-32800`) or may win the race with a normal result.
5. The host tombstones cancelled IDs for a bounded period so a late response is
   ignored and diagnosed rather than killing the connection.
6. After the bounded grace period, a non-cooperative process generation is
   marked degraded and terminated.
7. Side-effecting operations report cancellation as "requested" rather than
   claiming rollback; an ambiguous external outcome is never replayed.

The transport uses a dedicated bounded writer task that serializes complete
frames. Dropping a request waiter cannot drop a partially written future or
close an otherwise healthy persistent connection.
Timeout and host shutdown use the same cancellation machinery but retain
distinct terminal reasons.

### 2. Correlated progress and extension-originated requests

Extensions use a request-scoped notification:

```json
{
  "jsonrpc": "2.0",
  "method": "$/progress",
  "params": {
    "request_id": 42,
    "sequence": 7,
    "event": {"type": "status", "message": "Fetched 3 of 10 results"}
  }
}
```

Event variants map directly to native progress:

- `status {message, current?, total?, unit?}`;
- `output {stream: stdout|stderr, encoding: utf8|base64, data}` with bounded
  payloads;
- a dropped/coalesced diagnostic generated by the host, not by the extension.

Sequences are monotonic per request. The host applies existing 8 KiB chunking,
bounded-channel dropping, and aggregate drop reporting. Progress is never added
to the model conversation or session transcript as a result.

Every extension-originated confirmation, input, artifact publication, policy
evaluation, secret lookup, or agent-session operation carries
`parent_request_id`. Global notifications, context, and status contributions
may omit it. This prevents concurrent extension calls from racing to display or
answer another call's prompt. `input/request` carries
`{prompt, secret, parent_request_id}` and returns `{value: string|null}`;
prompts/answers are bounded to 16/256 KiB UTF-8, and secret values stay on the
ephemeral private reply channel and never enter logs, progress, or persistence.
When the parent settles, the host atomically denies/cancels all unresolved
child requests; late replies are ignored. The same cancellation notification
may be used in the opposite direction when an extension abandons one of its
host requests.

### 3. Structured and media output

API `0.2` replaces string-only subprocess output with an MCP-like result that bridges to
native `ToolOutput`:

```json
{
  "content": [
    {"type": "text", "text": "Found 3 sources."},
    {
      "type": "image",
      "artifact_id": "artifact_01",
      "mime_type": "image/png",
      "alt": "Browser screenshot after submit"
    }
  ],
  "structured_content": {"sources": [{"title": "...", "url": "..."}]},
  "is_error": false,
  "metadata": {"cache": "miss"}
}
```

Rules:

- text remains the explicit compact model-visible representation;
- `structured_content` is required exactly when the tool declared
  `output_schema`, validated against that schema, retained for UI/session use,
  and lowered to the model only by host policy;
- image/audio parts become existing `ygg_ai::Media` values;
- arbitrary local paths and remote media URLs are not trusted directly;
- media references are accepted only through the matching verified artifact
  owner and generation, not as arbitrary extension paths or URLs;
- API `0.2` `metadata` is retained as bounded non-model-visible result detail;
  frozen API `0.1` continues to accept and discard it.

The native canonical result envelope now carries optional
`structured_content` and vetted metadata rather than creating a subprocess-only
parallel model. Provider lowering preserves supported media and explicit text.

Large media stays out of line. Each process generation receives a host-owned
scratch directory and `artifact/publish` accepts either bounded inline base64
or a relative scratch path plus claimed MIME type, size, and SHA-256. The host
opens it with bounded no-follow semantics, checks size/digest/type, snapshots
it, and returns an opaque artifact ID bound to the publishing host-derived
session owner and process generation. Media resolution supplies that same
owner; another session owner cannot use a leaked ID. A browser screenshot
should not need to fit base64 plus JSON inside the 1 MiB control-frame limit.

### 4. Complete lifecycle events

Interceptable `before_*` hooks remain bounded requests. API `0.2` adds
observational, non-veto lifecycle notifications:

- `session/started`, `session/settled`;
- `turn/started`, `turn/settled`;
- `tool/started`, `tool/settled` where an extension subscribes to global tool
  observation.

Every admitted turn receives exactly one host-side `turn/settled` outcome:

- `completed`;
- `failed`;
- `cancelled`;
- `interrupted`;
- `frontend_disconnected`;
- `shutdown`;
- `limit_reached` where applicable.

Include stable session/run/turn IDs, duration, and a bounded reason; do not send
full prompts, secrets, or model output unless a contribution explicitly needs
and is allowed to receive it. Notification delivery is best effort because the
process may already have failed. Therefore host process cleanup and persistence
completion remain kernel finalizers. An extension owns its domain cleanup while
healthy and treats process-group termination/generation invalidation as the
final crash fence.

The success-implying `after_response` remains frozen for `0.1` compatibility
only; API `0.2` consumers use `turn/settled`.

### 5. Host-mediated policy intents

`confirmation/request` remains cooperative UI with parent correlation and is
not enforcement. Negotiated `policy_intents` sends a structured request:

```json
{"jsonrpc":"2.0","id":"policy-1","method":"policy/evaluate","params":{
  "parent_request_id":42,
  "intent":{
    "kind":"external_side_effect",
    "operation":"browser.submit_form",
    "target":{"origin":"https://example.com","label":"Publish comment"},
    "data_classes":["user_text"],
    "adapter_hints":{"read_only":false,"destructive":false}
  }
}}
```

The response is `{decision: "allow"|"ask"|"deny", approval_token?: string}`.
The host derives the decision from authoritative context. Adapter hints can
only increase caution, never lower it. When conditional `approvals` is offered
and negotiated alongside `policy_intents`, a trusted frontend approval returns
`ask` plus a short-lived opaque token. The extension must retry the exact
original intent under the same still-active owner/parent and process generation.
The core atomically consumes the token at that retry boundary and returns
`allow` once; expiry, reuse, or any intent/parent/generation mismatch returns
`deny` and invalidates a recognized token. Tokens are not remembered policy
rules and cannot cross a replacement generation. Current coding-product paths
leave approvals off and have no domain adapter, so the policy supervisor
returns `deny` without a token.

This token still cannot constrain malicious unsandboxed code. Actual
enforcement requires either a host-executed broker (network, secret fill,
artifact write, browser action) or a future OS sandbox. Ygg must continue to say
so plainly.

### 6. Host-mediated secrets

The conditional `secrets` feature exposes only `secret/get` with
`{parent_request_id, name}`. The host offers it when a broker is configured and
the manifest declares at least one exact `[capabilities].secrets` name. For
every lookup, the broker receives the manifest-bound extension identity, full
`(session_id, extension_instance_id, process_generation)` owner, active parent
request, and logical name. Neither identity nor owner is accepted from the
extension's JSON.

The allowlist is exact and duplicate-free; undeclared names are rejected. A
broker no-value result and provider failure are deliberately indistinguishable
as `secret is unavailable`. Values are bounded UTF-8 and stay out of host logs,
progress, persistence, and ordinary diagnostics. Host broker and writer buffers
are best-effort wiped, but the receiving extension holds an ordinary language
string, so this is not end-to-end zeroization. The coding product currently has
no secret broker and does not offer the feature.

### 7. Dynamic tool catalogs and stable ownership

An extension negotiating `dynamic_tools` starts at catalog epoch `0`, matching
the exact tools returned during initialize. That catalog is the only
deterministic turn-one catalog. It may then send transactional `tools/register`
and `tools/unregister` requests. Each accepted mutation increments the
per-process epoch and returns the complete policy-accepted name set. Conflicts
or invalid schemas preserve the old catalog.

The agent snapshots schemas and implementations at each model-request boundary.
A later catalog mutation appears at the next boundary after publication, never
halfway through one provider response. There is no implicit quiescence period
for registrations sent immediately after initialization; first-turn tools must
be in the bootstrap catalog. Every call into a dynamic extension carries the
frozen `catalog_revision`; the extension dispatches through the matching
historical handler snapshot. A replacement generation resets to its initialize
catalog at epoch zero, and later mutations follow the same next-boundary rule.

API `0.2` model-tool and tool-hook contexts carry a host-derived
`resource_owner {session_id, extension_instance_id, process_generation}`.
Stateful extensions key browser, MCP, LSP, memory, and similar handles by that
triple. The durable session component survives reopening a persisted Ygg
session at the same canonical path. The instance component changes across a
complete process-host rebuild, and the generation component prevents a reloaded
or automatically restarted extension process from accepting stale handles
within one host instance.

### 8. Reload, drain, and health

The process health vocabulary is:

```text
discovered
  -> starting -> initializing -> ready
  -> draining -> stopped
  -> degraded/crashed
  -> backoff/parked (managed restart/permanent-failure state)
```

Reload sequence:

1. start and fully negotiate candidate generation `N+1` while `N` remains ready;
2. if the extension negotiated `dynamic_tools`, reserve the candidate catalog;
   otherwise reject changed schemas/contributions with `re-registration
   required` so a product rebuild can replace the static registry safely;
3. mark `N` draining and stop new dispatch;
4. allow admitted operations to settle and cancel the rest by deadline;
5. emit any remaining `N` lifecycle terminals, then use shutdown acknowledgement
   (or its bounded timeout) as the old-generation processing barrier;
6. seed replacement session/turn lifecycle state and atomically route new
   operations to `N+1`;
7. terminate `N`'s process group if bounded shutdown did not exit cleanly, and
   reject all stale progress, handles, approvals, secret lookups, and
   confirmations from `N`.

Never automatically replay an unresolved unsafe tool call. Retry only calls
whose domain contract and idempotency policy permit it. Browser tab, MCP server,
and other remote handles include the owning generation and fail stale rather
than aliasing replacement state.

Health, generation, pending request count, negotiated features, and the last
bounded error are exposed through `/extensions` inspection. Automatic
restart/backoff supervision is implemented after one successful initialization.
It reacts to exit or terminal transport failure rather than heartbeating an
otherwise live child. A full product rebuild creates a new extension instance
and resets the supervisor's in-memory retry/parked state.

## Lifecycle and security state machines

### Per-operation lifecycle

```text
queued -> writing -> sent -> running -> settling
  -> completed
  -> failed
  -> cancelled
  -> timed_out
  -> connection_lost (possibly ambiguous)
```

Only the host records the terminal state, exactly once. A cancellation races
with completion; whichever terminal transition wins is durable. Progress after
the terminal transition is dropped. Unsafe `connection_lost` operations are
reported as ambiguous and are not retried.

### Managed-resource lifecycle

Browser contexts/tabs, MCP connections, LSP clients/documents, artifacts, and
sleep leases have an explicit session owner and process generation:

```text
absent -> creating -> active -> closing -> closed
                    \-> lost/invalidated
```

Session settlement asks the owning extension to clean up; process settlement
reaps its process group and invalidates the generation even if event delivery
fails. The kernel does not need domain-specific handle implementations to make
stale use fail safely.

### Approval lifecycle

```text
intent received
  -> host classification
     -> deny
     -> ask -> user deny -> deny
            -> user allow -> issue one-use capability
               -> retry exact intent under same active owner/parent
                  -> atomically consume -> allow once
                  -> mismatch/expiry/reuse -> deny
     -> allow directly
  -> execute admitted operation
  -> record bounded outcome
```

Cancellation while waiting is denial. Headless frontends deny unresolved
interactive prompts unless an explicit non-interactive policy already allows
the exact intent. The implemented capability is not a remembered rule: it is
bound to the canonical intent, active parent/owner, process generation, and
short expiry, then consumed even on a recognized mismatch. Approval tokens and
secret values must never enter logs/session state.

### Trust boundary

There are two materially different threat models:

1. **Trusted executable extension:** arbitrary local code running as the user.
   Manifests, prompts, and approval tokens are consent/UX, not containment.
2. **Untrusted remote content/server data:** pages, search results, MCP tool
   descriptions/results, LSP text, and retrieved memory. These remain data and
   cannot grant local authority even when transported by a trusted process.

Daily-driver extensions should minimize the first boundary by requesting
narrowly scoped secrets and approvals from generic host brokers and persisting
model-visible outcomes through the normal tool/session path. The extension
still performs its domain actions. Without an OS sandbox or a broker that
executes a particular operation, Ygg must not claim that a policy request can
contain malicious trusted code.

## Persistence, observability, and UX

### Persistence rules

Persist:

- terminal tool result and admitted structured/media references;
- service configuration identity and catalog/schema revisions;
- subagent durable dispatch/completion/claim state;
- memory records and provenance;
- bounded policy decision metadata without secret answers;
- enough generation/ownership data to invalidate stale handles.

Do not persist as model context:

- live progress chunks;
- secret input/credential-fill values;
- raw OAuth tokens or extension environment;
- unbounded browser snapshots, MCP logs, or LSP diagnostics;
- approval prompts that never admitted an operation.

Use a host artifact store with content hashes, size/type limits, ownership,
retention, and garbage collection. Use atomic files or SQLite WAL where durable
concurrent state is required; detached in-memory tasks are insufficient for
background delivery.

### Daily-driver status surfaces

Users should be able to inspect, without reading logs:

- extension generation, negotiated features, pending calls, and last error;
- MCP server state, auth need, catalog revision, refresh/reconnect action;
- browser contexts/tabs and whether a personal profile is attached;
- LSP server/root, indexed/open files, and last diagnostic state;
- subagent tree, task/status/model/workspace/cost, and interrupt action;
- memory scope, source/provenance, retrieval reason, edit/delete controls;
- whether sleep inhibition is active and which tasks own the lease.

All modes must settle resources consistently. Interactive mode may render rich
progress and confirmations; print/plain/RPC/serve modes still need deterministic
allow/deny behavior, cancellation, terminal events, and cleanup.

## Phased implementation

The ordering is dependency-driven: finish the kernel bus before building
capability extensions.

### Phase 0 — extension API `0.2` foundation (implemented)

- The serialized writer, request cancellation, tombstones, and SDK cancellation
  tokens are implemented.
- Request-scoped progress, parent correlation, and ephemeral input are
  implemented.
- Content parts, structured output, retained details, and native media bridge
  through the owner-isolated artifact store.
- Lifecycle notifications and structured policy-intent transport are present;
  product policy defaults to deny without a domain adapter.
- Conditional single-use approval retry and manifest-allowlisted owner-scoped
  secret brokerage are implemented in the kernel/SDK; the coding product keeps
  approvals off and configures no broker.
- Ready/draining/degraded/crashed health and explicit reload drain are active.
- API `0.1` remains available without emulating `0.2` guarantees.

### Phase 1 — live catalogs and ownership (implemented in this working tree)

- Negotiate `dynamic_tools`; implement transactional `tools/register` and
  `tools/unregister` with per-process epochs.
- Freeze a coherent schema/implementation snapshot for every model request.
- Carry host-derived durable session ownership plus process-instance and
  process-generation fences in API `0.2` tool contexts.

### Phase 2 — supervision and host services

- Automatic long-lived restart with bounded jittered backoff,
  stable-generation cutover, crash budget, and parked state is implemented.
- Finish the product gates for process-principal-scoped child model sessions
  exposed to a subagent orchestrator extension.
- Keep artifact, policy, approval, and secret services generic while wiring
  product-specific adapters without adding domain managers.

### Phase 3 — proving extensions

- Build `ygg-mcp` as the live-catalog proof: one extension, multiple MCP
  servers, dynamic publication, and generation-safe reconnect.
- Use the migrated API `0.2` Caffeinate example as the terminal-lifecycle and
  subprocess-ownership proof; no core inhibitor remains.
- Ship web search, browser, computer use, memory, LSP, and subagent orchestration
  as independent extensions, composing only through declared tools and generic
  host services.

### Phase 4 — breadth without kernel growth

- Expand domain behavior inside the owning extensions based on observed use.
- Keep browser, computer use, web search, hosted agents, and in-harness
  subagents distinct in permissions, state, and product language.
- Reject proposals that require the host to understand a capability's external
  protocol when the JSON-RPC bus and generic brokers are sufficient.

## Verification gates

### Protocol foundation

- Cancellation before queueing, during queueing, after full write, while
  running, and racing with a response.
- A dropped waiter never leaves a partial frame or closes a healthy connection.
- Late replies to tombstoned IDs are ignored; unrelated concurrent calls finish.
- A non-cooperative process is escalated and its descendants are reaped.
- Progress flood is bounded/coalesced, reports drops, and is absent from durable
  model context.
- Concurrent calls receive only their own progress, input, confirmations, and
  artifacts.
- Inline and scratch artifacts reject oversize data, bad digest/MIME, absolute
  paths, traversal, links, replacement races, stale generations, and resolution
  by a different session owner.
- Every terminal frontend path emits one outcome and releases every owned lease.
- Reload never exposes a half-built registry or aliases stale handles.
- Dynamic registration/replacement/removal is transactional; a model request
  uses the exact schema and handler snapshot it was shown.
- Per-process catalog epochs reset only at a new generation, and an old
  in-flight turn either reaches its pinned handler or gets a deterministic
  retired-revision error.
- Unexpected child exit removes dead tools, retries with bounded jittered
  backoff, and parks after the tested restart budget without racing shutdown or
  manual reload.
- Agent-session calls are scoped to the requesting extension principal;
  idempotent spawn cannot duplicate a child and one extension cannot address
  another extension's children.

### Security

- Missing/false/malformed provider and MCP hints cannot lower host risk.
- Approval retry succeeds once only for the canonical original intent under the
  same active owner/parent and generation; mismatch, expiry, and reuse deny.
- Headless unresolved approval is denial.
- Secret lookup rejects names outside the exact manifest allowlist, supplies
  full host-derived principal/owner/parent context to the broker, and collapses
  no-value/provider failures to the same unavailable response.
- SSRF tests cover IPv4/IPv6 loopback/private/link-local/metadata, alternate IP
  forms, mixed DNS answers, redirect pivots, rebinding/pinning, and proxy mode.
- Browser page text and MCP descriptions cannot inject policy decisions.
- Secrets do not appear in progress, diagnostics, screenshots, session exports,
  telemetry, or memory; host buffers are best-effort wiped without claiming
  end-to-end process-memory zeroization.

### Capability-specific

- The caffeinate extension releases on success, model error, tool error, user
  cancel, frontend disconnect, process crash/restart, and host shutdown.
- Web citations remain stable across search/open/cache and provider adapters.
- Browser cancellation invalidates only the necessary resource and never
  retries an ambiguous consequential action.
- MCP catalogs paginate/refresh safely; auth/config failures park while transient
  failures back off; structured/media results survive lowering.
- LSP responses are rejected when document versions are stale; push/pull
  diagnostics and idle shutdown are bounded.
- Subagent completions survive restart between completion, claim, and ack;
  parent cancellation and depth/concurrency limits apply to the whole tree.
- Memory writes are atomic, scoped, provenance-preserving, removable, and do not
  mutate an existing session prompt snapshot.

Run these checks through interactive, print, plain, RPC, and serve integration
paths where each exists. Language-neutral conformance fixtures should include
at least the Python SDK and a deliberately adversarial raw JSON-RPC child.

## Protocol-reference discrepancies resolved in this pass

The following mismatches were observed before the 2026-08-16 contract-document
correction. The canonical prose now describes both the frozen API `0.1` runtime
and the implemented API `0.2` contract;
the rows remain here as an audit record and as conformance-test requirements:

| Topic | Previous documentation | Inspected runtime | Resolution |
| --- | --- | --- | --- |
| Initialization contributions | Protocol reference said a process could omit a manifest-declared tool/command | `ensure_same_contributions` requires exact, duplicate-free set equality | Reference now requires exact duplicate-free name sets; SDK guide already did |
| Hook payloads | `after_response` documented `message_id`; tool hooks documented `tool` | Product sends `{response}`; tool hooks send `name`, and `after_tool_call` also sends `arguments` | Reference and SDK guide now list the serialized runtime payloads |
| Terminal hook | Name and examples suggested a general response lifecycle | `after_response` is invoked only after successful complete responses in inspected frontend paths | API `0.1` success-only behavior is explicit; API `0.2` uses `turn/settled` |
| Tool metadata | Described as retained for frontend/renderer use | `ProcessTool` discards `metadata` when constructing `ToolOutput` | API `0.1` docs now say it is accepted but discarded, with no retention guarantee |
| Confirmation string ID | Reference said at most 64 bytes | Runtime accepts up to 256 bytes | Reference and SDK guide now specify 256 UTF-8 bytes |
| Shutdown timing | Reference listed one 3-second grace | `ExtensionRuntimeConfig` defaults to 2 seconds per connection stage; normal product shutdown also has a separate 3-second aggregate deadline, while coordinated-signal exits impose a 1.4-second outer cap before force-kill | Reference now names both normal 2-second stages, the normal 3-second aggregate deadline, and the 1.4-second signal fast path |
| Shutdown signal | Reference header said the host closes stdin | Runtime first sends a JSON-RPC `shutdown` request, waits, then terminates as needed | Reference and SDK guide now document request/ack/exit/kill ordering; stdin EOF is loss/final teardown |
| Contact policies | `permanent`, `on_demand`, `auto_permanent`, and `tool_execute` were listed | No manifest field or dispatch implementation was found; enabled/trusted processes start during product construction | Unsupported policies were removed; the single resident contact policy and generic crash supervisor are documented |
| Manifest limit | Shared resource docs listed only 256 KiB | Product resolver uses its 256 KiB resource bound then parses; direct `ExtensionManifest::load` defaults to 64 KiB | Both layers and the actual product path are now named |
| Caffeinate parent binding | Example README said `-w` bound the inhibitor to the extension PID | Example command uses `-i -t 1800` and no `-w` | README now documents timeout plus explicit cleanup without claiming PID binding |

The source implementation and executable conformance tests should be normative.
Protocol prose should be generated or checked against shared constants and
serialized fixtures where practical.

## Approaches not recommended

- Do not add more success-only hooks and use them as resource cleanup.
- Do not call a generic confirmation API "enforced" for unsandboxed code.
- Do not implement BrowserUse as a fresh one-shot tool process.
- Do not merge search retrieval, browser control, and computer use into one
  authority boundary.
- Do not add MCP or LSP protocol managers to the Ygg host; keep each in its
  long-lived extension and publish model tools over JSON-RPC.
- Do not let a subagent-orchestrator extension access the global conversation
  registry or widen budgets; give it a scoped host service instead.
- Do not let memory providers append arbitrary prompt context every turn.
- Do not inline screenshots/audio into the 1 MiB JSON control channel by
  default.
- Do not replay an unknown side effect after timeout, cancellation, reload, or
  transport loss.
- Do not document lifetime modes or limits before runtime tests enforce them.

## Evidence

### Ygg

- Repository root commit:
  `84c2fb8b654b107e869ed9b8add29b3a50043e60`; workspace version `0.4.0`.
- The worktree was dirty during inspection. In particular, the tracked
  `crates/ygg-agent/src/delegation.rs` and
  `crates/ygg-agent/tests/delegation.rs` contain working-tree changes beyond the
  repository-root commit. Their V2 implementation informs the working-tree
  status above but is not attributed to that commit or treated as shipped.
- Primary implementation:
  - `crates/ygg-agent/src/extension_process.rs`
  - `crates/ygg-agent/src/extension.rs`
  - `crates/ygg-agent/src/tool.rs`
  - `crates/ygg-ai/src/types.rs`
  - `crates/ygg-coding-agent/src/extensions.rs`
  - frontend call sites under `crates/ygg-coding-agent/src/modes/`, plus
    `host.rs` and `extensions/serve.rs`
- Contract/docs:
  - `docs/extensions.md`
  - `docs/extensions/PROTOCOL-REFERENCE.md`
  - `docs/resources.md`
  - `sdk/python/README.md`
  - `examples/extensions/caffeinate/extension.py`
  - `examples/extensions/caffeinate/README.md`

### OpenAI Codex

- Repository: `https://github.com/openai/codex.git` at commit
  `15fde8c1f2d48812c1ad3b5d2fb7a1e7da4053fa` (**OSS**).
- WebSearch:
  - `codex-rs/ext/web-search/src/extension.rs`
  - `codex-rs/ext/web-search/src/tool.rs`
  - `codex-rs/ext/web-search/web_run_description.md`
  - `codex-rs/core/src/web_search.rs`
- Subagents:
  - `codex-rs/core/src/agent/registry.rs`
  - `codex-rs/core/src/session/multi_agents.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_v2.rs`
  - `codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs`
- MCP:
  - `codex-rs/codex-mcp/src/connection_manager.rs`
  - `codex-rs/codex-mcp/src/connection_manager/startup.rs`
  - `codex-rs/codex-mcp/src/connection_manager/tool_catalog.rs`
  - `codex-rs/codex-mcp/src/connection_manager/resources.rs`
  - `codex-rs/config/src/mcp_types.rs`
  - `codex-rs/core/src/mcp.rs`
  - `codex-rs/core/src/mcp_tool_call.rs`
  - `codex-rs/rmcp-client/src/logging_client_handler.rs`
- Memory:
  - `codex-rs/memories/README.md`
  - `codex-rs/ext/memories/src/extension.rs`
  - `codex-rs/ext/memories/src/tools/mod.rs`
  - `codex-rs/ext/memories/templates/memories/read_path.md`
  - `codex-rs/features/src/lib.rs`
- Sleep inhibition:
  - `codex-rs/utils/sleep-inhibitor/src/lib.rs`
  - `codex-rs/utils/sleep-inhibitor/src/macos.rs`
  - `codex-rs/utils/sleep-inhibitor/src/linux_inhibitor.rs`
  - `codex-rs/utils/sleep-inhibitor/src/windows_inhibitor.rs`
- Browser package `26.803.41515` (**PACKAGE**, proprietary implementation):
  - `.codex-plugin/plugin.json`
  - `skills/control-in-app-browser/SKILL.md`
  - `docs/api-use-behavior.md`
  - `docs/browser-safety.md`
  - `docs/confirmations.md`
  - `docs/screenshots.md`
  - `docs/capabilities/tab/cdp.md`
- LSP conclusion is a scoped negative search of the examined Rust snapshot, not
  a product-wide proof of absence.

### Claude Code

- Native binary version `2.1.233` (**PACKAGE/CLI/STATIC**).
- SHA-256:
  `bc466b6cde63edafc773f471a1fb98787fabb31f52240c8616ce7e1f587b212d`.
- Size: `306,981,408` bytes, arm64 macOS package.
- Packaged generated tool schema:
  `node_modules/@anthropic-ai/claude-code/sdk-tools.d.ts`.
- Native CLI evidence included `claude mcp ...` and `claude agents`; the JS
  wrapper itself failed with `Error: claude native binary not installed.`, so
  the packaged native binary was invoked directly.
- Official plugin marketplace snapshot:
  `27c667db53e238cdd6d5806f3f0a47673bf91ace` (**PACKAGE**), especially:
  - `.claude-plugin/marketplace.json`
  - `external_plugins/playwright/.mcp.json`
  - `plugins/rust-analyzer-lsp/README.md`
  - marketplace `lspServers` entries for rust-analyzer, TypeScript, gopls,
    clangd, and others.
- LSP runtime, auto-memory, and sleep-inhibitor implementation details are based
  on generated-schema and binary-string evidence, not source. Some existing
  reverse-engineering notes describe `2.1.206`; they were not treated as proof
  of unchanged `2.1.233` behavior.

### Google Antigravity

- `agy` version `1.1.13` (**STATIC**).
- SHA-256:
  `067ca11a713f61b54aea18358a40efb65d32276401b8c173d0a97e94de215286`.
- Size: `176,838,544` bytes; native arm64 Go executable.
- Static inspection identified approximately 143,067 functions. Relevant
  symbols include `NewSearchWebTool`, `NewMcpManager`, `newCallMcpToolTool`,
  `deriveSubagentTools`, `CancelSubagent`, `summarystore`, and
  `language_server/lsp/lsp.Serve`.
- Inspection artifacts:
  - `/tmp/agy-1.1.13-inspection.json`
  - `/tmp/agy-feature-functions.txt`
- `google-antigravity/antigravity-cli/REPORT.md` describes version `1.1.5`, not
  the inspected `1.1.13`; it is background only. Symbols establish packaged
  implementation evidence, not complete reachability, policy, or backend
  behavior. No sleep-inhibitor symbols were found in the scoped search.

### Hermes Agent

- Repository: `https://github.com/NousResearch/hermes-agent.git` at commit
  `7095e23eb2066fe9a2f93b99cdbfe0e2b5ece397`; package version `0.20.1`
  (**OSS**).
- Web/network:
  - `tools/web_tools.py`
  - `tools/url_safety.py`
  - `website/docs/developer-guide/web-search-provider-plugin.md`
- Browser:
  - `tools/browser_tool.py`
  - `tools/browser_supervisor.py`
  - `website/docs/developer-guide/browser-provider-plugin.md`
  - `website/docs/developer-guide/browser-supervisor.md`
- MCP:
  - `tools/mcp_tool.py`
  - `hermes_cli/mcp_security.py`
  - `website/docs/user-guide/features/mcp.md`
- Delegation:
  - `tools/delegate_tool.py`
  - `tools/async_delegation.py`
  - `tools/subagent_worktree.py`
  - `agent/subagent_lifecycle.py`
  - `website/docs/developer-guide/subagent-lifecycle-api.md`
  - `website/docs/user-guide/features/delegation.md`
- LSP:
  - `agent/lsp/client.py`
  - `agent/lsp/manager.py`
  - `agent/lsp/protocol.py`
  - `website/docs/user-guide/features/lsp.md`
- Memory/persistence:
  - `agent/memory_manager.py`
  - `tools/memory_tool.py`
  - `hermes_state_search.py`
  - `website/docs/developer-guide/memory-provider-plugin.md`
  - `website/docs/developer-guide/session-storage.md`
  - `website/docs/developer-guide/trajectory-format.md`
  - `website/docs/user-guide/features/memory.md`

## Final recommendation

Build the small, bulletproof agent kernel and let independent long-lived
extensions own every capability above it. Any language can participate through
the same JSON-RPC bus. Dynamic catalogs let `ygg-mcp` and similar bridges
publish what they discover; stable ownership and supervision isolate their
state; scoped host services let an orchestrator create child model sessions
without moving orchestration into core. The extra local hop is a deliberate
trade for replaceability, failure isolation, and a host whose responsibilities
stay easy to name.
