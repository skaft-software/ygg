//! Exclusive per-session actor, snapshot reducer, replay, and idempotency.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    ActorOwnerState, AttentionState, AuthorityProfile, CommandAck, CommandId, DeviceId,
    DriverCommandOutcome, ErrorCode, EventEnvelope, EventJournal, EventPayload, HostId, ItemDelta,
    ItemLifecycle, ItemPayload, JournalConfig, JournalError, ProtocolValidation, ReplayResponse,
    RequestAnswer, RequestState, SanitizedError, ServiceError, SessionCommand,
    SessionCommandEnvelope, SessionCursor, SessionDriver, SessionLiveState, SessionSeed,
    SessionSnapshot, SessionSummary, TimestampedEvent, ValidationError,
};

const MAX_COMMAND_CACHE_CAPACITY: usize = 65_536;
const MAX_MAILBOX_CAPACITY: usize = 4_096;

/// Actor bounds and authority ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActorConfig {
    /// Replay journal limits.
    pub journal: JournalConfig,
    /// Exact command acknowledgement cache.
    pub command_cache_capacity: usize,
    /// Bounded actor mailbox.
    pub mailbox_capacity: usize,
    /// Maximum authority selectable through this actor.
    pub authority_ceiling: AuthorityProfile,
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            journal: JournalConfig::default(),
            command_cache_capacity: 2_048,
            mailbox_capacity: 64,
            authority_ceiling: AuthorityProfile::FullAccess,
        }
    }
}

/// Actor construction/projection failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActorError {
    /// Invalid limits.
    #[error("invalid session actor configuration: {0}")]
    InvalidConfiguration(String),
    /// Invalid initial seed.
    #[error("invalid session seed")]
    InvalidSeed,
    /// Event or snapshot failed validation.
    #[error("invalid session projection: {0}")]
    InvalidProjection(String),
    /// Replay journal failure.
    #[error("replay journal failure: {0}")]
    Journal(#[from] JournalError),
    /// Event sequence exhausted.
    #[error("session sequence exhausted")]
    SequenceExhausted,
    /// Actor task closed.
    #[error("session actor closed")]
    Closed,
}

/// Current observable actor view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorView {
    /// Sidebar summary.
    pub summary: SessionSummary,
    /// Complete selected-session snapshot.
    pub snapshot: SessionSnapshot,
}

/// Idempotent admission result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandAdmission {
    /// Exact acknowledgement.
    pub ack: CommandAck,
    /// Whether it came from the exact bounded cache.
    pub cached: bool,
    /// Immediate events published by first admission.
    pub published: Vec<EventEnvelope>,
}

#[derive(Clone)]
struct CachedCommand {
    identity: CachedCommandIdentity,
    ack: CommandAck,
}

#[derive(Clone)]
enum CachedCommandIdentity {
    Exact(Box<SessionCommandEnvelope>),
    /// Free-form one-shot answers are deliberately not retained after the
    /// driver consumes them. Reuse of the same device-scoped ID returns the
    /// original acknowledgement regardless of the retransmitted body.
    ConsumedSecret,
}

type CommandCacheKey = (DeviceId, CommandId);

/// Synchronous state that must remain with the mutable session owner.
pub struct SessionActorCore {
    host_id: HostId,
    session_id: crate::SessionId,
    generation: u64,
    authority_ceiling: AuthorityProfile,
    view: ActorView,
    journal: EventJournal,
    command_cache_capacity: usize,
    command_cache: HashMap<CommandCacheKey, CachedCommand>,
    command_order: VecDeque<CommandCacheKey>,
}

