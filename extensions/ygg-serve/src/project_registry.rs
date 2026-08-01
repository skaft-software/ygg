//! Durable, server-private project root registry.
//!
//! This module intentionally has no dependency on the Serve protocol. The
//! first-party host adapter translates [`ProjectSummary`] into a public wire
//! DTO while keeping [`ProjectRoot`] on the trusted server side.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

const STATE_VERSION: u16 = 1;
const STATE_FILE_NAME: &str = "projects.json";
const TEMP_FILE_PREFIX: &str = ".projects.tmp-";
const PROJECT_ID_PREFIX: &str = "prj_";
const PROJECT_ID_RANDOM_BYTES: usize = 16;
const PROJECT_ID_HEX_BYTES: usize = PROJECT_ID_RANDOM_BYTES * 2;
/// Maximum project count shared with the public host bootstrap contract.
pub const MAX_PROJECTS: usize = 256;
const MAX_SESSION_BINDINGS: usize = 20_000;
const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_DISPLAY_NAME_BYTES: usize = 160;
const MAX_CANONICAL_ROOT_BYTES: usize = 8_192;

/// Maximum accepted size of the private registry state file.
pub const MAX_REGISTRY_STATE_BYTES: u64 = 1024 * 1024;

/// Opaque, randomly minted project identity.
///
/// IDs are deliberately independent of a project's path so renaming a label
/// cannot disclose or alter filesystem authority.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    /// Parses and validates a stored or wire project ID.
    pub fn parse(value: impl Into<String>) -> Result<Self, ProjectRegistryError> {
        let value = value.into();
        if valid_project_id(&value) {
            Ok(Self(value))
        } else {
            Err(ProjectRegistryError::InvalidProjectId)
        }
    }

    /// Returns the opaque identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Safe public lifecycle and trust state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectState {
    /// The root is active but has not been granted project trust.
    Untrusted,
    /// The root is active and an owner explicitly granted project trust.
    Trusted,
    /// The project is archived and cannot be selected or trusted.
    Archived,
}

/// Public project metadata that contains no filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSummary {
    /// Stable opaque project identity.
    pub id: ProjectId,
    /// Bounded user-facing label.
    pub display_name: String,
    /// Trust and archive state.
    pub state: ProjectState,
    /// Whether this project is the current registry default.
    pub is_default: bool,
    /// Whether the originally imported directory identity is currently usable.
    pub available: bool,
}

/// A server-side project root capability.
///
/// This type intentionally does not implement `Serialize`, and its `Debug`
/// representation is redacted. It should be consumed only by trusted host
/// code after a project ID has been resolved.
#[derive(Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    path: PathBuf,
    identity: StoredRootIdentity,
}

impl ProjectRoot {
    /// Returns the canonical server-side path.
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Checks an opened root descriptor against the imported directory identity.
    pub(crate) fn matches_metadata(&self, metadata: &std::fs::Metadata) -> bool {
        root_identity_matches(self.identity, metadata)
    }
}

impl fmt::Debug for ProjectRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProjectRoot(<redacted>)")
    }
}

