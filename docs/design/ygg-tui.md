# Ygg TUI design

**Status:** Current implementation contract.

The interactive frontend owns terminal setup/restoration and presentation only;
`Agent` remains the sole model/tool runtime.

## Terminal guarantees

- The interactive frontend renders on the primary screen so newly committed
  transcript rows flow into terminal-native scrollback.
- A theme swap clears and repaints every cell in the visible viewport. Rows
  already committed to native scrollback cannot be rewritten by portable
  terminal APIs and retain their original styling; Ygg preserves that history
  rather than clearing it implicitly. Application-owned mouse mode re-renders
  retained semantic rows with the current theme as they enter its viewport.
- Raw mode, bracketed paste, keyboard enhancements, and synchronized output are
  enabled only when supported and are restored idempotently.
- Mouse reporting is disabled by default, preserving native selection and wheel
  scrolling. Application-owned transcript selection is an explicit compatibility
  mode (`--mouse app`).
- Redirected, unknown, or explicitly plain terminals use the chronological
  fallback without cursor-control sequences.
- Provider and tool text is sanitized before terminal output.
- Rendering never relies on color alone; no-color and ANSI-16 paths preserve
  structure.

## Transcript and input

The transcript is semantic blocks rather than a terminal framebuffer. Wrapped
layouts are cached per block, and streaming invalidates only changed blocks.
The default primary-screen path exposes committed rows to native scrolling and
selection while redrawing only a mutable suffix. The
optional application-owned selection mode, copy, resize, and new streamed output
retain stable semantic coordinates. Resume materializes only a bounded tail for
first input; older active-branch blocks are loaded when semantic navigation or
select-all reaches beyond that tail.

Held-key repeats are accepted only for text editing and navigation. One-shot
actions such as submit, panel confirmation, close, abort, and tool expansion
require a fresh key press.

The composer supports multiline editing, bracketed paste, large-paste chips,
media attachments, dropped paths, and gitignore-aware `@` completion. Media is
capability-gated at attachment time and remains ordered with text when submitted.

## Tool presentation

Tool calls default to deterministic intent summaries. Protocol arguments and
bulk output stay hidden unless needed for a failure summary. Users can:

- press Ctrl+O to expand or collapse the most recent tool or shell block;
- toggle the latest or a named call with `/tool [call-id]`;
- expand or collapse all calls with `/verbose [on|off]`.

Per-call expansion state is independent, survives transcript reflow, and never
changes provider-visible tool results. Bounded output, unified edit diffs, exit
status, and recoverable failures remain inspectable. Completed `exec` calls
show a bold command followed by at most five muted tail lines and a quiet
`Took ...` duration. Their compact hint counts only UI-hidden lines that Ctrl+O
can reveal; a separate warning identifies output already discarded by bounded
process capture.

## Sessions and resources

`/tree` presents durable entry IDs and kinds in a deterministic connector tree.
It marks every ancestor on the selected branch with `+`, the exact durable head
with `*`, and keeps abandoned forks visible. `/checkout <entry-id>` changes the
durable head and hydrates the selected branch. `/reload` recomposes AGENTS
instructions, reloads theme files, rescans skills, and rebuilds the Agent only
at an idle boundary.

Model selection is available through a picker, direct `/model <id>`, and
`/cycle-model`. Thinking choices include only the active model's advertised
`min_effort..=max_effort` range.

## Active-run controls

- Enter queues a follow-up.
- Ctrl+S steers at the next model boundary.
- Escape or Ctrl+C interrupts active work.
- Safe presentation commands execute immediately.
- Model, reasoning, session, compaction, reload, and checkout work is queued in
  order and applied after the active `Run` releases its Agent borrow.
