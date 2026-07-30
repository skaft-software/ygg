//! Host, project, session, item, and resource DTOs.

use std::collections::BTreeSet;

use crate::bounds::{
    validate_public_text, validate_serialized_size, ProtocolValidation, ValidationError,
    MAX_BOOTSTRAP_BYTES, MAX_ITEM_TEXT_BYTES, MAX_PROMPT_BYTES, MAX_SNAPSHOT_BYTES,
};
use crate::{
    ArtifactId, DurableEntryId, HostId, ItemId, ProjectId, RequestId, RunId, SessionId, SourceId,
    ThemeId, ThemeOption, TurnId, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};

const MAX_PROJECTS: usize = 256;
const MAX_SESSION_SUMMARIES: usize = 2_000;
const MAX_SESSION_ITEMS: usize = 10_000;
const MAX_BRANCH_ENTRIES: usize = 2_048;
const MAX_PENDING_REQUESTS: usize = 128;
const MAX_RESOURCES: usize = 2_048;
const MAX_TAGS: usize = 32;
const MAX_PLAN_STEPS: usize = 256;
const MAX_CHOICES: usize = 32;
const MAX_MODELS: usize = 256;
const MAX_REASONING_OPTIONS: usize = 32;
/// Maximum long-context input pricing tiers advertised for one model.
pub const MAX_MODEL_INPUT_PRICING_TIERS: usize = 32;
const MAX_JSON_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_THEMES: usize = 64;
const MAX_AUTHORITY_PROFILES: usize = 8;
const MAX_COMMAND_SUGGESTIONS: usize = 512;
const MAX_SKILL_SUGGESTIONS: usize = 512;
const MAX_COMMAND_DISCOVERY_BYTES: usize = 256 * 1024;

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
    /// Bounded text, Markdown, and PDF document ingest is available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub documents: bool,
    /// Root-confined trusted-project file browsing and context selection is available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted_project_files: bool,
    /// Root-confined trusted-project directory, search, and text-read routes are available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_file_browser: bool,
    /// Full-file replacement through the trusted-project browser is available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_file_write: bool,
    /// Authenticated search over durable, already-redacted transcript projections is available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transcript_search: bool,
    /// Live preview targets are supported.
    pub previews: bool,
    /// A connected-device/pairing surface is available.
    pub connected_devices: bool,
    /// Durable session rename, pin, and archive mutations are available.
    pub session_metadata: bool,
    /// Durable branch graph inspection and idle-boundary checkout are available.
    pub session_branches: bool,
    /// Edit, retry-with-model, and new-session conversation branching are
    /// available with durable provenance.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conversation_branching: bool,
    /// Recoverable trash, retention metadata, and confirmed permanent delete
    /// are available.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_trash: bool,
    /// Authenticated, redacted portable session downloads are available.
    pub session_export: bool,
    /// LAN connected clients are supported.
    pub lan_clients: bool,
    /// Interactive PTY support through the authenticated loopback transport.
    pub terminal: bool,
    /// Nested child agents; false until Ygg implements them.
    pub child_agents: bool,
}

/// Source of one slash-command suggestion admitted by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandSuggestionKind {
    /// A built-in coding-agent slash command.
    BuiltIn,
    /// A prompt template admitted by the shared resource resolver.
    Prompt,
    /// An enabled executable-extension command.
    Extension,
}

/// One bounded command shown by the graphical composer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandSuggestion {
    /// Invocation name without its leading slash.
    pub name: String,
    /// Complete label/usage string, including the leading slash.
    pub usage: String,
    /// Short host-authored description.
    pub description: String,
    /// Optional argument placeholder supplied by a prompt template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Whether choosing the command should leave the composer ready for arguments.
    pub accepts_argument: bool,
    /// Trusted source that admitted this command.
    pub kind: CommandSuggestionKind,
}

/// One trusted skill available to `/skills` operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillSuggestion {
    /// Stable skill identifier accepted by `/skills`.
    pub id: String,
    /// Human-readable skill name.
    pub name: String,
    /// Short host-authored description.
    pub description: String,
    /// Whether this skill is active for the selected session.
    pub active: bool,
}

/// Read-only, bounded slash-command and skill discovery payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandDiscovery {
    /// Protocol major.
    pub protocol: u16,
    /// TUI-ordered command/template/extension suggestions.
    pub commands: Vec<CommandSuggestion>,
    /// Trusted skills available to `/skills` operations.
    pub skills: Vec<SkillSuggestion>,
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
            documents: false,
            trusted_project_files: false,
            project_file_browser: false,
            project_file_write: false,
            transcript_search: false,
            previews: false,
            connected_devices: false,
            session_metadata: false,
            session_branches: false,
            conversation_branching: false,
            session_trash: false,
            session_export: false,
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
    /// Whether the project has been archived and is no longer selectable.
    pub archived: bool,
    /// Whether the exact imported directory identity is currently available.
    pub available: bool,
    /// Whether this project is selected when a create command omits a project.
    pub is_default: bool,
    /// Number of sessions with a durable project association.
    pub session_count: u32,
    /// Number of currently live sessions.
    pub live_session_count: u32,
}

/// Session-free project catalog used for trust onboarding.
///
/// Unlike [`HostBootstrap`], this response never creates or opens a session,
/// so an untrusted launch workspace can safely render its onboarding surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectCatalog {
    /// Protocol major.
    pub protocol: u16,
    /// Host identity needed to target an idempotent host command.
    pub host: HostDescriptor,
    /// Current host catalog revision.
    pub catalog_cursor: CatalogCursor,
    /// Whether rename/default/trust/revoke/archive mutations are available.
    pub lifecycle_mutations_supported: bool,
    /// Whether a host-native folder picker can mint opaque import candidates.
    pub import_supported: bool,
    /// Bounded path-free project summaries.
    pub projects: Vec<ProjectSummary>,
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

