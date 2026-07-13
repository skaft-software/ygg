#![allow(missing_docs)]

use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use sexy_tui_rs::{visible_width, wrap_text_with_ansi, Component, TUI};
use unicode_width::UnicodeWidthChar;
use ygg_agent::{AgentEvent, OutputChannel, Session, ToolProgress};
use ygg_ai::{ToolCallId, Usage};

use crate::commands;
use crate::config::Config;
use crate::hydrate::{hydrate_transcript, TranscriptItem};
use crate::tui::keymap::EditAction;
use crate::tui::terminal::{force_restore, TerminalSize, YggTerminal};

const MAX_PANEL_BYTES: usize = 64 * 1024;
const RENDER_INTERVAL: Duration = Duration::from_millis(16);
const ELISION_MARKER: &str = "\n… older tool output elided …\n";
const PROMPT_CURSOR: &str = "▎";

/// Append display output while retaining only the newest 64 KiB.
pub fn bounded_append(existing: &mut String, additional: &str) {
    if existing.len().saturating_add(additional.len()) <= MAX_PANEL_BYTES {
        existing.push_str(additional);
        return;
    }

    let mut combined = String::with_capacity(existing.len().saturating_add(additional.len()));
    combined.push_str(existing);
    combined.push_str(additional);
    let tail_budget = MAX_PANEL_BYTES.saturating_sub(ELISION_MARKER.len());
    let mut start = combined.len().saturating_sub(tail_budget);
    while start < combined.len() && !combined.is_char_boundary(start) {
        start += 1;
    }
    existing.clear();
    existing.push_str(ELISION_MARKER);
    existing.push_str(&combined[start..]);
}

#[derive(Clone, Debug)]
enum TranscriptBlock {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool(ToolPanel),
    Notice(String),
    Compaction(String),
}

#[derive(Clone, Debug)]
struct ToolPanel {
    id: ToolCallId,
    name: String,
    args: String,
    output: String,
    finished: bool,
    is_error: bool,
}

#[derive(Clone, Debug)]
struct TranscriptCache {
    width: Option<u16>,
    lines: Vec<String>,
    block_starts: Vec<usize>,
    block_lengths: Vec<usize>,
    block_revisions: Vec<u64>,
    dirty: bool,
    generation: u64,
}

