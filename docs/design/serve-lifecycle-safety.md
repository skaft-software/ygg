# Serve lifecycle and safety design

This document records the safety contracts implemented by the experimental
`ygg serve` host. They protect Ygg's own trust, persistence, and protocol
boundaries. They do **not** turn Ygg into an operating-system sandbox.

## Security model

`ygg serve` is a loopback service for a trusted local user. The agent and any
enabled command run with that user's operating-system authority. A hostile
repository, model, or child process still requires isolation with a restricted
user, container, VM, or platform sandbox.

Within that model, Serve fails closed at externally controlled boundaries:
project IDs and relative paths, uploaded bytes, durable store files, process
lifetime, protocol payloads, and client generations.

## Owned subprocesses

Git helpers and PTY shells own a process tree rather than only a direct child.

- Git commands start in a fresh Unix process group and verify that group before
  enabling ancestry snapshots.
- PTY shells use the fresh session created by `portable-pty`; Serve verifies the
  session leader before enabling descendant discovery.
- Before signalling, Serve snapshots observable descendants and their process
  groups. PTY discovery also includes every process still in the owned session.
- Graceful termination is followed by forced group termination on a fixed
  deadline.
- Output readers and child waiters have bounded settlement. Cleanup never waits
  forever for a descendant that retained an output descriptor.
- A naturally exiting PTY shell immediately settles its remaining background
  groups; numeric process identities are not retained until a later shutdown.
- Output is decoded incrementally, so UTF-8 code points split across reads are
  preserved. Replay truncation always begins at a valid UTF-8 boundary.

This is bounded lifecycle cleanup, not process containment. A program with the
user's authority can deliberately create a new session, fully daemonize before
it is observed, or launch work through another service. Use an OS sandbox or
container when that behavior must be prevented.

The implementation is Serve-local in `extensions/ygg-serve/src/process_tree.rs`
so `extensions/ygg-serve` remains independently buildable.

## Trusted project filesystem

Project filesystem operations begin with an opaque project ID resolved through
the private project registry. On Unix, the registry records the imported root's
device, inode, and creation time. Every operation reopens the root
without following a symlink and verifies that identity. The creation time also
disambiguates filesystems that immediately reuse an inode for a replacement
directory.

Traversal below the root is descriptor-relative:

- components are bounded, UTF-8, normal relative path components;
- directories and final files are opened with `openat` and `O_NOFOLLOW`;
- reads accept only regular, single-link files;
- listings and searches have entry, depth, file-count, and byte budgets;
- writes require the SHA-256 from a complete prior read unless the caller
  explicitly confirms a force write;
- writes create an exclusive temporary file in the already-open parent,
  preserve permissions, sync content, recheck parent and target identity,
  rename relative to that descriptor, and sync the directory; and
- a concurrent replacement reports `ProjectFileSystemError::Conflict` rather
  than writing through a stale path.

This protects the trusted-root contract from symlink and rename races. It does
not defend against an enabled command with the user's general filesystem
permissions.

If the directory at the registered canonical path is explicitly replaced, the
registry marks it unavailable and retains the old trust bit without using it.
The host's explicit launch-workspace trust action may rebind that same canonical
path to its new device, inode, and creation time, revoke the old
trust, and grant trust again in the same user-directed flow. It cannot rebind
another project or accept a browser-supplied path.

## Bounded durable stores

Attachments, documents, evidence resources, goals, and run projections use
owner-private, bounded stores. Durable publication uses synced files and synced
owning directories.

Attachment quota checks reserve both count and bytes while an ingest is in
flight. The reservation is released through RAII on every failure path, closing
the concurrent check-then-write race. Startup recovery accepts a duplicate
fingerprint only when it maps one-to-one to identical content; ambiguity fails
closed. Association publication and session cleanup share a mutation lock, so
cleanup either observes a completed retained association or association fails
before it can point at reclaimed bytes. The public quota error remains
`attachment storage quota reached`.

Evidence startup removes invalid run records and corrupt ownership pairs, while
retaining valid ownerless legacy records. Legacy records with a top-level
`sessionId` remain attributable during permanent deletion.

## Client generations

