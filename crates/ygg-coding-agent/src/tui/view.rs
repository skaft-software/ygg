#![allow(missing_docs)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{IsTerminal, Write as IoWrite};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sexy_tui_rs::{
    parse_markdown, strip_terminal_sequences, visible_width, wrap_text_with_ansi, RichRenderer,
    CURSOR_MARKER, TUI,
};
use ygg_agent::{AgentEvent, EntryValue, OutputChannel, Session, ToolProgress};
use ygg_ai::{ModalitySet, Model, ModelId, ToolCallId, Usage};

use crate::config::Config;
use crate::hydrate::hydrate_transcript_tail;
#[cfg(test)]
use crate::presentation::summarize_tool;
use crate::presentation::{
    summarize_tool_with_workspace, tool_failure_reason, tool_result_is_failure,
    ModelDisplayMetadata, PriceDisplay, RunId, RunOutcome, RunTracker, ToolDisplay,
};
use crate::tui::composer::{self, ComposedInput};
use crate::tui::keymap::{EditAction, SlashMenuAction};
use crate::tui::terminal::{force_restore, TerminalSize, YggTerminal};
#[cfg(test)]
use crate::tui::theme::ThemeSurfaceChrome;
use crate::tui::theme::{ModelLab, ThemeDensity, YggTheme};

#[cfg(test)]
use self::assistant_block::reasoning_heading_from_block;
use self::assistant_block::AssistantBlock;
use self::editor_layout::{
    editor_column, editor_layout, editor_offset_at_column, normalize_paste, EditorLayoutCache,
};
use self::input_overlays::input_slash_suggestions;
#[cfg(test)]
use self::input_overlays::render_slash_suggestions;
#[cfg(test)]
use self::native_scrollback::{render_shell, render_shell_at, render_shell_update};
use self::panel_render::filtered_indices;
#[cfg(test)]
use self::panel_render::render_panel;
use self::reasoning_render::collapsed_reasoning_lines;
#[cfg(test)]
use self::renderer_runtime::{
    event_dot_animating, reconcile_terminal_size, ShellComponent, ShellFrameState,
};
use self::renderer_runtime::{render_loop, RenderCommand, SharedState};
#[cfg(test)]
use self::shell_chrome::responsive_identity;
use self::shell_chrome::shell_chrome;
use self::status_telemetry::{
    output_tokens_per_second, status_telemetry, styled_status_text,
    usage_cache_hit_rate_basis_points,
};
#[cfg(test)]
use self::surface_frame::event_margin_marker;
pub use self::terminal_text::bounded_append;
use self::terminal_text::{sanitize_extension_surface, sanitize_extension_tool_render_segments};
pub(crate) use self::terminal_text::{sanitize_for_terminal, sanitized_editor};
#[cfg(test)]
use self::tool_render::looks_like_diff;
use self::transcript_cache::TranscriptCache;
use self::transcript_history::{
    materialize_deferred_session_history, DeferredSessionHistory, NextTranscriptCommitId,
};
use self::transcript_hydration::append_hydrated_items;
#[cfg(test)]
use self::transcript_render::{render_block, render_block_planned};
use self::transcript_selection::{
    block_copy_text, selection_position_for_visual_cell, semantic_selected_text,
    TranscriptPosition, TranscriptSelection,
};
use self::viewport::{
    max_scroll_for_available, max_scroll_from_bottom, transcript_lines,
    transcript_viewport_capacity, transcript_viewport_capacity_for_state,
};
#[cfg(test)]
use self::viewport::{render_shell_viewport_at, render_shell_viewport_update};

/// A compact tool row keeps enough terminal context to recognize a result
/// while preventing noisy output from swallowing the transcript.
const COMPACT_EXEC_OUTPUT_LINES: usize = 5;

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
}

#[derive(Clone, Debug)]
struct CompactionBlock {
    /// Concise durable-event annotation shown while collapsed.
    label: String,
    /// Complete model-produced summary retained for inline inspection.
    summary: String,
    expanded: bool,
}

