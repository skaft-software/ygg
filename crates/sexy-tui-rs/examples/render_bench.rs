//! Dependency-free smoke benchmark. Run with:
//! `cargo run --release --example render_bench --features benchmarks`

use std::hint::black_box;
use std::time::Instant;

use sexy_tui_rs::{
    parse_markdown, ColorDepth, RenderOptions, RichRenderer, StreamingMarkdown,
    StreamingRenderCache, TerminalCapabilities, Theme,
};

fn fixture(repetitions: usize) -> String {
    let section = r#"## Recovery step

The invalid **final record** is removed before the next append.

- preserve records
- truncate a partial tail

```rust
fn recover(bytes: &[u8]) -> Result<usize> {
    scan(bytes)
}
```

| state | action |
| --- | --- |
| valid | preserve |
| partial | truncate |

"#;
    section.repeat(repetitions)
}

fn main() {
    let source = fixture(250);
    let capabilities = TerminalCapabilities::plain();
    let renderer = RichRenderer::new(
        Theme::with_capabilities(capabilities),
        capabilities,
        RenderOptions::default(),
    );

    let parse_start = Instant::now();
    let document = parse_markdown(black_box(&source));
    let parse_elapsed = parse_start.elapsed();

    let render_start = Instant::now();
    for _ in 0..50 {
        black_box(renderer.render(black_box(&document), 80));
    }
    let render_elapsed = render_start.elapsed();

    let stream_source = fixture(100);
    let stream_start = Instant::now();
    let mut stream = StreamingMarkdown::new();
    let mut cache = StreamingRenderCache::default();
    for chunk in stream_source.as_bytes().chunks(7) {
        stream.push_bytes(chunk);
        black_box(cache.render(&stream, &renderer, 80));
    }
    stream.finish();
    black_box(cache.render(&stream, &renderer, 80));
    let stream_elapsed = stream_start.elapsed();
    let stats = stream.stats();

    // Exercise syntax-cache misses and hits when that feature is enabled.
    let syntax_capabilities = TerminalCapabilities::interactive(ColorDepth::TrueColor, true);
    let syntax_renderer = RichRenderer::new(
        Theme::with_capabilities(syntax_capabilities),
        syntax_capabilities,
        RenderOptions::default(),
    );
    black_box(syntax_renderer.render(&document, 80));
    black_box(syntax_renderer.render(&document, 80));
    let syntax = syntax_renderer.syntax_cache_stats();

    println!(
        "fixture: {} bytes, {} blocks",
        source.len(),
        document.blocks.len()
    );
    println!("static parse: {parse_elapsed:?}");
    println!("50 static renders: {render_elapsed:?}");
    println!("7-byte streaming + live layout: {stream_elapsed:?}");
    println!(
        "stream parse passes={}, reparsed={} bytes; syntax hits={}, misses={}, cache={} bytes",
        stats.parse_passes, stats.reparsed_bytes, syntax.hits, syntax.misses, syntax.bytes
    );
}
