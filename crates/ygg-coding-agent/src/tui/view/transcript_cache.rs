use std::cell::Ref;
use std::time::Instant;

use super::transcript_render::render_block_planned;
use super::{render_welcome_card, ShellState};

/// Final block-local geometry shared by transcript rendering and semantic
/// selection. Decorative rows and columns never enter copy offsets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SurfaceGeometry {
    pub(super) transition_rows: usize,
    pub(super) leading_rows: usize,
    pub(super) trailing_rows: usize,
    pub(super) content_left: u16,
    pub(super) content_width: u16,
}

impl SurfaceGeometry {
    pub(super) fn content_row(self, local_row: usize, total_rows: usize) -> Option<usize> {
        let start = self.transition_rows.checked_add(self.leading_rows)?;
        let end = total_rows.checked_sub(self.trailing_rows)?;
        (local_row >= start && local_row < end).then(|| local_row - start)
    }

    pub(super) fn content_col(self, column: u16) -> u16 {
        column
            .saturating_sub(self.content_left)
            .min(self.content_width)
    }
}

#[derive(Clone, Debug)]
pub(super) struct RenderedTranscriptBlock {
    pub(super) lines: Vec<String>,
    pub(super) geometry: SurfaceGeometry,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptCache {
    pub(super) width: Option<u16>,
    pub(super) lines: Vec<String>,
    pub(super) block_starts: Vec<usize>,
    pub(super) block_lengths: Vec<usize>,
    pub(super) block_geometries: Vec<SurfaceGeometry>,
    pub(super) block_revisions: Vec<u64>,
    /// Blocks changed since the last layout pass. Keeping this explicit avoids
    /// scanning every historic block for each streamed token.
    pub(super) dirty_blocks: Vec<usize>,
    pub(super) dirty: bool,
    pub(super) generation: u64,
    /// First visual row changed by the most recent layout update.
    pub(super) last_update_start: usize,
}

impl Default for TranscriptCache {
    fn default() -> Self {
        Self {
            width: None,
            lines: Vec::new(),
            block_starts: Vec::new(),
            block_lengths: Vec::new(),
            block_geometries: Vec::new(),
            block_revisions: Vec::new(),
            dirty_blocks: Vec::new(),
            dirty: true,
            generation: 0,
            last_update_start: 0,
        }
    }
}

impl ShellState {
    pub(super) fn rendered_transcript(&self, width: u16) -> Ref<'_, Vec<String>> {
        let stale = {
            let cache = self.transcript_cache.borrow();
            cache.dirty || cache.width != Some(width)
        };
        if stale {
            let mut rich_renderer_slot = self.rich_renderer.borrow_mut();
            if rich_renderer_slot.is_none() {
                *rich_renderer_slot = Some(self.theme.rich_renderer());
            }
            let rich_renderer = rich_renderer_slot
                .as_ref()
                .expect("rich renderer initialized above");
            let mut reasoning_renderer_slot = self.reasoning_renderer.borrow_mut();
            if reasoning_renderer_slot.is_none() {
                *reasoning_renderer_slot = Some(self.theme.reasoning_renderer());
            }
            let reasoning_renderer = reasoning_renderer_slot
                .as_ref()
                .expect("reasoning renderer initialized above");
            let mut cache = self.transcript_cache.borrow_mut();
            let previous_line_count = cache.lines.len();
            let mut first_changed = cache.lines.len();
            let rebuild =
                cache.width != Some(width) || cache.block_revisions.len() > self.transcript.len();

            if rebuild {
                first_changed = 0;
                cache.lines.clear();
                cache.block_starts.clear();
                cache.block_lengths.clear();
                cache.block_geometries.clear();
                cache.block_revisions.clear();
                cache.dirty_blocks.clear();
                cache.width = Some(width);
                cache
                    .lines
                    .extend(render_welcome_card(self, width, 10, Instant::now()));

                for (index, block) in self.transcript.iter().enumerate() {
                    let rendered = render_block_planned(
                        index
                            .checked_sub(1)
                            .and_then(|previous| self.transcript.get(previous)),
                        block,
                        &self.theme,
                        rich_renderer,
                        reasoning_renderer,
                        width,
                        self.show_tool_details(block),
                        self.event_dot_visible,
                    );
                    let start = cache.lines.len();
                    let length = rendered.lines.len();
                    cache.lines.extend(rendered.lines);
                    cache.block_starts.push(start);
                    cache.block_lengths.push(length);
                    cache.block_geometries.push(rendered.geometry);
                    cache.block_revisions.push(self.block_revisions[index]);
                }
            } else {
                // New blocks are appended in normal operation. Render them
                // once and leave every existing block's layout untouched.
                while cache.block_revisions.len() < self.transcript.len() {
                    let index = cache.block_revisions.len();
                    let rendered = render_block_planned(
                        index
                            .checked_sub(1)
                            .and_then(|previous| self.transcript.get(previous)),
                        &self.transcript[index],
                        &self.theme,
                        rich_renderer,
                        reasoning_renderer,
                        width,
                        self.show_tool_details(&self.transcript[index]),
                        self.event_dot_visible,
                    );
                    let start = cache.lines.len();
                    first_changed = first_changed.min(start);
                    let length = rendered.lines.len();
                    cache.lines.extend(rendered.lines);
                    cache.block_starts.push(start);
                    cache.block_lengths.push(length);
                    cache.block_geometries.push(rendered.geometry);
                    cache.block_revisions.push(self.block_revisions[index]);
                }

                // `touch_block` records mutations as they happen. In
                // particular, a token delta normally changes only the active
                // tail block; iterating `0..transcript.len()` here used to make
                // every streaming frame progressively slower as history grew.
                let mut dirty_blocks = std::mem::take(&mut cache.dirty_blocks);
                dirty_blocks.sort_unstable();
                dirty_blocks.dedup();
                for index in dirty_blocks {
                    // A newly appended block is rendered above with its latest
                    // revision. A stale queued index can therefore be skipped.
                    if index >= cache.block_revisions.len()
                        || cache.block_revisions[index] == self.block_revisions[index]
                    {
                        continue;
                    }
                    let start = cache.block_starts[index];
                    first_changed = first_changed.min(start);
                    let old_length = cache.block_lengths[index];
                    let rendered = render_block_planned(
                        index
                            .checked_sub(1)
                            .and_then(|previous| self.transcript.get(previous)),
                        &self.transcript[index],
                        &self.theme,
                        rich_renderer,
                        reasoning_renderer,
                        width,
                        self.show_tool_details(&self.transcript[index]),
                        self.event_dot_visible,
                    );
                    let new_length = rendered.lines.len();
                    cache
                        .lines
                        .splice(start..start + old_length, rendered.lines);
                    cache.block_lengths[index] = new_length;
                    cache.block_geometries[index] = rendered.geometry;
                    cache.block_revisions[index] = self.block_revisions[index];

                    let delta = new_length as isize - old_length as isize;
                    if delta != 0 {
                        for following in cache.block_starts.iter_mut().skip(index + 1) {
                            if delta > 0 {
                                *following += delta as usize;
                            } else {
                                *following = following.saturating_sub((-delta) as usize);
                            }
                        }
                    }
                }
            }

            cache.last_update_start = first_changed.min(cache.lines.len());
            cache.dirty = false;
            cache.generation = cache.generation.saturating_add(1);
            let history_prepended = self.history_prepended.replace(false);
            if !self.follow_tail && !history_prepended {
                let current = self.scroll_from_bottom.get();
                if cache.lines.len() >= previous_line_count {
                    self.scroll_from_bottom
                        .set(current.saturating_add(cache.lines.len() - previous_line_count));
                } else {
                    self.scroll_from_bottom
                        .set(current.saturating_sub(previous_line_count - cache.lines.len()));
                }
            }
        }
        Ref::map(self.transcript_cache.borrow(), |cache| &cache.lines)
    }
}
