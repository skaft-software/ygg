#![allow(missing_docs)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use sexy_tui_rs::widgets::Markdown;
use sexy_tui_rs::{visible_width, Component, TUI};
use unicode_width::UnicodeWidthChar;
use ygg_agent::{AgentEvent, OutputChannel, Session, ToolProgress};
use ygg_ai::{ToolCallId, Usage};

use crate::config::Config;
use crate::hydrate::{hydrate_transcript, TranscriptItem};
use crate::tui::keymap::EditAction;
use crate::tui::terminal::{force_restore, YggTerminal};
use crate::tui::theme::markdown_theme;

const MAX_PANEL_BYTES: usize = 64 * 1024;
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

#[derive(Clone, Debug, Default)]
struct ShellState {
    theme: sexy_tui_rs::theme::Theme,
    transcript: Vec<TranscriptBlock>,
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
    fn append_text_block(&mut self, channel: OutputChannel, text: &str) {
        let active = match channel {
            OutputChannel::Text => &mut self.active_text,
            OutputChannel::Reasoning => &mut self.active_reasoning,
        };
        if let Some(index) = *active {
            match self.transcript.get_mut(index) {
                Some(TranscriptBlock::Assistant(existing)) if channel == OutputChannel::Text => {
                    existing.push_str(text);
                    return;
                }
                Some(TranscriptBlock::Reasoning(existing))
                    if channel == OutputChannel::Reasoning =>
                {
                    existing.push_str(text);
                    return;
                }
                _ => *active = None,
            }
        }

        let index = self.transcript.len();
        self.transcript.push(match channel {
            OutputChannel::Text => TranscriptBlock::Assistant(text.to_owned()),
            OutputChannel::Reasoning => TranscriptBlock::Reasoning(text.to_owned()),
        });
        *active = Some(index);
    }

    fn close_streaming_blocks(&mut self) {
        self.active_text = None;
        self.active_reasoning = None;
    }

    fn tool_output_mut(&mut self, id: &ToolCallId) -> Option<&mut ToolPanel> {
        let index = *self.tool_panels.get(id)?;
        match self.transcript.get_mut(index) {
            Some(TranscriptBlock::Tool(panel)) => Some(panel),
            _ => None,
        }
    }
}

/// The retained root component. It reads the shell state at render time, while
/// `InteractiveShell` mutates that same state in response to events.
struct ShellComponent {
    state: Rc<RefCell<ShellState>>,
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
    let markdown = Markdown::new(text, 1, 0, Some(markdown_theme(theme)));
    markdown.render(width.max(1))
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

fn transcript_lines(state: &ShellState, width: u16) -> Vec<String> {
    let mut lines = Vec::new();
    for block in &state.transcript {
        lines.extend(render_block(block, &state.theme, width));
    }
    lines
}

fn max_scroll_for_available(transcript_len: usize, available: usize) -> usize {
    // A scrolled viewport reserves one line for its return-to-live indicator.
    if available <= 1 {
        0
    } else {
        transcript_len.saturating_sub(available - 1)
    }
}

fn max_scroll_from_bottom(state: &ShellState, width: u16) -> usize {
    if state.overlay.is_some() {
        return 0;
    }
    let rows = usize::from(state.size.1.max(5));
    let header_lines = 1 + usize::from(state.error.is_some());
    let prompt_max_rows = rows.saturating_sub(header_lines).saturating_sub(3).max(1);
    let prompt_height = render_prompt_box(state, width, prompt_max_rows).len();
    let available = rows
        .saturating_sub(header_lines)
        .saturating_sub(prompt_height);
    max_scroll_for_available(transcript_lines(state, width).len(), available)
}

fn render_shell(state: &ShellState, width: u16) -> Vec<String> {
    let rows = usize::from(state.size.1.max(5));
    let mut lines = Vec::new();

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
    lines.push(state.theme.bold(&state.theme.fg("accent", &status)));

    if let Some(error) = &state.error {
        lines.push(state.theme.fg("error", &format!("Error: {error}")));
    }

    if let Some(overlay) = &state.overlay {
        lines.push(state.theme.bold(&state.theme.fg("accent", "─ overlay ─")));
        for line in wrap_plain(overlay, usize::from(width.saturating_sub(2))) {
            lines.push(format!(" {line}"));
        }
        lines.push(state.theme.dim("Press Esc or any printable key to close."));
        let prompt_max_rows = rows.saturating_sub(lines.len()).saturating_sub(2).max(1);
        lines.extend(render_prompt_box(state, width, prompt_max_rows));
        return lines;
    }

    let prompt_max_rows = rows.saturating_sub(lines.len()).saturating_sub(3).max(1);
    let prompt = render_prompt_box(state, width, prompt_max_rows);
    let available = rows
        .saturating_sub(lines.len())
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
    lines.extend(prompt);
    lines
}

/// Full-screen terminal shell. It owns all terminal I/O and no Agent state.
pub struct InteractiveShell {
    tui: TUI<'static>,
    state: Rc<RefCell<ShellState>>,
    size: Rc<Cell<(u16, u16)>>,
    theme_config: Option<Config>,
}

impl InteractiveShell {
    /// Enter alternate-screen raw mode and start the retained TUI.
    pub fn enter(theme: sexy_tui_rs::theme::Theme, size: Rc<Cell<(u16, u16)>>) -> Result<Self> {
        let terminal = YggTerminal::enter_with_size(size.clone())?;
        let state = Rc::new(RefCell::new(ShellState {
            theme,
            size: size.get(),
            ..ShellState::default()
        }));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(ShellComponent {
            state: state.clone(),
        }));
        tui.start();
        Ok(Self {
            tui,
            state,
            size,
            theme_config: None,
        })
    }