impl SessionActorCore {
    /// Creates one actor core from an authoritative driver seed.
    pub fn new(
        host_id: HostId,
        seed: SessionSeed,
        config: ActorConfig,
    ) -> Result<Self, ActorError> {
        if config.command_cache_capacity == 0
            || config.command_cache_capacity > MAX_COMMAND_CACHE_CAPACITY
        {
            return Err(ActorError::InvalidConfiguration(format!(
                "command_cache_capacity must be 1..={MAX_COMMAND_CACHE_CAPACITY}"
            )));
        }
        if config.mailbox_capacity == 0 || config.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(ActorError::InvalidConfiguration(format!(
                "mailbox_capacity must be 1..={MAX_MAILBOX_CAPACITY}"
            )));
        }
        seed.validate().map_err(|_| ActorError::InvalidSeed)?;
        if authority_rank(seed.snapshot.authority) > authority_rank(config.authority_ceiling) {
            return Err(ActorError::InvalidSeed);
        }
        let generation = seed.snapshot.actor_generation;
        let session_id = seed.snapshot.session_id.clone();
        let journal = EventJournal::new(seed.snapshot.cursor, config.journal)?;
        Ok(Self {
            host_id,
            session_id,
            generation,
            authority_ceiling: config.authority_ceiling,
            view: ActorView {
                summary: seed.summary,
                snapshot: seed.snapshot,
            },
            journal,
            command_cache_capacity: config.command_cache_capacity,
            command_cache: HashMap::with_capacity(config.command_cache_capacity.min(4_096)),
            command_order: VecDeque::with_capacity(config.command_cache_capacity.min(4_096)),
        })
    }

    /// Session identity.
    pub fn session_id(&self) -> &crate::SessionId {
        &self.session_id
    }

    /// Current actor generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Current observable view.
    pub fn view(&self) -> ActorView {
        self.view.clone()
    }

    /// Current selected-session snapshot.
    pub fn snapshot(&self) -> SessionSnapshot {
        self.view.snapshot.clone()
    }

    /// Replays after a generation-bound cursor or returns a snapshot gap.
    pub fn replay_after(&self, after: SessionCursor) -> ReplayResponse {
        self.journal.replay_after(after, &self.view.snapshot)
    }

    /// Projects one asynchronous driver event.
    pub fn publish(&mut self, event: TimestampedEvent) -> Result<EventEnvelope, ActorError> {
        let mut published = self.publish_batch(vec![event])?;
        Ok(published.remove(0))
    }

    /// Admits a command exactly once inside the bounded cache horizon.
    pub async fn admit_command<F, Fut>(
        &mut self,
        command: SessionCommandEnvelope,
        acknowledged_at_ms: u64,
        dispatch: F,
    ) -> CommandAdmission
    where
        F: FnOnce(SessionCommand) -> Fut,
        Fut: Future<Output = Result<DriverCommandOutcome, ServiceError>>,
    {
        let cache_key = (command.device_id.clone(), command.command_id.clone());
        if let Some(cached) = self.command_cache.get(&cache_key) {
            let is_duplicate = match &cached.identity {
                CachedCommandIdentity::Exact(original) => original.as_ref() == &command,
                CachedCommandIdentity::ConsumedSecret => true,
            };
            if is_duplicate {
                return CommandAdmission {
                    ack: cached.ack.clone(),
                    cached: true,
                    published: Vec::new(),
                };
            }
            return CommandAdmission {
                ack: CommandAck::rejected(
                    self.session_id.clone(),
                    command.command_id,
                    acknowledged_at_ms,
                    self.view.snapshot.cursor,
                    SanitizedError::public(
                        ErrorCode::CommandIdConflict,
                        "This command ID was already used for different content.",
                    ),
                ),
                cached: false,
                published: Vec::new(),
            };
        }

        if let Err(error) = self.preflight_command(&command) {
            let ack = CommandAck::rejected(
                self.session_id.clone(),
                command.command_id.clone(),
                acknowledged_at_ms,
                self.view.snapshot.cursor,
                error,
            );
            self.cache_command(command, ack.clone());
            return CommandAdmission {
                ack,
                cached: false,
                published: Vec::new(),
            };
        }

        let result = dispatch(command.command.clone()).await;
        let (ack, published) = match result {
            Ok(outcome) => match self.publish_batch(outcome.events) {
                Ok(published) => (
                    CommandAck::accepted(
                        self.session_id.clone(),
                        command.command_id.clone(),
                        acknowledged_at_ms,
                        self.view.snapshot.cursor,
                        outcome.run_id,
                    ),
                    published,
                ),
                Err(_) => (
                    CommandAck::rejected(
                        self.session_id.clone(),
                        command.command_id.clone(),
                        acknowledged_at_ms,
                        self.view.snapshot.cursor,
                        SanitizedError::internal(),
                    ),
                    Vec::new(),
                ),
            },
            Err(error) => (
                CommandAck::rejected(
                    self.session_id.clone(),
                    command.command_id.clone(),
                    acknowledged_at_ms,
                    self.view.snapshot.cursor,
                    error.into_public(),
                ),
                Vec::new(),
            ),
        };
        self.cache_command(command, ack.clone());
        CommandAdmission {
            ack,
            cached: false,
            published,
        }
    }

    fn preflight_command(&self, command: &SessionCommandEnvelope) -> Result<(), SanitizedError> {
        command.validate().map_err(SanitizedError::from)?;
        if command.host_id != self.host_id || command.session_id != self.session_id {
            return Err(SanitizedError::public(
                ErrorCode::InvalidCommand,
                "The command target does not match this session owner.",
            ));
        }
        if let Some(expected) = command.expected_actor_generation {
            if expected != self.generation {
                return Err(SanitizedError::public(
                    ErrorCode::StaleGeneration,
                    "The session ownership generation changed.",
                )
                .with_current_generation(self.generation));
            }
        }
        if let SessionCommand::SetAuthority { authority } = command.command {
            if authority_rank(authority) > authority_rank(self.authority_ceiling) {
                return Err(SanitizedError::public(
                    ErrorCode::Unauthorized,
                    "The requested authority exceeds this host's configured ceiling.",
                ));
            }
        }
        Ok(())
    }

    fn publish_batch(
        &mut self,
        events: Vec<TimestampedEvent>,
    ) -> Result<Vec<EventEnvelope>, ActorError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let mut replacement_view = self.view.clone();
        let mut replacement_journal = self.journal.clone();
        let mut published = Vec::with_capacity(events.len());

        for event in events {
            let cursor = replacement_view
                .snapshot
                .cursor
                .checked_next()
                .ok_or(ActorError::SequenceExhausted)?;
            let envelope = EventEnvelope::new(
                self.session_id.clone(),
                cursor,
                event.timestamp_ms,
                event.payload,
            );
            envelope
                .validate()
                .map_err(|error| ActorError::InvalidProjection(error.to_string()))?;
            reduce_snapshot(&mut replacement_view.snapshot, &envelope.event)?;
            replacement_view.snapshot.cursor = cursor;
            if authority_rank(replacement_view.snapshot.authority)
                > authority_rank(self.authority_ceiling)
            {
                return Err(ActorError::InvalidProjection(
                    "projected authority exceeds host ceiling".into(),
                ));
            }
            replacement_view
                .snapshot
                .validate()
                .map_err(|error| ActorError::InvalidProjection(error.to_string()))?;
            reduce_summary(
                &mut replacement_view.summary,
                &replacement_view.snapshot,
                &envelope,
            );
            replacement_view
                .summary
                .validate()
                .map_err(|error| ActorError::InvalidProjection(error.to_string()))?;
            replacement_journal.append(envelope.clone())?;
            published.push(envelope);
        }

        self.view = replacement_view;
        self.journal = replacement_journal;
        Ok(published)
    }

    fn cache_command(&mut self, command: SessionCommandEnvelope, ack: CommandAck) {
        let key = (command.device_id.clone(), command.command_id.clone());
        if self.command_cache.len() == self.command_cache_capacity {
            if let Some(oldest) = self.command_order.pop_front() {
                self.command_cache.remove(&oldest);
            }
        }
        let identity = if is_secret_answer(&command.command) {
            CachedCommandIdentity::ConsumedSecret
        } else {
            CachedCommandIdentity::Exact(Box::new(command))
        };
        self.command_order.push_back(key.clone());
        self.command_cache
            .insert(key, CachedCommand { identity, ack });
    }
}

