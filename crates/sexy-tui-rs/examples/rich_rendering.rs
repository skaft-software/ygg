use sexy_tui_rs::{parse_markdown, RenderOptions, RichRenderer, TerminalCapabilities, Theme};

const DEMO: &str = r#"# Session recovery

The invalid **final record** is removed before the next append. See the
[format documentation](https://example.com/session-format).

## Changes

- preserves valid records without a trailing newline
- removes invalid trailing bytes
- keeps `append()` atomic

> Recovery never rewrites a valid prefix.

```rust
fn recover(path: &Path) -> Result<usize> {
    let valid = scan_records(path)?;
    truncate(path, valid)?;
    Ok(valid)
}
```

| Mode | Result |
| :--- | :--- |
| clean | unchanged |
| partial | trailing bytes removed |
"#;

fn main() {
    let capabilities = TerminalCapabilities::detect();
    let renderer = RichRenderer::new(
        Theme::with_capabilities(capabilities),
        capabilities,
        RenderOptions::default(),
    );
    let width = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(80);
    let rendered = renderer.render(&parse_markdown(DEMO), width);
    println!("{}", rendered.styled_text());
}
