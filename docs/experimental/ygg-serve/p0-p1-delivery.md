# Ygg Serve P0/P1 delivery checklist

This checklist turns the July 27 Claude.app comparison audit into observable
delivery requirements. Audit claims are requirements only after they have been
confirmed against the current checkout. A checked item has production
implementation evidence and automated or explicitly recorded validation;
fixture-only behavior does not count.

At the current hardening checkpoint, completed rows are checked below. Rows left
unchecked are deliberate product gaps, not implied fixture capabilities. The
full verification evidence is recorded in [Current state](current-state.md).

## Constraints

- Keep unredacted command output, secrets, and host paths behind the
  coding-agent adapter. The public Serve protocol exposes every Bash command
  as bounded, control-safe text, redacting only credential-like values, plus
  opaque resource handles.
- Keep non-command tool arguments, progress, and raw results behind the
  coding-agent adapter.
- Keep workspace trust, agent authority, and future device authentication as
  separate decisions.
- Do not present conversation checkpoints as Git branches.
- Do not imply that shell network access is denied unless an OS-level policy
  enforces it.
- Do not invent phases, verification counts, child-agent attribution, or
  completion facts with another model.
- Preserve the optional package boundary and all non-Serve TUI/CLI behavior.

## P0 — current agent experience

### Semantic activity

- [x] Project every supported tool to a deterministic semantic kind, title,
      target, full bounded command/cwd preview, state, exit information, and
      duration.
- [x] Keep raw non-command tool arguments, progress, and results out of the
      public protocol.
- [x] Group work into deterministic investigate/change/verify/produce phases.
- [x] Aggregate repeated commands and expose compact summaries by default.
- [x] Link actions to sources, changed paths, diffs, outputs, approvals, and
      actors when the runtime can prove those relationships.
- [x] Preserve action/run/turn identity across live streaming, replay,
      hydration, restart, and conversation-checkpoint checkout.

### Working-state hierarchy

- [x] Use a static sidebar state marker.
- [x] Remove the composer perimeter and generic activity-rail animation.
- [x] Show at most one continuous indicator beside the current visible
      operation.
- [x] Replace generic `Working…` copy with the current deterministic phase.
- [x] Make reduced-motion and product motion settings remove nonessential
      motion.

### Reviewable completion

- [x] Emit a structured run review with outcome, duration, actions, changed
      files, verification, warnings/failures, outputs, and evidence coverage.
- [x] Render a dominant completion review in the transcript and activity rail.
- [x] Open exact changed-file diffs and snapshots after host restart.
- [x] Never claim a test count unless a supported parser proves it.

### Streaming performance

- [x] Isolate store subscriptions so unrelated shell/sidebar/inspector surfaces
      do not rerender for selected-message deltas.
- [x] Coalesce compatible deltas and avoid rebuilding catalog summaries for
      transcript-only events.
- [x] Index item updates and resource-to-action links.
- [x] Memoize completed rows and parse Markdown only after commit (or at a
      measured bounded cadence).
- [x] Use one coalesced scroll-follow scheduler and preserve manual scroll-away.
- [x] Apply long-transcript containment and keep mobile overlays opaque.

### Performance and visual gates

- [x] Add a gated 1,000-item/100-command fixture with long code, concurrent
      sessions, reconnect/replay, and mobile inspection.
- [x] Prove one publication per frame for a 120-delta batch and stable sidebar
      identity for transcript-only events.
- [x] Prove completed Markdown does not reparse during an unrelated live delta.
- [x] Prove manual scroll-away survives at least 50 streamed deltas.
- [x] Assert no persistent sidebar/composer animation and at most one visible
      conversation animation.
- [x] Add settled screenshot comparison for the mobile inspector and completion
      review.
- [x] Record a realistic performance trace meeting 55 FPS steady-state and no
      task above 50 ms on the reference laptop.

## P1 — complete the local coding-agent product

### Projects, trust, and files

- [ ] Persist a real project registry with canonical roots, stable identity,
      create/import/update/archive, defaults, and session binding.
- [x] Explain, grant, persist, and revoke project trust before loading
      project-controlled config, instructions, skills, or extensions.
- [x] Show loaded folder instructions with origin, precedence, and errors.
- [x] Project repository/worktree/HEAD/branch/dirty/ahead-behind state without
      confusing it with conversation history.
- [x] Ingest bounded, MIME-sniffed UTF-8 text, Markdown, and ordinary PDFs with
      immutable extraction provenance and hostile-document limits.
- [x] Browse/search only inside trusted roots and attach immutable file
      snapshots as context without accepting client-authored host paths.

The registry, launch-workspace import, rename, archive, defaults, trust, and
session bindings are durable. The combined first row remains unchecked because
production browser import/create still awaits a host-native picker that can mint
a one-use opaque folder candidate; the browser must never submit a host path.

### Review workflow

