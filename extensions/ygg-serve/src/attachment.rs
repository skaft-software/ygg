//! Private, bounded storage for browser-ingested image attachments.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ygg_agent::secure_fs::{self, SecureFileError};

use crate::{sanitize_public_text, AttachmentPolicy, AttachmentRef, SessionId};

/// Maximum number of image attachments accepted by one prompt.
pub const MAX_ATTACHMENT_COUNT: usize = 8;
/// Maximum bytes accepted for one image attachment.
pub const MAX_ATTACHMENT_FILE_BYTES: usize = 5 * 1024 * 1024;
/// Maximum aggregate image bytes accepted by one prompt.
pub const MAX_ATTACHMENT_TOTAL_BYTES: usize = 20 * 1024 * 1024;
const MAX_STORED_ATTACHMENTS: usize = 1_024;
const MAX_STORED_ATTACHMENT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 8 * 1024;
const MAX_ASSOCIATION_BYTES: u64 = 64 * 1024;
const METADATA_VERSION: u16 = 1;
const ASSOCIATION_VERSION: u16 = 1;
const HANDLE_BYTES: usize = 32;
const HANDLE_HEX_BYTES: usize = HANDLE_BYTES * 2;

const ACCEPTED_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Attachment storage or validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentError {
    /// The attachment feature is unavailable.
    #[error("attachment storage is unavailable")]
    Unavailable,
    /// The supplied display name is invalid.
    #[error("invalid attachment display name")]
    InvalidName,
    /// The declared media type is unsupported.
    #[error("unsupported attachment media type")]
    UnsupportedMediaType,
    /// The image bytes do not match the declared type or are truncated.
    #[error("invalid attachment content")]
    InvalidContent,
    /// A file or aggregate limit was exceeded.
    #[error("attachment exceeds a size limit")]
    TooLarge,
    /// The persistent store reached its bounded quota.
    #[error("attachment storage quota reached")]
    QuotaExceeded,
    /// The opaque attachment handle does not exist.
    #[error("attachment was not found")]
    NotFound,
    /// A command reference does not match authoritative stored metadata.
    #[error("attachment metadata mismatch")]
    MetadataMismatch,
    /// Private persistent storage failed.
    #[error("attachment storage failed")]
    Storage,
}

/// Authoritative attachment bytes returned to the adapter or HTTP transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAttachment {
    /// Public, path-free metadata.
    pub reference: AttachmentRef,
    /// Exact validated image bytes.
    pub bytes: Bytes,
    /// Lowercase SHA-256 digest used for corruption checks and recovery.
    pub sha256: String,
}

/// Stable media identity used to recover a crash-interrupted association.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentFingerprint {
    /// Validated media type.
    pub media_type: String,
    /// Exact byte length.
    pub byte_len: u64,
    /// Lowercase SHA-256 digest.
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
    created_at_ms: u64,
}

impl StoredMetadata {
    fn attachment_ref(&self) -> AttachmentRef {
        AttachmentRef {
            handle: self.handle.clone(),
            display_name: self.display_name.clone(),
            media_type: self.media_type.clone(),
            byte_len: self.byte_len,
        }
    }

    fn matches_ref(&self, reference: &AttachmentRef) -> bool {
        self.handle == reference.handle
            && self.display_name == reference.display_name
            && self.media_type == reference.media_type
            && self.byte_len == reference.byte_len
    }

