//! Native-scrollback commit boundaries for the rendered transcript.

use std::time::Instant;

use sexy_tui_rs::{CommitCursor, CommitPosition, PinnedFrame};

use super::renderer_runtime::welcome_animating;
use super::{ShellState, TranscriptBlock};

pub(super) const FINAL_COMMIT_SEGMENT: u64 = u64::MAX;

fn transcript_block_is_final(block: &TranscriptBlock) -> bool {
    match block {
        TranscriptBlock::Assistant(block) | TranscriptBlock::Reasoning(block) => block.finished,
        TranscriptBlock::Tool(panel) => panel.finished,
        TranscriptBlock::Shell(shell) => !shell.running,
        TranscriptBlock::Compaction(_)
        | TranscriptBlock::User { .. }
        | TranscriptBlock::Outcome(_)
        | TranscriptBlock::Notice(_) => true,
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
) -> PinnedFrame {
    let maximum_row = total_rows.saturating_sub(usize::from(state.size.1.max(1)));
    let target = transcript_commit_target(state, maximum_row, acknowledged);
    PinnedFrame {
        generation: state.transcript_epoch,
        acknowledged: acknowledged.and_then(|cursor| transcript_commit_position(state, cursor)),
        target,
    }
}
