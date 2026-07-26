# Ygg TUI design

**Status:** Current implementation contract.

The interactive frontend owns terminal setup/restoration and presentation only;
`Agent` remains the sole model/tool runtime.

## Terminal guarantees

- The interactive frontend renders on the primary screen. `auto`, `terminal`,
  and `off` use a logical-height frame whose committed rows flow into
  terminal-native scrollback. Explicit `--mouse app` uses a bounded,
  application-owned semantic viewport instead.
- A theme swap clears and repaints every cell in the visible viewport.
  Application-owned history is semantic and therefore adopts the current theme
  whenever it becomes visible. Rows already committed by a terminal-owned mode
  cannot be rewritten through portable terminal APIs and retain their original
  styling; Ygg preserves that history rather than clearing it implicitly.
- Raw mode, bracketed paste, keyboard enhancements, synchronized output, and
  mouse reporting are enabled only when supported and restored idempotently.
- Mouse reporting is disabled by default, preserving native drag selection and
  wheel scrolling. `--mouse app` explicitly enables capture for semantic
  scrolling and selection. Portable terminal protocols do not report a user's
  native scrollback offset, so terminal-owned modes cannot guarantee stable
  read-while-streaming anchoring.
- Redirected, unknown, or explicitly plain terminals use the chronological
  fallback without cursor-control sequences.
- Provider, tool, and user text is sanitized before terminal output.
- Rendering never relies on color alone; no-color and ANSI-16 paths preserve
  structure.
- The generic renderer crate enforces `#![forbid(unsafe_code)]`; OS-specific
  terminal setup remains isolated in Ygg's terminal boundary.

## Transcript and input

The transcript is semantic blocks rather than a terminal framebuffer. Wrapped
layouts are cached per block and width, and streaming invalidates only changed
blocks. The default terminal-owned renderer reuses the retained stable prefix;
ordinary frames lay out and paint only the mutable or newly appended suffix.
Chrome follows logical content height rather than occupying a fixed full-screen
viewport, and committed rows naturally enter terminal history.

A resize reflows the retained semantic transcript at the new width, resets the
terminal's saved-line presentation, and replays Ygg's transcript once. Deferred
session history is materialized before that destructive replay. Explicit
application-owned mode instead uses `follow_tail` and `scroll_from_bottom` to
select one bounded viewport. Scrolling above its tail keeps semantic rows fixed
while one Markdown block continues to grow, increments the new-output state,
and exposes the PageDown return-to-live affordance.

Terminal-owned modes preserve native selection and long-lived scrollback, but
Ygg cannot observe or freeze a reader's position inside that history. Semantic
copy retains stable coordinates in either renderer; application-owned drag
selection is available only while mouse capture is enabled. Resume materializes
only a bounded tail for first input; older active-branch blocks are loaded when
semantic navigation, select-all, or resize replay reaches beyond that tail.

Held-key repeats are accepted only for text editing and navigation. One-shot
actions such as submit, panel confirmation, close, abort, and reasoning/summary expansion
require a fresh key press.

The composer supports multiline editing, bracketed paste, large-paste chips,
media attachments, dropped paths, and gitignore-aware `@` completion. Media is
capability-gated at attachment time and remains ordered with text when submitted.
PDFs are not decoded or sent as multimodal payloads: a dropped PDF receives a
composer chip, but submission resolves that chip to the file path as text so the
model can inspect it with file tools.

The compiled default composer leaves the terminal canvas unfilled and keeps a
restrained perimeter at rest; live work animates a model-colored shimmer around
that perimeter. Fenced Markdown code is borderless and uses `#202630` on known
dark profiles or `#f1f5f4` on known light profiles, falling back to an unpainted
surface when the background is unknown. Named and custom themes keep their
authored chrome.

## Reasoning presentation

Live reasoning is collapsed into exactly two width-bounded rows: a plain-weight
row with a blinking model-colored `•` and model-colored `<heading>`, plus a
subdued `└ (ctrl+o to expand)` disclosure row. The heading cache advances only from
semantic ATX headings or paragraphs consisting solely of bold text. It never
infers a label from body prose, sanitizes provider text before display, and uses
`Thinking` until the model emits a heading. Completed collapsed reasoning has
no rows. `Ctrl+O` preserves the existing full, verbatim Markdown rendering.

## Run outcomes

A failed run keeps the compact `failed · <duration>` lifecycle row and follows it
with the actionable error reason. The reason is credential-redacted at the
inference request boundary, then terminal-sanitized and capped at 4 KiB by the
TUI. It is included in semantic transcript copy so it can be reported without
recovering raw provider envelopes or headers.

## Tool presentation

Tool calls expose deterministic intent and lifecycle rows. Raw protocol
arguments and envelopes, unsanitized failure evidence, and extension-rendered
payloads remain internal accountability/provenance data and are excluded from
transcript copy. For operational feedback, the TUI renders bounded sanitized
projections: search results and Bash/local-shell output use a muted tail, while
edit/write results use a bounded unified diff. Omission metadata distinguishes a
collapsible UI tail from bytes already discarded by the tool capture.

Ctrl+O toggles the global disclosure mode for retained reasoning, compaction,
search output, Bash/local-shell output, and edit/write diffs. `/verbose [on|off]`
controls the same mode; `/tool [call-id]` remains accepted for command
compatibility and reports that evidence is internal. Expansion cannot recover
capture bytes that the tool already discarded.

Final structured tool results remain provider-visible and persisted when the
agent protocol requires them to continue a tool turn. This is operational
model context, not a TUI disclosure channel. Live `ToolProgress` is ephemeral
and is not persisted or sent to the model. Terminal-gate action receipts are
bounded accountability input to the gate checker only, not ordinary model
context.

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
- Escape interrupts active work. Ctrl+C first clears a nonempty draft; with an
  empty draft it interrupts active work and is ignored while idle.
- Ctrl+D requests a coordinated close from every input owner, including
  pickers, tool prompts, lifecycle waits, and local shell commands. Active work
  is aborted and settled before the process exits.
- Safe presentation commands execute immediately.
- Model, reasoning, session, compaction, reload, and checkout work is queued in
  order and applied after the active `Run` releases its Agent borrow.