impl Default for TranscriptCache {
    fn default() -> Self {
        Self {
            width: None,
            lines: Vec::new(),
            block_starts: Vec::new(),
            block_lengths: Vec::new(),
            block_revisions: Vec::new(),
            dirty: true,
            generation: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ShellState {
    theme: sexy_tui_rs::theme::Theme,
    transcript: Vec<TranscriptBlock>,
    /// Monotonic revisions let the renderer update only blocks whose text or
    /// tool output changed.
    block_revisions: Vec<u64>,
    /// Steering messages accepted while a run is active but not yet injected.
    steering_queue: Vec<String>,
    /// Cached wrapped transcript lines. Scrolling only slices this cache rather
    /// than markdown-rendering the complete history for every wheel event.
    transcript_cache: RefCell<TranscriptCache>,
    editor: String,
    /// Byte offset into `editor`; always kept at a UTF-8 character boundary.
    editor_cursor: usize,
    status: String,
    status_detail: String,
    error: Option<String>,
    overlay: Option<String>,
    tool_panels: HashMap<ToolCallId, usize>,
    active_text: Option<usize>,
    active_reasoning: Option<usize>,
    scroll_from_bottom: usize,
    context_estimate: Option<(u64, u64)>,
    last_turn_usage: Option<Usage>,
    run_label: String,
    size: (u16, u16),
}

impl ShellState {
    fn invalidate_transcript(&mut self) {
        self.transcript_cache.get_mut().dirty = true;
    }

    fn invalidate_transcript_layout(&mut self) {
        let cache = self.transcript_cache.get_mut();
        cache.width = None;
        cache.dirty = true;
    }

    fn push_block(&mut self, block: TranscriptBlock) {
        self.transcript.push(block);
        self.block_revisions.push(0);
        self.invalidate_transcript();
    }

    fn touch_block(&mut self, index: usize) {
        if let Some(revision) = self.block_revisions.get_mut(index) {
            *revision = revision.saturating_add(1);
        }
        self.invalidate_transcript();
    }

    fn rendered_transcript(&self, width: u16) -> Ref<'_, Vec<String>> {
        let stale = self.transcript_cache.borrow().dirty;
        if stale {
            let mut cache = self.transcript_cache.borrow_mut();
            let rebuild =
                cache.width != Some(width) || cache.block_revisions.len() > self.transcript.len();

            if rebuild {
                cache.lines.clear();
                cache.block_starts.clear();
                cache.block_lengths.clear();
                cache.block_revisions.clear();
                cache.width = Some(width);

                for (index, block) in self.transcript.iter().enumerate() {
                    let rendered = render_block(block, &self.theme, width);
                    let start = cache.lines.len();
                    let length = rendered.len();
                    cache.lines.extend(rendered);
                    cache.block_starts.push(start);
                    cache.block_lengths.push(length);
                    cache.block_revisions.push(self.block_revisions[index]);
                }
            } else {
                // New blocks are appended in normal operation. Render them
                // once and leave every existing block's layout untouched.
                while cache.block_revisions.len() < self.transcript.len() {
                    let index = cache.block_revisions.len();
                    let rendered = render_block(&self.transcript[index], &self.theme, width);
                    let start = cache.lines.len();
                    let length = rendered.len();
                    cache.lines.extend(rendered);
                    cache.block_starts.push(start);
                    cache.block_lengths.push(length);
                    cache.block_revisions.push(self.block_revisions[index]);
                }

                for index in 0..self.transcript.len() {
                    if cache.block_revisions[index] == self.block_revisions[index] {
                        continue;
                    }
                    let start = cache.block_starts[index];
                    let old_length = cache.block_lengths[index];
                    let rendered = render_block(&self.transcript[index], &self.theme, width);
                    let new_length = rendered.len();
                    cache.lines.splice(start..start + old_length, rendered);
                    cache.block_lengths[index] = new_length;
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

            cache.dirty = false;
            cache.generation = cache.generation.saturating_add(1);
        }
        Ref::map(self.transcript_cache.borrow(), |cache| &cache.lines)
    }

    fn append_text_block(&mut self, channel: OutputChannel, text: &str) {
        let active_index = match channel {
            OutputChannel::Text => self.active_text,
            OutputChannel::Reasoning => self.active_reasoning,
        };
        if let Some(index) = active_index {
            let updated = match self.transcript.get_mut(index) {
                Some(TranscriptBlock::Assistant(existing)) if channel == OutputChannel::Text => {
                    existing.push_str(text);
                    true
                }
                Some(TranscriptBlock::Reasoning(existing))
                    if channel == OutputChannel::Reasoning =>
                {
                    existing.push_str(text);
                    true
                }
                _ => false,
            };
            if updated {
                self.touch_block(index);
                return;
            }
            match channel {
                OutputChannel::Text => self.active_text = None,
                OutputChannel::Reasoning => self.active_reasoning = None,
            }
        }

        let index = self.transcript.len();
        self.push_block(match channel {
            OutputChannel::Text => TranscriptBlock::Assistant(text.to_owned()),
            OutputChannel::Reasoning => TranscriptBlock::Reasoning(text.to_owned()),
        });
        match channel {
            OutputChannel::Text => self.active_text = Some(index),
            OutputChannel::Reasoning => self.active_reasoning = Some(index),
        }
    }

    fn close_streaming_blocks(&mut self) {
        self.active_text = None;
        self.active_reasoning = None;
    }

    fn tool_output_mut(&mut self, id: &ToolCallId) -> Option<&mut ToolPanel> {
        let index = *self.tool_panels.get(id)?;
        self.touch_block(index);
        match self.transcript.get_mut(index) {
            Some(TranscriptBlock::Tool(panel)) => Some(panel),
            _ => None,
        }
    }
}

/// Thread-safe handle to the mutable shell model. The TUI renderer owns a
/// clone of this handle and performs all expensive layout work away from the
/// async agent/input loop.
#[derive(Clone)]
struct SharedState(Arc<Mutex<ShellState>>);

impl SharedState {
    fn new(state: ShellState) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    fn borrow(&self) -> MutexGuard<'_, ShellState> {
        self.0.lock().expect("shell state mutex poisoned")
    }

    fn borrow_mut(&self) -> MutexGuard<'_, ShellState> {
        self.0.lock().expect("shell state mutex poisoned")
    }
}

enum RenderCommand {
    Render,
    Stop,
}

fn render_loop(terminal: YggTerminal, state: SharedState, rx: Receiver<RenderCommand>) {
    let mut tui = TUI::new(Box::new(terminal));
    tui.add_child(Box::new(ShellComponent { state }));
    tui.start();

    let mut last_render: Option<Instant> = None;
    while let Ok(command) = rx.recv() {
        if matches!(command, RenderCommand::Stop) {
            break;
        }

        // Keep rendering bounded to one frame per 16 ms. The channel is
        // bounded too, so a burst of model deltas coalesces into the latest
        // state instead of queueing unbounded full-frame work.
        if let Some(last) = last_render {
            let elapsed = last.elapsed();
            if elapsed < RENDER_INTERVAL {
                thread::sleep(RENDER_INTERVAL - elapsed);
            }
        }

        // Discard redundant render requests. The shared state already holds
        // the newest transcript, so only the final request matters.
        let mut stop = false;
        while let Ok(next) = rx.try_recv() {
            if matches!(next, RenderCommand::Stop) {
                stop = true;
                break;
            }
        }
        if stop {
            break;
        }

        tui.request_render();
        last_render = Some(Instant::now());
    }

    tui.stop();
}

/// The retained root component. It reads the shell state at render time, while
/// `InteractiveShell` mutates that same state in response to events.
struct ShellComponent {
    state: SharedState,
}

impl Component for ShellComponent {
    fn render(&self, width: u16) -> Vec<String> {
        render_shell(&self.state.borrow(), width)
    }

    fn invalidate(&mut self) {}
}

fn render_block(
    block: &TranscriptBlock,
    theme: &sexy_tui_rs::theme::Theme,
    width: u16,
) -> Vec<String> {
    match block {
        TranscriptBlock::User(text) => {
            let heading = theme.bold(&theme.fg("accent", "You"));
            let mut lines = vec![heading];
            lines.extend(markdown_lines(text, theme, width));
            lines.push(String::new());
            lines
        }
        TranscriptBlock::Assistant(text) => {
            let heading = theme.bold(&theme.fg("accent", "Assistant"));
            let mut lines = vec![heading];
            lines.extend(markdown_lines(text, theme, width));
            lines.push(String::new());
            lines
        }
        TranscriptBlock::Reasoning(text) => {
            let heading = theme.dim("Thinking");
            let mut lines = vec![heading];
            for line in wrap_plain(text, width.saturating_sub(2) as usize) {
                lines.push(theme.dim(&format!("  {line}")));
            }
            lines.push(String::new());
            lines
        }
        TranscriptBlock::Tool(panel) => {
            let state = if panel.finished {
                if panel.is_error {
                    theme.fg("error", "failed")
                } else {
                    theme.fg("success", "done")
                }
            } else {
                theme.fg("accent", "running")
            };
            let mut lines =
                vec![theme.bold(&format!("Tool {} ({}) [{state}]", panel.name, panel.id.0))];
            if !panel.args.is_empty() && panel.args != "null" {
                lines.push(theme.dim(&format!("  args: {}", panel.args)));
            }
            if panel.output.is_empty() {
                lines.push(theme.dim("  (waiting for output)"));
            } else {
                for line in panel.output.lines() {
                    lines.push(theme.dim(&format!("  {line}")));
                }
            }
            lines.push(String::new());
            lines
        }
        TranscriptBlock::Notice(text) => vec![theme.fg("muted", text), String::new()],
        TranscriptBlock::Compaction(text) => {
            vec![theme.dim(&format!("[compacted] {text}")), String::new()]
        }
    }
}

fn markdown_lines(text: &str, theme: &sexy_tui_rs::theme::Theme, width: u16) -> Vec<String> {
    let inner_width = usize::from(width.saturating_sub(2).max(1));
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut previous_was_blank = false;

    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            lines.push(format!(" {}", theme.fg("muted", &"─".repeat(inner_width))));
            previous_was_blank = false;
            continue;
        }

        if trimmed.is_empty() {
            // Consecutive source blank lines should not turn a compact transcript
            // into a sparse document.
            if !previous_was_blank {
                lines.push(String::new());
            }
            previous_was_blank = true;
            continue;
        }
        previous_was_blank = false;

        let rendered = if in_code_block {
            theme.fg("muted", &format!("  {raw}"))
        } else if is_horizontal_rule(trimmed) {
            theme.fg("muted", &"─".repeat(inner_width))
        } else if let Some((level, heading)) = markdown_heading(trimmed) {
            let heading = render_inline_markdown(heading, theme);
            if level <= 2 {
                theme.bold(&theme.fg("accent", &heading))
            } else {
                theme.bold(&heading)
            }
        } else if let Some((prefix, item)) = markdown_list_item(raw) {
            format!(
                "{}{}",
                theme.fg("accent", &prefix),
                render_inline_markdown(item, theme)
            )
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            theme.dim(&format!("│ {}", render_inline_markdown(quote, theme)))
        } else {
            render_inline_markdown(raw, theme)
        };

        for wrapped in wrap_text_with_ansi(&rendered, inner_width) {
            lines.push(format!(" {wrapped}"));
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn is_horizontal_rule(line: &str) -> bool {
    let compact: String = line
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact.len() >= 3
        && compact
            .chars()
            .next()
            .is_some_and(|marker| matches!(marker, '-' | '*' | '_'))
        && compact
            .chars()
            .all(|character| character == compact.chars().next().unwrap_or_default())
}

fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let level = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (1..=6).contains(&level) {
        line.get(level..)
            .and_then(|remaining| remaining.strip_prefix(' '))
            .map(|heading| (level, heading))
    } else {
        None
    }
}

fn markdown_list_item(line: &str) -> Option<(String, &str)> {
    let indent = line.len().saturating_sub(line.trim_start().len());
    let trimmed = line.trim_start();
    let prefix = " ".repeat(indent);
    for marker in ["- ", "* ", "+ "] {
        if let Some(item) = trimmed.strip_prefix(marker) {
            return Some((format!("{prefix}• "), item));
        }
    }

    let dot = trimmed.find('.')?;
    let (number, remainder) = trimmed.split_at(dot);
    if !number.is_empty() && number.chars().all(|character| character.is_ascii_digit()) {
        remainder
            .strip_prefix(". ")
            .map(|item| (format!("{prefix}{number}. "), item))
    } else {
        None
    }
}

fn render_inline_markdown(text: &str, theme: &sexy_tui_rs::theme::Theme) -> String {
    let mut rendered = String::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if let Some((content, rest)) =
            delimited_markdown(remaining, "**").or_else(|| delimited_markdown(remaining, "__"))
        {
            rendered.push_str(&theme.bold(&render_inline_markdown(content, theme)));
            remaining = rest;
            continue;
        }
        if let Some((content, rest)) = delimited_markdown(remaining, "`") {
            rendered.push_str(&theme.fg("muted", content));
            remaining = rest;
            continue;
        }

        let character = remaining
            .chars()
            .next()
            .expect("remaining is checked non-empty");
        rendered.push(character);
        remaining = &remaining[character.len_utf8()..];
    }

    rendered
}

fn delimited_markdown<'a>(text: &'a str, delimiter: &str) -> Option<(&'a str, &'a str)> {
    let body = text.strip_prefix(delimiter)?;
    let end = body.find(delimiter)?;
    Some((&body[..end], &body[end + delimiter.len()..]))
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut remaining = raw_line;
        while remaining.chars().count() > width {
            let split = remaining
                .char_indices()
                .nth(width)
                .map_or(remaining.len(), |(index, _)| index);
            lines.push(remaining[..split].to_owned());
            remaining = &remaining[split..];
        }
        lines.push(remaining.to_owned());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[derive(Clone, Debug)]
struct EditorVisualLine {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
struct EditorLayout {
    lines: Vec<EditorVisualLine>,
    cursor_row: usize,
}

fn prompt_content_width(width: u16) -> usize {
    // left border + left padding + content + right padding + right border
    usize::from(width).saturating_sub(4)
}

fn editor_wrap_width(width: u16) -> usize {
    // Reserve one cell for the rendered bar cursor so it never pushes a line
    // past the prompt border.
    prompt_content_width(width).saturating_sub(1).max(1)
}

/// Normalize terminal paste line endings before placing them in the editor.
/// Bracketed paste must never submit the prompt or turn CRLF into visual `\r`
/// characters in a multi-line editor.
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn editor_layout(text: &str, cursor: usize, width: u16) -> EditorLayout {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }

    let wrap_width = editor_wrap_width(width);
    let mut lines = Vec::new();
    let mut start = 0;
    let mut columns: usize = 0;

    for (offset, character) in text.char_indices() {
        if character == '\n' {
            lines.push(EditorVisualLine { start, end: offset });
            start = offset + character.len_utf8();
            columns = 0;
            continue;
        }

        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if columns > 0 && columns.saturating_add(character_width) > wrap_width {
            lines.push(EditorVisualLine { start, end: offset });
            start = offset;
            columns = 0;
        }
        columns = columns.saturating_add(character_width);
    }
    // Keep a visible editable row for empty text and after a trailing newline.
    lines.push(EditorVisualLine {
        start,
        end: text.len(),
    });

    let cursor_row = lines
        .iter()
        .position(|line| {
            (line.start == line.end && cursor == line.start)
                || (cursor >= line.start && cursor < line.end)
        })
        .or_else(|| lines.iter().rposition(|line| cursor == line.end))
        .unwrap_or(0);

    EditorLayout { lines, cursor_row }
}

fn editor_column(text: &str, line: &EditorVisualLine, cursor: usize) -> usize {
    visible_width(&text[line.start..cursor.clamp(line.start, line.end)])
}

fn editor_offset_at_column(text: &str, line: &EditorVisualLine, target: usize) -> usize {
    let mut offset = line.start;
    let mut column: usize = 0;
    for (relative, character) in text[line.start..line.end].char_indices() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if column.saturating_add(width) > target {
            break;
        }
        column = column.saturating_add(width);
        offset = line.start + relative + character.len_utf8();
    }
    offset
}

