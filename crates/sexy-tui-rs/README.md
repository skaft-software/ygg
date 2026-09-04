# sexy-tui-rs

A small retained terminal UI and a reusable semantic rich-text renderer for Rust.
It renders directly to terminal rows (no Ratatui dependency), keeps differential
updates stable, and degrades to deterministic escape-free text.

**Ygg vendored package 0.3.1 · workspace MSRV Rust 1.86**

## Highlights

- Typed `Document` / `Block` / `Inline` rich text; raw ANSI is not the content model.
- CommonMark + GFM Markdown: headings, emphasis, links, quotes, nested/task lists,
  fenced code, rules, tables, autolinks, and visible fallback text.
- Stable-prefix, bounded-tail streaming Markdown with arbitrary UTF-8 byte chunks.
- Optional `syntect` highlighting mapped to semantic theme roles.
- Unified diffs with visible `+`/`-` prefixes and optional line numbers.
- One grapheme/display-cell width policy for CJK, combining marks, emoji, and tabs.
- Pure `TextEditor` buffer/cursor/layout model with grapheme-safe edits, visual-row motion,
  and marker-free structured cursor projections; applications retain key, theme, and submission policy.
- Conservative terminal capabilities, explicit overrides, Unicode/ASCII glyph sets,
  color quantization, safe OSC 8 links, complete plain mode, and a bounded
  out-of-band Kitty/iTerm2 image foundation.
- Stable-ID `LiveRegion` updates with stale-generation rejection and chronological
  log events for noninteractive frontends.
- Retained `Component`/`TUI` line-differential rendering and resize reflow.
- A crate-level `#![forbid(unsafe_code)]` contract for the complete renderer and
  terminal abstraction.

## Workspace dependency

Ygg consumes this directory directly and pins its package version exactly:

```toml
[dependencies]
sexy-tui-rs = { version = "=0.3.1", path = "../sexy-tui-rs" }
```

The path above is relative to `crates/ygg-coding-agent`; adjust it for another
workspace. The `0.3.1` vendored line has not been synchronized to a public
standalone tag, so use this Ygg source rather than assuming an external release.

Default features include syntax highlighting:

```toml
# Smaller build; unknown/all code remains readable plain code.
sexy-tui-rs = { version = "=0.3.1", path = "../sexy-tui-rs", default-features = false }
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

## Terminal-image foundation

The image API is intentionally out-of-band: image protocol bytes are never
returned as component rows, copy text, or log text. Validate only caller-owned
bytes with `TerminalImage`, derive a bounded `ImageRenderPlan`, render its
`semantic_rows()` normally, and write its optional `ImageTerminalCommand`
separately to the terminal output sink. Emit a real-image command while the
cursor is at its first reserved row, before advancing the blank semantic
reservation, but never concatenate it into a row string. That separation keeps
selection, scrollback, diagnostics, and plain mode free of APC/OSC payloads.

```rust,no_run
use sexy_tui_rs::{
    ImageCapabilities, ImageCapabilityOverrides, ImageId, ImageLimits, ImagePlanner,
    ImageViewport, TerminalCapabilities,
};

# fn place(image: sexy_tui_rs::TerminalImage) -> Result<(), Box<dyn std::error::Error>> {
let terminal = TerminalCapabilities::detect();
let images = ImageCapabilities::detect(&terminal, &ImageCapabilityOverrides::default());
let planner = ImagePlanner::new(images, ImageLimits::default());
let viewport = ImageViewport::with_capabilities(80, 24, images)?;
let plan = planner.plan_place(ImageId::new(1)?, &image, viewport)?;

