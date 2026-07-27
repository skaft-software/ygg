//! Private, durable storage for session-scoped evidence resources.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{sanitize_public_text, DurableEntryId, SessionId, StoredResource};

// Commit manifests are the v2 visibility boundary. Keep any experimental
// pre-manifest v1 store untouched instead of interpreting it as crash debris.
const ROOT_NAME: &str = "evidence-v2";
const MAX_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESOURCE_COUNT: usize = 2_048;
const MAX_RESOURCE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SESSION_RESOURCE_COUNT: usize = 512;
const MAX_SESSION_RESOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 16 * 1024;
const MAX_BINDING_BYTES: u64 = 16 * 1024;
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const MAX_COMMIT_BYTES: u64 = 64 * 1024;
const MAX_COMMIT_BINDINGS: usize = 64;
const METADATA_VERSION: u16 = 1;
const BINDING_VERSION: u16 = 1;
const COMMIT_VERSION: u16 = 1;
const HANDLE_BYTES: usize = 32;
const HANDLE_HEX_BYTES: usize = HANDLE_BYTES * 2;

/// Durable evidence-store failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResourceStoreError {
    /// Public metadata or content exceeded a boundary.
    #[error("invalid evidence resource boundary")]
    InvalidBoundary,
    /// The private store reached its bounded quota.
    #[error("evidence resource quota reached")]
    QuotaExceeded,
    /// The resource or evidence record does not exist for this session.
    #[error("evidence resource was not found")]
    NotFound,
    /// A known, session-bound resource failed its authoritative integrity check.
    #[error("evidence resource is corrupt")]
    Corrupt,
    /// Private persistent storage failed or contained invalid state.
    #[error("evidence resource storage failed")]
    Storage,
}

/// Path-free metadata returned after one durable resource binding is committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceReference {
    /// Random opaque handle.
    pub handle: String,
    /// Safe basename.
    pub display_name: String,
    /// Validated media type.
    pub media_type: String,
    /// Exact byte length.
    pub byte_len: u64,
    /// Lowercase SHA-256 digest used only for internal integrity checks.
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredMetadata {
    version: u16,
    handle: String,
    display_name: String,
    media_type: String,
    byte_len: u64,
    sha256: String,
}

