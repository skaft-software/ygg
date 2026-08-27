//! Native-scrollback commit boundaries for the rendered transcript.

use std::time::Instant;

use sexy_tui_rs::{CommitCursor, CommitPosition, PinnedFrame};

use super::bash_render::bash_output_changes_when_expanded;
use super::tool_render::tool_diff;
use super::welcome_card::welcome_animating;
use super::{ShellState, ToolPanel, TranscriptBlock, COMPACT_EXEC_OUTPUT_ROWS};

pub(super) const FINAL_COMMIT_SEGMENT: u64 = u64::MAX;

fn transcript_block_is_final(block: &TranscriptBlock) -> bool {
    match block {
        TranscriptBlock::Assistant(block) | TranscriptBlock::Reasoning(block) => block.finished,
        TranscriptBlock::Tool(panel) => panel.finished,
        TranscriptBlock::Shell(shell) => !shell.running,
        TranscriptBlock::Compaction(_)
        | TranscriptBlock::User { .. }
        | TranscriptBlock::Outcome(_)
        | TranscriptBlock::Notice(_)
        | TranscriptBlock::NoticeStatus { .. } => true,
    }
}

pub(super) fn transcript_commit_cursor(
    state: &ShellState,
    block: usize,
    segment: u64,
) -> CommitCursor {
    CommitCursor {
        generation: state.transcript_epoch,
        block: *state
            .transcript_commit_ids
            .get(block)
            .expect("transcript block missing commit identity"),
        segment,
    }
}

pub(super) fn transcript_commit_position(
    state: &ShellState,
    cursor: CommitCursor,
) -> Option<CommitPosition> {
    if cursor.generation != state.transcript_epoch {
        return None;
    }
    let cache = state.transcript_cache.borrow();
    let block_index = match state.transcript_commit_ids.binary_search(&cursor.block) {
        Ok(index) => index,
        Err(insertion) => {
            // A cancelled provider attempt can remove a streaming tail after
            // some of its finalized segments entered native history. Preserve
            // that tombstoned seam at the removal point; later blocks receive
            // larger IDs and can continue the append-only tape.
            let row = cache
                .block_starts
                .get(insertion)
                .copied()
                .unwrap_or(cache.lines.len());
            return Some(CommitPosition { cursor, row });
        }
    };
    let block = state.transcript.get(block_index)?;
    let block_start = *cache.block_starts.get(block_index)?;
    let block_len = *cache.block_lengths.get(block_index)?;

    let row = if cursor.segment == FINAL_COMMIT_SEGMENT {
        transcript_block_is_final(block).then_some(block_start.saturating_add(block_len))?
    } else {
        let TranscriptBlock::Assistant(markdown) = block else {
            return None;
        };
        let geometry = *cache.block_geometries.get(block_index)?;
        let segment = usize::try_from(cursor.segment).ok()?;
        let layout = markdown.layout.borrow();
        let content_end = *layout.committed_block_ends().get(segment)?;
        block_start
            .saturating_add(geometry.transition_rows)
            .saturating_add(geometry.leading_rows)
            .saturating_add(content_end)
            .min(block_start.saturating_add(block_len))
    };
    Some(CommitPosition { cursor, row })
}

fn finalized_tool_rows_are_stable(panel: &ToolPanel) -> bool {
    if let Some(disclosure_sensitive) = *panel.cached_disclosure_sensitive.borrow() {
        return !disclosure_sensitive;
    }

    let disclosure_sensitive = if let Some(activity) = panel.subagent_activity.as_ref() {
        let retained = if activity.telemetry.is_empty() {
            activity.activities.len()
        } else {
            activity.telemetry.len()
        };
        retained > 2
    } else {
        match panel.name.as_str() {
            "bash" | "exec" => {
                panel.display.shell_command.is_some() && bash_output_changes_when_expanded(panel)
            }
            "search" if !panel.is_error => panel
                .output
                .lines()
                .filter(|line| !line.trim().is_empty() && *line != "(no output)")
                .nth(COMPACT_EXEC_OUTPUT_ROWS)
                .is_some(),
            // Rendering determines diff truncation after width-dependent wrap.
            // A recognized diff is therefore kept atomic conservatively.
            "edit" | "write" if !panel.is_error => tool_diff(panel).is_some(),
            _ => false,
        }
    };
    *panel.cached_disclosure_sensitive.borrow_mut() = Some(disclosure_sensitive);
    !disclosure_sensitive
}

