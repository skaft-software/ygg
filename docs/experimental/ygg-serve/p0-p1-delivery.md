# Ygg Serve P0/P1 delivery checklist

This checklist turns the July 27 Claude.app comparison audit into observable
delivery requirements. Audit claims are requirements only after they have been
confirmed against the current checkout. A checked item must have production
implementation evidence and an automated or explicitly recorded validation;
fixture-only behavior does not count.

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

- [ ] Project every supported tool to a deterministic semantic kind, title,
      target, full bounded command/cwd preview, state, exit information, and
      duration.
- [ ] Keep raw non-command tool arguments, progress, and results out of the
      public protocol.
- [ ] Group work into deterministic investigate/change/verify/produce phases.
- [ ] Aggregate repeated commands and expose compact summaries by default.
- [ ] Link actions to sources, changed paths, diffs, outputs, approvals, and
      actors when the runtime can prove those relationships.
- [ ] Preserve action/run/turn identity across live streaming, replay,
      hydration, restart, and conversation-checkpoint checkout.

### Working-state hierarchy

- [ ] Use a static sidebar state marker.
- [ ] Remove the composer perimeter and generic activity-rail animation.
- [ ] Show at most one continuous indicator beside the current visible
      operation.
- [ ] Replace generic `Working…` copy with the current deterministic phase.
- [ ] Make reduced-motion and product motion settings remove nonessential
      motion.

### Reviewable completion

- [ ] Emit a structured run review with outcome, duration, actions, changed
      files, verification, warnings/failures, outputs, and evidence coverage.
- [ ] Render a dominant completion review in the transcript and activity rail.
- [ ] Open exact changed-file diffs and snapshots after host restart.
- [ ] Never claim a test count unless a supported parser proves it.

### Streaming performance

- [ ] Isolate store subscriptions so unrelated shell/sidebar/inspector surfaces
      do not rerender for selected-message deltas.
- [ ] Coalesce compatible deltas and avoid rebuilding catalog summaries for
      transcript-only events.
- [ ] Index item updates and resource-to-action links.
- [ ] Memoize completed rows and parse Markdown only after commit (or at a
      measured bounded cadence).
- [ ] Use one coalesced scroll-follow scheduler and preserve manual scroll-away.
- [ ] Apply long-transcript containment and keep mobile overlays opaque.

### Performance and visual gates

- [ ] Add a gated 1,000-item/100-command fixture with long code, concurrent
      sessions, reconnect/replay, and mobile inspection.
- [ ] Prove one publication per frame for a 120-delta batch and stable sidebar
      identity for transcript-only events.
- [ ] Prove completed Markdown does not reparse during an unrelated live delta.
- [ ] Prove manual scroll-away survives at least 50 streamed deltas.
- [ ] Assert no persistent sidebar/composer animation and at most one visible
      conversation animation.
- [ ] Add settled screenshot comparison for the mobile inspector and completion
      review.
- [ ] Record a realistic performance trace meeting 55 FPS steady-state and no
      task above 50 ms on the reference laptop.

## P1 — complete the local coding-agent product

### Projects, trust, and files

- [ ] Persist a real project registry with canonical roots, stable identity,
      create/import/update/archive, defaults, and session binding.
- [ ] Explain, grant, persist, and revoke project trust before loading
      project-controlled config, instructions, skills, or extensions.
- [ ] Show loaded folder instructions with origin, precedence, and errors.
- [ ] Project repository/worktree/HEAD/branch/dirty/ahead-behind state without
      confusing it with conversation history.
- [ ] Ingest bounded, MIME-sniffed UTF-8 text, Markdown, and ordinary PDFs with
      immutable extraction provenance and hostile-document limits.
- [ ] Browse/search only inside trusted roots and attach immutable file
      snapshots as context without accepting client-authored host paths.

### Review workflow

- [ ] Aggregate created/modified/deleted/renamed/binary changes into a tree,
      including shell and extension mutations where observable.
- [ ] Add split/unified diff modes, hunk navigation, and large/binary states.
- [ ] Persist redacted command history with cwd, exit, duration, bounded streams,
      and authenticated full-log handles.
- [ ] Parse supported test frameworks into honest suite/case/pass/fail/skip
      records linked to the originating command.
- [ ] Implement host-owned preview start/readiness/failure/restart/stop and
      reconnect lifecycle.
- [ ] Add capability-gated host-mediated open-in-editor/terminal/browser actions
      that never accept arbitrary paths or URL schemes from the client.
- [ ] Project Git and pull-request status with explicit source and refresh state.

### Conversation and recovery

- [ ] Edit an earlier user turn by creating a sibling conversation branch.
- [ ] Retry a response, retry once with another model, and retain provenance.
- [ ] Fork a checkpoint into a new session without implying external side
      effects were rolled back.
- [ ] Search persisted user/assistant/tool/error/attachment text with snippets,
      filters, highlighting, and jump-to-item while excluding hidden secrets.
- [ ] Persist independent text and attachment drafts per session; clear only
      after acknowledged submission.
- [ ] Send deduplicated opt-in notifications for background attention
      transitions with deep links and graceful denial.
- [ ] Surface classified reconnect/provider/command recovery with manual retry
      or cancel while preserving idempotency.
- [ ] Add archive browsing, restore, trash retention, and guarded permanent
      deletion.

### Runtime, context, and policy

- [ ] Project child-agent parentage, objective, state, timing, and outcome
      across live/replay/restart.
- [ ] Add typed lifecycle and management for MCP servers.
- [ ] Add trusted skill/extension catalog, enable/disable/reload, generation,
      contribution, and atomic failure visibility.
- [ ] Add project/language LSP lifecycle and diagnostics status.
- [ ] Export context categories and replayable compaction start/finish with
      totals that reconcile.
- [ ] Replace coarse authority labels with enforced filesystem/tool/command/
      remote-read/process-network/approval/secrets consequences and explicit
      command/domain policies.

## Final gates

- [ ] Web unit, typecheck, lint, production build, boundary, external-request,
      font, embedded-bundle, fixture Playwright, and production-host Playwright
      gates pass.
- [ ] Serve Rust unit/golden tests, coding-agent Serve tests, strict Clippy,
      formatting, exact-feature build, installed-binary smoke, and full
      workspace gates pass.
- [ ] Real configured-provider acceptance covers fresh/restore, concurrent
      sessions, tools, files/documents, steer/follow-up/stop, reconnect,
      conversation branching, review, search, and restart.
- [ ] The package-boundary gate passes and documentation describes only shipped
      production behavior.
