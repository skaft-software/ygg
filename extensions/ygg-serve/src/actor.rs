//! Exclusive per-session actor, snapshot reducer, replay, and idempotency.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::timeout;

use crate::{
    ActorOwnerState, AttentionState, AuthorityProfile, CommandAck, CommandId, DeviceId,
    DriverCommandOutcome, ErrorCode, EventEnvelope, EventJournal, EventPayload, FinalizeCompletion,
    FinalizeDecision, HostId, ItemDelta, ItemLifecycle, ItemPayload, JournalConfig, JournalError,
    ProtocolValidation, ReplayResponse, RequestAnswer, RequestState, SanitizedError, ServiceError,
    SessionCommand, SessionCommandEnvelope, SessionCursor, SessionDriver, SessionLiveState,
    SessionSeed, SessionSnapshot, SessionSummary, TimestampedEvent, ValidationError,
    MAX_DRIVER_OUTCOME_EVENTS,
};

const MAX_COMMAND_CACHE_CAPACITY: usize = 65_536;
const MAX_MAILBOX_CAPACITY: usize = 4_096;
const EVENT_BROADCAST_CAPACITY: usize = 2_048;
const MAX_BRANCH_GRAPH_ENTRIES: usize = 2_048;
const MAX_FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Maximum wait for a driver's durable/App finalization confirmation.
    pub finalize_timeout: Duration,
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            journal: JournalConfig::default(),
            command_cache_capacity: 2_048,
            mailbox_capacity: 64,
            authority_ceiling: AuthorityProfile::FullAccess,
            finalize_timeout: MAX_FINALIZE_TIMEOUT,
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

enum OutcomePublicationError {
    Rejected,
    Fatal(ActorError),
}

#[derive(Clone, PartialEq, Eq)]
enum CachedCommandIdentity {
    Exact(Box<SessionCommandEnvelope>),
    /// Free-form one-shot answer body retained only as a non-reversible
    /// digest, alongside its nonsecret command shape.
    ConsumedSecret(ConsumedSecretIdentity),
}

