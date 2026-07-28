//! Durable per-session goal state for graphical Ygg clients.
//!
//! The goal store is intentionally independent of the session actor. The
//! extension and a future continuation driver can both open the same private
//! directory and use this API without coupling their lifetimes.

use std::fs::OpenOptions;
#[cfg(unix)]
use std::fs::Permissions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{validate_public_text, SessionId};

const STORE_VERSION: u16 = 1;
const MAX_GOAL_FILE_BYTES: u64 = 16 * 1024;
const MAX_CREATED_AT_BYTES: usize = 64;
const TEMP_RANDOM_BYTES: usize = 12;
const MAX_TURN_BUDGET: u32 = 100_000;
const GOAL_DIRECTORY_MODE: u32 = 0o700;
const GOAL_FILE_MODE: u32 = 0o600;

/// Maximum UTF-8 bytes accepted for a persistent objective.
pub const MAX_GOAL_OBJECTIVE_BYTES: usize = 4 * 1024;

/// Goal lifecycle states shared by the graphical extension and future drivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The driver may continue working toward the objective.
    Active,
    /// The objective remains stored but continuation is suspended.
    Paused,
    /// The objective was reached.
    Complete,
    /// The driver could not make further progress.
    Blocked,
    /// The configured turn budget was exhausted.
    BudgetLimited,
}

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

/// Public, path-free state for one session objective.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalState {
    /// The bounded objective the driver should pursue.
    pub objective: String,
    /// Current lifecycle state.
    pub status: GoalStatus,
    /// Maximum continuation turns, or unlimited when absent.
    pub turn_budget: Option<u32>,
    /// Number of continuation turns already consumed.
    pub turns_used: u32,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
}

impl GoalState {
    fn new(objective: &str, turn_budget: Option<u32>) -> Result<Self, GoalStoreError> {
        let objective = objective.trim();
        validate_objective(objective)?;
        validate_turn_budget(turn_budget)?;
        Ok(Self {
            objective: objective.to_owned(),
            status: GoalStatus::Active,
            turn_budget,
            turns_used: 0,
            created_at: now_rfc3339(),
        })
    }

    fn validate(&self) -> Result<(), GoalStoreError> {
        validate_objective(&self.objective)?;
        validate_turn_budget(self.turn_budget)?;
        if self.created_at.is_empty() || self.created_at.len() > MAX_CREATED_AT_BYTES {
            return Err(GoalStoreError::CorruptState);
        }
        if validate_public_text(
            "goal.createdAt",
            &self.created_at,
            MAX_CREATED_AT_BYTES,
            false,
        )
        .is_err()
        {
            return Err(GoalStoreError::CorruptState);
        }
        if self
            .turn_budget
            .is_some_and(|budget| self.turns_used > budget)
        {
            return Err(GoalStoreError::CorruptState);
        }
        Ok(())
    }
}

/// Persistent goal-store failures.
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredGoal {
    version: u16,
    state: GoalState,
}

struct GoalStoreInner {
    root: PathBuf,
    lock: Mutex<()>,
}

/// Cloneable, file-backed goal store keyed by validated session IDs.
#[derive(Clone)]
pub struct GoalStore {
    inner: Arc<GoalStoreInner>,
}

impl GoalStore {
    /// Opens or creates a private directory for session goal files.
    pub fn open(root: &Path) -> Result<Self, GoalStoreError> {
        if !root.is_absolute() {
            return Err(GoalStoreError::UnsafePath);
        }
        ensure_private_directory(root)?;
        Ok(Self {
            inner: Arc::new(GoalStoreInner {
                root: root.to_owned(),
                lock: Mutex::new(()),
            }),
        })
    }

    /// Returns the current goal, or `None` when the session has no goal.
    pub fn get(&self, session_id: &SessionId) -> Result<Option<GoalState>, GoalStoreError> {
        let _guard = self.lock()?;
        self.read_locked(session_id)
    }

    /// Replaces or creates a session goal and resets its lifecycle counters.
    pub fn set(
        &self,
        session_id: &SessionId,
        objective: &str,
        turn_budget: Option<u32>,
    ) -> Result<GoalState, GoalStoreError> {
        let _guard = self.lock()?;
        let state = GoalState::new(objective, turn_budget)?;
        self.write_locked(session_id, &state)?;
        Ok(state)
    }

    /// Applies a pause, resume, or clear action and returns the resulting goal.
    pub fn apply(
        &self,
        session_id: &SessionId,
        action: GoalAction,
    ) -> Result<Option<GoalState>, GoalStoreError> {
        let _guard = self.lock()?;
        if action == GoalAction::Clear {
            self.remove_locked(session_id)?;
            return Ok(None);
        }
        let mut state = self
            .read_locked(session_id)?
            .ok_or(GoalStoreError::NotFound)?;
        match (action, state.status) {
            (GoalAction::Pause, GoalStatus::Active) => state.status = GoalStatus::Paused,
            (GoalAction::Pause, GoalStatus::Paused) => {}
            (GoalAction::Resume, GoalStatus::Paused) => state.status = GoalStatus::Active,
            (GoalAction::Resume, GoalStatus::Active) => {}
            _ => return Err(GoalStoreError::InvalidTransition),
        }
        self.write_locked(session_id, &state)?;
        Ok(Some(state))
    }

