# Ygg TUI design

**Status:** Current implementation contract.

The interactive frontend owns terminal setup/restoration and presentation only;
`Agent` remains the sole model/tool runtime.

## Terminal guarantees

- The interactive frontend renders on the primary screen. `auto`, `terminal`,
  and `off` use Pi's complete logical-frame renderer: the first frame writes
  every materialized row, pure appends flow naturally into terminal scrollback,
  and a width/height change or mutation above the previous viewport clears the
  screen and saved lines before replaying the complete frame. PageUp transfers
  rendering to the bounded, application-owned semantic viewport for the rest of
  that shell. Explicit `--mouse app` selects that viewport from startup.
- Ygg v0.6.3 uses one compiled default theme. Theme selection and runtime theme reload are disabled; terminal/background capability detection still adapts that default safely. Its model-aware accent palette changes atmosphere without changing layout or semantic status colours.
- Raw mode, bracketed paste, keyboard enhancements, and mouse reporting are
  enabled only when supported and restored idempotently. Matching Pi, every
  interactive frame is bracketed by CSI 2026 synchronized-output markers;
  terminals that do not implement the private mode ignore it, while Ygg's
  backend still uses the markers to batch each frame into one flush. Ygg's
  composer uses the positioned hardware cursor rather than Pi's painted fake
  cursor, so every renderer construction explicitly keeps that cursor visible.
- Mouse reporting is disabled by default, preserving native drag selection and
  wheel scrolling. `--mouse app` enables capture for semantic wheel navigation
  and selection, but keyboard viewport ownership does not depend on capture.
  Portable terminal protocols do not report a user's native scrollback offset,
  so uncaptured wheel history remains terminal-owned.
- Redirected, unknown, or explicitly plain terminals use the chronological
  fallback without cursor-control sequences.
- Provider, tool, and user text is sanitized before terminal output.
- Rendering never relies on color alone; no-color and ANSI-16 paths preserve
  structure.
- The generic renderer crate enforces `#![forbid(unsafe_code)]`; OS-specific
  terminal setup remains isolated in Ygg's terminal boundary.
- Raw ANSI diagnostics are disabled by default. `YGG_TUI_WRITE_LOG` enables an
  exact backend-byte capture to an explicit file or a unique file in an existing
  directory; these traces are sensitive because they include displayed content.

## Transcript and input

The transcript is semantic blocks rather than a terminal framebuffer. Wrapped
layouts are cached per block and width, and streaming invalidates only changed
blocks, but the root component presents the complete materialized logical frame
to `sexy-tui-rs` on every interactive render. The terminal renderer follows Pi's
`previousLines`, logical cursor, hardware cursor, maximum working height, and
previous viewport-top state. It finds the first and last changed physical rows,
repaints only that range when it remains addressable, and uses bottom-row CRLF
appends to let new rows enter native scrollback.

A change above the old viewport cannot be repaired with cursor addressing.
Matching Pi, that path emits `ED 2`, homes, clears saved lines with `ED 3`, and
replays the complete materialized frame. Width changes do the same because line
wrapping changed; height changes do so outside Termux. Disclosure contraction,
theme repaint, overlays, and dynamic composer chrome therefore cannot leave an
unwritten semantic gap in terminal history: they either take Pi's visible-row
differential path or its authoritative full replay path. Kitty image placements
participate in the same changed-range expansion, targeted deletion, reserved-row
painting, and full-replay fallback as upstream Pi.

Default terminal-owned resume materializes the complete active branch before it
is rendered, because terminal scrollback cannot prepend a deferred prefix later.
Explicit application-owned mode may retain the bounded tail-first hydration
optimization: semantic PageUp, selection, or copy can materialize older rows in
that mode without claiming they already exist in native history.
Application-owned mode—selected at startup by `--mouse app` or claimed by
PageUp—uses `follow_tail` plus a monotonic transcript commit ID, semantic copy
text offset/affinity, visual fallback, and desired screen row to select one
bounded viewport. `scroll_from_bottom` remains only the cheap navigation delta;
the semantic anchor rebases it after growth, contraction, deferred-history
prepends, and width/height changes. Scrolling above the tail keeps semantic rows
fixed while one Markdown block continues to grow, increments the new-output
state, and exposes the PageDown return-to-live affordance.

