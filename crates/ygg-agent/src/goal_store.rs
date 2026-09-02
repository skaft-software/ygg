//! Provider-neutral durable persistence for session goals.
//!
//! The graphical Serve extension has its own public goal DTOs, but the on-disk
//! format is deliberately small and shared with this store. Keeping this
//! implementation in `ygg-agent` lets the terminal frontend use durable goals
//! without depending on the optional Serve crate.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::goal_driver::{GoalState, GoalStatus, GoalStore};

const STORE_VERSION: u16 = 1;
const GOAL_LOCK_FILE_NAME: &str = ".goal.lock";
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
    lock_path: PathBuf,
    lock_file: File,
    lock: Mutex<()>,
}

struct GoalStoreLock<'a> {
    _process: MutexGuard<'a, ()>,
    file: &'a File,
    path: &'a Path,
    identity: crate::secure_fs::PrivateLockIdentity,
    unlocked: bool,
}

impl GoalStoreLock<'_> {
    fn revalidate(&self) -> Result<(), DurableGoalStoreError> {
        crate::secure_fs::revalidate_private_lock_before_release(
            self.path,
            self.file,
            &self.identity,
        )
        .map_err(map_lock_error)
    }

    fn finish(mut self) -> Result<(), DurableGoalStoreError> {
        // Revalidate before releasing the OS lock, and attempt the release even
        // when revalidation fails. Keep the first error because a replaced lock
        // object is the transaction's primary safety failure.
        let revalidation = self.revalidate();
        let unlock = fs2::FileExt::unlock(self.file).map_err(DurableGoalStoreError::Storage);
        self.unlocked = unlock.is_ok();
        revalidation.and(unlock)
    }
}

