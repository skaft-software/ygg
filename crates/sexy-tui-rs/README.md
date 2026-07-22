# sexy-tui-rs

A small retained terminal UI and a reusable semantic rich-text renderer for Rust.
It renders directly to terminal rows (no Ratatui dependency), keeps differential
updates stable, and degrades to deterministic escape-free text.

**Version 0.2 · Ygg workspace MSRV Rust 1.86**

## Highlights

- Typed `Document` / `Block` / `Inline` rich text; raw ANSI is not the content model.
- CommonMark + GFM Markdown: headings, emphasis, links, quotes, nested/task lists,
  fenced code, rules, tables, autolinks, and visible fallback text.
- Stable-prefix, bounded-tail streaming Markdown with arbitrary UTF-8 byte chunks.
- Optional `syntect` highlighting mapped to semantic theme roles.
- Unified diffs with visible `+`/`-` prefixes and optional line numbers.
- One grapheme/display-cell width policy for CJK, combining marks, emoji, and tabs.
- Conservative terminal capabilities, explicit overrides, Unicode/ASCII glyph sets,
  color quantization, safe OSC 8 links, and complete plain mode.
- Stable-ID `LiveRegion` updates with stale-generation rejection and chronological
  log events for noninteractive frontends.
- Retained `Component`/`TUI` line-differential rendering and resize reflow.

## Install

```toml
[dependencies]
sexy-tui-rs = "0.2"
```

Default features include syntax highlighting:

```toml
# Smaller build; unknown/all code remains readable plain code.
sexy-tui-rs = { version = "0.2", default-features = false }
```

Features:

| Feature | Default | Purpose |
|---|---:|---|
| `syntax-highlighting` | yes | `syntect` syntax parsing and semantic scope mapping |
| `benchmarks` | no | enables the dependency-free benchmark example |

## Static Markdown

```rust
use sexy_tui_rs::{parse_markdown, RenderOptions, RichRenderer, TerminalCapabilities, Theme};

let capabilities = TerminalCapabilities::detect();
let renderer = RichRenderer::new(
    Theme::with_capabilities(capabilities),
    capabilities,
    RenderOptions::default(),
);
let document = parse_markdown(
    "# Recovery\n\nRemoved the **invalid tail**. See [format](https://example.com/format).",
);
let rendered = renderer.render(&document, 80);

for row in rendered.lines {
    println!("{}", row.styled); // terminal output
}
// `copy_text` is escape-free, sanitized, and keeps visible link targets.
```

Run the full demo:

```sh
cargo run --example rich_rendering -- 80
NO_COLOR=1 cargo run --example rich_rendering -- 40
```

## Typed rich text (without Markdown)

```rust
use sexy_tui_rs::{Block, Document, Inline, StatusKind};

let document = Document::new(vec![
    Block::Heading {
        level: 2,
        content: vec![Inline::Text("Status".into())],
    },
    Block::Paragraph(vec![
        Inline::status(StatusKind::Success, "complete"),
        Inline::Text(" — 12 records".into()),
    ]),
]);
```

`Inline::Styled` supports explicit typed styles when a semantic role is not
sufficient. `Block::Detail(DetailBlock)` provides generic expanded/collapsed
content with visible `[-]`/`[+]` ASCII fallbacks; the application owns its state.
Escape strings are introduced only by the final terminal encoder.

## Streaming Markdown

```rust
use sexy_tui_rs::{RichRenderer, StreamingMarkdown, StreamingRenderCache};

let renderer = RichRenderer::plain();
let mut stream = StreamingMarkdown::new();
let mut layout = StreamingRenderCache::default();

for bytes in [b"# Res".as_slice(), b"ult\n\n**par", b"tial**"] {
    stream.push_bytes(bytes); // incomplete UTF-8 and syntax are safe
    let frame = layout.render(&stream, &renderer, 40);
    // Replace the existing live node with `frame.lines`.
}

let final_document = stream.finish();
// Exactly equal to static parsing of stream.raw_text().
```

The stream retains original `raw_bytes()`, buffers split UTF-8 scalar values,
commits proven top-level prefixes, and limits active CommonMark parsing to a
64 KiB suffix. Unclosed fenced code is accumulated without reparsing the whole
transcript. Finalization performs one authoritative static parse.

## Diffs

```rust
use sexy_tui_rs::{DiffRenderOptions, RichRenderer, UnifiedDiff};

let diff = UnifiedDiff::parse("@@ -1 +1 @@\n-old\n+new");
let output = RichRenderer::plain().render_diff(
    &diff,
    80,
    DiffRenderOptions { line_numbers: true, wrap: false },
);
```

Color never carries the only meaning: source prefixes, headers, binary notices,
renames, and incomplete hunks remain visible in plain text.