fn is_secret_answer(command: &SessionCommand) -> bool {
    matches!(
        command,
        SessionCommand::AnswerRequest {
            answer: RequestAnswer::Text { .. },
            ..
        }
    )
}

fn authority_rank(authority: AuthorityProfile) -> u8 {
    match authority {
        AuthorityProfile::ReadOnly => 0,
        AuthorityProfile::Workspace => 1,
        AuthorityProfile::FullAccess => 2,
    }
}

fn reduce_summary(summary: &mut SessionSummary, snapshot: &SessionSnapshot, event: &EventEnvelope) {
    summary.live_state = snapshot.live_state;
    summary.model = snapshot.model.clone();
    summary.modified_at_ms = summary.modified_at_ms.max(event.timestamp_ms);
    summary.owner = ActorOwnerState::Hosted;
    summary.attention = match snapshot.live_state {
        SessionLiveState::NeedsApproval => AttentionState::Approval,
        SessionLiveState::NeedsInput => AttentionState::Input,
        SessionLiveState::Done => AttentionState::UnreadCompletion,
        SessionLiveState::Failed => AttentionState::Failure,
        SessionLiveState::Idle
        | SessionLiveState::Working
        | SessionLiveState::Stopped
        | SessionLiveState::Offline
        | SessionLiveState::Locked => AttentionState::None,
    };
    if matches!(event.event, EventPayload::ItemCommitted { .. }) {
        summary.provisional = false;
    }
}