#[derive(Clone, PartialEq, Eq)]
struct ConsumedSecretIdentity {
    protocol: u16,
    host_id: HostId,
    session_id: crate::SessionId,
    issued_at_ms: u64,
    expected_actor_generation: Option<u64>,
    request_id: crate::RequestId,
    answer_digest: [u8; 32],
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
    finalize_timeout: Duration,
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
        if config.finalize_timeout.is_zero() || config.finalize_timeout > MAX_FINALIZE_TIMEOUT {
            return Err(ActorError::InvalidConfiguration(format!(
                "finalize_timeout must be greater than zero and at most {MAX_FINALIZE_TIMEOUT:?}"
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
            finalize_timeout: config.finalize_timeout,
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
    ) -> Result<CommandAdmission, ActorError>
    where
        F: FnOnce(SessionCommand) -> Fut,
        Fut: Future<Output = Result<DriverCommandOutcome, ServiceError>>,
    {
        let cache_key = (command.device_id.clone(), command.command_id.clone());
        if let Some(cached) = self.command_cache.get(&cache_key) {
            let is_duplicate = match &cached.identity {
                CachedCommandIdentity::Exact(original) => original.as_ref() == &command,
                CachedCommandIdentity::ConsumedSecret(original) => {
                    secret_answer_identity(&command).as_ref() == Some(original)
                }
            };
            if is_duplicate {
                return Ok(CommandAdmission {
                    ack: cached.ack.clone(),
                    cached: true,
                    published: Vec::new(),
                });
            }
            return Ok(CommandAdmission {
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
            });
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
            return Ok(CommandAdmission {
                ack,
                cached: false,
                published: Vec::new(),
            });
        }

        let result = dispatch(command.command.clone()).await;
        let (ack, published) = match result {
            Ok(outcome) => {
                let run_id = outcome.run_id.clone();
                let created_session_id = outcome.created_session_id.clone();
                match self
                    .publish_driver_outcome(outcome, acknowledged_at_ms)
                    .await
                {
                    Ok(published) => {
                        let ack = match created_session_id {
                            Some(created_session_id) => CommandAck::accepted_fork(
                                self.session_id.clone(),
                                command.command_id.clone(),
                                acknowledged_at_ms,
                                self.view.snapshot.cursor,
                                created_session_id,
                            ),
                            None => CommandAck::accepted(
                                self.session_id.clone(),
                                command.command_id.clone(),
                                acknowledged_at_ms,
                                self.view.snapshot.cursor,
                                run_id,
                            ),
                        };
                        (ack, published)
                    }
                    Err(OutcomePublicationError::Rejected) => (
                        CommandAck::rejected(
                            self.session_id.clone(),
                            command.command_id.clone(),
                            acknowledged_at_ms,
                            self.view.snapshot.cursor,
                            SanitizedError::internal(),
                        ),
                        Vec::new(),
                    ),
                    Err(OutcomePublicationError::Fatal(error)) => {
                        return Err(error);
                    }
                }
            }
            Err(ServiceError::OwnerLost) => return Err(ActorError::Closed),
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
        Ok(CommandAdmission {
            ack,
            cached: false,
            published,
        })
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
        if matches!(
            command.command,
            SessionCommand::Checkout { .. }
                | SessionCommand::EditUserTurn { .. }
                | SessionCommand::RetryResponse { .. }
                | SessionCommand::ForkConversation { .. }
        )
            && (self.view.snapshot.active_run_id.is_some()
                || !self.view.snapshot.pending_requests.is_empty()
                || !matches!(
                    self.view.snapshot.live_state,
                    SessionLiveState::Idle
                        | SessionLiveState::Done
                        | SessionLiveState::Failed
                        | SessionLiveState::Stopped
                ))
        {
            return Err(SanitizedError::public(
                ErrorCode::InvalidBoundary,
                "A session branch can only be checked out after current work finishes.",
            ));
        }
        if let SessionCommand::Checkout { entry_id } = &command.command {
            let checkoutable = self
                .view
                .snapshot
                .branches
                .entries
                .iter()
                .any(|entry| &entry.entry_id == entry_id && entry.checkoutable);
            if !checkoutable {
                return Err(SanitizedError::public(
                    ErrorCode::InvalidBoundary,
                    "That session checkpoint is not available for checkout.",
                ));
            }
        }
        let required_kind = match &command.command {
            SessionCommand::EditUserTurn {
                source_user_entry_id,
                ..
            } => Some((source_user_entry_id, crate::SessionBranchEntryKind::UserMessage)),
            SessionCommand::RetryResponse {
                source_assistant_entry_id,
                ..
            } => Some((
                source_assistant_entry_id,
                crate::SessionBranchEntryKind::AssistantMessage,
            )),
            SessionCommand::ForkConversation { entry_id } => {
                let available = self
                    .view
                    .snapshot
                    .branches
                    .entries
                    .iter()
                    .any(|entry| &entry.entry_id == entry_id && entry.checkoutable);
                if !available {
                    return Err(SanitizedError::public(
                        ErrorCode::InvalidBoundary,
                        "That committed checkpoint is not available to fork.",
                    ));
                }
                None
            }
            _ => None,
        };
        if let Some((entry_id, kind)) = required_kind {
            let available = self
                .view
                .snapshot
                .branches
                .entries
                .iter()
                .any(|entry| &entry.entry_id == entry_id && entry.kind == kind);
            if !available {
                return Err(SanitizedError::public(
                    ErrorCode::InvalidBoundary,
                    "The selected conversation entry is not available for this operation.",
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

    async fn publish_driver_outcome(
        &mut self,
        mut outcome: DriverCommandOutcome,
        timestamp_ms: u64,
    ) -> Result<Vec<EventEnvelope>, OutcomePublicationError> {
        let Some(mut replacement) = outcome.replacement.take() else {
            if outcome.events.len() > MAX_DRIVER_OUTCOME_EVENTS {
                return Err(OutcomePublicationError::Rejected);
            }
            return self
                .publish_batch(outcome.events)
                .map_err(|_| OutcomePublicationError::Rejected);
        };
        let prepared = (|| -> Result<(SessionSeed, EventJournal, EventEnvelope), ActorError> {
            if !outcome.events.is_empty()
                || outcome.run_id.is_some()
                || outcome.created_session_id.is_some()
                || outcome.events.len() > MAX_DRIVER_OUTCOME_EVENTS
            {
                return Err(ActorError::InvalidProjection(
                    "projection replacement must be the complete driver outcome".into(),
                ));
            }
            let mut seed = replacement.seed.clone();
            seed.validate().map_err(|_| ActorError::InvalidSeed)?;
            if seed.snapshot.session_id != self.session_id
                || seed.snapshot.actor_generation != self.generation
            {
                return Err(ActorError::InvalidProjection(
                    "replacement projection identity changed".into(),
                ));
            }
            let cursor = self
                .view
                .snapshot
                .cursor
                .checked_next()
                .ok_or(ActorError::SequenceExhausted)?;
            seed.snapshot.cursor = cursor;
            seed.summary.owner = ActorOwnerState::Hosted;
            seed.summary.live_state = seed.snapshot.live_state;
            seed.summary.model = seed.snapshot.model.clone();
            seed.validate().map_err(|_| ActorError::InvalidSeed)?;
            if authority_rank(seed.snapshot.authority) > authority_rank(self.authority_ceiling) {
                return Err(ActorError::InvalidProjection(
                    "replacement authority exceeds host ceiling".into(),
                ));
            }
            let event = EventEnvelope::new(
                self.session_id.clone(),
                cursor,
                timestamp_ms,
                EventPayload::SessionProjectionReplaced {
                    durable_entry_id: seed.snapshot.durable_head.clone(),
                },
            );
            event
                .validate()
                .map_err(|error| ActorError::InvalidProjection(error.to_string()))?;
            let mut journal = self.journal.clone();
            journal.append(event.clone())?;
            Ok((seed, journal, event))
        })();
        let (seed, journal, event) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finalize_replacement(
                    &mut replacement,
                    FinalizeDecision::Rollback,
                    FinalizeCompletion::RolledBack,
                )
                .await
                .map_err(OutcomePublicationError::Fatal)?;
                let _ = error;
                return Err(OutcomePublicationError::Rejected);
            }
        };
        self.finalize_replacement(
            &mut replacement,
            FinalizeDecision::Commit,
            FinalizeCompletion::Committed,
        )
        .await
        .map_err(OutcomePublicationError::Fatal)?;
        self.view = ActorView {
            summary: seed.summary,
            snapshot: seed.snapshot,
        };
        self.journal = journal;
        Ok(vec![event])
    }

    async fn finalize_replacement(
        &self,
        replacement: &mut crate::service::ProjectionReplacement,
        decision: FinalizeDecision,
        expected: FinalizeCompletion,
    ) -> Result<(), ActorError> {
        let completion = replacement
            .begin_finalize(decision)
            .map_err(|_| ActorError::Closed)?;
        let completion = timeout(self.finalize_timeout, completion)
            .await
            .map_err(|_| ActorError::Closed)?
            .map_err(|_| ActorError::Closed)?;
        match completion {
            Ok(actual) if actual == expected => Ok(()),
            Ok(_) | Err(_) => Err(ActorError::Closed),
        }
    }

    fn cache_command(&mut self, command: SessionCommandEnvelope, ack: CommandAck) {
        let key = (command.device_id.clone(), command.command_id.clone());
        if self.command_cache.len() == self.command_cache_capacity {
            if let Some(oldest) = self.command_order.pop_front() {
                self.command_cache.remove(&oldest);
            }
        }
        let identity = secret_answer_identity(&command)
            .map(CachedCommandIdentity::ConsumedSecret)
            .unwrap_or_else(|| CachedCommandIdentity::Exact(Box::new(command)));
        self.command_order.push_back(key.clone());
        self.command_cache
            .insert(key, CachedCommand { identity, ack });
    }
}

fn secret_answer_identity(command: &SessionCommandEnvelope) -> Option<ConsumedSecretIdentity> {
    let SessionCommand::AnswerRequest {
        request_id,
        answer: RequestAnswer::Text { text },
    } = &command.command
    else {
        return None;
    };
    let answer_digest = Sha256::digest(text.as_bytes()).into();
    Some(ConsumedSecretIdentity {
        protocol: command.protocol,
        host_id: command.host_id.clone(),
        session_id: command.session_id.clone(),
        issued_at_ms: command.issued_at_ms,
        expected_actor_generation: command.expected_actor_generation,
        request_id: request_id.clone(),
        answer_digest,
    })
}

fn authority_rank(authority: AuthorityProfile) -> u8 {
    match authority {
        AuthorityProfile::ReadOnly => 0,
        AuthorityProfile::Workspace => 1,
        AuthorityProfile::FullAccess => 2,
    }
}

fn reduce_summary(summary: &mut SessionSummary, snapshot: &SessionSnapshot, event: &EventEnvelope) {
    if let EventPayload::SessionMetadataChanged {
        title,
        pinned,
        archived,
    } = &event.event
    {
        if let Some(title) = title {
            summary.title = title.clone();
        }
        if let Some(pinned) = pinned {
            summary.pinned = *pinned;
        }
        if let Some(archived) = archived {
            summary.archived = *archived;
            summary.lifecycle = if *archived {
                crate::SessionCatalogState::Archived
            } else {
                crate::SessionCatalogState::Active
            };
            summary.retention = None;
        }
    }
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
        EventPayload::SessionMetadataChanged { .. } => {}
        EventPayload::SessionDurableHeadChanged { durable_entry_id } => {
            if durable_entry_id.as_ref().is_some_and(|head| {
                !snapshot
                    .branches
                    .entries
                    .iter()
                    .any(|entry| &entry.entry_id == head)
            }) {
                return Err(ActorError::InvalidProjection(
                    "durable head is missing from the branch graph".into(),
                ));
            }
            snapshot.durable_head = durable_entry_id.clone();
            snapshot.branches.head = durable_entry_id.clone();
        }
        EventPayload::SessionBranchEntriesAppended { entries } => {
            snapshot.branches.entries.extend(entries.iter().cloned());
            if snapshot.branches.entries.len() > MAX_BRANCH_GRAPH_ENTRIES {
                let selected_head = snapshot.branches.head.as_ref().and_then(|head| {
                    snapshot
                        .branches
                        .entries
                        .iter()
                        .find(|entry| &entry.entry_id == head)
                        .cloned()
                });
                let split_at = snapshot
                    .branches
                    .entries
                    .len()
                    .saturating_sub(MAX_BRANCH_GRAPH_ENTRIES);
                let mut recent = snapshot.branches.entries.split_off(split_at);
                if let Some(selected_head) = selected_head {
                    if !recent
                        .iter()
                        .any(|entry| entry.entry_id == selected_head.entry_id)
                    {
                        recent.remove(0);
                        recent.insert(0, selected_head);
                    }
                }
                snapshot.branches.entries = recent;
                snapshot.branches.truncated = true;
            }
        }
        EventPayload::SessionProjectionReplaced { .. } => {}
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
                (ItemPayload::ToolCall(current), ItemDelta::ToolActivity { activity }) => {
                    *current = activity.clone()
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
    Retire,
}

/// Handle to one serialized session actor.
#[derive(Clone)]
pub struct SessionActorHandle {
    session_id: crate::SessionId,
    sender: mpsc::Sender<ActorMessage>,
    view: watch::Receiver<Arc<ActorView>>,
    events: broadcast::Sender<EventEnvelope>,
    quiesced: watch::Receiver<bool>,
}

impl SessionActorHandle {
    /// Session identity.
    pub fn session_id(&self) -> &crate::SessionId {
        &self.session_id
    }

    /// Whether the serialized actor task can no longer receive work.
    pub(crate) fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// Whether two handles address the exact same actor channel.
    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    /// Waits until this exact actor channel is closed.
    pub(crate) async fn closed(&self) {
        self.sender.closed().await;
    }

    /// Whether the closed actor's driver and detached durable writer settled.
    pub(crate) fn is_quiesced(&self) -> bool {
        *self.quiesced.borrow()
    }

    /// Waits for the driver to quiesce after the actor mailbox closes.
    pub(crate) async fn quiesced(&self) {
        let mut quiesced = self.quiesced.clone();
        while !*quiesced.borrow_and_update() {
            if quiesced.changed().await.is_err() {
                // A task failure without an explicit settled signal is not
                // proof that a detached durable writer stopped. Keep the
                // ownership fence closed.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Latest observable view.
    pub fn view(&self) -> ActorView {
        self.view.borrow().as_ref().clone()
    }

    /// Subscribes to complete view replacements.
    pub fn subscribe(&self) -> watch::Receiver<Arc<ActorView>> {
        self.view.clone()
    }

    /// Subscribes to newly published typed events.
    ///
    /// Lag is explicit in [`broadcast::error::RecvError::Lagged`]; clients
    /// recover through the cursor-bound replay endpoint.
    pub fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events.subscribe()
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

    /// Fences new work and asks this exact owner to stop and quiesce.
    pub(crate) async fn retire(&self) {
        let _ = self.sender.send(ActorMessage::Retire).await;
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
        let (event_sender, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let task_event_sender = event_sender.clone();
        let (quiesced_sender, quiesced) = watch::channel(false);

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
                        let Ok(admission) = admission else {
                            break;
                        };
                        for event in &admission.published {
                            let _ = task_event_sender.send(event.clone());
                        }
                        let _ = response.send(admission);
                        let _ = view_sender.send(Arc::new(core.view()));
                    }
                    Input::Message(Some(ActorMessage::Replay { after, response })) => {
                        let _ = response.send(core.replay_after(after));
                    }
                    Input::Message(Some(ActorMessage::Retire)) => break,
                    Input::Message(None) => break,
                    Input::DriverEvent(Some(event)) => {
                        if let Ok(event) = core.publish(event) {
                            let _ = task_event_sender.send(event);
                            let _ = view_sender.send(Arc::new(core.view()));
                        }
                    }
                    Input::DriverEvent(None) => {
                        driver_events_open = false;
                    }
                }
            }
            receiver.close();
            drop(receiver);
            driver.shutdown().await;
            drop(driver);
            let _ = quiesced_sender.send(true);
        });

        Ok(SessionActorHandle {
            session_id,
            sender,
            view,
            events: event_sender,
            quiesced,
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
        AckDisposition, ActivityPhase, ContextUsage, DurableEntryId, ItemId, ModelSelection,
        PendingRequest, PromptInput, RequestId, RequestKind, RequestState, RunId, SessionBranchEntry,
        SessionBranchEntryKind, SessionBranchGraph, SessionId, SessionItem, SessionLiveState,
        ToolActivity, ToolActivityStatus, ToolKind, TurnId,
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
                lifecycle: crate::SessionCatalogState::Active,
                retention: None,
                forked_from: None,
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
                branches: crate::SessionBranchGraph::default(),
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

    fn committed_message(item_id: &str, entry_id: &str, text: &str) -> SessionItem {
        SessionItem {
            id: ItemId::new(item_id).unwrap(),
            run_id: None,
            turn_id: None,
            provider_attempt: None,
            lifecycle: ItemLifecycle::Committed,
            durable_entry_id: Some(DurableEntryId::new(entry_id).unwrap()),
            payload: ItemPayload::AssistantMessage { text: text.into() },
        }
    }

    fn checkout_seed() -> SessionSeed {
        let mut seed = seed();
        let root = DurableEntryId::new("entry-root").unwrap();
        let old_head = DurableEntryId::new("entry-old-head").unwrap();
        seed.summary.provisional = false;
        seed.snapshot.durable_head = Some(old_head.clone());
        seed.snapshot.branches = SessionBranchGraph {
            head: Some(old_head.clone()),
            entries: vec![
                SessionBranchEntry {
                    entry_id: root.clone(),
                    parent_entry_id: None,
                    kind: SessionBranchEntryKind::UserMessage,
                    checkoutable: true,
                    label: "Root prompt".into(),
                },
                SessionBranchEntry {
                    entry_id: old_head,
                    parent_entry_id: Some(root),
                    kind: SessionBranchEntryKind::AssistantMessage,
                    checkoutable: true,
                    label: "Old answer".into(),
                },
            ],
            truncated: false,
        };
        seed.snapshot.items = vec![committed_message(
            "item-old-answer",
            "entry-old-head",
            "Old answer",
        )];
        seed
    }

    #[derive(Clone, Copy)]
    enum FinalizerBehavior {
        Mismatch,
        Error,
        Cancel,
        Timeout,
        OwnerLost,
    }

    struct FinalizingDriver {
        seed: SessionSeed,
        behavior: FinalizerBehavior,
    }

    #[async_trait::async_trait]
    impl SessionDriver for FinalizingDriver {
        fn seed(&self) -> SessionSeed {
            self.seed.clone()
        }

        async fn dispatch(
            &mut self,
            command: SessionCommand,
        ) -> Result<DriverCommandOutcome, ServiceError> {
            assert!(matches!(command, SessionCommand::Checkout { .. }));
            if matches!(self.behavior, FinalizerBehavior::OwnerLost) {
                return Err(ServiceError::OwnerLost);
            }
            let mut replacement = self.seed.clone();
            let root = DurableEntryId::new("entry-root").unwrap();
            replacement.snapshot.durable_head = Some(root.clone());
            replacement.snapshot.branches.head = Some(root.clone());
            replacement.snapshot.items =
                vec![committed_message("item-root", "entry-root", "Root prompt")];
            let (outcome, mut finalizer) = DriverCommandOutcome::guarded_replace(replacement);
            let behavior = self.behavior;
            tokio::spawn(async move {
                if matches!(behavior, FinalizerBehavior::Cancel) {
                    return;
                }
                let decision = finalizer.decision().await.unwrap();
                assert_eq!(decision, FinalizeDecision::Commit);
                match behavior {
                    FinalizerBehavior::Mismatch => {
                        let _ = finalizer.complete(Ok(FinalizeCompletion::RolledBack));
                    }
                    FinalizerBehavior::Error => {
                        let _ = finalizer.complete(Err(ServiceError::Internal));
                    }
                    FinalizerBehavior::Timeout => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let _ = finalizer.complete(Ok(FinalizeCompletion::Committed));
                    }
                    FinalizerBehavior::Cancel | FinalizerBehavior::OwnerLost => {
                        unreachable!()
                    }
                }
            });
            Ok(outcome)
        }

        async fn next_event(&mut self) -> Option<TimestampedEvent> {
            None
        }
    }

    fn checkout_command(command_id: &str) -> SessionCommandEnvelope {
        SessionCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            SessionId::new("session-actor").unwrap(),
            CommandId::new(command_id).unwrap(),
            1,
            Some(1),
            SessionCommand::Checkout {
                entry_id: DurableEntryId::new("entry-root").unwrap(),
            },
        )
    }

    fn branch_command(command_id: &str, command: SessionCommand) -> SessionCommandEnvelope {
        SessionCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            SessionId::new("session-actor").unwrap(),
            CommandId::new(command_id).unwrap(),
            1,
            Some(1),
            command,
        )
    }

    #[test]
    fn committed_items_and_branch_head_retain_exact_durable_identity() {
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
        let entry_id = DurableEntryId::new("entry-1").unwrap();
        core.publish(TimestampedEvent::new(
            2,
            EventPayload::SessionBranchEntriesAppended {
                entries: vec![crate::SessionBranchEntry {
                    entry_id: entry_id.clone(),
                    parent_entry_id: None,
                    kind: crate::SessionBranchEntryKind::AssistantMessage,
                    checkoutable: true,
                    label: "done".into(),
                }],
            },
        ))
        .unwrap();
        core.publish(TimestampedEvent::new(
            3,
            EventPayload::SessionDurableHeadChanged {
                durable_entry_id: Some(entry_id),
            },
        ))
        .unwrap();
        assert_eq!(
            core.snapshot().durable_head,
            Some(DurableEntryId::new("entry-1").unwrap())
        );
        assert!(!core.view().summary.provisional);
    }

    #[test]
    fn session_metadata_event_updates_only_the_catalog_projection() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            seed(),
            ActorConfig::default(),
        )
        .unwrap();
        let original_snapshot = core.snapshot();

        core.publish(TimestampedEvent::new(
            7,
            EventPayload::SessionMetadataChanged {
                title: Some("Renamed session".into()),
                pinned: Some(true),
                archived: Some(true),
            },
        ))
        .unwrap();

        let view = core.view();
        assert_eq!(view.summary.title, "Renamed session");
        assert!(view.summary.pinned);
        assert!(view.summary.archived);
        assert_eq!(view.snapshot.durable_head, original_snapshot.durable_head);
        assert_eq!(view.snapshot.items, original_snapshot.items);
        assert_eq!(view.snapshot.cursor.sequence, 1);
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
    async fn checkout_atomically_replaces_projection_and_advances_once() {
        let initial = checkout_seed();
        let mut replacement = initial.clone();
        let root = DurableEntryId::new("entry-root").unwrap();
        replacement.snapshot.durable_head = Some(root.clone());
        replacement.snapshot.branches.head = Some(root.clone());
        replacement.snapshot.items =
            vec![committed_message("item-root", "entry-root", "Root prompt")];
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            initial,
            ActorConfig::default(),
        )
        .unwrap();
        let command = checkout_command("command-checkout");
        let duplicate_command = command.clone();

        let admission = core
            .admit_command(command, 50, move |command| {
                let replacement = replacement.clone();
                let root = root.clone();
                async move {
                    assert_eq!(command, SessionCommand::Checkout { entry_id: root });
                    let (outcome, mut finalizer) =
                        DriverCommandOutcome::guarded_replace(replacement);
                    tokio::spawn(async move {
                        assert_eq!(
                            finalizer.decision().await.unwrap(),
                            FinalizeDecision::Commit
                        );
                        finalizer
                            .complete(Ok(FinalizeCompletion::Committed))
                            .unwrap();
                    });
                    Ok(outcome)
                }
            })
            .await
            .unwrap();

        assert!(matches!(
            admission.ack.disposition,
            AckDisposition::Accepted { .. }
        ));
        assert_eq!(admission.published.len(), 1);
        assert!(matches!(
            admission.published[0].event,
            EventPayload::SessionProjectionReplaced { .. }
        ));
        assert_eq!(core.snapshot().cursor.sequence, 1);
        assert_eq!(
            core.snapshot().durable_head,
            Some(DurableEntryId::new("entry-root").unwrap())
        );
        assert_eq!(
            core.snapshot()
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["item-root"]
        );
        assert_eq!(core.snapshot().branches.entries.len(), 2);

        let duplicate = core
            .admit_command(duplicate_command, 99, |_| async {
                panic!("an accepted checkout command must finalize exactly once")
            })
            .await
            .unwrap();
        assert!(duplicate.cached);
        assert_eq!(duplicate.ack, admission.ack);
        assert!(duplicate.published.is_empty());
        assert_eq!(core.snapshot().cursor.sequence, 1);
    }

    #[tokio::test]
    async fn invalid_finalizer_completion_closes_the_actor_without_an_ack() {
        for (index, behavior) in [
            FinalizerBehavior::Mismatch,
            FinalizerBehavior::Error,
            FinalizerBehavior::Cancel,
            FinalizerBehavior::Timeout,
            FinalizerBehavior::OwnerLost,
        ]
        .into_iter()
        .enumerate()
        {
            let config = ActorConfig {
                finalize_timeout: Duration::from_millis(20),
                ..ActorConfig::default()
            };
            let handle = SessionActor::spawn(
                HostId::new("host-test").unwrap(),
                FinalizingDriver {
                    seed: checkout_seed(),
                    behavior,
                },
                config,
            )
            .unwrap();

            assert!(matches!(
                handle
                    .command(
                        checkout_command(&format!("command-fatal-finalizer-{index}")),
                        50,
                    )
                    .await,
                Err(ActorError::Closed)
            ));
            tokio::time::timeout(Duration::from_secs(1), handle.closed())
                .await
                .expect("fatal finalizer result must close the owner");
            assert!(handle.is_closed());
        }
    }

    #[tokio::test]
    async fn failed_actor_publication_rejects_the_guarded_driver_checkout() {
        let initial = checkout_seed();
        let before = initial.snapshot.clone();
        let mut invalid_replacement = initial.clone();
        let wrong_session = SessionId::new("session-other").unwrap();
        invalid_replacement.summary.id = wrong_session.clone();
        invalid_replacement.snapshot.session_id = wrong_session;
        let (outcome, mut finalizer) = DriverCommandOutcome::guarded_replace(invalid_replacement);
        let rollback = tokio::spawn(async move {
            assert_eq!(
                finalizer.decision().await.unwrap(),
                FinalizeDecision::Rollback
            );
            finalizer
                .complete(Ok(FinalizeCompletion::RolledBack))
                .unwrap();
        });
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            initial,
            ActorConfig::default(),
        )
        .unwrap();
        let command = SessionCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            SessionId::new("session-actor").unwrap(),
            CommandId::new("command-checkout-publication-fails").unwrap(),
            1,
            Some(1),
            SessionCommand::Checkout {
                entry_id: DurableEntryId::new("entry-root").unwrap(),
            },
        );

        let admission = core
            .admit_command(command, 50, |_| async { Ok(outcome) })
            .await
            .unwrap();

        assert_eq!(
            admission.ack.error().map(|error| error.code),
            Some(ErrorCode::Internal)
        );
        assert!(admission.published.is_empty());
        rollback.await.unwrap();
        assert_eq!(core.snapshot(), before);
    }

    #[tokio::test]
    async fn checkout_refuses_pending_request_even_if_live_state_is_idle() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            checkout_seed(),
            ActorConfig::default(),
        )
        .unwrap();
        core.view.snapshot.pending_requests.push(PendingRequest {
            id: RequestId::new("request-pending").unwrap(),
            actor_generation: 1,
            kind: RequestKind::UserInput {
                prompt: "Choose".into(),
                choices: Vec::new(),
            },
            state: RequestState::Pending,
        });
        let before = core.snapshot();
        let command = SessionCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            SessionId::new("session-actor").unwrap(),
            CommandId::new("command-checkout-pending").unwrap(),
            1,
            Some(1),
            SessionCommand::Checkout {
                entry_id: DurableEntryId::new("entry-root").unwrap(),
            },
        );

        let admission = core
            .admit_command(command, 51, |_| async {
                panic!("pending request checkout must not reach the driver")
            })
            .await
            .unwrap();

        assert_eq!(
            admission.ack.error().map(|error| error.code),
            Some(ErrorCode::InvalidBoundary)
        );
        assert!(admission.published.is_empty());
        assert_eq!(core.snapshot(), before);
    }