#[derive(Clone, Debug)]
struct OutcomeBlock {
    outcome: RunOutcome,
    /// Final provider-reported output rate captured when the run settles.
    tokens_per_second: Option<f64>,
}

impl OutcomeBlock {
    fn new(outcome: RunOutcome, tokens_per_second: Option<f64>) -> Self {
        Self {
            outcome,
            tokens_per_second,
        }
    }
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
    Outcome(OutcomeBlock),
    Notice(String),
    Compaction(Box<CompactionBlock>),
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
    /// Whether Ctrl+O can change this finalized tool's physical rows. The
    /// result is computed only after output becomes immutable.
    cached_disclosure_sensitive: RefCell<Option<bool>>,
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
            cached_disclosure_sensitive: RefCell::new(None),
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
    /// Small set of transcript indices that can currently own the shared
    /// activity pulse. Keeping it explicit makes each tick O(active work)
    /// instead of O(total session history).
    active_event_blocks: Vec<usize>,
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
    skill_commands: Arc<[(String, String)]>,
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
    /// Global transcript disclosure mode. Ctrl+O and `/verbose` toggle this.
    pub(crate) verbose_tools: bool,
    pub(crate) size: (u16, u16),
    /// Start of the animated invocation header. It remains mutable until the
    /// first real conversation block so model changes can recolor it in place.
    startup_card_started_at: Option<Instant>,
    /// Cached editor layout so input and transcript updates do not repeatedly
    /// re-wrap an unchanged prompt.
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

    fn register_active_event(&mut self, index: usize) {
        if !self.active_event_blocks.contains(&index) {
            self.active_event_blocks.push(index);
        }
    }

    fn unregister_active_event(&mut self, index: usize) {
        self.active_event_blocks.retain(|active| *active != index);
    }

    fn reindex_active_events_after_removal(&mut self, removed: usize) {
        for active in &mut self.active_event_blocks {
            if *active > removed {
                *active -= 1;
            }
        }
    }

    fn remove_transient_activity_block(&mut self, index: usize) {
        if index >= self.transcript.len() {
            return;
        }
        self.unregister_active_event(index);
        self.transcript.remove(index);
        self.transcript_commit_ids.remove(index);
        self.block_revisions.remove(index);
        self.reindex_active_events_after_removal(index);
        self.active_text = self
            .active_text
            .and_then(|active| (active != index).then_some(active - usize::from(active > index)));
        self.active_reasoning = self
            .active_reasoning
            .and_then(|active| (active != index).then_some(active - usize::from(active > index)));
        for panel_index in self.tool_panels.values_mut() {
            if *panel_index > index {
                *panel_index -= 1;
            }
        }
        if !self.follow_tail {
            self.new_output_count = self.new_output_count.saturating_sub(1);
        }
        let selection_touches_removed =
            self.transcript_selection.as_ref().is_some_and(|selection| {
                selection.anchor.block == index || selection.focus.block == index
            });
        if selection_touches_removed {
            self.transcript_selection = None;
            self.selection_dragging = false;
        } else if let Some(selection) = &mut self.transcript_selection {
            selection.anchor.block -= usize::from(selection.anchor.block > index);
            selection.focus.block -= usize::from(selection.focus.block > index);
        }
        if self
            .pending_selection_anchor
            .is_some_and(|position| position.block == index)
        {
            self.pending_selection_anchor = None;
            self.selection_dragging = false;
        } else if let Some(position) = &mut self.pending_selection_anchor {
            position.block -= usize::from(position.block > index);
        }
        self.invalidate_transcript_layout();
    }

    fn show_tool_details(&self, _block: &TranscriptBlock) -> bool {
        self.verbose_tools
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
        let expandable = self.reasoning_status_enabled();
        self.open_activity_status((!expandable).then_some("Working"), expandable);
    }

