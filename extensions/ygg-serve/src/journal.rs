//! Bounded per-session replay journal.

use std::collections::VecDeque;

use crate::{
    EventEnvelope, ProtocolValidation, ReplayGap, ReplayResponse, SessionCursor, SessionSnapshot,
};

const MAX_REPLAY_EVENTS: usize = 65_536;
const MAX_REPLAY_BYTES: usize = 64 * 1024 * 1024;

/// Replay retention limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalConfig {
    /// Maximum retained event count.
    pub event_capacity: usize,
    /// Maximum aggregate serialized event bytes.
    pub byte_capacity: usize,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            event_capacity: 2_048,
            byte_capacity: 8 * 1024 * 1024,
        }
    }
}

/// Replay journal failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    /// Invalid retention limits.
    #[error("invalid replay journal configuration: {0}")]
    InvalidConfiguration(String),
    /// Invalid or non-contiguous event.
    #[error("invalid replay event: {0}")]
    InvalidEvent(String),
    /// Event exceeds the configured byte capacity.
    #[error("event exceeds the replay journal byte capacity")]
    EventTooLarge,
}

/// A bounded journal retaining exact sequenced events for one actor generation.
#[derive(Clone, Debug)]
pub struct EventJournal {
    config: JournalConfig,
    current: SessionCursor,
    retained_bytes: usize,
    events: VecDeque<(usize, EventEnvelope)>,
}

impl EventJournal {
    /// Creates an empty journal at an authoritative snapshot cursor.
    pub fn new(current: SessionCursor, config: JournalConfig) -> Result<Self, JournalError> {
        if current.actor_generation == 0 {
            return Err(JournalError::InvalidConfiguration(
                "actor generation must be non-zero".into(),
            ));
        }
        if config.event_capacity == 0 || config.event_capacity > MAX_REPLAY_EVENTS {
            return Err(JournalError::InvalidConfiguration(format!(
                "event_capacity must be 1..={MAX_REPLAY_EVENTS}"
            )));
        }
        if config.byte_capacity == 0 || config.byte_capacity > MAX_REPLAY_BYTES {
            return Err(JournalError::InvalidConfiguration(format!(
                "byte_capacity must be 1..={MAX_REPLAY_BYTES}"
            )));
        }
        Ok(Self {
            config,
            current,
            retained_bytes: 0,
            events: VecDeque::with_capacity(config.event_capacity.min(4_096)),
        })
    }

    /// Latest authoritative cursor.
    pub fn current(&self) -> SessionCursor {
        self.current
    }

    /// Number of retained events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no events are retained.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Aggregate retained serialized bytes.
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Appends one validated, strictly next event.
    pub fn append(&mut self, event: EventEnvelope) -> Result<(), JournalError> {
        event
            .validate()
            .map_err(|error| JournalError::InvalidEvent(error.to_string()))?;
        let expected = self
            .current
            .checked_next()
            .ok_or_else(|| JournalError::InvalidEvent("session sequence exhausted".into()))?;
        if event.cursor != expected {
            return Err(JournalError::InvalidEvent(format!(
                "expected cursor {:?}, received {:?}",
                expected, event.cursor
            )));
        }
        let bytes = serde_json::to_vec(&event)
            .map_err(|_| JournalError::InvalidEvent("event could not be serialized".into()))?
            .len();
        if bytes > self.config.byte_capacity {
            return Err(JournalError::EventTooLarge);
        }

        self.current = event.cursor;
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.events.push_back((bytes, event));
        while self.events.len() > self.config.event_capacity
            || self.retained_bytes > self.config.byte_capacity
        {
            let Some((bytes, _)) = self.events.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
        }
        Ok(())
    }

    /// Replays retained history or returns a complete snapshot gap fallback.
    pub fn replay_after(&self, after: SessionCursor, snapshot: &SessionSnapshot) -> ReplayResponse {
        debug_assert_eq!(snapshot.cursor, self.current);
        let latest = self.current;
        let earliest = self.events.front().map_or_else(
            || latest.checked_next().unwrap_or(latest),
            |(_, event)| event.cursor,
        );
        let generation_changed = after.actor_generation != latest.actor_generation;
        let cursor_ahead = !generation_changed && after.sequence > latest.sequence;
        let cursor_too_old = !generation_changed
            && after
                .sequence
                .checked_add(1)
                .is_some_and(|next| next < earliest.sequence);

        if generation_changed || cursor_ahead || cursor_too_old {
            return ReplayResponse::Gap {
                gap: ReplayGap {
                    requested_after: after,
                    earliest_available: earliest,
                    latest_available: latest,
                },
                snapshot: Box::new(snapshot.clone()),
            };
        }

        ReplayResponse::Events {
            after,
            through: latest,
            events: self
                .events
                .iter()
                .filter(|(_, event)| event.cursor.sequence > after.sequence)
                .map(|(_, event)| event.clone())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AuthorityProfile, ContextUsage, EventPayload, ModelSelection, SessionId, SessionLiveState,
    };

    use super::*;

    fn snapshot(cursor: SessionCursor) -> SessionSnapshot {
        SessionSnapshot {
            session_id: SessionId::new("session-journal").unwrap(),
            actor_generation: cursor.actor_generation,
            cursor,
            durable_head: None,
            live_state: SessionLiveState::Idle,
            active_run_id: None,
            model: ModelSelection {
                provider: "test".into(),
                model: "test".into(),
                reasoning: "off".into(),
            },
            authority: AuthorityProfile::FullAccess,
            context: ContextUsage::default(),
            items: Vec::new(),
            pending_requests: Vec::new(),
            sources: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn bounded_history_returns_gap_snapshot() {
        let start = SessionCursor::zero(1);
        let mut journal = EventJournal::new(
            start,
            JournalConfig {
                event_capacity: 2,
                byte_capacity: 1024 * 1024,
            },
        )
        .unwrap();
        for sequence in 1..=3 {
            journal
                .append(EventEnvelope::new(
                    SessionId::new("session-journal").unwrap(),
                    SessionCursor {
                        actor_generation: 1,
                        sequence,
                    },
                    sequence,
                    EventPayload::UsageUpdated {
                        usage: Default::default(),
                    },
                ))
                .unwrap();
        }
        let snapshot = snapshot(journal.current());
        assert!(matches!(
            journal.replay_after(start, &snapshot),
            ReplayResponse::Gap { .. }
        ));
        let ReplayResponse::Events { events, .. } = journal.replay_after(
            SessionCursor {
                actor_generation: 1,
                sequence: 1,
            },
            &snapshot,
        ) else {
            panic!("retained cursor must replay");
        };
        assert_eq!(events.len(), 2);
    }
}
