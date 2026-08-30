# Ygg agent design

## Responsibilities

`ygg-agent` owns one mutable session, reconstructs canonical provider context, opens and consumes provider streams, executes registered tools, persists complete semantic records, and emits frontend-neutral events. Provider wire formats stay in `ygg-ai`; terminal policy stays in `ygg-coding-agent`.

## Commit and cancellation invariants

1. Streaming deltas are provisional and never enter the session.
2. A complete assistant message is persisted before any emitted tool is executed.
3. Each tool result is persisted immediately after its execution outcome is committed.
4. Crash replay requires both `ReplaySafety::Safe` and an exact host classification of `Pure` or `WorkspaceRead`. Every other unresolved call becomes an indeterminate error and is not executed.
5. One level-triggered abort signal is selected against provider open/body consumption, retries, tools, and autonomous compaction. Cancellation wins same-poll races. A cancelled compaction persists neither usage nor summary.
6. Every driven run emits exactly one `RunFinished` and one durable checkpoint.
7. Optional telemetry is an observer outside the session ledger. When installed,
   it receives an opaque run-start hook and coarse request/tool/compaction
   boundaries; it records hashes and bounded measurements, never raw prompts,
   arguments, results, or provider payloads.
8. `Agent::prompt_without_tools` starts with a sticky tool-free policy.
   `RunControl::finish_now` persists its input at the next safe turn boundary and
   makes that policy sticky for the remainder of the run. Subsequent provider
   requests contain no tool schemas and set `ToolChoice::None`; calls emitted by
   an already-open provider request are paired with synthetic errors rather than
   executed. Effects already admitted at the time of the control settle under
   the ordinary cancellation and commit rules.

## Effect admission boundary

Every registered `Tool` classifies the exact parsed call through host-owned code. The model cannot provide or lower this classification, and the trait default is `Unknown`. Unknown effects fail closed under every policy, including `UnsafeHost`. Before any hook or tool implementation receives a call, the agent constructs a bounded canonical `EffectIntent` over the principal, run, tool-catalog generation, provider call ID, tool name, effect, arguments, and policy version.

The default `Controlled` policy admits `Pure` and `WorkspaceRead`, requests interactive confirmation for workspace mutation and non-whitelisted `HostProcess` calls, auto-approves a conservative set of known-safe read-only `bash` commands, and otherwise denies host reads/mutations, native processes, network, delegation, executable extensions, and unknown effects. `UnsafeHost` admits classified effects but is not containment. The coding product
selects it by default; `--safe-mode` selects `ControlledBashApproval`. Product code
must additionally prevent executable-extension process startup under controlled
policies because a broker check at later tool invocation cannot contain an
already-running executable.

Workspace-mutation approval creates a random, short-lived capability bound to the canonical intent digest. Tokens are atomically single-use, stored by one-way verifier, redacted in debug output, and never supplied to tools. Dispatch reserves admission before `before_tool_call`, then commits and consumes the exact grant only after all hooks pass and immediately before calling `Tool::execute`. Hook denial or cancellation drops and revokes an uncommitted reservation; cancellation after commit cannot restore it. `after_tool_call` runs only for a committed effect.

Sequential, parallel, and crash-recovery dispatch all use this boundary. Static `ToolConcurrency::Parallel` and `ReplaySafety::Safe` declarations are intersected with the exact host classification: only `Pure` and `WorkspaceRead` calls may run in a parallel batch or be replayed after a crash. A denied call is returned to the provider as a paired tool error without invoking hooks or executable code.

The broker is a deterministic admission reference monitor, not an OS sandbox. Controlled intentionally denies effect classes that still lack isolation or dedicated brokers, while allowing safe read-only `bash` commands through the `Controlled` process channel; `ControlledBashApproval` (selected by `--safe-mode`) confirms every `bash` call. The default `UnsafeHost` policy lets classified command and process effects use ambient host authority.

## Sessions