- [ ] Aggregate created/modified/deleted/renamed/binary changes into a tree,
      including shell and extension mutations where observable.
- [ ] Add split/unified diff modes, hunk navigation, and large/binary states.
- [ ] Persist redacted command history with cwd, exit, duration, bounded streams,
      and authenticated full-log handles.
- [x] Parse supported test frameworks into honest suite/case/pass/fail/skip
      records linked to the originating command.
- [ ] Implement host-owned preview start/readiness/failure/restart/stop and
      reconnect lifecycle.
- [ ] Add capability-gated host-mediated open-in-editor/terminal/browser actions
      that never accept arbitrary paths or URL schemes from the client.
- [ ] Project Git and pull-request status with explicit source and refresh state.

The unchecked review rows remain real limitations. Durable evidence is complete
for successful built-in `read`, `read_skill_resource`, `edit`, and `write`
operations, but arbitrary Bash/extension mutations, comprehensive binary and
large-file review, full command logs, preview ownership, host-open actions, and
pull-request integration are not generalized end to end.

### Conversation and recovery

- [x] Edit an earlier user turn by creating a sibling conversation branch.
- [x] Retry a response, retry once with another model, and retain provenance.
- [x] Fork a checkpoint into a new session without implying external side
      effects were rolled back.
- [x] Search persisted user/assistant/tool/error/attachment text with snippets,
      filters, highlighting, and jump-to-item while excluding hidden secrets.
- [x] Persist independent text and attachment drafts per session; clear only
      after acknowledged submission.
- [x] Send deduplicated opt-in notifications for background attention
      transitions with deep links and graceful denial.
- [x] Surface classified reconnect/provider/command recovery with manual retry
      or cancel while preserving idempotency.
- [x] Add archive browsing, restore, trash retention, and guarded permanent
      deletion.

### Runtime, context, and policy

- [ ] Project child-agent parentage, objective, state, timing, and outcome
      across live/replay/restart.
- [ ] Add typed lifecycle and management for MCP servers.
- [ ] Add trusted skill/extension catalog, enable/disable/reload, generation,
      contribution, and atomic failure visibility.
- [ ] Add project/language LSP lifecycle and diagnostics status.
- [x] Export context categories and replayable compaction start/finish with
      totals that reconcile.
- [ ] Replace coarse authority labels with enforced filesystem/tool/command/
      remote-read/process-network/approval/secrets consequences and explicit
      command/domain policies.

The unchecked runtime rows are intentionally unavailable rather than synthesized
from UI state. The production authority catalog advertises `FullAccess` only;
child-agent capability remains false, and MCP, extension/skill, and LSP status
must stay limited to facts supplied by real host integrations.

## Hardening closure

- [x] Own Git and PTY process trees with bounded graceful/forced descendant
      termination and bounded output-reader settlement.
- [x] Use descriptor-relative trusted-root traversal, root identity checks,
      no-follow opens, conflict detection, and synced atomic writes.
- [x] Reserve attachment count and bytes across concurrent ingest and recover
      duplicate fingerprints only when the mapping is unambiguous.
- [x] Scope WebSocket callbacks, replay, initialization, retries, and timers to a
      monotonic client generation.
- [x] Emit source-aware unknown-configuration diagnostics that warn by default
      and fail only under explicit strict mode.
- [x] Decode PTY output incrementally and truncate replay only at UTF-8
      boundaries.
- [x] Journal permanent deletion around the transcript boundary, recover
      idempotently after interruption, reclaim session-owned sidecars, retain
      shared payloads and host accounting, and fail closed on missing stores.
- [x] Publish exact, replayable context/run lifecycle state without mutating
      durable conversation history or fabricating provider attribution.
- [x] Cover hostile process, filesystem, document, quota, deletion, reconnect,
      terminal, and telemetry cases with adversarial tests.
- [x] Document the trusted-local-agent security model, persistence contracts,
      and configuration diagnostic behavior.

## Final gates

- [x] Web unit, typecheck, lint, production build, boundary, external-request,
      font, embedded-bundle, fixture Playwright, and production-host Playwright
      gates pass.
- [x] Serve Rust unit/golden tests, coding-agent Serve tests, strict Clippy,
      formatting, exact-feature build, installed-binary smoke, and full
      workspace gates pass.
- [ ] Real configured-provider acceptance covers fresh/restore, concurrent
      sessions, tools, files/documents, steer/follow-up/stop, reconnect,
      conversation branching, review, search, and restart.
- [x] The package-boundary gate passes and documentation describes only shipped
      production behavior.

All automated web checks above were rerun under Node `22.13.0`; Rust 1.86 checks
passed for both the workspace and independent Serve manifest. Publishable core
package assembly (`--workspace --exclude ygg-coding-agent`) passed with
`--allow-dirty`; omitting it is expected to reject this intentionally
uncommitted checkpoint. See [Current state](current-state.md#validation-evidence)
for the command-level matrix.
