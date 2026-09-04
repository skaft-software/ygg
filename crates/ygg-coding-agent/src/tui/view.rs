#![allow(missing_docs)]

use std::cell::{Cell, Ref, RefCell};
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
    parse_markdown, strip_terminal_sequences, visible_width, wrap_text_with_ansi, ImageAnchor,
    ImageCapabilities, ImagePlanner, ImageRegistry, ImageViewport, RichRenderer, TextEditor, TUI,
};
use ygg_agent::{
    AgentEvent, EntryValue, OutputChannel, Session, ToolProgress, ToolProgressDecoration,
};
use ygg_ai::{ModalitySet, Model, ModelId, ToolCallId, Usage};

use crate::config::Config;
use crate::hydrate::{
    hydrate_transcript_at_with_image_budget, hydrate_transcript_tail_with_image_budget,
    project_tool_images, tool_image_limits, ToolImageBudget, ToolImagePlaceholder, ToolResultImage,
};
#[cfg(test)]
use crate::presentation::summarize_tool;
use crate::presentation::{
    provider_lifecycle_label, summarize_tool_with_workspace, tool_failure_reason,
    tool_result_is_failure, ModelDisplayMetadata, PriceDisplay, RunId, RunOutcome, RunTracker,
    ToolDisplay,
};
use crate::session_store::SessionMeta;
use crate::tui::composer::{self, ComposedInput};
use crate::tui::composer_surface::{
    composer_editor_geometry, ComposerEditorCache, ComposerEditorGeometry,
    ComposerEditorProjection, ComposerEditorSource,
};
use crate::tui::keymap::{EditAction, SlashMenuAction};
use crate::tui::terminal::{force_restore, TerminalImageStore, TerminalSize, YggTerminal};
#[cfg(test)]
use crate::tui::theme::ThemeSurfaceChrome;
use crate::tui::theme::{ModelLab, ThemeDensity, YggTheme};

#[cfg(test)]
use self::assistant_block::reasoning_heading_from_block;
use self::assistant_block::AssistantBlock;
use self::input_overlays::input_slash_suggestions;
#[cfg(test)]
use self::input_overlays::render_slash_suggestions;
#[cfg(test)]
use self::native_scrollback::{render_shell, render_shell_at, render_shell_update};
pub(crate) use self::ordinary_surface::{
    fit_prioritized_footer, footer_width, join_ordinary_metadata, render_ordinary_status,
    FooterSegment, OrdinarySurfaceLifecycle, OrdinarySurfaceMetadata,
};
use self::output_window::bounded_tail_rows;
#[cfg(test)]
pub(crate) use self::panel_render::panel_render_test_hook;
#[cfg(test)]
use self::panel_render::render_panel;
use self::panel_render::{filtered_indices, filtered_indices_for_action, session_picker_ordering};
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
    output_tokens_per_second, status_telemetry, styled_extension_output, styled_status_text,
    usage_cache_hit_rate_basis_points,
};
#[cfg(test)]
use self::surface_frame::event_margin_marker;
pub use self::terminal_text::bounded_live_append;
use self::terminal_text::{
    normalize_carriage_return_progress, sanitize_extension_tool_render_segments,
};
pub(crate) use self::terminal_text::{
    sanitize_for_terminal, sanitize_ordinary_surface_cell, EditorDisplayMap,
};
#[cfg(test)]
use self::tool_render::looks_like_diff;
use self::transcript_cache::TranscriptCache;
pub(crate) use self::transcript_document::delegated_session_document;
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
#[cfg(test)]
use self::viewport::{
    max_scroll_for_available, render_shell_viewport_at, render_shell_viewport_update,
};
use self::viewport::{
    max_scroll_from_bottom, resolved_scroll_from_bottom, transcript_lines,
    transcript_viewport_capacity, transcript_viewport_capacity_for_state,
};

const SUBAGENT_TOOL_NAMES: [&str; 4] = [
    "subagent_spawn",
    "subagent_status",
    "subagent_wait",
    "subagent_stop",
];

fn is_subagent_tool(name: &str) -> bool {
    SUBAGENT_TOOL_NAMES.contains(&name)
}

/// Maximum physical rows retained in a collapsed command-output tail.
const COMPACT_EXEC_OUTPUT_ROWS: usize = 5;
/// Maximum physical rows one inline tool image can reserve inside its tool card.
const MAX_TOOL_IMAGE_RENDER_ROWS: u16 = 16;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeTone {
    Success,
    Error,
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
    /// A neutral notice retained for compatibility with the many ordinary
    /// informational events. Approval/denial notices use `NoticeStatus` so
    /// their margin marker can carry the outcome without colouring the text.
    Notice(String),
    NoticeStatus {
        text: String,
        tone: NoticeTone,
    },
    Compaction(Box<CompactionBlock>),
}

/// Current opt-in image display mode copied into each retained tool panel.
/// It contains capability metadata only; image bytes stay in `ToolResultImage`.
#[derive(Clone, Copy, Debug)]
struct ToolImageRendering {
    enabled: bool,
    capabilities: ImageCapabilities,
}

impl Default for ToolImageRendering {
    fn default() -> Self {
        Self {
            enabled: false,
            capabilities: ImageCapabilities::forced(None, None),
        }
    }
}

#[derive(Clone, Debug)]
struct ToolPanel {
    id: ToolCallId,
    name: String,
    args: String,
    display: ToolDisplay,
    output: String,
    /// Validated opaque image media, kept separate from text/copy output.
    images: Vec<ToolResultImage>,
    image_rendering: ToolImageRendering,
    finished: bool,
    is_error: bool,
    /// Wall time the call took, known once the outcome is final.
    duration: Option<Duration>,
    failure_reason: Option<String>,
    /// Optional extension-owned semantic presentation. These are always plain,
    /// sanitized segments; roles are resolved against the current theme only
    /// while rendering. The durable provider-visible `output` stays intact.
    extension_render_segments: Vec<ygg_agent::extension_process::ToolRenderSegment>,
    /// One bounded, replaceable live-progress annotation. This is cleared once
    /// the immutable tool result arrives and never enters session persistence.
    progress_decoration: Option<ToolProgressDecoration>,
    /// Presentation-only delegated-worker event. It deliberately uses the
    /// ordinary tool block lifecycle so its margin dot, scrollback stability,
    /// and disclosure behavior match real tool calls without pretending that
    /// `subagents` was a provider tool invocation.
    subagent_activity: Option<SubagentActivityView>,
    /// Model family captured with the call for durable presentation
    /// provenance. Lifecycle chrome deliberately no longer consumes it:
    /// active, successful, and failed headers use muted, foreground, and
    /// error roles respectively.
    #[allow(dead_code)]
    model_lab: Option<crate::tui::theme::ModelLab>,
    /// Lazily cached diff scan. `None` means not yet computed.
    cached_diff: RefCell<Option<Option<String>>>,
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
            images: Vec::new(),
            image_rendering: ToolImageRendering::default(),
            finished,
            is_error,
            duration: None,
            failure_reason,
            extension_render_segments: Vec::new(),
            progress_decoration: None,
            subagent_activity: None,
            model_lab,
            cached_diff: RefCell::new(None),
            cached_disclosure_sensitive: RefCell::new(None),
        }
    }

    fn subagent_activity(view: &SubagentActivityView) -> Self {
        let mut panel = Self::new(
            ToolCallId("subagents".into()),
            "subagents".into(),
            "{}".into(),
            summarize_tool_with_workspace("subagents", &serde_json::json!({}), None),
            subagent_activity_copy_text(view),
            !subagent_activity_is_active(view),
            subagent_activity_has_failure(view),
            subagent_activity_failure_reason(view),
            None,
        );
        panel.subagent_activity = Some(view.clone());
        panel
    }

    fn update_subagent_activity(&mut self, view: &SubagentActivityView) {
        self.subagent_activity = Some(view.clone());
        self.finished = !subagent_activity_is_active(view);
        self.is_error = subagent_activity_has_failure(view);
        self.failure_reason = subagent_activity_failure_reason(view);
        self.output = subagent_activity_copy_text(view);
        self.cached_disclosure_sensitive.replace(None);
    }

    /// Produce visual image reservations only. The returned DCS anchor contains
    /// stable IDs/layout metadata but never protocol payload bytes; the terminal
    /// adapter resolves it through its private image store.
    fn image_rows(&self, width: u16) -> Vec<String> {
        self.images
            .iter()
            .flat_map(|image| {
                if !self.image_rendering.enabled {
                    return vec![image.fallback_text(true)];
                }
                let Some(id) = image.id() else {
                    return vec![image.fallback_text(false)];
                };
                let Some(terminal_image) = image.terminal_image() else {
                    return vec![image.fallback_text(false)];
                };
                let viewport = ImageViewport::with_capabilities(
                    width.max(1),
                    MAX_TOOL_IMAGE_RENDER_ROWS,
                    self.image_rendering.capabilities,
                )
                .expect("fixed nonzero tool image viewport is valid");
                let plan =
                    ImagePlanner::new(self.image_rendering.capabilities, tool_image_limits())
                        .plan_place(id, &terminal_image, viewport);
                match plan {
                    Ok(plan) => match (plan.terminal_command(), plan.layout()) {
                        (Some(command), Some(layout)) => {
                            let mut rows = plan.semantic_rows();
                            if let Some(first) = rows.first_mut() {
                                first.insert_str(
                                    0,
                                    &ImageAnchor::new(command.protocol(), id, layout).marker(),
                                );
                            }
                            rows
                        }
                        _ => plan.semantic_rows(),
                    },
                    Err(_) => vec![image.fallback_text(false)],
                }
            })
            .collect()
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
    /// Legacy one-shot text overlay retained for transient compatibility paths
    /// that do not yet own an ordinary report record.
    Text(String),
    /// A scrollable, semantic report using the same title/purpose/status/action
    /// contract as ordinary pickers without becoming a persistent dashboard.
    Report(ReportOverlay),
}

/// Body data for an ordinary report. Text is sanitized at the producer boundary;
/// only explicit, internally styled content may retain trusted theme ANSI.
#[derive(Clone, Debug)]
enum ReportBody {
    Text { text: String, styled: bool },
    Context(crate::tui::context::ContextReport),
}

/// Mutable presentation state for a report over the transcript viewport.
/// `scroll_from_top` starts at the report heading so help and accounting reports
/// retain their task context before a reader chooses to inspect later rows.
#[derive(Clone, Debug)]
struct ReportOverlay {
    surface: OrdinarySurfaceMetadata,
    body: ReportBody,
    scroll_from_top: usize,
}

/// Scope used by the session picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickerScope {
    Current,
    All,
}

impl PickerScope {
    pub(crate) fn toggle(self) -> Self {
        match self {
            Self::Current => Self::All,
            Self::All => Self::Current,
        }
    }
}

/// Ordering used by the session picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickerSort {
    Recent,
    Name,
    Messages,
}

impl PickerSort {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Recent => Self::Name,
            Self::Name => Self::Messages,
            Self::Messages => Self::Recent,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Name => "Name",
            Self::Messages => "Messages",
        }
    }
}

/// An operation requested by a view-only picker and executed by its driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PanelRequest {
    LoadAll,
    TrashSession {
        id: String,
        path: PathBuf,
    },
    RenameSession {
        id: String,
        path: PathBuf,
        name: String,
    },
}

/// Mutable state for the enhanced session picker.
#[derive(Clone, Debug)]
pub(crate) struct PickerState {
    pub(crate) rows: Vec<SessionMeta>,
    pub(crate) all_rows: Option<Vec<SessionMeta>>,
    pub(crate) scope: PickerScope,
    pub(crate) sort: PickerSort,
    pub(crate) named_only: bool,
    pub(crate) show_path: bool,
    pub(crate) filter: String,
    pub(crate) selected: usize,
    pub(crate) scroll: usize,
    pub(crate) confirming_delete: bool,
    /// The active rename buffer, when Ctrl+R has entered rename mode.
    pub(crate) rename: Option<String>,
    /// Typed ordinary title, purpose, and lifecycle state. Unlike the former
    /// free-form message tuple, rendering cannot infer tone from its wording.
    pub(crate) surface: OrdinarySurfaceMetadata,
    pub(crate) current_session_path: Option<PathBuf>,
}

impl PickerState {
    pub(crate) fn new(rows: Vec<SessionMeta>, current_session_path: Option<PathBuf>) -> Self {
        Self {
            rows,
            all_rows: None,
            scope: PickerScope::Current,
            sort: PickerSort::Recent,
            named_only: false,
            show_path: false,
            filter: String::new(),
            selected: 0,
            scroll: 0,
            confirming_delete: false,
            rename: None,
            surface: OrdinarySurfaceMetadata::with_purpose(
                "Resume Session",
                "Select a saved session to continue",
            ),
            current_session_path,
        }
    }

    pub(crate) fn active_rows(&self) -> &[SessionMeta] {
        match self.scope {
            PickerScope::Current => &self.rows,
            PickerScope::All => self.all_rows.as_deref().unwrap_or(&[]),
        }
    }
}

/// One user-message boundary available to `/fork`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForkMessage {
    pub(crate) entry_id: String,
    pub(crate) text: String,
    pub(crate) whole_conversation: bool,
}

/// State for the `/fork` message selector.
#[derive(Clone, Debug)]
pub(crate) struct MessagePicker {
    pub(crate) surface: OrdinarySurfaceMetadata,
    pub(crate) messages: Vec<ForkMessage>,
    pub(crate) selected: usize,
}