impl Drop for GoalStoreLock<'_> {
    fn drop(&mut self) {
        // Transaction completion is explicit and fallible. Drop only provides
        // best-effort cleanup for errors and unwinding.
        if !self.unlocked {
            let _ = fs2::FileExt::unlock(self.file);
        }
    }
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
        let lock_path = root.join(GOAL_LOCK_FILE_NAME);
        let lock_file =
            crate::secure_fs::open_private_lock_file(&lock_path).map_err(map_lock_error)?;
        Ok(Self {
            inner: Arc::new(DurableGoalStoreInner {
                root: root.to_owned(),
                lock_path,
                lock_file,
                lock: Mutex::new(()),
            }),
        })
    }

    /// Returns the current goal, if one exists.
    pub fn get(&self, session_id: &str) -> Result<Option<GoalState>, DurableGoalStoreError> {
        self.transaction(|_| self.read_locked(session_id))
    }

    /// Returns the latest durable revision, including a cleared goal's
    /// tombstone revision.
    pub fn revision(&self, session_id: &str) -> Result<u64, DurableGoalStoreError> {
        self.transaction(|_| self.revision_locked(session_id))
    }

    /// Returns the current goal and its durable revision from one locked read.
    pub fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<(Option<GoalState>, u64), DurableGoalStoreError> {
        self.transaction(|_| {
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
        })
    }

    /// Creates or replaces a goal and resets its lifecycle counters.
    pub fn set(
        &self,
        session_id: &str,
        objective: &str,
        turn_budget: Option<u32>,
    ) -> Result<GoalState, DurableGoalStoreError> {
        self.transaction(|lock| {
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
            self.write_locked(session_id, &state, lock)?;
            Ok(state)
        })
    }

    /// Applies a pause, resume, or clear operation.
    pub fn apply(
        &self,
        session_id: &str,
        action: GoalAction,
    ) -> Result<Option<GoalState>, DurableGoalStoreError> {
        self.transaction(|lock| {
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
                    lock,
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
                | (GoalAction::Pause, GoalStatus::BudgetLimited) => {
                    state.status = GoalStatus::Paused;
                }
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
            self.write_locked(session_id, &state, lock)?;
            Ok(Some(state))
        })
    }

    /// Removes a goal for a deleted session. Missing goals are ignored.
    pub fn delete_session(&self, session_id: &str) -> Result<(), DurableGoalStoreError> {
        self.transaction(|lock| {
            validate_session_id(session_id)?;
            self.remove_locked(session_id, lock)
        })
    }

    /// Records one continuation turn and enforces the configured budget.
    pub fn record_turn(&self, session_id: &str) -> Result<GoalState, DurableGoalStoreError> {
        self.transaction(|lock| {
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
            self.write_locked(session_id, &state, lock)?;
            Ok(state)
        })
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
        self.transaction(|lock| {
            validate_session_id(session_id)?;
            let mut state = self
                .read_locked(session_id)?
                .ok_or(DurableGoalStoreError::NotFound)?;
            if state.status != status || state.revision == 0 {
                state.status = status;
                bump_revision(&mut state)?;
                self.write_locked(session_id, &state, lock)?;
            }
            Ok(state)
        })
    }

    fn transaction<T>(
        &self,
        operation: impl FnOnce(&GoalStoreLock<'_>) -> Result<T, DurableGoalStoreError>,
    ) -> Result<T, DurableGoalStoreError> {
        let lock = self.lock()?;
        let result = operation(&lock);
        match (result, lock.finish()) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) | (Err(error), _) => Err(error),
        }
    }

    fn lock(&self) -> Result<GoalStoreLock<'_>, DurableGoalStoreError> {
        // Keep the existing mutex for clones of one store and add the OS lock
        // for stores opened independently by another handle or process. Both
        // locks must cover the whole read/validate/mutate/publish operation.
        let process_guard = self.inner.lock.lock().map_err(|_| {
            DurableGoalStoreError::Storage(std::io::Error::other("goal store lock poisoned"))
        })?;
        let file = &self.inner.lock_file;
        file.lock_exclusive()
            .map_err(DurableGoalStoreError::Storage)?;
        let identity = match crate::secure_fs::validate_private_lock_after_acquire(
            &self.inner.lock_path,
            file,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = fs2::FileExt::unlock(file);
                return Err(map_lock_error(error));
            }
        };
        Ok(GoalStoreLock {
            _process: process_guard,
            file,
            path: &self.inner.lock_path,
            identity,
            unlocked: false,
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
        lock: &GoalStoreLock<'_>,
    ) -> Result<(), DurableGoalStoreError> {
        validate_state(state)?;
        self.write_record_locked(
            session_id,
            &StoredGoal {
                version: STORE_VERSION,
                state: Some(state.clone()),
                revision: state.revision,
            },
            lock,
        )
    }

    fn write_record_locked(
        &self,
        session_id: &str,
        stored: &StoredGoal,
        lock: &GoalStoreLock<'_>,
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
        if let Err(error) = lock.revalidate() {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&temp_path, self.goal_path(session_id)) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(DurableGoalStoreError::Storage(error));
        }
        sync_directory(&self.inner.root)
    }

    fn remove_locked(
        &self,
        session_id: &str,
        lock: &GoalStoreLock<'_>,
    ) -> Result<(), DurableGoalStoreError> {
        let path = self.goal_path(session_id);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(DurableGoalStoreError::UnsafePath);
                }
                lock.revalidate()?;
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