impl StoredMetadata {
    fn reference(&self) -> ResourceReference {
        ResourceReference {
            handle: self.handle.clone(),
            display_name: self.display_name.clone(),
            media_type: self.media_type.clone(),
            byte_len: self.byte_len,
            sha256: self.sha256.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BindingKey {
    session_id: String,
    tool_call_id: String,
    slot: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredBinding {
    version: u16,
    session_id: String,
    tool_call_id: String,
    slot: String,
    handle: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RecordKey {
    session_id: String,
    durable_entry_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCommitBinding {
    slot: String,
    handle: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCommit {
    version: u16,
    session_id: String,
    durable_entry_id: String,
    tool_call_id: String,
    bindings: Vec<StoredCommitBinding>,
}

#[derive(Default)]
struct StoreIndex {
    metadata: BTreeMap<String, StoredMetadata>,
    bindings: BTreeMap<BindingKey, String>,
    commits: BTreeMap<RecordKey, StoredCommit>,
    sessions_by_handle: BTreeMap<String, BTreeSet<String>>,
    corrupt_handles: BTreeSet<String>,
    session_usage: BTreeMap<String, (usize, u64)>,
    stored_bytes: u64,
}

struct StoreInner {
    #[cfg(test)]
    root: PathBuf,
    blobs: PathBuf,
    metadata: PathBuf,
    bindings: PathBuf,
    records: PathBuf,
    commits: PathBuf,
    index: Mutex<StoreIndex>,
}

/// Cloneable, session-scoped, durable evidence resource store.
///
/// The store intentionally performs no eviction. Reaching the quota fails new
/// writes instead of deleting resources that may still be reachable from an
/// older session branch.
///
/// The implementation rejects symlinked store directories and uses
/// `O_NOFOLLOW` for final file components. It remains path based, so it does
/// not claim protection from a hostile same-user process that can replace an
/// already-validated parent directory between operations. Such I/O failures
/// are treated as unavailable/corrupt rather than trusted content.
#[derive(Clone)]
pub struct ResourceStore {
    inner: Arc<StoreInner>,
}

impl ResourceStore {
    /// Opens or creates the versioned store below an already-private serve state directory.
    pub fn open(serve_state_dir: &Path) -> Result<Self, ResourceStoreError> {
        let root = serve_state_dir.join(ROOT_NAME);
        ensure_private_directory(&root)?;
        let blobs = root.join("blobs");
        let metadata = root.join("metadata");
        let bindings = root.join("bindings");
        let records = root.join("records");
        let commits = root.join("commits");
        ensure_private_directory(&blobs)?;
        ensure_private_directory(&metadata)?;
        ensure_private_directory(&bindings)?;
        ensure_private_directory(&records)?;
        ensure_private_directory(&commits)?;
        cleanup_temporary_files(&blobs);
        cleanup_temporary_files(&metadata);
        cleanup_temporary_files(&bindings);
        cleanup_temporary_files(&records);
        cleanup_temporary_files(&commits);

        let mut candidates = BTreeMap::new();
        let mut corrupt_candidates = BTreeSet::new();
        load_metadata(&metadata, &blobs, &mut candidates, &mut corrupt_candidates)?;
        let mut index = StoreIndex::default();
        load_bindings(&bindings, &candidates, &mut index)?;
        load_commits(&commits, &records, &index.bindings, &mut index.commits)?;
        let committed_bindings = index
            .commits
            .values()
            .flat_map(|commit| {
                commit.bindings.iter().map(|binding| {
                    (
                        BindingKey {
                            session_id: commit.session_id.clone(),
                            tool_call_id: commit.tool_call_id.clone(),
                            slot: binding.slot.clone(),
                        },
                        binding.handle.clone(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        index
            .bindings
            .retain(|key, handle| committed_bindings.get(key) == Some(handle));
        for (key, handle) in &index.bindings {
            index
                .sessions_by_handle
                .entry(handle.clone())
                .or_default()
                .insert(key.session_id.clone());
        }
        let referenced = index
            .sessions_by_handle
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        for handle in referenced {
            let Some(stored) = candidates.remove(&handle) else {
                continue;
            };
            if index.metadata.len() >= MAX_RESOURCE_COUNT
                || index
                    .stored_bytes
                    .checked_add(stored.byte_len)
                    .is_none_or(|total| total > MAX_RESOURCE_TOTAL_BYTES)
            {
                return Err(ResourceStoreError::QuotaExceeded);
            }
            index.stored_bytes = index.stored_bytes.saturating_add(stored.byte_len);
            let sessions = index
                .sessions_by_handle
                .get(&handle)
                .cloned()
                .unwrap_or_default();
            for session_id in sessions {
                let usage = index.session_usage.entry(session_id).or_default();
                usage.0 = usage.0.saturating_add(1);
                usage.1 = usage.1.saturating_add(stored.byte_len);
                if usage.0 > MAX_SESSION_RESOURCE_COUNT || usage.1 > MAX_SESSION_RESOURCE_BYTES {
                    return Err(ResourceStoreError::QuotaExceeded);
                }
            }
            if corrupt_candidates.contains(&handle) {
                index.corrupt_handles.insert(handle.clone());
            }
            index.metadata.insert(handle, stored);
        }
        index
            .bindings
            .retain(|_, handle| index.metadata.contains_key(handle));
        index
            .sessions_by_handle
            .retain(|handle, _| index.metadata.contains_key(handle));
        cleanup_unbound_resources(&metadata, &blobs, &index.metadata);
        cleanup_invalid_bindings(&bindings, &index.bindings);
        cleanup_invalid_records(&records, &index.commits);
        cleanup_invalid_records(&commits, &index.commits);

        Ok(Self {
            inner: Arc::new(StoreInner {
                #[cfg(test)]
                root,
                blobs,
                metadata,
                bindings,
                records,
                commits,
                index: Mutex::new(index),
            }),
        })
    }

    /// Persists immutable bytes and commits their session/tool/slot binding last.
    pub fn register(
        &self,
        session_id: &SessionId,
        tool_call_id: &str,
        slot: &str,
        display_name: &str,
        media_type: &str,
        bytes: Bytes,
    ) -> Result<ResourceReference, ResourceStoreError> {
        if bytes.is_empty() || bytes.len() > MAX_RESOURCE_BYTES || !safe_media_type(media_type) {
            return Err(ResourceStoreError::InvalidBoundary);
        }
        let display_name = safe_resource_name(display_name)?;
        let tool_call_id = safe_component(tool_call_id, 512)?;
        let slot = safe_slot(slot)?;
        let key = BindingKey {
            session_id: session_id.as_str().to_owned(),
            tool_call_id,
            slot,
        };
        let sha256 = sha256_hex(&bytes);
        let mut index = self
            .inner
            .index
            .lock()
            .map_err(|_| ResourceStoreError::Storage)?;
        if let Some(handle) = index.bindings.get(&key) {
            let stored = index
                .metadata
                .get(handle)
                .ok_or(ResourceStoreError::Storage)?;
            if stored.display_name == display_name
                && stored.media_type == media_type
                && stored.byte_len == bytes.len() as u64
                && stored.sha256 == sha256
            {
                return Ok(stored.reference());
            }
            return Err(ResourceStoreError::Storage);
        }
        if index.metadata.len() >= MAX_RESOURCE_COUNT
            || index
                .stored_bytes
                .checked_add(bytes.len() as u64)
                .is_none_or(|total| total > MAX_RESOURCE_TOTAL_BYTES)
        {
            return Err(ResourceStoreError::QuotaExceeded);
        }
        let session_usage = index
            .session_usage
            .get(session_id.as_str())
            .copied()
            .unwrap_or_default();
        if session_usage.0 >= MAX_SESSION_RESOURCE_COUNT
            || session_usage
                .1
                .checked_add(bytes.len() as u64)
                .is_none_or(|total| total > MAX_SESSION_RESOURCE_BYTES)
        {
            return Err(ResourceStoreError::QuotaExceeded);
        }
        let handle = loop {
            let candidate = random_hex(HANDLE_BYTES)?;
            if !index.metadata.contains_key(&candidate) {
                break candidate;
            }
        };
        let stored = StoredMetadata {
            version: METADATA_VERSION,
            handle: handle.clone(),
            display_name,
            media_type: media_type.to_owned(),
            byte_len: bytes.len() as u64,
            sha256,
        };
        let binding = StoredBinding {
            version: BINDING_VERSION,
            session_id: key.session_id.clone(),
            tool_call_id: key.tool_call_id.clone(),
            slot: key.slot.clone(),
            handle: handle.clone(),
        };
        let blob_path = self.inner.blobs.join(format!("{handle}.blob"));
        atomic_write(&self.inner.blobs, &blob_path, &bytes)?;
        let metadata_path = self.inner.metadata.join(format!("{handle}.json"));
        let metadata_bytes =
            serde_json::to_vec(&stored).map_err(|_| ResourceStoreError::Storage)?;
        if let Err(error) = atomic_write(&self.inner.metadata, &metadata_path, &metadata_bytes) {
            let _ = std::fs::remove_file(&blob_path);
            return Err(error);
        }
        let binding_path = self
            .inner
            .bindings
            .join(format!("{}.json", binding_file_key(&key)));
        let binding_bytes =
            serde_json::to_vec(&binding).map_err(|_| ResourceStoreError::Storage)?;
        if let Err(error) = atomic_write(&self.inner.bindings, &binding_path, &binding_bytes) {
            let _ = std::fs::remove_file(&metadata_path);
            let _ = std::fs::remove_file(&blob_path);
            return Err(error);
        }

        index.stored_bytes = index.stored_bytes.saturating_add(stored.byte_len);
        index.metadata.insert(handle.clone(), stored.clone());
        index.bindings.insert(key, handle.clone());
        index
            .sessions_by_handle
            .entry(handle)
            .or_default()
            .insert(session_id.as_str().to_owned());
        let usage = index
            .session_usage
            .entry(session_id.as_str().to_owned())
            .or_default();
        usage.0 = usage.0.saturating_add(1);
        usage.1 = usage.1.saturating_add(stored.byte_len);
        Ok(stored.reference())
    }

    /// Resolves one handle only when it is bound to the requested session.
    pub fn content(
        &self,
        session_id: &SessionId,
        handle: &str,
    ) -> Result<StoredResource, ResourceStoreError> {
        if !valid_handle(handle) {
            return Err(ResourceStoreError::NotFound);
        }
        let stored = {
            let index = self
                .inner
                .index
                .lock()
                .map_err(|_| ResourceStoreError::Storage)?;
            if !index
                .sessions_by_handle
                .get(handle)
                .is_some_and(|sessions| sessions.contains(session_id.as_str()))
            {
                return Err(ResourceStoreError::NotFound);
            }
            if index.corrupt_handles.contains(handle) {
                return Err(ResourceStoreError::Corrupt);
            }
            index
                .metadata
                .get(handle)
                .cloned()
                .ok_or(ResourceStoreError::NotFound)?
        };
        let path = self.inner.blobs.join(format!("{handle}.blob"));
        let bytes = match read_regular_file(&path, stored.byte_len) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.mark_corrupt(handle);
                return Err(ResourceStoreError::Corrupt);
            }
        };
        if bytes.len() as u64 != stored.byte_len || sha256_hex(&bytes) != stored.sha256 {
            self.mark_corrupt(handle);
            return Err(ResourceStoreError::Corrupt);
        }
        Ok(StoredResource {
            display_name: stored.display_name,
            media_type: stored.media_type,
            bytes: Bytes::from(bytes),
            sha256: stored.sha256,
        })
    }

    /// Commits one adapter-owned evidence record after all resource bindings.
    ///
    /// The commit sidecar is published last and is the sole restart visibility
    /// boundary for both the record and its resources. A publication failure
    /// rolls back the staged tool bindings in the live store; a process crash
    /// is recovered by [`Self::open`], which discards anything without a valid
    /// commit sidecar.
    pub fn persist_record(
        &self,
        session_id: &SessionId,
        durable_entry_id: &DurableEntryId,
        tool_call_id: &str,
        bytes: &[u8],
    ) -> Result<(), ResourceStoreError> {
        if bytes.is_empty()
            || bytes.len() as u64 > MAX_RECORD_BYTES
            || serde_json::from_slice::<serde_json::Value>(bytes).is_err()
        {
            return Err(ResourceStoreError::InvalidBoundary);
        }
        let tool_call_id = safe_component(tool_call_id, 512)?;
        let key = RecordKey {
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.as_str().to_owned(),
        };
        let file_key = record_file_key(session_id, durable_entry_id);
        let record_path = self.inner.records.join(format!("{file_key}.json"));
        let commit_path = self.inner.commits.join(format!("{file_key}.json"));
        let mut index = self
            .inner
            .index
            .lock()
            .map_err(|_| ResourceStoreError::Storage)?;
        let mut bindings = index
            .bindings
            .iter()
            .filter(|(binding, _)| {
                binding.session_id == session_id.as_str() && binding.tool_call_id == tool_call_id
            })
            .map(|(binding, handle)| StoredCommitBinding {
                slot: binding.slot.clone(),
                handle: handle.clone(),
            })
            .collect::<Vec<_>>();
        bindings.sort_by(|left, right| left.slot.cmp(&right.slot));
        if bindings.is_empty() || bindings.len() > MAX_COMMIT_BINDINGS {
            rollback_uncommitted_tool_locked(
                &self.inner,
                &mut index,
                session_id.as_str(),
                &tool_call_id,
            );
            return Err(ResourceStoreError::InvalidBoundary);
        }
        let commit = StoredCommit {
            version: COMMIT_VERSION,
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.as_str().to_owned(),
            tool_call_id: tool_call_id.clone(),
            bindings,
        };
        if let Some(existing) = index.commits.get(&key) {
            let existing_record = read_regular_file(&record_path, MAX_RECORD_BYTES)?;
            if existing == &commit && existing_record == bytes {
                return Ok(());
            }
            return Err(ResourceStoreError::Storage);
        }
        if index.commits.values().any(|existing| {
            existing.session_id == session_id.as_str()
                && existing.tool_call_id == tool_call_id
                && existing.durable_entry_id != durable_entry_id.as_str()
        }) {
            return Err(ResourceStoreError::Storage);
        }
        let commit_bytes = serde_json::to_vec(&commit).map_err(|_| ResourceStoreError::Storage)?;
        if commit_bytes.len() as u64 > MAX_COMMIT_BYTES {
            rollback_uncommitted_tool_locked(
                &self.inner,
                &mut index,
                session_id.as_str(),
                &tool_call_id,
            );
            return Err(ResourceStoreError::InvalidBoundary);
        }
        let record_preexisted = match read_regular_file(&record_path, MAX_RECORD_BYTES) {
            Ok(existing) if existing == bytes => true,
            Ok(_) => {
                if remove_file_if_exists(&record_path).is_err() {
                    rollback_uncommitted_tool_locked(
                        &self.inner,
                        &mut index,
                        session_id.as_str(),
                        &tool_call_id,
                    );
                    return Err(ResourceStoreError::Storage);
                }
                false
            }
            Err(ResourceStoreError::NotFound) => false,
            Err(error) => {
                rollback_uncommitted_tool_locked(
                    &self.inner,
                    &mut index,
                    session_id.as_str(),
                    &tool_call_id,
                );
                return Err(error);
            }
        };
        if !record_preexisted {
            if let Err(error) = atomic_write(&self.inner.records, &record_path, bytes) {
                rollback_uncommitted_tool_locked(
                    &self.inner,
                    &mut index,
                    session_id.as_str(),
                    &tool_call_id,
                );
                return Err(error);
            }
        }
        if let Err(error) = atomic_write(&self.inner.commits, &commit_path, &commit_bytes) {
            if !record_preexisted {
                let _ = remove_file_if_exists(&record_path);
            }
            rollback_uncommitted_tool_locked(
                &self.inner,
                &mut index,
                session_id.as_str(),
                &tool_call_id,
            );
            return Err(error);
        }
        index.commits.insert(key, commit);
        Ok(())
    }

    /// Reads one bounded adapter-owned evidence record for an active durable entry.
    pub fn record(
        &self,
        session_id: &SessionId,
        durable_entry_id: &DurableEntryId,
    ) -> Result<Bytes, ResourceStoreError> {
        let key = RecordKey {
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.as_str().to_owned(),
        };
        let index = self
            .inner
            .index
            .lock()
            .map_err(|_| ResourceStoreError::Storage)?;
        if !index.commits.contains_key(&key) {
            return Err(ResourceStoreError::NotFound);
        }
        let path = self.inner.records.join(format!(
            "{}.json",
            record_file_key(session_id, durable_entry_id)
        ));
        read_regular_file(&path, MAX_RECORD_BYTES).map(Bytes::from)
    }

    /// Removes staged resources for a tool that has no committed evidence record.
    ///
    /// This is idempotent and deliberately refuses to remove any tool whose
    /// commit sidecar is already durable.
    pub fn rollback_uncommitted_tool_resources(
        &self,
        session_id: &SessionId,
        tool_call_id: &str,
    ) -> Result<(), ResourceStoreError> {
        let tool_call_id = safe_component(tool_call_id, 512)?;
        let mut index = self
            .inner
            .index
            .lock()
            .map_err(|_| ResourceStoreError::Storage)?;
        if index.commits.values().any(|commit| {
            commit.session_id == session_id.as_str() && commit.tool_call_id == tool_call_id
        }) {
            return Ok(());
        }
        if rollback_uncommitted_tool_locked(
            &self.inner,
            &mut index,
            session_id.as_str(),
            &tool_call_id,
        ) {
            Ok(())
        } else {
            Err(ResourceStoreError::Storage)
        }
    }

    fn mark_corrupt(&self, handle: &str) {
        if let Ok(mut index) = self.inner.index.lock() {
            index.corrupt_handles.insert(handle.to_owned());
        }
    }
}

fn safe_media_type(value: &str) -> bool {
    value.contains('/')
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.'))
}

fn safe_resource_name(value: &str) -> Result<String, ResourceStoreError> {
    let normalized = value.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or_default().trim();
    if basename.is_empty() || matches!(basename, "." | "..") {
        return Err(ResourceStoreError::InvalidBoundary);
    }
    let basename = sanitize_public_text(basename, 512, false);
    if basename.is_empty() {
        return Err(ResourceStoreError::InvalidBoundary);
    }
    Ok(basename)
}

fn safe_component(value: &str, max: usize) -> Result<String, ResourceStoreError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(ResourceStoreError::InvalidBoundary);
    }
    Ok(value.to_owned())
}

fn safe_slot(value: &str) -> Result<String, ResourceStoreError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ResourceStoreError::InvalidBoundary);
    }
    Ok(value.to_owned())
}

fn ensure_private_directory(path: &Path) -> Result<(), ResourceStoreError> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(ResourceStoreError::Storage),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder
                .create(path)
                .map_err(|_| ResourceStoreError::Storage)?;
        }
        Err(_) => return Err(ResourceStoreError::Storage),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| ResourceStoreError::Storage)?;
    }
    Ok(())
}

fn cleanup_temporary_files(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(".tmp-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn load_metadata(
    metadata_dir: &Path,
    blobs_dir: &Path,
    output: &mut BTreeMap<String, StoredMetadata>,
    corrupt: &mut BTreeSet<String>,
) -> Result<(), ResourceStoreError> {
    let entries = std::fs::read_dir(metadata_dir).map_err(|_| ResourceStoreError::Storage)?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(handle) = name.strip_suffix(".json") else {
            continue;
        };
        if !valid_handle(handle) {
            continue;
        }
        let Ok(bytes) = read_regular_file(&entry.path(), MAX_METADATA_BYTES) else {
            continue;
        };
        let Ok(stored) = serde_json::from_slice::<StoredMetadata>(&bytes) else {
            continue;
        };
        if !valid_metadata(&stored, handle) {
            continue;
        }
        let blob_path = blobs_dir.join(format!("{handle}.blob"));
        match read_regular_file(&blob_path, stored.byte_len) {
            Ok(blob)
                if blob.len() as u64 == stored.byte_len && sha256_hex(&blob) == stored.sha256 => {}
            _ => {
                corrupt.insert(handle.to_owned());
            }
        }
        output.insert(handle.to_owned(), stored);
    }
    Ok(())
}

fn load_bindings(
    directory: &Path,
    metadata: &BTreeMap<String, StoredMetadata>,
    index: &mut StoreIndex,
) -> Result<(), ResourceStoreError> {
    let entries = std::fs::read_dir(directory).map_err(|_| ResourceStoreError::Storage)?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(file_key) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(bytes) = read_regular_file(&entry.path(), MAX_BINDING_BYTES) else {
            continue;
        };
        let Ok(binding) = serde_json::from_slice::<StoredBinding>(&bytes) else {
            continue;
        };
        let Ok(session_id) = SessionId::new(binding.session_id.clone()) else {
            continue;
        };
        let Ok(tool_call_id) = safe_component(&binding.tool_call_id, 512) else {
            continue;
        };
        let Ok(slot) = safe_slot(&binding.slot) else {
            continue;
        };
        let key = BindingKey {
            session_id: session_id.as_str().to_owned(),
            tool_call_id,
            slot,
        };
        if binding.version != BINDING_VERSION
            || binding_file_key(&key) != file_key
            || !metadata.contains_key(&binding.handle)
            || index.bindings.contains_key(&key)
        {
            continue;
        }
        index.bindings.insert(key, binding.handle);
    }
    Ok(())
}

fn load_commits(
    directory: &Path,
    records: &Path,
    bindings: &BTreeMap<BindingKey, String>,
    output: &mut BTreeMap<RecordKey, StoredCommit>,
) -> Result<(), ResourceStoreError> {
    let entries = std::fs::read_dir(directory).map_err(|_| ResourceStoreError::Storage)?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(file_key) = name.strip_suffix(".json") else {
            continue;
        };
        if !valid_handle(file_key) {
            continue;
        }
        let Ok(bytes) = read_regular_file(&entry.path(), MAX_COMMIT_BYTES) else {
            continue;
        };
        let Ok(commit) = serde_json::from_slice::<StoredCommit>(&bytes) else {
            continue;
        };
        let Ok(session_id) = SessionId::new(commit.session_id.clone()) else {
            continue;
        };
        let Ok(durable_entry_id) = DurableEntryId::new(commit.durable_entry_id.clone()) else {
            continue;
        };
        let Ok(tool_call_id) = safe_component(&commit.tool_call_id, 512) else {
            continue;
        };
        if commit.version != COMMIT_VERSION
            || record_file_key(&session_id, &durable_entry_id) != file_key
            || commit.bindings.is_empty()
            || commit.bindings.len() > MAX_COMMIT_BINDINGS
        {
            continue;
        }
        let mut seen_slots = BTreeSet::new();
        let valid_bindings = commit.bindings.iter().all(|binding| {
            let Ok(slot) = safe_slot(&binding.slot) else {
                return false;
            };
            if !valid_handle(&binding.handle) || !seen_slots.insert(slot.clone()) {
                return false;
            }
            bindings.get(&BindingKey {
                session_id: session_id.as_str().to_owned(),
                tool_call_id: tool_call_id.clone(),
                slot,
            }) == Some(&binding.handle)
        });
        if !valid_bindings {
            continue;
        }
        let record_path = records.join(format!("{file_key}.json"));
        let Ok(record) = read_regular_file(&record_path, MAX_RECORD_BYTES) else {
            continue;
        };
        if record.is_empty() || serde_json::from_slice::<serde_json::Value>(&record).is_err() {
            continue;
        }
        let key = RecordKey {
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.as_str().to_owned(),
        };
        if output.contains_key(&key)
            || output.values().any(|existing| {
                existing.session_id == session_id.as_str() && existing.tool_call_id == tool_call_id
            })
        {
            continue;
        }
        output.insert(key, commit);
    }
    Ok(())
}

