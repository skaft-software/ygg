//! Adapter traits between the transport-neutral host and the real Ygg App.

use std::future;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::oneshot;

use crate::{
    AttachmentError, AttachmentPolicy, AttachmentRef, AuthorityProfile, DocumentReference,
    FileEntryId, HostCapabilities, HostDescriptor, LifetimeUsage, ModelSelection, ModelSummary,
    PermanentDeleteConfirmation, ProjectId, ProjectSummary, RepositoryContextSnapshot, RunId,
    SanitizedError, SessionCatalogState, SessionCommand, SessionId, SessionSnapshot,
    SessionSummary, StoredAttachment, ThemeId, ThemeOption, TimestampedEvent,
    TranscriptSearchRequest, TranscriptSearchResult, TrustedFileEntry, TrustedFileIndexSummary,
    TrustedFileRead, TrustedFileSearchResult, UsageActivity, UsagePeriod, UsageStats,
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
#[derive(Debug, Default)]
pub struct DriverCommandOutcome {
    /// Run admitted by the command when available.
    pub run_id: Option<RunId>,
    /// Genuinely new durable session created by a conversation fork.
    pub created_session_id: Option<SessionId>,
    /// Immediate public events. Long-running streams arrive through
    /// [`SessionDriver::next_event`].
    pub events: Vec<TimestampedEvent>,
    /// Complete internal replacement after a durable branch checkout.
    ///
    /// This does not serialize into the event journal. The actor installs it
    /// atomically, assigns the next cursor, and publishes only a compact
    /// replacement signal for clients to refetch once.
    pub(crate) replacement: Option<ProjectionReplacement>,
}

pub(crate) struct ProjectionReplacement {
    pub(crate) seed: SessionSeed,
    decision: Option<oneshot::Sender<FinalizeDecision>>,
    completion: Option<oneshot::Receiver<Result<FinalizeCompletion, ServiceError>>>,
}

impl std::fmt::Debug for ProjectionReplacement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectionReplacement")
            .field("seed", &self.seed)
            .field("guarded", &self.decision.is_some())
            .finish()
    }
}

impl ProjectionReplacement {
    pub(crate) fn begin_finalize(
        &mut self,
        decision: FinalizeDecision,
    ) -> Result<oneshot::Receiver<Result<FinalizeCompletion, ServiceError>>, ServiceError> {
        let sender = self.decision.take().ok_or(ServiceError::Internal)?;
        let completion = self.completion.take().ok_or(ServiceError::Internal)?;
        sender
            .send(decision)
            .map_err(|_| ServiceError::Unavailable)?;
        Ok(completion)
    }
}

/// Actor decision at the durable projection replacement boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalizeDecision {
    /// Install the rebuilt App/projection for the selected durable head.
    Commit,
    /// Restore the previous durable head and reopen the previous App.
    Rollback,
}

/// Driver confirmation after completing an actor finalization decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalizeCompletion {
    /// The replacement App/projection is installed.
    Committed,
    /// The previous durable head and App/projection are restored.
    RolledBack,
}

/// Driver side of the bounded two-way replacement finalization protocol.
pub struct DriverFinalizer {
    decision: oneshot::Receiver<FinalizeDecision>,
    completion: Option<oneshot::Sender<Result<FinalizeCompletion, ServiceError>>>,
}

impl std::fmt::Debug for DriverFinalizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DriverFinalizer").finish()
    }
}

impl DriverFinalizer {
    /// Waits for the actor's one finalization decision.
    pub async fn decision(&mut self) -> Result<FinalizeDecision, ServiceError> {
        (&mut self.decision)
            .await
            .map_err(|_| ServiceError::Unavailable)
    }

    /// Reports completion only after the chosen durable/App state is installed.
    pub fn complete(
        mut self,
        completion: Result<FinalizeCompletion, ServiceError>,
    ) -> Result<(), ServiceError> {
        self.completion
            .take()
            .ok_or(ServiceError::Internal)?
            .send(completion)
            .map_err(|_| ServiceError::Unavailable)
    }
}

