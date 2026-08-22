# Changelog

## Unreleased

### Added

- Live worker activity in the `/subagents` list: the host now exposes a
  bounded rolling `recent_tools` array (last six tool calls with flattened
  argument summaries, timing, and an error flag) on each `agent/list` record.
  Picker rows lead with the latest action — e.g. `read
  crates/ygg-agent/src/delegation.rs` or `* search pattern=spawn_agent` for a
  call in flight, `!` prefixed after an error — so you can see what every
  worker is doing without opening its transcript. The inspect detail gains a
  "Recent tool activity" section and the headless narrow list shows the same
  action per row.

### Changed

- Workers now inherit the parent's full standard tool scope (`read`, `search`,
  `edit`, `write`, `bash`) by default; pass `tools: [read, search]` for a hard
  read-only guarantee. The child prompt states the granted scope instead of a
  blanket read-only ban, the `test-analysis` profile may run the checks it
  proposes, and spawn schema/skill/README guidance were updated.
- Terminal-gate rejections now carry cumulative session cost so per-worker cost
  tracks token usage between accepted turns.

## 0.2.0

### Added

- `subagent_continue`: steer an active worker through `agent/message` or
  resume a settled worker through `agent/follow_up`. A resumed worker keeps
  its durable conversation context; the host clears the stale completion
  timestamp and re-anchors an elapsed wall deadline so the new run owns a
  fresh budget.
- Per-spawn mutation grants: workers may be granted `edit`, `write`, and
  `bash` through the spawn `tools` list (the default remains the read-only
  `read, search` pair). The host's scoped tool snapshot is the enforcement
  boundary, not the worker's self-discipline.
- Per-spawn ceilings are now optional: omitted (or `null`) `timeout_seconds`,
  `max_turns`, and `max_cost_microdollars` inherit the parent session's
  ceilings, so an unlimited parent produces an unlimited child.
- Regression tests for granted mutation scope, the continue tool (steer,
  resume, stopping, and orphaned rejections), and protocol-level policy
  handling.

### Changed

- Raised limits from 2 active/16 retained to **8 active/32 retained**
  children per owner, with explicit ceilings raised to 256 turns, 50,000,000
  microdollars, and 24 hours of wall time (all overridable per spawn, all
  optional).
- The TUI composer-adjacent activity strip now appears only while workers
  are actively working, uses `•`/`└` glyphs with model-matched colours, and
  `Ctrl+O` expands it from the two to the five most recent workers (falling
  back to the verbose tool-output toggle only when no strip is visible).
- `agent/follow_up` on a settled child is a resume, not a rejection; the
  worker's persistent task and transcript survive between runs.

## 0.1.0

- Add four bounded API `0.2` subagent tools over the host-owned `agent_sessions` service.
- Enforce two-child, depth-one, read/search-only orchestration policy with owner-derived scoping, idempotent spawn, budgets, timeout observation, cancellation, restart reconciliation, and shutdown settlement.
- Add generic semantic worker tree/activity/detail/actions and the cached `/subagents` narrow fallback.
- Ship the explicit activation skill, deterministic fake host fixtures, protocol/orchestration/presentation tests, synchronized Python SDK, and release smoke comparison.