fn valid_metadata(stored: &StoredMetadata, expected_handle: &str) -> bool {
    stored.version == METADATA_VERSION
        && stored.handle == expected_handle
        && valid_handle(&stored.handle)
        && !stored.display_name.is_empty()
        && stored.display_name.len() <= 512
        && safe_media_type(&stored.media_type)
        && (1..=MAX_RESOURCE_BYTES as u64).contains(&stored.byte_len)
        && valid_handle(&stored.sha256)
}

fn cleanup_unbound_resources(
    metadata_dir: &Path,
    blobs_dir: &Path,
    metadata: &BTreeMap<String, StoredMetadata>,
) {
    cleanup_handle_files(metadata_dir, ".json", |handle| {
        metadata.contains_key(handle)
    });
    cleanup_handle_files(blobs_dir, ".blob", |handle| metadata.contains_key(handle));
}

fn cleanup_invalid_bindings(directory: &Path, bindings: &BTreeMap<BindingKey, String>) {
    let expected = bindings
        .keys()
        .map(binding_file_key)
        .collect::<BTreeSet<_>>();
    cleanup_handle_files(directory, ".json", |key| expected.contains(key));
}

fn cleanup_invalid_records(directory: &Path, commits: &BTreeMap<RecordKey, StoredCommit>) {
    let expected = commits
        .keys()
        .map(|key| sha256_hex(format!("{}\0{}", key.session_id, key.durable_entry_id).as_bytes()))
        .collect::<BTreeSet<_>>();
    cleanup_handle_files(directory, ".json", |key| expected.contains(key));
}