impl MessagePicker {
    pub(crate) fn new(messages: Vec<ForkMessage>) -> Self {
        Self {
            surface: OrdinarySurfaceMetadata::with_purpose(
                "Fork from Message",
                "Select a message to copy its path into a new session",
            ),
            selected: messages.len().saturating_sub(1),
            messages,
        }
    }
}

/// An interactive panel wedged between the transcript and composer.
/// Two horizontal rules delimit it; the interior renders form content.
#[derive(Clone, Debug)]
pub(crate) enum Panel {
    /// Select-list panel (model picker, session picker, or thinking picker).
    SelectList {
        surface: OrdinarySurfaceMetadata,
        items: Vec<String>,
        descriptions: Vec<Option<String>>,
        selected: usize,
        filter: String,
        /// What to do with the confirmed index.
        action: PanelAction,
    },
    /// Searchable session browser with lazy all-workspaces discovery.
    SessionPicker { picker: PickerState },
    /// User-message boundary picker used by `/fork`.
    MessagePicker { picker: MessagePicker },
    /// Scrollable, read-only document used for delegated worker transcripts.
    /// `styled` marks text that was already sanitized at the producing
    /// boundary and carries trusted theme ANSI; rendering must preserve it.
    ReadOnlyDocument {
        title: String,
        text: String,
        styled: bool,
        /// Visual rows retained below the current viewport tail.
        scroll_from_bottom: usize,
    },
}

/// What happens when the user confirms a panel selection.
#[derive(Clone, Debug)]
#[allow(dead_code, clippy::enum_variant_names)]
pub(crate) enum PanelAction {
    /// Select a model by id.
    SelectModel(Vec<ModelId>),
    /// Select a model while rendering non-selectable provider headings. The
    /// provider vector is aligned one-for-one with `models`; selection remains
    /// indexed only over models, never over headings.
    SelectGroupedModel {
        models: Vec<ModelId>,
        providers: Vec<String>,
    },
    /// Select a session by path.
    SelectSession(Vec<std::path::PathBuf>),
    /// Select a thinking level.
    SelectThinking(Vec<crate::config::ThinkingLevel>),
    /// Select a reasoning execution mode.
    SelectReasoningMode(Vec<ygg_ai::ReasoningMode>),
    /// Select an installed executable-extension bundle.
    SelectExtension(Vec<String>),
    /// Select one subagent presentation node.
    SelectSubagent(Vec<String>),
    /// Select one step in guided provider onboarding. Kept distinct from
    /// extension selection so ordinary-surface consumers retain the workflow
    /// purpose rather than inferring it from labels.
    ProviderSetup(Vec<String>),
    /// Drive the enhanced session browser without copying its row data.
    SessionPicker,
    /// Drive the user-message fork browser without copying its row data.
    MessagePicker,
    /// Navigate a read-only transcript document.
    ReadOnlyDocument,
    /// Confirm or deny a typed tool request.
    Confirmation,
}

impl PanelAction {
    pub(crate) fn is_model_picker(&self) -> bool {
        matches!(self, Self::SelectModel(_) | Self::SelectGroupedModel { .. })
    }

    pub(crate) fn model_provider_groups(&self) -> Option<&[String]> {
        match self {
            Self::SelectGroupedModel { providers, .. } => Some(providers),
            _ => None,
        }
    }
}

/// Outcome produced by closing a panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PanelResult {
    /// User confirmed the selection at the given index.
    Confirm(usize),
    /// User selected an outbox-backed item.
    Select(String),
    /// User cancelled (Esc).
    Cancel,
}

/// Result of dispatching a key to a transient report overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayInputResult {
    /// The legacy overlay owner retains its historical any-key dismissal path.
    Legacy,
    /// The report remains open after consuming navigation.
    Consumed,
    /// The report acknowledged dismissal and has closed itself.
    Closed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubagentActivityView {
    pub(crate) status_label: String,
    pub(crate) activities: Vec<ygg_agent::ExtensionPresentationActivity>,
    pub(crate) telemetry: Vec<ygg_agent::DelegationTelemetryChild>,
    pub(crate) failure_class: Option<String>,
    pub(crate) failure_reason: Option<String>,
    pub(crate) include_cost_in_session_total: bool,
}

fn subagent_activity_is_active(view: &SubagentActivityView) -> bool {
    if !view.telemetry.is_empty() {
        return view
            .telemetry
            .iter()
            .any(|child| matches!(child.state.as_str(), "pending" | "running"));
    }
    view.activities.iter().any(|activity| {
        matches!(
            activity.state,
            ygg_agent::ExtensionPresentationState::Loading
                | ygg_agent::ExtensionPresentationState::Pending
                | ygg_agent::ExtensionPresentationState::Active
                | ygg_agent::ExtensionPresentationState::Running
        )
    })
}

fn subagent_activity_has_failure(view: &SubagentActivityView) -> bool {
    view.failure_reason.is_some()
        || view.telemetry.iter().any(|child| {
            matches!(child.state.as_str(), "failed" | "cancelled" | "stopped")
                || child.failure_reason.is_some()
        })
        || view.activities.iter().any(|activity| {
            matches!(
                activity.state,
                ygg_agent::ExtensionPresentationState::Failed
                    | ygg_agent::ExtensionPresentationState::Cancelled
                    | ygg_agent::ExtensionPresentationState::Stopped
                    | ygg_agent::ExtensionPresentationState::Unavailable
            )
        })
}

fn subagent_activity_failure_reason(view: &SubagentActivityView) -> Option<String> {
    view.failure_reason.clone().or_else(|| {
        view.telemetry
            .iter()
            .find_map(|child| child.failure_reason.clone())
    })
}