/// Registry and root-validation failures.
#[derive(Debug, Error)]
pub enum ProjectRegistryError {
    /// The state or root path was relative.
    #[error("the path must be absolute")]
    RelativePath,
    /// The supplied path contained a lexical parent traversal.
    #[error("parent traversal is not accepted")]
    PathTraversal,
    /// The private state directory is missing a usable parent.
    #[error("the private state directory parent is unavailable")]
    StateParentUnavailable,
    /// A private state path was a symlink or another unexpected file type.
    #[error("the private registry state path is unsafe")]
    UnsafeStatePath,
    /// State directory or file permissions grant group or other access.
    #[error("the private registry state permissions are unsafe")]
    UnsafePermissions,
    /// The state file exceeded its fixed bound.
    #[error("the project registry state exceeds its size limit")]
    StateTooLarge,
    /// The state file was malformed or violated registry invariants.
    #[error("the project registry state is corrupt")]
    CorruptState,
    /// The state file uses an unsupported schema version.
    #[error("the project registry state version is unsupported")]
    UnsupportedStateVersion,
    /// The stored revision counter cannot advance.
    #[error("the project registry revision is exhausted")]
    RevisionExhausted,
    /// A project ID was malformed.
    #[error("the project ID is invalid")]
    InvalidProjectId,
    /// Secure randomness was unavailable while minting an ID.
    #[error("a project ID could not be minted")]
    RandomnessUnavailable,
    /// The project limit was reached.
    #[error("the project registry is full")]
    ProjectLimitReached,
    /// A display name was empty, unsafe, or too large.
    #[error("the project display name is invalid")]
    InvalidDisplayName,
    /// The imported root does not exist.
    #[error("the project root is unavailable")]
    RootUnavailable,
    /// The imported root was a symbolic link.
    #[error("symbolic links cannot be imported as project roots")]
    RootSymlink,
    /// The imported root was not a directory.
    #[error("the project root must be a directory")]
    RootNotDirectory,
    /// The canonical root could not be represented safely in state.
    #[error("the project root cannot be represented safely")]
    InvalidCanonicalRoot,
    /// The project root overlaps the registry's private state.
    #[error("the project root overlaps private registry state")]
    RootOverlapsState,
    /// The canonical root is already registered.
    #[error("the canonical project root is already registered")]
    DuplicateRoot,
    /// The directory at a stored root is no longer the imported directory.
    #[error("the project root identity changed")]
    RootIdentityChanged,
    /// The project ID is not registered.
    #[error("the project was not found")]
    ProjectNotFound,
    /// The session identity cannot be persisted safely.
    #[error("the session ID is invalid")]
    InvalidSessionId,
    /// A session cannot be assigned to two project roots.
    #[error("the session is already bound to another project")]
    SessionAlreadyBound,
    /// The session binding limit was reached.
    #[error("the project session binding registry is full")]
    SessionBindingLimitReached,
    /// Archived projects cannot perform this operation.
    #[error("the project is archived")]
    ProjectArchived,
    /// Execution cannot begin until the owner grants trust.
    #[error("the project is not trusted")]
    ProjectUntrusted,
    /// A private storage operation failed.
    #[error("the project registry storage operation failed")]
    Storage(#[source] std::io::Error),
}

impl From<std::io::Error> for ProjectRegistryError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRegistry {
    version: u16,
    revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_project_id: Option<ProjectId>,
    projects: Vec<StoredProject>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    session_projects: BTreeMap<String, ProjectId>,
}

impl Default for StoredRegistry {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            revision: 0,
            default_project_id: None,
            projects: Vec::new(),
            session_projects: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredProject {
    id: ProjectId,
    display_name: String,
    canonical_root: PathBuf,
    root_identity: StoredRootIdentity,
    trusted: bool,
    archived: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRootIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<u64>,
}

/// Single-writer durable registry for local project roots.
///
/// Mutations first build a validated candidate state, atomically replace the
/// private state file, and only then update the in-memory view. Callers should
/// serialize access to one instance; cross-process writer locking belongs at
/// the host integration boundary.
pub struct ProjectRegistry {
    state_directory: PathBuf,
    state_path: PathBuf,
    state: StoredRegistry,
}

impl fmt::Debug for ProjectRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectRegistry")
            .field("state_directory", &"<redacted>")
            .field("revision", &self.state.revision)
            .field("project_count", &self.state.projects.len())
            .finish()
    }
}

impl ProjectRegistry {
    /// Opens or creates a registry beneath an owner-private state directory.
    pub fn open(state_directory: impl AsRef<Path>) -> Result<Self, ProjectRegistryError> {
        let requested = clean_absolute_path(state_directory.as_ref())?;
        let state_directory = ensure_private_state_directory(&requested)?;
        cleanup_stale_temporary_files(&state_directory);
        let state_path = state_directory.join(STATE_FILE_NAME);
        let state = match read_state(&state_path)? {
            Some(state) => state,
            None => {
                let state = StoredRegistry::default();
                persist_state(&state_directory, &state_path, &state)?;
                state
            }
        };
        validate_state(&state)?;
        validate_state_boundaries(&state, &state_directory)?;
        Ok(Self {
            state_directory,
            state_path,
            state,
        })
    }

