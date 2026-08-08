//! Host catalog and exclusive multi-session actor supervision.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{broadcast, oneshot, watch, Mutex, RwLock};

use crate::{
    ActorConfig, ActorError, ActorView, AuthorityProfile, CatalogCursor, CommandAdmission,
    CommandId, CreateSessionRequest, DeviceId, ErrorCode, HostBootstrap, HostCommand,
    HostCommandAck, HostCommandEnvelope, HostService, HostStreamEvent, ModelSelection,
    ProjectCatalog, ProjectId, ProjectSummary, ProtocolValidation, ReplayResponse, SanitizedError,
    ServiceError, SessionActor, SessionActorHandle, SessionCommandEnvelope, SessionCursor,
    SessionDriver, SessionId, SessionSnapshot, SessionSummary, PROTOCOL_VERSION,
};

const HOST_COMMAND_CACHE_CAPACITY: usize = 2_048;
const HOST_EVENT_BROADCAST_CAPACITY: usize = 4_096;
const MAX_FRESH_PROVISIONAL_OWNERS: usize = 64;
const MAX_ACTIVE_SESSION_OWNERS: usize = 512;
const MAX_BOOTSTRAP_SESSION_SUMMARIES: usize = 2_000;
const DEFAULT_QUARANTINE_WAIT_TIMEOUT: Duration = Duration::from_millis(250);

/// Supervisor configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Per-session actor configuration.
    pub actor: ActorConfig,
    /// Requested initial authority for fresh sessions.
    pub fresh_session_authority: AuthorityProfile,
    /// Maximum time one open request waits for a retired owner to quiesce.
    ///
    /// Expiry is retryable and does not release the ownership fence. The
    /// background observer keeps the quarantined owner registered until its
    /// driver proves that every durable writer has stopped.
    pub quarantine_wait_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            actor: ActorConfig::default(),
            // Preserve Ygg's current default. HostService may clamp the remote
            // selection ceiling without changing local Agent defaults.
            fresh_session_authority: AuthorityProfile::FullAccess,
            quarantine_wait_timeout: DEFAULT_QUARANTINE_WAIT_TIMEOUT,
        }
    }
}

/// Host-supervision failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorError {
    /// Host adapter failure.
    #[error("host service failure: {0}")]
    Service(#[from] ServiceError),
    /// Session actor failure.
    #[error("session actor failure: {0}")]
    Actor(#[from] ActorError),
    /// Adapter attempted to create a second owner for the same session.
    #[error("session already has a graphical owner")]
    DuplicateOwner,
    /// Driver identity disagreed with the requested session.
    #[error("session driver returned a mismatched identity")]
    IdentityMismatch,
    /// Bootstrap failed public validation.
    #[error("invalid host bootstrap")]
    InvalidBootstrap,
}

/// Host-command admission result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCommandAdmission {
    /// Exact acknowledgement.
    pub ack: HostCommandAck,
    /// Whether this call reused an existing or in-flight acknowledgement.
    pub cached: bool,
}

type HostCommandKey = (DeviceId, CommandId);

enum HostCommandCacheEntry {
    InFlight {
        envelope: HostCommandEnvelope,
        waiters: Vec<oneshot::Sender<HostCommandAck>>,
    },
    Complete {
        envelope: HostCommandEnvelope,
        ack: HostCommandAck,
    },
}

struct SupervisorState {
    actors: BTreeMap<SessionId, SessionActorHandle>,
    blocked_projects: BTreeSet<ProjectId>,
    project_gates: BTreeMap<ProjectId, Weak<RwLock<()>>>,
    blocked_sessions: BTreeSet<SessionId>,
    session_gates: BTreeMap<SessionId, Weak<RwLock<()>>>,
    session_openings:
        BTreeMap<SessionId, Vec<oneshot::Sender<Result<SessionActorHandle, SupervisorError>>>>,
    host_commands: BTreeMap<HostCommandKey, HostCommandCacheEntry>,
    host_command_order: VecDeque<HostCommandKey>,
}

/// Owns the host catalog and exactly one graphical actor per live session.
pub struct SessionSupervisor<H: HostService> {
    host: Arc<H>,
    config: SupervisorConfig,
    state: Arc<Mutex<SupervisorState>>,
    catalog_cursor: Arc<AtomicU64>,
    host_event_order: Arc<std::sync::Mutex<u64>>,
    host_events: broadcast::Sender<HostStreamEvent>,
}

impl<H: HostService> Clone for SessionSupervisor<H> {
    fn clone(&self) -> Self {
        Self {
            host: Arc::clone(&self.host),
            config: self.config,
            state: Arc::clone(&self.state),
            catalog_cursor: Arc::clone(&self.catalog_cursor),
            host_event_order: Arc::clone(&self.host_event_order),
            host_events: self.host_events.clone(),
        }
    }
}

impl<H: HostService> SessionSupervisor<H> {
    /// Creates an empty supervisor around a first-party host adapter.
    pub fn new(host: Arc<H>, mut config: SupervisorConfig) -> Self {
        config.actor.authority_ceiling = host.authority_ceiling();
        config.fresh_session_authority =
            clamp_authority(config.fresh_session_authority, host.authority_ceiling());
        let (host_events, _) = broadcast::channel(HOST_EVENT_BROADCAST_CAPACITY);
        Self {
            host,
            config,
            state: Arc::new(Mutex::new(SupervisorState {
                actors: BTreeMap::new(),
                blocked_projects: BTreeSet::new(),
                project_gates: BTreeMap::new(),
                blocked_sessions: BTreeSet::new(),
                session_gates: BTreeMap::new(),
                session_openings: BTreeMap::new(),
                host_commands: BTreeMap::new(),
                host_command_order: VecDeque::new(),
            })),
            catalog_cursor: Arc::new(AtomicU64::new(1)),
            host_event_order: Arc::new(std::sync::Mutex::new(0)),
            host_events,
        }
    }

    /// Cold graphical launch: create a fresh provisional session and return
    /// the complete shell bootstrap with that session selected.
    pub async fn launch(
        &self,
        project_id: Option<crate::ProjectId>,
    ) -> Result<HostBootstrap, SupervisorError> {
        let handle = self.create_fresh_session(project_id).await?;
        self.bootstrap(handle.session_id()).await
    }

    /// Host-scoped New session operation.
    pub async fn create_fresh_session(
        &self,
        project_id: Option<crate::ProjectId>,
    ) -> Result<SessionActorHandle, SupervisorError> {
        self.create_owned_session(CreateSessionRequest {
            project_id,
            provisional: true,
            authority: self.config.fresh_session_authority,
            model: None,
        })
        .await
    }