/// Structured pull-request state supplied by a host integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PullRequestState {
    /// Work is still in progress.
    InProgress,
    /// The pull request is ready for review.
    Ready,
    /// The pull request has been merged.
    Merged,
}

/// Evidence-backed pull-request summary for a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PullRequestSummary {
    /// Current pull-request state.
    pub state: PullRequestState,
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

/// Input pricing tier advertised to graphical clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelInputPricingTier {
    /// Minimum context tokens required to use this rate.
    pub min_input_tokens: u64,
    /// Input rate in microdollars per one million tokens.
    pub microdollars_per_million_tokens: u64,
}

/// Bounded input pricing needed for a current-context cost estimate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelInputPricing {
    /// Base input rate in microdollars per one million tokens.
    pub base_microdollars_per_million_tokens: u64,
    /// Ascending long-context input-rate overrides.
    pub tiers: Vec<ModelInputPricingTier>,
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
    /// Input pricing used to estimate the cost of the current context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_pricing: Option<ModelInputPricing>,
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
    /// Explicit catalog lifecycle. `archived` remains on the wire for
    /// protocol-v1 clients but must agree with this stronger state.
    #[serde(default)]
    pub lifecycle: SessionCatalogState,
    /// Trash retention metadata, present only while the session is recoverable
    /// from trash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<SessionRetention>,
    /// Durable provenance when this session was forked from another committed
    /// conversation checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ConversationBranchProvenance>,
    /// Fresh host-allocated session not yet promoted into durable Recents.
    pub provisional: bool,
    /// Strong live state.
    pub live_state: SessionLiveState,
    /// User-attention state.
    pub attention: AttentionState,
    /// Structured pull-request evidence, when supplied by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestSummary>,
    /// Mutable owner state.
    pub owner: ActorOwnerState,
    /// Model shown in compact metadata.
    pub model: ModelSelection,
}

/// Durable catalog lifecycle for a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCatalogState {
    /// Selectable in the ordinary recent-session catalog.
    #[default]
    Active,
    /// Hidden from recents but retained indefinitely.
    Archived,
    /// Recoverable until its retention deadline.
    Trash,
}

/// Host-owned trash retention metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRetention {
    /// Host timestamp when the session entered trash.
    pub trashed_at_ms: u64,
    /// Earliest host timestamp at which automatic purge is permitted.
    pub purge_after_ms: u64,
    /// Permanent deletion always requires an exact, fresh confirmation.
    pub permanent_delete_requires_confirmation: bool,
}

/// User-facing conversation branch operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationBranchOperation {
    /// An earlier user turn was replaced on a sibling branch.
    EditUserTurn,
    /// An assistant response was retried from its originating user checkpoint.
    RetryResponse,
    /// A committed checkpoint was copied into a genuinely new session.
    ForkSession,
}

/// Durable, path-free provenance for edit, retry, and new-session forks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversationBranchProvenance {
    /// Operation that created this branch.
    pub operation: ConversationBranchOperation,
    /// Source session.
    pub source_session_id: SessionId,
    /// Exact source entry selected by the user.
    pub source_entry_id: DurableEntryId,
    /// Originating user checkpoint for retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_user_entry_id: Option<DurableEntryId>,
    /// Explicit alternate model, if one was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<ModelSelection>,
    /// Always true: transcript branching does not undo external side effects.
    pub external_effects_preserved: bool,
    /// Explicit product warning explaining that boundary.
    pub warning: String,
}

/// One durable entry in the complete preserved session graph.
///
/// Entries are deliberately path-free and carry only enough presentation
/// text for a branch picker. The exact transcript for the selected head stays
/// in [`SessionSnapshot::items`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBranchEntry {
    /// Exact durable Ygg entry identity.
    pub entry_id: DurableEntryId,
    /// Parent entry, or none for a root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_entry_id: Option<DurableEntryId>,
    /// Coarse presentation class. Internal nodes remain only to preserve
    /// exact parent closure for visible conversation checkpoints.
    pub kind: SessionBranchEntryKind,
    /// Whether a user-facing client may offer this node as a checkout target.
    pub checkoutable: bool,
    /// Bounded human-readable branch picker label.
    pub label: String,
}

/// Coarse, non-sensitive durable entry classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionBranchEntryKind {
    /// User-authored conversation checkpoint.
    UserMessage,
    /// Assistant-authored conversation checkpoint.
    AssistantMessage,
    /// Deliberate transcript compaction checkpoint.
    Compaction,
    /// Configuration, provider state, skills, and structural nodes.
    Internal,
}

/// Bounded durable branch projection plus the currently selected head.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBranchGraph {
    /// Exact active durable head, or none for a fresh empty session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<DurableEntryId>,
    /// Recent preserved durable entries in append order.
    pub entries: Vec<SessionBranchEntry>,
    /// Whether older entries and parent links were intentionally omitted.
    #[serde(default)]
    pub truncated: bool,
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
    /// Opaque host-owned handle for the exact unified diff.
    pub handle: String,
    /// Opaque host-owned handle for the exact post-change file snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_handle: Option<String>,
    /// Safe relative/display path.
    pub display_path: String,
    /// Semantic tool activity that produced the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_item_id: Option<ItemId>,
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

