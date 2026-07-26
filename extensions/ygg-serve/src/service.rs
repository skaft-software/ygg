//! Adapter traits between the transport-neutral host and the real Ygg App.

use std::future;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    AttachmentError, AttachmentPolicy, AttachmentRef, AuthorityProfile, HostCapabilities,
    HostDescriptor, ModelSelection, ModelSummary, ProjectId, ProjectSummary, RunId, SanitizedError,
    SessionCommand, SessionSnapshot, SessionSummary, StoredAttachment, ThemeId, ThemeOption,
    TimestampedEvent,
};

/// Immutable, path-free content behind one host-minted opaque resource handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResource {
    /// Safe basename used for inline content disposition.
    pub display_name: String,
    /// Validated media type.
    pub media_type: String,
    /// Exact bounded bytes snapshotted by the host.
    pub bytes: Bytes,
    /// Lowercase SHA-256 digest of [`Self::bytes`].
    pub sha256: String,
}

/// Maximum immediate events one driver dispatch may return.
///
/// Long-running output must flow through [`SessionDriver::next_event`] so a
/// single command acknowledgement cannot allocate an unbounded projection
/// batch.
pub const MAX_DRIVER_OUTCOME_EVENTS: usize = 256;

/// Host-scoped fresh-session operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateSessionRequest {
    /// Optional project context.
    pub project_id: Option<ProjectId>,
    /// Fresh launch sessions remain provisional until meaningful mutation.
    pub provisional: bool,
    /// Initial authority, already clamped to the host ceiling.
    pub authority: AuthorityProfile,
    /// Explicit initial model/reasoning selection, or the host default.
    pub model: Option<ModelSelection>,
}

/// One valid seed for a session actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSeed {
    /// Sidebar/catalog projection.
    pub summary: SessionSummary,
    /// Complete selected-session projection.
    pub snapshot: SessionSnapshot,
}

impl SessionSeed {
    /// Validates cross-object identity and ownership invariants.
    pub fn validate(&self) -> Result<(), ServiceError> {
        use crate::ProtocolValidation;

        self.summary
            .validate()
            .map_err(|_| ServiceError::InvalidSeed)?;
        self.snapshot
            .validate()
            .map_err(|_| ServiceError::InvalidSeed)?;
        if self.summary.id != self.snapshot.session_id {
            return Err(ServiceError::InvalidSeed);
        }
        Ok(())
    }
}

/// Immediate result of routing one admitted command into a session driver.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriverCommandOutcome {
    /// Run admitted by the command when available.
    pub run_id: Option<RunId>,
    /// Immediate public events. Long-running streams arrive through
    /// [`SessionDriver::next_event`].
    pub events: Vec<TimestampedEvent>,
}

impl DriverCommandOutcome {
    /// Creates an outcome containing immediate events.
    pub fn with_events(events: Vec<TimestampedEvent>) -> Self {
        Self {
            run_id: None,
            events,
        }
    }

    /// Creates an outcome for a newly admitted run.
    pub fn run(run_id: RunId, events: Vec<TimestampedEvent>) -> Self {
        Self {
            run_id: Some(run_id),
            events,
        }
    }
}

/// Adapter failure categories. Private source chains stay in adapter logs.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// Session or project was not found.
    #[error("not found")]
    NotFound,
    /// Another process owns the session.
    #[error("session locked")]
    Locked,
    /// Command is invalid while the driver is in its current state.
    #[error("invalid command boundary")]
    InvalidBoundary,
    /// Requested capability is temporarily unavailable.
    #[error("temporarily unavailable")]
    Unavailable,
    /// Authenticated client lacks permission.
    #[error("unauthorized")]
    Unauthorized,
    /// Adapter returned an invalid initial seed.
    #[error("invalid session seed")]
    InvalidSeed,
    /// Private internal failure.
    #[error("internal service failure")]
    Internal,
}

