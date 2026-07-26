//! Frontend-neutral backend contracts for the optional `ygg serve` experiment.
//!
//! This crate is intentionally independent of Ygg's TUI and core Agent crates.
//! A first-party adapter owns the real application and projects its semantics
//! through [`HostService`] and [`SessionDriver`].

#![forbid(unsafe_code)]

mod actor;
mod bounds;
mod command;
mod error;
mod event;
mod ids;
mod journal;
mod model;
mod service;
mod supervisor;
mod theme;

pub use actor::{
    ActorConfig, ActorError, ActorView, CommandAdmission, SessionActor, SessionActorCore,
    SessionActorHandle,
};
pub use bounds::{
    sanitize_public_text, validate_json, validate_public_text, ProtocolValidation, ValidationError,
    MAX_BOOTSTRAP_BYTES, MAX_COMMAND_BYTES, MAX_EVENT_BYTES, MAX_ITEM_TEXT_BYTES, MAX_PROMPT_BYTES,
    MAX_PUBLIC_TEXT_BYTES, MAX_SNAPSHOT_BYTES,
};
pub use command::{
    AckDisposition, AttachmentRef, CommandAck, HostAckDisposition, HostCommand, HostCommandAck,
    HostCommandEnvelope, PromptInput, RequestAnswer, SessionCommand, SessionCommandEnvelope,
};
pub use error::{ErrorCode, SanitizedError};
pub use event::{
    EventEnvelope, EventPayload, ItemDelta, ReplayGap, ReplayResponse, TimestampedEvent,
};
pub use ids::{
    ArtifactId, CommandId, DeviceId, DurableEntryId, HostId, ItemId, ProjectId, RequestId, RunId,
    SessionId, SourceId, ThemeId, TurnId,
};
pub use journal::{EventJournal, JournalConfig, JournalError};
pub use model::{
    ActorOwnerState, ArtifactKind, ArtifactRef, AttentionState, AuthorityProfile, CatalogCursor,
    ContextUsage, FileChange, HostBootstrap, HostCapabilities, HostDescriptor, InputModality,
    ItemLifecycle, ItemPayload, ModelSelection, ModelSummary, PendingRequest, PlanStep,
    PlanStepState, PreviewRef, ProjectSummary, RequestKind, RequestState, RunOutcome,
    SessionCursor, SessionItem, SessionLiveState, SessionSnapshot, SessionSummary, SourceKind,
    SourceRef, UsageSnapshot,
};
pub use service::{
    CreateSessionRequest, DriverCommandOutcome, HostService, ServiceError, SessionDriver,
    SessionSeed,
};
pub use supervisor::{HostCommandAdmission, SessionSupervisor, SupervisorConfig, SupervisorError};
pub use theme::{
    ColorScheme, SemanticRole, ThemeColor, ThemeDensity, ThemeDto, ThemeMotion, ThemeOption,
    ThemeRoleStyle, ThemeSourceClass, ThemeTypography,
};

/// Current experimental wire-protocol major.
pub const PROTOCOL_VERSION: u16 = 1;