/// Stable semantic classification for one tool activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolKind {
    /// Read a local or remote resource.
    Read,
    /// Search local content.
    Search,
    /// Edit an existing file.
    Edit,
    /// Write or replace a file.
    Write,
    /// Run a bounded shell command.
    Command,
    /// Consult the web through a host tool.
    Web,
    /// Read or apply a Ygg skill/resource.
    Skill,
    /// Extension or unknown tool with no argument-derived public detail.
    Other,
}

/// Deterministic high-level phase used to group tool activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityPhase {
    /// Read/search/resource inspection.
    Investigated,
    /// Workspace mutation.
    Changed,
    /// Test, check, build, lint, or other recognized verification.
    Verified,
    /// A user-facing output was produced.
    Produced,
    /// Activity that cannot be classified safely.
    Other,
}

/// Public lifecycle of one tool activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolActivityStatus {
    /// Tool execution is in progress.
    Running,
    /// Tool execution completed successfully.
    Succeeded,
    /// Tool execution completed with failure.
    Failed,
    /// Tool execution stopped before a terminal tool result was available.
    Stopped,
}

/// Bounded, argument-free semantic projection of one tool call.
///
/// Raw tool arguments, progress output, and result bodies are deliberately
/// absent from the public protocol. Opaque output handles may be added by the
/// host only after a separately authenticated, redacted log is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolActivity {
    /// Normalized, bounded protocol tool name.
    pub raw_tool_name: String,
    /// Stable semantic class.
    pub kind: ToolKind,
    /// Deterministic review phase.
    pub phase: ActivityPhase,
    /// Current public lifecycle.
    pub status: ToolActivityStatus,
    /// Short user-facing action title.
    pub title: String,
    /// Optional bounded semantic status; never stdout/stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Validated workspace-relative path or sanitized remote target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Validated workspace-relative command working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Full bounded Bash command text; credential-like values may be redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_preview: Option<String>,
    /// Process exit code when deterministically parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Process signal number when deterministically parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Host timestamp captured when execution started.
    pub started_at_ms: u64,
    /// Host timestamp captured when execution settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    /// Frozen elapsed execution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Bounded semantic outcome; never raw tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    /// Optional authenticated handle for a separately stored redacted log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_handle: Option<String>,
    /// Raw output bytes observed and intentionally not forwarded.
    #[serde(default)]
    pub observed_output_bytes: u64,
    /// Output bytes dropped before the adapter observed them.
    #[serde(default)]
    pub dropped_output_bytes: u64,
    /// Validated changed paths linked to this action.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<String>,
    /// Deterministic consulted-source links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_ids: Vec<SourceId>,
    /// Deterministic produced-output links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_ids: Vec<ArtifactId>,
}

/// Durable semantic projection of a tool result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolResultSummary {
    /// Item identity of the matching tool activity.
    pub tool_call_item_id: ItemId,
    /// Terminal public lifecycle.
    pub status: ToolActivityStatus,
    /// Short terminal summary.
    pub summary: String,
    /// Bounded semantic outcome; never raw output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    /// Optional authenticated handle for a separately stored redacted log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_handle: Option<String>,
    /// Process exit code when deterministically parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Process signal number when deterministically parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Host timestamp captured when execution settled.
    pub completed_at_ms: u64,
    /// Frozen elapsed execution time.
    pub duration_ms: u64,
    /// Raw output bytes observed and intentionally not forwarded.
    #[serde(default)]
    pub observed_output_bytes: u64,
    /// Output bytes dropped before the adapter observed them.
    #[serde(default)]
    pub dropped_output_bytes: u64,
}

/// How completely deterministic evidence covers a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceCoverage {
    /// No source/change/output evidence was captured.
    None,
    /// Some facts are linked, or shell/extension mutation cannot be ruled out.
    Partial,
    /// Every relevant built-in action has deterministic linked evidence.
    Complete,
}

/// Aggregate counts for one deterministic activity phase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivityPhaseSummary {
    /// Phase identity.
    pub phase: ActivityPhase,
    /// Total actions in the phase.
    pub action_count: u32,
    /// Successfully completed actions.
    pub succeeded_count: u32,
    /// Failed actions.
    pub failed_count: u32,
    /// Stopped actions.
    pub stopped_count: u32,
}

/// Structured, factual completion review for one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionReview {
    /// Short deterministic terminal summary.
    pub summary: String,
    /// Frozen end-to-end run duration.
    pub duration_ms: u64,
    /// Number of tool activities.
    pub action_count: u32,
    /// Deterministic phase aggregates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<ActivityPhaseSummary>,
    /// File-change transcript items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_file_item_ids: Vec<ItemId>,
    /// Recognized verification action items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_action_item_ids: Vec<ItemId>,
    /// Failed action items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_action_item_ids: Vec<ItemId>,
    /// Non-fatal warning action items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warning_action_item_ids: Vec<ItemId>,
    /// Consulted source identities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_ids: Vec<SourceId>,
    /// Produced output identities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_ids: Vec<ArtifactId>,
    /// Deterministically parsed test evidence linked to originating command items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_results: Vec<crate::StructuredTestResults>,
    /// Explicit evidence completeness.
    pub evidence_coverage: EvidenceCoverage,
    /// Host-proven open questions. Empty when none can be determined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
}

