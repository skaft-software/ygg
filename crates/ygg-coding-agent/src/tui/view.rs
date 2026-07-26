#![allow(missing_docs)]

use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::io::{IsTerminal, Write as IoWrite};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sexy_tui_rs::{
    parse_markdown, strip_terminal_sequences, visible_width, wrap_text_with_ansi, Block, Color,
    CommitCursor, CommitPosition, Component, DiffRenderOptions, FrameUpdate, Inline, PinnedFrame,
    RichRenderer, StreamingMarkdown, StreamingRenderCache, UnifiedDiff, CURSOR_MARKER, TUI,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;
use ygg_agent::{AgentEvent, EntryValue, OutputChannel, Session, ToolProgress};
use ygg_ai::{ModalitySet, Model, ModelId, ToolCallId, Usage};

use crate::commands;
use crate::config::Config;
use crate::hydrate::{hydrate_transcript_tail, TranscriptItem};
use crate::presentation::{
    format_duration, summarize_tool, summarize_tool_with_workspace, tool_failure_reason,
    tool_result_is_failure, ModelDisplayMetadata, PriceDisplay, RunId, RunOutcome, RunPhase,
    RunTracker, ToolDisplay,
};
use crate::tui::composer::{self, ComposedInput};
use crate::tui::keymap::{EditAction, SlashMenuAction};
use crate::tui::terminal::{force_restore, TerminalSize, YggTerminal};
use crate::tui::theme::{
    ModelLab, ThemeDensity, ThemeSurfaceAlign, ThemeSurfaceChrome, ThemeSurfaceHeading,
    ThemeSurfaceWidth, YggTheme,
};

use self::transcript_history::{
    materialize_deferred_session_history, DeferredSessionHistory, NextTranscriptCommitId,
};

const MAX_PANEL_BYTES: usize = 64 * 1024;
const MAX_OUTCOME_DETAIL_BYTES: usize = 4 * 1024;
const MAX_EXTENSION_TOOL_RENDER_SEGMENTS: usize = 128;
/// Default render cap — roughly 60 fps. Decorative shimmer uses the separate,
/// deliberately slower cap below; input and streamed output stay on this path.
const RENDER_INTERVAL: Duration = Duration::from_millis(16);
/// Modern terminals get a restrained 20 FPS shimmer. The retained renderer
/// emits only changed border cells, so this leaves input and streaming work
/// well ahead of decorative frames.
const ANIMATION_RENDER_INTERVAL: Duration = Duration::from_millis(50);
/// Wake near the next eligible animation frame; input commands still preempt
/// this wait through the bounded render channel.
const ANIMATION_POLL_TIMEOUT: Duration = Duration::from_millis(45);
/// Tool activity dots share one restrained 900 ms cycle. Toggling the cell
/// ourselves works in terminals that intentionally ignore SGR blink.
const EVENT_DOT_TOGGLE_INTERVAL: Duration = Duration::from_millis(450);
/// Resize events are normally delivered by crossterm, but polling while idle
/// also catches terminal-manager resizes that do not emit an event.
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ELISION_MARKER: &str = "\n… older tool output elided …\n";
/// A compact tool row keeps enough terminal context to recognize a result
/// while preventing noisy output from swallowing the transcript.
const COMPACT_EXEC_OUTPUT_LINES: usize = 5;

/// Strip complete terminal sequences, replace remaining controls (except line
/// feeds), and normalize CRLF so raw tool/provider output cannot execute
/// terminal commands or leave color-protocol debris in the transcript.
///
/// NULL becomes `␀`, BEL becomes `␇`, and other C0/C1 controls become `·`.
pub(crate) fn sanitize_for_terminal(raw: &str) -> String {
    // Command output often carries color, OSC hyperlinks, or a charset reset.
    // Remove complete terminal sequences as units: exposing only their ESC
    // byte leaves artifacts such as `[32m` and `(B` in the transcript.
    let stripped;
    let raw = if raw
        .chars()
        .any(|character| character == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&character))
    {
        stripped = strip_terminal_sequences(raw);
        stripped.as_str()
    } else {
        raw
    };

    // Fast path: most tool output is clean text.
    if raw
        .chars()
        .all(|character| !character.is_control() || character == '\n')
    {
        return raw.to_owned();
    }

    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => out.push('\n'),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push('\n');
            }
            '\r' => out.push('␍'),
            '\t' => out.push_str("    "),
            '\x00' => out.push('␀'),
            '\x07' => out.push('␇'),
            '\x1b' => {
                // If the next char starts a CSI sequence (ESC [), swallow
                // until the final byte so the terminal never sees a live
                // escape. Render the whole thing as visible text.
                out.push('␛');
                if chars.peek() == Some(&'[') {
                    out.push('[');
                    chars.next();
                    // Consume parameter bytes (0x30-0x3F) and intermediate
                    // bytes (0x20-0x2F), then the final byte (0x40-0x7E).
                    while let Some(&next) = chars.peek() {
                        let b = next as u32;
                        if (0x30..=0x3F).contains(&b) || (0x20..=0x2F).contains(&b) {
                            out.push(next);
                            chars.next();
                        } else if (0x40..=0x7E).contains(&b) {
                            out.push(next);
                            chars.next();
                            break;
                        } else {
                            break;
                        }
                    }
                }
            }
            c if c.is_control() => out.push('·'),
            other => out.push(other),
        }
    }
    out
}

/// Normalize a process-supplied one-line semantic contribution at the TUI
/// boundary. Extension text never gets to smuggle terminal controls or extra
/// physical rows into persistent chrome, and an invalid role simply falls
/// back to the conventional surface role.
fn sanitize_extension_surface(
    contribution: Option<(String, Option<String>)>,
) -> Option<(String, Option<String>)> {
    contribution.and_then(|(text, role)| {
        let text = sanitize_for_terminal(&text).replace('\n', " ");
        let text = text.trim().to_owned();
        if text.is_empty() {
            return None;
        }
        let role = role.and_then(|role| {
            let role = role.trim();
            (role.len() <= 96
                && !role.is_empty()
                && !role.starts_with('.')
                && !role.ends_with('.')
                && role
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
            .then(|| role.to_owned())
        });
        Some((text, role))
    })
}

fn bounded_plain_prefix(mut text: String, byte_budget: usize) -> String {
    if text.len() <= byte_budget {
        return text;
    }
    let mut end = byte_budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn sanitize_extension_tool_render_segments(
    segments: &[ygg_agent::extension_process::ToolRenderSegment],
) -> Vec<ygg_agent::extension_process::ToolRenderSegment> {
    let mut remaining = MAX_PANEL_BYTES;
    let mut sanitized = Vec::new();
    for segment in segments.iter().take(MAX_EXTENSION_TOOL_RENDER_SEGMENTS) {
        if remaining == 0 {
            break;
        }
        let text = bounded_plain_prefix(sanitize_for_terminal(&segment.text), remaining);
        remaining = remaining.saturating_sub(text.len());
        if text.is_empty() {
            continue;
        }
        let style_role = segment.style_role.as_deref().and_then(|role| {
            let role = sanitize_for_terminal(role).replace('\n', " ");
            let role = bounded_plain_prefix(role.trim().to_owned(), 128);
            (!role.is_empty()).then_some(role)
        });
        sanitized.push(ygg_agent::extension_process::ToolRenderSegment { text, style_role });
    }
    sanitized
}

fn visualize_editor_controls(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\n' => out.push('\n'),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push('\n');
            }
            '\r' => out.push('␍'),
            '\t' => out.push_str("    "),
            '\x00' => out.push('␀'),
            '\x07' => out.push('␇'),
            '\x1b' => out.push('␛'),
            control if control.is_control() => out.push('·'),
            visible => out.push(visible),
        }
    }
    out
}

pub(crate) fn sanitized_editor(raw: &str, cursor: usize) -> (String, usize) {
    let mut cursor = cursor.min(raw.len());
    while cursor > 0 && !raw.is_char_boundary(cursor) {
        cursor -= 1;
    }
    // Composer input remains authoritative and editable. Unlike command logs,
    // controls are visualized rather than removed so cursor offsets can map to
    // every source byte without executing it.
    let before = visualize_editor_controls(&raw[..cursor]);
    let safe_cursor = before.len();
    let after = visualize_editor_controls(&raw[cursor..]);
    (before + &after, safe_cursor)
}

/// Append display output while retaining only the newest 64 KiB.
pub fn bounded_append(existing: &mut String, additional: &str) {
    let safe = sanitize_for_terminal(additional);
    if existing.len().saturating_add(safe.len()) <= MAX_PANEL_BYTES {
        existing.push_str(&safe);
        return;
    }

    // Retain the newest bytes in place. The old implementation allocated a
    // second combined String on every overflow event, which is a hot path for
    // noisy tools; reserve once and shift only the retained tail.
    let tail_budget = MAX_PANEL_BYTES.saturating_sub(ELISION_MARKER.len());
    let mut additional_start = if safe.len() >= tail_budget {
        safe.len() - tail_budget
    } else {
        0
    };
    while additional_start < safe.len() && !safe.is_char_boundary(additional_start) {
        additional_start += 1;
    }
    let existing_budget = tail_budget.saturating_sub(safe.len() - additional_start);
    let mut existing_start = existing.len().saturating_sub(existing_budget);
    while existing_start < existing.len() && !existing.is_char_boundary(existing_start) {
        existing_start += 1;
    }

    let final_len = ELISION_MARKER.len()
        + existing.len().saturating_sub(existing_start)
        + safe.len().saturating_sub(additional_start);
    existing.replace_range(..existing_start, "");
    existing.reserve(final_len.saturating_sub(existing.len()));
    existing.insert_str(0, ELISION_MARKER);
    existing.push_str(&safe[additional_start..]);
}

/// Output from an interactive `!` shell command, stored as a collapsible
/// block so the transcript is not overwhelmed by long command output.
#[derive(Clone, Debug)]
struct ShellOutput {
    id: String,
    command: String,
    output: String,
    exit_code: i32,
    /// True while the child process is still running.
    running: bool,
    /// Current spinner frame character (Unicode braille).
    spinner: String,
}

#[derive(Clone, Debug)]
struct CompactionBlock {
    /// Concise durable-event annotation shown while collapsed.
    label: String,
    /// Complete model-produced summary retained for inline inspection.
    summary: String,
    expanded: bool,
}

enum TranscriptBlock {
    User {
        text: String,
        /// Model that was active when this prompt was submitted, so the
        /// prompt card can be rendered in that model's accent colour.
        model_lab: Option<ModelLab>,
        /// Exact sRGB row colour captured when this prompt was submitted.
        /// This value is immutable presentation history, not a theme token.
        prompt_color: Option<String>,
        /// Whether this prompt is represented in the durable Session.
        persisted: bool,
    },
    Assistant(Box<AssistantBlock>),
    Reasoning(Box<AssistantBlock>),
    Tool(Box<ToolPanel>),
    Shell(Box<ShellOutput>),
    Outcome(RunOutcome),
    Notice(String),
    Compaction(Box<CompactionBlock>),
}

fn reasoning_markdown_projection(source: &str) -> String {
    // OpenAI-style reasoning summaries can concatenate independently bolded
    // sections without whitespace: `**Plan****Verify**`. CommonMark treats the
    // middle four asterisks as literal text inside one strong span. Insert a
    // display-only block boundary while retaining `AssistantBlock::text` as the
    // exact provider/session source.
    source
        .replace("****", "**\n\n**")
        .replace("____", "__\n\n__")
}

fn append_reasoning_inline_text(inlines: &[Inline], output: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) | Inline::Raw(text) => output.push_str(text),
            Inline::Styled(span) => append_reasoning_inline_text(&span.content, output),
            Inline::Role { content, .. }
            | Inline::Status { content, .. }
            | Inline::Emphasis(content)
            | Inline::Strong(content)
            | Inline::Strikethrough(content) => append_reasoning_inline_text(content, output),
            Inline::Link { label, .. } => append_reasoning_inline_text(label, output),
            Inline::SoftBreak | Inline::HardBreak => output.push(' '),
        }
    }
}

fn normalized_reasoning_heading(inlines: &[Inline]) -> Option<String> {
    let mut heading = String::new();
    append_reasoning_inline_text(inlines, &mut heading);
    let heading = sanitize_for_terminal(&heading);
    let heading = heading.split_whitespace().collect::<Vec<_>>().join(" ");
    (!heading.is_empty()).then_some(heading)
}

fn reasoning_heading_from_block(block: &Block) -> Option<String> {
    match block {
        Block::Heading { content, .. } => normalized_reasoning_heading(content),
        Block::Paragraph(content) => {
            let mut meaningful = content.iter().filter(|inline| {
                !matches!(inline, Inline::Text(text) | Inline::Raw(text) if text.trim().is_empty())
            });
            let Inline::Strong(heading) = meaningful.next()? else {
                return None;
            };
            meaningful
                .next()
                .is_none()
                .then(|| normalized_reasoning_heading(heading))
                .flatten()
        }
        _ => None,
    }
}

fn reasoning_delimiter_crosses_chunk_boundary(previous: &str, next: &str) -> bool {
    ['*', '_'].into_iter().any(|marker| {
        let trailing = previous
            .chars()
            .rev()
            .take_while(|character| *character == marker)
            .take(3)
            .count();
        let leading = next
            .chars()
            .take_while(|character| *character == marker)
            .take(3)
            .count();
        trailing > 0 && leading > 0 && trailing + leading >= 4
    })
}

#[derive(Clone, Debug)]
struct AssistantBlock {
    text: String,
    markdown: StreamingMarkdown,
    layout: RefCell<StreamingRenderCache>,
    /// Model that generated this block, for stable accent colour across
    /// model switches mid-session.
    model_lab: Option<crate::tui::theme::ModelLab>,
    finished: bool,
    /// Reasoning is retained verbatim but stays out of the mutable native
    /// scrollback tail until the user explicitly asks to inspect it.
    reasoning_expanded: bool,
    /// First streamed reasoning delta, used to freeze elapsed timing when the
    /// block closes.
    reasoning_started_at: Option<Instant>,
    /// Frozen reasoning duration after the block closes.
    reasoning_elapsed: Option<Duration>,
    /// Latest explicit ATX or standalone-bold heading emitted by the model.
    reasoning_heading: Option<String>,
    /// Committed semantic blocks already inspected for reasoning headings.
    reasoning_heading_committed_blocks: usize,
    /// Only the newest reasoning block advertises the global disclosure key.
    /// Older repeated hints become noise once a newer thinking event exists.
    show_reasoning_hint: bool,
}

impl AssistantBlock {
    fn streaming(text: &str) -> Self {
        let mut markdown = StreamingMarkdown::new();
        markdown.push_str(text);
        Self {
            text: text.to_owned(),
            markdown,
            layout: RefCell::new(StreamingRenderCache::default()),
            model_lab: None,
            finished: false,
            reasoning_expanded: false,
            reasoning_started_at: None,
            reasoning_elapsed: None,
            reasoning_heading: None,
            reasoning_heading_committed_blocks: 0,
            show_reasoning_hint: true,
        }
    }

    fn finalized(text: String) -> Self {
        let mut block = Self::streaming(&text);
        block.finish();
        block.text = text;
        block
    }

    fn streaming_reasoning(text: &str) -> Self {
        let projection = reasoning_markdown_projection(text);
        let mut block = Self::streaming(&projection);
        block.text = text.to_owned();
        block.reasoning_started_at = Some(Instant::now());
        block.refresh_reasoning_heading();
        block
    }

    fn finalized_reasoning(text: String) -> Self {
        let mut block = Self::streaming_reasoning(&text);
        // Hydrated sessions preserve reasoning text but do not currently store
        // provider-phase timing, so do not invent a duration on replay.
        block.reasoning_started_at = None;
        block.finish_reasoning();
        block
    }

    fn with_model_lab(mut self, lab: Option<crate::tui::theme::ModelLab>) -> Self {
        self.model_lab = lab;
        self
    }

    fn append(&mut self, text: &str) {
        self.text.push_str(text);
        self.markdown.push_str(text);
    }

    fn append_reasoning(&mut self, text: &str) {
        let repairs_boundary = reasoning_delimiter_crosses_chunk_boundary(&self.text, text);
        self.text.push_str(text);
        if repairs_boundary {
            // This is rare (normally one boundary per provider summary
            // heading), so repair the cross-delta delimiter only when needed.
            self.markdown =
                StreamingMarkdown::from_text(&reasoning_markdown_projection(&self.text));
            self.reasoning_heading_committed_blocks = 0;
            self.invalidate_layout();
        } else {
            // Preserve the parser's committed prefix for ordinary token deltas.
            // Rebuilding here made verbose reasoning quadratic. Most deltas do
            // not contain the provider-specific adjacency at all, so avoid an
            // allocation on that hot path too.
            if text.contains("****") || text.contains("____") {
                self.markdown.push_str(&reasoning_markdown_projection(text));
            } else {
                self.markdown.push_str(text);
            }
        }
        self.refresh_reasoning_heading();
    }

    fn refresh_reasoning_heading(&mut self) {
        let (committed_blocks, heading) = {
            let committed = &self.markdown.committed().blocks;
            let start = self.reasoning_heading_committed_blocks.min(committed.len());
            let mut heading = committed[start..]
                .iter()
                .filter_map(reasoning_heading_from_block)
                .next_back();
            if let Some(preview_heading) = self
                .markdown
                .preview()
                .blocks
                .iter()
                .filter_map(reasoning_heading_from_block)
                .next_back()
            {
                heading = Some(preview_heading);
            }
            (committed.len(), heading)
        };
        self.reasoning_heading_committed_blocks = committed_blocks;
        if let Some(heading) = heading {
            self.reasoning_heading = Some(heading);
        }
    }

    fn finish_reasoning(&mut self) {
        // A four-character emphasis boundary can straddle provider deltas. Fix
        // that rare boundary once at completion rather than reparsing the full
        // trace after every delta.
        let projection = reasoning_markdown_projection(&self.text);
        if self.markdown.raw_text() != projection {
            self.markdown = StreamingMarkdown::from_text(&projection);
            self.reasoning_heading_committed_blocks = 0;
            self.invalidate_layout();
        }
        if self.reasoning_elapsed.is_none() {
            self.reasoning_elapsed = self.reasoning_started_at.map(|started| started.elapsed());
        }
        self.finish();
        self.refresh_reasoning_heading();
    }

    fn finish(&mut self) {
        self.markdown.finish();
        self.finished = true;
    }

    fn invalidate_layout(&self) {
        *self.layout.borrow_mut() = StreamingRenderCache::default();
    }

    #[cfg(test)]
    fn render(&self, renderer: &RichRenderer, theme: &YggTheme, width: u16) -> Vec<String> {
        self.render_on_surface(renderer, theme, width, None)
    }

    fn render_on_surface(
        &self,
        renderer: &RichRenderer,
        theme: &YggTheme,
        width: u16,
        background: Option<Color>,
    ) -> Vec<String> {
        // Blocks are rendered at the caller's exact content width. Every
        // transcript block shares the same outer baseline; semantic styling
        // supplies hierarchy without changing horizontal geometry.
        if looks_like_diff(&self.text) {
            return renderer
                .render_diff(
                    &UnifiedDiff::parse(&self.text),
                    width,
                    DiffRenderOptions {
                        line_numbers: width >= 70,
                        wrap: true,
                    },
                )
                .lines
                .into_iter()
                .map(|line| {
                    if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
                        line.plain
                    } else {
                        line.styled
                    }
                })
                .collect();
        }
        let rendered =
            if self.finished && background.is_some_and(|background| background != Color::Default) {
                renderer.render_on_background(
                    &parse_markdown(self.markdown.raw_text()),
                    width,
                    background.expect("checked above"),
                )
            } else {
                self.layout
                    .borrow_mut()
                    .render(&self.markdown, renderer, width)
            };
        rendered
            .lines
            .into_iter()
            .map(|line| {
                if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
                    line.plain
                } else {
                    line.styled
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
struct ToolPanel {
    id: ToolCallId,
    name: String,
    args: String,
    display: ToolDisplay,
    output: String,
    finished: bool,
    is_error: bool,
    failure_reason: Option<String>,
    /// Optional extension-owned semantic presentation. These are always plain,
    /// sanitized segments; roles are resolved against the current theme only
    /// while rendering. The durable provider-visible `output` stays intact.
    extension_render_segments: Vec<ygg_agent::extension_process::ToolRenderSegment>,
    /// Model family captured with the call for durable presentation
    /// provenance. Lifecycle chrome deliberately no longer consumes it:
    /// active, successful, and failed headers use muted, foreground, and
    /// error roles respectively.
    #[allow(dead_code)]
    model_lab: Option<crate::tui::theme::ModelLab>,
    /// Lazily cached diff scan. `None` means not yet computed.
    cached_diff: RefCell<Option<Option<String>>>,
    /// Lazily cached metadata string for completed bash results.
    cached_metadata: RefCell<Option<Option<String>>>,
}

impl ToolPanel {
    // Construction mirrors the protocol event fields plus presentation state;
    // keeping it explicit avoids an error-prone partially initialized panel.
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: ToolCallId,
        name: String,
        args: String,
        display: ToolDisplay,
        output: String,
        finished: bool,
        is_error: bool,
        failure_reason: Option<String>,
        model_lab: Option<crate::tui::theme::ModelLab>,
    ) -> Self {
        Self {
            id,
            name,
            args,
            display,
            output,
            finished,
            is_error,
            failure_reason,
            extension_render_segments: Vec::new(),
            model_lab,
            cached_diff: RefCell::new(None),
            cached_metadata: RefCell::new(None),
        }
    }
}

/// Durable transcript coordinate. It deliberately names a semantic block and
/// an offset in that block's clean copy text, never a terminal row. Reflow,
/// streaming, and composer animation can therefore not invalidate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptPosition {
    block: usize,
    offset: usize,
    /// At a wrapped boundary, retain which side the pointer came from.
    trailing_affinity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptSelection {
    anchor: TranscriptPosition,
    focus: TranscriptPosition,
}

/// Final block-local geometry shared by transcript rendering and semantic
/// selection. Decorative rows and columns never enter copy offsets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SurfaceGeometry {
    transition_rows: usize,
    leading_rows: usize,
    trailing_rows: usize,
    content_left: u16,
    content_width: u16,
}

impl SurfaceGeometry {
    fn content_row(self, local_row: usize, total_rows: usize) -> Option<usize> {
        let start = self.transition_rows.checked_add(self.leading_rows)?;
        let end = total_rows.checked_sub(self.trailing_rows)?;
        (local_row >= start && local_row < end).then(|| local_row - start)
    }

