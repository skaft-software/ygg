#![deny(missing_docs)]

//! `ygg-agent` — stateful agent loop with tool execution and event streaming.
//!
//! Sits above [`ygg_ai`]: the AI crate turns a `Request` into a stream of
//! `StreamEvent`s; this crate orchestrates that stream. It reconstructs
//! provider requests from a persistent JSONL [`Session`], executes tool calls
//! through a small extension boundary, persists every semantic boundary
//! (complete messages and individual tool results — never streaming deltas),
//! and emits [`AgentEvent`]s to the caller. Only `ygg-ai`'s public canonical
//! types are used; provider wire formats never leak into this crate.
//!
//! See the [agent design](https://github.com/skaft-software/ygg/blob/main/docs/design/ygg-agent.md)
//! for the normative design.
//!
//! # Example
//!
//! ```no_run
//! use ygg_agent::{
//!     Agent, AgentConfig, CoreTools, EffectBroker, ExtensionHost, SandboxConfig, Session,
//! };
//! use ygg_ai::{AiClient, CacheRetention, ModelCatalog, ModelId, ReasoningConfig, ReasoningMode};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let catalog = ModelCatalog::builtin()?;
//! let mut extensions = ExtensionHost::new();
//! extensions.load(&CoreTools);
//!
//! let mut agent = Agent::new(AgentConfig {
//!     client: AiClient::new(),
//!     model: catalog.resolve(&ModelId("gpt-4o-mini".into()))?,
//!     session: Session::create("session.jsonl")?,
//!     system: "You are a coding agent.".into(),
//!     sandbox: SandboxConfig::new("."),
//!     effect_broker: EffectBroker::default(),
//!     extensions,
//!     max_turns: Some(40),
//!     reasoning: ReasoningConfig::Off,
//!     reasoning_mode: ReasoningMode::Standard,
//!     cache_retention: CacheRetention::Short,
//!     session_id: None,
//! })?;
//!
//! // Streaming: drive events and control concurrently.
//! let mut run = agent.prompt("Find where auth logic lives").await?;
//! let control = run.control();
//! while let Some(event) = run.next().await {
//!     // Render AgentEvent; use `control` (clonable) to steer/follow_up/abort.
//!     let _ = (&event, &control);
//! }
//! drop(run);
//!
//! // Or run to completion:
//! let output = agent.complete("Fix the failing tests").await?;
//! println!("{}", output.text);
//! # Ok(())
//! # }
//! ```
//!
//! # Crash semantics
//!
//! Read-only tools may opt into automatic replay after an unclean crash.
//! Mutating and extension tools are non-replayable by default: an unresolved
//! call is durably paired with an `indeterminate` error and requires explicit
//! user reconciliation, so Ygg never silently repeats an irreversible effect.
//! Session appends are synced before returning; see [`session`] for the
//! precise persistence and recovery rules.

pub mod agent;
pub mod artifact;
pub mod cache;
pub mod compaction;
pub mod context;
pub mod delegation;
pub mod effect;
pub mod events;
pub mod extension;
pub mod extension_policy;
pub mod extension_process;
pub mod extension_secret;
pub mod goal_driver;
pub mod goal_store;
pub mod input;
pub mod sandbox;
pub mod secure_fs;
pub mod session;
/// The generic skill substrate containing descriptors, load errors, trust levels, and the registry trait.
pub mod skills;
pub mod tool;
pub mod tools;

