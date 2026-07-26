//! Host catalog and exclusive multi-session actor supervision.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, oneshot, Mutex};

use crate::{
    ActorConfig, ActorError, ActorView, AuthorityProfile, CatalogCursor, CommandAdmission,
    CommandId, CreateSessionRequest, DeviceId, ErrorCode, HostBootstrap, HostCommand,
    HostCommandAck, HostCommandEnvelope, HostService, HostStreamEvent, ModelSelection,
    ProtocolValidation, ReplayResponse, SanitizedError, ServiceError, SessionActor,
    SessionActorHandle, SessionCommandEnvelope, SessionCursor, SessionDriver, SessionId,
    PROTOCOL_VERSION,
};

const HOST_COMMAND_CACHE_CAPACITY: usize = 2_048;
const HOST_EVENT_BROADCAST_CAPACITY: usize = 4_096;
const MAX_FRESH_PROVISIONAL_OWNERS: usize = 64;
const MAX_ACTIVE_SESSION_OWNERS: usize = 512;
const MAX_BOOTSTRAP_SESSION_SUMMARIES: usize = 2_000;

/// Supervisor configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Per-session actor configuration.
    pub actor: ActorConfig,
    /// Requested initial authority for fresh sessions.
    pub fresh_session_authority: AuthorityProfile,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            actor: ActorConfig::default(),
            // Preserve Ygg's current default. HostService may clamp the remote
            // selection ceiling without changing local Agent defaults.
            fresh_session_authority: AuthorityProfile::FullAccess,
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
        }

        let begin = {
            let mut state = self.state.lock().await;
            if let Some(existing) = state.actors.get(session_id) {
                Begin::Return(existing.clone())
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
            Begin::Lead => {}
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

    /// Builds a fresh catalog/bootstrap around an already hosted session.
    pub async fn bootstrap(
        &self,
        selected_session_id: &SessionId,
    ) -> Result<HostBootstrap, SupervisorError> {
        let projects = self.host.list_projects().await?;
        let mut summaries = self
            .host
            .list_sessions()
            .await?
            .into_iter()
            .map(|summary| (summary.id.clone(), summary))
            .collect::<BTreeMap<_, _>>();
        let (selected, active_views) = {
            let state = self.state.lock().await;
            let selected = state
                .actors
                .get(selected_session_id)
                .cloned()
                .ok_or(ServiceError::NotFound)?;
            let active_views = state
                .actors
                .values()
                .map(SessionActorHandle::view)
                .collect::<Vec<_>>();
            (selected, active_views)
        };
        for view in active_views {
            summaries.insert(view.summary.id.clone(), view.summary);
        }
        let mut sessions = summaries.into_values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left.archived
                .cmp(&right.archived)
                .then_with(|| right.pinned.cmp(&left.pinned))
                .then_with(|| {
                    session_activity_rank(right.live_state)
                        .cmp(&session_activity_rank(left.live_state))
                })
                .then_with(|| right.modified_at_ms.cmp(&left.modified_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        if sessions.len() > MAX_BOOTSTRAP_SESSION_SUMMARIES {
            if let Some(selected_index) = sessions
                .iter()
                .position(|summary| &summary.id == selected_session_id)
            {
                if selected_index >= MAX_BOOTSTRAP_SESSION_SUMMARIES {
                    sessions.swap(selected_index, MAX_BOOTSTRAP_SESSION_SUMMARIES - 1);
                }
            }
            sessions.truncate(MAX_BOOTSTRAP_SESSION_SUMMARIES);
        }

        let selected_view = selected.view();
        let bootstrap = HostBootstrap {
            protocol: PROTOCOL_VERSION,
            host: self.host.descriptor(),
            capabilities: self.host.capabilities(),
            catalog_cursor: self.catalog_cursor(),
            models: self.host.model_catalog(),
            authority_profiles: self.host.authority_profiles(),
            authority_ceiling: self.host.authority_ceiling(),
            themes: self.host.theme_catalog(),
            selected_theme_id: self.host.selected_theme_id(),
            projects,
            sessions,
            selected_session_id: selected_session_id.clone(),
            selected_session: selected_view.snapshot,
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

    /// Reads one authenticated opaque resource without interpreting its handle.
    pub async fn resource_content(
        &self,
        handle: &str,
    ) -> Result<crate::StoredResource, crate::ServiceError> {
        self.host.resource_content(handle).await
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
        // The potentially slow factory is deliberately outside the actor-map
        // lock. Duplicate IDs are resolved before an actor task is spawned.
        let driver = self.host.create_session(request).await?;
        let session_id = driver.seed().summary.id;
        let mut state = self.state.lock().await;
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
        let summary = handle.view().summary;
        self.observe_actor(&handle);
        state.actors.insert(session_id, handle.clone());
        drop(state);
        self.publish_catalog(summary);
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
        if let Some(existing) = state.actors.get(session_id) {
            return Ok(existing.clone());
        }
        if state.actors.len() >= MAX_ACTIVE_SESSION_OWNERS {
            return Err(SupervisorError::Service(ServiceError::Unavailable));
        }
        let handle = self.spawn_driver(driver)?;
        if handle.session_id() != session_id {
            return Err(SupervisorError::IdentityMismatch);
        }
        let summary = handle.view().summary;
        self.observe_actor(&handle);
        state.actors.insert(session_id.clone(), handle.clone());
        drop(state);
        self.publish_catalog(summary);
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
        }
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

    fn observe_actor(&self, handle: &SessionActorHandle) {
        let mut views = handle.subscribe();
        let cursor = Arc::clone(&self.catalog_cursor);
        let sender = self.host_events.clone();
        let order = Arc::clone(&self.host_event_order);
        tokio::spawn(async move {
            while views.changed().await.is_ok() {
                let summary = views.borrow_and_update().summary.clone();
                let Some(catalog_cursor) = advance_catalog(&cursor) else {
                    break;
                };
                let Some(streamed) = ordered_catalog_event(&order, catalog_cursor, summary) else {
                    break;
                };
                let _ = sender.send(streamed);
            }
        });

        let mut events = handle.subscribe_events();
        let sender = self.host_events.clone();
        let order = Arc::clone(&self.host_event_order);
        tokio::spawn(async move { forward_actor_events(&mut events, &sender, &order).await });
    }

    fn publish_catalog(&self, summary: crate::SessionSummary) {
        let Some(catalog_cursor) = advance_catalog(&self.catalog_cursor) else {
            return;
        };
        let Some(streamed) = ordered_catalog_event(&self.host_event_order, catalog_cursor, summary)
        else {
            return;
        };
        let _ = self.host_events.send(streamed);
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

fn ordered_catalog_event(
    order: &std::sync::Mutex<u64>,
    catalog_cursor: CatalogCursor,
    summary: crate::SessionSummary,
) -> Option<HostStreamEvent> {
    let mut sequence = order.lock().ok()?;
    let next = sequence.checked_add(1)?;
    *sequence = next;
    Some(HostStreamEvent::catalog(next, catalog_cursor, summary))
}

async fn forward_actor_events(
    events: &mut broadcast::Receiver<crate::EventEnvelope>,
    sender: &broadcast::Sender<HostStreamEvent>,
    order: &std::sync::Mutex<u64>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                let streamed = {
                    let mut sequence = order.lock().expect("host event order poisoned");
                    let Some(next) = sequence.checked_add(1) else {
                        break;
                    };
                    *sequence = next;
                    HostStreamEvent::new(next, event)
                };
                let _ = sender.send(streamed);
            }
            // Do not synthesize continuity. The next retained event keeps its
            // original per-session cursor, exposing the gap so clients can
            // recover with replay or a snapshot.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        create_barrier: Option<Arc<tokio::sync::Barrier>>,
        create_entered: Option<Arc<tokio::sync::Barrier>>,
        create_release: Option<Arc<tokio::sync::Barrier>>,
    }

    impl MockHost {
        fn new() -> Self {
            Self {
                next_session: Arc::new(AtomicUsize::new(1)),
                seeds: Arc::new(StdMutex::new(BTreeMap::new())),
                dispatches: Arc::new(StdMutex::new(BTreeMap::new())),
                opens: Arc::new(AtomicUsize::new(0)),
                create_barrier: None,
                create_entered: None,
                create_release: None,
            }
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
                    provisional: request.provisional,
                    live_state: SessionLiveState::Idle,
                    attention: AttentionState::None,
                    owner: ActorOwnerState::Hosted,
                    model: model.clone(),
                },
                snapshot: crate::SessionSnapshot {
                    session_id: id,
                    actor_generation: 1,
                    cursor: SessionCursor::zero(1),
                    durable_head: None,
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
            Ok(Vec::new())
        }

        async fn list_sessions(&self) -> Result<Vec<crate::SessionSummary>, ServiceError> {
            Ok(self
                .seeds
                .lock()
                .unwrap()
                .values()
                .map(|seed| seed.summary.clone())
                .collect())
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
            })
        }

        async fn open_session(&self, session_id: &SessionId) -> Result<Self::Driver, ServiceError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
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
        let first_id = first.selected_session_id;
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
        let session_id = bootstrap.selected_session_id;
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
            bootstrap.selected_session.authority,
            AuthorityProfile::FullAccess
        );
        assert!(!bootstrap.capabilities.lan_clients);
        let summary = bootstrap
            .sessions
            .iter()
            .find(|summary| summary.id == bootstrap.selected_session_id)
            .unwrap();
        assert!(summary.provisional);
    }
}
