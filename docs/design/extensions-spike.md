# Executable Extensions Spike: Daily-Driver Capabilities

> **Status:** research spike and architectural recommendation
> **Observed:** 2026-08-16
> **Ygg target:** workspace version `0.4.0`, repository rooted at
> `84c2fb8b654b107e869ed9b8add29b3a50043e60`
> **Capabilities:** WebSearch, BrowserUse, Caffeinate, Subagents, MCP,
> Language Server Protocol (LSP), and Memory

## Executive decision

Ygg should keep its language-neutral JSON-RPC subprocess boundary, but API
`0.1` is not yet a safe foundation for stateful daily-driver features. Before
shipping advanced executable extensions, Ygg needs end-to-end cancellation,
correlated progress, structured/media results, complete terminal lifecycle
outcomes, host-owned policy decisions, and explicit drain/restart semantics.

The central architectural conclusion is:

> **Installability is not runtime ownership.** A feature may be distributed by
> an extension package while Ygg still owns the lifecycle, policy, persistence,
> and model-facing tool.

Use three integration shapes rather than forcing every feature into an ordinary
`tool/call`:

1. **Ordinary executable tools** for bounded request/response work.
2. **Typed provider adapters** for replaceable web, browser, or memory
   backends, supervised by a host manager.
3. **Direct standard-protocol managers** for MCP and LSP, where wrapping one
   subprocess protocol inside another would lose lifecycle and security
   semantics.

Recommended ownership:

| Capability | Ygg host/core owns | Extension/package seam |
| --- | --- | --- |
| WebSearch | model-facing tool, result IDs, citations, cache, URL/network policy, truncation | search/fetch provider adapter |
| BrowserUse | sessions/tabs, action policy, confirmations, artifacts, cancellation, cleanup | trusted Playwright/CDP or cloud-browser adapter |
| Caffeinate | turn/task leases and platform inhibitor | none; platform backend is a core implementation detail |
| Subagents | agent tree, context, budgets, permissions, messaging, persistence, cancellation, workspaces | declarative roles/prompts/tool profiles |
| MCP | transports, OAuth/secrets, catalogs, approvals, reconnect, cancellation, progress | server configuration/discovery and optional auth helpers |
| LSP | server processes, document versions, requests, diagnostics, cancellation, cleanup | declarative server descriptors/install guidance |
| Memory | scopes, provenance, prompt injection, frozen snapshots, retention, writes, consolidation triggers | storage/retrieval/consolidation provider adapter |

WebSearch is the best first executable-provider vertical slice: it exercises
network policy, structured results, progress, cancellation, citations, and
caching without BrowserUse's long-lived mutable state. Caffeinate should move
into core immediately as the first consumer of a true terminal turn lifecycle.
Subagent work can proceed in parallel in core; it should not wait for extension
API `0.2`.

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

### Gaps that block daily-driver extensions

| Gap | Observed API `0.1` behavior | Consequence |
| --- | --- | --- |
| Operation cancellation | `ProcessTool::execute` does not select on `ToolContext.cancellation`; `ProcessConnection::request_inner` has no request-cancel message | Aborting a turn drops the waiter but does not tell the extension to stop work or side effects |
| Framed-write cancellation | Dropping `FramedWriteGuard` marks the whole connection closed because a write may be partial | Cancellation can sacrifice the persistent service instead of cancelling one operation |
| Late replies | Dropping `PendingRegistration` only removes the pending sender | There is no explicit cancellation acknowledgement or observable operation outcome |
| Correlated progress | Native progress exists, but extensions can only emit general notifications/status events | Concurrent calls cannot reliably attribute progress or prompts to the initiating operation |
| Result fidelity | `ToolCallOutput` accepts string `content`, `is_error`, and `metadata`; `ProcessTool` converts success to `ToolOutput::new(content)` | Metadata is discarded and executable extensions cannot return native image/audio media |
| Terminal lifecycle | Hooks are only `before_prompt`, `after_response`, `before_tool_call`, and `after_tool_call`; product paths call `after_response` after successful complete responses | Failure, cancellation, interruption, frontend loss, and shutdown are not terminal hook outcomes |
| Policy enforcement | An extension can request a generic confirmation, but it runs with the user's privileges and may bypass that request | Confirmation is cooperative UX, not a security boundary |
| Service health | Startup and replacement are bounded, but there is no common ready/degraded/parked/backoff state machine | Persistent browser/MCP/LSP/provider failures become ad hoc and noisy |
| Reload drain | A replacement is swapped and the old connection is shut down; contribution changes are rejected | In-flight work has no documented drain/cancel rule, and schema changes require a larger rebuild |
| Process lifetime | API `0.1` has one resident lifetime: enabled, trusted processes start during product construction and remain until reload, shutdown, or connection failure | On-demand, per-call, and health-managed lifetimes are not expressible |