pub use agent::{
    public_error_diagnostic, Agent, AgentCompactionMode, AgentConfig, AgentError, CompletionPolicy,
    RequestContextEstimate, Run, RunControl, RunOutput,
};
pub use artifact::{
    ArtifactError, ArtifactGenerationSettlement, ArtifactId, ArtifactPublication, ArtifactSource,
    ArtifactStore, ArtifactStoreLimits, PublishedArtifact, ResolvedArtifact,
    DEFAULT_MAX_ARTIFACTS_PER_GENERATION, DEFAULT_MAX_ARTIFACT_BYTES,
    DEFAULT_MAX_ARTIFACT_GENERATION_BYTES, DEFAULT_MAX_INLINE_ARTIFACT_BYTES,
    MAX_ARTIFACT_RELATIVE_PATH_BYTES,
};
pub use cache::{
    analyze_session_cache, analyze_session_cache_stats, CacheMiss, CacheStats,
    CACHE_MISS_NOISE_TOKENS,
};
pub use compaction::{
    build_branch_handoff_message, build_handoff_message, build_turn_prefix_handoff_message,
    choose_first_kept_by_tokens, finish_branch_handoff, finish_handoff, format_file_operations,
    prepare_branch_handoff, prepare_handoff, serialize_conversation, BranchHandoffPreparation,
    CompactionDetails, HandoffPreparation, BRANCH_SUMMARY_PREAMBLE, DEFAULT_KEEP_RECENT_TOKENS,
    SUMMARIZATION_SYSTEM_PROMPT, SUMMARY_OUTPUT_TOKENS, TURN_PREFIX_OUTPUT_TOKENS,
};
pub use context::{
    ActiveContextCompaction, ContextBreakdown, ContextSnapshot, FinishedContextCompaction,
    RunPhase, RunTerminalState,
};
pub use delegation::{
    delegation_runtime_supports, DelegatedAgentStatus, DelegationConfig, DelegationError,
    DelegationLimits, DelegationMode, COLLABORATION_TOOL_NAMES,
};
pub use effect::{
    EffectAuthorization, EffectBroker, EffectBrokerError, EffectGrantToken, EffectIntent,
    EffectPolicy, EffectReceipt, ToolEffect, EFFECT_POLICY_VERSION, MAX_EFFECT_GRANTS,
    MAX_EFFECT_GRANT_TTL, MAX_EFFECT_INTENT_BYTES,
};
pub use events::{
    AgentEvent, CompactionInfo, CompactionKind, CompactionReason, Control, FinishReason,
    OutputChannel, QueueDeliveryMode,
};
pub use extension::{EventObserver, Extension, ExtensionHost, ToolCallHook};
pub use extension_policy::{
    ExtensionActionIntent, ExtensionAdapterHints, ExtensionApprovalStore, ExtensionApprovalToken,
    ExtensionIntentPolicy, ExtensionPolicyDecision, ExtensionPolicyError, ExtensionPolicyFrontend,
    MAX_EXTENSION_ACTION_INTENT_BYTES, MAX_EXTENSION_APPROVALS, MAX_EXTENSION_APPROVAL_TTL,
};
pub use extension_process::{
    default_extension_roots, discover_extension_manifests, load_extension_manifest_paths,
    AgentSessionListRequest, AgentSessionMessageRequest, AgentSessionSpawnRequest,
    AgentSessionTargetRequest, AgentSessionWaitRequest,
    CommandDefinition as ExtensionCommandDefinition, CommandOutput as ExtensionCommandOutput,
    ConfirmationRequest as ExtensionConfirmationRequest,
    ConfirmationResponse as ExtensionConfirmationResponse, ContextContribution,
    DiscoveredExtension, ExtensionActivation, ExtensionCapabilities, ExtensionCatalog,
    ExtensionContributions, ExtensionDiagnostic, ExtensionDiagnosticLevel, ExtensionEntrypoint,
    ExtensionEvent, ExtensionExecutionContext, ExtensionFilesystemAccess, ExtensionHealthSnapshot,
    ExtensionHealthState, ExtensionHook, ExtensionHookDisposition, ExtensionHookOutput,
    ExtensionHostState, ExtensionInputRequest, ExtensionInputResponse, ExtensionLifecycleEvent,
    ExtensionLifecycleOutcome, ExtensionManifest, ExtensionManifestInput,
    ExtensionNegotiatedProtocol, ExtensionNotification, ExtensionNotificationLevel,
    ExtensionOperationToken, ExtensionPolicy, ExtensionPolicyEvaluationRequest,
    ExtensionPolicyEvaluationResponse, ExtensionProcess, ExtensionProgressEncoding,
    ExtensionProgressEvent, ExtensionProgressStream, ExtensionProtocolLimits,
    ExtensionProtocolRequest, ExtensionProtocolResponse, ExtensionReloadReport, ExtensionRequestId,
    ExtensionResourceOwner, ExtensionRoot, ExtensionRuntimeConfig, ExtensionRuntimeError,
    ExtensionSource, ExtensionStatusContribution, ExtensionTrust, ExtensionUiSurface,
    RenderedToolCall, ToolCallOutput as ExtensionToolCallOutput, ToolCatalogUpdateResponse,
    ToolDefinition as ExtensionToolDefinition, ToolRegistrationRequest, ToolRenderSegment,
    ToolUnregistrationRequest, EXTENSION_API_VERSION, EXTENSION_API_VERSION_0_1,
    EXTENSION_API_VERSION_0_2, EXTENSION_FEATURE_AGENT_SESSIONS, EXTENSION_FEATURE_APPROVALS,
    EXTENSION_FEATURE_ARTIFACTS, EXTENSION_FEATURE_CONTENT_PARTS, EXTENSION_FEATURE_DYNAMIC_TOOLS,
    EXTENSION_FEATURE_LIFECYCLE_EVENTS, EXTENSION_FEATURE_POLICY_INTENTS,
    EXTENSION_FEATURE_REQUEST_CANCELLATION, EXTENSION_FEATURE_REQUEST_PROGRESS,
    EXTENSION_FEATURE_SECRETS, EXTENSION_MANIFEST_FILENAME,
    MAX_EXTENSION_CHILD_REQUEST_IDS_PER_GENERATION, MAX_EXTENSION_INPUT_PROMPT_BYTES,
    MAX_EXTENSION_INPUT_VALUE_BYTES, MAX_EXTENSION_RESULT_CONTENT_PARTS,
    MAX_EXTENSION_RESULT_MEDIA_BYTES,
};
pub use extension_secret::{
    ExtensionSecretBroker, ExtensionSecretError, ExtensionSecretRequest, ExtensionSecretValue,
    MAX_EXTENSION_SECRET_BYTES,
};
pub use goal_driver::{
    continuation_prompt, detect_goal_marker, GoalContinuation, GoalDecision, GoalDriver,
    GoalDriverError, GoalMarker, GoalState, GoalStatus, GoalStore, GoalTurnSource,
    DEFAULT_GOAL_GRACE_PERIOD, GOAL_CONTINUATION_PROMPT_TEMPLATE,
};
pub use goal_store::{
    DurableGoalStore, DurableGoalStoreError, GoalAction, MAX_GOAL_OBJECTIVE_BYTES,
    MAX_GOAL_TURN_BUDGET,
};
pub use input::{InputPart, UserInput};
pub use sandbox::SandboxConfig;
pub use session::{
    Checkpoint, Entry, EntryId, EntryMetadata, EntryValue, Session, SessionError, SessionRecord,
    SessionRunOutcome, SessionRunOutcomeStatus, UsageRecord, UsageRecordKind,
};
pub use skills::{
    ContentHash, LoadedSkill, SkillActivationId, SkillDescriptor, SkillId, SkillLoadError,
    SkillQuery, SkillRegistry, SkillSearchResult, SkillSource, SkillTrust,
};
pub use tool::{
    content_hash, CancellationToken, ErasedTool, ErasedToolAdapter, OutputStream, ReplaySafety,
    Tool, ToolConcurrency, ToolContext, ToolDefinition, ToolDescriptor, ToolError,
    ToolInputRequest, ToolInputResponse, ToolInputValidationIssue, ToolOutput,
    ToolOutputContentPart, ToolOutputDetails, ToolOutputMediaKind, ToolOutputValidationError,
    ToolProgress, ToolProgressSink, TypedTool, TypedToolAdapter, ValidateToolInput,
    MAX_PROGRESS_CHUNK_BYTES, MAX_TOOL_METADATA_BYTES, MAX_TOOL_STRUCTURED_CONTENT_BYTES,
};
pub use tools::{BashTool, CoreTools, EditTool, ReadTool, SearchTool, WriteTool};