    fn matches_fingerprint(&self, fingerprint: &AttachmentFingerprint) -> bool {
        self.media_type == fingerprint.media_type
            && self.byte_len == fingerprint.byte_len
            && self.sha256 == fingerprint.sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AssociationKey {
    session_id: String,
    durable_entry_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAssociation {
    version: u16,
    session_id: String,
    durable_entry_id: String,
    handles: Vec<String>,
}

#[derive(Default)]
struct StoreIndex {
    metadata: BTreeMap<String, StoredMetadata>,
    associations: BTreeMap<AssociationKey, Vec<String>>,
    stored_bytes: u64,
    in_flight_count: usize,
    in_flight_bytes: u64,
}

struct StoreInner {
    #[cfg(test)]
    root: PathBuf,
    blobs: PathBuf,
    metadata: PathBuf,
    associations: PathBuf,
    association_mutations: Mutex<()>,
    index: Mutex<StoreIndex>,
}

struct QuotaReservation {
    inner: Arc<StoreInner>,
    byte_len: u64,
    active: bool,
}

impl QuotaReservation {
    fn commit(mut self, stored: StoredMetadata) -> Result<(), AttachmentError> {
        let mut index = self
            .inner
            .index
            .lock()
            .map_err(|_| AttachmentError::Storage)?;
        if index.metadata.contains_key(&stored.handle)
            || index.in_flight_count == 0
            || index.in_flight_bytes < self.byte_len
        {
            return Err(AttachmentError::Storage);
        }
        let stored_bytes = index
            .stored_bytes
            .checked_add(stored.byte_len)
            .ok_or(AttachmentError::Storage)?;
        index.in_flight_count -= 1;
        index.in_flight_bytes -= self.byte_len;
        index.stored_bytes = stored_bytes;
        index.metadata.insert(stored.handle.clone(), stored);
        self.active = false;
        Ok(())
    }
}

impl Drop for QuotaReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Ok(mut index) = self.inner.index.lock() else {
            return;
        };
        debug_assert!(index.in_flight_count > 0);
        debug_assert!(index.in_flight_bytes >= self.byte_len);
        index.in_flight_count = index.in_flight_count.saturating_sub(1);
        index.in_flight_bytes = index.in_flight_bytes.saturating_sub(self.byte_len);
    }
}

/// Cloneable private attachment store shared by the HTTP host and session drivers.
#[derive(Clone)]
pub struct AttachmentStore {
    inner: Arc<StoreInner>,
}

impl AttachmentStore {
    /// Opens or creates the store below an already-private Ygg serve state directory.
    pub fn open(serve_state_dir: &Path) -> Result<Self, AttachmentError> {
        let root = serve_state_dir.join("attachments");
        ensure_private_directory(&root)?;
        let blobs = root.join("blobs");
        let metadata = root.join("metadata");
        let associations = root.join("associations");
        ensure_private_directory(&blobs)?;
        ensure_private_directory(&metadata)?;
        ensure_private_directory(&associations)?;

        cleanup_temporary_files(&blobs)?;
        cleanup_temporary_files(&metadata)?;
        cleanup_temporary_files(&associations)?;

        let mut index = StoreIndex::default();
        load_metadata(&metadata, &blobs, &mut index)?;
        cleanup_unindexed_metadata(&metadata, &blobs, &index)?;
        load_associations(&associations, &mut index)?;
        cleanup_unindexed_associations(&associations, &index)?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                #[cfg(test)]
                root,
                blobs,
                metadata,
                associations,
                association_mutations: Mutex::new(()),
                index: Mutex::new(index),
            }),
        })
    }

    /// Advertised image policy for this store.
    pub fn policy(&self) -> AttachmentPolicy {
        AttachmentPolicy::image_defaults()
    }

    fn reserve_quota(
        &self,
        byte_len: u64,
        handle: &str,
    ) -> Result<QuotaReservation, AttachmentError> {
        let mut index = self
            .inner
            .index
            .lock()
            .map_err(|_| AttachmentError::Storage)?;
        let reserved_count = index
            .metadata
            .len()
            .checked_add(index.in_flight_count)
            .and_then(|count| count.checked_add(1));
        let reserved_bytes = index
            .stored_bytes
            .checked_add(index.in_flight_bytes)
            .and_then(|bytes| bytes.checked_add(byte_len));
        if reserved_count.is_none_or(|count| count > MAX_STORED_ATTACHMENTS)
            || reserved_bytes.is_none_or(|bytes| bytes > MAX_STORED_ATTACHMENT_BYTES)
        {
            return Err(AttachmentError::QuotaExceeded);
        }
        if index.metadata.contains_key(handle) {
            return Err(AttachmentError::Storage);
        }
        index.in_flight_count = index
            .in_flight_count
            .checked_add(1)
            .ok_or(AttachmentError::Storage)?;
        index.in_flight_bytes = index
            .in_flight_bytes
            .checked_add(byte_len)
            .ok_or(AttachmentError::Storage)?;
        Ok(QuotaReservation {
            inner: Arc::clone(&self.inner),
            byte_len,
            active: true,
        })
    }

    /// Ingests one fully buffered, transport-bounded image.
    pub fn ingest(
        &self,
        display_name: &str,
        declared_media_type: &str,
        bytes: Bytes,
    ) -> Result<AttachmentRef, AttachmentError> {
        if bytes.is_empty() || bytes.len() > MAX_ATTACHMENT_FILE_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        if !ACCEPTED_MEDIA_TYPES.contains(&declared_media_type) {
            return Err(AttachmentError::UnsupportedMediaType);
        }
        validate_image(declared_media_type, &bytes)?;
        let display_name = safe_display_name(display_name)?;
        let sha256 = sha256_hex(&bytes);
        let handle = random_hex(HANDLE_BYTES)?;
        let created_at_ms = now_ms();
        let stored = StoredMetadata {
            version: METADATA_VERSION,
            handle: handle.clone(),
            display_name,
            media_type: declared_media_type.to_owned(),
            byte_len: bytes.len() as u64,
            sha256,
            created_at_ms,
        };

        let reservation = self.reserve_quota(stored.byte_len, &handle)?;

        let blob_path = self.inner.blobs.join(format!("{handle}.blob"));
        atomic_write(&blob_path, &bytes, MAX_ATTACHMENT_FILE_BYTES)?;
        let metadata_bytes = serde_json::to_vec(&stored).map_err(|_| AttachmentError::Storage)?;
        let metadata_path = self.inner.metadata.join(format!("{handle}.json"));
        if let Err(error) =
            atomic_write(&metadata_path, &metadata_bytes, MAX_METADATA_BYTES as usize)
        {
            let _ = remove_file_if_exists(&blob_path);
            return Err(error);
        }

        let reference = stored.attachment_ref();
        if let Err(error) = reservation.commit(stored) {
            let _ = remove_file_if_exists(&metadata_path);
            let _ = remove_file_if_exists(&blob_path);
            return Err(error);
        }
        Ok(reference)
    }

    /// Resolves and corruption-checks one exact authoritative reference.
    pub fn resolve(&self, reference: &AttachmentRef) -> Result<StoredAttachment, AttachmentError> {
        let metadata = {
            let index = self
                .inner
                .index
                .lock()
                .map_err(|_| AttachmentError::Storage)?;
            index
                .metadata
                .get(&reference.handle)
                .cloned()
                .ok_or(AttachmentError::NotFound)?
        };
        if !metadata.matches_ref(reference) {
            return Err(AttachmentError::MetadataMismatch);
        }
        self.read_metadata_content(metadata)
    }

    /// Resolves one handle for an authenticated content response.
    pub fn content(&self, handle: &str) -> Result<StoredAttachment, AttachmentError> {
        if !valid_handle(handle) {
            return Err(AttachmentError::NotFound);
        }
        let metadata = {
            let index = self
                .inner
                .index
                .lock()
                .map_err(|_| AttachmentError::Storage)?;
            index
                .metadata
                .get(handle)
                .cloned()
                .ok_or(AttachmentError::NotFound)?
        };
        self.read_metadata_content(metadata)
    }

    /// Resolves an ordered prompt attachment list and enforces aggregate limits.
    pub fn resolve_many(
        &self,
        references: &[AttachmentRef],
    ) -> Result<Vec<StoredAttachment>, AttachmentError> {
        validate_reference_set(references)?;
        references
            .iter()
            .map(|reference| self.resolve(reference))
            .collect()
    }

    /// Persists the exact association between one durable media entry and its handles.
    pub fn associate(
        &self,
        session_id: &SessionId,
        durable_entry_id: &str,
        references: &[AttachmentRef],
    ) -> Result<(), AttachmentError> {
        // Keep reference validation, durable publication, and index publication
        // atomic with respect to permanent-session cleanup. Otherwise cleanup
        // can reclaim a handle after validation but before the association is
        // published, leaving a durable association to missing bytes.
        let _mutation = self
            .inner
            .association_mutations
            .lock()
            .map_err(|_| AttachmentError::Storage)?;
        let resolved = self.resolve_many(references)?;
        let handles = resolved
            .iter()
            .map(|attachment| attachment.reference.handle.clone())
            .collect::<Vec<_>>();
        self.persist_association(session_id, durable_entry_id, handles)
    }

    /// Returns a previously persisted exact entry association.
    pub fn refs_for_entry(
        &self,
        session_id: &SessionId,
        durable_entry_id: &str,
    ) -> Result<Option<Vec<AttachmentRef>>, AttachmentError> {
        let key = AssociationKey {
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.to_owned(),
        };
        let (handles, metadata) = {
            let index = self
                .inner
                .index
                .lock()
                .map_err(|_| AttachmentError::Storage)?;
            let Some(handles) = index.associations.get(&key).cloned() else {
                return Ok(None);
            };
            let metadata = handles
                .iter()
                .map(|handle| index.metadata.get(handle).cloned())
                .collect::<Option<Vec<_>>>()
                .ok_or(AttachmentError::NotFound)?;
            (handles, metadata)
        };
        debug_assert_eq!(handles.len(), metadata.len());
        Ok(Some(
            metadata
                .into_iter()
                .map(|metadata| metadata.attachment_ref())
                .collect(),
        ))
    }

    /// Recovers an association by exact media fingerprints after a crash window.
    pub fn recover_association(
        &self,
        session_id: &SessionId,
        durable_entry_id: &str,
        fingerprints: &[AttachmentFingerprint],
    ) -> Result<Option<Vec<AttachmentRef>>, AttachmentError> {
        if fingerprints.is_empty() || fingerprints.len() > MAX_ATTACHMENT_COUNT {
            return Ok(None);
        }
        let references = {
            let index = self
                .inner
                .index
                .lock()
                .map_err(|_| AttachmentError::Storage)?;
            let mut references = Vec::with_capacity(fingerprints.len());
            let mut assigned_handles = BTreeSet::new();
            for fingerprint in fingerprints {
                let candidate = index
                    .metadata
                    .values()
                    .filter(|metadata| {
                        metadata.matches_fingerprint(fingerprint)
                            && !assigned_handles.contains(&metadata.handle)
                    })
                    .max_by(|left, right| {
                        left.created_at_ms
                            .cmp(&right.created_at_ms)
                            .then_with(|| left.handle.cmp(&right.handle))
                    });
                let Some(candidate) = candidate else {
                    return Ok(None);
                };
                assigned_handles.insert(candidate.handle.clone());
                references.push(candidate.attachment_ref());
            }
            references
        };
        self.associate(session_id, durable_entry_id, &references)?;
        Ok(Some(references))
    }

    /// Removes every association owned by a permanently deleted session.
    ///
    /// Attachment bytes are removed only when no retained association refers to
    /// their handle, so a handle shared by another session remains available.
    /// The operation is idempotent to support retry from a deletion journal.
    pub fn delete_session(&self, session_id: &SessionId) -> Result<(), AttachmentError> {
        let _mutation = self
            .inner
            .association_mutations
            .lock()
            .map_err(|_| AttachmentError::Storage)?;
        let mut index = self
            .inner
            .index
            .lock()
            .map_err(|_| AttachmentError::Storage)?;
        let removed_keys = index
            .associations
            .keys()
            .filter(|key| key.session_id == session_id.as_str())
            .cloned()
            .collect::<Vec<_>>();
        if removed_keys.is_empty() {
            return Ok(());
        }
        let removed_key_set = removed_keys.iter().cloned().collect::<BTreeSet<_>>();
        let candidate_handles = removed_keys
            .iter()
            .filter_map(|key| index.associations.get(key))
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>();
        let retained_handles = index
            .associations
            .iter()
            .filter(|(key, _)| !removed_key_set.contains(*key))
            .flat_map(|(_, handles)| handles.iter().cloned())
            .collect::<BTreeSet<_>>();
        let orphaned_handles = candidate_handles
            .difference(&retained_handles)
            .cloned()
            .collect::<Vec<_>>();

        // Reclaim unshared bytes before removing the durable associations that
        // identify their owner. If deletion is interrupted, recovery can still
        // rediscover this session until its bytes are durably gone.
        for handle in &orphaned_handles {
            remove_file_if_exists(&self.inner.metadata.join(format!("{handle}.json")))?;
            remove_file_if_exists(&self.inner.blobs.join(format!("{handle}.blob")))?;
        }
        for key in &removed_keys {
            let file_key = sha256_hex(format!("{}\0{}", key.session_id, key.durable_entry_id));
            remove_file_if_exists(&self.inner.associations.join(format!("{file_key}.json")))?;
        }
        for key in removed_keys {
            index.associations.remove(&key);
        }
        for handle in orphaned_handles {
            if let Some(metadata) = index.metadata.remove(&handle) {
                index.stored_bytes = index.stored_bytes.saturating_sub(metadata.byte_len);
            }
        }
        Ok(())
    }

    fn read_metadata_content(
        &self,
        metadata: StoredMetadata,
    ) -> Result<StoredAttachment, AttachmentError> {
        let path = self.inner.blobs.join(format!("{}.blob", metadata.handle));
        let bytes = read_regular_file(&path, metadata.byte_len)?;
        if sha256_hex(&bytes) != metadata.sha256 {
            return Err(AttachmentError::Storage);
        }
        validate_image(&metadata.media_type, &bytes)?;
        Ok(StoredAttachment {
            reference: metadata.attachment_ref(),
            bytes: Bytes::from(bytes),
            sha256: metadata.sha256,
        })
    }

    fn persist_association(
        &self,
        session_id: &SessionId,
        durable_entry_id: &str,
        handles: Vec<String>,
    ) -> Result<(), AttachmentError> {
        if durable_entry_id.is_empty()
            || durable_entry_id.len() > 256
            || durable_entry_id.chars().any(char::is_control)
        {
            return Err(AttachmentError::MetadataMismatch);
        }
        let association = StoredAssociation {
            version: ASSOCIATION_VERSION,
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.to_owned(),
            handles: handles.clone(),
        };
        let bytes = serde_json::to_vec(&association).map_err(|_| AttachmentError::Storage)?;
        if bytes.len() as u64 > MAX_ASSOCIATION_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        let file_name = format!(
            "{}.json",
            sha256_hex(format!("{}\0{}", session_id.as_str(), durable_entry_id).as_bytes())
        );
        atomic_write(
            &self.inner.associations.join(file_name),
            &bytes,
            MAX_ASSOCIATION_BYTES as usize,
        )?;
        let key = AssociationKey {
            session_id: session_id.as_str().to_owned(),
            durable_entry_id: durable_entry_id.to_owned(),
        };
        self.inner
            .index
            .lock()
            .map_err(|_| AttachmentError::Storage)?
            .associations
            .insert(key, handles);
        Ok(())
    }

    #[cfg(test)]
    fn root(&self) -> &Path {
        &self.inner.root
    }
}

