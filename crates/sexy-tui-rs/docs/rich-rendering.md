# Rich rendering architecture

## Boundary

The library owns generic presentation mechanics. Applications own domain events,
transcript policy, tool/provider/model state, persistence, and scheduling.
Nothing in the rich-text AST assumes a coding agent.

The pipeline is:

```text
Markdown or typed values
        │
        ▼
Document<Block<Inline>>      UnifiedDiff
        │                         │
        └────────────┬────────────┘
                     ▼
        width-aware semantic layout
                     ▼
        typed runs (style + optional safe URL)
                     ▼
       terminal encoder / plain encoder
                     ▼
             RenderedLine[]
```

Escape sequences never occur in semantic values. `RenderedLine::styled` is the
terminal form; `RenderedLine::plain` and `RenderedDocument::copy_text` are
escape-free.

## Semantic model

`Document` contains paragraphs, headings, code blocks, nested ordered/unordered
lists, quotes, dividers, tables, disclosure details, and plain fallback blocks. Inline values contain
text, semantic roles, explicit typed style spans, emphasis, strong,
strikethrough, code, links, and soft/hard breaks.

Use `Inline::Status` when a state needs a non-color success/warning/error/pending
marker, and `Inline::Role` for other application-defined presentation. The generic
roles (`Accent`, `Success`, `Warning`, `Error`, `Muted`, and so on) do not encode
what generated a message.

Unsupported Markdown and HTML-like input remains visible as text. Image Markdown
renders a visible `[image: alt] (destination)` link; terminal graphics are a
separate generic API.

## Static Markdown

`rich_text::markdown::parse` adapts `pulldown-cmark` CommonMark/GFM events. Tight
list items receive implicit paragraphs, preserving adjacent text/code/emphasis
runs. Parser source ranges expose top-level starts for streaming without storing
another full event tree.

The renderer owns a small, bounded syntax cache. Cache values are semantic scope
roles—not syntect colors—so a theme change does not rerun syntax parsing. The
cache has no global render mutex and evicts after 24 entries or approximately
4 MiB of source.

With `default-features = false`, fenced code uses one readable code role. Unknown
languages do the same. The compact syntect default set maps TypeScript through
JavaScript and TOML through properties syntax; all other requested common
languages use native definitions.

## Streaming Markdown

`StreamingMarkdown` has four conceptual pieces:

1. exact authoritative raw bytes;
2. a lossy display string with only malformed UTF-8 replaced;
3. committed top-level semantic blocks;
4. one mutable suffix preview.

Incomplete UTF-8 scalar values remain in a small pending buffer. An unclosed
fence is detected by an incremental lexical scanner and its code payload is
appended directly—there is no full Markdown reparse per token. Ordinary long
suffixes are reparsed at geometric thresholds or proven block boundaries, and
CommonMark work is capped at `MAX_UNSTABLE_PARSE_BYTES` (64 KiB). A huge single
mutable block falls back to visible literal preview until completion.

The stable-prefix rule is deliberately conservative: a top-level block commits
only after a following top-level start proves its end. Finalization parses the
complete display text once; therefore `finish()` equals static parsing regardless
of chunk boundaries.

`StreamingRenderCache` retains committed rows at a stable width/theme revision
and lays out only newly committed blocks plus the mutable preview. Width or theme
changes trigger a deterministic reflow. Callers should keep one cache per live
surface.

## Layout contract

`WidthPolicy` is the sole semantic cell policy:

- extended grapheme clusters are indivisible;
- combining sequences have their terminal width;
- CJK and emoji use Unicode display width;
- East Asian Ambiguous width is configurable;
- tabs advance to configured stops from the current cell;
- wrapping prefers whitespace and hard-breaks long identifiers by grapheme;
- a grapheme too wide for a one-cell viewport becomes visible `?` rather than
  violating the row bound.

Paragraphs wrap. Code clips with an ellipsis by default so source line identity
is stable; wrapping is explicit. Tables allocate readable columns and become a
labeled list when too narrow. Lists use hanging indents. Quotes and nested lists
reduce the available inner width rather than overflowing.

`copy_text` is independent of visual wrapping and clipping, so copying does not
lose hidden code suffixes or insert visual table borders.

## Terminal capabilities

`TerminalCapabilities` is shared by terminal backends, themes, rich rendering,
glyphs, links, images, and TUI control flow. There is no separate color or image
capability model.

Detection is passive and conservative. Explicit overrides have final authority.
A custom backend should implement `Terminal::capabilities()` with negotiated
facts. `plain()` is the deterministic fallback for redirected output and tests.

Progressive enhancement rules:

- no Unicode: ASCII bullets, quotes, rules, and table borders;
- no color: textual structure and attributes where usable;
- unknown italics: underlined emphasis;
- no hyperlink support: visible destination only;
- no synchronized output: ordinary differential writes;
- no cursor addressing: no live cursor placement;
- plain: no escape sequence or animation at all.

Private-use icon glyphs require explicit `nerd_font`; no heuristic enables them.

## Safety

All semantic text passes through `sanitize_text` immediately before run layout.
The sanitizer visualizes or strips terminal controls according to output mode,
normalizes line endings, and exposes bidi override characters. Raw bytes remain
available to the application for logs/forensics.

`SafeUrl` allows `http`, `https`, `mailto`, `file`, `ssh`, and `git`, rejects
controls/bidi overrides/active schemes, and percent-encodes unsafe payload bytes.
OSC 8 is emitted only when both capability and validation succeed. Link targets
remain visible even when OSC 8 is active, preventing label-only destination
spoofing.

The retained TUI closes SGR and OSC 8 on every row and on stop, conditionally
closes synchronized output, restores the cursor, and delegates backend cleanup.

## Themes

A `Theme` starts with terminal-neutral defaults. TOML may override stable tokens;
`[colors]`, `[tokens]`, `[spacing]`, and `[icons]` provide friendly aliases.
Runtime token, role-style, and block-style overrides are highest precedence and
survive `reload()`.

RGB values are quantized at encoding time to ANSI 256 or ANSI 16. Semantic syntax
roles remain unchanged. Defaults avoid fixed surfaces, so light and dark
terminals both retain terminal-controlled backgrounds. Applications may inject
one accent with `set_accent` without recoloring unrelated syntax/diff roles.

## Live updates

`LiveRegion` is a generic retained vertical node list:

- `NodeId` gives stable identity;
- a generation in `NodeHandle` rejects updates after remove/reuse;
- active nodes update in place and commit into history;
- committed nodes reject late mutation;
- globally sequenced `RenderUpdate`s reject duplicate/out-of-order events;
- per-node layout caches invalidate only changed content;
- `PlainEvent`s provide chronological noninteractive output.

The region is intentionally single-owner and lock-free. Multi-producer
applications should serialize events in their own event loop before applying
them.

## Performance expectations

Keep parsers/renderers/caches with the UI surface rather than constructing them
per frame. A normal token update should append bytes, update one mutable preview,
and replace one stable live node. Do not rebuild the complete transcript
`Document` merely to display one changed message.

Run the smoke benchmark in release mode:

```sh
cargo run --release --example render_bench --features benchmarks
```

It reports static parse/render time, 7-byte incremental layout time, streaming
reparse bytes, and syntax-cache hit/miss data. The counters are also public for
application telemetry and regression tests.