    fn content_col(self, column: u16) -> u16 {
        column
            .saturating_sub(self.content_left)
            .min(self.content_width)
    }
}

#[derive(Clone, Debug)]
struct RenderedTranscriptBlock {
    lines: Vec<String>,
    geometry: SurfaceGeometry,
}

#[derive(Clone, Debug)]
struct TranscriptCache {
    width: Option<u16>,
    lines: Vec<String>,
    block_starts: Vec<usize>,
    block_lengths: Vec<usize>,
    block_geometries: Vec<SurfaceGeometry>,
    block_revisions: Vec<u64>,
    /// Blocks changed since the last layout pass. Keeping this explicit avoids
    /// scanning every historic block for each streamed token.
    dirty_blocks: Vec<usize>,
    dirty: bool,
    generation: u64,
    /// First visual row changed by the most recent layout update.
    last_update_start: usize,
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

#[derive(Clone, Debug)]
struct QueuedSteering {
    /// Readable transcript projection (large pasted text expanded).
    display: String,
    /// Original editor projection used if an undelivered message is restored.
    editor_display: String,
    attachments: Vec<composer::Attachment>,
}

#[derive(Clone, Debug)]
enum ShellOverlay {
    Text(String),
    Context(crate::tui::context::ContextReport),
}

/// An interactive panel wedged between the transcript and composer.
/// Two horizontal rules delimit it; the interior renders form content.
#[derive(Clone, Debug)]
pub(crate) enum Panel {
    /// Select-list panel (model picker, session picker, thinking picker, theme picker).
    SelectList {
        title: String,
        items: Vec<String>,
        descriptions: Vec<Option<String>>,
        selected: usize,
        filter: String,
        /// What to do with the confirmed index.
        action: PanelAction,
    },
}

/// What happens when the user confirms a panel selection.
#[derive(Clone, Debug)]
#[allow(dead_code, clippy::enum_variant_names)]
pub(crate) enum PanelAction {
    /// Select a model by id.
    SelectModel(Vec<ModelId>),
    /// Select a session by path.
    SelectSession(Vec<std::path::PathBuf>),
    /// Select a thinking level.
    SelectThinking(Vec<crate::config::ThinkingLevel>),
    /// Select a reasoning execution mode.
    SelectReasoningMode(Vec<ygg_ai::ReasoningMode>),
    /// Select a theme name.
    SelectTheme(Vec<String>),
    /// Confirm or deny a typed executable-extension request.
    ExtensionConfirmation,
}

/// Outcome produced by closing a panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PanelResult {
    /// User confirmed the selection at the given index.
    Confirm(usize),
    /// User cancelled (Esc).
    Cancel,
}

#[derive(Default)]
pub(crate) struct ShellState {
    /// Active interactive panel, if any.
    pub(crate) panel: Option<Panel>,
    pub(crate) theme: YggTheme,
    /// Theme swap revision. The retained terminal renderer uses this
    /// to repaint the complete visible viewport even when some logical rows
    /// (notably blank separators) are byte-identical across themes.
    theme_epoch: u64,
    /// Changes only when the logical transcript is replaced (for example by
    /// `/new` or session hydration). Visual row counts may shrink while a
    /// streaming Markdown block reparses and must not look like a new session.
    transcript_epoch: u64,
    /// Stable width-independent identity parallel to each transcript block.
    /// IDs survive deferred prepends and are never reused after tail removal.
    transcript_commit_ids: Vec<u64>,
    next_transcript_commit_id: NextTranscriptCommitId,
    /// Creator family for the active model. The dedicated model accent is
    /// reapplied whenever a named theme is loaded.
    pub(crate) model_lab: Option<crate::tui::theme::ModelLab>,
    /// Exact deterministic row colour assigned to the next submitted prompt.
    pub(crate) prompt_color: Option<String>,
    transcript: Vec<TranscriptBlock>,
    /// Shared phase for every active event marker. This is presentation-only
    /// and toggles at a fixed cadence on the renderer thread.
    event_dot_visible: bool,
    /// Snapshot backing an intentionally tail-only first paint. The complete
    /// branch is materialized on scroll or before a destructive resize replay,
    /// so resume readiness does not scale with old history.
    deferred_session_history: Option<DeferredSessionHistory>,
    /// One-shot marker for a cache rebuild caused by prepending deferred
    /// history. Those rows are above the current viewport, not new output
    /// below it, so the normal scroll-anchor rebase must be skipped once.
    history_prepended: Cell<bool>,
    /// Monotonic revisions let the renderer update only blocks whose text or
    /// tool output changed.
    block_revisions: Vec<u64>,
    /// Steering messages accepted while a run is active but not yet injected.
    steering_queue: Vec<QueuedSteering>,
    /// Chip-backed attachments awaiting submit.
    ledger: composer::AttachmentLedger,
    /// Input modalities of the active model; gates attach attempts.
    pub(crate) input_modalities: ModalitySet,
    /// Workspace root and its lazily built mention-completion index.
    workspace: Option<PathBuf>,
    file_index: Option<Vec<String>>,
    /// Cached wrapped transcript lines. Scrolling only slices this cache, and
    /// streaming updates re-render only the changed block.
    transcript_cache: RefCell<TranscriptCache>,
    /// Persistent rich renderers: their syntax caches survive token updates.
    rich_renderer: RefCell<Option<RichRenderer>>,
    reasoning_renderer: RefCell<Option<RichRenderer>>,
    pub(crate) editor: String,
    /// Ephemeral tool-owned prompt rendered in place of the editor. Secret
    /// keystrokes never enter `editor` or any transcript/session structure.
    pub(crate) tool_input_prompt: Option<String>,
    /// Durable request to leave the interactive frontend. Exclusive picker and
    /// lifecycle loops set this so the owning outer loop can finish in-flight
    /// cleanup before exiting.
    close_requested: bool,
    /// Selection and viewport for the slash-command popup. Filtering resets
    /// both; Escape dismisses it until the command token changes again.
    prompt_templates: Arc<[crate::prompts::PromptTemplateDescriptor]>,
    extension_commands: Arc<[(String, String)]>,
    slash_selection: usize,
    slash_scroll: usize,
    slash_popup_dismissed: bool,
    /// Byte offset into `editor`; always kept at a UTF-8 character boundary.
    pub(crate) editor_cursor: usize,
    status_detail: String,
    pub(crate) extension_header: Option<(String, Option<String>)>,
    pub(crate) extension_status: Option<(String, Option<String>)>,
    pub(crate) extension_footer: Option<(String, Option<String>)>,
    pub(crate) error: Option<String>,
    overlay: Option<ShellOverlay>,
    tool_panels: HashMap<ToolCallId, usize>,
    active_text: Option<usize>,
    active_reasoning: Option<usize>,
    /// Distance from the live tail in visual rows. Kept for cheap wheel/page
    /// movement; `follow_tail` decides whether new output may change it.
    scroll_from_bottom: Cell<usize>,
    /// New output follows only while the reader is at the tail. Scrolling is
    /// never a modal operation and never moves editor focus.
    pub(crate) follow_tail: bool,
    /// Output received while the reader intentionally stays on history.
    pub(crate) new_output_count: usize,
    /// Application-owned transcript selection; composer selection remains
    /// entirely separate in the editor widget.
    transcript_selection: Option<TranscriptSelection>,
    /// Mouse-down position that has not yet begun a drag. A click without
    /// movement clears any prior selection; the first drag event promotes
    /// this anchor into `transcript_selection`.
    pending_selection_anchor: Option<TranscriptPosition>,
    /// A drag which began in the transcript remains transcript-owned even
    /// when its pointer crosses into the pinned composer/footer.
    selection_dragging: bool,
    /// Escape-free fallback retained when no clipboard transport is available.
    copy_buffer: Option<String>,
    pub(crate) context_estimate: Option<(u64, u64)>,
    pub(crate) last_turn_usage: Option<Usage>,
    /// Measured output-generation rate for the most recently completed model
    /// turn. This deliberately excludes provider wait time and tool execution.
    pub(crate) last_turn_tokens_per_second: Option<f64>,
    /// Measured generation duration and output-token delta backing the final
    /// throughput value. Kept for the detailed `/status` provenance view.
    pub(crate) last_turn_generation_elapsed: Option<Duration>,
    pub(crate) last_turn_generated_tokens: Option<u64>,
    /// Start of the visible model-generation portion of the current turn.
    pub(crate) turn_generation_started_at: Option<Instant>,
    /// Bytes streamed during the current provider attempt. This supports a
    /// cheap live token estimate without tokenizing the complete transcript on
    /// every frame.
    pub(crate) turn_streamed_output_bytes: u64,
    /// Cumulative output tokens before the current model turn began streaming.
    pub(crate) turn_output_tokens_before_generation: u64,
    /// Cumulative session cost in microdollars (1/1,000,000 USD).
    /// `None` when no priced model has been used yet in this session.
    pub(crate) session_cost_microdollars: Option<u64>,
    pub(crate) max_session_cost_microdollars: Option<u64>,
    /// Latest-turn raw cache-read rate, refreshed at idle boundaries.
    ///
    /// This mirrors the provider-reported ratio Pi places in its footer rather
    /// than Ygg's cumulative material-miss diagnostic.
    pub(crate) cache_hit_rate_basis_points: Option<u16>,
    /// Cost accrued during the current or most recently completed run.
    pub(crate) run_cost_microdollars: u64,
    /// Distinguishes an exact zero from a legacy/unavailable resumed value.
    pub(crate) run_cost_available: bool,
    /// Opt-in compact-footer visibility for the current provider turn's cost.
    /// Accounting and detailed diagnostics do not depend on this flag.
    pub(crate) show_turn_cost: bool,
    /// One authoritative presentation lifecycle for the newest run.
    pub(crate) run: RunTracker,
    /// Sum of settled agent-run durations for this interactive session.
    /// User reading/composition time is deliberately excluded.
    pub(crate) session_work_elapsed: Duration,
    pub(crate) provider: String,
    /// Canonical model identifier retained for `/status` and diagnostics.
    pub(crate) model: String,
    /// Stable friendly identity resolved only when model metadata changes.
    pub(crate) model_display: String,
    pub(crate) model_compact_names: Vec<String>,
    /// Canonical identity and lab captured when a run starts. Selection may
    /// change while the run is active, but streaming blocks and telemetry must
    /// continue to belong to the model actually executing that run.
    pub(crate) run_model: Option<String>,
    pub(crate) run_model_lab: Option<ModelLab>,
    pub(crate) run_prompt_color: Option<String>,
    pub(crate) run_model_display: Option<String>,
    pub(crate) run_model_compact_names: Vec<String>,
    pub(crate) run_reasoning: Option<String>,
    pub(crate) run_price_display: Option<PriceDisplay>,
    pub(crate) run_context_estimate: Option<(u64, u64)>,
    /// Canonical model that owns the retained completed-turn instruments.
    /// `None` is deliberately neutral for legacy records with no attribution.
    pub(crate) telemetry_model: Option<String>,
    pub(crate) price_display: PriceDisplay,
    pub(crate) latest_compaction_summary: Option<String>,
    pub(crate) reasoning: String,
    /// Non-agent work such as compaction or sign-in. Agent runs never use this
    /// field; their phase always comes from `run`.
    pub(crate) run_label: String,
    /// Wall-clock anchor for the current working shimmer. Keeping this outside
    /// the run phase means a phase transition cannot change the wave velocity
    /// or reset its position.
    pub(crate) shimmer_started_at: Option<Instant>,
    /// Global transcript disclosure mode. Ctrl+O and `/verbose` toggle this.
    pub(crate) verbose_tools: bool,
    pub(crate) size: (u16, u16),
    /// Start of the animated invocation header. It remains mutable until the
    /// first real conversation block so model changes can recolor it in place.
    startup_card_started_at: Option<Instant>,
    /// Cached editor layout so the composer shimmer animation doesn't
    /// re-wrap the prompt on every frame.
    cached_layout: RefCell<Option<EditorLayoutCache>>,
}

impl ShellState {
    fn welcome_is_mutable(&self) -> bool {
        self.startup_card_started_at.is_some()
            && !self.transcript.iter().any(|block| {
                matches!(
                    block,
                    TranscriptBlock::User { .. }
                        | TranscriptBlock::Assistant(_)
                        | TranscriptBlock::Reasoning(_)
                        | TranscriptBlock::Tool(_)
                        | TranscriptBlock::Shell(_)
                        | TranscriptBlock::Outcome(_)
                        | TranscriptBlock::Compaction(_)
                )
            })
    }

    fn restart_welcome_animation(&mut self) {
        if self.welcome_is_mutable() {
            self.startup_card_started_at = Some(Instant::now());
            self.invalidate_transcript_layout();
        }
    }

    fn invalidate_transcript(&mut self) {
        self.transcript_cache.get_mut().dirty = true;
    }

    fn invalidate_transcript_layout(&mut self) {
        let cache = self.transcript_cache.get_mut();
        cache.width = None;
        cache.dirty = true;
    }

    fn invalidate_disclosure(&mut self) {
        let indices = self
            .transcript
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                matches!(
                    block,
                    TranscriptBlock::Reasoning(_)
                        | TranscriptBlock::Compaction(_)
                        | TranscriptBlock::Tool(_)
                        | TranscriptBlock::Shell(_)
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        for index in &indices {
            if let Some(revision) = self.block_revisions.get_mut(*index) {
                *revision = revision.saturating_add(1);
            }
        }
        let cache = self.transcript_cache.get_mut();
        cache.dirty = true;
        cache.dirty_blocks.extend(indices);
    }

    fn invalidate_rich_text(&mut self) {
        *self.rich_renderer.get_mut() = None;
        *self.reasoning_renderer.get_mut() = None;
        for block in &self.transcript {
            if let TranscriptBlock::Assistant(markdown) | TranscriptBlock::Reasoning(markdown) =
                block
            {
                markdown.invalidate_layout();
            }
        }
        self.invalidate_transcript_layout();
    }

    fn push_block(&mut self, block: TranscriptBlock) {
        let commit_id = self.next_transcript_commit_id.0;
        self.next_transcript_commit_id.0 = commit_id
            .checked_add(1)
            .expect("transcript commit identity space exhausted");
        self.transcript.push(block);
        self.transcript_commit_ids.push(commit_id);
        self.block_revisions.push(0);
        if !self.follow_tail {
            self.new_output_count = self.new_output_count.saturating_add(1);
        }
        // Transcript blocks are append-only in normal operation, so historic
        // layout remains valid regardless of whether the new block is prose,
        // reasoning, or a tool event.
        self.invalidate_transcript();
    }

    pub(crate) fn jump_to_tail(&mut self) {
        self.scroll_from_bottom.set(0);
        self.follow_tail = true;
        self.new_output_count = 0;
    }

    fn clear_turn_telemetry(&mut self) {
        self.last_turn_usage = None;
        self.last_turn_tokens_per_second = None;
        self.last_turn_generation_elapsed = None;
        self.last_turn_generated_tokens = None;
        self.turn_generation_started_at = None;
        self.turn_streamed_output_bytes = 0;
        self.turn_output_tokens_before_generation = 0;
        self.run_cost_microdollars = 0;
        self.run_cost_available = false;
        self.cache_hit_rate_basis_points = None;
        self.telemetry_model = None;
    }

    fn executing_model_lab(&self) -> Option<ModelLab> {
        if self.run.is_active() {
            self.run_model_lab
        } else {
            self.model_lab
        }
    }

    fn executing_prompt_color(&self) -> Option<String> {
        if self.run.is_active() {
            self.run_prompt_color.clone()
        } else {
            self.prompt_color.clone()
        }
    }

    pub(crate) fn selected_model_owns_telemetry(&self) -> bool {
        self.telemetry_model
            .as_deref()
            .is_some_and(|model| model == self.model)
    }

    pub(crate) fn live_generated_tokens(&self) -> Option<u64> {
        self.turn_generation_started_at
            .map(|_| self.turn_streamed_output_bytes.div_ceil(4))
            .filter(|tokens| *tokens > 0)
    }

    pub(crate) fn displayed_output_tokens(&self) -> Option<(u64, bool)> {
        if let Some(live) = self.live_generated_tokens() {
            return Some((
                self.turn_output_tokens_before_generation
                    .saturating_add(live),
                true,
            ));
        }
        self.last_turn_usage
            .map(|usage| (usage.output_tokens, false))
    }

    pub(crate) fn cached_editor_layout(
        &self,
        width: u16,
        editor: Option<&String>,
        cursor: Option<usize>,
    ) -> EditorLayout {
        let text = editor.map(String::as_str).unwrap_or("");
        let cursor = cursor.unwrap_or(0);
        let cursor = cursor.min(text.len());
        // Only recompute when the input actually changed: text, cursor, or width.
        let cache = self.cached_layout.borrow();
        if let Some(ref cached) = *cache {
            if cached.width == width
                && cached.text_len == text.len()
                && cached.cursor == cursor
                && cached.text_hash == hash_str(text)
            {
                return cached.layout.clone();
            }
        }
        drop(cache);
        let layout = editor_layout(text, cursor, width);
        *self.cached_layout.borrow_mut() = Some(EditorLayoutCache {
            width,
            text_len: text.len(),
            cursor,
            text_hash: hash_str(text),
            layout: layout.clone(),
        });
        layout
    }

    fn touch_block(&mut self, index: usize) {
        if let Some(revision) = self.block_revisions.get_mut(index) {
            *revision = revision.saturating_add(1);
        }
        let cache = self.transcript_cache.get_mut();
        cache.dirty = true;
        // A render is coalesced, so a hot streaming block can be touched many
        // times before the next frame. Record it once rather than making each
        // frame linearly scan the complete transcript for revision changes.
        if !cache.dirty_blocks.contains(&index) {
            cache.dirty_blocks.push(index);
        }
    }

    fn show_tool_details(&self, _block: &TranscriptBlock) -> bool {
        self.verbose_tools
    }

    fn rendered_transcript(&self, width: u16) -> Ref<'_, Vec<String>> {
        let stale = self.transcript_cache.borrow().dirty;
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

    fn reasoning_status_enabled(&self) -> bool {
        let reasoning = self
            .run_reasoning
            .as_deref()
            .unwrap_or(&self.reasoning)
            .trim()
            .to_ascii_lowercase();
        !reasoning.is_empty() && !matches!(reasoning.as_str(), "off" | "none" | "disabled")
    }

    fn open_reasoning_status(&mut self) {
        if !self.reasoning_status_enabled() || self.active_reasoning.is_some() {
            return;
        }
        if let Some(previous) = self
            .transcript
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::Reasoning(_)))
        {
            if let Some(TranscriptBlock::Reasoning(reasoning)) = self.transcript.get_mut(previous) {
                reasoning.show_reasoning_hint = false;
            }
            self.touch_block(previous);
        }
        let index = self.transcript.len();
        let model_lab = self.executing_model_lab();
        self.event_dot_visible = true;
        self.push_block(TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::streaming_reasoning("").with_model_lab(model_lab),
        )));
        self.active_reasoning = Some(index);
    }

    fn append_text_block(&mut self, channel: OutputChannel, text: &str) {
        if channel == OutputChannel::Text {
            if let Some(index) = self.active_reasoning.take() {
                if let Some(TranscriptBlock::Reasoning(reasoning)) = self.transcript.get_mut(index)
                {
                    reasoning.finish_reasoning();
                    self.touch_block(index);
                }
            }
        }
        let active_index = match channel {
            OutputChannel::Text => self.active_text,
            OutputChannel::Reasoning => self.active_reasoning,
        };
        if let Some(index) = active_index {
            let updated = match self.transcript.get_mut(index) {
                Some(TranscriptBlock::Assistant(existing)) if channel == OutputChannel::Text => {
                    existing.append(text);
                    true
                }
                Some(TranscriptBlock::Reasoning(existing))
                    if channel == OutputChannel::Reasoning =>
                {
                    existing.append_reasoning(text);
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

        if channel == OutputChannel::Reasoning {
            if let Some(previous) = self
                .transcript
                .iter()
                .rposition(|block| matches!(block, TranscriptBlock::Reasoning(_)))
            {
                if let Some(TranscriptBlock::Reasoning(reasoning)) =
                    self.transcript.get_mut(previous)
                {
                    reasoning.show_reasoning_hint = false;
                }
                self.touch_block(previous);
            }
        }

        let index = self.transcript.len();
        let model_lab = self.executing_model_lab();
        if channel == OutputChannel::Reasoning {
            self.event_dot_visible = true;
        }
        self.push_block(match channel {
            OutputChannel::Text => TranscriptBlock::Assistant(Box::new(
                AssistantBlock::streaming(text).with_model_lab(model_lab),
            )),
            OutputChannel::Reasoning => TranscriptBlock::Reasoning(Box::new(
                AssistantBlock::streaming_reasoning(text).with_model_lab(model_lab),
            )),
        });
        match channel {
            OutputChannel::Text => self.active_text = Some(index),
            OutputChannel::Reasoning => self.active_reasoning = Some(index),
        }
    }

    /// Remove provisional model output from an interrupted provider attempt.
    /// These blocks have no corresponding persisted assistant message and a
    /// replacement attempt will stream a fresh version of the same turn.
    fn discard_streaming_blocks(&mut self) {
        let mut indices = [self.active_text.take(), self.active_reasoning.take()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices.dedup();
        let removed = indices.len();
        for index in indices.into_iter().rev() {
            if index >= self.transcript.len() {
                continue;
            }
            self.transcript.remove(index);
            self.transcript_commit_ids.remove(index);
            self.block_revisions.remove(index);
            for panel_index in self.tool_panels.values_mut() {
                if *panel_index > index {
                    *panel_index -= 1;
                }
            }
        }
        if !self.follow_tail {
            self.new_output_count = self.new_output_count.saturating_sub(removed);
        }
        // Durable coordinates into removed blocks cannot be repaired without
        // guessing which retry text corresponds to the old byte offset.
        self.transcript_selection = None;
        self.pending_selection_anchor = None;
        self.invalidate_transcript_layout();
    }

    fn close_streaming_blocks(&mut self) {
        if let Some(index) = self.active_text.take() {
            if let Some(TranscriptBlock::Assistant(assistant)) = self.transcript.get_mut(index) {
                assistant.finish();
                self.touch_block(index);
            }
        }
        if let Some(index) = self.active_reasoning.take() {
            if let Some(TranscriptBlock::Reasoning(reasoning)) = self.transcript.get_mut(index) {
                reasoning.finish_reasoning();
                self.touch_block(index);
            }
        }
    }

    fn has_active_event_dot(&self) -> bool {
        self.transcript.iter().rev().any(|block| match block {
            TranscriptBlock::Reasoning(reasoning) => {
                !self.verbose_tools && !reasoning.finished && !reasoning.reasoning_expanded
            }
            TranscriptBlock::Tool(panel) => !panel.finished,
            TranscriptBlock::Shell(shell) => shell.running,
            _ => false,
        })
    }

    fn advance_event_dot_animation(&mut self) {
        let active = self
            .transcript
            .iter()
            .enumerate()
            .filter_map(|(index, block)| match block {
                TranscriptBlock::Reasoning(reasoning)
                    if !self.verbose_tools
                        && !reasoning.finished
                        && !reasoning.reasoning_expanded =>
                {
                    Some(index)
                }
                TranscriptBlock::Tool(panel) if !panel.finished => Some(index),
                TranscriptBlock::Shell(shell) if shell.running => Some(index),
                _ => None,
            })
            .collect::<Vec<_>>();
        if active.is_empty() {
            return;
        }
        self.event_dot_visible = !self.event_dot_visible;
        for index in active {
            self.touch_block(index);
        }
    }

    fn tool_output_mut(&mut self, id: &ToolCallId) -> Option<&mut ToolPanel> {
        let index = *self.tool_panels.get(id)?;
        match self.transcript.get_mut(index) {
            Some(TranscriptBlock::Tool(panel)) => Some(panel),
            _ => None,
        }
    }

    fn refresh_tool_displays(&mut self) {
        let workspace = self.workspace.clone();
        for block in &mut self.transcript {
            let TranscriptBlock::Tool(panel) = block else {
                continue;
            };
            let Ok(args) = serde_json::from_str::<serde_json::Value>(&panel.args) else {
                continue;
            };
            panel.display = summarize_tool_with_workspace(&panel.name, &args, workspace.as_deref());
        }
        // Tool summaries are part of the cached transcript layout, so a
        // workspace becoming known must force historic rows to be rebuilt too.
        self.invalidate_transcript_layout();
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

/// True when the perimeter-shimmer animation is visible and moving.  When
/// false we can use a lazy poll interval to save CPU.
fn shimmer_animating(state: &ShellState) -> bool {
    let capabilities = state.theme.capabilities();
    if !capabilities.animation
        || capabilities.color == crate::tui::terminal::ColorDepth::None
        || state.size.0 < 12
    {
        return false;
    }
    if state.run_label == "compacting" {
        return true;
    }
    let Some(run) = state.run.current() else {
        return false;
    };
    if !run.is_active() || state.reasoning.trim().eq_ignore_ascii_case("off") {
        return false;
    }
    // The helper returns a fixed positive velocity only for working phases;
    // approval waits deliberately leave the border still and do not need a
    // high-frequency repaint.
    crate::tui::composer_surface::phase_speed_for(Some(run.phase())) > 0.0
}

fn welcome_animating(state: &ShellState, now: Instant) -> bool {
    state.welcome_is_mutable()
        && state.theme.capabilities().animation
        && state.overlay.is_none()
        && state.startup_card_started_at.is_some_and(|started| {
            now.saturating_duration_since(started).as_secs_f32() < crate::tui::splash::DURATION
        })
}

fn event_dot_animating(state: &ShellState) -> bool {
    let capabilities = state.theme.capabilities();
    capabilities.animation && capabilities.interactive && state.has_active_event_dot()
}

/// Reconcile the renderer's shared dimensions with the terminal itself. This
/// is a fallback for environments where the resize signal is delayed or
/// swallowed; the normal input path still updates the same cells immediately.
fn synchronize_terminal_size(state: &SharedState, size: &TerminalSize) -> bool {
    let Ok(dimensions) = crossterm::terminal::size() else {
        return false;
    };
    reconcile_terminal_size(state, size, dimensions)
}

fn reconcile_terminal_size(
    state: &SharedState,
    size: &TerminalSize,
    dimensions: (u16, u16),
) -> bool {
    let changed = {
        let mut current = size.lock().expect("terminal size mutex poisoned");
        if *current == dimensions {
            false
        } else {
            *current = dimensions;
            true
        }
    };
    if !changed {
        return false;
    }

    // This delayed-signal fallback must obey the same transcript contract as
    // the normal resize path: destructive replay cannot run from a bounded
    // first-paint tail.
    if let Err(error) = materialize_deferred_session_history(state) {
        state.borrow_mut().error = Some(format!("could not load older session history: {error}"));
    }

    let mut shell = state.borrow_mut();
    shell.size = dimensions;
    // Do not ask for transcript geometry here: that would synchronously reflow
    // the complete history and then invalidate it, paying the resize cost
    // twice. Viewport readers clamp the retained scroll offset after the render
    // thread performs the single required layout pass.
    shell.invalidate_transcript_layout();
    true
}

fn render_loop(
    terminal: YggTerminal,
    state: SharedState,
    size: TerminalSize,
    rx: Receiver<RenderCommand>,
    application_viewport: bool,
) {
    let mut tui = TUI::new(Box::new(terminal));
    tui.set_inline_scrollback(true);
    tui.add_child(Box::new(ShellComponent {
        state: state.clone(),
        frame: RefCell::new(ShellFrameState::default()),
        application_viewport,
    }));
    tui.start();

    let mut last_render: Option<Instant> = None;
    let mut last_event_dot_toggle = Instant::now();
    loop {
        // Choose the poll timeout based on whether the shimmer animation
        // would be rendered this frame.  When it is, use a short timeout so
        // the wave stays fluid on high-refresh terminals. Otherwise use a
        // 100 ms status/resize poll; idle timeouts do not render unless the
        // terminal dimensions actually changed.
        let (animating, welcome, event_dot, is_active) = {
            let s = state.borrow();
            let active = s.run.is_active();
            let compacting = s.run_label == "compacting";
            let shimmer = (active || compacting) && shimmer_animating(&s);
            let welcome = welcome_animating(&s, Instant::now());
            let event_dot = event_dot_animating(&s);
            (
                shimmer || welcome,
                welcome,
                event_dot,
                active || compacting || welcome || event_dot,
            )
        };
        if !event_dot {
            last_event_dot_toggle = Instant::now();
        }
        let command = if animating {
            let poll = if welcome {
                RENDER_INTERVAL
            } else {
                ANIMATION_POLL_TIMEOUT
            };
            match rx.recv_timeout(poll) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv_timeout(RESIZE_POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        if matches!(command, Some(RenderCommand::Stop)) {
            break;
        }

        let resized = if command.is_none() {
            synchronize_terminal_size(&state, &size)
        } else {
            false
        };
        if command.is_none() && !resized && !animating && !is_active {
            continue;
        }

        // Cap rendering to a sensible upper bound. Shimmer is deliberately
        // slower than input/streaming frames and changes only a few cells.
        let cap = if welcome {
            RENDER_INTERVAL
        } else if animating {
            ANIMATION_RENDER_INTERVAL
        } else {
            RENDER_INTERVAL
        };
        if let Some(last) = last_render {
            let elapsed = last.elapsed();
            if elapsed < cap {
                thread::sleep(cap - elapsed);
            }
        }

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

        let advance_event_dot =
            event_dot && last_event_dot_toggle.elapsed() >= EVENT_DOT_TOGGLE_INTERVAL;
        if welcome || advance_event_dot {
            let mut shell = state.borrow_mut();
            if welcome {
                shell.invalidate_transcript_layout();
            }
            if advance_event_dot {
                shell.advance_event_dot_animation();
                last_event_dot_toggle = Instant::now();
            }
        }
        tui.request_render();
        last_render = Some(Instant::now());
    }

    tui.stop();
}

#[derive(Default)]
struct ShellFrameState {
    initialized: bool,
    width: u16,
    height: u16,
    theme_epoch: u64,
    transcript_epoch: u64,
    transcript_generation: u64,
    transcript_len: usize,
    verbose_tools: bool,
    overlay_active: bool,
    /// Rows of the native transcript frame retained above the screen-sized
    /// overlay surface. This bounds lazy diffs when mutable chrome changes the
    /// overlay's seam with terminal-owned history.
    overlay_prefix_len: usize,
}

/// The retained root component. It reads the shell state at render time, while
/// `InteractiveShell` mutates that same state in response to events.
struct ShellComponent {
    state: SharedState,
    frame: RefCell<ShellFrameState>,
    /// Explicit `--mouse app` compatibility mode keeps the bounded semantic
    /// viewport. The default path emits committed transcript rows into native
    /// terminal scrollback instead.
    application_viewport: bool,
}

impl Component for ShellComponent {
    fn render(&self, width: u16) -> Vec<String> {
        let state = self.state.borrow();
        if self.application_viewport {
            let lines = render_shell_viewport_at(&state, width, Instant::now());
            let mut frame = self.frame.borrow_mut();
            frame.initialized = true;
            frame.width = width;
            frame.height = state.size.1;
            frame.theme_epoch = state.theme_epoch;
            frame.transcript_epoch = state.transcript_epoch;
            frame.verbose_tools = state.verbose_tools;
            lines
        } else {
            let lines = render_shell(&state, width);
            synchronize_shell_frame(&state, width, &mut self.frame.borrow_mut());
            lines
        }
    }

    fn render_update(&self, width: u16) -> Option<FrameUpdate> {
        self.render_update_with_cursor(width, None)
    }

    fn render_update_with_cursor(
        &self,
        width: u16,
        cursor: Option<CommitCursor>,
    ) -> Option<FrameUpdate> {
        let state = self.state.borrow();
        Some(if self.application_viewport {
            render_shell_viewport_update(
                &state,
                width,
                Instant::now(),
                &mut self.frame.borrow_mut(),
            )
        } else {
            render_shell_update_with_cursor(
                &state,
                width,
                Instant::now(),
                &mut self.frame.borrow_mut(),
                cursor,
            )
        })
    }

    fn invalidate(&mut self) {
        *self.frame.get_mut() = ShellFrameState::default();
    }
}

fn branch_active(theme: &YggTheme) -> &str {
    theme.glyph("branch")
}

fn prompt_marker(theme: &YggTheme) -> &str {
    theme.glyph("prompt")
}

pub(crate) fn semantic_separator(theme: &YggTheme) -> &str {
    theme.glyph("separator")
}

fn live_reasoning_label(theme: &YggTheme, reasoning: &AssistantBlock) -> String {
    let label = reasoning.reasoning_heading.as_deref().unwrap_or("Thinking");
    theme.model_fg(reasoning.model_lab, label)
}

fn collapsed_reasoning_lines(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    include_margin_marker: bool,
) -> Vec<String> {
    // Finished reasoning leaves no trace in the collapsed transcript. Active
    // reasoning occupies exactly two rows so heading updates cannot reflow the
    // transcript around it.
    if reasoning.finished {
        Vec::new()
    } else {
        let label = live_reasoning_label(theme, reasoning);
        let label = if include_margin_marker {
            format!(
                "{} {label}",
                theme.model_fg(reasoning.model_lab, theme.glyph("bullet"))
            )
        } else {
            label
        };
        let disclosure_indent = if include_margin_marker { "  " } else { "" };
        vec![
            label,
            subdued_text(
                theme,
                &format!(
                    "{disclosure_indent}{} (ctrl+o to expand)",
                    theme.glyph("last_branch")
                ),
            ),
        ]
    }
}

/// A low-contrast annotation that remains readable without relying on a
/// painted background. This is used for viewport chrome and secondary tool
/// metadata, never for the answer itself.
fn subdued_text(theme: &YggTheme, text: &str) -> String {
    theme.fg("muted", text)
}

fn understated_tool_output(theme: &YggTheme, text: &str) -> String {
    theme
        .role_rgb("muted")
        .map_or_else(|| text.to_owned(), |color| theme.rgb_fg(color, text))
}

#[cfg(test)]
fn render_reasoning(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    show_reasoning: bool,
) -> Vec<String> {
    render_reasoning_on_surface(
        reasoning,
        renderer,
        theme,
        width,
        show_reasoning,
        None,
        false,
    )
}

fn render_reasoning_on_surface(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    show_reasoning: bool,
    background: Option<Color>,
    use_margin_marker: bool,
) -> Vec<String> {
    let marker = theme.glyph("reasoning");
    let prefix_width = visible_width(marker).saturating_add(1);
    if !reasoning.reasoning_expanded && !show_reasoning {
        return collapsed_reasoning_lines(theme, reasoning, !use_margin_marker)
            .into_iter()
            .map(|line| {
                let line = fit_line(&line, width);
                if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
                    strip_terminal_sequences(&line)
                } else {
                    line
                }
            })
            .collect();
    }
    let content_width = width.saturating_sub(prefix_width as u16).max(1);
    let lines = finish_transcript_block(reasoning.render_on_surface(
        renderer,
        theme,
        content_width,
        background,
    ));

    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if line.is_empty() {
                String::new()
            } else if index == 0 {
                fit_line(&format!("{} {line}", theme.fg("muted", marker)), width)
            } else {
                fit_line(&format!("{}{line}", " ".repeat(prefix_width)), width)
            }
        })
        .collect()
}

fn finish_transcript_block(mut lines: Vec<String>) -> Vec<String> {
    // Block renderers return content only. Transition spacing is decided once
    // in `render_block`, where both semantic neighbours are known.
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn transcript_transition_rows(previous: Option<&TranscriptBlock>, density: ThemeDensity) -> usize {
    // Density changes only the boundary between semantic blocks. A tool's
    // compact header, result, and diff still live inside one block and remain
    // adjacent, so compact themes never destroy meaningful grouping.
    if previous.is_none() {
        return 0;
    }
    match density {
        ThemeDensity::Compact => 0,
        ThemeDensity::Comfortable => 1,
        ThemeDensity::Airy => 2,
    }
}

fn render_user_prompt(
    text: &str,
    model_lab: &Option<ModelLab>,
    prompt_color: Option<&str>,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
) -> Vec<String> {
    let inner_width = width.saturating_sub(2).max(1);
    let document = parse_markdown(text);
    let render_result = renderer.render(&document, inner_width);
    let accent = |glyph: &str| match model_lab.filter(|lab| *lab != ModelLab::Unknown) {
        Some(lab) => theme.model_fg(Some(lab), glyph),
        None => theme.fg("muted", glyph),
    };

    // A persisted prompt colour owns the entire terminal row, including its
    // trailing cells. Plain text inside the coloured prompt prevents Markdown
    // resets from punching holes through the provenance background.
    let marker_glyph = sanitize_for_terminal(prompt_marker(theme));
    let rail_glyph = sanitize_for_terminal(theme.glyph("rail"));
    let cell = |glyph: &str| {
        let plain = format!("{glyph} ");
        if prompt_color.is_some() {
            theme.prompt_color_cell(prompt_color, &plain)
        } else {
            format!("{} ", accent(glyph))
        }
    };
    let marker = cell(&marker_glyph);
    let rail = cell(&rail_glyph);
    let mut lines = Vec::new();
    for (index, line) in render_result.lines.into_iter().enumerate() {
        let prefix = if index == 0 { &marker } else { &rail };
        if prompt_color.is_some()
            && theme.capabilities().color != crate::tui::terminal::ColorDepth::None
        {
            let mut row = format!(
                "{}{}",
                if index == 0 {
                    format!("{marker_glyph} ")
                } else {
                    format!("{rail_glyph} ")
                },
                line.plain
            );
            let row_width = visible_width(&row);
            row.push_str(&" ".repeat(usize::from(width).saturating_sub(row_width)));
            lines.push(theme.prompt_color_cell(prompt_color, &row));
        } else {
            let content = if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
                line.plain
            } else {
                line.styled
            };
            lines.push(fit_line(&format!("{prefix}{content}"), width));
        }
    }
    if lines.is_empty() {
        lines.push(fit_line(&marker, width));
    }
    finish_transcript_block(lines)
}

fn outcome_line(outcome: &RunOutcome, theme: &YggTheme) -> String {
    let separator = semantic_separator(theme);
    match outcome {
        RunOutcome::Completed { elapsed, .. } => {
            let text = subdued_text(
                theme,
                &format!("completed{separator}{}", format_duration(*elapsed)),
            );
            format!("{} {text}", theme.fg("success", theme.glyph("success")))
        }
        RunOutcome::CompletedWithWarnings {
            elapsed, warnings, ..
        } => format!(
            "{} {}",
            theme.fg("warning", theme.glyph("success")),
            subdued_text(
                theme,
                &format!(
                    "completed with {} note{}{separator}{}",
                    warnings,
                    if *warnings == 1 { "" } else { "s" },
                    format_duration(*elapsed)
                )
            )
        ),
        RunOutcome::Failed { elapsed, .. } => format!(
            "{} {}",
            theme.fg("error", theme.glyph("error")),
            theme.fg(
                "error",
                &format!("failed{separator}{}", format_duration(*elapsed))
            )
        ),
        RunOutcome::Interrupted { elapsed } | RunOutcome::Cancelled { elapsed } => format!(
            "{} {}",
            theme.fg("warning", theme.glyph("interrupt")),
            subdued_text(
                theme,
                &format!("interrupted{separator}{}", format_duration(*elapsed))
            )
        ),
        RunOutcome::NeedsInput { .. } => format!(
            "{} {}",
            theme.fg("warning", theme.glyph("note")),
            subdued_text(theme, "needs input")
        ),
    }
}

fn bounded_outcome_detail(raw: &str) -> String {
    let mut safe = sanitize_for_terminal(raw);
    if safe.len() <= MAX_OUTCOME_DETAIL_BYTES {
        return safe;
    }

    let mut end = MAX_OUTCOME_DETAIL_BYTES - '…'.len_utf8();
    while end > 0 && !safe.is_char_boundary(end) {
        end -= 1;
    }
    safe.truncate(end);
    safe.push('…');
    safe
}

fn render_outcome(outcome: &RunOutcome, theme: &YggTheme, width: u16) -> Vec<String> {
    let mut lines = vec![fit_line(&outcome_line(outcome, theme), width)];
    let detail = match outcome {
        // Inference diagnostics are credential-redacted at the request boundary.
        // Bound and terminal-sanitize them again at this presentation boundary.
        RunOutcome::Failed { reason, .. } => Some(("error", reason.as_str())),
        RunOutcome::NeedsInput { prompt } => Some(("warning", prompt.as_str())),
        _ => None,
    };
    if let Some((role, detail)) = detail {
        let safe = bounded_outcome_detail(detail);
        for source_line in safe.split('\n') {
            if source_line.is_empty() {
                lines.push(String::new());
                continue;
            }
            lines.extend(wrap_hanging(
                &theme.fg(role, source_line),
                "  ",
                "  ",
                width,
            ));
        }
    }
    finish_transcript_block(lines)
}

/// Wrap content after a visible prefix while preserving that prefix on the
/// first row and aligning every continuation under the content column.
fn wrap_hanging(text: &str, prefix: &str, continuation: &str, width: u16) -> Vec<String> {
    let width = usize::from(width).max(1);
    let prefix_width = visible_width(prefix);
    let continuation_width = visible_width(continuation);
    let content_width = width
        .saturating_sub(prefix_width.max(continuation_width))
        .max(1);
    wrap_text_with_ansi(text, content_width)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 { prefix } else { continuation };
            fit_line(&format!("{prefix}{line}"), width as u16)
        })
        .collect()
}

fn looks_like_diff(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim_start);
    let Some(first) = lines.find(|line| !line.is_empty()) else {
        return false;
    };
    if first.starts_with("diff --git ") {
        return true;
    }
    // Only promote a bare unified diff. Explanatory Markdown that happens to
    // contain a fenced `diff` block must stay in the Markdown renderer so its
    // prose, lists, and fence boundaries retain their structure.
    first.starts_with("--- ")
        && lines.any(|line| line.starts_with("+++ "))
        && text.lines().any(|line| line.trim_start().starts_with("@@"))
}

fn looks_like_legacy_write_creation(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim_start);
    let Some(first) = lines.find(|line| !line.is_empty()) else {
        return false;
    };
    first == "--- /dev/null" && lines.any(|line| line.starts_with("+++ b/"))
}