    #[test]
    fn checkout_accepts_only_connected_terminal_states() {
        let checkout_command = || {
            SessionCommandEnvelope::new(
                HostId::new("host-test").unwrap(),
                DeviceId::new("device-test").unwrap(),
                SessionId::new("session-actor").unwrap(),
                CommandId::new("command-checkout-state").unwrap(),
                1,
                Some(1),
                SessionCommand::Checkout {
                    entry_id: DurableEntryId::new("entry-root").unwrap(),
                },
            )
        };
        for state in [
            SessionLiveState::Idle,
            SessionLiveState::Done,
            SessionLiveState::Failed,
            SessionLiveState::Stopped,
        ] {
            let mut core = SessionActorCore::new(
                HostId::new("host-test").unwrap(),
                checkout_seed(),
                ActorConfig::default(),
            )
            .unwrap();
            core.view.summary.live_state = state;
            core.view.snapshot.live_state = state;
            assert!(
                core.preflight_command(&checkout_command()).is_ok(),
                "{state:?} should permit branch checkout"
            );
        }
        for state in [
            SessionLiveState::Working,
            SessionLiveState::NeedsApproval,
            SessionLiveState::NeedsInput,
            SessionLiveState::Offline,
            SessionLiveState::Locked,
        ] {
            let mut core = SessionActorCore::new(
                HostId::new("host-test").unwrap(),
                checkout_seed(),
                ActorConfig::default(),
            )
            .unwrap();
            core.view.summary.live_state = state;
            core.view.snapshot.live_state = state;
            assert_eq!(
                core.preflight_command(&checkout_command())
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidBoundary,
                "{state:?} must reject branch checkout"
            );
        }
    }

