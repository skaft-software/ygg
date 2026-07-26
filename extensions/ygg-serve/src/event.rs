//! Typed session events, deltas, and replay responses.

use serde::{Deserialize, Serialize};

use crate::bounds::{
    validate_public_text, validate_serialized_size, ProtocolValidation, ValidationError,
    MAX_EVENT_BYTES, MAX_PUBLIC_TEXT_BYTES,
};
use crate::{
    ArtifactRef, AuthorityProfile, DurableEntryId, ItemId, ItemLifecycle, ModelSelection,
    PendingRequest, RunId, SessionCursor, SessionId, SessionItem, SessionLiveState,
    SessionSnapshot, SourceRef, UsageSnapshot, PROTOCOL_VERSION,
};

/// Typed provisional item delta.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ItemDelta {
    /// Append visible assistant text.
    AssistantText {
        /// Exact UTF-8 suffix.
        append: String,
    },
    /// Append reasoning text.
    ReasoningText {
        /// Exact UTF-8 suffix.
        append: String,
    },
    /// Replace bounded live tool progress.
    ToolProgress {
        /// Latest retained progress text.
        text: String,
        /// Total producer-dropped bytes.
        dropped_bytes: u64,
    },
}

/// Public state transition projected from one owning session driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all_fields = "camelCase")]
pub enum EventPayload {
    /// Strong live state changed.
    #[serde(rename = "session.stateChanged")]
    SessionStateChanged {
        /// New state.
        state: SessionLiveState,
        /// Active run, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_run_id: Option<RunId>,
    },
    /// Model/reasoning or authority changed at a valid boundary.
    #[serde(rename = "session.settingsChanged")]
    SessionSettingsChanged {
        /// Complete model/provider/reasoning selection.
        model: ModelSelection,
        /// Effective authority after host clamping.
        authority: AuthorityProfile,
    },
    /// The authoritative append-only head changed, including invisible config
    /// or provider sidecar entries.
    #[serde(rename = "session.durableHeadChanged")]
    SessionDurableHeadChanged {
        /// Exact current head, or none for an empty provisional session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        durable_entry_id: Option<DurableEntryId>,
    },
    /// A provisional item began.
    #[serde(rename = "item.started")]
    ItemStarted {
        /// Complete initial provisional item.
        item: SessionItem,
    },
    /// A provisional item received a delta.
    #[serde(rename = "item.delta")]
    ItemDelta {
        /// Stable item identity.
        item_id: ItemId,
        /// Typed delta.
        delta: ItemDelta,
    },
    /// A complete item became durable.
    #[serde(rename = "item.committed")]
    ItemCommitted {
        /// Authoritative committed item with exact durable entry ID.
        item: SessionItem,
    },
    /// A provisional provider candidate was rejected.
    #[serde(rename = "item.retracted")]
    ItemRetracted {
        /// Stable item identity.
        item_id: ItemId,
        /// Provider attempt being retracted.
        provider_attempt: u32,
        /// Public reason.
        reason: String,
    },
    /// A public request opened or resolved.
    #[serde(rename = "request.changed")]
    PendingRequestChanged {
        /// Request state.
        request: PendingRequest,
    },
    /// Source registry changed.
    #[serde(rename = "source.upserted")]
    SourceUpserted {
        /// Complete authoritative source reference.
        source: SourceRef,
    },
    /// Artifact registry changed.
    #[serde(rename = "artifact.upserted")]
    ArtifactUpserted {
        /// Complete authoritative artifact reference.
        artifact: ArtifactRef,
    },
    /// Usage/context counters changed.
    #[serde(rename = "usage.updated")]
    UsageUpdated {
        /// Latest usage.
        usage: UsageSnapshot,
    },
}

/// Driver event before actor sequence assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimestampedEvent {
    /// Host timestamp.
    pub timestamp_ms: u64,
    /// Public typed payload.
    pub payload: EventPayload,
}

impl TimestampedEvent {
    /// Creates a driver event.
    pub fn new(timestamp_ms: u64, payload: EventPayload) -> Self {
        Self {
            timestamp_ms,
            payload,
        }
    }
}

/// Sequenced public event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventEnvelope {
    /// Protocol major.
    pub protocol: u16,
    /// Session identity.
    pub session_id: SessionId,
    /// Generation-bound cursor.
    pub cursor: SessionCursor,
    /// Host timestamp.
    pub timestamp_ms: u64,
    /// Typed payload.
    pub event: EventPayload,
}

impl EventEnvelope {
    /// Creates a protocol-v1 event envelope.
    pub fn new(
        session_id: SessionId,
        cursor: SessionCursor,
        timestamp_ms: u64,
        event: EventPayload,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            session_id,
            cursor,
            timestamp_ms,
            event,
        }
    }
}

/// One event in the host-global live stream.
///
/// Session cursors remain the authority for replay. `host_sequence` only
/// provides deterministic ordering across concurrently running session
/// actors on one live connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostStreamEvent {
    /// Protocol major.
    pub protocol: u16,
    /// Monotonic sequence for this running host process.
    pub host_sequence: u64,
    /// Exact session event.
    pub event: EventEnvelope,
}

