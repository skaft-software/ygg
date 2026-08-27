# Ygg TUI design

**Status:** Current implementation contract.

The interactive frontend owns terminal setup/restoration and presentation only;
`Agent` remains the sole model/tool runtime.

## Terminal guarantees

- The interactive frontend renders on the primary screen. `auto`, `terminal`,
  and `off` begin with a logical-height frame whose committed rows flow into
  terminal-native scrollback. PageUp transfers rendering to the bounded,
  application-owned semantic viewport for the rest of that shell. Explicit
  `--mouse app` selects that viewport from startup.
- A theme swap clears and repaints every cell in the visible viewport.
  Application-owned history is semantic and therefore adopts the current theme
  whenever it becomes visible. Rows already committed by a terminal-owned mode
  cannot be rewritten through portable terminal APIs and retain their original
  styling; Ygg preserves that history rather than clearing it implicitly.
- Raw mode, bracketed paste, keyboard enhancements, synchronized output, and
  mouse reporting are enabled only when supported and restored idempotently.
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
blocks. The default terminal-owned renderer reuses the retained stable prefix;
ordinary frames lay out and paint only the mutable or newly appended suffix. It
separately tracks immutable physical rows and width-independent semantic commit
cursors: stable rows enter history through bottom-row newlines, while finalized
Ctrl+O-sensitive blocks cross only at complete semantic boundaries. Composer
completions, panels, reports, and other temporary chrome paint as bounded
screen-relative surfaces without advancing that history seam. If a streaming
Markdown layout contracts behind rows already owned by the terminal, the
renderer similarly freezes the ledger and repaints a temporary surface until
the semantic viewport catches up, then stages newly stable rows exactly once.
Ordinary streaming therefore avoids replaying a shifted grid while a terminal
user reads scrollback. Chrome follows logical content height rather than
occupying a fixed full-screen viewport, and committed rows naturally enter
terminal history.

A text-only native resize accepts the terminal's saved-line reflow and repaints
one visible grid without clearing scrollback. If a replacement transcript
generation arrives in the same frame, old native history remains terminal-owned
but the new semantic tape starts at row zero rather than inheriting that history
seam. Temporary surfaces and Kitty placements retain a bounded destructive
fallback because their physical state cannot be reconstructed portably. Deferred
session history remains lazy and is loaded only when semantic navigation or copy
crosses into it.
Application-owned mode—selected at startup by `--mouse app` or claimed by
PageUp—uses `follow_tail` plus a monotonic transcript commit ID, semantic copy
text offset/affinity, visual fallback, and desired screen row to select one
bounded viewport. `scroll_from_bottom` remains only the cheap navigation delta;
the semantic anchor rebases it after growth, contraction, deferred-history
prepends, and width/height changes. Scrolling above the tail keeps semantic rows
fixed while one Markdown block continues to grow, increments the new-output
state, and exposes the PageDown return-to-live affordance.

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
run has workers, the host renders its owner-fenced `subagent` activities in a
live block immediately above the composer. Each worker uses a content-free task/
phase line and a structured `Tool Calls • ↑input ↓output • cost` line. A
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
Fenced Markdown code is borderless and uses `#202630` on known dark profiles or
`#f1f5f4` on known light profiles, falling back to an unpainted surface when the
background is unknown. Named and custom themes keep their authored chrome.

## Reasoning presentation

Live reasoning is collapsed into a plain-weight model-colored row with a
blinking model-colored dot in the event margin, plus a subdued, aligned
`└ (ctrl+o to expand)` disclosure row. The heading cache advances only from
semantic ATX headings or paragraphs consisting solely of bold text. It never
infers a label from body prose, sanitizes provider text before display, and uses
`Thinking` until the model emits a heading. A reasoning-off wait uses the
truthful label `Working` but no disclosure affordance; compaction similarly uses
`Compacting context`. Expanded reasoning keeps the same inset without an
event-margin dot or a synthetic first-row bullet. Completed collapsed reasoning
and transient activity leave no rows.

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
dot blinks; other active dots pulse through foreground and muted tones rather
than changing size. Successful completed event dots use green, and failed tools
use red. Raw protocol arguments and envelopes, unsanitized failure evidence,
and extension-rendered payloads remain internal accountability/provenance data
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
instructions, reloads theme files, rescans skills, and rebuilds the Agent only
at an idle boundary.

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