fn cleanup_handle_files(directory: &Path, suffix: &str, keep: impl Fn(&str) -> bool) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(key) = name.strip_suffix(suffix) else {
            continue;
        };
        if valid_handle(key) && !keep(key) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn binding_file_key(key: &BindingKey) -> String {
    sha256_hex(format!("{}\0{}\0{}", key.session_id, key.tool_call_id, key.slot).as_bytes())
}

fn record_file_key(session_id: &SessionId, durable_entry_id: &DurableEntryId) -> String {
    sha256_hex(format!("{}\0{}", session_id.as_str(), durable_entry_id.as_str()).as_bytes())
}

fn rollback_uncommitted_tool_locked(
    inner: &StoreInner,
    index: &mut StoreIndex,
    session_id: &str,
    tool_call_id: &str,
) -> bool {
    let keys = index
        .bindings
        .keys()
        .filter(|key| key.session_id == session_id && key.tool_call_id == tool_call_id)
        .cloned()
        .collect::<Vec<_>>();
    let mut clean = true;
    let mut handles = Vec::with_capacity(keys.len());
    for key in keys {
        let binding_path = inner
            .bindings
            .join(format!("{}.json", binding_file_key(&key)));
        if remove_file_if_exists(&binding_path).is_err() {
            clean = false;
        }
        if let Some(handle) = index.bindings.remove(&key) {
            handles.push(handle);
        }
    }
    for handle in handles {
        if index.bindings.values().any(|bound| bound == &handle) {
            continue;
        }
        let metadata_path = inner.metadata.join(format!("{handle}.json"));
        let blob_path = inner.blobs.join(format!("{handle}.blob"));
        if remove_file_if_exists(&metadata_path).is_err()
            || remove_file_if_exists(&blob_path).is_err()
        {
            clean = false;
        }
        if let Some(stored) = index.metadata.remove(&handle) {
            index.stored_bytes = index.stored_bytes.saturating_sub(stored.byte_len);
            if let Some(usage) = index.session_usage.get_mut(session_id) {
                usage.0 = usage.0.saturating_sub(1);
                usage.1 = usage.1.saturating_sub(stored.byte_len);
                if usage.0 == 0 {
                    index.session_usage.remove(session_id);
                }
            }
        }
        index.sessions_by_handle.remove(&handle);
        index.corrupt_handles.remove(&handle);
    }
    sync_directory(&inner.bindings);
    sync_directory(&inner.metadata);
    sync_directory(&inner.blobs);
    clean
}