fn map_lock_error(error: crate::secure_fs::SecureFileError) -> DurableGoalStoreError {
    match error {
        crate::secure_fs::SecureFileError::InvalidPath(_)
        | crate::secure_fs::SecureFileError::NotRegular
        | crate::secure_fs::SecureFileError::InsecurePrivateObject(_)
        | crate::secure_fs::SecureFileError::Changed => DurableGoalStoreError::UnsafePath,
        crate::secure_fs::SecureFileError::Io(error) => DurableGoalStoreError::Storage(error),
        error => DurableGoalStoreError::Storage(std::io::Error::other(error.to_string())),
    }
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
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    #[cfg(unix)]
    fn wait_for_marker(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if path.is_file() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_waits_for_other_handle<F>(root: &Path, operation: F)
    where
        F: FnOnce(DurableGoalStore) -> Result<(), DurableGoalStoreError> + Send + 'static,
    {
        let holder = DurableGoalStore::open(root).unwrap();
        let contender = DurableGoalStore::open(root).unwrap();
        let held = holder.lock().unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (done_tx, done_rx) = mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(operation(contender).map_err(|error| error.to_string()))
                .unwrap();
        });

        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        drop(held);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(())
        );
        worker.join().unwrap();
    }

    fn replace_lock_while_held(store: &DurableGoalStore) -> File {
        let old_lock = crate::secure_fs::open_private_lock_file(&store.inner.lock_path).unwrap();
        let error = fs2::FileExt::try_lock_exclusive(&old_lock).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        std::fs::remove_file(&store.inner.lock_path).unwrap();
        let replacement = crate::secure_fs::open_private_lock_file(&store.inner.lock_path).unwrap();
        drop(replacement);
        old_lock
    }

    fn assert_lock_released(lock: File) {
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        fs2::FileExt::unlock(&lock).unwrap();
    }

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

    #[test]
    fn successful_transaction_reports_replaced_lock_and_releases_original() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableGoalStore::open(directory.path()).unwrap();
        let mut old_lock = None;

        let result: Result<(), DurableGoalStoreError> = store.transaction(|_| {
            old_lock = Some(replace_lock_while_held(&store));
            Ok(())
        });

        assert!(matches!(result, Err(DurableGoalStoreError::UnsafePath)));
        assert_lock_released(old_lock.unwrap());
    }

    #[test]
    fn replaced_lock_prevents_goal_publication_and_releases_original() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableGoalStore::open(directory.path()).unwrap();
        let goal_path = store.goal_path("session");
        let mut old_lock = None;

        let result = store.transaction(|lock| {
            old_lock = Some(replace_lock_while_held(&store));
            let state = GoalState {
                revision: 1,
                objective: "Do not publish".to_owned(),
                status: GoalStatus::Active,
                turn_budget: None,
                turns_used: 0,
                created_at: now_rfc3339(),
            };
            store.write_locked("session", &state, lock)
        });

        assert!(matches!(result, Err(DurableGoalStoreError::UnsafePath)));
        assert!(!goal_path.exists());
        assert_lock_released(old_lock.unwrap());
    }

    #[test]
    fn replaced_lock_prevents_goal_removal_and_releases_original() {
        let directory = tempfile::tempdir().unwrap();
        let store = DurableGoalStore::open(directory.path()).unwrap();
        store.set("session", "Keep this goal", None).unwrap();
        let goal_path = store.goal_path("session");
        let original = std::fs::read(&goal_path).unwrap();
        let mut old_lock = None;

        let result = store.transaction(|lock| {
            old_lock = Some(replace_lock_while_held(&store));
            store.remove_locked("session", lock)
        });

        assert!(matches!(result, Err(DurableGoalStoreError::UnsafePath)));
        assert_eq!(std::fs::read(goal_path).unwrap(), original);
        assert_lock_released(old_lock.unwrap());
    }

    #[test]
    fn independent_handles_serialize_each_goal_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        assert_waits_for_other_handle(root, |store| {
            let state = store.set("set", "set objective", None)?;
            assert_eq!(state.revision, 1);
            Ok(())
        });

        let store = DurableGoalStore::open(root).unwrap();
        store.set("apply", "apply objective", None).unwrap();
        drop(store);
        assert_waits_for_other_handle(root, |store| {
            let state = store.apply("apply", GoalAction::Pause)?.unwrap();
            assert_eq!(state.status, GoalStatus::Paused);
            assert_eq!(state.revision, 2);
            Ok(())
        });
        assert_waits_for_other_handle(root, |store| {
            let state = store.apply("apply", GoalAction::Resume)?.unwrap();
            assert_eq!(state.status, GoalStatus::Active);
            assert_eq!(state.revision, 3);
            Ok(())
        });

        let store = DurableGoalStore::open(root).unwrap();
        store.set("turn", "turn objective", None).unwrap();
        drop(store);
        assert_waits_for_other_handle(root, |store| {
            let state = store.record_turn("turn")?;
            assert_eq!(state.turns_used, 1);
            assert_eq!(state.revision, 2);
            Ok(())
        });

        let store = DurableGoalStore::open(root).unwrap();
        store.set("complete", "complete objective", None).unwrap();
        drop(store);
        assert_waits_for_other_handle(root, |store| {
            let state = store.mark_complete("complete")?;
            assert_eq!(state.status, GoalStatus::Complete);
            assert_eq!(state.revision, 2);
            Ok(())
        });

        let store = DurableGoalStore::open(root).unwrap();
        store.set("blocked", "blocked objective", None).unwrap();
        drop(store);
        assert_waits_for_other_handle(root, |store| {
            let state = store.mark_blocked("blocked")?;
            assert_eq!(state.status, GoalStatus::Blocked);
            assert_eq!(state.revision, 2);
            Ok(())
        });

        let store = DurableGoalStore::open(root).unwrap();
        store.set("clear", "clear objective", None).unwrap();
        drop(store);
        assert_waits_for_other_handle(root, |store| {
            assert_eq!(store.apply("clear", GoalAction::Clear)?, None);
            Ok(())
        });
        assert_eq!(
            DurableGoalStore::open(root)
                .unwrap()
                .snapshot("clear")
                .unwrap(),
            (None, 2)
        );

        let store = DurableGoalStore::open(root).unwrap();
        store.set("delete", "delete objective", None).unwrap();
        drop(store);
        assert_waits_for_other_handle(root, |store| {
            store.delete_session("delete")?;
            Ok(())
        });
        let store = DurableGoalStore::open(root).unwrap();
        assert_eq!(store.get("delete").unwrap(), None);
        assert_eq!(store.revision("delete").unwrap(), 0);
    }

    #[test]
    fn independent_handles_do_not_lose_turns_or_revisions() {
        const TURN_COUNT: usize = 16;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let store = DurableGoalStore::open(root).unwrap();
        store
            .set("turns", "record all turns", Some(TURN_COUNT as u32))
            .unwrap();
        drop(store);

        let barrier = Arc::new(Barrier::new(TURN_COUNT));
        let mut workers = Vec::with_capacity(TURN_COUNT);
        for _ in 0..TURN_COUNT {
            let store = DurableGoalStore::open(root).unwrap();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                store.record_turn("turns")
            }));
        }

        let mut states = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        states.sort_by_key(|state| state.revision);
        assert_eq!(
            states
                .iter()
                .map(|state| state.revision)
                .collect::<Vec<_>>(),
            (2..=(TURN_COUNT as u64 + 1)).collect::<Vec<_>>()
        );
        assert_eq!(
            states
                .iter()
                .map(|state| state.turns_used)
                .collect::<Vec<_>>(),
            (1..=TURN_COUNT as u32).collect::<Vec<_>>()
        );

        let state = DurableGoalStore::open(root)
            .unwrap()
            .get("turns")
            .unwrap()
            .unwrap();
        assert_eq!(state.turns_used, TURN_COUNT as u32);
        assert_eq!(state.revision, TURN_COUNT as u64 + 1);
        assert_eq!(state.status, GoalStatus::BudgetLimited);
    }

    #[test]
    fn independent_handles_preserve_set_and_status_revision_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let barrier = Arc::new(Barrier::new(2));
        let first = DurableGoalStore::open(root).unwrap();
        let second = DurableGoalStore::open(root).unwrap();
        let first_barrier = Arc::clone(&barrier);
        let first_worker = thread::spawn(move || {
            first_barrier.wait();
            first.set("set-race", "first", None)
        });
        let second_barrier = Arc::clone(&barrier);
        let second_worker = thread::spawn(move || {
            second_barrier.wait();
            second.set("set-race", "second", None)
        });
        let mut set_states = vec![first_worker.join().unwrap().unwrap()];
        set_states.push(second_worker.join().unwrap().unwrap());
        set_states.sort_by_key(|state| state.revision);
        assert_eq!(
            set_states
                .iter()
                .map(|state| state.revision)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let state = DurableGoalStore::open(root)
            .unwrap()
            .get("set-race")
            .unwrap()
            .unwrap();
        assert_eq!(state.revision, 2);
        assert!(matches!(state.objective.as_str(), "first" | "second"));

        DurableGoalStore::open(root)
            .unwrap()
            .set("status-race", "status", None)
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let first = DurableGoalStore::open(root).unwrap();
        let second = DurableGoalStore::open(root).unwrap();
        let first_barrier = Arc::clone(&barrier);
        let first_worker = thread::spawn(move || {
            first_barrier.wait();
            first.mark_complete("status-race")
        });
        let second_barrier = Arc::clone(&barrier);
        let second_worker = thread::spawn(move || {
            second_barrier.wait();
            second.mark_blocked("status-race")
        });
        let mut status_states = vec![first_worker.join().unwrap().unwrap()];
        status_states.push(second_worker.join().unwrap().unwrap());
        status_states.sort_by_key(|state| state.revision);
        assert_eq!(
            status_states
                .iter()
                .map(|state| state.revision)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        let state = DurableGoalStore::open(root)
            .unwrap()
            .get("status-race")
            .unwrap()
            .unwrap();
        assert_eq!(state.revision, 3);
        assert!(matches!(
            state.status,
            GoalStatus::Complete | GoalStatus::Blocked
        ));
    }

    #[cfg(unix)]
    #[test]
    fn independent_processes_respect_goal_store_lock() {
        const CHILD_ENV: &str = "YGG_GOAL_STORE_PROCESS_CHILD";
        const ROOT_ENV: &str = "YGG_GOAL_STORE_PROCESS_ROOT";
        const READY_ENV: &str = "YGG_GOAL_STORE_PROCESS_READY";
        const START_ENV: &str = "YGG_GOAL_STORE_PROCESS_START";
        const DONE_ENV: &str = "YGG_GOAL_STORE_PROCESS_DONE";

        if std::env::var_os(CHILD_ENV).is_some() {
            let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("missing child root"));
            let ready = PathBuf::from(std::env::var_os(READY_ENV).expect("missing ready marker"));
            let start = PathBuf::from(std::env::var_os(START_ENV).expect("missing start marker"));
            let done = PathBuf::from(std::env::var_os(DONE_ENV).expect("missing done marker"));
            let store = DurableGoalStore::open(&root).unwrap();

            std::fs::write(&ready, b"opened").unwrap();
            assert!(
                wait_for_marker(&start, Duration::from_secs(5)),
                "parent did not start the child transaction"
            );
            let state = store.record_turn("session").unwrap();
            std::fs::write(
                &done,
                format!(
                    "turns_used={} revision={}",
                    state.turns_used, state.revision
                ),
            )
            .unwrap();
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let store = DurableGoalStore::open(root).unwrap();
        store
            .set("session", "record a process-boundary turn", Some(2))
            .unwrap();

        let ready = root.join(".child-ready");
        let start = root.join(".child-start");
        let done = root.join(".child-done");
        let held = store.lock().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "goal_store::tests::independent_processes_respect_goal_store_lock",
            ])
            .env(CHILD_ENV, "1")
            .env(ROOT_ENV, root.as_os_str())
            .env(READY_ENV, ready.as_os_str())
            .env(START_ENV, start.as_os_str())
            .env(DONE_ENV, done.as_os_str())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let child_ready = wait_for_marker(&ready, Duration::from_secs(5));
        std::fs::write(&start, b"start").unwrap();
        let child_completed_while_locked =
            child_ready && wait_for_marker(&done, Duration::from_millis(250));
        drop(held);

        let output = child.wait_with_output().unwrap();
        let child_stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            child_ready,
            "child did not reach the transaction boundary; stderr:\n{child_stderr}"
        );
        assert!(
            !child_completed_while_locked,
            "child completed while the parent held the goal-store lock"
        );
        assert!(
            output.status.success(),
            "child process failed; stderr:\n{child_stderr}"
        );
        assert_eq!(
            std::fs::read_to_string(&done).unwrap(),
            "turns_used=1 revision=2"
        );
        let state = store.get("session").unwrap().unwrap();
        assert_eq!(state.turns_used, 1);
        assert_eq!(state.revision, 2);
    }
}
