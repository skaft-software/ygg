# Ygg agent design

## Responsibilities

`ygg-agent` owns one mutable session, reconstructs canonical provider context, opens and consumes provider streams, executes registered tools, persists complete semantic records, and emits frontend-neutral events. Provider wire formats stay in `ygg-ai`; terminal policy stays in `ygg-coding-agent`.

## Commit and cancellation invariants

1. Streaming deltas are provisional and never enter the session.
2. A complete assistant message is persisted before any emitted tool is executed.
3. Each tool result is persisted immediately after its execution commit point.
4. Read-only tools may opt into crash replay with `ReplaySafety::Safe`. Every other unresolved call becomes an indeterminate error and is not executed.
5. One level-triggered abort signal is selected against provider open/body consumption, retries, tools, and autonomous compaction. Cancellation wins same-poll races. A cancelled compaction persists neither usage nor summary.
6. Every driven run emits exactly one `RunFinished` and one durable checkpoint.

## Sessions

Sessions are append-only JSONL records containing entries, head updates, provider usage, and checkpoints. Entries form a parent-linked tree and the latest durable head selects the active branch. Compaction adds a Pi-structured summary, `first_kept` boundary, active-skill snapshot, and cumulative `readFiles`/`modifiedFiles` details without deleting ancestry. Both product-triggered and autonomous compaction use the same serialized handoff contract.

Writes use an advisory exclusive lock, compare the observed file length under that lock, append complete record buffers, and call `sync_data` before updating in-memory state. Read-only inspection uses a shared lock and never repairs or truncates. Writable open performs explicit torn-tail recovery while exclusively locked. Files are `0600` on Unix and parsing is bounded by bytes and record count.

## V2 task delegation

`ygg-agent` owns host execution for `AgentDelegation::V2`; the model capability in
`ygg-ai` is metadata only. `Agent::enable_v2_delegation` installs
`spawn_agent`, `followup_task`, `send_message`, `wait_agent`, `list_agents`, and
`interrupt_agent`. Available mode exposes the tools on demand; proactive mode
also instructs the root to use sub-agents when parallel work would materially
improve speed or quality and to verify child results before integrating them.

Each child has a stable ID and ancestry path, an isolated append-only `Session`,
and its own agent loop. It inherits the effective root system prompt at spawn
time, approved extension host/tool set, sandbox, model, reasoning and cache
settings, compaction model/policy, completion policy, output modalities, resolved
output-token limit, retry policy, turn limit, and session cost ceiling. Children
can message peers, steer active work, queue messages for an idle worker, receive
follow-up runs, wait without lost notifications, and spawn within the remaining
depth and concurrency bounds.

The default team limit is four concurrent agents including the root, depth two,
and sixteen total agents including the root during each owning run. Host
validation permits 2–32 concurrent agents, depth 1–8, and at most 256 total,
with total capacity never below concurrent capacity. A semaphore and ancestry
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
descendants are stopped with their parent.

## Filesystem tools

Workspace-only path shapes reject absolute roots and parent components. On Unix, file operations canonicalize the accepted target and then walk every component using directory descriptors and `O_NOFOLLOW`. Reads open the final object nonblocking, require a regular file from descriptor metadata, and stream at most limit+1 bytes. Mutations retain the open parent descriptor, write a sibling `create_new` temporary, re-read and compare the target immediately before commit, and rename relative to the same descriptor. Parent symlink replacement therefore cannot redirect the operation.

The path guard applies to explicit built-in paths. It is not process containment: enabled commands have the current user's authority.

## Resource limits

- Local file read/edit/preview: 32 MiB per file.
- Tool calls per assistant turn: 32.
- Model-visible aggregate tool results per turn: 16 KiB.
- Delegation provenance text per task/message/status payload: 128 KiB; configurable teams remain capped at 32 concurrent, depth 8, and 256 total agents.
- Progress: bounded messages and chunks.
- Session replay: 256 MiB and 1,000,000 records.
- Command timeout/output: host-configured with product-level upper bounds.

## Extension boundary

All tools implement `Tool` and register through `ExtensionHost`; core tools are not privileged inside the run loop. A product policy filters the host before `Agent::new`, ensuring provider definitions and executable implementations are the same set. Extension tools are non-replayable by default.
