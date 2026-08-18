use sexy_tui_rs::wrap_text_with_ansi;
use unicode_segmentation::UnicodeSegmentation;

use crate::presentation::{format_duration, RunOutcome};

use super::outcome_render::{bounded_outcome_detail, completion_text};
use super::terminal_text::sanitize_for_terminal;
use super::tool_render::looks_like_diff;
use super::{ShellState, TranscriptBlock};

/// Durable transcript coordinate. It deliberately names a semantic block and
/// an offset in that block's clean copy text, never a terminal row. Reflow,
/// streaming, and composer animation can therefore not invalidate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TranscriptPosition {
    pub(super) block: usize,
    pub(super) offset: usize,
    /// At a wrapped boundary, retain which side the pointer came from.
    pub(super) trailing_affinity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TranscriptSelection {
    pub(super) anchor: TranscriptPosition,
    pub(super) focus: TranscriptPosition,
}

/// Clean semantic text used by the application-owned selection/copy path.
/// It intentionally never uses visual rows, ANSI styling, borders, elision,
/// composer text, or footer text.
pub(super) fn block_copy_text(block: &TranscriptBlock) -> String {
    match block {
        TranscriptBlock::User { text, .. } | TranscriptBlock::Notice(text) => {
            sanitize_for_terminal(text)
        }
        TranscriptBlock::NoticeStatus { text, .. } => sanitize_for_terminal(text),
        TranscriptBlock::Compaction(compaction) => format!(
            "{}\n{}",
            sanitize_for_terminal(&compaction.label),
            sexy_tui_rs::parse_markdown(&compaction.summary).plain_text()
        ),
        TranscriptBlock::Assistant(markdown) => {
            sexy_tui_rs::parse_markdown(&markdown.text).plain_text()
        }
        TranscriptBlock::Reasoning(reasoning) => {
            sexy_tui_rs::parse_markdown(reasoning.markdown.raw_text()).plain_text()
        }
        TranscriptBlock::Tool(panel) => {
            let summary = if panel.finished {
                if panel.is_error {
                    &panel.display.failure
                } else {
                    &panel.display.success
                }
            } else {
                &panel.display.active
            };
            let text = if let Some(command) = &panel.display.shell_command {
                format!("$ {command}")
            } else {
                format!("{}  {summary}", panel.display.label)
            };
            sanitize_for_terminal(&text)
        }
        TranscriptBlock::Shell(shell) => {
            let status = if shell.running {
                "running"
            } else if shell.exit_code == 0 {
                "completed"
            } else {
                "failed"
            };
            sanitize_for_terminal(&format!("$ {} [{status}]", shell.command))
        }
        TranscriptBlock::Outcome(outcome) => match &outcome.outcome {
            RunOutcome::Completed { elapsed, .. }
            | RunOutcome::CompletedWithWarnings { elapsed, .. } => {
                completion_text(*elapsed, " · ", outcome.tokens_per_second)
            }
            RunOutcome::Failed { elapsed, reason } => format!(
                "failed · {}\n{}",
                format_duration(*elapsed),
                bounded_outcome_detail(reason.as_str())
            ),
            RunOutcome::Interrupted { elapsed } => {
                format!("interrupted · {}", format_duration(*elapsed))
            }
            RunOutcome::NeedsInput { prompt } => format!("needs input · {prompt}"),
            RunOutcome::Cancelled { elapsed } => {
                format!("cancelled · {}", format_duration(*elapsed))
            }
        },
    }
}

/// Side-effect-free semantic selection projection. Prompt expansion can read
/// this without mutating the retained copy buffer or touching the clipboard.
pub(super) fn semantic_selected_text(state: &ShellState) -> Option<String> {
    let selection = state.transcript_selection.clone()?;
    let (start, end) = if (selection.anchor.block, selection.anchor.offset)
        <= (selection.focus.block, selection.focus.offset)
    {
        (selection.anchor, selection.focus)
    } else {
        (selection.focus, selection.anchor)
    };
    let mut blocks = Vec::new();
    for index in start.block..=end.block {
        let text = block_copy_text(state.transcript.get(index)?);
        let from = if index == start.block {
            clamp_copy_offset(&text, start.offset)
        } else {
            0
        };
        let to = if index == end.block {
            clamp_copy_offset(&text, end.offset)
        } else {
            text.len()
        };
        blocks.push(text[from.min(to)..to].to_owned());
    }
    Some(blocks.join("\n\n"))
}

