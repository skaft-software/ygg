use std::cell::Ref;
use std::time::Instant;

use sexy_tui_rs::{wrap_text_with_ansi, FrameUpdate};

use super::renderer_runtime::ShellFrameState;
use super::shell_chrome::{append_viewport_chrome, shell_chrome};
use super::transcript_selection::{
    selection_position_for_visual_cell, visual_line_for_transcript_position, TranscriptPosition,
};
use super::{
    fit_line, semantic_separator, ShellOverlay, ShellState, TranscriptBlock, ViewportAnchor,
};

pub(super) fn transcript_lines(state: &ShellState, width: u16) -> Ref<'_, Vec<String>> {
    state.rendered_transcript(width)
}

pub(super) fn transcript_viewport_capacity(available: usize, scrolled: bool) -> usize {
    if available == 0 {
        return 0;
    }
    // Keep the transcript visually separate from the pinned surfaces whenever
    // there is room. A scrolled viewport also owns one row for its navigation
    // indicator, leaving every other row for semantic transcript content.
    let breathing_row = 1;
    // On a two-row transcript surface the navigation indicator temporarily
    // occupies the breathing row so one semantic row remains inspectable.
    let indicator_row = usize::from(scrolled && available > 2);
    available.saturating_sub(breathing_row + indicator_row)
}

pub(super) fn max_scroll_for_available(transcript_len: usize, available: usize) -> usize {
    let live_capacity = transcript_viewport_capacity(available, false);
    if live_capacity == 0 || transcript_len <= live_capacity {
        0
    } else {
        let scrolled_capacity = transcript_viewport_capacity(available, true).max(1);
        transcript_len.saturating_sub(scrolled_capacity)
    }
}

pub(super) fn max_scroll_from_bottom(state: &ShellState, width: u16) -> usize {
    if state.overlay.is_some() {
        return 0;
    }
    let chrome = shell_chrome(state, width, Instant::now());
    max_scroll_for_available(transcript_lines(state, width).len(), chrome.transcript_rows)
}

pub(super) fn transcript_viewport_capacity_for_state(state: &ShellState, width: u16) -> usize {
    if state.overlay.is_some() {
        return 0;
    }
    let chrome = shell_chrome(state, width, Instant::now());
    let transcript = transcript_lines(state, width);
    let scroll = resolved_scroll_from_bottom(state, transcript.len(), chrome.transcript_rows);
    transcript_viewport_capacity(chrome.transcript_rows, scroll > 0)
}

/// Wrap each logical overlay row independently and terminate its SGR state.
/// Picker rows use the legacy closure-styled compatibility API, so each row is
/// explicitly closed even though sexy-tui 0.2 now preserves extended colors
/// safely across wraps.
pub(super) fn wrap_overlay_text(text: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for source_line in text.split('\n') {
        if source_line.contains('\x1b') {
            let terminated = format!("{source_line}\x1b[0m");
            for line in wrap_text_with_ansi(&terminated, width.max(1)) {
                wrapped.push(format!("{line}\x1b[0m"));
            }
        } else {
            wrapped.extend(wrap_text_with_ansi(source_line, width.max(1)));
        }
    }
    wrapped
}

pub(super) fn overlay_lines(state: &ShellState, width: u16) -> Vec<String> {
    let Some(overlay) = &state.overlay else {
        return Vec::new();
    };
    match overlay {
        ShellOverlay::Text(text) => wrap_overlay_text(text, usize::from(width).max(1)),
        ShellOverlay::Context(report) => report.render(&state.theme, width),
    }
}

fn viewport_anchor_for_visual_row(
    state: &ShellState,
    visual_row: usize,
    desired_screen_row: usize,
) -> Option<ViewportAnchor> {
    let (block, block_start, block_length) = {
        let cache = state.transcript_cache.borrow();
        let block = cache
            .block_starts
            .partition_point(|start| *start <= visual_row)
            .checked_sub(1)?;
        (
            block,
            *cache.block_starts.get(block)?,
            *cache.block_lengths.get(block)?,
        )
    };
    let fallback_block_row = visual_row.checked_sub(block_start)?;
    if fallback_block_row >= block_length {
        return None;
    }
    let commit_id = *state.transcript_commit_ids.get(block)?;
    let position = selection_position_for_visual_cell(state, visual_row, 0);
    // Tool and local-shell copy intentionally excludes disclosed output. A
    // visual row is the only honest fallback for those non-copy surfaces.
    let semantic = position.is_some()
        && !matches!(
            state.transcript.get(block),
            Some(TranscriptBlock::Tool(_) | TranscriptBlock::Shell(_))
        );
    let position = position.unwrap_or(TranscriptPosition {
        block,
        offset: 0,
        trailing_affinity: false,
    });
    Some(ViewportAnchor {
        commit_id,
        block_hint: block,
        text_offset: position.offset,
        trailing_affinity: position.trailing_affinity,
        fallback_block_row,
        fallback_visual_row: visual_row,
        desired_screen_row,
        semantic,
    })
}

fn capture_viewport_anchor(state: &ShellState, start: usize, end: usize) {
    if state.viewport_anchor.get().is_some() || start >= end {
        return;
    }
    let mut fallback = None;
    for (desired_screen_row, visual_row) in (start..end).enumerate() {
        let Some(anchor) = viewport_anchor_for_visual_row(state, visual_row, desired_screen_row)
        else {
            continue;
        };
        if anchor.semantic {
            state.viewport_anchor.set(Some(anchor));
            return;
        }
        fallback.get_or_insert(anchor);
    }
    state.viewport_anchor.set(fallback);
}