fn prompt_border(theme: &sexy_tui_rs::theme::Theme, width: u16, top: bool) -> String {
    let width = usize::from(width);
    let border = match (width, top) {
        (0, _) => String::new(),
        (1, true) => "┏".to_owned(),
        (1, false) => "┗".to_owned(),
        (2, true) => "┏┓".to_owned(),
        (2, false) => "┗┛".to_owned(),
        (_, true) if width >= 11 => format!("┏━ prompt {}┓", "━".repeat(width - 11)),
        (_, true) => format!("┏{}┓", "━".repeat(width - 2)),
        (_, false) => format!("┗{}┛", "━".repeat(width - 2)),
    };
    theme.fg("accent", &border)
}

fn prompt_content_line(theme: &sexy_tui_rs::theme::Theme, content: String, width: u16) -> String {
    let width = usize::from(width);
    let border = |text| theme.fg("accent", text);
    match width {
        0 => return String::new(),
        1 => return border("┃"),
        2 => return border("┃┃"),
        3 => return border("┃ ┃"),
        _ => {}
    }

    let content_width = width.saturating_sub(4);
    let content = if content_width == 0 {
        String::new()
    } else if visible_width(&content) > content_width {
        sexy_tui_rs::truncate_to_width(&content, content_width, None)
    } else {
        content
    };
    let padding = " ".repeat(content_width.saturating_sub(visible_width(&content)));
    let border = border("┃");
    format!("{border} {content}{padding} {border}")
}

fn render_prompt_box(state: &ShellState, width: u16, max_content_rows: usize) -> Vec<String> {
    let mut lines = vec![prompt_border(&state.theme, width, true)];

    if state.editor.is_empty() {
        let cursor = state.theme.fg("accent", PROMPT_CURSOR);
        let placeholder = state.theme.dim(" Type a prompt or /help");
        lines.push(prompt_content_line(
            &state.theme,
            format!("{cursor}{placeholder}"),
            width,
        ));
    } else {
        let layout = editor_layout(&state.editor, state.editor_cursor, width);
        let visible_rows = max_content_rows.max(1).min(layout.lines.len());
        let mut start = layout
            .cursor_row
            .saturating_add(1)
            .saturating_sub(visible_rows);
        let mut end = (start + visible_rows).min(layout.lines.len());
        if end.saturating_sub(start) < visible_rows {
            start = end.saturating_sub(visible_rows);
        }
        // Keep this assignment explicit: it makes the viewport invariant clear
        // if the selection policy above changes later.
        end = (start + visible_rows).min(layout.lines.len());

        for (index, line) in layout.lines[start..end].iter().enumerate() {
            let index = start + index;
            let content = if index == layout.cursor_row {
                let cursor = state.editor_cursor.clamp(line.start, line.end);
                let before = &state.editor[line.start..cursor];
                let after = &state.editor[cursor..line.end];
                format!("{before}{}{after}", state.theme.fg("accent", PROMPT_CURSOR))
            } else {
                state.editor[line.start..line.end].to_owned()
            };
            lines.push(prompt_content_line(&state.theme, content, width));
        }
    }

    lines.push(prompt_border(&state.theme, width, false));
    lines
}

