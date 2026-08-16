//! Provider-neutral durable persistence for session goals.
//!
//! The graphical Serve extension has its own public goal DTOs, but the on-disk
//! format is deliberately small and shared with this store. Keeping this
//! implementation in `ygg-agent` lets the terminal frontend use durable goals
//! without depending on the optional Serve crate.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::goal_driver::{GoalState, GoalStatus, GoalStore};

const STORE_VERSION: u16 = 1;
const MAX_GOAL_FILE_BYTES: u64 = 16 * 1024;
const MAX_CREATED_AT_BYTES: usize = 64;
const MAX_TURN_BUDGET: u32 = 100_000;
/// Maximum number of continuation turns accepted by the durable store.
pub const MAX_GOAL_TURN_BUDGET: u32 = MAX_TURN_BUDGET;
const GOAL_DIRECTORY_MODE: u32 = 0o700;
const GOAL_FILE_MODE: u32 = 0o600;

/// Maximum UTF-8 bytes accepted for a persistent objective.
pub const MAX_GOAL_OBJECTIVE_BYTES: usize = 4 * 1024;

/// A user-controlled lifecycle mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalAction {
    /// Suspend an active goal.
    Pause,
    /// Resume a paused goal.
    Resume,
    /// Remove the goal.
    Clear,
}

/// Errors returned by [`DurableGoalStore`].
#[derive(Debug, thiserror::Error)]
pub enum DurableGoalStoreError {
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
    /// The current goal, or `None` for a durable clear tombstone.
    state: Option<GoalState>,
    /// The last state revision, retained after a clear so projections cannot
    /// accept an older goal event after the tombstone.
    #[serde(default)]
    revision: u64,
}

struct DurableGoalStoreInner {
    root: PathBuf,
    lock: Mutex<()>,
}

/// Cloneable, file-backed goal store keyed by session IDs.
///
/// Writes use a private temporary file, `sync_all`, and an atomic rename. The
/// store is intentionally compatible with the Serve goal file format so a
/// session can be resumed by either first-party frontend.
#[derive(Clone)]
pub struct DurableGoalStore {
    inner: Arc<DurableGoalStoreInner>,
}

impl DurableGoalStore {
    /// Opens or creates a private goal directory.
    pub fn open(root: &Path) -> Result<Self, DurableGoalStoreError> {
        if !root.is_absolute() {
            return Err(DurableGoalStoreError::UnsafePath);
        }
        ensure_private_directory(root)?;
        Ok(Self {
            inner: Arc::new(DurableGoalStoreInner {
                root: root.to_owned(),
                lock: Mutex::new(()),
            }),
        })
    }

    /// Returns the current goal, if one exists.
    pub fn get(&self, session_id: &str) -> Result<Option<GoalState>, DurableGoalStoreError> {
        let _guard = self.lock()?;
        self.read_locked(session_id)
    }

    /// Returns the latest durable revision, including a cleared goal's
    /// tombstone revision.
    pub fn revision(&self, session_id: &str) -> Result<u64, DurableGoalStoreError> {
        let _guard = self.lock()?;
        self.revision_locked(session_id)
    }

    /// Returns the current goal and its durable revision from one locked read.
    pub fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<(Option<GoalState>, u64), DurableGoalStoreError> {
        let _guard = self.lock()?;
        let stored = self.read_record_locked(session_id)?;
        let revision = stored
            .as_ref()
            .map(|stored| {
                stored
                    .state
                    .as_ref()
                    .map(|state| state.revision)
                    .unwrap_or(stored.revision)
            })
            .unwrap_or(0);
        let goal = stored.and_then(|stored| stored.state);
        Ok((goal, revision))
    }

    /// Creates or replaces a goal and resets its lifecycle counters.
    pub fn set(
        &self,
        session_id: &str,
        objective: &str,
        turn_budget: Option<u32>,
    ) -> Result<GoalState, DurableGoalStoreError> {
        let _guard = self.lock()?;
        validate_session_id(session_id)?;
        validate_objective(objective)?;
        validate_turn_budget(turn_budget)?;
        let revision = self
            .revision_locked(session_id)?
            .checked_add(1)
            .ok_or(DurableGoalStoreError::InvalidTransition)?;
        let state = GoalState {
            revision,
            objective: objective.trim().to_owned(),
            status: GoalStatus::Active,
            turn_budget,
            turns_used: 0,
            created_at: now_rfc3339(),
        };
        self.write_locked(session_id, &state)?;
        Ok(state)
    }

