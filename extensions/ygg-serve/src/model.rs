//! Host, project, session, item, and resource DTOs.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bounds::{
    validate_json, validate_public_text, validate_serialized_size, ProtocolValidation,
    ValidationError, MAX_BOOTSTRAP_BYTES, MAX_ITEM_TEXT_BYTES, MAX_PROMPT_BYTES,
    MAX_SNAPSHOT_BYTES,
};
use crate::{
    ArtifactId, DurableEntryId, HostId, ItemId, ProjectId, RequestId, RunId, SessionId, SourceId,
    ThemeId, ThemeOption, TurnId, PROTOCOL_VERSION,
};

const MAX_PROJECTS: usize = 256;
const MAX_SESSION_SUMMARIES: usize = 2_000;
const MAX_SESSION_ITEMS: usize = 10_000;
const MAX_PENDING_REQUESTS: usize = 128;
const MAX_RESOURCES: usize = 2_048;
const MAX_TAGS: usize = 32;
const MAX_PLAN_STEPS: usize = 256;
const MAX_CHOICES: usize = 32;
const MAX_MODELS: usize = 256;
const MAX_REASONING_OPTIONS: usize = 32;
const MAX_THEMES: usize = 64;
const MAX_AUTHORITY_PROFILES: usize = 8;

/// Monotonic host-catalog revision.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct CatalogCursor(pub u64);

impl CatalogCursor {
    /// Cursor before the first host-catalog revision.
    pub const ZERO: Self = Self(0);

    /// Returns the next cursor.
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Per-session cursor bound to one actor ownership generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCursor {
    /// Actor generation.
    pub actor_generation: u64,
    /// Monotonic event sequence within that generation.
    pub sequence: u64,
}

impl SessionCursor {
    /// Creates the cursor before any event in a generation.
    pub const fn zero(actor_generation: u64) -> Self {
        Self {
            actor_generation,
            sequence: 0,
        }
    }

    /// Returns the next cursor without changing generation.
    pub fn checked_next(self) -> Option<Self> {
        self.sequence.checked_add(1).map(|sequence| Self {
            actor_generation: self.actor_generation,
            sequence,
        })
    }
}

/// Public host identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostDescriptor {
    /// Stable cryptographic host identity.
    pub id: HostId,
    /// User-assigned display name.
    pub name: String,
}

/// Negotiated host capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostCapabilities {
    /// Multiple sessions may run concurrently.
    pub concurrent_sessions: bool,
    /// Sources and artifacts use authenticated opaque handles.
    pub opaque_resources: bool,
    /// Host-ingested attachment handles are available.
    pub attachments: bool,
    /// Bounded ingest policy when attachments are available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_policy: Option<AttachmentPolicy>,
    /// Live preview targets are supported.
    pub previews: bool,
    /// A connected-device/pairing surface is available.
    pub connected_devices: bool,
    /// LAN connected clients are supported.
    pub lan_clients: bool,
    /// Interactive PTY support; false for the first web release.
    pub terminal: bool,
    /// Nested child agents; false until Ygg implements them.
    pub child_agents: bool,
}

/// Bounded host-ingest policy advertised to graphical clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentPolicy {
    /// Exact accepted media types.
    pub accepted_media_types: Vec<String>,
    /// Maximum attachments accepted by one prompt.
    pub max_count: u32,
    /// Maximum bytes accepted for one file.
    pub max_file_bytes: u64,
    /// Maximum aggregate bytes accepted by one prompt.
    pub max_total_bytes: u64,
}

impl AttachmentPolicy {
    /// Returns the conservative image-only first-party policy.
    pub fn image_defaults() -> Self {
        Self {
            accepted_media_types: vec![
                "image/png".into(),
                "image/jpeg".into(),
                "image/gif".into(),
                "image/webp".into(),
            ],
            max_count: crate::MAX_ATTACHMENT_COUNT as u32,
            max_file_bytes: crate::MAX_ATTACHMENT_FILE_BYTES as u64,
            max_total_bytes: crate::MAX_ATTACHMENT_TOTAL_BYTES as u64,
        }
    }
}

