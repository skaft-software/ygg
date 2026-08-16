//! Durable per-session goal state for graphical Ygg clients.
//!
//! The storage implementation is shared with `ygg-agent`. This module keeps
//! the Serve-facing error and validated identifier adapter small while both
//! frontends use the same on-disk schema and atomic-write behavior.

use std::path::Path;

use serde::{Deserialize, Serialize};
use ygg_agent::{DurableGoalStore, DurableGoalStoreError, GoalAction as DurableGoalAction};
pub use ygg_agent::{MAX_GOAL_OBJECTIVE_BYTES, MAX_GOAL_TURN_BUDGET};

use crate::{validate_public_text, ProtocolValidation, SessionId, ValidationError};

const MAX_CREATED_AT_BYTES: usize = 64;

pub use ygg_agent::{GoalState, GoalStatus};

/// A goal action accepted by the session goal API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalAction {
    /// Suspend an active goal.
    Pause,
    /// Resume a paused goal.
    Resume,
    /// Remove the goal from the session.
    Clear,
}

/// Persistent goal-store failures exposed by Serve.
#[derive(Debug, thiserror::Error)]
pub enum GoalStoreError {
    /// The objective is empty, oversized, or contains unsafe controls.
    #[error("invalid goal objective")]
    InvalidObjective,
    /// The turn budget is zero or exceeds the bounded store limit.
    #[error("invalid goal turn budget")]
    InvalidTurnBudget,
    /// No goal exists for the requested session.
    #[error("goal not found")]
    NotFound,
    /// The requested lifecycle transition is not valid.
    #[error("invalid goal lifecycle transition")]
    InvalidTransition,
    /// A goal file was malformed or violated a stored invariant.
    #[error("goal state is corrupt")]
    CorruptState,
    /// A goal directory or file could not be safely used.
    #[error("goal storage path is unsafe")]
    UnsafePath,
    /// Private goal storage failed.
    #[error("goal storage failed: {0}")]
    Storage(#[source] std::io::Error),
}

/// Serve adapter over the provider-neutral durable goal store.
#[derive(Clone)]
pub struct GoalStore {
    inner: DurableGoalStore,
}

impl GoalStore {
    /// Opens or creates a private directory for session goal files.
    pub fn open(root: &Path) -> Result<Self, GoalStoreError> {
        Ok(Self {
            inner: DurableGoalStore::open(root).map_err(map_store_error)?,
        })
    }

    /// Returns the current goal, or `None` when the session has no goal.
    pub fn get(&self, session_id: &SessionId) -> Result<Option<GoalState>, GoalStoreError> {
        self.inner.get(session_id.as_str()).map_err(map_store_error)
    }

    /// Returns the latest durable revision, including a cleared goal's
    /// tombstone revision.
    pub fn revision(&self, session_id: &SessionId) -> Result<u64, GoalStoreError> {
        self.inner
            .revision(session_id.as_str())
            .map_err(map_store_error)
    }

    /// Returns the current goal and its durable revision from one locked read.
    pub fn snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<(Option<GoalState>, u64), GoalStoreError> {
        self.inner
            .snapshot(session_id.as_str())
            .map_err(map_store_error)
    }

    /// Replaces or creates a session goal and resets its lifecycle counters.
    pub fn set(
        &self,
        session_id: &SessionId,
        objective: &str,
        turn_budget: Option<u32>,
    ) -> Result<GoalState, GoalStoreError> {
        self.inner
            .set(session_id.as_str(), objective, turn_budget)
            .map_err(map_store_error)
    }

    /// Applies a pause, resume, or clear action and returns the resulting goal.
    pub fn apply(
        &self,
        session_id: &SessionId,
        action: GoalAction,
    ) -> Result<Option<GoalState>, GoalStoreError> {
        self.inner
            .apply(session_id.as_str(), action.into())
            .map_err(map_store_error)
    }

    /// Removes a permanently deleted session's goal, if present.
    pub fn delete_session(&self, session_id: &SessionId) -> Result<(), GoalStoreError> {
        self.inner
            .delete_session(session_id.as_str())
            .map_err(map_store_error)
    }

    /// Records one continuation turn and enforces the configured budget.
    pub fn record_turn(&self, session_id: &SessionId) -> Result<GoalState, GoalStoreError> {
        self.inner
            .record_turn(session_id.as_str())
            .map_err(map_store_error)
    }

    /// Marks a goal complete after the driver observes its completion marker.
    pub fn mark_complete(&self, session_id: &SessionId) -> Result<GoalState, GoalStoreError> {
        self.inner
            .mark_complete(session_id.as_str())
            .map_err(map_store_error)
    }

