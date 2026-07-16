# ygg-agent

Stateful agent loop with tool execution and event streaming for Ygg.

`ygg-agent` sits above [`ygg-ai`](../ygg-ai): it reconstructs provider
requests from a persistent, branchable JSONL session, drives the model
stream, executes tool calls through a small extension boundary, persists
every semantic boundary (complete messages and individual tool results —
never streaming deltas), and emits a streaming event surface including
`OutputDelta`, batched `SteeringDelivered`, tool lifecycle events,
`TurnFinished`, and `RunFinished` to the caller.

Included:

- Typed `UserInput` / `InputPart` boundary for `prompt`, `steer`, and
  `follow_up`: ordered text and media parts (`ygg_ai::Media`) pass through
  the agent to the model unchanged; text-only callers remain compatible via
  `From<String>` / `From<&str>`.
- Four built-in tools — `read`, `search`, `edit`, `exec` — registered through
  the same `Extension` boundary available to third-party tools.
- A concrete `SandboxConfig`: relative paths use the workspace and hosts may
  enable trusted-local absolute/`~/`/external paths, or opt into a workspace-only
  accidental-path guard. It also provides independent
  `allow_edit` / `allow_process` / `allow_shell` gates, an execution timeout,
  output-byte limits, and process-group cleanup for cancelled child processes
  (`exec` is unix-only in v0.1 — it fails clearly rather than weakening cleanup
  on other platforms). Neither path mode is an OS sandbox: spawned processes
  run with the current user's full access. Ygg is a trusted local agent — see
  the repository-root `SECURITY.md`.
- `Run` + clonable `RunControl` with steering, follow-up, and abort — built
  for `tokio::select!` alongside user input.
- Session checkout/branching, manual compaction, and torn-tail crash recovery.
  Appends are process-crash safe (plain writes, no fsync — not power-loss
  durable), and tool execution is **at-least-once** after an unclean crash;
  this crate never claims exactly-once.

See [`../../docs/design/ygg-agent.md`](../../docs/design/ygg-agent.md) for the
normative design and the crate-level Rust documentation for the public API.