/// How a live user message was delivered to the running agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UserMessageDelivery {
    /// Began a new run.
    Submit,
    /// Redirected the active run immediately.
    Steer,
    /// Queued input for the next turn of the active run.
    FollowUp,
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
        /// Uploaded document references resolved by the host for this turn.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        documents: Vec<crate::DocumentReference>,
        /// Trusted project-file snapshots resolved by the host for this turn.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        project_files: Vec<crate::TrustedFileEntry>,
        /// Live delivery semantics, omitted when durable history cannot prove them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivery: Option<UserMessageDelivery>,
        /// Durable edit/retry provenance for a conversation branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_provenance: Option<ConversationBranchProvenance>,
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
    /// Bounded semantic tool invocation.
    ToolCall(ToolActivity),
    /// Bounded semantic tool result.
    ToolResult(ToolResultSummary),
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
        /// Deterministic completion review.
        review: CompletionReview,
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
    /// Bounded durable branching state.
    pub branches: SessionBranchGraph,
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

fn validate_discovery_name(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_public_text(field, value, 128, false)?;
    if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
        return Err(ValidationError::new(
            field,
            "must be a non-blank, whitespace-free identifier",
        ));
    }
    Ok(())
}

impl ProtocolValidation for CommandSuggestion {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_discovery_name("command_discovery.commands.name", &self.name)?;
        validate_public_text("command_discovery.commands.usage", &self.usage, 512, false)?;
        if !self.usage.starts_with('/') {
            return Err(ValidationError::new(
                "command_discovery.commands.usage",
                "must begin with a slash",
            ));
        }
        validate_public_text(
            "command_discovery.commands.description",
            &self.description,
            2_048,
            false,
        )?;
        if let Some(argument_hint) = &self.argument_hint {
            validate_public_text(
                "command_discovery.commands.argument_hint",
                argument_hint,
                512,
                false,
            )?;
        }
        Ok(())
    }
}

impl ProtocolValidation for SkillSuggestion {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_discovery_name("command_discovery.skills.id", &self.id)?;
        validate_public_text("command_discovery.skills.name", &self.name, 256, false)?;
        validate_public_text(
            "command_discovery.skills.description",
            &self.description,
            2_048,
            false,
        )
    }
}