impl Default for HostCapabilities {
    fn default() -> Self {
        Self {
            concurrent_sessions: true,
            opaque_resources: false,
            attachments: false,
            attachment_policy: None,
            previews: false,
            connected_devices: false,
            // Capability advertisements describe the transport that is
            // actually running. The transport-neutral backend does not imply
            // an authenticated LAN listener.
            lan_clients: false,
            terminal: false,
            child_agents: false,
        }
    }
}

/// Safe project catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSummary {
    /// Stable opaque project identity.
    pub id: ProjectId,
    /// Display name.
    pub name: String,
    /// Whether the host trusts the project's roots.
    pub trusted: bool,
    /// Number of durable sessions.
    pub session_count: u32,
    /// Number of currently live sessions.
    pub live_session_count: u32,
}

/// Strong session state derived from owner events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionLiveState {
    /// Fresh or idle.
    Idle,
    /// Run is active.
    Working,
    /// Run is blocked on an approval.
    NeedsApproval,
    /// Run is blocked on user input.
    NeedsInput,
    /// Last run completed and has not been acknowledged in this projection.
    Done,
    /// Last run failed.
    Failed,
    /// Last run was explicitly stopped.
    Stopped,
    /// Owning host is disconnected.
    Offline,
    /// Another process owns the session.
    Locked,
}

/// Compact attention state for the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttentionState {
    /// No user action needed.
    None,
    /// New completion is available.
    UnreadCompletion,
    /// Approval is required.
    Approval,
    /// User input is required.
    Input,
    /// Failure needs inspection.
    Failure,
}

/// Session ownership truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActorOwnerState {
    /// Durable session exists but no graphical actor owns it.
    Inactive,
    /// This host service owns the mutable session.
    Hosted,
    /// A different process owns the mutable session.
    ExternallyLocked,
}

/// Model/provider selection displayed by the composer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSelection {
    /// Provider identity.
    pub provider: String,
    /// Canonical model identity.
    pub model: String,
    /// Product-facing reasoning selection.
    pub reasoning: String,
}

/// Input modality accepted by one model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputModality {
    /// UTF-8 text.
    Text,
    /// Image attachment.
    Image,
    /// Audio attachment.
    Audio,
    /// Document or other host-ingested file.
    Document,
}

/// Bounded model-picker catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSummary {
    /// Canonical model identity used by [`ModelSelection::model`].
    pub id: String,
    /// Product-facing display name.
    pub name: String,
    /// Provider identity used by [`ModelSelection::provider`].
    pub provider: String,
    /// Whether inference stays on the host's local endpoint.
    pub local: bool,
    /// Whether this entry may currently be selected.
    pub available: bool,
    /// Product reasoning choices.
    pub reasoning: Vec<String>,
    /// Host-selected default reasoning choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning: Option<String>,
    /// Supported model inputs.
    pub input_modalities: Vec<InputModality>,
}

/// Agent authority profile. Transport trust remains separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthorityProfile {
    /// No mutation.
    ReadOnly,
    /// Mutations stay in the selected workspace.
    Workspace,
    /// Broad agent authority.
    FullAccess,
}

/// Token/context usage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageSnapshot {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Current context tokens.
    pub context_tokens: u64,
    /// Context limit when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
}

/// Context display state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextUsage {
    /// Usage counters.
    pub usage: UsageSnapshot,
    /// Number of completed compactions.
    pub compactions: u32,
}

/// Bounded sidebar entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSummary {
    /// Session identity.
    pub id: SessionId,
    /// Owning project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Display title.
    pub title: String,
    /// User tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation timestamp.
    pub created_at_ms: u64,
    /// Last durable/live update.
    pub modified_at_ms: u64,
    /// Pinned state.
    pub pinned: bool,
    /// Archived state.
    pub archived: bool,
    /// Fresh host-allocated session not yet promoted into durable Recents.
    pub provisional: bool,
    /// Strong live state.
    pub live_state: SessionLiveState,
    /// User-attention state.
    pub attention: AttentionState,
    /// Mutable owner state.
    pub owner: ActorOwnerState,
    /// Model shown in compact metadata.
    pub model: ModelSelection,
}