    #[test]
    fn edit_retry_and_fork_require_idle_committed_entries_of_the_right_kind() {
        let replacement = PromptInput {
            text: "replacement".into(),
            attachments: Vec::new(),
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        let commands = [
            branch_command(
                "command-edit",
                SessionCommand::EditUserTurn {
                    source_user_entry_id: DurableEntryId::new("entry-root").unwrap(),
                    input: replacement.clone(),
                },
            ),
            branch_command(
                "command-retry",
                SessionCommand::RetryResponse {
                    source_assistant_entry_id: DurableEntryId::new("entry-old-head").unwrap(),
                    model: None,
                },
            ),
            branch_command(
                "command-fork",
                SessionCommand::ForkConversation {
                    entry_id: DurableEntryId::new("entry-old-head").unwrap(),
                },
            ),
        ];
        let core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            checkout_seed(),
            ActorConfig::default(),
        )
        .unwrap();
        for command in &commands {
            core.preflight_command(command).unwrap();
        }

        let wrong_edit = branch_command(
            "command-wrong-edit",
            SessionCommand::EditUserTurn {
                source_user_entry_id: DurableEntryId::new("entry-old-head").unwrap(),
                input: replacement,
            },
        );
        let wrong_retry = branch_command(
            "command-wrong-retry",
            SessionCommand::RetryResponse {
                source_assistant_entry_id: DurableEntryId::new("entry-root").unwrap(),
                model: None,
            },
        );
        assert_eq!(
            core.preflight_command(&wrong_edit).unwrap_err().code,
            ErrorCode::InvalidBoundary
        );
        assert_eq!(
            core.preflight_command(&wrong_retry).unwrap_err().code,
            ErrorCode::InvalidBoundary
        );

        let mut working = core;
        working.view.snapshot.live_state = SessionLiveState::Working;
        working.view.summary.live_state = SessionLiveState::Working;
        for command in &commands {
            assert_eq!(
                working.preflight_command(command).unwrap_err().code,
                ErrorCode::InvalidBoundary
            );
        }
    }

