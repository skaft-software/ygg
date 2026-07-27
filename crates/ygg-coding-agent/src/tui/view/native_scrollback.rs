use std::time::Instant;

use sexy_tui_rs::{CommitCursor, FrameUpdate};

use super::shell_chrome::{
    append_chrome, append_viewport_chrome, shell_chrome, shell_chrome_rows, ShellChrome,
};
use super::transcript_commit::transcript_pinned_frame;
use super::viewport::{overlay_lines, transcript_lines};
use super::{ShellFrameState, ShellState};

fn native_overlay_prefix_len(transcript_len: usize, chrome: &ShellChrome) -> usize {
    let chrome_rows = shell_chrome_rows(chrome);
    let normal_rows = transcript_len
        .saturating_add(usize::from(transcript_len > 0))
        .saturating_add(chrome_rows);
    let overlay_rows = chrome.transcript_rows.saturating_add(chrome_rows);
    normal_rows.saturating_sub(overlay_rows)
}

/// Build the native overlay as a screen-sized surface over the visible tail of
/// the normal transcript frame. Rows above that surface remain part of the
/// logical frame, so a destructive resize can replay terminal-owned history
/// without copying the overlay itself into scrollback.
fn render_native_overlay_suffix(
    state: &ShellState,
    width: u16,
    chrome: ShellChrome,
    transcript: &[String],
    requested_stable_prefix: usize,
) -> (usize, Vec<String>, usize, usize) {
    let overlay_prefix_len = native_overlay_prefix_len(transcript.len(), &chrome);
    let mut overlay = overlay_lines(state, width);
    append_viewport_chrome(&mut overlay, chrome);

    let transcript_prefix_len = overlay_prefix_len.min(transcript.len());
    let stable_prefix = requested_stable_prefix.min(transcript_prefix_len);

    let mut replacement = transcript[stable_prefix..transcript_prefix_len].to_vec();
    replacement.resize(
        replacement
            .len()
            .saturating_add(overlay_prefix_len.saturating_sub(transcript_prefix_len)),
        String::new(),
    );
    replacement.extend(overlay);
    let total_rows = stable_prefix.saturating_add(replacement.len());
    (stable_prefix, replacement, total_rows, overlay_prefix_len)
}

/// Full logical frame for the default terminal-owned renderer. The backend
/// paints only its visible tail; committed rows naturally move into native
/// scrollback and are never sliced into an application-owned viewport.
pub(super) fn render_shell_at(state: &ShellState, width: u16, now: Instant) -> Vec<String> {
    let chrome = shell_chrome(state, width, now);
    let transcript = transcript_lines(state, width);
    if state.overlay.is_some() {
        let (_, lines, _, _) = render_native_overlay_suffix(state, width, chrome, &transcript, 0);
        lines
    } else {
        let mut lines = transcript.clone();
        drop(transcript);
        append_chrome(&mut lines, chrome, 0);
        lines
    }
}

pub(super) fn synchronize_shell_frame(state: &ShellState, width: u16, frame: &mut ShellFrameState) {
    let _ = transcript_lines(state, width);
    let cache = state.transcript_cache.borrow();
    let overlay_prefix_len = state.overlay.as_ref().map_or(0, |_| {
        native_overlay_prefix_len(
            cache.lines.len(),
            &shell_chrome(state, width, Instant::now()),
        )
    });
    frame.initialized = true;
    frame.width = width;
    frame.height = state.size.1;
    frame.theme_epoch = state.theme_epoch;
    frame.transcript_epoch = state.transcript_epoch;
    frame.transcript_generation = cache.generation;
    frame.transcript_len = cache.lines.len();
    frame.verbose_tools = state.verbose_tools;
    frame.overlay_active = state.overlay.is_some();
    frame.overlay_prefix_len = overlay_prefix_len;
}

