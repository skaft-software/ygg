# ygg-subagents

`ygg-subagents` is a small API `0.2` executable extension for Claude Code-like background workers. It launches named, single-purpose child conversations through Ygg's **host-owned `agent_sessions` service**. It is not an agent team, graph/recipe runtime, swarm, hosted-agent scheduler, or second Agent loop.

Version `0.1.0` targets exactly Ygg `0.5.0`.

## Safety model

V1 is deliberately read-only and bounded:

- at most **two active children** and sixteen retained workers per parent owner;
- depth one; a recursively admitted descendant is immediately interrupted when its host path/depth is observed;
- four predefined profiles (`explore`, `review`, `test-analysis`, `research`);
- inherited model only (`model: "inherit"`), because the API `0.2` service does not accept a model override;
- requested tool scope is a non-empty subset of `read` and `search` only;
- 5–900 second wall-time request, 1–12 turns, 512–16384 output bytes, 1k–64k tokens, and bounded microdollar reservations;
- aggregate reservations of 96k tokens and 500,000 microdollars;
- strict owner derivation from `tool/call.context.resource_owner`; no tool schema accepts an owner;
- retry-safe spawn keys, bounded output/error retention, cooperative cancellation, and explicit stop.

Mutation tool names cannot enter the child request through model-generated `tools` data: the JSON Schema and runtime both accept only `read` and `search`. The canonical child policy also prohibits shell/process, edit/write, network/browser/computer control, mailbox/team primitives, recursion, and manager-generated commands. Repository content and task text are explicitly data, not policy.

API `0.2` creates the child with inherited model, cwd/workspace, environment,
sandbox, approval policy, and extension policy, but `agent/spawn.policy` is the
hard per-child boundary: Ygg installs a detached `read`/`search` tool snapshot
(no mutation or collaboration tools), lowers parent turns/cost when stricter,
accounts cumulative tokens/cost, caps UTF-8 summary bytes, and owns the absolute
wall deadline. The two-child/depth-one/aggregate reservation limits are also
checked by the real host service, so extension restart or absence of polling
cannot relax them. A shared cwd/filesystem is **not isolation**.

The host returns an opaque `agent-session:*` reference rather than the private
delegation JSONL path. Serve resolves that reference only by inventorying its
owner-private delegation directories, opens the transcript through a
no-follow descriptor, and exposes a locked read-only session projection. The
reference carries no filesystem path and cannot be used to submit another
prompt or bypass the worker policy.

There is no writer profile in V1. Adding one requires evidence from the read-only path plus a host-enforced, single-writer approval/tool-policy boundary. Cooperative prompts alone are not sufficient.

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
- `interrupt_agent` → `agent/interrupt`.

It does not use `agent/message`/`agent/follow_up`, the graph/recipe spike, built-in team mailboxes, or another scheduler.

## Install, enable, and trust

The release archive has one root directory named `ygg-subagents`. Install a local archive with:

```console
ygg extension install --path ./ygg-subagents-0.1.0.tar.gz
```

Installation/discovery is inert: it does not enable, trust, or start the process. Explicitly enable and trust the selected manifest in full-access mode, preferably inside separate OS isolation:

```console
ygg \
  --enable-extension ygg-subagents \
  --trust-extension ygg-subagents
```

`--safe-mode` never starts executable extensions. Use `/extensions` to inspect source, trust, API, generation, and negotiated features. The tools return an explicit unavailable result when the selected model/reasoning mode does not offer `agent_sessions`.

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
  "max_tokens": 32000,
  "max_cost_microdollars": 200000,
  "background": true,
  "idempotency_key": "auth-audit-v1"
}
```

If no key is supplied, the extension derives one from the complete canonical request. Keys are scoped by Ygg to the extension principal and durable session owner. Identical retries return the same child; reuse with different input fails. The orchestration fingerprint is also placed in the canonical child message so a restart cannot accidentally make host-visible input equality narrower than extension input equality.

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

The host validates the target against the extension principal and current resource owner and interrupts the selected descendant tree. Repeated stop on a terminal worker is a bounded no-op.

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
tree. The opaque worker resource reference is stable and owner-scoped. Serve
opens it only after host-written provenance binds the exact parent session,
path-free extension principal, and resource owner; the web view is locked and
read-only. The TUI routes the same current presentation reference through
`/extensions inspect agent-session:<digest>` and opens only a child in the
current parent's delegation team. Neither frontend can submit prompts or mutate
a worker; all mutation remains on owner-bound `agent_sessions`. The package
supplies no Rust TUI plugin, web code, or frontend scheduler. Generic rendering,
selection/navigation, reconnect and instance/generation fencing, authenticated
action routing, and Serve transport are host-owned.

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