fn reduce_snapshot(snapshot: &mut SessionSnapshot, event: &EventPayload) -> Result<(), ActorError> {
    match event {
        EventPayload::SessionStateChanged {
            state,
            active_run_id,
        } => {
            snapshot.live_state = *state;
            snapshot.active_run_id = active_run_id.clone();
        }
        EventPayload::SessionSettingsChanged { model, authority } => {
            snapshot.model = model.clone();
            snapshot.authority = *authority;
        }
        EventPayload::ItemStarted { item } => {
            if let Some(existing) = snapshot.items.iter().position(|entry| entry.id == item.id) {
                if snapshot.items[existing].lifecycle == ItemLifecycle::Committed {
                    return Err(ActorError::InvalidProjection(
                        "cannot replace a committed item with a provisional item".into(),
                    ));
                }
                snapshot.items[existing] = item.clone();
            } else {
                snapshot.items.push(item.clone());
            }
        }
        EventPayload::ItemDelta { item_id, delta } => {
            let item = snapshot
                .items
                .iter_mut()
                .find(|item| &item.id == item_id)
                .ok_or_else(|| {
                    ActorError::InvalidProjection("delta targets an unknown item".into())
                })?;
            if item.lifecycle != ItemLifecycle::Provisional {
                return Err(ActorError::InvalidProjection(
                    "delta targets a committed item".into(),
                ));
            }
            match (&mut item.payload, delta) {
                (ItemPayload::AssistantMessage { text }, ItemDelta::AssistantText { append })
                | (ItemPayload::Reasoning { text }, ItemDelta::ReasoningText { append }) => {
                    text.push_str(append);
                }
                (
                    ItemPayload::ToolCall {
                        progress,
                        dropped_progress_bytes,
                        ..
                    },
                    ItemDelta::ToolProgress {
                        text,
                        dropped_bytes,
                    },
                ) => {
                    *progress = Some(text.clone());
                    *dropped_progress_bytes = *dropped_bytes;
                }
                _ => {
                    return Err(ActorError::InvalidProjection(
                        "delta type does not match its item payload".into(),
                    ));
                }
            }
            item.validate()
                .map_err(|error| ActorError::InvalidProjection(error.to_string()))?;
        }
        EventPayload::ItemCommitted { item } => {
            if let Some(existing) = snapshot.items.iter().position(|entry| entry.id == item.id) {
                snapshot.items[existing] = item.clone();
            } else {
                snapshot.items.push(item.clone());
            }
            snapshot.durable_head = item.durable_entry_id.clone();
        }
        EventPayload::ItemRetracted {
            item_id,
            provider_attempt,
            ..
        } => {
            let position = snapshot
                .items
                .iter()
                .position(|item| {
                    &item.id == item_id
                        && item.lifecycle == ItemLifecycle::Provisional
                        && item.provider_attempt == Some(*provider_attempt)
                })
                .ok_or_else(|| {
                    ActorError::InvalidProjection(
                        "retraction targets an unknown provisional attempt".into(),
                    )
                })?;
            snapshot.items.remove(position);
        }
        EventPayload::PendingRequestChanged { request } => {
            if request.actor_generation != snapshot.actor_generation {
                return Err(ActorError::InvalidProjection(
                    "request generation does not match its session actor".into(),
                ));
            }
            let position = snapshot
                .pending_requests
                .iter()
                .position(|pending| pending.id == request.id);
            if request.state == RequestState::Pending {
                if let Some(position) = position {
                    snapshot.pending_requests[position] = request.clone();
                } else {
                    snapshot.pending_requests.push(request.clone());
                }
            } else if let Some(position) = position {
                snapshot.pending_requests.remove(position);
            }
        }
        EventPayload::SourceUpserted { source } => {
            if let Some(existing) = snapshot
                .sources
                .iter()
                .position(|candidate| candidate.id == source.id)
            {
                snapshot.sources[existing] = source.clone();
            } else {
                snapshot.sources.push(source.clone());
            }
        }
        EventPayload::ArtifactUpserted { artifact } => {
            if let Some(existing) = snapshot
                .artifacts
                .iter()
                .position(|candidate| candidate.id == artifact.id)
            {
                snapshot.artifacts[existing] = artifact.clone();
            } else {
                snapshot.artifacts.push(artifact.clone());
            }
        }
        EventPayload::UsageUpdated { usage } => {
            snapshot.context.usage = usage.clone();
        }
    }
    Ok(())
}