## Stable live updates

```rust
use sexy_tui_rs::{LiveRegion, NodeId, RichRenderer};

let mut region = LiveRegion::new(RichRenderer::plain());
let status = region.insert_with_id(NodeId(10), "starting");
assert!(region.update(status, "running"));
assert!(region.commit(status));
assert!(!region.update(status, "late update"));

// A log backend drains these instead of performing cursor rewrites.
for event in region.drain_plain_events() {
    println!("{}", event.text);
}
```

`RenderUpdate { sequence, .. }` rejects duplicate/out-of-order producer events.
Removing and reusing an ID increments its generation, so cancellation cannot
resurrect stale content.

## Capabilities and plain mode

`TerminalCapabilities::detect()` is conservative and never sends query escape
sequences. Detection considers TTY attachment, `TERM`, `TERM_PROGRAM`, locale,
`COLORTERM`, `NO_COLOR`, multiplexers, Windows Terminal, and known terminals.
Callers may apply explicit `CapabilityOverrides` after their own negotiation.

```rust
use sexy_tui_rs::{CapabilityOverrides, ColorDepth, TerminalCapabilities};

let capabilities = TerminalCapabilities::detect().with_overrides(&CapabilityOverrides {
    color_depth: Some(ColorDepth::Ansi256),
    hyperlinks: Some(false),
    ..CapabilityOverrides::default()
});
```

`TerminalCapabilities::plain()` guarantees:

- no SGR, OSC, CSI, cursor movement, image protocol, or animation;
- ASCII structure glyphs;
- visible link destinations;
- copyable chronological output.

Unknown italics fall back to underline for semantic emphasis. Colors quantize to
ANSI 16/256 or disappear while text and structural markers remain.

## Themes

Theme resolution has three layers:

1. restrained built-in semantic defaults;
2. optional TOML values;
3. runtime token/style/block overrides.

```toml
[colors]
accent = "#16876d"
md_link = "#287fb8"
diff_added = "#26a269"
syntax_keyword = "#7656a6"

[spacing]
sm = 2
```

```rust
use sexy_tui_rs::{Color, TerminalCapabilities, Theme};

let mut theme = Theme::load_with_capabilities(Some("theme.toml"), TerminalCapabilities::detect());
theme.set_accent(Color::Rgb(80, 160, 220));
theme.reload(); // reloads TOML and preserves runtime overrides
```

Defaults use terminal foreground/background rather than assuming a dark palette.
Code backgrounds are absent unless a theme explicitly supplies one.

## Width and safety contracts

- Width means terminal display cells, not bytes or Unicode scalar values.
- Wrapping and clipping never split grapheme clusters or ANSI sequences.
- Code clips by default (source rows remain stable); `CodeOverflow::Wrap` is opt-in.
- Every rich-rendered row is bounded at widths including 0 and 1.
- Model/tool/Markdown text is sanitized before encoding. ESC, CSI, OSC (including
  clipboard/title controls), DCS/APC, C0/C1, DEL, and bidi overrides cannot execute.
- OSC 8 uses an allowlisted scheme and percent-encoded payload. A destination is
  always visible when it differs from its label.
- Application code should use semantic APIs for untrusted text. Legacy helpers that
  accept pre-styled ANSI strings are for trusted compatibility content only.

See [`docs/rich-rendering.md`](docs/rich-rendering.md) for architecture and
[`docs/ygg-integration.md`](docs/ygg-integration.md) for migration boundaries.

## Components and TUI

The retained API remains available:

```rust
pub trait Component {
    fn render(&self, width: u16) -> Vec<String>;
    fn handle_input(&mut self, data: &str) {}
    fn invalidate(&mut self) {}
}
```

Rich components include `RichText`, `Markdown`, and
`StreamingMarkdownWidget`. `Input` and `Editor` move/delete whole grapheme
clusters and emit a trusted cursor marker that `TUI` maps to a hardware cell.
`TUI` performs changed-tail updates, full resize reflow, conditional CSI 2026,
and terminal cleanup on interruption/stop.

Terminal event-loop ownership is intentionally backend-specific: construct a
`Terminal`, feed input to `TUI::handle_input`, call `request_render` after state
changes, and call `stop` on every exit path.

## Validation and benchmarks

```sh
cargo test --all-features --all-targets
cargo test --no-default-features
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo run --release --example render_bench --features benchmarks
```

Goldens cover widths 20/40/60/80/120/160 and plain/ANSI16/ANSI256/truecolor
capability profiles. Unit tests include malformed Markdown, arbitrary byte chunk
boundaries, hostile terminal controls, CJK/combining/emoji layout, syntax-cache
hits, stale live updates, and resize redraws.

## License

MIT