fn render_slash_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    let suggestions = commands::slash_suggestions(&state.editor);
    if suggestions.is_empty() || max_rows == 0 {
        return Vec::new();
    }

    let mut lines = vec![state.theme.dim("  Slash commands · Tab completes")];
    let item_rows = max_rows.saturating_sub(1);
    if item_rows == 0 {
        return lines;
    }

    let hidden = suggestions.len().saturating_sub(item_rows);
    let visible = if hidden == 0 {
        suggestions.len()
    } else {
        item_rows.saturating_sub(1)
    };
    for command in suggestions.into_iter().take(visible) {
        let usage = format!("  {:<20}", command.usage);
        let description_width = usize::from(width)
            .saturating_sub(visible_width(&usage))
            .saturating_sub(1);
        let description =
            sexy_tui_rs::truncate_to_width(command.description, description_width, None);
        lines.push(format!(
            "{}{}",
            state.theme.fg("accent", &usage),
            state.theme.dim(&description)
        ));
    }
    if hidden > 0 {
        lines.push(state.theme.dim(&format!("  … {hidden} more commands")));
    }
    lines
}

fn render_pending_steering(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if state.steering_queue.is_empty() || max_rows == 0 {
        return Vec::new();
    }

    let mut lines = vec![state.theme.dim(&format!(
        "  Queued steering ({})",
        state.steering_queue.len()
    ))];
    let item_rows = max_rows.saturating_sub(1);
    if item_rows == 0 {
        return lines;
    }

    let available_width = usize::from(width).saturating_sub(1);
    let visible = state.steering_queue.len().min(item_rows);
    for message in state.steering_queue.iter().take(visible) {
        // Keep each queued message on one predictable row so a burst of
        // steering prompts cannot consume the whole transcript viewport.
        let compact = message.replace(['\r', '\n'], " ↵ ");
        let line = sexy_tui_rs::truncate_to_width(
            &format!("  Steering: {compact}"),
            available_width,
            None,
        );
        lines.push(state.theme.dim(&line));
    }
    let hidden = state.steering_queue.len().saturating_sub(visible);
    if hidden > 0 {
        lines.push(
            state
                .theme
                .dim(&format!("  … {hidden} more steering messages")),
        );
    }
    lines.truncate(max_rows);
    lines
}

fn transcript_lines(state: &ShellState, width: u16) -> Ref<'_, Vec<String>> {
    state.rendered_transcript(width)
}

fn max_scroll_for_available(transcript_len: usize, available: usize) -> usize {
    // A scrolled viewport reserves one line for its return-to-live indicator,
    // but an exactly-full transcript still has no hidden content to scroll.
    if available <= 1 || transcript_len <= available {
        0
    } else {
        transcript_len - (available - 1)
    }
}

fn max_scroll_from_bottom(state: &ShellState, width: u16) -> usize {
    if state.overlay.is_some() {
        return 0;
    }
    let rows = usize::from(state.size.1.max(5));
    // Status bar + optional error line are fixed rows below the prompt box.
    let fixed_bottom = 1 + usize::from(state.error.is_some());
    let pending_budget = rows.saturating_sub(fixed_bottom).saturating_sub(4);
    let pending = render_pending_steering(state, width, pending_budget);
    let suggestion_budget = rows
        .saturating_sub(fixed_bottom)
        .saturating_sub(pending.len())
        .saturating_sub(4);
    let suggestions = render_slash_suggestions(state, width, suggestion_budget);
    let prompt_max_rows = rows
        .saturating_sub(fixed_bottom)
        .saturating_sub(pending.len())
        .saturating_sub(suggestions.len())
        .saturating_sub(3)
        .max(1);
    let prompt_height = render_prompt_box(state, width, prompt_max_rows).len();
    let available = rows
        .saturating_sub(fixed_bottom)
        .saturating_sub(pending.len())
        .saturating_sub(suggestions.len())
        .saturating_sub(prompt_height);
    max_scroll_for_available(transcript_lines(state, width).len(), available)
}

fn render_shell(state: &ShellState, width: u16) -> Vec<String> {
    let rows = usize::from(state.size.1.max(5));
    let mut lines = Vec::new();

    // Build the status bar line (rendered below the prompt box).
    let mut status = state.status.clone();
    if !state.run_label.is_empty() {
        if !status.is_empty() {
            status.push_str("  ·  ");
        }
        status.push_str(&state.run_label);
    }
    if let Some((estimate, budget)) = state.context_estimate {
        if !status.is_empty() {
            status.push_str("  ·  ");
        }
        status.push_str(&format!("context ~{estimate}/{budget}"));
    }
    if let Some(usage) = state.last_turn_usage {
        if !status.is_empty() {
            status.push_str("  ·  ");
        }
        status.push_str(&format!(
            "last turn {} in / {} out",
            usage.input_tokens, usage.output_tokens
        ));
    }
    if status.is_empty() {
        status = "ygg".to_owned();
    }
    let status_line = state.theme.bold(&state.theme.fg("accent", &status));
    let error_line = state
        .error
        .as_ref()
        .map(|e| state.theme.fg("error", &format!("Error: {e}")));

    // Fixed rows consumed below the prompt box.
    let fixed_bottom = 1 + usize::from(error_line.is_some());

    if let Some(overlay) = &state.overlay {
        lines.push(state.theme.bold(&state.theme.fg("accent", "─ overlay ─")));
        for line in wrap_plain(overlay, usize::from(width.saturating_sub(2))) {
            lines.push(format!(" {line}"));
        }
        lines.push(state.theme.dim("Press Esc or any printable key to close."));
        let pending_budget = rows
            .saturating_sub(lines.len() + fixed_bottom)
            .saturating_sub(2);
        let pending = render_pending_steering(state, width, pending_budget);
        let prompt_max_rows = rows
            .saturating_sub(lines.len() + fixed_bottom)
            .saturating_sub(pending.len())
            .saturating_sub(2)
            .max(1);
        lines.extend(pending);
        lines.extend(render_prompt_box(state, width, prompt_max_rows));
        lines.push(status_line);
        if let Some(err) = error_line {
            lines.push(err);
        }
        return lines;
    }

    let pending_budget = rows.saturating_sub(fixed_bottom).saturating_sub(4);
    let pending = render_pending_steering(state, width, pending_budget);
    let suggestion_budget = rows
        .saturating_sub(fixed_bottom)
        .saturating_sub(pending.len())
        .saturating_sub(4);
    let suggestions = render_slash_suggestions(state, width, suggestion_budget);
    let prompt_max_rows = rows
        .saturating_sub(fixed_bottom)
        .saturating_sub(pending.len())
        .saturating_sub(suggestions.len())
        .saturating_sub(3)
        .max(1);
    let prompt = render_prompt_box(state, width, prompt_max_rows);
    let available = rows
        .saturating_sub(fixed_bottom)
        .saturating_sub(pending.len())
        .saturating_sub(suggestions.len())
        .saturating_sub(prompt.len());
    let transcript = transcript_lines(state, width);
    let max_scroll = max_scroll_for_available(transcript.len(), available);
    let scroll_from_bottom = state.scroll_from_bottom.min(max_scroll);
    let show_scroll_indicator = scroll_from_bottom > 0 && available > 1;
    let transcript_capacity = available.saturating_sub(usize::from(show_scroll_indicator));
    let end = transcript.len().saturating_sub(scroll_from_bottom);
    let start = end.saturating_sub(transcript_capacity);
    lines.extend(transcript[start..end].iter().cloned());
    if show_scroll_indicator {
        lines.push(
            state
                .theme
                .dim("↑ scrolled transcript (mouse/trackpad or PageDown returns to live)"),
        );
    }
    lines.extend(pending);
    lines.extend(suggestions);
    lines.extend(prompt);
    lines.push(status_line);
    if let Some(err) = error_line {
        lines.push(err);
    }
    lines
}