A WebSocket connection and a `YggStore.initialize()` call each belong to one
monotonic generation. Reconnect or reinitialization invalidates prior work.
Callbacks, replay responses, reconnect timers, and initialization completions
check that generation before mutating state. A stale connection therefore
cannot overwrite a newer snapshot or schedule another reconnect.

## Permanent session deletion

Permanent deletion is a journaled state machine, not a best-effort chain of
unrelated removals.

1. The supervisor takes a per-session lifecycle write gate, rejects new opens,
   waits for any in-flight open factory, and does not enter host mutation until
   the exact actor and its durable worker have quiesced. Session-scoped document,
   resource, and export operations hold the corresponding read gate.
2. The host validates the exact session ID, trash timestamp, and confirmation
   phrase.
3. It verifies that every required sidecar store is available. Missing
   attachment, document, or resource storage returns `ServiceError::Unavailable`
   before any deletion intent is committed.
4. It writes and syncs a `committed: false` intent under
   `session-deletions-v1`.
5. The session store stages the transcript and metadata. The irreversible
   boundary is disappearance of the canonical JSONL transcript.
6. The host durably advances the journal to `committed: true` and idempotently
   removes primary staging files, the project binding, attachment associations
   and unshared payloads, documents, evidence and run records, goals, and the
   transcript-search projection.
7. Only after every owning directory is synced is the journal entry removed and
   that removal synced.

Startup recovery handles both sides of the boundary:

- if an uncommitted record still has its canonical transcript, staged metadata
  is restored and staging debris is removed idempotently;
- if the transcript is absent, recovery advances the record and completes all
  cleanup; and
- an unreadable or unsafe transcript cannot be interpreted as absence, and a
  failed or unavailable cleanup leaves the journal in place for a later startup.

Journal records are bounded no-follow regular files. Existing intent cannot be
replaced, and `committed: true` cannot be downgraded. Live deletion and startup
recovery also share one host lock, making journal transitions monotonic across
concurrent callers.

Payloads referenced by another session are retained. Repeating any committed
cleanup step is safe and reclaims quota once the final reference disappears.

Inference usage is deliberately different: `InferenceRequestStore` is a
conversation-content-free, append-only host accounting log. Permanent session
deletion keeps those provider/model/token/timestamp totals, while deleting
every sidecar that can rehydrate session content. Removing the Serve state
directory removes that accounting history.

## Context and lifecycle telemetry

`ygg-agent` owns an ephemeral `ContextTracker` for each run. It observes the
actual model input, provider stream, retries, tool boundaries, compaction, and
terminal settlement. Reading a snapshot does not append to or rewrite the
conversation session.

The coding-agent adapter projects complete snapshots through replayable
`context.updated` events. Categories reconcile exactly to the current total:
system prompt, project instructions, conversation, tools, attachments,
documents, project files, and `other`. Adapter-known prompt sources are
attributed only at their delivery boundary. Any provider-reported excess stays
in `other`; the adapter does not invent provider-authoritative attribution.

Lifecycle counters obey these invariants at every published revision:

- `responses_started = responses_finished + responses_discarded +
  response_active`;
- phase `responding` is equivalent to `response_active`;
- tool and compaction completions never exceed starts;
- successful compaction reconciles its projected `after` totals with current
  context;
- failed or interrupted compaction has identical `before` and `after` totals and
  reclaims zero tokens; and
- same-revision snapshots are either identical idempotent replays or an internal
  error.

Consuming a run through `Run::into_context_snapshot` settles unfinished work as
`Dropped` before the final event is published. Context telemetry is replayable
operational state, not persisted conversation history.

## Verification anchors

Adversarial coverage lives with the owning package:

- process descendants and UTF-8: `extensions/ygg-serve/src/pty.rs` and
  `tests/repository_context.rs`;
- root swaps, symlinks, hard links, concurrent replacement, and synced writes:
  `extensions/ygg-serve/tests/project_fs.rs`;
- quota races and duplicate recovery: `extensions/ygg-serve/src/attachment.rs`;
- deletion, shared references, legacy/corrupt records, and restart recovery:
  store unit tests plus coding-agent Serve adapter tests; and
- telemetry reconciliation and strict wire decoding:
  `crates/ygg-agent`, coding-agent adapter tests, and `apps/web/src/wire.test.ts`.