Terminal-owned modes preserve native selection and ordinary append scrollback,
but Ygg cannot observe or freeze a reader's position. A Pi full replay replaces
the application's saved-line presentation and therefore returns the terminal to
the live frame. Semantic copy retains stable coordinates in either renderer;
application-owned drag selection is available only while mouse capture is
enabled. Terminal-owned resume eagerly loads the complete active branch;
application-owned resume loads a bounded tail and materializes older blocks when
semantic navigation or selection reaches them.

Held-key repeats are accepted only for text editing and navigation. One-shot
actions such as submit, panel confirmation, close, abort, and reasoning/summary expansion
require a fresh key press.

The composer supports multiline editing, bracketed paste, large-paste chips,
media attachments, dropped paths, gitignore-aware `@` completion, and Tab
completion for relative, parent, home-relative, and absolute path tokens.
Slash-command discovery, file mentions, and filesystem completion render inline
directly below the composer. While matches are visible, the suggestion surface
temporarily replaces the model and token status row; the status returns as soon
as completion closes. Matches use compact rows with action hints in a footer,
and the active match and hint keys use the selected model's adaptive accent.
Executable-extension status/header/footer contributions never occupy that row.
Generic presentation snapshots do not create persistent chrome. The first-party
`ygg-subagents` observation surface is the bounded exception: while an owning
run has workers, the host renders its complete owner-fenced `subagent` roster in
a persistent transcript event immediately above the composer. Each worker uses
a content-free task/phase line and a structured
`Tool Calls • ↑input ↓output • cost` line; ordinary tool disclosure never
truncates the roster. A
nonblocking 250 ms host tick invokes the owner-scoped status command, coalesces
with normal extension events, and retains the last accepted snapshot on failure.
Live child cost is added to the host-owned cumulative footer only until root
settlement persists matching `delegated_agent` usage records; idle rendering
therefore cannot count it twice.

`/extensions` opens an interactive installed-bundle activation panel instead.
The no-argument `/subagents` command supplied by `ygg-subagents` opens a
frontend-owned worker list; Up/Down moves focus, Enter opens the selected bounded
read-only transcript, and Escape or Left returns from the transcript to the
list. While open, the same owner-bound status command reconciles the host's
authoritative worker state and publishes complete presentation revisions; the
frontend preserves focus by stable node ID and revalidates the latest typed
session reference immediately before opening. Transcript panels start at the
live tail and support arrow, PageUp/PageDown, Home, and End scrolling.
Directory completions retain their trailing separator so completion can continue
one level at a time, and whitespace in completed names is backslash-escaped.
Media is capability-gated at attachment time and remains ordered with text when
submitted. PDFs are not decoded or sent as multimodal
payloads: a dropped PDF receives a composer chip, but submission resolves that
chip to the file path as text so the model can inspect it with file tools.

The compiled default composer leaves the terminal canvas unfilled. It is
framed by a top and bottom rule in a restrained form of the model accent at
rest and the captured executing model accent while focused or active, but it
never animates. The rules hold model identity; the transcript owns liveness.
Content rows span the full width with no side borders, so prompt text
selected from the composer copies without border characters. Historical prompts
mark only their first content row; wrapped and explicit continuation rows use a
blank indent instead of a vertical rail. Historical prompts with a persisted
model-color highlight include one painted internal padding row above and below
their content; those decorative rows stay outside semantic copy.
Fenced Markdown code is borderless and uses the compiled default's terminal-adaptive shading. The unknown-profile fallback remains unpainted.

## Reasoning presentation