/// Full-screen terminal shell. It owns all terminal I/O and no Agent state.
pub struct InteractiveShell {
    // Production rendering runs on a dedicated OS thread. Tests keep an
    // inline TUI so they can inspect rendering deterministically without a
    // background thread.
    tui: Option<TUI<'static>>,
    state: SharedState,
    size: TerminalSize,
    render_tx: Option<SyncSender<RenderCommand>>,
    render_thread: Option<JoinHandle<()>>,
    theme_config: Option<Config>,
}

impl InteractiveShell {
    /// Enter alternate-screen raw mode and start the retained TUI renderer.
    pub fn enter(theme: sexy_tui_rs::theme::Theme, size: TerminalSize) -> Result<Self> {
        let terminal = YggTerminal::enter_with_size(size.clone())?;
        let initial_size = *size.lock().expect("terminal size mutex poisoned");
        let state = SharedState::new(ShellState {
            theme,
            size: initial_size,
            ..ShellState::default()
        });
        let (render_tx, render_rx) = mpsc::sync_channel(1);
        let render_state = state.clone();
        let render_thread = thread::Builder::new()
            .name("ygg-tui-render".to_owned())
            .spawn(move || render_loop(terminal, render_state, render_rx))?;

        Ok(Self {
            tui: None,
            state,
            size,
            render_tx: Some(render_tx),
            render_thread: Some(render_thread),
            theme_config: None,
        })
    }

    #[cfg(test)]
    pub fn test_shell() -> Self {
        let size = Arc::new(Mutex::new((120, 40)));
        let initial_size = *size.lock().expect("terminal size mutex poisoned");
        let state = SharedState::new(ShellState {
            theme: sexy_tui_rs::theme::Theme::load(
                None,
                sexy_tui_rs::theme::capability::CapabilityTier::Baseline,
            ),
            size: initial_size,
            ..ShellState::default()
        });
        let mut tui = TUI::new(Box::new(TestTerminal { size: size.clone() }));
        tui.add_child(Box::new(ShellComponent {
            state: state.clone(),
        }));
        tui.start();
        Self {
            tui: Some(tui),
            state,
            size,
            render_tx: None,
            render_thread: None,
            theme_config: None,
        }
    }

    fn stop_renderer(&mut self) {
        if let Some(render_tx) = self.render_tx.take() {
            let _ = render_tx.send(RenderCommand::Stop);
        }
        if let Some(render_thread) = self.render_thread.take() {
            let _ = render_thread.join();
        }
        if let Some(mut tui) = self.tui.take() {
            tui.stop();
        }
    }

    /// Stop rendering and restore the process terminal.
    pub fn leave(mut self) {
        self.stop_renderer();
        force_restore();
    }

    /// Queue a retained-frame render without doing layout on the async loop.
    /// The bounded renderer queue coalesces bursts of model/tool events.
    pub fn render(&mut self) {
        if let Some(render_tx) = &self.render_tx {
            let _ = render_tx.try_send(RenderCommand::Render);
        } else if let Some(tui) = self.tui.as_mut() {
            tui.request_render();
        }
    }

    pub fn on_agent_event(&mut self, event: &AgentEvent) {
        let mut state = self.state.borrow_mut();
        match event {
            AgentEvent::OutputDelta { channel, text } => state.append_text_block(*channel, text),
            AgentEvent::SteeringDelivered { messages } => {
                state.close_streaming_blocks();
                for message in messages {
                    if let Some(index) = state
                        .steering_queue
                        .iter()
                        .position(|queued| queued == message)
                    {
                        state.steering_queue.remove(index);
                        state.push_block(TranscriptBlock::User(message.clone()));
                    }
                }
                state.scroll_from_bottom = 0;
            }
            AgentEvent::ToolStarted { id, name, args } => {
                state.close_streaming_blocks();
                let index = state.transcript.len();
                state.push_block(TranscriptBlock::Tool(ToolPanel {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.to_string(),
                    output: String::new(),
                    finished: false,
                    is_error: false,
                }));
                state.tool_panels.insert(id.clone(), index);
                state.run_label = format!("tool: {name}");
            }
            AgentEvent::ToolProgress { id, progress } => {
                if let Some(panel) = state.tool_output_mut(id) {
                    match progress {
                        ToolProgress::Output { bytes, .. } => {
                            bounded_append(&mut panel.output, &String::from_utf8_lossy(bytes));
                        }
                        ToolProgress::Status(message) => {
                            bounded_append(&mut panel.output, &format!("{message}\n"));
                        }
                        ToolProgress::Dropped { bytes } => {
                            bounded_append(
                                &mut panel.output,
                                &format!("… {bytes} bytes of live output elided …\n"),
                            );
                        }
                    }
                }
            }
            AgentEvent::ToolFinished { id, result } => {
                if let Some(panel) = state.tool_output_mut(id) {
                    panel.finished = true;
                    match result {
                        Ok(output) => {
                            panel.is_error = false;
                            if panel.output.is_empty() {
                                bounded_append(&mut panel.output, &output.text);
                            }
                        }
                        Err(error) => {
                            panel.is_error = true;
                            bounded_append(&mut panel.output, &error.message);
                        }
                    }
                    state.run_label.clear();
                }
            }
            AgentEvent::TurnFinished { usage, .. } => {
                state.close_streaming_blocks();
                state.last_turn_usage = Some(*usage);
                state.run_label = "turn complete".to_owned();
            }
            AgentEvent::RunFinished { reason, .. } => {
                state.close_streaming_blocks();
                state.run_label = format!("run: {reason:?}");
            }
        }
    }

    pub fn on_turn_finished(&mut self, usage: &Usage) {
        let mut state = self.state.borrow_mut();
        state.close_streaming_blocks();
        state.last_turn_usage = Some(*usage);
    }

    /// Update the request-context estimate at an idle boundary, where App is
    /// available to reconstruct the actual next request safely.
    pub fn set_context_estimate(&mut self, estimate: u64, budget: u64) {
        self.state.borrow_mut().context_estimate = Some((estimate, budget));
    }

    /// Add a locally submitted prompt immediately; Agent persistence follows
    /// only after `Agent::prompt` succeeds.
    pub fn on_prompt_submitted(&mut self, prompt: &str) {
        let mut state = self.state.borrow_mut();
        state.close_streaming_blocks();
        state.push_block(TranscriptBlock::User(prompt.to_owned()));
        state.scroll_from_bottom = 0;
    }

    /// Keep a steering message in the pending area until the Agent reports
    /// that it has appended the message at the next model-turn boundary.
    pub fn queue_steering(&mut self, message: &str) {
        if message.is_empty() {
            return;
        }
        let mut state = self.state.borrow_mut();
        state.steering_queue.push(message.to_owned());
        state.scroll_from_bottom = 0;
    }