enum ActorMessage {
    Command {
        envelope: SessionCommandEnvelope,
        acknowledged_at_ms: u64,
        response: oneshot::Sender<CommandAdmission>,
    },
    Replay {
        after: SessionCursor,
        response: oneshot::Sender<ReplayResponse>,
    },
}

/// Handle to one serialized session actor.
#[derive(Clone)]
pub struct SessionActorHandle {
    session_id: crate::SessionId,
    sender: mpsc::Sender<ActorMessage>,
    view: watch::Receiver<Arc<ActorView>>,
}

impl SessionActorHandle {
    /// Session identity.
    pub fn session_id(&self) -> &crate::SessionId {
        &self.session_id
    }

    /// Latest observable view.
    pub fn view(&self) -> ActorView {
        self.view.borrow().as_ref().clone()
    }

    /// Subscribes to complete view replacements.
    pub fn subscribe(&self) -> watch::Receiver<Arc<ActorView>> {
        self.view.clone()
    }

    /// Routes one command to the exclusive driver.
    pub async fn command(
        &self,
        envelope: SessionCommandEnvelope,
        acknowledged_at_ms: u64,
    ) -> Result<CommandAdmission, ActorError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(ActorMessage::Command {
                envelope,
                acknowledged_at_ms,
                response,
            })
            .await
            .map_err(|_| ActorError::Closed)?;
        receiver.await.map_err(|_| ActorError::Closed)
    }

    /// Requests retained replay or a complete snapshot gap fallback.
    pub async fn replay_after(&self, after: SessionCursor) -> Result<ReplayResponse, ActorError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(ActorMessage::Replay { after, response })
            .await
            .map_err(|_| ActorError::Closed)?;
        receiver.await.map_err(|_| ActorError::Closed)
    }
}

/// Spawns serialized ownership for one concrete [`SessionDriver`].
pub struct SessionActor;

impl SessionActor {
    /// Spawns the actor and returns its handle.
    pub fn spawn<D: SessionDriver>(
        host_id: HostId,
        mut driver: D,
        config: ActorConfig,
    ) -> Result<SessionActorHandle, ActorError> {
        let seed = driver.seed();
        let core = SessionActorCore::new(host_id, seed, config)?;
        let session_id = core.session_id().clone();
        let (sender, mut receiver) = mpsc::channel(config.mailbox_capacity);
        let (view_sender, view) = watch::channel(Arc::new(core.view()));

        tokio::spawn(async move {
            let mut core = core;
            let mut driver_events_open = true;

            loop {
                enum Input {
                    Message(Option<ActorMessage>),
                    DriverEvent(Option<TimestampedEvent>),
                }

                let input = if driver_events_open {
                    tokio::select! {
                        message = receiver.recv() => Input::Message(message),
                        event = driver.next_event() => Input::DriverEvent(event),
                    }
                } else {
                    Input::Message(receiver.recv().await)
                };

                match input {
                    Input::Message(Some(ActorMessage::Command {
                        envelope,
                        acknowledged_at_ms,
                        response,
                    })) => {
                        let admission = core
                            .admit_command(envelope, acknowledged_at_ms, |command| {
                                driver.dispatch(command)
                            })
                            .await;
                        let _ = response.send(admission);
                        let _ = view_sender.send(Arc::new(core.view()));
                    }
                    Input::Message(Some(ActorMessage::Replay { after, response })) => {
                        let _ = response.send(core.replay_after(after));
                    }
                    Input::Message(None) => break,
                    Input::DriverEvent(Some(event)) => {
                        if core.publish(event).is_ok() {
                            let _ = view_sender.send(Arc::new(core.view()));
                        }
                    }
                    Input::DriverEvent(None) => {
                        driver_events_open = false;
                    }
                }
            }
        });

        Ok(SessionActorHandle {
            session_id,
            sender,
            view,
        })
    }
}

impl From<ValidationError> for ActorError {
    fn from(error: ValidationError) -> Self {
        Self::InvalidProjection(error.to_string())
    }
}