/// Validates the metadata-only attachment set carried by one command.
pub fn validate_reference_set(references: &[AttachmentRef]) -> Result<(), AttachmentError> {
    if references.len() > MAX_ATTACHMENT_COUNT {
        return Err(AttachmentError::TooLarge);
    }
    let mut handles = BTreeSet::new();
    let mut total = 0u64;
    for reference in references {
        if !valid_handle(&reference.handle)
            || reference.byte_len == 0
            || reference.byte_len > MAX_ATTACHMENT_FILE_BYTES as u64
            || !ACCEPTED_MEDIA_TYPES.contains(&reference.media_type.as_str())
        {
            return Err(AttachmentError::MetadataMismatch);
        }
        if !handles.insert(reference.handle.as_str()) {
            return Err(AttachmentError::MetadataMismatch);
        }
        total = total
            .checked_add(reference.byte_len)
            .ok_or(AttachmentError::TooLarge)?;
        if total > MAX_ATTACHMENT_TOTAL_BYTES as u64 {
            return Err(AttachmentError::TooLarge);
        }
    }
    Ok(())
}

fn safe_display_name(value: &str) -> Result<String, AttachmentError> {
    let normalized = value.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or_default().trim();
    if basename.is_empty() || matches!(basename, "." | "..") {
        return Err(AttachmentError::InvalidName);
    }
    let sanitized = sanitize_public_text(basename, 512, false);
    if sanitized.is_empty() {
        return Err(AttachmentError::InvalidName);
    }
    Ok(sanitized)
}