impl HostStreamEvent {
    /// Wraps one session event with a live host sequence.
    pub fn new(host_sequence: u64, event: EventEnvelope) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            host_sequence,
            event,
        }
    }
}

/// Explicit cursor gap requiring snapshot replacement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplayGap {
    /// Client cursor.
    pub requested_after: SessionCursor,
    /// Earliest retained event cursor.
    pub earliest_available: SessionCursor,
    /// Latest authoritative cursor.
    pub latest_available: SessionCursor,
}

/// Replay result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ReplayResponse {
    /// Retained history covers the requested cursor.
    Events {
        /// Requested cursor.
        after: SessionCursor,
        /// Cursor represented after applying all events.
        through: SessionCursor,
        /// Strictly increasing events.
        events: Vec<EventEnvelope>,
    },
    /// Cursor is outside retained history or actor generation changed.
    Gap {
        /// Explicit gap.
        gap: ReplayGap,
        /// Complete authoritative replacement.
        snapshot: Box<SessionSnapshot>,
    },
}

impl ProtocolValidation for ItemDelta {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::AssistantText { append } => {
                validate_public_text("event.delta.assistant", append, MAX_PUBLIC_TEXT_BYTES, true)
            }
            Self::ReasoningText { append } => {
                validate_public_text("event.delta.reasoning", append, MAX_PUBLIC_TEXT_BYTES, true)
            }
            Self::ToolProgress { text, .. } => validate_public_text(
                "event.delta.tool_progress",
                text,
                MAX_PUBLIC_TEXT_BYTES,
                true,
            ),
        }
    }
}

impl ProtocolValidation for EventPayload {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::SessionStateChanged {
                state,
                active_run_id,
            } => {
                let run_required = matches!(
                    state,
                    SessionLiveState::Working
                        | SessionLiveState::NeedsApproval
                        | SessionLiveState::NeedsInput
                );
                if run_required && active_run_id.is_none() {
                    return Err(ValidationError::new(
                        "event.active_run_id",
                        "is required for an active or blocked session state",
                    ));
                }
            }
            Self::SessionSettingsChanged { model, .. } => model.validate()?,
            Self::SessionDurableHeadChanged { .. } => {}
            Self::ItemStarted { item } => {
                item.validate()?;
                if item.lifecycle != ItemLifecycle::Provisional {
                    return Err(ValidationError::new(
                        "event.item.lifecycle",
                        "must be provisional when an item starts",
                    ));
                }
            }
            Self::ItemDelta { delta, .. } => delta.validate()?,
            Self::ItemCommitted { item } => {
                item.validate()?;
                if item.lifecycle != ItemLifecycle::Committed {
                    return Err(ValidationError::new(
                        "event.item.lifecycle",
                        "must be committed when an item commits",
                    ));
                }
            }
            Self::ItemRetracted { reason, .. } => {
                validate_public_text("event.retraction.reason", reason, 8192, true)?;
            }
            Self::PendingRequestChanged { request } => request.validate()?,
            Self::SourceUpserted { source } => source.validate()?,
            Self::ArtifactUpserted { artifact } => artifact.validate()?,
            Self::UsageUpdated { .. } => {}
        }
        Ok(())
    }
}

impl ProtocolValidation for EventEnvelope {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "event.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        if self.cursor.actor_generation == 0 || self.cursor.sequence == 0 {
            return Err(ValidationError::new(
                "event.cursor",
                "requires non-zero generation and sequence",
            ));
        }
        self.event.validate()?;
        validate_serialized_size("event", self, MAX_EVENT_BYTES)
    }
}

impl ProtocolValidation for HostStreamEvent {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "host_stream.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        if self.host_sequence == 0 {
            return Err(ValidationError::new(
                "host_stream.host_sequence",
                "must be non-zero",
            ));
        }
        self.event.validate()?;
        validate_serialized_size("host_stream", self, MAX_EVENT_BYTES)
    }
}

impl ProtocolValidation for ReplayResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Events {
                after,
                through,
                events,
            } => {
                if after.actor_generation != through.actor_generation {
                    return Err(ValidationError::new(
                        "replay.through",
                        "must use the requested actor generation",
                    ));
                }
                let mut expected = after.sequence;
                for event in events {
                    event.validate()?;
                    if event.cursor.actor_generation != after.actor_generation
                        || event.cursor.sequence != expected.saturating_add(1)
                    {
                        return Err(ValidationError::new(
                            "replay.events",
                            "must be contiguous and generation-consistent",
                        ));
                    }
                    expected = event.cursor.sequence;
                }
                if expected != through.sequence {
                    return Err(ValidationError::new(
                        "replay.through",
                        "must equal the last replayed event cursor",
                    ));
                }
            }
            Self::Gap { gap, snapshot } => {
                snapshot.validate()?;
                if gap.latest_available != snapshot.cursor {
                    return Err(ValidationError::new(
                        "replay.gap.latest_available",
                        "must equal the replacement snapshot cursor",
                    ));
                }
            }
        }
        Ok(())
    }
}
