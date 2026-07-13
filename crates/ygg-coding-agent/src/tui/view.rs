#![allow(missing_docs)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use sexy_tui_rs::widgets::Markdown;
use sexy_tui_rs::{Component, TUI};
use ygg_agent::{AgentEvent, OutputChannel, Session, ToolProgress};
use ygg_ai::{ToolCallId, Usage};

use crate::config::Config;
use crate::hydrate::{hydrate_transcript, TranscriptItem};
use crate::tui::keymap::EditAction;
use crate::tui::terminal::{force_restore, YggTerminal};
use crate::tui::theme::markdown_theme;

const MAX_PANEL_BYTES: usize = 64 * 1024;
const ELISION_MARKER: &str = "\n… older tool output elided …\n";

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
    status: String,
    status_detail: String,
    error: Option<String>,
    overlay: Option<String>,
    tool_panels: HashMap<ToolCallId, usize>,
    active_text: Option<usize>,
    active_reasoning: Option<usize>,
    scroll_from_bottom: usize,
    usage: Option<(u64, u64)>,
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
    if let Some((estimate, budget)) = state.usage {
        if !status.is_empty() {
            status.push_str("  ·  ");
        }
        status.push_str(&format!("context ~{estimate}/{budget}"));
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
    } else {
        let mut transcript_lines = Vec::new();
        for block in &state.transcript {
            transcript_lines.extend(render_block(block, &state.theme, width));
        }
        let editor_lines = state.editor.lines().count().max(1) + 2;
        let available = rows
            .saturating_sub(lines.len())
            .saturating_sub(editor_lines)
            .max(1);
        let end = transcript_lines
            .len()
            .saturating_sub(state.scroll_from_bottom);
        let start = end.saturating_sub(available);
        lines.extend(transcript_lines[start..end].iter().cloned());
        if state.scroll_from_bottom > 0 {
            lines.push(
                state
                    .theme
                    .dim("↑ scrolled transcript (PageDown returns to live)"),
            );
        }
    }

    lines.push(state.theme.fg("accent", "┌─ prompt ─"));
    if state.editor.is_empty() {
        lines.push(state.theme.dim("│ Type a prompt or /help"));
    } else {
        for line in state.editor.lines() {
            lines.push(format!("│ {line}"));
        }
    }
    lines.push(state.theme.fg("accent", "└──────────"));
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
                state.usage = Some((
                    usage.total_tokens.saturating_sub(usage.output_tokens),
                    usage.total_tokens,
                ));
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
        state.usage = Some((
            usage.total_tokens.saturating_sub(usage.output_tokens),
            usage.total_tokens,
        ));
    }

    pub fn apply_edit(&mut self, action: EditAction) {
        let mut state = self.state.borrow_mut();
        match action {
            EditAction::Char(character) => state.editor.push(character),
            EditAction::Backspace => {
                state.editor.pop();
            }
            EditAction::Newline => state.editor.push('\n'),
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
        self.state.borrow_mut().size = (columns, rows);
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
        state.editor.drain(..).collect()
    }

    pub fn scroll(&mut self, direction: i16) {
        let mut state = self.state.borrow_mut();
        let page = usize::from(state.size.1.max(4) / 2);
        if direction < 0 {
            state.scroll_from_bottom = state.scroll_from_bottom.saturating_add(page);
        } else {
            state.scroll_from_bottom = state.scroll_from_bottom.saturating_sub(page);
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
}