    #[tokio::test]
    async fn conversation_fork_ack_returns_only_the_new_session_and_is_idempotent() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            checkout_seed(),
            ActorConfig::default(),
        )
        .unwrap();
        let command = branch_command(
            "command-fork-ack",
            SessionCommand::ForkConversation {
                entry_id: DurableEntryId::new("entry-old-head").unwrap(),
            },
        );
        let duplicate = command.clone();
        let created = SessionId::new("session-created").unwrap();
        let admission = core
            .admit_command(command, 90, {
                let created = created.clone();
                move |command| async move {
                    assert!(matches!(command, SessionCommand::ForkConversation { .. }));
                    Ok(DriverCommandOutcome::fork(created))
                }
            })
            .await
            .unwrap();
        assert!(matches!(
            admission.ack.disposition,
            AckDisposition::Accepted {
                run_id: None,
                created_session_id: Some(ref session_id),
            } if session_id == &created
        ));

        let repeated = core
            .admit_command(duplicate, 91, |_| async {
                panic!("a cached conversation fork must never create another session")
            })
            .await
            .unwrap();
        assert!(repeated.cached);
        assert_eq!(repeated.ack, admission.ack);
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
            .await
            .unwrap();
        let key = (
            DeviceId::new("device-test").unwrap(),
            CommandId::new("command-secret").unwrap(),
        );
        assert!(matches!(
            core.command_cache.get(&key).map(|entry| &entry.identity),
            Some(CachedCommandIdentity::ConsumedSecret(_))
        ));

        let retransmit = core
            .admit_command(make_command("first secret"), 99, |_| async {
                panic!("a consumed device-scoped command must never dispatch twice")
            })
            .await
            .unwrap();
        assert!(retransmit.cached);
        assert_eq!(first.ack, retransmit.ack);

        let altered = core
            .admit_command(
                make_command("different retransmitted body"),
                100,
                |_| async { panic!("a conflicting device-scoped command must never dispatch") },
            )
            .await
            .unwrap();
        assert!(!altered.cached);
        assert_eq!(
            altered.ack.error().map(|error| error.code),
            Some(ErrorCode::CommandIdConflict)
        );
    }

    #[tokio::test]
    async fn oversized_immediate_driver_batch_is_rejected_before_projection() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            seed(),
            ActorConfig::default(),
        )
        .unwrap();
        let command = SessionCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            SessionId::new("session-actor").unwrap(),
            CommandId::new("command-event-flood").unwrap(),
            1,
            Some(1),
            SessionCommand::Abort { run_id: None },
        );
        let admission = core
            .admit_command(command, 10, |_| async {
                Ok(DriverCommandOutcome::with_events(
                    (0..=MAX_DRIVER_OUTCOME_EVENTS)
                        .map(|timestamp| {
                            TimestampedEvent::new(
                                timestamp as u64,
                                EventPayload::UsageUpdated {
                                    usage: Default::default(),
                                },
                            )
                        })
                        .collect(),
                ))
            })
            .await
            .unwrap();
        assert!(matches!(
            admission.ack.disposition,
            AckDisposition::Rejected { .. }
        ));
        assert!(admission.published.is_empty());
        assert_eq!(core.snapshot().cursor, SessionCursor::zero(1));
    }

    #[test]
    fn semantic_tool_delta_replay_matches_the_live_snapshot_without_raw_output() {
        let mut core = SessionActorCore::new(
            HostId::new("host-test").unwrap(),
            seed(),
            ActorConfig::default(),
        )
        .unwrap();
        let running = ToolActivity {
            raw_tool_name: "bash".into(),
            kind: ToolKind::Command,
            phase: ActivityPhase::Verified,
            status: ToolActivityStatus::Running,
            title: "Run cargo test".into(),
            summary: Some("Running".into()),
            target: None,
            cwd: Some(".".into()),
            command_preview: Some("cargo test".into()),
            exit_code: None,
            signal: None,
            started_at_ms: 100,
            completed_at_ms: None,
            duration_ms: None,
            output_summary: None,
            output_handle: None,
            observed_output_bytes: 0,
            dropped_output_bytes: 0,
            changed_paths: Vec::new(),
            source_ids: Vec::new(),
            artifact_ids: Vec::new(),
        };
        let item_id = ItemId::new("item-semantic-tool").unwrap();
        let started = core
            .publish(TimestampedEvent::new(
                100,
                EventPayload::ItemStarted {
                    item: SessionItem {
                        id: item_id.clone(),
                        run_id: Some(RunId::new("run-semantic-tool").unwrap()),
                        turn_id: Some(TurnId::new("turn-semantic-tool").unwrap()),
                        provider_attempt: Some(1),
                        lifecycle: ItemLifecycle::Provisional,
                        durable_entry_id: None,
                        payload: ItemPayload::ToolCall(running.clone()),
                    },
                },
            ))
            .unwrap();
        let mut replayed = core.snapshot();
        let mut completed = running;
        completed.status = ToolActivityStatus::Succeeded;
        completed.summary = Some("Completed".into());
        completed.completed_at_ms = Some(350);
        completed.duration_ms = Some(250);
        completed.exit_code = Some(0);
        completed.output_summary = Some("Verification completed".into());
        completed.observed_output_bytes = 4_096;
        let settled = core
            .publish(TimestampedEvent::new(
                350,
                EventPayload::ItemDelta {
                    item_id,
                    delta: ItemDelta::ToolActivity {
                        activity: completed,
                    },
                },
            ))
            .unwrap();
        let ReplayResponse::Events {
            events, through, ..
        } = core.replay_after(started.cursor)
        else {
            panic!("retained semantic delta should replay");
        };
        assert_eq!(events, vec![settled.clone()]);
        assert_eq!(through, settled.cursor);
        for event in events {
            reduce_snapshot(&mut replayed, &event.event).unwrap();
            replayed.cursor = event.cursor;
        }
        assert_eq!(replayed, core.snapshot());
        let serialized = serde_json::to_string(&settled).unwrap();
        assert!(!serialized.contains("stdout"));
        assert!(!serialized.contains("arguments"));
    }
}