    #[cfg(test)]
    pub fn test_shell() -> Self {
        let size = Rc::new(Cell::new((120, 40)));
        let state = Rc::new(RefCell::new(ShellState {
            theme: sexy_tui_rs::theme::Theme::load(
                None,
                sexy_tui_rs::theme::capability::CapabilityTier::Baseline,
            ),
            size: size.get(),
            ..ShellState::default()
        }));
        let mut tui = TUI::new(Box::new(TestTerminal { size: size.clone() }));
        tui.add_child(Box::new(ShellComponent {
            state: state.clone(),
        }));
        tui.start();
        Self {
            tui,
            state,
            size,
            theme_config: None,
        }
    }

    /// Stop rendering and restore the process terminal.
    pub fn leave(mut self) {
        self.tui.stop();
        force_restore();
    }

    /// Render the updated retained view.
    pub fn render(&mut self) {
        self.tui.request_render();
    }

    pub fn on_agent_event(&mut self, event: &AgentEvent) {
        let mut state = self.state.borrow_mut();
        match event {
            AgentEvent::OutputDelta { channel, text } => state.append_text_block(*channel, text),
            AgentEvent::ToolStarted { id, name, args } => {
                state.close_streaming_blocks();
                let index = state.transcript.len();
                state.transcript.push(TranscriptBlock::Tool(ToolPanel {
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
        state
            .transcript
            .push(TranscriptBlock::User(prompt.to_owned()));
        state.scroll_from_bottom = 0;
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
        self.size.set((columns, rows));
        let mut state = self.state.borrow_mut();
        state.size = (columns, rows);
        let maximum = max_scroll_from_bottom(&state, columns);
        state.scroll_from_bottom = state.scroll_from_bottom.min(maximum);
    }

    pub fn columns(&self) -> u16 {
        self.size.get().0
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
        self.state
            .borrow_mut()
            .transcript
            .push(TranscriptBlock::Notice(message.into()));
    }

    pub fn compaction_marker(&mut self, summary: impl Into<String>) {
        self.state
            .borrow_mut()
            .transcript
            .push(TranscriptBlock::Compaction(summary.into()));
    }

    pub fn set_theme(&mut self, theme: sexy_tui_rs::theme::Theme) {
        self.state.borrow_mut().theme = theme;
    }

    /// Rebuild the visible transcript from the session's active branch.
    pub fn hydrate(&mut self, session: &Session) -> Result<()> {
        let items = hydrate_transcript(session)?;
        let mut state = self.state.borrow_mut();
        state.transcript.clear();
        state.tool_panels.clear();
        state.close_streaming_blocks();
        for item in items {
            match item {
                TranscriptItem::User(text) => state.transcript.push(TranscriptBlock::User(text)),
                TranscriptItem::Assistant(text) => {
                    state.transcript.push(TranscriptBlock::Assistant(text))
                }
                TranscriptItem::Reasoning(text) => {
                    state.transcript.push(TranscriptBlock::Reasoning(text))
                }
                TranscriptItem::ToolCall { id, name, args } => {
                    let index = state.transcript.len();
                    state.transcript.push(TranscriptBlock::Tool(ToolPanel {
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
                        state.transcript.push(TranscriptBlock::Tool(ToolPanel {
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
                    state
                        .transcript
                        .push(TranscriptBlock::Compaction(summary_preview));
                }
            }
        }
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

#[cfg(test)]
struct TestTerminal {
    size: Rc<Cell<(u16, u16)>>,
}

#[cfg(test)]
impl sexy_tui_rs::Terminal for TestTerminal {
    fn start(&mut self, _on_input: Box<dyn FnMut(&str)>, _on_resize: Box<dyn FnMut()>) {}
    fn stop(&mut self) {}
    fn write(&mut self, _data: &str) {}
    fn columns(&self) -> u16 {
        self.size.get().0
    }
    fn rows(&self) -> u16 {
        self.size.get().1
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