    /// Move undelivered steering messages back into the editor. This is used
    /// when an active run is aborted before the Agent can consume its queue.
    pub fn restore_queued_steering(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.steering_queue.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut state.steering_queue).join("\n\n");
        let current = std::mem::take(&mut state.editor);
        state.editor = if current.trim().is_empty() {
            queued
        } else if queued.is_empty() {
            current
        } else {
            format!("{queued}\\n\\n{current}")
        };
        state.editor_cursor = state.editor.len();
    }

    pub fn apply_edit(&mut self, action: EditAction) {
        let mut state = self.state.borrow_mut();
        state.editor_cursor = state.editor_cursor.min(state.editor.len());
        match action {
            EditAction::Char(character) => {
                let cursor = state.editor_cursor;
                state.editor.insert(cursor, character);
                state.editor_cursor = cursor + character.len_utf8();
            }
            EditAction::Paste(text) => {
                let pasted = normalize_paste(&text);
                let cursor = state.editor_cursor;
                state.editor.insert_str(cursor, &pasted);
                state.editor_cursor = cursor + pasted.len();
            }
            EditAction::Backspace => {
                if state.editor_cursor > 0 {
                    let previous = state.editor[..state.editor_cursor]
                        .char_indices()
                        .last()
                        .map_or(0, |(offset, _)| offset);
                    let cursor = state.editor_cursor;
                    state.editor.replace_range(previous..cursor, "");
                    state.editor_cursor = previous;
                }
            }
            EditAction::Delete => {
                if let Some(character) = state.editor[state.editor_cursor..].chars().next() {
                    let end = state.editor_cursor + character.len_utf8();
                    let cursor = state.editor_cursor;
                    state.editor.replace_range(cursor..end, "");
                }
            }
            EditAction::Newline => {
                let cursor = state.editor_cursor;
                state.editor.insert(cursor, '\n');
                state.editor_cursor = cursor + 1;
            }
            EditAction::Left => {
                if state.editor_cursor > 0 {
                    state.editor_cursor = state.editor[..state.editor_cursor]
                        .char_indices()
                        .last()
                        .map_or(0, |(offset, _)| offset);
                }
            }
            EditAction::Right => {
                if let Some(character) = state.editor[state.editor_cursor..].chars().next() {
                    state.editor_cursor += character.len_utf8();
                }
            }
            EditAction::Up | EditAction::Down => {
                let layout = editor_layout(&state.editor, state.editor_cursor, state.size.0);
                let current = &layout.lines[layout.cursor_row];
                let target_row = if matches!(action, EditAction::Up) {
                    layout.cursor_row.saturating_sub(1)
                } else {
                    (layout.cursor_row + 1).min(layout.lines.len().saturating_sub(1))
                };
                let column = editor_column(&state.editor, current, state.editor_cursor);
                state.editor_cursor =
                    editor_offset_at_column(&state.editor, &layout.lines[target_row], column);
            }
            EditAction::Home | EditAction::End => {
                let layout = editor_layout(&state.editor, state.editor_cursor, state.size.0);
                let line = &layout.lines[layout.cursor_row];
                state.editor_cursor = if matches!(action, EditAction::Home) {
                    line.start
                } else {
                    line.end
                };
            }
        }
    }

    /// Complete a unique slash-command prefix at the end of the prompt.
    pub fn complete_slash_command(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor_cursor != state.editor.len() {
            return;
        }
        if let Some(completed) = commands::complete_slash_command(&state.editor) {
            state.editor = completed;
            state.editor_cursor = state.editor.len();
        }
    }

    pub fn set_status(&mut self, status: &str) {
        self.state.borrow_mut().status = status.to_owned();
    }

    pub fn set_status_detail(&mut self, detail: String) {
        self.state.borrow_mut().status_detail = detail;
    }

    pub fn status_detail(&self) -> String {
        self.state.borrow().status_detail.clone()
    }

    pub fn set_run_label(&mut self, label: &str) {
        self.state.borrow_mut().run_label = label.to_owned();
    }

    pub fn set_size(&mut self, columns: u16, rows: u16) {
        *self.size.lock().expect("terminal size mutex poisoned") = (columns, rows);
        let mut state = self.state.borrow_mut();
        state.size = (columns, rows);
        let maximum = max_scroll_from_bottom(&state, columns);
        state.scroll_from_bottom = state.scroll_from_bottom.min(maximum);
    }

    pub fn columns(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").0
    }

    pub fn theme(&self) -> sexy_tui_rs::theme::Theme {
        self.state.borrow().theme.clone()
    }

    pub fn set_theme_config(&mut self, config: Config) {
        self.theme_config = Some(config);
    }

    pub fn theme_config(&self) -> Option<&Config> {
        self.theme_config.as_ref()
    }

    pub fn pending_is_empty(&self) -> bool {
        self.state.borrow().editor.is_empty()
    }

    pub fn pending(&self) -> String {
        self.state.borrow().editor.clone()
    }

    pub fn drain_editor(&mut self) -> String {
        let mut state = self.state.borrow_mut();
        state.editor_cursor = 0;
        std::mem::take(&mut state.editor)
    }

    pub fn scroll(&mut self, direction: i16) {
        let mut state = self.state.borrow_mut();
        let page = usize::from(state.size.1.max(4) / 2);
        if direction < 0 {
            state.scroll_from_bottom = state.scroll_from_bottom.saturating_add(page);
            let maximum = max_scroll_from_bottom(&state, state.size.0);
            state.scroll_from_bottom = state.scroll_from_bottom.min(maximum);
        } else {
            // PageDown is the explicit return-to-live control; reset rather
            // than decrementing an overshot viewport a page at a time.
            state.scroll_from_bottom = 0;
        }
    }

    /// Scroll the transcript in small, trackpad-friendly increments.
    pub fn scroll_lines(&mut self, direction: i16) {
        let mut state = self.state.borrow_mut();
        if direction < 0 {
            state.scroll_from_bottom = state
                .scroll_from_bottom
                .saturating_add(direction.unsigned_abs() as usize);
            let maximum = max_scroll_from_bottom(&state, state.size.0);
            state.scroll_from_bottom = state.scroll_from_bottom.min(maximum);
        } else {
            state.scroll_from_bottom = state.scroll_from_bottom.saturating_sub(direction as usize);
        }
    }

    pub fn show_overlay_text(&mut self, text: String) {
        self.state.borrow_mut().overlay = Some(text);
    }

    pub fn close_overlay(&mut self) {
        self.state.borrow_mut().overlay = None;
    }

    pub fn has_overlay(&self) -> bool {
        self.state.borrow().overlay.is_some()
    }

    pub fn error(&mut self, message: String) {
        self.state.borrow_mut().error = Some(message);
    }

    pub fn clear_error(&mut self) {
        self.state.borrow_mut().error = None;
    }

    pub fn notice(&mut self, message: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        state.push_block(TranscriptBlock::Notice(message.into()));
    }

    pub fn compaction_marker(&mut self, summary: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        state.push_block(TranscriptBlock::Compaction(summary.into()));
    }

    pub fn set_theme(&mut self, theme: sexy_tui_rs::theme::Theme) {
        let mut state = self.state.borrow_mut();
        state.theme = theme;
        state.invalidate_transcript_layout();
    }

    /// Rebuild the visible transcript from the session's active branch.
    pub fn hydrate(&mut self, session: &Session) -> Result<()> {
        let items = hydrate_transcript(session)?;
        let mut state = self.state.borrow_mut();
        state.transcript.clear();
        state.block_revisions.clear();
        state.invalidate_transcript_layout();
        state.steering_queue.clear();
        state.tool_panels.clear();
        state.close_streaming_blocks();
        state.scroll_from_bottom = 0;
        state.last_turn_usage = None;
        state.error = None;
        for item in items {
            match item {
                TranscriptItem::User(text) => state.push_block(TranscriptBlock::User(text)),
                TranscriptItem::Assistant(text) => {
                    state.push_block(TranscriptBlock::Assistant(text))
                }
                TranscriptItem::Reasoning(text) => {
                    state.push_block(TranscriptBlock::Reasoning(text))
                }
                TranscriptItem::ToolCall { id, name, args } => {
                    let index = state.transcript.len();
                    state.push_block(TranscriptBlock::Tool(ToolPanel {
                        id: id.clone(),
                        name,
                        args: args.to_string(),
                        output: String::new(),
                        finished: false,
                        is_error: false,
                    }));
                    state.tool_panels.insert(id, index);
                }
                TranscriptItem::ToolResult { id, text, is_error } => {
                    if let Some(panel) = state.tool_output_mut(&id) {
                        panel.finished = true;
                        panel.is_error = is_error;
                        bounded_append(&mut panel.output, &text);
                    } else {
                        let index = state.transcript.len();
                        state.push_block(TranscriptBlock::Tool(ToolPanel {
                            id: id.clone(),
                            name: "tool result".into(),
                            args: String::new(),
                            output: text,
                            finished: true,
                            is_error,
                        }));
                        state.tool_panels.insert(id, index);
                    }
                }
                TranscriptItem::CompactionMarker { summary_preview } => {
                    state.push_block(TranscriptBlock::Compaction(summary_preview));
                }
            }
        }
        state.invalidate_transcript();
        Ok(())
    }

    /// Human-readable state used by headless unit tests and regression checks.
    #[cfg(test)]
    pub fn debug_snapshot(&self) -> String {
        let state = self.state.borrow();
        let mut result = state.status.clone();
        for block in &state.transcript {
            match block {
                TranscriptBlock::User(text)
                | TranscriptBlock::Assistant(text)
                | TranscriptBlock::Reasoning(text)
                | TranscriptBlock::Notice(text)
                | TranscriptBlock::Compaction(text) => {
                    result.push('\n');
                    result.push_str(text);
                }
                TranscriptBlock::Tool(panel) => {
                    result.push('\n');
                    result.push_str(&panel.name);
                    result.push('\n');
                    result.push_str(&panel.output);
                }
            }
        }
        for message in &state.steering_queue {
            result.push('\n');
            result.push_str("Steering: ");
            result.push_str(message);
        }
        result
    }

    #[cfg(test)]
    pub fn debug_tool_output(&self, id: &ToolCallId) -> Option<String> {
        let state = self.state.borrow();
        let index = *state.tool_panels.get(id)?;
        match state.transcript.get(index) {
            Some(TranscriptBlock::Tool(panel)) => Some(panel.output.clone()),
            _ => None,
        }
    }
}