fn resolve_viewport_anchor(state: &ShellState, mut anchor: ViewportAnchor) -> usize {
    let block = if state.transcript_commit_ids.get(anchor.block_hint) == Some(&anchor.commit_id) {
        Some(anchor.block_hint)
    } else {
        state
            .transcript_commit_ids
            .iter()
            .position(|commit_id| *commit_id == anchor.commit_id)
    };
    let Some(block) = block else {
        return anchor.fallback_visual_row;
    };
    anchor.block_hint = block;
    state.viewport_anchor.set(Some(anchor));

    let (start, length) = {
        let cache = state.transcript_cache.borrow();
        let Some(start) = cache.block_starts.get(block).copied() else {
            return anchor.fallback_visual_row;
        };
        let Some(length) = cache.block_lengths.get(block).copied() else {
            return anchor.fallback_visual_row;
        };
        (start, length)
    };
    if length == 0 {
        return start;
    }
    if anchor.semantic {
        let position = TranscriptPosition {
            block,
            offset: anchor.text_offset,
            trailing_affinity: anchor.trailing_affinity,
        };
        if let Some(row) = visual_line_for_transcript_position(state, position) {
            return row.clamp(start, start.saturating_add(length - 1));
        }
    }
    start.saturating_add(anchor.fallback_block_row.min(length - 1))
}

fn anchored_scroll_from_bottom(
    state: &ShellState,
    transcript_len: usize,
    available: usize,
    maximum: usize,
) -> Option<usize> {
    let anchor = state.viewport_anchor.get()?;
    let capacity = transcript_viewport_capacity(available, true).max(1);
    let desired = anchor.desired_screen_row.min(capacity - 1);
    let anchor_row = resolve_viewport_anchor(state, anchor).min(transcript_len);
    let start = anchor_row.saturating_sub(desired);
    let end = start.saturating_add(capacity).min(transcript_len);
    Some(transcript_len.saturating_sub(end).min(maximum))
}

pub(super) fn resolved_scroll_from_bottom(
    state: &ShellState,
    transcript_len: usize,
    available: usize,
) -> usize {
    let maximum = max_scroll_for_available(transcript_len, available);
    let requested = state.scroll_from_bottom.get().min(maximum);
    let scroll = if state.follow_tail {
        0
    } else {
        anchored_scroll_from_bottom(state, transcript_len, available, maximum).unwrap_or(requested)
    };
    state.scroll_from_bottom.set(scroll);
    scroll
}

fn transcript_viewport_lines(state: &ShellState, width: u16, available: usize) -> Vec<String> {
    let transcript = transcript_lines(state, width);
    let scroll = resolved_scroll_from_bottom(state, transcript.len(), available);
    let scrolled = scroll > 0;
    let capacity = transcript_viewport_capacity(available, scrolled);
    let end = transcript.len().saturating_sub(scroll);
    let start = end.saturating_sub(capacity);
    let mut lines = transcript[start..end].to_vec();
    if scrolled {
        capture_viewport_anchor(state, start, end);
    } else if state.follow_tail {
        state.viewport_anchor.set(None);
    }
    drop(transcript);

    if scrolled && lines.len() < available {
        let new_output = if state.new_output_count == 0 {
            String::new()
        } else {
            format!(
                "{}{} new",
                semantic_separator(&state.theme),
                state.new_output_count
            )
        };
        lines.push(fit_line(
            &state.theme.fg(
                "muted",
                &format!("↑ {scroll} rows back{new_output} · PageDown returns to live"),
            ),
            width,
        ));
    }
    lines
}

pub(super) fn render_shell_viewport_at(
    state: &ShellState,
    width: u16,
    now: Instant,
) -> Vec<String> {
    let chrome = shell_chrome(state, width, now);
    let mut lines = if state.overlay.is_some() {
        let mut overlay = overlay_lines(state, width);
        overlay.truncate(chrome.transcript_rows);
        overlay
    } else {
        transcript_viewport_lines(state, width, chrome.transcript_rows)
    };
    append_viewport_chrome(&mut lines, chrome);
    lines
}

pub(super) fn render_shell_viewport_update(
    state: &ShellState,
    width: u16,
    now: Instant,
    frame: &mut ShellFrameState,
) -> FrameUpdate {
    let repaint_theme = frame.initialized && frame.theme_epoch != state.theme_epoch;
    let resized = frame.initialized && (frame.width != width || frame.height != state.size.1);
    let entering_application_viewport = frame.initialized && !frame.application_viewport;
    frame.initialized = true;
    frame.width = width;
    frame.height = state.size.1;
    frame.theme_epoch = state.theme_epoch;
    frame.transcript_epoch = state.transcript_epoch;
    frame.verbose_tools = state.verbose_tools;
    frame.application_viewport = true;
    FrameUpdate {
        stable_prefix: 0,
        replacement: render_shell_viewport_at(state, width, now),
        pinned: None,
        resize_replay: None,
        reanchor_viewport: repaint_theme || resized || entering_application_viewport,
        rebuild_scrollback: false,
    }
}

#[cfg(test)]
mod tests {
    use super::wrap_overlay_text;

    #[test]
    fn overlay_truecolor_does_not_leak_a_background_to_following_rows() {
        let theme = crate::tui::theme::test_theme();
        let selected = theme.fg("accent", "selected");
        // The universal Ygg green includes RGB channel 107. It must remain an
        // RGB component rather than becoming a bright-white background SGR.
        assert!(selected.contains(";107m"));

        let wrapped = wrap_overlay_text(&format!("{selected}\nnext row"), 80);
        assert_eq!(wrapped.len(), 2);
        assert!(wrapped[0].contains("selected"));
        assert!(wrapped[1].contains("next row"));
        assert!(!wrapped[1].contains("\x1b[107m"));
    }
}
