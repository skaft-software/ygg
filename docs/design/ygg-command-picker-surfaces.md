# Ordinary command and picker surface contract

**Status:** Current implementation contract.

This contract gives transient command discovery and picker surfaces one ordinary
TUI vocabulary. It applies over the shared grid in
[`ygg-presentation.md`](ygg-presentation.md) and the terminal mechanics in
[`ygg-tui.md`](ygg-tui.md); it does not introduce a second renderer, theme, or
modal system.

An **ordinary surface** is a short-lived command suggestion list, picker, or
its adjacent status. It helps a user find, inspect, and choose an already
available command or item. It is not a persistent dashboard, provider-branded
view, diagnostic console, or authority boundary.

## Scope and ownership

- Inline slash and path discovery remain part of the composer. The composer
  supplies their title and purpose, so an inline list need not repeat a panel
  heading.
- A picker supplies a title and, where needed, one concise purpose line. Its
  driver remains responsible for fetching, mutation, cancellation, and
  confirmation.
- The existing `PanelAction`, picker driver, and keymap remain authoritative.
  Presentation neither makes an item selectable nor grants a capability.
- Approval panels remain enforcement surfaces. They retain the separate
  no-filter/no-count and visible-selected-action rules in `ygg-tui.md`; this
  ordinary contract must not weaken them.
- Existing surfaces migrate only when their owner needs this vocabulary. This
  contract is deliberately not a mandate to rewrite every panel in one change.

## Surface record and hierarchy

