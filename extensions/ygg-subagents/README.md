# ygg-subagents

`ygg-subagents` is a small API `0.2` executable extension for Claude Code-like background workers. It launches named, single-purpose child conversations through Ygg's **host-owned `agent_sessions` service**. It is not an agent team, graph/recipe runtime, swarm, hosted-agent scheduler, or second Agent loop.

Version `0.2.0` targets exactly Ygg `0.6.0-dev`.

## Safety model

V1 is deliberately bounded, with the parent's full standard tool scope as the default grant:

- at most **eight active children** and thirty-two retained workers per parent owner;
- depth one; a recursively admitted descendant is immediately interrupted when its host path/depth is observed;
- four predefined profiles (`explore`, `review`, `test-analysis`, `research`);
- inherited model only (`model: "inherit"`), because the API `0.2` service does not accept a model override;
- requested tool scope is a non-empty duplicate-free subset of `read`, `search`, `edit`, `write`, and `bash`; the default grant is the full five-tool scope, and `tools: [read, search]` narrows a worker to hard read-only for pure investigations;
- wall-time, turn, and cost ceilings are optional per spawn: when omitted they inherit the parent session's ceilings (an unlimited parent remains unlimited); explicit values are bounded to 5 s–24 h, 1–256 turns, and 1–50,000,000 microdollars; returned output is 512–16,384 bytes;
- fresh child contexts inherit the parent's model, context/output limits, and optional session token ceiling exactly; an unlimited parent remains unlimited and the model-facing spawn schema has no separate token-budget field;
- strict owner derivation from `tool/call.context.resource_owner`; no tool schema accepts an owner;
- retry-safe spawn keys, bounded output/error retention, cooperative cancellation, explicit stop, and continue (steer active / resume settled).

`edit`, `write`, and `bash` are part of the default grant; the `tools`
argument narrows or restores any subset within the five-tool whitelist:
network, browser, computer control, mailbox/team
primitives, another agent primitive, and any other tool are rejected. The
canonical child policy keeps repository content and task text as data, not
policy, and never grants recursion or manager-generated commands.

API `0.2` creates the child with inherited model, cwd/workspace, environment,
sandbox, approval policy, and extension policy, but `agent/spawn.policy` is the
hard per-child boundary: Ygg installs a detached tool snapshot containing only
the granted tools (never collaboration or agent primitives), applies the
requested per-child turn/cost ceilings or inherits the parent's ceilings when
they are omitted, inherits the parent's context/output and optional
session-token settings without inventing a child ceiling, accounts cumulative
tokens/cost, caps UTF-8 summary bytes, and owns the absolute wall deadline.
Each child starts a fresh context; its usage is mirrored into the root ledger
for accounting only, never inserted into the parent's model context, and never
charged to the parent's own-context token ceiling. The
eight-active/depth-one/thirty-two-retained limits are also checked by the
real host service. Extension restart or absence of polling cannot relax those
limits. A shared cwd/filesystem is **not isolation**.

The host returns an opaque `agent-session:*` reference rather than the private
delegation JSONL path. Serve resolves that reference only by inventorying its
owner-private delegation directories, opens the transcript through a
no-follow descriptor, and exposes a locked read-only session projection. The
reference carries no filesystem path and cannot be used to submit another
prompt or bypass the worker policy.

There is no dedicated writer profile in V1: mutation capability is granted per
spawn through the requested tool list and is enforced by the host's scoped
tool snapshot, not by cooperative prompts alone. A worker granted `edit`,
`write`, or `bash` operates inside the same shared filesystem the parent sees,
so grant mutation only for tightly scoped, verifiable work.

## Kernel boundary

The package contains decomposition/completion policy, tool and command definitions, semantic projection, fixtures, and tests. It does **not** contain a model loop or session store.

Ygg owns:

- the child model conversations and durable session files;
- ancestry, concurrency/depth/team limits, inherited permissions, and cost limits;
- owner/principal checks for every `agent/*` request;
- persistence, cancellation, restart service continuity, and descendant shutdown;
- completion mailbox claim/ack and delivery as a legal new parent event/turn.

The extension calls only these SDK helpers, which map directly to API `0.2`:

- `spawn_agent` → `agent/spawn`;
- `list_agents` → `agent/list`;
- `wait_agents` → `agent/wait`;
- `interrupt_agent` → `agent/interrupt`;
- `send_agent_message` → `agent/message`;
- `follow_up_agent` → `agent/follow_up`.

`agent/message` steers an active worker and `agent/follow_up` resumes a
settled one; both are exposed only through `subagent_continue`. It does not
use the graph/recipe spike, built-in team mailboxes, or another scheduler.

## Install, enable, and trust

The release archive has one root directory named `ygg-subagents`. Install a local archive with:

```console
ygg extension install --path ./ygg-subagents-0.6.0-dev.tar.gz
```

Installation/discovery is inert: it does not enable, trust, or start the process. Explicitly enable and trust the selected manifest in full-access mode, preferably inside separate OS isolation:

The current workspace bundle can be rebuilt and installed deterministically with:

```console
./scripts/reinstall-ygg-subagents.sh
```

This updates `~/.ygg/extensions/ygg-subagents`; rebuilding `ygg` with
`cargo run` alone does not replace an already installed extension bundle.

Enable and trust it explicitly:

```console
ygg --enable-extension ygg-subagents --trust-extension ygg-subagents
```

`--safe-mode` never starts executable extensions. Use `/extensions` to enable or
disable installed executable bundles, and `/extensions status` to inspect source,
trust, API, generation, and negotiated features. Enabling never grants trust. The
tools return an explicit unavailable result when the trusted extension is not
running or the host has not offered its owner-bound `agent_sessions` service.

The bundle is self-contained and has no install hook or third-party dependency. `vendor/ygg_extension/` is a synchronized copy of Ygg's dependency-free Python SDK. Python 3.9+ is required at runtime.

The optional packaged skill at `skills/ygg-subagents/SKILL.md` is discovered after installation but remains inactive until the user explicitly loads it.

## Tools

### `subagent_spawn`

Launch a worker in the background by default:

```json
{
  "name": "explore-auth",
  "task": "Trace authentication ownership and report relevant files and invariants.",
  "profile": "explore",
  "model": "inherit",
  "tools": ["read", "search"],
  "timeout_seconds": 300,
  "max_turns": 8,
  "max_output_bytes": 8192,
  "max_cost_microdollars": 200000,
  "background": true,
  "idempotency_key": "auth-audit-v1"
}
```

There is intentionally no `max_tokens` argument. The child gets a fresh model
context with the parent's model context/output limits and inherits the parent's
optional cumulative session-token ceiling exactly (`null` remains unlimited).

If no key is supplied, the extension derives one from the complete canonical request. Keys are scoped by Ygg to the extension principal and durable session owner. Identical retries return the same retained owning-run child; when a new root run clears that tree, both the host and extension prune the stale key instead of returning a nonexistent worker. Reuse with different input fails. The orchestration fingerprint is also placed in the canonical child message so a restart cannot accidentally make host-visible input equality narrower than extension input equality.

The immediate result is an acknowledgement, not completion. Continue independent parent work and let Ygg deliver the worker's concise final output through its durable parent mailbox. Set `background: false` only when a bounded foreground wait is actually useful.

### `subagent_status`

Refresh the authoritative owned tree. `target` may be a displayed name, stable agent ID, or host path. Without a target it returns a compact list. It never accepts a caller-supplied owner and never infers state from output prose.

### `subagent_wait`

Wait 1–60 seconds for one target or all owned workers. The host reverse request is cancellable and sliced to keep cancellation responsive. Expiring or cancelling the wait leaves workers in the background; reaching a worker's wall deadline requests host interruption and produces the distinct `timed_out` state.

### `subagent_stop`

Provide exactly one of:

```json
{"target": "agent-1"}
```

or:

```json
{"all": true}
```

The host validates the target against the extension principal and current resource owner and interrupts the selected descendant tree. An accepted request remains `stopping` until a subsequent authoritative `agent/list` or `agent/wait` record reports the terminal interruption; acknowledgement alone is never presented as completion. Repeated stop on a terminal worker is a bounded no-op.

### `subagent_continue`

Provide a `target` (displayed name, stable agent ID, or host path) and a `message`:

```json
{"target": "explore-auth", "message": "Also check the revocation path."}
```

An active worker receives the message through `agent/message` as a queued
turn on its running session; a settled worker (`done`, `failed`,
`cancelled`, `stopped`, or `timed_out`) is resumed through `agent/follow_up`
as a new run of the worker's durable session, so the earlier conversation
context is retained. Workers still draining a stop (`stopping`) and orphaned
workers (host shutdown) are rejected with stable errors rather than raced.
The host clears a settled record's completion timestamp on resume, so elapsed
time always measures the current run.

## Lifecycle and restart behavior

Worker states are authoritative projections of `agent/list`/`agent/wait`:

- `pending` → `queued`;
- `running` → `running` (temporarily `waiting` during a wait call);
- `completed` → `done`, with bounded exact host output in detail/results;
- `failed` → `failed`, with a bounded error;
- `interrupted` → `cancelled`, or `stopped`/`timed_out` when the extension issued that reason;
- `shutdown`/missing active record → `orphaned`.

A supervised extension restart receives a new process generation but the host service retains trees by stable extension principal plus durable session owner. The next owner-scoped call resyncs with `agent/list`, marks recovered records as restarted, and restores the public task name, profile, idempotency fingerprint, host-created/started/completed/deadline timestamps, policy, usage, and stable session reference. Retrying the same spawn key returns the same child without creating another session. A complete process-host rebuild creates a new service boundary for mutation; retained transcript inspection remains separately read-only and provenance-authorized.

Outstanding API requests are cooperatively cancelled. Cancelling a wait does not stop the worker. If spawn cancellation races a durable host create, the required idempotency key makes the next identical call safe; unsafe ambiguous work is not replayed with new input.

On extension shutdown the local projection settles, while Ygg's API `0.2` process shutdown stops every child tree owned by the extension service. The shutdown callback never reuses a stale parent request ID.

