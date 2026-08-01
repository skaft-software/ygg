//! Dependency-free smoke benchmark. Run with:
//! `cargo run --release --example render_bench --features benchmarks`

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sexy_tui_rs::{
    parse_markdown, ColorDepth, RenderOptions, RichRenderer, StreamingMarkdown,
    StreamingRenderCache, TerminalCapabilities, Theme,
};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[derive(Clone, Copy, Debug)]
struct AllocationStats {
    allocations: u64,
    bytes: u64,
}

fn reset_allocations() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
}

fn allocation_stats() -> AllocationStats {
    AllocationStats {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
    }
}

fn run_stream(
    source: &str,
    renderer: &RichRenderer,
    lines_only: bool,
) -> (Duration, AllocationStats, sexy_tui_rs::StreamingStats) {
    let mut stream = StreamingMarkdown::new();
    let mut cache = StreamingRenderCache::default();
    reset_allocations();
    let start = Instant::now();
    for chunk in source.as_bytes().chunks(7) {
        stream.push_bytes(chunk);
        if lines_only {
            black_box(cache.render_lines(&stream, renderer, 80, true));
        } else {
            black_box(cache.render(&stream, renderer, 80));
        }
    }
    stream.finish();
    if lines_only {
        black_box(cache.render_lines(&stream, renderer, 80, true));
    } else {
        black_box(cache.render(&stream, renderer, 80));
    }
    let elapsed = start.elapsed();
    let stats = allocation_stats();
    (elapsed, stats, stream.stats())
}

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
    // Run both APIs against the same tokenization workload. The legacy path
    // remains here as a regression baseline for the lines-only hot path.
    let (legacy_stream_elapsed, legacy_allocations, stats) =
        run_stream(&stream_source, &renderer, false);
    let (stream_elapsed, stream_allocations, selected_stats) =
        run_stream(&stream_source, &renderer, true);

    assert_eq!(stats, selected_stats);

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
    println!(
        "7-byte streaming + legacy layout: {legacy_stream_elapsed:?} ({} allocs, {} bytes requested)",
        legacy_allocations.allocations, legacy_allocations.bytes
    );
    println!(
        "7-byte streaming + lines-only layout: {stream_elapsed:?} ({} allocs, {} bytes requested)",
        stream_allocations.allocations, stream_allocations.bytes
    );
    println!(
        "stream parse passes={}, reparsed={} bytes; syntax hits={}, misses={}, cache={} bytes",
        stats.parse_passes, stats.reparsed_bytes, syntax.hits, syntax.misses, syntax.bytes
    );
}