Sessions are append-only JSONL records containing entries, head updates, provider usage, and checkpoints. Entries form a parent-linked tree and the latest durable head selects the active branch. Compaction adds a Pi-structured summary, `first_kept` boundary, active-skill snapshot, and cumulative `readFiles`/`modifiedFiles` details without deleting ancestry. Both product-triggered and autonomous compaction use the same serialized handoff contract.

Before every provider turn, the agent estimates the complete request and retains a fixed 16K output reserve (or a larger explicit reasoning floor). The provider-advertised maximum completion size remains the model ceiling; the individual request is clamped only to the context space remaining after input. The default compaction threshold is the full context window, so the fixed reserve is not combined with an additional percentage buffer. If a provider nevertheless ends at the output limit while emitting tools, the assistant envelope is persisted, every call is paired with a synthetic error without execution, and a corrective continuation asks the model to reissue complete arguments.

Writes use an advisory exclusive lock, compare the observed file length under that lock, append complete record buffers, and call `sync_data` before updating in-memory state. Read-only inspection uses a shared lock and never repairs or truncates. Writable open performs explicit torn-tail recovery while exclusively locked. Files are `0600` on Unix and parsing is bounded by bytes and record count.

## V2 task delegation

For explicit orchestration boundaries (hosted delegation vs local delegated children, sandbox/approval/env/cwd inheritance, extension trust propagation, and explicit non-goals), see [`docs/design/extension-capability-and-orchestration-boundaries.md`](extension-capability-and-orchestration-boundaries.md).

`ygg-agent` owns host execution for `AgentDelegation::V2`; the model capability in
`ygg-ai` is metadata only. The generic `Agent::enable_v2_delegation` API can
install the native collaboration surface for embedders that explicitly choose
it. The coding product instead enables the manager in extension-only mode: only
the trusted, enabled `ygg-subagents` extension receives the owner-bound
`agent_sessions` service, and the root model never receives the parallel native
`spawn_agent`, `followup_task`, `send_message`, `wait_agent`, `list_agents`, or
`interrupt_agent` tools. Available/proactive mode guidance applies only to the
generic API; product orchestration and observation remain extension-owned.

Each child has a stable ID and ancestry path, an isolated append-only `Session`,
and its own agent loop. It inherits the effective root system prompt at spawn
time, approved extension host/tool set, sandbox, model, reasoning and cache
settings, compaction model/policy, completion policy, output modalities, resolved
context/output limits, retry policy, turn limit, optional session token ceiling,
session cost ceiling, and the root's cloned effect broker. A missing root session
token ceiling remains missing in the child; the host does not invent one. Each
child starts a fresh independent context. Its settled usage is mirrored into the
root ledger for accounting and cost-limit checks, never inserted into the
parent's prompt context or charged to the parent's own-context token ceiling. The
broker clone preserves a shared policy/grant store; it is not yet child-specific
authority attenuation. Controlled therefore denies the `Delegation` effect
entirely, while UnsafeHost delegation must be treated as ambient-authority
compatibility mode. Children
can message peers, steer active work, queue messages for an idle worker, receive
follow-up runs, wait without lost notifications, and spawn within the remaining
depth and concurrency bounds.

The default team limit is ten concurrent agents including the root, depth two,
and thirty-two total agents during each owning run. Host
validation permits 2–32 concurrent agents, depth 1–8, and at most 256 total,
with total capacity never below concurrent capacity. The first-party
`ygg-subagents` service that the coding product actually uses is stricter: its
children sit exactly one level below the root and are bounded to eight active
children per parent with thirty-two retained records per resource owner, and a
worker inherits the parent's full standard tool scope (`read`, `search`,
`edit`, `write`, `bash`) unless the spawn narrows it. A semaphore and ancestry
checks enforce those limits independently of model behavior; an idle worker is
reserved as `Pending` before a follow-up is published so concurrent follow-ups
cannot start overlapping runs. Each worker command channel is capped at 32;
the accepted follow-up backlog is capped at 32 messages and 4,325,376 bytes.
Accepted steering and follow-up reservations remain charged until the child
emits its durable delivery acknowledgement. Direct messages moved into a child
prompt likewise remain reserved until the prompt append succeeds; a failed append
restores them at the front. Failure, interruption, and control backpressure
requeue unacknowledged work in FIFO order rather than releasing or discarding it.
Pending direct messages are capped at 96 and 4,325,376 bytes per child, including
in-flight and prompt-delivery reservations; overflow and inputs above the 128 KiB
durable-text limit are rejected before provenance is written rather than evicting
or truncating accepted work.
Agent mailboxes retain at most 64 messages and 1 MiB; automatic status
notifications evict only the oldest unleased automatic notifications when
necessary and are dropped when no such entry can be evicted. Accepted direct
messages are never evicted, and direct messages to a full root mailbox are
rejected. `wait_agent` leases a UTF-8-safe bounded page and exposes continuation
metadata when one message spans pages. The lease commits only after the complete,
untruncated tool result is durably appended to the owning agent's session;
cancellation, persistence failure, serialization failure, or generic output
truncation restores the page. Concurrent `wait_agent` calls are capped at the
configured total-agent limit and released by cancellation-safe RAII guards.