/// Item persistence lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemLifecycle {
    /// Live and not durable yet.
    Provisional,
    /// Committed to Ygg's append-only session.
    Committed,
}

/// Plan-step state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepState {
    /// Not started.
    Pending,
    /// Current work.
    InProgress,
    /// Finished.
    Completed,
    /// Cannot continue.
    Blocked,
}

/// One deterministic plan/progress step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanStep {
    /// Stable step identity within the item.
    pub id: String,
    /// User-facing content.
    pub content: String,
    /// Active-form content while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// Current state.
    pub state: PlanStepState,
}

/// Deterministic file-change evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileChange {
    /// Opaque host-owned file handle.
    pub handle: String,
    /// Safe relative/display path.
    pub display_path: String,
    /// Added lines when known.
    pub additions: u32,
    /// Removed lines when known.
    pub deletions: u32,
}

/// Source classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    /// User attachment.
    Attachment,
    /// Host file or resource.
    File,
    /// Web URL/result.
    Web,
    /// Ygg resource such as prompt/skill documentation.
    Resource,
    /// Other deterministic source.
    Other,
}

/// Actual source consulted by a tool or explicitly cited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceRef {
    /// Stable source identity.
    pub id: SourceId,
    /// Source class.
    pub kind: SourceKind,
    /// Safe display title.
    pub title: String,
    /// Opaque authenticated host handle.
    pub handle: String,
    /// Originating item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_item_id: Option<ItemId>,
    /// Host timestamp when consulted.
    pub consulted_at_ms: u64,
    /// Whether an explicit citation relationship exists.
    pub cited: bool,
    /// Whether the source can still be opened.
    pub available: bool,
}

/// Artifact classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArtifactKind {
    /// Text or source file.
    File,
    /// Image.
    Image,
    /// Document.
    Document,
    /// Spreadsheet.
    Spreadsheet,
    /// Presentation.
    Presentation,
    /// HTML/site output.
    Site,
    /// Other typed output.
    Other,
}

/// Host-owned output reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactRef {
    /// Stable artifact identity.
    pub id: ArtifactId,
    /// Artifact class.
    pub kind: ArtifactKind,
    /// Safe display name.
    pub name: String,
    /// Media type.
    pub media_type: String,
    /// Opaque authenticated content handle.
    pub handle: String,
    /// Content size.
    pub byte_len: u64,
    /// Optional content digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Originating durable/provisional item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_item_id: Option<ItemId>,
    /// Whether content is still available.
    pub available: bool,
}

/// Registered live or static preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewRef {
    /// Opaque target identity.
    pub handle: String,
    /// Display title.
    pub title: String,
    /// Whether this target may refresh live.
    pub live: bool,
}

/// Run terminal outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunOutcome {
    /// Completed successfully.
    Completed,
    /// Stopped by user.
    Stopped,
    /// Failed.
    Failed,
}

