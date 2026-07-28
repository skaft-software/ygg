//! Bash tool-result projection and compact terminal rendering.

use sexy_tui_rs::{visible_width, RichRenderer};

use super::terminal_text::sanitize_for_terminal;
use super::tool_render::tool_value_indent_width;
use super::{fit_line, subdued_text, wrap_hanging, ToolPanel, COMPACT_EXEC_OUTPUT_LINES};
use crate::tui::theme::YggTheme;

fn tool_metadata(panel: &ToolPanel) -> Option<String> {
    if let Some(ref cached) = *panel.cached_metadata.borrow() {
        return cached.clone();
    }
    let result = compute_tool_metadata(panel);
    *panel.cached_metadata.borrow_mut() = Some(result.clone());
    result
}

/// Locate the final canonical `bash` result after any live progress bytes.
/// The bash tool streams output while it runs, then emits a durable envelope
/// containing the exit status and bounded stdout/stderr capture. The panel
/// retains both, so presentation should prefer the last envelope without
/// mutating the stored tool result.
fn final_bash_result(output: &str) -> &str {
    for (index, _) in output.rmatch_indices("exit=") {
        let candidate = &output[index..];
        let mut lines = candidate.lines();
        let header = lines.next().unwrap_or_default();
        if !header
            .split_whitespace()
            .any(|part| part.starts_with("duration=") && part.len() > "duration=".len())
        {
            continue;
        }
        let next = lines.next().unwrap_or_default().trim();
        if index == 0 || next == "(no output)" || is_bash_stream_header(next) {
            return candidate;
        }
    }
    output
}

