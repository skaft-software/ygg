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
//! use ygg_agent::{Agent, AgentConfig, CoreTools, ExtensionHost, SandboxConfig, Session};
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
pub mod cache;
pub mod compaction;
pub mod context;
pub mod events;
pub mod extension;
pub mod extension_process;
pub mod input;
pub mod sandbox;
pub mod secure_fs;
pub mod session;
/// The generic skill substrate containing descriptors, load errors, trust levels, and the registry trait.
pub mod skills;
pub mod tool;
pub mod tools;

pub use agent::{
    Agent, AgentConfig, AgentError, CompletionPolicy, RequestContextEstimate, Run, RunControl,
    RunOutput,
};
pub use cache::{
    analyze_session_cache, analyze_session_cache_stats, CacheMiss, CacheStats,
    CACHE_MISS_NOISE_TOKENS,
};
pub use compaction::{
    build_branch_handoff_message, build_handoff_message, finish_branch_handoff, finish_handoff,
    format_file_operations, prepare_branch_handoff, prepare_handoff, serialize_conversation,
    BranchHandoffPreparation, CompactionDetails, HandoffPreparation, BRANCH_SUMMARY_PREAMBLE,
    SUMMARIZATION_SYSTEM_PROMPT,
};
pub use context::ContextSnapshot;
pub use events::{
    AgentEvent, CompactionInfo, CompactionReason, Control, FinishReason, OutputChannel,
};
pub use extension::{EventObserver, Extension, ExtensionHost, ToolCallHook};
pub use extension_process::{
    default_extension_roots, discover_extension_manifests, load_extension_manifest_paths,
    CommandDefinition as ExtensionCommandDefinition, CommandOutput as ExtensionCommandOutput,
    ConfirmationRequest as ExtensionConfirmationRequest,
    ConfirmationResponse as ExtensionConfirmationResponse, ContextContribution,
    DiscoveredExtension, ExtensionActivation, ExtensionCapabilities, ExtensionCatalog,
    ExtensionContributions, ExtensionDiagnostic, ExtensionDiagnosticLevel, ExtensionEntrypoint,
    ExtensionEvent, ExtensionExecutionContext, ExtensionFilesystemAccess, ExtensionHook,
    ExtensionHookDisposition, ExtensionHookOutput, ExtensionHostState, ExtensionManifest,
    ExtensionManifestInput, ExtensionNotification, ExtensionNotificationLevel, ExtensionPolicy,
    ExtensionProcess, ExtensionReloadReport, ExtensionRoot, ExtensionRuntimeConfig,
    ExtensionRuntimeError, ExtensionSource, ExtensionStatusContribution, ExtensionTrust,
    ExtensionUiSurface, RenderedToolCall, ToolCallOutput as ExtensionToolCallOutput,
    ToolDefinition as ExtensionToolDefinition, ToolRenderSegment, EXTENSION_API_VERSION,
    EXTENSION_MANIFEST_FILENAME,
};
pub use input::{InputPart, UserInput};
pub use sandbox::SandboxConfig;
pub use session::{
    Checkpoint, Entry, EntryId, EntryMetadata, EntryValue, Session, SessionError, SessionRecord,
    UsageRecord, UsageRecordKind,
};
pub use skills::{
    ContentHash, LoadedSkill, SkillActivationId, SkillDescriptor, SkillId, SkillLoadError,
    SkillQuery, SkillRegistry, SkillSearchResult, SkillSource, SkillTrust,
};
pub use tool::{
    content_hash, CancellationToken, ErasedTool, ErasedToolAdapter, OutputStream, ReplaySafety,
    Tool, ToolContext, ToolDefinition, ToolDescriptor, ToolError, ToolInputRequest,
    ToolInputResponse, ToolInputValidationIssue, ToolOutput, ToolProgress, ToolProgressSink,
    TypedTool, TypedToolAdapter, ValidateToolInput, MAX_PROGRESS_CHUNK_BYTES,
};
pub use tools::{BashTool, CoreTools, EditTool, ReadTool, SearchTool, WriteTool};
