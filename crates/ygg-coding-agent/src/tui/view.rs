#![allow(missing_docs)]

use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, HashSet};
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
    parse_markdown, strip_terminal_sequences, visible_width, wrap_text_with_ansi, Block, CodeBlock,
    Color, Component, DetailBlock, DiffRenderOptions, Document, FrameUpdate, RichRenderer,
    StreamingMarkdown, StreamingRenderCache, UnifiedDiff, CURSOR_MARKER, TUI,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;
use ygg_agent::{AgentEvent, EntryValue, OutputChannel, Session, ToolProgress};
use ygg_ai::{ModalitySet, Model, ModelId, ToolCallId, Usage};

use crate::commands;
use crate::config::Config;
use crate::hydrate::{hydrate_transcript, hydrate_transcript_tail, TranscriptItem};
use crate::presentation::{
    format_duration, is_hidden_tool_detail, summarize_tool, summarize_tool_with_workspace,
    tool_failure_reason, tool_result_is_failure, ModelDisplayMetadata, PriceDisplay, RunId,
    RunOutcome, RunPhase, RunTracker, ToolDisplay,
};
use crate::tui::composer::{self, ComposedInput};
use crate::tui::keymap::{EditAction, SlashMenuAction};
use crate::tui::terminal::{force_restore, TerminalSize, YggTerminal};
use crate::tui::theme::{
    ModelLab, ThemeDensity, ThemeSurfaceAlign, ThemeSurfaceChrome, ThemeSurfaceHeading,
    ThemeSurfaceWidth, YggTheme,
};

const MAX_PANEL_BYTES: usize = 64 * 1024;
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
/// Resize events are normally delivered by crossterm, but polling while idle
/// also catches terminal-manager resizes that do not emit an event.
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ELISION_MARKER: &str = "\n… older tool output elided …\n";
/// A compact exec row keeps enough terminal context to recognize a result
/// while preventing a noisy command from swallowing the transcript.
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