    /// Applies a pause, resume, or clear operation.
    pub fn apply(
        &self,
        session_id: &str,
        action: GoalAction,
    ) -> Result<Option<GoalState>, DurableGoalStoreError> {
        let _guard = self.lock()?;
        validate_session_id(session_id)?;
        if action == GoalAction::Clear {
            let Some(stored) = self.read_record_locked(session_id)? else {
                return Ok(None);
            };
            if stored.state.is_none() {
                // Clearing an already-cleared goal is idempotent and must not
                // advance the tombstone revision.
                return Ok(None);
            }
            let revision = stored
                .state
                .as_ref()
                .map(|state| state.revision)
                .unwrap_or(stored.revision)
                .checked_add(1)
                .ok_or(DurableGoalStoreError::InvalidTransition)?;
            self.write_record_locked(
                session_id,
                &StoredGoal {
                    version: STORE_VERSION,
                    state: None,
                    revision,
                },
            )?;
            return Ok(None);
        }
        let mut state = self
            .read_locked(session_id)?
            .ok_or(DurableGoalStoreError::NotFound)?;
        let previous_status = state.status;
        match (action, state.status) {
            (GoalAction::Pause, GoalStatus::Active)
            | (GoalAction::Pause, GoalStatus::Paused)
            | (GoalAction::Pause, GoalStatus::BudgetLimited) => state.status = GoalStatus::Paused,
            (GoalAction::Resume, GoalStatus::Paused) => {
                state.status = if state
                    .turn_budget
                    .is_some_and(|budget| state.turns_used >= budget)
                {
                    GoalStatus::BudgetLimited
                } else {
                    GoalStatus::Active
                };
            }
            (GoalAction::Resume, GoalStatus::Active) => state.status = GoalStatus::Active,
            _ => return Err(DurableGoalStoreError::InvalidTransition),
        }
        if state.status != previous_status || state.revision == 0 {
            bump_revision(&mut state)?;
        }
        self.write_locked(session_id, &state)?;
        Ok(Some(state))
    }

    /// Removes a goal for a deleted session. Missing goals are ignored.
    pub fn delete_session(&self, session_id: &str) -> Result<(), DurableGoalStoreError> {
        let _guard = self.lock()?;
        validate_session_id(session_id)?;
        self.remove_locked(session_id)
    }

    /// Records one continuation turn and enforces the configured budget.
    pub fn record_turn(&self, session_id: &str) -> Result<GoalState, DurableGoalStoreError> {
        let _guard = self.lock()?;
        validate_session_id(session_id)?;
        let mut state = self
            .read_locked(session_id)?
            .ok_or(DurableGoalStoreError::NotFound)?;
        if state.status != GoalStatus::Active
            || state
                .turn_budget
                .is_some_and(|budget| state.turns_used >= budget)
        {
            return Err(DurableGoalStoreError::InvalidTransition);
        }
        state.turns_used = state
            .turns_used
            .checked_add(1)
            .ok_or(DurableGoalStoreError::InvalidTransition)?;
        if state
            .turn_budget
            .is_some_and(|budget| state.turns_used >= budget)
        {
            state.status = GoalStatus::BudgetLimited;
        }
        bump_revision(&mut state)?;
        self.write_locked(session_id, &state)?;
        Ok(state)
    }

    /// Marks a goal complete.
    pub fn mark_complete(&self, session_id: &str) -> Result<GoalState, DurableGoalStoreError> {
        self.set_status(session_id, GoalStatus::Complete)
    }

    /// Marks a goal blocked.
    pub fn mark_blocked(&self, session_id: &str) -> Result<GoalState, DurableGoalStoreError> {
        self.set_status(session_id, GoalStatus::Blocked)
    }

    /// Pauses an active goal. Repeated pauses are idempotent.
    pub fn pause(&self, session_id: &str) -> Result<GoalState, DurableGoalStoreError> {
        self.apply(session_id, GoalAction::Pause)?
            .ok_or(DurableGoalStoreError::NotFound)
    }