    /// Lists active and archived projects without exposing their roots.
    pub fn list(&self) -> Vec<ProjectSummary> {
        self.state
            .projects
            .iter()
            .map(|project| self.summary(project))
            .collect()
    }

    /// Returns safe public metadata for one project.
    pub fn get(&self, id: &ProjectId) -> Option<ProjectSummary> {
        self.project(id).map(|project| self.summary(project))
    }

    /// Returns the current default project, if one is configured.
    pub fn default_project(&self) -> Option<ProjectSummary> {
        self.state
            .default_project_id
            .as_ref()
            .and_then(|id| self.get(id))
    }

    /// Imports an existing real directory as an initially untrusted project.
    pub fn import(
        &mut self,
        root: impl AsRef<Path>,
        display_name: Option<&str>,
    ) -> Result<ProjectSummary, ProjectRegistryError> {
        self.import_with_sessions(root, display_name, std::iter::empty::<&str>())
    }

    /// Imports a directory and binds its existing durable session IDs in one
    /// atomic registry replacement.
    pub fn import_with_sessions<'a>(
        &mut self,
        root: impl AsRef<Path>,
        display_name: Option<&str>,
        session_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<ProjectSummary, ProjectRegistryError> {
        if self.state.projects.len() >= MAX_PROJECTS {
            return Err(ProjectRegistryError::ProjectLimitReached);
        }
        let canonical_root = validate_import_root(root.as_ref(), &self.state_directory)?;
        if self
            .state
            .projects
            .iter()
            .any(|project| project.canonical_root == canonical_root)
        {
            return Err(ProjectRegistryError::DuplicateRoot);
        }
        let display_name = match display_name {
            Some(display_name) => normalize_display_name(display_name)?,
            None => default_display_name(&canonical_root)?,
        };
        let metadata = canonical_root
            .symlink_metadata()
            .map_err(map_root_metadata_error)?;
        let id = self.mint_project_id()?;
        let project = StoredProject {
            id: id.clone(),
            display_name,
            canonical_root,
            root_identity: capture_root_identity(&metadata),
            trusted: false,
            archived: false,
        };
        let mut next = self.state.clone();
        next.projects.push(project);
        bind_sessions_in_state(&mut next, &id, session_ids)?;
        self.commit(next)?;
        self.get(&id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Finds the project already registered for an exact real directory.
    ///
    /// The supplied path is canonicalized and validated exactly like a fresh
    /// import. Only path-free public metadata is returned.
    pub fn find_by_root(
        &self,
        root: impl AsRef<Path>,
    ) -> Result<Option<ProjectSummary>, ProjectRegistryError> {
        let canonical_root = validate_import_root(root.as_ref(), &self.state_directory)?;
        Ok(self
            .state
            .projects
            .iter()
            .find(|project| project.canonical_root == canonical_root)
            .map(|project| self.summary(project)))
    }

    /// Atomically binds one durable session to a registered project.
    pub fn bind_session(
        &mut self,
        session_id: &str,
        id: &ProjectId,
    ) -> Result<(), ProjectRegistryError> {
        self.bind_sessions(id, std::iter::once(session_id))
    }

    /// Atomically binds durable sessions to a registered project.
    pub fn bind_sessions<'a>(
        &mut self,
        id: &ProjectId,
        session_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), ProjectRegistryError> {
        self.project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        let mut next = self.state.clone();
        let revision_before = next.session_projects.len();
        bind_sessions_in_state(&mut next, id, session_ids)?;
        if next.session_projects.len() != revision_before {
            self.commit(next)?;
        }
        Ok(())
    }

    /// Returns the project bound to one durable session.
    pub fn project_for_session(&self, session_id: &str) -> Option<ProjectId> {
        self.state.session_projects.get(session_id).cloned()
    }

    /// Atomically removes one durable session binding, returning its previous
    /// project so a higher-level transactional delete can restore it on
    /// failure.
    pub fn unbind_session(
        &mut self,
        session_id: &str,
    ) -> Result<Option<ProjectId>, ProjectRegistryError> {
        let mut next = self.state.clone();
        let previous = next.session_projects.remove(session_id);
        if previous.is_some() {
            self.commit(next)?;
        }
        Ok(previous)
    }

    /// Returns bounded durable session IDs assigned to one project.
    pub fn sessions_for_project(&self, id: &ProjectId) -> Vec<String> {
        self.state
            .session_projects
            .iter()
            .filter_map(|(session_id, project_id)| (project_id == id).then_some(session_id.clone()))
            .collect()
    }

    /// Updates a project's bounded public display name.
    pub fn update_display_name(
        &mut self,
        id: &ProjectId,
        display_name: &str,
    ) -> Result<ProjectSummary, ProjectRegistryError> {
        let display_name = normalize_display_name(display_name)?;
        let mut next = self.state.clone();
        let project = project_mut(&mut next, id)?;
        if project.display_name != display_name {
            project.display_name = display_name;
            self.commit(next)?;
        }
        self.get(id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Makes an active, available project the registry default.
    pub fn set_default(&mut self, id: &ProjectId) -> Result<ProjectSummary, ProjectRegistryError> {
        let project = self
            .project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        ensure_active(project)?;
        validate_live_root(project)?;
        if self.state.default_project_id.as_ref() != Some(id) {
            let mut next = self.state.clone();
            next.default_project_id = Some(id.clone());
            self.commit(next)?;
        }
        self.get(id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Clears the registry default without changing any project.
    pub fn clear_default(&mut self) -> Result<(), ProjectRegistryError> {
        if self.state.default_project_id.is_some() {
            let mut next = self.state.clone();
            next.default_project_id = None;
            self.commit(next)?;
        }
        Ok(())
    }

    /// Archives a project, revokes its trust, and clears it as default.
    pub fn archive(&mut self, id: &ProjectId) -> Result<ProjectSummary, ProjectRegistryError> {
        let mut next = self.state.clone();
        let project = project_mut(&mut next, id)?;
        let changed = !project.archived || project.trusted;
        project.archived = true;
        project.trusted = false;
        if next.default_project_id.as_ref() == Some(id) {
            next.default_project_id = None;
        }
        if changed || self.state.default_project_id.as_ref() == Some(id) {
            self.commit(next)?;
        }
        self.get(id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Rebinds an explicitly selected launch root after its directory identity
    /// changed, preserving the opaque project ID and session bindings.
    ///
    /// The canonical path must remain the same. A changed identity revokes
    /// trust before the caller explicitly grants it again, so replacing a
    /// directory can never silently retain execution authority.
    pub fn rebind_root(
        &mut self,
        id: &ProjectId,
        root: impl AsRef<Path>,
    ) -> Result<ProjectSummary, ProjectRegistryError> {
        let canonical_root = validate_import_root(root.as_ref(), &self.state_directory)?;
        let current = self
            .project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        ensure_active(current)?;
        if current.canonical_root != canonical_root {
            return Err(ProjectRegistryError::RootIdentityChanged);
        }
        let metadata = canonical_root
            .symlink_metadata()
            .map_err(map_root_metadata_error)?;
        let identity = capture_root_identity(&metadata);
        if current.root_identity != identity {
            let mut next = self.state.clone();
            let project = project_mut(&mut next, id)?;
            project.root_identity = identity;
            project.trusted = false;
            self.commit(next)?;
        }
        self.get(id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Grants trust to the exact imported directory identity.
    pub fn grant_trust(&mut self, id: &ProjectId) -> Result<ProjectSummary, ProjectRegistryError> {
        let current = self
            .project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        ensure_active(current)?;
        validate_live_root(current)?;
        if !current.trusted {
            let mut next = self.state.clone();
            project_mut(&mut next, id)?.trusted = true;
            self.commit(next)?;
        }
        self.get(id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Revokes trust even when a project is archived or temporarily missing.
    pub fn revoke_trust(&mut self, id: &ProjectId) -> Result<ProjectSummary, ProjectRegistryError> {
        let mut next = self.state.clone();
        let project = project_mut(&mut next, id)?;
        if project.trusted {
            project.trusted = false;
            self.commit(next)?;
        }
        self.get(id).ok_or(ProjectRegistryError::CorruptState)
    }

    /// Resolves an active project ID into a revalidated server-only root.
    pub fn resolve_root(&self, id: &ProjectId) -> Result<ProjectRoot, ProjectRegistryError> {
        let project = self
            .project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        ensure_active(project)?;
        let path = validate_live_root(project)?;
        Ok(ProjectRoot {
            path,
            identity: project.root_identity,
        })
    }

    /// Resolves a registered project into a revalidated root for durable-state
    /// cleanup, including after the project was archived.
    ///
    /// This does not grant execution trust. Callers must use it only to finish
    /// cleanup that was authorized while the project was active.
    pub fn resolve_root_for_cleanup(
        &self,
        id: &ProjectId,
    ) -> Result<ProjectRoot, ProjectRegistryError> {
        let project = self
            .project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        let path = validate_live_root(project)?;
        Ok(ProjectRoot {
            path,
            identity: project.root_identity,
        })
    }

    /// Resolves an active and explicitly trusted project into a server-only
    /// root capability suitable for execution.
    pub fn resolve_trusted_root(
        &self,
        id: &ProjectId,
    ) -> Result<ProjectRoot, ProjectRegistryError> {
        let project = self
            .project(id)
            .ok_or(ProjectRegistryError::ProjectNotFound)?;
        ensure_active(project)?;
        if !project.trusted {
            return Err(ProjectRegistryError::ProjectUntrusted);
        }
        let path = validate_live_root(project)?;
        Ok(ProjectRoot {
            path,
            identity: project.root_identity,
        })
    }

    fn summary(&self, project: &StoredProject) -> ProjectSummary {
        let state = if project.archived {
            ProjectState::Archived
        } else if project.trusted {
            ProjectState::Trusted
        } else {
            ProjectState::Untrusted
        };
        ProjectSummary {
            id: project.id.clone(),
            display_name: project.display_name.clone(),
            state,
            is_default: self.state.default_project_id.as_ref() == Some(&project.id),
            available: !project.archived && validate_live_root(project).is_ok(),
        }
    }

    fn project(&self, id: &ProjectId) -> Option<&StoredProject> {
        self.state.projects.iter().find(|project| &project.id == id)
    }

    fn mint_project_id(&self) -> Result<ProjectId, ProjectRegistryError> {
        for _ in 0..16 {
            let mut bytes = [0u8; PROJECT_ID_RANDOM_BYTES];
            getrandom::fill(&mut bytes).map_err(|_| ProjectRegistryError::RandomnessUnavailable)?;
            let mut value = String::with_capacity(PROJECT_ID_PREFIX.len() + PROJECT_ID_HEX_BYTES);
            value.push_str(PROJECT_ID_PREFIX);
            for byte in bytes {
                use fmt::Write as _;
                write!(&mut value, "{byte:02x}")
                    .map_err(|_| ProjectRegistryError::RandomnessUnavailable)?;
            }
            let id = ProjectId::parse(value)?;
            if self.project(&id).is_none() {
                return Ok(id);
            }
        }
        Err(ProjectRegistryError::RandomnessUnavailable)
    }

    fn commit(&mut self, mut next: StoredRegistry) -> Result<(), ProjectRegistryError> {
        next.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or(ProjectRegistryError::RevisionExhausted)?;
        validate_state(&next)?;
        persist_state(&self.state_directory, &self.state_path, &next)?;
        self.state = next;
        Ok(())
    }
}

fn project_mut<'a>(
    state: &'a mut StoredRegistry,
    id: &ProjectId,
) -> Result<&'a mut StoredProject, ProjectRegistryError> {
    state
        .projects
        .iter_mut()
        .find(|project| &project.id == id)
        .ok_or(ProjectRegistryError::ProjectNotFound)
}

fn bind_sessions_in_state<'a>(
    state: &mut StoredRegistry,
    id: &ProjectId,
    session_ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), ProjectRegistryError> {
    let session_ids = session_ids
        .into_iter()
        .map(|session_id| {
            if valid_session_id(session_id) {
                Ok(session_id.to_owned())
            } else {
                Err(ProjectRegistryError::InvalidSessionId)
            }
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    for session_id in &session_ids {
        if state
            .session_projects
            .get(session_id)
            .is_some_and(|existing| existing != id)
        {
            return Err(ProjectRegistryError::SessionAlreadyBound);
        }
    }
    let new_count = session_ids
        .iter()
        .filter(|session_id| !state.session_projects.contains_key(*session_id))
        .count();
    if state.session_projects.len().saturating_add(new_count) > MAX_SESSION_BINDINGS {
        return Err(ProjectRegistryError::SessionBindingLimitReached);
    }
    for session_id in session_ids {
        state.session_projects.insert(session_id, id.clone());
    }
    Ok(())
}

fn ensure_active(project: &StoredProject) -> Result<(), ProjectRegistryError> {
    if project.archived {
        Err(ProjectRegistryError::ProjectArchived)
    } else {
        Ok(())
    }
}

fn valid_project_id(value: &str) -> bool {
    value.len() == PROJECT_ID_PREFIX.len() + PROJECT_ID_HEX_BYTES
        && value.starts_with(PROJECT_ID_PREFIX)
        && value[PROJECT_ID_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn normalize_display_name(value: &str) -> Result<String, ProjectRegistryError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_DISPLAY_NAME_BYTES
        || matches!(value, "." | "..")
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        return Err(ProjectRegistryError::InvalidDisplayName);
    }
    Ok(value.to_owned())
}

fn default_display_name(root: &Path) -> Result<String, ProjectRegistryError> {
    let value = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ProjectRegistryError::InvalidDisplayName)?;
    normalize_display_name(value)
}

fn clean_absolute_path(path: &Path) -> Result<PathBuf, ProjectRegistryError> {
    if !path.is_absolute() {
        return Err(ProjectRegistryError::RelativePath);
    }
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(ProjectRegistryError::PathTraversal),
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                cleaned.push(component.as_os_str());
            }
        }
    }
    Ok(cleaned)
}

fn ensure_private_state_directory(path: &Path) -> Result<PathBuf, ProjectRegistryError> {
    match path.symlink_metadata() {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(ProjectRegistryError::UnsafeStatePath);
            }
            ensure_owner_only_permissions(&metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or(ProjectRegistryError::StateParentUnavailable)?;
            let parent_metadata = parent
                .symlink_metadata()
                .map_err(|_| ProjectRegistryError::StateParentUnavailable)?;
            if !parent_metadata.file_type().is_dir() {
                return Err(ProjectRegistryError::StateParentUnavailable);
            }
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder.create(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            }
            let metadata = path.symlink_metadata()?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(ProjectRegistryError::UnsafeStatePath);
            }
            ensure_owner_only_permissions(&metadata)?;
        }
        Err(error) => return Err(error.into()),
    }
    path.canonicalize().map_err(Into::into)
}

fn ensure_owner_only_permissions(metadata: &std::fs::Metadata) -> Result<(), ProjectRegistryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ProjectRegistryError::UnsafePermissions);
        }
    }
    Ok(())
}