fn clamp_copy_offset(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn visual_col_to_offset(line: &str, col: usize) -> usize {
    let mut current_col = 0;
    let mut byte_offset = 0;
    for grapheme in line.graphemes(true) {
        if current_col >= col {
            break;
        }
        let width = unicode_width::UnicodeWidthStr::width(grapheme);
        if current_col + width > col {
            break;
        }
        current_col += width;
        byte_offset += grapheme.len();
    }
    byte_offset
}

fn newline_col_offset(text: &str, line_index: usize, col: u16) -> usize {
    let start_offset = newline_offset(text, line_index);
    let line = text.split('\n').nth(line_index).unwrap_or("");
    let cell_offset = visual_col_to_offset(line, usize::from(col));
    start_offset + cell_offset
}

fn wrapped_line_col_offset(text: &str, line_index: usize, col: u16, wrap_width: usize) -> usize {
    let wrapped = wrap_text_with_ansi(text, wrap_width);
    let start_offset: usize = wrapped.iter().take(line_index).map(|line| line.len()).sum();
    let line = wrapped.get(line_index).map(String::as_str).unwrap_or("");
    let cell_offset = visual_col_to_offset(line, usize::from(col));
    start_offset + cell_offset
}

fn visual_cell_to_copy_offset(
    block: &TranscriptBlock,
    copy_text: &str,
    local_row: usize,
    col: u16,
    width: u16,
) -> usize {
    match block {
        TranscriptBlock::Assistant(assistant) => {
            if looks_like_diff(&assistant.text) {
                return newline_col_offset(copy_text, local_row, col);
            }
            wrapped_line_col_offset(copy_text, local_row, col, usize::from(width).max(1))
        }
        TranscriptBlock::Reasoning(_) => {
            wrapped_line_col_offset(copy_text, local_row, col, usize::from(width).max(1))
        }
        TranscriptBlock::User { .. } => {
            let inner_width = (width.saturating_sub(2) as usize).max(1);
            let col_in_text = col.saturating_sub(2);
            wrapped_line_col_offset(copy_text, local_row, col_in_text, inner_width)
        }
        TranscriptBlock::Notice(_)
        | TranscriptBlock::NoticeStatus { .. }
        | TranscriptBlock::Compaction(_) => {
            wrapped_line_col_offset(copy_text, local_row, col, usize::from(width).max(1))
        }
        TranscriptBlock::Outcome(_) => visual_col_to_offset(copy_text, usize::from(col)),
        TranscriptBlock::Tool(_) => {
            let indent = if width < 60 { 7 } else { 8 };
            let col_in_text = col.saturating_sub(indent);
            newline_col_offset(copy_text, local_row, col_in_text)
        }
        TranscriptBlock::Shell(_) => {
            wrapped_line_col_offset(copy_text, local_row, col, usize::from(width).max(1))
        }
    }
}

pub(super) fn selection_position_for_visual_cell(
    state: &ShellState,
    visual_line: usize,
    col: u16,
) -> Option<TranscriptPosition> {
    let cache = state.transcript_cache.borrow();
    let block = cache
        .block_starts
        .partition_point(|start| *start <= visual_line)
        .checked_sub(1)?;
    let local_row = visual_line.checked_sub(cache.block_starts[block])?;
    let total_rows = *cache.block_lengths.get(block)?;
    let geometry = *cache.block_geometries.get(block)?;
    if local_row >= total_rows {
        return None;
    }
    let content_row = geometry.content_row(local_row, total_rows)?;
    let content_col = geometry.content_col(col);
    drop(cache);

    let transcript_block = state.transcript.get(block)?;
    let text = block_copy_text(transcript_block);
    let offset = visual_cell_to_copy_offset(
        transcript_block,
        &text,
        content_row,
        content_col,
        geometry.content_width,
    );
    Some(TranscriptPosition {
        block,
        offset: clamp_copy_offset(&text, offset),
        trailing_affinity: false,
    })
}

/// Byte-offset after `line_index` newline-delimited segments (current
/// behaviour for blocks where wrapping correspondence is unavailable).
fn newline_offset(text: &str, line_index: usize) -> usize {
    text.split_inclusive('\n')
        .take(line_index)
        .map(str::len)
        .sum::<usize>()
        .min(text.len())
}
