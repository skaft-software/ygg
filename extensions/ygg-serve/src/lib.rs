//! Frontend-neutral backend contracts for the optional `ygg serve` experiment.
//!
//! This crate is intentionally independent of Ygg's TUI and core Agent crates.
//! A first-party adapter owns the real application and projects its semantics
//! through [`HostService`] and [`SessionDriver`].

#![forbid(unsafe_code)]

mod actor;
mod attachment;
mod bounds;
mod command;
mod document_ingest;
mod document_store;
mod embedded_web;
mod error;
mod event;
mod fs;
mod goal;
mod ids;
mod journal;
mod model;
mod process_tree;
mod project_registry;
mod prompt_context;
mod pty;
mod repository_context;
mod resource;
mod runtime_status;
mod service;
mod supervisor;
mod test_results;
mod theme;
mod transcript_search;
mod transport;
mod trusted_files;
mod usage;

pub use actor::{
    ActorConfig, ActorError, ActorView, CommandAdmission, SessionActor, SessionActorCore,
    SessionActorHandle,
};
pub use attachment::{
    validate_reference_set, AttachmentError, AttachmentFingerprint, AttachmentStore,
    StoredAttachment, MAX_ATTACHMENT_COUNT, MAX_ATTACHMENT_FILE_BYTES, MAX_ATTACHMENT_TOTAL_BYTES,
};
pub use bounds::{
    sanitize_public_text, validate_json, validate_public_text, ProtocolValidation, ValidationError,
    MAX_BOOTSTRAP_BYTES, MAX_COMMAND_BYTES, MAX_EVENT_BYTES, MAX_ITEM_TEXT_BYTES, MAX_PROMPT_BYTES,
    MAX_PUBLIC_TEXT_BYTES, MAX_SNAPSHOT_BYTES,
};
pub use command::{
    AckDisposition, AttachmentRef, CommandAck, HostAckDisposition, HostCommand, HostCommandAck,
    HostCommandEnvelope, PermanentDeleteConfirmation, PromptInput, RequestAnswer, SessionCommand,
    SessionCommandEnvelope, SlashCommandInvocation,
};
pub use document_ingest::{
    ingest_document, DocumentIngestError, DocumentMediaType, DocumentProvenance,
    ExtractionFidelity, IngestedDocument, MAX_DOCUMENT_FILE_BYTES, MAX_DOCUMENT_TEXT_BYTES,
    MAX_PDF_NESTING_DEPTH, MAX_PDF_OBJECTS, MAX_PDF_PAGES, MAX_PDF_STREAM_DECOMPRESSED_BYTES,
    MAX_PDF_TOTAL_DECOMPRESSED_BYTES,
};
pub use document_store::{
    DocumentId, DocumentPromptContext, DocumentReference, DocumentStore, DocumentStoreError,
    StoredDocument, MAX_DOCUMENTS_PER_PROMPT, MAX_STORED_DOCUMENTS_PER_SESSION,
};
pub use error::{ErrorCode, SanitizedError};
pub use event::{
    EventEnvelope, EventPayload, HostCatalogChange, HostStreamEvent, ItemDelta, ReplayGap,
    ReplayResponse, TimestampedEvent,
};
pub use fs::{
    ProjectFileEntry, ProjectFileEntryKind, ProjectFileRead, ProjectFileSearchHit,
    ProjectFileSearchResult, ProjectFileSystem, ProjectFileSystemError, ProjectFileTree,
    ProjectFileWrite, MAX_PROJECT_FILE_PATH_BYTES, MAX_PROJECT_FILE_PATH_COMPONENTS,
    MAX_PROJECT_FILE_READ_BYTES, MAX_PROJECT_FILE_SEARCH_BYTES, MAX_PROJECT_FILE_SEARCH_DEPTH,
    MAX_PROJECT_FILE_SEARCH_FILES, MAX_PROJECT_FILE_SEARCH_QUERY_BYTES,
    MAX_PROJECT_FILE_SEARCH_RESULTS, MAX_PROJECT_FILE_TREE_ENTRIES, MAX_PROJECT_FILE_WRITE_BYTES,
};
pub use goal::{
    GoalAction, GoalState, GoalStatus, GoalStore, GoalStoreError, MAX_GOAL_OBJECTIVE_BYTES,
    MAX_GOAL_TURN_BUDGET,
};
pub use ids::{
    ArtifactId, CommandId, DeviceId, DurableEntryId, HostId, ItemId, ProjectId, RequestId, RunId,
    SessionId, SourceId, ThemeId, TurnId,
};
pub use journal::{EventJournal, JournalConfig, JournalError};
pub use model::{
    ActivityPhase, ActivityPhaseSummary, ActorOwnerState, AgentRunPhase, AgentRunTelemetry,
    AgentRunTerminalState, ArtifactKind, ArtifactRef, AttachmentPolicy, AttentionState,
    AuthorityProfile, CatalogCursor, CommandDiscovery, CommandSuggestion, CommandSuggestionKind,
    CompletionReview, ContextUsage, ConversationBranchOperation, ConversationBranchProvenance,
    EvidenceCoverage, ExtensionPresentation, FileChange, HostBootstrap, HostCapabilities,
    HostDescriptor, InputModality, ItemLifecycle, ItemPayload, ModelInputPricing,
    ModelInputPricingTier, ModelSelection, ModelSummary, PendingRequest, PlanStep, PlanStepState,
    PreviewRef, ProjectCatalog, ProjectSummary, PullRequestState, PullRequestSummary, RequestKind,
    RequestState, RunOutcome, SessionBranchEntry, SessionBranchEntryKind, SessionBranchGraph,
    SessionCatalogState, SessionCursor, SessionItem, SessionLiveState, SessionRetention,
    SessionSnapshot, SessionSummary, SkillSuggestion, SourceKind, SourceRef, ToolActivity,
    ToolActivityStatus, ToolKind, ToolResultSummary, UsageSnapshot, UserMessageDelivery,
    MAX_MODEL_INPUT_PRICING_TIERS,
};
pub use project_registry::{
    ProjectId as RegistryProjectId, ProjectRegistry, ProjectRegistryError, ProjectRoot,
    ProjectState as RegistryProjectState, ProjectSummary as RegistryProjectSummary,
    MAX_PROJECTS as MAX_REGISTERED_PROJECTS, MAX_REGISTRY_STATE_BYTES,
};
pub use prompt_context::{
    compose_prompt_text, ComposedPromptText, PromptContextError,
    MAX_AUXILIARY_PROMPT_CONTEXT_BYTES, MAX_DOCUMENT_CONTEXT_BYTES, MAX_PROJECT_FILE_CONTEXT_BYTES,
};
pub use pty::{
    PtyAttachment, PtyError, PtyEvent, PtyExit, PtyManager, PtyOpenRequest, TerminalConfig,
    TerminalSession, MAX_PTY_COLUMNS, MAX_PTY_INPUT_BYTES, MAX_PTY_REPLAY_BYTES, MAX_PTY_ROWS,
    MAX_PTY_SESSIONS,
};
pub use repository_context::{
    refresh_repository_context, ContextRefreshState, ContextRefreshStatus,
    FolderInstructionContext, FolderInstructionFile, GitBranchState, GitFileStatus,
    GitFileStatusKind, GitRepositoryContext, GitWorktreeState, InstructionLoadError,
    InstructionLoadErrorCode, InstructionOrigin, InstructionStateSource, RepositoryContextError,
    RepositoryContextLoader, RepositoryContextSnapshot, RepositoryStateSource, RepositoryTrust,
};
pub use resource::{ResourceReference, ResourceStore, ResourceStoreError};
pub use runtime_status::*;
pub use service::{
    CreateSessionRequest, DriverCommandOutcome, DriverFinalizer, FinalizeCompletion,
    FinalizeDecision, HostService, ServiceError, SessionDriver, SessionSeed, StoredResource,
    MAX_DRIVER_OUTCOME_EVENTS,
};
pub use supervisor::{HostCommandAdmission, SessionSupervisor, SupervisorConfig, SupervisorError};
pub use test_results::{
    decode_structured_test_results, parse_test_output, ReportedTestCounts, StructuredTestCase,
    StructuredTestResults, StructuredTestSuite, TestCommandOutcome, TestCommandStatus,
    TestEvidenceCoverage, TestFramework, TestOutputInput, TestParseCoverage, TestResultDecodeError,
    TestResultParseError, TestResultParser, TestStatus, TestVerificationOutcome,
    MAX_REPORTED_TESTS, MAX_STRUCTURED_TEST_RESULTS_BYTES, MAX_TEST_CASES,
    MAX_TEST_CASES_PER_SUITE, MAX_TEST_LABEL_BYTES, MAX_TEST_OUTPUT_BYTES, MAX_TEST_OUTPUT_LINES,
    MAX_TEST_OUTPUT_LINE_BYTES, MAX_TEST_RESULT_LABEL_BYTES, MAX_TEST_SUITES, MAX_TEST_SUMMARIES,
};
pub use theme::{
    ColorScheme, SemanticRole, ThemeColor, ThemeDensity, ThemeDto, ThemeMotion, ThemeOption,
    ThemeRoleStyle, ThemeSourceClass, ThemeTypography,
};
pub use transcript_search::{
    SearchDocument, SearchDocumentKind, SearchError, SearchFilter, SearchHit, SearchMatchRange,
    TranscriptSearchIndex, TranscriptSearchLimits, TranscriptSearchRequest, TranscriptSearchResult,
    TranscriptSearchStats, MAX_SEARCH_DOCUMENTS, MAX_SEARCH_DOCUMENTS_PER_SESSION,
    MAX_SEARCH_DOCUMENT_TEXT_BYTES, MAX_SEARCH_INDEXED_TEXT_BYTES, MAX_SEARCH_POSTINGS,
    MAX_SEARCH_QUERY_CHARS, MAX_SEARCH_QUERY_TERMS, MAX_SEARCH_RESULTS, MAX_SEARCH_SNIPPET_CHARS,
    MAX_SEARCH_TERMS_PER_DOCUMENT, MAX_SEARCH_TERM_CHARS, MAX_SEARCH_UNIQUE_TERMS,
};
pub use transport::{LoopbackConfig, LoopbackServer, TransportError};
pub use trusted_files::{
    FileEntryId, TrustedFileContext, TrustedFileEntry, TrustedFileError, TrustedFileIndexSummary,
    TrustedFileKind, TrustedFileRead, TrustedFileSearchHit, TrustedFileSearchResult,
    TrustedProjectFiles, MAX_TRUSTED_FILES_PER_CONTEXT, MAX_TRUSTED_FILE_BYTES,
    MAX_TRUSTED_FILE_CONTEXT_BYTES,
};
pub use usage::{
    InferenceRequest, InferenceRequestStore, LifetimeMetricsStore, LifetimeUsage, ModelUsage,
    UsageActivity, UsageActivityDay, UsagePeriod, UsageStats, UsageStoreError,
    MAX_USAGE_MODEL_ROWS, USAGE_ACTIVITY_WEEKS,
};

/// Current experimental wire-protocol major.
pub const PROTOCOL_VERSION: u16 = 1;