fn validate_image(media_type: &str, bytes: &[u8]) -> Result<(), AttachmentError> {
    let valid = match media_type {
        "image/png" => valid_png(bytes),
        "image/jpeg" => {
            bytes.len() >= 4
                && bytes.starts_with(&[0xff, 0xd8, 0xff])
                && bytes.ends_with(&[0xff, 0xd9])
        }
        "image/gif" => {
            bytes.len() >= 14
                && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
                && bytes.last() == Some(&0x3b)
        }
        "image/webp" => {
            bytes.len() >= 20
                && bytes.starts_with(b"RIFF")
                && &bytes[8..12] == b"WEBP"
                && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8 == bytes.len()
                && matches!(&bytes[12..16], b"VP8 " | b"VP8L" | b"VP8X")
        }
        _ => false,
    };
    valid.then_some(()).ok_or(AttachmentError::InvalidContent)
}

fn valid_png(bytes: &[u8]) -> bool {
    if bytes.len() < 45 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut offset = 8usize;
    let mut first = true;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let Some(end) = offset
            .checked_add(12)
            .and_then(|base| base.checked_add(length))
        else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        let chunk_type = &bytes[offset + 4..offset + 8];
        if first && (chunk_type != b"IHDR" || length != 13) {
            return false;
        }
        if chunk_type == b"IEND" {
            return length == 0 && end == bytes.len();
        }
        first = false;
        offset = end;
    }
    false
}