    fn set_status(
        &self,
        session_id: &str,
        status: GoalStatus,
    ) -> Result<GoalState, DurableGoalStoreError> {
        let _guard = self.lock()?;
        validate_session_id(session_id)?;
        let mut state = self
            .read_locked(session_id)?
            .ok_or(DurableGoalStoreError::NotFound)?;
        if state.status != status || state.revision == 0 {
            state.status = status;
            bump_revision(&mut state)?;
            self.write_locked(session_id, &state)?;
        }
        Ok(state)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, DurableGoalStoreError> {
        self.inner.lock.lock().map_err(|_| {
            DurableGoalStoreError::Storage(std::io::Error::other("goal store lock poisoned"))
        })
    }

    fn goal_path(&self, session_id: &str) -> PathBuf {
        self.inner.root.join(format!("{session_id}.json"))
    }

    fn read_locked(&self, session_id: &str) -> Result<Option<GoalState>, DurableGoalStoreError> {
        Ok(self
            .read_record_locked(session_id)?
            .and_then(|stored| stored.state))
    }

    fn revision_locked(&self, session_id: &str) -> Result<u64, DurableGoalStoreError> {
        Ok(self
            .read_record_locked(session_id)?
            .map(|stored| {
                stored
                    .state
                    .as_ref()
                    .map(|state| state.revision)
                    .unwrap_or(stored.revision)
            })
            .unwrap_or(0))
    }

    fn read_record_locked(
        &self,
        session_id: &str,
    ) -> Result<Option<StoredGoal>, DurableGoalStoreError> {
        validate_session_id(session_id)?;
        let path = self.goal_path(session_id);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(DurableGoalStoreError::Storage(error)),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(DurableGoalStoreError::UnsafePath);
        }
        if metadata.len() > MAX_GOAL_FILE_BYTES {
            return Err(DurableGoalStoreError::CorruptState);
        }
        let file = open_read_no_follow(&path)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_GOAL_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(DurableGoalStoreError::Storage)?;
        if bytes.len() as u64 > MAX_GOAL_FILE_BYTES {
            return Err(DurableGoalStoreError::CorruptState);
        }
        let stored: StoredGoal =
            serde_json::from_slice(&bytes).map_err(|_| DurableGoalStoreError::CorruptState)?;
        if stored.version != STORE_VERSION {
            return Err(DurableGoalStoreError::CorruptState);
        }
        if let Some(state) = &stored.state {
            validate_state(state)?;
            if stored.revision != 0 && stored.revision != state.revision {
                return Err(DurableGoalStoreError::CorruptState);
            }
        } else if stored.revision == 0 {
            return Err(DurableGoalStoreError::CorruptState);
        }
        Ok(Some(stored))
    }

    fn write_locked(
        &self,
        session_id: &str,
        state: &GoalState,
    ) -> Result<(), DurableGoalStoreError> {
        validate_state(state)?;
        self.write_record_locked(
            session_id,
            &StoredGoal {
                version: STORE_VERSION,
                state: Some(state.clone()),
                revision: state.revision,
            },
        )
    }

    fn write_record_locked(
        &self,
        session_id: &str,
        stored: &StoredGoal,
    ) -> Result<(), DurableGoalStoreError> {
        validate_session_id(session_id)?;
        if let Some(state) = &stored.state {
            validate_state(state)?;
            if stored.revision != state.revision {
                return Err(DurableGoalStoreError::CorruptState);
            }
        } else if stored.revision == 0 {
            return Err(DurableGoalStoreError::CorruptState);
        }
        let bytes = serde_json::to_vec(stored).map_err(|_| DurableGoalStoreError::CorruptState)?;
        if bytes.len() as u64 > MAX_GOAL_FILE_BYTES {
            return Err(DurableGoalStoreError::CorruptState);
        }
        let temp_path = self.inner.root.join(format!(
            ".{session_id}.tmp-{}-{}",
            std::process::id(),
            next_temp_id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(GOAL_FILE_MODE).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(DurableGoalStoreError::Storage)?;
        let result = (|| -> std::io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()
        })();
        drop(file);
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp_path);
            return Err(DurableGoalStoreError::Storage(error));
        }
        if let Err(error) = std::fs::rename(&temp_path, self.goal_path(session_id)) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(DurableGoalStoreError::Storage(error));
        }
        sync_directory(&self.inner.root)
    }

    fn remove_locked(&self, session_id: &str) -> Result<(), DurableGoalStoreError> {
        let path = self.goal_path(session_id);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(DurableGoalStoreError::UnsafePath);
                }
                std::fs::remove_file(path).map_err(DurableGoalStoreError::Storage)?;
                sync_directory(&self.inner.root)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DurableGoalStoreError::Storage(error)),
        }
    }
}

impl GoalStore for DurableGoalStore {
    fn get(&self, session_id: &str) -> Result<Option<GoalState>, String> {
        self.get(session_id).map_err(|error| error.to_string())
    }