Delegation state is stored under a descriptor-bound, owner-only random team
directory. `provenance.jsonl` records `team_started`, `agent_spawned`,
`agent_status`, `message`, `interrupt_requested`, and `team_shutdown`; child
session files and the journal are private. Directory allocation, child-session
creation, activation rollback, and cleanup are descriptor-relative and no-follow;
a failed activation removes only its exact empty private team directory and
reports both activation and rollback failures if cleanup cannot complete. Every
spawn, message, follow-up, status transition, and interrupt is appended and
`sync_data`-ed before delivery or visible state mutation. If append or sync
fails, the manager records a visible persistence diagnostic, rejects new work,
and cancels every worker rather than operating without provenance.

Cancellation propagates down ancestry. Every owning run terminal (including
normal completion, failure, max-turn termination, explicit abort, and incomplete
`Run` drop), root `Agent` drop, worker interruption/failure, closed command
channels, and team shutdown cancel worker tokens and send shutdown commands;
descendants are stopped with their parent. A normally driven root terminal also
waits up to two seconds for extension-owned descendants to settle, aggregates
each child session's durable disjoint usage and exact category cost (including
picodollar remainder), and appends one `UsageRecordKind::DelegatedAgent` entry
per child to the root session before its checkpoint. The child remains the
detailed source of truth; the root mirror is the cumulative accounting and cost-
limit ledger.

## Filesystem tools

Workspace-only path shapes reject absolute roots and parent components. On Unix, file operations canonicalize the accepted target and then walk every component using directory descriptors and `O_NOFOLLOW`. Reads open the final object nonblocking, require a regular file from descriptor metadata, and stream at most limit+1 bytes. Mutations retain the open parent descriptor, write a sibling `create_new` temporary, re-read and compare the target immediately before commit, and rename relative to the same descriptor. Parent symlink replacement therefore cannot redirect the operation.

The path guard applies to explicit built-in paths. It is not process containment: commands admitted by UnsafeHost have the current user's authority. When external paths are enabled, local file tools conservatively classify every call as a host effect so a path-resolution race cannot lower admission authority; the coding product forces external paths off under controlled policies, including `--safe-mode`.

## Resource limits

- Local file read/edit/preview: 32 MiB per file.
- Tool calls per assistant turn: 32.
- Default model-visible text per tool result: 50 KiB (host-configurable).
- Delegation provenance text per task/message/status payload: 128 KiB; configurable teams remain capped at 32 concurrent, depth 8, and 256 total agents.
- Progress: bounded messages and chunks.
- Session replay: 256 MiB and 1,000,000 records.
- Command timeout/output: host-configured with product-level upper bounds.

## Extension boundary

All tools implement `Tool` and register through `ExtensionHost`; core tools are not privileged inside the run loop. A product policy filters the host before `Agent::new`, ensuring provider definitions and executable implementations are the same set. Tool implementations own effect metadata and default to `Unknown`; provider schemas and model arguments cannot select authority. Executable extension tools classify as `Extension`, remain non-replayable and sequential, and the coding product prevents their process from starting under Controlled.