fn finalized_block_rows_are_stable(block: &TranscriptBlock) -> bool {
    match block {
        TranscriptBlock::Assistant(_) | TranscriptBlock::User { .. } => true,
        TranscriptBlock::Tool(panel) => finalized_tool_rows_are_stable(panel),
        TranscriptBlock::Shell(shell) => shell.output.trim().is_empty(),
        TranscriptBlock::Outcome(_)
        | TranscriptBlock::Notice(_)
        | TranscriptBlock::NoticeStatus { .. } => true,
        // These presentations can shrink when Ctrl+O changes disclosure. They
        // may still cross history atomically through a semantic target, but no
        // partial physical prefix is safe to pin.
        TranscriptBlock::Reasoning(_) | TranscriptBlock::Compaction(_) => false,
    }
}

fn transcript_stable_rows(state: &ShellState, acknowledged: Option<CommitCursor>) -> usize {
    if welcome_animating(state, Instant::now()) {
        return 0;
    }

    // Semantic acknowledgement proves the earlier prefix is already terminal
    // owned. Resume classification at that block instead of rescanning a long
    // settled transcript on every streaming tick.
    let (mut stable_rows, start_block) = acknowledged
        .filter(|cursor| cursor.generation == state.transcript_epoch)
        .and_then(|cursor| {
            let position = transcript_commit_position(state, cursor)?;
            let start = match state.transcript_commit_ids.binary_search(&cursor.block) {
                Ok(index) if cursor.segment == FINAL_COMMIT_SEGMENT => index.saturating_add(1),
                Ok(index) | Err(index) => index,
            };
            Some((position.row, start))
        })
        .unwrap_or((0, 0));

    let cache = state.transcript_cache.borrow();
    for (index, block) in state.transcript.iter().enumerate().skip(start_block) {
        let Some(block_start) = cache.block_starts.get(index).copied() else {
            break;
        };
        let Some(block_len) = cache.block_lengths.get(index).copied() else {
            break;
        };
        let block_end = block_start.saturating_add(block_len);
        if transcript_block_is_final(block) {
            if finalized_block_rows_are_stable(block) {
                stable_rows = block_end;
                continue;
            }
            break;
        }

        if let TranscriptBlock::Assistant(markdown) = block {
            let Some(geometry) = cache.block_geometries.get(index).copied() else {
                break;
            };
            let layout = markdown.layout.borrow();
            let committed_rows = layout.committed_rows();
            if committed_rows > 0 {
                let content_start = block_start
                    .saturating_add(geometry.transition_rows)
                    .saturating_add(geometry.leading_rows);
                stable_rows = content_start.saturating_add(committed_rows).min(block_end);

                // The merged streaming layout inserts one blank separator
                // between its committed document and mutable tail. Once both
                // sides exist that row is structural and cannot be changed by
                // later deltas, even though it is not a semantic block end.
                let content_end = block_end.saturating_sub(geometry.trailing_rows);
                if stable_rows < content_end {
                    stable_rows = stable_rows.saturating_add(1).min(content_end);
                }
            }
        }
        break;
    }
    stable_rows
}

/// Furthest semantic boundary whose current-layout rows fit entirely above the
/// mutable viewport. Stable block IDs survive deferred prepends and tail
/// removal while width-dependent row positions are recomputed after reflow.
fn transcript_commit_target(
    state: &ShellState,
    maximum_row: usize,
    acknowledged: Option<CommitCursor>,
) -> Option<CommitPosition> {
    if welcome_animating(state, Instant::now()) {
        return None;
    }
    let cache = state.transcript_cache.borrow();
    let mut target = None;
    let start_block = acknowledged
        .filter(|cursor| cursor.generation == state.transcript_epoch)
        .map_or(0, |cursor| {
            state
                .transcript_commit_ids
                .binary_search(&cursor.block)
                .unwrap_or_else(|insertion| insertion)
        });

    for (index, block) in state.transcript.iter().enumerate().skip(start_block) {
        let Some(block_start) = cache.block_starts.get(index).copied() else {
            break;
        };
        let Some(block_len) = cache.block_lengths.get(index).copied() else {
            break;
        };
        let Some(geometry) = cache.block_geometries.get(index).copied() else {
            break;
        };
        let block_end = block_start.saturating_add(block_len);
        let final_block = transcript_block_is_final(block);

        if let TranscriptBlock::Assistant(markdown) = block {
            // A completed block can be acknowledged as one outer transcript
            // unit when it fits. If it straddles the viewport, retain the most
            // recent parser-committed inner Markdown boundary instead.
            if final_block && block_end <= maximum_row {
                target = Some(CommitPosition {
                    cursor: transcript_commit_cursor(state, index, FINAL_COMMIT_SEGMENT),
                    row: block_end,
                });
                continue;
            }

            let content_start = block_start
                .saturating_add(geometry.transition_rows)
                .saturating_add(geometry.leading_rows);
            let layout = markdown.layout.borrow();
            for (segment, content_end) in layout.committed_block_ends().iter().enumerate() {
                let row = content_start.saturating_add(*content_end).min(block_end);
                if row > maximum_row {
                    break;
                }
                target = Some(CommitPosition {
                    cursor: transcript_commit_cursor(state, index, segment as u64),
                    row,
                });
            }
            break;
        }

        if !final_block || block_end > maximum_row {
            break;
        }
        target = Some(CommitPosition {
            cursor: transcript_commit_cursor(state, index, FINAL_COMMIT_SEGMENT),
            row: block_end,
        });
    }

    target
}