/// Plain semantic text used by copy/width calculations for the presentation
/// block. The renderer owns styling and compact row selection.
pub(super) fn subagent_activity_copy_text(view: &SubagentActivityView) -> String {
    let mut lines = vec!["Subagents".to_owned()];
    if !view.telemetry.is_empty() {
        lines.extend(view.telemetry.iter().map(|child| {
            let state = if child.state.is_empty() {
                "running"
            } else {
                child.state.as_str()
            };
            format!(
                "{} · {state} · {} calls",
                child.task_name, child.tool_use_count
            )
        }));
    } else {
        lines.extend(
            view.activities
                .iter()
                .map(|activity| format!("{} · {:?}", activity.summary, activity.state)),
        );
    }
    if let Some(reason) = view.failure_reason.as_deref() {
        lines.push(format!("failed · {reason}"));
    }
    lines.join("\n")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewportAnchor {
    commit_id: u64,
    block_hint: usize,
    text_offset: usize,
    trailing_affinity: bool,
    fallback_block_row: usize,
    fallback_visual_row: usize,
    desired_screen_row: usize,
    semantic: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellExtensionUi {
    pub statuses: Vec<ShellExtensionUiLine>,
    pub above_editor: Vec<ShellExtensionUiLine>,
    pub below_editor: Vec<ShellExtensionUiLine>,
    pub working: Option<ShellExtensionWorking>,
    pub hidden_thinking_label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellExtensionUiLine {
    pub text: String,
    pub style_role: Option<String>,
    pub priority: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellExtensionWorking {
    pub message: Option<String>,
    pub visible: Option<bool>,
    pub frames: Option<Vec<String>>,
    pub interval_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellEditorSnapshot {
    pub text: String,
    pub cursor: usize,
    pub revision: u64,
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellAutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellAutocompleteOverlay {
    text: String,
    cursor: usize,
    revision: u64,
    prefix: String,
    items: Vec<ShellAutocompleteItem>,
}

#[derive(Default)]
pub(crate) struct ShellState {
    /// Active interactive panel, if any.
    pub(crate) panel: Option<Panel>,
    /// View-layer requests waiting for the picker driver to execute.
    pending_panel_requests: Vec<PanelRequest>,
    /// Selected session `(id, path)` produced by the session picker.
    picker_selection: Option<(String, PathBuf)>,
    /// Selected message `(entry id, text)` produced by the fork picker.
    message_picker_selection: Option<(String, String)>,
    pub(crate) theme: YggTheme,
    /// Whether this session uses explicit approval gates instead of full host access.
    pub(crate) safe_mode: bool,
    /// Opt-in terminal-image mode and conservative terminal capability state.
    image_rendering: ToolImageRendering,
    /// Monotonic image IDs and the private backend payload map stay separate
    /// from semantic transcript text.
    image_registry: ImageRegistry,
    terminal_images: TerminalImageStore,
    /// Session-wide retained image accounting shared by live and resumed tools.
    tool_image_budget: ToolImageBudget,
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
    /// Current frame in the optional theme-owned animated spinner.
    event_spinner_frame: usize,
    /// Independent frame for the model-adaptive `Working`/`Thinking` text
    /// shimmer. Keeping it separate cannot change tool-dot cadence.
    status_shimmer_frame: usize,
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
    /// Reusable text model for the normal composer draft and cursor.
    pub(crate) editor: TextEditor,
    /// Cached app-owned display mapping and generic visual layout for the
    /// active composer source. It is invalidated by the editor text revision,
    /// tool prompt revision, or chrome-aware text width; cursor motion updates
    /// only the structured projection over the shared rows.
    composer_editor_cache: RefCell<Option<ComposerEditorCache>>,
    /// Sticky desired visual column while moving vertically through the
    /// composer. Horizontal/edit actions clear it.
    composer_preferred_column: Option<usize>,
    /// Changes whenever the ephemeral tool prompt changes without mutating the
    /// normal editor draft.
    tool_input_revision: u64,
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
    /// Live state for the current root run. The corresponding presentation
    /// block is retained in the transcript after settlement.
    pub(crate) subagent_activity: Option<SubagentActivityView>,
    /// Transcript index of the current run's presentation-only subagent tool
    /// block. It is reset at the next root run so settled history remains
    /// immutable while a new delegation event gets its own row.
    pub(crate) subagent_activity_block: Option<usize>,
    slash_selection: usize,
    slash_scroll: usize,
    slash_popup_dismissed: bool,
    /// Current host-projected extension UI. It contains only semantic text and
    /// finite roles; rendering remains owned by the shell theme and layout.
    extension_ui: ShellExtensionUi,
    /// One bounded extension autocomplete result awaiting explicit host accept.
    extension_autocomplete: Option<ShellAutocompleteOverlay>,
    status_detail: String,
    pub(crate) error: Option<String>,
    overlay: Option<ShellOverlay>,
    tool_panels: HashMap<ToolCallId, usize>,
    active_text: Option<usize>,
    active_reasoning: Option<usize>,
    /// Once keyboard navigation requests a semantic viewport, rendering stays
    /// application-owned for the rest of this shell. Mouse capture remains an
    /// independent terminal-input policy.
    pub(crate) application_viewport_requested: bool,
    /// Distance from the live tail in visual rows. Kept for cheap wheel/page
    /// movement; `follow_tail` decides whether new output may change it.
    scroll_from_bottom: Cell<usize>,
    /// Stable semantic coordinate held at a fixed screen row while the
    /// application-owned viewport is away from the live tail. The commit ID
    /// survives prepends; the text offset survives streaming reflow and resize.
    viewport_anchor: Cell<Option<ViewportAnchor>>,
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
    /// When the most recent provider request for the current turn was
    /// opened (AgentEvent::TurnStarted). Anchors the first-token latency
    /// measurement and is refreshed on every retry.
    pub(crate) turn_requested_at: Option<Instant>,
    /// First-token latency of the most recently completed provider
    /// response: request opened until the first generated token.
    pub(crate) last_turn_first_token: Option<Duration>,
    /// Total provider time of the most recently completed provider
    /// response: request opened until the response was fully generated.
    pub(crate) last_turn_provider_elapsed: Option<Duration>,
    /// (tool name, wall time) of recently completed tool calls, most
    /// recent last. Session-scoped and bounded; powers the `/status`
    /// tool wall-time line.
    pub(crate) tool_durations: Vec<(String, Duration)>,
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
}

const STATUS_RAINBOW_DURATION: Duration = Duration::from_secs(2);

fn status_rainbow_strength_at(reasoning: Option<&str>, elapsed: Option<Duration>) -> u16 {
    if !matches!(reasoning, Some("max" | "ultra")) {
        return 0;
    }
    let Some(elapsed) = elapsed else {
        return 0;
    };
    let remaining = STATUS_RAINBOW_DURATION.saturating_sub(elapsed);
    let duration_ms = STATUS_RAINBOW_DURATION.as_millis();
    let remaining_ms = remaining.as_millis();
    (remaining_ms.saturating_mul(100) / duration_ms).min(100) as u16
}

/// Lifecycle labels are generated locally from a finite state enum. This
/// recognizer is used only to avoid retaining them as transcript history at a
/// turn boundary; it deliberately excludes unrelated activity labels.
fn is_provider_lifecycle_status(heading: &str) -> bool {
    let base = heading.split_once(" · ").map_or(heading, |(base, _)| base);
    base.starts_with("Loading ") || base.ends_with(" queued") || base.ends_with(" ready")
}

fn invalidate_extension_autocomplete(state: &mut ShellState) {
    state.extension_autocomplete = None;
}

fn normal_editor_focused(state: &ShellState) -> bool {
    state.panel.is_none() && state.overlay.is_none() && state.tool_input_prompt.is_none()
}

impl ShellState {
    /// Borrow the one app-owned safe display map and generic visual layout for
    /// the current composer source and chrome-aware text cell width.
    pub(crate) fn composer_editor_projection(
        &self,
        geometry: ComposerEditorGeometry,
    ) -> Ref<'_, ComposerEditorProjection> {
        let (source, text, cursor) = match &self.tool_input_prompt {
            Some(prompt) => (
                ComposerEditorSource::ToolPrompt(self.tool_input_revision),
                prompt.as_str(),
                prompt.len(),
            ),
            None => (
                ComposerEditorSource::Draft(self.editor.text_revision()),
                self.editor.text(),
                self.editor.cursor(),
            ),
        };
        let text_width = geometry.text_width();
        let needs_refresh = self
            .composer_editor_cache
            .borrow()
            .as_ref()
            .is_none_or(|cache| !cache.matches(source, text_width));
        if needs_refresh {
            self.composer_editor_cache
                .replace(Some(ComposerEditorCache::new(
                    source, text, cursor, text_width,
                )));
        } else {
            self.composer_editor_cache
                .borrow_mut()
                .as_mut()
                .expect("composer editor cache is initialized")
                .refresh_cursor(cursor);
        }
        Ref::map(self.composer_editor_cache.borrow(), |cache| {
            cache
                .as_ref()
                .expect("composer editor cache is initialized")
                .projection()
        })
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

    fn update_image_rendering(&mut self, enabled: bool, capabilities: ImageCapabilities) {
        self.image_rendering = ToolImageRendering {
            enabled,
            capabilities,
        };
        for block in &mut self.transcript {
            if let TranscriptBlock::Tool(panel) = block {
                panel.image_rendering = self.image_rendering;
            }
        }
        self.invalidate_transcript_layout();
    }

    /// Assign stable IDs and register only the opaque validated payloads for
    /// terminal placement. Registry exhaustion becomes a bounded text fallback.
    fn register_tool_images(&mut self, mut images: Vec<ToolResultImage>) -> Vec<ToolResultImage> {
        for image in &mut images {
            let Some(terminal_image) = image.terminal_image() else {
                continue;
            };
            match self.image_registry.place() {
                Ok(action) => {
                    let id = action.id();
                    image.set_id(id);
                    self.terminal_images.register(id, terminal_image);
                }
                Err(_) => {
                    image.replace_with_placeholder(ToolImagePlaceholder::TerminalRegistryLimit)
                }
            }
        }
        images
    }

    /// Retire IDs before their semantic anchors are replaced. `ImageRegistry`
    /// never reuses a retired value, so a replay cannot mistake stale terminal
    /// pixels for a newly hydrated image with the same logical position.
    fn retire_tool_image_ids(&mut self) {
        let registry = &mut self.image_registry;
        for block in &mut self.transcript {
            let TranscriptBlock::Tool(panel) = block else {
                continue;
            };
            for image in &mut panel.images {
                if let Some(id) = image.id() {
                    let _ = registry.delete(id);
                    image.clear_id();
                }
            }
        }
    }

    /// Discard terminal-owned payload references before replacing the logical
    /// transcript. The TUI still sees old Kitty anchors and performs its normal
    /// targeted/destructive cleanup during the next replay.
    fn reset_terminal_images(&mut self) {
        self.retire_tool_image_ids();
        self.terminal_images.clear();
        self.tool_image_budget = ToolImageBudget::default();
    }

    /// Rebuild IDs/store after deferred history materialization. The walk is
    /// chronological, so an active local tail consumes whatever remains after
    /// the durable snapshot and cannot exceed the same session budget.
    fn rebuild_terminal_images(&mut self) {
        self.retire_tool_image_ids();
        self.terminal_images.clear();
        let store = self.terminal_images.clone();
        let registry = &mut self.image_registry;
        let mut budget = ToolImageBudget::default();
        for block in &mut self.transcript {
            let TranscriptBlock::Tool(panel) = block else {
                continue;
            };
            for image in &mut panel.images {
                image.clear_id();
                let Some(bytes) = image.byte_len() else {
                    continue;
                };
                if let Err(reason) = budget.retain_existing(bytes) {
                    image.replace_with_placeholder(reason);
                    continue;
                }
                let Some(terminal_image) = image.terminal_image() else {
                    continue;
                };
                match registry.place() {
                    Ok(action) => {
                        let id = action.id();
                        image.set_id(id);
                        store.register(id, terminal_image);
                    }
                    Err(_) => {
                        image.replace_with_placeholder(ToolImagePlaceholder::TerminalRegistryLimit)
                    }
                }
            }
        }
        self.tool_image_budget = budget;
    }

    fn push_block(&mut self, mut block: TranscriptBlock) {
        if let TranscriptBlock::Tool(panel) = &mut block {
            panel.image_rendering = self.image_rendering;
        }
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

    fn insert_block(&mut self, index: usize, mut block: TranscriptBlock) {
        if let TranscriptBlock::Tool(panel) = &mut block {
            panel.image_rendering = self.image_rendering;
        }
        let index = index.min(self.transcript.len());
        let commit_id = self.next_transcript_commit_id.0;
        self.next_transcript_commit_id.0 = commit_id
            .checked_add(1)
            .expect("transcript commit identity space exhausted");
        self.transcript.insert(index, block);
        self.transcript_commit_ids.insert(index, commit_id);
        self.block_revisions.insert(index, 0);
        if !self.follow_tail {
            self.new_output_count = self.new_output_count.saturating_add(1);
        }
        for active in &mut self.active_event_blocks {
            if *active >= index {
                *active += 1;
            }
        }
        self.active_text = self
            .active_text
            .map(|active| active + usize::from(active >= index));
        self.active_reasoning = self
            .active_reasoning
            .map(|active| active + usize::from(active >= index));
        self.subagent_activity_block = self
            .subagent_activity_block
            .map(|active| active + usize::from(active >= index));
        for panel_index in self.tool_panels.values_mut() {
            if *panel_index >= index {
                *panel_index += 1;
            }
        }
        if let Some(selection) = &mut self.transcript_selection {
            selection.anchor.block += usize::from(selection.anchor.block >= index);
            selection.focus.block += usize::from(selection.focus.block >= index);
        }
        if let Some(anchor) = &mut self.pending_selection_anchor {
            anchor.block += usize::from(anchor.block >= index);
        }
        self.invalidate_transcript();
    }

    /// A transient empty thinking row is removed before the first event so the
    /// new block is immediately followed by the model's replacement thinking
    /// row, matching the tool-call presentation.
    fn set_subagent_activity(&mut self, view: SubagentActivityView) {
        self.subagent_activity = Some(view.clone());
        if let Some(index) = self.subagent_activity_block {
            if let Some(TranscriptBlock::Tool(panel)) = self.transcript.get_mut(index) {
                if panel.subagent_activity.is_some() {
                    let was_active = !panel.finished;
                    panel.update_subagent_activity(&view);
                    let is_active = !panel.finished;
                    if was_active && !is_active {
                        self.unregister_active_event(index);
                    } else if !was_active && is_active {
                        self.register_active_event(index);
                    }
                    self.touch_block(index);
                    return;
                }
            }
            self.subagent_activity_block = None;
        }

        if let Some(index) = self.active_reasoning {
            let empty = matches!(
                self.transcript.get(index),
                Some(TranscriptBlock::Reasoning(reasoning)) if reasoning.text.is_empty()
            );
            if empty {
                self.remove_transient_activity_block(index);
            }
        }

        let index = self.active_reasoning.unwrap_or(self.transcript.len());
        let panel = ToolPanel::subagent_activity(&view);
        let active = !panel.finished;
        self.insert_block(index, TranscriptBlock::Tool(Box::new(panel)));
        self.subagent_activity_block = Some(index);
        if active {
            self.register_active_event(index);
            // Keep the model-status row below the delegated event while the
            // child is alive. This is the same status the hidden spawn tool
            // would otherwise reopen after its ToolFinished event.
            self.open_working_status();
        }
    }

    fn reindex_subagent_activity_after_removal(&mut self, removed: usize) {
        self.subagent_activity_block = self.subagent_activity_block.and_then(|index| {
            if index == removed {
                None
            } else {
                Some(index.saturating_sub(usize::from(index > removed)))
            }
        });
    }

    pub(crate) fn jump_to_tail(&mut self) {
        self.scroll_from_bottom.set(0);
        self.viewport_anchor.set(None);
        self.follow_tail = true;
        self.new_output_count = 0;
    }

    fn clear_turn_telemetry(&mut self) {
        self.last_turn_usage = None;
        self.last_turn_tokens_per_second = None;
        self.last_turn_generation_elapsed = None;
        self.last_turn_generated_tokens = None;
        self.turn_generation_started_at = None;
        self.turn_requested_at = None;
        self.last_turn_first_token = None;
        self.last_turn_provider_elapsed = None;
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

    pub(crate) fn displayed_session_cost_microdollars(&self) -> Option<u64> {
        let live_subagent_cost = self
            .subagent_activity
            .as_ref()
            .filter(|view| view.include_cost_in_session_total)
            .and_then(|view| {
                if !view.telemetry.is_empty() {
                    view.telemetry.iter().try_fold(0u64, |total, child| {
                        Some(total.saturating_add(child.cost_microdollars?))
                    })
                } else {
                    view.activities.iter().try_fold(0u64, |total, activity| {
                        Some(total.saturating_add(activity.metrics?.cost_microdollars?))
                    })
                }
            });
        match (self.session_cost_microdollars, live_subagent_cost) {
            (Some(session), Some(delegated)) => Some(session.saturating_add(delegated)),
            (Some(session), None) => Some(session),
            (None, Some(delegated)) => Some(delegated),
            (None, None) => None,
        }
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
        let cache_truncated = self
            .transcript_cache
            .get_mut()
            .truncate_tail_block(index, self.transcript.len());
        self.unregister_active_event(index);
        self.reindex_subagent_activity_after_removal(index);
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
        if !cache_truncated {
            self.invalidate_transcript_layout();
        }
    }

    fn show_tool_details(&self, _block: &TranscriptBlock) -> bool {
        self.verbose_tools
    }

    fn open_working_status(&mut self) {
        self.open_activity_status(Some("Working"), false);
    }

    /// Replace the current transient liveness row with an opted-in provider
    /// readiness label. It remains presentation-only and is removed when real
    /// model output or a terminal run outcome arrives.
    fn set_provider_lifecycle_status(&mut self, label: String) {
        if let Some(index) = self.active_reasoning {
            if let Some(TranscriptBlock::Reasoning(reasoning)) = self.transcript.get_mut(index) {
                let replaceable = reasoning.text.is_empty()
                    && !reasoning.show_reasoning_hint
                    && reasoning
                        .reasoning_heading
                        .as_deref()
                        .is_some_and(|heading| {
                            heading == "Working" || is_provider_lifecycle_status(heading)
                        });
                if replaceable {
                    reasoning.reasoning_heading = Some(label);
                    self.touch_block(index);
                }
            }
            return;
        }
        self.open_activity_status(Some(&label), false);
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
        let activity_started_at = self.run.current().map(|run| run.started_at());
        self.event_dot_visible = true;
        self.event_spinner_frame = 0;
        self.status_shimmer_frame = 0;
        let mut status = AssistantBlock::streaming_reasoning("")
            .with_model_lab(model_lab)
            .with_activity_started_at(activity_started_at);
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
        // The first public response delta ends the reasoning phase. Move the
        // transient row behind the response rather than dropping liveness: a
        // provider can continue generating invisible tokens after visible text.
        if channel == OutputChannel::Text && self.active_text.is_none() {
            if let Some(index) = self.active_reasoning {
                let transient = matches!(
                    self.transcript.get(index),
                    Some(TranscriptBlock::Reasoning(reasoning)) if reasoning.text.is_empty()
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
                        && existing
                            .reasoning_heading
                            .as_deref()
                            .is_some_and(|heading| {
                                heading == "Working" || is_provider_lifecycle_status(heading)
                            })
                    {
                        // A generic or lifecycle request placeholder becomes
                        // `Thinking` only when the provider actually emits
                        // reasoning content.
                        existing.reasoning_heading = None;
                        existing.show_reasoning_hint = true;
                    }
                    existing.append_reasoning(text);
                    true
                }
                _ => false,
            };
            if updated {
                if channel == OutputChannel::Text {
                    self.register_active_event(index);
                }
                self.touch_block(index);
                if channel == OutputChannel::Text && self.run.is_active() {
                    self.open_working_status();
                }
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
        let activity_started_at = self.run.current().map(|run| run.started_at());
        self.event_dot_visible = true;
        self.event_spinner_frame = 0;
        self.status_shimmer_frame = 0;
        self.push_block(match channel {
            OutputChannel::Text => TranscriptBlock::Assistant(Box::new(
                AssistantBlock::streaming(text).with_model_lab(model_lab),
            )),
            OutputChannel::Reasoning => TranscriptBlock::Reasoning(Box::new(
                AssistantBlock::streaming_reasoning(text)
                    .with_model_lab(model_lab)
                    .with_activity_started_at(activity_started_at),
            )),
        });
        match channel {
            OutputChannel::Text => {
                self.active_text = Some(index);
                self.register_active_event(index);
                if self.run.is_active() {
                    self.open_working_status();
                }
            }
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
            self.reindex_subagent_activity_after_removal(index);
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
            self.unregister_active_event(index);
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

    /// Settle one provider turn without implying that the owning run is done.
    /// Public text keeps one trailing `Working` row while the provider remains
    /// active, so this boundary only finalizes the text and repairs a missing
    /// status. Only an authoritative run outcome removes that transient row.
    fn finish_turn_streaming_blocks(&mut self) {
        if let Some(index) = self.active_text.take() {
            self.unregister_active_event(index);
            if let Some(TranscriptBlock::Assistant(assistant)) = self.transcript.get_mut(index) {
                assistant.finish();
                self.touch_block(index);
            }
        }

        let working = self.active_reasoning.is_some_and(|index| {
            matches!(
                self.transcript.get(index),
                Some(TranscriptBlock::Reasoning(reasoning))
                    if reasoning.text.is_empty()
                        && !reasoning.show_reasoning_hint
                        && reasoning.reasoning_heading.as_deref().is_some_and(|heading| {
                            heading == "Working" || is_provider_lifecycle_status(heading)
                        })
            )
        });
        if working {
            if let Some(index) = self.active_reasoning {
                if let Some(TranscriptBlock::Reasoning(reasoning)) = self.transcript.get_mut(index)
                {
                    // Lifecycle labels are ephemeral; turn completion resumes
                    // the ordinary generic liveness row rather than retaining
                    // endpoint telemetry in the transcript.
                    reasoning.reasoning_heading = Some("Working".into());
                    self.touch_block(index);
                }
            }
        } else {
            if let Some(index) = self.active_reasoning.take() {
                self.unregister_active_event(index);
                if let Some(TranscriptBlock::Reasoning(reasoning)) = self.transcript.get_mut(index)
                {
                    reasoning.finish_reasoning();
                    self.touch_block(index);
                }
            }
            self.open_working_status();
        }
    }

    fn has_active_event_dot(&self) -> bool {
        let markers_enabled = self.theme.resolve::<bool>("margin_markers").unwrap_or(true);
        let thinking_spinner = self
            .theme
            .resolve::<bool>("thinking_spinner")
            .unwrap_or(false);
        self.active_event_blocks
            .iter()
            .any(|index| match self.transcript.get(*index) {
                Some(TranscriptBlock::Reasoning(_)) => false,
                Some(TranscriptBlock::Tool(panel)) => markers_enabled && !panel.finished,
                Some(TranscriptBlock::Shell(shell)) => markers_enabled && shell.running,
                _ => false,
            })
            && (markers_enabled || thinking_spinner)
    }

    fn status_rainbow_strength(&self) -> u16 {
        if !self.run.is_active() {
            return 0;
        }
        status_rainbow_strength_at(
            self.run_reasoning.as_deref(),
            self.run.current().map(|run| run.elapsed_at(Instant::now())),
        )
    }

    fn status_shimmer_active(&self, reasoning: &AssistantBlock) -> bool {
        reasoning.is_working_activity()
            || (!reasoning.text.is_empty()
                && !self.verbose_tools
                && !reasoning.finished
                && !reasoning.reasoning_expanded)
    }

    pub(crate) fn has_active_status_shimmer(&self) -> bool {
        self.active_event_blocks.iter().any(|index| {
            matches!(
                self.transcript.get(*index),
                Some(TranscriptBlock::Reasoning(reasoning))
                    if self.status_shimmer_active(reasoning)
            )
        })
    }

    pub(crate) fn advance_status_shimmer(&mut self) {
        if !self.has_active_status_shimmer() {
            return;
        }
        self.status_shimmer_frame = self.status_shimmer_frame.wrapping_add(1) % 12;
        let active = self
            .active_event_blocks
            .iter()
            .copied()
            .filter(|index| {
                matches!(
                    self.transcript.get(*index),
                    Some(TranscriptBlock::Reasoning(reasoning))
                        if self.status_shimmer_active(reasoning)
                )
            })
            .collect::<Vec<_>>();
        for index in active {
            self.touch_block(index);
        }
    }

    pub(crate) fn has_active_status_timer(&self) -> bool {
        self.active_event_blocks.iter().any(|index| {
            matches!(
                self.transcript.get(*index),
                Some(TranscriptBlock::Reasoning(reasoning))
                    if !reasoning.finished && reasoning.activity_started_at.is_some()
            )
        })
    }

    pub(crate) fn advance_status_timer(&mut self) {
        if !self.has_active_status_timer() {
            return;
        }
        let active = self
            .active_event_blocks
            .iter()
            .copied()
            .filter(|index| {
                matches!(
                    self.transcript.get(*index),
                    Some(TranscriptBlock::Reasoning(reasoning))
                        if !reasoning.finished && reasoning.activity_started_at.is_some()
                )
            })
            .collect::<Vec<_>>();
        for index in active {
            self.touch_block(index);
        }
    }

    /// Whether any live collapsed reasoning block currently shows the theme's
    /// braille thinking spinner. Themes without `thinking_spinner` keep the
    /// slow shared event-dot cadence.
    pub(crate) fn has_active_thinking_spinner(&self) -> bool {
        let thinking_spinner = self
            .theme
            .resolve::<bool>("thinking_spinner")
            .unwrap_or(false);
        thinking_spinner
            && !self.verbose_tools
            && self.active_event_blocks.iter().any(|index| {
                matches!(
                    self.transcript.get(*index),
                    Some(TranscriptBlock::Reasoning(reasoning))
                        if !reasoning.finished && !reasoning.reasoning_expanded
                )
            })
    }

    /// Advance only the braille thinking spinner. Unlike the shared event-dot
    /// cycle this never toggles the tool/shell dots, so a fast spinner frame
    /// rate does not hurry the rest of the transcript's pulse.
    pub(crate) fn advance_thinking_spinner(&mut self) {
        if !self.has_active_thinking_spinner() {
            return;
        }
        self.event_spinner_frame = self.event_spinner_frame.wrapping_add(1) % 10;
        let active = self
            .active_event_blocks
            .iter()
            .copied()
            .filter(|index| {
                matches!(
                    self.transcript.get(*index),
                    Some(TranscriptBlock::Reasoning(reasoning))
                        if !reasoning.finished && !reasoning.reasoning_expanded
                )
            })
            .collect::<Vec<_>>();
        for index in active {
            self.touch_block(index);
        }
    }

    fn advance_event_dot_animation(&mut self) {
        if !self.has_active_event_dot() {
            return;
        }
        self.event_dot_visible = !self.event_dot_visible;
        self.event_spinner_frame = self.event_spinner_frame.wrapping_add(1) % 10;
        for position in 0..self.active_event_blocks.len() {
            let index = self.active_event_blocks[position];
            let markers_enabled = self.theme.resolve::<bool>("margin_markers").unwrap_or(true);
            let visible = match self.transcript.get(index) {
                Some(TranscriptBlock::Reasoning(_)) => false,
                Some(TranscriptBlock::Tool(panel)) => markers_enabled && !panel.finished,
                Some(TranscriptBlock::Shell(shell)) => markers_enabled && shell.running,
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

/// Indent continuation rows to the first text cell after an activity marker.
pub(crate) const ACTIVITY_DETAIL_INDENT: &str = "  ";

/// A shared continuation mark for transient activity details. Keep steering
/// and collapsed thinking visually aligned without repurposing tree glyphs.
pub(crate) fn activity_elbow(theme: &YggTheme) -> &'static str {
    if theme.unicode() {
        "└"
    } else {
        "`-"
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
    let marker_glyph = sanitize_for_terminal(prompt_marker(theme));
    let marker_width = visible_width(&marker_glyph);
    let inner_width = width
        .saturating_sub(u16::try_from(marker_width.saturating_add(1)).unwrap_or(u16::MAX))
        .max(1);
    let safe_text = sanitize_for_terminal(text);
    let document = parse_markdown(&safe_text);
    let render_result = renderer.render(&document, inner_width);
    // New records use their exact persisted source colour. Legacy records may
    // derive the same small marker from historical model identity, but neither
    // path paints the prompt text or trailing terminal cells.
    let marker = if prompt_color.is_some() {
        theme.prompt_color_marker(prompt_color, &marker_glyph)
    } else {
        match model_lab.filter(|lab| *lab != ModelLab::Unknown) {
            Some(lab) => theme.model_fg(Some(lab), &marker_glyph),
            None => theme.fg("muted", &marker_glyph),
        }
    };
    let continuation_prefix = " ".repeat(marker_width.saturating_add(1));
    let mut lines = Vec::new();
    for (index, line) in render_result.lines.into_iter().enumerate() {
        let prefix = if index == 0 {
            format!("{marker} ")
        } else {
            continuation_prefix.clone()
        };
        let content = if theme.capabilities().color == crate::tui::terminal::ColorDepth::None {
            line.plain
        } else {
            line.styled
        };
        lines.push(fit_line(&format!("{prefix}{content}"), width));
    }
    if lines.is_empty() {
        lines.push(fit_line(&format!("{marker} "), width));
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
    output_indent: &str,
) -> Vec<String> {
    let normalized = normalize_carriage_return_progress(&shell.output);
    let safe = sanitize_for_terminal(&normalized);
    let mut output_rows = Vec::new();
    for line in safe.lines().filter(|line| !line.trim().is_empty()) {
        output_rows.extend(wrap_hanging(
            &understated_tool_output(theme, line),
            output_indent,
            output_indent,
            width,
        ));
    }
    if output_rows.is_empty() {
        let placeholder = if shell.running {
            "(waiting for output)"
        } else {
            "(no output)"
        };
        output_rows.extend(wrap_hanging(
            &understated_tool_output(theme, placeholder),
            output_indent,
            output_indent,
            width,
        ));
    }

    if verbose {
        return output_rows;
    }

    let ellipsis = if theme.unicode() { "…" } else { "..." };
    bounded_tail_rows(
        output_rows,
        COMPACT_EXEC_OUTPUT_ROWS,
        false,
        |hidden_rows| {
            let unit = if hidden_rows == 1 { "row" } else { "rows" };
            fit_line(
                &format!(
                    "{output_indent}{}",
                    understated_tool_output(
                        theme,
                        &format!("{ellipsis} {hidden_rows} earlier visual {unit} hidden")
                    )
                ),
                width,
            )
        },
    )
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
        let image_store = TerminalImageStore::default();
        let terminal = YggTerminal::enter_with_mouse_and_images(
            size.clone(),
            capture_mouse,
            image_store.clone(),
        )?;
        let image_capabilities = terminal.image_capabilities();
        let initial_size = *size.lock().expect("terminal size mutex poisoned");
        let state = SharedState::new(ShellState {
            theme,
            size: initial_size,
            follow_tail: true,
            application_viewport_requested: capture_mouse,
            startup_card_started_at: Some(Instant::now()),
            image_rendering: ToolImageRendering {
                enabled: false,
                capabilities: image_capabilities,
            },
            terminal_images: image_store,
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
                    false,
                )
            })?;

        Ok(Self {
            tui: None,
            state,
            size,
            render_tx: Some(render_tx),
            render_thread: Some(render_thread),
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
        tui.add_child(Box::new(ShellComponent::new(state.clone(), false)));
        tui.start();
        Self {
            tui: Some(tui),
            state,
            size,
            render_tx: None,
            render_thread: None,
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

    /// Temporarily leave raw primary-screen rendering while preserving shell state.
    /// OAuth uses this so the hosted verification code and browser fallback are
    /// visible in an ordinary terminal.
    pub fn suspend(&mut self) {
        self.stop_renderer();
        force_restore();
    }

    /// Re-enter the primary-screen renderer after a suspended operation.
    pub fn resume(&mut self) -> Result<()> {
        if self.render_thread.is_some() || self.tui.is_some() {
            return Ok(());
        }
        let image_store = self.state.borrow().terminal_images.clone();
        let terminal = YggTerminal::enter_with_mouse_and_images(
            self.size.clone(),
            self.capture_mouse,
            image_store,
        )?;
        let image_capabilities = terminal.image_capabilities();
        {
            let mut state = self.state.borrow_mut();
            let enabled = state.image_rendering.enabled;
            state.update_image_rendering(enabled, image_capabilities);
        }
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
                    true,
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
        // A delegation team is scoped to one owning run. Do not carry the
        // previous run's already-accounted worker costs into the new live
        // overlay while its first authoritative refresh is still pending.
        state.subagent_activity = None;
        state.subagent_activity_block = None;
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
        state.open_working_status();
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
            AgentEvent::ProviderLifecycle { lifecycle } => {
                // The run tracker accepts the event but deliberately refuses a
                // late readiness update once real output or tool work began.
                // Keep the mutable transcript status aligned with that phase.
                let visible = matches!(
                    state.run.current().map(|run| run.phase()),
                    Some(crate::presentation::RunPhase::ProviderLifecycle { .. })
                );
                if visible {
                    let provider = state
                        .run
                        .current()
                        .map(|run| run.endpoint())
                        .unwrap_or(state.provider.as_str())
                        .to_owned();
                    let label = provider_lifecycle_label(&provider, lifecycle);
                    state.set_provider_lifecycle_status(label);
                }
            }
            AgentEvent::ProviderRetry { .. } | AgentEvent::CandidateRejected { .. } => {
                state.discard_streaming_blocks();
                state.open_working_status();
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
                state.open_working_status();
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
                state.open_working_status();
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
                    state.open_working_status();
                }
            }
            AgentEvent::TurnStarted => {
                // A new provider attempt for this model turn is beginning;
                // the request is opened immediately after this event, so
                // this moment anchors the attempt's first-token latency.
                state.turn_requested_at = Some(Instant::now());
            }
            AgentEvent::ToolStarted { id, name, args } => {
                state.close_streaming_blocks();
                state.event_dot_visible = true;
                state.event_spinner_frame = 0;
                if !is_subagent_tool(name) {
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
            }
            // The tool panel retains the model-facing failure text; detailed
            // policy diagnostics are intentionally available through telemetry
            // and the native host protocol instead of the transcript.
            AgentEvent::ToolPolicyDecision { .. } => {}
            AgentEvent::ToolProgress { id, progress } => {
                let index = state.tool_panels.get(id).copied();
                let refreshes_compact_tail = matches!(
                    progress,
                    ToolProgress::Output { .. }
                        | ToolProgress::Status(_)
                        | ToolProgress::Decoration(_)
                        | ToolProgress::Dropped { .. }
                );
                if let Some(panel) = state.tool_output_mut(id) {
                    match progress {
                        ToolProgress::Output { bytes, .. } => {
                            bounded_live_append(&mut panel.output, &String::from_utf8_lossy(bytes));
                        }
                        ToolProgress::Status(message) => {
                            bounded_live_append(&mut panel.output, &format!("{message}\n"));
                        }
                        ToolProgress::Decoration(decoration) => {
                            panel.progress_decoration = Some(decoration.clone());
                        }
                        ToolProgress::Confirmation(request) => {
                            bounded_live_append(
                                &mut panel.output,
                                &format!("confirmation requested: {}\n", request.prompt),
                            );
                        }
                        ToolProgress::Input(_) => {}
                        ToolProgress::Dropped { bytes, events } => {
                            if *bytes > 0 {
                                bounded_live_append(
                                    &mut panel.output,
                                    &format!("... {bytes} bytes of live output elided ...\n"),
                                );
                            }
                            if *events > 0 {
                                bounded_live_append(
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
            AgentEvent::ToolFinished {
                id,
                result,
                duration,
            } => {
                let index = state.tool_panels.get(id).copied();
                let completed_images = if index.is_some() {
                    match result {
                        Ok(output) => project_tool_images(
                            output.content_parts().iter().filter_map(|part| match part {
                                ygg_agent::ToolOutputContentPart::Media(media) => Some(media),
                                ygg_agent::ToolOutputContentPart::Text(_) => None,
                            }),
                            &mut state.tool_image_budget,
                        ),
                        Err(_) => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                let completed_images = state.register_tool_images(completed_images);
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
                let mut completed_name = String::new();
                if let Some(panel) = state.tool_output_mut(id) {
                    completed_name = panel.name.clone();
                    panel.finished = true;
                    panel.duration = Some(*duration);
                    panel.is_error = tool_result_is_failure(&panel.name, result);
                    panel.failure_reason = tool_failure_reason(&panel.name, result);
                    panel.images = completed_images;
                    panel.progress_decoration = None;
                    match result {
                        Ok(output) => {
                            panel.display.mark_media_read(output.media_kinds());
                            panel.output.clear();
                            panel.output.push_str(&output.text);
                        }
                        Err(error) => {
                            panel.output.clear();
                            panel.output.push_str(&error.message);
                        }
                    }
                }
                if let Some(index) = index {
                    state.unregister_active_event(index);
                    state.touch_block(index);
                }
                if !completed_name.is_empty() {
                    state.tool_durations.push((completed_name, *duration));
                    if state.tool_durations.len() > 64 {
                        let excess = state.tool_durations.len() - 64;
                        state.tool_durations.drain(0..excess);
                    }
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
                    state.open_working_status();
                }
            }
            AgentEvent::TurnFinished {
                turn_usage,
                session_cost_microdollars,
                run_cost_microdollars,
                ..
            } => {
                state.finish_turn_streaming_blocks();
                let requested_at = state.turn_requested_at;
                if let Some(started_at) = state.turn_generation_started_at.take() {
                    let elapsed = started_at.elapsed();
                    state.last_turn_tokens_per_second =
                        output_tokens_per_second(turn_usage.output_tokens, elapsed);
                    state.last_turn_generation_elapsed = Some(elapsed);
                    state.last_turn_generated_tokens = Some(turn_usage.output_tokens);
                    state.last_turn_first_token = requested_at
                        .map(|requested| started_at.saturating_duration_since(requested));
                }
                state.last_turn_provider_elapsed = requested_at
                    .map(|requested| Instant::now().saturating_duration_since(requested));
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
            AgentEvent::DelegationUpdated { snapshot } => {
                // Keep the last non-empty snapshot as a transcript event. The
                // manager may publish an empty cleanup snapshot after settlement;
                // dropping it here would make the visual event disappear just
                // when the dot should settle green or red.
                if !snapshot.children.is_empty() || snapshot.failure_reason.is_some() {
                    state.set_subagent_activity(SubagentActivityView {
                        status_label: "Subagents".into(),
                        activities: Vec::new(),
                        telemetry: snapshot.children.clone(),
                        failure_class: snapshot.failure_class.clone(),
                        failure_reason: snapshot.failure_reason.clone(),
                        include_cost_in_session_total: true,
                    });
                }
            }
            AgentEvent::RunFinished { .. } => {
                state.close_streaming_blocks();
                if let Some(view) = state.subagent_activity.as_mut() {
                    // The root ledger is committed before RunFinished is
                    // emitted; from this boundary onward the footer must use
                    // the durable amount, not provisional child spend.
                    view.include_cost_in_session_total = false;
                }
            }
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
        let current = state.editor.take_text();
        state.editor.set_text(if current.trim().is_empty() {
            restored
        } else if restored.is_empty() {
            current
        } else {
            format!("{restored}\n\n{current}")
        });
        invalidate_extension_autocomplete(&mut state);
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
        let geometry = composer_editor_geometry(&state, state.size.0);
        let visual_navigation = matches!(
            &action,
            EditAction::Up | EditAction::Down | EditAction::Home | EditAction::End
        );

        if visual_navigation && state.tool_input_prompt.is_none() {
            // The display copy can differ from source when controls are
            // visualized or tabs use a row-relative width. Ask the cached
            // generic projection for visual movement, then map the trusted
            // display boundary back into the source editor.
            let mut preferred_column = state.composer_preferred_column;
            let target = {
                let projection = state.composer_editor_projection(geometry);
                projection.visual_source_target(&action, &mut preferred_column)
            };
            state.composer_preferred_column = preferred_column;
            if let Some(target) = target {
                state.editor.set_cursor(target);
            }
        } else {
            state.composer_preferred_column = None;
            match action {
                EditAction::Paste(text) => {
                    // Attachment policy remains shell-owned, but the reusable
                    // editor is the sole authority for normalized text insertion
                    // and cursor movement.
                    let pasted = TextEditor::normalize_paste(&text);
                    let inserted = match composer::classify_paste(&pasted) {
                        composer::PasteKind::Verbatim | composer::PasteKind::NonMediaFile(_) => {
                            pasted
                        }
                        composer::PasteKind::LargeText => state.ledger.attach_pasted_text(pasted),
                        composer::PasteKind::MediaFile(path) => {
                            let modalities = state.input_modalities;
                            match state.ledger.attach_media(&path, modalities) {
                                Ok(chip) => chip,
                                Err(error) => {
                                    state.push_block(TranscriptBlock::Notice(error.to_string()));
                                    pasted
                                }
                            }
                        }
                        composer::PasteKind::DocumentFile(path) => {
                            match state.ledger.attach_file_reference(&path) {
                                Ok(chip) => chip,
                                Err(error) => {
                                    state.push_block(TranscriptBlock::Notice(error.to_string()));
                                    pasted
                                }
                            }
                        }
                    };
                    state
                        .editor
                        .apply(EditAction::Paste(inserted), geometry.text_width());
                }
                action => {
                    state.editor.apply(action, geometry.text_width());
                }
            }
        }

        if resets_slash_menu {
            state.slash_selection = 0;
            state.slash_scroll = 0;
            state.slash_popup_dismissed = false;
        }
        if state.editor.cursor() == state.editor.text().len()
            && composer::active_mention(state.editor.text())
                .is_some_and(|query| !composer::is_path_query(query))
            && state.file_index.is_none()
        {
            if let Some(root) = state.workspace.clone() {
                state.file_index = Some(composer::workspace_files(&root, 10_000));
            }
        }
        invalidate_extension_autocomplete(&mut state);
    }

    /// Complete a unique slash-command prefix at the end of the prompt.
    pub fn complete_slash_command(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor.cursor() != state.editor.text().len() {
            return;
        }
        let suggestions = input_slash_suggestions(&state);
        if let [suggestion] = suggestions.as_slice() {
            let completed = format!(
                "/{}{}",
                suggestion.name,
                if suggestion.accepts_argument { " " } else { "" }
            );
            state.editor.set_text(completed);
            state.slash_popup_dismissed = true;
            invalidate_extension_autocomplete(&mut state);
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
        // Use the actual rendered popup viewport (excluding its one footer
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
                state.editor.set_text(format!(
                    "/{}{}",
                    command.name,
                    if command.accepts_argument { " " } else { "" }
                ));
                state.slash_popup_dismissed = true;
                invalidate_extension_autocomplete(&mut state);
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
        if state.prompt_templates.as_ref() == templates.as_ref() {
            return;
        }
        state.prompt_templates = templates;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    pub fn set_skill_commands(&mut self, commands: Arc<[(String, String)]>) {
        let mut state = self.state.borrow_mut();
        if state.skill_commands.as_ref() == commands.as_ref() {
            return;
        }
        state.skill_commands = commands;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    pub fn set_extension_commands(&mut self, commands: Arc<[(String, String)]>) {
        let mut state = self.state.borrow_mut();
        // Background extension polling republishes this snapshot on every tick;
        // an equivalent catalog must not reset the live popup cursor.
        if state.extension_commands.as_ref() == commands.as_ref() {
            return;
        }
        state.extension_commands = commands;
        state.slash_selection = 0;
        state.slash_scroll = 0;
    }

    #[allow(dead_code)]
    pub fn set_subagent_presentation(
        &mut self,
        snapshot: Option<&ygg_agent::ExtensionPresentationSnapshot>,
        include_cost_in_session_total: bool,
    ) -> bool {
        let next = snapshot.and_then(|snapshot| {
            let activities = snapshot
                .activities
                .iter()
                .filter(|activity| activity.kind == "subagent")
                .cloned()
                .collect::<Vec<_>>();
            (!activities.is_empty()).then_some(SubagentActivityView {
                status_label: "Subagents".into(),
                activities,
                telemetry: Vec::new(),
                failure_class: None,
                failure_reason: None,
                include_cost_in_session_total,
            })
        });
        let mut state = self.state.borrow_mut();
        if let Some(next) = next {
            if state.subagent_activity.as_ref() == Some(&next)
                && state.subagent_activity_block.is_some()
            {
                return false;
            }
            state.set_subagent_activity(next);
            return true;
        }
        if snapshot.is_none() {
            state.subagent_activity = None;
            state.subagent_activity_block = None;
        }
        false
    }

    /// Replace the host-owned delegation telemetry for the active root run.
    /// Unlike generic extension presentation, this path is fed directly by
    /// `AgentEvent::DelegationUpdated` and never polls a slash command.
    #[allow(dead_code)]
    pub fn set_subagent_telemetry(
        &mut self,
        snapshot: Option<&ygg_agent::DelegationTelemetrySnapshot>,
        include_cost_in_session_total: bool,
    ) -> bool {
        let next = snapshot.and_then(|snapshot| {
            (!snapshot.children.is_empty() || snapshot.failure_reason.is_some()).then_some(
                SubagentActivityView {
                    status_label: "Subagents".into(),
                    activities: Vec::new(),
                    telemetry: snapshot.children.clone(),
                    failure_class: snapshot.failure_class.clone(),
                    failure_reason: snapshot.failure_reason.clone(),
                    include_cost_in_session_total,
                },
            )
        });
        let mut state = self.state.borrow_mut();
        if let Some(next) = next {
            if state.subagent_activity.as_ref() == Some(&next)
                && state.subagent_activity_block.is_some()
            {
                return false;
            }
            state.set_subagent_activity(next);
            return true;
        }
        if snapshot.is_none() {
            state.subagent_activity = None;
            state.subagent_activity_block = None;
        }
        false
    }

    /// Complete one trailing mention or literal path at the end of the draft.
    /// Media and PDF completions remain composer policy and become attachment
    /// chips; literal paths are inserted as text. Directory completions omit
    /// the trailing space so another Tab can descend into them.
    pub fn complete_path(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor.cursor() != state.editor.text().len() {
            return;
        }
        let Some(root) = state.workspace.clone() else {
            return;
        };

        if let Some(query) = composer::active_mention(state.editor.text()).map(str::to_owned) {
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
            let token_start = state.editor.text().len() - (query.len() + 1);
            let replacement = if suggestion.is_dir {
                format!("@{}", suggestion.completion)
            } else if composer::media_kind_for_path(&suggestion.path).is_some() {
                let modalities = state.input_modalities;
                match state.ledger.attach_media(&suggestion.path, modalities) {
                    Ok(chip) => chip,
                    Err(error) => {
                        state.push_block(TranscriptBlock::Notice(error.to_string()));
                        format!("@{} ", suggestion.completion)
                    }
                }
            } else if composer::file_kind_for_path(&suggestion.path).is_some() {
                match state.ledger.attach_file_reference(&suggestion.path) {
                    Ok(chip) => chip,
                    Err(error) => {
                        state.push_block(TranscriptBlock::Notice(error.to_string()));
                        format!("@{} ", suggestion.completion)
                    }
                }
            } else {
                format!("@{} ", suggestion.completion)
            };
            let end = state.editor.text().len();
            let _ = state.editor.replace_range(token_start..end, &replacement);
            state.editor.move_to_end();
            invalidate_extension_autocomplete(&mut state);
            return;
        }

        let Some(query) = composer::active_path(state.editor.text()).map(str::to_owned) else {
            return;
        };
        let Some(suggestion) = composer::path_matches(&root, &query, 1).into_iter().next() else {
            return;
        };
        let token_start = state.editor.text().len() - query.len();
        let suffix = if suggestion.is_dir { "" } else { " " };
        let replacement = format!("{}{suffix}", suggestion.completion);
        let end = state.editor.text().len();
        let _ = state.editor.replace_range(token_start..end, &replacement);
        state.editor.move_to_end();
        invalidate_extension_autocomplete(&mut state);
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

            // Identity arrives before the full model registry metadata. Apply
            // the canonical lab immediately so the composer never flashes or
            // remains on the generic Ygg accent; `set_model_theme` can refine
            // this from API and endpoint metadata later.
            let model_lab = (!model.trim().is_empty())
                .then(|| crate::tui::theme::classify_model_identity(model, model, provider));
            if state.model_lab != model_lab {
                crate::tui::theme::apply_model_lab(
                    &mut state.theme,
                    model_lab.unwrap_or(crate::tui::theme::ModelLab::Unknown),
                );
                state.model_lab = model_lab;
                state.invalidate_rich_text();
            }
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
        *self.size.lock().expect("terminal size mutex poisoned") = (columns, rows);
        let mut state = self.state.borrow_mut();
        state.size = (columns, rows);
        // Deferred session history remains semantic and lazy. Resize reflows
        // only the materialized branch tail; PageUp/select-all loads older
        // blocks if and when the user asks for them.
        // Reflow belongs exclusively to `ygg-tui-render`. Computing the scroll
        // maximum here used to rebuild a long transcript on the input thread,
        // immediately discard that layout, and rebuild it again for paint.
        state.invalidate_transcript_layout();
    }

    pub fn theme(&self) -> YggTheme {
        self.state.borrow().theme.clone()
    }

    /// Content width inside the read-only panel's horizontal inset. Styled
    /// transcript producers render at this width so the panel never has to
    /// reflow already laid-out Markdown or tool surfaces.
    pub fn read_only_document_width(&self) -> u16 {
        let width = self.size.lock().expect("terminal size mutex poisoned").0;
        self::panel_render::document_content_width(&self.state.borrow().theme, width)
    }

    pub fn set_runtime_config(&mut self, config: Config) {
        let show_images = config.show_images;
        let mut state = self.state.borrow_mut();
        state.safe_mode = config.effect_policy != ygg_agent::EffectPolicy::UnsafeHost;
        state.max_session_cost_microdollars = config.max_cost_microdollars;
        drop(state);
        self.set_show_images(show_images);
    }

    /// Toggle opt-in inline image placement for the current interactive shell.
    /// The setting affects only visual reservations; transcript text, copy,
    /// plain/print output, and durable payload handling remain unchanged.
    pub fn set_show_images(&mut self, enabled: bool) {
        let mut state = self.state.borrow_mut();
        let capabilities = state.image_rendering.capabilities;
        state.update_image_rendering(enabled, capabilities);
    }

    /// Replace the complete host-projected semantic extension UI. The caller
    /// owns stale-generation filtering; this shell only retains data and keeps
    /// all terminal rendering/theme decisions host-side.
    pub fn set_extension_ui(&mut self, ui: ShellExtensionUi) -> bool {
        let mut state = self.state.borrow_mut();
        if state.extension_ui == ui {
            return false;
        }
        state.extension_ui = ui;
        true
    }

    /// Snapshot the normal host editor for a bounded extension handoff.
    pub fn extension_editor_snapshot(&self) -> ShellEditorSnapshot {
        let state = self.state.borrow();
        ShellEditorSnapshot {
            text: state.editor.text().to_owned(),
            cursor: state.editor.cursor(),
            revision: state.editor.revision(),
            focused: normal_editor_focused(&state),
        }
    }

    /// Replace editor text only while the ordinary host editor owns input.
    /// Attachments are deliberately cleared because extension text cannot refer
    /// to opaque attachment ledger entries.
    pub fn extension_set_editor(&mut self, text: String) -> ShellEditorSnapshot {
        let mut state = self.state.borrow_mut();
        if normal_editor_focused(&state) {
            state.editor.set_text(text);
            state.ledger.clear();
            state.slash_selection = 0;
            state.slash_scroll = 0;
            state.slash_popup_dismissed = false;
            invalidate_extension_autocomplete(&mut state);
        }
        ShellEditorSnapshot {
            text: state.editor.text().to_owned(),
            cursor: state.editor.cursor(),
            revision: state.editor.revision(),
            focused: normal_editor_focused(&state),
        }
    }

    /// Paste through the same host policy used for terminal bracketed paste.
    pub fn extension_paste_editor(&mut self, text: String) -> ShellEditorSnapshot {
        if self.extension_editor_snapshot().focused {
            self.apply_edit(EditAction::Paste(text));
        }
        self.extension_editor_snapshot()
    }

    /// Focus requests are advisory: exclusive host panels and tool-input
    /// pickers remain owners until they finish, so an extension cannot steal
    /// terminal input or bypass the keymap.
    pub fn extension_focus_editor(&self) -> ShellEditorSnapshot {
        self.extension_editor_snapshot()
    }

    /// Install a bounded autocomplete response only if the exact host snapshot
    /// that originated it is still current. This is the frontend half of the
    /// revision fence and rejects late/reordered extension replies.
    pub fn set_extension_autocomplete(
        &mut self,
        snapshot: &ShellEditorSnapshot,
        prefix: String,
        items: Vec<ShellAutocompleteItem>,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let current = {
            let editor = &state.editor;
            normal_editor_focused(&state)
                && editor.revision() == snapshot.revision
                && editor.text() == snapshot.text.as_str()
                && editor.cursor() == snapshot.cursor
                && snapshot.cursor >= prefix.len()
                && editor.text()[..snapshot.cursor].ends_with(&prefix)
        };
        if !current || items.is_empty() {
            return false;
        }
        state.extension_autocomplete = Some(ShellAutocompleteOverlay {
            text: snapshot.text.clone(),
            cursor: snapshot.cursor,
            revision: snapshot.revision,
            prefix,
            items,
        });
        true
    }

    /// Accept the first extension autocomplete choice through a normal host
    /// editor mutation. Selection/navigation remains host-owned for now.
    pub fn accept_extension_autocomplete(&mut self) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(overlay) = state.extension_autocomplete.clone() else {
            return false;
        };
        let current = {
            let editor = &state.editor;
            normal_editor_focused(&state)
                && editor.revision() == overlay.revision
                && editor.text() == overlay.text.as_str()
                && editor.cursor() == overlay.cursor
                && overlay.cursor >= overlay.prefix.len()
                && editor.text()[..overlay.cursor].ends_with(&overlay.prefix)
        };
        if !current {
            state.extension_autocomplete = None;
            return false;
        }
        let Some(item) = overlay.items.first() else {
            state.extension_autocomplete = None;
            return false;
        };
        let start = overlay.cursor - overlay.prefix.len();
        if !state
            .editor
            .replace_range(start..overlay.cursor, &item.value)
        {
            state.extension_autocomplete = None;
            return false;
        }
        invalidate_extension_autocomplete(&mut state);
        true
    }

    pub fn pending_is_empty(&self) -> bool {
        self.state.borrow().editor.is_empty()
    }

    pub fn pending(&self) -> String {
        self.state.borrow().editor.text().to_owned()
    }

    pub fn set_tool_input_prompt(&mut self, prompt: Option<String>) {
        let mut state = self.state.borrow_mut();
        state.tool_input_prompt = prompt.map(|prompt| {
            sanitize_for_terminal(&prompt)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        });
        state.tool_input_revision = state.tool_input_revision.saturating_add(1);
    }

    pub fn set_input_modalities(&mut self, modalities: ModalitySet) {
        self.state.borrow_mut().input_modalities = modalities;
    }

    /// Drain the editor and resolve chips into ordered parts.
    pub fn drain_composed(&mut self) -> ComposedInput {
        let mut state = self.state.borrow_mut();
        let mut text = state.editor.take_text();
        invalidate_extension_autocomplete(&mut state);

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
        state.editor.set_text(composed.display_text);
        state.ledger.restore(composed.attachments);
        invalidate_extension_autocomplete(&mut state);
    }

    /// Discard the current draft and every attachment it owns.
    pub fn clear_editor(&mut self) {
        let mut state = self.state.borrow_mut();
        state.editor.clear();
        state.ledger.clear();
        state.slash_selection = 0;
        state.slash_scroll = 0;
        state.slash_popup_dismissed = false;
        invalidate_extension_autocomplete(&mut state);
    }

    pub fn drain_editor(&mut self) -> String {
        let mut state = self.state.borrow_mut();
        state.slash_selection = 0;
        state.slash_scroll = 0;
        state.slash_popup_dismissed = false;
        let text = state.editor.take_text();
        invalidate_extension_autocomplete(&mut state);
        text
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
        state.viewport_anchor.set(None);
        if direction < 0 {
            state.application_viewport_requested = true;
        }
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
        state.viewport_anchor.set(None);
        if direction < 0 {
            state.application_viewport_requested = true;
        }
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
        let scroll = resolved_scroll_from_bottom(state, transcript.len(), chrome.transcript_rows);
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
            state.viewport_anchor.set(None);
            let maximum = max_scroll_from_bottom(&state, state.size.0);
            let next = state
                .scroll_from_bottom
                .get()
                .saturating_add(2)
                .min(maximum);
            state.scroll_from_bottom.set(next);
            state.follow_tail = next == 0;
        } else if usize::from(row) >= transcript_rows {
            state.viewport_anchor.set(None);
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
    ///
    /// `pbcopy`'s stdin write plus child wait block the caller, and this runs
    /// from the interactive event loop, so the process handoff happens on a
    /// detached thread. A detached thread (rather than `tokio::spawn`) keeps
    /// this method callable from the synchronous view API and its runtime-less
    /// unit tests. Errors stay ignored: `copy_buffer` remains authoritative.
    fn set_clipboard(text: &str) {
        #[cfg(target_os = "macos")]
        {
            let text = text.to_owned();
            let _ = std::thread::Builder::new()
                .name("clipboard-pbcopy".to_owned())
                .spawn(move || {
                    if let Ok(mut child) = std::process::Command::new("pbcopy")
                        .stdin(std::process::Stdio::piped())
                        .spawn()
                    {
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = stdin.write_all(text.as_bytes());
                        }
                        let _ = child.wait();
                    }
                });
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

    #[cfg(test)]
    fn copy_buffer(&self) -> Option<String> {
        self.state.borrow().copy_buffer.clone()
    }

    pub fn show_overlay_text(&mut self, text: String) {
        self.state.borrow_mut().overlay = Some(ShellOverlay::Text(sanitize_for_terminal(&text)));
    }

    fn show_report(&mut self, surface: OrdinarySurfaceMetadata, body: ReportBody) {
        self.state.borrow_mut().overlay = Some(ShellOverlay::Report(ReportOverlay {
            surface,
            body,
            scroll_from_top: 0,
        }));
    }

    /// Show a terminal-safe read-only report using ordinary title, purpose,
    /// status, and footer chrome. Report text remains outside transcript copy.
    pub fn show_report_text(
        &mut self,
        title: impl Into<String>,
        purpose: impl Into<String>,
        text: String,
    ) {
        self.show_report(
            OrdinarySurfaceMetadata::with_purpose(title, purpose),
            ReportBody::Text {
                text: sanitize_for_terminal(&text),
                styled: false,
            },
        );
    }

    /// Styled report text must already have been terminal-sanitized at its
    /// producing boundary. Only Ygg-owned theme SGR is retained while wrapping.
    pub fn show_styled_report_text(
        &mut self,
        title: impl Into<String>,
        purpose: impl Into<String>,
        text: String,
    ) {
        self.show_report(
            OrdinarySurfaceMetadata::with_purpose(title, purpose),
            ReportBody::Text { text, styled: true },
        );
    }

    /// Extension slash-command output, framed with heading chrome. The body
    /// is sanitized; only trusted theme styling added here survives.
    pub fn show_extension_output(&mut self, command: &str, text: String) {
        let mut state = self.state.borrow_mut();
        state.overlay = Some(ShellOverlay::Text(styled_extension_output(
            &state.theme,
            command,
            &text,
        )));
    }

    pub fn show_context_report(&mut self, report: crate::tui::context::ContextReport) {
        self.show_report(
            OrdinarySurfaceMetadata::with_purpose(
                "Context",
                "Review the estimated request context before the next turn",
            ),
            ReportBody::Context(report),
        );
    }

    /// Toggle the one global transcript disclosure mode (ctrl+o).
    pub fn toggle_disclosure(&mut self) {
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

    pub fn show_status_text_with_telemetry(&mut self, text: String) {
        let (theme, text) = {
            let state = self.state.borrow();
            let text = format!("{text}\n\n{}", status_telemetry(&state, Instant::now()));
            (state.theme.clone(), text)
        };
        self.show_styled_report_text(
            "Status",
            "Review active model, session, and safety diagnostics",
            styled_status_text(&theme, &text),
        );
    }

    pub fn close_overlay(&mut self) {
        self.state.borrow_mut().overlay = None;
    }

    pub fn has_overlay(&self) -> bool {
        self.state.borrow().overlay.is_some()
    }

    /// Let a shared report retain navigation while preserving the legacy
    /// one-shot overlay dismissal semantics for every other transient overlay.
    pub(crate) fn overlay_input(&mut self, event: &crossterm::event::Event) -> OverlayInputResult {
        let mut state = self.state.borrow_mut();
        if !matches!(state.overlay.as_ref(), Some(ShellOverlay::Report(_))) {
            return OverlayInputResult::Legacy;
        }
        let (maximum, page_rows) =
            self::viewport::report_scroll_metrics_for_state(&state).unwrap_or((0, 1));
        let mut close = false;
        if let crossterm::event::Event::Key(key) = event {
            if crate::tui::keymap::accepts_key_event(key) {
                let Some(ShellOverlay::Report(report)) = state.overlay.as_mut() else {
                    return OverlayInputResult::Legacy;
                };
                report.scroll_from_top = report.scroll_from_top.min(maximum);
                match key.code {
                    crossterm::event::KeyCode::Esc | crossterm::event::KeyCode::Left
                        if key.modifiers.is_empty() =>
                    {
                        close = true;
                    }
                    crossterm::event::KeyCode::Up if key.modifiers.is_empty() => {
                        report.scroll_from_top = report.scroll_from_top.saturating_sub(1);
                    }
                    crossterm::event::KeyCode::Down if key.modifiers.is_empty() => {
                        report.scroll_from_top =
                            report.scroll_from_top.saturating_add(1).min(maximum);
                    }
                    crossterm::event::KeyCode::PageUp if key.modifiers.is_empty() => {
                        report.scroll_from_top = report.scroll_from_top.saturating_sub(page_rows);
                    }
                    crossterm::event::KeyCode::PageDown if key.modifiers.is_empty() => {
                        report.scroll_from_top = report
                            .scroll_from_top
                            .saturating_add(page_rows)
                            .min(maximum);
                    }
                    crossterm::event::KeyCode::Home if key.modifiers.is_empty() => {
                        report.scroll_from_top = 0;
                    }
                    crossterm::event::KeyCode::End if key.modifiers.is_empty() => {
                        report.scroll_from_top = maximum;
                    }
                    _ => close = true,
                }
            }
        } else {
            // Legacy overlays dismiss on arbitrary input. A report retains
            // that familiar escape hatch for non-navigation events without
            // letting text leak into the composer behind it.
            close = true;
        }
        if close {
            state.overlay = None;
            OverlayInputResult::Closed
        } else {
            OverlayInputResult::Consumed
        }
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

    /// Drain view-owned picker operations for the driver that has store access.
    pub(crate) fn drain_panel_requests(&mut self) -> Vec<PanelRequest> {
        std::mem::take(&mut self.state.borrow_mut().pending_panel_requests)
    }

    /// Take the selected session after the panel has closed itself.
    pub(crate) fn take_picker_selection(&mut self) -> Option<(String, PathBuf)> {
        self.state.borrow_mut().picker_selection.take()
    }

    /// Take the selected fork boundary and the text to restore in the editor.
    pub(crate) fn take_message_picker_selection(&mut self) -> Option<(String, String)> {
        self.state.borrow_mut().message_picker_selection.take()
    }

    /// Replace session rows after discovery or a picker mutation while keeping
    /// the selected session stable when it still exists.
    pub(crate) fn refresh_panel_sessions(
        &mut self,
        rows: Vec<SessionMeta>,
        all_rows: Option<Vec<SessionMeta>>,
    ) {
        let mut state = self.state.borrow_mut();
        let Some(Panel::SessionPicker { picker }) = state.panel.as_mut() else {
            return;
        };
        let previous = session_picker_ordering(picker)
            .get(picker.selected)
            .and_then(|index| picker.active_rows().get(*index))
            .map(|meta| (meta.id.clone(), meta.path.clone()));
        let old_selected = picker.selected;
        picker.rows = rows;
        picker.all_rows = all_rows;
        let ordering = session_picker_ordering(picker);
        picker.selected = previous
            .and_then(|(id, path)| {
                ordering.iter().position(|index| {
                    picker
                        .active_rows()
                        .get(*index)
                        .is_some_and(|meta| meta.id == id && meta.path == path)
                })
            })
            .unwrap_or_else(|| old_selected.min(ordering.len().saturating_sub(1)));
        picker.scroll = 0;
        if picker.surface.lifecycle.is_loading() {
            picker.surface.lifecycle = OrdinarySurfaceLifecycle::Ready;
        }
    }

    /// Set an explicit semantic lifecycle status on the session picker.
    pub(crate) fn set_picker_lifecycle(&mut self, lifecycle: OrdinarySurfaceLifecycle) {
        let mut state = self.state.borrow_mut();
        if let Some(Panel::SessionPicker { picker }) = state.panel.as_mut() {
            picker.surface.lifecycle = lifecycle;
        }
    }

    /// Put a selected user message back into the ordinary composer.
    pub(crate) fn prefill_editor(&mut self, text: String) {
        let mut state = self.state.borrow_mut();
        state.editor.set_text(text);
        state.slash_selection = 0;
        state.slash_scroll = 0;
        state.slash_popup_dismissed = false;
        invalidate_extension_autocomplete(&mut state);
    }

    /// Replace a live subagent list without losing its filter or stable-node
    /// selection while presentation revisions arrive in the background.
    pub fn refresh_subagent_panel(
        &mut self,
        title: String,
        items: Vec<String>,
        descriptions: Vec<Option<String>>,
        node_ids: Vec<String>,
    ) {
        let mut state = self.state.borrow_mut();
        let Some(Panel::SelectList {
            surface: current_surface,
            items: current_items,
            descriptions: current_descriptions,
            selected,
            filter,
            action,
        }) = state.panel.as_mut()
        else {
            return;
        };
        let PanelAction::SelectSubagent(current_ids) = action else {
            return;
        };
        let current_raw = filtered_indices(current_items, current_descriptions, filter)
            .get(*selected)
            .copied();
        let current_id = current_raw
            .and_then(|index| current_ids.get(index))
            .cloned();

        current_surface.title = title;
        *current_items = items;
        *current_descriptions = descriptions;
        *current_ids = node_ids;
        let filtered = filtered_indices(current_items, current_descriptions, filter);
        *selected = current_id
            .as_ref()
            .and_then(|id| current_ids.iter().position(|candidate| candidate == id))
            .and_then(|raw| filtered.iter().position(|candidate| *candidate == raw))
            .unwrap_or_else(|| (*selected).min(filtered.len().saturating_sub(1)));
    }

    /// Replace the body of a read-only document without changing its title or
    /// scroll anchor. A reader browsing upward stays at that logical offset;
    /// a reader at the tail follows newly appended transcript rows. The text
    /// is sanitized; styled documents must use
    /// [`Self::update_read_only_document_styled`] instead.
    #[cfg(test)]
    pub fn update_read_only_document(&mut self, text: String) {
        self.update_read_only_document_inner(crate::tui::view::sanitize_for_terminal(&text));
    }

    /// Styled variant of [`Self::update_read_only_document`]: the text was
    /// already sanitized at the producing boundary and carries trusted theme
    /// ANSI, which must survive refreshes verbatim.
    pub fn update_read_only_document_styled(&mut self, text: String) {
        self.update_read_only_document_inner(text);
    }

    fn update_read_only_document_inner(&mut self, new_text: String) {
        let mut state = self.state.borrow_mut();
        let (old_text, old_scroll, styled) = match state.panel.as_ref() {
            Some(Panel::ReadOnlyDocument {
                text: current_text,
                scroll_from_bottom,
                styled,
                ..
            }) => (current_text.clone(), *scroll_from_bottom, *styled),
            _ => return,
        };
        let panel_rows = self::panel_render::render_panel_with_limit(
            &state,
            state.size.0,
            usize::from(state.size.1.max(5)).saturating_sub(4),
        )
        .len();
        let viewport_rows =
            self::panel_render::document_body_rows(&state, state.size.0, panel_rows);
        let old_max = self::panel_render::document_visual_row_count_styled(
            &old_text,
            &state.theme,
            state.size.0,
            styled,
        )
        .saturating_sub(viewport_rows);
        let new_max = self::panel_render::document_visual_row_count_styled(
            &new_text,
            &state.theme,
            state.size.0,
            styled,
        )
        .saturating_sub(viewport_rows);
        let old_top = old_max.saturating_sub(old_scroll.min(old_max));
        let new_scroll = if old_scroll == 0 {
            0
        } else {
            new_max.saturating_sub(old_top).min(new_max)
        };
        if let Some(Panel::ReadOnlyDocument {
            text: current_text,
            scroll_from_bottom,
            ..
        }) = state.panel.as_mut()
        {
            *current_text = new_text;
            *scroll_from_bottom = new_scroll;
        }
    }

    /// `Some((result, action))` when the panel has finished; `None` when
    /// the panel consumed the event but remains open.
    pub fn panel_input(
        &mut self,
        event: &crossterm::event::Event,
    ) -> Option<(PanelResult, PanelAction)> {
        let mut state = self.state.borrow_mut();
        let size = state.size;
        let base_page_step = usize::from(size.1).saturating_sub(8).max(1);
        let picker_layout =
            crate::tui::layout::PresentationLayout::new(&state.theme, size.0).picker;
        let stacked_picker = match state.panel.as_ref() {
            Some(Panel::SessionPicker { .. })
                if picker_layout == crate::tui::layout::PickerLayout::Stacked =>
            {
                true
            }
            Some(Panel::SelectList {
                action,
                descriptions,
                ..
            }) if action.is_model_picker()
                && picker_layout == crate::tui::layout::PickerLayout::Stacked
                && descriptions.iter().any(|description| {
                    description
                        .as_deref()
                        .is_some_and(|description| !description.is_empty())
                }) =>
            {
                true
            }
            _ => false,
        };
        // A stacked row consumes two terminal lines, so a page movement should
        // advance by the number of visible items rather than twice that amount.
        let page_step = if stacked_picker {
            base_page_step.div_ceil(2).max(1)
        } else {
            base_page_step
        };
        let rendered_panel = shell_chrome(&state, size.0, Instant::now()).panel;
        let visible_panel_rows = rendered_panel.len();
        let confirmation_render = match state.panel.as_ref() {
            Some(Panel::SelectList {
                action: PanelAction::Confirmation,
                ..
            }) => self::panel_render::confirmation_metadata_for_rendered_panel(
                &state,
                size.0,
                &rendered_panel,
            ),
            _ => None,
        };
        let document_page_step =
            self::panel_render::document_body_rows(&state, size.0, visible_panel_rows);
        let document_visual_rows = match state.panel.as_ref() {
            Some(Panel::ReadOnlyDocument { text, styled, .. }) => {
                self::panel_render::document_visual_row_count_styled(
                    text,
                    &state.theme,
                    size.0,
                    *styled,
                )
            }
            _ => 0,
        };
        let unicode = state.theme.unicode();
        let panel = state.panel.as_mut()?;
        // Snapshot the action before we potentially mutate/drop the panel.
        let action = match panel {
            Panel::SelectList { action, .. } => action.clone(),
            Panel::SessionPicker { .. } => PanelAction::SessionPicker,
            Panel::MessagePicker { .. } => PanelAction::MessagePicker,
            Panel::ReadOnlyDocument { .. } => PanelAction::ReadOnlyDocument,
        };
        let confirmation = matches!(&action, PanelAction::Confirmation);
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
                                let filtered = filtered_indices_for_action(
                                    items,
                                    descriptions,
                                    &action,
                                    filter,
                                );
                                if let Some(&index) = filtered.get(*selected) {
                                    if confirmation
                                        && !self::panel_render::confirmation_enter_allowed(
                                            confirmation_render.as_ref(),
                                            index,
                                            &items[index],
                                            unicode,
                                        )
                                    {
                                        return None;
                                    }
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
                                    < filtered_indices_for_action(
                                        items,
                                        descriptions,
                                        &action,
                                        filter,
                                    )
                                    .len()
                                {
                                    *selected += 1;
                                }
                            }
                            KeyCode::Home if key.modifiers.is_empty() => {
                                *selected = 0;
                            }
                            KeyCode::End if key.modifiers.is_empty() => {
                                *selected = filtered_indices_for_action(
                                    items,
                                    descriptions,
                                    &action,
                                    filter,
                                )
                                .len()
                                .saturating_sub(1);
                            }
                            KeyCode::PageUp if key.modifiers.is_empty() => {
                                *selected = selected.saturating_sub(page_step);
                            }
                            KeyCode::PageDown if key.modifiers.is_empty() => {
                                let last = filtered_indices_for_action(
                                    items,
                                    descriptions,
                                    &action,
                                    filter,
                                )
                                .len()
                                .saturating_sub(1);
                                *selected = selected.saturating_add(page_step).min(last);
                            }
                            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if *selected + 1
                                    < filtered_indices_for_action(
                                        items,
                                        descriptions,
                                        &action,
                                        filter,
                                    )
                                    .len()
                                {
                                    *selected += 1;
                                }
                            }
                            KeyCode::Char(c)
                                if !confirmation
                                    && !key.modifiers.intersects(
                                        KeyModifiers::CONTROL
                                            | KeyModifiers::ALT
                                            | KeyModifiers::SUPER,
                                    ) =>
                            {
                                filter.push(c);
                                // The match set changed; restart at the top.
                                *selected = 0;
                            }
                            KeyCode::Backspace if !confirmation && key.modifiers.is_empty() => {
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
            Panel::SessionPicker { picker } => {
                use crossterm::event::{Event, KeyCode, KeyModifiers};

                match event {
                    Event::Key(key) if crate::tui::keymap::accepts_key_event(key) => {
                        // Rename owns the complete key stream until it is
                        // committed or cancelled. This keeps ordinary picker
                        // shortcuts from mutating the name buffer.
                        if picker.rename.is_some() {
                            match key.code {
                                KeyCode::Esc if key.modifiers.is_empty() => {
                                    picker.rename = None;
                                    picker.surface.lifecycle = OrdinarySurfaceLifecycle::cancelled(
                                        "rename",
                                        Instant::now() + Duration::from_secs(2),
                                    );
                                }
                                KeyCode::Backspace if key.modifiers.is_empty() => {
                                    if let Some(rename) = picker.rename.as_mut() {
                                        rename.pop();
                                    }
                                }
                                KeyCode::Char(character)
                                    if !key.modifiers.intersects(
                                        KeyModifiers::CONTROL
                                            | KeyModifiers::ALT
                                            | KeyModifiers::SUPER,
                                    ) =>
                                {
                                    if let Some(rename) = picker.rename.as_mut() {
                                        rename.push(character);
                                    }
                                }
                                KeyCode::Enter if key.modifiers.is_empty() => {
                                    let name = picker
                                        .rename
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|name| !name.is_empty())
                                        .map(str::to_owned);
                                    if let Some(name) = name {
                                        let ordering = session_picker_ordering(picker);
                                        if let Some(index) = ordering.get(picker.selected).copied()
                                        {
                                            if let Some(meta) = picker.active_rows().get(index) {
                                                let request = PanelRequest::RenameSession {
                                                    id: meta.id.clone(),
                                                    path: meta.path.clone(),
                                                    name,
                                                };
                                                picker.rename = None;
                                                state.pending_panel_requests.push(request);
                                            }
                                        }
                                    }
                                }
                                _ => picker.rename = None,
                            }
                            return None;
                        }

                        // Delete confirmation intentionally ignores every key
                        // other than the two terminal decisions.
                        if picker.confirming_delete {
                            match key.code {
                                KeyCode::Enter if key.modifiers.is_empty() => {
                                    let ordering = session_picker_ordering(picker);
                                    let request = ordering
                                        .get(picker.selected)
                                        .and_then(|index| picker.active_rows().get(*index))
                                        .map(|meta| PanelRequest::TrashSession {
                                            id: meta.id.clone(),
                                            path: meta.path.clone(),
                                        });
                                    picker.confirming_delete = false;
                                    if let Some(request) = request {
                                        state.pending_panel_requests.push(request);
                                    }
                                }
                                KeyCode::Esc if key.modifiers.is_empty() => {
                                    picker.confirming_delete = false;
                                    picker.surface.lifecycle = OrdinarySurfaceLifecycle::cancelled(
                                        "delete",
                                        Instant::now() + Duration::from_secs(2),
                                    );
                                }
                                _ => {}
                            }
                            return None;
                        }

                        match key.code {
                            KeyCode::Tab if key.modifiers.is_empty() => {
                                picker.scope = picker.scope.toggle();
                                picker.selected = 0;
                                picker.scroll = 0;
                                if picker.scope == PickerScope::All && picker.all_rows.is_none() {
                                    picker.surface.lifecycle =
                                        OrdinarySurfaceLifecycle::loading("all workspaces");
                                    state.pending_panel_requests.push(PanelRequest::LoadAll);
                                }
                            }
                            KeyCode::Char('s') if key.modifiers == KeyModifiers::CONTROL => {
                                picker.sort = picker.sort.next();
                                picker.selected = 0;
                                picker.scroll = 0;
                            }
                            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                                picker.named_only = !picker.named_only;
                                picker.selected = 0;
                                picker.scroll = 0;
                            }
                            KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                                picker.show_path = !picker.show_path;
                            }
                            KeyCode::Char('r') if key.modifiers == KeyModifiers::CONTROL => {
                                let ordering = session_picker_ordering(picker);
                                if let Some(index) = ordering.get(picker.selected).copied() {
                                    if let Some(meta) = picker.active_rows().get(index) {
                                        picker.rename = Some(
                                            meta.name.clone().unwrap_or_else(|| meta.title.clone()),
                                        );
                                    }
                                }
                            }
                            KeyCode::Delete if key.modifiers.is_empty() => {
                                let ordering = session_picker_ordering(picker);
                                if let Some(index) = ordering.get(picker.selected).copied() {
                                    if let Some(meta) = picker.active_rows().get(index) {
                                        if picker.current_session_path.as_ref() == Some(&meta.path)
                                        {
                                            picker.surface.lifecycle =
                                                OrdinarySurfaceLifecycle::recoverable_error(
                                                    "cannot delete the currently active session",
                                                    Instant::now() + Duration::from_secs(3),
                                                );
                                        } else {
                                            picker.confirming_delete = true;
                                        }
                                    }
                                }
                            }
                            KeyCode::Esc if key.modifiers.is_empty() => {
                                drop(state);
                                self.close_panel();
                                return Some((PanelResult::Cancel, action));
                            }
                            KeyCode::Enter if key.modifiers.is_empty() => {
                                let ordering = session_picker_ordering(picker);
                                if let Some(index) = ordering.get(picker.selected).copied() {
                                    if let Some(meta) = picker.active_rows().get(index).cloned() {
                                        let selection = (meta.id.clone(), meta.path.clone());
                                        let result = PanelResult::Select(meta.id);
                                        state.picker_selection = Some(selection);
                                        drop(state);
                                        self.close_panel();
                                        return Some((result, action));
                                    }
                                }
                            }
                            KeyCode::Up if key.modifiers.is_empty() => {
                                picker.selected = picker.selected.saturating_sub(1);
                            }
                            KeyCode::Down if key.modifiers.is_empty() => {
                                let count = session_picker_ordering(picker).len();
                                if picker.selected + 1 < count {
                                    picker.selected += 1;
                                }
                            }
                            KeyCode::Home if key.modifiers.is_empty() => {
                                picker.selected = 0;
                            }
                            KeyCode::End if key.modifiers.is_empty() => {
                                picker.selected =
                                    session_picker_ordering(picker).len().saturating_sub(1);
                            }
                            KeyCode::PageUp if key.modifiers.is_empty() => {
                                picker.selected = picker.selected.saturating_sub(page_step);
                            }
                            KeyCode::PageDown if key.modifiers.is_empty() => {
                                let last = session_picker_ordering(picker).len().saturating_sub(1);
                                picker.selected =
                                    picker.selected.saturating_add(page_step).min(last);
                            }
                            KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => {
                                picker.selected = picker.selected.saturating_sub(1);
                            }
                            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                                let count = session_picker_ordering(picker).len();
                                if picker.selected + 1 < count {
                                    picker.selected += 1;
                                }
                            }
                            KeyCode::Char(character)
                                if !key.modifiers.intersects(
                                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                                ) =>
                            {
                                picker.filter.push(character);
                                picker.selected = 0;
                                picker.scroll = 0;
                            }
                            KeyCode::Backspace if key.modifiers.is_empty() => {
                                picker.filter.pop();
                                picker.selected = 0;
                                picker.scroll = 0;
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
            Panel::MessagePicker { picker } => {
                use crossterm::event::{Event, KeyCode, KeyModifiers};
                match event {
                    Event::Key(key) if crate::tui::keymap::accepts_key_event(key) => {
                        match key.code {
                            KeyCode::Esc if key.modifiers.is_empty() => {
                                drop(state);
                                self.close_panel();
                                return Some((PanelResult::Cancel, action));
                            }
                            KeyCode::Enter if key.modifiers.is_empty() => {
                                if let Some(message) = picker.messages.get(picker.selected).cloned()
                                {
                                    let selection = (message.entry_id.clone(), message.text);
                                    let result = PanelResult::Select(message.entry_id);
                                    state.message_picker_selection = Some(selection);
                                    drop(state);
                                    self.close_panel();
                                    return Some((result, action));
                                }
                            }
                            KeyCode::Up if key.modifiers.is_empty() => {
                                picker.selected = picker.selected.saturating_sub(1);
                            }
                            KeyCode::Down if key.modifiers.is_empty() => {
                                picker.selected = picker
                                    .selected
                                    .saturating_add(1)
                                    .min(picker.messages.len().saturating_sub(1));
                            }
                            KeyCode::Home if key.modifiers.is_empty() => picker.selected = 0,
                            KeyCode::End if key.modifiers.is_empty() => {
                                picker.selected = picker.messages.len().saturating_sub(1)
                            }
                            KeyCode::PageUp if key.modifiers.is_empty() => {
                                picker.selected = picker.selected.saturating_sub(page_step);
                            }
                            KeyCode::PageDown if key.modifiers.is_empty() => {
                                picker.selected = picker
                                    .selected
                                    .saturating_add(page_step)
                                    .min(picker.messages.len().saturating_sub(1));
                            }
                            KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => {
                                picker.selected = picker.selected.saturating_sub(1);
                            }
                            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                                picker.selected = picker
                                    .selected
                                    .saturating_add(1)
                                    .min(picker.messages.len().saturating_sub(1));
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
            Panel::ReadOnlyDocument {
                scroll_from_bottom, ..
            } => {
                use crossterm::event::{Event, KeyCode};
                let viewport_rows = document_page_step;
                let visual_rows = document_visual_rows;
                let maximum = visual_rows.saturating_sub(viewport_rows);
                *scroll_from_bottom = (*scroll_from_bottom).min(maximum);
                match event {
                    Event::Key(key) if crate::tui::keymap::accepts_key_event(key) => {
                        match key.code {
                            KeyCode::Esc | KeyCode::Left if key.modifiers.is_empty() => {
                                drop(state);
                                self.close_panel();
                                return Some((PanelResult::Cancel, action));
                            }
                            KeyCode::Up if key.modifiers.is_empty() => {
                                *scroll_from_bottom =
                                    scroll_from_bottom.saturating_add(1).min(maximum);
                            }
                            KeyCode::Down if key.modifiers.is_empty() => {
                                *scroll_from_bottom = scroll_from_bottom.saturating_sub(1);
                            }
                            KeyCode::PageUp if key.modifiers.is_empty() => {
                                *scroll_from_bottom = scroll_from_bottom
                                    .saturating_add(document_page_step)
                                    .min(maximum);
                            }
                            KeyCode::PageDown if key.modifiers.is_empty() => {
                                *scroll_from_bottom =
                                    scroll_from_bottom.saturating_sub(document_page_step);
                            }
                            KeyCode::Home if key.modifiers.is_empty() => {
                                *scroll_from_bottom = maximum;
                            }
                            KeyCode::End if key.modifiers.is_empty() => {
                                *scroll_from_bottom = 0;
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

    /// Add an informational event whose margin marker communicates success.
    pub fn notice_success(&mut self, message: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        state.push_block(TranscriptBlock::NoticeStatus {
            text: message.into(),
            tone: NoticeTone::Success,
        });
    }

    /// Add an informational event whose margin marker communicates denial or
    /// failure. The message itself remains neutral, like a tool event.
    pub fn notice_error(&mut self, message: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        state.push_block(TranscriptBlock::NoticeStatus {
            text: message.into(),
            tone: NoticeTone::Error,
        });
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
        // Native terminal history cannot be recoloured in place. Materialize a
        // deferred resume before the swap so the replay below includes every
        // persisted tool card, not just the first-paint tail.
        if let Err(error) = self.materialize_deferred_history() {
            self.state.borrow_mut().error = Some(format!(
                "could not load older session history before theme change: {error}"
            ));
        }
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
        let (items, history_deferred, image_budget) = if self.capture_mouse {
            // Explicit application-owned mode can hydrate older rows when its
            // semantic viewport reaches the bounded first-paint tail.
            hydrate_transcript_tail_with_image_budget(session, entry_budget)?
        } else {
            // Pi's primary-screen renderer writes the complete logical frame.
            // Native terminal scrollback cannot prepend deferred rows later, so
            // materialize the active branch before rendering it.
            match session.head() {
                Some(head) => {
                    let (items, budget) = hydrate_transcript_at_with_image_budget(session, &head)?;
                    (items, false, budget)
                }
                None => (Vec::new(), false, ToolImageBudget::default()),
            }
        };
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
        state.reset_terminal_images();
        state.tool_image_budget = image_budget;
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
        // Delegation telemetry belongs to the previously active root run. A
        // session hydrate may reuse this shell, so clear the live pointer;
        // hydrated history never contains an executable worker event.
        state.subagent_activity = None;
        state.subagent_activity_block = None;
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
                TranscriptBlock::NoticeStatus { text, .. } => {
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
mod input_overlays;
mod native_scrollback;
mod ordinary_surface;
mod outcome_render;
mod output_window;
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
mod transcript_document;
mod transcript_history;
mod transcript_hydration;
mod transcript_render;
mod transcript_selection;
mod viewport;
mod welcome_card;

#[cfg(test)]
mod ordinary_surface_contract_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod extension_handoff_tests {
    use super::*;

    #[test]
    fn extension_autocomplete_is_revision_fenced_before_acceptance() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Char('@'));
        let snapshot = shell.extension_editor_snapshot();
        assert!(shell.set_extension_autocomplete(
            &snapshot,
            "@".into(),
            vec![ShellAutocompleteItem {
                value: "file".into(),
                label: "file".into(),
                description: None,
            }],
        ));

        shell.apply_edit(EditAction::Char('x'));
        assert!(!shell.accept_extension_autocomplete());
        assert_eq!(shell.pending(), "@x");

        let current = shell.extension_editor_snapshot();
        assert!(!shell.set_extension_autocomplete(
            &snapshot,
            "@".into(),
            vec![ShellAutocompleteItem {
                value: "stale".into(),
                label: "stale".into(),
                description: None,
            }],
        ));
        assert!(shell.set_extension_autocomplete(
            &current,
            "@x".into(),
            vec![ShellAutocompleteItem {
                value: "file".into(),
                label: "file".into(),
                description: None,
            }],
        ));
        assert!(shell.accept_extension_autocomplete());
        assert_eq!(shell.pending(), "file");
    }
}