    /// Records one continuation turn and enforces the configured budget.
    pub fn record_turn(&self, session_id: &SessionId) -> Result<GoalState, GoalStoreError> {
        let _guard = self.lock()?;
        let mut state = self
            .read_locked(session_id)?
            .ok_or(GoalStoreError::NotFound)?;
        if state.status != GoalStatus::Active {
            return Err(GoalStoreError::InvalidTransition);
        }
        state.turns_used = state
            .turns_used
            .checked_add(1)
            .ok_or(GoalStoreError::InvalidTransition)?;
        if state
            .turn_budget
            .is_some_and(|budget| state.turns_used >= budget)
        {
            state.status = GoalStatus::BudgetLimited;
        }
        self.write_locked(session_id, &state)?;
        Ok(state)
    }

    /// Marks a goal complete after the driver observes its completion marker.
    pub fn mark_complete(&self, session_id: &SessionId) -> Result<GoalState, GoalStoreError> {
        self.set_status(session_id, GoalStatus::Complete)
    }

    /// Marks a goal blocked after the driver observes that no progress is possible.
    pub fn mark_blocked(&self, session_id: &SessionId) -> Result<GoalState, GoalStoreError> {
        self.set_status(session_id, GoalStatus::Blocked)
    }

    fn set_status(
        &self,
        session_id: &SessionId,
        status: GoalStatus,
    ) -> Result<GoalState, GoalStoreError> {
        let _guard = self.lock()?;
        let mut state = self
            .read_locked(session_id)?
            .ok_or(GoalStoreError::NotFound)?;
        state.status = status;
        self.write_locked(session_id, &state)?;
        Ok(state)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, GoalStoreError> {
        self.inner
            .lock
            .lock()
            .map_err(|_| GoalStoreError::Storage(std::io::Error::other("goal store lock poisoned")))
    }

    fn goal_path(&self, session_id: &SessionId) -> PathBuf {
        self.inner
            .root
            .join(format!("{}.json", session_id.as_str()))
    }

    fn read_locked(&self, session_id: &SessionId) -> Result<Option<GoalState>, GoalStoreError> {
        let path = self.goal_path(session_id);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(GoalStoreError::Storage(error)),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(GoalStoreError::UnsafePath);
        }
        if metadata.len() > MAX_GOAL_FILE_BYTES {
            return Err(GoalStoreError::CorruptState);
        }
        let file = open_read_no_follow(&path)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_GOAL_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(GoalStoreError::Storage)?;
        if bytes.len() as u64 > MAX_GOAL_FILE_BYTES {
            return Err(GoalStoreError::CorruptState);
        }
        let stored: StoredGoal =
            serde_json::from_slice(&bytes).map_err(|_| GoalStoreError::CorruptState)?;
        if stored.version != STORE_VERSION {
            return Err(GoalStoreError::CorruptState);
        }
        stored.state.validate()?;
        Ok(Some(stored.state))
    }

    fn write_locked(
        &self,
        session_id: &SessionId,
        state: &GoalState,
    ) -> Result<(), GoalStoreError> {
        state.validate()?;
        let bytes = serde_json::to_vec(&StoredGoal {
            version: STORE_VERSION,
            state: state.clone(),
        })
        .map_err(|_| GoalStoreError::CorruptState)?;
        if bytes.len() as u64 > MAX_GOAL_FILE_BYTES {
            return Err(GoalStoreError::CorruptState);
        }
        let path = self.goal_path(session_id);
        let temp_path = self.inner.root.join(format!(
            ".{}.tmp-{}",
            session_id.as_str(),
            random_hex(TEMP_RANDOM_BYTES)?
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(GOAL_FILE_MODE).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = match options.open(&temp_path) {
            Ok(file) => file,
            Err(error) => return Err(GoalStoreError::Storage(error)),
        };
        let write_result = (|| -> Result<(), std::io::Error> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(GoalStoreError::Storage(error));
        }
        if let Err(error) = std::fs::rename(&temp_path, &path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(GoalStoreError::Storage(error));
        }
        Ok(())
    }

    fn remove_locked(&self, session_id: &SessionId) -> Result<(), GoalStoreError> {
        let path = self.goal_path(session_id);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(GoalStoreError::UnsafePath);
                }
                std::fs::remove_file(path).map_err(GoalStoreError::Storage)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(GoalStoreError::Storage(error)),
        }
    }
}

fn validate_objective(objective: &str) -> Result<(), GoalStoreError> {
    if objective.is_empty()
        || validate_public_text("goal.objective", objective, MAX_GOAL_OBJECTIVE_BYTES, false)
            .is_err()
    {
        return Err(GoalStoreError::InvalidObjective);
    }
    Ok(())
}

fn validate_turn_budget(turn_budget: Option<u32>) -> Result<(), GoalStoreError> {
    if turn_budget.is_some_and(|budget| budget == 0 || budget > MAX_TURN_BUDGET) {
        return Err(GoalStoreError::InvalidTurnBudget);
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), GoalStoreError> {
    std::fs::create_dir_all(path).map_err(GoalStoreError::Storage)?;
    let metadata = std::fs::symlink_metadata(path).map_err(GoalStoreError::Storage)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(GoalStoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, Permissions::from_mode(GOAL_DIRECTORY_MODE))
            .map_err(GoalStoreError::Storage)?;
    }
    Ok(())
}

fn open_read_no_follow(path: &Path) -> Result<std::fs::File, GoalStoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(GoalStoreError::Storage)
}

fn random_hex(byte_count: usize) -> Result<String, GoalStoreError> {
    let mut bytes = vec![0u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|_| {
        GoalStoreError::Storage(std::io::Error::other("secure goal randomness unavailable"))
    })?;
    Ok(bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn now_rfc3339() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let seconds = duration.as_secs();
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// Howard Hinnant's Gregorian civil date conversion, shifted to the Unix epoch.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month + 2) / 5 + 1;
    let year = year + if month < 10 { 0 } else { 1 };
    let month = month + if month < 10 { 3 } else { -9 };
    (year, month, day)
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