Every accepted run opens a stable `Working` row immediately. It becomes
`Thinking` with a subdued, aligned `└ (ctrl+o to expand)` disclosure only after
the provider emits an actual reasoning delta, and returns to `Working` while
public output or finalization continues. The steady model-colored event-margin
dot does not repaint on a timer. The heading cache advances only from semantic
ATX headings or paragraphs consisting solely of bold text; it never infers a
label from body prose and sanitizes provider text before display. Compaction uses
`Compacting context`, and tool execution may retain its tool-specific lifecycle
row. Expanded reasoning keeps the same inset without an event-margin dot or a
synthetic first-row bullet. Only authoritative run settlement removes transient
activity rows.

## Run outcomes

A failed run keeps the compact `failed · <duration>` lifecycle row and follows it
with the actionable error reason. The reason is credential-redacted at the
inference request boundary, then terminal-sanitized and capped at 4 KiB by the
TUI. It is included in semantic transcript copy so it can be reported without
recovering raw provider envelopes or headers.

## Tool presentation

Tool calls expose deterministic intent and lifecycle rows. Event-margin dots
identify active collapsed reasoning, assistant responses, and tool or shell
execution, and every dot uses the same glyph footprint. The collapsed-reasoning
dot remains steady; active tool and shell dots may pulse through foreground and
muted tones rather than changing size. Successful completed event dots use green,
and failed tools use red. Raw protocol arguments and envelopes, unsanitized
failure evidence, and extension-rendered payloads remain internal
accountability/provenance data
and are excluded from transcript copy. For operational feedback, the TUI renders
bounded sanitized projections: search results and Bash/local-shell output use a
muted tail, while edit/write results use a bounded unified diff. Omission
metadata distinguishes a collapsible UI tail from bytes already discarded by
the tool capture.

Ctrl+O toggles the global disclosure mode for retained reasoning, compaction,
search output, Bash/local-shell output, and edit/write diffs. `/verbose [on|off]`
controls the same mode. Expansion cannot recover capture bytes that the tool
already discarded.

Final structured tool results remain provider-visible and persisted when the
agent protocol requires them to continue a tool turn. This is operational
model context, not a TUI disclosure channel. Live `ToolProgress` is ephemeral
and is not persisted or sent to the model. Terminal-gate action receipts are
bounded accountability input to the gate checker only, not ordinary model
context.

## Sessions and resources

The session resume panel opens with current-workspace sessions and can lazily
switch to all workspace directories. It supports fuzzy, quoted-phrase, and
`re:` regular-expression filtering; recent, title, and message-count ordering;
named-only filtering; optional path details; pinned/fork/current markers;
recoverable trash; and in-place renaming. Cross-workspace rows are browseable
but cannot be resumed into a differently scoped live App.

`/fork` opens a bounded active-branch user-message picker, including a
whole-conversation head row, and restores the selected prompt into the new
composer. `/clone` copies the active head without opening a picker. Both create
provenance metadata before the ordinary idle-boundary session rebuild.

`/tree` presents durable entry IDs and kinds in a deterministic connector tree.
It marks every ancestor on the selected branch with `+`, the exact durable head
with `*`, and keeps abandoned forks visible. `/checkout <entry-id>` changes the
durable head and hydrates the selected branch. `/reload` recomposes AGENTS
instructions, rescans skills and prompts, and rebuilds the Agent only at an idle
boundary.

Model selection is available through a picker or direct `/model <id>`. Thinking
choices include only the active model's advertised `min_effort..=max_effort`
range.

`/extensions` lists managed executable bundles only; the separately packaged
`ygg-serve` application is not an activation target. Enter updates only the
selected name in the user config's `enabled_extensions`, never trust, then
rebuilds the Agent and extension host at the idle boundary so enable and disable
take effect immediately. A project or explicit definition shadowing the managed
global bundle is visible but not toggleable from this menu. If project,
environment, or command-line activation participates in the effective list, the
menu is read-only rather than claiming a user-config edit will survive the next
launch; project precedence is rechecked at action time. Enabled-but-unavailable
bundles remain disable-only, while source-changing trust, tool collisions, and
explicit required-tool removal fail closed.

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