fn remove_file_if_exists(path: &Path) -> Result<(), ResourceStoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ResourceStoreError::Storage),
    }
}

fn atomic_write(
    directory: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), ResourceStoreError> {
    let temp_path = directory.join(format!(".tmp-{}", random_hex(16)?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&temp_path)
        .map_err(|_| ResourceStoreError::Storage)?;
    if file.write_all(bytes).is_err() || file.sync_all().is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(ResourceStoreError::Storage);
    }
    drop(file);
    if std::fs::hard_link(&temp_path, destination).is_err() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(ResourceStoreError::Storage);
    }
    let _ = std::fs::remove_file(&temp_path);
    sync_directory(directory);
    Ok(())
}

fn read_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ResourceStoreError> {
    let metadata = path.symlink_metadata().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ResourceStoreError::NotFound
        } else {
            ResourceStoreError::Storage
        }
    })?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > max_bytes
    {
        return Err(ResourceStoreError::Storage);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| ResourceStoreError::Storage)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ResourceStoreError::Storage)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ResourceStoreError::Storage);
    }
    Ok(bytes)
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn random_hex(byte_len: usize) -> Result<String, ResourceStoreError> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|_| ResourceStoreError::Storage)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes.as_ref())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_handle(value: &str) -> bool {
    value.len() == HANDLE_HEX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (SessionId, DurableEntryId) {
        (
            SessionId::new("session-resource-test").unwrap(),
            DurableEntryId::new("entry-resource-test").unwrap(),
        )
    }

    #[test]
    fn resources_survive_reopen_and_are_session_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let (session_id, entry_id) = ids();
        let store = ResourceStore::open(directory.path()).unwrap();
        let reference = store
            .register(
                &session_id,
                "call-1",
                "source",
                "../src/lib.rs",
                "text/plain",
                Bytes::from_static(b"pub fn ygg() {}\n"),
            )
            .unwrap();
        store
            .persist_record(&session_id, &entry_id, "call-1", br#"{"version":1}"#)
            .unwrap();
        drop(store);

        let reopened = ResourceStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .content(&session_id, &reference.handle)
                .unwrap()
                .bytes,
            Bytes::from_static(b"pub fn ygg() {}\n")
        );
        assert_eq!(
            reopened.record(&session_id, &entry_id).unwrap(),
            Bytes::from_static(br#"{"version":1}"#)
        );
        let wrong_session = SessionId::new("session-wrong").unwrap();
        assert_eq!(
            reopened
                .content(&wrong_session, &reference.handle)
                .unwrap_err(),
            ResourceStoreError::NotFound
        );
    }

    #[test]
    fn corrupt_or_symlinked_blobs_are_never_served() {
        let directory = tempfile::tempdir().unwrap();
        let (session_id, _) = ids();
        let store = ResourceStore::open(directory.path()).unwrap();
        let reference = store
            .register(
                &session_id,
                "call-1",
                "source",
                "lib.rs",
                "text/plain",
                Bytes::from_static(b"trusted"),
            )
            .unwrap();
        let blob = store
            .inner
            .root
            .join("blobs")
            .join(format!("{}.blob", reference.handle));
        std::fs::write(&blob, b"corrupt").unwrap();
        assert_eq!(
            store.content(&session_id, &reference.handle).unwrap_err(),
            ResourceStoreError::Corrupt
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::remove_file(&blob).unwrap();
            symlink("/dev/null", &blob).unwrap();
            assert_eq!(
                store.content(&session_id, &reference.handle).unwrap_err(),
                ResourceStoreError::Corrupt
            );
        }
    }

    #[test]
    fn binding_is_idempotent_but_cannot_be_retargeted() {
        let directory = tempfile::tempdir().unwrap();
        let (session_id, _) = ids();
        let store = ResourceStore::open(directory.path()).unwrap();
        let first = store
            .register(
                &session_id,
                "call-1",
                "diff",
                "lib.rs.diff",
                "text/plain",
                Bytes::from_static(b"-old\n+new\n"),
            )
            .unwrap();
        let second = store
            .register(
                &session_id,
                "call-1",
                "diff",
                "lib.rs.diff",
                "text/plain",
                Bytes::from_static(b"-old\n+new\n"),
            )
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            store
                .register(
                    &session_id,
                    "call-1",
                    "diff",
                    "lib.rs.diff",
                    "text/plain",
                    Bytes::from_static(b"-different\n"),
                )
                .unwrap_err(),
            ResourceStoreError::Storage
        );
    }

    #[test]
    fn restart_reclaims_crash_like_partial_bindings_and_records() {
        let directory = tempfile::tempdir().unwrap();
        let (session_id, entry_id) = ids();
        let store = ResourceStore::open(directory.path()).unwrap();
        let reference = store
            .register(
                &session_id,
                "call-crash",
                "diff",
                "lib.rs.diff",
                "text/plain",
                Bytes::from_static(b"--- a/lib.rs\n+++ b/lib.rs\n"),
            )
            .unwrap();
        let orphan_record = store
            .inner
            .records
            .join(format!("{}.json", record_file_key(&session_id, &entry_id)));
        atomic_write(
            &store.inner.records,
            &orphan_record,
            br#"{"version":1,"toolCallId":"call-crash"}"#,
        )
        .unwrap();
        drop(store);

        let reopened = ResourceStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .content(&session_id, &reference.handle)
                .unwrap_err(),
            ResourceStoreError::NotFound
        );
        assert_eq!(
            reopened.record(&session_id, &entry_id).unwrap_err(),
            ResourceStoreError::NotFound
        );
        assert_eq!(
            std::fs::read_dir(&reopened.inner.bindings).unwrap().count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(&reopened.inner.metadata).unwrap().count(),
            0
        );
        assert_eq!(std::fs::read_dir(&reopened.inner.blobs).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(&reopened.inner.records).unwrap().count(),
            0
        );
    }

    #[test]
    fn commit_io_failure_rolls_back_every_staged_resource() {
        let directory = tempfile::tempdir().unwrap();
        let (session_id, entry_id) = ids();
        let store = ResourceStore::open(directory.path()).unwrap();
        let diff = store
            .register(
                &session_id,
                "call-fail",
                "diff",
                "lib.rs.diff",
                "text/plain",
                Bytes::from_static(b"--- a/lib.rs\n+++ b/lib.rs\n"),
            )
            .unwrap();
        let result = store
            .register(
                &session_id,
                "call-fail",
                "result",
                "lib.rs",
                "text/plain",
                Bytes::from_static(b"pub fn changed() {}\n"),
            )
            .unwrap();
        std::fs::remove_dir(&store.inner.commits).unwrap();
        std::fs::write(&store.inner.commits, b"not a directory").unwrap();

        assert_eq!(
            store
                .persist_record(&session_id, &entry_id, "call-fail", br#"{"version":1}"#)
                .unwrap_err(),
            ResourceStoreError::Storage
        );
        assert_eq!(
            store.content(&session_id, &diff.handle).unwrap_err(),
            ResourceStoreError::NotFound
        );
        assert_eq!(
            store.content(&session_id, &result.handle).unwrap_err(),
            ResourceStoreError::NotFound
        );
        assert_eq!(std::fs::read_dir(&store.inner.bindings).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&store.inner.metadata).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&store.inner.blobs).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&store.inner.records).unwrap().count(), 0);

        std::fs::remove_file(&store.inner.commits).unwrap();
        ensure_private_directory(&store.inner.commits).unwrap();
        drop(store);
        let reopened = ResourceStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened.record(&session_id, &entry_id).unwrap_err(),
            ResourceStoreError::NotFound
        );
    }
}