/// Build only the mutable suffix of the native-scrollback frame. Historic
/// transcript strings are neither cloned nor compared on streaming/status
/// ticks; sexy-tui reuses the committed prefix already retained in its frame.
pub(super) fn render_shell_update_with_cursor(
    state: &ShellState,
    width: u16,
    now: Instant,
    frame: &mut ShellFrameState,
    acknowledged: Option<CommitCursor>,
) -> FrameUpdate {
    let repaint_theme = frame.initialized && frame.theme_epoch != state.theme_epoch;
    let resized = frame.initialized && (frame.width != width || frame.height != state.size.1);
    let presentation_changed = frame.initialized && frame.verbose_tools != state.verbose_tools;
    let entering_overlay = frame.initialized && !frame.overlay_active && state.overlay.is_some();
    let leaving_overlay = frame.initialized && frame.overlay_active && state.overlay.is_none();
    let chrome = shell_chrome(state, width, now);

    let transcript_len = {
        let transcript = transcript_lines(state, width);
        transcript.len()
    };
    // Hydrating `/new` (or another session) replaces the logical transcript.
    // Visual row counts alone cannot identify that transition: a streaming
    // Markdown table routinely shrinks while incomplete syntax reparses.
    let cache = state.transcript_cache.borrow();
    let generation = cache.generation;
    let transcript_replaced = frame.initialized
        && frame.width == width
        && !frame.overlay_active
        && frame.transcript_epoch != state.transcript_epoch;
    let mut requested_stable_prefix =
        if !presentation_changed && frame.initialized && frame.width == width && !leaving_overlay {
            if frame.transcript_generation == cache.generation {
                frame.transcript_len.min(transcript_len)
            } else {
                cache
                    .last_update_start
                    .min(frame.transcript_len)
                    .min(transcript_len)
            }
        } else {
            0
        };
    if frame.overlay_active {
        requested_stable_prefix = requested_stable_prefix.min(frame.overlay_prefix_len);
    }

    if state.overlay.is_some() {
        let resize_replay = resized.then(|| {
            let mut replay = cache.lines.clone();
            append_chrome(&mut replay, chrome.clone(), 0);
            replay
        });
        let (stable_prefix, replacement, total_rows, overlay_prefix_len) =
            render_native_overlay_suffix(
                state,
                width,
                chrome,
                &cache.lines,
                requested_stable_prefix,
            );
        let pinned = transcript_pinned_frame(state, total_rows, acknowledged);
        drop(cache);

        frame.initialized = true;
        frame.width = width;
        frame.height = state.size.1;
        frame.theme_epoch = state.theme_epoch;
        frame.transcript_epoch = state.transcript_epoch;
        frame.transcript_generation = generation;
        frame.transcript_len = transcript_len;
        frame.verbose_tools = state.verbose_tools;
        frame.overlay_active = true;
        frame.overlay_prefix_len = overlay_prefix_len;
        return FrameUpdate {
            stable_prefix,
            replacement,
            pinned: Some(pinned),
            resize_replay,
            reanchor_viewport: repaint_theme || resized || entering_overlay,
            // Overlay rows are a temporary screen surface. Presentation changes
            // repaint that surface without restyling terminal-owned history.
            rebuild_scrollback: false,
        };
    }

    let stable_prefix = requested_stable_prefix;
    let mut replacement = cache.lines[stable_prefix..].to_vec();
    drop(cache);
    append_chrome(&mut replacement, chrome, stable_prefix);
    let pinned = transcript_pinned_frame(
        state,
        stable_prefix.saturating_add(replacement.len()),
        acknowledged,
    );

    frame.initialized = true;
    frame.width = width;
    frame.height = state.size.1;
    frame.theme_epoch = state.theme_epoch;
    frame.transcript_epoch = state.transcript_epoch;
    frame.transcript_generation = generation;
    frame.transcript_len = transcript_len;
    frame.verbose_tools = state.verbose_tools;
    frame.overlay_active = false;
    frame.overlay_prefix_len = 0;
    FrameUpdate {
        stable_prefix,
        replacement,
        pinned: Some(pinned),
        resize_replay: None,
        reanchor_viewport: repaint_theme || resized || leaving_overlay || transcript_replaced,
        rebuild_scrollback: presentation_changed,
    }
}

#[cfg(test)]
pub(super) fn render_shell_update(
    state: &ShellState,
    width: u16,
    now: Instant,
    frame: &mut ShellFrameState,
) -> FrameUpdate {
    render_shell_update_with_cursor(state, width, now, frame, None)
}

pub(super) fn render_shell(state: &ShellState, width: u16) -> Vec<String> {
    render_shell_at(state, width, Instant::now())
}