## TUI and Serve presentation

The manifest declares:

```toml
[contributes]
presentation = true
```

The extension emits complete monotonic `presentation/update` snapshots using the generic host contract:

- compact status counts;
- content-free activity rows;
- stable list/tree nodes and parentage;
- queued/running/waiting/done/failed/stopped/cancelled/timed-out/orphaned/restarted distinctions;
- elapsed time, inherited model/profile, current structured phase/tool, turns, token/cost budgets, session/artifact references;
- selected detail with `parent > worker` breadcrumb, policy provenance, inherited cwd/sandbox/approval/environment facts, the host-observed terminal summary (unsafe controls visibly escaped), artifacts, bounded error, and restart state;
- declared inspect, stop, and stop-all actions routed only to the manifest command.

Prompts, tool arguments/results, and running model prose never appear in the
worker list or composer-adjacent activity block. The host returns per-worker
structured phase/current tool, host-observed tool calls, disjoint provider token
buckets, turn count, and priced cost. The extension places those values in
generic activity `metrics`; it never supplies terminal rows or footer text. In
the TUI, Ygg renders the latest owner-fenced worker activities immediately
above the composer from native `AgentEvent::DelegationUpdated` events; it does
not poll `/subagents status` for the composer block. A worker row has the
compact form `N Tool Calls • ↑input ↓output • $cost`; input includes the three
disjoint uncached/cache-read/cache-write buckets, while reasoning remains a
subset of output.

Before the root run settles, Ygg stops and briefly joins its children, sums each
child session's durable usage/cost records including picodollar remainders, and
writes one `delegated_agent` usage record per worker into the root session. The
live child total is included in the footer only until that durable handoff, so
delegated spend contributes exactly once to cumulative session cost and later
cost-limit checks.

The opaque worker resource reference is stable and owner-scoped.
Serve opens it only after host-written provenance binds the exact parent session,
path-free extension principal, and resource owner; the web view is locked and
read-only. The TUI's live block is host-rendered from semantic activity metrics;
no extension status or footer contribution is rendered. The no-argument
`/subagents` command opens a host-owned list: Up/Down moves between workers,
Enter opens the selected scrollable read-only transcript, and Escape or Left
returns to the list. The same owner-bound status command used by the live tick
and open panel reconciles authoritative `agent_sessions` state and publishes the
next complete presentation revision; the frontend keeps focus by stable node ID
and revalidates the latest owner-scoped reference before opening it.
`/extensions inspect agent-session:<digest>` remains the explicit reference
fallback. Both paths open only a child in the current parent's delegation team.
Neither frontend can submit prompts or mutate a worker; all mutation remains on
owner-bound `agent_sessions`. The package supplies no Rust TUI plugin, web code,
or frontend scheduler. Generic rendering, selection/navigation, reconnect and
instance/generation fencing, authenticated action routing, and Serve transport
are host-owned.

### `/subagents` headless/narrow fallback

```text
Subagents · 1 running · 1 done
├─ explore-auth         running    00:42  agent-1
└─ inspect-tests        done       01:08  agent-2
```

Use `/subagents inspect <name-or-id>` for cached read-only detail. The Ygg coding
host binds API `0.2` command requests to their host-derived owner, so an explicit
`/subagents stop ...` and the generic TUI/Serve stop action use the same
owner-checked `agent_sessions` path. A host or headless integration that omits
`context.resource_owner` fails closed without issuing a stop. The extension
never smuggles a stale request ID into a command. A cached list may lag; run
`subagent_status` from an active model turn to resync.

## Release smoke recipe

Compare measurements from the same task run once directly and once with up to two read-only workers:

```console
./release-smoke.py \
  --direct /tmp/direct.json \
  --subagents /tmp/subagents.json \
  --require-gain
```

Each input records accepted finding IDs, input/output tokens, wall time, CPU time, peak RSS, duplicate findings, and failure classes. The script reports quality gain and resource deltas. It consumes caller-captured measurements and never starts a provider or makes a network call during packaging.

A deterministic fixture smoke is:

```console
./release-smoke.py \
  --direct fixtures/smoke/direct.json \
  --subagents fixtures/smoke/subagents.json \
  --require-gain
```

For a real evaluation, keep the prompt, model, reasoning, workspace revision, and acceptance rubric fixed. Count only reviewed/accepted unique findings; record timeouts, provider failures, cancellation, duplicate findings, and policy violations rather than discarding failed trials.

## Tests

From the package root:

```console
python3 -m unittest discover -s tests -v
```

The package-owned fake host service covers owner/principal isolation, concurrency, duplicate keys, cancellation races, timeout interruption, supervised restart/resync, completion claim/ack and legal parent-turn delivery, session/export inspection, and descendant shutdown. Protocol tests run the vendored SDK over JSON-RPC streams and verify negotiation, owner correlation, presentation updates, `/subagents`, cancellation, and graceful shutdown. Release tests verify manifest/archive bounds, SDK synchronization, fixtures, executable bits, and the smoke report.