    fn record_turn(&self, session_id: &str) -> Result<GoalState, String> {
        self.record_turn(session_id)
            .map_err(|error| error.to_string())
    }

    fn mark_complete(&self, session_id: &str) -> Result<GoalState, String> {
        self.mark_complete(session_id)
            .map_err(|error| error.to_string())
    }

    fn mark_blocked(&self, session_id: &str) -> Result<GoalState, String> {
        self.mark_blocked(session_id)
            .map_err(|error| error.to_string())
    }

    fn pause(&self, session_id: &str) -> Result<GoalState, String> {
        self.pause(session_id).map_err(|error| error.to_string())
    }
}

fn validate_session_id(session_id: &str) -> Result<(), DurableGoalStoreError> {
    if session_id.is_empty()
        || session_id.len() > 256
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(DurableGoalStoreError::UnsafePath);
    }
    Ok(())
}

fn validate_objective(objective: &str) -> Result<(), DurableGoalStoreError> {
    let objective = objective.trim();
    if objective.is_empty()
        || objective.len() > MAX_GOAL_OBJECTIVE_BYTES
        || objective
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(DurableGoalStoreError::InvalidObjective);
    }
    Ok(())
}

fn validate_turn_budget(turn_budget: Option<u32>) -> Result<(), DurableGoalStoreError> {
    if turn_budget.is_some_and(|budget| budget == 0 || budget > MAX_TURN_BUDGET) {
        return Err(DurableGoalStoreError::InvalidTurnBudget);
    }
    Ok(())
}

fn bump_revision(state: &mut GoalState) -> Result<(), DurableGoalStoreError> {
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or(DurableGoalStoreError::InvalidTransition)?;
    Ok(())
}

fn validate_state(state: &GoalState) -> Result<(), DurableGoalStoreError> {
    validate_objective(&state.objective)?;
    validate_turn_budget(state.turn_budget)?;
    if state.created_at.is_empty()
        || state.created_at.len() > MAX_CREATED_AT_BYTES
        || state
            .created_at
            .chars()
            .any(|character| character.is_control())
        || state
            .turn_budget
            .is_some_and(|budget| state.turns_used > budget)
    {
        return Err(DurableGoalStoreError::CorruptState);
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), DurableGoalStoreError> {
    std::fs::create_dir_all(path).map_err(DurableGoalStoreError::Storage)?;
    let metadata = std::fs::symlink_metadata(path).map_err(DurableGoalStoreError::Storage)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(DurableGoalStoreError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(GOAL_DIRECTORY_MODE))
            .map_err(DurableGoalStoreError::Storage)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), DurableGoalStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(DurableGoalStoreError::Storage)
}

fn open_read_no_follow(path: &Path) -> Result<File, DurableGoalStoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(DurableGoalStoreError::Storage)
}

fn next_temp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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
        let store = DurableGoalStore::open(directory.path()).unwrap();
        let created = store.set("session", "Write the README", Some(2)).unwrap();
        assert_eq!(created.status, GoalStatus::Active);
        assert_eq!(
            DurableGoalStore::open(directory.path())
                .unwrap()
                .get("session")
                .unwrap(),
            Some(created)
        );
    }

    #[test]
    fn clear_retains_a_durable_revision_tombstone() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableGoalStore::open(directory.path()).unwrap();
        let created = store.set("session", "Ship it", None).unwrap();
        assert_eq!(created.revision, 1);

        assert_eq!(store.apply("session", GoalAction::Clear).unwrap(), None);
        assert_eq!(store.snapshot("session").unwrap(), (None, 2));
        assert_eq!(
            DurableGoalStore::open(directory.path())
                .unwrap()
                .revision("session")
                .unwrap(),
            2
        );
        assert_eq!(store.apply("session", GoalAction::Clear).unwrap(), None);
        assert_eq!(
            store
                .set("session", "Ship it again", None)
                .unwrap()
                .revision,
            3
        );
    }
    #[test]
    fn clear_is_idempotent_and_budget_is_durable() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableGoalStore::open(directory.path()).unwrap();
        store.set("session", "Verify the build", Some(1)).unwrap();
        assert_eq!(
            store.record_turn("session").unwrap().status,
            GoalStatus::BudgetLimited
        );
        assert_eq!(store.apply("session", GoalAction::Clear).unwrap(), None);
        assert_eq!(store.apply("session", GoalAction::Clear).unwrap(), None);
    }
}
