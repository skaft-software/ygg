//! Bash tool-result projection and compact terminal rendering.

use sexy_tui_rs::{visible_width, RichRenderer};

use super::output_window::bounded_tail_rows;
use super::terminal_text::{normalize_carriage_return_progress, sanitize_for_terminal};
use super::tool_render::tool_value_indent_width;
use super::{fit_line, subdued_text, wrap_hanging, ToolPanel, COMPACT_EXEC_OUTPUT_ROWS};
use crate::tui::theme::YggTheme;

/// Locate the final canonical `bash` result after any legacy live progress
/// bytes. New panels replace live output at completion, while hydrated sessions
/// may still contain the older concatenated representation.
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

/// Project a bounded result into display-oriented output. Protocol envelope
/// lines are excluded; capture loss is retained separately because Ctrl+O can
/// reveal UI-tail omissions but cannot recover bytes discarded by the tool.
fn compact_bash_output(panel: &ToolPanel) -> CompactBashOutput {
    let normalized = normalize_carriage_return_progress(final_bash_result(&panel.output));
    let result = sanitize_for_terminal(&normalized);
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

    CompactBashOutput {
        lines: content,
        capture_truncations,
        panel_elided,
    }
}

/// Disclosure-sensitive Bash rows must cross native history only at a complete
/// semantic boundary. Width-dependent wrapping makes every retained output row
/// conservatively sensitive, including a single long logical line.
pub(super) fn bash_output_changes_when_expanded(panel: &ToolPanel) -> bool {
    let compact = compact_bash_output(panel);
    compact.panel_elided || !compact.capture_truncations.is_empty() || !compact.lines.is_empty()
}

fn bash_content_gutter() -> usize {
    let action = "Bash";
    visible_width(action) + 6usize.saturating_sub(visible_width(action)).max(2)
}

fn capture_loss_details(compact: &CompactBashOutput) -> Vec<String> {
    let mut details = Vec::new();
    if compact.panel_elided {
        details.push("older live output was elided; unavailable to expand".to_owned());
    }
    for truncation in &compact.capture_truncations {
        let omitted = truncation
            .omitted_bytes
            .map_or_else(|| "some bytes".to_owned(), |bytes| format!("{bytes} bytes"));
        details.push(format!(
            "{} capture omitted {omitted}; unavailable to expand",
            truncation.stream
        ));
    }
    details
}

pub(super) fn render_compact_bash_output(
    panel: &ToolPanel,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    output_indent: &str,
) -> Vec<String> {
    let compact = compact_bash_output(panel);
    let ellipsis = if theme.unicode() { "…" } else { "..." };
    let loss_details = capture_loss_details(&compact);
    if panel.is_error && !expanded {
        let hidden = vec![fit_line(
            &format!(
                "{output_indent}{}",
                subdued_text(theme, "(failed output hidden; ctrl+o to expand)")
            ),
            width,
        )];
        return bounded_tail_rows(hidden, COMPACT_EXEC_OUTPUT_ROWS, false, |_| {
            unreachable!("short failure placeholder never needs omission metadata")
        });
    }
    let mut output_rows = Vec::new();
    for output_line in compact.lines {
        output_rows.extend(wrap_hanging(
            &subdued_text(theme, &output_line),
            output_indent,
            output_indent,
            width,
        ));
    }
    if output_rows.is_empty() {
        let placeholder = if panel.finished {
            "(no output)"
        } else {
            "(waiting for output)"
        };
        output_rows.extend(wrap_hanging(
            &subdued_text(theme, placeholder),
            output_indent,
            output_indent,
            width,
        ));
    }

    if expanded {
        let mut rows = Vec::new();
        for detail in loss_details {
            rows.extend(wrap_hanging(
                &subdued_text(theme, &format!("{ellipsis} ({detail})")),
                output_indent,
                output_indent,
                width,
            ));
        }
        rows.extend(output_rows);
        return rows;
    }

    let force_metadata = !loss_details.is_empty();
    bounded_tail_rows(
        output_rows,
        COMPACT_EXEC_OUTPUT_ROWS,
        force_metadata,
        move |hidden_rows| {
            let mut details = loss_details;
            if hidden_rows > 0 {
                let unit = if hidden_rows == 1 { "row" } else { "rows" };
                details.push(format!("{hidden_rows} earlier visual {unit} hidden"));
            }
            let detail = format!("{ellipsis} {}", details.join(" · "));
            fit_line(
                &format!("{output_indent}{}", subdued_text(theme, &detail)),
                width,
            )
        },
    )
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