fn valid_extension_tool_render_role(role: &str) -> bool {
    !role.is_empty()
        && role.len() <= 96
        && !role.starts_with('.')
        && !role.ends_with('.')
        && role
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
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
    /// First streamed reasoning delta, used only for the settled compact label.
    reasoning_started_at: Option<Instant>,
    /// Frozen reasoning duration after the block closes.
    reasoning_elapsed: Option<Duration>,
    /// Decorative live-label animation frame; never persisted as content.
    reasoning_animation_frame: u64,
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
            reasoning_animation_frame: 0,
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
        block.reasoning_animation_frame = 2;
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
    }

    fn finish_reasoning(&mut self) {
        // A four-character emphasis boundary can straddle provider deltas. Fix
        // that rare boundary once at completion rather than reparsing the full
        // trace after every delta.
        let projection = reasoning_markdown_projection(&self.text);
        if self.markdown.raw_text() != projection {
            self.markdown = StreamingMarkdown::from_text(&projection);
            self.invalidate_layout();
        }
        if self.reasoning_elapsed.is_none() {
            self.reasoning_elapsed = self.reasoning_started_at.map(|started| started.elapsed());
        }
        self.finish();
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
    /// Lazily cached diff scan.  `None` = not yet computed.
    cached_diff: RefCell<Option<Option<String>>>,
    /// Lazily cached metadata string (test results / duration for exec).
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
    /// Creator family for the active model. The dedicated model accent is
    /// reapplied whenever a named theme is loaded.
    pub(crate) model_lab: Option<crate::tui::theme::ModelLab>,
    /// Exact deterministic row colour assigned to the next submitted prompt.
    pub(crate) prompt_color: Option<String>,
    transcript: Vec<TranscriptBlock>,
    /// Session backing an intentionally tail-only first paint. The complete
    /// branch is materialized once, on the first attempt to scroll beyond the
    /// retained tail, so resume readiness does not scale with old history.
    deferred_session_path: Option<PathBuf>,
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
    /// Per-call disclosure state. `/tool` toggles one panel without exposing
    /// protocol details for every tool in the transcript.
    expanded_tools: HashSet<ToolCallId>,
    /// Per-block expansion for `!` shell commands.
    expanded_shells: HashSet<String>,
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
    pub(crate) verbose_tools: bool,
    pub(crate) size: (u16, u16),
    /// Cached editor layout so the composer shimmer animation doesn't
    /// re-wrap the prompt on every frame.
    cached_layout: RefCell<Option<EditorLayoutCache>>,
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
        self.transcript.push(block);
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

    fn show_tool_details(&self, block: &TranscriptBlock) -> bool {
        self.verbose_tools
            || matches!(
                block,
                TranscriptBlock::Tool(panel) if self.expanded_tools.contains(&panel.id)
            )
            || matches!(
                block,
                TranscriptBlock::Shell(shell) if self.expanded_shells.contains(&shell.id)
            )
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

        let index = self.transcript.len();
        let model_lab = self.executing_model_lab();
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

    fn advance_reasoning_animation(&mut self) {
        let Some(index) = self.active_reasoning else {
            return;
        };
        let advanced = match self.transcript.get_mut(index) {
            Some(TranscriptBlock::Reasoning(reasoning))
                if !reasoning.finished && !reasoning.reasoning_expanded =>
            {
                reasoning.reasoning_animation_frame =
                    reasoning.reasoning_animation_frame.wrapping_add(1);
                true
            }
            _ => false,
        };
        if advanced {
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

/// Reconcile the renderer's shared dimensions with the terminal itself. This
/// is a fallback for environments where the resize signal is delayed or
/// swallowed; the normal input path still updates the same cells immediately.
fn synchronize_terminal_size(state: &SharedState, size: &TerminalSize) -> bool {
    let Ok(dimensions) = crossterm::terminal::size() else {
        return false;
    };
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

    let mut shell = state.borrow_mut();
    shell.size = dimensions;
    let maximum = max_scroll_from_bottom(&shell, dimensions.0);
    shell
        .scroll_from_bottom
        .set(shell.scroll_from_bottom.get().min(maximum));
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
    loop {
        // Choose the poll timeout based on whether the shimmer animation
        // would be rendered this frame.  When it is, use a short timeout so
        // the wave stays fluid on high-refresh terminals. Otherwise use a
        // 100 ms status/resize poll; idle timeouts do not render unless the
        // terminal dimensions actually changed.
        let (animating, is_active) = {
            let s = state.borrow();
            let active = s.run.is_active();
            let compacting = s.run_label == "compacting";
            (
                (active || compacting) && shimmer_animating(&s),
                active || compacting,
            )
        };
        let command = if animating {
            match rx.recv_timeout(ANIMATION_POLL_TIMEOUT) {
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
        let cap = if animating {
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

        if animating {
            state.borrow_mut().advance_reasoning_animation();
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
    theme_epoch: u64,
    transcript_generation: u64,
    transcript_len: usize,
    /// Height of the mutable header/popup/composer tail in the last frame.
    /// Unlike transcript growth, a height change here inserts or removes rows
    /// around already-painted chrome and must reanchor the native viewport.
    chrome_rows: usize,
    overlay_active: bool,
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
            frame.theme_epoch = state.theme_epoch;
            lines
        } else {
            let lines = render_shell(&state, width);
            synchronize_shell_frame(&state, width, &mut self.frame.borrow_mut());
            lines
        }
    }

    fn render_update(&self, width: u16) -> Option<FrameUpdate> {
        let state = self.state.borrow();
        Some(if self.application_viewport {
            render_shell_viewport_update(
                &state,
                width,
                Instant::now(),
                &mut self.frame.borrow_mut(),
            )
        } else {
            render_shell_update(&state, width, Instant::now(), &mut self.frame.borrow_mut())
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

fn compact_thought_duration(duration: Duration) -> String {
    let rounded = duration.as_secs_f64().round().max(1.0) as u64;
    if rounded < 60 {
        format!("{rounded}s")
    } else {
        format!("{}m{:02}s", rounded / 60, rounded % 60)
    }
}

fn mix_rgb(base: (u8, u8, u8), accent: (u8, u8, u8), amount: f64) -> (u8, u8, u8) {
    let channel = |base: u8, accent: u8| {
        (f64::from(base) + (f64::from(accent) - f64::from(base)) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (
        channel(base.0, accent.0),
        channel(base.1, accent.1),
        channel(base.2, accent.2),
    )
}

fn live_reasoning_label(theme: &YggTheme, reasoning: &AssistantBlock) -> String {
    const UNICODE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    const ASCII_FRAMES: [&str; 4] = ["-", "\\", "|", "/"];
    let capabilities = theme.capabilities();
    let frame = reasoning.reasoning_animation_frame as usize;
    let spinner = if capabilities.unicode {
        UNICODE_FRAMES[frame % UNICODE_FRAMES.len()]
    } else {
        ASCII_FRAMES[frame % ASCII_FRAMES.len()]
    };
    let label = format!("{spinner} thinking");
    let Some(accent) = theme.model_rgb(reasoning.model_lab) else {
        return label;
    };
    if !capabilities.animation
        || capabilities.color == crate::tui::terminal::ColorDepth::None
        || !capabilities.interactive
    {
        return theme.model_fg(reasoning.model_lab, &label);
    }
    let base = theme.composer_idle_rgb(accent);
    let cells = label.chars().count().max(1);
    label
        .chars()
        .enumerate()
        .map(|(index, character)| {
            let distance = (index + cells - (frame % cells)) % cells;
            let wave = match distance {
                0 => 1.0,
                1 | 2 => 0.78,
                3 | 4 => 0.52,
                _ => 0.30,
            };
            theme.rgb_fg(mix_rgb(base, accent, wave), &character.to_string())
        })
        .collect()
}

fn collapsed_reasoning_lines(theme: &YggTheme, reasoning: &AssistantBlock) -> [String; 2] {
    let label = if reasoning.finished {
        let text = reasoning.reasoning_elapsed.map_or_else(
            || "thought".to_owned(),
            |elapsed| format!("thought for {}", compact_thought_duration(elapsed)),
        );
        theme.model_fg(reasoning.model_lab, &theme.italic(&text))
    } else {
        theme.bold(&live_reasoning_label(theme, reasoning))
    };
    let elbow = if theme.capabilities().unicode {
        "└"
    } else {
        "`"
    };
    [
        label,
        subdued_text(theme, &format!("{elbow} ctrl+o to expand")),
    ]
}

/// A low-contrast annotation that remains readable without relying on a
/// painted background. This is used for viewport chrome and secondary tool
/// metadata, never for the answer itself.
fn subdued_text(theme: &YggTheme, text: &str) -> String {
    let italic = theme.italic(text);
    theme.fg("muted", &italic)
}

/// Render low-contrast command output without inheriting a theme role's bold
/// flag. Commands are deliberately prominent; their output must remain the
/// quieter second level even when a user theme makes other muted chrome bold.
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
    render_reasoning_on_surface(reasoning, renderer, theme, width, show_reasoning, None)
}

fn render_reasoning_on_surface(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    _show_reasoning: bool,
    background: Option<Color>,
) -> Vec<String> {
    let marker = theme.glyph("reasoning");
    let prefix_width = visible_width(marker).saturating_add(1);
    if !reasoning.reasoning_expanded {
        return collapsed_reasoning_lines(theme, reasoning)
            .into_iter()
            .map(|line| fit_line(&line, width))
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

fn transcript_transition_rows(
    previous: Option<&TranscriptBlock>,
    density: ThemeDensity,
    show_reasoning: bool,
) -> usize {
    // Density changes only the boundary between semantic blocks. A tool's
    // compact header, result, and diff still live inside one block and remain
    // adjacent, so compact themes never destroy meaningful grouping.
    if previous.is_none()
        || (!show_reasoning && matches!(previous, Some(TranscriptBlock::Reasoning(_))))
    {
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
    // trailing cells. This is intentionally a full-width rectangle rather
    // than a narrow gutter: the durable RGB is the prompt's identity and must
    // remain visible after reflow. Plain text is used inside coloured prompts
    // so nested Markdown resets cannot punch holes through the background.
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
            theme.fg("warning", theme.glyph("note")),
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

fn render_outcome(outcome: &RunOutcome, theme: &YggTheme, width: u16) -> Vec<String> {
    let mut lines = vec![fit_line(&outcome_line(outcome, theme), width)];
    let detail = match outcome {
        RunOutcome::Failed { reason, .. } => Some(("error", reason.as_str())),
        RunOutcome::NeedsInput { prompt } => Some(("warning", prompt.as_str())),
        _ => None,
    };
    if let Some((role, detail)) = detail {
        let safe = sanitize_for_terminal(detail);
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

fn render_failure_details(panel: &ToolPanel, theme: &YggTheme, width: u16) -> Vec<String> {
    let branch = theme.fg("error", "!");
    // Failure details are still part of the tool block, so they begin on the
    // same outer baseline as the action row rather than acquiring a second,
    // unrelated transcript inset.
    let prefix = format!("{branch} ");
    let continuation = " ".repeat(visible_width(&prefix));
    let mut lines = Vec::new();
    for raw in panel
        .output
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty() && !is_hidden_tool_detail(line))
        .take(8)
    {
        lines.extend(wrap_hanging(
            &sanitize_for_terminal(raw),
            &prefix,
            &continuation,
            width,
        ));
    }
    lines
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

/// Render `read` tool output with colored line numbers for readability.
/// The output format is:
///   path:offset-end/total hash=...
///   NNN: code line
///   ...
///   next_offset=N truncated=bool
fn render_read_output(output: &str, theme: &YggTheme, width: u16) -> Vec<String> {
    let indent = "  ";
    let content_width = usize::from(width).saturating_sub(visible_width(indent));
    let muted = |text: &str| theme.fg("muted", text);
    let mut lines: Vec<String> = Vec::new();

    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        // Header line: "path:1-20/100 hash=sha256:..."
        if line.contains("hash=") && line.contains('/') {
            lines.push(format!("{indent}{}", muted(line)));
            continue;
        }
        // Footer: "next_offset=N" or "truncated=bool"
        if line.starts_with("next_offset=") || line.starts_with("truncated=") {
            lines.push(format!("{indent}{}", muted(line)));
            continue;
        }
        // "(empty file)" marker
        if line == "(empty file)" {
            lines.push(format!("{indent}{}", muted(line)));
            continue;
        }
        // Content line: "NNN: rest of line"
        if let Some((num, code)) = line.split_once(':') {
            if num.chars().all(|c| c.is_ascii_digit()) {
                let number_gutter = theme.fg("syntax_number", &format!("{num}:"));
                let code_str = sanitize_for_terminal(code);
                let combined = format!("{number_gutter}{code_str}");
                lines.push(format!(
                    "{indent}{}",
                    fit_line(&combined, content_width as u16)
                ));
                continue;
            }
        }
        // Fallback: render as plain text
        lines.push(format!("{indent}{}", sanitize_for_terminal(line)));
    }
    lines
}

fn render_extension_tool_segments(panel: &ToolPanel, theme: &YggTheme, width: u16) -> Vec<String> {
    if panel.extension_render_segments.is_empty() {
        return Vec::new();
    }
    let mut semantic = String::new();
    for segment in &panel.extension_render_segments {
        if !panel.finished {
            // Live extension output follows the same lifecycle hierarchy as
            // built-in tools. Extension roles become active again after the
            // call completes, but cannot turn an in-flight feed into a bright
            // competing surface.
            semantic.push_str(&segment.text);
        } else if let Some(role) = segment
            .style_role
            .as_deref()
            .filter(|role| valid_extension_tool_render_role(role))
        {
            semantic.push_str(&theme.apply_semantic_role(role, &segment.text));
        } else {
            semantic.push_str(&segment.text);
        }
    }
    if semantic.is_empty() {
        Vec::new()
    } else {
        let semantic = if panel.finished {
            semantic
        } else {
            understated_tool_output(theme, &semantic)
        };
        wrap_hanging(&semantic, "  ", "  ", width)
    }
}

/// Expanded proof stays in sexy-tui's semantic technical renderers. The
/// compact action row remains a concise observation; code, diffs, arguments,
/// and raw logs are bounded local objects rather than unstyled transcript
/// lines.
fn render_tool_details(
    panel: &ToolPanel,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
) -> Vec<String> {
    let display_line = |line: sexy_tui_rs::RenderedLine| {
        let content = if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
            line.plain
        } else {
            line.styled
        };
        format!("  {content}")
    };

    let mut lines = Vec::new();
    let mut evidence = Vec::new();
    let diff = tool_diff(panel);
    if !panel.args.is_empty() {
        evidence.push(Block::CodeBlock(CodeBlock::with_language(
            "json",
            format!(
                "{{\n  \"tool_call_id\": \"{}\",\n  \"arguments\": {}\n}}",
                panel.id.0, panel.args
            ),
        )));
    }
    let non_diff_output = diff
        .as_ref()
        .map(|d| panel.output[..panel.output.len().saturating_sub(d.len())].trim_end())
        .unwrap_or(&panel.output);
    if !non_diff_output.is_empty() {
        if panel.name == "read" {
            lines.extend(render_read_output(non_diff_output, theme, width));
        } else {
            evidence.push(Block::CodeBlock(CodeBlock::with_language(
                "text",
                non_diff_output.to_owned(),
            )));
        }
    }
    if !evidence.is_empty() {
        let document = Document::new(vec![Block::Detail(DetailBlock::new(
            "evidence", evidence, true,
        ))]);
        lines.extend(
            renderer
                .render(&document, width.saturating_sub(2))
                .lines
                .into_iter()
                .map(&display_line),
        );
    }
    if let Some(ref diff) = diff {
        let rendered = renderer.render_diff(
            &UnifiedDiff::parse(diff),
            width.saturating_sub(2),
            DiffRenderOptions {
                line_numbers: width >= 70,
                wrap: false,
            },
        );
        lines.extend(rendered.lines.into_iter().map(display_line));
    }
    if panel.finished {
        lines
    } else {
        // Expanded live evidence is still active tool output. Strip the rich
        // renderer's nested colours until completion so every visible cell is
        // predictably muted; the durable evidence is untouched and is
        // re-rendered richly as soon as the tool settles.
        lines
            .into_iter()
            .map(|line| understated_tool_output(theme, &strip_terminal_sequences(&line)))
            .collect()
    }
}

/// Max diff lines to show in compact (non-expanded) mode before truncating.
const COMPACT_DIFF_LINES: usize = 10;

/// Render only the diff portion of tool output, without the full evidence
/// panel.  Long diffs are truncated with an indicator; ctrl+o expands them.
fn render_diff_only(
    panel: &ToolPanel,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
) -> Vec<String> {
    let display_line = |line: sexy_tui_rs::RenderedLine| {
        let content = if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
            line.plain
        } else {
            line.styled
        };
        format!("  {content}")
    };
    let Some(ref diff) = tool_diff(panel) else {
        return Vec::new();
    };
    let rendered = renderer.render_diff(
        &UnifiedDiff::parse(diff),
        width.saturating_sub(2),
        DiffRenderOptions {
            line_numbers: width >= 70,
            wrap: true,
        },
    );
    let mut lines: Vec<String> = rendered.lines.into_iter().map(display_line).collect();
    if lines.len() > COMPACT_DIFF_LINES + 1 {
        let remaining = lines.len() - COMPACT_DIFF_LINES;
        lines.truncate(COMPACT_DIFF_LINES);
        let hint = if theme.unicode() {
            format!("  … {remaining} more lines · ctrl+o to expand")
        } else {
            format!("  ... {remaining} more lines - ctrl+o to expand")
        };
        lines.push(subdued_text(theme, &hint));
    }
    lines
}

fn without_redundant_tool_lead(tool: &str, text: &str) -> String {
    let mut words = text.splitn(2, char::is_whitespace);
    let Some(first) = words.next() else {
        return String::new();
    };
    let redundant = match tool {
        "read" => matches!(first, "read" | "reading"),
        "search" => matches!(first, "search" | "searched" | "searching"),
        "exec" => matches!(first, "exec" | "run" | "ran" | "running"),
        "write" => matches!(first, "write" | "wrote" | "writing"),
        _ => first == tool,
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

/// Locate the final canonical `exec` result after any live progress bytes.
/// The exec tool streams output while it runs, then emits a durable envelope
/// containing the exit status and bounded stdout/stderr capture. The panel
/// retains both, so presentation should prefer the last envelope without
/// mutating the evidence stored for expansion and copy.
fn final_exec_result(output: &str) -> &str {
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
        if index == 0 || next == "(no output)" || is_exec_stream_header(next) {
            return candidate;
        }
    }
    output
}

fn is_exec_stream_header(line: &str) -> bool {
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
struct ExecCaptureTruncation {
    stream: &'static str,
    omitted_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CompactExecOutput {
    lines: Vec<String>,
    omitted_lines: usize,
    capture_truncations: Vec<ExecCaptureTruncation>,
    panel_elided: bool,
}

fn exec_capture_footer(line: &str) -> Option<(&'static str, &str)> {
    ["stdout", "stderr"].into_iter().find_map(|stream| {
        line.strip_prefix("truncated_")
            .and_then(|line| line.strip_prefix(stream))
            .and_then(|line| line.strip_prefix('='))
            .map(|detail| (stream, detail))
    })
}

fn is_exec_complete_footer(line: &str) -> bool {
    ["stdout", "stderr"].into_iter().any(|stream| {
        line.strip_prefix("complete_")
            .and_then(|line| line.strip_prefix(stream))
            .is_some_and(|detail| detail == "=true")
    })
}

/// Project a bounded result into Pi-style tail output. Protocol envelope lines
/// are excluded; capture loss is retained separately because Ctrl+O can reveal
/// UI-tail omissions but cannot recover bytes discarded by the exec tool.
fn compact_exec_output(panel: &ToolPanel) -> CompactExecOutput {
    let result = sanitize_for_terminal(final_exec_result(&panel.output));
    let mut capture_truncations = Vec::new();
    for line in result.lines().map(str::trim) {
        let Some((stream, detail)) = exec_capture_footer(line) else {
            continue;
        };
        if detail == "false" {
            continue;
        }
        let omitted_bytes = detail
            .split_whitespace()
            .find_map(|part| part.strip_prefix("omitted_bytes:"))
            .and_then(|count| count.parse::<usize>().ok());
        capture_truncations.push(ExecCaptureTruncation {
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
        if expect_stream_header && is_exec_stream_header(trimmed) {
            protocol_error = false;
            expect_stream_header = false;
            continue;
        }
        if exec_capture_footer(trimmed).is_some() || is_exec_complete_footer(trimmed) {
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

    let omitted_lines = content.len().saturating_sub(COMPACT_EXEC_OUTPUT_LINES);
    if omitted_lines > 0 {
        content.drain(..omitted_lines);
    }
    CompactExecOutput {
        lines: content,
        omitted_lines,
        capture_truncations,
        panel_elided,
    }
}

fn compute_tool_metadata(panel: &ToolPanel) -> Option<String> {
    if panel.name != "exec" {
        return None;
    }
    let output = final_exec_result(&panel.output);
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

fn render_compact_exec_output(
    panel: &ToolPanel,
    theme: &YggTheme,
    width: u16,
    show_tool_duration: bool,
) -> Vec<String> {
    let compact = compact_exec_output(panel);
    let ellipsis = if theme.unicode() { "…" } else { "..." };
    let mut lines = Vec::new();
    if compact.panel_elided {
        let hint = format!(
            "  {ellipsis} (older live output was elided before display; unavailable to expand)"
        );
        lines.extend(wrap_hanging(
            &understated_tool_output(theme, &hint),
            "",
            "  ",
            width,
        ));
    }
    for truncation in compact.capture_truncations {
        let detail = truncation
            .omitted_bytes
            .map_or_else(|| "some bytes".to_owned(), |bytes| format!("{bytes} bytes"));
        let hint = format!(
            "  {ellipsis} ({} capture omitted {detail}; unavailable to expand)",
            truncation.stream
        );
        lines.extend(wrap_hanging(
            &understated_tool_output(theme, &hint),
            "",
            "  ",
            width,
        ));
    }
    if compact.omitted_lines > 0 {
        let unit = if compact.omitted_lines == 1 {
            "line"
        } else {
            "lines"
        };
        let hint = format!(
            "  {ellipsis} ({} earlier {unit}, ctrl+o to expand)",
            compact.omitted_lines,
        );
        lines.extend(wrap_hanging(
            &understated_tool_output(theme, &hint),
            "",
            "  ",
            width,
        ));
    }
    for output_line in compact.lines {
        let output_line = understated_tool_output(theme, &output_line);
        lines.extend(wrap_hanging(&output_line, "  ", "  ", width));
    }
    if show_tool_duration {
        if let Some(duration) = tool_metadata(panel) {
            lines.push(fit_line(
                &understated_tool_output(theme, &format!("  Took {duration}")),
                width,
            ));
        }
    }
    lines
}

fn exec_timeout_annotation(panel: &ToolPanel) -> Option<String> {
    let timeout_ms = serde_json::from_str::<serde_json::Value>(&panel.args)
        .ok()?
        .get("timeout_ms")?
        .as_u64()?;
    Some(if timeout_ms % 1_000 == 0 {
        format!("timeout {}s", timeout_ms / 1_000)
    } else {
        format!("timeout {timeout_ms}ms")
    })
}

fn render_exec_row(panel: &ToolPanel, command: &str, theme: &YggTheme, width: u16) -> Vec<String> {
    let lifecycle_role = if !panel.finished {
        "muted"
    } else if panel.is_error {
        "error"
    } else {
        "foreground"
    };
    let marker = theme.bold(&theme.fg(lifecycle_role, "$"));
    let prefix = format!("{marker} ");
    let continuation = " ".repeat(visible_width(&prefix));
    let safe_command = sanitize_for_terminal(command);
    let mut command = theme.bold(&theme.fg(lifecycle_role, &safe_command));
    if let Some(timeout) = exec_timeout_annotation(panel) {
        command.push(' ');
        command.push_str(&understated_tool_output(theme, &format!("({timeout})")));
    }
    let mut lines = wrap_hanging(&command, &prefix, &continuation, width);

    if panel.is_error {
        if let Some(reason) = &panel.failure_reason {
            let error_marker = theme.fg("error", "!");
            let error_prefix = format!("{error_marker} ");
            let error_continuation = " ".repeat(visible_width(&error_prefix));
            lines.extend(wrap_hanging(
                &theme.fg("error", &sanitize_for_terminal(reason)),
                &error_prefix,
                &error_continuation,
                width,
            ));
        }
    }
    lines
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
            collapsed_reasoning_lines(theme, reasoning).join("\n")
        }
        TranscriptBlock::Compaction(compaction) if !compaction.expanded => {
            format!("{} · ctrl+o to view", compaction.label)
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
    let inset = layout.transcript_inset.min(outer_width.saturating_sub(1));
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
    let leading_rows = if chrome == ThemeSurfaceChrome::Card
        || chrome == ThemeSurfaceChrome::Rule
        || heading != ThemeSurfaceHeading::None
    {
        1
    } else {
        0
    };
    let trailing_rows = usize::from(chrome == ThemeSurfaceChrome::Card);
    SurfacePlan {
        kind,
        chrome,
        heading,
        label: resolved.label,
        padding,
        frame_left,
        frame_width,
        geometry: SurfaceGeometry {
            transition_rows: transcript_transition_rows(previous, layout.density, true),
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

fn decorate_surface(
    content: Vec<String>,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    outer_width: u16,
    prompt_color: Option<&str>,
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
    rows.extend(
        content
            .iter()
            .map(|line| render_surface_content_line(line, plan, theme, prompt_color)),
    );
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

    rows.into_iter()
        .enumerate()
        .map(|(row, line)| {
            if row < plan.geometry.transition_rows || line.is_empty() {
                String::new()
            } else {
                fit_line(
                    &format!("{}{line}", " ".repeat(usize::from(plan.frame_left))),
                    outer_width,
                )
            }
        })
        .collect()
}

fn render_block_planned(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    reasoning_renderer: &RichRenderer,
    outer_width: u16,
    verbose_tools: bool,
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
            layout.show_reasoning,
            content_background,
        ),
        TranscriptBlock::Tool(panel) => {
            let compact_exec = panel.name == "exec" && panel.display.shell_command.is_some();
            let mut lines = if let Some(command) = panel.display.shell_command.as_deref() {
                render_exec_row(panel, command, theme, width)
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
                let tool = panel.display.label.as_str();
                // Tool chrome describes lifecycle, not model identity: muted
                // while executing, normal foreground after success, and an
                // explicit error role on failure.
                let lifecycle_role = if !panel.finished {
                    "muted"
                } else if panel.is_error {
                    "error"
                } else {
                    "foreground"
                };
                let label = theme.bold(&theme.fg(lifecycle_role, tool));
                let mut text =
                    without_redundant_tool_lead(&panel.name, &sanitize_for_terminal(summary));
                if panel.is_error {
                    if let Some(reason) = &panel.failure_reason {
                        text.push_str(semantic_separator(theme));
                        text.push_str(&sanitize_for_terminal(reason));
                    }
                }
                let text = theme.fg(lifecycle_role, &text);
                let label_width: usize = if compact { 7 } else { 8 };
                let label_prefix = format!(
                    "{label}{}",
                    " ".repeat(label_width.saturating_sub(visible_width(tool)))
                );
                let continuation = " ".repeat(label_width);
                wrap_hanging(&text, &label_prefix, &continuation, width)
            };

            if panel.is_error && !verbose_tools && !compact_exec {
                lines.extend(render_failure_details(panel, theme, width));
            }
            lines.extend(render_extension_tool_segments(panel, theme, width));
            if verbose_tools {
                lines.extend(render_tool_details(panel, rich_renderer, theme, width));
            } else if compact_exec {
                lines.extend(render_compact_exec_output(
                    panel,
                    theme,
                    width,
                    layout.show_tool_duration,
                ));
            } else if tool_diff(panel).is_some() {
                // Diffs remain visible at a glance without expanding protocol
                // arguments and raw evidence.
                lines.extend(render_diff_only(panel, rich_renderer, theme, width));
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
            let action = if compaction.expanded {
                "ctrl+o to collapse"
            } else {
                "ctrl+o to view"
            };
            let label = format!("{} · {action}", sanitize_for_terminal(&compaction.label));
            let mut lines = wrap_hanging(&label, &prefix, &continuation, width);
            if compaction.expanded {
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
            if shell.running {
                // In-progress: command + "…" + spinner (icon on the right)
                let line = format!(
                    "{} {} {} {}",
                    prefix,
                    theme.dim(&sanitize_for_terminal(&shell.command)),
                    theme.dim("…"),
                    shell.spinner,
                );
                finish_transcript_block(vec![fit_line(&line, width)])
            } else if verbose_tools {
                // Expanded: command header (icon on right) + full output + exit code
                let mut lines: Vec<String> = Vec::new();
                let header = format!(
                    "{} {} {}",
                    prefix,
                    theme.fg("model_accent", &sanitize_for_terminal(&shell.command)),
                    shell.spinner,
                );
                lines.push(fit_line(&header, width));
                // Output
                let output = shell.output.trim();
                if !output.is_empty() {
                    for line in output.lines() {
                        lines.push(format!("  {}", theme.dim(&sanitize_for_terminal(line))));
                    }
                }
                // Exit code
                let exit = if shell.exit_code == 0 {
                    format!("[exit: {}]", shell.exit_code)
                } else {
                    theme.fg("error", &format!("[exit: {}]", shell.exit_code))
                };
                lines.push(format!("  {exit}"));
                finish_transcript_block(lines)
            } else {
                // Collapsed: command + summary + exit + icon (on the right)
                let summary = if shell.output.is_empty() {
                    "(no output)".to_string()
                } else {
                    let trimmed = shell.output.trim();
                    let first_line = trimmed.lines().next().unwrap_or("");
                    let summary = sanitize_for_terminal(first_line);
                    if trimmed.lines().count() > 1 {
                        format!("{summary} …")
                    } else {
                        summary
                    }
                };
                let exit = if shell.exit_code == 0 {
                    " [ok]".to_string()
                } else {
                    format!(" [exit: {}]", shell.exit_code)
                };
                let line = format!(
                    "{} {}  {}",
                    prefix,
                    theme.dim(&format!(
                        "{}  {}{}",
                        sanitize_for_terminal(&shell.command),
                        summary,
                        exit
                    )),
                    shell.spinner,
                );
                finish_transcript_block(vec![fit_line(&line, width)])
            }
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
    let lines = decorate_surface(lines, &plan, theme, outer_width, prompt_color);
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
            let mut text = if let Some(command) = &panel.display.shell_command {
                format!("$ {command}")
            } else {
                format!("{}  {summary}", panel.display.label)
            };
            if !panel.output.is_empty() {
                text.push('\n');
                text.push_str(&panel.output);
            }
            sanitize_for_terminal(&text)
        }
        TranscriptBlock::Shell(shell) => {
            let mut text = format!("$ {}", shell.command);
            if !shell.output.is_empty() {
                text.push('\n');
                text.push_str(&shell.output);
            }
            if shell.exit_code != 0 {
                text.push_str(&format!("\n[exit: {}]", shell.exit_code));
            }
            sanitize_for_terminal(&text)
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
            RunOutcome::Failed { elapsed, reason } => {
                format!("failed · {reason} · {}", format_duration(*elapsed))
            }
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

fn transcript_lines(state: &ShellState, width: u16) -> Ref<'_, Vec<String>> {
    state.rendered_transcript(width)
}

fn transcript_commit_boundary(state: &ShellState, width: u16) -> usize {
    let transcript_len = transcript_lines(state, width).len();
    let first_live = state.transcript.iter().position(|block| match block {
        TranscriptBlock::Assistant(block) | TranscriptBlock::Reasoning(block) => !block.finished,
        TranscriptBlock::Tool(panel) => !panel.finished,
        TranscriptBlock::Shell(shell) => shell.running,
        TranscriptBlock::User { .. }
        | TranscriptBlock::Outcome(_)
        | TranscriptBlock::Notice(_)
        | TranscriptBlock::Compaction(_) => false,
    });
    let Some(first_live) = first_live else {
        return transcript_len;
    };
    let cache = state.transcript_cache.borrow();
    let block_start = cache.block_starts.get(first_live).copied().unwrap_or(0);
    let settled_rows = match &state.transcript[first_live] {
        TranscriptBlock::Assistant(block) => {
            let geometry = cache.block_geometries.get(first_live).copied();
            geometry.map_or(0, |geometry| {
                geometry
                    .transition_rows
                    .saturating_add(geometry.leading_rows)
                    .saturating_add(block.layout.borrow().committed_rows())
            })
        }
        _ => 0,
    };
    block_start.saturating_add(settled_rows).min(transcript_len)
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

struct ShellChrome {
    header: Vec<String>,
    composer: Vec<String>,
    panel: Vec<String>,
    pending: Vec<String>,
    suggestions: Vec<String>,
    error: Vec<String>,
    transcript_rows: usize,
}

impl ShellChrome {
    fn rows(&self) -> usize {
        self.header.len()
            + self.error.len()
            + self.pending.len()
            + self.suggestions.len()
            + self.panel.len()
            + self.composer.len()
    }
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
                    &state.theme.fg("error", source),
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
    frame.initialized = true;
    frame.width = width;
    frame.theme_epoch = state.theme_epoch;
    frame.chrome_rows = shell_chrome(state, width, now).rows();
    FrameUpdate {
        stable_prefix: 0,
        replacement: render_shell_viewport_at(state, width, now),
        commit_boundary: None,
        reanchor_viewport: repaint_theme,
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

/// Full logical primary-screen frame. The terminal backend paints only its
/// visible tail; committed rows naturally move into native scrollback and are
/// never sliced into an application-owned viewport on the default path.
fn render_shell_at(state: &ShellState, width: u16, now: Instant) -> Vec<String> {
    let chrome = shell_chrome(state, width, now);
    let mut lines = if state.overlay.is_some() {
        overlay_lines(state, width)
    } else {
        transcript_lines(state, width).clone()
    };
    append_chrome(&mut lines, chrome, 0);
    lines
}

fn synchronize_shell_frame(state: &ShellState, width: u16, frame: &mut ShellFrameState) {
    frame.chrome_rows = shell_chrome(state, width, Instant::now()).rows();
    if state.overlay.is_some() {
        frame.initialized = true;
        frame.width = width;
        frame.theme_epoch = state.theme_epoch;
        frame.transcript_generation = 0;
        frame.transcript_len = 0;
        frame.overlay_active = true;
        return;
    }
    let _ = transcript_lines(state, width);
    let cache = state.transcript_cache.borrow();
    frame.initialized = true;
    frame.width = width;
    frame.theme_epoch = state.theme_epoch;
    frame.transcript_generation = cache.generation;
    frame.transcript_len = cache.lines.len();
    frame.overlay_active = false;
}

/// Build only the mutable suffix of the native-scrollback frame. Historic
/// transcript strings are neither cloned nor compared on streaming/status
/// ticks; sexy-tui reuses the committed prefix already retained in its frame.
fn render_shell_update(
    state: &ShellState,
    width: u16,
    now: Instant,
    frame: &mut ShellFrameState,
) -> FrameUpdate {
    let repaint_theme = frame.initialized && frame.theme_epoch != state.theme_epoch;
    let chrome = shell_chrome(state, width, now);
    let chrome_rows = chrome.rows();
    let reanchor_chrome =
        frame.initialized && frame.width == width && frame.chrome_rows != chrome_rows;
    if state.overlay.is_some() {
        let mut replacement = overlay_lines(state, width);
        append_chrome(&mut replacement, chrome, 0);
        frame.initialized = true;
        frame.width = width;
        frame.theme_epoch = state.theme_epoch;
        frame.transcript_generation = 0;
        frame.transcript_len = 0;
        frame.chrome_rows = chrome_rows;
        frame.overlay_active = true;
        return FrameUpdate {
            stable_prefix: 0,
            replacement,
            commit_boundary: Some(0),
            reanchor_viewport: repaint_theme || reanchor_chrome,
            // Portable terminals cannot restyle rows already owned by native
            // scrollback. Repaint the visible viewport and preserve history.
            rebuild_scrollback: false,
        };
    }

    let transcript_len = {
        let transcript = transcript_lines(state, width);
        transcript.len()
    };
    let commit_boundary = transcript_commit_boundary(state, width);
    // Hydrating `/new` (or a shorter resumed session) replaces the logical
    // transcript rather than merely editing the visible tail. The terminal's
    // native-scrollback renderer must repaint that new viewport from home;
    // otherwise its physical bottom remains anchored inside the old long
    // frame and later pickers expand above a composer stranded mid-screen.
    let leaving_overlay = frame.initialized && frame.overlay_active;
    let reanchor_viewport = repaint_theme
        || reanchor_chrome
        || leaving_overlay
        || (frame.initialized
            && frame.width == width
            && !frame.overlay_active
            && transcript_len < frame.transcript_len);
    let cache = state.transcript_cache.borrow();
    let stable_prefix = if frame.initialized && frame.width == width && !frame.overlay_active {
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
    let generation = cache.generation;
    drop(cache);

    let transcript = transcript_lines(state, width);
    let mut replacement = transcript[stable_prefix..].to_vec();
    drop(transcript);
    append_chrome(&mut replacement, chrome, stable_prefix);

    frame.initialized = true;
    frame.width = width;
    frame.theme_epoch = state.theme_epoch;
    frame.transcript_generation = generation;
    frame.transcript_len = transcript_len;
    frame.chrome_rows = chrome_rows;
    frame.overlay_active = false;
    FrameUpdate {
        stable_prefix,
        replacement,
        commit_boundary: Some(commit_boundary),
        reanchor_viewport,
        rebuild_scrollback: false,
    }
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
            AgentEvent::ProviderRetry { .. } => {
                state.discard_streaming_blocks();
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
                        state.latest_compaction_summary = Some(info.summary.clone());
                        state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
                            label: format!("Context compacted automatically · {reason}"),
                            summary: info.summary.clone(),
                            expanded: false,
                        })));
                    }
                    Err(error) => {
                        state.error = Some(format!("automatic compaction failed: {error}"));
                    }
                }
            }
            AgentEvent::ToolStarted { id, name, args } => {
                state.close_streaming_blocks();
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
                let index = state.tool_panels.get(id).copied();
                if let Some(panel) = state.tool_output_mut(id) {
                    panel.finished = true;
                    panel.is_error = tool_result_is_failure(&panel.name, result);
                    panel.failure_reason = tool_failure_reason(&panel.name, result);
                    match result {
                        Ok(output) => bounded_append(&mut panel.output, &output.text),
                        Err(error) => bounded_append(&mut panel.output, &error.message),
                    }
                }
                if let Some(index) = index {
                    state.touch_block(index);
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
    }

    pub fn set_verbose_tools(&mut self, verbose: bool) {
        let mut state = self.state.borrow_mut();
        if state.verbose_tools != verbose {
            state.verbose_tools = verbose;
            state.invalidate_transcript_layout();
        }
    }

    pub fn verbose_tools(&self) -> bool {
        self.state.borrow().verbose_tools
    }

    /// Toggle one tool panel by call ID, or the most recent panel when omitted.
    pub fn toggle_tool_details(
        &mut self,
        requested_id: Option<&str>,
    ) -> std::result::Result<(String, bool), String> {
        let mut state = self.state.borrow_mut();
        let index = if let Some(id) = requested_id {
            state
                .tool_panels
                .get(&ToolCallId(id.to_owned()))
                .copied()
                .ok_or_else(|| format!("unknown tool call id {id}"))?
        } else {
            state
                .transcript
                .iter()
                .rposition(|block| matches!(block, TranscriptBlock::Tool(_)))
                .ok_or_else(|| "there are no tool calls in this session".to_string())?
        };
        let TranscriptBlock::Tool(panel) = &state.transcript[index] else {
            return Err("tool panel index is inconsistent".to_string());
        };
        let id = panel.id.clone();
        let expanded = if state.expanded_tools.remove(&id) {
            false
        } else {
            state.expanded_tools.insert(id.clone());
            true
        };
        state.touch_block(index);
        Ok((id.0, expanded))
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
        *self.size.lock().expect("terminal size mutex poisoned") = (columns, rows);
        let mut state = self.state.borrow_mut();
        state.size = (columns, rows);
        let maximum = max_scroll_from_bottom(&state, columns);
        state
            .scroll_from_bottom
            .set(state.scroll_from_bottom.get().min(maximum));
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

    pub fn drain_editor(&mut self) -> String {
        let mut state = self.state.borrow_mut();
        state.editor_cursor = 0;
        state.slash_selection = 0;
        state.slash_scroll = 0;
        state.slash_popup_dismissed = false;
        std::mem::take(&mut state.editor)
    }

    fn materialize_deferred_history(&mut self) -> Result<bool> {
        let path = {
            let state = self.state.borrow();
            if state.run.is_active() {
                return Ok(false);
            }
            state.deferred_session_path.clone()
        };
        let Some(path) = path else {
            return Ok(false);
        };

        let session = Session::open_read_only(path)?;
        let items = hydrate_transcript(&session)?;
        let mut state = self.state.borrow_mut();
        let local_blocks = std::mem::take(&mut state.transcript)
            .into_iter()
            .filter(|block| {
                matches!(
                    block,
                    TranscriptBlock::Outcome(_)
                        | TranscriptBlock::Notice(_)
                        | TranscriptBlock::Shell(_)
                        | TranscriptBlock::User {
                            persisted: false,
                            ..
                        }
                )
            })
            .collect::<Vec<_>>();
        state.block_revisions.clear();
        state.tool_panels.clear();
        state.expanded_tools.clear();
        state.deferred_session_path = None;
        append_hydrated_items(&mut state, items);
        for block in local_blocks {
            state.push_block(block);
        }
        state.history_prepended.set(true);
        state.invalidate_transcript_layout();
        Ok(true)
    }

    pub fn scroll(&mut self, direction: i16) {
        if direction < 0 {
            let should_materialize = {
                let state = self.state.borrow();
                let page = usize::from(state.size.1.max(4) / 2);
                let maximum = max_scroll_from_bottom(&state, state.size.0);
                state.deferred_session_path.is_some()
                    && !state.run.is_active()
                    && state.scroll_from_bottom.get().saturating_add(page) >= maximum
            };
            if should_materialize {
                if let Err(error) = self.materialize_deferred_history() {
                    let mut state = self.state.borrow_mut();
                    state.deferred_session_path = None;
                    state.error = Some(format!("could not load older session history: {error}"));
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
                state.deferred_session_path.is_some()
                    && !state.run.is_active()
                    && state
                        .scroll_from_bottom
                        .get()
                        .saturating_add(direction.unsigned_abs() as usize)
                        >= maximum
            };
            if should_materialize {
                if let Err(error) = self.materialize_deferred_history() {
                    let mut state = self.state.borrow_mut();
                    state.deferred_session_path = None;
                    state.error = Some(format!("could not load older session history: {error}"));
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
            let mut state = self.state.borrow_mut();
            state.deferred_session_path = None;
            state.error = Some(format!("could not load older session history: {error}"));
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

    /// Toggle the most recent expandable block (reasoning, tool, shell, or
    /// compaction summary; ctrl+o).
    pub fn expand_focused_tool(&mut self) {
        let mut state = self.state.borrow_mut();
        let most_recent = state.transcript.iter().rposition(|block| {
            matches!(
                block,
                TranscriptBlock::Reasoning(_)
                    | TranscriptBlock::Tool(_)
                    | TranscriptBlock::Shell(_)
                    | TranscriptBlock::Compaction(_)
            )
        });
        if let Some(index) = most_recent {
            if let TranscriptBlock::Reasoning(reasoning) = &mut state.transcript[index] {
                reasoning.reasoning_expanded = !reasoning.reasoning_expanded;
                reasoning.invalidate_layout();
                state.touch_block(index);
                return;
            }
            if let TranscriptBlock::Compaction(compaction) = &mut state.transcript[index] {
                compaction.expanded = !compaction.expanded;
                state.touch_block(index);
                return;
            }
            match &state.transcript[index] {
                TranscriptBlock::Tool(panel) => {
                    let id = panel.id.clone();
                    if state.expanded_tools.remove(&id) {
                        // Collapse.
                    } else {
                        state.expanded_tools.insert(id);
                    }
                }
                TranscriptBlock::Shell(shell) => {
                    let id = shell.id.clone();
                    if state.expanded_shells.remove(&id) {
                        // Collapse.
                    } else {
                        state.expanded_shells.insert(id);
                    }
                }
                _ => return,
            }
            state.touch_block(index);
        } else {
            state.error = Some(
                "no reasoning, tool calls, shell commands, or compaction summaries in this session"
                    .into(),
            );
        }
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
                // Replace spinner with result icon
                shell.spinner = if exit_code == 0 {
                    "✓".to_string()
                } else {
                    "✗".to_string()
                };
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
        // Native scrollback cannot be recoloured retroactively. The epoch
        // forces the terminal to clear saved lines and replay Ygg's retained
        // logical frame once in the new theme.
    }

    /// Rebuild the visible transcript from the session's active branch.
    pub fn hydrate(&mut self, session: &Session) -> Result<()> {
        let entry_budget = usize::from(self.state.borrow().size.1)
            .saturating_mul(4)
            .clamp(64, 256);
        let (items, history_deferred) = hydrate_transcript_tail(session, entry_budget)?;
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
        state.deferred_session_path = history_deferred.then(|| session.path().to_owned());
        state.latest_compaction_summary =
            session
                .entries()
                .iter()
                .rev()
                .find_map(|entry| match &entry.value {
                    EntryValue::Compaction { summary, .. } => Some(summary.clone()),
                    _ => None,
                });
        state.transcript.clear();
        state.block_revisions.clear();
        state.invalidate_transcript_layout();
        state.steering_queue.clear();
        state.tool_panels.clear();
        state.expanded_tools.clear();
        state.expanded_shells.clear();
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

#[cfg(test)]
mod tests {
    use super::*;

    struct EmulatedTerminal {
        size: (u16, u16),
        bytes: Arc<Mutex<Vec<u8>>>,
        synchronized_output: bool,
    }

    impl EmulatedTerminal {
        fn push(&self, bytes: &[u8]) {
            self.bytes
                .lock()
                .expect("emulated terminal output mutex poisoned")
                .extend_from_slice(bytes);
        }
    }

    impl sexy_tui_rs::Terminal for EmulatedTerminal {
        fn start_events(
            &mut self,
            _on_input: Box<dyn FnMut(sexy_tui_rs::TerminalInput)>,
            _on_resize: Box<dyn FnMut()>,
        ) {
        }

        fn stop(&mut self) {}

        fn write(&mut self, data: &str) {
            // The production primary-screen terminal uses the normal output
            // post-processing convention where LF returns to column zero.
            // vt100 deliberately models raw bytes, so make that convention
            // explicit in the test backend.
            let mut previous = None;
            for byte in data.bytes() {
                if byte == b'\n' && previous != Some(b'\r') {
                    self.push(b"\r");
                }
                self.push(&[byte]);
                previous = Some(byte);
            }
        }

        fn columns(&self) -> u16 {
            self.size.0
        }

        fn rows(&self) -> u16 {
            self.size.1
        }

        fn move_by(&mut self, lines: i16) {
            if lines < 0 {
                self.push(format!("\x1b[{}A", lines.unsigned_abs()).as_bytes());
            } else if lines > 0 {
                self.push(format!("\x1b[{}B", lines.unsigned_abs()).as_bytes());
            }
        }

        fn hide_cursor(&mut self) {
            self.push(b"\x1b[?25l");
        }

        fn show_cursor(&mut self) {
            self.push(b"\x1b[?25h");
        }

        fn clear_line(&mut self) {
            self.push(b"\x1b[0m\x1b[2K");
        }

        fn clear_from_cursor(&mut self) {
            self.push(b"\x1b[0m\x1b[0J");
        }

        fn clear_screen(&mut self) {
            self.push(b"\x1b[0m\x1b[2J");
        }

        fn capabilities(&self) -> sexy_tui_rs::TerminalCapabilities {
            let mut capabilities = sexy_tui_rs::TerminalCapabilities::interactive(
                sexy_tui_rs::ColorDepth::TrueColor,
                true,
            );
            capabilities.synchronized_output = self.synchronized_output;
            capabilities.sync_output = self.synchronized_output;
            capabilities
        }
    }

    fn emulated_shell(
        theme: YggTheme,
        width: u16,
        height: u16,
    ) -> (InteractiveShell, Arc<Mutex<Vec<u8>>>) {
        emulated_shell_with_sync(theme, width, height, false)
    }

    fn emulated_shell_with_sync(
        theme: YggTheme,
        width: u16,
        height: u16,
        synchronized_output: bool,
    ) -> (InteractiveShell, Arc<Mutex<Vec<u8>>>) {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let size = Arc::new(Mutex::new((width, height)));
        let state = SharedState::new(ShellState {
            theme,
            size: (width, height),
            follow_tail: true,
            ..ShellState::default()
        });
        let mut tui = TUI::new(Box::new(EmulatedTerminal {
            size: (width, height),
            bytes: bytes.clone(),
            synchronized_output,
        }));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(ShellComponent {
            state: state.clone(),
            frame: RefCell::new(ShellFrameState::default()),
            application_viewport: false,
        }));
        tui.start();
        (
            InteractiveShell {
                tui: Some(tui),
                state,
                size,
                render_tx: None,
                render_thread: None,
                theme_config: None,
                capture_mouse: false,
            },
            bytes,
        )
    }

    fn emulate_rows(lines: &[String], width: u16) -> vt100::Parser {
        let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
        let mut terminal = vt100::Parser::new(rows, width, 0);
        for (index, line) in lines.iter().enumerate() {
            terminal.process(line.as_bytes());
            if index + 1 < lines.len() {
                terminal.process(b"\r\n");
            }
        }
        terminal
    }

    fn find_ascii_cell(screen: &vt100::Screen, needle: &str) -> Option<(u16, u16)> {
        screen
            .rows(0, screen.size().1)
            .enumerate()
            .find_map(|(row, contents)| {
                contents.find(needle).map(|byte| {
                    (
                        row as u16,
                        u16::try_from(visible_width(&contents[..byte])).unwrap_or(u16::MAX),
                    )
                })
            })
    }

    fn assert_ascii_foreground(terminal: &vt100::Parser, needle: &str, expected: vt100::Color) {
        let (row, column) = find_ascii_cell(terminal.screen(), needle).unwrap_or_else(|| {
            panic!("{needle:?} not found in {:?}", terminal.screen().contents())
        });
        for offset in 0..needle.len() as u16 {
            let cell = terminal
                .screen()
                .cell(row, column + offset)
                .expect("text cell inside terminal bounds");
            assert_eq!(
                cell.fgcolor(),
                expected,
                "foreground mismatch for {needle:?} at ({row}, {})",
                column + offset
            );
        }
    }

    fn assert_ascii_bold(terminal: &vt100::Parser, needle: &str) {
        let (row, column) = find_ascii_cell(terminal.screen(), needle).unwrap_or_else(|| {
            panic!("{needle:?} not found in {:?}", terminal.screen().contents())
        });
        for offset in 0..needle.len() as u16 {
            assert!(
                terminal
                    .screen()
                    .cell(row, column + offset)
                    .expect("text cell inside terminal bounds")
                    .bold(),
                "{needle:?} was not bold at offset {offset}"
            );
        }
    }

    fn assert_ascii_default_rendition(terminal: &vt100::Parser, needle: &str) {
        let (row, column) = find_ascii_cell(terminal.screen(), needle).unwrap_or_else(|| {
            panic!("{needle:?} not found in {:?}", terminal.screen().contents())
        });
        for offset in 0..needle.len() as u16 {
            let cell = terminal
                .screen()
                .cell(row, column + offset)
                .expect("text cell inside terminal bounds");
            assert_eq!(cell.fgcolor(), vt100::Color::Default);
            assert_eq!(cell.bgcolor(), vt100::Color::Default);
            assert!(!cell.bold(), "{needle:?} retained bold at offset {offset}");
            assert!(
                !cell.italic(),
                "{needle:?} retained italic at offset {offset}"
            );
            assert!(
                !cell.underline(),
                "{needle:?} retained underline at offset {offset}"
            );
            assert!(
                !cell.inverse(),
                "{needle:?} retained inverse at offset {offset}"
            );
        }
    }

    fn role_rgb_color(theme: &YggTheme, role: &str) -> vt100::Color {
        let (red, green, blue) = theme
            .role_rgb(role)
            .unwrap_or_else(|| panic!("test theme role {role:?} did not resolve to RGB"));
        vt100::Color::Rgb(red, green, blue)
    }

    /// Build a key-press event for panel input tests.
    fn panel_key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn panel_key_kind(
        code: crossterm::event::KeyCode,
        kind: crossterm::event::KeyEventKind,
    ) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new_with_kind(
            code,
            crossterm::event::KeyModifiers::NONE,
            kind,
        ))
    }

    /// Open a select-list panel with no descriptions.
    fn open_select_panel(shell: &mut InteractiveShell, items: &[&str]) {
        shell.open_panel(Panel::SelectList {
            title: "Select model".into(),
            items: items.iter().map(|item| item.to_string()).collect(),
            descriptions: vec![None; items.len()],
            selected: 0,
            filter: String::new(),
            action: PanelAction::SelectTheme(vec![]),
        });
    }

    fn panel_state(shell: &InteractiveShell) -> (Vec<String>, usize, String) {
        let state = shell.state.borrow();
        let Some(Panel::SelectList {
            items,
            selected,
            filter,
            ..
        }) = state.panel.as_ref()
        else {
            panic!("panel should be open");
        };
        (items.clone(), *selected, filter.clone())
    }

    fn plain_composer_surface(shell: &InteractiveShell, width: u16, now: Instant) -> Vec<String> {
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), width, now)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect()
    }

    fn plain_footer(shell: &InteractiveShell, width: u16, now: Instant) -> String {
        plain_composer_surface(shell, width, now)
            .pop()
            .expect("composer always has a status row at useful widths")
    }

    #[test]
    fn select_list_filter_narrows_items_and_confirm_returns_original_index() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 24);
        open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

        for c in "amm".chars() {
            assert!(
                shell
                    .panel_input(&panel_key(crossterm::event::KeyCode::Char(c)))
                    .is_none(),
                "typing must keep the panel open"
            );
        }

        let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(rendered.contains("gamma"), "matching item must render");
        assert!(
            !rendered.contains("alpha"),
            "filtered-out item must not render"
        );
        assert!(
            !rendered.contains("beta"),
            "filtered-out item must not render"
        );

        let (result, _) = shell
            .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
            .expect("enter should confirm the sole match");
        // "gamma" is index 2 in the original list.
        assert_eq!(result, PanelResult::Confirm(2));
        assert!(!shell.has_panel());
    }

    #[test]
    fn select_list_filter_is_case_insensitive_and_matches_descriptions() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 24);
        shell.open_panel(Panel::SelectList {
            title: "Select model".into(),
            items: vec!["gpt-4o".into(), "claude-sonnet".into()],
            descriptions: vec![
                Some("openai · 128k context".into()),
                Some("anthropic · 200k context".into()),
            ],
            selected: 0,
            filter: String::new(),
            action: PanelAction::SelectModel(vec![]),
        });

        // Multi-term uppercase query must match across label + description.
        for c in "CLAUDE ANTHROPIC".chars() {
            shell.panel_input(&panel_key(crossterm::event::KeyCode::Char(c)));
        }
        let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(rendered.contains("claude-sonnet"));
        assert!(!rendered.contains("gpt-4o"));

        let (result, _) = shell
            .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
            .expect("enter should confirm the description match");
        assert_eq!(result, PanelResult::Confirm(1));
    }

    #[test]
    fn select_list_filter_resets_cursor_and_bounds_navigation() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 24);
        open_select_panel(&mut shell, &["apple", "banana", "cherry"]);

        // Move to the last row, then filter: the cursor must restart at the
        // first match.
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('a')));
        let (_, selected, filter) = panel_state(&shell);
        assert_eq!(filter, "a");
        assert_eq!(
            selected, 0,
            "typing must reset the cursor to the first match"
        );

        // 'a' matches "apple" and "banana" only; one Down moves to the second
        // match, and a further Down is out of bounds.
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
        let (_, selected, _) = panel_state(&shell);
        assert_eq!(selected, 1, "navigation must stop at the last match");

        let (result, _) = shell
            .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
            .expect("enter should confirm the second match");
        assert_eq!(result, PanelResult::Confirm(1));
    }

    #[test]
    fn select_list_accepts_held_key_repeats_but_ignores_release() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 24);
        open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

        shell.panel_input(&panel_key_kind(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyEventKind::Repeat,
        ));
        assert_eq!(panel_state(&shell).1, 1);
        shell.panel_input(&panel_key_kind(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyEventKind::Release,
        ));
        assert_eq!(panel_state(&shell).1, 1);

        assert!(
            shell
                .panel_input(&panel_key_kind(
                    crossterm::event::KeyCode::Enter,
                    crossterm::event::KeyEventKind::Repeat,
                ))
                .is_none(),
            "a held Enter key must not confirm a panel twice"
        );
        assert!(shell.has_panel());
    }

    #[test]
    fn select_list_filter_without_matches_keeps_panel_open_on_enter() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 24);
        open_select_panel(&mut shell, &["apple", "banana", "cherry"]);

        for c in "zzz".chars() {
            shell.panel_input(&panel_key(crossterm::event::KeyCode::Char(c)));
        }
        let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(rendered.contains("No matches for"));

        // Enter is a no-op while nothing matches; Esc still cancels.
        assert!(
            shell
                .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
                .is_none(),
            "enter must not confirm when no item matches"
        );
        assert!(shell.has_panel());

        // Deleting the filter restores the full list.
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
        let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(rendered.contains("apple"));
        assert!(rendered.contains("cherry"));

        let (result, _) = shell
            .panel_input(&panel_key(crossterm::event::KeyCode::Esc))
            .expect("esc should cancel the panel");
        assert_eq!(result, PanelResult::Cancel);
    }

    #[test]
    fn select_list_has_a_stable_filter_row_and_owns_the_only_cursor() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 24);
        open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

        let empty_panel_rows = render_panel(&shell.state.borrow(), 80).len();
        let empty = render_shell(&shell.state.borrow(), 80).join("\n");
        let empty_plain = strip_terminal_sequences(&empty);
        assert!(empty_plain.contains("Filter"));
        assert!(empty_plain.contains("type to filter"));
        assert!(empty_plain.contains("1/3"));
        assert_eq!(empty.matches(CURSOR_MARKER).count(), 1);

        shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('a')));
        let filtered_panel_rows = render_panel(&shell.state.borrow(), 80).len();
        let filtered = render_shell(&shell.state.borrow(), 80).join("\n");
        let filtered_plain = strip_terminal_sequences(&filtered);
        assert_eq!(filtered_panel_rows, empty_panel_rows);
        assert!(filtered_plain.contains("Filter  a"));
        assert!(filtered_plain.contains("1/3"));
        assert_eq!(filtered.matches(CURSOR_MARKER).count(), 1);

        shell.close_panel();
        let composer = render_shell(&shell.state.borrow(), 80).join("\n");
        assert_eq!(composer.matches(CURSOR_MARKER).count(), 1);
    }

    #[test]
    fn select_list_long_filter_keeps_its_tail_and_cursor_in_narrow_panes() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(24, 12);
        open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);
        for character in "abcdefghijklmnopqrstuvwxyz".chars() {
            shell.panel_input(&panel_key(crossterm::event::KeyCode::Char(character)));
        }

        let rendered = render_shell(&shell.state.borrow(), 24).join("\n");
        assert_eq!(rendered.matches(CURSOR_MARKER).count(), 1);
        let cursor_line = rendered
            .lines()
            .find(|line| line.contains(CURSOR_MARKER))
            .expect("the active filter must own the cursor");
        let plain = strip_terminal_sequences(cursor_line).replace(CURSOR_MARKER, "");
        assert!(plain.contains("wxyz"), "{plain:?}");
        assert!(!plain.contains("abcdef"), "{plain:?}");
        assert!(visible_width(&plain) <= 24, "{plain:?}");
    }

    #[test]
    fn select_list_keeps_a_focused_filter_row_in_a_tiny_busy_terminal() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(20, 5);
        shell
            .error("a wrapped background error that would otherwise consume the picker row".into());
        open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

        let rendered = render_shell(&shell.state.borrow(), 20);
        assert_eq!(rendered.len(), 5, "{rendered:?}");
        assert_eq!(rendered.join("\n").matches(CURSOR_MARKER).count(), 1);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Filter") && line.contains(CURSOR_MARKER)),
            "{rendered:?}"
        );
    }

    #[test]
    fn select_list_aligns_muted_metadata_and_drops_it_before_narrow_labels() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(100, 24);
        shell.open_panel(Panel::SelectList {
            title: "Select model".into(),
            items: vec![
                "GPT-5.6".into(),
                "Claude Opus 4.8".into(),
                "Qwen3.6 35B A3B".into(),
            ],
            descriptions: vec![
                Some("openai · 400k context".into()),
                Some("anthropic · 1M context".into()),
                Some("openrouter · 256k context".into()),
            ],
            selected: 1,
            filter: String::new(),
            action: PanelAction::SelectModel(vec![]),
        });

        let wide = render_panel(&shell.state.borrow(), 100)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        let description_columns = ["openai", "anthropic", "openrouter"]
            .iter()
            .map(|provider| {
                wide.iter()
                    .find_map(|line| line.find(provider).map(|byte| visible_width(&line[..byte])))
                    .expect("provider metadata should be visible")
            })
            .collect::<Vec<_>>();
        assert!(
            description_columns
                .windows(2)
                .all(|columns| columns[0] == columns[1]),
            "{wide:?}"
        );
        let selected = wide
            .iter()
            .find(|line| line.contains("Claude Opus"))
            .expect("selected model should render");
        assert!(selected.trim_start().starts_with('›') || selected.trim_start().starts_with('>'));

        let narrow = render_panel(&shell.state.borrow(), 30)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        assert!(narrow.iter().any(|line| line.contains("Claude Opus")));
        assert!(!narrow.iter().any(|line| line.contains("openrouter")));
        assert!(narrow.iter().all(|line| visible_width(line) <= 30));
    }

    #[test]
    fn select_list_home_end_and_page_navigation_stay_bounded() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(52, 12);
        let items = (0..60)
            .map(|index| format!("Model {index:02}"))
            .collect::<Vec<_>>();
        shell.open_panel(Panel::SelectList {
            title: "Select model".into(),
            descriptions: vec![Some("provider · context".into()); items.len()],
            items,
            selected: 0,
            filter: String::new(),
            action: PanelAction::SelectModel(vec![]),
        });

        shell.panel_input(&panel_key(crossterm::event::KeyCode::End));
        assert_eq!(panel_state(&shell).1, 59);
        let at_end = render_panel(&shell.state.borrow(), 52)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        assert!(at_end.iter().any(|line| line.contains("60/60")));
        assert!(at_end.iter().any(|line| line.contains("Model 59")));
        assert!(at_end.iter().all(|line| visible_width(line) <= 52));

        shell.panel_input(&panel_key(crossterm::event::KeyCode::PageUp));
        assert_eq!(panel_state(&shell).1, 55);
        shell.panel_input(&panel_key(crossterm::event::KeyCode::PageDown));
        assert_eq!(panel_state(&shell).1, 59);
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Home));
        assert_eq!(panel_state(&shell).1, 0);
    }

    #[test]
    fn bounded_append_retains_a_tail_and_marks_elision() {
        let mut output = "prefix".repeat(20_000);
        bounded_append(&mut output, "THE-TAIL");
        assert!(output.len() <= MAX_PANEL_BYTES);
        assert!(output.contains("elided"));
        assert!(output.ends_with("THE-TAIL"));
    }

    #[test]
    fn sanitize_for_terminal_strips_sequences_without_leaving_protocol_debris() {
        // Clean text passes through unchanged.
        assert_eq!(sanitize_for_terminal("hello world\n"), "hello world\n");
        // NULL, BEL, and remaining C0 controls are still visible diagnostics.
        assert_eq!(sanitize_for_terminal("a\x00b\x07c\x01e"), "a␀b␇c·e");
        assert_eq!(sanitize_for_terminal("a\r\nb\rc\td"), "a\nb␍c    d");
        // Valid color, hyperlink, and charset sequences disappear as units;
        // their printable payload remains.
        assert_eq!(sanitize_for_terminal("\x1b[31mRED\x1b[0m"), "RED");
        assert_eq!(sanitize_for_terminal("\x1b(B\x1b[m\x1b[32m+"), "+");
        assert_eq!(
            sanitize_for_terminal("\x1b]8;;https://example.com\x1b\\docs\x1b]8;;\x1b\\"),
            "docs"
        );
        // Incomplete sequences are dropped rather than exposed as `[38;5`.
        assert_eq!(sanitize_for_terminal("before\x1b[38;5"), "before");
        // C1 forms are stripped with their parameters too.
        assert_eq!(sanitize_for_terminal("a\u{009b}31m"), "a");
    }

    #[test]
    fn composer_sanitization_preserves_the_cursor_without_mutating_input() {
        let raw = "before \x1b[31m after";
        let cursor = "before \x1b".len();
        let (safe, safe_cursor) = sanitized_editor(raw, cursor);
        assert_eq!(raw, "before \x1b[31m after");
        assert_eq!(&safe[..safe_cursor], "before ␛");
        assert_eq!(safe, "before ␛[31m after");
        assert!(safe.is_char_boundary(safe_cursor));
    }

    #[test]
    fn secret_tool_prompt_temporarily_owns_composer_without_touching_the_editor() {
        let mut shell = InteractiveShell::test_shell();
        for character in "ordinary draft".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.set_tool_input_prompt(Some("Password:".into()));
        let secret_surface = crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            80,
            Instant::now(),
        )
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
        assert!(secret_surface.contains("Password:"), "{secret_surface}");
        assert!(
            !secret_surface.contains("ordinary draft"),
            "{secret_surface}"
        );
        assert_eq!(shell.pending(), "ordinary draft");

        shell.set_tool_input_prompt(None);
        let restored = crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            80,
            Instant::now(),
        )
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
        assert!(restored.contains("ordinary draft"), "{restored}");
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
    fn wrapped_truecolor_never_reopens_rgb_components_as_backgrounds() {
        let mut theme = crate::tui::theme::test_theme();
        theme.override_token("accent", "#16846b");
        let styled = theme.fg("accent", "alpha beta gamma");
        assert!(styled.contains(";107m"));

        let wrapped = wrap_text_with_ansi(&styled, 6);
        assert!(wrapped.len() > 1);
        assert!(!wrapped.iter().any(|line| line.contains("\x1b[107m")));
        assert!(!wrapped.iter().any(|line| line.contains("\x1b[38;2m")));
        assert!(wrapped.join("").contains("\x1b[38;2;22;132;107m"));
    }

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

    #[test]
    fn styled_overlay_wraps_by_visible_width_without_splitting_ansi() {
        let theme = sexy_tui_rs::theme::Theme::load(
            None,
            sexy_tui_rs::theme::capability::CapabilityTier::Baseline,
        );
        let selected = format!(
            "{} — {}",
            theme.bold(&theme.fg("accent", "gpt-audio-1.5")),
            theme.fg("muted", "gpt-audio-1.5")
        );
        // This is 29 visible cells but 82 raw characters. At an 80-column
        // terminal the old raw-character wrapper split off the final reset as
        // a literal `[39m` line.
        assert_eq!(visible_width(&selected), 29);
        let wrapped = wrap_text_with_ansi(&selected, 78);
        assert_eq!(wrapped, vec![selected.clone()]);

        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 20);
        shell.show_styled_overlay_text(selected);
        let rendered = render_shell(&shell.state.borrow(), 80);
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("gpt-audio-1.5"))
                .count(),
            1,
            "one styled item must occupy one overlay row at 80 columns"
        );
        assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));
        assert!(!rendered.iter().any(|line| line == "[39m"));
    }

    #[test]
    fn markdown_transcript_renders_common_headings_lists_code_and_rules() {
        let theme = crate::tui::theme::test_theme();
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
    fn rich_text_renders_gfm_tables_tasks_links_and_fenced_code() {
        let theme = crate::tui::theme::test_theme();
        let rendered = markdown_lines(
            "- [x] migrated\n\n| Name | State |\n| --- | --- |\n| TUI | ready |\n\n[docs](https://example.com)\n\n```rust\nfn main() {}\n```",
            &theme,
            80,
        )
        .join("\n");
        assert!(rendered.contains("[x]"), "{rendered}");
        assert!(rendered.contains("migrated"), "{rendered}");
        assert!(rendered.contains("Name"));
        assert!(rendered.contains("ready"));
        assert!(rendered.contains("https://example.com"));
        assert!(rendered.contains("fn"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn slash_command_menu_lists_commands_and_tab_completes_a_unique_prefix() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Char('/'));
        let rendered = render_slash_suggestions(&shell.state.borrow(), 120, 100);
        for command in ["/new", "/model", "/login", "/cost"] {
            assert!(rendered.iter().any(|line| line.contains(command)));
        }
        let popup = rendered.join("\n");
        assert!(popup.contains("commands"));
        assert!(!popup.contains("Session"));
        assert!(!popup.contains("opens picker"));
        assert!(!popup.contains("/help"));
        assert!(popup.contains("› /new"));

        shell.slash_menu(SlashMenuAction::Last);
        let scrolled = render_slash_suggestions(&shell.state.borrow(), 80, 7).join("\n");
        assert!(scrolled.contains("/quit"), "{scrolled}");
        assert!(scrolled.contains('/'), "{scrolled}");

        shell.slash_menu(SlashMenuAction::First);
        shell.slash_menu(SlashMenuAction::Next);
        shell.slash_menu(SlashMenuAction::Select);
        assert_eq!(shell.pending(), "/resume ");
        assert!(!shell.slash_popup_open());

        shell.drain_editor();
        shell.apply_edit(EditAction::Char('/'));
        for character in "mod".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.complete_slash_command();
        assert_eq!(shell.pending(), "/model ");
    }

    #[test]
    fn discovered_prompt_templates_join_slash_autocomplete() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_prompt_templates(Arc::from(vec![crate::prompts::PromptTemplateDescriptor {
            name: "local-review".into(),
            description: "Focused local review".into(),
            argument_hint: Some("[focus]".into()),
            path: PathBuf::from("/tmp/local-review.md"),
            trust: crate::prompts::PromptTrust::UserInstalled,
            content_hash: "hash".into(),
        }]));
        for character in "/loc".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        let rendered = render_slash_suggestions(&shell.state.borrow(), 100, 10).join("\n");
        assert!(rendered.contains("/local-review [focus]"), "{rendered}");
        assert!(
            rendered.contains("prompt · Focused local review"),
            "{rendered}"
        );
        let narrow = render_slash_suggestions(&shell.state.borrow(), 32, 10).join("\n");
        assert!(narrow.contains("/local-review [focus]"), "{narrow}");
        shell.complete_slash_command();
        assert_eq!(shell.pending(), "/local-review ");
    }

    #[test]
    fn dynamic_slash_discovery_contains_only_registered_executable_names() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_prompt_templates(Arc::from(vec![crate::prompts::PromptTemplateDescriptor {
            name: "local-review".into(),
            description: "Focused local review".into(),
            argument_hint: None,
            path: PathBuf::from("/tmp/local-review.md"),
            trust: crate::prompts::PromptTrust::UserInstalled,
            content_hash: "hash".into(),
        }]));
        shell.set_extension_commands(Arc::from(vec![
            ("checkpoint".into(), "Save checkpoint".into()),
            // A dynamic command cannot shadow a working built-in.
            ("status".into(), "Shadow status".into()),
        ]));
        shell.apply_edit(EditAction::Char('/'));

        let state = shell.state.borrow();
        let suggestions = input_slash_suggestions(&state);
        let prompt_names = state
            .prompt_templates
            .iter()
            .map(|template| template.name.as_str())
            .collect::<HashSet<_>>();
        let extension_names = state
            .extension_commands
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<HashSet<_>>();
        for suggestion in suggestions.iter().filter(|suggestion| {
            suggestion.description.starts_with("prompt ·")
                || suggestion.description.starts_with("extension ·")
        }) {
            let registered = if suggestion.description.starts_with("prompt ·") {
                prompt_names.contains(suggestion.name.as_str())
            } else {
                extension_names.contains(suggestion.name.as_str())
            };
            assert!(registered, "unregistered suggestion: {suggestion:?}");
        }
        assert_eq!(
            suggestions
                .iter()
                .filter(|suggestion| suggestion.name == "status")
                .count(),
            1,
            "dynamic command shadowed the built-in route"
        );
        assert!(suggestions.iter().any(|suggestion| {
            suggestion.name == "local-review" && suggestion.description.starts_with("prompt ·")
        }));
        assert!(suggestions.iter().any(|suggestion| {
            suggestion.name == "checkpoint" && suggestion.description.starts_with("extension ·")
        }));
    }

    #[test]
    fn mention_completion_inserts_path_reference_for_text_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"x").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        for character in "see @main".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        let rendered = render_shell(&shell.state.borrow(), 120);
        assert!(rendered
            .iter()
            .any(|line| line.contains("project files · tab completes")));
        assert!(rendered.iter().any(|line| line.contains("src/main.rs")));
        shell.complete_mention();
        assert_eq!(shell.pending(), "see @src/main.rs ");
    }

    #[test]
    fn mention_completion_attaches_media_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        shell.set_input_modalities(ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image));
        for character in "@shot".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.complete_mention();
        assert_eq!(shell.pending(), "[Image #1]");
        let composed = shell.drain_composed();
        assert!(composed
            .parts
            .iter()
            .any(|part| matches!(part, ygg_agent::InputPart::Media(_))));
    }

    #[test]
    fn set_workspace_keeps_the_file_index_when_the_root_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"x").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        for character in "@a".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        assert!(shell.state.borrow().file_index.is_some());

        // Re-asserting the same root (update_status runs after every turn)
        // must not drop the lazily built index and force a workspace re-walk.
        shell.set_workspace(dir.path().to_path_buf());
        assert!(shell.state.borrow().file_index.is_some());

        // A genuinely different root invalidates it.
        let other = tempfile::tempdir().unwrap();
        shell.set_workspace(other.path().to_path_buf());
        assert!(shell.state.borrow().file_index.is_none());
    }

    #[test]
    fn invalidate_file_index_forces_a_fresh_walk_for_new_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"x").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        for character in "@a".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        assert!(shell.state.borrow().file_index.is_some());

        // A run may have created files; invalidation makes the next mention
        // pick them up.
        std::fs::write(dir.path().join("brand_new.rs"), b"x").unwrap();
        shell.invalidate_file_index();
        assert!(shell.state.borrow().file_index.is_none());
        shell.apply_edit(EditAction::Char('_'));
        let state = shell.state.borrow();
        let files = state.file_index.as_ref().unwrap();
        assert!(files.iter().any(|file| file == "brand_new.rs"));
    }

    #[test]
    fn unsupported_media_mention_falls_back_to_a_path_and_notice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        for character in "@shot".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.complete_mention();

        assert_eq!(shell.pending(), "@shot.png ");
        assert!(shell
            .debug_snapshot()
            .contains("does not accept image input"));
    }

    #[test]
    fn output_token_rate_uses_authoritative_usage_and_generation_elapsed_time() {
        assert_eq!(
            output_tokens_per_second(120, Duration::from_secs(2)),
            Some(60.0)
        );
        assert!(output_tokens_per_second(1, Duration::from_millis(250))
            .is_some_and(|rate| (rate - 4.0).abs() < f64::EPSILON));
        assert_eq!(output_tokens_per_second(0, Duration::from_secs(1)), None);
        assert_eq!(output_tokens_per_second(1, Duration::ZERO), None);
    }

    #[test]
    fn context_uses_single_turn_provider_total_not_cumulative_run_usage() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("openai", "gpt-5", "high");
        shell.set_context_estimate(80, 272_000);
        shell.begin_run("openai");
        let turn_usage = Usage {
            input_tokens: 10_000,
            cache_read_tokens: 200_000,
            output_tokens: 10_000,
            total_tokens: 220_000,
            ..Usage::default()
        };
        shell.on_agent_event(&AgentEvent::TurnFinished {
            message: ygg_ai::AssistantMessage {
                content: vec![ygg_ai::AssistantPart::Text("done".into())],
                model: ModelId("gpt-5".into()),
                protocol: ygg_ai::Protocol::OpenAiResponses,
            },
            turn_usage,
            usage: Usage {
                input_tokens: 20_000,
                cache_read_tokens: 370_000,
                output_tokens: 20_000,
                total_tokens: 410_000,
                ..Usage::default()
            },
            session_cost_microdollars: None,
            run_cost_microdollars: 0,
        });

        let state = shell.state.borrow();
        assert_eq!(state.last_turn_usage, Some(turn_usage));
        assert_eq!(state.run_context_estimate, Some((220_000, 272_000)));
        assert_eq!(state.context_estimate, Some((220_000, 272_000)));
    }

    #[test]
    fn submitted_prompts_render_immediately_with_real_context_budget() {
        let mut shell = InteractiveShell::test_shell();
        shell.on_prompt_submitted("second prompt");
        shell.set_identity("deepseek", "deepseek-v4-pro", "high");
        shell.set_context_estimate(900_000, 967_232);
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("second prompt"));
        let rendered = render_shell(&shell.state.borrow(), 120);
        let footer = rendered.last().expect("single composer footer");
        assert!(
            strip_terminal_sequences(footer).contains("900.0k/967.2k"),
            "footer was {footer:?}"
        );
    }

    #[test]
    fn local_shell_commands_do_not_claim_a_model_prompt_color() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("openai", "gpt-5.6", "high");
        shell.on_local_command_submitted("!git status");
        let state = shell.state.borrow();
        let TranscriptBlock::User { prompt_color, .. } = &state.transcript[0] else {
            panic!("local command transcript row expected");
        };
        assert_eq!(prompt_color, &None);
        let rendered = render_block(
            None,
            &state.transcript[0],
            &state.theme,
            &state.theme.rich_renderer(),
            &state.theme.reasoning_renderer(),
            80,
            false,
        )
        .join("\n");
        assert!(!rendered.contains("\x1b[48;"), "{rendered:?}");
    }

    #[test]
    fn steering_messages_are_queued_above_prompt_and_delivered_as_a_batch() {
        let mut shell = InteractiveShell::test_shell();
        shell.queue_steering(&ComposedInput::from_text("check the docs".into()));
        shell.queue_steering(&ComposedInput::from_text("then run the tests".into()));

        let rendered = render_shell(&shell.state.borrow(), 120);
        let prompt = rendered
            .iter()
            .position(|line| line.contains(CURSOR_MARKER))
            .expect("prompt line");
        let plain = rendered
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        let queue = plain
            .iter()
            .position(|line| line.contains("Steering prompts · 2 queued"))
            .expect("steering queue");
        assert!(queue < prompt);
        assert!(plain
            .iter()
            .any(|line| line.starts_with("    ↳ check the docs")));
        assert!(plain
            .iter()
            .any(|line| line.starts_with("    ↳ then run the tests")));

        shell.on_agent_event(&AgentEvent::SteeringDelivered {
            messages: vec!["check the docs".into(), "then run the tests".into()],
        });
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("check the docs"));
        assert!(snapshot.contains("then run the tests"));
        assert!(!render_shell(&shell.state.borrow(), 120)
            .iter()
            .any(|line| line.contains("Steering prompts")));
    }

    #[test]
    fn composer_soft_wraps_at_word_boundaries_and_hard_wraps_long_words() {
        let text = "alpha beta gamma";
        let lines = editor_visual_lines(text, 10);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["alpha beta", "gamma"]);
        assert_eq!(
            lines
                .iter()
                .map(|line| &text[line.start..line.end])
                .collect::<String>(),
            text,
            "soft wrapping must retain every source byte for cursor editing"
        );

        // A word that exactly follows a full row must not create a
        // whitespace-only row or split despite fitting on its own.
        let text = "one two";
        let lines = editor_visual_lines(text, 3);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["one", "two"]);

        let text = "supercalifragilistic";
        let lines = editor_visual_lines(text, 5);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["super", "calif", "ragil", "istic"]);
    }

    #[test]
    fn composer_word_wrap_preserves_explicit_newlines() {
        let text = "one two\nthree four\n";
        let lines = editor_visual_lines(text, 6);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["one", "two", "three", "four", ""]);
    }

    #[test]
    fn terminal_native_prompt_wraps_and_shrinks_without_a_panel() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(24, 10);
        for character in "abcdefghijklmnopqrstuvwxyz0123456789".chars() {
            shell.apply_edit(EditAction::Char(character));
        }

        let rendered = render_prompt_box(&shell.state.borrow(), 24, 8);
        assert!(rendered.len() > 1, "long input should grow the editor");
        assert!(rendered.iter().all(|line| visible_width(line) <= 24));
        assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));
        assert!(!rendered.iter().any(|line| {
            line.chars()
                .any(|character| matches!(character, '┏' | '┓' | '┗' | '┛'))
        }));

        shell.drain_editor();
        let rendered = render_prompt_box(&shell.state.borrow(), 24, 8);
        assert_eq!(rendered.len(), 1, "empty editor should shrink to one row");
        assert!(rendered[0].contains('›'));
    }

    #[test]
    fn terminal_native_prompt_stays_within_every_viewport() {
        for (width, height) in [
            (1, 5),
            (2, 5),
            (3, 5),
            (4, 5),
            (8, 5),
            (12, 7),
            (24, 10),
            (40, 12),
            (60, 18),
            (80, 24),
            (120, 30),
            (160, 40),
        ] {
            let mut shell = InteractiveShell::test_shell();
            shell.set_size(width, height);
            for character in "a long prompt that must wrap cleanly at every width".chars() {
                shell.apply_edit(EditAction::Char(character));
            }

            let rendered = render_shell(&shell.state.borrow(), width);
            assert!(rendered.len() <= usize::from(height));
            assert!(
                rendered
                    .iter()
                    .all(|line| visible_width(line) <= usize::from(width)),
                "{width}x{height}: {rendered:?}"
            );
            assert!(!rendered.iter().any(|line| {
                line.chars()
                    .any(|character| matches!(character, '┏' | '┓' | '┗' | '┛'))
            }));
            if width >= 4 {
                assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));
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
    fn media_path_paste_attaches_a_chip_and_composes_media_parts() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_input_modalities(
            ygg_ai::ModalitySet::none()
                .with(ygg_ai::Modality::Image)
                .with(ygg_ai::Modality::Audio),
        );
        for character in "see ".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.apply_edit(EditAction::Paste(image.display().to_string()));

        let composed = shell.drain_composed();
        assert_eq!(composed.display_text, "see [Image #1]");
        assert!(composed
            .parts
            .iter()
            .any(|part| matches!(part, ygg_agent::InputPart::Media(_))));
    }

    #[test]
    fn raw_key_drop_with_surrounding_prompt_still_attaches_media() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("screen shot.png");
        std::fs::write(&image, b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_input_modalities(ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image));
        let escaped = image.display().to_string().replace(' ', "\\ ");
        for character in format!("{escaped} diagnose this UI").chars() {
            shell.apply_edit(EditAction::Char(character));
        }

        let composed = shell.drain_composed();
        assert!(composed
            .display_text
            .contains("[Image #1] diagnose this UI"));
        assert!(composed
            .parts
            .iter()
            .any(|part| matches!(part, ygg_agent::InputPart::Media(ygg_ai::Media::Image(_)))));
        assert!(composed.parts.iter().any(
            |part| matches!(part, ygg_agent::InputPart::Text(text) if text.contains("diagnose this UI"))
        ));
    }

    #[test]
    fn media_paste_without_capability_inserts_plain_path_and_notice() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_input_modalities(ygg_ai::ModalitySet::none());
        shell.apply_edit(EditAction::Paste(image.display().to_string()));

        let composed = shell.drain_composed();
        assert_eq!(composed.display_text, image.display().to_string());
        assert!(composed
            .parts
            .iter()
            .all(|part| matches!(part, ygg_agent::InputPart::Text(_))));
        assert!(shell
            .debug_snapshot()
            .contains("does not accept image input"));
    }

    #[test]
    fn large_paste_collapses_to_chip_and_splices_back_on_drain() {
        let mut shell = InteractiveShell::test_shell();
        let large = "line\n".repeat(20);
        shell.apply_edit(EditAction::Paste(large.clone()));

        let state_text = shell.pending();
        assert!(state_text.starts_with("[Pasted text #1: 20 lines]"));

        let composed = shell.drain_composed();
        assert!(matches!(
            composed.parts.as_slice(),
            [ygg_agent::InputPart::Text(text)] if text.matches("line").count() == 20
        ));
    }

    #[test]
    fn small_paste_still_inserts_verbatim() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Paste("first\nsecond".into()));
        assert_eq!(shell.pending(), "first\nsecond");
    }

    #[test]
    fn steering_restore_returns_chips_and_attachments() {
        let mut shell = InteractiveShell::test_shell();
        let large = "line\n".repeat(20);
        shell.apply_edit(EditAction::Paste(large));
        let composed = shell.drain_composed();
        shell.queue_steering(&composed);

        shell.restore_queued_steering();
        assert!(shell.pending().contains("[Pasted text #1: 20 lines]"));
        // The ledger got its entry back: draining resolves the chip again.
        let recomposed = shell.drain_composed();
        assert!(matches!(
            recomposed.parts.as_slice(),
            [ygg_agent::InputPart::Text(text)] if text.matches("line").count() == 20
        ));
    }

    #[test]
    fn aborted_final_frame_shows_interruption_and_restored_steering() {
        use ygg_agent::{EntryId, FinishReason};

        const WIDTH: u16 = 72;
        const HEIGHT: u16 = 18;
        for synchronized_output in [false, true] {
            let (mut shell, bytes) = emulated_shell_with_sync(
                crate::tui::theme::test_theme(),
                WIDTH,
                HEIGHT,
                synchronized_output,
            );
            let run_id = shell.begin_run("temper");
            shell.queue_steering(&ComposedInput::from_text("inspect renderer".into()));
            shell.queue_steering(&ComposedInput::from_text("then run tests".into()));

            // This is the production ordering at the terminal run boundary:
            // settle the outcome, restore any undelivered queue, then publish
            // one complete frame.
            shell.on_run_event(
                run_id,
                &AgentEvent::RunFinished {
                    head: EntryId("aborted-head".into()),
                    reason: FinishReason::Aborted,
                },
            );
            shell.restore_queued_steering();
            shell.render();

            let output = bytes.lock().unwrap().clone();
            let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
            terminal.process(&output);
            let physical = terminal.screen().contents();
            assert_eq!(physical.matches("interrupted").count(), 1, "{physical}");
            assert!(physical.contains("inspect renderer"), "{physical}");
            assert!(physical.contains("then run tests"), "{physical}");
            assert!(!physical.contains("Steering prompt"), "{physical}");
        }
    }

    #[test]
    fn steering_delivery_is_positional_fifo() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Paste("go left".into()));
        let first = shell.drain_composed();
        shell.queue_steering(&first);
        shell.apply_edit(EditAction::Paste("go right".into()));
        let second = shell.drain_composed();
        shell.queue_steering(&second);

        shell.on_agent_event(&AgentEvent::SteeringDelivered {
            messages: vec!["go left".into()],
        });
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("go left"));
        // Second message still pending.
        assert!(render_shell(&shell.state.borrow(), 120)
            .iter()
            .any(|line| line.contains("go right")));
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
            .find(|line| line.contains(CURSOR_MARKER))
            .unwrap();
        assert!(line.find("abcdX").unwrap() < line.find(CURSOR_MARKER).unwrap());
        assert!(line.find(CURSOR_MARKER).unwrap() < line.find("ef").unwrap());
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
    fn new_output_does_not_move_a_scrolled_reader_viewport() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 18);
        for number in 0..100 {
            shell.notice(format!("anchor notice {number}"));
        }
        let _ = render_shell(&shell.state.borrow(), 80);
        shell.scroll_lines(-6);
        let before = render_shell(&shell.state.borrow(), 80)
            .into_iter()
            .filter(|line| line.contains("anchor notice"))
            .collect::<Vec<_>>();

        shell.notice("new output while reading");
        let after = render_shell(&shell.state.borrow(), 80)
            .into_iter()
            .filter(|line| line.contains("anchor notice"))
            .collect::<Vec<_>>();
        assert_eq!(after, before);
    }

    #[test]
    fn resumed_history_is_tail_first_and_materializes_when_scrolling_past_it() {
        use ygg_agent::EntryValue;
        use ygg_ai::{Message, UserMessage, UserPart};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut session = Session::create(&path).unwrap();
        for index in 0..100 {
            session
                .append(EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::Text(format!("prompt {index}"))],
                })))
                .unwrap();
        }

        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 12);
        shell.hydrate(&session).unwrap();
        assert!(shell.debug_snapshot().contains("prompt 99"));
        assert!(!shell.debug_snapshot().contains("prompt 0\n"));
        assert!(shell.state.borrow().deferred_session_path.is_some());
        shell.on_local_command_submitted("!local-only command");

        let page = usize::from(shell.state.borrow().size.1.max(4) / 2);
        let mut crossing_scroll = None;
        for _ in 0..100 {
            let before = shell.state.borrow().scroll_from_bottom.get();
            shell.scroll(-1);
            if shell.state.borrow().deferred_session_path.is_none() {
                crossing_scroll = Some((before, shell.state.borrow().scroll_from_bottom.get()));
                break;
            }
        }
        assert!(shell.state.borrow().deferred_session_path.is_none());
        let (before, after) = crossing_scroll.expect("deferred history crossing");
        assert!(
            after <= before.saturating_add(page),
            "prepending history must advance one page, not jump to oldest: {before} -> {after}"
        );
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("prompt 0\n"));
        assert_eq!(snapshot.matches("!local-only command").count(), 1);
    }

    #[test]
    fn resumed_session_restores_every_write_as_a_diff_panel() {
        use ygg_agent::EntryValue;
        use ygg_ai::{
            AssistantMessage, AssistantPart, Message, Protocol, ToolCall, ToolResult,
            ToolResultPart, UserMessage, UserPart,
        };

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("write both files".into())],
            })))
            .unwrap();

        let writes = [
            (
                "write-current",
                "new.rs",
                "ok\nnew.rs  created hash=x\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+current format\n",
            ),
            (
                "write-legacy",
                "legacy.rs",
                "ok\nlegacy.rs  created hash=y\n--- /dev/null\n+++ b/legacy.rs\n+legacy format\n",
            ),
        ];
        for (id, path, result) in writes {
            session
                .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId(id.into()),
                        name: "write".into(),
                        arguments_json: serde_json::json!({
                            "path": path,
                            "content": format!("{path} contents\n"),
                        })
                        .to_string(),
                    })],
                    model: ModelId("gpt-5.6-sol".into()),
                    protocol: Protocol::OpenAiResponses,
                })))
                .unwrap();
            session
                .append(EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::ToolResult(ToolResult {
                        tool_call_id: ToolCallId(id.into()),
                        content: vec![ToolResultPart::Text(result.into())],
                        is_error: false,
                    })],
                })))
                .unwrap();
        }

        let mut shell = InteractiveShell::test_shell();
        shell.set_size(120, 40);
        shell.hydrate(&session).unwrap();
        let rendered =
            strip_terminal_sequences(&render_shell(&shell.state.borrow(), 120).join("\n"));

        assert!(rendered.contains("current format"), "{rendered}");
        assert!(rendered.contains("legacy format"), "{rendered}");
        assert!(rendered.matches("/dev/null").count() >= 2, "{rendered}");
    }

    #[test]
    fn duplicate_hydrated_tool_call_ids_never_leave_a_running_card() {
        use ygg_ai::{
            AssistantMessage, AssistantPart, Message, Protocol, ToolCall, ToolResult,
            ToolResultPart, UserMessage, UserPart,
        };

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![
                    AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("duplicate".into()),
                        name: "read".into(),
                        arguments_json: r#"{"path":"first"}"#.into(),
                    }),
                    AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("duplicate".into()),
                        name: "read".into(),
                        arguments_json: r#"{"path":"second"}"#.into(),
                    }),
                ],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("duplicate".into()),
                    content: vec![ToolResultPart::Text("durable result".into())],
                    is_error: false,
                })],
            })))
            .unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.hydrate(&session).unwrap();
        let state = shell.state.borrow();
        let panels = state
            .transcript
            .iter()
            .filter_map(|block| match block {
                TranscriptBlock::Tool(panel) => Some(panel.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(panels.len(), 2);
        assert!(
            panels.iter().all(|panel| panel.finished),
            "duplicate recovered IDs must never revive a running card: {panels:?}"
        );
        assert!(panels.iter().any(|panel| panel.is_error));
        assert!(panels.iter().any(|panel| !panel.is_error));
    }

    #[test]
    fn streamed_delta_marks_only_its_changed_cached_block() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_theme(crate::tui::theme::test_theme_from_source(
            SURFACE_TEST_THEME,
        ));
        for number in 0..500 {
            shell.notice(format!("historic {number}"));
        }
        shell.on_agent_event(&AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "first".into(),
        });
        let _ = render_shell(&shell.state.borrow(), 120);
        let assistant_index = shell
            .state
            .borrow()
            .active_text
            .expect("active assistant block");

        // Keep a later block in the layout so this exercises the splice/start
        // adjustments as well as the no-history-scan dirty path.
        shell.notice("later block");
        shell.on_agent_event(&AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: " second".into(),
        });
        {
            let state = shell.state.borrow();
            let cache = state.transcript_cache.borrow();
            assert_eq!(cache.dirty_blocks, vec![assistant_index]);
        }

        let rendered = render_shell(&shell.state.borrow(), 120).join("\n");
        assert!(rendered.contains("first second"));
        assert!(rendered.contains("later block"));
        assert!(shell
            .state
            .borrow()
            .transcript_cache
            .borrow()
            .dirty_blocks
            .is_empty());
        {
            let state = shell.state.borrow();
            let cache = state.transcript_cache.borrow();
            assert_eq!(cache.block_geometries.len(), state.transcript.len());
            assert_eq!(cache.block_geometries[assistant_index].leading_rows, 1);
            assert_eq!(cache.block_geometries[assistant_index].trailing_rows, 1);
            let later = assistant_index + 1;
            assert_eq!(
                cache.block_starts[later],
                cache.block_starts[assistant_index] + cache.block_lengths[assistant_index]
            );
        }
    }

    #[test]
    fn hidden_reasoning_stream_does_not_grow_native_scrollback() {
        const WIDTH: u16 = 64;
        const HEIGHT: u16 = 10;
        let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        terminal.process(&drain(&bytes));
        terminal.set_scrollback(usize::MAX);
        let baseline_scrollback = terminal.screen().scrollback();
        terminal.set_scrollback(0);

        let run_id = shell.begin_run("openai");
        for index in 0..160 {
            shell.on_run_event(
                run_id,
                &AgentEvent::OutputDelta {
                    channel: OutputChannel::Reasoning,
                    text: format!("private sentinel {index}\n"),
                },
            );
            shell.render();
        }
        terminal.process(&drain(&bytes));
        terminal.set_scrollback(usize::MAX);
        assert_eq!(
            terminal.screen().scrollback(),
            baseline_scrollback,
            "collapsed streaming reasoning must not commit mutable rows"
        );
        terminal.set_scrollback(0);
        let visible = terminal.screen().contents();
        assert!(visible.contains("thinking"), "{visible:?}");
        assert!(visible.contains("ctrl+o to expand"), "{visible:?}");
        assert!(!visible.contains("private sentinel"), "{visible:?}");
        let state = shell.state.borrow();
        let TranscriptBlock::Reasoning(reasoning) = state.transcript.last().unwrap() else {
            panic!("reasoning block expected");
        };
        assert!(reasoning.text.contains("private sentinel 159"));
    }

    #[test]
    fn streamed_assistant_rows_enter_native_scrollback_once() {
        const WIDTH: u16 = 96;
        const HEIGHT: u16 = 48;
        let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        terminal.process(&drain(&bytes));

        let run_id = shell.begin_run("openai");
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: "private reasoning sentinel".into(),
            },
        );
        shell.render();
        terminal.process(&drain(&bytes));
        let mut response = String::from("# Stream report\n\n## Findings\n\n");
        for index in 0..48 {
            response.push_str(&format!(
                "- **stream-sentinel-{index:02}**: detailed finding for row {index}\n"
            ));
            if index == 15 {
                response.push_str("\n## Nested concerns\n\n");
            } else if index == 31 {
                response.push_str("\n## Final checks\n\n");
            }
        }
        let response_chars = response.chars().collect::<Vec<_>>();
        for chunk in response_chars.chunks(7) {
            shell.state.borrow_mut().advance_reasoning_animation();
            shell.render();
            terminal.process(&drain(&bytes));
            shell.on_run_event(
                run_id,
                &AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: chunk.iter().collect(),
                },
            );
            shell.render();
            terminal.process(&drain(&bytes));
        }

        // Grow the parser's viewport before looking back so its public
        // contents API can expose the complete retained history at once.
        terminal.set_size(512, WIDTH);
        terminal.set_scrollback(usize::MAX);
        let physical = terminal.screen().contents();

        for index in 0..48 {
            let sentinel = format!("stream-sentinel-{index:02}");
            assert_eq!(
                physical.matches(&sentinel).count(),
                1,
                "{sentinel} was duplicated in native scrollback:\n{physical}"
            );
        }
    }

    #[test]
    fn ctrl_o_expands_and_collapses_the_inline_compaction_summary() {
        let mut shell = InteractiveShell::test_shell();
        shell.compaction_marker(
            "Context compacted · 12,000 input tokens summarized",
            "# Grounded summary\n\n- kept decision\n- **summary sentinel**",
        );
        let plain = |shell: &InteractiveShell| {
            strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"))
        };

        let collapsed = plain(&shell);
        assert!(
            collapsed.contains("12,000 input tokens summarized"),
            "{collapsed}"
        );
        assert!(collapsed.contains("ctrl+o to view"), "{collapsed}");
        assert!(!collapsed.contains("summary sentinel"), "{collapsed}");

        shell.expand_focused_tool();
        let expanded = plain(&shell);
        assert!(expanded.contains("Grounded summary"), "{expanded}");
        assert!(expanded.contains("summary sentinel"), "{expanded}");
        assert!(expanded.contains("ctrl+o to collapse"), "{expanded}");
        assert!(!shell.has_overlay(), "compaction must expand inline");

        shell.expand_focused_tool();
        let collapsed_again = plain(&shell);
        assert!(
            !collapsed_again.contains("summary sentinel"),
            "{collapsed_again}"
        );
        assert!(
            collapsed_again.contains("ctrl+o to view"),
            "{collapsed_again}"
        );
    }

    #[test]
    fn autonomous_compaction_events_show_work_success_and_failure_inline() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("openai", "gpt-5.6", "high");
        let run_id = shell.begin_run("openai");
        shell.on_run_event(
            run_id,
            &AgentEvent::CompactionStarted {
                reason: ygg_agent::CompactionReason::Threshold,
            },
        );
        let working = strip_terminal_sequences(
            &crate::tui::composer_surface::render_composer_surface(
                &shell.state.borrow(),
                80,
                Instant::now() + Duration::from_secs(1),
            )
            .join("\n"),
        );
        assert!(working.contains("compacting"), "{working}");
        assert!(!working.contains("waiting for API"), "{working}");

        shell.on_run_event(
            run_id,
            &AgentEvent::CompactionFinished {
                reason: ygg_agent::CompactionReason::Threshold,
                result: Ok(ygg_agent::CompactionInfo {
                    summary: "# Automatic summary\n\nauto-summary sentinel".into(),
                    first_kept: ygg_agent::EntryId("kept".into()),
                }),
            },
        );
        let collapsed =
            strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"));
        assert!(
            collapsed.contains("Context compacted automatically"),
            "{collapsed}"
        );
        assert!(!collapsed.contains("auto-summary sentinel"), "{collapsed}");
        shell.expand_focused_tool();
        let expanded =
            strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"));
        assert!(expanded.contains("auto-summary sentinel"), "{expanded}");

        let mut failed_shell = InteractiveShell::test_shell();
        let failed_run = failed_shell.begin_run("openai");
        failed_shell.on_run_event(
            failed_run,
            &AgentEvent::CompactionStarted {
                reason: ygg_agent::CompactionReason::Overflow,
            },
        );
        failed_shell.on_run_event(
            failed_run,
            &AgentEvent::CompactionFinished {
                reason: ygg_agent::CompactionReason::Overflow,
                result: Err("cold endpoint timed out".into()),
            },
        );
        assert_eq!(
            failed_shell.debug_error().as_deref(),
            Some("automatic compaction failed: cold endpoint timed out")
        );
        assert!(failed_shell.state.borrow().run_label.is_empty());
    }

    #[test]
    fn resumed_compaction_summary_remains_expandable_after_theme_switch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut session = Session::create(&path).unwrap();
        let first_kept = session
            .append(EntryValue::Config {
                model: Some("gpt-5.6".into()),
                reasoning: Some("high".into()),
            })
            .unwrap();
        session
            .append(EntryValue::Compaction {
                summary: "# Resumed summary\n\nresume-only sentinel".into(),
                first_kept,
                active_skills: Vec::new(),
                skill_resources: Vec::new(),
            })
            .unwrap();
        drop(session);

        let resumed = Session::open(path).unwrap();
        let mut shell = InteractiveShell::test_shell();
        shell.show_overlay_text("stale session overlay".into());
        shell.hydrate(&resumed).unwrap();
        assert!(
            !shell.has_overlay(),
            "resume must close session-local overlays"
        );
        let render = |shell: &InteractiveShell| {
            strip_terminal_sequences(&shell.state.borrow().rendered_transcript(72).join("\n"))
        };
        assert!(!render(&shell).contains("resume-only sentinel"));

        shell.expand_focused_tool();
        assert!(render(&shell).contains("resume-only sentinel"));
        shell.set_theme(crate::tui::theme::test_theme());
        let restyled = render(&shell);
        assert!(restyled.contains("resume-only sentinel"), "{restyled}");
        assert!(restyled.contains("ctrl+o to collapse"), "{restyled}");
    }

    #[test]
    fn compaction_expansion_does_not_replay_native_scrollback() {
        const WIDTH: u16 = 88;
        const HEIGHT: u16 = 18;
        for synchronized_output in [false, true] {
            let (mut shell, bytes) = emulated_shell_with_sync(
                crate::tui::theme::test_theme(),
                WIDTH,
                HEIGHT,
                synchronized_output,
            );
            let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
            let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
                std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
            };
            terminal.process(&drain(&bytes));

            for index in 0..24 {
                shell.notice(format!("compaction-history-{index:02}"));
            }
            shell.compaction_marker(
                "Context compacted",
                "# Summary\n\n- compaction-detail-a\n- compaction-detail-b",
            );
            shell.render();
            terminal.process(&drain(&bytes));

            shell.expand_focused_tool();
            shell.render();
            let expansion = drain(&bytes);
            let expansion_text = String::from_utf8_lossy(&expansion);
            assert!(
                expansion_text.contains("compaction-detail-a"),
                "{expansion_text:?}"
            );
            assert!(
                !expansion_text.contains("compaction-history"),
                "expansion replayed committed history: {expansion_text:?}"
            );
            terminal.process(&expansion);

            terminal.set_size(512, WIDTH);
            terminal.set_scrollback(usize::MAX);
            let physical = terminal.screen().contents();
            for index in 0..24 {
                let sentinel = format!("compaction-history-{index:02}");
                assert_eq!(
                    physical.matches(&sentinel).count(),
                    1,
                    "{sentinel} replayed with synchronized_output={synchronized_output}:\n{physical}"
                );
            }
            assert_eq!(
                physical.matches("compaction-detail-a").count(),
                1,
                "{physical}"
            );
        }
    }

    #[test]
    fn streamed_table_and_wrapped_lists_never_commit_provisional_duplicates() {
        const WIDTH: u16 = 96;
        const HEIGHT: u16 = 22;
        let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        terminal.process(&drain(&bytes));

        let response = "\
# Files

| File | Lines | Content |
|---|---:|---|
| README.md | 603 | Full architecture deep-dive |
| extracted-prompts.md | 76 | Verbatim prompt excerpts |
| env-vars-reference.md | 518 | 512 environment variables |

## Key Findings

1. **Multi-Agent Architecture**

- **Coordinator** spawns subagents and workers with isolated context windows
- **Three worker types** run concurrently and preserve their own tool state
- **Guidance** remains visible exactly once even when this sentence wraps across terminal rows

2. **System Prompt Architecture**
";
        let run_id = shell.begin_run("openai");
        for chunk in response.as_bytes().chunks(5) {
            shell.on_run_event(
                run_id,
                &AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: String::from_utf8(chunk.to_vec()).unwrap(),
                },
            );
            shell.render();
            terminal.process(&drain(&bytes));
        }

        terminal.set_size(256, WIDTH);
        terminal.set_scrollback(usize::MAX);
        let physical = terminal.screen().contents();
        for sentinel in [
            "Files",
            "Key Findings",
            "Multi-Agent Architecture",
            "Coordinator",
            "Three worker types",
            "Guidance",
            "System Prompt Architecture",
        ] {
            assert_eq!(
                physical.matches(sentinel).count(),
                1,
                "{sentinel:?} was duplicated in native scrollback:\n{physical}"
            );
        }
    }

    #[test]
    fn closing_overlay_reanchors_without_replaying_native_scrollback() {
        const WIDTH: u16 = 80;
        const HEIGHT: u16 = 16;
        for synchronized_output in [false, true] {
            let (mut shell, bytes) = emulated_shell_with_sync(
                crate::tui::theme::test_theme(),
                WIDTH,
                HEIGHT,
                synchronized_output,
            );
            let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
            let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
                std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
            };
            terminal.process(&drain(&bytes));

            for index in 0..32 {
                shell.notice(format!("overlay-history-{index:02}"));
            }
            shell.render();
            terminal.process(&drain(&bytes));

            shell.show_overlay_text("status overlay\nclose me".into());
            shell.render();
            terminal.process(&drain(&bytes));

            shell.close_overlay();
            shell.render();
            let close_frame = drain(&bytes);
            // Closing a one-viewport overlay must repaint only the visible
            // tail. Replaying the retained transcript here is what created
            // duplicated lines in terminal-owned scrollback.
            assert!(
                close_frame.len() < 8 * 1024,
                "overlay close replayed an unbounded frame ({} bytes)",
                close_frame.len()
            );
            terminal.process(&close_frame);

            terminal.set_size(512, WIDTH);
            terminal.set_scrollback(usize::MAX);
            let physical = terminal.screen().contents();
            for index in 0..32 {
                let sentinel = format!("overlay-history-{index:02}");
                assert_eq!(
                    physical.matches(&sentinel).count(),
                    1,
                    "{sentinel} replayed with synchronized_output={synchronized_output}:\n{physical}"
                );
            }
        }
    }

    #[test]
    fn long_exec_tail_and_head_trim_never_replay_native_history() {
        const WIDTH: u16 = 96;
        const HEIGHT: u16 = 24;
        for synchronized_output in [false, true] {
            let (mut shell, bytes) = emulated_shell_with_sync(
                crate::tui::theme::test_theme(),
                WIDTH,
                HEIGHT,
                synchronized_output,
            );
            let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 4_096);
            let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
                std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
            };
            terminal.process(&drain(&bytes));

            for index in 0..24 {
                shell.notice(format!("history-sentinel-{index:02}"));
            }
            shell.render();
            terminal.process(&drain(&bytes));

            let run_id = shell.begin_run("openai");
            let id = ToolCallId("long-exec".into());
            shell.on_run_event(
                run_id,
                &AgentEvent::ToolStarted {
                    id: id.clone(),
                    name: "exec".into(),
                    args: serde_json::json!({"command": "long-running-audit"}),
                },
            );
            shell.render();
            terminal.process(&drain(&bytes));

            // Default live exec presentation is a bounded five-row tail even
            // when the producer emits far more output than the viewport.
            let first = (0..32)
                .map(|index| format!("first-live-row-{index:03} {}\n", "a".repeat(80)))
                .collect::<String>();
            shell.on_run_event(
                run_id,
                &AgentEvent::ToolProgress {
                    id: id.clone(),
                    progress: ToolProgress::Output {
                        stream: ygg_agent::OutputStream::Stdout,
                        bytes: bytes::Bytes::from(first),
                    },
                },
            );
            shell.render();
            terminal.process(&drain(&bytes));

            let second = (0..128)
                .map(|index| format!("second-live-row-{index:03} {}\n", "b".repeat(80)))
                .collect::<String>();
            shell.on_run_event(
                run_id,
                &AgentEvent::ToolProgress {
                    id: id.clone(),
                    progress: ToolProgress::Output {
                        stream: ygg_agent::OutputStream::Stdout,
                        bytes: bytes::Bytes::from(second),
                    },
                },
            );
            shell.render();
            let growth = drain(&bytes);
            let growth_text = String::from_utf8_lossy(&growth);
            assert_eq!(
                growth_text.contains("\x1b[?2026h"),
                synchronized_output,
                "synchronized-output gating changed: {growth_text:?}"
            );
            assert!(
                growth_text.contains("second-live-row-127"),
                "latest exec tail was not painted: {growth_text:?}"
            );
            assert!(
                !growth_text.contains("first-live-row-000"),
                "{growth_text:?}"
            );
            assert!(
                !growth.contains(&b'\n'),
                "tail replacement must not scroll the bottom row: {growth_text:?}"
            );
            terminal.process(&growth);

            // Cross the 64 KiB panel bound so bounded_append replaces the
            // retained head. This mutates an already committed block far above
            // the viewport and must still repaint only final visible cells.
            let trimmed = (0..224)
                .map(|index| format!("trimmed-live-row-{index:03} {}\n", "c".repeat(300)))
                .collect::<String>();
            shell.on_run_event(
                run_id,
                &AgentEvent::ToolProgress {
                    id: id.clone(),
                    progress: ToolProgress::Output {
                        stream: ygg_agent::OutputStream::Stdout,
                        bytes: bytes::Bytes::from(trimmed),
                    },
                },
            );
            shell.render();
            let trimmed = drain(&bytes);
            let trimmed_text = String::from_utf8_lossy(&trimmed);
            assert!(
                trimmed_text.contains("trimmed-live-row-223"),
                "{trimmed_text:?}"
            );
            assert!(
                !trimmed_text.contains("long-running-audit"),
                "{trimmed_text:?}"
            );
            assert!(
                trimmed.len() < 16_384,
                "unbounded repaint: {} B",
                trimmed.len()
            );
            terminal.process(&trimmed);

            terminal.set_size(2_048, WIDTH);
            terminal.set_scrollback(usize::MAX);
            let physical = terminal.screen().contents();
            for index in 0..24 {
                let sentinel = format!("history-sentinel-{index:02}");
                assert_eq!(
                    physical.matches(&sentinel).count(),
                    1,
                    "{sentinel} was replayed with synchronized_output={synchronized_output}:\n{physical}"
                );
            }
        }
    }

    #[test]
    fn short_transcript_chrome_follows_content_without_viewport_padding() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 40);
        shell.set_identity("codex", "gpt-5.6", "high");
        let run_id = shell.begin_run("codex");
        let now = Instant::now();

        let composer_row = |lines: &[String]| {
            lines
                .iter()
                .position(|line| line.contains(CURSOR_MARKER))
                .expect("composer cursor row")
        };
        let initial = render_shell_at(&shell.state.borrow(), 80, now);
        let initial_composer = composer_row(&initial);
        assert!(
            initial.len() < 40,
            "native mode must not pad a short frame to the terminal height"
        );

        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: "I’ll inspect the tree.".into(),
            },
        );
        let streamed = render_shell_at(&shell.state.borrow(), 80, now);
        assert!(composer_row(&streamed) > initial_composer);
        assert!(streamed.len() < 40);

        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: ToolCallId("read-1".into()),
                name: "read".into(),
                args: serde_json::json!({"path": "src/main.rs"}),
            },
        );
        let tool = render_shell_at(&shell.state.borrow(), 80, now);
        assert!(composer_row(&tool) > composer_row(&streamed));
        assert!(tool.len() < 40);

        shell.queue_steering(&ComposedInput::from_text("also inspect tests".into()));
        let steering = render_shell_at(&shell.state.borrow(), 80, now);
        assert!(composer_row(&steering) > composer_row(&tool));
        assert!(steering.len() < 40);
        let steering_plain = steering
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(steering_plain.contains("Steering prompt · queued"));
        assert!(steering_plain.contains("    ↳ also inspect tests"));

        // The native-scrollback renderer retains committed transcript rows and
        // returns only the mutable suffix after the first frame.
        let mut frame = ShellFrameState::default();
        let first = render_shell_update(&shell.state.borrow(), 80, now, &mut frame);
        assert_eq!(first.stable_prefix, 0);
        assert_eq!(first.replacement, steering);
        assert!(!first.rebuild_scrollback);
        let next = render_shell_update(&shell.state.borrow(), 80, now, &mut frame);
        assert!(next.stable_prefix > 0);
        assert!(!next.rebuild_scrollback);
        assert!(next.stable_prefix + next.replacement.len() < 40);
        assert!(next
            .replacement
            .iter()
            .any(|line| line.contains(CURSOR_MARKER)));
    }

    #[test]
    fn emulated_native_short_frame_does_not_pin_composer_to_terminal_bottom() {
        const WIDTH: u16 = 80;
        const HEIGHT: u16 = 40;
        let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
        shell.set_identity("codex", "gpt-5.6", "high");
        shell.notice("recent transcript row");
        shell.render();

        let output = bytes.lock().unwrap().clone();
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 0);
        terminal.process(&output);
        assert!(
            terminal
                .screen()
                .contents()
                .contains("recent transcript row"),
            "transcript was not painted: {:?}",
            terminal.screen().contents()
        );
        let (cursor_row, _) = terminal.screen().cursor_position();
        assert!(
            cursor_row < HEIGHT / 2,
            "short native frame pinned the composer at terminal row {cursor_row}"
        );
    }

    #[test]
    fn slash_popup_height_changes_reanchor_native_viewport() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 20);
        for character in "/res".chars() {
            shell.apply_edit(EditAction::Char(character));
        }

        let mut frame = ShellFrameState::default();
        let initial = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert!(!initial.reanchor_viewport);

        for _ in 0..3 {
            shell.apply_edit(EditAction::Backspace);
        }
        assert_eq!(shell.pending(), "/");
        let expanded = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert!(
            expanded.reanchor_viewport,
            "growing suggestions before the composer must repaint the viewport"
        );

        for character in "res".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        let collapsed = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert!(
            collapsed.reanchor_viewport,
            "shrinking suggestions must erase the previously taller popup"
        );
    }

    #[test]
    fn native_scrollback_frame_exposes_committed_rows_and_reuses_stable_history() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        for number in 0..80 {
            shell.notice(format!("older {number}"));
        }
        shell.notice("live streamed result");

        let full = render_shell(&shell.state.borrow(), 80);
        let full_text = full.join("\n");
        assert!(full.len() > 14);
        assert!(full_text.contains("older 0"));
        assert!(full_text.contains("live streamed result"));

        let mut frame = ShellFrameState::default();
        let initial = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert_eq!(initial.stable_prefix, 0);
        assert!(!initial.rebuild_scrollback);
        let committed = frame.transcript_len;

        shell.notice("new native row");
        let appended = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert_eq!(appended.stable_prefix, committed);
        assert!(!appended.rebuild_scrollback);
        let appended_text = appended.replacement.join("\n");
        assert!(appended_text.contains("new native row"));
        assert!(!appended_text.contains("older 0"));
    }

    #[test]
    fn theme_swap_repaints_visible_cells_but_preserves_native_scrollback_styles() {
        const WIDTH: u16 = 32;
        const HEIGHT: u16 = 10;
        let theme_source = |name: &str, foreground: &str| {
            crate::tui::theme::test_theme_from_source(&format!(
                r##"
                    [metadata]
                    name = "{name}"

                    [colors]
                    foreground = "{foreground}"
                "##
            ))
        };
        let first_theme = theme_source("Viewport red", "#b01020");
        let old_foreground = role_rgb_color(&first_theme, "foreground");
        let (mut shell, bytes) = emulated_shell(first_theme, WIDTH, HEIGHT);
        {
            let mut state = shell.state.borrow_mut();
            for number in 0..12 {
                state.push_block(TranscriptBlock::Assistant(Box::new(
                    AssistantBlock::finalized(format!("historic-{number}")),
                )));
            }
        }
        shell.render();

        let before = bytes
            .lock()
            .expect("emulated terminal output mutex poisoned")
            .clone();
        let mut before_terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
        before_terminal.process(&before);
        let blank_row = before_terminal
            .screen()
            .rows(0, WIDTH)
            .enumerate()
            .find_map(|(row, contents)| contents.trim().is_empty().then_some(row as u16))
            .expect("fixture should leave a visible semantic separator row");

        // Put a cell into a row that is byte-identical across the two logical
        // frames. A changed-row diff would leave this corruption behind; the
        // required full visible repaint must erase it.
        bytes
            .lock()
            .expect("emulated terminal output mutex poisoned")
            .extend_from_slice(
                format!("\x1b[{};{}H\x1b[48;2;1;2;3mX\x1b[0m", blank_row + 1, WIDTH).as_bytes(),
            );

        let second_theme = theme_source("Viewport blue", "#2040c0");
        let new_foreground = role_rgb_color(&second_theme, "foreground");
        shell.set_theme(second_theme);
        shell.render();

        let complete = bytes
            .lock()
            .expect("emulated terminal output mutex poisoned")
            .clone();
        assert!(
            !complete
                .windows(b"\x1b[3J".len())
                .any(|window| window == b"\x1b[3J"),
            "theme swap cleared terminal-owned scrollback"
        );
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
        terminal.process(&complete);
        assert!(
            find_ascii_cell(terminal.screen(), "historic-").is_some(),
            "visible tail lost after theme repaint: {:?}",
            terminal.screen().contents()
        );
        assert_ascii_foreground(&terminal, "historic-11", new_foreground);
        assert!(
            find_ascii_cell(terminal.screen(), "X").is_none(),
            "full viewport repaint left a stale cell: {:?}",
            terminal.screen().contents()
        );

        let mut native_history = None;
        for offset in 1..=usize::from(HEIGHT) {
            terminal.set_scrollback(offset);
            for (row, contents) in terminal.screen().rows(0, WIDTH).enumerate() {
                let Some(column) = contents.find("historic-") else {
                    continue;
                };
                let color = terminal
                    .screen()
                    .cell(row as u16, column as u16)
                    .expect("historic cell inside terminal bounds")
                    .fgcolor();
                if color == old_foreground {
                    native_history = Some((row as u16, column as u16));
                    break;
                }
            }
            if native_history.is_some() {
                break;
            }
        }
        assert!(
            native_history.is_some(),
            "rows committed before the theme swap should retain their original cell style"
        );
    }

    #[test]
    fn application_viewport_theme_swap_repaints_without_clearing_shell_scrollback() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        let mut frame = ShellFrameState::default();
        let now = Instant::now();

        let initial = render_shell_viewport_update(&shell.state.borrow(), 80, now, &mut frame);
        assert!(!initial.reanchor_viewport);
        assert!(!initial.rebuild_scrollback);

        shell.set_theme(crate::tui::theme::test_theme_from_source(
            r##"
                [metadata]
                name = "Application viewport theme"

                [colors]
                foreground = "#2040c0"
            "##,
        ));
        let repainted = render_shell_viewport_update(&shell.state.borrow(), 80, now, &mut frame);
        assert!(repainted.reanchor_viewport);
        assert!(!repainted.rebuild_scrollback);
    }

    #[test]
    fn switching_back_to_default_clears_named_theme_attributes() {
        const WIDTH: u16 = 48;
        const HEIGHT: u16 = 10;
        let capabilities = crate::tui::terminal::TerminalCapabilities::test(
            true,
            true,
            crate::tui::terminal::ColorDepth::TrueColor,
        );
        let violet = crate::tui::theme::test_bundled_theme_with(
            "violet-hour",
            capabilities,
            crate::tui::theme::TerminalBackground::Unknown,
        );
        let (mut shell, bytes) = emulated_shell(violet, WIDTH, HEIGHT);
        shell
            .state
            .borrow_mut()
            .push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized("plain-default-prose".into()),
            )));
        shell.render();

        // Model a theme renderer ending a frame with every supported text
        // attribute active. Returning to default must reset the terminal's
        // rendition before it clears and repaints the visible viewport.
        bytes
            .lock()
            .expect("emulated terminal output mutex poisoned")
            .extend_from_slice(b"\x1b[1;3;4;7;48;2;12;34;56m");

        shell.set_theme(crate::tui::theme::test_theme());
        shell.render();

        let complete = bytes
            .lock()
            .expect("emulated terminal output mutex poisoned")
            .clone();
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
        terminal.process(&complete);
        assert_ascii_default_rendition(&terminal, "plain-default-prose");
    }

    #[test]
    fn new_session_shrink_requests_viewport_reanchoring_before_picker_growth() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        for number in 0..80 {
            shell.notice(format!("resumed history {number}"));
        }

        let mut frame = ShellFrameState::default();
        let resumed = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert!(!resumed.reanchor_viewport);
        assert!(!resumed.rebuild_scrollback);
        assert!(frame.transcript_len > 14);

        {
            let mut state = shell.state.borrow_mut();
            state.transcript.clear();
            state.block_revisions.clear();
            state.invalidate_transcript_layout();
            state.push_block(TranscriptBlock::Notice("new session created".into()));
        }
        let fresh = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert!(fresh.reanchor_viewport);
        assert!(!fresh.rebuild_scrollback);
        assert!(fresh.stable_prefix + fresh.replacement.len() < 14);

        shell.open_panel(Panel::SelectList {
            title: "Models".into(),
            items: vec!["model-a".into(), "model-b".into()],
            descriptions: vec![None, None],
            selected: 0,
            filter: String::new(),
            action: PanelAction::SelectModel(vec![
                ModelId("model-a".into()),
                ModelId("model-b".into()),
            ]),
        });
        let picker = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
        assert!(
            picker.reanchor_viewport,
            "inserting picker rows before the composer must reanchor the viewport"
        );
        assert!(!picker.rebuild_scrollback);
        assert!(picker.stable_prefix + picker.replacement.len() <= 14);
        assert!(picker
            .replacement
            .iter()
            .any(|line| line.contains("Models")));
        assert!(picker
            .replacement
            .iter()
            .any(|line| line.contains(CURSOR_MARKER)));
    }

    #[test]
    fn explicit_application_viewport_bounds_history_and_keeps_old_rows_reachable() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        for number in 0..80 {
            shell.notice(format!("older {number}"));
        }
        shell.notice("live streamed result");

        let live = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
        let live_text = live.join("\n");
        assert_eq!(live.len(), 14);
        assert!(!live_text.contains("older 0"));
        assert!(live_text.contains("older 79"));
        assert!(live_text.contains("live streamed result"));

        shell.scroll_lines(-10_000);
        let oldest = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
        let oldest_text = oldest.join("\n");
        assert_eq!(oldest.len(), 14);
        assert!(oldest_text.contains("older 0"), "{oldest_text}");
        assert!(oldest_text.contains("PageDown returns to live"));
        assert!(!oldest_text.contains("live streamed result"));

        shell.scroll_lines(10_000);
        let returned =
            render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now()).join("\n");
        assert!(returned.contains("live streamed result"));
        assert!(!returned.contains("PageDown returns to live"));

        shell.select_all_transcript();
        let copied = shell.copy_selected_plain_text().expect("semantic copy");
        assert!(copied.contains("older 0"));
        assert!(copied.contains("live streamed result"));
    }

    #[test]
    fn select_all_copy_is_semantic_and_excludes_pinned_chrome() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("openai", "gpt-test", "high");
        for number in 0..120 {
            shell.on_prompt_submitted(&format!("user {number}"));
            shell
                .state
                .borrow_mut()
                .push_block(TranscriptBlock::Assistant(Box::new(
                    AssistantBlock::finalized(format!(
                        "**assistant {number}**\n\n```rust\nlet n = {number};\n```"
                    )),
                )));
        }
        shell.select_all_transcript();
        let copied = shell.copy_selected_plain_text().expect("selection copy");
        assert!(copied.contains("user 0"));
        assert!(copied.contains("assistant 119"));
        assert!(copied.contains("let n = 119;"));
        assert!(!copied.contains(CURSOR_MARKER));
        assert!(!copied.contains("gpt-test"));
        assert!(!copied.contains("\x1b["));
        assert_eq!(shell.copy_buffer().as_deref(), Some(copied.as_str()));
    }

    #[test]
    fn drag_selection_autoscrolls_through_a_transcript_ten_viewports_tall() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        for number in 0..180 {
            shell.on_prompt_submitted(&format!("record {number}"));
        }
        // Establish the cached viewport before mapping mouse rows.
        let _ = render_shell(&shell.state.borrow(), 80);
        let available = shell_chrome(&shell.state.borrow(), 80, Instant::now()).transcript_rows;
        let bottom_row = transcript_viewport_capacity(available, false).saturating_sub(1) as u16;
        // Begin at the physical end of the newest row so the reverse drag
        // includes that complete semantic block as well as the oldest one.
        shell.begin_transcript_selection(bottom_row, 79, false);
        for _ in 0..240 {
            shell.extend_transcript_selection(0, 0);
        }
        shell.end_transcript_selection(0, 0);
        let copied = shell.copy_selected_plain_text().expect("drag copy");
        assert!(copied.contains("record 0"), "{copied}");
        assert!(copied.contains("record 179"), "{copied}");
        assert!(!copied.contains(CURSOR_MARKER));
    }

    #[test]
    fn dragging_into_pinned_chrome_clamps_to_last_semantic_transcript_row() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        for number in 0..40 {
            shell.on_prompt_submitted(&format!("record {number}"));
        }
        let _ = render_shell(&shell.state.borrow(), 80);
        let capacity = transcript_viewport_capacity_for_state(&shell.state.borrow(), 80);
        assert!(capacity > 1);

        shell.begin_transcript_selection(0, 0, false);
        shell.extend_transcript_selection(13, 0);

        let state = shell.state.borrow();
        let expected = InteractiveShell::transcript_position_at_screen_cell(
            &state,
            capacity.saturating_sub(1) as u16,
            0,
        )
        .expect("last transcript row");
        assert_eq!(
            state
                .transcript_selection
                .as_ref()
                .expect("drag selection")
                .focus,
            expected
        );
    }

    #[test]
    fn overscrolled_viewport_clamps_to_available_transcript() {
        let mut shell = InteractiveShell::test_shell();
        shell.on_prompt_submitted("visible prompt");
        shell.state.borrow().scroll_from_bottom.set(9_999);
        let rendered = render_shell(&shell.state.borrow(), 120);
        assert!(rendered.iter().any(|line| line.contains("visible prompt")));
        shell.scroll(1);
        assert_eq!(shell.state.borrow().scroll_from_bottom.get(), 0);
    }

    #[test]
    fn character_accurate_selection_maps_correct_columns() {
        for inset in [0_u16, 2, 4] {
            let mut shell = InteractiveShell::test_shell();
            shell.set_size(80, 14);
            shell.set_theme(theme_with_layout(&format!("transcript_inset = {inset}")));
            shell.on_prompt_submitted("hello world");

            // Establish the cached viewport. Physical columns include both
            // the theme transcript inset and the prompt's two-cell marker.
            let _ = render_shell(&shell.state.borrow(), 80);
            let start = inset + 3; // marker (2) + byte/cell index of 'e' (1)
            let end = start + 4;
            shell.begin_transcript_selection(0, start, false);
            shell.extend_transcript_selection(0, end);
            shell.end_transcript_selection(0, end);

            let copied = shell
                .copy_selected_plain_text()
                .expect("character drag copy");
            assert_eq!(copied, "ello", "transcript inset {inset}");
        }
    }

    fn rendered_phase(phase: RunPhase) -> String {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("relay", "gpt-5.6", "high");
        let now = Instant::now();
        {
            let mut state = shell.state.borrow_mut();
            let id = state.run.begin_at("relay", now).unwrap();
            state.run.set_phase_at(id, phase, now);
        }
        let rendered =
            render_shell_at(&shell.state.borrow(), 80, now + Duration::from_millis(600)).join("\n");
        rendered
    }

    #[test]
    fn renderer_covers_idle_and_every_active_run_phase() {
        let mut idle = InteractiveShell::test_shell();
        idle.set_identity("relay", "gpt-5.6", "high");
        let idle = render_shell(&idle.state.borrow(), 80).join("\n");
        assert!(idle.contains("GPT-5.6"), "{idle}");
        assert!(!idle.contains("relay / "));
        // No newline shortcut hint is shown in idle footer

        let cases = [
            (
                RunPhase::AwaitingProvider {
                    provider: "relay".into(),
                },
                Some("waiting for API"),
            ),
            (RunPhase::Thinking, None),
            (RunPhase::StreamingResponse, None),
            (RunPhase::PreparingToolCall, None),
            (
                RunPhase::RunningTool {
                    summary: "running tests".into(),
                },
                None,
            ),
            (
                RunPhase::AwaitingApproval {
                    prompt: "allow edit".into(),
                },
                Some("waiting"),
            ),
            (
                RunPhase::Preparing {
                    summary: "compacting".into(),
                },
                Some("compacting"),
            ),
        ];
        for (phase, expected) in cases {
            let rendered = rendered_phase(phase);
            if let Some(expected) = expected {
                assert!(
                    rendered.contains(expected),
                    "missing {expected:?}: {rendered}"
                );
            }
            assert!(!rendered.contains("0.6s"), "timer leaked: {rendered}");
            for hidden in ["thinking", "responding", "tool"] {
                assert!(!rendered.contains(hidden), "{hidden:?} leaked: {rendered}");
            }
        }
    }

    #[test]
    fn named_theme_may_retain_elapsed_only_active_work_status() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_theme(crate::tui::theme::test_bundled_theme_with(
            "bone-machine",
            crate::tui::terminal::TerminalCapabilities::test(
                true,
                true,
                crate::tui::terminal::ColorDepth::TrueColor,
            ),
            crate::tui::theme::TerminalBackground::Dark,
        ));
        let now = Instant::now();
        {
            let mut state = shell.state.borrow_mut();
            let id = state.run.begin_at("relay", now).unwrap();
            state.run.set_phase_at(id, RunPhase::Thinking, now);
        }
        let rendered =
            render_shell_at(&shell.state.borrow(), 80, now + Duration::from_millis(600)).join("\n");
        let status = rendered
            .lines()
            .find(|line| line.contains("0.6s"))
            .expect("active status");
        assert!(!status.contains("thinking"));
        assert!(!status.contains("tool"));
        assert!(status.contains("\x1b[38;2;"));
        assert!(!status.contains("\x1b[3m"));
        assert!(!status.contains("\x1b[2m"));
    }

    #[test]
    fn default_footer_accumulates_work_but_never_shows_the_stopwatch() {
        let shell = InteractiveShell::test_shell();
        let now = Instant::now();
        {
            let mut state = shell.state.borrow_mut();
            let first = state
                .run
                .begin_at("relay", now - Duration::from_secs(4))
                .unwrap();
            let outcome = state.run.interrupt_at(first, now).unwrap();
            InteractiveShell::append_outcome(&mut state, outcome);
            assert_eq!(state.session_work_elapsed, Duration::from_secs(4));

            let second = state.run.begin_at("relay", now).unwrap();
            state.run.set_phase_at(second, RunPhase::Thinking, now);
        }

        let active = strip_terminal_sequences(
            &crate::tui::composer_surface::render_composer_surface(
                &shell.state.borrow(),
                80,
                now + Duration::from_secs(2),
            )
            .join("\n"),
        );
        assert!(!active.contains("6.0s"), "{active}");

        {
            let mut state = shell.state.borrow_mut();
            let second = state.run.current_id().unwrap();
            let outcome = state
                .run
                .interrupt_at(second, now + Duration::from_secs(2))
                .unwrap();
            InteractiveShell::append_outcome(&mut state, outcome);
            assert_eq!(state.session_work_elapsed, Duration::from_secs(6));
        }
        let idle_later = strip_terminal_sequences(
            &crate::tui::composer_surface::render_composer_surface(
                &shell.state.borrow(),
                80,
                now + Duration::from_secs(32),
            )
            .join("\n"),
        );
        assert!(!idle_later.contains("6.0s"), "{idle_later}");
        assert!(!idle_later.contains("36.0s"), "{idle_later}");
    }

    #[test]
    fn renderer_covers_all_terminal_outcomes() {
        let theme = crate::tui::theme::test_theme();
        let summary = crate::presentation::RunSummary {
            files_changed: 2,
            tool_calls: 4,
            warnings: 0,
        };
        let outcomes = [
            (
                RunOutcome::Completed {
                    elapsed: Duration::from_millis(13700),
                    summary: summary.clone(),
                },
                "completed · 13.7s",
            ),
            (
                RunOutcome::CompletedWithWarnings {
                    elapsed: Duration::from_millis(18200),
                    warnings: 2,
                    summary: crate::presentation::RunSummary {
                        warnings: 2,
                        ..summary.clone()
                    },
                },
                "completed with 2 notes · 18.2s",
            ),
            (
                RunOutcome::Failed {
                    elapsed: Duration::from_millis(9400),
                    reason: "command exited 1".into(),
                },
                "failed",
            ),
            (
                RunOutcome::Interrupted {
                    elapsed: Duration::from_millis(6800),
                },
                "interrupted · 6.8s",
            ),
            (
                RunOutcome::NeedsInput {
                    prompt: "choose an implementation".into(),
                },
                "needs input",
            ),
            (
                RunOutcome::Cancelled {
                    elapsed: Duration::from_secs(1),
                },
                "interrupted · 1.0s",
            ),
        ];
        for (outcome, expected) in outcomes {
            let rendered = outcome_line(&outcome, &theme);
            assert!(rendered.contains(expected), "{rendered:?}");
            assert!(
                rendered.contains('✓')
                    || rendered.contains('◇')
                    || rendered.contains('×')
                    || rendered.contains('■')
            );
        }
    }

    #[test]
    fn tool_rendering_shows_intent_hides_protocol_and_contains_failures() {
        use ygg_agent::{ToolError, ToolOutput};

        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("openai");
        let id = ToolCallId("provider-call-secret".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: "exec".into(),
                args: serde_json::json!({
                    "command": "cargo test --workspace"
                }),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id,
                result: Ok(ToolOutput::new(
                    "exit=1 duration=0.2s\nstderr:\ntest result: FAILED. 76 passed; 2 failed",
                )),
            },
        );
        let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
        let plain = strip_terminal_sequences(&rendered);
        // General shell execution uses terminal semantics while preserving the
        // internal tool ID and full command for expanded evidence.
        assert!(plain.contains("$ cargo test --workspace"), "{plain:?}");
        assert!(plain.contains("command exited 1"), "{plain:?}");
        assert!(plain.contains("76 passed; 2 failed"));
        assert!(!rendered.contains("provider-call-secret"));
        // The command may be truncated in collapsed view; the full args are
        // visible when expanded (verified below).

        let (toggled, expanded) = shell
            .toggle_tool_details(Some("provider-call-secret"))
            .unwrap();
        assert_eq!(toggled, "provider-call-secret");
        assert!(expanded);
        let expanded_one = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(expanded_one.contains("provider-call-secret"));
        assert!(expanded_one.contains("--workspace"));
        assert!(
            !shell
                .toggle_tool_details(Some("provider-call-secret"))
                .unwrap()
                .1
        );

        shell.set_verbose_tools(true);
        let verbose = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(verbose.contains("provider-call-secret"));
        assert!(verbose.contains("--workspace"));

        let stale_id = ToolCallId("stale-edit-id".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: stale_id.clone(),
                name: "edit".into(),
                args: serde_json::json!({"path":"src/lib.rs"}),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id: stale_id,
                result: Err(ToolError::new(
                    "error stale_file\nsrc/lib.rs expected hash=aaa actual=bbb\nThe file changed",
                )),
            },
        );
        shell.set_verbose_tools(false);
        let default = render_shell(&shell.state.borrow(), 120).join("\n");
        assert!(default.contains("The file changed"));
        assert!(!default.contains("hash=aaa"));
        assert!(!default.contains("actual=bbb"));

        shell.set_verbose_tools(true);
        let verbose = render_shell(&shell.state.borrow(), 120).join("\n");
        assert!(verbose.contains("hash=aaa"));
        assert!(verbose.contains("actual=bbb"));
    }

    #[test]
    fn tool_metadata_ignores_file_content() {
        let panel = ToolPanel {
            id: ToolCallId("read".into()),
            name: "read".into(),
            args: String::new(),
            display: summarize_tool("read", &serde_json::json!({"path":"docs/design/ygg-ai.md"})),
            output:
                "docs/design/ygg-ai.md:1-2/2 hash=x\n2: `Model` is the value passed to the client"
                    .into(),
            finished: true,
            is_error: false,
            failure_reason: None,
            extension_render_segments: Vec::new(),
            model_lab: None,
            cached_diff: RefCell::new(None),
            cached_metadata: RefCell::new(None),
        };
        assert_eq!(tool_metadata(&panel), None);

        let exec = ToolPanel {
            name: "exec".into(),
            output: "exit=0 duration=0.2s\ntest result: ok. 1 passed".into(),
            cached_diff: RefCell::new(None),
            cached_metadata: RefCell::new(None),
            ..panel
        };
        assert_eq!(tool_metadata(&exec).as_deref(), Some("0.2s"));
    }

    #[test]
    fn responsive_header_drops_metadata_instead_of_truncating_every_field() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("relay", "gpt-5.6", "high");
        let wide = responsive_identity(&shell.state.borrow(), 120);
        assert!(wide.contains("relay / "));
        assert!(wide.contains("GPT-5.6"));
        assert!(wide.contains("high"));

        shell.set_identity(
            "custom-openai",
            "custom/Intel/Qwen3.6-27B-int4-AutoRound",
            "high",
        );
        let custom = strip_terminal_sequences(&responsive_identity(&shell.state.borrow(), 120));
        assert!(custom.contains("custom-openai / Qwen3.6 27B"), "{custom}");
        assert!(!custom.contains("custom/Intel"), "{custom}");

        shell.set_identity(
            "a-very-long-gateway-provider-name",
            "a-very-long-model-name-that-does-not-fit",
            "high",
        );
        let narrow = responsive_identity(&shell.state.borrow(), 40);
        assert!(visible_width(&narrow) <= 40);
        assert!(!narrow.contains("..."));
        assert!(!narrow.contains('…'));
        assert!(narrow.contains("ygg"));
    }

    #[test]
    fn status_metadata_uses_the_model_accent_but_no_color_stays_plain() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

        let mut theme = crate::tui::theme::test_theme();
        crate::tui::theme::apply_model_lab(&mut theme, crate::tui::theme::ModelLab::Anthropic);
        let styled = styled_status_text(
            &theme,
            "Provider       anthropic\nModel          claude\nReasoning      high\n\nSecurity model: trusted local agent",
        );
        assert!(styled.contains("38;2;169;99;76"), "{styled:?}");
        assert!(styled.contains("Model"));
        assert!(styled.contains("claude"));

        let mut plain = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::None,
        ));
        crate::tui::theme::apply_model_lab(&mut plain, crate::tui::theme::ModelLab::Anthropic);
        let plain = styled_status_text(&plain, "Model          claude");
        assert_eq!(plain, "Model          claude");
        assert!(!plain.contains('\x1b'));
    }

    #[test]
    fn ascii_plain_and_unicode_no_colour_keep_the_same_structure() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

        let ascii_theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            false,
            false,
            ColorDepth::None,
        ));
        let mut ascii = InteractiveShell::test_shell_with_theme(ascii_theme);
        ascii.set_identity("relay", "gpt-5.6", "off");
        ascii.on_prompt_submitted("fix it");
        {
            let mut state = ascii.state.borrow_mut();
            state.push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized("# Result\n\n- done".into()),
            )));
            state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                ToolCallId("id".into()),
                "edit".into(),
                "{}".into(),
                summarize_tool("edit", &serde_json::json!({"path":"src/lib.rs"})),
                String::new(),
                true,
                false,
                None,
                None,
            ))));
            state.push_block(TranscriptBlock::Outcome(RunOutcome::Completed {
                elapsed: Duration::from_secs(1),
                summary: crate::presentation::RunSummary {
                    files_changed: 1,
                    tool_calls: 1,
                    warnings: 0,
                },
            }));
        }
        ascii.set_size(40, 20);
        let ascii = render_shell(&ascii.state.borrow(), 40)
            .join("\n")
            .replace(CURSOR_MARKER, "");
        assert!(ascii.is_ascii(), "{ascii:?}");
        assert!(!ascii.contains('\x1b'));
        assert!(ascii.contains("> fix it"));
        assert!(ascii.contains("Result"));
        assert!(ascii.contains("* done"));
        assert!(ascii.contains("edit"));
        assert!(ascii.contains("updated lib.rs"));
        assert!(ascii.contains("completed - 1.0s"));
        assert!(!ascii.contains("ok completed"));

        let unicode_theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::None,
        ));
        let mut unicode = InteractiveShell::test_shell_with_theme(unicode_theme);
        unicode.on_prompt_submitted("fix it");
        let unicode = render_shell(&unicode.state.borrow(), 60)
            .join("\n")
            .replace(CURSOR_MARKER, "");
        assert!(unicode.contains("› fix it"));
        assert!(!unicode.contains('\x1b'));
    }

    #[test]
    fn narrow_tool_paths_use_basenames_and_wide_paths_remain_inspectable() {
        let theme = crate::tui::theme::test_theme();
        let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("id".into()),
            "edit".into(),
            serde_json::json!({"path":"crates/ygg-agent/src/session.rs"}).to_string(),
            summarize_tool(
                "edit",
                &serde_json::json!({"path":"crates/ygg-agent/src/session.rs"}),
            ),
            String::new(),
            true,
            false,
            None,
            None,
        )));
        let renderer = theme.rich_renderer();
        let narrow = render_block(None, &panel, &theme, &renderer, &renderer, 40, false).join("\n");
        let wide = render_block(None, &panel, &theme, &renderer, &renderer, 120, false).join("\n");
        assert!(narrow.contains("updated session.rs"));
        assert!(!narrow.contains("crates/ygg-agent"));
        assert!(wide.contains("updated crates/ygg-agent/src/session.rs"));
    }

    #[test]
    fn edit_status_prefix_does_not_hide_the_unified_diff() {
        let theme = crate::tui::theme::test_theme();
        let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("edit-diff".into()),
            "edit".into(),
            "{}".into(),
            summarize_tool("edit", &serde_json::json!({"path":"src/lib.rs"})),
            concat!(
                "ok modified=1\n",
                "src/lib.rs  +1 -1 hash=abc\n",
                "--- a/src/lib.rs\n",
                "+++ b/src/lib.rs\n",
                "@@ -1,1 +1,1 @@\n",
                "-old\n",
                "+new\n"
            )
            .into(),
            true,
            false,
            None,
            None,
        )));
        let renderer = theme.rich_renderer();
        let rendered =
            render_block(None, &panel, &theme, &renderer, &renderer, 100, false).join("\n");
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(rendered.contains("+new"), "{rendered}");
        assert!(!rendered.contains("hash=abc"), "{rendered}");
    }

    #[test]
    fn recognized_assistant_diffs_use_the_pretty_diff_renderer() {
        let theme = crate::tui::theme::test_theme();
        let assistant = AssistantBlock::finalized(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new".into(),
        );
        let rendered = assistant
            .render(&theme.rich_renderer(), &theme, 80)
            .join("\n");
        assert!(rendered.contains("@@ -1 +1 @@"));
        assert!(rendered.contains("-old"));
        assert!(rendered.contains("+new"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn fenced_diff_inside_markdown_does_not_hijack_the_whole_answer() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

        let markdown = concat!(
            "## Why this changed\n\n",
            "The cache remains authoritative.\n\n",
            "```diff\n",
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
            "```\n",
        );
        assert!(!looks_like_diff(markdown));
        assert!(looks_like_diff(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new"
        ));
        assert!(looks_like_diff(
            "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+fn main() {}"
        ));

        let theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::None,
        ));
        let rendered = AssistantBlock::finalized(markdown.to_owned())
            .render(&theme.rich_renderer(), &theme, 60)
            .join("\n");
        assert!(rendered.contains("Why this changed"), "{rendered}");
        assert!(
            rendered.contains("cache remains authoritative"),
            "{rendered}"
        );
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(!rendered.contains("```"), "{rendered}");
    }

    #[test]
    fn assistant_markdown_uses_full_rich_pipeline_without_rewriting_source() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

        let theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::None,
        ));
        let source = concat!(
            "# Result\n\n",
            "> Safe presentation projection\n\n",
            "- [x] CommonMark\n",
            "- [ ] cached source\n\n",
            "| Feature | State |\n| --- | --- |\n| tables | on |\n\n",
            "See [the docs](https://example.com/ygg).\n\n",
            "```rust\nlet complete_value = 12345678901234567890;\n```",
        );
        let assistant = AssistantBlock::finalized(source.to_owned());
        let renderer = theme.rich_renderer();
        let rendered = assistant.render(&renderer, &theme, 32).join("\n");

        // Rendering is a view over the exact provider/session payload. It may
        // add terminal structure, but it never normalizes the cached Markdown.
        assert_eq!(assistant.text, source);
        assert_eq!(assistant.markdown.raw_text(), source);
        assert_eq!(
            renderer.options().code_overflow,
            sexy_tui_rs::CodeOverflow::Wrap
        );
        assert!(renderer.options().syntax_highlighting);
        assert!(renderer.options().tables);
        assert!(renderer.options().code_borders);

        assert!(rendered.contains("Result"), "{rendered}");
        assert!(
            rendered.contains("Safe presentation projection"),
            "{rendered}"
        );
        assert!(rendered.contains("CommonMark"), "{rendered}");
        assert!(rendered.contains("Feature"), "{rendered}");
        assert!(rendered.contains("tables"), "{rendered}");
        assert!(rendered.contains("https://example.com/ygg"), "{rendered}");
        // The end of a long code row remains visible because transcript code
        // wraps instead of being irretrievably clipped.
        assert!(rendered.contains("67890"), "{rendered}");
        assert!(!rendered.contains("```"), "{rendered}");
        assert!(!rendered.contains("\x1b[48;"), "{rendered:?}");
    }

    #[test]
    fn verbose_reasoning_deltas_keep_complete_incremental_state() {
        let theme = crate::tui::theme::test_theme();
        let mut reasoning = AssistantBlock::streaming_reasoning("First complete thought.\n\n");
        let initial_revision = reasoning.markdown.tail_revision();

        for step in 0..256 {
            reasoning.append_reasoning(&format!("Thought {step} stays visible.\n\n"));
        }

        assert!(
            reasoning.markdown.tail_revision() >= initial_revision + 256,
            "ordinary deltas must extend one incremental Markdown stream"
        );
        reasoning.reasoning_expanded = true;
        let live = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(live.contains("First complete thought."), "{live}");
        assert!(live.contains("Thought 0 stays visible."), "{live}");
        assert!(live.contains("Thought 255 stays visible."), "{live}");

        reasoning.finish_reasoning();
        let finished = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(finished.contains("First complete thought."), "{finished}");
        assert!(finished.contains("Thought 0 stays visible."), "{finished}");
        assert!(
            finished.contains("Thought 255 stays visible."),
            "{finished}"
        );
    }

    #[test]
    fn streamed_reasoning_stays_two_stable_collapsed_rows_until_ctrl_o() {
        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("openai");
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: "first private sentinel".into(),
            },
        );
        let transcript = |shell: &InteractiveShell| {
            shell
                .state
                .borrow()
                .rendered_transcript(80)
                .iter()
                .map(|line| strip_terminal_sequences(line))
                .collect::<Vec<_>>()
        };
        let initial = transcript(&shell);
        assert_eq!(
            initial
                .iter()
                .filter(|line| line.contains("thinking"))
                .count(),
            1,
            "{initial:?}"
        );
        assert!(!initial.join("\n").contains("first private sentinel"));

        let continuation = (0..128)
            .map(|index| format!("\nprivate reasoning row {index}"))
            .collect::<String>();
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: continuation.clone(),
            },
        );
        let after_stream = transcript(&shell);
        assert_eq!(
            after_stream, initial,
            "hidden deltas changed transcript geometry"
        );
        {
            let state = shell.state.borrow();
            let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
                panic!("reasoning block expected");
            };
            assert_eq!(
                reasoning.text,
                format!("first private sentinel{continuation}")
            );
        }

        shell.expand_focused_tool();
        let expanded = transcript(&shell).join("\n");
        assert!(expanded.contains("first private sentinel"), "{expanded}");
        assert!(expanded.contains("private reasoning row 127"), "{expanded}");
        assert!(!expanded.contains("ctrl+o to expand"), "{expanded}");

        shell.expand_focused_tool();
        assert_eq!(transcript(&shell), initial);
    }

    #[test]
    fn collapsed_reasoning_shimmers_in_model_color_and_settles_with_duration() {
        let theme = crate::tui::theme::test_theme();
        let renderer = theme.reasoning_renderer();
        let mut reasoning =
            AssistantBlock::streaming_reasoning("private").with_model_lab(Some(ModelLab::Alibaba));
        reasoning.reasoning_animation_frame = 2;
        let live = render_reasoning(&reasoning, &renderer, &theme, 80, false);
        let plain_live = live
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert_eq!(plain_live, ["⠹ thinking", "└ ctrl+o to expand"]);
        assert!(live[0].contains("\x1b[38;2;"), "{live:?}");
        assert!(live[0].contains("\x1b[1m"), "{live:?}");
        assert!(live[0].contains("\x1b[22m"), "{live:?}");

        reasoning.reasoning_animation_frame += 1;
        let next = render_reasoning(&reasoning, &renderer, &theme, 80, false);
        assert_ne!(next[0], live[0], "shimmer frame must advance");

        reasoning.reasoning_elapsed = Some(Duration::from_millis(13_700));
        reasoning.finish_reasoning();
        let settled = render_reasoning(&reasoning, &renderer, &theme, 80, false);
        assert_eq!(strip_terminal_sequences(&settled[0]), "thought for 14s");
        assert_eq!(strip_terminal_sequences(&settled[1]), "└ ctrl+o to expand");
        assert!(settled[0].contains("\x1b[3m"), "{settled:?}");
        assert!(settled[0].contains("\x1b[38;2;"), "{settled:?}");
        assert!(!settled[0].contains("\x1b[1m"), "{settled:?}");
    }

    #[test]
    fn hydrated_reasoning_is_retained_but_collapsed_until_ctrl_o() {
        use ygg_ai::{AssistantMessage, AssistantPart, Message, ModelId, Protocol, ReasoningPart};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let source = "durable private thought\nwith a second line";
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Reasoning(ReasoningPart {
                    text: Some(source.into()),
                    state: None,
                })],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiResponses,
            })))
            .unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.hydrate(&session).unwrap();
        let render = |shell: &InteractiveShell| {
            shell
                .state
                .borrow()
                .rendered_transcript(80)
                .iter()
                .map(|line| strip_terminal_sequences(line))
                .collect::<Vec<_>>()
        };
        let collapsed = render(&shell);
        assert_eq!(collapsed.len(), 2, "{collapsed:?}");
        assert!(collapsed[0].contains("thought"), "{collapsed:?}");
        assert!(collapsed[1].contains("ctrl+o to expand"), "{collapsed:?}");
        assert!(!collapsed.join("\n").contains(source));
        let state = shell.state.borrow();
        let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
            panic!("hydrated reasoning block expected");
        };
        assert_eq!(reasoning.text, source);
        assert!(reasoning.finished);
        drop(state);

        shell.expand_focused_tool();
        let expanded = render(&shell).join("\n");
        assert!(expanded.contains("durable private thought"), "{expanded}");
        assert!(expanded.contains("with a second line"), "{expanded}");

        shell.expand_focused_tool();
        assert_eq!(render(&shell), collapsed);
    }

    #[test]
    fn completed_reasoning_uses_rich_markdown_without_raw_delimiters() {
        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("openai");
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: "**Planning validation**".into(),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: "**Inspecting `render.rs`**".into(),
            },
        );
        let collapsed = render_shell(&shell.state.borrow(), 80).join("\n");
        let collapsed_plain = strip_terminal_sequences(&collapsed);
        assert!(collapsed_plain.contains("thinking"), "{collapsed_plain}");
        assert!(
            !collapsed_plain.contains("Planning validation"),
            "{collapsed_plain}"
        );
        {
            let state = shell.state.borrow();
            let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
                panic!("first block must be reasoning Markdown");
            };
            assert!(reasoning.text.contains("****"));
            assert!(!reasoning.markdown.raw_text().contains("****"));
        }
        shell.expand_focused_tool();
        let live = render_shell(&shell.state.borrow(), 80).join("\n");
        assert!(live.contains("Planning validation"), "{live}");
        assert!(live.contains("Inspecting"), "{live}");
        assert!(!live.contains("**"), "{live}");
        assert!(!live.contains("`render.rs`"), "{live}");

        // A tool boundary finalizes both assistant and reasoning streams.
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: ToolCallId("read-1".into()),
                name: "read".into(),
                args: serde_json::json!({"path":"render.rs"}),
            },
        );

        let state = shell.state.borrow();
        let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
            panic!("first block must be reasoning Markdown");
        };
        assert!(reasoning.markdown.is_finished());
        let rendered = render_shell(&state, 80).join("\n");
        assert!(rendered.contains("Planning validation"), "{rendered}");
        assert!(rendered.contains("Inspecting"), "{rendered}");
        assert!(!rendered.contains("**"), "{rendered}");
        assert!(!rendered.contains("`render.rs`"), "{rendered}");
    }

    #[test]
    fn reasoning_is_subdued_without_losing_inline_code_colour() {
        let theme = crate::tui::theme::test_theme();
        let response = AssistantBlock::finalized("Answer with `Session`".into())
            .render(&theme.rich_renderer(), &theme, 80)
            .join("\n");
        let reasoning = AssistantBlock::finalized_reasoning("Thinking about `Session`".into())
            .render(&theme.reasoning_renderer(), &theme, 80)
            .join("\n");
        let prompt = render_block(
            None,
            &TranscriptBlock::User {
                text: "prompt".into(),
                model_lab: None,
                prompt_color: None,
                persisted: true,
            },
            &theme,
            &theme.rich_renderer(),
            &theme.reasoning_renderer(),
            80,
            false,
        )
        .into_iter()
        .next()
        .expect("prompt line");
        let reasoning_block = render_block(
            None,
            &TranscriptBlock::Reasoning(Box::new(AssistantBlock::finalized_reasoning(
                "Thinking about `Session`".into(),
            ))),
            &theme,
            &theme.rich_renderer(),
            &theme.reasoning_renderer(),
            80,
            false,
        )
        .into_iter()
        .next()
        .expect("thinking line");
        let reasoning_code = AssistantBlock::finalized_reasoning(
            "Thinking before code:\n\n```rust\nlet answer = 42;\n```".into(),
        )
        .render(&theme.reasoning_renderer(), &theme, 80)
        .join("\n");
        let linked_reasoning =
            AssistantBlock::finalized_reasoning("See [the docs](https://example.com)".into())
                .render(&theme.reasoning_renderer(), &theme, 80)
                .join("\n");
        let conservative_theme =
            crate::tui::theme::test_theme_with(crate::tui::terminal::TerminalCapabilities::test(
                true,
                true,
                crate::tui::terminal::ColorDepth::Ansi16,
            ));
        let conservative_reasoning = AssistantBlock::finalized_reasoning("thinking".into())
            .render(
                &conservative_theme.reasoning_renderer(),
                &conservative_theme,
                80,
            )
            .join("\n");
        let code_line = reasoning_code
            .lines()
            .find(|line| line.contains("answer"))
            .expect("thinking code line");

        assert!(
            response.starts_with("Answer"),
            "responses stay flush: {response:?}"
        );
        assert!(
            strip_terminal_sequences(&prompt).starts_with("  › prompt"),
            "prompts use the shared two-cell inset: {prompt:?}"
        );
        assert!(
            !prompt.contains("\x1b[48;"),
            "prompt identity should use only a restrained foreground marker"
        );
        assert!(
            prompt.contains("\x1b[38;2;"),
            "prompt needs readable text colour"
        );
        assert!(response.contains("Session"));
        assert!(
            response.contains("\x1b[38;2;"),
            "inline code should be coloured"
        );
        assert!(
            strip_terminal_sequences(&reasoning_block).starts_with("  thought"),
            "thinking shares the transcript inset: {reasoning_block:?}"
        );
        assert!(reasoning.contains("Session"));
        assert!(
            reasoning.contains("\x1b[38;2;"),
            "reasoning should use a muted foreground"
        );
        assert!(
            reasoning.contains("\x1b[3m"),
            "reasoning should be italicized"
        );
        assert!(
            !reasoning.contains("\x1b[2m"),
            "reasoning must not use SGR faint"
        );
        assert!(
            !code_line.contains("\x1b[3m"),
            "thinking code blocks must stay upright"
        );
        assert!(
            linked_reasoning.contains("\x1b]8;;https://example.com"),
            "thinking links retain native hyperlink support"
        );
        assert!(
            !conservative_reasoning.contains("\x1b[4m"),
            "unsupported italics must not degrade into underlines"
        );
        assert!(
            !response.contains("\x1b[2m"),
            "ordinary response prose must not inherit reasoning dim"
        );
    }

    #[test]
    fn prompt_row_keeps_exact_persisted_color_across_theme_changes() {
        let mut first_theme = crate::tui::theme::test_theme();
        crate::tui::theme::apply_model_lab(&mut first_theme, ModelLab::OpenAi);
        let block = TranscriptBlock::User {
            text: "safe\u{1b}[31m prompt".into(),
            model_lab: Some(ModelLab::OpenAi),
            prompt_color: Some("#123456".into()),
            persisted: true,
        };
        let first = render_block(
            None,
            &block,
            &first_theme,
            &first_theme.rich_renderer(),
            &first_theme.reasoning_renderer(),
            40,
            false,
        )
        .join("\n");

        let mut second_theme = crate::tui::theme::test_theme();
        crate::tui::theme::apply_model_lab(&mut second_theme, ModelLab::DeepSeek);
        let second = render_block(
            None,
            &block,
            &second_theme,
            &second_theme.rich_renderer(),
            &second_theme.reasoning_renderer(),
            40,
            false,
        )
        .join("\n");

        for rendered in [&first, &second] {
            assert!(
                rendered.contains("48;2;18;52;86m"),
                "persisted prompt background changed: {rendered:?}"
            );
            assert!(!rendered.contains("\x1b[31m"), "{rendered:?}");
            assert!(visible_width(rendered) <= 40, "{rendered:?}");
        }
    }

    #[test]
    fn persisted_prompt_background_fills_the_semantic_row_in_terminal_cells() {
        const WIDTH: u16 = 24;
        let theme = crate::tui::theme::test_theme();
        let block = TranscriptBlock::User {
            text: "first line  \nsecond line".into(),
            model_lab: Some(ModelLab::OpenAi),
            prompt_color: Some("#123456".into()),
            persisted: true,
        };
        let rendered = render_block(
            None,
            &block,
            &theme,
            &theme.rich_renderer(),
            &theme.reasoning_renderer(),
            WIDTH,
            false,
        );
        let terminal = emulate_rows(&rendered, WIDTH);
        let expected = vt100::Color::Rgb(0x12, 0x34, 0x56);
        let inset = theme.layout_for_width(WIDTH).transcript_inset;

        assert_eq!(rendered.len(), 2, "fixture should wrap to two prompt rows");
        for row in 0..rendered.len() as u16 {
            for column in 0..WIDTH {
                let background = terminal
                    .screen()
                    .cell(row, column)
                    .expect("prompt row cell inside terminal bounds")
                    .bgcolor();
                if column < inset {
                    assert_eq!(
                        background,
                        vt100::Color::Default,
                        "theme transcript inset must remain outside the prompt rectangle"
                    );
                } else {
                    assert_eq!(
                        background, expected,
                        "prompt background did not reach row {row}, column {column}"
                    );
                }
            }
        }

        const CARD_WIDTH: u16 = 80;
        let card_theme = crate::tui::theme::test_theme_from_source(SURFACE_TEST_THEME);
        let card_plan = compile_surface_plan(None, &block, &card_theme, CARD_WIDTH);
        assert_eq!(card_plan.chrome, ThemeSurfaceChrome::Card);
        let card_rendered = render_block(
            None,
            &block,
            &card_theme,
            &card_theme.rich_renderer(),
            &card_theme.reasoning_renderer(),
            CARD_WIDTH,
            false,
        );
        let card_terminal = emulate_rows(&card_rendered, CARD_WIDTH);
        let content_row =
            u16::try_from(card_plan.geometry.transition_rows + card_plan.geometry.leading_rows)
                .expect("card content row fits in terminal coordinates");
        let left_border = card_plan.frame_left;
        let right_border = card_plan
            .frame_left
            .saturating_add(card_plan.frame_width)
            .saturating_sub(1);

        assert_ne!(
            card_terminal
                .screen()
                .cell(content_row, left_border)
                .expect("card left border cell")
                .bgcolor(),
            expected,
            "structural card border must remain outside the prompt rectangle"
        );
        for column in left_border.saturating_add(1)..right_border {
            assert_eq!(
                card_terminal
                    .screen()
                    .cell(content_row, column)
                    .expect("card inner prompt cell")
                    .bgcolor(),
                expected,
                "card prompt background did not cover theme padding at column {column}"
            );
        }
        assert_ne!(
            card_terminal
                .screen()
                .cell(content_row, right_border)
                .expect("card right border cell")
                .bgcolor(),
            expected,
            "structural card border must remain outside the prompt rectangle"
        );
    }

    #[test]
    fn tool_lifecycle_styles_are_visible_in_terminal_cells() {
        let theme = crate::tui::theme::test_theme_from_source(
            r##"
                [metadata]
                name = "Tool lifecycle cells"

                [colors]
                foreground = "#f4f4f4"
                muted = "#686868"
                error = "#e43f4f"

                [roles."extension.live"]
                foreground = "#00ff00"
                bold = true
            "##,
        );
        let renderer = theme.rich_renderer();
        let muted = role_rgb_color(&theme, "muted");
        let foreground = role_rgb_color(&theme, "foreground");
        let error = role_rgb_color(&theme, "error");
        assert_ne!(muted, foreground);
        assert_ne!(error, foreground);

        let args = serde_json::json!({"path":"src/lib.rs"});
        let mut active_panel = ToolPanel::new(
            ToolCallId("active-read".into()),
            "read".into(),
            args.to_string(),
            summarize_tool("read", &args),
            "live raw evidence".into(),
            false,
            false,
            None,
            None,
        );
        active_panel.extension_render_segments =
            vec![ygg_agent::extension_process::ToolRenderSegment {
                text: "live output".into(),
                style_role: Some("extension.live".into()),
            }];
        let active = render_block(
            None,
            &TranscriptBlock::Tool(Box::new(active_panel)),
            &theme,
            &renderer,
            &renderer,
            80,
            true,
        );
        let active = emulate_rows(&active, 80);
        assert_ascii_foreground(&active, "read", muted);
        assert_ascii_bold(&active, "read");
        assert_ascii_foreground(&active, "src/lib.rs", muted);
        assert_ascii_foreground(&active, "live output", muted);
        assert_ascii_foreground(&active, "live raw evidence", muted);

        let completed = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("completed-read".into()),
            "read".into(),
            args.to_string(),
            summarize_tool("read", &args),
            String::new(),
            true,
            false,
            None,
            None,
        )));
        let completed = render_block(None, &completed, &theme, &renderer, &renderer, 80, false);
        let completed = emulate_rows(&completed, 80);
        assert_ascii_foreground(&completed, "read", foreground);
        assert_ascii_bold(&completed, "read");
        assert_ascii_foreground(&completed, "src/lib.rs", foreground);

        let failed = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("failed-read".into()),
            "read".into(),
            args.to_string(),
            summarize_tool("read", &args),
            "error\npermission denied".into(),
            true,
            true,
            Some("permission denied".into()),
            None,
        )));
        let failed = render_block(None, &failed, &theme, &renderer, &renderer, 80, false);
        let failed = emulate_rows(&failed, 80);
        assert_ascii_foreground(&failed, "read", error);
        assert_ascii_bold(&failed, "read");
        assert_ascii_foreground(&failed, "permission denied", error);

        let active_exec_args = serde_json::json!({"command":"echo active"});
        let active_exec = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("active-exec".into()),
            "exec".into(),
            active_exec_args.to_string(),
            summarize_tool("exec", &active_exec_args),
            "streaming output".into(),
            false,
            false,
            None,
            None,
        )));
        let active_exec = render_block(None, &active_exec, &theme, &renderer, &renderer, 80, true);
        let active_exec = emulate_rows(&active_exec, 80);
        assert_ascii_foreground(&active_exec, "echo active", muted);
        assert_ascii_foreground(&active_exec, "streaming output", muted);

        for (command, is_error, expected) in [
            ("echo complete", false, foreground),
            ("echo failed", true, error),
        ] {
            let args = serde_json::json!({"command":command});
            let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
                ToolCallId(command.into()),
                "exec".into(),
                args.to_string(),
                summarize_tool("exec", &args),
                String::new(),
                true,
                is_error,
                is_error.then(|| "exit 1".into()),
                None,
            )));
            let rendered = render_block(None, &panel, &theme, &renderer, &renderer, 80, false);
            let terminal = emulate_rows(&rendered, 80);
            assert_ascii_foreground(&terminal, command, expected);
        }
    }

    #[test]
    fn tool_summaries_do_not_repeat_the_action_label() {
        assert_eq!(
            without_redundant_tool_lead("read", "read /tmp/src/lib.rs"),
            "/tmp/src/lib.rs"
        );
        assert_eq!(
            without_redundant_tool_lead("search", "searched src for pattern"),
            "src for pattern"
        );
        assert_eq!(
            without_redundant_tool_lead("exec", "running cargo test --workspace"),
            "cargo test --workspace"
        );
        assert_eq!(
            without_redundant_tool_lead("edit", "updated src/lib.rs"),
            "updated src/lib.rs"
        );
        assert_eq!(
            without_redundant_tool_lead("write", "wrote src/lib.rs"),
            "src/lib.rs"
        );
    }

    #[test]
    fn wrapped_tool_summaries_keep_their_action_indent() {
        let theme = crate::tui::theme::test_theme();
        let args = serde_json::json!({
            "path": "crates/ygg-coding-agent/src/tui/view.rs",
            "query": "a-very-long-search-query-that-must-wrap-without-losing-the-tool-label"
        });
        let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("id".into()),
            "search".into(),
            args.to_string(),
            summarize_tool("search", &args),
            String::new(),
            false,
            false,
            None,
            None,
        )));
        let renderer = theme.rich_renderer();
        let lines = render_block(None, &panel, &theme, &renderer, &renderer, 80, false)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();

        assert!(lines.len() > 1, "the long summary should wrap: {lines:?}");
        assert!(lines[0].starts_with("  search"), "{lines:?}");
        assert!(
            lines[1..]
                .iter()
                .filter(|line| !line.is_empty())
                .all(|line| line.starts_with("          ")),
            "continuations must hang under the summary column: {lines:?}"
        );
        assert!(lines.last().is_some_and(|line| !line.is_empty()));
    }

    #[test]
    fn default_rich_markdown_never_paints_selection_like_backgrounds() {
        let theme = crate::tui::theme::test_theme();
        let source = concat!(
            "Use `Session` without a painted chip.\n\n",
            "```text\nclone/read-only projection\nterminal rows\n```"
        );
        let rendered =
            AssistantBlock::finalized(source.into()).render(&theme.rich_renderer(), &theme, 160);
        let joined = rendered.join("\n");
        assert!(!joined.contains("\x1b[48;"), "{joined:?}");
        assert!(!joined.contains("```"), "{joined}");
        let widths = rendered
            .iter()
            .filter(|line| line.contains('┌') || line.contains('│') || line.contains('└'))
            .map(|line| visible_width(line))
            .collect::<Vec<_>>();
        assert!(!widths.is_empty(), "{rendered:?}");
        assert!(widths.iter().all(|width| *width < 80), "{widths:?}");
    }

    #[test]
    fn streamed_markdown_settles_into_rich_structure() {
        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("openai");
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: "## Session recovery\n\n**Changes**\n- preserves ".into(),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: "valid records\n- removes invalid bytes".into(),
            },
        );
        // Finalization performs the authoritative CommonMark parse. The live
        // suffix remains deliberately literal until its boundary is proven.
        shell.state.borrow_mut().close_streaming_blocks();
        {
            let state = shell.state.borrow();
            let TranscriptBlock::Assistant(assistant) = &state.transcript[0] else {
                panic!("first block must be assistant Markdown");
            };
            assert!(assistant.markdown.is_finished());
            assert_eq!(
                assistant.markdown.committed(),
                &sexy_tui_rs::parse_markdown(&assistant.text)
            );
        }
        let rendered = render_shell(&shell.state.borrow(), 60).join("\n");
        for raw in ["##", "**", "- preserves"] {
            assert!(!rendered.contains(raw), "raw marker leaked: {rendered}");
        }
        assert!(rendered.contains("Session recovery"));
        assert!(rendered.contains('•'));
    }

    #[test]
    fn truecolour_renderer_uses_only_foregrounds_for_identity() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
        let theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::TrueColor,
        ));
        let mut shell = InteractiveShell::test_shell_with_theme(theme);
        shell.set_identity("anthropic", "claude-sonnet-4", "high");
        let rendered = render_shell(&shell.state.borrow(), 120).join("\n");
        assert!(rendered.contains("38;2;"));
        // The composer top border uses foreground colour only; no background
        // colour escapes (48;2; or 48;5;) should leak.
        assert!(!rendered.contains("\x1b[48;"));
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
                turn_usage: Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    total_tokens: 15,
                    ..Usage::default()
                },
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    total_tokens: 15,
                    ..Usage::default()
                },
                session_cost_microdollars: Some(4200),
                run_cost_microdollars: 4200,
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

    #[test]
    fn active_exec_renders_command_immediately_and_refreshes_bounded_live_tail() {
        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("openai");
        let id = ToolCallId("live-exec".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: "exec".into(),
                args: serde_json::json!({"command": "long-running-check"}),
            },
        );

        let render = |shell: &InteractiveShell| {
            strip_terminal_sequences(&render_shell(&shell.state.borrow(), 100).join("\n"))
        };
        let started = render(&shell);
        assert!(started.contains("$ long-running-check"), "{started}");

        shell.on_run_event(
            run_id,
            &AgentEvent::ToolProgress {
                id: id.clone(),
                progress: ToolProgress::Output {
                    stream: ygg_agent::OutputStream::Stdout,
                    bytes: bytes::Bytes::from_static(
                        b"live-1\nlive-2\nlive-3\nlive-4\nlive-5\nlive-6\n",
                    ),
                },
            },
        );
        let streamed = render(&shell);
        assert!(streamed.contains("live-6"), "{streamed}");
        assert!(streamed.contains("1 earlier line"), "{streamed}");
        assert!(!streamed.contains("  live-1\n"), "{streamed}");

        shell.on_run_event(
            run_id,
            &AgentEvent::ToolProgress {
                id: id.clone(),
                progress: ToolProgress::Status("waiting for child".into()),
            },
        );
        let status = render(&shell);
        assert!(status.contains("waiting for child"), "{status}");
        let state = shell.state.borrow();
        let TranscriptBlock::Tool(panel) = &state.transcript[0] else {
            panic!("tool panel expected");
        };
        assert!(
            !panel.finished,
            "regression must prove pre-finish rendering"
        );
    }

    #[test]
    fn footer_collapses_semantically_and_keeps_one_adjacent_row() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity(
            "custom-openai",
            "custom/unsloth/Qwen3.6-35B-A3B-MTP-GGUF",
            "high",
        );
        {
            let mut state = shell.state.borrow_mut();
            state.last_turn_usage = Some(Usage {
                input_tokens: 26_800,
                output_tokens: 422,
                total_tokens: 27_222,
                ..Usage::default()
            });
            state.last_turn_tokens_per_second = Some(41.9);
            state.context_estimate = Some((5_600, 246_000));
            state.price_display = PriceDisplay::ExplicitZero;
            state.show_turn_cost = true;
            state.telemetry_model = Some(state.model.clone());
        }
        let now = Instant::now();
        assert_eq!(
            plain_footer(&shell, 100, now),
            "  Qwen3.6 35B A3B · high   5.6k/246k   ↑26.8k ↓422   41.9 tok/s   $0"
        );
        assert_eq!(
            plain_footer(&shell, 68, now),
            "  Qwen3.6 35B A3B   5.6k/246k   ↑26.8k ↓422   41.9 tok/s   $0"
        );
        assert_eq!(
            plain_footer(&shell, 44, now),
            "  Qwen3.6 35B A3B   41.9 tok/s   $0"
        );
        assert_eq!(plain_footer(&shell, 30, now), "  Qwen3.6  41.9 tok/s  $0");

        let surface = plain_composer_surface(&shell, 100, now);
        assert_eq!(surface.len(), 4, "one editor row, two borders, one footer");
        assert!(!surface[surface.len() - 2].is_empty());
        assert_eq!(surface.last().unwrap(), &plain_footer(&shell, 100, now));
        assert!(surface.iter().all(|line| visible_width(line) <= 100));
    }

    #[test]
    fn footer_omits_unknown_cost_and_shows_live_throughput_with_active_status() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("openai", "gpt-5.6", "high");
        let started = Instant::now();
        {
            let mut state = shell.state.borrow_mut();
            let id = state.run.begin_at("codex", started).unwrap();
            state.run_model = Some(state.model.clone());
            state.telemetry_model = state.run_model.clone();
            state.run_model_display = Some(state.model_display.clone());
            state.run_model_compact_names = state.model_compact_names.clone();
            state.run_reasoning = Some(state.reasoning.clone());
            state.run_price_display = Some(PriceDisplay::Unknown);
            state.run_context_estimate = Some((21_000, 256_000));
            state.show_turn_cost = true;
            state.run.set_phase_at(
                id,
                RunPhase::AwaitingProvider {
                    provider: "codex".into(),
                },
                started,
            );
            state.turn_generation_started_at = Some(started);
            state.turn_streamed_output_bytes = 2_520;
            state.context_estimate = Some((21_000, 256_000));
            state.price_display = PriceDisplay::Unknown;
            state.run_cost_available = false;
        }
        let now = started + Duration::from_millis(8_700);
        let live = plain_footer(&shell, 100, now);
        assert!(live.ends_with("waiting for API"), "{live:?}");
        assert!(!live.contains("8.7s"), "default timer leaked: {live:?}");
        assert!(
            live.contains("~72.4 tok/s"),
            "live estimate missing: {live:?}"
        );
        assert!(
            live.contains("~↓630"),
            "live output estimate missing: {live:?}"
        );
        assert!(
            live.contains("~21.6k/256k"),
            "live context missing: {live:?}"
        );
        assert!(
            !live.contains("cost"),
            "unknown price stays quiet: {live:?}"
        );
        assert!(!live.contains('—'), "unknown price stays quiet: {live:?}");
        assert!(
            !live.contains("esc"),
            "implicit controls stay out: {live:?}"
        );
        assert_eq!(visible_width(&live), 98, "status ends at the right inset");
        let live_diagnostics = status_telemetry(&shell.state.borrow(), now);
        assert!(live_diagnostics.contains("awaiting turn completion"));
        assert!(!live_diagnostics.contains("tok/s"));

        {
            let mut state = shell.state.borrow_mut();
            state.price_display = PriceDisplay::Priced;
            state.run_price_display = Some(PriceDisplay::Priced);
            state.run_cost_available = true;
            state.run_cost_microdollars = 82_000;
            state.session_cost_microdollars = Some(120_000);
        }
        let paid = plain_footer(&shell, 100, now);
        assert!(
            paid.contains("~$0.082"),
            "turn cost should be visible: {paid:?}"
        );
        assert!(
            !paid.contains("session"),
            "session cost stays in /status: {paid:?}"
        );
        assert!(paid.ends_with("waiting for API"));
        assert!(!paid.contains("8.7s"), "default timer leaked: {paid:?}");

        {
            let mut state = shell.state.borrow_mut();
            state.turn_generation_started_at = None;
            state.turn_streamed_output_bytes = 0;
            state.last_turn_tokens_per_second = Some(72.4);
            state.last_turn_generation_elapsed = Some(Duration::from_secs(2));
            state.last_turn_generated_tokens = Some(145);
            let id = state.run.current_id().unwrap();
            state.run.set_phase_at(
                id,
                RunPhase::RunningTool {
                    summary: "running tests".into(),
                },
                started,
            );
        }
        let active_sample = plain_footer(&shell, 100, now);
        assert!(
            active_sample.contains("72.4 tok/s"),
            "provider-final throughput should remain visible while tools run: {active_sample:?}"
        );
        assert!(
            !active_sample.contains("8.7s"),
            "default timer leaked: {active_sample:?}"
        );
        assert!(!active_sample.contains("tool"));
        let final_diagnostics = status_telemetry(&shell.state.borrow(), now);
        assert!(final_diagnostics.contains("72.4 tok/s final"));

        {
            let mut state = shell.state.borrow_mut();
            let id = state.run.current_id().unwrap();
            state.run.interrupt_at(id, now);
        }
        let completed_sample = plain_footer(&shell, 100, now);
        assert!(
            completed_sample.contains("72.4 tok/s"),
            "final metrics should appear after the whole run settles: {completed_sample:?}"
        );
        assert!(!completed_sample.contains('~'), "{completed_sample:?}");
    }

    #[test]
    fn footer_distinguishes_explicit_zero_from_unavailable_pricing() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("local", "qwen3.6-35b-a3b", "high");
        let now = Instant::now();
        shell.state.borrow_mut().show_turn_cost = true;

        shell.state.borrow_mut().price_display = PriceDisplay::Unknown;
        let unknown = plain_footer(&shell, 80, now);
        assert!(!unknown.contains('$'));
        assert!(!unknown.contains("cost"));

        {
            let mut state = shell.state.borrow_mut();
            state.price_display = PriceDisplay::Priced;
            state.run_cost_available = true;
            state.run_cost_microdollars = 0;
        }
        let not_yet_charged = plain_footer(&shell, 80, now);
        assert!(!not_yet_charged.contains('$'));

        shell.state.borrow_mut().price_display = PriceDisplay::ExplicitZero;
        let free = plain_footer(&shell, 80, now);
        assert!(free.ends_with("$0"), "{free:?}");

        for width in 1..=120 {
            let surface = plain_composer_surface(&shell, width, now);
            assert!(surface
                .iter()
                .all(|line| visible_width(line) <= usize::from(width)));
        }
    }

    #[test]
    fn idle_footer_shows_cache_but_keeps_session_cost_in_status() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("openai", "gpt-5.6-luna", "high");
        {
            let mut state = shell.state.borrow_mut();
            state.price_display = PriceDisplay::Priced;
            state.session_cost_microdollars = Some(91_400);
            state.cache_hit_rate_basis_points = Some(9_240);
            state.context_estimate = Some((102, 272_000));
            state.telemetry_model = Some(state.model.clone());
        }

        let footer = plain_footer(&shell, 120, Instant::now());
        assert!(footer.contains("102/272k"), "{footer:?}");
        assert!(footer.contains("cache 92.4%"), "{footer:?}");
        assert!(!footer.contains("session"), "{footer:?}");
        assert!(!footer.contains('$'), "no completed turn cost: {footer:?}");
        assert!(!footer.contains('~'), "{footer:?}");
    }

    #[test]
    fn semantic_transcript_blocks_have_uniform_transition_spacing() {
        let theme = crate::tui::theme::test_theme();
        let rich_renderer = theme.rich_renderer();
        let reasoning_renderer = theme.reasoning_renderer();
        let transcript = (0..12)
            .map(|step| {
                let mut reasoning = AssistantBlock::finalized_reasoning(format!("Step {step}"));
                reasoning.reasoning_expanded = true;
                TranscriptBlock::Reasoning(Box::new(reasoning))
            })
            .collect::<Vec<_>>();

        let mut visible = Vec::new();
        for (index, block) in transcript.iter().enumerate() {
            visible.extend(render_block(
                index.checked_sub(1).and_then(|index| transcript.get(index)),
                block,
                &theme,
                &rich_renderer,
                &reasoning_renderer,
                80,
                false,
            ));
        }
        let plain = visible
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert_eq!(
            plain.iter().filter(|line| line.contains("Step ")).count(),
            12
        );
        assert!(plain.iter().any(|line| line.contains("Step 0")));
        assert!(!plain.iter().any(|line| line.contains("earlier analysis")));
        assert_eq!(plain.iter().filter(|line| line.is_empty()).count(), 11);
        for step in 1..12 {
            let label = format!("Step {step}");
            let index = plain
                .iter()
                .position(|line| line.contains(&label))
                .expect("every reasoning block is rendered");
            assert_eq!(
                plain.get(index.wrapping_sub(1)).map(String::as_str),
                Some("")
            );
            assert!(index < 2 || !plain[index - 2].is_empty());
        }

        let mut verbose_reasoning = AssistantBlock::finalized_reasoning(
            "First complete thought.\n\nSecond complete thought.".into(),
        );
        verbose_reasoning.reasoning_expanded = true;
        let verbose = TranscriptBlock::Reasoning(Box::new(verbose_reasoning));
        let verbose = render_block(
            None,
            &verbose,
            &theme,
            &rich_renderer,
            &reasoning_renderer,
            80,
            false,
        )
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
        assert!(verbose.contains("First complete thought."), "{verbose}");
        assert!(verbose.contains("Second complete thought."), "{verbose}");

        let tool = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("read-compact".into()),
            "read".into(),
            serde_json::json!({"path":"src/lib.rs"}).to_string(),
            summarize_tool("read", &serde_json::json!({"path":"src/lib.rs"})),
            String::new(),
            false,
            false,
            None,
            None,
        )));
        let transition = render_block(
            transcript.last(),
            &tool,
            &theme,
            &rich_renderer,
            &reasoning_renderer,
            80,
            false,
        );
        assert_eq!(transition.first().map(String::as_str), Some(""));
        assert!(transition.get(1).is_some_and(|line| !line.is_empty()));
    }

    #[test]
    fn consecutive_tool_calls_have_one_breathing_row_between_them() {
        let theme = crate::tui::theme::test_theme();
        let renderer = theme.rich_renderer();
        let tool = |id: &str, name: &str, args: serde_json::Value| {
            TranscriptBlock::Tool(Box::new(ToolPanel::new(
                ToolCallId(id.into()),
                name.into(),
                args.to_string(),
                summarize_tool(name, &args),
                String::new(),
                true,
                false,
                None,
                None,
            )))
        };
        let tools = [
            tool("read", "read", serde_json::json!({"path":"src/lib.rs"})),
            tool(
                "exec",
                "exec",
                serde_json::json!({"command":"cargo test -p ygg-coding-agent"}),
            ),
            tool("edit", "edit", serde_json::json!({"path":"src/lib.rs"})),
        ];

        for (index, block) in tools.iter().enumerate() {
            let rendered = render_block(
                index
                    .checked_sub(1)
                    .and_then(|previous| tools.get(previous)),
                block,
                &theme,
                &renderer,
                &renderer,
                80,
                false,
            );
            if index == 0 {
                assert!(rendered.first().is_some_and(|line| !line.is_empty()));
            } else {
                assert_eq!(rendered.first().map(String::as_str), Some(""));
                assert!(rendered.get(1).is_some_and(|line| !line.is_empty()));
                assert!(rendered.get(2).is_none_or(|line| !line.is_empty()));
            }
        }
    }

    #[test]
    fn shell_tool_rows_wrap_without_exec_labels_or_internal_blank_rows() {
        let theme = crate::tui::theme::test_theme();
        let args = serde_json::json!({
            "command": "cargo test --workspace --all-targets --all-features"
        });
        let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("exec-wrap".into()),
            "exec".into(),
            args.to_string(),
            summarize_tool("exec", &args),
            "exit=0 duration=0.2s\ntest result: ok. 12 passed".into(),
            true,
            false,
            None,
            Some(ModelLab::OpenAi),
        )));
        let renderer = theme.rich_renderer();
        let lines = render_block(None, &panel, &theme, &renderer, &renderer, 32, false)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        let joined = lines.join("\n");

        assert!(joined.contains("$ cargo test"), "{joined:?}");
        assert!(joined.contains("0.2s"), "{joined:?}");
        assert!(!joined.to_ascii_lowercase().contains("exec"), "{joined:?}");
        assert!(lines.iter().all(|line| !line.is_empty()));
        assert!(lines.iter().all(|line| visible_width(line) <= 32));
    }

    #[test]
    fn compact_exec_rows_show_a_muted_five_line_tail_under_a_bold_command() {
        let theme = crate::tui::theme::test_theme();
        let args = serde_json::json!({
            "command": "ssh host journalctl -k -b",
            "timeout_ms": 60_000
        });
        let output_lines = (1..=9)
            .map(|line| format!("kernel output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let panel = ToolPanel::new(
            ToolCallId("exec-tail".into()),
            "exec".into(),
            args.to_string(),
            summarize_tool("exec", &args),
            format!(
                "exit=0 duration=1.0s\nstdout: 9 lines\n{output_lines}\ntruncated_stdout=false"
            ),
            true,
            false,
            None,
            Some(ModelLab::OpenAi),
        );

        let compact = compact_exec_output(&panel);
        assert_eq!(compact.omitted_lines, 4);
        assert_eq!(
            compact.lines,
            (5..=9)
                .map(|line| format!("kernel output {line}"))
                .collect::<Vec<_>>()
        );

        let renderer = theme.rich_renderer();
        let rendered = render_block(
            None,
            &TranscriptBlock::Tool(Box::new(panel)),
            &theme,
            &renderer,
            &renderer,
            80,
            false,
        );
        let command = rendered
            .iter()
            .find(|line| strip_terminal_sequences(line).contains("ssh host"))
            .expect("command row");
        assert!(
            command.contains("\x1b[1m"),
            "command should be bold: {command:?}"
        );
        let output = rendered
            .iter()
            .find(|line| strip_terminal_sequences(line).contains("kernel output 9"))
            .expect("tail output row");
        assert!(
            !output.contains("\x1b[1m"),
            "output must never inherit bold: {output:?}"
        );
        let duration = rendered
            .iter()
            .find(|line| strip_terminal_sequences(line).contains("Took 1.0s"))
            .expect("duration row");
        assert!(
            !duration.contains("\x1b[1m"),
            "duration must remain understated: {duration:?}"
        );
        let plain = rendered
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("… (4 earlier lines, ctrl+o to expand)"),
            "{plain}"
        );
        assert!(!plain.contains("kernel output 4"), "{plain}");
        assert!(plain.contains("kernel output 5"), "{plain}");
        assert!(plain.contains("(timeout 60s)"), "{plain}");
        assert!(plain.find("kernel output 9") < plain.find("Took 1.0s"));
    }

    #[test]
    fn compact_exec_rows_distinguish_capture_loss_from_expandable_ui_elision() {
        let theme = crate::tui::theme::test_theme();
        let args = serde_json::json!({"command": "produce-huge-output"});
        let panel = ToolPanel::new(
            ToolCallId("exec-capture-truncated".into()),
            "exec".into(),
            args.to_string(),
            summarize_tool("exec", &args),
            "exit=0 duration=0.8s\nstdout: 4096 bytes, showing first 2 and last 2 lines\nhead one\nhead two\n...\ntail one\ntail two\ntruncated_stdout=head:2 tail:2 omitted_bytes:3000".into(),
            true,
            false,
            None,
            None,
        );
        let compact = compact_exec_output(&panel);
        assert_eq!(compact.omitted_lines, 0);
        assert_eq!(
            compact.lines,
            ["head one", "head two", "tail one", "tail two"]
        );
        assert_eq!(
            compact.capture_truncations,
            [ExecCaptureTruncation {
                stream: "stdout",
                omitted_bytes: Some(3000),
            }]
        );

        let renderer = theme.rich_renderer();
        let plain = render_block(
            None,
            &TranscriptBlock::Tool(Box::new(panel)),
            &theme,
            &renderer,
            &renderer,
            80,
            false,
        )
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
        assert!(
            plain.contains("stdout capture omitted 3000 bytes; unavailable to expand"),
            "{plain}"
        );
        assert!(!plain.contains("ctrl+o"), "nothing UI-hidden: {plain}");
    }

    #[test]
    fn compact_exec_hides_current_and_legacy_complete_capture_footers() {
        let theme = crate::tui::theme::test_theme();
        let renderer = theme.rich_renderer();
        for footer in [
            "complete_stdout=true",
            "complete_stderr=true",
            "truncated_stdout=false",
            "truncated_stderr=false",
        ] {
            let args = serde_json::json!({"command": "printf ready"});
            let panel = ToolPanel::new(
                ToolCallId(format!("exec-{footer}")),
                "exec".into(),
                args.to_string(),
                summarize_tool("exec", &args),
                format!("exit=0 duration=0.1s\nstdout: 1 lines\nready\n{footer}"),
                true,
                false,
                None,
                None,
            );
            let compact = compact_exec_output(&panel);
            assert_eq!(compact.lines, ["ready"], "{footer}");
            assert!(compact.capture_truncations.is_empty(), "{footer}");
            let plain = render_block(
                None,
                &TranscriptBlock::Tool(Box::new(panel)),
                &theme,
                &renderer,
                &renderer,
                80,
                false,
            )
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n");
            assert!(plain.contains("ready"), "{plain}");
            assert!(!plain.contains(footer), "{plain}");
        }
    }

    #[test]
    fn ctrl_o_roundtrip_expands_exec_evidence_without_mutating_or_restyling_output() {
        use ygg_agent::ToolOutput;

        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("local");
        let id = ToolCallId("exec-roundtrip".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: "exec".into(),
                args: serde_json::json!({"command": "printf '\\033[31mline\\033[0m\\n'"}),
            },
        );
        let body = (1..=8)
            .map(|line| format!("\x1b[31mresult {line}\x1b[0m"))
            .collect::<Vec<_>>()
            .join("\n");
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id: id.clone(),
                result: Ok(ToolOutput::new(format!(
                    "exit=0 duration=0.1s\nstdout: 8 lines\n{body}\ntruncated_stdout=false"
                ))),
            },
        );
        let evidence = shell.debug_tool_output(&id).expect("stored evidence");
        let plain_transcript = |shell: &InteractiveShell| {
            shell
                .state
                .borrow()
                .rendered_transcript(100)
                .iter()
                .map(|line| strip_terminal_sequences(line))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let collapsed = plain_transcript(&shell);
        assert!(!collapsed.contains("result 1"), "{collapsed}");
        assert!(collapsed.contains("result 8"), "{collapsed}");
        assert!(
            collapsed
                .lines()
                .filter(|line| line.contains("result"))
                .all(|line| !line.contains("[31m")),
            "output ANSI leaked: {collapsed}"
        );

        shell.expand_focused_tool();
        let expanded = plain_transcript(&shell);
        assert!(expanded.contains("result 1"), "{expanded}");
        assert_eq!(
            shell.debug_tool_output(&id).as_deref(),
            Some(evidence.as_str())
        );

        shell.expand_focused_tool();
        let collapsed_again = plain_transcript(&shell);
        assert!(!collapsed_again.contains("result 1"), "{collapsed_again}");
        assert!(collapsed_again.contains("result 8"), "{collapsed_again}");
        assert_eq!(
            shell.debug_tool_output(&id).as_deref(),
            Some(evidence.as_str())
        );
        assert_eq!(
            block_copy_text(&shell.state.borrow().transcript[0]),
            format!("$ printf '\\033[31mline\\033[0m\\n'\n{evidence}")
        );
    }

    #[test]
    fn extension_tool_renderer_is_live_semantic_presentation_not_evidence() {
        use ygg_agent::extension_process::ToolRenderSegment;
        use ygg_agent::ToolOutput;

        let red_theme = crate::tui::theme::test_theme_from_source(
            r##"
                [metadata]
                name = "Renderer red"

                [roles."extension.test.label"]
                foreground = "#ff0000"
                bold = true

                [roles."extension.test.value"]
                foreground = "#00aa00"
            "##,
        );
        let mut shell = InteractiveShell::test_shell_with_theme(red_theme);
        let run_id = shell.begin_run("local");
        let id = ToolCallId("extension-render".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: "git_status".into(),
                args: serde_json::json!({"workspace": "."}),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id: id.clone(),
                result: Ok(ToolOutput::new("RAW EVIDENCE\nsecond raw line")),
            },
        );

        let evidence_before = shell.debug_tool_output(&id).expect("stored evidence");
        let copy_before = {
            let state = shell.state.borrow();
            let index = *state.tool_panels.get(&id).expect("tool panel index");
            block_copy_text(&state.transcript[index])
        };
        shell.apply_extension_tool_renderer(
            &id,
            &[
                ToolRenderSegment {
                    text: "\x1b[31mbranch:\x1b[0m ".into(),
                    style_role: Some("extension.test.label\x1b[31m".into()),
                },
                ToolRenderSegment {
                    text: "main\n".into(),
                    style_role: Some("extension.test.value".into()),
                },
                ToolRenderSegment {
                    text: "clean\x07".into(),
                    style_role: Some("extension bad".into()),
                },
            ],
        );

        let stored_before = {
            let state = shell.state.borrow();
            let index = *state.tool_panels.get(&id).expect("tool panel index");
            let TranscriptBlock::Tool(panel) = &state.transcript[index] else {
                panic!("tool panel index should resolve to a tool")
            };
            assert_eq!(panel.output, evidence_before);
            assert!(!panel.extension_render_segments.is_empty());
            assert!(panel
                .extension_render_segments
                .iter()
                .all(|segment| !segment.text.contains('\x1b')));
            assert!(panel
                .extension_render_segments
                .iter()
                .filter_map(|segment| segment.style_role.as_deref())
                .all(|role| !role.contains('\x1b')));
            panel.extension_render_segments.clone()
        };
        let rendered_transcript = |shell: &InteractiveShell| {
            shell
                .state
                .borrow()
                .rendered_transcript(100)
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        };

        let collapsed = rendered_transcript(&shell);
        let collapsed_plain = strip_terminal_sequences(&collapsed);
        assert!(
            collapsed_plain.contains("branch: main"),
            "{collapsed_plain}"
        );
        assert!(collapsed_plain.contains("clean␇"), "{collapsed_plain}");
        assert!(
            !collapsed_plain.contains("RAW EVIDENCE"),
            "{collapsed_plain}"
        );
        let branch_line = collapsed
            .lines()
            .find(|line| strip_terminal_sequences(line).contains("branch:"))
            .expect("semantic renderer row");
        assert!(
            branch_line.contains("\x1b[38;2;255;0;0m"),
            "{branch_line:?}"
        );
        let branch = collapsed_plain.find("branch: main").unwrap();
        let clean = collapsed_plain.find("clean␇").unwrap();
        assert!(
            collapsed_plain[branch..clean].contains('\n'),
            "explicit segment newline was lost: {collapsed_plain:?}"
        );

        shell.expand_focused_tool();
        let expanded = strip_terminal_sequences(&rendered_transcript(&shell));
        assert!(expanded.contains("branch: main"), "{expanded}");
        assert!(expanded.contains("RAW EVIDENCE"), "{expanded}");
        assert_eq!(
            shell.debug_tool_output(&id).as_deref(),
            Some(evidence_before.as_str())
        );

        shell.expand_focused_tool();
        let blue_theme = crate::tui::theme::test_theme_from_source(
            r##"
                [metadata]
                name = "Renderer blue"

                [roles."extension.test.label"]
                foreground = "#0000ff"
                bold = true

                [roles."extension.test.value"]
                foreground = "#00aa00"
            "##,
        );
        shell.set_theme(blue_theme);
        let restyled = rendered_transcript(&shell);
        let branch_line = restyled
            .lines()
            .find(|line| strip_terminal_sequences(line).contains("branch:"))
            .expect("restyled semantic renderer row");
        assert!(
            branch_line.contains("\x1b[38;2;0;0;255m"),
            "{branch_line:?}"
        );
        assert!(
            !branch_line.contains("\x1b[38;2;255;0;0m"),
            "{branch_line:?}"
        );

        let state = shell.state.borrow();
        let index = *state.tool_panels.get(&id).expect("tool panel index");
        let TranscriptBlock::Tool(panel) = &state.transcript[index] else {
            panic!("tool panel index should resolve to a tool")
        };
        assert_eq!(panel.output, evidence_before);
        assert_eq!(panel.extension_render_segments, stored_before);
        assert_eq!(block_copy_text(&state.transcript[index]), copy_before);
    }

    #[test]
    fn extension_tool_renderer_degrades_to_plain_text_without_color() {
        use ygg_agent::extension_process::ToolRenderSegment;
        use ygg_agent::ToolOutput;

        let capabilities = crate::tui::terminal::TerminalCapabilities::test(
            false,
            false,
            crate::tui::terminal::ColorDepth::None,
        );
        let theme = crate::tui::theme::test_theme_source_with(
            r##"
                [metadata]
                name = "Renderer plain"

                [roles."extension.test"]
                foreground = "#ff0000"
                bold = true
            "##,
            capabilities,
            crate::tui::theme::TerminalBackground::Unknown,
        );
        let mut shell = InteractiveShell::test_shell_with_theme(theme);
        let run_id = shell.begin_run("local");
        let id = ToolCallId("extension-render-plain".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: "custom_tool".into(),
                args: serde_json::json!({}),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id: id.clone(),
                result: Ok(ToolOutput::new("private raw evidence")),
            },
        );
        shell.apply_extension_tool_renderer(
            &id,
            &[ToolRenderSegment {
                text: "semantic summary".into(),
                style_role: Some("extension.test".into()),
            }],
        );

        let render = |shell: &InteractiveShell| {
            shell
                .state
                .borrow()
                .rendered_transcript(80)
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        };
        let collapsed = render(&shell);
        assert!(!collapsed.contains('\x1b'), "{collapsed:?}");
        assert!(collapsed.contains("semantic summary"), "{collapsed}");
        assert!(!collapsed.contains("private raw evidence"), "{collapsed}");

        shell.expand_focused_tool();
        let expanded = render(&shell);
        assert!(!expanded.contains('\x1b'), "{expanded:?}");
        assert!(expanded.contains("semantic summary"), "{expanded}");
        assert!(expanded.contains("private raw evidence"), "{expanded}");
        assert_eq!(
            shell.debug_tool_output(&id).as_deref(),
            Some("private raw evidence")
        );
    }

    #[test]
    fn compact_exec_empty_and_single_line_outputs_degrade_cleanly_without_color() {
        let capabilities = crate::tui::terminal::TerminalCapabilities::test(
            false,
            false,
            crate::tui::terminal::ColorDepth::None,
        );
        let theme = crate::tui::theme::test_theme_with(capabilities);
        let renderer = theme.rich_renderer();
        for (output, expected) in [
            ("exit=0 duration=0.2s\n(no output)", None),
            (
                "exit=0 duration=0.2s\nstdout: 1 lines\n1827 /tmp/timeline.txt\ntruncated_stdout=false",
                Some("1827 /tmp/timeline.txt"),
            ),
        ] {
            let args = serde_json::json!({"command": "wc -l /tmp/timeline.txt"});
            let block = TranscriptBlock::Tool(Box::new(ToolPanel::new(
                ToolCallId(format!("plain-{expected:?}")),
                "exec".into(),
                args.to_string(),
                summarize_tool("exec", &args),
                output.into(),
                true,
                false,
                None,
                None,
            )));
            let lines = render_block(None, &block, &theme, &renderer, &renderer, 28, false);
            assert!(lines.iter().all(|line| !line.contains('\x1b')));
            assert!(lines.iter().all(|line| visible_width(line) <= 28));
            let joined = lines.join("\n");
            match expected {
                Some(expected) => assert!(joined.contains(expected), "{joined}"),
                None => assert!(!joined.contains("no output"), "{joined}"),
            }
        }
    }

    #[test]
    fn active_model_switch_keeps_run_identity_and_clears_stale_idle_telemetry() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(46, 12);
        shell.set_identity("openai", "gpt-5.6", "high");
        {
            let mut state = shell.state.borrow_mut();
            crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::OpenAi);
            state.model_lab = Some(ModelLab::OpenAi);
            state.context_estimate = Some((12_000, 256_000));
            state.price_display = PriceDisplay::Priced;
            state.show_turn_cost = true;
        }
        shell.on_prompt_submitted("prompt for A");
        let run_id = shell.begin_run("openai");
        let now = Instant::now();
        let before =
            crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 46, now);

        shell.set_identity("deepseek", "deepseek-v4-pro", "medium");
        {
            let mut state = shell.state.borrow_mut();
            crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::DeepSeek);
            state.model_lab = Some(ModelLab::DeepSeek);
            state.context_estimate = Some((2_000, 128_000));
            state.last_turn_tokens_per_second = Some(55.0);
            state.run_cost_microdollars = 3_000;
            state.run_cost_available = true;
        }
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: "Checking ownership".into(),
            },
        );
        let active_footer = plain_footer(&shell, 46, now);
        assert!(active_footer.contains("GPT-5.6"), "{active_footer:?}");
        assert!(!active_footer.contains("DeepSeek"), "{active_footer:?}");
        shell.set_size(24, 12);
        let narrow_active = plain_footer(&shell, 24, now);
        assert!(
            !narrow_active.contains("0.0s"),
            "default timer leaked: {narrow_active:?}"
        );
        assert!(!narrow_active.contains("thinking"), "{narrow_active:?}");
        assert!(!narrow_active.contains("tool"), "{narrow_active:?}");
        shell.set_size(46, 12);
        {
            let state = shell.state.borrow();
            let TranscriptBlock::Reasoning(reasoning) = state.transcript.last().unwrap() else {
                panic!("streamed reasoning block expected");
            };
            assert_eq!(reasoning.model_lab, Some(ModelLab::OpenAi));
        }
        let after =
            crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 46, now);
        assert_eq!(before.len(), after.len());
        assert_eq!(visible_width(&before[0]), visible_width(&after[0]));

        shell.interrupt_run(run_id);
        let idle_footer = plain_footer(&shell, 46, now);
        assert!(idle_footer.contains("DeepSeek V4 Pro"), "{idle_footer:?}");
        assert!(!idle_footer.contains("55.0 tok/s"), "{idle_footer:?}");
        assert!(!idle_footer.contains("$0.003"), "{idle_footer:?}");
        shell.on_prompt_submitted("prompt for B");
        let state = shell.state.borrow();
        let prompts = state
            .transcript
            .iter()
            .filter_map(|block| match block {
                TranscriptBlock::User {
                    text,
                    model_lab,
                    prompt_color,
                    ..
                } => Some((text.as_str(), *model_lab, prompt_color.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prompts,
            vec![
                (
                    "prompt for A",
                    Some(ModelLab::OpenAi),
                    Some(crate::tui::theme::prompt_color_for_model_id("gpt-5.6")),
                ),
                (
                    "prompt for B",
                    Some(ModelLab::DeepSeek),
                    Some(crate::tui::theme::prompt_color_for_model_id(
                        "deepseek-v4-pro",
                    )),
                ),
            ]
        );
    }

    #[test]
    fn transcript_and_composer_have_exactly_one_breathing_row() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 5);
        shell.on_prompt_submitted("question");
        shell
            .state
            .borrow_mut()
            .push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized("answer".into()),
            )));
        let lines = render_shell(&shell.state.borrow(), 80)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        let composer = lines
            .iter()
            .position(|line| {
                line.starts_with('┌') || line.starts_with('╭') || line.starts_with('+')
            })
            .expect("composer top border");
        assert!(composer > 0);
        assert!(lines[composer - 1].is_empty());
        assert!(composer < 2 || !lines[composer - 2].is_empty());
    }

    #[test]
    fn slash_popup_keeps_selection_visible_across_paging_filtering_and_resize() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        shell.apply_edit(EditAction::Char('/'));
        shell.slash_menu(SlashMenuAction::Last);
        let last = commands::slash_suggestions("/").len() - 1;
        assert_eq!(shell.state.borrow().slash_selection, last);

        shell.set_size(34, 9);
        let resized = shell_chrome(&shell.state.borrow(), 34, Instant::now()).suggestions;
        let resized_plain = resized
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert!(resized_plain.iter().any(|line| line.contains("/quit")));
        assert!(resized_plain
            .iter()
            .any(|line| line.contains('›') && line.contains("/quit")));
        assert!(resized_plain.first().is_some_and(|line| line.contains('/')));
        assert!(resized.iter().all(|line| visible_width(line) <= 34));

        let page = resized.len().saturating_sub(1).max(1);
        shell.slash_menu(SlashMenuAction::PageUp);
        assert_eq!(
            shell.state.borrow().slash_selection,
            last.saturating_sub(page)
        );
        shell.slash_menu(SlashMenuAction::First);
        shell.slash_menu(SlashMenuAction::PageDown);
        assert_eq!(shell.state.borrow().slash_selection, page.min(last));

        shell.slash_menu(SlashMenuAction::Last);
        shell.apply_edit(EditAction::Char('m'));
        let state = shell.state.borrow();
        assert_eq!(state.editor, "/m");
        assert_eq!(state.slash_selection, 0);
        assert_eq!(state.slash_scroll, 0);
        drop(state);

        shell.set_size(1, 9);
        let narrow = render_slash_suggestions(&shell.state.borrow(), 1, 5);
        assert!(narrow.iter().all(|line| visible_width(line) <= 1));
    }

    #[test]
    fn composer_border_is_restrained_at_rest_and_uses_model_accent_when_focused() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_identity("anthropic", "claude-sonnet-4", "high");
        {
            let mut state = shell.state.borrow_mut();
            crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::Anthropic);
            state.model_lab = Some(ModelLab::Anthropic);
        }
        let now = Instant::now();
        let idle =
            crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 60, now);
        shell.apply_edit(EditAction::Char('x'));
        let focused =
            crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 60, now);

        assert_ne!(idle[0], focused[0]);
        assert!(!idle[0].contains("38;2;169;99;76"), "{:?}", idle[0]);
        assert!(focused[0].contains("38;2;169;99;76"), "{:?}", focused[0]);
        assert_eq!(visible_width(&idle[0]), 60);
        assert_eq!(visible_width(&focused[0]), 60);
        let wide =
            crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 120, now);
        assert_eq!(wide[0].matches("\x1b[38;2;").count(), 1);
        assert!(
            wide[0].len() < 450,
            "120-column uniform border encoded {} bytes",
            wide[0].len()
        );
        for edge in [
            &idle[0],
            &idle[idle.len() - 2],
            &focused[0],
            &focused[focused.len() - 2],
        ] {
            assert_eq!(
                edge.matches("\x1b[38;2;").count(),
                1,
                "uniform border reopened its RGB style per cell: {edge:?}"
            );
            assert!(
                edge.len() < 240,
                "uniform border encoded {} bytes",
                edge.len()
            );
        }
    }

    fn theme_with_layout(layout: &str) -> YggTheme {
        crate::tui::theme::test_theme_from_source(&format!("[layout]\n{layout}"))
    }

    #[test]
    fn theme_density_and_transcript_inset_change_semantic_block_geometry() {
        let previous = TranscriptBlock::Notice("previous".into());
        let current = TranscriptBlock::Notice("current".into());
        let render = |density: &str, inset: u16| {
            let theme = theme_with_layout(&format!(
                "density = \"{density}\"\ntranscript_inset = {inset}"
            ));
            let renderer = theme.rich_renderer();
            render_block(
                Some(&previous),
                &current,
                &theme,
                &renderer,
                &renderer,
                80,
                false,
            )
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
        };

        let compact = render("compact", 1);
        let comfortable = render("comfortable", 2);
        let airy = render("airy", 4);
        assert_eq!(compact.iter().take_while(|line| line.is_empty()).count(), 0);
        assert_eq!(
            comfortable
                .iter()
                .take_while(|line| line.is_empty())
                .count(),
            1
        );
        assert_eq!(airy.iter().take_while(|line| line.is_empty()).count(), 2);
        assert!(compact[0].starts_with(' '));
        assert!(comfortable[1].starts_with("  "));
        assert!(airy[2].starts_with("    "));

        let hidden_theme = theme_with_layout(
            "density = \"airy\"\nshow_reasoning = false\nnarrow_show_reasoning = false",
        );
        let hidden_renderer = hidden_theme.rich_renderer();
        let hidden_reasoning = TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::finalized_reasoning("hidden".into()),
        ));
        let collapsed_reasoning = render_block(
            None,
            &hidden_reasoning,
            &hidden_theme,
            &hidden_renderer,
            &hidden_renderer,
            80,
            false,
        );
        assert_eq!(collapsed_reasoning.len(), 2);
        assert!(strip_terminal_sequences(&collapsed_reasoning[0]).contains("thought"));
        assert!(strip_terminal_sequences(&collapsed_reasoning[1]).contains("ctrl+o to expand"));
        let first_visible = render_block(
            Some(&hidden_reasoning),
            &current,
            &hidden_theme,
            &hidden_renderer,
            &hidden_renderer,
            80,
            false,
        );
        assert_eq!(
            first_visible
                .iter()
                .take_while(|line| line.is_empty())
                .count(),
            2
        );
    }

    #[test]
    fn layout_breakpoint_is_resolved_from_terminal_width_before_inset() {
        let theme = theme_with_layout(
            r#"
                transcript_inset = 4
                narrow_breakpoint = 72
                show_reasoning = true
                narrow_show_reasoning = false
                show_tool_duration = true
                narrow_show_tool_duration = false
            "#,
        );
        let renderer = theme.rich_renderer();
        let reasoning = TranscriptBlock::Reasoning(Box::new(AssistantBlock::finalized_reasoning(
            "visible at the breakpoint".into(),
        )));
        let at_breakpoint = render_block(None, &reasoning, &theme, &renderer, &renderer, 72, false);
        let below_breakpoint =
            render_block(None, &reasoning, &theme, &renderer, &renderer, 71, false);
        assert!(strip_terminal_sequences(&at_breakpoint.join("\n")).contains("thought"));
        assert!(strip_terminal_sequences(&below_breakpoint.join("\n")).contains("thought"));

        let args = serde_json::json!({"command": "cargo check"});
        let tool = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("duration-breakpoint".into()),
            "exec".into(),
            args.to_string(),
            summarize_tool("exec", &args),
            "exit=0 duration=0.2s".into(),
            true,
            false,
            None,
            None,
        )));
        let at_breakpoint = strip_terminal_sequences(
            &render_block(None, &tool, &theme, &renderer, &renderer, 72, false).join("\n"),
        );
        let below_breakpoint = strip_terminal_sequences(
            &render_block(None, &tool, &theme, &renderer, &renderer, 71, false).join("\n"),
        );
        assert!(at_breakpoint.contains("0.2s"), "{at_breakpoint:?}");
        assert!(!below_breakpoint.contains("0.2s"), "{below_breakpoint:?}");
    }

    #[test]
    fn selection_mapping_excludes_density_rows_and_transcript_inset() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_theme(theme_with_layout(
            "density = \"airy\"\ntranscript_inset = 4",
        ));
        {
            let mut state = shell.state.borrow_mut();
            state.push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized("alpha".into()),
            )));
            state.push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized("bravo".into()),
            )));
        }
        let second_start = {
            let state = shell.state.borrow();
            let _ = state.rendered_transcript(80);
            let second_start = state.transcript_cache.borrow().block_starts[1];
            second_start
        };
        assert!(
            selection_position_for_visual_cell(&shell.state.borrow(), second_start, 4).is_none()
        );
        let start = selection_position_for_visual_cell(&shell.state.borrow(), second_start + 2, 4)
            .expect("first content cell should map");
        assert_eq!(start.block, 1);
        assert_eq!(start.offset, 0);
        let two_cells =
            selection_position_for_visual_cell(&shell.state.borrow(), second_start + 2, 6)
                .expect("content cell should map");
        assert_eq!(two_cells.offset, 2);
    }

    const SURFACE_TEST_THEME: &str = r##"
        [metadata]
        name = "Surface fixture"
        adaptive = false

        [roles."surface.user"]
        foreground = "default"
        background = "#112233"
        [roles."surface.user.border"]
        foreground = "#6688aa"
        [roles."surface.user.label"]
        foreground = "#99ccff"
        bold = true

        [roles."surface.assistant"]
        foreground = "default"
        background = "#221133"
        [roles."surface.assistant.border"]
        foreground = "#9966bb"
        [roles."surface.assistant.label"]
        foreground = "#ddbbff"
        bold = true

        [surfaces.user]
        chrome = "card"
        heading = "tab"
        label = "INPUT"
        padding = 1
        width = "full"
        narrow_chrome = "rail"
        narrow_heading = "none"
        narrow_padding = 0

        [surfaces.assistant]
        chrome = "card"
        heading = "overline"
        label = "RESPONSE"
        padding = 1
        width = "full"
        narrow_chrome = "plain"
        narrow_heading = "none"
        narrow_padding = 0

        [glyphs]
        top_left = "╭"
        top_right = "╮"
        bottom_left = "╰"
        bottom_right = "╯"
        horizontal = "─"
        vertical = "│"
        rail = "┃"
        prompt = "›"

        [glyphs_ascii]
        top_left = "+"
        top_right = "+"
        bottom_left = "+"
        bottom_right = "+"
        horizontal = "-"
        vertical = "|"
        rail = "|"
        prompt = ">"

        [layout]
        density = "compact"
        transcript_inset = 1
        narrow_breakpoint = 60
    "##;

    #[test]
    fn card_geometry_keeps_prompt_identity_and_decorations_out_of_selection() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_theme(crate::tui::theme::test_theme_from_source(
            SURFACE_TEST_THEME,
        ));
        {
            let mut state = shell.state.borrow_mut();
            state.push_block(TranscriptBlock::User {
                text: "hello surface".into(),
                model_lab: Some(ModelLab::Alibaba),
                prompt_color: Some("#ff7018".into()),
                persisted: true,
            });
        }
        let (start, length, geometry, rows) = {
            let state = shell.state.borrow();
            let rows = state.rendered_transcript(80).clone();
            let cache = state.transcript_cache.borrow();
            (
                cache.block_starts[0],
                cache.block_lengths[0],
                cache.block_geometries[0],
                rows,
            )
        };
        assert_eq!(geometry.leading_rows, 1);
        assert_eq!(geometry.trailing_rows, 1);
        assert!(selection_position_for_visual_cell(&shell.state.borrow(), start, 0).is_none());
        assert!(
            selection_position_for_visual_cell(&shell.state.borrow(), start + length - 1, 79,)
                .is_none()
        );

        let body_row = start + geometry.transition_rows + geometry.leading_rows;
        let first = selection_position_for_visual_cell(
            &shell.state.borrow(),
            body_row,
            geometry.content_left + 2,
        )
        .expect("first prompt text cell");
        assert_eq!(first.offset, 0);
        let second = selection_position_for_visual_cell(
            &shell.state.borrow(),
            body_row,
            geometry.content_left + 3,
        )
        .expect("second prompt text cell");
        assert_eq!(second.offset, 1);

        let body = &rows[body_row];
        assert!(body.contains("\x1b[48;2;255;112;24m"), "{body:?}");
        assert!(
            !body.contains("\x1b[48;2;17;34;51m"),
            "the full persisted-prompt row must keep its model background: {body:?}"
        );
        assert!(
            body.ends_with("\x1b[0m"),
            "surface background leaked: {body:?}"
        );

        {
            let mut state = shell.state.borrow_mut();
            state.transcript_selection = Some(TranscriptSelection {
                anchor: TranscriptPosition {
                    block: 0,
                    offset: 1,
                    trailing_affinity: false,
                },
                focus: TranscriptPosition {
                    block: 0,
                    offset: 5,
                    trailing_affinity: false,
                },
            });
            assert!(state.copy_buffer.is_none());
        }
        assert_eq!(shell.selected_plain_text().as_deref(), Some("ello"));
        assert!(shell.state.borrow().copy_buffer.is_none());
    }

    #[test]
    fn card_surface_degrades_to_rail_with_exact_cached_narrow_geometry() {
        let theme = crate::tui::theme::test_theme_from_source(SURFACE_TEST_THEME);
        let block = TranscriptBlock::User {
            text: "narrow request".into(),
            model_lab: None,
            prompt_color: None,
            persisted: true,
        };
        let wide = compile_surface_plan(None, &block, &theme, 80);
        assert_eq!(wide.chrome, ThemeSurfaceChrome::Card);
        assert_eq!(wide.geometry.leading_rows, 1);
        assert_eq!(wide.geometry.trailing_rows, 1);

        let narrow = compile_surface_plan(None, &block, &theme, 40);
        assert_eq!(narrow.chrome, ThemeSurfaceChrome::Rail);
        assert_eq!(narrow.heading, ThemeSurfaceHeading::None);
        assert_eq!(narrow.geometry.leading_rows, 0);
        assert_eq!(narrow.geometry.trailing_rows, 0);
        let renderer = theme.rich_renderer();
        let rendered = render_block_planned(None, &block, &theme, &renderer, &renderer, 40, false);
        assert_eq!(rendered.geometry, narrow.geometry);
        let plain = rendered
            .lines
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert!(
            plain.first().is_some_and(|line| line.contains("┃")),
            "{plain:?}"
        );
        assert!(plain
            .iter()
            .all(|line| !line.contains('╭') && !line.contains('╰')));
    }

    #[test]
    fn card_background_and_glyphs_degrade_across_terminal_capabilities() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
        use crate::tui::theme::TerminalBackground;

        let block = TranscriptBlock::Assistant(Box::new(AssistantBlock::finalized(
            "# Result\n\n```rust\nlet answer = 42;\n```".into(),
        )));
        let render = |capabilities, background| {
            let theme = crate::tui::theme::test_theme_source_with(
                SURFACE_TEST_THEME,
                capabilities,
                background,
            );
            let renderer = theme.rich_renderer();
            render_block(None, &block, &theme, &renderer, &renderer, 72, false)
        };

        let truecolor = render(
            TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
            TerminalBackground::Dark,
        );
        assert!(truecolor
            .iter()
            .any(|line| line.contains("\x1b[48;2;34;17;51m")));
        assert!(truecolor
            .iter()
            .filter(|line| !line.is_empty())
            .all(|line| line.ends_with("\x1b[0m")));

        let ansi = render(
            TerminalCapabilities::test(true, false, ColorDepth::Ansi16),
            TerminalBackground::Dark,
        );
        assert!(ansi
            .iter()
            .any(|line| line.contains("\x1b[4") || line.contains("\x1b[10")));
        assert!(ansi.iter().all(|line| !line.contains("48;2")));
        let ansi_plain = ansi
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert!(ansi_plain
            .first()
            .is_some_and(|line| line.trim_start().starts_with('+')));

        let no_color = render(
            TerminalCapabilities::test(false, false, ColorDepth::None),
            TerminalBackground::Dark,
        );
        assert!(no_color.iter().all(|line| !line.contains('\x1b')));
        assert!(no_color
            .first()
            .is_some_and(|line| line.trim_start().starts_with('+')));

        let adaptive_source = SURFACE_TEST_THEME.replace("adaptive = false", "adaptive = true");
        let unknown = crate::tui::theme::test_theme_source_with(
            &adaptive_source,
            TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
            TerminalBackground::Unknown,
        );
        let renderer = unknown.rich_renderer();
        let unknown = render_block(None, &block, &unknown, &renderer, &renderer, 72, false);
        assert!(unknown.iter().all(|line| !line.contains("\x1b[48;")));
    }

    #[test]
    fn theme_header_footer_status_and_composer_padding_have_narrow_fallbacks() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 20);
        shell.set_identity("local", "qwen3.6-27b", "high");
        shell.set_theme(theme_with_layout(
            r#"
                show_header = true
                show_footer = false
                show_status_line = false
                composer_padding = 3
                narrow_breakpoint = 50
                narrow_show_header = false
                narrow_show_footer = false
                narrow_show_status_line = false
            "#,
        ));

        let now = Instant::now();
        let composer = plain_composer_surface(&shell, 80, now);
        assert_eq!(composer.len(), 3, "hidden footer leaves only the box");
        assert!(composer[1].starts_with("│   ›"), "{composer:?}");
        assert!(composer.iter().all(|line| visible_width(line) == 80));

        let wide_header = shell_chrome(&shell.state.borrow(), 80, now).header;
        assert_eq!(wide_header.len(), 1);
        let wide_header = strip_terminal_sequences(&wide_header[0]);
        assert!(wide_header.contains("ygg"));
        assert!(
            wide_header.contains("local / Qwen3.6 27B"),
            "{wide_header:?}"
        );
        assert!(shell_chrome(&shell.state.borrow(), 40, now)
            .header
            .is_empty());

        shell.set_extension_header(Some((
            "EXT\x1b[31m red\ntail".into(),
            Some("invalid role!".into()),
        )));
        shell.set_extension_status(Some(("branch main".into(), None)));
        let narrow = shell_chrome(&shell.state.borrow(), 40, now);
        assert_eq!(narrow.header.len(), 1);
        let extension_header = strip_terminal_sequences(&narrow.header[0]);
        assert!(
            extension_header.contains("EXT red tail"),
            "{extension_header:?}"
        );
        assert!(!extension_header.contains('\x1b'));
        let composer = plain_composer_surface(&shell, 40, now);
        assert_eq!(composer.len(), 4, "explicit status restores one footer row");
        assert!(composer
            .last()
            .is_some_and(|line| line.contains("branch main")));
    }

    #[test]
    fn panel_border_layout_degrades_to_unframed_narrow_picker() {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 20);
        shell.set_theme(theme_with_layout(
            r#"
                show_panel_borders = true
                narrow_breakpoint = 60
                narrow_show_panel_borders = false
            "#,
        ));
        open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

        let wide = render_panel(&shell.state.borrow(), 80)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        assert_eq!(wide.len(), 7);
        assert!(wide
            .first()
            .is_some_and(|line| line.chars().all(|ch| ch == '─')));
        assert!(wide
            .last()
            .is_some_and(|line| line.chars().all(|ch| ch == '─')));

        let narrow = render_panel(&shell.state.borrow(), 40)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        assert_eq!(narrow.len(), 5);
        assert!(narrow
            .first()
            .is_some_and(|line| line.contains("Select model")));
        assert!(narrow.iter().all(|line| !line.chars().all(|ch| ch == '─')));
    }

    const BUNDLED_THEME_NAMES: [&str; 10] = [
        "bone-machine",
        "circuit-garden",
        "field-notes",
        "oxide-console",
        "paper-ledger",
        "signal-noir",
        "synthwave-relay",
        "tidepool",
        "violet-hour",
        "zen-mono",
    ];

    fn populate_theme_fixture(shell: &mut InteractiveShell) {
        shell.set_identity("local", "qwen3.6-27b", "high");
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::User {
            text: "Review `src/lib.rs` and keep the public API stable.".into(),
            model_lab: Some(ModelLab::Alibaba),
            prompt_color: Some("#ff7018".into()),
            persisted: true,
        });
        state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized(
                "# Patch plan\n\nKeep the change **small** and verify it.\n\n```rust\nfn answer() -> u8 { 42 }\n```"
                    .into(),
            ),
        )));
        state.push_block(TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::finalized_reasoning(
                "Checking ownership, invariants, and the narrow fallback.".into(),
            ),
        )));
        let args = serde_json::json!({"path": "src/lib.rs"});
        state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("fixture-edit".into()),
            "edit".into(),
            args.to_string(),
            summarize_tool("edit", &args),
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new".into(),
            true,
            false,
            None,
            Some(ModelLab::Alibaba),
        ))));
        state.push_block(TranscriptBlock::Shell(Box::new(ShellOutput {
            id: "fixture-shell".into(),
            command: "cargo test -p ygg-coding-agent".into(),
            output: "test result: ok. 386 passed".into(),
            exit_code: 0,
            running: false,
            spinner: "✓".into(),
        })));
        state.push_block(TranscriptBlock::Notice(
            "Extension reloaded with one status contribution.".into(),
        ));
        state.push_block(TranscriptBlock::Outcome(RunOutcome::Completed {
            elapsed: Duration::from_millis(13700),
            summary: crate::presentation::RunSummary {
                files_changed: 1,
                tool_calls: 2,
                warnings: 0,
            },
        }));
        state.extension_header = Some(("workspace · main".into(), None));
        state.extension_status = Some(("git clean".into(), None));
        state.editor = "draft a local patch".into();
    }

    /// Remove colors and semantic words while retaining whitespace,
    /// punctuation, rails, rules, and card geometry. Palette/wordmark-only
    /// changes therefore cannot satisfy the bundled identity test.
    fn structural_signature(rendered: &str) -> String {
        let plain = strip_terminal_sequences(rendered);
        let mut signature = String::with_capacity(plain.len());
        let mut word = false;
        for character in plain.chars() {
            if character.is_alphanumeric() || character == '_' {
                if !word {
                    signature.push('x');
                    word = true;
                }
            } else {
                word = false;
                signature.push(character);
            }
        }
        signature
    }

    fn ansi_background_is_open_at_end(line: &str) -> bool {
        let bytes = line.as_bytes();
        let mut index = 0;
        let mut background_open = false;
        while index + 2 < bytes.len() {
            if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
                index += 1;
                continue;
            }
            let Some(relative_end) = bytes[index + 2..].iter().position(|byte| *byte == b'm')
            else {
                break;
            };
            let end = index + 2 + relative_end;
            let parameters = std::str::from_utf8(&bytes[index + 2..end]).unwrap_or("");
            if parameters.is_empty() {
                background_open = false;
            } else {
                for parameter in parameters
                    .split(';')
                    .filter_map(|value| value.parse::<u16>().ok())
                {
                    match parameter {
                        0 | 49 => background_open = false,
                        40..=47 | 48 | 100..=107 => background_open = true,
                        _ => {}
                    }
                }
            }
            index = end + 1;
        }
        background_open
    }

    #[test]
    fn bundled_theme_pack_has_ten_color_independent_wide_and_narrow_identities() {
        use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
        use crate::tui::theme::TerminalBackground;

        let mut wide = HashSet::new();
        let mut ascii = HashSet::new();
        let mut narrow = HashSet::new();
        for name in BUNDLED_THEME_NAMES {
            let mut shell = InteractiveShell::test_shell();
            shell.set_size(96, 80);
            shell.set_theme(crate::tui::theme::test_bundled_theme_with(
                name,
                TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
                TerminalBackground::Dark,
            ));
            populate_theme_fixture(&mut shell);
            let transcript = shell.state.borrow().rendered_transcript(96).join("\n");
            assert!(
                transcript.contains("\x1b[48;2;255;112;24m"),
                "{name} changed the immutable prompt-row rectangle"
            );
            let unclosed_backgrounds = transcript
                .lines()
                .filter(|line| ansi_background_is_open_at_end(line))
                .collect::<Vec<_>>();
            assert!(
                unclosed_backgrounds.is_empty(),
                "{name} leaked a painted surface beyond its row: {unclosed_backgrounds:?}"
            );
            assert!(
                wide.insert(structural_signature(&transcript)),
                "{name} duplicated another color-stripped transcript geometry"
            );

            let mut plain_shell = InteractiveShell::test_shell();
            plain_shell.set_size(96, 80);
            plain_shell.set_theme(crate::tui::theme::test_bundled_theme_with(
                name,
                TerminalCapabilities::test(false, false, ColorDepth::None),
                TerminalBackground::Dark,
            ));
            populate_theme_fixture(&mut plain_shell);
            let plain = plain_shell
                .state
                .borrow()
                .rendered_transcript(96)
                .join("\n");
            assert!(
                !plain.contains('\x1b'),
                "{name} emitted ANSI in no-color mode"
            );
            assert!(
                ascii.insert(structural_signature(&plain)),
                "{name} duplicated another ASCII transcript geometry"
            );

            let mut narrow_shell = InteractiveShell::test_shell();
            narrow_shell.set_size(40, 80);
            narrow_shell.set_theme(crate::tui::theme::test_bundled_theme_with(
                name,
                TerminalCapabilities::test(false, false, ColorDepth::None),
                TerminalBackground::Dark,
            ));
            populate_theme_fixture(&mut narrow_shell);
            let narrow_frame = narrow_shell
                .state
                .borrow()
                .rendered_transcript(40)
                .join("\n");
            assert!(
                narrow_frame.lines().all(|line| visible_width(line) <= 40),
                "{name} overflowed a narrow terminal"
            );
            assert!(
                narrow.insert(structural_signature(&narrow_frame)),
                "{name} duplicated another narrow transcript geometry"
            );

            if std::env::var_os("YGG_DUMP_THEME_FRAMES").is_some() {
                eprintln!(
                    "\n===== {name} / wide =====\n{}",
                    strip_terminal_sequences(&transcript)
                );
                eprintln!("\n===== {name} / narrow =====\n{narrow_frame}");
            }
        }
        assert_eq!(wide.len(), 10);
        assert_eq!(ascii.len(), 10);
        assert_eq!(narrow.len(), 10);
    }
}