/// Typed transcript payload. The model never supplies component schemas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ItemPayload {
    /// Submitted user input.
    UserMessage {
        /// Text content.
        text: String,
        /// Host-ingested attachments.
        #[serde(default)]
        attachments: Vec<crate::AttachmentRef>,
    },
    /// Visible assistant response.
    AssistantMessage {
        /// Complete or provisional text.
        text: String,
    },
    /// Reasoning/summary channel.
    Reasoning {
        /// Complete or provisional reasoning text.
        text: String,
    },
    /// Structured tool invocation.
    ToolCall {
        /// Tool name.
        name: String,
        /// Inert bounded arguments.
        arguments: Value,
        /// Latest bounded live progress.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<String>,
        /// Bytes omitted by the producer's bounded progress buffer.
        #[serde(default)]
        dropped_progress_bytes: u64,
    },
    /// Structured tool result.
    ToolResult {
        /// Item identity of the call.
        tool_call_item_id: ItemId,
        /// Bounded textual result.
        content: String,
        /// Failure indicator.
        is_error: bool,
    },
    /// Deterministic plan.
    Plan {
        /// Plan steps.
        steps: Vec<PlanStep>,
    },
    /// File change.
    FileChange(FileChange),
    /// Source reference.
    Source(SourceRef),
    /// Artifact/output.
    Artifact(ArtifactRef),
    /// Preview availability.
    Preview(PreviewRef),
    /// Context compaction.
    Compaction {
        /// Human-safe reason.
        reason: String,
    },
    /// Run outcome.
    RunOutcome {
        /// Final outcome.
        outcome: RunOutcome,
        /// Optional public explanation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// One transcript/operational item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionItem {
    /// Stable item identity across streaming and commit.
    pub id: ItemId,
    /// Owning run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// Owning turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Provider attempt for provisional candidate output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_attempt: Option<u32>,
    /// Persistence lifecycle.
    pub lifecycle: ItemLifecycle,
    /// Exact append-only session entry after commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_entry_id: Option<DurableEntryId>,
    /// Typed semantics.
    pub payload: ItemPayload,
}

/// Public request category. Private senders/handles stay inside the driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RequestKind {
    /// Tool or policy approval.
    Approval {
        /// Human-readable action.
        action: String,
        /// Optional originating item.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<ItemId>,
    },
    /// User input.
    UserInput {
        /// Prompt.
        prompt: String,
        /// Optional bounded choices.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<String>,
    },
}

/// Public request state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestState {
    /// Waiting for an answer.
    Pending,
    /// Allowed/answered.
    Resolved,
    /// Explicitly denied.
    Denied,
    /// Expired because ownership changed.
    Expired,
}

/// Opaque public request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingRequest {
    /// Public request identity.
    pub id: RequestId,
    /// Actor generation that owns the private handle.
    pub actor_generation: u64,
    /// Request kind.
    pub kind: RequestKind,
    /// State.
    pub state: RequestState,
}

/// Complete selected-session projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSnapshot {
    /// Session identity.
    pub session_id: SessionId,
    /// Mutable owner generation.
    pub actor_generation: u64,
    /// Latest session cursor.
    pub cursor: SessionCursor,
    /// Durable active head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_head: Option<DurableEntryId>,
    /// Strong live state.
    pub live_state: SessionLiveState,
    /// Active run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<RunId>,
    /// Model/provider/reasoning state.
    pub model: ModelSelection,
    /// Agent authority, separate from device authorization.
    pub authority: AuthorityProfile,
    /// Usage/context.
    pub context: ContextUsage,
    /// Durable tail plus current provisional items.
    pub items: Vec<SessionItem>,
    /// Active public requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_requests: Vec<PendingRequest>,
    /// Consulted sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
    /// Produced outputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
}

/// Initial shell/catalog and selected fresh-session projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostBootstrap {
    /// Protocol major.
    pub protocol: u16,
    /// Host identity.
    pub host: HostDescriptor,
    /// Negotiated capabilities.
    pub capabilities: HostCapabilities,
    /// Host catalog revision.
    pub catalog_cursor: CatalogCursor,
    /// Model-picker catalog.
    pub models: Vec<ModelSummary>,
    /// Authority choices permitted by this host.
    pub authority_profiles: Vec<AuthorityProfile>,
    /// Effective maximum authority a remote command may select.
    pub authority_ceiling: AuthorityProfile,
    /// Host-resolved bounded theme catalog.
    pub themes: Vec<ThemeOption>,
    /// Current theme selection.
    pub selected_theme_id: ThemeId,
    /// Projects.
    pub projects: Vec<ProjectSummary>,
    /// Bounded session summaries.
    pub sessions: Vec<SessionSummary>,
    /// Freshly selected session.
    pub selected_session_id: SessionId,
    /// Complete selected-session snapshot.
    pub selected_session: SessionSnapshot,
}