impl ServiceError {
    /// Converts to a deliberately generic public error.
    pub fn into_public(self) -> SanitizedError {
        use crate::ErrorCode;

        match self {
            Self::NotFound => SanitizedError::public(
                ErrorCode::NotFound,
                "The requested session or project was not found.",
            ),
            Self::Locked => SanitizedError::public(
                ErrorCode::Locked,
                "Another process currently owns this session.",
            ),
            Self::InvalidBoundary => SanitizedError::public(
                ErrorCode::InvalidBoundary,
                "This command is not valid at the current run boundary.",
            ),
            Self::Unavailable => SanitizedError::public(
                ErrorCode::Unavailable,
                "The requested capability is temporarily unavailable.",
            )
            .with_retryable(true),
            Self::Unauthorized => SanitizedError::public(
                ErrorCode::Unauthorized,
                "This connected device is not authorized for that action.",
            ),
            Self::InvalidSeed | Self::Internal => SanitizedError::internal(),
        }
    }
}

/// One exclusive App/Agent/Session owner.
///
/// A real adapter stores `App` directly in this value. `dispatch` must route
/// admitted control without waiting for an entire run to finish; live
/// `AgentEvent` projection is yielded by `next_event`.
#[async_trait]
pub trait SessionDriver: Send + 'static {
    /// Returns the authoritative initial projection.
    fn seed(&self) -> SessionSeed;

    /// Routes a command to the owning App/Run boundary.
    async fn dispatch(
        &mut self,
        command: SessionCommand,
    ) -> Result<DriverCommandOutcome, ServiceError>;

    /// Waits for the next public semantic event.
    ///
    /// Returning `None` permanently closes the live-event side of the driver;
    /// command routing may continue. The default waits forever.
    async fn next_event(&mut self) -> Option<TimestampedEvent> {
        future::pending().await
    }
}

/// Host-level catalog and exclusive session-driver factory.
#[async_trait]
pub trait HostService: Send + Sync + 'static {
    /// Concrete exclusive session owner.
    type Driver: SessionDriver;

    /// Stable host identity.
    fn descriptor(&self) -> HostDescriptor;

    /// Capabilities actually available in the running transport.
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities::default()
    }

    /// Attachment policy when authenticated host ingest is available.
    fn attachment_policy(&self) -> Option<AttachmentPolicy> {
        self.capabilities().attachment_policy
    }

    /// Ingests one bounded attachment from the authenticated graphical transport.
    async fn ingest_attachment(
        &self,
        _display_name: &str,
        _media_type: &str,
        _bytes: Bytes,
    ) -> Result<AttachmentRef, AttachmentError> {
        Err(AttachmentError::Unavailable)
    }

    /// Returns one authoritative attachment for an authenticated content request.
    async fn attachment_content(&self, _handle: &str) -> Result<StoredAttachment, AttachmentError> {
        Err(AttachmentError::Unavailable)
    }

    /// Returns one immutable opaque resource for an authenticated content request.
    ///
    /// Handles are minted and registered by the trusted host. Transports must
    /// never reinterpret the handle as a path or URL.
    async fn resource_content(&self, _handle: &str) -> Result<StoredResource, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Maximum authority remote commands may select.
    ///
    /// FullAccess preserves Ygg's current default. Transport code may clamp
    /// this without changing the Agent's default configuration.
    fn authority_ceiling(&self) -> AuthorityProfile {
        AuthorityProfile::FullAccess
    }

    /// Authority choices displayed by graphical clients.
    fn authority_profiles(&self) -> Vec<AuthorityProfile> {
        match self.authority_ceiling() {
            AuthorityProfile::ReadOnly => vec![AuthorityProfile::ReadOnly],
            AuthorityProfile::Workspace => {
                vec![AuthorityProfile::ReadOnly, AuthorityProfile::Workspace]
            }
            AuthorityProfile::FullAccess => vec![
                AuthorityProfile::ReadOnly,
                AuthorityProfile::Workspace,
                AuthorityProfile::FullAccess,
            ],
        }
    }

    /// Bounded model-picker catalog, including unavailable selected entries.
    fn model_catalog(&self) -> Vec<ModelSummary>;

    /// Host-resolved, inert theme catalog.
    fn theme_catalog(&self) -> Vec<ThemeOption>;

    /// Current host theme selection.
    fn selected_theme_id(&self) -> ThemeId;

    /// Lists bounded project summaries.
    async fn list_projects(&self) -> Result<Vec<ProjectSummary>, ServiceError>;

    /// Lists bounded session summaries without replaying every transcript.
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ServiceError>;

    /// Creates one fresh session owner. This is the host-scoped operation used
    /// by every cold graphical launch.
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Self::Driver, ServiceError>;

    /// Opens an existing durable session under one exclusive owner.
    async fn open_session(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Self::Driver, ServiceError>;
}