fn is_bash_stream_header(line: &str) -> bool {
    ["stdout", "stderr"].into_iter().any(|stream| {
        let Some(detail) = line
            .strip_prefix(stream)
            .and_then(|line| line.strip_prefix(':'))
        else {
            return false;
        };
        let detail = detail.trim();
        detail.is_empty()
            || detail
                .strip_suffix(" lines")
                .is_some_and(|count| count.parse::<usize>().is_ok())
            || (detail.contains(" bytes, showing first ") && detail.contains(" and last "))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BashCaptureTruncation {
    stream: &'static str,
    omitted_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CompactBashOutput {
    lines: Vec<String>,
    omitted_lines: usize,
    capture_truncations: Vec<BashCaptureTruncation>,
    panel_elided: bool,
}

fn bash_capture_footer(line: &str) -> Option<(&'static str, &str)> {
    ["stdout", "stderr"].into_iter().find_map(|stream| {
        line.strip_prefix("truncated_")
            .and_then(|line| line.strip_prefix(stream))
            .and_then(|line| line.strip_prefix('='))
            .map(|detail| (stream, detail))
    })
}

fn is_bash_complete_footer(line: &str) -> bool {
    ["stdout", "stderr"].into_iter().any(|stream| {
        line.strip_prefix("complete_")
            .and_then(|line| line.strip_prefix(stream))
            .is_some_and(|detail| detail == "=true")
    })
}

/// Project a bounded result into Pi-style tail output. Protocol envelope lines
/// are excluded; capture loss is retained separately because Ctrl+O can reveal
/// UI-tail omissions but cannot recover bytes discarded by the bash tool.
fn compact_bash_output(panel: &ToolPanel, expanded: bool) -> CompactBashOutput {
    let result = sanitize_for_terminal(final_bash_result(&panel.output));
    let mut capture_truncations = Vec::new();
    for line in result.lines().map(str::trim) {
        let Some((stream, detail)) = bash_capture_footer(line) else {
            continue;
        };
        if detail == "false" {
            continue;
        }
        let omitted_bytes = detail
            .split_whitespace()
            .find_map(|part| part.strip_prefix("omitted_bytes:"))
            .and_then(|count| count.parse::<usize>().ok());
        capture_truncations.push(BashCaptureTruncation {
            stream,
            omitted_bytes,
        });
    }

    let capture_was_truncated = !capture_truncations.is_empty();
    let failure_reason = panel.failure_reason.as_deref().map(str::trim);
    let mut content = Vec::new();
    let mut panel_elided = false;
    let mut protocol_error = false;
    let mut expect_stream_header = false;
    for (line_index, raw) in result.lines().enumerate() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if line_index == 0 && trimmed.starts_with("error ") {
            protocol_error = true;
            expect_stream_header = true;
            continue;
        }
        if trimmed.starts_with("exit=") && trimmed.contains("duration=") {
            expect_stream_header = true;
            continue;
        }
        if expect_stream_header && is_bash_stream_header(trimmed) {
            protocol_error = false;
            expect_stream_header = false;
            continue;
        }
        if bash_capture_footer(trimmed).is_some() || is_bash_complete_footer(trimmed) {
            expect_stream_header = true;
            continue;
        }
        if trimmed.is_empty()
            || trimmed == "(no output)"
            || (capture_was_truncated && trimmed == "...")
            || (content.is_empty() && failure_reason.is_some_and(|reason| reason == trimmed))
        {
            continue;
        }
        if trimmed == "… older tool output elided …" {
            panel_elided = true;
            continue;
        }
        content.push(line.to_owned());
        if !protocol_error {
            expect_stream_header = false;
        }
    }

    let omitted_lines = if expanded {
        0
    } else {
        let omitted_lines = content.len().saturating_sub(COMPACT_EXEC_OUTPUT_LINES);
        if omitted_lines > 0 {
            content.drain(..omitted_lines);
        }
        omitted_lines
    };
    CompactBashOutput {
        lines: content,
        omitted_lines,
        capture_truncations,
        panel_elided,
    }
}

pub(super) fn bash_output_changes_when_expanded(panel: &ToolPanel) -> bool {
    compact_bash_output(panel, false) != compact_bash_output(panel, true)
}

fn compute_tool_metadata(panel: &ToolPanel) -> Option<String> {
    if !matches!(panel.name.as_str(), "bash" | "exec") {
        return None;
    }
    let output = final_bash_result(&panel.output);
    if let Some(duration) = output
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .find_map(|part| part.strip_prefix("duration="))
        .map(|value| value.trim_end_matches([',', ';']))
        .filter(|value| !value.is_empty())
    {
        return Some(
            if duration.chars().last().is_some_and(char::is_alphabetic) {
                duration.to_owned()
            } else {
                format!("{duration}s")
            },
        );
    }
    None
}

fn bash_content_gutter() -> usize {
    let action = "Bash";
    visible_width(action) + 6usize.saturating_sub(visible_width(action)).max(2)
}

pub(super) fn render_compact_bash_output(
    panel: &ToolPanel,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    show_tool_duration: bool,
    output_indent: &str,
) -> Vec<String> {
    let compact = compact_bash_output(panel, expanded);
    let ellipsis = if theme.unicode() { "…" } else { "..." };
    let mut lines = Vec::new();
    let mut first_detail = true;
    let push_output = |lines: &mut Vec<String>, first_detail: &mut bool, output: String| {
        *first_detail = false;
        lines.extend(wrap_hanging(
            &subdued_text(theme, &output),
            output_indent,
            output_indent,
            width,
        ));
    };
    let push_metadata = |lines: &mut Vec<String>, first_detail: &mut bool, detail: String| {
        *first_detail = false;
        lines.extend(wrap_hanging(
            &subdued_text(theme, &detail),
            output_indent,
            output_indent,
            width,
        ));
    };
    if compact.panel_elided {
        push_metadata(
            &mut lines,
            &mut first_detail,
            format!(
                "{ellipsis} (older live output was elided before display; unavailable to expand)"
            ),
        );
    }
    for truncation in compact.capture_truncations {
        let detail = truncation
            .omitted_bytes
            .map_or_else(|| "some bytes".to_owned(), |bytes| format!("{bytes} bytes"));
        push_metadata(
            &mut lines,
            &mut first_detail,
            format!(
                "{ellipsis} ({} capture omitted {detail}; unavailable to expand)",
                truncation.stream
            ),
        );
    }
    if compact.omitted_lines > 0 {
        let unit = if compact.omitted_lines == 1 {
            "line"
        } else {
            "lines"
        };
        push_metadata(
            &mut lines,
            &mut first_detail,
            format!("{ellipsis} {} {unit} hidden", compact.omitted_lines),
        );
    }
    for output_line in compact.lines {
        push_output(&mut lines, &mut first_detail, output_line);
    }
    if first_detail {
        push_metadata(
            &mut lines,
            &mut first_detail,
            if panel.finished {
                "(no output)".to_owned()
            } else {
                "(waiting for output)".to_owned()
            },
        );
    }
    if show_tool_duration {
        if let Some(duration) = tool_metadata(panel) {
            lines.push(fit_line(
                &subdued_text(theme, &format!("{output_indent}Took {duration}")),
                width,
            ));
        }
    }
    lines
}

pub(super) fn render_bash_row(
    command: &str,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
) -> Vec<String> {
    let action = "Bash";
    let action_gap = tool_value_indent_width(action).saturating_sub(visible_width(action));
    let prefix = format!(
        "{}{}",
        theme.bold(&theme.fg("foreground", action)),
        " ".repeat(action_gap)
    );
    let continuation = " ".repeat(bash_content_gutter());
    let content_width = width
        .saturating_sub(u16::try_from(visible_width(&prefix)).unwrap_or(u16::MAX))
        .max(1);
    let command = renderer.render_inline_syntax(command, "bash", content_width);
    let use_plain = theme.capabilities().color == crate::tui::terminal::ColorDepth::None;
    command
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 { &prefix } else { &continuation };
            let content = if use_plain { line.plain } else { line.styled };
            fit_line(&format!("{prefix}{content}"), width)
        })
        .collect()
}
