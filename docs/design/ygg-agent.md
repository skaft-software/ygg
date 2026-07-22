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

Sessions are append-only JSONL records containing entries, head updates, provider usage, and checkpoints. Entries form a parent-linked tree and the latest durable head selects the active branch. Compaction adds a summary and `first_kept` boundary without deleting ancestry.

Writes use an advisory exclusive lock, compare the observed file length under that lock, append complete record buffers, and call `sync_data` before updating in-memory state. Read-only inspection uses a shared lock and never repairs or truncates. Writable open performs explicit torn-tail recovery while exclusively locked. Files are `0600` on Unix and parsing is bounded by bytes and record count.

## Filesystem tools

Workspace-only path shapes reject absolute roots and parent components. On Unix, file operations canonicalize the accepted target and then walk every component using directory descriptors and `O_NOFOLLOW`. Reads open the final object nonblocking, require a regular file from descriptor metadata, and stream at most limit+1 bytes. Mutations retain the open parent descriptor, write a sibling `create_new` temporary, re-read and compare the target immediately before commit, and rename relative to the same descriptor. Parent symlink replacement therefore cannot redirect the operation.

The path guard applies to explicit built-in paths. It is not process containment: enabled commands have the current user's authority.

## Resource limits

- Local file read/edit/preview: 32 MiB per file.
- Tool calls per assistant turn: 32.
- Model-visible aggregate tool results per turn: 16 KiB.
- Progress: bounded messages and chunks.
- Session replay: 256 MiB and 1,000,000 records.
- Command timeout/output: host-configured with product-level upper bounds.

## Extension boundary

All tools implement `Tool` and register through `ExtensionHost`; core tools are not privileged inside the run loop. A product policy filters the host before `Agent::new`, ensuring provider definitions and executable implementations are the same set. Extension tools are non-replayable by default.