The Caffeinate example demonstrates the terminal-lifecycle problem. It acquires
at `before_prompt` and releases at `after_response`, shutdown, or stream loss.
An aborted or failed run can leave `/usr/bin/caffeinate -i -t 1800` active until
its fallback timeout. The timeout bounds the leak; it does not establish correct
ownership.

## Cross-product comparison

Cells summarize the inspected snapshot, not an evergreen product claim.

| Capability | Current Ygg | OpenAI Codex | Claude Code | Google Antigravity | Hermes Agent |
| --- | --- | --- | --- | --- | --- |
| **WebSearch** | No first-party search manager; an extension can return text only | Open-source `web.run` extension covers search/image search/open/click/find/screenshot and vertical data commands, with typed begin/end items and result payloads (**OSS**) | Packaged `WebSearch` and `WebFetch` schemas expose domain filtering, URL fetch, processed text, and structured hit URLs/titles (**PACKAGE**) | `NewSearchWebTool` and related symbols indicate an integrated search tool (**STATIC**) | Brave, DDGS, SearXNG, Exa, Parallel, Tavily, and Firecrawl adapters; extraction/cache limits, secret checks, DNS-aware SSRF checks, pinned-IP transport (**OSS**) |
| **BrowserUse** | No browser manager; native media exists but subprocess tools cannot bridge screenshots | Bundled Browser plugin `26.803.41515` documents persistent tabs/REPL handles, semantic DOM interaction, post-action checks, screenshots, scoped CDP, untrusted-page rules, and action-time confirmation (**PACKAGE**) | No equivalent native persistent browser was established; official marketplace distributes Playwright as an external MCP server (**PACKAGE**) | Browser tools and `BrowserSubagent` symbols indicate integrated browsing/subagent paths (**STATIC**) | Local/cloud providers, CDP and Browser Use, semantic accessibility snapshots, task-isolated persistent sessions, reaping, dialogs, frames/OOPIF, redaction, and network policy (**OSS**) |
| **Caffeinate** | macOS example extension; success-only hook ownership leaks on abort/failure | Core cross-platform `SleepInhibitor`: macOS IOKit assertion, Linux helper backends with parent-death handling, Windows power request, drop cleanup (**OSS**) | Binary strings indicate macOS `caffeinate`, Linux `systemd-inhibit`, restart/spawn-error/explicit-stop paths (**STATIC**) | No sleep-inhibitor symbols found in the inspected binary (**NEGATIVE/STATIC**) | No sleep-inhibitor implementation found in the inspected tree (**NEGATIVE**) |
| **Subagents** | No committed subsystem in the cited HEAD; a substantial local untracked V2 prototype exposes spawn/follow-up/message/wait/list/interrupt | Hierarchical registry, roles/paths, optional turn forking, follow-ups, messaging, waits, interrupts, shared depth/concurrency controls (**OSS**) | Packaged `Agent` schema and `claude agents` expose background agents, models/effort/permissions, addressable names, worktree/remote isolation, output and stop controls (**PACKAGE/CLI**) | Agent derivation, cancellation, workspace isolation, and subagent-management symbols are present (**STATIC**) | Isolated child conversations, summary return, parallel/background/nested agents, steering/stopping, limits, stalls/timeouts, worktrees, cost rollups, lifecycle plugins, durable async delivery (**OSS**) |
| **MCP** | No native manager; a generic extension could wrap MCP but would hide important semantics | Stdio and Streamable HTTP, OAuth/config/env/headers, parallel/deferred startup, reusable connections, required/optional servers, cached revisioned catalogs, resources, cancellation, elicitation, approval policy (**OSS**) | `mcp add/get/list/remove/login/logout`, stdio/HTTP, headers/env, user/local/project scopes, project-config approval, health and OAuth login (**CLI/PACKAGE**) | MCP manager/call symbols plus tools/prompts/resources/progress-related symbols indicate broad support (**STATIC**) | Stdio, Streamable HTTP and SSE; reuse, keepalive, reconnect/backoff/parking/revival, pagination/refresh, tools/resources/prompts, sampling, elicitation, structured/media content, cancellation and cleanup (**OSS**) |
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

