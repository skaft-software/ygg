# Ygg integration guide

This is integration guidance, not a library dependency direction. Ygg keeps all
coding-agent concepts; `sexy-tui-rs` remains a generic presentation crate.

## Keep in Ygg

Do **not** move these into `sexy-tui-rs`:

- `RunId`, `RunTracker`, `RunPhase`, outcomes, cancellation, and stale-run rules;
- `AgentEvent` interpretation and output-channel policy;
- tool invocation/result models, argument summaries, approval, and failure policy;
- provider/model identity and model-lab classification;
- transcript ordering, persistence, compaction, and verbose-tool policy;
- event-loop scheduling and async producer arbitration.

In particular, `crates/ygg-coding-agent/src/presentation.rs` should remain the
authoritative state machine. The generic renderer consumes its presentation
results; it must not learn what a run, tool call, provider, or model is.

## Composer editor boundary

`TextEditor` owns only the reusable text mechanics: the UTF-8 buffer,
grapheme-boundary cursor, semantic `TextEditAction`s, visual-wrap layout,
vertical preferred-column movement, and plain cursor-marker projection. The
coding agent keeps every product decision around it:

- `tui/keymap.rs` translates crossterm events and exports its `EditAction` as the
  generic `TextEditAction`;
- `ShellState` owns one `TextEditor` instead of a string plus a separate byte
  cursor;
- composer paste classification, attachment chips, slash/path completion,
  submission, focus, and adaptive composer chrome remain in Ygg;
- terminal-control visualization remains at Ygg's render boundary. When that
  produces a safe display copy, render it with `TextEditor::layout_for` or
  `TextEditor::render_projection_for`, not by mutating the source draft.

Ygg computes and passes the usable text-cell width after its prompt marker,
chrome, and padding reservations. It must not expose mutable buffer access or
reimplement grapheme movement around the generic model. A non-empty projection
marker occurs exactly once; Ygg still decides whether a panel suppresses it and
whether the retained TUI shows a hardware cursor.

## Replace generic duplicates

The current Ygg workspace pins vendored package `0.3.1`. When replacing any
remaining generic mechanisms, preserve these ownership boundaries:

| Current Ygg location | Replace with |
|---|---|
| `tui/terminal.rs` capability/color structs and heuristics | `TerminalCapabilities`, `CapabilityProbe`, `CapabilityOverrides`, `ColorDepth` |
| `tui/highlight.rs` regex rules | fenced `CodeBlock` + `RichRenderer` optional syntax feature |
| `tui/view.rs::sanitize_for_terminal` | semantic content + renderer sanitization; `sanitize_text` for explicit log boundaries |
| local ANSI width/protection wrappers | `WidthPolicy`, rich layout, and compatibility `visible_width` only for trusted legacy ANSI |
| shell-owned draft string + byte cursor + visual editor layout | `TextEditor`, `TextEditorLayout`, and `TextEditorProjection`; keep attachment and composer policy in Ygg |
| local RGB → ANSI16/256 quantization | typed `Color` + `Theme` encoder |
| local structural glyph decisions | `GlyphSet::for_capabilities` |
| local Markdown line formatting | `parse_markdown` / `StreamingMarkdown` |
| revision-only generic live rows | `LiveRegion` or one `StreamingRenderCache` per mutable transcript block |

Ygg may retain a thin `YggTheme` adapter while migrating, but it should delegate
terminal encoding and quantization to `Theme`. Application-specific contrast
selection and model palette classification can produce typed colors or semantic
overrides, then call `set_accent`, `override_token`, or `override_style`.

## Dependency pin

Ygg uses an exact package version and a workspace-relative path:

```toml
sexy-tui-rs = { version = "=0.3.1", path = "../sexy-tui-rs" }
```

This is intentional: the vendored `0.3.1` history is not yet available from a
public standalone tag. [`../VENDORED.md`](../VENDORED.md) records the imported
standalone revision and the synchronization boundary. Do not replace the path
with an unverified branch or tag.

## Capability handoff

Ygg's CLI `ColorMode` and explicit plain flag remain application policy. Map them
to overrides on the central profile rather than maintaining a second capability
type.

```rust
let mut overrides = CapabilityOverrides::default();
overrides.plain = Some(config.plain);
if config.plain {
    overrides.interactive = Some(false);
} else {
    overrides.color_depth = Some(match config.color {
        ColorMode::Never => ColorDepth::None,
        // Map Auto/forced modes according to Ygg's CLI contract.
        _ => detected.color_depth,
    });
}
let capabilities = TerminalCapabilities::detect().with_overrides(&overrides);
```

Have `YggTerminal` implement:

```rust
fn capabilities(&self) -> sexy_tui_rs::TerminalCapabilities {
    self.capabilities
}
```

Store that exact profile in the terminal, renderer, theme, glyph selection, and
image policy. Ygg remains responsible for primary-screen raw-mode guards and
panic restoration; the generic TUI also performs Pi-compatible stop cleanup.
Test both cleanup layers as idempotent.

Ygg's terminal write coalescing recognizes Pi's CSI 2026 frame markers. The
Pi-equivalent interactive renderer emits them for every frame even when a
terminal merely ignores the private mode; the plain and explicit compatibility
paths can still issue ordinary non-synchronized writes, so the backend must
flush those correctly as well.