    fn open_activity_status(&mut self, label: Option<&str>, show_reasoning_hint: bool) {
        if self.active_reasoning.is_some() {
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
        let mut status = AssistantBlock::streaming_reasoning("").with_model_lab(model_lab);
        status.reasoning_heading = label.map(str::to_owned);
        status.show_reasoning_hint = show_reasoning_hint;
        self.push_block(TranscriptBlock::Reasoning(Box::new(status)));
        self.active_reasoning = Some(index);
        self.register_active_event(index);
    }

    fn close_activity_status(&mut self, label: &str) {
        let Some(index) = self.active_reasoning else {
            return;
        };
        let matches = matches!(
            self.transcript.get(index),
            Some(TranscriptBlock::Reasoning(reasoning))
                if reasoning.text.is_empty()
                    && !reasoning.show_reasoning_hint
                    && reasoning.reasoning_heading.as_deref() == Some(label)
        );
        if matches {
            self.remove_transient_activity_block(index);
        }
    }

    fn append_text_block(&mut self, channel: OutputChannel, text: &str) {
        if channel == OutputChannel::Text {
            if let Some(index) = self.active_reasoning {
                let transient = matches!(
                    self.transcript.get(index),
                    Some(TranscriptBlock::Reasoning(reasoning))
                        if reasoning.text.is_empty()
                );
                if transient {
                    self.remove_transient_activity_block(index);
                } else {
                    self.active_reasoning = None;
                    self.unregister_active_event(index);
                    if let Some(TranscriptBlock::Reasoning(reasoning)) =
                        self.transcript.get_mut(index)
                    {
                        reasoning.finish_reasoning();
                        self.touch_block(index);
                    }
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
                    if existing.text.is_empty()
                        && !existing.show_reasoning_hint
                        && existing.reasoning_heading.as_deref() == Some("Working")
                    {
                        // A reasoning-off placeholder is still truthful before
                        // first output. If the provider nevertheless emits a
                        // private trace, promote it to the normal expandable
                        // reasoning presentation.
                        existing.reasoning_heading = None;
                        existing.show_reasoning_hint = true;
                    }
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
            OutputChannel::Reasoning => {
                self.active_reasoning = Some(index);
                self.register_active_event(index);
            }
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
            self.unregister_active_event(index);
            self.transcript.remove(index);
            self.transcript_commit_ids.remove(index);
            self.block_revisions.remove(index);
            self.reindex_active_events_after_removal(index);
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
            self.unregister_active_event(index);
            let transient = matches!(
                self.transcript.get(index),
                Some(TranscriptBlock::Reasoning(reasoning)) if reasoning.text.is_empty()
            );
            if transient {
                self.remove_transient_activity_block(index);
            } else if let Some(TranscriptBlock::Reasoning(reasoning)) =
                self.transcript.get_mut(index)
            {
                reasoning.finish_reasoning();
                self.touch_block(index);
            }
        }
    }

    fn has_active_event_dot(&self) -> bool {
        self.active_event_blocks
            .iter()
            .any(|index| match self.transcript.get(*index) {
                Some(TranscriptBlock::Reasoning(reasoning)) => {
                    !self.verbose_tools && !reasoning.finished && !reasoning.reasoning_expanded
                }
                Some(TranscriptBlock::Tool(panel)) => !panel.finished,
                Some(TranscriptBlock::Shell(shell)) => shell.running,
                _ => false,
            })
    }

    fn advance_event_dot_animation(&mut self) {
        if !self.has_active_event_dot() {
            return;
        }
        self.event_dot_visible = !self.event_dot_visible;
        for position in 0..self.active_event_blocks.len() {
            let index = self.active_event_blocks[position];
            let visible = match self.transcript.get(index) {
                Some(TranscriptBlock::Reasoning(reasoning)) => {
                    !self.verbose_tools && !reasoning.finished && !reasoning.reasoning_expanded
                }
                Some(TranscriptBlock::Tool(panel)) => !panel.finished,
                Some(TranscriptBlock::Shell(shell)) => shell.running,
                _ => false,
            };
            if visible {
                self.touch_block(index);
            }
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

fn prompt_marker(theme: &YggTheme) -> &str {
    theme.glyph("prompt")
}

pub(crate) fn semantic_separator(theme: &YggTheme) -> &str {
    theme.glyph("separator")
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
    // User text crosses a system boundary before Markdown parsing. Strip
    // complete terminal protocols here as well as in the semantic copy path;
    // otherwise the rich renderer safely exposes an escape sequence as text
    // while content-width planning measures the shorter stripped projection.
    let safe_text = sanitize_for_terminal(text);
    let document = parse_markdown(&safe_text);
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

#[allow(dead_code)]
fn prompt_cursor(_theme: &YggTheme) -> &'static str {
    CURSOR_MARKER
}

pub(crate) fn fit_line(line: &str, width: u16) -> String {
    let width = usize::from(width);
    if visible_width(line) <= width {
        return line.to_owned();
    }
    let truncated = sexy_tui_rs::truncate_to_width(line, width, Some(""));
    if line.contains('\x1b') {
        truncated
    } else {
        // The generic ANSI-aware truncator closes style state even when its
        // input was plain. Do not introduce an orphan reset into no-color
        // transcript output.
        strip_terminal_sequences(&truncated)
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
        tui.add_child(Box::new(ShellComponent::new(state.clone(), false)));
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
        let model = state.model.clone();
        let id = state
            .run
            .begin_route(&provider_status, provider, model)
            .expect("a new prompt is accepted only after the previous run terminates");
        state.open_reasoning_status();
        id
    }

    pub fn current_run_id(&self) -> Option<RunId> {
        self.state.borrow().run.current_id()
    }

    pub fn current_run_route(&self) -> Option<(String, String)> {
        self.state
            .borrow()
            .run
            .current()
            .map(|run| (run.endpoint().to_owned(), run.model().to_owned()))
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
        state.close_streaming_blocks();
        let tokens_per_second = state
            .last_turn_tokens_per_second
            .filter(|rate| rate.is_finite() && *rate > 0.0);
        state.push_block(TranscriptBlock::Outcome(OutcomeBlock::new(
            outcome,
            tokens_per_second,
        )));
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
            AgentEvent::OutputMedia { .. } => {
                // Generated media is binary and may still be invalidated by a
                // provider retry. TurnFinished carries the durable assembled
                // message; embedded callers receive the payload here directly.
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
            AgentEvent::FollowUpDelivered { messages } => {
                state.close_streaming_blocks();
                let model_lab = state.executing_model_lab();
                let prompt_color = state.executing_prompt_color();
                for message in messages {
                    state.push_block(TranscriptBlock::User {
                        text: message.clone(),
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
                state.open_activity_status(Some("Compacting context"), false);
                state.turn_generation_started_at = None;
                state.turn_streamed_output_bytes = 0;
            }
            AgentEvent::CompactionFinished { reason, result } => {
                state.run_label.clear();
                state.close_activity_status("Compacting context");
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
                state.register_active_event(index);
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
                    state.unregister_active_event(index);
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
            && composer::active_mention(&state.editor)
                .is_some_and(|query| !composer::is_path_query(query))
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
        // update_status re-asserts the workspace after every turn. Historic
        // tool summaries and their cached layouts only change with the root.
        if state.workspace.as_deref() == Some(root.as_path()) {
            return;
        }
        state.file_index = None;
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

    pub fn set_skill_commands(&mut self, commands: Arc<[(String, String)]>) {
        let mut state = self.state.borrow_mut();
        state.skill_commands = commands;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    pub fn set_extension_commands(&mut self, commands: Arc<[(String, String)]>) {
        let mut state = self.state.borrow_mut();
        state.extension_commands = commands;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    /// Complete the trailing path token. `@` mentions retain their attachment
    /// behavior; literal paths are inserted as text. Directory completions omit
    /// the trailing space so another Tab can descend into them.
    pub fn complete_path(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor_cursor != state.editor.len() {
            return;
        }
        let Some(root) = state.workspace.clone() else {
            return;
        };

        if let Some(query) = composer::active_mention(&state.editor).map(str::to_owned) {
            let suggestion = if composer::is_path_query(&query) {
                composer::path_matches(&root, &query, 1).into_iter().next()
            } else {
                if state.file_index.is_none() {
                    state.file_index = Some(composer::workspace_files(&root, 10_000));
                }
                let top = {
                    let files = state.file_index.as_ref().expect("file index just built");
                    composer::mention_matches(files, &query, 1)
                        .first()
                        .copied()
                        .map(str::to_owned)
                };
                top.map(|completion| composer::PathSuggestion {
                    path: root.join(&completion),
                    completion,
                    is_dir: false,
                })
            };
            let Some(suggestion) = suggestion else {
                return;
            };
            let token_start = state.editor.len() - (query.len() + 1);

            if suggestion.is_dir {
                state
                    .editor
                    .replace_range(token_start.., &format!("@{}", suggestion.completion));
            } else if composer::media_kind_for_path(&suggestion.path).is_some() {
                let modalities = state.input_modalities;
                match state.ledger.attach_media(&suggestion.path, modalities) {
                    Ok(chip) => state.editor.replace_range(token_start.., &chip),
                    Err(error) => {
                        state.push_block(TranscriptBlock::Notice(error.to_string()));
                        state
                            .editor
                            .replace_range(token_start.., &format!("@{} ", suggestion.completion));
                    }
                }
            } else if composer::file_kind_for_path(&suggestion.path).is_some() {
                match state.ledger.attach_file_reference(&suggestion.path) {
                    Ok(chip) => state.editor.replace_range(token_start.., &chip),
                    Err(error) => {
                        state.push_block(TranscriptBlock::Notice(error.to_string()));
                        state
                            .editor
                            .replace_range(token_start.., &format!("@{} ", suggestion.completion));
                    }
                }
            } else {
                state
                    .editor
                    .replace_range(token_start.., &format!("@{} ", suggestion.completion));
            }
            state.editor_cursor = state.editor.len();
            return;
        }

        let Some(query) = composer::active_path(&state.editor).map(str::to_owned) else {
            return;
        };
        let Some(suggestion) = composer::path_matches(&root, &query, 1).into_iter().next() else {
            return;
        };
        let token_start = state.editor.len() - query.len();
        let suffix = if suggestion.is_dir { "" } else { " " };
        state
            .editor
            .replace_range(token_start.., &format!("{}{suffix}", suggestion.completion));
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
            welcome_card::restart_welcome_animation(&mut state);
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
        let was_compacting = state.run_label == "compacting";
        let run_label = if label == "idle" || label.starts_with("run:") {
            String::new()
        } else {
            label
                .trim_end_matches('…')
                .trim_end_matches("...")
                .to_owned()
        };
        if was_compacting && run_label != "compacting" {
            state.close_activity_status("Compacting context");
        }
        state.run_label = run_label;
        if state.run_label == "compacting" {
            state.open_activity_status(Some("Compacting context"), false);
        }
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

    /// Append a running shell command placeholder. The shared transcript
    /// activity marker supplies its restrained pulse.
    /// Returns the block id so the caller can update and finalize it.
    pub fn append_shell_in_progress(&mut self, command: String) -> String {
        let mut state = self.state.borrow_mut();
        state.event_dot_visible = true;
        let id = format!("shell-{}", state.transcript.len());
        let index = state.transcript.len();
        state.push_block(TranscriptBlock::Shell(Box::new(ShellOutput {
            id: id.clone(),
            command,
            output: String::new(),
            exit_code: 0,
            running: true,
        })));
        state.register_active_event(index);
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
            }
            state.unregister_active_event(index);
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
        welcome_card::restart_welcome_animation(&mut state);
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
        state.active_event_blocks.clear();
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
                    result.push_str(&format!("{:?}", outcome.outcome));
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

mod assistant_block;
mod bash_render;
mod editor_layout;
mod input_overlays;
mod native_scrollback;
mod outcome_render;
mod panel_render;
mod reasoning_render;
mod renderer_runtime;
mod shell_chrome;
mod status_telemetry;
mod surface_frame;
mod surface_layout;
mod terminal_text;
mod tool_render;
mod transcript_cache;
mod transcript_commit;
mod transcript_history;
mod transcript_hydration;
mod transcript_render;
mod transcript_selection;
mod viewport;
mod welcome_card;

#[cfg(test)]
mod tests;
