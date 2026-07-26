# Web acceptance

The web-first gate is complete only when every item below has observed test or
inspection evidence.

## Real execution

- Production uses real coding-agent sessions and contains no fixture import.
- Opening the root application creates one fresh provisional session.
- Reloading or reconnecting an explicit session route reopens that session
  without creating another.
- Two sessions can execute concurrently without crossing events, approvals,
  model state, or errors.
- Two observers of one session converge without duplicate execution.
- Reopening a session hydrates its durable items without duplicated prefixes or
  tool output.

## Interaction

- The empty state is a calm transcript and composer, not a dashboard.
- Prior, live, pinned, and recent sessions are available in the sidebar.
- Rename, pin, archive, delete, and stop behave consistently across clients.
- Streaming assistant, reasoning, tool, approval, input, compaction, source,
  output, diff/change, preview, and run-outcome items have typed renderers.
- Composer text, supported attachments, model, reasoning, authority, send,
  stop, steer, and queued follow-up use real controls.
- Sources, outputs, and changed files come from deterministic evidence.
- Preview opens in a resizable split on wide screens and a full-screen surface
  on phones.
- Closing a presentation surface never stops underlying work.

## Recovery and security

- Duplicate command IDs execute at most once.
- Reconnect replays missing events; a replay gap replaces state from a complete
  authoritative snapshot.
- Host, origin, request, frame, replay, and attachment bounds are enforced.
- Public errors are sanitized.
- Generated HTML cannot access the main app or host bridge.
- Production performs no analytics, remote-font, CDN, or hosted-control-plane
  request.
- The local web server is loopback-only.

## Visual and accessibility

- The interface is inspected at 1440×900, 1024×768, 768×1024, 390×844, and
  360×800.
- Mobile displays one primary surface at a time.
- Core flows work by keyboard and return focus correctly after overlays.
- Focus is visible, contrast is safe, and state is not encoded by color alone.
- Reduced motion removes nonessential movement.
- Two-hundred-percent zoom preserves session selection, transcript reading,
  composer, approval, and preview close.
- Automated checks find no critical or serious accessibility issue on the
  fresh, working, approval, output, preview, settings, and reconnect states.

## Repository

- The rejected frontend has no visual or ontology inheritance.
- No excluded feature appears as an empty placeholder.
- Generated server assets exactly match the tested frontend source.
- Focused Rust and web tests pass before full workspace tests.
- The main checkout remains untouched.