    /// Marks a goal blocked after the driver observes that no progress is possible.
    pub fn mark_blocked(&self, session_id: &SessionId) -> Result<GoalState, GoalStoreError> {
        self.inner
            .mark_blocked(session_id.as_str())
            .map_err(map_store_error)
    }
}

impl From<GoalAction> for DurableGoalAction {
    fn from(action: GoalAction) -> Self {
        match action {
            GoalAction::Pause => Self::Pause,
            GoalAction::Resume => Self::Resume,
            GoalAction::Clear => Self::Clear,
        }
    }
}

impl ProtocolValidation for GoalState {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text(
            "goal.objective",
            &self.objective,
            MAX_GOAL_OBJECTIVE_BYTES,
            false,
        )?;
        if self
            .turn_budget
            .is_some_and(|budget| budget == 0 || budget > MAX_GOAL_TURN_BUDGET)
        {
            return Err(ValidationError::new(
                "goal.turnBudget",
                "must be absent or within the durable goal budget limit",
            ));
        }
        if self
            .turn_budget
            .is_some_and(|budget| self.turns_used > budget)
        {
            return Err(ValidationError::new(
                "goal.turnsUsed",
                "must not exceed the configured turn budget",
            ));
        }
        validate_public_text(
            "goal.createdAt",
            &self.created_at,
            MAX_CREATED_AT_BYTES,
            false,
        )
    }
}

fn map_store_error(error: DurableGoalStoreError) -> GoalStoreError {
    match error {
        DurableGoalStoreError::InvalidObjective => GoalStoreError::InvalidObjective,
        DurableGoalStoreError::InvalidTurnBudget => GoalStoreError::InvalidTurnBudget,
        DurableGoalStoreError::NotFound => GoalStoreError::NotFound,
        DurableGoalStoreError::InvalidTransition => GoalStoreError::InvalidTransition,
        DurableGoalStoreError::CorruptState => GoalStoreError::CorruptState,
        DurableGoalStoreError::UnsafePath => GoalStoreError::UnsafePath,
        DurableGoalStoreError::Storage(error) => GoalStoreError::Storage(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reopens_goal_state() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new("goal-session").unwrap();
        let store = GoalStore::open(directory.path()).unwrap();
        let created = store.set(&session, "Write the README", Some(10)).unwrap();
        assert_eq!(created.status, GoalStatus::Active);
        assert_eq!(created.turn_budget, Some(10));
        assert_eq!(created.turns_used, 0);
        assert_eq!(created.revision, 1);
        assert!(created.created_at.ends_with('Z'));

        let reopened = GoalStore::open(directory.path()).unwrap();
        assert_eq!(reopened.get(&session).unwrap(), Some(created));
    }

    #[test]
    fn lifecycle_actions_are_durable_and_clear_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new("goal-lifecycle").unwrap();
        let store = GoalStore::open(directory.path()).unwrap();
        store.set(&session, "Ship it", None).unwrap();
        assert_eq!(
            store
                .apply(&session, GoalAction::Pause)
                .unwrap()
                .unwrap()
                .status,
            GoalStatus::Paused
        );
        assert_eq!(
            store
                .apply(&session, GoalAction::Resume)
                .unwrap()
                .unwrap()
                .status,
            GoalStatus::Active
        );
        assert_eq!(store.apply(&session, GoalAction::Clear).unwrap(), None);
        assert_eq!(store.apply(&session, GoalAction::Clear).unwrap(), None);
        assert_eq!(store.get(&session).unwrap(), None);
    }

    #[test]
    fn permanent_session_deletion_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new("goal-deleted").unwrap();
        let store = GoalStore::open(directory.path()).unwrap();
        store.set(&session, "Remove this goal", None).unwrap();

        store.delete_session(&session).unwrap();
        store.delete_session(&session).unwrap();

        assert_eq!(store.get(&session).unwrap(), None);
        assert!(!directory
            .path()
            .join(format!("{}.json", session.as_str()))
            .exists());
    }

    #[test]
    fn budget_marks_goal_limited_at_the_configured_turn() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new("goal-budget").unwrap();
        let store = GoalStore::open(directory.path()).unwrap();
        store.set(&session, "Verify the build", Some(2)).unwrap();
        assert_eq!(
            store.record_turn(&session).unwrap().status,
            GoalStatus::Active
        );
        let state = store.record_turn(&session).unwrap();
        assert_eq!(state.turns_used, 2);
        assert_eq!(state.status, GoalStatus::BudgetLimited);
        assert_eq!(
            store.record_turn(&session).unwrap_err().to_string(),
            "invalid goal lifecycle transition"
        );
    }

    #[test]
    fn invalid_state_does_not_cross_the_store_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new("goal-invalid").unwrap();
        let store = GoalStore::open(directory.path()).unwrap();
        assert!(matches!(
            store.set(&session, "\u{1b}[31munsafe", None),
            Err(GoalStoreError::InvalidObjective)
        ));
        assert!(matches!(
            store.set(&session, "valid", Some(0)),
            Err(GoalStoreError::InvalidTurnBudget)
        ));
        assert!(matches!(
            store.apply(&session, GoalAction::Pause),
            Err(GoalStoreError::NotFound)
        ));
    }
}