fn tool_diff(panel: &ToolPanel) -> Option<String> {
    // Only cache when finished — the output may still be streaming.
    if panel.finished {
        if let Some(ref cached) = *panel.cached_diff.borrow() {
            return cached.clone();
        }
    }
    let result = compute_tool_diff(panel);
    if panel.finished {
        *panel.cached_diff.borrow_mut() = Some(result.clone());
    }
    result
}

fn compute_tool_diff(panel: &ToolPanel) -> Option<String> {
    if looks_like_diff(&panel.output) {
        return Some(panel.output.clone());
    }
    if panel.name != "edit" && panel.name != "write" {
        return None;
    }
    let mut offset = 0;
    for line in panel.output.split_inclusive('\n') {
        let candidate = &panel.output[offset..];
        if (line.trim_start().starts_with("--- ") || line.trim_start().starts_with("diff --git "))
            && (looks_like_diff(candidate)
                || (panel.name == "write" && looks_like_legacy_write_creation(candidate)))
        {
            return Some(candidate.to_owned());
        }
        offset += line.len();
    }
    None
}

/// Minimum width reserved for the tool label before its value/output column.
const TOOL_VALUE_MIN_WIDTH: usize = 6;

fn tool_display_label(name: &str) -> &'static str {
    match name {
        "read" => "Read",
        "search" => "Explored",
        "edit" => "Edit",
        "write" => "Write",
        _ => "Used",
    }
}

fn tool_value_indent_width(label: &str) -> usize {
    TOOL_VALUE_MIN_WIDTH.max(visible_width(label).saturating_add(2))
}

fn tool_value_indent(label: &str) -> String {
    " ".repeat(tool_value_indent_width(label))
}

/// Max diff lines to show in terse mode before truncating.
const COMPACT_DIFF_LINES: usize = 10;

/// Render an edit/write diff. Long diffs are truncated in terse mode.
fn render_diff_only(
    panel: &ToolPanel,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    output_indent: &str,
) -> Vec<String> {
    let output_indent_width = u16::try_from(visible_width(output_indent)).unwrap_or(u16::MAX);
    let display_line = |line: sexy_tui_rs::RenderedLine| {
        let content = if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
            line.plain
        } else {
            line.styled
        };
        format!("{output_indent}{content}")
    };
    let Some(ref diff) = tool_diff(panel) else {
        return Vec::new();
    };
    let rendered = renderer.render_diff(
        &UnifiedDiff::parse(diff),
        width.saturating_sub(output_indent_width),
        DiffRenderOptions {
            line_numbers: width >= 70,
            wrap: true,
        },
    );
    let mut lines: Vec<String> = rendered.lines.into_iter().map(display_line).collect();
    if !expanded && lines.len() > COMPACT_DIFF_LINES + 1 {
        let remaining = lines.len() - COMPACT_DIFF_LINES;
        lines.truncate(COMPACT_DIFF_LINES);
        let unit = if remaining == 1 { "line" } else { "lines" };
        let hint = format!("{output_indent}{remaining} {unit} hidden");
        lines.push(subdued_text(theme, &hint));
    }
    lines
}

fn render_compact_tool_output(
    panel: &ToolPanel,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    output_indent: &str,
) -> Vec<String> {
    let output = sanitize_for_terminal(&panel.output);
    let mut lines = output
        .lines()
        .filter(|line| !line.trim().is_empty() && *line != "(no output)")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let omitted = if expanded {
        0
    } else {
        let omitted = lines.len().saturating_sub(COMPACT_EXEC_OUTPUT_LINES);
        if omitted > 0 {
            lines.drain(..omitted);
        }
        omitted
    };
    let mut rendered = Vec::new();
    if omitted > 0 {
        let unit = if omitted == 1 { "line" } else { "lines" };
        let hint = format!("{omitted} {unit} hidden");
        rendered.extend(wrap_hanging(
            &understated_tool_output(theme, &hint),
            output_indent,
            output_indent,
            width,
        ));
    }
    for line in lines {
        rendered.extend(wrap_hanging(
            &understated_tool_output(theme, &line),
            output_indent,
            output_indent,
            width,
        ));
    }
    rendered
}

fn render_shell_output(
    shell: &ShellOutput,
    theme: &YggTheme,
    width: u16,
    verbose: bool,
) -> Vec<String> {
    let mut lines = shell
        .output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let omitted = if verbose {
        0
    } else {
        let omitted = lines.len().saturating_sub(COMPACT_EXEC_OUTPUT_LINES);
        if omitted > 0 {
            lines.drain(..omitted);
        }
        omitted
    };
    let mut rendered = Vec::new();
    if omitted > 0 {
        let unit = if omitted == 1 { "line" } else { "lines" };
        let hint = format!("  {omitted} {unit} hidden");
        rendered.extend(wrap_hanging(
            &understated_tool_output(theme, &hint),
            "",
            "  ",
            width,
        ));
    }
    for line in lines {
        rendered.extend(wrap_hanging(
            &understated_tool_output(theme, &sanitize_for_terminal(&line)),
            "  ",
            "  ",
            width,
        ));
    }
    rendered
}

fn without_redundant_tool_lead(tool: &str, text: &str) -> String {
    let mut words = text.splitn(2, char::is_whitespace);
    let Some(first) = words.next() else {
        return String::new();
    };
    let redundant = match tool {
        "read" => matches!(first, "read" | "reading"),
        "search" => matches!(first, "search" | "searched" | "searching" | "explored"),
        "bash" | "exec" => {
            matches!(
                first,
                "bash" | "exec" | "run" | "ran" | "running" | "failed:"
            )
        }
        "edit" => matches!(first, "edit" | "edited" | "updating" | "updated"),
        "write" => matches!(first, "write" | "wrote" | "writing"),
        _ => matches!(first, "run" | "running" | "finished") || first == tool,
    };
    if redundant {
        words.next().unwrap_or_default().trim_start().to_owned()
    } else {
        text.to_owned()
    }
}

fn tool_metadata(panel: &ToolPanel) -> Option<String> {
    if let Some(ref cached) = *panel.cached_metadata.borrow() {
        return cached.clone();
    }
    let result = compute_tool_metadata(panel);
    *panel.cached_metadata.borrow_mut() = Some(result.clone());
    result
}

/// Locate the final canonical `bash` result after any live progress bytes.
/// The bash tool streams output while it runs, then emits a durable envelope
/// containing the exit status and bounded stdout/stderr capture. The panel
/// retains both, so presentation should prefer the last envelope without
/// mutating the stored tool result.
fn final_bash_result(output: &str) -> &str {
    for (index, _) in output.rmatch_indices("exit=") {
        let candidate = &output[index..];
        let mut lines = candidate.lines();
        let header = lines.next().unwrap_or_default();
        if !header
            .split_whitespace()
            .any(|part| part.starts_with("duration=") && part.len() > "duration=".len())
        {
            continue;
        }
        let next = lines.next().unwrap_or_default().trim();
        if index == 0 || next == "(no output)" || is_bash_stream_header(next) {
            return candidate;
        }
    }
    output
}

fn is_bash_stream_header(line: &str) -> bool {
    ["stdout", "stderr"].into_iter().any(|stream| {
        let Some(detail) = line
            .strip_prefix(stream)
            .and_then(|line| line.strip_prefix(':'))
        else {
            return false;
        };
        let detail = detail.trim();
        detail.is_empty()
            || detail
                .strip_suffix(" lines")
                .is_some_and(|count| count.parse::<usize>().is_ok())
            || (detail.contains(" bytes, showing first ") && detail.contains(" and last "))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BashCaptureTruncation {
    stream: &'static str,
    omitted_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CompactBashOutput {
    lines: Vec<String>,
    omitted_lines: usize,
    capture_truncations: Vec<BashCaptureTruncation>,
    panel_elided: bool,
}

fn bash_capture_footer(line: &str) -> Option<(&'static str, &str)> {
    ["stdout", "stderr"].into_iter().find_map(|stream| {
        line.strip_prefix("truncated_")
            .and_then(|line| line.strip_prefix(stream))
            .and_then(|line| line.strip_prefix('='))
            .map(|detail| (stream, detail))
    })
}

fn is_bash_complete_footer(line: &str) -> bool {
    ["stdout", "stderr"].into_iter().any(|stream| {
        line.strip_prefix("complete_")
            .and_then(|line| line.strip_prefix(stream))
            .is_some_and(|detail| detail == "=true")
    })
}

/// Project a bounded result into Pi-style tail output. Protocol envelope lines
/// are excluded; capture loss is retained separately because Ctrl+O can reveal
/// UI-tail omissions but cannot recover bytes discarded by the bash tool.
fn compact_bash_output(panel: &ToolPanel, expanded: bool) -> CompactBashOutput {
    let result = sanitize_for_terminal(final_bash_result(&panel.output));
    let mut capture_truncations = Vec::new();
    for line in result.lines().map(str::trim) {
        let Some((stream, detail)) = bash_capture_footer(line) else {
            continue;
        };
        if detail == "false" {
            continue;
        }
        let omitted_bytes = detail
            .split_whitespace()
            .find_map(|part| part.strip_prefix("omitted_bytes:"))
            .and_then(|count| count.parse::<usize>().ok());
        capture_truncations.push(BashCaptureTruncation {
            stream,
            omitted_bytes,
        });
    }

    let capture_was_truncated = !capture_truncations.is_empty();
    let failure_reason = panel.failure_reason.as_deref().map(str::trim);
    let mut content = Vec::new();
    let mut panel_elided = false;
    let mut protocol_error = false;
    let mut expect_stream_header = false;
    for (line_index, raw) in result.lines().enumerate() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if line_index == 0 && trimmed.starts_with("error ") {
            protocol_error = true;
            expect_stream_header = true;
            continue;
        }
        if trimmed.starts_with("exit=") && trimmed.contains("duration=") {
            expect_stream_header = true;
            continue;
        }
        if expect_stream_header && is_bash_stream_header(trimmed) {
            protocol_error = false;
            expect_stream_header = false;
            continue;
        }
        if bash_capture_footer(trimmed).is_some() || is_bash_complete_footer(trimmed) {
            expect_stream_header = true;
            continue;
        }
        if trimmed.is_empty()
            || trimmed == "(no output)"
            || (capture_was_truncated && trimmed == "...")
            || (content.is_empty() && failure_reason.is_some_and(|reason| reason == trimmed))
        {
            continue;
        }
        if trimmed == "… older tool output elided …" {
            panel_elided = true;
            continue;
        }
        content.push(line.to_owned());
        if !protocol_error {
            expect_stream_header = false;
        }
    }

    let omitted_lines = if expanded {
        0
    } else {
        let omitted_lines = content.len().saturating_sub(COMPACT_EXEC_OUTPUT_LINES);
        if omitted_lines > 0 {
            content.drain(..omitted_lines);
        }
        omitted_lines
    };
    CompactBashOutput {
        lines: content,
        omitted_lines,
        capture_truncations,
        panel_elided,
    }
}

fn compute_tool_metadata(panel: &ToolPanel) -> Option<String> {
    if !matches!(panel.name.as_str(), "bash" | "exec") {
        return None;
    }
    let output = final_bash_result(&panel.output);
    if let Some(duration) = output
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .find_map(|part| part.strip_prefix("duration="))
        .map(|value| value.trim_end_matches([',', ';']))
        .filter(|value| !value.is_empty())
    {
        return Some(
            if duration.chars().last().is_some_and(char::is_alphabetic) {
                duration.to_owned()
            } else {
                format!("{duration}s")
            },
        );
    }
    None
}

fn bash_content_gutter() -> usize {
    let action = "Bash";
    visible_width(action) + 6usize.saturating_sub(visible_width(action)).max(2)
}

fn render_compact_bash_output(
    panel: &ToolPanel,
    theme: &YggTheme,
    width: u16,
    expanded: bool,
    show_tool_duration: bool,
    output_indent: &str,
) -> Vec<String> {
    let compact = compact_bash_output(panel, expanded);
    let ellipsis = if theme.unicode() { "…" } else { "..." };
    let mut lines = Vec::new();
    let mut first_detail = true;
    let push_output = |lines: &mut Vec<String>, first_detail: &mut bool, output: String| {
        *first_detail = false;
        lines.extend(wrap_hanging(
            &subdued_text(theme, &output),
            output_indent,
            output_indent,
            width,
        ));
    };
    let push_metadata = |lines: &mut Vec<String>, first_detail: &mut bool, detail: String| {
        *first_detail = false;
        lines.extend(wrap_hanging(
            &subdued_text(theme, &detail),
            output_indent,
            output_indent,
            width,
        ));
    };
    if compact.panel_elided {
        push_metadata(
            &mut lines,
            &mut first_detail,
            format!(
                "{ellipsis} (older live output was elided before display; unavailable to expand)"
            ),
        );
    }
    for truncation in compact.capture_truncations {
        let detail = truncation
            .omitted_bytes
            .map_or_else(|| "some bytes".to_owned(), |bytes| format!("{bytes} bytes"));
        push_metadata(
            &mut lines,
            &mut first_detail,
            format!(
                "{ellipsis} ({} capture omitted {detail}; unavailable to expand)",
                truncation.stream
            ),
        );
    }
    if compact.omitted_lines > 0 {
        let unit = if compact.omitted_lines == 1 {
            "line"
        } else {
            "lines"
        };
        push_metadata(
            &mut lines,
            &mut first_detail,
            format!("{ellipsis} {} {unit} hidden", compact.omitted_lines),
        );
    }
    for output_line in compact.lines {
        push_output(&mut lines, &mut first_detail, output_line);
    }
    if first_detail {
        push_metadata(
            &mut lines,
            &mut first_detail,
            if panel.finished {
                "(no output)".to_owned()
            } else {
                "(waiting for output)".to_owned()
            },
        );
    }
    if show_tool_duration {
        if let Some(duration) = tool_metadata(panel) {
            lines.push(fit_line(
                &subdued_text(theme, &format!("{output_indent}Took {duration}")),
                width,
            ));
        }
    }
    lines
}

fn render_bash_row(
    command: &str,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
) -> Vec<String> {
    let action = "Bash";
    let action_gap = tool_value_indent_width(action).saturating_sub(visible_width(action));
    let prefix = format!(
        "{}{}",
        theme.bold(&theme.fg("foreground", action)),
        " ".repeat(action_gap)
    );
    let continuation = " ".repeat(bash_content_gutter());
    let content_width = width
        .saturating_sub(u16::try_from(visible_width(&prefix)).unwrap_or(u16::MAX))
        .max(1);
    let command = renderer.render_inline_syntax(command, "bash", content_width);
    let use_plain = theme.capabilities().color == crate::tui::terminal::ColorDepth::None;
    command
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 { &prefix } else { &continuation };
            let content = if use_plain { line.plain } else { line.styled };
            fit_line(&format!("{prefix}{content}"), width)
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct SurfacePlan<'a> {
    kind: &'static str,
    chrome: ThemeSurfaceChrome,
    heading: ThemeSurfaceHeading,
    label: Option<&'a str>,
    padding: u16,
    frame_left: u16,
    frame_width: u16,
    geometry: SurfaceGeometry,
}

fn transcript_surface_kind(block: &TranscriptBlock) -> &'static str {
    match block {
        TranscriptBlock::User { .. } => "user",
        TranscriptBlock::Assistant(_) => "assistant",
        TranscriptBlock::Reasoning(_) => "reasoning",
        TranscriptBlock::Tool(_) => "tool",
        TranscriptBlock::Shell(_) => "shell",
        TranscriptBlock::Outcome(_) => "outcome",
        TranscriptBlock::Notice(_) => "notice",
        TranscriptBlock::Compaction(_) => "compaction",
    }
}

fn surface_roles(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "user" => ("surface.user", "surface.user.border", "surface.user.label"),
        "assistant" => (
            "surface.assistant",
            "surface.assistant.border",
            "surface.assistant.label",
        ),
        "reasoning" => (
            "surface.reasoning",
            "surface.reasoning.border",
            "surface.reasoning.label",
        ),
        "tool" => ("surface.tool", "surface.tool.border", "surface.tool.label"),
        "shell" => (
            "surface.shell",
            "surface.shell.border",
            "surface.shell.label",
        ),
        "outcome" => (
            "surface.outcome",
            "surface.outcome.border",
            "surface.outcome.label",
        ),
        "notice" => (
            "surface.notice",
            "surface.notice.border",
            "surface.notice.label",
        ),
        "compaction" => (
            "surface.compaction",
            "surface.compaction.border",
            "surface.compaction.label",
        ),
        _ => ("text", "border", "muted"),
    }
}

fn natural_surface_width(block: &TranscriptBlock, theme: &YggTheme) -> u16 {
    let copy = match block {
        TranscriptBlock::Reasoning(reasoning) if !reasoning.reasoning_expanded => {
            collapsed_reasoning_lines(theme, reasoning, false).join("\n")
        }
        TranscriptBlock::Compaction(compaction) if !compaction.expanded => {
            format!("{} · (ctrl+o to view)", compaction.label)
        }
        _ => block_copy_text(block),
    };
    let natural = copy.lines().map(visible_width).max().unwrap_or(1);
    let inner_prefix = match block {
        TranscriptBlock::User { .. } => 2,
        TranscriptBlock::Reasoning(_) => visible_width(theme.glyph("reasoning")).saturating_add(1),
        TranscriptBlock::Tool(_) => 8,
        TranscriptBlock::Notice(_) | TranscriptBlock::Compaction(_) => {
            visible_width(theme.glyph("note")).saturating_add(1)
        }
        TranscriptBlock::Shell(_) => visible_width(theme.glyph("shell")).saturating_add(1),
        TranscriptBlock::Assistant(_) | TranscriptBlock::Outcome(_) => 0,
    };
    u16::try_from(natural.saturating_add(inner_prefix)).unwrap_or(u16::MAX)
}

fn compile_surface_plan<'a>(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &'a YggTheme,
    outer_width: u16,
) -> SurfacePlan<'a> {
    let layout = theme.layout_for_width(outer_width);
    let kind = transcript_surface_kind(block);
    let resolved = theme.surface_for_width(kind, outer_width);
    let inset = if matches!(block, TranscriptBlock::User { .. }) {
        0
    } else {
        layout.transcript_inset.min(outer_width.saturating_sub(1))
    };
    let available = outer_width.saturating_sub(inset).max(1);
    let mut chrome = resolved.chrome;
    let mut heading = if resolved.label.is_some() {
        resolved.heading
    } else {
        ThemeSurfaceHeading::None
    };
    let mut padding = resolved.padding;

    let overhead_for = |chrome: ThemeSurfaceChrome, padding: u16| -> u16 {
        let horizontal_padding = padding.saturating_mul(2);
        match chrome {
            ThemeSurfaceChrome::Plain | ThemeSurfaceChrome::Band | ThemeSurfaceChrome::Rule => {
                horizontal_padding
            }
            ThemeSurfaceChrome::Rail => u16::try_from(visible_width(theme.glyph("rail")))
                .unwrap_or(u16::MAX)
                .saturating_add(1)
                .saturating_add(horizontal_padding),
            ThemeSurfaceChrome::Card => 2u16.saturating_add(horizontal_padding),
        }
    };
    let mut overhead = overhead_for(chrome, padding);
    if available <= overhead.saturating_add(3) {
        chrome = ThemeSurfaceChrome::Plain;
        heading = ThemeSurfaceHeading::None;
        padding = 0;
        overhead = 0;
    }

    let frame_limit = resolved
        .max_width
        .unwrap_or(available)
        .min(available)
        .max(1);
    let frame_width = match resolved.width {
        ThemeSurfaceWidth::Full => frame_limit,
        ThemeSurfaceWidth::Content => {
            let requested = natural_surface_width(block, theme).saturating_add(overhead);
            requested.max(frame_limit.min(12)).min(frame_limit)
        }
    };
    if frame_width <= overhead {
        chrome = ThemeSurfaceChrome::Plain;
        heading = ThemeSurfaceHeading::None;
        padding = 0;
        overhead = 0;
    }
    let frame_offset = match resolved.align {
        ThemeSurfaceAlign::Left => 0,
        ThemeSurfaceAlign::Center => available.saturating_sub(frame_width) / 2,
        ThemeSurfaceAlign::Right => available.saturating_sub(frame_width),
    };
    let frame_left = inset.saturating_add(frame_offset);
    let chrome_left = match chrome {
        ThemeSurfaceChrome::Rail => u16::try_from(visible_width(theme.glyph("rail")))
            .unwrap_or(u16::MAX)
            .saturating_add(1),
        ThemeSurfaceChrome::Card => 1,
        ThemeSurfaceChrome::Plain | ThemeSurfaceChrome::Band | ThemeSurfaceChrome::Rule => 0,
    };
    let content_left = frame_left
        .saturating_add(chrome_left)
        .saturating_add(padding);
    let content_width = frame_width.saturating_sub(overhead).max(1);
    let is_user_card = kind == "user" && chrome == ThemeSurfaceChrome::Card;
    let leading_rows = usize::from(
        chrome == ThemeSurfaceChrome::Card
            || chrome == ThemeSurfaceChrome::Rule
            || heading != ThemeSurfaceHeading::None,
    ) + usize::from(is_user_card);
    let trailing_rows = usize::from(chrome == ThemeSurfaceChrome::Card) + usize::from(is_user_card);
    SurfacePlan {
        kind,
        chrome,
        heading,
        label: resolved.label,
        padding,
        frame_left,
        frame_width,
        geometry: SurfaceGeometry {
            transition_rows: transcript_transition_rows(previous, layout.density),
            leading_rows,
            trailing_rows,
            content_left,
            content_width,
        },
    }
}