fn valid_handle(value: &str) -> bool {
    value.len() == HANDLE_HEX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_private_directory(path: &Path) -> Result<(), AttachmentError> {
    secure_fs::create_private_directory_all(path).map_err(|_| AttachmentError::Storage)
}

fn cleanup_temporary_files(directory: &Path) -> Result<(), AttachmentError> {
    let entries = std::fs::read_dir(directory).map_err(|_| AttachmentError::Storage)?;
    for entry in entries {
        let entry = entry.map_err(|_| AttachmentError::Storage)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp-")
            || name.starts_with(".ygg-tmp-")
            || name.starts_with(".ygg-delete-")
        {
            remove_file_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

fn load_metadata(
    metadata_dir: &Path,
    blobs_dir: &Path,
    index: &mut StoreIndex,
) -> Result<(), AttachmentError> {
    let entries = std::fs::read_dir(metadata_dir).map_err(|_| AttachmentError::Storage)?;
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
        let Ok(metadata) = serde_json::from_slice::<StoredMetadata>(&bytes) else {
            continue;
        };
        if !valid_stored_metadata(&metadata, handle) {
            continue;
        }
        let blob_path = blobs_dir.join(format!("{handle}.blob"));
        let Ok(blob) = read_regular_file(&blob_path, metadata.byte_len) else {
            continue;
        };
        if blob.len() as u64 != metadata.byte_len {
            continue;
        }
        if index.metadata.len() >= MAX_STORED_ATTACHMENTS
            || index
                .stored_bytes
                .checked_add(metadata.byte_len)
                .is_none_or(|total| total > MAX_STORED_ATTACHMENT_BYTES)
        {
            break;
        }
        index.stored_bytes = index.stored_bytes.saturating_add(metadata.byte_len);
        index.metadata.insert(handle.to_owned(), metadata);
    }
    Ok(())
}

fn load_associations(
    association_dir: &Path,
    index: &mut StoreIndex,
) -> Result<(), AttachmentError> {
    let entries = std::fs::read_dir(association_dir).map_err(|_| AttachmentError::Storage)?;
    for entry in entries.flatten() {
        let Ok(bytes) = read_regular_file(&entry.path(), MAX_ASSOCIATION_BYTES) else {
            continue;
        };
        let Ok(association) = serde_json::from_slice::<StoredAssociation>(&bytes) else {
            continue;
        };
        if association.version != ASSOCIATION_VERSION
            || SessionId::new(association.session_id.clone()).is_err()
            || association.durable_entry_id.is_empty()
            || association.handles.is_empty()
            || association.handles.len() > MAX_ATTACHMENT_COUNT
            || association
                .handles
                .iter()
                .any(|handle| !index.metadata.contains_key(handle))
        {
            continue;
        }
        index.associations.insert(
            AssociationKey {
                session_id: association.session_id,
                durable_entry_id: association.durable_entry_id,
            },
            association.handles,
        );
    }
    Ok(())
}

fn cleanup_unindexed_metadata(
    metadata_dir: &Path,
    blobs_dir: &Path,
    index: &StoreIndex,
) -> Result<(), AttachmentError> {
    cleanup_handle_files(metadata_dir, ".json", |handle| {
        index.metadata.contains_key(handle)
    })?;
    cleanup_handle_files(blobs_dir, ".blob", |handle| {
        index.metadata.contains_key(handle)
    })
}

fn cleanup_handle_files(
    directory: &Path,
    suffix: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<(), AttachmentError> {
    let entries = std::fs::read_dir(directory).map_err(|_| AttachmentError::Storage)?;
    for entry in entries {
        let entry = entry.map_err(|_| AttachmentError::Storage)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(handle) = name.strip_suffix(suffix) else {
            continue;
        };
        if valid_handle(handle) && !keep(handle) {
            remove_file_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

fn cleanup_unindexed_associations(
    directory: &Path,
    index: &StoreIndex,
) -> Result<(), AttachmentError> {
    let expected = index
        .associations
        .keys()
        .map(|key| sha256_hex(format!("{}\0{}", key.session_id, key.durable_entry_id).as_bytes()))
        .collect::<BTreeSet<_>>();
    cleanup_handle_files(directory, ".json", |handle| expected.contains(handle))
}

fn valid_stored_metadata(metadata: &StoredMetadata, expected_handle: &str) -> bool {
    metadata.version == METADATA_VERSION
        && metadata.handle == expected_handle
        && valid_handle(&metadata.handle)
        && !metadata.display_name.is_empty()
        && metadata.display_name.len() <= 512
        && ACCEPTED_MEDIA_TYPES.contains(&metadata.media_type.as_str())
        && (1..=MAX_ATTACHMENT_FILE_BYTES as u64).contains(&metadata.byte_len)
        && metadata.sha256.len() == 64
        && metadata
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn attachment_error(error: SecureFileError) -> AttachmentError {
    match error {
        SecureFileError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            AttachmentError::NotFound
        }
        _ => AttachmentError::Storage,
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), AttachmentError> {
    secure_fs::remove_regular_file_if_exists(path)
        .map(|_| ())
        .map_err(attachment_error)
}

fn atomic_write(destination: &Path, bytes: &[u8], limit: usize) -> Result<(), AttachmentError> {
    secure_fs::write_private_atomic(destination, bytes, limit).map_err(attachment_error)
}

fn read_regular_file(path: &Path, expected_or_max_bytes: u64) -> Result<Vec<u8>, AttachmentError> {
    let limit = usize::try_from(expected_or_max_bytes).map_err(|_| AttachmentError::Storage)?;
    secure_fs::read_private_file_bounded(path, limit).map_err(attachment_error)
}

fn random_hex(byte_len: usize) -> Result<String, AttachmentError> {
    let mut bytes = vec![0u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|_| AttachmentError::Storage)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes.as_ref())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Bytes {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"IEND");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        Bytes::from(bytes)
    }

    #[test]
    fn ingest_sniffs_content_and_sanitizes_basename() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store
            .ingest("../screens/alignment.png", "image/png", png())
            .unwrap();
        assert_eq!(reference.display_name, "alignment.png");
        assert_eq!(reference.byte_len, png().len() as u64);
        assert_eq!(store.resolve(&reference).unwrap().bytes, png());
        assert_eq!(
            store.ingest("spoof.jpg", "image/jpeg", png()).unwrap_err(),
            AttachmentError::InvalidContent
        );
        assert_eq!(
            store
                .ingest("truncated.png", "image/png", png().slice(..20))
                .unwrap_err(),
            AttachmentError::InvalidContent
        );
    }

    #[test]
    fn exact_metadata_and_duplicate_sets_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store.ingest("alignment.png", "image/png", png()).unwrap();
        let mut tampered = reference.clone();
        tampered.display_name = "other.png".into();
        assert_eq!(
            store.resolve(&tampered).unwrap_err(),
            AttachmentError::MetadataMismatch
        );
        assert_eq!(
            store
                .resolve_many(&[reference.clone(), reference])
                .unwrap_err(),
            AttachmentError::MetadataMismatch
        );
    }

    #[test]
    fn all_advertised_image_types_require_complete_matching_magic() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let jpeg = Bytes::from_static(&[0xff, 0xd8, 0xff, 0xe0, 0, 2, 0xff, 0xd9]);
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0; 7]);
        gif.push(0x3b);
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&12u32.to_le_bytes());
        webp.extend_from_slice(b"WEBPVP8 ");
        webp.extend_from_slice(&[0; 4]);
        for (name, media_type, bytes) in [
            ("one.png", "image/png", png()),
            ("one.jpg", "image/jpeg", jpeg.clone()),
            ("one.gif", "image/gif", Bytes::from(gif.clone())),
            ("one.webp", "image/webp", Bytes::from(webp.clone())),
        ] {
            assert!(
                store.ingest(name, media_type, bytes).is_ok(),
                "{media_type}"
            );
        }
        assert_eq!(
            store
                .ingest("short.jpg", "image/jpeg", jpeg.slice(..jpeg.len() - 1))
                .unwrap_err(),
            AttachmentError::InvalidContent
        );
        assert_eq!(
            store
                .ingest(
                    "short.gif",
                    "image/gif",
                    Bytes::from(gif[..gif.len() - 1].to_vec())
                )
                .unwrap_err(),
            AttachmentError::InvalidContent
        );
        assert_eq!(
            store
                .ingest(
                    "short.webp",
                    "image/webp",
                    Bytes::from(webp[..webp.len() - 1].to_vec())
                )
                .unwrap_err(),
            AttachmentError::InvalidContent
        );
    }

    #[test]
    fn store_and_association_survive_restart_and_corrupt_metadata_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new("session-one").unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store.ingest("alignment.png", "image/png", png()).unwrap();
        store
            .associate(&session_id, "entry-one", std::slice::from_ref(&reference))
            .unwrap();
        std::fs::write(
            store
                .root()
                .join("metadata")
                .join(format!("{}.json", "a".repeat(64))),
            b"{bad",
        )
        .unwrap();
        drop(store);

        let reopened = AttachmentStore::open(directory.path()).unwrap();
        assert_eq!(reopened.resolve(&reference).unwrap().bytes, png());
        assert_eq!(
            reopened.refs_for_entry(&session_id, "entry-one").unwrap(),
            Some(vec![reference])
        );
    }

    #[test]
    fn digest_recovers_missing_association_and_corrupt_blob_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new("session-recovery").unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store.ingest("alignment.png", "image/png", png()).unwrap();
        let fingerprint = AttachmentFingerprint {
            media_type: reference.media_type.clone(),
            byte_len: reference.byte_len,
            sha256: sha256_hex(png()),
        };
        drop(store);

        let reopened = AttachmentStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .recover_association(&session_id, "entry-recovered", &[fingerprint])
                .unwrap(),
            Some(vec![reference.clone()])
        );
        let blob = reopened
            .root()
            .join("blobs")
            .join(format!("{}.blob", reference.handle));
        let mut corrupted = png().to_vec();
        corrupted[20] ^= 1;
        std::fs::write(blob, corrupted).unwrap();
        assert_eq!(
            reopened.resolve(&reference).unwrap_err(),
            AttachmentError::Storage
        );
    }

    #[test]
    fn duplicate_fingerprints_recover_to_distinct_uploaded_handles() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new("session-duplicate-recovery").unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let first = store.ingest("first.png", "image/png", png()).unwrap();
        let second = store.ingest("second.png", "image/png", png()).unwrap();
        let fingerprint = AttachmentFingerprint {
            media_type: "image/png".to_owned(),
            byte_len: png().len() as u64,
            sha256: sha256_hex(png()),
        };

        let recovered = store
            .recover_association(
                &session_id,
                "entry-with-duplicates",
                &[fingerprint.clone(), fingerprint],
            )
            .unwrap()
            .unwrap();

        assert_eq!(recovered.len(), 2);
        assert_ne!(recovered[0].handle, recovered[1].handle);
        assert_eq!(
            recovered
                .iter()
                .map(|reference| reference.handle.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([first.handle.as_str(), second.handle.as_str()])
        );
        assert_eq!(
            store
                .refs_for_entry(&session_id, "entry-with-duplicates")
                .unwrap(),
            Some(recovered)
        );
    }

    #[test]
    fn deleting_a_session_reclaims_only_its_unshared_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let deleted_session = SessionId::new("session-deleted").unwrap();
        let retained_session = SessionId::new("session-retained").unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let unique = store.ingest("unique.png", "image/png", png()).unwrap();
        let shared = store.ingest("shared.png", "image/png", png()).unwrap();
        store
            .associate(
                &deleted_session,
                "deleted-entry",
                &[unique.clone(), shared.clone()],
            )
            .unwrap();
        store
            .associate(
                &retained_session,
                "retained-entry",
                std::slice::from_ref(&shared),
            )
            .unwrap();

        store.delete_session(&deleted_session).unwrap();
        store.delete_session(&deleted_session).unwrap();

        assert_eq!(
            store
                .refs_for_entry(&deleted_session, "deleted-entry")
                .unwrap(),
            None
        );
        assert_eq!(
            store.content(&unique.handle).unwrap_err(),
            AttachmentError::NotFound
        );
        assert_eq!(
            store
                .refs_for_entry(&retained_session, "retained-entry")
                .unwrap(),
            Some(vec![shared.clone()])
        );
        assert_eq!(store.content(&shared.handle).unwrap().reference, shared);

        drop(store);
        let reopened = AttachmentStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .refs_for_entry(&retained_session, "retained-entry")
                .unwrap(),
            Some(vec![shared])
        );
    }

    #[test]
    fn concurrent_association_and_deletion_never_publish_dangling_handles() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();

        for iteration in 0..32 {
            let deleted_session = SessionId::new(format!("session-deleted-{iteration}")).unwrap();
            let retained_session = SessionId::new(format!("session-retained-{iteration}")).unwrap();
            let reference = store
                .ingest(&format!("shared-{iteration}.png"), "image/png", png())
                .unwrap();
            store
                .associate(
                    &deleted_session,
                    "deleted-entry",
                    std::slice::from_ref(&reference),
                )
                .unwrap();

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let associating = {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                let retained_session = retained_session.clone();
                let reference = reference.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.associate(&retained_session, "retained-entry", &[reference])
                })
            };
            let deleting = {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                let deleted_session = deleted_session.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.delete_session(&deleted_session)
                })
            };

            let associated = associating.join().unwrap();
            deleting.join().unwrap().unwrap();
            match associated {
                Ok(()) => {
                    assert_eq!(
                        store
                            .refs_for_entry(&retained_session, "retained-entry")
                            .unwrap(),
                        Some(vec![reference.clone()])
                    );
                    assert_eq!(
                        store.content(&reference.handle).unwrap().reference,
                        reference
                    );
                }
                Err(AttachmentError::NotFound) => {
                    assert_eq!(
                        store
                            .refs_for_entry(&retained_session, "retained-entry")
                            .unwrap(),
                        None
                    );
                    assert_eq!(
                        store.content(&reference.handle),
                        Err(AttachmentError::NotFound)
                    );
                }
                Err(error) => panic!("unexpected association result: {error}"),
            }
        }

        drop(store);
        AttachmentStore::open(directory.path()).unwrap();
    }

    #[test]
    fn startup_removes_only_unusable_orphan_store_files() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store.ingest("alignment.png", "image/png", png()).unwrap();
        let root = store.root().to_owned();
        let orphan_blob = root.join("blobs").join(format!("{}.blob", "b".repeat(64)));
        let orphan_metadata = root
            .join("metadata")
            .join(format!("{}.json", "c".repeat(64)));
        let orphan_association = root
            .join("associations")
            .join(format!("{}.json", "d".repeat(64)));
        std::fs::write(&orphan_blob, png()).unwrap();
        std::fs::write(&orphan_metadata, b"{bad").unwrap();
        std::fs::write(&orphan_association, b"{bad").unwrap();
        drop(store);

        let reopened = AttachmentStore::open(directory.path()).unwrap();
        assert_eq!(reopened.resolve(&reference).unwrap().bytes, png());
        assert!(!orphan_blob.exists());
        assert!(!orphan_metadata.exists());
        assert!(!orphan_association.exists());
    }

    #[test]
    fn startup_cleans_secure_filesystem_recovery_files() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let root = store.root().to_owned();
        let leftovers = [
            root.join("blobs").join(".tmp-legacy-write"),
            root.join("metadata").join(".ygg-tmp-interrupted-write"),
            root.join("associations")
                .join(".ygg-delete-interrupted-cleanup"),
        ];
        for leftover in &leftovers {
            std::fs::write(leftover, b"incomplete").unwrap();
        }
        drop(store);

        AttachmentStore::open(directory.path()).unwrap();
        for leftover in leftovers {
            assert!(!leftover.exists(), "{} was not cleaned", leftover.display());
        }
    }

    fn fake_metadata(handle: String) -> StoredMetadata {
        StoredMetadata {
            version: METADATA_VERSION,
            handle,
            display_name: "existing.png".to_owned(),
            media_type: "image/png".to_owned(),
            byte_len: 1,
            sha256: "0".repeat(64),
            created_at_ms: 0,
        }
    }

    fn concurrent_ingests(
        store: &AttachmentStore,
        workers: usize,
    ) -> Vec<Result<AttachmentRef, AttachmentError>> {
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        (0..workers)
            .map(|worker| {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.ingest(&format!("image-{worker}.png"), "image/png", png())
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    }

    #[test]
    fn in_flight_count_reservations_close_the_concurrent_quota_race() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        {
            let mut index = store.inner.index.lock().unwrap();
            for slot in 0..MAX_STORED_ATTACHMENTS - 1 {
                let handle = format!("slot-{slot}");
                index.metadata.insert(handle.clone(), fake_metadata(handle));
            }
        }

        let results = concurrent_ingests(&store, 24);
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AttachmentError::QuotaExceeded)))
                .count(),
            23
        );
        let index = store.inner.index.lock().unwrap();
        assert_eq!(index.metadata.len(), MAX_STORED_ATTACHMENTS);
        assert_eq!(index.in_flight_count, 0);
        assert_eq!(index.in_flight_bytes, 0);
        assert_eq!(
            AttachmentError::QuotaExceeded.to_string(),
            "attachment storage quota reached"
        );
    }

    #[test]
    fn in_flight_byte_reservations_close_the_concurrent_quota_race() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let attachment_bytes = png().len() as u64;
        store.inner.index.lock().unwrap().stored_bytes =
            MAX_STORED_ATTACHMENT_BYTES - attachment_bytes;

        let results = concurrent_ingests(&store, 24);
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AttachmentError::QuotaExceeded)))
                .count(),
            23
        );
        let index = store.inner.index.lock().unwrap();
        assert_eq!(index.stored_bytes, MAX_STORED_ATTACHMENT_BYTES);
        assert_eq!(index.in_flight_count, 0);
        assert_eq!(index.in_flight_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_and_symlink_store_rejection() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store.ingest("alignment.png", "image/png", png()).unwrap();
        let root_mode = store.root().metadata().unwrap().permissions().mode() & 0o777;
        let blob_mode = store
            .root()
            .join("blobs")
            .join(format!("{}.blob", reference.handle))
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
        assert_eq!(blob_mode, 0o600);

        let other = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        symlink(other.path(), state.path().join("attachments")).unwrap();
        assert!(AttachmentStore::open(state.path()).is_err());
    }
}
