//! Shared tool-output classification, alignment, and compact terminal rendering.

use sexy_tui_rs::{visible_width, DiffRenderOptions, RichRenderer, UnifiedDiff};

use super::terminal_text::sanitize_for_terminal;
use super::{
    subdued_text, understated_tool_output, wrap_hanging, ToolPanel, COMPACT_EXEC_OUTPUT_LINES,
};
use crate::tui::theme::YggTheme;

/// Only promote complete, bare unified diffs into the dedicated diff renderer.
pub(super) fn looks_like_diff(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim_start);
    let Some(first) = lines.find(|line| !line.is_empty()) else {
        return false;
    };
    if first.starts_with("diff --git ") {
        return true;
    }
    // Explanatory Markdown that happens to contain a fenced `diff` block must
    // stay in the Markdown renderer so its prose and fence structure survive.
    first.starts_with("--- ")
        && lines.any(|line| line.starts_with("+++ "))
        && text.lines().any(|line| line.trim_start().starts_with("@@"))
}

fn looks_like_legacy_write_creation(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim_start);
    let Some(first) = lines.find(|line| !line.is_empty()) else {
        return false;
    };
    first == "--- /dev/null" && lines.any(|line| line.starts_with("+++ b/"))
}

pub(super) fn tool_diff(panel: &ToolPanel) -> Option<String> {
    // Only cache when finished — the output may still be streaming.
    if panel.finished {
        if let Some(ref cached) = *panel.cached_diff.borrow() {
            return cached.clone();
        }
    }
    let result = compute_tool_diff(panel);
    if panel.finished {
        *panel.cached_diff.borrow_mut() = Some(result.clone());
    }
    result
}

fn compute_tool_diff(panel: &ToolPanel) -> Option<String> {
    if looks_like_diff(&panel.output) {
        return Some(panel.output.clone());
    }
    if panel.name != "edit" && panel.name != "write" {
        return None;
    }
    let mut offset = 0;
    for line in panel.output.split_inclusive('\n') {
        let candidate = &panel.output[offset..];
        if (line.trim_start().starts_with("--- ") || line.trim_start().starts_with("diff --git "))
            && (looks_like_diff(candidate)
                || (panel.name == "write" && looks_like_legacy_write_creation(candidate)))
        {
            return Some(candidate.to_owned());
        }
        offset += line.len();
    }
    None
}

/// Minimum width reserved for the tool label before its value/output column.
const TOOL_VALUE_MIN_WIDTH: usize = 6;

pub(super) fn tool_display_label(name: &str) -> &'static str {
    match name {
        "read" => "Read",
        "search" => "Explored",
        "edit" => "Edit",
        "write" => "Write",
        _ => "Used",
    }
}

pub(super) fn tool_value_indent_width(label: &str) -> usize {
    TOOL_VALUE_MIN_WIDTH.max(visible_width(label).saturating_add(2))
}

pub(super) fn tool_value_indent(label: &str) -> String {
    " ".repeat(tool_value_indent_width(label))
}

/// Max diff lines to show in terse mode before truncating.
const COMPACT_DIFF_LINES: usize = 10;

/// Render an edit/write diff. Long diffs are truncated in terse mode.
pub(super) fn render_diff_only(
    panel: &ToolPanel,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    output_indent: &str,
) -> Vec<String> {
    let output_indent_width = u16::try_from(visible_width(output_indent)).unwrap_or(u16::MAX);
    let display_line = |line: sexy_tui_rs::RenderedLine| {
        let content = if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
            line.plain
        } else {
            line.styled
        };
        format!("{output_indent}{content}")
    };
    let Some(ref diff) = tool_diff(panel) else {
        return Vec::new();
    };
    let rendered = renderer.render_diff(
        &UnifiedDiff::parse(diff),
        width.saturating_sub(output_indent_width),
        DiffRenderOptions {
            line_numbers: width >= 70,
            wrap: true,
        },
    );
    let mut lines: Vec<String> = rendered.lines.into_iter().map(display_line).collect();
    if !expanded && lines.len() > COMPACT_DIFF_LINES + 1 {
        let remaining = lines.len() - COMPACT_DIFF_LINES;
        lines.truncate(COMPACT_DIFF_LINES);
        let unit = if remaining == 1 { "line" } else { "lines" };
        let hint = format!("{output_indent}{remaining} {unit} hidden");
        lines.push(subdued_text(theme, &hint));
    }
    lines
}

pub(super) fn render_compact_tool_output(
    panel: &ToolPanel,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    output_indent: &str,
) -> Vec<String> {
    let output = sanitize_for_terminal(&panel.output);
    let mut lines = output
        .lines()
        .filter(|line| !line.trim().is_empty() && *line != "(no output)")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let omitted = if expanded {
        0
    } else {
        let omitted = lines.len().saturating_sub(COMPACT_EXEC_OUTPUT_LINES);
        if omitted > 0 {
            lines.drain(..omitted);
        }
        omitted
    };
    let mut rendered = Vec::new();
    if omitted > 0 {
        let unit = if omitted == 1 { "line" } else { "lines" };
        let hint = format!("{omitted} {unit} hidden");
        rendered.extend(wrap_hanging(
            &understated_tool_output(theme, &hint),
            output_indent,
            output_indent,
            width,
        ));
    }
    for line in lines {
        rendered.extend(wrap_hanging(
            &understated_tool_output(theme, &line),
            output_indent,
            output_indent,
            width,
        ));
    }
    rendered
}

pub(super) fn without_redundant_tool_lead(tool: &str, text: &str) -> String {
    let mut words = text.splitn(2, char::is_whitespace);
    let Some(first) = words.next() else {
        return String::new();
    };
    let redundant = match tool {
        "read" => matches!(first, "read" | "reading"),
        "search" => matches!(first, "search" | "searched" | "searching" | "explored"),
        "bash" | "exec" => {
            matches!(
                first,
                "bash" | "exec" | "run" | "ran" | "running" | "failed:"
            )
        }
        "edit" => matches!(first, "edit" | "edited" | "updating" | "updated"),
        "write" => matches!(first, "write" | "wrote" | "writing"),
        _ => matches!(first, "run" | "running" | "finished") || first == tool,
    };
    if redundant {
        words.next().unwrap_or_default().trim_start().to_owned()
    } else {
        text.to_owned()
    }
}