impl Drop for InteractiveShell {
    fn drop(&mut self) {
        self.stop_renderer();
        force_restore();
    }
}

#[cfg(test)]
struct TestTerminal {
    size: TerminalSize,
}

#[cfg(test)]
impl sexy_tui_rs::Terminal for TestTerminal {
    fn start(&mut self, _on_input: Box<dyn FnMut(&str)>, _on_resize: Box<dyn FnMut()>) {}
    fn stop(&mut self) {}
    fn write(&mut self, _data: &str) {}
    fn columns(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").0
    }
    fn rows(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").1
    }
    fn move_by(&mut self, _lines: i16) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_append_retains_a_tail_and_marks_elision() {
        let mut output = "prefix".repeat(20_000);
        bounded_append(&mut output, "THE-TAIL");
        assert!(output.len() <= MAX_PANEL_BYTES);
        assert!(output.contains("elided"));
        assert!(output.ends_with("THE-TAIL"));
    }

    #[test]
    fn bounded_append_keeps_valid_utf8_at_the_cut_boundary() {
        let mut output = "é".repeat(40_000);
        bounded_append(&mut output, " tail");
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with(" tail"));
    }

    #[test]
    fn plain_wrapping_is_nonempty_for_empty_text() {
        assert_eq!(wrap_plain("", 10), vec![String::new()]);
    }

    #[test]
    fn markdown_transcript_renders_common_headings_lists_code_and_rules() {
        let theme = sexy_tui_rs::theme::Theme::load(
            None,
            sexy_tui_rs::theme::capability::CapabilityTier::Baseline,
        );
        let rendered = markdown_lines(
            "### 🔍 **Read & Search**\n- **`read`** — inspect a file\n\n---",
            &theme,
            80,
        )
        .join("\n");
        for marker in ["###", "**", "`", "---"] {
            assert!(!rendered.contains(marker), "marker {marker:?} leaked");
        }
        assert!(rendered.contains("Read & Search"));
        assert!(rendered.contains("read"));
        assert!(rendered.contains('•'));
        assert!(rendered.contains('─'));
    }

    #[test]
    fn slash_command_menu_lists_commands_and_tab_completes_a_unique_prefix() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Char('/'));
        let rendered = render_shell(&shell.state.borrow(), 120);
        for command in ["/model [id]", "/thinking [level]", "/theme [name]", "/quit"] {
            assert!(rendered.iter().any(|line| line.contains(command)));
        }
        assert!(rendered
            .iter()
            .any(|line| line.contains("Slash commands · Tab completes")));

        for character in "mod".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.complete_slash_command();
        assert_eq!(shell.pending(), "/model ");
    }

    #[test]
    fn submitted_prompts_render_immediately_with_real_context_budget() {
        let mut shell = InteractiveShell::test_shell();
        shell.on_prompt_submitted("second prompt");
        shell.set_status("deepseek-v4-pro");
        shell.set_context_estimate(4_096, 967_232);
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("second prompt"));
        let rendered = render_shell(&shell.state.borrow(), 120);
        assert!(rendered
            .iter()
            .any(|line| line.contains("context ~4096/967232")));
    }

    #[test]
    fn steering_messages_are_queued_above_prompt_and_delivered_as_a_batch() {
        let mut shell = InteractiveShell::test_shell();
        shell.queue_steering("check the docs");
        shell.queue_steering("then run the tests");

        let rendered = render_shell(&shell.state.borrow(), 120);
        let prompt = rendered
            .iter()
            .position(|line| line.contains("prompt"))
            .expect("prompt box");
        let queue = rendered
            .iter()
            .position(|line| line.contains("Queued steering (2)"))
            .expect("steering queue");
        assert!(queue < prompt);
        assert!(rendered.iter().any(|line| line.contains("check the docs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("then run the tests")));

        shell.on_agent_event(&AgentEvent::SteeringDelivered {
            messages: vec!["check the docs".into(), "then run the tests".into()],
        });
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("check the docs"));
        assert!(snapshot.contains("then run the tests"));
        assert!(!render_shell(&shell.state.borrow(), 120)
            .iter()
            .any(|line| line.contains("Queued steering")));
    }

    #[test]
    fn prompt_box_wraps_to_terminal_width_and_grows_then_shrinks() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(24, 10);
        for character in "abcdefghijklmnopqrstuvwxyz0123456789".chars() {
            shell.apply_edit(EditAction::Char(character));
        }

        let rendered = render_shell(&shell.state.borrow(), 24);
        let top = rendered
            .iter()
            .position(|line| line.contains("prompt"))
            .unwrap();
        let bottom = rendered
            .iter()
            .skip(top + 1)
            .position(|line| line.contains('┗'))
            .map(|index| top + index + 1)
            .unwrap();
        assert!(bottom - top + 1 > 3, "long input should grow the editor");
        assert!(
            rendered.len() <= 10,
            "the editor must not exceed the viewport"
        );
        for line in &rendered[top..=bottom] {
            assert_eq!(
                visible_width(line),
                24,
                "prompt border must fit terminal: {line:?}"
            );
        }
        assert!(rendered.iter().any(|line| line.contains(PROMPT_CURSOR)));

        shell.drain_editor();
        let rendered = render_shell(&shell.state.borrow(), 24);
        let top = rendered
            .iter()
            .position(|line| line.contains("prompt"))
            .unwrap();
        let bottom = rendered
            .iter()
            .skip(top + 1)
            .position(|line| line.contains('┗'))
            .map(|index| top + index + 1)
            .unwrap();
        assert_eq!(bottom - top + 1, 3, "empty editor should shrink to one row");
    }

    #[test]
    fn prompt_box_keeps_perfect_geometry_across_viewport_sizes() {
        for (width, height) in [
            (1, 5),
            (2, 5),
            (3, 5),
            (4, 5),
            (8, 5),
            (12, 7),
            (24, 10),
            (80, 24),
            (173, 61),
        ] {
            let mut shell = InteractiveShell::test_shell();
            shell.set_size(width, height);
            for character in "a long prompt that must wrap cleanly at every width".chars() {
                shell.apply_edit(EditAction::Char(character));
            }

            let rendered = render_shell(&shell.state.borrow(), width);
            let top = rendered
                .iter()
                .position(|line| line.contains('┏'))
                .expect("prompt top border");
            let bottom = rendered
                .iter()
                .skip(top + 1)
                .position(|line| line.contains('┗'))
                .map(|index| top + index + 1)
                .expect("prompt bottom border");
            assert!(rendered.len() <= usize::from(height));
            for line in &rendered[top..=bottom] {
                assert_eq!(visible_width(line), usize::from(width), "{width}x{height}");
            }
            assert!(rendered[top].contains('┏'));
            assert!(rendered[bottom].contains('┗'));
            if width >= 2 {
                assert!(rendered[top].contains('┓'));
                assert!(rendered[bottom].contains('┛'));
            }
        }
    }

    #[test]
    fn bracketed_paste_preserves_multiline_editor_text_without_submitting() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Char('a'));
        shell.apply_edit(EditAction::Paste("b\r\nc\rd".into()));
        assert_eq!(shell.pending(), "ab\nc\nd");
        assert_eq!(shell.state.borrow().editor_cursor, "ab\nc\nd".len());
        let rendered = render_shell(&shell.state.borrow(), 120);
        assert!(rendered.iter().any(|line| line.contains("ab")));
        assert!(rendered.iter().any(|line| line.contains("c")));
    }

    #[test]
    fn prompt_bar_cursor_tracks_insertions_and_cursor_motion() {
        let mut shell = InteractiveShell::test_shell();
        for character in "abcdef".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.apply_edit(EditAction::Left);
        shell.apply_edit(EditAction::Left);
        shell.apply_edit(EditAction::Char('X'));
        assert_eq!(shell.state.borrow().editor, "abcdXef");

        let rendered = render_shell(&shell.state.borrow(), 120);
        let line = rendered
            .iter()
            .find(|line| line.contains(PROMPT_CURSOR))
            .unwrap();
        assert!(line.find("abcdX").unwrap() < line.find(PROMPT_CURSOR).unwrap());
        assert!(line.find(PROMPT_CURSOR).unwrap() < line.find("ef").unwrap());
    }

    #[test]
    fn scrolling_reuses_the_cached_transcript_layout() {
        let mut shell = InteractiveShell::test_shell();
        for number in 0..200 {
            shell.notice(format!("notice {number}"));
        }
        let _ = render_shell(&shell.state.borrow(), 120);
        let first_generation = shell.state.borrow().transcript_cache.borrow().generation;

        shell.scroll_lines(-3);
        let _ = render_shell(&shell.state.borrow(), 120);
        assert_eq!(
            shell.state.borrow().transcript_cache.borrow().generation,
            first_generation,
            "scrolling must only slice the existing layout"
        );

        shell.notice("new transcript block");
        let _ = render_shell(&shell.state.borrow(), 120);
        assert_eq!(
            shell.state.borrow().transcript_cache.borrow().generation,
            first_generation + 1
        );
    }

    #[test]
    fn exact_viewport_has_no_hidden_scroll_range() {
        assert_eq!(max_scroll_for_available(10, 10), 0);
        assert_eq!(max_scroll_for_available(11, 10), 2);
    }

    #[test]
    fn trackpad_scroll_moves_the_transcript_in_small_clamped_steps() {
        let mut shell = InteractiveShell::test_shell();
        for number in 0..30 {
            shell.notice(format!("notice {number}"));
        }
        shell.scroll_lines(-3);
        assert!(shell.state.borrow().scroll_from_bottom > 0);
        let rendered = render_shell(&shell.state.borrow(), 120);
        assert!(rendered.iter().any(|line| line.contains("notice")));
        shell.scroll_lines(3);
        assert_eq!(shell.state.borrow().scroll_from_bottom, 0);
    }

    #[test]
    fn overscrolled_viewport_clamps_to_available_transcript() {
        let mut shell = InteractiveShell::test_shell();
        shell.on_prompt_submitted("visible prompt");
        shell.state.borrow_mut().scroll_from_bottom = 9_999;
        let rendered = render_shell(&shell.state.borrow(), 120);
        assert!(rendered.iter().any(|line| line.contains("visible prompt")));
        shell.scroll(1);
        assert_eq!(shell.state.borrow().scroll_from_bottom, 0);
    }

    #[test]
    fn scripted_agent_events_map_to_distinct_transcript_and_tool_state() {
        use ygg_agent::{EntryId, FinishReason, ToolOutput};
        use ygg_ai::{AssistantMessage, AssistantPart, ModelId, Protocol};

        let mut shell = InteractiveShell::test_shell();
        let id = ToolCallId("call-1".into());
        let events = vec![
            AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: "considering".into(),
            },
            AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: "answer".into(),
            },
            AgentEvent::ToolStarted {
                id: id.clone(),
                name: "read".into(),
                args: serde_json::json!({"path": "src/lib.rs"}),
            },
            AgentEvent::ToolProgress {
                id: id.clone(),
                progress: ToolProgress::Status("reading".into()),
            },
            AgentEvent::ToolFinished {
                id: id.clone(),
                result: Ok(ToolOutput::new("contents")),
            },
            AgentEvent::TurnFinished {
                message: AssistantMessage {
                    content: vec![AssistantPart::Text("answer".into())],
                    model: ModelId("m".into()),
                    protocol: Protocol::OpenAiChat,
                },
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    total_tokens: 15,
                    ..Usage::default()
                },
            },
            AgentEvent::RunFinished {
                head: EntryId("003".into()),
                reason: FinishReason::Completed,
            },
        ];
        for event in &events {
            shell.on_agent_event(event);
        }
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("considering"));
        assert!(snapshot.contains("answer"));
        assert!(snapshot.contains("read"));
        assert!(shell.debug_tool_output(&id).unwrap().contains("reading"));
    }
}