### 1. Managers own invariants; adapters own backend variation

Hermes's web, browser, and memory provider APIs and Codex's memory backend are
useful extension seams. In each good example, a central manager still owns
session timing, cache/prompt behavior, cleanup, synchronization, and policy.
Ygg should copy that split rather than allow a provider subprocess to redefine
the product lifecycle.

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
losing completion during a crash. Ygg should use the same principle for
subagents and other detached work.

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
Agent / turn coordinator
├── terminal lifecycle + cancellation
├── central policy / approval service
├── artifact and structured-output store
├── common service supervisor
│   ├── WebSearch manager ── typed provider extension
│   ├── Browser manager ──── trusted browser sidecar/provider
│   ├── MCP manager ───────── direct MCP transports/servers
│   ├── LSP manager ───────── direct LSP transports/servers
│   └── Memory manager ────── local backend or typed provider extension
├── Delegation manager ────── child Ygg agents/worktrees
└── Sleep inhibitor ───────── platform backend

Executable-extension runtime (JSON-RPC/JSONL)
├── bounded ordinary tools, commands, hooks, UI
├── typed provider contracts
└── package-supplied declarative contributions
```

A common service supervisor should provide process groups, generation IDs,
startup/shutdown deadlines, health, backoff, drain, and diagnostics. It should
not erase domain protocols: MCP and LSP managers still speak MCP and LSP
respectively.

### Core-versus-extension decision rule

Keep a responsibility in core if any of these are true:

- correctness depends on every terminal path;
- it changes or persists conversation context;
- it allocates model, token, concurrency, or workspace budgets;
- it must enforce approval, secret, network, or filesystem policy;
- it multiplexes a long-lived process across calls;
- it must work identically in interactive, print, plain, RPC, and serve modes;
- a crash can leak a child process, tab, lock, inhibitor, or background result.

Use an extension/provider seam when the implementation is replaceable, has a
narrow typed contract, can be restarted without corrupting host state, and
either uses host-brokered authority or is explicitly accepted as trusted local
code whose extra authority cannot be technically constrained yet.

### Registration shape

Do not add one generic "managed service" contribution. Package manifests should
use kind-specific declarations:

- executable `web_search`, `browser`, or `memory` provider adapters name an
  entrypoint and the typed adapter contract they implement;
- MCP server and language-server entries are declarative launch/transport
  descriptors; Ygg speaks MCP/LSP directly rather than routing either protocol
  through `tool/call`;
- subagent roles/prompts are declarative inputs to core orchestration, not child
  processes that own the agent tree.

Namespace each contribution by package, kind, and name. Initialization must
exactly match the declared contribution and may negotiate only additive API
`0.2` features. A schema or contribution-set change creates a new generation
and triggers catalog re-registration; existing handles never cross generations.
Manifest declarations remain consent and discovery metadata, not a sandbox.

## Capability recommendations

### WebSearch

#### Product contract

Expose a host-owned search surface with at least:

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

The provider adapter performs provider-specific request translation, response
normalization, and declares the authentication scheme it requires. The host
owns:

- credential lookup and narrowly scoped authentication injection;
- result-reference allocation and citation rendering;
- bounded raw-content cache and expiry;
- output truncation and deterministic model-visible text;
- query/URL redaction and secret detection;
- endpoint allow/deny policy and SSRF protection;
- retries, cancellation, telemetry, and billing-aware limits.

Provider credentials should come from a host secret broker or scoped launch
environment, never an ambient dotenv inherited by every extension. For network
policy to be enforceable, the preferred provider contract has the host perform
HTTP through a scoped broker while the adapter builds provider requests and
normalizes responses. An adapter that opens its own sockets is trusted local
code; Ygg may validate its declared target and results, but cannot claim to
constrain a malicious adapter without an OS sandbox.

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

Secrets should be filled through scoped host tokens where possible and should
not be returned in snapshots, logs, screenshots, or model-visible text. Raw CDP
access needs a small allowed domain set or a separate high-risk capability.
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

The host owns these rules. A trusted Playwright/CDP sidecar may be distributed
as an executable extension, but it implements actions against already admitted
host operations rather than owning confirmations or session policy.

### Caffeinate

Implement sleep inhibition in core as a reference-counted RAII lease:

- acquire after an eligible turn/task is admitted;
- keep one platform assertion while at least one eligible owner is active;
- release exactly once on completed, failed, cancelled, interrupted, frontend
  disconnect, and shutdown paths;
- make repeated acquire/release idempotent;
- detect and reacquire if a helper process exits unexpectedly;
- expose failure as a non-fatal status/diagnostic.

Prevent idle **system** sleep, not display sleep or explicit user sleep.
Recommended platform backends follow the Codex implementation shape:

- macOS: native IOKit power assertion; `/usr/bin/caffeinate` may be a fallback;
- Linux: logind inhibitor where practical, with `systemd-inhibit` and
  `gnome-session-inhibit` helper fallbacks and parent-death/process-group cleanup;
- Windows: `PowerCreateRequest`/`PowerSetRequest` with system-required;
- unsupported platforms: no-op with one bounded diagnostic.

A maximum helper duration is a useful final safety net, but a timer must not be
the primary release mechanism. Extension hooks may observe turn outcomes, but
core resource cleanup must never depend on successful hook delivery.

### Subagents

Subagents are agent orchestration, not an executable-extension tool
implementation. Core must own:

- stable agent IDs, parent/child tree, role/path metadata, and status registry;
- blank/summary/full or explicit turn-fork context modes;
- model, effort, token, time, depth, and global concurrency budgets;
- permission inheritance (a child cannot exceed its parent/host policy);
- spawn, follow-up, message, wait, list, interrupt, and steering operations;
- cancellation propagation and explicit detach rules;
- workspace selection and optional temporary git worktrees;
- completion summaries, artifacts, cost rollups, and cleanup;
- durable background dispatch and completion delivery.

The local untracked V2 prototype already uses the right control vocabulary:
`spawn_agent`, `followup_task`, `send_message`, `wait_agent`, `list_agents`, and
`interrupt_agent`. It should be evaluated and integrated as core work; because
it is uncommitted, it is not treated as shipped behavior in this spike.

Background delivery should use a durable state progression such as:

```text
queued -> running -> completed|failed|cancelled -> claimed -> acknowledged
```

After restart, an unacknowledged completion can be claimed again. Delivery
enters the parent as a new legal turn/event; it does not splice a message into
an already completed prefix. Child agents should return a concise summary plus
references to durable artifacts/worktree changes rather than copying their full
conversation into the parent.

Extensions may contribute declarative roles, prompts, or restricted tool/model
profiles. They must not own the child conversation, registry, or workspace
lifecycle.

### MCP

Build MCP as a host manager speaking MCP directly. Do not require users to wrap
an MCP server inside a generic Ygg extension: that would duplicate framing and
hide catalog revisions, annotations, progress, cancellation, sampling,
elicitation, resources, and transport health.

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

Ygg should directly manage LSP processes and let extension packages contribute
declarative descriptors such as:

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

The manager owns:

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
one-process-per-tool-call LSP extension would lose indexes, document versions,
and diagnostics, so it is the wrong ownership boundary.

### Memory

Memory needs explicit layers instead of one unscoped text blob:

| Layer | Typical scope | Purpose | Injection policy |
| --- | --- | --- | --- |
| User rules/preferences | user/profile | durable user choices and constraints | bounded frozen session snapshot |
| Project knowledge | canonical project | repository facts not already obvious from files | bounded frozen session snapshot with citations |
| Episodic summaries | session/task/rollout | what happened and why | retrieved or compacted, not globally injected |
| Transient retrieval | current turn | relevant prior sessions/artifacts | query-time, bounded, provenance-preserving |
| Procedural skills | installed/project resource | reusable workflows | keep in the existing skill system, not memory |

Core owns scope, provenance, prompt budget, retrieval trigger, write policy,
editing/deletion, retention, and consolidation timing. A backend extension may
search, store, or propose a consolidation, but it cannot inject arbitrary
system context or commit a cross-scope write without host validation.

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

API `0.1` should remain frozen for existing simple extensions. Introduce `0.2`
for the breaking result/lifecycle semantics below. Continue exact version
selection in the manifest, then negotiate additive features during
`initialize`; do not infer support from extension version strings.

A sketch:

```json
{
  "protocol": {
    "version": "0.2",
    "required_features": ["request_cancellation", "content_parts"],
    "optional_features": ["request_progress", "artifacts", "lifecycle_events"],
    "limits": {"max_concurrent_requests": 4}
  }
}
```

The initialization response returns the supported subset and an accepted limit;
the host caps every value. Missing required features reject the candidate before
registration. The SDK reader loop must never execute a handler inline: it keeps
reading control frames and schedules handlers behind the negotiated concurrency
semaphore, which is necessary for a queued or running handler to observe
cancellation. Domain provider contracts have their own version so a
browser-provider change does not require changing the base JSON-RPC framing
version.

### 1. Real request cancellation

Use the originating host JSON-RPC ID as the cancellation target, following the
LSP convention:

```json
{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":42,"reason":"user"}}
```

Required semantics:

1. Before the writer starts a frame, cancellation removes the request from the
   queue and sends nothing.
2. Once a frame write starts, the writer completes that frame without
   cancellation; the host then sends one idempotent cancellation notification.
3. The SDK exposes an ambient cancellation token/event to the active handler.
4. A cooperative extension completes the original request with a cancellation
   error (use `-32800`) or may win the race with a normal result.
5. The host tombstones cancelled IDs for a bounded period so a late response is
   ignored and diagnosed rather than killing the connection.
6. After a configurable grace period, a non-cooperative process is terminated
   or its domain resource is invalidated according to manager policy.
7. Side-effecting operations report cancellation as "requested" rather than
   claiming rollback; an ambiguous external outcome is never replayed.

Replace direct cancellable writes under a mutex with a dedicated bounded writer
task that serializes complete frames. Dropping a request waiter must not drop a
partially written future or close an otherwise healthy persistent connection.
Timeout and host shutdown use the same cancellation machinery but retain
distinct terminal reasons.

### 2. Correlated progress and extension-originated requests

Extensions need a request-scoped notification:

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

Initial event variants should map directly to native progress:

- `status {message, current?, total?, unit?}`;
- `output {stream: stdout|stderr, encoding: utf8|base64, data}` with bounded
  payloads;
- a dropped/coalesced diagnostic generated by the host, not by the extension.

Sequences are monotonic per request. The host applies existing 8 KiB chunking,
bounded-channel dropping, and aggregate drop reporting. Progress is never added
to the model conversation or session transcript as a result.

Every extension-originated confirmation, input, artifact publication, or other
operation-specific request must carry `parent_request_id`. Global notifications
and status contributions may omit it. This prevents concurrent extension calls
from racing to display or answer another call's prompt. When the parent settles,
the host atomically denies/cancels all unresolved child requests; late replies
are tombstoned. The same cancellation notification may be used in the opposite
direction when an extension abandons one of its host requests.

### 3. Structured and media output

Replace string-only subprocess output with an MCP-like result that bridges to
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
- optional `structured_content` is validated against a declared output schema,
  retained for UI/session use, and lowered to the model only by host policy;
- image/audio parts become existing `ygg_ai::Media` values;
- arbitrary local paths and remote media URLs are not trusted directly;
- provider references are accepted only from the matching host/provider manager,
  not from a generic extension;
- `metadata` remains non-model-visible but must actually be retained, unlike the
  current subprocess adapter.

The current `ToolOutput` struct carries text plus media only. Extend it, or its
canonical persisted result envelope, with optional `structured_content` and
vetted metadata rather than creating a subprocess-only parallel model. Provider
lowering then preserves supported media and emits explicit placeholders when a
provider cannot represent a part.

Large media must be out of line. Give each process generation a host-owned
scratch directory and support `artifact/publish` with either a small bounded
inline payload or a relative scratch path plus claimed MIME type, size, and
SHA-256. The host opens it with descriptor-relative, no-follow semantics, checks
size/digest/type, ingests it, and returns an opaque artifact ID. A browser
screenshot should not need to fit base64 plus JSON inside the 1 MiB
control-frame limit.

### 4. Complete lifecycle events

Keep interceptable `before_*` hooks as bounded requests. Add observational,
non-veto lifecycle notifications:

- `session_started`, `session_settled`;
- `turn_started`, `turn_settled`;
- `tool_started`, `tool_settled` where an extension subscribes to global tool
  observation.

Every admitted turn receives exactly one host-side `turn_settled` outcome:

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
process may already have failed. Therefore host cleanup, Caffeinate release,
and persistence completion remain core finalizers, never extension hooks.

Deprecate success-implying `after_response` in favor of `turn_settled`; keep it
for `0.1` compatibility only.

### 5. Host-mediated policy intents

Retain `confirmation/request` as cooperative UI, add parent correlation, and do
not describe it as enforcement. For host-managed capabilities, classify a
structured action intent before execution:

```json
{
  "kind": "external_side_effect",
  "operation": "browser.submit_form",
  "target": {"origin": "https://example.com", "label": "Publish comment"},
  "data_classes": ["user_text"],
  "adapter_hints": {"read_only": false, "destructive": false}
}
```

The host derives `allow`, `ask`, or `deny` from authoritative context. Adapter
hints can only increase caution, never lower it. If a cooperative generic
extension receives an approval token, bind the single-use token to the
canonical intent hash, process generation, parent request, and expiry.

This token still cannot constrain malicious unsandboxed code. Actual
enforcement requires either a host-executed broker (network, secret fill,
artifact write, browser action) or a future OS sandbox. Ygg must continue to say
so plainly.

### 6. Reload, drain, and health

Standardize process states:

```text
discovered
  -> starting -> initializing -> ready
  -> draining -> stopped
  -> degraded/crashed -> backoff -> starting
  -> parked (permanent config/auth/protocol failure)
```

Reload sequence:

1. start and fully negotiate candidate generation `N+1` while `N` remains ready;
2. if schemas changed, build a complete replacement registry generation rather
   than mutating registered tools in place;
3. atomically route new operations to `N+1`;
4. mark `N` draining and stop new dispatch;
5. allow explicitly drainable reads to settle and cancel the rest by deadline;
6. send shutdown, then terminate the process group if needed;
7. reject all stale progress, handles, approvals, and confirmations from `N`.

Never automatically replay an unresolved unsafe tool call. Retry only calls
whose domain contract and idempotency policy permit it. Browser tab, MCP server,
and other remote handles include the owning generation and fail stale rather
than aliasing replacement state.

Expose health transitions and last bounded error to `/extensions` and machine
interfaces. Optional services degrade without blocking startup; required
services fail with an actionable reason.

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
sleep leases have an explicit owner and generation:

```text
absent -> creating -> active -> closing -> closed
                    \-> lost/invalidated
```

Owner settlement triggers cleanup even if extension event delivery fails.
Dropping a Rust handle should be a final safety path, not the only normal path.

### Approval lifecycle

```text
intent received
  -> host classification
     -> deny
     -> ask -> user deny
            -> allow once
            -> remember scoped rule
     -> allow by existing scoped rule
  -> execute admitted operation
  -> record bounded outcome
```

Cancellation while waiting is denial. Headless frontends deny unresolved
interactive prompts unless an explicit non-interactive policy already allows
the exact intent. Remembered rules bind to identity and schema revisions and
must never include secret values in logs/session state.

### Trust boundary

There are two materially different threat models:

1. **Trusted executable extension:** arbitrary local code running as the user.
   Manifests, prompts, and approval tokens are consent/UX, not containment.
2. **Untrusted remote content/server data:** pages, search results, MCP tool
   descriptions/results, LSP text, and retrieved memory. These remain data and
   cannot grant local authority even when transported by a trusted process.

Daily-driver managers should minimize the first boundary by keeping secrets,
policy, persistence, and consequential actions in the host. A future OS sandbox
can strengthen generic extensions, but protocol design should not pretend it
already exists.

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

The ordering below is dependency-driven. Parallel core work is called out
explicitly.

### Phase 0 — make current behavior truthful and terminal

- Reconcile the protocol reference with executable behavior and add wire
  conformance fixtures.
- Add a host-owned exactly-once turn terminal outcome across every frontend.
- Move Caffeinate into core using an RAII/reference-counted lease.
- Define one authoritative set of limits and shutdown timing semantics.
- Preserve and test process-tree cleanup on cancellation and shutdown.

### Phase 1 — extension API `0.2` foundation

- Add the serialized writer task, request cancellation, tombstones, and SDK
  cancellation tokens.
- Add request-scoped progress and parent correlation for extension-originated
  requests.
- Bridge content parts, structured output, and native media through the artifact
  store.
- Add lifecycle notifications and structured policy intents.
- Implement ready/draining/degraded/parked states and explicit reload drain.
- Keep API `0.1` available for simple existing extensions; do not emulate `0.2`
  guarantees for a `0.1` process.

### Phase 2 — proving slices and parallel core work

- Ship WebSearch manager plus one first-party provider adapter as the protocol
  vertical slice.
- Add shared URL/SSRF/redirect policy and citation/cache infrastructure.
- In parallel, integrate and harden core V2 delegation with durable background
  completion and workspace isolation.
- In parallel, ship manual scoped memory plus local session search and frozen
  session snapshots; defer automatic extraction.

### Phase 3 — common managed services

- Build the reusable service supervisor and native MCP manager.
- Add direct LSP management using the same process/health primitives but LSP's
  native cancellation/document semantics.
- Support declarative package contributions for MCP discovery and language
  server descriptors without proxying their protocols through `tool/call`.

### Phase 4 — BrowserUse

- Ship a trusted Playwright/CDP sidecar/provider and host Browser manager.
- Add semantic snapshots, generation-scoped handles, post-action observations,
  screenshots/artifacts, task isolation, confirmations, and reaping.
- Add pixel/CUA fallback only after semantic actions and safety evaluation are
  reliable.

### Phase 5 — automatic memory and provider breadth

- Add root-session extraction/consolidation with provenance, pruning, privacy
  controls, and evaluation.
- Add replaceable memory/search/browser providers after the built-in contract is
  stable.
- Expand WebSearch commands and MCP/LSP operations based on observed use, not
  speculative parity.

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
  paths, traversal, links, replacement races, and stale generations.
- Every terminal frontend path emits one outcome and releases every owned lease.
- Reload never exposes a half-built registry or aliases stale handles.

### Security

- Missing/false/malformed provider and MCP hints cannot lower host risk.
- Stale or differently hashed approval intents cannot reuse a token/rule.
- Headless unresolved approval is denial.
- SSRF tests cover IPv4/IPv6 loopback/private/link-local/metadata, alternate IP
  forms, mixed DNS answers, redirect pivots, rebinding/pinning, and proxy mode.
- Browser page text and MCP descriptions cannot inject policy decisions.
- Secrets do not appear in progress, diagnostics, screenshots, session exports,
  telemetry, or memory.

### Capability-specific

- Caffeinate releases on success, model error, tool error, user cancel,
  frontend disconnect, panic/drop simulation, and host shutdown.
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
correction. The canonical prose now describes the inspected API `0.1` runtime;
the rows remain here as an audit record and as conformance-test requirements:

| Topic | Previous documentation | Inspected runtime | Resolution |
| --- | --- | --- | --- |
| Initialization contributions | Protocol reference said a process could omit a manifest-declared tool/command | `ensure_same_contributions` requires exact, duplicate-free set equality | Reference now requires exact duplicate-free name sets; SDK guide already did |
| Hook payloads | `after_response` documented `message_id`; tool hooks documented `tool` | Product sends `{response}`; tool hooks send `name`, and `after_tool_call` also sends `arguments` | Reference and SDK guide now list the serialized runtime payloads |
| Terminal hook | Name and examples suggested a general response lifecycle | `after_response` is invoked only after successful complete responses in inspected frontend paths | API `0.1` success-only behavior is explicit; `turn_settled` remains an API `0.2` requirement |
| Tool metadata | Described as retained for frontend/renderer use | `ProcessTool` discards `metadata` when constructing `ToolOutput` | API `0.1` docs now say it is accepted but discarded, with no retention guarantee |
| Confirmation string ID | Reference said at most 64 bytes | Runtime accepts up to 256 bytes | Reference and SDK guide now specify 256 UTF-8 bytes |
| Shutdown timing | Reference listed one 3-second grace | `ExtensionRuntimeConfig` defaults to 2 seconds per connection stage; normal product shutdown also has a separate 3-second aggregate deadline, while coordinated-signal exits impose a 1.4-second outer cap before force-kill | Reference now names both normal 2-second stages, the normal 3-second aggregate deadline, and the 1.4-second signal fast path |
| Shutdown signal | Reference header said the host closes stdin | Runtime first sends a JSON-RPC `shutdown` request, waits, then terminates as needed | Reference and SDK guide now document request/ack/exit/kill ordering; stdin EOF is loss/final teardown |
| Contact policies | `permanent`, `on_demand`, `auto_permanent`, and `tool_execute` were listed | No manifest field or dispatch implementation was found; enabled/trusted processes start during product construction | Unsupported policies were removed; the single resident API `0.1` lifetime is documented |
| Manifest limit | Shared resource docs listed only 256 KiB | Product resolver uses its 256 KiB resource bound then parses; direct `ExtensionManifest::load` defaults to 64 KiB | Both layers and the actual product path are now named |
| Caffeinate parent binding | Example README said `-w` bound the inhibitor to the extension PID | Example command uses `-i -t 1800` and no `-w` | README now documents timeout plus explicit cleanup without claiming PID binding |

The source implementation and executable conformance tests should be normative.
Protocol prose should be generated or checked against shared constants and
serialized fixtures where practical.

## Approaches not recommended

- Do not add more success-only hooks and use them as resource cleanup.
- Do not call a generic confirmation API "enforced" for unsandboxed code.
- Do not implement BrowserUse as a fresh one-shot tool process.
- Do not merge search retrieval and mutable browser control into one manager.
- Do not proxy MCP or LSP through generic extension tool calls.
- Do not let a subprocess own child-agent conversations or global budgets.
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
- The worktree was dirty during inspection. In particular,
  `crates/ygg-agent/src/delegation.rs` and
  `crates/ygg-agent/tests/delegation.rs` were untracked. Their V2 design informed
  the recommendation but is not attributed to the commit or treated as shipped.
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

Do not build seven independent generic subprocess tools. First make Ygg's
transport and terminal lifecycle trustworthy, then build host managers with
narrow replaceable seams. That preserves the language-agnostic extension API
without outsourcing the invariants that make WebSearch, BrowserUse, MCP, LSP,
Subagents, Memory, and Caffeinate safe and pleasant as daily drivers.