fn validate_media_type(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_public_text(field, value, 255, false)?;
    if !value.contains('/')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.'))
    {
        return Err(ValidationError::new(field, "is not a safe media type"));
    }
    Ok(())
}

impl ProtocolValidation for HostDescriptor {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("host.name", &self.name, 256, false)
    }
}

impl ProtocolValidation for AttachmentPolicy {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.accepted_media_types.is_empty() || self.accepted_media_types.len() > 32 {
            return Err(ValidationError::new(
                "attachment_policy.accepted_media_types",
                "must contain 1..=32 entries",
            ));
        }
        let mut media_types = BTreeSet::new();
        for media_type in &self.accepted_media_types {
            validate_media_type("attachment_policy.accepted_media_types", media_type)?;
            if !media_types.insert(media_type) {
                return Err(ValidationError::new(
                    "attachment_policy.accepted_media_types",
                    "contains a duplicate media type",
                ));
            }
        }
        if self.max_count == 0
            || self.max_count as usize > crate::MAX_ATTACHMENT_COUNT
            || self.max_file_bytes == 0
            || self.max_file_bytes > crate::MAX_ATTACHMENT_FILE_BYTES as u64
            || self.max_total_bytes < self.max_file_bytes
            || self.max_total_bytes > crate::MAX_ATTACHMENT_TOTAL_BYTES as u64
        {
            return Err(ValidationError::new(
                "attachment_policy",
                "contains an unsupported attachment bound",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for ProjectSummary {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("project.name", &self.name, 256, false)
    }
}

impl ProtocolValidation for ModelSelection {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("model.provider", &self.provider, 128, false)?;
        validate_public_text("model.model", &self.model, 256, false)?;
        validate_public_text("model.reasoning", &self.reasoning, 128, false)
    }
}

impl ProtocolValidation for ModelSummary {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("model_summary.id", &self.id, 256, false)?;
        validate_public_text("model_summary.name", &self.name, 256, false)?;
        validate_public_text("model_summary.provider", &self.provider, 128, false)?;
        if self.id.trim().is_empty()
            || self.name.trim().is_empty()
            || self.provider.trim().is_empty()
        {
            return Err(ValidationError::new(
                "model_summary",
                "identity, name, and provider must not be blank",
            ));
        }
        if self.reasoning.len() > MAX_REASONING_OPTIONS {
            return Err(ValidationError::new(
                "model_summary.reasoning",
                format!("exceeds the {MAX_REASONING_OPTIONS}-option limit"),
            ));
        }
        let mut reasoning = BTreeSet::new();
        for option in &self.reasoning {
            validate_public_text("model_summary.reasoning", option, 128, false)?;
            if option.trim().is_empty() || !reasoning.insert(option) {
                return Err(ValidationError::new(
                    "model_summary.reasoning",
                    "contains a blank or duplicate option",
                ));
            }
        }
        if let Some(default) = &self.default_reasoning {
            if !reasoning.contains(default) {
                return Err(ValidationError::new(
                    "model_summary.default_reasoning",
                    "must be one of the advertised reasoning options",
                ));
            }
        }
        let modalities = self
            .input_modalities
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if modalities.len() != self.input_modalities.len()
            || !modalities.contains(&InputModality::Text)
        {
            return Err(ValidationError::new(
                "model_summary.input_modalities",
                "must contain text exactly once and no duplicate modalities",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for SessionSummary {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("session.title", &self.title, 512, false)?;
        if self.tags.len() > MAX_TAGS {
            return Err(ValidationError::new(
                "session.tags",
                format!("exceeds the {MAX_TAGS}-tag limit"),
            ));
        }
        for tag in &self.tags {
            validate_public_text("session.tag", tag, 64, false)?;
        }
        self.model.validate()
    }
}

impl ProtocolValidation for PlanStep {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("plan.step.id", &self.id, 128, false)?;
        validate_public_text("plan.step.content", &self.content, 4096, true)?;
        if let Some(active) = &self.active_form {
            validate_public_text("plan.step.active_form", active, 4096, true)?;
        }
        Ok(())
    }
}

impl ProtocolValidation for FileChange {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("file_change.handle", &self.handle, 256, false)?;
        validate_public_text("file_change.display_path", &self.display_path, 1024, false)
    }
}

impl ProtocolValidation for SourceRef {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("source.title", &self.title, 512, false)?;
        validate_public_text("source.handle", &self.handle, 512, false)
    }
}

impl ProtocolValidation for ArtifactRef {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("artifact.name", &self.name, 512, false)?;
        validate_media_type("artifact.media_type", &self.media_type)?;
        validate_public_text("artifact.handle", &self.handle, 512, false)?;
        if let Some(hash) = &self.content_hash {
            validate_public_text("artifact.content_hash", hash, 256, false)?;
        }
        Ok(())
    }
}

impl ProtocolValidation for PreviewRef {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("preview.handle", &self.handle, 512, false)?;
        validate_public_text("preview.title", &self.title, 512, false)
    }
}