impl From<ActorError> for SanitizedError {
    fn from(error: ActorError) -> Self {
        match error {
            ActorError::Closed => SanitizedError::public(
                ErrorCode::Unavailable,
                "The session owner is no longer available.",
            )
            .with_retryable(true),
            ActorError::InvalidConfiguration(_)
            | ActorError::InvalidSeed
            | ActorError::InvalidProjection(_)
            | ActorError::Journal(_)
            | ActorError::SequenceExhausted => SanitizedError::internal(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AckDisposition, ContextUsage, DurableEntryId, ItemId, ModelSelection, RequestId, SessionId,
        SessionItem, SessionLiveState,
    };

    use super::*;

    fn seed() -> SessionSeed {
        let id = SessionId::new("session-actor").unwrap();
        let model = ModelSelection {
            provider: "test".into(),
            model: "model".into(),
            reasoning: "off".into(),
        };
        SessionSeed {
            summary: SessionSummary {
                id: id.clone(),
                project_id: None,
                title: "Fresh session".into(),
                tags: Vec::new(),
                created_at_ms: 0,
                modified_at_ms: 0,
                pinned: false,
                archived: false,
                provisional: true,
                live_state: SessionLiveState::Idle,
                attention: AttentionState::None,
                owner: ActorOwnerState::Hosted,
                model: model.clone(),
            },
            snapshot: SessionSnapshot {
                session_id: id,
                actor_generation: 1,
                cursor: SessionCursor::zero(1),
                durable_head: None,
                live_state: SessionLiveState::Idle,
                active_run_id: None,
                model,
                authority: AuthorityProfile::FullAccess,
                context: ContextUsage::default(),
                items: Vec::new(),
                pending_requests: Vec::new(),
                sources: Vec::new(),
                artifacts: Vec::new(),
            },
        }
    }

    #[test]
    fn committed_items_require_and_retain_exact_durable_identity() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            seed(),
            ActorConfig::default(),
        )
        .unwrap();
        let item = SessionItem {
            id: ItemId::new("item-1").unwrap(),
            run_id: None,
            turn_id: None,
            provider_attempt: None,
            lifecycle: ItemLifecycle::Committed,
            durable_entry_id: Some(DurableEntryId::new("entry-1").unwrap()),
            payload: ItemPayload::AssistantMessage {
                text: "done".into(),
            },
        };
        core.publish(TimestampedEvent::new(
            1,
            EventPayload::ItemCommitted { item },
        ))
        .unwrap();
        assert_eq!(
            core.snapshot().durable_head,
            Some(DurableEntryId::new("entry-1").unwrap())
        );
        assert!(!core.view().summary.provisional);
    }

    #[test]
    fn authority_cannot_exceed_host_ceiling() {
        let config = ActorConfig {
            authority_ceiling: AuthorityProfile::Workspace,
            ..ActorConfig::default()
        };
        assert!(SessionActorCore::new(HostId::new("host-test").unwrap(), seed(), config).is_err());
    }

    #[test]
    fn ack_disposition_is_publicly_inspectable() {
        let ack = CommandAck::rejected(
            SessionId::new("session-actor").unwrap(),
            CommandId::new("command-1").unwrap(),
            1,
            SessionCursor::zero(1),
            SanitizedError::public(ErrorCode::InvalidBoundary, "idle only"),
        );
        assert!(matches!(ack.disposition, AckDisposition::Rejected { .. }));
    }

    #[tokio::test]
    async fn free_form_request_answers_are_consumed_without_secret_retention() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            seed(),
            ActorConfig::default(),
        )
        .unwrap();
        let make_command = |text: &str| {
            SessionCommandEnvelope::new(
                HostId::new("host-test").unwrap(),
                DeviceId::new("device-test").unwrap(),
                SessionId::new("session-actor").unwrap(),
                CommandId::new("command-secret").unwrap(),
                1,
                Some(1),
                SessionCommand::AnswerRequest {
                    request_id: RequestId::new("request-secret").unwrap(),
                    answer: RequestAnswer::Text { text: text.into() },
                },
            )
        };

        let first = core
            .admit_command(make_command("first secret"), 10, |_| async {
                Ok(DriverCommandOutcome::default())
            })
            .await;
        let key = (
            DeviceId::new("device-test").unwrap(),
            CommandId::new("command-secret").unwrap(),
        );
        assert!(matches!(
            core.command_cache.get(&key).map(|entry| &entry.identity),
            Some(CachedCommandIdentity::ConsumedSecret)
        ));

        let retransmit = core
            .admit_command(
                make_command("different retransmitted body"),
                99,
                |_| async { panic!("a consumed device-scoped command must never dispatch twice") },
            )
            .await;
        assert!(retransmit.cached);
        assert_eq!(first.ack, retransmit.ack);
    }
}