    /// Opens or reuses the one graphical actor for an existing session.
    pub async fn open_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionActorHandle, SupervisorError> {
        enum Begin {
            Lead,
            Wait(oneshot::Receiver<Result<SessionActorHandle, SupervisorError>>),
            Return(SessionActorHandle),
            Quarantined(SessionActorHandle),
        }

        loop {
            let begin = {
                let mut state = self.state.lock().await;
                if state.blocked_sessions.contains(session_id) {
                    return Err(ServiceError::Unavailable.into());
                }
                if state.actors.get(session_id).is_some_and(|handle| {
                    handle
                        .view()
                        .summary
                        .project_id
                        .as_ref()
                        .is_some_and(|project_id| state.blocked_projects.contains(project_id))
                }) {
                    return Err(ServiceError::Unauthorized.into());
                }
                if let Some(existing) = state.actors.get(session_id) {
                    if existing.is_closed() {
                        Begin::Quarantined(existing.clone())
                    } else {
                        Begin::Return(existing.clone())
                    }
                } else if let Some(waiters) = state.session_openings.get_mut(session_id) {
                    let (sender, receiver) = oneshot::channel();
                    waiters.push(sender);
                    Begin::Wait(receiver)
                } else {
                    state
                        .session_openings
                        .insert(session_id.clone(), Vec::new());
                    Begin::Lead
                }
            };
            match begin {
                Begin::Return(handle) => return Ok(handle),
                Begin::Wait(receiver) => {
                    return receiver
                        .await
                        .map_err(|_| SupervisorError::Service(ServiceError::Unavailable))?
                }
                Begin::Quarantined(handle) => {
                    tokio::time::timeout(self.config.quarantine_wait_timeout, handle.quiesced())
                        .await
                        .map_err(|_| SupervisorError::Service(ServiceError::Unavailable))?;
                    self.wait_for_actor_registry_release(&handle).await;
                }
                Begin::Lead => break,
            }
        }

        // The owned task survives cancellation of the initiating request, so
        // the per-session reservation cannot strand later waiters.
        let supervisor = self.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let result = supervisor.open_session_factory(&session_id).await;
            supervisor
                .complete_session_open(session_id, result.clone())
                .await;
            result
        })
        .await
        .map_err(|_| SupervisorError::Service(ServiceError::Unavailable))?
    }

    /// Builds a catalog bootstrap without creating, opening, or selecting a session.
    pub async fn inventory_bootstrap(&self) -> Result<HostBootstrap, SupervisorError> {
        // Anchor the replay cursor before crossing asynchronous catalog
        // boundaries. Any change racing the snapshot then has a strictly newer
        // revision and remains replayable instead of being hidden behind a
        // cursor captured after stale list data.
        let catalog_cursor = self.catalog_cursor();
        let projects = self.project_catalog().await?.projects;
        let mut summaries = self
            .host
            .list_sessions()
            .await?
            .into_iter()
            .map(|summary| (summary.id.clone(), summary))
            .collect::<BTreeMap<_, _>>();
        let active = {
            let state = self.state.lock().await;
            state
                .actors
                .values()
                .filter(|handle| !handle.is_closed())
                .cloned()
                .collect::<Vec<_>>()
        };
        for handle in active {
            let view = handle.view();
            if !handle.is_closed() {
                insert_active_summary(&mut summaries, view.summary);
            }
        }
        let sessions = bounded_bootstrap_sessions(summaries, None);
        self.build_bootstrap(catalog_cursor, projects, sessions, None, None)
    }

    /// Builds a fresh catalog/bootstrap around an already hosted session.
    pub async fn bootstrap(
        &self,
        selected_session_id: &SessionId,
    ) -> Result<HostBootstrap, SupervisorError> {
        let catalog_cursor = self.catalog_cursor();
        let projects = self.project_catalog().await?.projects;
        let mut summaries = self
            .host
            .list_sessions()
            .await?
            .into_iter()
            .map(|summary| (summary.id.clone(), summary))
            .collect::<BTreeMap<_, _>>();
        // Catalog reads are adapter-owned asynchronous boundaries. Revalidate
        // ownership only after both complete so a driver that retired during
        // either call cannot contribute a stale selected snapshot.
        let selected = self.open_session(selected_session_id).await?;
        let active = {
            let state = self.state.lock().await;
            let selected_is_current = state
                .actors
                .get(selected_session_id)
                .is_some_and(|current| current.same_actor(&selected) && !current.is_closed());
            if !selected_is_current {
                return Err(ServiceError::Unavailable.into());
            }
            state
                .actors
                .values()
                .filter(|handle| !handle.is_closed())
                .cloned()
                .collect::<Vec<_>>()
        };
        let selected_view = selected.view();
        if selected.is_closed() {
            return Err(ServiceError::Unavailable.into());
        }
        let active_views = active
            .into_iter()
            .filter_map(|handle| {
                let view = handle.view();
                (!handle.is_closed()).then_some(view)
            })
            .collect::<Vec<_>>();
        for view in active_views {
            insert_active_summary(&mut summaries, view.summary);
        }
        let sessions = bounded_bootstrap_sessions(summaries, Some(selected_session_id));

        // This is the bootstrap's ownership linearization point. A close
        // observed here makes the caller retry through the quarantine-aware
        // open path instead of returning the retired actor's projection.
        if selected.is_closed() {
            return Err(ServiceError::Unavailable.into());
        }
        self.build_bootstrap(
            catalog_cursor,
            projects,
            sessions,
            Some(selected_session_id.clone()),
            Some(selected_view.snapshot),
        )
    }

    fn build_bootstrap(
        &self,
        catalog_cursor: CatalogCursor,
        projects: Vec<ProjectSummary>,
        sessions: Vec<SessionSummary>,
        selected_session_id: Option<SessionId>,
        selected_session: Option<SessionSnapshot>,
    ) -> Result<HostBootstrap, SupervisorError> {
        let bootstrap = HostBootstrap {
            protocol: PROTOCOL_VERSION,
            host: self.host.descriptor(),
            capabilities: self.host.capabilities(),
            catalog_cursor,
            models: self.host.model_catalog(),
            authority_profiles: self.host.authority_profiles(),
            authority_ceiling: self.host.authority_ceiling(),
            themes: self.host.theme_catalog(),
            selected_theme_id: self.host.selected_theme_id(),
            projects,
            sessions,
            selected_session_id,
            selected_session,
        };
        bootstrap
            .validate()
            .map_err(|_| SupervisorError::InvalidBootstrap)?;
        Ok(bootstrap)
    }

    /// Routes a device-scoped idempotent host command.
    pub async fn host_command(
        &self,
        envelope: HostCommandEnvelope,
        acknowledged_at_ms: u64,
    ) -> Result<HostCommandAdmission, SupervisorError> {
        enum Begin {
            Lead,
            Wait(oneshot::Receiver<HostCommandAck>),
            Return(HostCommandAdmission),
        }

        let key = (envelope.device_id.clone(), envelope.command_id.clone());
        let begin = {
            let mut state = self.state.lock().await;
            match state.host_commands.get_mut(&key) {
                Some(HostCommandCacheEntry::Complete {
                    envelope: original,
                    ack,
                }) if original == &envelope => Begin::Return(HostCommandAdmission {
                    ack: ack.clone(),
                    cached: true,
                }),
                Some(HostCommandCacheEntry::InFlight {
                    envelope: original,
                    waiters,
                }) if original == &envelope => {
                    let (sender, receiver) = oneshot::channel();
                    waiters.push(sender);
                    Begin::Wait(receiver)
                }
                Some(_) => Begin::Return(HostCommandAdmission {
                    ack: HostCommandAck::rejected(
                        self.host.descriptor().id,
                        envelope.command_id.clone(),
                        acknowledged_at_ms,
                        self.catalog_cursor(),
                        SanitizedError::public(
                            ErrorCode::CommandIdConflict,
                            "This command ID was already used for different content.",
                        ),
                    ),
                    cached: false,
                }),
                None => {
                    state.host_commands.insert(
                        key.clone(),
                        HostCommandCacheEntry::InFlight {
                            envelope: envelope.clone(),
                            waiters: Vec::new(),
                        },
                    );
                    Begin::Lead
                }
            }
        };

        match begin {
            Begin::Return(admission) => return Ok(admission),
            Begin::Wait(receiver) => {
                let ack = receiver
                    .await
                    .map_err(|_| SupervisorError::Service(ServiceError::Unavailable))?;
                return Ok(HostCommandAdmission { ack, cached: true });
            }
            Begin::Lead => {}
        }

        // Creation owns its own task so dropping the initiating HTTP request
        // cannot leave a permanent InFlight reservation.
        let supervisor = self.clone();
        let task_envelope = envelope.clone();
        let task_key = key;
        let ack = tokio::spawn(async move {
            let ack = supervisor
                .execute_host_command(&task_envelope, acknowledged_at_ms)
                .await;
            supervisor
                .complete_host_command(task_key, task_envelope, ack.clone())
                .await;
            ack
        })
        .await
        .map_err(|_| SupervisorError::Service(ServiceError::Unavailable))?;
        Ok(HostCommandAdmission { ack, cached: false })
    }

    /// Routes one session command, lazily opening its exclusive actor.
    pub async fn command(
        &self,
        envelope: SessionCommandEnvelope,
        acknowledged_at_ms: u64,
    ) -> Result<CommandAdmission, SupervisorError> {
        let handle = self.open_session(&envelope.session_id).await?;
        Ok(handle.command(envelope, acknowledged_at_ms).await?)
    }

    /// Returns slash-command and skill discovery for one exclusive session owner.
    pub async fn command_discovery(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::CommandDiscovery, SupervisorError> {
        let handle = self.open_session(session_id).await?;
        Ok(handle.command_discovery().await?)
    }

    /// Replays one session after a cursor.
    pub async fn replay_after(
        &self,
        session_id: &SessionId,
        after: SessionCursor,
    ) -> Result<ReplayResponse, SupervisorError> {
        let handle = self.open_session(session_id).await?;
        Ok(handle.replay_after(after).await?)
    }

    /// Current actor view, lazily opening the session if needed.
    pub async fn session_view(&self, session_id: &SessionId) -> Result<ActorView, SupervisorError> {
        Ok(self.open_session(session_id).await?.view())
    }

    /// Number of exclusive live graphical owners.
    pub async fn active_session_count(&self) -> usize {
        self.state.lock().await.actors.len()
    }

    /// Current host-catalog revision.
    pub fn catalog_cursor(&self) -> CatalogCursor {
        CatalogCursor(self.catalog_cursor.load(Ordering::Acquire))
    }

    /// Returns a session-free, path-free project catalog for trust onboarding.
    pub async fn project_catalog(&self) -> Result<ProjectCatalog, SupervisorError> {
        let mut projects = self.host.list_projects().await?;
        let live_project_ids = {
            let state = self.state.lock().await;
            state
                .actors
                .values()
                .filter(|handle| !handle.is_closed())
                .filter_map(|handle| {
                    let project_id = handle.view().summary.project_id?;
                    (!state.blocked_projects.contains(&project_id)).then_some(project_id)
                })
                .collect::<Vec<_>>()
        };
        for project in &mut projects {
            let live = live_project_ids
                .iter()
                .filter(|project_id| *project_id == &project.id)
                .count()
                .min(u32::MAX as usize) as u32;
            project.live_session_count = live;
            project.session_count = project.session_count.max(live);
        }
        let catalog = ProjectCatalog {
            protocol: PROTOCOL_VERSION,
            host: self.host.descriptor(),
            catalog_cursor: self.catalog_cursor(),
            lifecycle_mutations_supported: self.host.project_lifecycle_mutations_supported(),
            import_supported: self.host.project_import_supported(),
            projects,
        };
        catalog
            .validate()
            .map_err(|_| SupervisorError::InvalidBootstrap)?;
        Ok(catalog)
    }

    /// Attachment policy advertised by the running host.
    pub fn attachment_policy(&self) -> Option<crate::AttachmentPolicy> {
        self.host.attachment_policy()
    }

    /// Ingests one authenticated, transport-bounded attachment.
    pub async fn ingest_attachment(
        &self,
        display_name: &str,
        media_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<crate::AttachmentRef, crate::AttachmentError> {
        self.host
            .ingest_attachment(display_name, media_type, bytes)
            .await
    }

    /// Reads one authenticated attachment without exposing its host path.
    pub async fn attachment_content(
        &self,
        handle: &str,
    ) -> Result<crate::StoredAttachment, crate::AttachmentError> {
        self.host.attachment_content(handle).await
    }

    /// Whether the concrete host supports bounded document ingestion.
    pub fn document_ingest_supported(&self) -> bool {
        self.host.document_ingest_supported()
    }

    /// Ingests one document after the host verifies its session/project binding.
    pub async fn ingest_document(
        &self,
        session_id: &crate::SessionId,
        display_name: &str,
        media_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<crate::DocumentReference, crate::ServiceError> {
        let gate = self.session_gate(session_id).await;
        let _operation = gate.read().await;
        self.ensure_session_unblocked(session_id).await?;
        self.host
            .ingest_document(session_id, display_name, media_type, bytes)
            .await
    }

    /// Lists immutable documents owned by one session.
    pub async fn list_documents(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Vec<crate::DocumentReference>, crate::ServiceError> {
        let gate = self.session_gate(session_id).await;
        let _operation = gate.read().await;
        self.ensure_session_unblocked(session_id).await?;
        self.host.list_documents(session_id).await
    }

    /// Whether the concrete host supports root-confined project-file browsing.
    pub fn trusted_project_files_supported(&self) -> bool {
        self.host.trusted_project_files_supported()
    }

    /// Returns the trusted-file index status for one project.
    pub async fn trusted_file_index(
        &self,
        project_id: &crate::ProjectId,
    ) -> Result<crate::TrustedFileIndexSummary, crate::ServiceError> {
        self.host.trusted_file_index(project_id).await
    }

    /// Lists safe trusted-project file entries.
    pub async fn list_trusted_files(
        &self,
        project_id: &crate::ProjectId,
        limit: usize,
    ) -> Result<Vec<crate::TrustedFileEntry>, crate::ServiceError> {
        self.host.list_trusted_files(project_id, limit).await
    }

    /// Searches safe trusted-project file names and text.
    pub async fn search_trusted_files(
        &self,
        project_id: &crate::ProjectId,
        query: &str,
        limit: usize,
    ) -> Result<crate::TrustedFileSearchResult, crate::ServiceError> {
        self.host
            .search_trusted_files(project_id, query, limit)
            .await
    }

    /// Reads one trusted-project file by its opaque index identity.
    pub async fn read_trusted_file(
        &self,
        project_id: &crate::ProjectId,
        entry_id: &crate::FileEntryId,
    ) -> Result<crate::TrustedFileRead, crate::ServiceError> {
        self.host.read_trusted_file(project_id, entry_id).await
    }

    /// Whether root-confined project file-tree, text-read, and search routes are available.
    pub fn project_file_browser_supported(&self) -> bool {
        self.host.project_file_browser_supported()
    }

    /// Whether full-file replacement is available through the project file browser.
    pub fn project_file_write_supported(&self) -> bool {
        self.host.project_file_write_supported()
    }

    /// Lists one validated project-relative directory.
    pub async fn project_file_tree(
        &self,
        project_id: &crate::ProjectId,
        path: &str,
    ) -> Result<crate::ProjectFileTree, crate::ProjectFileSystemError> {
        self.host.project_file_tree(project_id, path).await
    }

    /// Reads bounded text from one validated project-relative file.
    pub async fn read_project_file(
        &self,
        project_id: &crate::ProjectId,
        path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Result<crate::ProjectFileRead, crate::ProjectFileSystemError> {
        self.host
            .read_project_file(project_id, path, start_line, end_line)
            .await
    }

    /// Searches bounded project text and relative paths.
    pub async fn search_project_files(
        &self,
        project_id: &crate::ProjectId,
        query: &str,
    ) -> Result<crate::ProjectFileSearchResult, crate::ProjectFileSystemError> {
        self.host.search_project_files(project_id, query).await
    }

    /// Replaces a complete project file after an optimistic version check.
    pub async fn write_project_file(
        &self,
        project_id: &crate::ProjectId,
        path: &str,
        content: &str,
        expected_sha256: &str,
        force: bool,
    ) -> Result<crate::ProjectFileWrite, crate::ProjectFileSystemError> {
        self.host
            .write_project_file(project_id, path, content, expected_sha256, force)
            .await
    }

    /// Whether the host exposes authenticated durable transcript search.
    pub fn transcript_search_supported(&self) -> bool {
        self.host.transcript_search_supported()
    }

    /// Searches already-redacted durable transcript projections.
    pub async fn search_transcripts(
        &self,
        request: &crate::TranscriptSearchRequest,
    ) -> Result<crate::TranscriptSearchResult, crate::ServiceError> {
        self.host.search_transcripts(request).await
    }

    /// Whether the host exposes trusted repository and folder-instruction context.
    pub fn repository_context_supported(&self) -> bool {
        self.host.repository_context_supported()
    }

    /// Refreshes path-free repository context for one authoritative project.
    pub async fn repository_context(
        &self,
        project_id: &crate::ProjectId,
    ) -> Result<crate::RepositoryContextSnapshot, crate::ServiceError> {
        self.host.repository_context(project_id).await
    }

    /// Reads one authenticated opaque resource without interpreting its handle.
    pub async fn resource_content(
        &self,
        session_id: &crate::SessionId,
        handle: &str,
    ) -> Result<crate::StoredResource, crate::ServiceError> {
        let gate = self.session_gate(session_id).await;
        let _operation = gate.read().await;
        self.ensure_session_unblocked(session_id).await?;
        self.host.resource_content(session_id, handle).await
    }

    /// Produces one authenticated, redacted portable session download.
    pub async fn session_export(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<bytes::Bytes, crate::ServiceError> {
        let gate = self.session_gate(session_id).await;
        let _operation = gate.read().await;
        self.ensure_session_unblocked(session_id).await?;
        self.host.session_export(session_id).await
    }

    /// Returns current daily or trailing-seven-day inference usage totals.
    pub async fn usage_stats(
        &self,
        period: crate::UsagePeriod,
    ) -> Result<crate::UsageStats, crate::ServiceError> {
        self.host.usage_stats(period).await
    }

    /// Returns all retained inference usage totals.
    pub async fn usage_lifetime(&self) -> Result<crate::LifetimeUsage, crate::ServiceError> {
        self.host.usage_lifetime().await
    }

    /// Returns recent daily inference activity and lifetime streaks.
    pub async fn usage_activity(&self) -> Result<crate::UsageActivity, crate::ServiceError> {
        self.host.usage_activity().await
    }

    /// Returns the sessions that currently have a live graphical owner.
    ///
    /// Host integrations use this snapshot to avoid duplicating background work
    /// already owned by a session driver. The result is advisory; ownership is
    /// rechecked before an integration-only catalog refresh is published.
    pub async fn hosted_session_ids(&self) -> BTreeSet<SessionId> {
        self.state
            .lock()
            .await
            .actors
            .iter()
            .filter_map(|(session_id, handle)| (!handle.is_closed()).then_some(session_id.clone()))
            .collect()
    }

    /// Publishes a host-owned summary refresh without creating a session actor.
    ///
    /// Returns `false` when a session/project lifecycle fence, an in-flight
    /// open, or a live/retiring actor requires the caller to retry. A live actor
    /// remains authoritative and consumes the refresh through its own driver.
    pub async fn publish_inactive_catalog_summary(
        &self,
        summary: SessionSummary,
    ) -> Result<bool, ServiceError> {
        summary.validate().map_err(|_| ServiceError::InvalidSeed)?;
        let state = self.state.lock().await;
        if state.blocked_sessions.contains(&summary.id)
            || state.session_openings.contains_key(&summary.id)
            || summary
                .project_id
                .as_ref()
                .is_some_and(|project_id| state.blocked_projects.contains(project_id))
        {
            return Ok(false);
        }
        if state.actors.contains_key(&summary.id) {
            return Ok(false);
        }
        // Publish while ownership is fenced by the registry lock. Otherwise an
        // actor could install and publish a newer view between this check and
        // the catalog send, only to be overwritten by this inactive summary.
        self.publish_catalog(summary);
        Ok(true)
    }

    /// Subscribes to the ordered live stream across all hosted sessions.
    ///
    /// A lagged subscriber must recover each affected session through replay
    /// or a fresh bootstrap snapshot.
    pub fn subscribe_events(&self) -> broadcast::Receiver<HostStreamEvent> {
        self.host_events.subscribe()
    }

    async fn create_owned_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionActorHandle, SupervisorError> {
        // Project lifecycle mutations take the write side of this gate, so a
        // creation cannot persist a session after trust/archive fencing starts.
        let project_gate = if let Some(project_id) = request.project_id.as_ref() {
            Some(self.project_gate(project_id).await)
        } else {
            None
        };
        let _project_operation = if let Some(gate) = &project_gate {
            Some(gate.clone().read_owned().await)
        } else {
            None
        };
        if let Some(project_id) = request.project_id.as_ref() {
            if self
                .state
                .lock()
                .await
                .blocked_projects
                .contains(project_id)
            {
                return Err(SupervisorError::Service(ServiceError::Unauthorized));
            }
        }

        // The potentially slow factory is deliberately outside the actor-map
        // lock. Duplicate IDs are resolved before an actor task is spawned.
        let driver = self.host.create_session(request).await?;
        let session_id = driver.seed().summary.id;
        let mut state = self.state.lock().await;
        if state.blocked_sessions.contains(&session_id) {
            return Err(SupervisorError::Service(ServiceError::Unavailable));
        }
        if driver
            .seed()
            .summary
            .project_id
            .as_ref()
            .is_some_and(|project_id| state.blocked_projects.contains(project_id))
        {
            return Err(SupervisorError::Service(ServiceError::Unauthorized));
        }
        let provisional_owners = state
            .actors
            .values()
            .filter(|handle| handle.view().summary.provisional)
            .count();
        if state.actors.len() >= MAX_ACTIVE_SESSION_OWNERS
            || provisional_owners >= MAX_FRESH_PROVISIONAL_OWNERS
        {
            return Err(SupervisorError::Service(ServiceError::Unavailable));
        }
        if state.actors.contains_key(&session_id)
            || state.session_openings.contains_key(&session_id)
        {
            return Err(SupervisorError::DuplicateOwner);
        }
        let handle = self.spawn_driver(driver)?;
        if handle.session_id() != &session_id {
            return Err(SupervisorError::IdentityMismatch);
        }
        let events = handle.subscribe_events();
        let mut views = handle.subscribe();
        let summary = views.borrow_and_update().summary.clone();
        state.actors.insert(session_id, handle.clone());
        // Publish the receiver's baseline before its observer can forward a
        // newer actor view; this prevents both gaps and baseline regression.
        self.publish_catalog(summary);
        self.observe_actor(&handle, views, events);
        drop(state);
        Ok(handle)
    }

    async fn open_session_factory(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionActorHandle, SupervisorError> {
        // Driver construction may perform filesystem/provider work. It stays
        // outside the registry lock so unrelated sessions can open in
        // parallel. The reservation ensures exactly one factory runs for this
        // session ID.
        let driver = self.host.open_session(session_id).await?;
        if driver.seed().summary.id != *session_id {
            return Err(SupervisorError::IdentityMismatch);
        }

        let mut state = self.state.lock().await;
        if state.blocked_sessions.contains(session_id) {
            return Err(SupervisorError::Service(ServiceError::Unavailable));
        }
        if driver
            .seed()
            .summary
            .project_id
            .as_ref()
            .is_some_and(|project_id| state.blocked_projects.contains(project_id))
        {
            return Err(SupervisorError::Service(ServiceError::Unauthorized));
        }
        if let Some(existing) = state.actors.get(session_id) {
            if existing.is_closed() {
                return Err(SupervisorError::Service(ServiceError::Unavailable));
            }
            return Ok(existing.clone());
        }
        if state.actors.len() >= MAX_ACTIVE_SESSION_OWNERS {
            return Err(SupervisorError::Service(ServiceError::Unavailable));
        }
        let handle = self.spawn_driver(driver)?;
        if handle.session_id() != session_id {
            return Err(SupervisorError::IdentityMismatch);
        }
        let events = handle.subscribe_events();
        let mut views = handle.subscribe();
        let summary = views.borrow_and_update().summary.clone();
        state.actors.insert(session_id.clone(), handle.clone());
        // Keep the subscribed baseline ahead of all observed replacements.
        self.publish_catalog(summary);
        self.observe_actor(&handle, views, events);
        drop(state);
        Ok(handle)
    }

    async fn complete_session_open(
        &self,
        session_id: SessionId,
        result: Result<SessionActorHandle, SupervisorError>,
    ) {
        let waiters = self
            .state
            .lock()
            .await
            .session_openings
            .remove(&session_id)
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    async fn execute_host_command(
        &self,
        envelope: &HostCommandEnvelope,
        acknowledged_at_ms: u64,
    ) -> HostCommandAck {
        let host_id = self.host.descriptor().id;
        let reject = |error: SanitizedError, cursor: CatalogCursor| {
            HostCommandAck::rejected(
                host_id.clone(),
                envelope.command_id.clone(),
                acknowledged_at_ms,
                cursor,
                error,
            )
        };
        if let Err(error) = envelope.validate() {
            return reject(error.into(), self.catalog_cursor());
        }
        if envelope.host_id != host_id {
            return reject(
                SanitizedError::public(
                    ErrorCode::InvalidCommand,
                    "The command target does not match this host.",
                ),
                self.catalog_cursor(),
            );
        }

        match &envelope.command {
            HostCommand::CreateSession {
                project_id,
                authority,
                model,
            } => {
                if authority_rank(*authority) > authority_rank(self.host.authority_ceiling())
                    || !self.host.authority_profiles().contains(authority)
                {
                    return reject(
                        SanitizedError::public(
                            ErrorCode::Unauthorized,
                            "The requested authority is not permitted by this host.",
                        ),
                        self.catalog_cursor(),
                    );
                }
                if let Some(selection) = model {
                    if !model_is_selectable(selection, &self.host.model_catalog()) {
                        return reject(
                            SanitizedError::public(
                                ErrorCode::InvalidCommand,
                                "The requested model or reasoning choice is not available.",
                            ),
                            self.catalog_cursor(),
                        );
                    }
                }
                match self
                    .create_owned_session(CreateSessionRequest {
                        project_id: project_id.clone(),
                        provisional: true,
                        authority: *authority,
                        model: model.clone(),
                    })
                    .await
                {
                    Ok(handle) => HostCommandAck::accepted(
                        host_id,
                        envelope.command_id.clone(),
                        acknowledged_at_ms,
                        self.catalog_cursor(),
                        handle.session_id().clone(),
                    ),
                    Err(error) => reject(supervisor_error_to_public(error), self.catalog_cursor()),
                }
            }
            HostCommand::ImportProject {
                candidate_id,
                display_name,
            } => match self
                .host
                .import_project(candidate_id, display_name.as_deref())
                .await
            {
                Ok(project) => HostCommandAck::accepted_project(
                    host_id,
                    envelope.command_id.clone(),
                    acknowledged_at_ms,
                    self.advance_project_catalog(),
                    Some(project),
                ),
                Err(error) => reject(error.into_public(), self.catalog_cursor()),
            },
            HostCommand::RenameProject {
                project_id,
                display_name,
            } => match self.host.rename_project(project_id, display_name).await {
                Ok(project) => HostCommandAck::accepted_project(
                    host_id,
                    envelope.command_id.clone(),
                    acknowledged_at_ms,
                    self.advance_project_catalog(),
                    Some(project),
                ),
                Err(error) => reject(error.into_public(), self.catalog_cursor()),
            },
            HostCommand::SetDefaultProject { project_id } => {
                match self.host.set_default_project(project_id).await {
                    Ok(project) => HostCommandAck::accepted_project(
                        host_id,
                        envelope.command_id.clone(),
                        acknowledged_at_ms,
                        self.advance_project_catalog(),
                        Some(project),
                    ),
                    Err(error) => reject(error.into_public(), self.catalog_cursor()),
                }
            }
            HostCommand::ClearDefaultProject => match self.host.clear_default_project().await {
                Ok(()) => HostCommandAck::accepted_project(
                    host_id,
                    envelope.command_id.clone(),
                    acknowledged_at_ms,
                    self.advance_project_catalog(),
                    None,
                ),
                Err(error) => reject(error.into_public(), self.catalog_cursor()),
            },
            HostCommand::SetProjectTrust {
                project_id,
                trusted,
            } => {
                let gate = self.project_gate(project_id).await;
                let _mutation = gate.write().await;
                if !trusted {
                    self.block_and_retire_project(project_id).await;
                }
                match self.host.set_project_trust(project_id, *trusted).await {
                    Ok(project) => {
                        if *trusted {
                            self.unblock_project(project_id).await;
                        }
                        HostCommandAck::accepted_project(
                            host_id,
                            envelope.command_id.clone(),
                            acknowledged_at_ms,
                            self.advance_project_catalog(),
                            Some(project),
                        )
                    }
                    Err(error) => {
                        if !trusted {
                            self.unblock_project(project_id).await;
                        }
                        reject(error.into_public(), self.catalog_cursor())
                    }
                }
            }
            HostCommand::ArchiveProject { project_id } => {
                let gate = self.project_gate(project_id).await;
                let _mutation = gate.write().await;
                self.block_and_retire_project(project_id).await;
                match self.host.archive_project(project_id).await {
                    Ok(project) => HostCommandAck::accepted_project(
                        host_id,
                        envelope.command_id.clone(),
                        acknowledged_at_ms,
                        self.advance_project_catalog(),
                        Some(project),
                    ),
                    Err(error) => {
                        self.unblock_project(project_id).await;
                        reject(error.into_public(), self.catalog_cursor())
                    }
                }
            }
            HostCommand::SetSessionLifecycle {
                session_id,
                lifecycle,
            } => {
                let gate = self.session_gate(session_id).await;
                let _mutation = gate.write().await;
                self.block_and_retire_session(session_id).await;
                let result = self
                    .host
                    .set_session_lifecycle(session_id, *lifecycle, acknowledged_at_ms)
                    .await;
                self.unblock_session(session_id).await;
                match result {
                    Ok(_) => HostCommandAck::accepted_project(
                        host_id,
                        envelope.command_id.clone(),
                        acknowledged_at_ms,
                        self.advance_project_catalog(),
                        None,
                    ),
                    Err(error) => reject(error.into_public(), self.catalog_cursor()),
                }
            }
            HostCommand::DeleteSessionPermanently {
                session_id,
                confirmation,
            } => {
                let gate = self.session_gate(session_id).await;
                let _mutation = gate.write().await;
                self.block_and_retire_session(session_id).await;
                let result = self
                    .host
                    .delete_session_permanently(session_id, confirmation)
                    .await;
                self.unblock_session(session_id).await;
                match result {
                    Ok(()) => HostCommandAck::accepted_project(
                        host_id,
                        envelope.command_id.clone(),
                        acknowledged_at_ms,
                        self.advance_project_catalog(),
                        None,
                    ),
                    Err(error) => reject(error.into_public(), self.catalog_cursor()),
                }
            }
        }
    }

    fn advance_project_catalog(&self) -> CatalogCursor {
        let Ok(_order) = self.host_event_order.lock() else {
            return self.catalog_cursor();
        };
        advance_catalog(&self.catalog_cursor).unwrap_or_else(|| self.catalog_cursor())
    }

    async fn wait_for_actor_registry_release(&self, actor: &SessionActorHandle) {
        loop {
            let retained = self
                .state
                .lock()
                .await
                .actors
                .get(actor.session_id())
                .is_some_and(|current| current.same_actor(actor));
            if !retained {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn block_and_retire_project(&self, project_id: &ProjectId) {
        let actors = {
            let mut state = self.state.lock().await;
            state.blocked_projects.insert(project_id.clone());
            state
                .actors
                .values()
                .filter(|handle| handle.view().summary.project_id.as_ref() == Some(project_id))
                .cloned()
                .collect::<Vec<_>>()
        };
        for actor in &actors {
            actor.retire().await;
        }
        for actor in actors {
            actor.closed().await;
            actor.quiesced().await;
            self.wait_for_actor_registry_release(&actor).await;
        }
    }

    async fn unblock_project(&self, project_id: &ProjectId) {
        self.state.lock().await.blocked_projects.remove(project_id);
    }

    async fn project_gate(&self, project_id: &ProjectId) -> Arc<RwLock<()>> {
        let mut state = self.state.lock().await;
        state
            .project_gates
            .retain(|_, candidate| candidate.strong_count() > 0);
        if let Some(gate) = state.project_gates.get(project_id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(RwLock::new(()));
        state
            .project_gates
            .insert(project_id.clone(), Arc::downgrade(&gate));
        gate
    }

    async fn session_gate(&self, session_id: &SessionId) -> Arc<RwLock<()>> {
        let mut state = self.state.lock().await;
        state
            .session_gates
            .retain(|_, candidate| candidate.strong_count() > 0);
        if let Some(gate) = state.session_gates.get(session_id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(RwLock::new(()));
        state
            .session_gates
            .insert(session_id.clone(), Arc::downgrade(&gate));
        gate
    }

    async fn ensure_session_unblocked(&self, session_id: &SessionId) -> Result<(), ServiceError> {
        if self
            .state
            .lock()
            .await
            .blocked_sessions
            .contains(session_id)
        {
            Err(ServiceError::Unavailable)
        } else {
            Ok(())
        }
    }

    async fn block_and_retire_session(&self, session_id: &SessionId) {
        loop {
            let (opening, actor) = {
                let mut state = self.state.lock().await;
                state.blocked_sessions.insert(session_id.clone());
                if let Some(waiters) = state.session_openings.get_mut(session_id) {
                    let (sender, receiver) = oneshot::channel();
                    waiters.push(sender);
                    (Some(receiver), None)
                } else {
                    (None, state.actors.get(session_id).cloned())
                }
            };

            if let Some(opening) = opening {
                let _ = opening.await;
                continue;
            }
            let Some(actor) = actor else {
                return;
            };
            actor.retire().await;
            actor.closed().await;
            actor.quiesced().await;
            self.wait_for_actor_registry_release(&actor).await;
        }
    }

    async fn unblock_session(&self, session_id: &SessionId) {
        self.state.lock().await.blocked_sessions.remove(session_id);
    }

    async fn complete_host_command(
        &self,
        key: HostCommandKey,
        envelope: HostCommandEnvelope,
        ack: HostCommandAck,
    ) {
        let waiters = {
            let mut state = self.state.lock().await;
            let waiters = match state.host_commands.remove(&key) {
                Some(HostCommandCacheEntry::InFlight { waiters, .. }) => waiters,
                Some(HostCommandCacheEntry::Complete { .. }) | None => Vec::new(),
            };
            while state.host_command_order.len() >= HOST_COMMAND_CACHE_CAPACITY {
                if let Some(oldest) = state.host_command_order.pop_front() {
                    state.host_commands.remove(&oldest);
                }
            }
            state.host_command_order.push_back(key.clone());
            state.host_commands.insert(
                key,
                HostCommandCacheEntry::Complete {
                    envelope,
                    ack: ack.clone(),
                },
            );
            waiters
        };
        for waiter in waiters {
            let _ = waiter.send(ack.clone());
        }
    }

    fn spawn_driver(&self, driver: H::Driver) -> Result<SessionActorHandle, SupervisorError> {
        Ok(SessionActor::spawn(
            self.host.descriptor().id,
            driver,
            self.config.actor,
        )?)
    }

    fn observe_actor(
        &self,
        handle: &SessionActorHandle,
        mut views: watch::Receiver<Arc<ActorView>>,
        mut events: broadcast::Receiver<crate::EventEnvelope>,
    ) {
        let cursor = Arc::clone(&self.catalog_cursor);
        let sender = self.host_events.clone();
        let order = Arc::clone(&self.host_event_order);
        let catalog_observer = tokio::spawn(async move {
            while views.changed().await.is_ok() {
                let summary = views.borrow_and_update().summary.clone();
                if !send_ordered_catalog_event(&sender, &order, &cursor, summary) {
                    break;
                }
            }
        });

        let event_owner = handle.clone();
        let sender = self.host_events.clone();
        let order = Arc::clone(&self.host_event_order);
        let event_observer = tokio::spawn(async move {
            forward_actor_events_until_quiesced(&mut events, &sender, &order, &event_owner).await;
        });

        let observed = handle.clone();
        let session_id = handle.session_id().clone();
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            observed.closed().await;
            observed.quiesced().await;
            // Keep ownership fenced until the actor's final view and session
            // events have been forwarded; an inactive refresh must never
            // overtake either observer and then be regressed by delayed work.
            let _ = catalog_observer.await;
            let _ = event_observer.await;
            let mut state = state.lock().await;
            let remove = state.actors.get(&session_id).is_some_and(|current| {
                current.same_actor(&observed) && current.is_closed() && current.is_quiesced()
            });
            if remove {
                state.actors.remove(&session_id);
            }
        });
    }

    fn publish_catalog(&self, summary: crate::SessionSummary) {
        send_ordered_catalog_event(
            &self.host_events,
            &self.host_event_order,
            &self.catalog_cursor,
            summary,
        );
    }
}

fn advance_catalog(cursor: &AtomicU64) -> Option<CatalogCursor> {
    cursor
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .ok()
        .and_then(|previous| previous.checked_add(1))
        .map(CatalogCursor)
}

fn send_ordered_catalog_event(
    sender: &broadcast::Sender<HostStreamEvent>,
    order: &std::sync::Mutex<u64>,
    cursor: &AtomicU64,
    summary: crate::SessionSummary,
) -> bool {
    let Ok(mut sequence) = order.lock() else {
        return false;
    };
    let Some(next) = sequence.checked_add(1) else {
        return false;
    };
    let Some(catalog_cursor) = advance_catalog(cursor) else {
        return false;
    };
    *sequence = next;
    // Assign both revisions and broadcast while the order lock is held. A
    // concurrent producer therefore cannot enqueue host sequence or catalog
    // revision N + 1 before N.
    let _ = sender.send(HostStreamEvent::catalog(next, catalog_cursor, summary));
    true
}

fn send_ordered_actor_event(
    sender: &broadcast::Sender<HostStreamEvent>,
    order: &std::sync::Mutex<u64>,
    event: crate::EventEnvelope,
) -> bool {
    let mut sequence = order.lock().expect("host event order poisoned");
    let Some(next) = sequence.checked_add(1) else {
        return false;
    };
    *sequence = next;
    let _ = sender.send(HostStreamEvent::new(next, event));
    true
}

#[cfg(test)]
async fn forward_actor_events(
    events: &mut broadcast::Receiver<crate::EventEnvelope>,
    sender: &broadcast::Sender<HostStreamEvent>,
    order: &std::sync::Mutex<u64>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if !send_ordered_actor_event(sender, order, event) {
                    break;
                }
            }
            // Do not synthesize continuity. The next retained event keeps its
            // original per-session cursor, exposing the gap so clients can
            // recover with replay or a snapshot.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn forward_actor_events_until_quiesced(
    events: &mut broadcast::Receiver<crate::EventEnvelope>,
    sender: &broadcast::Sender<HostStreamEvent>,
    order: &std::sync::Mutex<u64>,
    owner: &SessionActorHandle,
) {
    loop {
        tokio::select! {
            _ = owner.quiesced() => {
                // Quiescence is signaled only after the actor has emitted its
                // final event, so the receiver can now be drained without
                // waiting for handle-owned broadcast senders to be dropped.
                loop {
                    match events.try_recv() {
                        Ok(event) => {
                            if !send_ordered_actor_event(sender, order, event) {
                                return;
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                        Err(
                            broadcast::error::TryRecvError::Empty
                            | broadcast::error::TryRecvError::Closed,
                        ) => return,
                    }
                }
            }
            result = events.recv() => match result {
                Ok(event) => {
                    if !send_ordered_actor_event(sender, order, event) {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    }
}

fn clamp_authority(requested: AuthorityProfile, ceiling: AuthorityProfile) -> AuthorityProfile {
    if authority_rank(requested) <= authority_rank(ceiling) {
        requested
    } else {
        ceiling
    }
}

fn model_is_selectable(selection: &ModelSelection, catalog: &[crate::ModelSummary]) -> bool {
    catalog.iter().any(|model| {
        model.available
            && model.provider == selection.provider
            && model.id == selection.model
            && model
                .reasoning
                .iter()
                .any(|option| option == &selection.reasoning)
    })
}

fn supervisor_error_to_public(error: SupervisorError) -> SanitizedError {
    match error {
        SupervisorError::Service(error) => error.into_public(),
        SupervisorError::Actor(error) => error.into(),
        SupervisorError::DuplicateOwner => SanitizedError::public(
            ErrorCode::Unavailable,
            "The session is already owned by another graphical actor.",
        )
        .with_retryable(true),
        SupervisorError::IdentityMismatch | SupervisorError::InvalidBootstrap => {
            SanitizedError::internal()
        }
    }
}

fn authority_rank(authority: AuthorityProfile) -> u8 {
    match authority {
        AuthorityProfile::ReadOnly => 0,
        AuthorityProfile::Workspace => 1,
        AuthorityProfile::FullAccess => 2,
    }
}

fn insert_active_summary(
    summaries: &mut BTreeMap<SessionId, SessionSummary>,
    mut active: SessionSummary,
) {
    if let Some(catalog) = summaries.get(&active.id) {
        // The host catalog owns durable external evidence. Persistence can
        // advance before a backpressured actor event updates its live view, so
        // only overlay actor-owned live fields onto that PR projection.
        active.pull_request.clone_from(&catalog.pull_request);
    }
    summaries.insert(active.id.clone(), active);
}

fn bounded_bootstrap_sessions(
    summaries: BTreeMap<SessionId, SessionSummary>,
    selected_session_id: Option<&SessionId>,
) -> Vec<SessionSummary> {
    let mut sessions = summaries.into_values().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        left.archived
            .cmp(&right.archived)
            .then_with(|| right.pinned.cmp(&left.pinned))
            .then_with(|| {
                session_activity_rank(right.live_state).cmp(&session_activity_rank(left.live_state))
            })
            .then_with(|| right.modified_at_ms.cmp(&left.modified_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    if sessions.len() > MAX_BOOTSTRAP_SESSION_SUMMARIES {
        if let Some(selected_session_id) = selected_session_id {
            if let Some(selected_index) = sessions
                .iter()
                .position(|summary| &summary.id == selected_session_id)
            {
                if selected_index >= MAX_BOOTSTRAP_SESSION_SUMMARIES {
                    sessions.swap(selected_index, MAX_BOOTSTRAP_SESSION_SUMMARIES - 1);
                }
            }
        }
        sessions.truncate(MAX_BOOTSTRAP_SESSION_SUMMARIES);
    }
    sessions
}

fn session_activity_rank(state: crate::SessionLiveState) -> u8 {
    use crate::SessionLiveState;

    match state {
        SessionLiveState::NeedsApproval | SessionLiveState::NeedsInput => 5,
        SessionLiveState::Working => 4,
        SessionLiveState::Failed => 3,
        SessionLiveState::Done => 2,
        SessionLiveState::Idle | SessionLiveState::Stopped => 1,
        SessionLiveState::Offline | SessionLiveState::Locked => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use crate::{
        AckDisposition, ActorOwnerState, AttentionState, ColorScheme, CommandId, ContextUsage,
        DeviceId, DriverCommandOutcome, DurableEntryId, EventPayload, HostCapabilities,
        HostCommand, HostCommandEnvelope, HostDescriptor, HostId, InputModality, ItemId,
        ItemLifecycle, ItemPayload, ModelSelection, ModelSummary, PromptInput, RunId,
        SessionCommand, SessionItem, SessionLiveState, SessionSeed, ThemeDensity, ThemeDto,
        ThemeId, ThemeMotion, ThemeOption, ThemeSourceClass, ThemeTypography, TimestampedEvent,
    };

    use super::*;

    #[derive(Clone)]
    struct MockHost {
        next_session: Arc<AtomicUsize>,
        seeds: Arc<StdMutex<BTreeMap<SessionId, SessionSeed>>>,
        dispatches: Arc<StdMutex<BTreeMap<SessionId, usize>>>,
        opens: Arc<AtomicUsize>,
        fail_next_finalization: Arc<AtomicBool>,
        never_quiesce: Arc<AtomicBool>,
        create_barrier: Option<Arc<tokio::sync::Barrier>>,
        create_entered: Option<Arc<tokio::sync::Barrier>>,
        create_release: Option<Arc<tokio::sync::Barrier>>,
        open_gate_armed: Arc<AtomicBool>,
        open_entered: Option<Arc<tokio::sync::Barrier>>,
        open_release: Option<Arc<tokio::sync::Barrier>>,
        list_projects_entered: Option<Arc<tokio::sync::Barrier>>,
        list_projects_release: Option<Arc<tokio::sync::Barrier>>,
        list_sessions_entered: Option<Arc<tokio::sync::Barrier>>,
        list_sessions_release: Option<Arc<tokio::sync::Barrier>>,
        project: Arc<StdMutex<Option<crate::ProjectSummary>>>,
    }

    impl MockHost {
        fn new() -> Self {
            Self {
                next_session: Arc::new(AtomicUsize::new(1)),
                seeds: Arc::new(StdMutex::new(BTreeMap::new())),
                dispatches: Arc::new(StdMutex::new(BTreeMap::new())),
                opens: Arc::new(AtomicUsize::new(0)),
                fail_next_finalization: Arc::new(AtomicBool::new(false)),
                never_quiesce: Arc::new(AtomicBool::new(false)),
                create_barrier: None,
                create_entered: None,
                create_release: None,
                open_gate_armed: Arc::new(AtomicBool::new(false)),
                open_entered: None,
                open_release: None,
                list_projects_entered: None,
                list_projects_release: None,
                list_sessions_entered: None,
                list_sessions_release: None,
                project: Arc::new(StdMutex::new(None)),
            }
        }

        fn with_project() -> (Self, ProjectId) {
            let project_id = ProjectId::new("prj_11111111111111111111111111111111").unwrap();
            let host = Self {
                project: Arc::new(StdMutex::new(Some(crate::ProjectSummary {
                    id: project_id.clone(),
                    name: "Project".into(),
                    trusted: true,
                    archived: false,
                    available: true,
                    is_default: true,
                    session_count: 0,
                    live_session_count: 0,
                }))),
                ..Self::new()
            };
            (host, project_id)
        }

        fn with_create_barrier(parties: usize) -> Self {
            Self {
                create_barrier: Some(Arc::new(tokio::sync::Barrier::new(parties))),
                ..Self::new()
            }
        }

        fn with_gated_create() -> (Self, Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>) {
            let entered = Arc::new(tokio::sync::Barrier::new(2));
            let release = Arc::new(tokio::sync::Barrier::new(2));
            (
                Self {
                    create_entered: Some(Arc::clone(&entered)),
                    create_release: Some(Arc::clone(&release)),
                    ..Self::new()
                },
                entered,
                release,
            )
        }

        fn with_gated_open() -> (Self, Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>) {
            let entered = Arc::new(tokio::sync::Barrier::new(2));
            let release = Arc::new(tokio::sync::Barrier::new(2));
            (
                Self {
                    open_entered: Some(Arc::clone(&entered)),
                    open_release: Some(Arc::clone(&release)),
                    ..Self::new()
                },
                entered,
                release,
            )
        }

        fn with_gated_catalog() -> (
            Self,
            Arc<tokio::sync::Barrier>,
            Arc<tokio::sync::Barrier>,
            Arc<tokio::sync::Barrier>,
            Arc<tokio::sync::Barrier>,
        ) {
            let projects_entered = Arc::new(tokio::sync::Barrier::new(2));
            let projects_release = Arc::new(tokio::sync::Barrier::new(2));
            let sessions_entered = Arc::new(tokio::sync::Barrier::new(2));
            let sessions_release = Arc::new(tokio::sync::Barrier::new(2));
            (
                Self {
                    list_projects_entered: Some(Arc::clone(&projects_entered)),
                    list_projects_release: Some(Arc::clone(&projects_release)),
                    list_sessions_entered: Some(Arc::clone(&sessions_entered)),
                    list_sessions_release: Some(Arc::clone(&sessions_release)),
                    ..Self::new()
                },
                projects_entered,
                projects_release,
                sessions_entered,
                sessions_release,
            )
        }

        fn dispatch_count(&self, session_id: &SessionId) -> usize {
            *self
                .dispatches
                .lock()
                .unwrap()
                .get(session_id)
                .unwrap_or(&0)
        }

        fn make_seed(&self, request: &CreateSessionRequest) -> SessionSeed {
            let number = self.next_session.fetch_add(1, Ordering::Relaxed);
            let id = SessionId::new(format!("session-{number}")).unwrap();
            let model = request.model.clone().unwrap_or_else(|| ModelSelection {
                provider: "mock".into(),
                model: "mock-model".into(),
                reasoning: "off".into(),
            });
            SessionSeed {
                summary: crate::SessionSummary {
                    id: id.clone(),
                    project_id: request.project_id.clone(),
                    title: "Fresh session".into(),
                    tags: Vec::new(),
                    created_at_ms: number as u64,
                    modified_at_ms: number as u64,
                    pinned: false,
                    archived: false,
                    lifecycle: crate::SessionCatalogState::Active,
                    retention: None,
                    forked_from: None,
                    provisional: request.provisional,
                    live_state: SessionLiveState::Idle,
                    attention: AttentionState::None,
                    pull_request: None,
                    owner: ActorOwnerState::Hosted,
                    model: model.clone(),
                },
                snapshot: crate::SessionSnapshot {
                    session_id: id,
                    actor_generation: 1,
                    cursor: SessionCursor::zero(1),
                    durable_head: None,
                    branches: crate::SessionBranchGraph::default(),
                    live_state: SessionLiveState::Idle,
                    active_run_id: None,
                    model,
                    authority: request.authority,
                    context: ContextUsage::default(),
                    items: Vec::new(),
                    pending_requests: Vec::new(),
                    sources: Vec::new(),
                    artifacts: Vec::new(),
                },
            }
        }
    }

    struct MockDriver {
        seed: SessionSeed,
        dispatches: Arc<StdMutex<BTreeMap<SessionId, usize>>>,
        fail_next_finalization: Arc<AtomicBool>,
        never_quiesce: Arc<AtomicBool>,
    }

    #[async_trait]
    impl crate::SessionDriver for MockDriver {
        fn seed(&self) -> SessionSeed {
            self.seed.clone()
        }

        async fn dispatch(
            &mut self,
            command: SessionCommand,
        ) -> Result<DriverCommandOutcome, ServiceError> {
            if self.fail_next_finalization.swap(false, Ordering::AcqRel) {
                let (outcome, mut finalizer) =
                    DriverCommandOutcome::guarded_replace(self.seed.clone());
                tokio::spawn(async move {
                    if finalizer.decision().await.is_ok() {
                        let _ = finalizer.complete(Err(ServiceError::Internal));
                    }
                });
                return Ok(outcome);
            }
            let input = match command {
                SessionCommand::SubmitPrompt { input } => input,
                SessionCommand::Rename { title } => {
                    return Ok(DriverCommandOutcome::with_events(vec![
                        TimestampedEvent::new(
                            1,
                            EventPayload::SessionMetadataChanged {
                                title: Some(title),
                                pinned: None,
                                archived: None,
                            },
                        ),
                    ]));
                }
                SessionCommand::SetPinned { pinned } => {
                    return Ok(DriverCommandOutcome::with_events(vec![
                        TimestampedEvent::new(
                            1,
                            EventPayload::SessionMetadataChanged {
                                title: None,
                                pinned: Some(pinned),
                                archived: None,
                            },
                        ),
                    ]));
                }
                SessionCommand::SetArchived { archived } => {
                    return Ok(DriverCommandOutcome::with_events(vec![
                        TimestampedEvent::new(
                            1,
                            EventPayload::SessionMetadataChanged {
                                title: None,
                                pinned: None,
                                archived: Some(archived),
                            },
                        ),
                    ]));
                }
                _ => return Ok(DriverCommandOutcome::default()),
            };
            let count = {
                let mut counts = self.dispatches.lock().unwrap();
                let count = counts.entry(self.seed.summary.id.clone()).or_default();
                *count += 1;
                *count
            };
            let run_id =
                RunId::new(format!("run-{}-{count}", self.seed.summary.id.as_str())).unwrap();
            let item_id =
                ItemId::new(format!("item-{}-{count}", self.seed.summary.id.as_str())).unwrap();
            let committed = SessionItem {
                id: item_id,
                run_id: Some(run_id.clone()),
                turn_id: None,
                provider_attempt: None,
                lifecycle: ItemLifecycle::Committed,
                durable_entry_id: Some(
                    DurableEntryId::new(format!("entry-{}-{count}", self.seed.summary.id.as_str()))
                        .unwrap(),
                ),
                payload: ItemPayload::UserMessage {
                    text: input.text,
                    attachments: input.attachments,
                    documents: Vec::new(),
                    project_files: Vec::new(),
                    delivery: None,
                    branch_provenance: None,
                },
            };
            Ok(DriverCommandOutcome::run(
                run_id.clone(),
                vec![
                    TimestampedEvent::new(
                        count as u64 * 10,
                        EventPayload::SessionStateChanged {
                            state: SessionLiveState::Working,
                            active_run_id: Some(run_id),
                        },
                    ),
                    TimestampedEvent::new(
                        count as u64 * 10 + 1,
                        EventPayload::ItemCommitted { item: committed },
                    ),
                    TimestampedEvent::new(
                        count as u64 * 10 + 2,
                        EventPayload::SessionStateChanged {
                            state: SessionLiveState::Done,
                            active_run_id: None,
                        },
                    ),
                ],
            ))
        }

        async fn shutdown(&mut self) {
            if self.never_quiesce.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
        }
    }

    #[async_trait]
    impl HostService for MockHost {
        type Driver = MockDriver;

        fn descriptor(&self) -> HostDescriptor {
            HostDescriptor {
                id: HostId::new("host-mock").unwrap(),
                name: "Mock host".into(),
            }
        }

        fn capabilities(&self) -> HostCapabilities {
            HostCapabilities {
                attachments: true,
                previews: true,
                ..HostCapabilities::default()
            }
        }

        fn model_catalog(&self) -> Vec<ModelSummary> {
            vec![ModelSummary {
                id: "mock-model".into(),
                name: "Mock model".into(),
                provider: "mock".into(),
                local: true,
                available: true,
                reasoning: vec!["off".into(), "high".into()],
                default_reasoning: Some("off".into()),
                input_pricing: None,
                input_modalities: vec![InputModality::Text],
            }]
        }

        fn theme_catalog(&self) -> Vec<ThemeOption> {
            vec![ThemeOption {
                id: ThemeId::new("mock-dark").unwrap(),
                theme: ThemeDto {
                    name: "Mock Dark".into(),
                    source: ThemeSourceClass::Bundled,
                    revision: 1,
                    scheme: ColorScheme::Dark,
                    density: ThemeDensity::Comfortable,
                    motion: ThemeMotion::Full,
                    typography: ThemeTypography {
                        body_family: "system-sans".into(),
                        mono_family: "system-mono".into(),
                        body_size: 17,
                        display_ratio_milli: 1235,
                    },
                    colors: BTreeMap::new(),
                    roles: BTreeMap::new(),
                },
            }]
        }

        fn selected_theme_id(&self) -> ThemeId {
            ThemeId::new("mock-dark").unwrap()
        }

        async fn list_projects(&self) -> Result<Vec<crate::ProjectSummary>, ServiceError> {
            let projects = self.project.lock().unwrap().clone().into_iter().collect();
            if let Some(entered) = &self.list_projects_entered {
                entered.wait().await;
            }
            if let Some(release) = &self.list_projects_release {
                release.wait().await;
            }
            Ok(projects)
        }

        fn project_lifecycle_mutations_supported(&self) -> bool {
            self.project.lock().unwrap().is_some()
        }

        async fn set_project_trust(
            &self,
            project_id: &ProjectId,
            trusted: bool,
        ) -> Result<crate::ProjectSummary, ServiceError> {
            let mut project = self.project.lock().unwrap();
            let project = project.as_mut().ok_or(ServiceError::NotFound)?;
            if &project.id != project_id {
                return Err(ServiceError::NotFound);
            }
            if project.archived && trusted {
                return Err(ServiceError::InvalidBoundary);
            }
            project.trusted = trusted;
            Ok(project.clone())
        }

        async fn archive_project(
            &self,
            project_id: &ProjectId,
        ) -> Result<crate::ProjectSummary, ServiceError> {
            let mut project = self.project.lock().unwrap();
            let project = project.as_mut().ok_or(ServiceError::NotFound)?;
            if &project.id != project_id {
                return Err(ServiceError::NotFound);
            }
            project.trusted = false;
            project.archived = true;
            project.is_default = false;
            project.live_session_count = 0;
            Ok(project.clone())
        }

        async fn list_sessions(&self) -> Result<Vec<crate::SessionSummary>, ServiceError> {
            let sessions = self
                .seeds
                .lock()
                .unwrap()
                .values()
                .map(|seed| seed.summary.clone())
                .collect();
            if let Some(entered) = &self.list_sessions_entered {
                entered.wait().await;
            }
            if let Some(release) = &self.list_sessions_release {
                release.wait().await;
            }
            Ok(sessions)
        }

        async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<Self::Driver, ServiceError> {
            if let Some(barrier) = &self.create_barrier {
                barrier.wait().await;
            }
            if let Some(entered) = &self.create_entered {
                entered.wait().await;
            }
            if let Some(release) = &self.create_release {
                release.wait().await;
            }
            let seed = self.make_seed(&request);
            self.seeds
                .lock()
                .unwrap()
                .insert(seed.summary.id.clone(), seed.clone());
            Ok(MockDriver {
                seed,
                dispatches: Arc::clone(&self.dispatches),
                fail_next_finalization: Arc::clone(&self.fail_next_finalization),
                never_quiesce: Arc::clone(&self.never_quiesce),
            })
        }

        async fn open_session(&self, session_id: &SessionId) -> Result<Self::Driver, ServiceError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            if self.open_gate_armed.swap(false, Ordering::AcqRel) {
                if let Some(entered) = &self.open_entered {
                    entered.wait().await;
                }
                if let Some(release) = &self.open_release {
                    release.wait().await;
                }
            }
            let seed = self
                .seeds
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .ok_or(ServiceError::NotFound)?;
            Ok(MockDriver {
                seed,
                dispatches: Arc::clone(&self.dispatches),
                fail_next_finalization: Arc::clone(&self.fail_next_finalization),
                never_quiesce: Arc::clone(&self.never_quiesce),
            })
        }
    }

    fn command(session_id: SessionId, command_id: &str, text: &str) -> SessionCommandEnvelope {
        SessionCommandEnvelope::new(
            HostId::new("host-mock").unwrap(),
            DeviceId::new("device-mock").unwrap(),
            session_id,
            CommandId::new(command_id).unwrap(),
            1,
            Some(1),
            SessionCommand::SubmitPrompt {
                input: PromptInput {
                    text: text.into(),
                    attachments: Vec::new(),
                    document_ids: Vec::new(),
                    project_file_ids: Vec::new(),
                },
            },
        )
    }

    fn metadata_command(
        session_id: SessionId,
        command_id: &str,
        command: SessionCommand,
    ) -> SessionCommandEnvelope {
        SessionCommandEnvelope::new(
            HostId::new("host-mock").unwrap(),
            DeviceId::new("device-mock").unwrap(),
            session_id,
            CommandId::new(command_id).unwrap(),
            1,
            Some(1),
            command,
        )
    }

    fn create_command(device_id: &str, command_id: &str) -> HostCommandEnvelope {
        HostCommandEnvelope::new(
            HostId::new("host-mock").unwrap(),
            DeviceId::new(device_id).unwrap(),
            CommandId::new(command_id).unwrap(),
            1,
            HostCommand::CreateSession {
                project_id: None,
                authority: AuthorityProfile::FullAccess,
                model: Some(ModelSelection {
                    provider: "mock".into(),
                    model: "mock-model".into(),
                    reasoning: "off".into(),
                }),
            },
        )
    }

    fn project_command(command_id: &str, command: HostCommand) -> HostCommandEnvelope {
        HostCommandEnvelope::new(
            HostId::new("host-mock").unwrap(),
            DeviceId::new("device-mock").unwrap(),
            CommandId::new(command_id).unwrap(),
            1,
            command,
        )
    }

    #[tokio::test]
    async fn trust_revocation_fences_commands_and_retires_matching_actors() {
        let (host, project_id) = MockHost::with_project();
        let supervisor = SessionSupervisor::new(Arc::new(host), SupervisorConfig::default());
        let handle = supervisor
            .create_fresh_session(Some(project_id.clone()))
            .await
            .unwrap();
        let session_id = handle.session_id().clone();

        let revoked = supervisor
            .host_command(
                project_command(
                    "project-revoke",
                    HostCommand::SetProjectTrust {
                        project_id: project_id.clone(),
                        trusted: false,
                    },
                ),
                10,
            )
            .await
            .unwrap();
        assert!(matches!(
            revoked.ack.disposition,
            crate::HostAckDisposition::Accepted {
                project: Some(crate::ProjectSummary { trusted: false, .. }),
                ..
            }
        ));
        let catalog = supervisor.project_catalog().await.unwrap();
        assert_eq!(catalog.projects[0].live_session_count, 0);
        assert!(matches!(
            supervisor
                .command(command(session_id, "blocked-command", "must not run"), 11)
                .await,
            Err(SupervisorError::Service(ServiceError::Unauthorized))
        ));
        assert!(matches!(
            supervisor
                .create_fresh_session(Some(project_id.clone()))
                .await,
            Err(SupervisorError::Service(ServiceError::Unauthorized))
        ));
        tokio::time::timeout(Duration::from_secs(1), handle.closed())
            .await
            .expect("revoked project owner must close");
        tokio::time::timeout(Duration::from_secs(1), handle.quiesced())
            .await
            .expect("revoked project owner must quiesce");

        let granted = supervisor
            .host_command(
                project_command(
                    "project-grant",
                    HostCommand::SetProjectTrust {
                        project_id: project_id.clone(),
                        trusted: true,
                    },
                ),
                12,
            )
            .await
            .unwrap();
        assert!(matches!(
            granted.ack.disposition,
            crate::HostAckDisposition::Accepted {
                project: Some(crate::ProjectSummary { trusted: true, .. }),
                ..
            }
        ));
        supervisor
            .create_fresh_session(Some(project_id))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fresh_sessions_publish_complete_catalog_summaries() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());
        let mut events = supervisor.subscribe_events();

        let handle = supervisor.create_fresh_session(None).await.unwrap();
        let streamed = events.recv().await.unwrap();
        let catalog = streamed.catalog.as_ref().expect("catalog change");

        assert!(streamed.event.is_none());
        assert_eq!(catalog.catalog_cursor, supervisor.catalog_cursor());
        assert_eq!(catalog.summary.id, *handle.session_id());
        assert!(catalog.summary.provisional);
        streamed.validate().unwrap();
    }

    #[tokio::test]
    async fn concurrent_catalog_producers_preserve_revision_delivery_order() {
        const PRODUCERS: usize = 32;

        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(host, SupervisorConfig::default()));
        let mut events = supervisor.subscribe_events();
        let handle = supervisor.create_fresh_session(None).await.unwrap();
        let initial = events.recv().await.unwrap();
        let initial_catalog = initial.catalog.expect("initial catalog change");
        let barrier = Arc::new(std::sync::Barrier::new(PRODUCERS + 1));
        let mut producers = Vec::with_capacity(PRODUCERS);

        for index in 0..PRODUCERS {
            let supervisor = Arc::clone(&supervisor);
            let barrier = Arc::clone(&barrier);
            let mut summary = handle.view().summary;
            summary.title = format!("Concurrent summary {index}");
            producers.push(std::thread::spawn(move || {
                barrier.wait();
                supervisor.publish_catalog(summary);
            }));
        }
        barrier.wait();
        for producer in producers {
            producer.join().unwrap();
        }

        let mut previous_host_sequence = initial.host_sequence;
        let mut previous_catalog_cursor = initial_catalog.catalog_cursor.0;
        for _ in 0..PRODUCERS {
            let streamed = events.try_recv().unwrap();
            let catalog = streamed.catalog.expect("ordered catalog change");
            assert_eq!(streamed.host_sequence, previous_host_sequence + 1);
            assert_eq!(catalog.catalog_cursor.0, previous_catalog_cursor + 1);
            previous_host_sequence = streamed.host_sequence;
            previous_catalog_cursor = catalog.catalog_cursor.0;
        }
        assert_eq!(previous_catalog_cursor, supervisor.catalog_cursor().0,);
    }

    #[tokio::test]
    async fn bootstrap_cursor_precedes_catalog_changes_that_race_listings() {
        let (host, projects_entered, projects_release, sessions_entered, sessions_release) =
            MockHost::with_gated_catalog();
        let seed = host.make_seed(&CreateSessionRequest {
            project_id: None,
            provisional: false,
            authority: AuthorityProfile::FullAccess,
            model: None,
        });
        let session_id = seed.summary.id.clone();
        host.seeds
            .lock()
            .unwrap()
            .insert(session_id.clone(), seed.clone());
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::new(host),
            SupervisorConfig::default(),
        ));
        let bootstrap_cursor = supervisor.catalog_cursor();
        let mut events = supervisor.subscribe_events();
        let bootstrap = {
            let supervisor = Arc::clone(&supervisor);
            tokio::spawn(async move { supervisor.inventory_bootstrap().await })
        };

        projects_entered.wait().await;
        projects_release.wait().await;
        sessions_entered.wait().await;
        let mut refreshed = seed.summary;
        refreshed.pull_request = Some(crate::PullRequestSummary {
            state: crate::PullRequestState::Ready,
        });
        supervisor.publish_catalog(refreshed.clone());
        let streamed = events.recv().await.unwrap();
        sessions_release.wait().await;

        let bootstrap = bootstrap.await.unwrap().unwrap();
        assert_eq!(bootstrap.catalog_cursor, bootstrap_cursor);
        assert!(
            bootstrap
                .sessions
                .iter()
                .find(|summary| summary.id == session_id)
                .unwrap()
                .pull_request
                .is_none(),
            "the gated host listing intentionally returned its earlier projection"
        );
        let catalog = streamed.catalog.expect("racing catalog change");
        assert!(catalog.catalog_cursor > bootstrap.catalog_cursor);
        assert_eq!(catalog.summary, refreshed);
    }

    #[tokio::test]
    async fn bootstrap_keeps_durable_pull_request_when_the_actor_view_lags() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(Arc::clone(&host), SupervisorConfig::default());
        let handle = supervisor.create_fresh_session(None).await.unwrap();
        let session_id = handle.session_id().clone();
        host.seeds
            .lock()
            .unwrap()
            .get_mut(&session_id)
            .unwrap()
            .summary
            .pull_request = Some(crate::PullRequestSummary {
            state: crate::PullRequestState::Merged,
        });

        let inventory = supervisor.inventory_bootstrap().await.unwrap();
        assert_eq!(
            inventory
                .sessions
                .iter()
                .find(|summary| summary.id == session_id)
                .and_then(|summary| summary.pull_request.as_ref())
                .map(|pull_request| pull_request.state),
            Some(crate::PullRequestState::Merged)
        );

        let selected = supervisor.bootstrap(&session_id).await.unwrap();
        assert_eq!(
            selected
                .sessions
                .iter()
                .find(|summary| summary.id == session_id)
                .and_then(|summary| summary.pull_request.as_ref())
                .map(|pull_request| pull_request.state),
            Some(crate::PullRequestState::Merged)
        );
    }

    #[tokio::test]
    async fn inactive_catalog_publication_waits_for_the_live_owner_to_retire() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());
        let mut events = supervisor.subscribe_events();
        let handle = supervisor.create_fresh_session(None).await.unwrap();
        events.recv().await.unwrap();
        let session_id = handle.session_id().clone();
        let mut inactive = handle.view().summary;
        inactive.owner = ActorOwnerState::Inactive;
        inactive.pull_request = Some(crate::PullRequestSummary {
            state: crate::PullRequestState::Ready,
        });

        assert!(supervisor.hosted_session_ids().await.contains(&session_id));
        assert!(!supervisor
            .publish_inactive_catalog_summary(inactive.clone())
            .await
            .unwrap());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );

        let admission = supervisor
            .command(
                metadata_command(
                    session_id.clone(),
                    "retiring-rename",
                    SessionCommand::Rename {
                        title: "Retiring session".into(),
                    },
                ),
                20,
            )
            .await
            .unwrap();
        assert!(matches!(
            admission.ack.disposition,
            AckDisposition::Accepted { .. }
        ));
        inactive = handle.view().summary;
        inactive.owner = ActorOwnerState::Inactive;
        inactive.pull_request = Some(crate::PullRequestSummary {
            state: crate::PullRequestState::Ready,
        });

        handle.retire().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if supervisor
                    .publish_inactive_catalog_summary(inactive.clone())
                    .await
                    .unwrap()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("inactive publication must resume after actor retirement");

        let mut previous_host_sequence = 0;
        let mut published = false;
        for _ in 0..8 {
            let streamed = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("retiring actor events must drain before inactive publication")
                .unwrap();
            assert!(streamed.host_sequence > previous_host_sequence);
            previous_host_sequence = streamed.host_sequence;
            if streamed
                .catalog
                .is_some_and(|catalog| catalog.summary == inactive)
            {
                published = true;
                break;
            }
        }
        assert!(published);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err(),
            "no stale actor event may overtake the inactive catalog summary"
        );
    }

    #[tokio::test]
    async fn session_metadata_changes_reach_the_live_event_and_catalog_streams() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());
        let mut events = supervisor.subscribe_events();
        let handle = supervisor.create_fresh_session(None).await.unwrap();
        events.recv().await.unwrap();
        let session_id = handle.session_id().clone();

        for (command_id, command) in [
            (
                "command-rename",
                SessionCommand::Rename {
                    title: "Renamed session".into(),
                },
            ),
            ("command-pin", SessionCommand::SetPinned { pinned: true }),
            (
                "command-archive",
                SessionCommand::SetArchived { archived: true },
            ),
        ] {
            let admission = supervisor
                .command(
                    metadata_command(session_id.clone(), command_id, command),
                    20,
                )
                .await
                .unwrap();
            assert!(matches!(
                admission.ack.disposition,
                AckDisposition::Accepted { .. }
            ));
        }

        let view = supervisor.session_view(&session_id).await.unwrap();
        assert_eq!(view.summary.title, "Renamed session");
        assert!(view.summary.pinned);
        assert!(view.summary.archived);

        let mut metadata_event_count = 0;
        let mut final_catalog = None;
        for _ in 0..12 {
            let streamed = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .expect("metadata change must reach the host stream")
                .unwrap();
            if matches!(
                streamed.event.as_ref().map(|event| &event.event),
                Some(EventPayload::SessionMetadataChanged { .. })
            ) {
                metadata_event_count += 1;
            }
            if let Some(catalog) = streamed.catalog {
                if catalog.summary.title == "Renamed session"
                    && catalog.summary.pinned
                    && catalog.summary.archived
                {
                    final_catalog = Some(catalog);
                    break;
                }
            }
        }
        assert_eq!(metadata_event_count, 3);
        assert_eq!(
            final_catalog.unwrap().catalog_cursor,
            supervisor.catalog_cursor()
        );
    }

    #[tokio::test]
    async fn actor_fan_in_preserves_a_detectable_session_cursor_gap_after_lag() {
        let session_id = SessionId::new("session-lag").unwrap();
        let (actor_sender, _) = broadcast::channel(2);
        let mut actor_receiver = actor_sender.subscribe();
        for sequence in 1..=3 {
            actor_sender
                .send(crate::EventEnvelope::new(
                    session_id.clone(),
                    SessionCursor {
                        actor_generation: 1,
                        sequence,
                    },
                    sequence,
                    EventPayload::SessionStateChanged {
                        state: SessionLiveState::Idle,
                        active_run_id: None,
                    },
                ))
                .unwrap();
        }
        drop(actor_sender);

        let (host_sender, mut host_receiver) = broadcast::channel(4);
        let order = std::sync::Mutex::new(0);
        forward_actor_events(&mut actor_receiver, &host_sender, &order).await;
        let first = host_receiver.recv().await.unwrap();
        let second = host_receiver.recv().await.unwrap();
        assert_eq!(first.event.unwrap().cursor.sequence, 2);
        assert_eq!(second.event.unwrap().cursor.sequence, 3);
        assert_eq!(first.host_sequence, 1);
        assert_eq!(second.host_sequence, 2);
    }

    #[tokio::test]
    async fn fresh_owner_cap_still_allows_an_explicit_durable_restore() {
        let host = Arc::new(MockHost::new());
        let existing = host.make_seed(&CreateSessionRequest {
            project_id: None,
            provisional: false,
            authority: AuthorityProfile::FullAccess,
            model: None,
        });
        let existing_id = existing.summary.id.clone();
        host.seeds
            .lock()
            .unwrap()
            .insert(existing_id.clone(), existing);
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());

        for _ in 0..MAX_FRESH_PROVISIONAL_OWNERS {
            supervisor.create_fresh_session(None).await.unwrap();
        }
        assert!(matches!(
            supervisor.create_fresh_session(None).await,
            Err(SupervisorError::Service(ServiceError::Unavailable))
        ));
        assert_eq!(
            supervisor
                .open_session(&existing_id)
                .await
                .unwrap()
                .session_id(),
            &existing_id
        );
    }

    #[tokio::test]
    async fn two_sessions_are_owned_and_projected_in_isolation() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host.clone(), SupervisorConfig::default());
        let first = supervisor.launch(None).await.unwrap();
        let second = supervisor.create_fresh_session(None).await.unwrap();
        let first_id = first.selected_session_id.unwrap();
        let second_id = second.session_id().clone();

        supervisor
            .command(command(first_id.clone(), "command-first", "alpha"), 20)
            .await
            .unwrap();
        supervisor
            .command(command(second_id.clone(), "command-second", "beta"), 21)
            .await
            .unwrap();

        let first_view = supervisor.session_view(&first_id).await.unwrap();
        let second_view = supervisor.session_view(&second_id).await.unwrap();
        assert_eq!(first_view.snapshot.items.len(), 1);
        assert_eq!(second_view.snapshot.items.len(), 1);
        assert!(matches!(
            &first_view.snapshot.items[0].payload,
            ItemPayload::UserMessage { text, .. } if text == "alpha"
        ));
        assert!(matches!(
            &second_view.snapshot.items[0].payload,
            ItemPayload::UserMessage { text, .. } if text == "beta"
        ));
        assert_eq!(first_view.snapshot.cursor.sequence, 3);
        assert_eq!(second_view.snapshot.cursor.sequence, 3);
        assert_eq!(host.dispatch_count(&first_id), 1);
        assert_eq!(host.dispatch_count(&second_id), 1);
        assert_eq!(supervisor.active_session_count().await, 2);
    }

    #[tokio::test]
    async fn duplicate_command_reuses_exact_ack_and_never_dispatches_twice() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host.clone(), SupervisorConfig::default());
        let bootstrap = supervisor.launch(None).await.unwrap();
        let session_id = bootstrap.selected_session_id.unwrap();
        let envelope = command(session_id.clone(), "command-once", "do it");

        let first = supervisor.command(envelope.clone(), 20).await.unwrap();
        let second = supervisor.command(envelope, 99).await.unwrap();
        assert!(!first.cached);
        assert!(second.cached);
        assert_eq!(first.ack, second.ack);
        assert_eq!(host.dispatch_count(&session_id), 1);

        let conflict = supervisor
            .command(
                command(session_id.clone(), "command-once", "different"),
                100,
            )
            .await
            .unwrap();
        let AckDisposition::Rejected { error } = conflict.ack.disposition else {
            panic!("reused ID with different content must reject");
        };
        assert_eq!(error.code, crate::ErrorCode::CommandIdConflict);
        assert_eq!(host.dispatch_count(&session_id), 1);

        let other_device = SessionCommandEnvelope::new(
            HostId::new("host-mock").unwrap(),
            DeviceId::new("device-other").unwrap(),
            session_id.clone(),
            CommandId::new("command-once").unwrap(),
            101,
            Some(1),
            SessionCommand::SubmitPrompt {
                input: PromptInput {
                    text: "independent device identity".into(),
                    attachments: Vec::new(),
                    document_ids: Vec::new(),
                    project_file_ids: Vec::new(),
                },
            },
        );
        let independent = supervisor.command(other_device, 101).await.unwrap();
        assert!(!independent.cached);
        assert_eq!(host.dispatch_count(&session_id), 2);
    }

    #[tokio::test]
    async fn duplicate_host_create_reuses_exact_ack_without_a_second_session() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());
        let envelope = create_command("device-mock", "command-create");

        let first = supervisor.host_command(envelope.clone(), 20).await.unwrap();
        let second = supervisor.host_command(envelope, 99).await.unwrap();
        assert!(!first.cached);
        assert!(second.cached);
        assert_eq!(first.ack, second.ack);
        assert_eq!(supervisor.active_session_count().await, 1);

        let other_device = supervisor
            .host_command(create_command("device-other", "command-create"), 100)
            .await
            .unwrap();
        assert!(!other_device.cached);
        assert_eq!(supervisor.active_session_count().await, 2);
    }

    #[tokio::test]
    async fn cancelled_host_create_finishes_and_retry_reuses_the_result() {
        let (host, entered, release) = MockHost::with_gated_create();
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::new(host),
            SupervisorConfig::default(),
        ));
        let envelope = create_command("device-mock", "command-cancelled-create");
        let initiating = {
            let supervisor = Arc::clone(&supervisor);
            let envelope = envelope.clone();
            tokio::spawn(async move { supervisor.host_command(envelope, 20).await })
        };

        entered.wait().await;
        initiating.abort();
        release.wait().await;
        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            supervisor.host_command(envelope, 99),
        )
        .await
        .expect("retry must not wait on an abandoned InFlight reservation")
        .unwrap();
        assert!(retry.cached);
        assert!(matches!(
            retry.ack.disposition,
            crate::HostAckDisposition::Accepted { .. }
        ));
        assert_eq!(supervisor.active_session_count().await, 1);
    }

    #[tokio::test]
    async fn concurrent_open_of_one_session_constructs_one_driver() {
        let host = Arc::new(MockHost::new());
        let request = CreateSessionRequest {
            project_id: None,
            provisional: false,
            authority: AuthorityProfile::FullAccess,
            model: None,
        };
        let seed = host.create_session(request).await.unwrap().seed();
        let session_id = seed.summary.id;
        let supervisor = SessionSupervisor::new(host.clone(), SupervisorConfig::default());

        let (first, second) = tokio::join!(
            supervisor.open_session(&session_id),
            supervisor.open_session(&session_id)
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.session_id(), second.session_id());
        assert_eq!(host.opens.load(Ordering::Relaxed), 1);
        assert_eq!(supervisor.active_session_count().await, 1);
    }

    #[tokio::test]
    async fn quarantined_owner_wait_is_bounded_without_releasing_the_fence() {
        let host = Arc::new(MockHost::new());
        let seed = host
            .create_session(CreateSessionRequest {
                project_id: None,
                provisional: false,
                authority: AuthorityProfile::FullAccess,
                model: None,
            })
            .await
            .unwrap()
            .seed();
        let session_id = seed.summary.id;
        let supervisor = SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig {
                quarantine_wait_timeout: Duration::from_millis(20),
                ..SupervisorConfig::default()
            },
        );
        let original = supervisor.open_session(&session_id).await.unwrap();
        assert_eq!(host.opens.load(Ordering::Relaxed), 1);

        host.never_quiesce.store(true, Ordering::Release);
        host.fail_next_finalization.store(true, Ordering::Release);
        assert!(matches!(
            original
                .command(
                    command(session_id.clone(), "command-never-quiesces", "retire owner",),
                    20,
                )
                .await,
            Err(ActorError::Closed)
        ));
        original.closed().await;
        assert!(!original.is_quiesced());

        let opens_before_retry = host.opens.load(Ordering::Relaxed);
        let retry = tokio::time::timeout(
            Duration::from_millis(200),
            supervisor.open_session(&session_id),
        )
        .await
        .expect("quarantine wait must be bounded");
        assert!(matches!(
            retry,
            Err(SupervisorError::Service(ServiceError::Unavailable))
        ));
        assert_eq!(
            host.opens.load(Ordering::Relaxed),
            opens_before_retry,
            "a timed-out caller must not construct a replacement driver"
        );
        assert_eq!(
            supervisor.active_session_count().await,
            1,
            "the never-quiesced owner remains registered as the durable fence"
        );
    }

    #[tokio::test]
    async fn bootstrap_reopens_after_selected_owner_closes_during_catalog_listing() {
        let (host, projects_entered, projects_release, sessions_entered, sessions_release) =
            MockHost::with_gated_catalog();
        let host = Arc::new(host);
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig {
                quarantine_wait_timeout: Duration::from_millis(20),
                ..SupervisorConfig::default()
            },
        ));
        let original = supervisor.create_fresh_session(None).await.unwrap();
        let session_id = original.session_id().clone();
        let bootstrap = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.bootstrap(&session_id).await })
        };

        projects_entered.wait().await;
        projects_release.wait().await;
        sessions_entered.wait().await;

        host.fail_next_finalization.store(true, Ordering::Release);
        assert!(matches!(
            original
                .command(
                    command(
                        session_id.clone(),
                        "command-close-during-bootstrap",
                        "retire owner",
                    ),
                    20,
                )
                .await,
            Err(ActorError::Closed)
        ));
        original.closed().await;
        original.quiesced().await;
        {
            let mut seeds = host.seeds.lock().unwrap();
            let refreshed = seeds.get_mut(&session_id).unwrap();
            refreshed.summary.title = "Refreshed after catalog race".into();
            refreshed.summary.modified_at_ms = 99;
            refreshed.snapshot.actor_generation = 2;
            refreshed.snapshot.cursor = SessionCursor::zero(2);
        }
        sessions_release.wait().await;

        let bootstrap = tokio::time::timeout(Duration::from_secs(1), bootstrap)
            .await
            .expect("bootstrap must not retain the closed selected actor")
            .unwrap()
            .unwrap();
        assert_eq!(host.opens.load(Ordering::Relaxed), 1);
        assert_eq!(
            bootstrap
                .selected_session
                .as_ref()
                .unwrap()
                .actor_generation,
            2
        );
        assert_eq!(
            bootstrap.selected_session.as_ref().unwrap().cursor,
            SessionCursor::zero(2)
        );
        assert_eq!(
            bootstrap
                .sessions
                .iter()
                .find(|summary| summary.id == session_id)
                .unwrap()
                .title,
            "Refreshed after catalog race"
        );
    }

    #[tokio::test]
    async fn bootstrap_does_not_overlay_a_closed_unquiesced_actor_summary() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig {
                quarantine_wait_timeout: Duration::from_millis(20),
                ..SupervisorConfig::default()
            },
        );
        let selected = supervisor.create_fresh_session(None).await.unwrap();
        let retired = supervisor.create_fresh_session(None).await.unwrap();
        let retired_id = retired.session_id().clone();
        let rename = SessionCommandEnvelope::new(
            HostId::new("host-mock").unwrap(),
            DeviceId::new("device-mock").unwrap(),
            retired_id.clone(),
            CommandId::new("command-transient-title").unwrap(),
            1,
            Some(1),
            SessionCommand::Rename {
                title: "Transient actor-only title".into(),
            },
        );
        retired.command(rename, 10).await.unwrap();
        assert_eq!(retired.view().summary.title, "Transient actor-only title");

        host.never_quiesce.store(true, Ordering::Release);
        host.fail_next_finalization.store(true, Ordering::Release);
        assert!(matches!(
            retired
                .command(
                    command(
                        retired_id.clone(),
                        "command-close-overlay-owner",
                        "retire owner",
                    ),
                    20,
                )
                .await,
            Err(ActorError::Closed)
        ));
        retired.closed().await;
        assert!(!retired.is_quiesced());

        let bootstrap = supervisor.bootstrap(selected.session_id()).await.unwrap();
        assert_eq!(
            bootstrap
                .sessions
                .iter()
                .find(|summary| summary.id == retired_id)
                .unwrap()
                .title,
            "Fresh session",
            "closed actor state must not override the durable catalog summary"
        );
    }

    #[tokio::test]
    async fn concurrent_reopen_after_owner_closes_installs_one_refreshed_actor() {
        let (host, entered, release) = MockHost::with_gated_open();
        let host = Arc::new(host);
        let request = CreateSessionRequest {
            project_id: None,
            provisional: false,
            authority: AuthorityProfile::FullAccess,
            model: None,
        };
        let seed = host.create_session(request).await.unwrap().seed();
        let session_id = seed.summary.id;
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));
        let original = supervisor.open_session(&session_id).await.unwrap();
        assert_eq!(host.opens.load(Ordering::Relaxed), 1);

        host.fail_next_finalization.store(true, Ordering::Release);
        assert!(matches!(
            original
                .command(
                    command(session_id.clone(), "command-close-owner", "close"),
                    20,
                )
                .await,
            Err(ActorError::Closed)
        ));
        original.closed().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while supervisor.active_session_count().await != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed actor must be proactively evicted");

        {
            let mut seeds = host.seeds.lock().unwrap();
            let refreshed = seeds.get_mut(&session_id).unwrap();
            refreshed.summary.title = "Refreshed durable session".into();
            refreshed.summary.modified_at_ms = 99;
            refreshed.snapshot.actor_generation = 2;
            refreshed.snapshot.cursor = SessionCursor::zero(2);
        }

        let opens_before_reopen = host.opens.load(Ordering::Relaxed);
        host.open_gate_armed.store(true, Ordering::Release);
        let first_reopen = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.open_session(&session_id).await })
        };
        entered.wait().await;
        let second_reopen = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.open_session(&session_id).await })
        };
        tokio::task::yield_now().await;
        release.wait().await;

        let (first_reopen, second_reopen) = tokio::join!(first_reopen, second_reopen);
        let first_reopen = first_reopen.unwrap().unwrap();
        let second_reopen = second_reopen.unwrap().unwrap();
        assert!(first_reopen.same_actor(&second_reopen));
        assert_eq!(host.opens.load(Ordering::Relaxed) - opens_before_reopen, 1);
        assert_eq!(supervisor.active_session_count().await, 1);
        let refreshed = first_reopen.view();
        assert_eq!(refreshed.summary.title, "Refreshed durable session");
        assert_eq!(refreshed.snapshot.actor_generation, 2);
        assert_eq!(refreshed.snapshot.cursor, SessionCursor::zero(2));
    }

    #[tokio::test]
    async fn session_mutation_fence_waits_for_an_inflight_open() {
        let (host, entered, release) = MockHost::with_gated_open();
        let host = Arc::new(host);
        let seed = host
            .create_session(CreateSessionRequest {
                project_id: None,
                provisional: false,
                authority: AuthorityProfile::FullAccess,
                model: None,
            })
            .await
            .unwrap()
            .seed();
        let session_id = seed.summary.id;
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));

        host.open_gate_armed.store(true, Ordering::Release);
        let opening = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.open_session(&session_id).await })
        };
        entered.wait().await;

        let fencing = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.block_and_retire_session(&session_id).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if supervisor
                    .state
                    .lock()
                    .await
                    .blocked_sessions
                    .contains(&session_id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the lifecycle fence must become visible");

        assert!(matches!(
            supervisor.open_session(&session_id).await,
            Err(SupervisorError::Service(ServiceError::Unavailable))
        ));
        assert!(!fencing.is_finished());
        release.wait().await;
        assert!(matches!(
            opening.await.unwrap(),
            Err(SupervisorError::Service(ServiceError::Unavailable))
        ));
        fencing.await.unwrap();
        assert_eq!(supervisor.active_session_count().await, 0);

        supervisor.unblock_session(&session_id).await;
        assert_eq!(
            supervisor
                .open_session(&session_id)
                .await
                .unwrap()
                .session_id(),
            &session_id
        );
    }

    #[tokio::test]
    async fn slow_session_factories_do_not_hold_the_global_actor_map_lock() {
        let host = Arc::new(MockHost::with_create_barrier(2));
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());
        let launches = async {
            tokio::join!(
                supervisor.create_fresh_session(None),
                supervisor.create_fresh_session(None)
            )
        };
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(1), launches)
            .await
            .expect("both factories must reach the barrier concurrently");
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(supervisor.active_session_count().await, 2);
    }

    #[tokio::test]
    async fn launch_bootstrap_selects_a_full_access_provisional_session() {
        let host = Arc::new(MockHost::new());
        let supervisor = SessionSupervisor::new(host, SupervisorConfig::default());
        let bootstrap = supervisor.launch(None).await.unwrap();
        assert_eq!(
            bootstrap.selected_session.as_ref().unwrap().authority,
            AuthorityProfile::FullAccess
        );
        assert!(!bootstrap.capabilities.lan_clients);
        let selected_session_id = bootstrap.selected_session_id.as_ref().unwrap();
        let summary = bootstrap
            .sessions
            .iter()
            .find(|summary| &summary.id == selected_session_id)
            .unwrap();
        assert!(summary.provisional);
    }
}
