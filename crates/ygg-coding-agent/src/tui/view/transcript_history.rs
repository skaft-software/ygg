//! Deferred transcript snapshot and stable commit-identity management.

use std::path::PathBuf;

use anyhow::Result;
use ygg_agent::{EntryId, Session};

use super::renderer_runtime::SharedState;
use super::transcript_hydration::append_hydrated_items;
use super::TranscriptBlock;
use crate::hydrate::hydrate_transcript_at;

#[derive(Clone, Copy)]
pub(super) struct NextTranscriptCommitId(pub(super) u64);

impl Default for NextTranscriptCommitId {
    fn default() -> Self {
        // Leave ample identity space below the live tail so deferred session
        // history can receive earlier IDs without renumbering retained blocks.
        Self(1_u64 << 63)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DeferredSessionHistory {
    pub(super) path: PathBuf,
    /// Immutable session head whose bounded tail was used for first paint.
    pub(super) head: EntryId,
    /// IDs below this exclusive boundary belong to that original tail. Blocks
    /// appended by the live shell stay above it and survive materialization.
    pub(super) retained_id_end: u64,
}

pub(super) fn materialize_deferred_session_history(state: &SharedState) -> Result<bool> {
    let deferred = state.borrow().deferred_session_history.clone();
    let Some(deferred) = deferred else {
        return Ok(false);
    };

    // Replay the immutable branch snapshot used for first paint, not the
    // session's moving head. Blocks added by the live shell are retained
    // separately below, so this remains safe during an active run.
    let session = Session::open_read_only(&deferred.path)?;
    let items = hydrate_transcript_at(&session, &deferred.head)?;
    let mut state = state.borrow_mut();
    if state.deferred_session_history.as_ref() != Some(&deferred) {
        return Ok(false);
    }

    debug_assert_eq!(state.transcript.len(), state.transcript_commit_ids.len());
    debug_assert_eq!(state.transcript.len(), state.block_revisions.len());
    let retained_tail_len = state
        .transcript_commit_ids
        .partition_point(|commit_id| *commit_id < deferred.retained_id_end);

    let mut retained_tail_blocks = std::mem::take(&mut state.transcript);
    let local_blocks = retained_tail_blocks.split_off(retained_tail_len);
    let mut retained_tail_ids = std::mem::take(&mut state.transcript_commit_ids);
    let local_commit_ids = retained_tail_ids.split_off(retained_tail_len);
    let mut retained_tail_revisions = std::mem::take(&mut state.block_revisions);
    let local_revisions = retained_tail_revisions.split_off(retained_tail_len);
    let original_tool_panels = std::mem::take(&mut state.tool_panels);
    let original_new_output_count = state.new_output_count;
    let next_commit_id = state.next_transcript_commit_id;

    append_hydrated_items(&mut state, items);
    state.new_output_count = original_new_output_count;
    let identity_plan = (|| {
        let original_snapshot_len = state.transcript.len();
        anyhow::ensure!(
            original_snapshot_len >= retained_tail_ids.len(),
            "full hydration did not retain the deferred transcript tail"
        );
        let prepended_blocks = original_snapshot_len - retained_tail_ids.len();
        let prepended_blocks_u64 = u64::try_from(prepended_blocks)
            .map_err(|_| anyhow::anyhow!("deferred transcript prefix is too large"))?;
        let retained_anchor = retained_tail_ids
            .first()
            .copied()
            .unwrap_or(deferred.retained_id_end);
        let prefix_start = retained_anchor
            .checked_sub(prepended_blocks_u64)
            .ok_or_else(|| anyhow::anyhow!("deferred history exhausted commit identity space"))?;
        Ok::<_, anyhow::Error>((original_snapshot_len, prepended_blocks, prefix_start))
    })();
    let (original_snapshot_len, prepended_blocks, prefix_start) = match identity_plan {
        Ok(plan) => plan,
        Err(error) => {
            // Hydration and identity planning are transactional. A malformed or
            // incompatible snapshot must leave every live transcript vector and
            // index exactly as it was so a later retry remains safe.
            retained_tail_blocks.extend(local_blocks);
            retained_tail_ids.extend(local_commit_ids);
            retained_tail_revisions.extend(local_revisions);
            state.transcript = retained_tail_blocks;
            state.transcript_commit_ids = retained_tail_ids;
            state.block_revisions = retained_tail_revisions;
            state.tool_panels = original_tool_panels;
            state.next_transcript_commit_id = next_commit_id;
            state.new_output_count = original_new_output_count;
            state.invalidate_transcript_layout();
            return Err(error);
        }
    };

    for (offset, commit_id) in state
        .transcript_commit_ids
        .iter_mut()
        .take(prepended_blocks)
        .enumerate()
    {
        *commit_id = prefix_start + offset as u64;
    }
    state.transcript_commit_ids[prepended_blocks..original_snapshot_len]
        .copy_from_slice(&retained_tail_ids);
    state.next_transcript_commit_id = next_commit_id;

    let local_start = state.transcript.len();
    state.transcript.extend(local_blocks);
    state.transcript_commit_ids.extend(local_commit_ids);
    state.block_revisions.extend(local_revisions);
    let local_tools = state.transcript[local_start..]
        .iter()
        .enumerate()
        .filter_map(|(offset, block)| match block {
            TranscriptBlock::Tool(panel) => Some((panel.id.clone(), local_start + offset)),
            _ => None,
        })
        .collect::<Vec<_>>();
    state.tool_panels.extend(local_tools);

    // Every previously loaded block moves down by the same prepended prefix.
    // Preserve all semantic block-index references, including an in-flight
    // stream and a selection active during resize.
    state.active_text = state
        .active_text
        .map(|index| index.saturating_add(prepended_blocks));
    state.active_reasoning = state
        .active_reasoning
        .map(|index| index.saturating_add(prepended_blocks));
    if let Some(selection) = &mut state.transcript_selection {
        selection.anchor.block = selection.anchor.block.saturating_add(prepended_blocks);
        selection.focus.block = selection.focus.block.saturating_add(prepended_blocks);
    }
    if let Some(position) = &mut state.pending_selection_anchor {
        position.block = position.block.saturating_add(prepended_blocks);
    }

    debug_assert!(
        state
            .transcript_commit_ids
            .windows(2)
            .all(|ids| ids[0] < ids[1]),
        "deferred history must preserve ordered commit identities"
    );
    state.deferred_session_history = None;
    state.history_prepended.set(true);
    state.invalidate_transcript_layout();
    Ok(true)
}