fn padded_to_width(line: &str, width: u16) -> String {
    let line = fit_line(line, width);
    let padding = usize::from(width).saturating_sub(visible_width(&line));
    if padding == 0 {
        line
    } else {
        format!("{line}{}", " ".repeat(padding))
    }
}

fn horizontal_rule(theme: &YggTheme, width: usize) -> String {
    theme.glyph("horizontal").repeat(width)
}

fn styled_surface_heading(plan: &SurfacePlan<'_>, theme: &YggTheme) -> String {
    let (_, border_role, label_role) = surface_roles(plan.kind);
    let frame_width = usize::from(plan.frame_width);
    let left = theme.glyph("top_left");
    let right = theme.glyph("top_right");
    let label = plan.label.unwrap_or("");
    let styled_label = theme.apply_semantic_role(label_role, label);

    let raw = if plan.chrome == ThemeSurfaceChrome::Card {
        let middle_width = frame_width.saturating_sub(2);
        if label.is_empty() || plan.heading == ThemeSurfaceHeading::None {
            format!("{left}{}{right}", horizontal_rule(theme, middle_width))
        } else {
            let label_width = visible_width(label).min(middle_width.saturating_sub(2));
            let rest = middle_width.saturating_sub(label_width.saturating_add(2));
            match plan.heading {
                ThemeSurfaceHeading::Inline => format!(
                    "{left}{styled_label} {}{right}",
                    horizontal_rule(theme, middle_width.saturating_sub(label_width + 1))
                ),
                ThemeSurfaceHeading::Tab => format!(
                    "{left} {styled_label} {}{right}",
                    horizontal_rule(theme, rest)
                ),
                ThemeSurfaceHeading::Overline => format!(
                    "{left}{} {styled_label} {right}",
                    horizontal_rule(theme, rest)
                ),
                ThemeSurfaceHeading::None => unreachable!("handled above"),
            }
        }
    } else if plan.chrome == ThemeSurfaceChrome::Rule
        || plan.heading == ThemeSurfaceHeading::Overline
    {
        if label.is_empty() || plan.heading == ThemeSurfaceHeading::None {
            horizontal_rule(theme, frame_width)
        } else {
            let used = visible_width(label).saturating_add(1).min(frame_width);
            format!(
                "{styled_label} {}",
                horizontal_rule(theme, frame_width - used)
            )
        }
    } else if plan.heading == ThemeSurfaceHeading::Tab {
        let label_width = visible_width(label).min(frame_width.saturating_sub(4));
        let tail = frame_width.saturating_sub(label_width.saturating_add(4));
        format!(
            "{left} {styled_label} {}{right}",
            horizontal_rule(theme, tail)
        )
    } else {
        styled_label
    };
    theme.apply_semantic_role_layered(border_role, &padded_to_width(&raw, plan.frame_width))
}

fn render_surface_content_line(
    line: &str,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    prompt_color: Option<&str>,
) -> String {
    let (content_role, border_role, _) = surface_roles(plan.kind);
    let content = fit_line(line, plan.geometry.content_width);
    let left_padding = " ".repeat(usize::from(plan.padding));
    let right_padding = " ".repeat(usize::from(plan.padding));
    let paint_prompt = |text: String, width: u16| {
        let text = padded_to_width(&strip_terminal_sequences(&text), width);
        theme.prompt_color_cell(prompt_color, &text)
    };
    match plan.chrome {
        ThemeSurfaceChrome::Card => {
            let inner_width = plan.frame_width.saturating_sub(2);
            let inner = padded_to_width(
                &format!("{left_padding}{content}{right_padding}"),
                inner_width,
            );
            let inner = if prompt_color.is_some() {
                paint_prompt(inner, inner_width)
            } else {
                theme.apply_semantic_role_layered(content_role, &inner)
            };
            format!(
                "{}{}{}",
                theme.apply_semantic_role(border_role, theme.glyph("vertical")),
                inner,
                theme.apply_semantic_role(border_role, theme.glyph("vertical")),
            )
        }
        ThemeSurfaceChrome::Band => {
            let inner = padded_to_width(
                &format!("{left_padding}{content}{right_padding}"),
                plan.frame_width,
            );
            if prompt_color.is_some() {
                paint_prompt(inner, plan.frame_width)
            } else {
                theme.apply_semantic_role_layered(content_role, &inner)
            }
        }
        ThemeSurfaceChrome::Rail => {
            let rail = theme.apply_semantic_role(border_role, theme.glyph("rail"));
            let body = format!(" {left_padding}{content}{right_padding}");
            let body = if prompt_color.is_some() {
                let rail_width = u16::try_from(visible_width(theme.glyph("rail")))
                    .unwrap_or(u16::MAX)
                    .min(plan.frame_width);
                paint_prompt(body, plan.frame_width.saturating_sub(rail_width))
            } else {
                theme.apply_semantic_role_layered(content_role, &body)
            };
            fit_line(&format!("{rail}{body}"), plan.frame_width)
        }
        ThemeSurfaceChrome::Plain | ThemeSurfaceChrome::Rule => {
            let body = format!("{left_padding}{content}{right_padding}");
            if prompt_color.is_some() {
                paint_prompt(body, plan.frame_width)
            } else {
                theme.apply_semantic_role_layered(content_role, &body)
            }
        }
    }
}

fn event_margin_marker(
    block: &TranscriptBlock,
    theme: &YggTheme,
    active_dot_visible: bool,
    collapsed_reasoning: bool,
) -> Option<String> {
    let dot = if theme.unicode() { "•" } else { "." };
    match block {
        TranscriptBlock::User { .. } | TranscriptBlock::Outcome(_) | TranscriptBlock::Notice(_) => {
            None
        }
        TranscriptBlock::Reasoning(reasoning) if collapsed_reasoning => {
            Some(if active_dot_visible {
                theme.model_fg(reasoning.model_lab, dot)
            } else {
                " ".to_owned()
            })
        }
        TranscriptBlock::Reasoning(_) => None,
        TranscriptBlock::Tool(panel) if !panel.finished => Some(if active_dot_visible {
            theme.fg("foreground", dot)
        } else {
            " ".to_owned()
        }),
        TranscriptBlock::Tool(panel) if panel.is_error => {
            Some(theme.settled_event_dot("error", dot))
        }
        TranscriptBlock::Tool(panel) if matches!(panel.name.as_str(), "bash" | "exec") => {
            Some(theme.settled_event_dot("success", dot))
        }
        TranscriptBlock::Shell(shell) if shell.running => Some(if active_dot_visible {
            theme.fg("foreground", dot)
        } else {
            " ".to_owned()
        }),
        TranscriptBlock::Shell(shell) => Some(theme.settled_event_dot(
            if shell.exit_code == 0 {
                "success"
            } else {
                "error"
            },
            dot,
        )),
        _ => Some(theme.settled_event_dot("neutral", dot)),
    }
}