pub(super) fn transcript_pinned_frame(
    state: &ShellState,
    total_rows: usize,
    acknowledged: Option<CommitCursor>,
    viewport_surface: bool,
) -> PinnedFrame {
    let maximum_row = total_rows.saturating_sub(usize::from(state.size.1.max(1)));
    let target = transcript_commit_target(state, maximum_row, acknowledged);
    let stable_rows = transcript_stable_rows(state, acknowledged).min(maximum_row);
    PinnedFrame {
        generation: state.transcript_epoch,
        acknowledged: acknowledged.and_then(|cursor| transcript_commit_position(state, cursor)),
        target,
        stable_rows,
        viewport_surface,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::summarize_tool;
    use ygg_ai::ToolCallId;

    use super::super::{AssistantBlock, CompactionBlock, ShellOutput, ToolPanel};

    fn finalized_tool(
        name: &str,
        args: serde_json::Value,
        output: impl Into<String>,
        is_error: bool,
    ) -> TranscriptBlock {
        TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId(format!("{name}-classification")),
            name.to_owned(),
            args.to_string(),
            summarize_tool(name, &args),
            output.into(),
            true,
            is_error,
            is_error.then(|| "failed".to_owned()),
            None,
        )))
    }

    #[test]
    fn finalized_disclosure_sensitive_rows_cross_history_only_atomically() {
        let five_lines = (0..COMPACT_EXEC_OUTPUT_ROWS)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let six_lines = format!("{five_lines}\nline 5");

        let short_bash = finalized_tool(
            "bash",
            serde_json::json!({"command": "printf short"}),
            &five_lines,
            false,
        );
        let long_bash = finalized_tool(
            "bash",
            serde_json::json!({"command": "printf long"}),
            &six_lines,
            false,
        );
        let failed_bash = finalized_tool(
            "bash",
            serde_json::json!({"command": "exit 1"}),
            &six_lines,
            true,
        );
        assert!(!finalized_block_rows_are_stable(&short_bash));
        assert!(!finalized_block_rows_are_stable(&long_bash));
        assert!(!finalized_block_rows_are_stable(&failed_bash));

        let short_search = finalized_tool(
            "search",
            serde_json::json!({"query": "needle", "path": "."}),
            &five_lines,
            false,
        );
        let long_search = finalized_tool(
            "search",
            serde_json::json!({"query": "needle", "path": "."}),
            &six_lines,
            false,
        );
        assert!(finalized_block_rows_are_stable(&short_search));
        assert!(!finalized_block_rows_are_stable(&long_search));

        let diff = finalized_tool(
            "edit",
            serde_json::json!({"path": "src/lib.rs"}),
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new",
            false,
        );
        assert!(!finalized_block_rows_are_stable(&diff));

        let short_shell = TranscriptBlock::Shell(Box::new(ShellOutput {
            id: "short".to_owned(),
            command: "printf short".to_owned(),
            output: five_lines,
            exit_code: 0,
            running: false,
        }));
        let long_shell = TranscriptBlock::Shell(Box::new(ShellOutput {
            id: "long".to_owned(),
            command: "printf long".to_owned(),
            output: six_lines,
            exit_code: 0,
            running: false,
        }));
        let empty_shell = TranscriptBlock::Shell(Box::new(ShellOutput {
            id: "empty".to_owned(),
            command: "true".to_owned(),
            output: String::new(),
            exit_code: 0,
            running: false,
        }));
        assert!(!finalized_block_rows_are_stable(&short_shell));
        assert!(!finalized_block_rows_are_stable(&long_shell));
        assert!(finalized_block_rows_are_stable(&empty_shell));

        let reasoning = TranscriptBlock::Reasoning(Box::new(AssistantBlock::finalized_reasoning(
            "private chain".to_owned(),
        )));
        let compaction = TranscriptBlock::Compaction(Box::new(CompactionBlock {
            label: "Context compacted".to_owned(),
            summary: "summary".to_owned(),
            expanded: false,
        }));
        assert!(!finalized_block_rows_are_stable(&reasoning));
        assert!(!finalized_block_rows_are_stable(&compaction));
    }
}
