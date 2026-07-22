use std::path::PathBuf;

use sexy_tui_rs::{
    parse_markdown, CapabilityOverrides, ColorDepth, RenderOptions, RichRenderer,
    StreamingMarkdown, SupportLevel, TerminalCapabilities, Theme, WidthPolicy,
};

const FIXTURE: &str = include_str!("fixtures/rich.md");
const WIDTH_GOLDEN: &str = include_str!("goldens/width-matrix.txt");
const CAPABILITY_GOLDEN: &str = include_str!("goldens/capability-matrix.txt");

fn renderer(capabilities: TerminalCapabilities) -> RichRenderer {
    RichRenderer::new(
        Theme::with_capabilities(capabilities),
        capabilities,
        RenderOptions {
            syntax_highlighting: false,
            code_borders: true,
            ..RenderOptions::default()
        },
    )
}

#[test]
fn static_width_matrix_matches_golden_and_cell_bounds() {
    let document = parse_markdown(FIXTURE);
    let renderer = renderer(TerminalCapabilities::plain());
    let mut actual = String::new();
    for width in [20u16, 40, 60, 80, 120, 160] {
        let rendered = renderer.render(&document, width);
        actual.push_str(&format!("===== width {width} =====\n"));
        actual.push_str(&rendered.plain_text());
        actual.push('\n');
        assert!(rendered
            .lines
            .iter()
            .all(|line| { WidthPolicy::default().line_width(&line.plain) <= usize::from(width) }));
    }
    assert_or_update("width-matrix.txt", &actual, WIDTH_GOLDEN);
}

#[test]
fn capability_matrix_matches_golden() {
    let document = parse_markdown(
        "# Heading\n\n**strong** *emphasis* [docs](https://example.com) `code`\n\n> quote\n\n- item",
    );
    let profiles = [
        ("plain-ascii", TerminalCapabilities::plain()),
        (
            "no-color-unicode",
            TerminalCapabilities::interactive(ColorDepth::None, true),
        ),
        (
            "ansi16-ascii",
            TerminalCapabilities::interactive(ColorDepth::Ansi16, false),
        ),
        (
            "ansi256-unicode",
            TerminalCapabilities::interactive(ColorDepth::Ansi256, true),
        ),
        (
            "truecolor-link",
            TerminalCapabilities::interactive(ColorDepth::TrueColor, true).with_overrides(
                &CapabilityOverrides {
                    italics: Some(SupportLevel::Supported),
                    hyperlinks: Some(true),
                    ..CapabilityOverrides::default()
                },
            ),
        ),
    ];
    let mut actual = String::new();
    for (name, capabilities) in profiles {
        actual.push_str(&format!("===== {name} =====\n"));
        let output = renderer(capabilities).render(&document, 60).styled_text();
        actual.push_str(&visualize_controls(&output));
        actual.push('\n');
    }
    assert_or_update("capability-matrix.txt", &actual, CAPABILITY_GOLDEN);
}

#[test]
fn deterministic_adversarial_bytes_never_break_width_or_static_equivalence() {
    let hostile = b"# title\n\ntext \x1b]52;c;Y2xpcA==\x07 **open \xf0\x9f\x92\xa1**\n\n```rs\nfn x() {}\n```\n";
    for seed in 0..64u64 {
        let mut state = seed.wrapping_add(1);
        let mut offset = 0usize;
        let mut stream = StreamingMarkdown::new();
        while offset < hostile.len() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let chunk = 1 + (state as usize % 9);
            let end = (offset + chunk).min(hostile.len());
            stream.push_bytes(&hostile[offset..end]);
            offset = end;
        }
        let expected = parse_markdown(stream.raw_text());
        assert_eq!(stream.finish(), &expected);
        for width in [1u16, 2, 20, 40, 160] {
            let rendered =
                renderer(TerminalCapabilities::plain()).render(stream.committed(), width);
            assert!(rendered.lines.iter().all(|line| {
                !line.styled.contains('\x1b')
                    && WidthPolicy::default().line_width(&line.plain) <= usize::from(width)
            }));
        }
    }
}

fn visualize_controls(value: &str) -> String {
    value.replace('\x1b', "<ESC>").replace('\x07', "<BEL>")
}

fn assert_or_update(name: &str, actual: &str, expected: &str) {
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/goldens")
            .join(name);
        std::fs::write(path, actual).expect("write golden");
        return;
    }
    assert_eq!(
        actual, expected,
        "golden {name} differs; run UPDATE_GOLDENS=1 cargo test --test rich_rendering"
    );
}