fn validate_import_root(
    requested: &Path,
    state_directory: &Path,
) -> Result<PathBuf, ProjectRegistryError> {
    let requested = clean_absolute_path(requested)?;
    let metadata = requested
        .symlink_metadata()
        .map_err(map_root_metadata_error)?;
    if metadata.file_type().is_symlink() {
        return Err(ProjectRegistryError::RootSymlink);
    }
    if !metadata.file_type().is_dir() {
        return Err(ProjectRegistryError::RootNotDirectory);
    }
    let canonical = requested.canonicalize().map_err(map_root_metadata_error)?;
    let canonical_text = canonical
        .to_str()
        .ok_or(ProjectRegistryError::InvalidCanonicalRoot)?;
    if canonical_text.len() > MAX_CANONICAL_ROOT_BYTES {
        return Err(ProjectRegistryError::InvalidCanonicalRoot);
    }
    if canonical == state_directory
        || canonical.starts_with(state_directory)
        || state_directory.starts_with(&canonical)
    {
        return Err(ProjectRegistryError::RootOverlapsState);
    }
    Ok(canonical)
}

fn map_root_metadata_error(error: std::io::Error) -> ProjectRegistryError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProjectRegistryError::RootUnavailable
    } else {
        ProjectRegistryError::Storage(error)
    }
}