impl ProtocolValidation for CommandDiscovery {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "command_discovery.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        if self.commands.len() > MAX_COMMAND_SUGGESTIONS {
            return Err(ValidationError::new(
                "command_discovery.commands",
                format!("exceeds the {MAX_COMMAND_SUGGESTIONS}-command limit"),
            ));
        }
        if self.skills.len() > MAX_SKILL_SUGGESTIONS {
            return Err(ValidationError::new(
                "command_discovery.skills",
                format!("exceeds the {MAX_SKILL_SUGGESTIONS}-skill limit"),
            ));
        }
        let mut command_names = BTreeSet::new();
        for command in &self.commands {
            command.validate()?;
            if !command_names.insert(command.name.as_str()) {
                return Err(ValidationError::new(
                    "command_discovery.commands",
                    "contains a duplicate command name",
                ));
            }
        }
        let mut skill_ids = BTreeSet::new();
        for skill in &self.skills {
            skill.validate()?;
            if !skill_ids.insert(skill.id.as_str()) {
                return Err(ValidationError::new(
                    "command_discovery.skills",
                    "contains a duplicate skill ID",
                ));
            }
        }
        validate_serialized_size("command_discovery", self, MAX_COMMAND_DISCOVERY_BYTES)
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
        validate_public_text("project.name", &self.name, 256, false)?;
        if self.archived && (self.trusted || self.is_default || self.live_session_count != 0) {
            return Err(ValidationError::new(
                "project",
                "archived projects must be untrusted, non-default, and have no live sessions",
            ));
        }
        if self.live_session_count > self.session_count {
            return Err(ValidationError::new(
                "project.live_session_count",
                "must not exceed the durable session count",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for ProjectCatalog {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "project_catalog.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        self.host.validate()?;
        if self.projects.len() > MAX_PROJECTS {
            return Err(ValidationError::new(
                "project_catalog.projects",
                format!("exceeds the {MAX_PROJECTS}-project limit"),
            ));
        }
        let mut project_ids = BTreeSet::new();
        let mut defaults = 0usize;
        for project in &self.projects {
            project.validate()?;
            if !project_ids.insert(project.id.clone()) {
                return Err(ValidationError::new(
                    "project_catalog.projects",
                    "contains a duplicate project ID",
                ));
            }
            defaults += usize::from(project.is_default);
        }
        if defaults > 1 {
            return Err(ValidationError::new(
                "project_catalog.projects",
                "contains more than one default project",
            ));
        }
        validate_serialized_size("project_catalog", self, MAX_BOOTSTRAP_BYTES)
    }
}

impl ProtocolValidation for ModelSelection {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("model.provider", &self.provider, 128, false)?;
        validate_public_text("model.model", &self.model, 256, false)?;
        validate_public_text("model.reasoning", &self.reasoning, 128, false)
    }
}

impl ProtocolValidation for ModelInputPricing {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.base_microdollars_per_million_tokens > MAX_JSON_SAFE_INTEGER {
            return Err(ValidationError::new(
                "model_summary.input_pricing.base_microdollars_per_million_tokens",
                "must fit in a JSON safe integer",
            ));
        }
        if self.tiers.len() > MAX_MODEL_INPUT_PRICING_TIERS {
            return Err(ValidationError::new(
                "model_summary.input_pricing.tiers",
                format!("exceeds the {MAX_MODEL_INPUT_PRICING_TIERS}-tier limit"),
            ));
        }
        let mut previous_threshold = None;
        for tier in &self.tiers {
            if tier.min_input_tokens > MAX_JSON_SAFE_INTEGER
                || tier.microdollars_per_million_tokens > MAX_JSON_SAFE_INTEGER
            {
                return Err(ValidationError::new(
                    "model_summary.input_pricing.tiers",
                    "values must fit in JSON safe integers",
                ));
            }
            if previous_threshold.is_some_and(|previous| previous >= tier.min_input_tokens) {
                return Err(ValidationError::new(
                    "model_summary.input_pricing.tiers",
                    "must have strictly ascending input thresholds",
                ));
            }
            previous_threshold = Some(tier.min_input_tokens);
        }
        Ok(())
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
        if let Some(pricing) = &self.input_pricing {
            pricing.validate()?;
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
        match (self.lifecycle, self.archived, self.retention.as_ref()) {
            (SessionCatalogState::Active, false, None)
            | (SessionCatalogState::Archived, true, None)
            | (SessionCatalogState::Trash, true, Some(_)) => {}
            _ => {
                return Err(ValidationError::new(
                    "session.lifecycle",
                    "does not agree with archived and retention state",
                ))
            }
        }
        if let Some(retention) = &self.retention {
            retention.validate()?;
        }
        if let Some(provenance) = &self.forked_from {
            provenance.validate()?;
            if provenance.operation != ConversationBranchOperation::ForkSession {
                return Err(ValidationError::new(
                    "session.forked_from.operation",
                    "must be forkSession for session-level provenance",
                ));
            }
        }
        self.model.validate()
    }
}

impl ProtocolValidation for SessionRetention {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.trashed_at_ms == 0 || self.purge_after_ms <= self.trashed_at_ms {
            return Err(ValidationError::new(
                "session.retention",
                "requires a positive trash time and a later purge deadline",
            ));
        }
        if !self.permanent_delete_requires_confirmation {
            return Err(ValidationError::new(
                "session.retention.permanent_delete_requires_confirmation",
                "must be true",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for ConversationBranchProvenance {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(model) = &self.model_override {
            model.validate()?;
        }
        if !self.external_effects_preserved {
            return Err(ValidationError::new(
                "conversation_branch.external_effects_preserved",
                "must explicitly preserve external effects",
            ));
        }
        validate_public_text(
            "conversation_branch.warning",
            &self.warning,
            2 * 1024,
            false,
        )?;
        let normalized_warning = self.warning.to_ascii_lowercase();
        if !normalized_warning.contains("external")
            || !normalized_warning.contains("not rolled back")
        {
            return Err(ValidationError::new(
                "conversation_branch.warning",
                "must explain that external effects are not rolled back",
            ));
        }
        match self.operation {
            ConversationBranchOperation::EditUserTurn
            | ConversationBranchOperation::ForkSession
                if self.originating_user_entry_id.is_some() || self.model_override.is_some() =>
            {
                Err(ValidationError::new(
                    "conversation_branch",
                    "originating user and model override are only valid for response retries",
                ))
            }
            ConversationBranchOperation::RetryResponse
                if self.originating_user_entry_id.is_none() =>
            {
                Err(ValidationError::new(
                    "conversation_branch.originating_user_entry_id",
                    "is required for response retries",
                ))
            }
            _ => Ok(()),
        }
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
        if let Some(handle) = &self.result_handle {
            validate_public_text("file_change.result_handle", handle, 256, false)?;
        }
        validate_public_text("file_change.display_path", &self.display_path, 1024, false)
    }
}

impl ProtocolValidation for ToolActivity {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("item.tool.raw_tool_name", &self.raw_tool_name, 128, false)?;
        validate_public_text("item.tool.title", &self.title, 512, false)?;
        if let Some(summary) = &self.summary {
            validate_public_text("item.tool.summary", summary, 2 * 1024, true)?;
        }
        if let Some(target) = &self.target {
            validate_public_text("item.tool.target", target, 1024, false)?;
        }
        if let Some(cwd) = &self.cwd {
            validate_public_text("item.tool.cwd", cwd, 1024, false)?;
        }
        if let Some(command) = &self.command_preview {
            validate_public_text("item.tool.command_preview", command, 1024, true)?;
        }
        if let Some(summary) = &self.output_summary {
            validate_public_text("item.tool.output_summary", summary, 2 * 1024, true)?;
        }
        if let Some(handle) = &self.output_handle {
            validate_public_text("item.tool.output_handle", handle, 512, false)?;
        }
        if self.started_at_ms == 0 {
            return Err(ValidationError::new(
                "item.tool.started_at_ms",
                "must be non-zero",
            ));
        }
        if self.exit_code.is_some() && self.signal.is_some() {
            return Err(ValidationError::new(
                "item.tool.exit",
                "exit_code and signal are mutually exclusive",
            ));
        }
        match self.status {
            ToolActivityStatus::Running
                if self.completed_at_ms.is_some() || self.duration_ms.is_some() =>
            {
                return Err(ValidationError::new(
                    "item.tool.status",
                    "running activity must not have terminal timing",
                ));
            }
            ToolActivityStatus::Succeeded
            | ToolActivityStatus::Failed
            | ToolActivityStatus::Stopped
                if self.completed_at_ms.is_none() || self.duration_ms.is_none() =>
            {
                return Err(ValidationError::new(
                    "item.tool.status",
                    "terminal activity requires completed_at_ms and duration_ms",
                ));
            }
            _ => {}
        }
        if self
            .completed_at_ms
            .is_some_and(|completed| completed < self.started_at_ms)
        {
            return Err(ValidationError::new(
                "item.tool.completed_at_ms",
                "must not precede started_at_ms",
            ));
        }
        if self.changed_paths.len() > MAX_RESOURCES
            || self.source_ids.len() > MAX_RESOURCES
            || self.artifact_ids.len() > MAX_RESOURCES
        {
            return Err(ValidationError::new(
                "item.tool.links",
                format!("exceeds the {MAX_RESOURCES}-link limit"),
            ));
        }
        let mut paths = BTreeSet::new();
        for path in &self.changed_paths {
            validate_public_text("item.tool.changed_path", path, 1024, false)?;
            if !paths.insert(path) {
                return Err(ValidationError::new(
                    "item.tool.changed_paths",
                    "contains a duplicate path",
                ));
            }
        }
        if self.source_ids.iter().collect::<BTreeSet<_>>().len() != self.source_ids.len()
            || self.artifact_ids.iter().collect::<BTreeSet<_>>().len() != self.artifact_ids.len()
        {
            return Err(ValidationError::new(
                "item.tool.links",
                "contains a duplicate identity",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for ToolResultSummary {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.status == ToolActivityStatus::Running {
            return Err(ValidationError::new(
                "item.tool_result.status",
                "must be terminal",
            ));
        }
        validate_public_text("item.tool_result.summary", &self.summary, 512, false)?;
        if let Some(summary) = &self.output_summary {
            validate_public_text("item.tool_result.output_summary", summary, 2 * 1024, true)?;
        }
        if let Some(handle) = &self.output_handle {
            validate_public_text("item.tool_result.output_handle", handle, 512, false)?;
        }
        if self.completed_at_ms == 0 {
            return Err(ValidationError::new(
                "item.tool_result.completed_at_ms",
                "must be non-zero",
            ));
        }
        if self.exit_code.is_some() && self.signal.is_some() {
            return Err(ValidationError::new(
                "item.tool_result.exit",
                "exit_code and signal are mutually exclusive",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for CompletionReview {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text(
            "item.completion_review.summary",
            &self.summary,
            2 * 1024,
            false,
        )?;
        if self.phases.len() > 16 {
            return Err(ValidationError::new(
                "item.completion_review.phases",
                "exceeds the 16-phase limit",
            ));
        }
        let mut phases = BTreeSet::new();
        let mut counted_actions = 0u64;
        for phase in &self.phases {
            if !phases.insert(phase.phase) {
                return Err(ValidationError::new(
                    "item.completion_review.phases",
                    "contains a duplicate phase",
                ));
            }
            let terminal = u64::from(phase.succeeded_count)
                .saturating_add(u64::from(phase.failed_count))
                .saturating_add(u64::from(phase.stopped_count));
            if terminal > u64::from(phase.action_count) {
                return Err(ValidationError::new(
                    "item.completion_review.phases",
                    "terminal counts exceed action_count",
                ));
            }
            counted_actions = counted_actions.saturating_add(u64::from(phase.action_count));
        }
        if counted_actions != u64::from(self.action_count) {
            return Err(ValidationError::new(
                "item.completion_review.action_count",
                "must equal the phase action total",
            ));
        }
        for (field, count) in [
            (
                "item.completion_review.changed_file_item_ids",
                self.changed_file_item_ids.len(),
            ),
            (
                "item.completion_review.verification_action_item_ids",
                self.verification_action_item_ids.len(),
            ),
            (
                "item.completion_review.failed_action_item_ids",
                self.failed_action_item_ids.len(),
            ),
            (
                "item.completion_review.warning_action_item_ids",
                self.warning_action_item_ids.len(),
            ),
            ("item.completion_review.source_ids", self.source_ids.len()),
            ("item.completion_review.output_ids", self.output_ids.len()),
            (
                "item.completion_review.test_results",
                self.test_results.len(),
            ),
        ] {
            if count > MAX_RESOURCES {
                return Err(ValidationError::new(
                    field,
                    format!("exceeds the {MAX_RESOURCES}-link limit"),
                ));
            }
        }
        let mut test_origins = BTreeSet::new();
        for result in &self.test_results {
            if !test_origins.insert(result.origin_item_id.clone()) {
                return Err(ValidationError::new(
                    "item.completion_review.test_results",
                    "contains duplicate originating command items",
                ));
            }
            if !self
                .verification_action_item_ids
                .contains(&result.origin_item_id)
            {
                return Err(ValidationError::new(
                    "item.completion_review.test_results",
                    "must link to a verification action item",
                ));
            }
            let encoded = serde_json::to_vec(result).map_err(|_| {
                ValidationError::new(
                    "item.completion_review.test_results",
                    "could not be encoded",
                )
            })?;
            crate::decode_structured_test_results(&encoded).map_err(|_| {
                ValidationError::new(
                    "item.completion_review.test_results",
                    "contains invalid structured test evidence",
                )
            })?;
        }
        if self.open_questions.len() > 64 {
            return Err(ValidationError::new(
                "item.completion_review.open_questions",
                "exceeds the 64-question limit",
            ));
        }
        for question in &self.open_questions {
            validate_public_text(
                "item.completion_review.open_question",
                question,
                2 * 1024,
                true,
            )?;
        }
        Ok(())
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
            Self::UserMessage {
                text,
                attachments,
                documents,
                project_files,
                branch_provenance,
                ..
            } => {
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
                if documents.len() > crate::MAX_DOCUMENTS_PER_PROMPT {
                    return Err(ValidationError::new(
                        "item.user.documents",
                        "exceeds the uploaded-document limit",
                    ));
                }
                for document in documents {
                    validate_public_text(
                        "item.user.document.display_name",
                        &document.display_name,
                        512,
                        false,
                    )?;
                    validate_public_text("item.user.document.sha256", &document.sha256, 64, false)?;
                }
                if project_files.len() > crate::MAX_TRUSTED_FILES_PER_CONTEXT {
                    return Err(ValidationError::new(
                        "item.user.project_files",
                        "exceeds the trusted project-file limit",
                    ));
                }
                for file in project_files {
                    validate_public_text(
                        "item.user.project_file.relative_path",
                        &file.relative_path,
                        2_048,
                        false,
                    )?;
                    validate_public_text(
                        "item.user.project_file.display_name",
                        &file.display_name,
                        512,
                        false,
                    )?;
                }
                if let Some(provenance) = branch_provenance {
                    provenance.validate()?;
                    if provenance.operation == ConversationBranchOperation::ForkSession {
                        return Err(ValidationError::new(
                            "item.user.branch_provenance.operation",
                            "forkSession provenance belongs on the session",
                        ));
                    }
                }
            }
            Self::AssistantMessage { text } => {
                validate_public_text("item.assistant.text", text, MAX_ITEM_TEXT_BYTES, true)?;
            }
            Self::Reasoning { text } => {
                validate_public_text("item.reasoning.text", text, MAX_ITEM_TEXT_BYTES, true)?;
            }
            Self::ToolCall(activity) => activity.validate()?,
            Self::ToolResult(result) => result.validate()?,
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
            Self::RunOutcome {
                message, review, ..
            } => {
                if let Some(message) = message {
                    validate_public_text("item.run_outcome.message", message, 8192, true)?;
                }
                review.validate()?;
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

impl ProtocolValidation for SessionBranchEntry {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("snapshot.branches.entry.label", &self.label, 256, false)?;
        if self.parent_entry_id.as_ref() == Some(&self.entry_id) {
            return Err(ValidationError::new(
                "snapshot.branches.entry.parent_entry_id",
                "must not reference itself",
            ));
        }
        if self.kind == SessionBranchEntryKind::Internal && self.checkoutable {
            return Err(ValidationError::new(
                "snapshot.branches.entry.checkoutable",
                "internal branch nodes must not be user-checkoutable",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for SessionBranchGraph {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.entries.len() > MAX_BRANCH_ENTRIES {
            return Err(ValidationError::new(
                "snapshot.branches.entries",
                format!("exceeds the {MAX_BRANCH_ENTRIES}-entry limit"),
            ));
        }
        let mut branch_ids = BTreeSet::new();
        for entry in &self.entries {
            entry.validate()?;
            if !branch_ids.insert(entry.entry_id.clone()) {
                return Err(ValidationError::new(
                    "snapshot.branches.entries",
                    "contains a duplicate durable entry ID",
                ));
            }
        }
        if !self.truncated {
            for entry in &self.entries {
                if entry
                    .parent_entry_id
                    .as_ref()
                    .is_some_and(|parent| !branch_ids.contains(parent))
                {
                    return Err(ValidationError::new(
                        "snapshot.branches.entry.parent_entry_id",
                        "must reference a preserved branch entry",
                    ));
                }
            }
        }
        if self
            .head
            .as_ref()
            .is_some_and(|head| !branch_ids.contains(head))
        {
            return Err(ValidationError::new(
                "snapshot.branches.head",
                "must reference a preserved branch entry",
            ));
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
        if self.durable_head != self.branches.head {
            return Err(ValidationError::new(
                "snapshot.branches.head",
                "must match the snapshot durable head",
            ));
        }
        self.branches.validate()?;
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
        if self.capabilities.project_file_write && !self.capabilities.project_file_browser {
            return Err(ValidationError::new(
                "bootstrap.capabilities.project_file_write",
                "requires the project file browser capability",
            ));
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
        let mut default_projects = 0usize;
        for project in &self.projects {
            project.validate()?;
            if !project_ids.insert(project.id.clone()) {
                return Err(ValidationError::new(
                    "bootstrap.projects",
                    "contains a duplicate project ID",
                ));
            }
            default_projects += usize::from(project.is_default);
        }
        if default_projects > 1 {
            return Err(ValidationError::new(
                "bootstrap.projects",
                "contains more than one default project",
            ));
        }
        let mut session_ids = BTreeSet::new();
        for session in &self.sessions {
            session.validate()?;
            if session
                .project_id
                .as_ref()
                .is_some_and(|project_id| !project_ids.contains(project_id))
            {
                return Err(ValidationError::new(
                    "bootstrap.sessions.project_id",
                    "must identify an advertised project",
                ));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_input_pricing_requires_bounded_safe_ascending_tiers() {
        let pricing = ModelInputPricing {
            base_microdollars_per_million_tokens: 3_000_000,
            tiers: vec![
                ModelInputPricingTier {
                    min_input_tokens: 100_000,
                    microdollars_per_million_tokens: 6_000_000,
                },
                ModelInputPricingTier {
                    min_input_tokens: 200_000,
                    microdollars_per_million_tokens: 9_000_000,
                },
            ],
        };
        pricing.validate().unwrap();

        let mut unordered = pricing.clone();
        unordered.tiers.reverse();
        assert!(unordered.validate().is_err());

        let mut unsafe_rate = pricing;
        unsafe_rate.base_microdollars_per_million_tokens = MAX_JSON_SAFE_INTEGER + 1;
        assert!(unsafe_rate.validate().is_err());

        let too_many = ModelInputPricing {
            base_microdollars_per_million_tokens: 1,
            tiers: (0..=MAX_MODEL_INPUT_PRICING_TIERS)
                .map(|index| ModelInputPricingTier {
                    min_input_tokens: index as u64,
                    microdollars_per_million_tokens: 1,
                })
                .collect(),
        };
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn user_message_delivery_is_additive_and_legacy_safe() {
        let legacy = serde_json::json!({
            "type": "userMessage",
            "data": {
                "text": "hello",
                "attachments": []
            }
        });
        let decoded: ItemPayload = serde_json::from_value(legacy.clone()).unwrap();
        assert!(matches!(
            decoded,
            ItemPayload::UserMessage { delivery: None, .. }
        ));
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);

        for (delivery, expected) in [
            (UserMessageDelivery::Submit, "submit"),
            (UserMessageDelivery::Steer, "steer"),
            (UserMessageDelivery::FollowUp, "followUp"),
        ] {
            let encoded = serde_json::to_value(ItemPayload::UserMessage {
                text: "hello".into(),
                attachments: Vec::new(),
                documents: Vec::new(),
                project_files: Vec::new(),
                delivery: Some(delivery),
                branch_provenance: None,
            })
            .unwrap();
            assert_eq!(encoded["data"]["delivery"], expected);
        }
    }

    #[test]
    fn truncated_branch_projection_allows_an_omitted_parent_but_keeps_its_head() {
        let head = DurableEntryId::new("entry-recent").unwrap();
        let graph = SessionBranchGraph {
            head: Some(head.clone()),
            entries: vec![SessionBranchEntry {
                entry_id: head,
                parent_entry_id: Some(DurableEntryId::new("entry-omitted").unwrap()),
                kind: SessionBranchEntryKind::AssistantMessage,
                checkoutable: true,
                label: "Recent answer".into(),
            }],
            truncated: true,
        };
        graph.validate().unwrap();

        let mut complete = graph;
        complete.truncated = false;
        assert!(complete.validate().is_err());
    }

    fn branch_provenance(operation: ConversationBranchOperation) -> ConversationBranchProvenance {
        ConversationBranchProvenance {
            operation,
            source_session_id: SessionId::new("source-session").unwrap(),
            source_entry_id: DurableEntryId::new("source-entry").unwrap(),
            originating_user_entry_id: None,
            model_override: None,
            external_effects_preserved: true,
            warning:
                "Conversation history changed; external effects are preserved and not rolled back."
                    .into(),
        }
    }

    fn session_summary(lifecycle: SessionCatalogState) -> SessionSummary {
        let (archived, retention) = match lifecycle {
            SessionCatalogState::Active => (false, None),
            SessionCatalogState::Archived => (true, None),
            SessionCatalogState::Trash => (
                true,
                Some(SessionRetention {
                    trashed_at_ms: 10,
                    purge_after_ms: 20,
                    permanent_delete_requires_confirmation: true,
                }),
            ),
        };
        SessionSummary {
            id: SessionId::new("session").unwrap(),
            project_id: None,
            title: "Session".into(),
            tags: Vec::new(),
            created_at_ms: 1,
            modified_at_ms: 2,
            pinned: false,
            archived,
            lifecycle,
            retention,
            forked_from: None,
            provisional: false,
            live_state: SessionLiveState::Idle,
            attention: AttentionState::None,
            pull_request: None,
            owner: ActorOwnerState::Inactive,
            model: ModelSelection {
                provider: "provider".into(),
                model: "model".into(),
                reasoning: "high".into(),
            },
        }
    }

    #[test]
    fn lifecycle_validation_keeps_trash_recoverable_and_confirmation_guarded() {
        for lifecycle in [
            SessionCatalogState::Active,
            SessionCatalogState::Archived,
            SessionCatalogState::Trash,
        ] {
            session_summary(lifecycle).validate().unwrap();
        }

        let mut missing_retention = session_summary(SessionCatalogState::Trash);
        missing_retention.retention = None;
        assert!(missing_retention.validate().is_err());

        let mut unsafe_delete = session_summary(SessionCatalogState::Trash);
        unsafe_delete
            .retention
            .as_mut()
            .unwrap()
            .permanent_delete_requires_confirmation = false;
        assert!(unsafe_delete.validate().is_err());

        let mut inconsistent_archive = session_summary(SessionCatalogState::Active);
        inconsistent_archive.archived = true;
        assert!(inconsistent_archive.validate().is_err());
    }

    #[test]
    fn branch_provenance_distinguishes_edit_retry_and_new_session_forks() {
        let edit = branch_provenance(ConversationBranchOperation::EditUserTurn);
        edit.validate().unwrap();
        let item = ItemPayload::UserMessage {
            text: "edited prompt".into(),
            attachments: Vec::new(),
            documents: Vec::new(),
            project_files: Vec::new(),
            delivery: Some(UserMessageDelivery::Submit),
            branch_provenance: Some(edit),
        };
        item.validate().unwrap();

        let mut retry = branch_provenance(ConversationBranchOperation::RetryResponse);
        retry.originating_user_entry_id = Some(DurableEntryId::new("originating-user").unwrap());
        retry.model_override = Some(ModelSelection {
            provider: "alternate-provider".into(),
            model: "alternate-model".into(),
            reasoning: "high".into(),
        });
        retry.validate().unwrap();

        let mut fork = session_summary(SessionCatalogState::Active);
        fork.forked_from = Some(branch_provenance(ConversationBranchOperation::ForkSession));
        fork.validate().unwrap();

        let mut misleading = branch_provenance(ConversationBranchOperation::EditUserTurn);
        misleading.warning = "Everything was reverted.".into();
        assert!(misleading.validate().is_err());

        let mut edit_with_model = branch_provenance(ConversationBranchOperation::EditUserTurn);
        edit_with_model.model_override = Some(ModelSelection {
            provider: "provider".into(),
            model: "model".into(),
            reasoning: "high".into(),
        });
        assert!(edit_with_model.validate().is_err());
    }
}