impl ProtocolValidation for ItemPayload {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::UserMessage { text, attachments } => {
                validate_public_text("item.user.text", text, MAX_PROMPT_BYTES, true)?;
                if attachments.len() > 32 {
                    return Err(ValidationError::new(
                        "item.user.attachments",
                        "exceeds the 32-attachment limit",
                    ));
                }
                for attachment in attachments {
                    attachment.validate()?;
                }
            }
            Self::AssistantMessage { text } => {
                validate_public_text("item.assistant.text", text, MAX_ITEM_TEXT_BYTES, true)?;
            }
            Self::Reasoning { text } => {
                validate_public_text("item.reasoning.text", text, MAX_ITEM_TEXT_BYTES, true)?;
            }
            Self::ToolCall {
                name,
                arguments,
                progress,
                ..
            } => {
                validate_public_text("item.tool.name", name, 128, false)?;
                validate_json("item.tool.arguments", arguments, 256 * 1024)?;
                if let Some(progress) = progress {
                    validate_public_text(
                        "item.tool.progress",
                        progress,
                        MAX_ITEM_TEXT_BYTES,
                        true,
                    )?;
                }
            }
            Self::ToolResult { content, .. } => {
                validate_public_text(
                    "item.tool_result.content",
                    content,
                    MAX_ITEM_TEXT_BYTES,
                    true,
                )?;
            }
            Self::Plan { steps } => {
                if steps.len() > MAX_PLAN_STEPS {
                    return Err(ValidationError::new(
                        "item.plan.steps",
                        format!("exceeds the {MAX_PLAN_STEPS}-step limit"),
                    ));
                }
                for step in steps {
                    step.validate()?;
                }
            }
            Self::FileChange(change) => change.validate()?,
            Self::Source(source) => source.validate()?,
            Self::Artifact(artifact) => artifact.validate()?,
            Self::Preview(preview) => preview.validate()?,
            Self::Compaction { reason } => {
                validate_public_text("item.compaction.reason", reason, 4096, true)?;
            }
            Self::RunOutcome { message, .. } => {
                if let Some(message) = message {
                    validate_public_text("item.run_outcome.message", message, 8192, true)?;
                }
            }
        }
        Ok(())
    }
}

impl ProtocolValidation for SessionItem {
    fn validate(&self) -> Result<(), ValidationError> {
        match (self.lifecycle, &self.durable_entry_id) {
            (ItemLifecycle::Provisional, None) | (ItemLifecycle::Committed, Some(_)) => {}
            (ItemLifecycle::Provisional, Some(_)) => {
                return Err(ValidationError::new(
                    "item.durable_entry_id",
                    "must be absent while provisional",
                ));
            }
            (ItemLifecycle::Committed, None) => {
                return Err(ValidationError::new(
                    "item.durable_entry_id",
                    "is required for a committed item",
                ));
            }
        }
        self.payload.validate()
    }
}