fn capture_root_identity(metadata: &std::fs::Metadata) -> StoredRootIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        StoredRootIdentity {
            device: Some(metadata.dev()),
            file: Some(metadata.ino()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        StoredRootIdentity {
            device: None,
            file: None,
        }
    }
}

fn validate_live_root(project: &StoredProject) -> Result<PathBuf, ProjectRegistryError> {
    let metadata = project
        .canonical_root
        .symlink_metadata()
        .map_err(map_root_metadata_error)?;
    if metadata.file_type().is_symlink() {
        return Err(ProjectRegistryError::RootSymlink);
    }
    if !metadata.file_type().is_dir() {
        return Err(ProjectRegistryError::RootNotDirectory);
    }
    let canonical = project
        .canonical_root
        .canonicalize()
        .map_err(map_root_metadata_error)?;
    if canonical != project.canonical_root
        || !root_identity_matches(project.root_identity, &metadata)
    {
        return Err(ProjectRegistryError::RootIdentityChanged);
    }
    Ok(canonical)
}

fn root_identity_matches(identity: StoredRootIdentity, metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        identity.device == Some(metadata.dev()) && identity.file == Some(metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (identity, metadata);
        true
    }
}

fn validate_state(state: &StoredRegistry) -> Result<(), ProjectRegistryError> {
    if state.version != STATE_VERSION {
        return Err(ProjectRegistryError::UnsupportedStateVersion);
    }
    if state.projects.len() > MAX_PROJECTS {
        return Err(ProjectRegistryError::CorruptState);
    }
    if state.session_projects.len() > MAX_SESSION_BINDINGS {
        return Err(ProjectRegistryError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for project in &state.projects {
        if !ids.insert(project.id.clone()) {
            return Err(ProjectRegistryError::CorruptState);
        }
        normalize_display_name(&project.display_name)
            .map_err(|_| ProjectRegistryError::CorruptState)?;
        validate_stored_root(project)?;
        if !roots.insert(project.canonical_root.clone()) {
            return Err(ProjectRegistryError::CorruptState);
        }
    }
    if let Some(default_id) = &state.default_project_id {
        let Some(project) = state
            .projects
            .iter()
            .find(|project| &project.id == default_id)
        else {
            return Err(ProjectRegistryError::CorruptState);
        };
        if project.archived {
            return Err(ProjectRegistryError::CorruptState);
        }
    }
    for (session_id, project_id) in &state.session_projects {
        if !valid_session_id(session_id) || !ids.contains(project_id) {
            return Err(ProjectRegistryError::CorruptState);
        }
    }
    Ok(())
}

fn validate_state_boundaries(
    state: &StoredRegistry,
    state_directory: &Path,
) -> Result<(), ProjectRegistryError> {
    for project in &state.projects {
        let effective_root = match project.canonical_root.canonicalize() {
            Ok(canonical) => {
                if canonical != project.canonical_root {
                    return Err(ProjectRegistryError::CorruptState);
                }
                canonical
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                project.canonical_root.clone()
            }
            Err(_) => return Err(ProjectRegistryError::CorruptState),
        };
        if effective_root == state_directory
            || effective_root.starts_with(state_directory)
            || state_directory.starts_with(&effective_root)
        {
            return Err(ProjectRegistryError::CorruptState);
        }
    }
    Ok(())
}

fn validate_stored_root(project: &StoredProject) -> Result<(), ProjectRegistryError> {
    let root = &project.canonical_root;
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(ProjectRegistryError::CorruptState);
    }
    let root_text = root.to_str().ok_or(ProjectRegistryError::CorruptState)?;
    if root_text.is_empty() || root_text.len() > MAX_CANONICAL_ROOT_BYTES {
        return Err(ProjectRegistryError::CorruptState);
    }
    #[cfg(unix)]
    if project.root_identity.device.is_none() || project.root_identity.file.is_none() {
        return Err(ProjectRegistryError::CorruptState);
    }
    #[cfg(not(unix))]
    if project.root_identity.device.is_some() || project.root_identity.file.is_some() {
        return Err(ProjectRegistryError::CorruptState);
    }
    Ok(())
}

fn read_state(path: &Path) -> Result<Option<StoredRegistry>, ProjectRegistryError> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ProjectRegistryError::UnsafeStatePath);
    }
    ensure_owner_only_permissions(&metadata)?;
    if metadata.len() > MAX_REGISTRY_STATE_BYTES {
        return Err(ProjectRegistryError::StateTooLarge);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.file_type().is_file() {
        return Err(ProjectRegistryError::UnsafeStatePath);
    }
    ensure_owner_only_permissions(&opened_metadata)?;
    if opened_metadata.len() > MAX_REGISTRY_STATE_BYTES {
        return Err(ProjectRegistryError::StateTooLarge);
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_REGISTRY_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_STATE_BYTES {
        return Err(ProjectRegistryError::StateTooLarge);
    }
    let state = serde_json::from_slice::<StoredRegistry>(&bytes)
        .map_err(|_| ProjectRegistryError::CorruptState)?;
    Ok(Some(state))
}

fn persist_state(
    state_directory: &Path,
    state_path: &Path,
    state: &StoredRegistry,
) -> Result<(), ProjectRegistryError> {
    validate_state(state)?;
    validate_state_boundaries(state, state_directory)?;
    let bytes = serde_json::to_vec(state).map_err(|_| ProjectRegistryError::CorruptState)?;
    if bytes.len() as u64 > MAX_REGISTRY_STATE_BYTES {
        return Err(ProjectRegistryError::StateTooLarge);
    }
    atomic_replace_with_hook(state_directory, state_path, &bytes, |_| Ok(()))
}

fn atomic_replace_with_hook(
    directory: &Path,
    destination: &Path,
    bytes: &[u8],
    before_replace: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), ProjectRegistryError> {
    let directory_metadata = directory.symlink_metadata()?;
    if !directory_metadata.file_type().is_dir() || directory_metadata.file_type().is_symlink() {
        return Err(ProjectRegistryError::UnsafeStatePath);
    }
    ensure_owner_only_permissions(&directory_metadata)?;
    match destination.symlink_metadata() {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(ProjectRegistryError::UnsafeStatePath);
            }
            ensure_owner_only_permissions(&metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let temporary_path = loop {
        let mut random = [0u8; PROJECT_ID_RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|_| ProjectRegistryError::RandomnessUnavailable)?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let candidate = directory.join(format!("{TEMP_FILE_PREFIX}{suffix}"));
        if !candidate.exists() {
            break candidate;
        }
    };

    let result = (|| -> Result<(), ProjectRegistryError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut temporary = options.open(&temporary_path)?;
        temporary.write_all(bytes)?;
        temporary.sync_all()?;
        drop(temporary);
        before_replace(&temporary_path)?;
        std::fs::rename(&temporary_path, destination)?;
        #[cfg(unix)]
        File::open(directory)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

fn cleanup_stale_temporary_files(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(suffix) = name.strip_prefix(TEMP_FILE_PREFIX) else {
            continue;
        };
        if suffix.len() == PROJECT_ID_HEX_BYTES
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn failed_replace_keeps_the_previous_complete_file_and_cleans_temp() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("private");
        let canonical = ensure_private_state_directory(&directory).unwrap();
        let destination = canonical.join(STATE_FILE_NAME);
        atomic_replace_with_hook(&canonical, &destination, b"before", |_| Ok(())).unwrap();

        let error = atomic_replace_with_hook(&canonical, &destination, b"after", |_| {
            Err(std::io::Error::other("injected before rename"))
        })
        .unwrap_err();
        assert!(matches!(error, ProjectRegistryError::Storage(_)));
        assert_eq!(std::fs::read(&destination).unwrap(), b"before");
        assert!(std::fs::read_dir(&canonical).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(TEMP_FILE_PREFIX)
        }));
    }
}