impl DriverCommandOutcome {
    /// Creates an outcome containing immediate events.
    pub fn with_events(events: Vec<TimestampedEvent>) -> Self {
        Self {
            run_id: None,
            created_session_id: None,
            events,
            replacement: None,
        }
    }

    /// Creates an outcome for a newly admitted run.
    pub fn run(run_id: RunId, events: Vec<TimestampedEvent>) -> Self {
        Self {
            run_id: Some(run_id),
            created_session_id: None,
            events,
            replacement: None,
        }
    }

    /// Creates a replacement that the driver must not finalize until the
    /// actor confirms its journal and projection can be committed atomically.
    pub fn guarded_replace(seed: SessionSeed) -> (Self, DriverFinalizer) {
        let (decision, resolution) = oneshot::channel();
        let (completion, finalized) = oneshot::channel();
        (
            Self {
                run_id: None,
                created_session_id: None,
                events: Vec::new(),
                replacement: Some(ProjectionReplacement {
                    seed,
                    decision: Some(decision),
                    completion: Some(finalized),
                }),
            },
            DriverFinalizer {
                decision: resolution,
                completion: Some(completion),
            },
        )
    }

    /// Creates an outcome for a committed new-session conversation fork.
    pub fn fork(created_session_id: SessionId) -> Self {
        Self {
            run_id: None,
            created_session_id: Some(created_session_id),
            events: Vec::new(),
            replacement: None,
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
    /// A known durable resource failed its integrity check.
    #[error("durable resource is corrupt")]
    CorruptResource,
    /// A bounded result exceeded the graphical transport limit.
    #[error("payload too large")]
    PayloadTooLarge,
    /// Authenticated client lacks permission.
    #[error("unauthorized")]
    Unauthorized,
    /// Adapter returned an invalid initial seed.
    #[error("invalid session seed")]
    InvalidSeed,
    /// A durable mutation could not be rolled back. The serialized owner must
    /// be retired without publishing or caching an acknowledgement.
    #[error("session owner lost durable consistency")]
    OwnerLost,
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
            Self::CorruptResource => SanitizedError::public(
                ErrorCode::Unavailable,
                "The requested resource is no longer available.",
            ),
            Self::PayloadTooLarge => SanitizedError::public(
                ErrorCode::PayloadTooLarge,
                "The requested payload exceeds the graphical transport limit.",
            ),
            Self::Unauthorized => SanitizedError::public(
                ErrorCode::Unauthorized,
                "This connected device is not authorized for that action.",
            ),
            Self::InvalidSeed | Self::OwnerLost | Self::Internal => SanitizedError::internal(),
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

    /// Quiesces every detached task and durable writer owned by this driver.
    ///
    /// The session supervisor does not permit a replacement owner until this
    /// future completes. Drivers without detached work may use the default.
    async fn shutdown(&mut self) {}
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

    /// Whether text/Markdown/PDF document ingest is available.
    fn document_ingest_supported(&self) -> bool {
        false
    }

    /// Ingests one immutable document for an authoritative project/session binding.
    async fn ingest_document(
        &self,
        _session_id: &crate::SessionId,
        _display_name: &str,
        _media_type: &str,
        _bytes: Bytes,
    ) -> Result<DocumentReference, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Lists path-free uploaded documents owned by one session.
    async fn list_documents(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<Vec<DocumentReference>, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Whether root-confined trusted-project file browsing is available.
    fn trusted_project_files_supported(&self) -> bool {
        false
    }

    /// Returns the bounded trusted-file index status for one project.
    async fn trusted_file_index(
        &self,
        _project_id: &ProjectId,
    ) -> Result<TrustedFileIndexSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Lists safe project-relative file metadata.
    async fn list_trusted_files(
        &self,
        _project_id: &ProjectId,
        _limit: usize,
    ) -> Result<Vec<TrustedFileEntry>, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Searches safe project-relative file names and bounded text.
    async fn search_trusted_files(
        &self,
        _project_id: &ProjectId,
        _query: &str,
        _limit: usize,
    ) -> Result<TrustedFileSearchResult, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Reads one immutable trusted-file snapshot by opaque entry identity.
    async fn read_trusted_file(
        &self,
        _project_id: &ProjectId,
        _entry_id: &FileEntryId,
    ) -> Result<TrustedFileRead, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Whether authenticated search over durable public transcript projections is available.
    fn transcript_search_supported(&self) -> bool {
        false
    }

    /// Searches durable, already-redacted transcript projections.
    async fn search_transcripts(
        &self,
        _request: &TranscriptSearchRequest,
    ) -> Result<TranscriptSearchResult, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Whether trusted repository and folder-instruction context is available.
    fn repository_context_supported(&self) -> bool {
        false
    }

    /// Refreshes path-free Git and folder-instruction context for one trusted project.
    async fn repository_context(
        &self,
        _project_id: &ProjectId,
    ) -> Result<RepositoryContextSnapshot, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Returns one immutable opaque resource for an authenticated session request.
    ///
    /// Handles are minted and registered by the trusted host. Transports must
    /// never reinterpret the handle as a path or URL, and a handle minted for
    /// one session must look absent from every other session.
    async fn resource_content(
        &self,
        _session_id: &crate::SessionId,
        _handle: &str,
    ) -> Result<StoredResource, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Produces one bounded, redacted portable session package.
    ///
    /// Adapters must not accept client-selected paths or raw-secret options.
    async fn session_export(&self, _session_id: &crate::SessionId) -> Result<Bytes, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Returns current daily or trailing-seven-day inference usage totals.
    async fn usage_stats(&self, _period: UsagePeriod) -> Result<UsageStats, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Returns all retained inference usage totals.
    async fn usage_lifetime(&self) -> Result<LifetimeUsage, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Returns recent daily inference activity and lifetime streaks.
    async fn usage_activity(&self) -> Result<UsageActivity, ServiceError> {
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

    /// Whether authenticated project import and lifecycle mutations are
    /// supported by this concrete host/platform.
    fn project_lifecycle_mutations_supported(&self) -> bool {
        false
    }

    /// Whether a host-native picker can mint opaque import candidates.
    fn project_import_supported(&self) -> bool {
        false
    }

    /// Consumes an opaque host-minted selection as an initially untrusted
    /// project. Browser-authored paths are never accepted at this boundary.
    async fn import_project(
        &self,
        _candidate_id: &str,
        _display_name: Option<&str>,
    ) -> Result<ProjectSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Renames one project without changing its root authority.
    async fn rename_project(
        &self,
        _project_id: &ProjectId,
        _display_name: &str,
    ) -> Result<ProjectSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Makes one active project the default.
    async fn set_default_project(
        &self,
        _project_id: &ProjectId,
    ) -> Result<ProjectSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Clears the project default.
    async fn clear_default_project(&self) -> Result<(), ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Grants or revokes explicit execution trust.
    async fn set_project_trust(
        &self,
        _project_id: &ProjectId,
        _trusted: bool,
    ) -> Result<ProjectSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Archives a project and revokes trust.
    async fn archive_project(
        &self,
        _project_id: &ProjectId,
    ) -> Result<ProjectSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Whether recoverable session trash and confirmed permanent deletion are
    /// supported by this host.
    fn session_trash_supported(&self) -> bool {
        false
    }

    /// Changes one durable session's catalog lifecycle after its graphical
    /// owner has quiesced.
    async fn set_session_lifecycle(
        &self,
        _session_id: &SessionId,
        _lifecycle: SessionCatalogState,
        _changed_at_ms: u64,
    ) -> Result<SessionSummary, ServiceError> {
        Err(ServiceError::Unavailable)
    }

    /// Permanently deletes one trashed session after exact confirmation.
    async fn delete_session_permanently(
        &self,
        _session_id: &SessionId,
        _confirmation: &PermanentDeleteConfirmation,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::Unavailable)
    }

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