impl ProtocolValidation for PendingRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.actor_generation == 0 {
            return Err(ValidationError::new(
                "request.actor_generation",
                "must be non-zero",
            ));
        }
        match &self.kind {
            RequestKind::Approval { action, .. } => {
                validate_public_text("request.approval.action", action, 8192, true)?;
            }
            RequestKind::UserInput { prompt, choices } => {
                validate_public_text("request.user_input.prompt", prompt, 8192, true)?;
                if choices.len() > MAX_CHOICES {
                    return Err(ValidationError::new(
                        "request.user_input.choices",
                        format!("exceeds the {MAX_CHOICES}-choice limit"),
                    ));
                }
                for choice in choices {
                    validate_public_text("request.user_input.choice", choice, 1024, false)?;
                }
            }
        }
        Ok(())
    }
}

impl ProtocolValidation for SessionSnapshot {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.actor_generation == 0 {
            return Err(ValidationError::new(
                "snapshot.actor_generation",
                "must be non-zero",
            ));
        }
        if self.cursor.actor_generation != self.actor_generation {
            return Err(ValidationError::new(
                "snapshot.cursor.actor_generation",
                "must match the snapshot actor generation",
            ));
        }
        self.model.validate()?;
        if self.items.len() > MAX_SESSION_ITEMS {
            return Err(ValidationError::new(
                "snapshot.items",
                format!("exceeds the {MAX_SESSION_ITEMS}-item limit"),
            ));
        }
        if self.pending_requests.len() > MAX_PENDING_REQUESTS {
            return Err(ValidationError::new(
                "snapshot.pending_requests",
                format!("exceeds the {MAX_PENDING_REQUESTS}-request limit"),
            ));
        }
        if self.sources.len() > MAX_RESOURCES || self.artifacts.len() > MAX_RESOURCES {
            return Err(ValidationError::new(
                "snapshot.resources",
                format!("exceeds the {MAX_RESOURCES}-resource limit"),
            ));
        }

        let mut item_ids = BTreeSet::new();
        for item in &self.items {
            item.validate()?;
            if !item_ids.insert(item.id.clone()) {
                return Err(ValidationError::new(
                    "snapshot.items",
                    "contains a duplicate item ID",
                ));
            }
        }
        let mut request_ids = BTreeSet::new();
        for request in &self.pending_requests {
            request.validate()?;
            if request.state != RequestState::Pending {
                return Err(ValidationError::new(
                    "snapshot.pending_requests",
                    "may contain only pending requests",
                ));
            }
            if !request_ids.insert(request.id.clone()) {
                return Err(ValidationError::new(
                    "snapshot.pending_requests",
                    "contains a duplicate request ID",
                ));
            }
        }
        let mut source_ids = BTreeSet::new();
        for source in &self.sources {
            source.validate()?;
            if !source_ids.insert(source.id.clone()) {
                return Err(ValidationError::new(
                    "snapshot.sources",
                    "contains a duplicate source ID",
                ));
            }
        }
        let mut artifact_ids = BTreeSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !artifact_ids.insert(artifact.id.clone()) {
                return Err(ValidationError::new(
                    "snapshot.artifacts",
                    "contains a duplicate artifact ID",
                ));
            }
        }
        validate_serialized_size("snapshot", self, MAX_SNAPSHOT_BYTES)
    }
}