Every ordinary surface can project the following semantic record. A field can
be absent when it would only repeat context already supplied by its owner (for
example, the composer owns an inline command list's title and purpose), or when
constrained geometry must retain an actionable row.

| Field | Contract |
| --- | --- |
| Title | A short imperative or noun phrase identifying the task, never a provider slogan. |
| Purpose | One subdued sentence explaining the result of a selection or the current operation. |
| Content | Selectable rows and non-selectable headings in a deterministic order. A heading is metadata, never a keyboard stop. |
| Status | An explicit textual lifecycle state, optionally with one bounded useful detail. |
| Search | A visible filter/query only when typing changes the candidate set. Approval choices never get one. |
| Count | The filtered selection position and total, or an explicit empty state. It describes candidates, not invisible headings. |
| Focus | One visual focus marker on the keyboard-active row or control. It follows the active shell accent, not a candidate's provider. |
| Selection | The semantic item identity/index returned to the driver. Filtering, resize, and live replacement preserve or revalidate it by the surface's existing rules. |
| Metadata | Subdued, terminal-safe facts that help compare rows. It moves below the primary label at regular widths and may share a row only when width permits. |
| Actions | A quiet footer naming only keys the current owner already handles. It is not a second command parser. |

Read title and purpose first, then current status/search/count, focused primary
content, metadata, and finally action hints. This order keeps the active choice
legible without turning passive facts into competing chrome.

## Shared grid and structural space

Ordinary surfaces use `PresentationLayout`; they do not create a new inset or
frame geometry.

| Column | Use |
| --- | --- |
| `0` | Full-width rules/cards when a surface already uses them, and the one-cell event/prompt/focus marker. |
| `2` | Primary title, label, and action text after the marker gutter. |
| `4` | Nested purpose, detail, and row metadata. |

At narrow widths (the deterministic fixture uses `46×8`), retain title, focused
primary text, explicit state, and the shortest useful action hint. Metadata can
be omitted before primary text is truncated. At `80×24`, rows use the ordinary
stacked label/detail rhythm. At `120×40` and wider, metadata may share a row
with its label when the shared layout selects columns; it must not invent a
separate provider column.

An empty detail line, a reserved metadata rhythm line, or the one breathing row
between durable content and the composer is structural space, not missing
content. Renderers must preserve that space when it keeps row identity, focus,
or the shared grid stable. They must not fill it with a border, placeholder, or
permanent status dashboard.

Borders remain optional existing chrome. A surface may be unframed on a narrow
terminal; this contract does not require a bordered modal.

## Status and recovery grammar

When an ordinary surface presents lifecycle state, its semantic copy and its
visible text use this grammar:

```text
[<marker> ]<state>[ · <bounded useful detail>]
```

| State | Required visible word | Typical detail and recovery |
| --- | --- | --- |
| Loading | `loading` | What is being discovered or refreshed; navigation remains limited to materialized candidates. |
| Success | `completed` or a concrete completed-result verb such as `renamed` | What changed, followed by the ordinary next action if one exists. |
| Empty | `no …` | The searched scope or candidate kind, plus a clear/filter/scope action when available. |
| Recoverable error | `failed` | A terminal-sanitized bounded reason and an existing retry, back, or close action. |
| Cancellation | `cancelled` | Only after the owner acknowledges cancellation; Escape keeps its current owner-specific behavior until then. |

When a surface presents a marker, it is redundant with the word: Unicode may
use the semantic success, warning, error, or pending glyph; ASCII uses its
existing one-cell fallback. Colour can reinforce hierarchy but is never the
only status signal. A missing row is not a success, and a cancelled operation
is never relabelled as a completion.

Status detail is presentation data, not a raw protocol envelope. It is
credential-redacted at the producing boundary where applicable, terminal-
sanitized before rendering, bounded before wrapping, and uses the same
sanitized semantic projection wherever the owning surface supports copy. Frame
characters, ANSI, hidden rows, and action hints are never recovered as copied
content.

## Search, count, focus, and actions

- Filtering changes only the visible candidate projection. Confirming a match
  returns the original semantic index or stable ID already owned by the driver.
- Count uses candidate rows only. Provider/group headings and status rows never
  become selectable or inflate the count.
- Focus follows keyboard ownership. Up/Down, PageUp/PageDown, Home/End, Enter,
  Escape, Left, and Ctrl+D retain the behavior assigned by the current picker
  or document driver; a footer may describe those keys but cannot redefine
  them.
- A live list preserves focus by its stable identity where one exists and
  revalidates that identity immediately before a privileged follow-up.
- All label, purpose, metadata, status-detail, and query strings originating
  outside trusted presentation code are terminal-sanitized. Inline command
  labels, hints, and descriptions become one terminal-safe display cell before
  width measurement or trusted-theme styling; selection and completion retain
  their raw command identity.

The common action-footer grammar is:

```text
<scope-or-count> · <key> <verb> · <key> <verb> [· <key> <verb>]
```

For example, command discovery uses `commands · ↑↓ navigate · ↵ select · esc
close`; ASCII falls back to `commands - up/down navigate - enter select - esc
close`. At compact widths, scope drops first. The remaining key/verb pairs are
complete priority-ordered segments: the rightmost close/cancel pair drops, then
select, before navigation; only navigation can truncate after every optional
segment is gone. A footer never claims a key that the current surface does not
handle. Destructive or trust-changing actions keep their existing explicit
approval semantics rather than being made safe by a terse footer.

## Capability variants

The information architecture is invariant across terminal profiles:

| Capability | Contract |
| --- | --- |
| Unicode | Use the compiled default's single-cell semantic glyphs and concise arrows. |
| ASCII | Use the existing ASCII marker, separator, ellipsis, and key-name fallbacks; no Unicode glyph is required for comprehension. |
| Truecolor/256/16 colour | Preserve the same words, focus marker, grid, count, and action grammar. Accent identifies active shell focus, never provider authority. |
| No colour | Emit no ANSI styling. Explicit status words, marker shape, indentation, and action text still distinguish every state. |
| Reduced motion | Keep the same static frame, state word, marker footprint, and focus. Disable nonessential animation rather than substituting changing text or geometry. |

## Deterministic contract fixtures

The focused Rust fixture matrix lives in
`crates/ygg-coding-agent/src/tui/view/ordinary_surface_contract_tests.rs`. It
uses fixed labels, metadata, filter state, selection, and command query; it
asserts semantic rows and width bounds rather than terminal-byte sequences.
That leaves PTY/frame-byte regression coverage to its own future issue while
making those tests able to reuse the same stable surface facts.

| Fixture | Size | Capability profile | Required observations |
| --- | --- | --- | --- |
| `narrow-46x8` | `46×8` | Unicode, truecolor | Compact label rhythm; title, count, focus, and action footer survive. |
| `narrow-ascii-40x8` | `40×8` | ASCII, truecolor | Scope and close hint drop before a navigation/select pair is clipped. |
| `regular-80x24` | `80×24` | Unicode, truecolor | Stacked label/metadata rhythm and filter/count survive. |
| `large-120x40` | `120×40` | Unicode, truecolor | Shared-row metadata is eligible without changing selection semantics. |
| `wide-144x48` | `144×48` | Unicode, truecolor | Wide content remains bounded and does not grow a dashboard. |
| `ascii-80x24` | `80×24` | ASCII, truecolor | ASCII focus marker, separators, and key names communicate the same choices. |
| `no-color-80x24` | `80×24` | Unicode, no colour | No ANSI styling; explicit words retain status and action meaning. |
| `reduced-motion-80x24` | `80×24` | Unicode, truecolor, animation disabled | Static projection matches the ordinary semantic frame. |

The fixtures also exercise loading, success, empty, recoverable-failure, and
cancellation semantics; title/purpose hierarchy; hostile prompt, skill, and
extension command metadata in rich and no-colour projections; and the
intentional blank metadata rhythm. They do not decide stale-startup-row
replacement, terminal replay, cursor byte streams, or PTY behavior. Those are
separate concerns owned by the renderer and its dedicated regressions.