Ygg must leave `TUI::set_inline_scrollback` disabled. The default interactive
path is the pinned Pi renderer and consumes the component's complete logical
frame; enabling the compatibility extension reintroduces the semantic-commit
ledger and changes resize/offscreen-mutation semantics. Ygg's composer emits a
cursor marker but does not paint Pi's fake inverted cursor cell, so the product
must call `TUI::set_show_hardware_cursor(true)` on every renderer construction
and resume. Incremental Markdown and transcript caches remain useful for
constructing that frame, but they are not a replacement terminal algorithm.

## Transcript mapping

Keep `TranscriptBlock` and its domain variants in Ygg. Add a conversion at the
view boundary:

- finalized assistant prose → `Document` from `parse_markdown`;
- streaming assistant prose → a per-block `StreamingMarkdown`;
- user/tool/status summaries → typed `Document` and generic roles;
- patches → `UnifiedDiff`;
- deliberately literal output → `Block::Plain`;
- important application statuses → `Inline::Status`; secondary styling → semantic `TextRole`s.

Do not concatenate the complete transcript and reparse it. Each mutable assistant
block owns:

```text
StreamingMarkdown
StreamingRenderCache
stable transcript identity/revision (owned by Ygg)
```

On a text delta, append only the delta and touch only that transcript block. On
completion, call `finish()`, retain the final `Document`, and drop streaming
state. `finish()` gives static/streaming semantic equivalence.

Ygg's raw transcript remains authoritative. Do not replace persisted bytes with
sanitized display text. The renderer sanitizes only at the terminal/copy
boundary.

## Stable live status and tool nodes

There are two valid integration shapes.

### Keep Ygg's transcript cache

Retain `ShellState::block_revisions` and `TranscriptCache`, but change each
`render_block` implementation to call one persistent `RichRenderer`. Keep a
`StreamingRenderCache` in mutable Markdown blocks. This is the smallest first
migration.

### Adopt `LiveRegion`

Map Ygg-owned IDs to generic `NodeId`s and retain the returned generation-safe
`NodeHandle`. Ygg still decides when a status is active, when a tool is committed,
and where it belongs. It then calls only generic operations:

```text
insert → update in place → commit or remove
```

Serialize async producer events in Ygg's event loop and assign monotonically
increasing `RenderUpdate::sequence` values. A cancelled/finished run should
remove or commit its handles before later producer events are drained; stale
handles will then be rejected by generation/state checks.

For redirected/plain output, drain `PlainEvent`s in sequence and write each
semantic state once. Do not print spinner frames, cursor controls, or repeated
full transcripts.

## Links and trust boundary

Stop pre-escaping content into ANSI strings before passing it to rich rendering.
Pass raw semantic text. This applies to model tokens, tool stdout/stderr, paths,
provider errors, Markdown, diff text, and editor-visible pasted values.

Use `SafeUrl`/semantic `Inline::Link`; never build OSC 8 manually. The visible
render includes a destination when label and target differ. Keep Ygg's URL/domain
policy if it is stricter than the generic allowlist.

For domain strings used outside rich rendering (window title, shell command,
filesystem operation, HTTP request), continue to apply domain-specific validation.
Terminal sanitization is not command, path, or network validation.

## Suggested migration sequence

1. **Pin and compile:** update the dependency; implement
   `Terminal::capabilities`; adapt renamed central fields.
2. **One capability profile:** map CLI flags to overrides and delete duplicate
   detection tests only after equivalent central tests plus Ygg policy tests pass.
3. **Theme delegation:** keep Ygg model/background policy, but emit typed colors
   and let `Theme` quantize/apply them.
4. **Static assistant Markdown:** replace local parser/line formatting for
   finalized blocks. Compare transcript goldens at 20/40/60/80/120/160 columns.
5. **Streaming per block:** append event deltas to `StreamingMarkdown`; verify the
   final `Document` equals the static parser for recorded provider chunk traces.
6. **Code/diffs:** remove regex highlighting and local diff coloring after semantic
   code/diff render snapshots pass under plain, ANSI16, ANSI256, and truecolor.
7. **Sanitization/width:** remove local terminal sanitizer and ANSI workarounds only
   after hostile-sequence and editor CJK/emoji tests use the generic path.
8. **Live status:** either retain revision cache or move generic row identity to
   `LiveRegion`; keep `RunTracker` unchanged.
9. **Plain mode:** add redirected-output tests proving chronological, escape-free
   output with no spinner-frame spam.
10. **Delete dead adapters:** remove duplicate dependencies such as `regex` or
    direct `unicode-width` only when no other Ygg subsystem uses them.

## Acceptance checks in Ygg

- Existing `presentation.rs` stale-run/cancellation tests remain unchanged.
- Recorded streaming responses produce the same final semantic document when
  split at every relevant UTF-8/Markdown boundary.
- Only the active transcript block changes during a token update.
- Resize reflows all cached rich rows without changing transcript order.
- Tool output containing OSC 52, CSI erase/query, DCS/APC, BEL, invalid UTF-8,
  or bidi overrides cannot execute a terminal command.
- Editor cursor cells remain correct around CJK, combining marks, emoji, CRLF paste,
  soft wraps, and resize; a cursor-marker projection contains one marker.
- ANSI16/light and plain snapshots retain all status/diff/link meaning.
- Panic, Ctrl-C, cancellation, and normal exit restore cursor/raw/paste/sync state.
- Release benchmark data shows bounded streaming reparse bytes and syntax-cache
  hits on repeated code blocks.

These checks preserve the design boundary: Ygg decides *what happened* and what
belongs in its transcript; `sexy-tui-rs` decides *how generic semantic content is
laid out safely for a terminal*.