#[allow(clippy::too_many_arguments)]
fn decorate_surface(
    content: Vec<String>,
    block: &TranscriptBlock,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    outer_width: u16,
    prompt_color: Option<&str>,
    active_dot_visible: bool,
    collapsed_reasoning: bool,
) -> Vec<String> {
    let mut rows = Vec::with_capacity(
        plan.geometry.transition_rows
            + plan.geometry.leading_rows
            + content.len()
            + plan.geometry.trailing_rows,
    );
    rows.extend(std::iter::repeat_n(
        String::new(),
        plan.geometry.transition_rows,
    ));
    if plan.geometry.leading_rows > 0 {
        rows.push(styled_surface_heading(plan, theme));
    }
    if plan.geometry.leading_rows > 1 {
        rows.extend(std::iter::repeat_n(
            render_surface_content_line("", plan, theme, prompt_color),
            plan.geometry.leading_rows - 1,
        ));
    }
    rows.extend(
        content
            .iter()
            .map(|line| render_surface_content_line(line, plan, theme, prompt_color)),
    );
    if plan.geometry.trailing_rows > 1 {
        rows.extend(std::iter::repeat_n(
            render_surface_content_line("", plan, theme, prompt_color),
            plan.geometry.trailing_rows - 1,
        ));
    }
    if plan.geometry.trailing_rows > 0 {
        let (_, border_role, _) = surface_roles(plan.kind);
        let middle = horizontal_rule(theme, usize::from(plan.frame_width.saturating_sub(2)));
        let bottom = format!(
            "{}{}{}",
            theme.glyph("bottom_left"),
            middle,
            theme.glyph("bottom_right")
        );
        rows.push(theme.apply_semantic_role_layered(border_role, &bottom));
    }

    let marker = event_margin_marker(block, theme, active_dot_visible, collapsed_reasoning);
    let mut marker_pending = true;
    rows.into_iter()
        .enumerate()
        .map(|(row, line)| {
            if row < plan.geometry.transition_rows || line.is_empty() {
                String::new()
            } else {
                let frame_left = usize::from(plan.frame_left);
                let prefix = if marker_pending && marker.is_some() {
                    marker_pending = false;
                    let marker = marker.as_deref().expect("checked above");
                    if frame_left >= 2 {
                        format!("{}{marker} ", " ".repeat(frame_left - 2))
                    } else if frame_left == 1 {
                        marker.to_owned()
                    } else {
                        format!("{marker} ")
                    }
                } else {
                    " ".repeat(frame_left)
                };
                fit_line(&format!("{prefix}{line}"), outer_width)
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render_block_planned(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    reasoning_renderer: &RichRenderer,
    outer_width: u16,
    verbose_tools: bool,
    active_dot_visible: bool,
) -> RenderedTranscriptBlock {
    let layout = theme.layout_for_width(outer_width);
    let plan = compile_surface_plan(previous, block, theme, outer_width);
    let width = plan.geometry.content_width;
    let content_background = matches!(
        plan.chrome,
        ThemeSurfaceChrome::Card | ThemeSurfaceChrome::Band
    )
    .then(|| theme.semantic_style(surface_roles(plan.kind).0).background)
    .filter(|background| *background != Color::Default);
    let collapsed_reasoning = matches!(
        block,
        TranscriptBlock::Reasoning(reasoning)
            if !reasoning.reasoning_expanded && !verbose_tools
    );
    let lines = match block {
        TranscriptBlock::User {
            text,
            model_lab,
            prompt_color,
            ..
        } => render_user_prompt(
            text,
            model_lab,
            prompt_color.as_deref(),
            rich_renderer,
            theme,
            width,
        ),
        TranscriptBlock::Assistant(assistant) => finish_transcript_block(
            assistant.render_on_surface(rich_renderer, theme, width, content_background),
        ),
        TranscriptBlock::Reasoning(reasoning) => render_reasoning_on_surface(
            reasoning,
            reasoning_renderer,
            theme,
            width,
            verbose_tools,
            content_background,
            true,
        ),
        TranscriptBlock::Tool(panel) => {
            let compact_bash = matches!(panel.name.as_str(), "bash" | "exec")
                && panel.display.shell_command.is_some();
            let tool = if panel.display.shell_command.is_some() {
                "Bash"
            } else {
                tool_display_label(&panel.name)
            };
            let output_indent = tool_value_indent(tool);
            let mut lines = if let Some(command) = panel.display.shell_command.as_deref() {
                render_bash_row(command, rich_renderer, theme, width)
            } else {
                let compact = width < 60;
                let summary = if !panel.finished {
                    if compact {
                        &panel.display.compact_active
                    } else {
                        &panel.display.active
                    }
                } else if panel.is_error {
                    if compact {
                        &panel.display.compact_failure
                    } else {
                        &panel.display.failure
                    }
                } else if compact {
                    &panel.display.compact_success
                } else {
                    &panel.display.success
                };
                let tool = tool_display_label(&panel.name);
                // The margin dot owns lifecycle colour. Tool text stays
                // neutral so failures do not wash the whole event red.
                let label = theme.bold(&theme.fg("foreground", tool));
                let text =
                    without_redundant_tool_lead(&panel.name, &sanitize_for_terminal(summary));
                let text = theme.fg("muted", &text);
                let gap = tool_value_indent_width(tool).saturating_sub(visible_width(tool));
                let label_prefix = format!("{label}{}", " ".repeat(gap));
                let continuation = " ".repeat(visible_width(&label_prefix));
                wrap_hanging(&text, &label_prefix, &continuation, width)
            };

            if !panel.is_error {
                match panel.name.as_str() {
                    "bash" | "exec" if compact_bash => lines.extend(render_compact_bash_output(
                        panel,
                        theme,
                        width,
                        verbose_tools,
                        layout.show_tool_duration,
                        &output_indent,
                    )),
                    "search" => lines.extend(render_compact_tool_output(
                        panel,
                        theme,
                        width,
                        verbose_tools,
                        &output_indent,
                    )),
                    "edit" | "write" if tool_diff(panel).is_some() => {
                        lines.extend(render_diff_only(
                            panel,
                            rich_renderer,
                            theme,
                            width,
                            verbose_tools,
                            &output_indent,
                        ))
                    }
                    _ => {}
                }
            }
            finish_transcript_block(lines)
        }
        TranscriptBlock::Outcome(outcome) => render_outcome(outcome, theme, width),
        TranscriptBlock::Notice(text) => {
            let marker = theme.glyph("note");
            let marker = if theme.has_semantic_role("notification") {
                theme.apply_semantic_role("notification", marker)
            } else {
                theme.fg("model_accent", marker)
            };
            let prefix = format!("{marker} ");
            let continuation = " ".repeat(visible_width(&prefix));
            let lines = wrap_hanging(&sanitize_for_terminal(text), &prefix, &continuation, width);
            finish_transcript_block(lines)
        }
        TranscriptBlock::Compaction(compaction) => {
            let marker = theme.glyph("note");
            let prefix = format!("{} ", theme.fg("model_accent", marker));
            let continuation = " ".repeat(visible_width(&prefix));
            let expanded = compaction.expanded || verbose_tools;
            let action = if expanded {
                "ctrl+o to collapse"
            } else {
                "ctrl+o to view"
            };
            let label = format!("{} · ({action})", sanitize_for_terminal(&compaction.label));
            let mut lines = wrap_hanging(&label, &prefix, &continuation, width);
            if expanded {
                let summary = AssistantBlock::finalized(compaction.summary.clone());
                let summary_width = width.saturating_sub(2).max(1);
                lines.extend(
                    summary
                        .render_on_surface(rich_renderer, theme, summary_width, content_background)
                        .into_iter()
                        .map(|line| {
                            if line.is_empty() {
                                String::new()
                            } else {
                                fit_line(&format!("  {line}"), width)
                            }
                        }),
                );
            }
            finish_transcript_block(lines)
        }
        TranscriptBlock::Shell(shell) => {
            let marker = theme.glyph("shell");
            let prefix = format!("{} ", theme.bold(&theme.fg("model_accent", marker)));
            let status = if shell.running {
                theme.dim("…")
            } else if shell.exit_code == 0 {
                theme.dim("[ok]")
            } else {
                theme.fg("error", "[failed]")
            };
            let mut lines = vec![fit_line(
                &format!(
                    "{} {} {}",
                    prefix,
                    theme.dim(&sanitize_for_terminal(&shell.command)),
                    status,
                ),
                width,
            )];
            lines.extend(render_shell_output(shell, theme, width, verbose_tools));
            finish_transcript_block(lines)
        }
    };

    if lines.is_empty() {
        return RenderedTranscriptBlock {
            lines,
            geometry: SurfaceGeometry::default(),
        };
    }
    let prompt_color = match block {
        TranscriptBlock::User { prompt_color, .. } => prompt_color.as_deref(),
        _ => None,
    };
    let lines = decorate_surface(
        lines,
        block,
        &plan,
        theme,
        outer_width,
        prompt_color,
        active_dot_visible,
        collapsed_reasoning,
    );
    RenderedTranscriptBlock {
        lines,
        geometry: plan.geometry,
    }
}

#[cfg(test)]
fn render_block(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    reasoning_renderer: &RichRenderer,
    outer_width: u16,
    verbose_tools: bool,
) -> Vec<String> {
    render_block_planned(
        previous,
        block,
        theme,
        rich_renderer,
        reasoning_renderer,
        outer_width,
        verbose_tools,
        true,
    )
    .lines
}

/// Clean semantic text used by the application-owned selection/copy path.
/// It intentionally never uses visual rows, ANSI styling, borders, elision,
/// composer text, or footer text.
fn block_copy_text(block: &TranscriptBlock) -> String {
    match block {
        TranscriptBlock::User { text, .. } | TranscriptBlock::Notice(text) => {
            sanitize_for_terminal(text)
        }
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
        TranscriptBlock::Outcome(outcome) => match outcome {
            RunOutcome::Completed { elapsed, summary } => format!(
                "completed · {} · {} actions",
                format_duration(*elapsed),
                summary.tool_calls
            ),
            RunOutcome::CompletedWithWarnings { elapsed, .. } => {
                format!("completed with notes · {}", format_duration(*elapsed))
            }
            RunOutcome::Failed { elapsed, reason } => format!(
                "failed · {}\n{}",
                format_duration(*elapsed),
                bounded_outcome_detail(reason)
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
fn semantic_selected_text(state: &ShellState) -> Option<String> {
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

#[cfg(test)]
fn markdown_lines(text: &str, theme: &YggTheme, width: u16) -> Vec<String> {
    let assistant = AssistantBlock::finalized(text.to_owned());
    assistant.render(&theme.rich_renderer(), theme, width)
}

#[cfg(test)]
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
pub(crate) struct EditorVisualLine {
    /// Source range owned by this visual row. A soft-wrap separator can be
    /// included in `end` while omitted from display via `visible_end`.
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) visible_end: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct EditorLayout {
    pub(crate) lines: Vec<EditorVisualLine>,
    pub(crate) cursor_row: usize,
}

/// Cache key for the editor layout so we don't re-wrap on every animation
/// frame when the editor content hasn't changed.
#[derive(Clone, Debug)]
struct EditorLayoutCache {
    width: u16,
    text_len: usize,
    cursor: usize,
    text_hash: u64,
    layout: EditorLayout,
}

/// Quick FNV-1a hash for cache-key purposes; we only need to detect changes.
fn hash_str(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn prompt_content_width(width: u16) -> usize {
    // Prompt marker + one separating space. Continuation rows use two spaces.
    usize::from(width).saturating_sub(2)
}

fn editor_wrap_width(width: u16) -> usize {
    // Reserve one cell for the rendered cursor.
    prompt_content_width(width).saturating_sub(1).max(1)
}

/// Normalize terminal paste line endings before placing them in the editor.
/// Bracketed paste must never submit the prompt or turn CRLF into visual `\r`
/// characters in a multi-line editor.
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn editor_visual_lines(text: &str, wrap_width: usize) -> Vec<EditorVisualLine> {
    let mut lines = Vec::new();
    let mut logical_start = 0;

    // Newlines are hard boundaries and are intentionally excluded from the
    // adjacent visual rows. `split_inclusive` preserves an empty row after a
    // trailing newline below.
    for logical in text.split_inclusive('\n') {
        let content_len = logical.strip_suffix('\n').map_or(logical.len(), str::len);
        let logical_end = logical_start + content_len;
        let mut start = logical_start;

        if start == logical_end {
            lines.push(EditorVisualLine {
                start,
                end: logical_end,
                visible_end: logical_end,
            });
        } else {
            while start < logical_end {
                let mut columns = 0usize;
                let mut hard_end = logical_end;
                let mut overflow_is_whitespace = false;
                // Byte range of the latest separator run after visible text.
                let mut word_break: Option<(usize, usize)> = None;
                let mut saw_non_whitespace = false;
                let mut consumed_any = false;

                for (relative, character) in text[start..logical_end].char_indices() {
                    let offset = start + relative;
                    let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                    if consumed_any && columns.saturating_add(character_width) > wrap_width {
                        hard_end = offset;
                        overflow_is_whitespace = character.is_whitespace();
                        break;
                    }
                    columns = columns.saturating_add(character_width);
                    consumed_any = true;
                    if character.is_whitespace() {
                        // A leading indentation is not a word boundary. Once a
                        // word has appeared, retain the whole separator run so
                        // it can be consumed (but visually trimmed) at a wrap.
                        if saw_non_whitespace {
                            let separator_end = offset + character.len_utf8();
                            match word_break.as_mut() {
                                Some((_, end)) if *end == offset => *end = separator_end,
                                _ => word_break = Some((offset, separator_end)),
                            }
                        }
                    } else {
                        saw_non_whitespace = true;
                    }
                }

                if hard_end == logical_end {
                    lines.push(EditorVisualLine {
                        start,
                        end: logical_end,
                        visible_end: logical_end,
                    });
                    break;
                }

                if overflow_is_whitespace {
                    // A word exactly filled the row. Consume and visually trim
                    // the separator run rather than creating a leading-space
                    // or whitespace-only row before the next word.
                    let mut separator_end = hard_end;
                    for character in text[hard_end..logical_end].chars() {
                        if !character.is_whitespace() {
                            break;
                        }
                        separator_end += character.len_utf8();
                    }
                    lines.push(EditorVisualLine {
                        start,
                        end: separator_end,
                        visible_end: hard_end,
                    });
                    start = separator_end;
                } else if let Some((separator_start, separator_end)) = word_break {
                    // Move the entire word that overflowed to the next row and
                    // hide only its separating whitespace. Source offsets stay
                    // owned by a row, so cursor motion remains lossless.
                    lines.push(EditorVisualLine {
                        start,
                        end: separator_end,
                        visible_end: separator_start,
                    });
                    start = separator_end;
                } else {
                    // A single word is wider than the composer: hard-wrap it.
                    lines.push(EditorVisualLine {
                        start,
                        end: hard_end,
                        visible_end: hard_end,
                    });
                    start = hard_end;
                }
            }
        }

        logical_start += logical.len();
    }

    // `split_inclusive` does not produce a final empty item. Keep an editable
    // row for an empty editor and after a trailing hard newline.
    if text.is_empty() || text.ends_with('\n') {
        lines.push(EditorVisualLine {
            start: text.len(),
            end: text.len(),
            visible_end: text.len(),
        });
    }
    lines
}

pub(crate) fn editor_layout(text: &str, cursor: usize, width: u16) -> EditorLayout {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }

    let lines = editor_visual_lines(text, editor_wrap_width(width));

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
    visible_width(&text[line.start..cursor.clamp(line.start, line.visible_end)])
}

fn editor_offset_at_column(text: &str, line: &EditorVisualLine, target: usize) -> usize {
    let mut offset = line.start;
    let mut column: usize = 0;
    for (relative, character) in text[line.start..line.visible_end].char_indices() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if column.saturating_add(width) > target {
            break;
        }
        column = column.saturating_add(width);
        offset = line.start + relative + character.len_utf8();
    }
    offset
}

#[allow(dead_code)]
fn prompt_cursor(_theme: &YggTheme) -> &'static str {
    CURSOR_MARKER
}

pub(crate) fn fit_line(line: &str, width: u16) -> String {
    let width = usize::from(width);
    if visible_width(line) <= width {
        line.to_owned()
    } else {
        sexy_tui_rs::truncate_to_width(line, width, Some(""))
    }
}

#[allow(dead_code)]
fn render_prompt_box(state: &ShellState, width: u16, max_content_rows: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let marker = state.theme.fg("model_accent", prompt_marker(&state.theme));
    let cursor_glyph = state.theme.fg("model_accent", prompt_cursor(&state.theme));
    let (editor, editor_cursor) = sanitized_editor(&state.editor, state.editor_cursor);
    if editor.is_empty() {
        return vec![fit_line(&format!("{marker} {cursor_glyph}"), width)];
    }

    let layout = editor_layout(&editor, editor_cursor, width);
    let visible_rows = max_content_rows.max(1).min(layout.lines.len());
    let mut start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let end = (start + visible_rows).min(layout.lines.len());
    if end.saturating_sub(start) < visible_rows {
        start = end.saturating_sub(visible_rows);
    }

    let mut rendered = Vec::with_capacity(end.saturating_sub(start));
    for index in start..end {
        let line = &layout.lines[index];
        let content = if index == layout.cursor_row {
            let cursor = editor_cursor.clamp(line.start, line.visible_end);
            let before = &editor[line.start..cursor];
            let after = &editor[cursor..line.visible_end];
            format!("{before}{cursor_glyph}{after}")
        } else {
            editor[line.start..line.visible_end].to_owned()
        };
        let prefix = if index == 0 {
            format!("{marker} ")
        } else {
            "  ".to_owned()
        };
        rendered.push(fit_line(&format!("{prefix}{content}"), width));
    }
    rendered
}

#[derive(Clone, Debug)]
struct InputSlashSuggestion {
    name: String,
    description: String,
    argument_hint: Option<String>,
    accepts_argument: bool,
}

fn input_slash_suggestions(state: &ShellState) -> Vec<InputSlashSuggestion> {
    let Some(query) = state.editor.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) || query.contains('\n') {
        return Vec::new();
    }
    let mut suggestions = commands::slash_suggestions(&state.editor)
        .into_iter()
        .map(|command| InputSlashSuggestion {
            name: command.name.to_owned(),
            description: command.description.to_owned(),
            argument_hint: None,
            accepts_argument: command.accepts_argument,
        })
        .collect::<Vec<_>>();
    for template in state
        .prompt_templates
        .iter()
        .filter(|template| template.name.starts_with(query))
    {
        if suggestions
            .iter()
            .any(|suggestion| suggestion.name == template.name)
        {
            continue;
        }
        suggestions.push(InputSlashSuggestion {
            name: template.name.clone(),
            description: format!("prompt · {}", template.description),
            argument_hint: template.argument_hint.clone(),
            accepts_argument: true,
        });
    }
    for (name, description) in state
        .extension_commands
        .iter()
        .filter(|(name, _)| name.starts_with(query))
    {
        if suggestions
            .iter()
            .any(|suggestion| suggestion.name == *name)
        {
            continue;
        }
        suggestions.push(InputSlashSuggestion {
            name: name.clone(),
            description: format!("extension · {description}"),
            argument_hint: None,
            accepts_argument: true,
        });
    }
    suggestions
}

fn render_slash_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if state.slash_popup_dismissed || max_rows < 2 {
        return Vec::new();
    }
    let suggestions = input_slash_suggestions(state);
    if suggestions.is_empty() {
        return Vec::new();
    }

    let item_rows = max_rows.saturating_sub(1).max(1);
    let selected = state
        .slash_selection
        .min(suggestions.len().saturating_sub(1));
    let max_start = suggestions.len().saturating_sub(item_rows);
    let mut start = state.slash_scroll.min(max_start);
    if selected < start {
        start = selected;
    } else if selected >= start.saturating_add(item_rows) {
        start = selected + 1 - item_rows;
    }
    start = start.min(max_start);
    let end = start.saturating_add(item_rows).min(suggestions.len());

    let heading = if suggestions.len() > item_rows {
        format!("  commands  {}–{}/{}", start + 1, end, suggestions.len())
    } else {
        "  commands".to_owned()
    };
    let mut lines = vec![state.theme.fg("muted", &fit_line(&heading, width))];
    let marker = state.theme.glyph("prompt");
    let label_width = suggestions[start..end]
        .iter()
        .map(|command| {
            visible_width(&format!(
                "/{}{}",
                command.name,
                command
                    .argument_hint
                    .as_deref()
                    .map(|hint| format!(" {hint}"))
                    .unwrap_or_default()
            ))
        })
        .max()
        .unwrap_or(1)
        .min(30)
        .min(usize::from(width).saturating_sub(6).max(1));
    for (index, command) in suggestions[start..end].iter().enumerate() {
        let absolute = start + index;
        let selected_row = absolute == selected;
        let prefix = if selected_row { marker } else { " " };
        let raw_label = format!(
            "/{}{}",
            command.name,
            command
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default()
        );
        let label = sexy_tui_rs::truncate_to_width(
            &raw_label,
            label_width,
            Some(if state.theme.unicode() { "…" } else { "..." }),
        );
        let label = format!(
            "{label}{}",
            " ".repeat(label_width.saturating_sub(visible_width(&label)))
        );
        let description_width =
            usize::from(width).saturating_sub(visible_width(prefix) + visible_width(&label) + 4);
        let description = sexy_tui_rs::truncate_to_width(
            &command.description,
            description_width,
            Some(if state.theme.unicode() { "…" } else { "..." }),
        );
        let row = format!("  {prefix} {label}  {description}");
        lines.push(if selected_row {
            state
                .theme
                .bold(&state.theme.fg("model_accent", &fit_line(&row, width)))
        } else {
            state.theme.fg("muted", &fit_line(&row, width))
        });
    }
    lines
}

fn render_mention_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if max_rows == 0 || state.editor_cursor != state.editor.len() {
        return Vec::new();
    }
    let Some(query) = composer::active_mention(&state.editor) else {
        return Vec::new();
    };

    // When the query looks like a path (contains / or starts with .),
    // do a live filesystem listing instead of searching the pre-built index.
    let looks_like_path = query.contains('/') || query.starts_with('.') || query.contains('\\');
    let matches: Vec<String> = if looks_like_path {
        let Some(root) = &state.workspace else {
            return Vec::new();
        };
        composer::live_path_matches(root, query, 5)
    } else {
        let Some(files) = state.file_index.as_ref() else {
            return Vec::new();
        };
        composer::mention_matches(files, query, 5)
            .into_iter()
            .map(str::to_owned)
            .collect()
    };
    if matches.is_empty() {
        return Vec::new();
    }

    let heading = if state.theme.unicode() {
        "  project files · tab completes"
    } else {
        "  project files - tab completes"
    };
    let mut lines = vec![state.theme.fg("model_accent", heading)];
    let item_rows = max_rows.saturating_sub(1).min(5);
    let available_width = usize::from(width).saturating_sub(2);
    for (index, path) in matches.into_iter().take(item_rows).enumerate() {
        let safe_path = sanitize_for_terminal(&path);
        let line = sexy_tui_rs::truncate_to_width(&safe_path, available_width, None);
        let line = format!("  {line}");
        lines.push(if index == 0 {
            state.theme.fg("model_accent", &line)
        } else {
            state.theme.dim(&line)
        });
    }
    lines
}

fn render_input_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    let slash = render_slash_suggestions(state, width, max_rows);
    if slash.is_empty() {
        render_mention_suggestions(state, width, max_rows)
    } else {
        slash
    }
}

fn render_pending_steering(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if state.steering_queue.is_empty() || max_rows == 0 {
        return Vec::new();
    }

    let count = state.steering_queue.len();
    let heading = if count == 1 {
        format!("Steering prompt{}queued", semantic_separator(&state.theme))
    } else {
        format!(
            "Steering prompts{}{} queued",
            semantic_separator(&state.theme),
            count
        )
    };
    let mut lines = vec![format!(
        "  {}",
        state.theme.bold(&state.theme.fg("model_accent", &heading))
    )];
    let item_rows = max_rows.saturating_sub(1);
    if item_rows == 0 {
        return lines;
    }

    let visible = state.steering_queue.len().min(item_rows);
    for message in state.steering_queue.iter().take(visible) {
        // Keep each queued message on one predictable row so a burst of
        // steering prompts cannot consume the whole transcript viewport.
        let line_separator = if state.theme.unicode() {
            " ↵ "
        } else {
            " / "
        };
        let compact = sanitize_for_terminal(&message.display).replace(['\r', '\n'], line_separator);
        let arrow = if state.theme.unicode() { "↳" } else { "->" };
        let prefix = format!("    {} ", state.theme.fg("model_accent", arrow));
        let line = format!("{prefix}{}", state.theme.fg("muted", &compact));
        lines.push(fit_line(&line, width));
    }
    let hidden = state.steering_queue.len().saturating_sub(visible);
    if hidden > 0 {
        lines.push(state.theme.dim(&format!(
            "    {} {hidden} more steering prompts",
            if state.theme.unicode() { "…" } else { "..." }
        )));
    }
    lines.truncate(max_rows);
    lines
}

fn welcome_workspace(state: &ShellState) -> String {
    let Some(path) = state.workspace.as_deref() else {
        return "workspace unavailable".to_owned();
    };
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(relative) = path.strip_prefix(home) {
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}

fn render_welcome_card(
    state: &ShellState,
    width: u16,
    max_rows: usize,
    now: Instant,
) -> Vec<String> {
    let Some(started) = state.startup_card_started_at else {
        return Vec::new();
    };
    if state.overlay.is_some() || width < 24 || max_rows < 7 {
        return Vec::new();
    }

    const ROWS: usize = 6;
    let elapsed = if state.theme.capabilities().animation && state.welcome_is_mutable() {
        now.saturating_duration_since(started).as_secs_f32()
    } else {
        crate::tui::splash::DURATION
    };
    let logo_width = (usize::from(width) / 3).clamp(14, 24);
    let adaptive_accent = state
        .theme
        .is_compiled_default()
        .then(|| state.theme.model_rgb(state.model_lab))
        .flatten();
    let logo =
        crate::tui::splash::render_logo(&state.theme, logo_width, ROWS, elapsed, adaptive_accent);

    let model = if state.model_display.trim().is_empty() {
        state.model.as_str()
    } else {
        state.model_display.as_str()
    };
    let model = if model.trim().is_empty() {
        "selecting model…"
    } else {
        model
    };
    let text = [
        format!(
            "{} {}",
            state.theme.bold(&state.theme.fg("model_accent", "ygg")),
            state.theme.dim(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        ),
        String::new(),
        state
            .theme
            .fg("foreground", &format!("{model} / {}", state.reasoning)),
        state.theme.dim(&welcome_workspace(state)),
        String::new(),
        format!(
            "{} {}",
            state.theme.bold("Ctrl+D"),
            state.theme.dim("to exit")
        ),
    ];

    let mut lines = Vec::with_capacity(ROWS + 2);
    lines.push(String::new());
    for row in 0..ROWS {
        lines.push(fit_line(&format!("  {}   {}", logo[row], text[row]), width));
    }
    lines.push(String::new());
    lines
}

fn transcript_lines(state: &ShellState, width: u16) -> Ref<'_, Vec<String>> {
    state.rendered_transcript(width)
}

const FINAL_COMMIT_SEGMENT: u64 = u64::MAX;

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

fn transcript_commit_cursor(state: &ShellState, block: usize, segment: u64) -> CommitCursor {
    CommitCursor {
        generation: state.transcript_epoch,
        block: *state
            .transcript_commit_ids
            .get(block)
            .expect("transcript block missing commit identity"),
        segment,
    }
}

fn transcript_commit_position(state: &ShellState, cursor: CommitCursor) -> Option<CommitPosition> {
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

fn transcript_pinned_frame(
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

fn transcript_viewport_capacity(available: usize, scrolled: bool) -> usize {
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

fn max_scroll_for_available(transcript_len: usize, available: usize) -> usize {
    let live_capacity = transcript_viewport_capacity(available, false);
    if live_capacity == 0 || transcript_len <= live_capacity {
        0
    } else {
        let scrolled_capacity = transcript_viewport_capacity(available, true).max(1);
        transcript_len.saturating_sub(scrolled_capacity)
    }
}

fn responsive_identity(state: &ShellState, width: u16) -> String {
    let wordmark = state.theme.bold(
        &state
            .theme
            .fg("model_accent", state.theme.glyph("wordmark")),
    );
    if state.model.is_empty() {
        return fit_line(&wordmark, width);
    }
    let provider = sanitize_for_terminal(&state.provider);
    let model_name = if state.model_display.is_empty() {
        &state.model
    } else {
        &state.model_display
    };
    let model = state
        .theme
        .fg("model_accent", &sanitize_for_terminal(model_name));
    let separator = semantic_separator(&state.theme);
    let reasoning = (!state.reasoning.is_empty() && state.reasoning != "off")
        .then(|| format!("{separator}{}", sanitize_for_terminal(&state.reasoning)));
    let provider_model = format!("{provider} / {model}");
    let right = format!("{provider_model}{}", reasoning.clone().unwrap_or_default());
    let wide_width = visible_width(&wordmark) + visible_width(&right) + 4;
    if usize::from(width) >= 72 && wide_width <= usize::from(width) {
        let gap =
            usize::from(width).saturating_sub(visible_width(&wordmark) + visible_width(&right));
        return format!("{wordmark}{}{right}", " ".repeat(gap));
    }

    let compact = format!(
        "{wordmark}{separator}{}/{}{}",
        provider,
        model,
        reasoning.unwrap_or_default()
    );
    if visible_width(&compact) <= usize::from(width) {
        return compact;
    }
    let model_only = format!("{wordmark}{separator}{model}");
    if visible_width(&model_only) <= usize::from(width) {
        return model_only;
    }
    fit_line(&wordmark, width)
}

fn render_shell_header(state: &ShellState, width: u16) -> Vec<String> {
    let layout = state.theme.layout_for_width(width);
    let mut lines = Vec::with_capacity(2);
    if layout.show_header {
        lines.push(responsive_identity(state, width));
    }
    if let Some((text, role)) = state
        .extension_header
        .as_ref()
        .filter(|(text, _)| !text.trim().is_empty())
    {
        let role = role.as_deref().unwrap_or("extension.header");
        let inset = usize::from(layout.transcript_inset).min(usize::from(width));
        let contribution = state
            .theme
            .apply_semantic_role(role, &sanitize_for_terminal(text));
        lines.push(fit_line(
            &format!("{}{contribution}", " ".repeat(inset)),
            width,
        ));
    }
    lines
}

#[allow(dead_code)]
fn compact_active_summary(summary: &str) -> String {
    summary
        .split_whitespace()
        .map(|part| {
            if part.contains('/') || part.contains('\\') {
                crate::presentation::compact_path(part)
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(dead_code)]
fn active_run_line(state: &ShellState, width: u16, now: Instant) -> Option<String> {
    let run = state.run.current()?;
    let label = match run.phase() {
        RunPhase::Preparing { summary } => summary.clone(),
        RunPhase::AwaitingProvider { provider } => format!("waiting for {provider}"),
        RunPhase::Thinking => "thinking".into(),
        RunPhase::StreamingResponse => "writing response".into(),
        RunPhase::PreparingToolCall => "preparing tool call".into(),
        RunPhase::RunningTool { summary } => {
            if width < 60 {
                compact_active_summary(summary)
            } else {
                summary.clone()
            }
        }
        RunPhase::AwaitingApproval { prompt } => {
            format!(
                "approval required{}{prompt}",
                semantic_separator(&state.theme)
            )
        }
        RunPhase::Finished(_) => return None,
    };
    let marker = state.theme.fg("model_accent", branch_active(&state.theme));
    let label = sanitize_for_terminal(&label);
    let elapsed = format_duration(run.phase_elapsed_at(now));
    Some(fit_line(
        &format!(
            "{marker} {label}{}{elapsed}",
            semantic_separator(&state.theme)
        ),
        width,
    ))
}

/// Calculate a nonzero output-generation rate from a token count and measured
/// generation interval. Completed turns pass provider-reported tokens; live
/// rendering passes the explicitly marked character-based estimate.
fn output_tokens_per_second(output_tokens: u64, elapsed: Duration) -> Option<f64> {
    (output_tokens > 0 && !elapsed.is_zero())
        .then(|| output_tokens as f64 / elapsed.as_secs_f64())
        .filter(|rate| rate.is_finite())
}

fn usage_cache_hit_rate_basis_points(usage: Usage) -> Option<u16> {
    let prompt_tokens = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    if prompt_tokens == 0 || (usage.cache_read_tokens == 0 && usage.cache_write_tokens == 0) {
        return None;
    }
    Some(((u128::from(usage.cache_read_tokens) * 10_000) / u128::from(prompt_tokens)) as u16)
}

#[derive(Clone)]
struct ShellChrome {
    header: Vec<String>,
    composer: Vec<String>,
    panel: Vec<String>,
    pending: Vec<String>,
    suggestions: Vec<String>,
    error: Vec<String>,
    transcript_rows: usize,
}

fn shell_chrome(state: &ShellState, width: u16, now: Instant) -> ShellChrome {
    let rows = usize::from(state.size.1.max(5));
    let header = render_shell_header(state, width);
    let mut error = state
        .error
        .as_ref()
        .map(|error| {
            let marker = state.theme.fg("error", state.theme.glyph("error"));
            let first_prefix = format!("  {marker} ");
            let continuation = " ".repeat(visible_width(&first_prefix));
            let mut rendered = Vec::new();
            for (index, source) in sanitize_for_terminal(error).split('\n').enumerate() {
                if source.is_empty() {
                    rendered.push(String::new());
                    continue;
                }
                let prefix = if index == 0 {
                    first_prefix.as_str()
                } else {
                    continuation.as_str()
                };
                rendered.extend(wrap_hanging(
                    &state.theme.fg("foreground", source),
                    prefix,
                    &continuation,
                    width,
                ));
            }
            rendered
        })
        .unwrap_or_default();

    // Render the new integrated composer surface (model status + input)
    let composer = crate::tui::composer_surface::render_composer_surface(state, width, now);
    if state.panel.is_some() {
        // The focused picker must retain at least its filter row and cursor,
        // even when a tiny terminal also has a wrapped error message.
        let error_limit = rows.saturating_sub(
            composer
                .len()
                .saturating_add(header.len())
                .saturating_add(1),
        );
        error.truncate(error_limit);
    }
    let mut remaining = rows.saturating_sub(header.len() + error.len() + composer.len());

    let panel = render_panel_with_limit(state, width, remaining);
    remaining = remaining.saturating_sub(panel.len());

    let suggestion_limit = remaining.min(10);
    let suggestions = render_input_suggestions(state, width, suggestion_limit);
    remaining = remaining.saturating_sub(suggestions.len());

    let pending_limit = remaining.min(4);
    let pending = render_pending_steering(state, width, pending_limit);
    remaining = remaining.saturating_sub(pending.len());

    ShellChrome {
        header,
        composer,
        panel,
        pending,
        suggestions,
        error,
        transcript_rows: remaining,
    }
}

fn max_scroll_from_bottom(state: &ShellState, width: u16) -> usize {
    if state.overlay.is_some() {
        return 0;
    }
    let chrome = shell_chrome(state, width, Instant::now());
    max_scroll_for_available(transcript_lines(state, width).len(), chrome.transcript_rows)
}

fn transcript_viewport_capacity_for_state(state: &ShellState, width: u16) -> usize {
    if state.overlay.is_some() {
        return 0;
    }
    let chrome = shell_chrome(state, width, Instant::now());
    let transcript = transcript_lines(state, width);
    let maximum = max_scroll_for_available(transcript.len(), chrome.transcript_rows);
    let scrolled = state.scroll_from_bottom.get().min(maximum) > 0;
    transcript_viewport_capacity(chrome.transcript_rows, scrolled)
}

/// Wrap each logical overlay row independently and terminate its SGR state.
/// Picker rows use the legacy closure-styled compatibility API, so each row is
/// explicitly closed even though sexy-tui 0.2 now preserves extended colors
/// safely across wraps.
fn status_dollars(microdollars: u64) -> String {
    format!("${:.6}", microdollars as f64 / 1_000_000.0)
}

fn status_telemetry(state: &ShellState, now: Instant) -> String {
    let mut lines = vec!["Telemetry".to_owned()];
    if let Some(usage) = state.last_turn_usage {
        lines.extend([
            "Usage source   provider-reported (exact)".to_owned(),
            format!("Input tokens   {}", usage.input_tokens),
            format!("Cache read     {}", usage.cache_read_tokens),
            format!("Cache write    {}", usage.cache_write_tokens),
            format!("Output tokens  {}", usage.output_tokens),
            format!("Reasoning      {}", usage.reasoning_tokens),
            format!("Total tokens   {}", usage.total_tokens),
        ]);
    } else if let Some(tokens) = state.live_generated_tokens() {
        lines.push(format!("Output tokens  ~{tokens} (stream estimate)"));
        lines.push("Usage source   awaiting provider report".to_owned());
    } else {
        lines.push("Usage source   unavailable (no completed model turn)".to_owned());
    }

    let active = state.run.current().is_some_and(|run| run.is_active());
    match state.price_display {
        PriceDisplay::Unknown => {
            lines.push("Turn cost      unavailable (pricing not configured)".to_owned());
            lines.push("Session cost   unavailable (pricing not configured)".to_owned());
        }
        PriceDisplay::ExplicitZero => {
            lines.push("Turn cost      $0 (configured zero-priced)".to_owned());
            lines.push("Session cost   $0 (configured zero-priced)".to_owned());
        }
        PriceDisplay::Priced => {
            if state.run_cost_available {
                let approximate = if active { "~" } else { "" };
                lines.push(format!(
                    "Turn cost      {approximate}{} ({})",
                    status_dollars(state.run_cost_microdollars),
                    if active { "incomplete" } else { "reported" }
                ));
            } else {
                lines.push("Turn cost      unavailable (no durable completed run)".to_owned());
            }
            lines.push(match state.session_cost_microdollars {
                Some(cost) => format!("Session cost   {} (reported)", status_dollars(cost)),
                None => "Session cost   awaiting first usage report".to_owned(),
            });
        }
    }

    if let (Some(rate), Some(tokens), Some(elapsed)) = (
        state.last_turn_tokens_per_second,
        state.last_turn_generated_tokens,
        state.last_turn_generation_elapsed,
    ) {
        lines.push(format!(
            "Throughput     {rate:.1} tok/s final ({tokens} reported tokens / {:.2}s measured)",
            elapsed.as_secs_f64()
        ));
    } else if let Some(started) = state.turn_generation_started_at {
        lines.push(format!(
            "Throughput     awaiting turn completion ({:.2}s generation in progress)",
            now.saturating_duration_since(started).as_secs_f64()
        ));
    } else {
        lines.push("Throughput     unavailable".to_owned());
    }
    lines.join("\n")
}

fn styled_status_text(theme: &YggTheme, text: &str) -> String {
    let safe = sanitize_for_terminal(text);
    let mut metadata = true;
    safe.lines()
        .map(|line| {
            if line.is_empty() {
                metadata = false;
                return String::new();
            }
            if !metadata {
                return line.to_owned();
            }
            let Some(separator) = line.find("  ") else {
                return line.to_owned();
            };
            let label = &line[..separator];
            let spacing_and_value = &line[separator..];
            let spacing = spacing_and_value
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect::<String>();
            let value = &spacing_and_value[spacing.len()..];
            let value = if label == "Model" {
                theme.bold(&theme.fg("model_accent", value))
            } else {
                value.to_owned()
            };
            format!("{}{}{}", theme.fg("model_accent", label), spacing, value)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_overlay_text(text: &str, width: usize) -> Vec<String> {
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

fn visual_col_to_offset(line: &str, col: usize) -> usize {
    let mut current_col = 0;
    let mut byte_offset = 0;
    for grapheme in line.graphemes(true) {
        if current_col >= col {
            break;
        }
        let w = unicode_width::UnicodeWidthStr::width(grapheme);
        if current_col + w > col {
            break;
        }
        current_col += w;
        byte_offset += grapheme.len();
    }
    byte_offset
}

#[allow(dead_code)]
fn copy_offsets_to_visual_cols(
    row_text: &str,
    start_byte: usize,
    end_byte: usize,
) -> (usize, usize) {
    let mut current_byte = 0;
    let mut current_col = 0;
    let mut start_col = 0;
    let mut end_col = 0;
    let mut found_start = false;
    let mut found_end = false;

    for grapheme in row_text.graphemes(true) {
        let w = unicode_width::UnicodeWidthStr::width(grapheme);
        if !found_start && current_byte >= start_byte {
            start_col = current_col;
            found_start = true;
        }
        if !found_end && current_byte >= end_byte {
            end_col = current_col;
            found_end = true;
        }
        current_byte += grapheme.len();
        current_col += w;
    }

    if !found_start {
        start_col = current_col;
    }
    if !found_end {
        end_col = current_col;
    }

    (start_col, end_col)
}

#[allow(dead_code)]
fn block_screen_indent(block: &TranscriptBlock, width: u16) -> usize {
    match block {
        TranscriptBlock::User { .. } => 2,
        TranscriptBlock::Tool(_) => {
            if width < 60 {
                7
            } else {
                8
            }
        }
        _ => 0,
    }
}

fn newline_col_offset(text: &str, n: usize, col: u16) -> usize {
    let start_offset = newline_offset(text, n);
    let line = text.split('\n').nth(n).unwrap_or("");
    let cell_offset = visual_col_to_offset(line, usize::from(col));
    start_offset + cell_offset
}

fn wrapped_line_col_offset(text: &str, n: usize, col: u16, wrap_width: usize) -> usize {
    let wrapped = wrap_text_with_ansi(text, wrap_width);
    let start_offset: usize = wrapped.iter().take(n).map(|line| line.len()).sum();
    let line = wrapped.get(n).map(String::as_str).unwrap_or("");
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
        TranscriptBlock::Notice(_) | TranscriptBlock::Compaction(_) => {
            let w = (width as usize).max(1);
            wrapped_line_col_offset(copy_text, local_row, col, w)
        }
        TranscriptBlock::Outcome(_) => visual_col_to_offset(copy_text, usize::from(col)),
        TranscriptBlock::Tool(_) => {
            let indent = if width < 60 { 7 } else { 8 };
            let col_in_text = col.saturating_sub(indent);
            newline_col_offset(copy_text, local_row, col_in_text)
        }
        TranscriptBlock::Shell(_) => {
            let w = (width as usize).max(1);
            wrapped_line_col_offset(copy_text, local_row, col, w)
        }
    }
}

fn selection_position_for_visual_cell(
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

#[allow(dead_code)]
fn style_selected_range(line: &str, start_col: usize, end_col: usize, theme: &YggTheme) -> String {
    if matches!(
        theme.capabilities().color,
        crate::tui::terminal::ColorDepth::None
    ) {
        let plain = strip_terminal_sequences(line);
        let start = visual_col_to_offset(&plain, start_col);
        let end = visual_col_to_offset(&plain, end_col);
        let prefix = &plain[..start];
        let mid = &plain[start..end];
        let suffix = &plain[end..];
        return format!("{}[{}]{}", prefix, mid, suffix);
    }

    let tokens = sexy_tui_rs::terminal_tokens(line);
    let mut output = String::new();
    let mut col_index = 0;
    let mut in_selection = false;

    for token in tokens {
        match token {
            sexy_tui_rs::TerminalToken::Escape(seq) => {
                if in_selection {
                    output.push_str("\x1b[27m");
                }
                output.push_str(seq);
                if in_selection {
                    output.push_str("\x1b[7m");
                }
            }
            sexy_tui_rs::TerminalToken::Text(val) => {
                for grapheme in val.graphemes(true) {
                    let w = unicode_width::UnicodeWidthStr::width(grapheme);
                    let is_selected = col_index >= start_col && col_index < end_col;

                    if is_selected && !in_selection {
                        output.push_str("\x1b[7m");
                        in_selection = true;
                    } else if !is_selected && in_selection {
                        output.push_str("\x1b[27m");
                        in_selection = false;
                    }

                    output.push_str(grapheme);
                    col_index += w;
                }
            }
        }
    }

    if in_selection {
        output.push_str("\x1b[27m");
    }

    output
}

/// Map a 0-indexed visual row within a transcript block to a byte offset in
/// that block's copy text. The visual renderer wraps rich content at a
/// block-type-specific width; this function replicates that wrapping so
/// pointer selection lands on the correct semantic position.
#[allow(dead_code)]
fn visual_row_to_copy_offset(
    block: &TranscriptBlock,
    copy_text: &str,
    local_row: usize,
    width: u16,
) -> usize {
    if local_row == 0 {
        return 0;
    }

    match block {
        TranscriptBlock::Assistant(assistant) => {
            if looks_like_diff(&assistant.text) {
                // Diff rendering uses line-number columns and side-by-side
                // layout; there is no simple wrapping correspondence.
                // Fall back to newline-based indexing.
                return newline_offset(copy_text, local_row);
            }
            wrapped_line_offset(copy_text, local_row, usize::from(width).max(1))
        }
        TranscriptBlock::Reasoning(_) => {
            wrapped_line_offset(copy_text, local_row, usize::from(width).max(1))
        }
        TranscriptBlock::User { .. } => {
            let inner_width = (width.saturating_sub(2) as usize).max(1);
            wrapped_line_offset(copy_text, local_row, inner_width)
        }
        TranscriptBlock::Notice(_) | TranscriptBlock::Compaction(_) => {
            let w = (width as usize).max(1);
            wrapped_line_offset(copy_text, local_row, w)
        }
        TranscriptBlock::Outcome(_) => {
            // Outcome blocks are always a single fitted line; any row
            // beyond the first maps to the end of the block.
            copy_text.len()
        }
        TranscriptBlock::Tool(_) => {
            // Tool blocks have a structured header + optional detail panels
            // that don't map neatly to wrapped copy text.  Fall back to
            // newline-based indexing which is correct for the common
            // one-line summary + output layout.
            newline_offset(copy_text, local_row)
        }
        TranscriptBlock::Shell(_) => {
            let w = (width as usize).max(1);
            wrapped_line_offset(copy_text, local_row, w)
        }
    }
}

/// Byte-offset after `n` newline-delimited segments (current behaviour for
/// blocks where wrapping correspondence is unavailable).
fn newline_offset(text: &str, n: usize) -> usize {
    text.split_inclusive('\n')
        .take(n)
        .map(str::len)
        .sum::<usize>()
        .min(text.len())
}

/// Byte-offset after `n` lines of `text` wrapped at `wrap_width`.  Uses the
/// same ANSI-aware word-wrapper the visual renderer relies on so that line
/// boundaries agree with what the user sees.
#[allow(dead_code)]
fn wrapped_line_offset(text: &str, n: usize, wrap_width: usize) -> usize {
    let wrapped = wrap_text_with_ansi(text, wrap_width);
    let count = n.min(wrapped.len());
    wrapped.iter().take(count).map(|line| line.len()).sum()
}

fn append_viewport_chrome(lines: &mut Vec<String>, chrome: ShellChrome) {
    // Explicit application-owned scrolling still renders exactly one terminal
    // viewport. Native mode uses `append_chrome` below so committed transcript
    // rows can enter terminal scrollback instead of being sliced away here.
    lines.truncate(chrome.transcript_rows);
    lines.resize(chrome.transcript_rows, String::new());
    lines.extend(chrome.header);
    lines.extend(chrome.error);
    lines.extend(chrome.pending);
    lines.extend(chrome.suggestions);
    lines.extend(chrome.panel);
    lines.extend(chrome.composer);
}

fn overlay_lines(state: &ShellState, width: u16) -> Vec<String> {
    let Some(overlay) = &state.overlay else {
        return Vec::new();
    };
    match overlay {
        ShellOverlay::Text(text) => wrap_overlay_text(text, usize::from(width).max(1)),
        ShellOverlay::Context(report) => report.render(&state.theme, width),
    }
}

fn transcript_viewport_lines(state: &ShellState, width: u16, available: usize) -> Vec<String> {
    let transcript = transcript_lines(state, width);
    let max_scroll = max_scroll_for_available(transcript.len(), available);
    let scroll = state.scroll_from_bottom.get().min(max_scroll);
    let scrolled = scroll > 0;
    let capacity = transcript_viewport_capacity(available, scrolled);
    let end = transcript.len().saturating_sub(scroll);
    let start = end.saturating_sub(capacity);
    let mut lines = transcript[start..end].to_vec();
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

fn render_shell_viewport_at(state: &ShellState, width: u16, now: Instant) -> Vec<String> {
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

fn render_shell_viewport_update(
    state: &ShellState,
    width: u16,
    now: Instant,
    frame: &mut ShellFrameState,
) -> FrameUpdate {
    let repaint_theme = frame.initialized && frame.theme_epoch != state.theme_epoch;
    let resized = frame.initialized && (frame.width != width || frame.height != state.size.1);
    frame.initialized = true;
    frame.width = width;
    frame.height = state.size.1;
    frame.theme_epoch = state.theme_epoch;
    frame.transcript_epoch = state.transcript_epoch;
    frame.verbose_tools = state.verbose_tools;
    FrameUpdate {
        stable_prefix: 0,
        replacement: render_shell_viewport_at(state, width, now),
        pinned: None,
        resize_replay: None,
        reanchor_viewport: repaint_theme || resized,
        rebuild_scrollback: false,
    }
}

fn append_chrome(lines: &mut Vec<String>, chrome: ShellChrome, stable_prefix_rows: usize) {
    // Native mode follows the logical content height. Padding a short frame to
    // the terminal height pins the composer to the bottom and creates a large
    // dead zone below the transcript. Once the frame naturally grows past the
    // viewport, sexy-tui moves committed rows into terminal-owned scrollback.
    // `lines` may be only a lazy suffix, so its retained prefix still decides
    // whether the transcript owns the single breathing row before chrome.
    let complete_transcript_rows = stable_prefix_rows.saturating_add(lines.len());
    if complete_transcript_rows > 0 {
        lines.push(String::new());
    }
    lines.extend(chrome.header);
    lines.extend(chrome.error);
    lines.extend(chrome.pending);
    lines.extend(chrome.suggestions);
    lines.extend(chrome.panel);
    lines.extend(chrome.composer);
}

fn shell_chrome_rows(chrome: &ShellChrome) -> usize {
    chrome
        .header
        .len()
        .saturating_add(chrome.error.len())
        .saturating_add(chrome.pending.len())
        .saturating_add(chrome.suggestions.len())
        .saturating_add(chrome.panel.len())
        .saturating_add(chrome.composer.len())
}

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

/// Full logical primary-screen frame. The terminal backend paints only its
/// visible tail; committed rows naturally move into native scrollback and are
/// never sliced into an application-owned viewport on the default path.
fn render_shell_at(state: &ShellState, width: u16, now: Instant) -> Vec<String> {
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

fn synchronize_shell_frame(state: &ShellState, width: u16, frame: &mut ShellFrameState) {
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
fn render_shell_update_with_cursor(
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
fn render_shell_update(
    state: &ShellState,
    width: u16,
    now: Instant,
    frame: &mut ShellFrameState,
) -> FrameUpdate {
    render_shell_update_with_cursor(state, width, now, frame, None)
}

fn render_shell(state: &ShellState, width: u16) -> Vec<String> {
    render_shell_at(state, width, Instant::now())
}

// ── panel rendering ──────────────────────────────────────────────────

/// Indices of the items matching the current filter. Every whitespace-delimited
/// term must appear in either the label or description, case-insensitively.
fn filtered_indices(items: &[String], descriptions: &[Option<String>], filter: &str) -> Vec<usize> {
    let needles = filter
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    items
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            if needles.is_empty() {
                return true;
            }
            let mut searchable = item.to_lowercase();
            if let Some(description) = descriptions
                .get(*index)
                .and_then(|description| description.as_deref())
            {
                searchable.push(' ');
                searchable.push_str(&description.to_lowercase());
            }
            needles.iter().all(|needle| searchable.contains(needle))
        })
        .map(|(index, _)| index)
        .collect()
}

fn panel_cell(text: &str) -> String {
    sanitize_for_terminal(text).replace('\n', " ")
}

fn panel_header(
    theme: &YggTheme,
    title: &str,
    selected: usize,
    matches: usize,
    width: u16,
) -> String {
    let terminal_width = width;
    let width = usize::from(width);
    let inset = usize::from(width >= 5) * 2;
    let available = width.saturating_sub(inset.saturating_mul(2));
    let title = panel_cell(
        if width < 28 && title.eq_ignore_ascii_case("select model") {
            "Models"
        } else {
            title
        },
    );
    let position = if matches == 0 {
        "0/0".to_owned()
    } else {
        format!("{}/{}", selected.min(matches - 1) + 1, matches)
    };
    let gap = available
        .saturating_sub(visible_width(&title))
        .saturating_sub(visible_width(&position));
    let line = format!(
        "{}{}{}{}{}",
        " ".repeat(inset),
        theme.bold(&title),
        " ".repeat(gap.max(1)),
        subdued_text(theme, &position),
        " ".repeat(inset)
    );
    fit_line(&line, terminal_width)
}

fn panel_filter_line(theme: &YggTheme, filter: &str, width: u16) -> String {
    let width = usize::from(width);
    let label_text = if width >= 12 {
        "Filter"
    } else if width >= 4 {
        "F"
    } else {
        ""
    };
    let label = subdued_text(theme, label_text);
    let prefix = if label_text.is_empty() {
        String::new()
    } else if label_text == "F" {
        format!("{label} ")
    } else {
        format!("  {label}  ")
    };
    let available = width.saturating_sub(visible_width(&prefix));
    let filter = panel_cell(filter);
    if filter.is_empty() {
        let placeholder = sexy_tui_rs::truncate_to_width(
            "type to filter",
            available,
            Some(if theme.unicode() { "…" } else { "..." }),
        );
        format!(
            "{prefix}{CURSOR_MARKER}{}",
            subdued_text(theme, &placeholder)
        )
    } else {
        let ellipsis = if theme.unicode() { "…" } else { "..." };
        let query = if visible_width(&filter) <= available {
            filter
        } else {
            let ellipsis_width = visible_width(ellipsis).min(available);
            let suffix_budget = available.saturating_sub(ellipsis_width);
            let mut suffix_start = filter.len();
            let mut suffix_width: usize = 0;
            for (index, grapheme) in filter.grapheme_indices(true).rev() {
                let grapheme_width = visible_width(grapheme);
                if suffix_width.saturating_add(grapheme_width) > suffix_budget {
                    break;
                }
                suffix_start = index;
                suffix_width += grapheme_width;
            }
            let visible_ellipsis = sexy_tui_rs::truncate_to_width(ellipsis, available, Some(""));
            format!("{visible_ellipsis}{}", &filter[suffix_start..])
        };
        format!("{prefix}{}{CURSOR_MARKER}", theme.fg("foreground", &query))
    }
}

fn panel_window(selected: usize, matches: usize, visible: usize) -> std::ops::Range<usize> {
    if matches == 0 || visible == 0 {
        return 0..0;
    }
    let selected = selected.min(matches - 1);
    let start = selected
        .saturating_sub(visible / 2)
        .min(matches.saturating_sub(visible));
    start..start.saturating_add(visible).min(matches)
}

fn panel_label_width(
    items: &[String],
    descriptions: &[Option<String>],
    filtered: &[usize],
    width: u16,
) -> Option<usize> {
    let content_width = usize::from(width).saturating_sub(4);
    let max_label = filtered
        .iter()
        .map(|index| visible_width(&panel_cell(&items[*index])))
        .max()
        .unwrap_or(0);
    let has_description = filtered.iter().any(|index| {
        descriptions
            .get(*index)
            .and_then(|description| description.as_deref())
            .is_some_and(|description| !description.is_empty())
    });
    if !has_description || content_width < 42 {
        return None;
    }
    let label_width = max_label.clamp(22, 44).min(content_width * 45 / 100);
    (content_width.saturating_sub(label_width + 2) >= 18).then_some(label_width)
}

fn render_panel_item(
    state: &ShellState,
    item: &str,
    description: Option<&str>,
    is_selected: bool,
    label_width: Option<usize>,
    width: u16,
) -> String {
    let item = panel_cell(item);
    let marker = state.theme.glyph("prompt");
    let prefix = if is_selected {
        format!("  {} ", state.theme.fg("model_accent", marker))
    } else {
        "    ".to_owned()
    };
    let available = usize::from(width).saturating_sub(visible_width(&prefix));
    let ellipsis = if state.theme.unicode() { "…" } else { "..." };

    let label = if let Some(label_width) = label_width {
        sexy_tui_rs::truncate_to_width(&item, label_width, Some(ellipsis))
    } else {
        sexy_tui_rs::truncate_to_width(&item, available, Some(ellipsis))
    };
    let label = if is_selected {
        state.theme.bold(&state.theme.fg("model_accent", &label))
    } else {
        label
    };

    let mut line = format!("{prefix}{label}");
    if let (Some(label_width), Some(description)) = (label_width, description) {
        let padding = label_width.saturating_sub(visible_width(&item));
        let description_width = available.saturating_sub(label_width + 2);
        let description = sexy_tui_rs::truncate_to_width(
            &panel_cell(description),
            description_width,
            Some(ellipsis),
        );
        line.push_str(&" ".repeat(padding + 2));
        line.push_str(&subdued_text(&state.theme, &description));
    }
    fit_line(&line, width)
}

/// How many rows the active panel needs (capped so it cannot squeeze the
/// transcript to zero).
#[cfg(test)]
fn panel_rows(state: &ShellState, width: u16) -> usize {
    let Some(ref panel) = state.panel else {
        return 0;
    };
    let term_rows = usize::from(state.size.1.max(5));
    let max_panel = term_rows.saturating_sub(4); // leave room for composer + footer
    match panel {
        Panel::SelectList {
            items,
            descriptions,
            filter,
            ..
        } => {
            // `(no matches)` still occupies one body row.
            let body = filtered_indices(items, descriptions, filter).len().max(1);
            let border_rows = usize::from(
                state.theme.layout_for_width(width).show_panel_borders && max_panel >= 4,
            ) * 2;
            // title + stable filter row + items (capped), optionally framed by
            // top/bottom semantic rules.
            (body + 2 + border_rows).min(max_panel)
        }
    }
}

#[cfg(test)]
fn render_panel(state: &ShellState, width: u16) -> Vec<String> {
    render_panel_with_limit(state, width, panel_rows(state, width))
}

fn render_panel_with_limit(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    let Some(ref panel) = state.panel else {
        return Vec::new();
    };
    if max_rows == 0 {
        return Vec::new();
    }
    let w = usize::from(width).max(1);
    let rule = state.theme.glyph("horizontal").repeat(w);
    let dim = |s: &str| subdued_text(&state.theme, s);

    match panel {
        Panel::SelectList {
            title,
            items,
            descriptions,
            selected,
            filter,
            ..
        } => {
            let filtered = filtered_indices(items, descriptions, filter);
            let header = panel_header(&state.theme, title, *selected, filtered.len(), width);
            let filter_line = panel_filter_line(&state.theme, filter, width);
            if max_rows == 1 {
                return vec![filter_line];
            }
            if max_rows == 2 {
                return vec![header, filter_line];
            }

            let show_borders =
                state.theme.layout_for_width(width).show_panel_borders && max_rows >= 4;
            let border_rows = usize::from(show_borders) * 2;
            let mut lines = Vec::with_capacity(max_rows);
            if show_borders {
                lines.push(dim(&rule));
            }
            lines.push(header);
            lines.push(filter_line);
            let max_body = max_rows.saturating_sub(2 + border_rows);
            if filtered.is_empty() && max_body > 0 {
                let message = if filter.is_empty() {
                    "  No matches".to_owned()
                } else if state.theme.unicode() {
                    format!("  No matches for “{}”", panel_cell(filter))
                } else {
                    format!("  No matches for \"{}\"", panel_cell(filter))
                };
                lines.push(fit_line(&dim(&message), width));
            } else if !filtered.is_empty() {
                let visible = filtered.len().min(max_body);
                let window = panel_window(*selected, filtered.len(), visible);
                let label_width = panel_label_width(items, descriptions, &filtered, width);
                for position in window {
                    let index = filtered[position];
                    lines.push(render_panel_item(
                        state,
                        &items[index],
                        descriptions.get(index).and_then(|value| value.as_deref()),
                        position == *selected,
                        label_width,
                        width,
                    ));
                }
            }
            if show_borders {
                lines.push(dim(&rule));
            }
            lines
        }
    }
}

/// Full-screen terminal shell. It owns all terminal I/O and no Agent state.
fn apply_hydrated_tool_result(panel: &mut ToolPanel, text: &str, is_error: bool) {
    panel.finished = true;
    let replayed = Ok(ygg_agent::ToolOutput::new(text.to_owned()));
    panel.is_error = is_error || tool_result_is_failure(&panel.name, &replayed);
    if !panel.is_error {
        panel.display.mark_media_read_from_result(text);
    }
    panel.failure_reason = if is_error {
        tool_failure_reason(
            &panel.name,
            &Err(ygg_agent::ToolError::new(text.to_owned())),
        )
    } else {
        tool_failure_reason(&panel.name, &replayed)
    };
    bounded_append(&mut panel.output, text);
}

fn append_hydrated_items(state: &mut ShellState, items: impl IntoIterator<Item = TranscriptItem>) {
    for item in items {
        match item {
            TranscriptItem::User {
                text,
                model_lab,
                prompt_color,
            } => {
                state.push_block(TranscriptBlock::User {
                    text,
                    model_lab,
                    prompt_color,
                    persisted: true,
                });
            }
            TranscriptItem::Assistant(text) => state.push_block(TranscriptBlock::Assistant(
                Box::new(AssistantBlock::finalized(text)),
            )),
            TranscriptItem::Reasoning(text) => state.push_block(TranscriptBlock::Reasoning(
                Box::new(AssistantBlock::finalized_reasoning(text)),
            )),
            TranscriptItem::ToolCall { id, name, args } => {
                let index = state.transcript.len();
                let display =
                    summarize_tool_with_workspace(&name, &args, state.workspace.as_deref());
                let model_lab = state.model_lab;
                state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                    id.clone(),
                    name,
                    args.to_string(),
                    display,
                    String::new(),
                    false,
                    false,
                    None,
                    model_lab,
                ))));
                state.tool_panels.insert(id, index);
            }
            TranscriptItem::ToolResult { id, text, is_error } => {
                // Malformed provider output can reuse one call ID within the
                // same assistant turn. The durable protocol cannot identify
                // which duplicate a result belongs to, so conservatively close
                // every still-open matching card. Leaving an older duplicate
                // active would revive a spinner for work that cannot still be
                // running after process restart.
                let pending = state
                    .transcript
                    .iter()
                    .enumerate()
                    .filter_map(|(index, block)| match block {
                        TranscriptBlock::Tool(panel) if panel.id == id && !panel.finished => {
                            Some(index)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !pending.is_empty() {
                    for index in pending {
                        if let Some(TranscriptBlock::Tool(panel)) = state.transcript.get_mut(index)
                        {
                            apply_hydrated_tool_result(panel, &text, is_error);
                        }
                    }
                } else if let Some(panel) = state.tool_output_mut(&id) {
                    apply_hydrated_tool_result(panel, &text, is_error);
                } else {
                    let index = state.transcript.len();
                    let model_lab = state.model_lab;
                    state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                        id.clone(),
                        "tool result".into(),
                        String::new(),
                        summarize_tool("tool result", &serde_json::Value::Null),
                        sanitize_for_terminal(&text),
                        true,
                        is_error,
                        is_error.then(|| {
                            tool_failure_reason(
                                "tool result",
                                &Err(ygg_agent::ToolError::new(text.clone())),
                            )
                            .unwrap_or_else(|| "tool failed".into())
                        }),
                        model_lab,
                    ))));
                    state.tool_panels.insert(id, index);
                }
            }
            TranscriptItem::CompactionMarker { summary } => {
                state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
                    label: "Context compacted".into(),
                    summary,
                    expanded: false,
                })));
            }
            TranscriptItem::NativeCompactionMarker => {
                state.push_block(TranscriptBlock::Notice(
                    "Context compacted natively · opaque Responses state retained".into(),
                ));
            }
        }
    }
}

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
    capture_mouse: bool,
}

impl InteractiveShell {
    /// Enter with explicit mouse ownership. The terminal still supports every
    /// keyboard transcript action when mouse reporting is disabled.
    pub fn enter_with_mouse(
        theme: YggTheme,
        size: TerminalSize,
        capture_mouse: bool,
    ) -> Result<Self> {
        if !theme.capabilities().interactive {
            anyhow::bail!("interactive terminal capabilities are unavailable");
        }
        let terminal = YggTerminal::enter_with_mouse(size.clone(), capture_mouse)?;
        let initial_size = *size.lock().expect("terminal size mutex poisoned");
        let state = SharedState::new(ShellState {
            theme,
            size: initial_size,
            follow_tail: true,
            startup_card_started_at: Some(Instant::now()),
            ..ShellState::default()
        });
        let (render_tx, render_rx) = mpsc::sync_channel(1);
        let render_state = state.clone();
        let render_size = size.clone();
        let render_thread = thread::Builder::new()
            .name("ygg-tui-render".to_owned())
            .spawn(move || {
                render_loop(
                    terminal,
                    render_state,
                    render_size,
                    render_rx,
                    capture_mouse,
                )
            })?;

        Ok(Self {
            tui: None,
            state,
            size,
            render_tx: Some(render_tx),
            render_thread: Some(render_thread),
            theme_config: None,
            capture_mouse,
        })
    }

    #[cfg(test)]
    pub fn test_shell() -> Self {
        Self::test_shell_with_theme(crate::tui::theme::test_theme())
    }

    #[cfg(test)]
    fn test_shell_with_theme(theme: YggTheme) -> Self {
        let size = Arc::new(Mutex::new((120, 40)));
        let initial_size = *size.lock().expect("terminal size mutex poisoned");
        let state = SharedState::new(ShellState {
            theme,
            size: initial_size,
            follow_tail: true,
            ..ShellState::default()
        });
        let mut tui = TUI::new(Box::new(TestTerminal { size: size.clone() }));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(ShellComponent {
            state: state.clone(),
            frame: RefCell::new(ShellFrameState::default()),
            application_viewport: false,
        }));
        tui.start();
        Self {
            tui: Some(tui),
            state,
            size,
            render_tx: None,
            render_thread: None,
            theme_config: None,
            capture_mouse: false,
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

    /// Temporarily leave the alternate screen while preserving shell state.
    /// OAuth uses this so the hosted verification code and browser fallback are
    /// visible in an ordinary terminal.
    pub fn suspend(&mut self) {
        self.stop_renderer();
        force_restore();
    }

    /// Re-enter the alternate screen after a suspended operation.
    pub fn resume(&mut self) -> Result<()> {
        if self.render_thread.is_some() || self.tui.is_some() {
            return Ok(());
        }
        let terminal = YggTerminal::enter_with_mouse(self.size.clone(), self.capture_mouse)?;
        let current_size = *self.size.lock().expect("terminal size mutex poisoned");
        self.set_size(current_size.0, current_size.1);
        let (render_tx, render_rx) = mpsc::sync_channel(1);
        let render_state = self.state.clone();
        let render_size = self.size.clone();
        let application_viewport = self.capture_mouse;
        let render_thread = thread::Builder::new()
            .name("ygg-tui-render".to_owned())
            .spawn(move || {
                render_loop(
                    terminal,
                    render_state,
                    render_size,
                    render_rx,
                    application_viewport,
                )
            })?;
        self.render_tx = Some(render_tx);
        self.render_thread = Some(render_thread);
        self.render();
        Ok(())
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

    /// Begin a presentation run as soon as input is accepted. This precedes
    /// compaction and `Agent::prompt`, so submission is acknowledged without
    /// waiting for a provider event.
    pub fn begin_run(&mut self, provider: &str) -> RunId {
        let mut state = self.state.borrow_mut();
        state.run_label.clear();
        state.clear_turn_telemetry();
        state.run_model = Some(state.model.clone());
        state.run_model_lab = state.model_lab;
        state.run_prompt_color = state.prompt_color.clone();
        state.telemetry_model = state.run_model.clone();
        state.run_model_display = Some(if state.model_display.is_empty() {
            state.model.clone()
        } else {
            state.model_display.clone()
        });
        state.run_model_compact_names = state.model_compact_names.clone();
        state.run_reasoning = Some(state.reasoning.clone());
        state.run_price_display = Some(state.price_display);
        state.run_context_estimate = state.context_estimate;
        let provider_status = crate::presentation::provider_status_name(provider);
        let id = state
            .run
            .begin(&provider_status)
            .expect("a new prompt is accepted only after the previous run terminates");
        state.shimmer_started_at = Some(Instant::now());
        state.open_reasoning_status();
        id
    }

    pub fn current_run_id(&self) -> Option<RunId> {
        self.state.borrow().run.current_id()
    }

    pub fn set_run_preparing(&mut self, id: RunId, summary: impl Into<String>) {
        self.state.borrow_mut().run.set_preparing(id, summary);
    }

    pub fn set_awaiting_provider(&mut self, id: RunId) {
        self.state.borrow_mut().run.awaiting_provider(id);
    }

    fn append_outcome(state: &mut ShellState, outcome: RunOutcome) {
        if let Some(run) = state.run.current() {
            state.session_work_elapsed = state
                .session_work_elapsed
                .saturating_add(run.elapsed_at(Instant::now()));
        }
        // The run's shimmer anchor is animation-only. Leaving it populated
        // after completion made the idle footer behave like a wall clock.
        state.shimmer_started_at = None;
        state.close_streaming_blocks();
        state.push_block(TranscriptBlock::Outcome(outcome));
        if !state.selected_model_owns_telemetry() {
            state.clear_turn_telemetry();
        }
    }

    #[cfg(test)]
    pub fn interrupt_run(&mut self, id: RunId) {
        let mut state = self.state.borrow_mut();
        if let Some(outcome) = state.run.interrupt(id) {
            Self::append_outcome(&mut state, outcome);
        }
    }

    pub fn fail_run(&mut self, id: RunId, reason: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        if let Some(outcome) = state.run.fail(id, reason) {
            Self::append_outcome(&mut state, outcome);
        }
    }

    /// Compatibility helper for focused shell tests. Production passes the
    /// explicit run id through `on_run_event`.
    #[cfg(test)]
    pub fn on_agent_event(&mut self, event: &AgentEvent) {
        let id = match self.current_run_id() {
            Some(id) => id,
            None => {
                let provider = self.state.borrow().provider.clone();
                self.begin_run(if provider.is_empty() {
                    "provider"
                } else {
                    &provider
                })
            }
        };
        self.on_run_event(id, event);
    }

    pub fn on_run_event(&mut self, id: RunId, event: &AgentEvent) {
        let mut state = self.state.borrow_mut();
        let update = state.run.apply_event(id, event);
        if !update.accepted {
            return;
        }
        match event {
            AgentEvent::OutputDelta { channel, text } => {
                if state.turn_generation_started_at.is_none() {
                    state.turn_generation_started_at = Some(Instant::now());
                    state.turn_streamed_output_bytes = 0;
                    state.last_turn_tokens_per_second = None;
                    state.last_turn_generation_elapsed = None;
                    state.last_turn_generated_tokens = None;
                    // Live output belongs only to this provider request. The
                    // prior turn remains in the prompt/context, not in this
                    // turn's output counter.
                    state.turn_output_tokens_before_generation = 0;
                }
                state.turn_streamed_output_bytes = state
                    .turn_streamed_output_bytes
                    .saturating_add(text.len() as u64);
                state.append_text_block(*channel, text);
            }
            AgentEvent::ProviderRetry { .. } | AgentEvent::CandidateRejected { .. } => {
                state.discard_streaming_blocks();
                state.open_reasoning_status();
                state.turn_generation_started_at = None;
                state.turn_streamed_output_bytes = 0;
            }
            AgentEvent::SteeringDelivered { messages } => {
                state.close_streaming_blocks();
                let model_lab = state.executing_model_lab();
                let prompt_color = state.executing_prompt_color();
                for message in messages {
                    let display = if state.steering_queue.is_empty() {
                        message.clone()
                    } else {
                        state.steering_queue.remove(0).display
                    };
                    state.push_block(TranscriptBlock::User {
                        text: display,
                        model_lab,
                        prompt_color: prompt_color.clone(),
                        persisted: true,
                    });
                }
                state.open_reasoning_status();
            }
            AgentEvent::CompactionStarted { .. } => {
                // Overflow recovery can begin after a partial provider
                // attempt. Its deltas were never durable and must not survive
                // beside the replacement compacted context.
                state.discard_streaming_blocks();
                state.run_label = "compacting".into();
                state.turn_generation_started_at = None;
                state.turn_streamed_output_bytes = 0;
            }
            AgentEvent::CompactionFinished { reason, result } => {
                state.run_label.clear();
                match result {
                    Ok(info) => {
                        let reason = match reason {
                            ygg_agent::CompactionReason::Threshold => "context threshold",
                            ygg_agent::CompactionReason::Overflow => "overflow recovery",
                        };
                        match &info.kind {
                            ygg_agent::CompactionKind::Local => {
                                state.latest_compaction_summary = Some(info.summary.clone());
                                state.push_block(TranscriptBlock::Compaction(Box::new(
                                    CompactionBlock {
                                        label: "Context compacted automatically".into(),
                                        summary: info.summary.clone(),
                                        expanded: false,
                                    },
                                )));
                            }
                            ygg_agent::CompactionKind::NativeResponses { .. } => {
                                state.push_block(TranscriptBlock::Notice(format!(
                                    "Context compacted natively · {reason} · opaque Responses state retained"
                                )));
                            }
                        }
                    }
                    Err(error) => {
                        state.error = Some(format!("automatic compaction failed: {error}"));
                    }
                }
                if result.is_ok() {
                    state.open_reasoning_status();
                }
            }
            AgentEvent::ToolStarted { id, name, args } => {
                state.close_streaming_blocks();
                state.event_dot_visible = true;
                let index = state.transcript.len();
                let workspace = state.workspace.clone();
                let display = summarize_tool_with_workspace(name, args, workspace.as_deref());
                let model_lab = state.executing_model_lab();
                state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                    id.clone(),
                    name.clone(),
                    args.to_string(),
                    display,
                    String::new(),
                    false,
                    false,
                    None,
                    model_lab,
                ))));
                state.tool_panels.insert(id.clone(), index);
            }
            AgentEvent::ToolProgress { id, progress } => {
                let index = state.tool_panels.get(id).copied();
                let refreshes_compact_tail = matches!(
                    progress,
                    ToolProgress::Output { .. }
                        | ToolProgress::Status(_)
                        | ToolProgress::Dropped { .. }
                );
                if let Some(panel) = state.tool_output_mut(id) {
                    match progress {
                        ToolProgress::Output { bytes, .. } => {
                            bounded_append(&mut panel.output, &String::from_utf8_lossy(bytes));
                        }
                        ToolProgress::Status(message) => {
                            bounded_append(&mut panel.output, &format!("{message}\n"));
                        }
                        ToolProgress::Confirmation(request) => {
                            bounded_append(
                                &mut panel.output,
                                &format!("confirmation requested: {}\n", request.prompt),
                            );
                        }
                        ToolProgress::Input(_) => {}
                        ToolProgress::Dropped { bytes, events } => {
                            if *bytes > 0 {
                                bounded_append(
                                    &mut panel.output,
                                    &format!("... {bytes} bytes of live output elided ...\n"),
                                );
                            }
                            if *events > 0 {
                                bounded_append(
                                    &mut panel.output,
                                    &format!(
                                        "... {events} session event(s) could not be recorded ...\n"
                                    ),
                                );
                            }
                        }
                        ToolProgress::SessionEvent(..) => {}
                    }
                }
                if state.verbose_tools || refreshes_compact_tail {
                    if let Some(index) = index {
                        state.touch_block(index);
                    }
                }
            }
            AgentEvent::ToolFinished { id, result } => {
                let estimated_result_tokens = match result {
                    Ok(output) => output.media().iter().fold(
                        crate::compaction::estimate_text_tokens(&output.text),
                        |tokens, media| {
                            tokens.saturating_add(crate::compaction::estimate_media_tokens(media))
                        },
                    ),
                    Err(error) => crate::compaction::estimate_text_tokens(&error.message),
                }
                .saturating_add(8);
                let index = state.tool_panels.get(id).copied();
                if let Some(panel) = state.tool_output_mut(id) {
                    panel.finished = true;
                    panel.is_error = tool_result_is_failure(&panel.name, result);
                    panel.failure_reason = tool_failure_reason(&panel.name, result);
                    match result {
                        Ok(output) => {
                            panel.display.mark_media_read(output.media_kinds());
                            bounded_append(&mut panel.output, &output.text);
                        }
                        Err(error) => bounded_append(&mut panel.output, &error.message),
                    }
                }
                if let Some(index) = index {
                    state.touch_block(index);
                }
                if let Some((used, _)) = state.run_context_estimate.as_mut() {
                    *used = used.saturating_add(estimated_result_tokens);
                }
                if state.run_model.as_deref() == Some(state.model.as_str()) {
                    if let Some((used, _)) = state.context_estimate.as_mut() {
                        *used = used.saturating_add(estimated_result_tokens);
                    }
                }
                let tool_still_running = state.tool_panels.values().any(|index| {
                    matches!(
                        state.transcript.get(*index),
                        Some(TranscriptBlock::Tool(panel)) if !panel.finished
                    )
                });
                if !tool_still_running {
                    state.open_reasoning_status();
                }
            }
            AgentEvent::TurnFinished {
                turn_usage,
                session_cost_microdollars,
                run_cost_microdollars,
                ..
            } => {
                state.close_streaming_blocks();
                if let Some(started_at) = state.turn_generation_started_at.take() {
                    let elapsed = started_at.elapsed();
                    state.last_turn_tokens_per_second =
                        output_tokens_per_second(turn_usage.output_tokens, elapsed);
                    state.last_turn_generation_elapsed = Some(elapsed);
                    state.last_turn_generated_tokens = Some(turn_usage.output_tokens);
                }
                // Provider usage is authoritative at this boundary. Prompt
                // cache buckets all occupy context, while reasoning is already
                // a subset of output, so canonical total_tokens is exactly the
                // request's prompt + generated output. Never add cumulative run
                // usage here: earlier autonomous/tool turns are already inside
                // each later request's prompt count.
                if turn_usage.total_tokens > 0 {
                    if let Some((used, _)) = state.run_context_estimate.as_mut() {
                        *used = turn_usage.total_tokens;
                    }
                    if state.run_model.as_deref() == Some(state.model.as_str()) {
                        if let Some((used, _)) = state.context_estimate.as_mut() {
                            *used = turn_usage.total_tokens;
                        }
                    }
                }
                state.turn_streamed_output_bytes = 0;
                state.last_turn_usage = (turn_usage.total_tokens > 0).then_some(*turn_usage);
                state.cache_hit_rate_basis_points = (turn_usage.total_tokens > 0)
                    .then(|| usage_cache_hit_rate_basis_points(*turn_usage))
                    .flatten();
                state.telemetry_model = state.run_model.clone();
                state.session_cost_microdollars = *session_cost_microdollars;
                state.run_cost_microdollars = *run_cost_microdollars;
                state.run_cost_available = true;
            }
            AgentEvent::RunFinished { .. } => state.close_streaming_blocks(),
        }
        if let Some(outcome) = update.outcome {
            Self::append_outcome(&mut state, outcome);
        }
    }

    /// Update the request-context estimate at an idle boundary, where App is
    /// available to reconstruct the actual next request safely.
    pub fn set_context_estimate(&mut self, estimate: u64, budget: u64) {
        let mut state = self.state.borrow_mut();
        state.context_estimate = Some((estimate, budget));
        if state.run.is_active() && state.run_model.as_deref() == Some(state.model.as_str()) {
            state.run_context_estimate = Some((estimate, budget));
        }
    }

    /// Refresh durable session instruments outside the render loop. These
    /// values change only at run boundaries, keeping the footer stable.
    pub fn set_session_telemetry(
        &mut self,
        session: &Session,
        cache_hit_rate_basis_points: Option<u16>,
    ) {
        let telemetry_model = session
            .latest_active_checkpoint()
            .and_then(|checkpoint| session.entry(&checkpoint.prompt))
            .and_then(|entry| entry.metadata.as_ref())
            .and_then(|metadata| metadata.prompt_model.as_ref())
            .map(|model| model.0.clone());
        let session_cost_microdollars = session
            .usage_records()
            .iter()
            .any(|record| record.cost_microdollars.is_some())
            .then(|| session.total_cost_microdollars());
        let mut state = self.state.borrow_mut();
        state.session_cost_microdollars = session_cost_microdollars;
        state.telemetry_model = telemetry_model;
        state.cache_hit_rate_basis_points = state
            .selected_model_owns_telemetry()
            .then_some(cache_hit_rate_basis_points)
            .flatten();
    }

    /// Add a locally submitted prompt immediately; Agent persistence follows
    /// only after `Agent::prompt` succeeds.
    pub fn on_prompt_submitted(&mut self, prompt: &str) {
        let prompt_color = self.state.borrow().prompt_color.clone();
        self.push_local_submission(prompt, prompt_color);
    }

    /// Add a local shell escape without implying that any model received it.
    pub fn on_local_command_submitted(&mut self, command: &str) {
        self.push_local_submission(command, None);
    }

    fn push_local_submission(&mut self, prompt: &str, prompt_color: Option<String>) {
        let mut state = self.state.borrow_mut();
        state.close_streaming_blocks();
        let model_lab = state.model_lab;
        state.push_block(TranscriptBlock::User {
            text: prompt.to_owned(),
            model_lab,
            prompt_color,
            persisted: false,
        });
        // A local submission deliberately returns to the live tail; model
        // output itself never does this while the reader is browsing history.
        state.jump_to_tail();
    }

    /// Mark the locally painted prompt durable after `Agent::prompt` has
    /// successfully appended it and created a run.
    pub fn mark_prompt_persisted(&mut self) {
        if let Some(TranscriptBlock::User { persisted, .. }) = self
            .state
            .borrow_mut()
            .transcript
            .iter_mut()
            .rfind(|block| {
                matches!(
                    block,
                    TranscriptBlock::User {
                        persisted: false,
                        ..
                    }
                )
            })
        {
            *persisted = true;
        }
    }

    /// Keep a steering message in the pending area until the Agent reports
    /// that it has appended the message at the next model-turn boundary.
    pub fn queue_steering(&mut self, composed: &ComposedInput) {
        if composed.is_empty() {
            return;
        }
        let mut state = self.state.borrow_mut();
        state.steering_queue.push(QueuedSteering {
            display: composed.transcript_text.clone(),
            editor_display: composed.display_text.clone(),
            attachments: composed.attachments.clone(),
        });
    }

    /// Move undelivered steering messages back into the editor. This is used
    /// when an active run is aborted before the Agent can consume its queue.
    pub fn restore_queued_steering(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.steering_queue.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut state.steering_queue);
        let mut attachments = Vec::new();
        let mut displays = Vec::with_capacity(queued.len());
        for entry in queued {
            displays.push(entry.editor_display);
            attachments.extend(entry.attachments);
        }
        state.ledger.restore(attachments);
        let restored = displays.join("\n\n");
        let current = std::mem::take(&mut state.editor);
        state.editor = if current.trim().is_empty() {
            restored
        } else if restored.is_empty() {
            current
        } else {
            format!("{restored}\n\n{current}")
        };
        state.editor_cursor = state.editor.len();
    }

    pub fn apply_edit(&mut self, action: EditAction) {
        let resets_slash_menu = matches!(
            &action,
            EditAction::Char(_)
                | EditAction::Paste(_)
                | EditAction::Backspace
                | EditAction::Delete
                | EditAction::Newline
        );
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
                match composer::classify_paste(&pasted) {
                    composer::PasteKind::Verbatim => {
                        let cursor = state.editor_cursor;
                        state.editor.insert_str(cursor, &pasted);
                        state.editor_cursor = cursor + pasted.len();
                    }
                    composer::PasteKind::LargeText => {
                        let chip = state.ledger.attach_pasted_text(pasted);
                        let cursor = state.editor_cursor;
                        state.editor.insert_str(cursor, &chip);
                        state.editor_cursor = cursor + chip.len();
                    }
                    composer::PasteKind::MediaFile(path) => {
                        let modalities = state.input_modalities;
                        match state.ledger.attach_media(&path, modalities) {
                            Ok(chip) => {
                                let cursor = state.editor_cursor;
                                state.editor.insert_str(cursor, &chip);
                                state.editor_cursor = cursor + chip.len();
                            }
                            Err(error) => {
                                state.push_block(TranscriptBlock::Notice(error.to_string()));
                                let cursor = state.editor_cursor;
                                state.editor.insert_str(cursor, &pasted);
                                state.editor_cursor = cursor + pasted.len();
                            }
                        }
                    }
                    composer::PasteKind::DocumentFile(path) => {
                        match state.ledger.attach_file_reference(&path) {
                            Ok(chip) => {
                                let cursor = state.editor_cursor;
                                state.editor.insert_str(cursor, &chip);
                                state.editor_cursor = cursor + chip.len();
                            }
                            Err(error) => {
                                state.push_block(TranscriptBlock::Notice(error.to_string()));
                                let cursor = state.editor_cursor;
                                state.editor.insert_str(cursor, &pasted);
                                state.editor_cursor = cursor + pasted.len();
                            }
                        }
                    }
                    composer::PasteKind::NonMediaFile(_) => {
                        let cursor = state.editor_cursor;
                        state.editor.insert_str(cursor, &pasted);
                        state.editor_cursor = cursor + pasted.len();
                    }
                }
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
                let last_row = layout.lines.len().saturating_sub(1);
                if matches!(action, EditAction::Up) && layout.cursor_row == 0 {
                    state.editor_cursor = 0;
                } else if matches!(action, EditAction::Down) && layout.cursor_row == last_row {
                    state.editor_cursor = state.editor.len();
                } else {
                    let current = &layout.lines[layout.cursor_row];
                    let target_row = if matches!(action, EditAction::Up) {
                        layout.cursor_row - 1
                    } else {
                        layout.cursor_row + 1
                    };
                    let column = editor_column(&state.editor, current, state.editor_cursor);
                    state.editor_cursor =
                        editor_offset_at_column(&state.editor, &layout.lines[target_row], column);
                }
            }
            EditAction::Home | EditAction::End => {
                let layout = editor_layout(&state.editor, state.editor_cursor, state.size.0);
                let line = &layout.lines[layout.cursor_row];
                state.editor_cursor = if matches!(action, EditAction::Home) {
                    line.start
                } else {
                    line.visible_end
                };
            }
        }

        if resets_slash_menu {
            state.slash_selection = 0;
            state.slash_scroll = 0;
            state.slash_popup_dismissed = false;
        }
        if state.editor_cursor == state.editor.len()
            && composer::active_mention(&state.editor).is_some()
            && state.file_index.is_none()
        {
            if let Some(root) = state.workspace.clone() {
                state.file_index = Some(composer::workspace_files(&root, 10_000));
            }
        }
    }

    /// Complete a unique slash-command prefix at the end of the prompt.
    pub fn complete_slash_command(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor_cursor != state.editor.len() {
            return;
        }
        let suggestions = input_slash_suggestions(&state);
        if let [suggestion] = suggestions.as_slice() {
            let completed = format!(
                "/{}{}",
                suggestion.name,
                if suggestion.accepts_argument { " " } else { "" }
            );
            state.editor = completed;
            state.editor_cursor = state.editor.len();
            state.slash_popup_dismissed = true;
        }
    }

    pub fn slash_popup_open(&self) -> bool {
        let state = self.state.borrow();
        !state.slash_popup_dismissed && !input_slash_suggestions(&state).is_empty()
    }

    /// Navigate or accept the live slash-command popup without turning it into
    /// a heavyweight modal panel.
    pub fn slash_menu(&mut self, action: SlashMenuAction) {
        let mut state = self.state.borrow_mut();
        let suggestions = input_slash_suggestions(&state);
        if suggestions.is_empty() {
            return;
        }
        let last = suggestions.len().saturating_sub(1);
        state.slash_selection = state.slash_selection.min(last);
        // Use the actual rendered popup viewport (excluding its one heading
        // row), so Page Up/Down remain correct after resize, wrapped errors, or
        // composer growth rather than relying on a stale terminal-height guess.
        let page = shell_chrome(&state, state.size.0, Instant::now())
            .suggestions
            .len()
            .saturating_sub(1)
            .max(1);
        match action {
            SlashMenuAction::Previous => {
                state.slash_selection = state.slash_selection.saturating_sub(1)
            }
            SlashMenuAction::Next => {
                state.slash_selection = state.slash_selection.saturating_add(1).min(last)
            }
            SlashMenuAction::First => state.slash_selection = 0,
            SlashMenuAction::Last => state.slash_selection = last,
            SlashMenuAction::PageUp => {
                state.slash_selection = state.slash_selection.saturating_sub(page)
            }
            SlashMenuAction::PageDown => {
                state.slash_selection = state.slash_selection.saturating_add(page).min(last)
            }
            SlashMenuAction::Select => {
                let command = &suggestions[state.slash_selection];
                state.editor = format!(
                    "/{}{}",
                    command.name,
                    if command.accepts_argument { " " } else { "" }
                );
                state.editor_cursor = state.editor.len();
                state.slash_popup_dismissed = true;
                return;
            }
            SlashMenuAction::Close => {
                state.slash_popup_dismissed = true;
                return;
            }
        }
        state.slash_popup_dismissed = false;
        if state.slash_selection < state.slash_scroll {
            state.slash_scroll = state.slash_selection;
        } else if state.slash_selection >= state.slash_scroll.saturating_add(page) {
            state.slash_scroll = state.slash_selection + 1 - page;
        }
        state.slash_scroll = state
            .slash_scroll
            .min(suggestions.len().saturating_sub(page));
    }

    /// Drop the mention file index so the next `@` completion re-walks the
    /// workspace. Called after a run ends, when tools may have created files.
    pub fn invalidate_file_index(&mut self) {
        self.state.borrow_mut().file_index = None;
    }

    pub fn set_workspace(&mut self, root: PathBuf) {
        let mut state = self.state.borrow_mut();
        // update_status re-asserts the workspace after every turn; only a
        // real root change invalidates the lazily built mention index.
        if state.workspace.as_deref() != Some(root.as_path()) {
            state.file_index = None;
        }
        state.workspace = Some(root);
        state.refresh_tool_displays();
    }

    /// Replace the immutable prompt-template autocomplete snapshot after a
    /// startup discovery or idle-boundary reload.
    pub fn set_prompt_templates(
        &mut self,
        templates: Arc<[crate::prompts::PromptTemplateDescriptor]>,
    ) {
        let mut state = self.state.borrow_mut();
        state.prompt_templates = templates;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    pub fn set_extension_commands(&mut self, commands: Arc<[(String, String)]>) {
        let mut state = self.state.borrow_mut();
        state.extension_commands = commands;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    /// Complete the trailing `@token`: media files attach, others insert a
    /// plain `@relative/path` reference.
    pub fn complete_mention(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor_cursor != state.editor.len() {
            return;
        }
        let Some(query) = composer::active_mention(&state.editor).map(str::to_owned) else {
            return;
        };
        let Some(root) = state.workspace.clone() else {
            return;
        };

        // When the query looks like a path (contains a separator or starts
        // with `.` / `..`), do a live filesystem listing so `@../../` and
        // `@src/` completions work.
        let looks_like_path = query.contains('/') || query.starts_with('.') || query.contains('\\');
        let top: Option<String> = if looks_like_path {
            let matches = composer::live_path_matches(&root, &query, 1);
            matches.into_iter().next()
        } else {
            if state.file_index.is_none() {
                state.file_index = Some(composer::workspace_files(&root, 10_000));
            }
            let files = state.file_index.as_ref().expect("file index just built");
            composer::mention_matches(files, &query, 1)
                .first()
                .copied()
                .map(str::to_owned)
        };
        let Some(top) = top else {
            return;
        };
        let token_start = state.editor.len() - (query.len() + 1);
        let absolute = root.join(&top);
        if composer::media_kind_for_path(&absolute).is_some() {
            let modalities = state.input_modalities;
            match state.ledger.attach_media(&absolute, modalities) {
                Ok(chip) => state.editor.replace_range(token_start.., &chip),
                Err(error) => {
                    state.push_block(TranscriptBlock::Notice(error.to_string()));
                    state
                        .editor
                        .replace_range(token_start.., &format!("@{top} "));
                }
            }
        } else if composer::file_kind_for_path(&absolute).is_some() {
            match state.ledger.attach_file_reference(&absolute) {
                Ok(chip) => state.editor.replace_range(token_start.., &chip),
                Err(error) => {
                    state.push_block(TranscriptBlock::Notice(error.to_string()));
                    state
                        .editor
                        .replace_range(token_start.., &format!("@{top} "));
                }
            }
        } else {
            state
                .editor
                .replace_range(token_start.., &format!("@{top} "));
        }
        state.editor_cursor = state.editor.len();
    }

    pub fn set_identity(&mut self, provider: &str, model: &str, reasoning: &str) {
        let mut state = self.state.borrow_mut();
        let welcome_changed = state.model != model || state.reasoning != reasoning;
        if state.model != model {
            if !state.run.is_active() && state.telemetry_model.as_deref() != Some(model) {
                state.clear_turn_telemetry();
            }
            let display = crate::presentation::derive_model_display_name(model);
            state.model_compact_names = crate::presentation::model_display_name_variants(&display);
            state.model_display = display;
            state.prompt_color = (!model.trim().is_empty())
                .then(|| crate::tui::theme::prompt_color_for_model_id(model));
        }
        state.provider = provider.to_owned();
        state.model = model.to_owned();
        state.reasoning = reasoning.to_owned();
        if welcome_changed {
            state.restart_welcome_animation();
        }
    }

    pub fn set_verbose_tools(&mut self, verbose: bool) {
        let mut state = self.state.borrow_mut();
        if state.verbose_tools == verbose {
            return;
        }
        state.verbose_tools = verbose;
        // Keep the existing width/layout caches and invalidate only blocks
        // whose disclosure actually changes. The old full-layout reset made
        // Ctrl+O reparse every assistant answer in long sessions.
        state.invalidate_disclosure();
    }

    pub fn verbose_tools(&self) -> bool {
        self.state.borrow().verbose_tools
    }

    pub fn toggle_verbose_tools(&mut self) -> bool {
        let next = !self.verbose_tools();
        self.set_verbose_tools(next);
        next
    }

    pub fn set_status_detail(&mut self, detail: String) {
        self.state.borrow_mut().status_detail = detail;
    }

    pub fn set_extension_header(&mut self, text: Option<(String, Option<String>)>) {
        self.state.borrow_mut().extension_header = sanitize_extension_surface(text);
    }

    pub fn set_extension_status(&mut self, text: Option<(String, Option<String>)>) {
        self.state.borrow_mut().extension_status = sanitize_extension_surface(text);
    }

    pub fn set_extension_footer(&mut self, text: Option<(String, Option<String>)>) {
        self.state.borrow_mut().extension_footer = sanitize_extension_surface(text);
    }

    pub fn apply_extension_tool_renderer(
        &mut self,
        id: &ToolCallId,
        segments: &[ygg_agent::extension_process::ToolRenderSegment],
    ) {
        let mut state = self.state.borrow_mut();
        let index = state.tool_panels.get(id).copied();
        if let Some(panel) = state.tool_output_mut(id) {
            panel.extension_render_segments = sanitize_extension_tool_render_segments(segments);
        }
        if let Some(index) = index {
            state.touch_block(index);
        }
    }

    pub fn status_detail(&self) -> String {
        self.state.borrow().status_detail.clone()
    }

    pub fn set_run_label(&mut self, label: &str) {
        let mut state = self.state.borrow_mut();
        let run_label = if label == "idle" || label.starts_with("run:") {
            String::new()
        } else {
            label
                .trim_end_matches('…')
                .trim_end_matches("...")
                .to_owned()
        };
        if run_label == "compacting" {
            state.shimmer_started_at = Some(Instant::now());
        } else if run_label.is_empty() {
            state.shimmer_started_at = None;
        }
        state.run_label = run_label;
    }

    pub fn set_size(&mut self, columns: u16, rows: u16) {
        let resized = self.state.borrow().size != (columns, rows);
        if resized {
            // A resize discards terminal-owned history. Materialize the exact
            // deferred branch snapshot first so the destructive replay owns a
            // complete transcript, even while a new run is active.
            if let Err(error) = self.materialize_deferred_history() {
                self.state.borrow_mut().error =
                    Some(format!("could not load older session history: {error}"));
            }
        }
        *self.size.lock().expect("terminal size mutex poisoned") = (columns, rows);
        let mut state = self.state.borrow_mut();
        state.size = (columns, rows);
        // Reflow belongs exclusively to `ygg-tui-render`. Computing the scroll
        // maximum here used to rebuild a long transcript on the input thread,
        // immediately discard that layout, and rebuild it again for paint.
        state.invalidate_transcript_layout();
    }

    #[allow(dead_code)]
    pub fn columns(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").0
    }

    #[allow(dead_code)]
    pub fn theme(&self) -> YggTheme {
        self.state.borrow().theme.clone()
    }

    pub fn set_theme_config(&mut self, config: Config) {
        let mut state = self.state.borrow_mut();
        state.max_session_cost_microdollars = config.max_cost_microdollars;
        state.show_turn_cost = config.show_turn_cost;
        drop(state);
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

    pub fn set_tool_input_prompt(&mut self, prompt: Option<String>) {
        self.state.borrow_mut().tool_input_prompt = prompt.map(|prompt| {
            sanitize_for_terminal(&prompt)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        });
    }

    pub fn set_input_modalities(&mut self, modalities: ModalitySet) {
        self.state.borrow_mut().input_modalities = modalities;
    }

    /// Drain the editor and resolve chips into ordered parts.
    pub fn drain_composed(&mut self) -> ComposedInput {
        let mut state = self.state.borrow_mut();
        state.editor_cursor = 0;
        let mut text = std::mem::take(&mut state.editor);

        // Drag/drop is not consistently delivered as a bracketed-paste event.
        // When it arrives as ordinary keys, promote every existing media path
        // at submit time even if the user added prompt text around it.
        let dropped = composer::dropped_paths_in_text(&text);
        if !dropped.is_empty() {
            let mut rewritten = String::with_capacity(text.len());
            let mut cursor = 0;
            let mut errors = Vec::new();
            for (range, path) in dropped {
                rewritten.push_str(&text[cursor..range.start]);
                let replacement = if composer::media_kind_for_path(&path).is_some() {
                    let modalities = state.input_modalities;
                    match state.ledger.attach_media(&path, modalities) {
                        Ok(chip) => Some(chip),
                        Err(error) => {
                            errors.push(error.to_string());
                            None
                        }
                    }
                } else if composer::file_kind_for_path(&path).is_some() {
                    match state.ledger.attach_file_reference(&path) {
                        Ok(chip) => Some(chip),
                        Err(error) => {
                            errors.push(error.to_string());
                            None
                        }
                    }
                } else {
                    None
                };
                if let Some(replacement) = replacement {
                    rewritten.push_str(&replacement);
                } else {
                    rewritten.push_str(&text[range.clone()]);
                }
                cursor = range.end;
            }
            rewritten.push_str(&text[cursor..]);
            text = rewritten;
            for error in errors {
                state.push_block(TranscriptBlock::Notice(error));
            }
        }

        if state.ledger.is_empty() {
            ComposedInput::from_text(text)
        } else {
            composer::compose(text, &mut state.ledger)
        }
    }

    /// Put a failed submission back in the editor without losing attachment
    /// payloads. Composition hooks run before persistence, so their failure
    /// must be observationally equivalent to a validation error.
    pub fn restore_composed(&mut self, composed: ComposedInput) {
        let mut state = self.state.borrow_mut();
        state.editor = composed.display_text;
        state.editor_cursor = state.editor.len();
        state.ledger.restore(composed.attachments);
    }

    /// Discard the current draft and every attachment it owns.
    pub fn clear_editor(&mut self) {
        let mut state = self.state.borrow_mut();
        state.editor.clear();
        state.editor_cursor = 0;
        state.ledger.clear();
        state.slash_selection = 0;
        state.slash_scroll = 0;
        state.slash_popup_dismissed = false;
    }

    pub fn drain_editor(&mut self) -> String {
        let mut state = self.state.borrow_mut();
        state.editor_cursor = 0;
        state.slash_selection = 0;
        state.slash_scroll = 0;
        state.slash_popup_dismissed = false;
        std::mem::take(&mut state.editor)
    }

    fn materialize_deferred_history(&mut self) -> Result<bool> {
        materialize_deferred_session_history(&self.state)
    }

    pub fn scroll(&mut self, direction: i16) {
        if direction < 0 {
            let should_materialize = {
                let state = self.state.borrow();
                let page = usize::from(state.size.1.max(4) / 2);
                let maximum = max_scroll_from_bottom(&state, state.size.0);
                state.deferred_session_history.is_some()
                    && !state.run.is_active()
                    && state.scroll_from_bottom.get().saturating_add(page) >= maximum
            };
            if should_materialize {
                if let Err(error) = self.materialize_deferred_history() {
                    self.state.borrow_mut().error =
                        Some(format!("could not load older session history: {error}"));
                }
            }
        }
        let mut state = self.state.borrow_mut();
        let page = usize::from(state.size.1.max(4) / 2);
        let maximum = max_scroll_from_bottom(&state, state.size.0);
        let current = state.scroll_from_bottom.get().min(maximum);
        state.scroll_from_bottom.set(current);
        if direction < 0 {
            let next = current.saturating_add(page).min(maximum);
            state.scroll_from_bottom.set(next);
            state.follow_tail = next == 0;
        } else {
            let next = current.saturating_sub(page);
            state.scroll_from_bottom.set(next);
            if next == 0 {
                state.jump_to_tail();
            }
        }
    }

    /// Scroll the transcript in small, trackpad-friendly increments.
    pub fn scroll_lines(&mut self, direction: i16) {
        if direction < 0 {
            let should_materialize = {
                let state = self.state.borrow();
                let maximum = max_scroll_from_bottom(&state, state.size.0);
                state.deferred_session_history.is_some()
                    && !state.run.is_active()
                    && state
                        .scroll_from_bottom
                        .get()
                        .saturating_add(direction.unsigned_abs() as usize)
                        >= maximum
            };
            if should_materialize {
                if let Err(error) = self.materialize_deferred_history() {
                    self.state.borrow_mut().error =
                        Some(format!("could not load older session history: {error}"));
                }
            }
        }
        let mut state = self.state.borrow_mut();
        if direction < 0 {
            let next = state
                .scroll_from_bottom
                .get()
                .saturating_add(direction.unsigned_abs() as usize);
            state.scroll_from_bottom.set(next);
            let maximum = max_scroll_from_bottom(&state, state.size.0);
            let next = state.scroll_from_bottom.get().min(maximum);
            state.scroll_from_bottom.set(next);
            state.follow_tail = next == 0;
        } else {
            let next = state
                .scroll_from_bottom
                .get()
                .saturating_sub(direction as usize);
            state.scroll_from_bottom.set(next);
            if next == 0 {
                state.jump_to_tail();
            }
        }
    }

    /// Explicit End/jump-to-live action. It preserves the draft and composer
    /// focus because it mutates only transcript viewport state.
    pub fn jump_to_tail(&mut self) {
        self.state.borrow_mut().jump_to_tail();
    }

    fn transcript_position_at_screen_cell(
        state: &ShellState,
        row: u16,
        col: u16,
    ) -> Option<TranscriptPosition> {
        let chrome = shell_chrome(state, state.size.0, Instant::now());
        let transcript = transcript_lines(state, state.size.0);
        let max_scroll = max_scroll_for_available(transcript.len(), chrome.transcript_rows);
        let scroll = state.scroll_from_bottom.get().min(max_scroll);
        let capacity = transcript_viewport_capacity(chrome.transcript_rows, scroll > 0);
        if usize::from(row) >= capacity {
            return None;
        }
        let end = transcript.len().saturating_sub(scroll);
        let start = end.saturating_sub(capacity);
        selection_position_for_visual_cell(state, start + usize::from(row), col)
    }

    /// Record a pointer-down position in the transcript area. No selection
    /// is created until the pointer actually moves. A stationary click
    /// simply clears any prior selection and does nothing else.
    /// Shift+click extends an existing selection.
    pub fn begin_transcript_selection(&mut self, row: u16, col: u16, extend: bool) {
        let mut state = self.state.borrow_mut();
        let Some(position) = Self::transcript_position_at_screen_cell(&state, row, col) else {
            state.pending_selection_anchor = None;
            state.selection_dragging = false;
            return;
        };
        if extend {
            // Shift-click: anchor from the prior selection (if any),
            // focus at the clicked position. Start the selection
            // immediately.
            let anchor = state
                .transcript_selection
                .as_ref()
                .map(|selection| selection.anchor)
                .unwrap_or(position);
            state.transcript_selection = Some(TranscriptSelection {
                anchor,
                focus: position,
            });
            state.pending_selection_anchor = None;
            state.selection_dragging = true;
        } else {
            // Plain click: defer selection creation until the first
            // movement. If the pointer is released without moving, the
            // prior selection is cleared in `end_transcript_selection`.
            state.pending_selection_anchor = Some(position);
            state.selection_dragging = false;
        }
    }

    /// Extend an active drag, or promote a pending click into a selection
    /// once the pointer has actually moved to a different terminal cell.
    /// Movement within the same semantic position remains a stationary
    /// click — no selection is created — so that trackpad jitter and
    /// low-movement mouse events don't accidentally start a selection.
    ///
    /// Crossing the top/bottom transcript boundary scrolls modestly and
    /// keeps selection ownership in the transcript even while the pointer
    /// is over pinned chrome.
    pub fn extend_transcript_selection(&mut self, row: u16, col: u16) {
        // The pending anchor remains a semantic transcript coordinate rather
        // than a screen cell. Reflow can therefore occur between press and
        // drag without changing which content owns the gesture or selecting
        // pinned composer/footer text.
        //
        // Promote only after observing real movement.
        let mut state = self.state.borrow_mut();
        if !state.selection_dragging {
            let anchor = match state.pending_selection_anchor {
                Some(anchor) => anchor,
                None => return,
            };
            let current = Self::transcript_position_at_screen_cell(&state, row, col);
            if current == Some(anchor) {
                return;
            }
            // A real cell transition promotes the pending click and starts the selection.
            state.pending_selection_anchor = None;
            state.transcript_selection = Some(TranscriptSelection {
                anchor,
                focus: current.unwrap_or(anchor),
            });
            state.selection_dragging = true;
        }

        let mut transcript_rows = transcript_viewport_capacity_for_state(&state, state.size.0);
        if transcript_rows == 0 {
            return;
        }
        if row == 0 {
            let maximum = max_scroll_from_bottom(&state, state.size.0);
            let next = state
                .scroll_from_bottom
                .get()
                .saturating_add(2)
                .min(maximum);
            state.scroll_from_bottom.set(next);
            state.follow_tail = next == 0;
        } else if usize::from(row) >= transcript_rows {
            let next = state.scroll_from_bottom.get().saturating_sub(2);
            state.scroll_from_bottom.set(next);
            if next == 0 {
                state.jump_to_tail();
            }
        }
        transcript_rows = transcript_viewport_capacity_for_state(&state, state.size.0);
        if transcript_rows == 0 {
            return;
        }
        let clamped = row.min(transcript_rows.saturating_sub(1) as u16);
        if let Some(position) = Self::transcript_position_at_screen_cell(&state, clamped, col) {
            if let Some(selection) = state.transcript_selection.as_mut() {
                selection.focus = position;
            }
        }
    }

    /// Finish a pointer gesture:
    /// - Drag that created a selection -> copy to clipboard, keep selection.
    /// - Stationary click (no drag)    -> clear any prior selection.
    pub fn end_transcript_selection(&mut self, row: u16, col: u16) {
        // Copy is semantic and application-owned: terminal padding, ANSI, the
        // composer, and footer never enter the payload. The retained buffer
        // remains available even when OSC 52 transport is unavailable.
        //
        let had_pending = self.state.borrow().pending_selection_anchor.is_some();
        if had_pending {
            // Clear any previous selection and discard the pending anchor.
            let mut state = self.state.borrow_mut();
            state.pending_selection_anchor = None;
            state.transcript_selection = None;
            state.selection_dragging = false;
            return;
        }

        self.extend_transcript_selection(row, col);
        if self.state.borrow().transcript_selection.is_some() {
            let _ = self.copy_selected_plain_text();
        }
        self.state.borrow_mut().selection_dragging = false;
    }

    /// Best-effort OSC 52 clipboard transport. The semantic fallback is
    /// retained separately in `copy_buffer`, so redirected output loses no data.
    fn set_clipboard(text: &str) {
        #[cfg(target_os = "macos")]
        {
            if let Ok(mut child) = std::process::Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = child.wait();
            }
        }

        if !std::io::stdout().is_terminal() {
            return;
        }
        // OSC 52 is best-effort transport; `copy_buffer` remains authoritative
        // when stdout is redirected or the terminal declines the sequence.
        let encoded = BASE64.encode(text);
        // Stay below the common 64 KiB OSC payload limit. Trim the source on a
        // UTF-8 boundary and re-encode so the transmitted payload stays valid.
        // The first encoding keeps the normal, untruncated path allocation-free
        // apart from the payload itself.
        let payload = if encoded.len() <= 64 * 1024 {
            encoded
        } else {
            let mut end = text.len();
            while end > 0 {
                let candidate = &text[..end];
                if BASE64.encode(candidate).len() <= 64 * 1024 {
                    break;
                }
                // Move back to the preceding complete scalar before retrying.
                end = end.saturating_sub(1);
                while end > 0 && !text.is_char_boundary(end) {
                    end = end.saturating_sub(1);
                }
            }
            // Re-encode only after the largest transport-safe UTF-8 prefix is
            // known; slicing encoded base64 would produce invalid padding.
            BASE64.encode(&text[..end])
        };
        // BEL termination is widely supported and avoids exposing a printable
        // suffix if a terminal does not implement OSC 52.
        let osc = format!("\x1b]52;c;{payload}\x07");
        let _ = std::io::stdout().write_all(osc.as_bytes());
        let _ = std::io::stdout().flush();
    }

    /// Select the complete semantic transcript. This is deliberately separate
    /// from editor selection, so pinned chrome can never enter the copy range.
    pub fn select_all_transcript(&mut self) {
        if let Err(error) = self.materialize_deferred_history() {
            self.state.borrow_mut().error =
                Some(format!("could not load older session history: {error}"));
        }
        let mut state = self.state.borrow_mut();
        let Some(last) = state.transcript.len().checked_sub(1) else {
            state.transcript_selection = None;
            return;
        };
        let last_offset = block_copy_text(&state.transcript[last]).len();
        state.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                block: 0,
                offset: 0,
                trailing_affinity: false,
            },
            focus: TranscriptPosition {
                block: last,
                offset: last_offset,
                trailing_affinity: true,
            },
        });
    }

    /// Return clean text for the logical selection and retain it as an
    /// explicit fallback copy buffer. A future/native clipboard transport can
    /// consume this value without ever scraping padded terminal cells.
    pub fn selected_plain_text(&self) -> Option<String> {
        semantic_selected_text(&self.state.borrow())
    }

    pub fn copy_selected_plain_text(&mut self) -> Option<String> {
        let copy = self.selected_plain_text()?;
        let mut state = self.state.borrow_mut();
        state.copy_buffer = Some(copy.clone());
        drop(state);
        Self::set_clipboard(&copy);
        Some(copy)
    }

    /// Copyable original Markdown for assistant blocks, with plain semantic
    /// text for non-Markdown events. This preserves code and links faithfully.
    #[allow(dead_code)] // Public presentation action; command wiring follows selection gestures.
    pub fn copy_selected_markdown(&mut self) -> Option<String> {
        let selection = self.state.borrow().transcript_selection.clone()?;
        let (start, end) = if selection.anchor.block <= selection.focus.block {
            (selection.anchor.block, selection.focus.block)
        } else {
            (selection.focus.block, selection.anchor.block)
        };
        let state = self.state.borrow();
        Some(
            (start..=end)
                .map(|index| match &state.transcript[index] {
                    TranscriptBlock::Assistant(assistant) => assistant.text.clone(),
                    block => block_copy_text(block),
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    #[cfg(test)]
    fn copy_buffer(&self) -> Option<String> {
        self.state.borrow().copy_buffer.clone()
    }

    pub fn show_overlay_text(&mut self, text: String) {
        self.state.borrow_mut().overlay = Some(ShellOverlay::Text(sanitize_for_terminal(&text)));
    }

    pub fn show_context_report(&mut self, report: crate::tui::context::ContextReport) {
        self.state.borrow_mut().overlay = Some(ShellOverlay::Context(report));
    }

    /// Toggle the global transcript disclosure mode (ctrl+o).
    pub fn expand_focused_tool(&mut self) {
        self.toggle_verbose_tools();
    }

    pub fn show_compaction_summary(&mut self) {
        let mut state = self.state.borrow_mut();
        if let Some(index) = state
            .transcript
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::Compaction(_)))
        {
            if let TranscriptBlock::Compaction(compaction) = &mut state.transcript[index] {
                compaction.expanded = true;
            }
            state.touch_block(index);
        } else {
            state.error = Some("no compaction summary found in session history".into());
        }
    }

    /// Show picker output that already contains Ygg-generated foreground SGR.
    #[allow(dead_code)]
    pub fn show_styled_overlay_text(&mut self, text: String) {
        self.state.borrow_mut().overlay = Some(ShellOverlay::Text(text));
    }

    #[allow(dead_code)]
    pub fn show_status_text(&mut self, text: String) {
        let mut state = self.state.borrow_mut();
        state.overlay = Some(ShellOverlay::Text(styled_status_text(&state.theme, &text)));
    }

    pub fn show_status_text_with_telemetry(&mut self, text: String) {
        let mut state = self.state.borrow_mut();
        let text = format!("{text}\n\n{}", status_telemetry(&state, Instant::now()));
        state.overlay = Some(ShellOverlay::Text(styled_status_text(&state.theme, &text)));
    }

    pub fn close_overlay(&mut self) {
        self.state.borrow_mut().overlay = None;
    }

    pub fn has_overlay(&self) -> bool {
        self.state.borrow().overlay.is_some()
    }

    /// Requests a coordinated interactive close at the next owning boundary.
    pub fn request_close(&mut self) {
        self.state.borrow_mut().close_requested = true;
    }

    /// Returns whether any input owner requested a coordinated close.
    pub fn close_requested(&self) -> bool {
        self.state.borrow().close_requested
    }

    /// Open an interactive panel.
    pub fn open_panel(&mut self, panel: Panel) {
        self.state.borrow_mut().panel = Some(panel);
    }

    /// Close any open panel and return to normal editing.
    pub fn close_panel(&mut self) {
        self.state.borrow_mut().panel = None;
    }

    pub fn has_panel(&self) -> bool {
        self.state.borrow().panel.is_some()
    }

    /// Handle a keyboard event destined for the active panel. Returns
    /// `Some((result, action))` when the panel has finished; `None` when
    /// the panel consumed the event but remains open.
    pub fn panel_input(
        &mut self,
        event: &crossterm::event::Event,
    ) -> Option<(PanelResult, PanelAction)> {
        let mut state = self.state.borrow_mut();
        let page_step = usize::from(state.size.1).saturating_sub(8).max(1);
        let panel = state.panel.as_mut()?;
        // Snapshot the action before we potentially mutate/drop the panel.
        let action = match panel {
            Panel::SelectList { action, .. } => action.clone(),
        };
        match panel {
            Panel::SelectList {
                items,
                descriptions,
                selected,
                filter,
                ..
            } => {
                use crossterm::event::{Event, KeyCode, KeyModifiers};
                match event {
                    Event::Key(key) if crate::tui::keymap::accepts_key_event(key) => {
                        match key.code {
                            KeyCode::Esc => {
                                drop(state);
                                self.close_panel();
                                return Some((PanelResult::Cancel, action));
                            }
                            KeyCode::Enter if key.modifiers.is_empty() => {
                                // `selected` is a position within the filtered
                                // list; map it back to the original item index.
                                let filtered = filtered_indices(items, descriptions, filter);
                                if let Some(&index) = filtered.get(*selected) {
                                    drop(state);
                                    self.close_panel();
                                    return Some((PanelResult::Confirm(index), action));
                                }
                                // Nothing matches the filter; keep the panel open.
                            }
                            KeyCode::Up if key.modifiers.is_empty() => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Down if key.modifiers.is_empty() => {
                                if *selected + 1
                                    < filtered_indices(items, descriptions, filter).len()
                                {
                                    *selected += 1;
                                }
                            }
                            KeyCode::Home if key.modifiers.is_empty() => {
                                *selected = 0;
                            }
                            KeyCode::End if key.modifiers.is_empty() => {
                                *selected = filtered_indices(items, descriptions, filter)
                                    .len()
                                    .saturating_sub(1);
                            }
                            KeyCode::PageUp if key.modifiers.is_empty() => {
                                *selected = selected.saturating_sub(page_step);
                            }
                            KeyCode::PageDown if key.modifiers.is_empty() => {
                                let last = filtered_indices(items, descriptions, filter)
                                    .len()
                                    .saturating_sub(1);
                                *selected = selected.saturating_add(page_step).min(last);
                            }
                            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if *selected + 1
                                    < filtered_indices(items, descriptions, filter).len()
                                {
                                    *selected += 1;
                                }
                            }
                            KeyCode::Char(c)
                                if !key.modifiers.intersects(
                                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                                ) =>
                            {
                                filter.push(c);
                                // The match set changed; restart at the top.
                                *selected = 0;
                            }
                            KeyCode::Backspace if key.modifiers.is_empty() => {
                                filter.pop();
                                *selected = 0;
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(columns, rows) => {
                        drop(state);
                        self.set_size(*columns, *rows);
                    }
                    _ => {}
                }
                None
            }
        }
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

    /// Append a running shell command placeholder with a spinner.
    /// Returns the block id so the caller can update and finalize it.
    pub fn append_shell_in_progress(&mut self, command: String) -> String {
        let mut state = self.state.borrow_mut();
        state.event_dot_visible = true;
        let id = format!("shell-{}", state.transcript.len());
        state.push_block(TranscriptBlock::Shell(Box::new(ShellOutput {
            id: id.clone(),
            command,
            output: String::new(),
            exit_code: 0,
            running: true,
            spinner: "⠋".to_string(),
        })));
        id
    }

    /// Replace the bounded live output shown by an in-progress local shell
    /// command. The caller coalesces pipe reads, so this updates one retained
    /// block without rebuilding unrelated transcript history.
    pub fn update_shell_output(&mut self, id: &str, output: String) {
        let mut state = self.state.borrow_mut();
        let index = state
            .transcript
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::Shell(shell) if shell.id == id));
        if let Some(index) = index {
            if let TranscriptBlock::Shell(shell) = &mut state.transcript[index] {
                shell.output = output;
            }
            state.touch_block(index);
        }
    }

    /// Update the spinner character on an in-progress shell block.
    pub fn update_shell_spinner(&mut self, id: &str, spinner: &str) {
        let mut state = self.state.borrow_mut();
        let index = state
            .transcript
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::Shell(shell) if shell.id == id));
        if let Some(index) = index {
            if let TranscriptBlock::Shell(shell) = &mut state.transcript[index] {
                shell.spinner = spinner.to_string();
            }
            state.touch_block(index);
        }
    }

    /// Finalize a shell block with its output and exit code.
    pub fn finalize_shell(&mut self, id: &str, output: String, exit_code: i32) {
        let mut state = self.state.borrow_mut();
        let index = state
            .transcript
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::Shell(shell) if shell.id == id));
        if let Some(index) = index {
            if let TranscriptBlock::Shell(shell) = &mut state.transcript[index] {
                shell.running = false;
                shell.output = output;
                shell.exit_code = exit_code;
                shell.spinner.clear();
            }
            state.touch_block(index);
        }
    }

    pub fn compaction_marker(&mut self, label: impl Into<String>, summary: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        let summary = summary.into();
        state.latest_compaction_summary = Some(summary.clone());
        state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
            label: label.into(),
            summary,
            expanded: false,
        })));
    }

    pub fn native_compaction_marker(&mut self, label: impl Into<String>) {
        self.state
            .borrow_mut()
            .push_block(TranscriptBlock::Notice(label.into()));
    }

    /// Update stable presentation metadata when the active model changes, then
    /// refresh the model-aware accent when its creator family changes.
    pub fn set_model_theme(&mut self, model: &Model) {
        let lab = crate::tui::theme::model_lab(model);
        let prompt_color = crate::tui::theme::prompt_color_for_model(model);
        let metadata = ModelDisplayMetadata::resolve(&model.spec);
        let price_display = PriceDisplay::from_pricing(model.spec.pricing.as_ref());
        let mut state = self.state.borrow_mut();
        state.model_display = metadata.name;
        state.model_compact_names = metadata.compact_names;
        state.price_display = price_display;
        state.prompt_color = Some(prompt_color);
        state.restart_welcome_animation();
        if state.model_lab == Some(lab) {
            return;
        }
        crate::tui::theme::apply_model_lab(&mut state.theme, lab);
        state.model_lab = Some(lab);
        state.invalidate_rich_text();
    }

    pub fn set_theme(&mut self, mut theme: YggTheme) {
        let mut state = self.state.borrow_mut();
        if let Some(lab) = state.model_lab {
            crate::tui::theme::apply_model_lab(&mut theme, lab);
        }
        state.theme = theme;
        state.theme_epoch = state.theme_epoch.wrapping_add(1);
        state.invalidate_rich_text();
        // Native scrollback cannot be recoloured retroactively. The theme
        // epoch forces a complete visible-grid repaint while terminal-owned
        // history keeps its original cells and styles.
    }

    /// Rebuild the visible transcript from the session's active branch.
    pub fn hydrate(&mut self, session: &Session) -> Result<()> {
        let entry_budget = usize::from(self.state.borrow().size.1)
            .saturating_mul(4)
            .clamp(64, 256);
        let (items, history_deferred) = hydrate_transcript_tail(session, entry_budget)?;
        let deferred_snapshot = history_deferred.then(|| DeferredSessionHistory {
            path: session.path().to_owned(),
            head: session
                .head()
                .expect("a truncated active branch must have a session head"),
            retained_id_end: 0,
        });
        let checkpoint = session.latest_active_checkpoint();
        let latest_turn = session.latest_active_assistant_usage();
        let checkpoint_usage = latest_turn
            .map(|record| record.usage)
            .filter(|usage| usage.total_tokens > 0);
        let checkpoint_cost = checkpoint.and_then(|checkpoint| checkpoint.run_cost_microdollars);
        let checkpoint_model = checkpoint
            .and_then(|checkpoint| session.entry(&checkpoint.prompt))
            .and_then(|entry| entry.metadata.as_ref())
            .and_then(|metadata| metadata.prompt_model.as_ref())
            .map(|model| model.0.clone());
        let session_cost = session.total_cost_microdollars();
        let mut state = self.state.borrow_mut();
        state.deferred_session_history = None;
        state.latest_compaction_summary =
            session
                .entries()
                .iter()
                .rev()
                .find_map(|entry| match &entry.value {
                    EntryValue::Compaction { summary, .. } => Some(summary.clone()),
                    _ => None,
                });
        state.transcript_epoch = state.transcript_epoch.wrapping_add(1);
        state.next_transcript_commit_id = NextTranscriptCommitId::default();
        state.transcript.clear();
        state.transcript_commit_ids.clear();
        state.block_revisions.clear();
        state.invalidate_transcript_layout();
        state.steering_queue.clear();
        state.tool_panels.clear();
        state.close_streaming_blocks();
        state.jump_to_tail();
        state.last_turn_usage = checkpoint_usage;
        state.last_turn_tokens_per_second = None;
        state.last_turn_generation_elapsed = None;
        state.last_turn_generated_tokens = None;
        state.turn_generation_started_at = None;
        state.turn_streamed_output_bytes = 0;
        state.turn_output_tokens_before_generation = 0;
        state.session_cost_microdollars = session
            .usage_records()
            .iter()
            .any(|record| record.cost_microdollars.is_some())
            .then_some(session_cost);
        state.telemetry_model = checkpoint_model;
        // `update_status` computes cache diagnostics once and installs the raw
        // latest-turn rate immediately after hydration.
        state.cache_hit_rate_basis_points = None;
        state.run_cost_microdollars = checkpoint_cost.unwrap_or_default();
        state.run_cost_available = checkpoint_cost.is_some();
        state.run.clear();
        state.session_work_elapsed = Duration::ZERO;
        state.run_model = None;
        state.run_model_lab = None;
        state.run_prompt_color = None;
        state.run_model_display = None;
        state.run_model_compact_names.clear();
        state.run_reasoning = None;
        state.run_price_display = None;
        state.run_context_estimate = None;
        state.run_label.clear();
        state.shimmer_started_at = None;
        state.overlay = None;
        state.error = None;
        append_hydrated_items(&mut state, items);
        state.deferred_session_history = deferred_snapshot.map(|mut deferred| {
            deferred.retained_id_end = state.next_transcript_commit_id.0;
            deferred
        });
        state.invalidate_transcript();
        Ok(())
    }

    /// Human-readable state used by headless unit tests and regression checks.
    #[cfg(test)]
    pub fn debug_snapshot(&self) -> String {
        let state = self.state.borrow();
        let mut result = String::new();
        for block in &state.transcript {
            match block {
                TranscriptBlock::User { text, .. } | TranscriptBlock::Notice(text) => {
                    result.push('\n');
                    result.push_str(text);
                }
                TranscriptBlock::Compaction(compaction) => {
                    result.push('\n');
                    result.push_str(&compaction.label);
                    result.push('\n');
                    result.push_str(&compaction.summary);
                }
                TranscriptBlock::Assistant(markdown) | TranscriptBlock::Reasoning(markdown) => {
                    result.push('\n');
                    result.push_str(&markdown.text);
                }
                TranscriptBlock::Tool(panel) => {
                    result.push('\n');
                    result.push_str(&panel.name);
                    result.push('\n');
                    result.push_str(&panel.output);
                }
                TranscriptBlock::Outcome(outcome) => {
                    result.push('\n');
                    result.push_str(&format!("{outcome:?}"));
                }
                TranscriptBlock::Shell(shell) => {
                    result.push('\n');
                    result.push_str(&format!("$ {}\n{}", shell.command, shell.output));
                }
            }
        }
        for message in &state.steering_queue {
            result.push('\n');
            result.push_str("Steering: ");
            result.push_str(&message.display);
        }
        result
    }

    #[cfg(test)]
    pub fn debug_error(&self) -> Option<String> {
        self.state.borrow().error.clone()
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
    fn start_events(
        &mut self,
        _on_input: Box<dyn FnMut(sexy_tui_rs::TerminalInput)>,
        _on_resize: Box<dyn FnMut()>,
    ) {
    }
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

mod transcript_history;

#[cfg(test)]
mod tests;