let semantic_rows = plan.semantic_rows(); // safe retained-frame/copy text
let mut protocol = Vec::new();
plan.write_protocol_to(&mut protocol)?; // write separately at the placement point
# let _ = semantic_rows;
# Ok(())
# }
```

`TerminalImage` accepts bounded static PNG, JPEG, GIF, and WebP container
headers without decoding or reading files, URLs, environment paths, or network
data; animated PNG/GIF/WebP containers are rejected before terminal-side frame
decoding. The direct protocol matrix is conservative: Kitty receives PNG only;
iTerm2 receives PNG, JPEG, and GIF. Unsupported terminals, formats, and
unaddressable operations receive deterministic ASCII fallback rows. `ImageRegistry`
allocates nonzero IDs monotonically and never reuses deleted values; iTerm2 replacement
and targeted deletion are deliberately unavailable instead of being guessed.

`ImageLimits` has default and non-bypassable hard caps for source bytes,
dimensions, pixels, metadata, base64 chunks, complete protocol output, replies,
and query deadlines. `ImageRegistry` also caps concurrent live IDs. Capability
detection is hint-only and sends no I/O.
Callers that perform a single bounded query can use
`ImageCapabilityQuery::parse_reply` to correlate only the expected reply type;
forced protocol selection is for tests or caller-managed negotiation, never
plain output.

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

## Text editing model

`TextEditor` is a reusable multiline text model, not a terminal-event parser or
styled widget. It owns editable UTF-8 text, a cursor that is always an extended
grapheme boundary, visual wrapping in terminal cells, and marker-free structured
cursor metadata. The embedding application supplies its usable text width after
reserving prompt, border, and padding columns.

```rust
use sexy_tui_rs::{TextEditAction, TextEditor};

let mut editor = TextEditor::with_text("alpha beta");
editor.apply(TextEditAction::Home, 6);
assert_eq!(editor.cursor(), "alpha ".len()); // second visual row

let projection = editor.projection(6);
let (before, after) = projection.cursor_parts(editor.text()).unwrap();
assert_eq!(format!("{before}<cursor>{after}"), "<cursor>beta");
```

`Char`, `Paste`, deletion, Left/Right, Up/Down, Home, and End are semantic
`TextEditAction`s. Paste normalizes CRLF and bare CR to LF; ordinary text set by
`set_text` stays authoritative. Up/Down preserve the selected display-cell
column across short rows and reflow. `TextEditorLayout` exposes grapheme-safe
source ranges for renderers, while `TextEditorProjection` supplies the cursor
row, byte offset, and cell column separately from source text. An application
can therefore insert its own trusted terminal marker without searching for or
removing an arbitrary marker-like value in the draft.

The model deliberately does **not** sanitize text, parse terminal input, draw
chrome, attach files, choose focus, or submit a draft. Sanitize only at the
application's render boundary. When that boundary renders a transformed safe
copy rather than the source buffer, keep an application-owned grapheme-safe
source/display map and use `TextEditor::layout_for` or
`TextEditor::projection_for` with the matching mapped display cursor. The
editor's `revision()` is a local cache key for an embedding application's
transformed projection; it changes on text and cursor mutations.

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
`StreamingMarkdownWidget`. Compose only the visible rows borrowed through
`TextEditorProjection::line` into an application-owned component; use its
structured cursor metadata to place a trusted hardware-cursor marker without
inspecting source text.

Interactive rendering defaults to a direct Rust port of Pi's retained-frame
algorithm at the pinned revision. It writes the complete first frame, tracks
Pi's logical/hardware cursor and viewport state, updates the exact first-to-last
changed range, lets pure CRLF appends enter native scrollback, and clears saved
lines plus replays the complete frame on width/height changes or changes above
the old viewport. Every interactive frame uses Pi's CSI 2026 delimiters. The
legacy embedded-Kitty compatibility path retains Pi's image row reservation,
changed-range expansion, targeted deletion, and fallback replay behavior; new
image callers should use the out-of-band `ImageRenderPlan` foundation above.
`set_clear_on_shrink`,
`set_show_hardware_cursor`, and `request_render_force` expose the corresponding
Pi policies.

`set_inline_scrollback(true)` retains the older Ygg-specific pinned-frame
experiment as an explicit compatibility extension. It is not the Pi-equivalent
core and Ygg's coding-agent frontend no longer enables it.

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
capability profiles. `tests/pi_tui_render.rs` contains named physical-terminal
ports of the pinned Pi resize, shrink, changed-range, cursor, synchronized-frame,
and Kitty-placement rendering cases. Additional tests cover malformed Markdown,
arbitrary byte chunk boundaries, hostile terminal controls,
CJK/combining/emoji layout, syntax-cache hits, stale live updates, and the
explicit legacy inline extension.

## Scope and provenance

The semantic rich renderer is a Rust-specific extension. Pi TUI remains the
normative reference for shared core behavior, and complete editor, autocomplete,
widget, and test parity has not been claimed. The pinned source, import history,
and current port status are recorded in [`VENDORED.md`](VENDORED.md) and
[`UPSTREAM-PARITY.md`](UPSTREAM-PARITY.md).

## License

MIT