impl ProtocolValidation for HostBootstrap {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "bootstrap.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        self.host.validate()?;
        if let Some(policy) = &self.capabilities.attachment_policy {
            policy.validate()?;
            if !self.capabilities.attachments {
                return Err(ValidationError::new(
                    "bootstrap.capabilities.attachment_policy",
                    "requires the attachments capability",
                ));
            }
        }
        if self.models.is_empty() || self.models.len() > MAX_MODELS {
            return Err(ValidationError::new(
                "bootstrap.models",
                format!("must contain 1..={MAX_MODELS} entries"),
            ));
        }
        let mut model_keys = BTreeSet::new();
        for model in &self.models {
            model.validate()?;
            if !model_keys.insert((model.provider.clone(), model.id.clone())) {
                return Err(ValidationError::new(
                    "bootstrap.models",
                    "contains a duplicate provider/model identity",
                ));
            }
        }
        if self.authority_profiles.is_empty()
            || self.authority_profiles.len() > MAX_AUTHORITY_PROFILES
        {
            return Err(ValidationError::new(
                "bootstrap.authority_profiles",
                format!("must contain 1..={MAX_AUTHORITY_PROFILES} entries"),
            ));
        }
        let authority_profiles = self
            .authority_profiles
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if authority_profiles.len() != self.authority_profiles.len()
            || !authority_profiles.contains(&self.authority_ceiling)
            || self
                .authority_profiles
                .iter()
                .any(|profile| authority_rank(*profile) > authority_rank(self.authority_ceiling))
        {
            return Err(ValidationError::new(
                "bootstrap.authority_profiles",
                "must be unique, include the ceiling, and not exceed it",
            ));
        }
        if self.themes.is_empty() || self.themes.len() > MAX_THEMES {
            return Err(ValidationError::new(
                "bootstrap.themes",
                format!("must contain 1..={MAX_THEMES} entries"),
            ));
        }
        let mut theme_ids = BTreeSet::new();
        for theme in &self.themes {
            theme.validate()?;
            if !theme_ids.insert(theme.id.clone()) {
                return Err(ValidationError::new(
                    "bootstrap.themes",
                    "contains a duplicate theme ID",
                ));
            }
        }
        if !theme_ids.contains(&self.selected_theme_id) {
            return Err(ValidationError::new(
                "bootstrap.selected_theme_id",
                "must identify an advertised theme",
            ));
        }
        if self.projects.len() > MAX_PROJECTS {
            return Err(ValidationError::new(
                "bootstrap.projects",
                format!("exceeds the {MAX_PROJECTS}-project limit"),
            ));
        }
        if self.sessions.len() > MAX_SESSION_SUMMARIES {
            return Err(ValidationError::new(
                "bootstrap.sessions",
                format!("exceeds the {MAX_SESSION_SUMMARIES}-session limit"),
            ));
        }
        let mut project_ids = BTreeSet::new();
        for project in &self.projects {
            project.validate()?;
            if !project_ids.insert(project.id.clone()) {
                return Err(ValidationError::new(
                    "bootstrap.projects",
                    "contains a duplicate project ID",
                ));
            }
        }
        let mut session_ids = BTreeSet::new();
        for session in &self.sessions {
            session.validate()?;
            if !session_ids.insert(session.id.clone()) {
                return Err(ValidationError::new(
                    "bootstrap.sessions",
                    "contains a duplicate session ID",
                ));
            }
        }
        if self.selected_session_id != self.selected_session.session_id {
            return Err(ValidationError::new(
                "bootstrap.selected_session_id",
                "must match the selected session snapshot",
            ));
        }
        if !session_ids.contains(&self.selected_session_id) {
            return Err(ValidationError::new(
                "bootstrap.sessions",
                "must include the selected session summary",
            ));
        }
        self.selected_session.validate()?;
        if authority_rank(self.selected_session.authority) > authority_rank(self.authority_ceiling)
        {
            return Err(ValidationError::new(
                "bootstrap.selected_session.authority",
                "exceeds the host authority ceiling",
            ));
        }
        let selected_model = (
            self.selected_session.model.provider.clone(),
            self.selected_session.model.model.clone(),
        );
        if !model_keys.contains(&selected_model) {
            return Err(ValidationError::new(
                "bootstrap.selected_session.model",
                "must identify an advertised provider/model",
            ));
        }
        validate_serialized_size("bootstrap", self, MAX_BOOTSTRAP_BYTES)
    }
}

fn authority_rank(authority: AuthorityProfile) -> u8 {
    match authority {
        AuthorityProfile::ReadOnly => 0,
        AuthorityProfile::Workspace => 1,
        AuthorityProfile::FullAccess => 2,
    }
}
