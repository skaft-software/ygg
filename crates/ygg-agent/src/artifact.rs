//! Bounded, generation-scoped artifact ingestion for trusted subprocesses.
//!
//! Extension scratch paths never become model/provider media directly. The
//! host opens them without following links, verifies the claimed size,
//! SHA-256, and MIME signature, then retains an immutable byte snapshot behind
//! an opaque generation-owned identifier.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use ygg_ai::{AudioFormat, Media, Mime};

use crate::secure_fs::{create_private_directory_all, read_regular_file_bounded};
use crate::tool::content_hash;

/// Default maximum bytes accepted directly inside one control message.
pub const DEFAULT_MAX_INLINE_ARTIFACT_BYTES: usize = 256 * 1024;
/// Default maximum bytes retained for one artifact.
pub const DEFAULT_MAX_ARTIFACT_BYTES: usize = 20 * 1024 * 1024;
/// Default aggregate retained bytes for one process generation.
pub const DEFAULT_MAX_ARTIFACT_GENERATION_BYTES: usize = 64 * 1024 * 1024;
/// Default artifact count retained for one process generation.
pub const DEFAULT_MAX_ARTIFACTS_PER_GENERATION: usize = 64;
/// Maximum UTF-8 bytes accepted in a relative scratch path.
pub const MAX_ARTIFACT_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_ARTIFACT_RELATIVE_COMPONENTS: usize = 64;
const ARTIFACT_ID_RANDOM_BYTES: usize = 16;
const ARTIFACT_ID_ATTEMPTS: usize = 128;

/// Resource limits enforced before an artifact enters host-owned memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactStoreLimits {
    /// Maximum bytes allowed in the inline publication form.
    pub max_inline_bytes: usize,
    /// Maximum bytes retained for one artifact, including scratch files.
    pub max_artifact_bytes: usize,
    /// Aggregate retained bytes for one generation.
    pub max_generation_bytes: usize,
    /// Maximum number of retained artifacts for one generation.
    pub max_artifacts_per_generation: usize,
}

impl Default for ArtifactStoreLimits {
    fn default() -> Self {
        Self {
            max_inline_bytes: DEFAULT_MAX_INLINE_ARTIFACT_BYTES,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_generation_bytes: DEFAULT_MAX_ARTIFACT_GENERATION_BYTES,
            max_artifacts_per_generation: DEFAULT_MAX_ARTIFACTS_PER_GENERATION,
        }
    }
}

/// Bytes supplied either in-band or through a generation-owned scratch path.
#[derive(Clone, Debug)]
pub enum ArtifactSource {
    /// Small bytes already decoded from the control protocol.
    Inline(Bytes),
    /// UTF-8 relative path beneath the generation scratch directory.
    ScratchPath(PathBuf),
}

/// One artifact publication claim from an extension generation.
#[derive(Clone, Debug)]
pub struct ArtifactPublication {
    /// Inline bytes or a relative scratch path.
    pub source: ArtifactSource,
    /// Canonical supported MIME type claimed by the publisher.
    pub mime_type: String,
    /// Exact claimed byte count.
    pub size: u64,
    /// Exact lowercase hexadecimal SHA-256 digest.
    pub sha256: String,
}

/// Opaque identifier issued only after complete artifact verification.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(String);

impl ArtifactId {
    /// Returns the protocol-safe opaque spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ArtifactId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Host-verified artifact metadata returned to the publisher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedArtifact {
    /// Opaque generation-scoped identifier.
    pub id: ArtifactId,
    /// Canonical verified MIME type.
    pub mime_type: String,
    /// Verified byte count.
    pub size: u64,
    /// Verified lowercase hexadecimal SHA-256 digest.
    pub sha256: String,
}

/// One atomically resolved descriptor and immutable native media snapshot.
#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    /// Verified descriptor originally returned by publication.
    pub artifact: PublishedArtifact,
    /// Existing Ygg image/audio value built from the ingested bytes.
    pub media: Media,
}

/// Counts invalidated when one generation settles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactGenerationSettlement {
    /// Number of opaque IDs that became stale.
    pub artifacts: usize,
    /// Aggregate bytes released from host ownership.
    pub bytes: usize,
}

/// Artifact admission, resolution, and lifecycle failures.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// Store limits are internally inconsistent or disable required bounds.
    #[error("invalid artifact store limits")]
    InvalidLimits,
    /// A generation was registered twice without first settling.
    #[error("artifact generation {0} is already active")]
    DuplicateGeneration(u64),
    /// A publication or resolution referenced an inactive generation.
    #[error("artifact generation {0} is stale or inactive")]
    StaleGeneration(u64),
    /// A scratch path was absolute, empty, non-UTF-8, or contained traversal.
    #[error("invalid relative artifact scratch path")]
    InvalidScratchPath,
    /// Inline bytes crossed the smaller control-message admission bound.
    #[error("inline artifact is too large ({actual} bytes, limit {limit})")]
    InlineTooLarge {
        /// Actual inline byte count.
        actual: usize,
        /// Configured inline byte limit.
        limit: usize,
    },
    /// The claimed or observed artifact crossed the per-artifact bound.
    #[error("artifact is too large ({actual} bytes, limit {limit})")]
    TooLarge {
        /// Claimed or observed byte count.
        actual: u64,
        /// Configured artifact byte limit.
        limit: usize,
    },
    /// Claimed and observed sizes differ.
    #[error("artifact size mismatch (claimed {claimed}, observed {observed})")]
    SizeMismatch {
        /// Publisher-provided byte count.
        claimed: u64,
        /// Host-observed byte count.
        observed: u64,
    },
    /// The claimed SHA-256 is not exactly 64 lowercase hexadecimal digits.
    #[error("artifact SHA-256 must be 64 lowercase hexadecimal digits")]
    InvalidDigest,
    /// Claimed and observed SHA-256 digests differ.
    #[error("artifact SHA-256 mismatch")]
    DigestMismatch,
    /// The claimed MIME type is not one of the strictly verified media types.
    #[error("unsupported artifact MIME type: {0}")]
    UnsupportedMime(String),
    /// The byte signature does not match the claimed MIME type.
    #[error("artifact bytes do not match claimed MIME type {0}")]
    MimeMismatch(String),
    /// The generation reached its artifact-count bound.
    #[error("artifact generation reached its count limit of {0}")]
    CountLimit(usize),
    /// The publication would cross the generation's aggregate byte bound.
    #[error("artifact generation byte limit exceeded ({actual} bytes, limit {limit})")]
    GenerationBytesExceeded {
        /// Aggregate bytes after the attempted publication.
        actual: usize,
        /// Configured aggregate byte limit.
        limit: usize,
    },
    /// No verified artifact exists for this opaque ID in the given generation.
    #[error("unknown or stale artifact ID")]
    UnknownArtifact,
    /// Secure no-follow filesystem access rejected the scratch object.
    #[error(transparent)]
    SecureFile(#[from] crate::secure_fs::SecureFileError),
    /// Temporary-directory creation or cleanup failed.
    #[error("artifact scratch filesystem failure: {0}")]
    Io(#[from] std::io::Error),
    /// Internal state synchronization was poisoned.
    #[error("artifact store state is unavailable")]
    StateUnavailable,
    /// The operating system could not produce an opaque identifier.
    #[error("secure artifact ID generation failed")]
    RandomUnavailable,
    /// The bounded blocking ingestion worker did not return normally.
    #[error("artifact ingestion worker failed")]
    WorkerFailed,
}

#[derive(Clone)]
enum ArtifactMediaKind {
    Image(Mime),
    Audio(AudioFormat),
}

#[derive(Clone)]
struct StoredArtifact {
    owner: String,
    bytes: Bytes,
    media_kind: ArtifactMediaKind,
    mime_type: String,
    sha256: String,
}

struct GenerationState {
    scratch: tempfile::TempDir,
    artifacts: HashMap<String, StoredArtifact>,
    retained_bytes: usize,
}

struct ArtifactStoreInner {
    generations: Mutex<HashMap<u64, GenerationState>>,
    limits: ArtifactStoreLimits,
    root: tempfile::TempDir,
}

/// Host-owned artifact registry shared by one supervised extension process.
///
/// Multiple generations may coexist during candidate initialization. Every
/// publication and resolution names its generation explicitly, and settlement
/// atomically invalidates that generation before scratch cleanup begins.
#[derive(Clone)]
pub struct ArtifactStore {
    inner: Arc<ArtifactStoreInner>,
}

impl std::fmt::Debug for ArtifactStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let generations = self
            .inner
            .generations
            .lock()
            .map(|generations| generations.len())
            .unwrap_or_default();
        formatter
            .debug_struct("ArtifactStore")
            .field("root", &self.inner.root.path())
            .field("limits", &self.inner.limits)
            .field("active_generations", &generations)
            .finish()
    }
}

impl ArtifactStore {
    /// Creates a temporary host-owned store with default bounds.
    pub fn new() -> Result<Self, ArtifactError> {
        Self::with_limits(ArtifactStoreLimits::default())
    }

    /// Creates a temporary host-owned store with explicit bounds.
    pub fn with_limits(limits: ArtifactStoreLimits) -> Result<Self, ArtifactError> {
        validate_limits(limits)?;
        let root = tempfile::Builder::new()
            .prefix("ygg-artifacts-")
            .tempdir()?;
        create_private_directory_all(root.path())?;
        Ok(Self {
            inner: Arc::new(ArtifactStoreInner {
                generations: Mutex::new(HashMap::new()),
                limits,
                root,
            }),
        })
    }

    /// Registers a process generation and returns its host-owned scratch path.
    pub fn begin_generation(&self, generation: u64) -> Result<PathBuf, ArtifactError> {
        let mut generations = self.generations()?;
        if generations.contains_key(&generation) {
            return Err(ArtifactError::DuplicateGeneration(generation));
        }
        let prefix = format!("generation-{generation}-");
        let scratch = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir_in(self.inner.root.path())?;
        create_private_directory_all(scratch.path())?;
        let path = scratch.path().to_path_buf();
        generations.insert(
            generation,
            GenerationState {
                scratch,
                artifacts: HashMap::new(),
                retained_bytes: 0,
            },
        );
        Ok(path)
    }

    /// Returns the active scratch path for a generation.
    pub fn scratch_directory(&self, generation: u64) -> Result<PathBuf, ArtifactError> {
        self.generations()?
            .get(&generation)
            .map(|state| state.scratch.path().to_path_buf())
            .ok_or(ArtifactError::StaleGeneration(generation))
    }

    /// Verifies, snapshots, and registers one inline or scratch artifact.
    pub fn publish(
        &self,
        generation: u64,
        publication: ArtifactPublication,
    ) -> Result<PublishedArtifact, ArtifactError> {
        self.publish_for_owner(generation, "", publication)
    }

    /// Verifies, snapshots, and registers one artifact for an exact
    /// host-derived session owner.
    pub fn publish_for_owner(
        &self,
        generation: u64,
        owner: impl Into<String>,
        publication: ArtifactPublication,
    ) -> Result<PublishedArtifact, ArtifactError> {
        let owner = owner.into();
        validate_digest_claim(&publication.sha256)?;
        if publication.size > self.inner.limits.max_artifact_bytes as u64 {
            return Err(ArtifactError::TooLarge {
                actual: publication.size,
                limit: self.inner.limits.max_artifact_bytes,
            });
        }

        let scratch = {
            let generations = self.generations()?;
            let state = generations
                .get(&generation)
                .ok_or(ArtifactError::StaleGeneration(generation))?;
            preflight_generation_capacity(state, self.inner.limits, publication.size)?;
            state.scratch.path().to_path_buf()
        };

        let bytes = match publication.source {
            ArtifactSource::Inline(bytes) => {
                if bytes.len() > self.inner.limits.max_inline_bytes {
                    return Err(ArtifactError::InlineTooLarge {
                        actual: bytes.len(),
                        limit: self.inner.limits.max_inline_bytes,
                    });
                }
                bytes
            }
            ArtifactSource::ScratchPath(relative) => {
                validate_scratch_path(&relative)?;
                Bytes::from(read_regular_file_bounded(
                    &scratch.join(relative),
                    self.inner.limits.max_artifact_bytes,
                )?)
            }
        };

        let observed_size = bytes.len() as u64;
        if observed_size != publication.size {
            return Err(ArtifactError::SizeMismatch {
                claimed: publication.size,
                observed: observed_size,
            });
        }
        let observed_sha256 = content_hash(&bytes);
        if observed_sha256 != publication.sha256 {
            return Err(ArtifactError::DigestMismatch);
        }
        let (canonical_mime, media_kind) = verify_media_type(&publication.mime_type, &bytes)?;

        let mut generations = self.generations()?;
        let state = generations
            .get_mut(&generation)
            .ok_or(ArtifactError::StaleGeneration(generation))?;
        preflight_generation_capacity(state, self.inner.limits, observed_size)?;
        let id = allocate_artifact_id(&state.artifacts)?;
        state.retained_bytes = state.retained_bytes.checked_add(bytes.len()).ok_or(
            ArtifactError::GenerationBytesExceeded {
                actual: usize::MAX,
                limit: self.inner.limits.max_generation_bytes,
            },
        )?;
        state.artifacts.insert(
            id.clone(),
            StoredArtifact {
                owner,
                bytes,
                media_kind,
                mime_type: canonical_mime.clone(),
                sha256: observed_sha256.clone(),
            },
        );
        Ok(PublishedArtifact {
            id: ArtifactId(id),
            mime_type: canonical_mime,
            size: observed_size,
            sha256: observed_sha256,
        })
    }

    /// Verifies and registers an artifact without blocking an async runtime
    /// worker on descriptor-bound scratch-file reads.
    pub async fn publish_async(
        &self,
        generation: u64,
        publication: ArtifactPublication,
    ) -> Result<PublishedArtifact, ArtifactError> {
        self.publish_async_for_owner(generation, "", publication)
            .await
    }

    /// Publishes without blocking the async runtime and binds the resulting
    /// handle to one exact host-derived session owner.
    pub async fn publish_async_for_owner(
        &self,
        generation: u64,
        owner: impl Into<String>,
        publication: ArtifactPublication,
    ) -> Result<PublishedArtifact, ArtifactError> {
        let store = self.clone();
        let owner = owner.into();
        tokio::task::spawn_blocking(move || store.publish_for_owner(generation, owner, publication))
            .await
            .map_err(|_| ArtifactError::WorkerFailed)?
    }

    /// Atomically resolves verified metadata and Ygg's native media value.
    pub fn resolve_artifact(
        &self,
        generation: u64,
        artifact_id: impl AsRef<str>,
    ) -> Result<ResolvedArtifact, ArtifactError> {
        self.resolve_artifact_for_owner(generation, "", artifact_id)
    }

    /// Resolves an artifact only for the owner that published it.
    pub fn resolve_artifact_for_owner(
        &self,
        generation: u64,
        owner: &str,
        artifact_id: impl AsRef<str>,
    ) -> Result<ResolvedArtifact, ArtifactError> {
        let generations = self.generations()?;
        let state = generations
            .get(&generation)
            .ok_or(ArtifactError::StaleGeneration(generation))?;
        let artifact_id = artifact_id.as_ref();
        let artifact = state
            .artifacts
            .get(artifact_id)
            .filter(|artifact| artifact.owner == owner)
            .ok_or(ArtifactError::UnknownArtifact)?;
        let media = match &artifact.media_kind {
            ArtifactMediaKind::Image(mime) => {
                Media::image_bytes(artifact.bytes.clone(), mime.clone())
            }
            ArtifactMediaKind::Audio(format) => Media::audio_bytes(artifact.bytes.clone(), *format),
        };
        Ok(ResolvedArtifact {
            artifact: PublishedArtifact {
                id: ArtifactId(artifact_id.to_owned()),
                mime_type: artifact.mime_type.clone(),
                size: artifact.bytes.len() as u64,
                sha256: artifact.sha256.clone(),
            },
            media,
        })
    }

    /// Resolves only Ygg's existing image/audio media value.
    pub fn resolve_media(
        &self,
        generation: u64,
        artifact_id: impl AsRef<str>,
    ) -> Result<Media, ArtifactError> {
        self.resolve_artifact(generation, artifact_id)
            .map(|resolved| resolved.media)
    }

    /// Resolves media only for the owner that published it.
    pub fn resolve_media_for_owner(
        &self,
        generation: u64,
        owner: &str,
        artifact_id: impl AsRef<str>,
    ) -> Result<Media, ArtifactError> {
        self.resolve_artifact_for_owner(generation, owner, artifact_id)
            .map(|resolved| resolved.media)
    }

    /// Removes one just-published artifact when its owning child response can
    /// no longer be delivered. This is crate-internal transaction rollback,
    /// not an extension-visible deletion capability.
    pub(crate) fn remove_artifact(
        &self,
        generation: u64,
        artifact_id: impl AsRef<str>,
    ) -> Result<bool, ArtifactError> {
        let mut generations = self.generations()?;
        let state = generations
            .get_mut(&generation)
            .ok_or(ArtifactError::StaleGeneration(generation))?;
        let Some(artifact) = state.artifacts.remove(artifact_id.as_ref()) else {
            return Ok(false);
        };
        state.retained_bytes = state.retained_bytes.saturating_sub(artifact.bytes.len());
        Ok(true)
    }

    /// Atomically invalidates a generation and removes its scratch directory.
    pub fn settle_generation(
        &self,
        generation: u64,
    ) -> Result<ArtifactGenerationSettlement, ArtifactError> {
        let state = self
            .generations()?
            .remove(&generation)
            .ok_or(ArtifactError::StaleGeneration(generation))?;
        let settlement = ArtifactGenerationSettlement {
            artifacts: state.artifacts.len(),
            bytes: state.retained_bytes,
        };
        state.scratch.close()?;
        Ok(settlement)
    }

    fn generations(&self) -> Result<MutexGuard<'_, HashMap<u64, GenerationState>>, ArtifactError> {
        self.inner
            .generations
            .lock()
            .map_err(|_| ArtifactError::StateUnavailable)
    }
}

fn validate_limits(limits: ArtifactStoreLimits) -> Result<(), ArtifactError> {
    if limits.max_inline_bytes == 0
        || limits.max_artifact_bytes == 0
        || limits.max_generation_bytes == 0
        || limits.max_artifacts_per_generation == 0
        || limits.max_inline_bytes > limits.max_artifact_bytes
        || limits.max_artifact_bytes > limits.max_generation_bytes
    {
        return Err(ArtifactError::InvalidLimits);
    }
    Ok(())
}

fn validate_scratch_path(path: &Path) -> Result<(), ArtifactError> {
    let spelling = path.to_str().ok_or(ArtifactError::InvalidScratchPath)?;
    if spelling.is_empty() || spelling.len() > MAX_ARTIFACT_RELATIVE_PATH_BYTES {
        return Err(ArtifactError::InvalidScratchPath);
    }
    let mut components = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => components += 1,
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => return Err(ArtifactError::InvalidScratchPath),
        }
        if components > MAX_ARTIFACT_RELATIVE_COMPONENTS {
            return Err(ArtifactError::InvalidScratchPath);
        }
    }
    (components > 0)
        .then_some(())
        .ok_or(ArtifactError::InvalidScratchPath)
}

fn validate_digest_claim(digest: &str) -> Result<(), ArtifactError> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(ArtifactError::InvalidDigest);
    }
    Ok(())
}

fn preflight_generation_capacity(
    state: &GenerationState,
    limits: ArtifactStoreLimits,
    additional_bytes: u64,
) -> Result<(), ArtifactError> {
    if state.artifacts.len() >= limits.max_artifacts_per_generation {
        return Err(ArtifactError::CountLimit(
            limits.max_artifacts_per_generation,
        ));
    }
    let additional_bytes =
        usize::try_from(additional_bytes).map_err(|_| ArtifactError::TooLarge {
            actual: additional_bytes,
            limit: limits.max_artifact_bytes,
        })?;
    let aggregate = state.retained_bytes.saturating_add(additional_bytes);
    if aggregate > limits.max_generation_bytes {
        return Err(ArtifactError::GenerationBytesExceeded {
            actual: aggregate,
            limit: limits.max_generation_bytes,
        });
    }
    Ok(())
}

fn allocate_artifact_id(
    artifacts: &HashMap<String, StoredArtifact>,
) -> Result<String, ArtifactError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for _ in 0..ARTIFACT_ID_ATTEMPTS {
        let mut random = [0u8; ARTIFACT_ID_RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|_| ArtifactError::RandomUnavailable)?;
        let mut id = String::with_capacity("artifact_".len() + random.len() * 2);
        id.push_str("artifact_");
        for byte in random {
            id.push(char::from(HEX[usize::from(byte >> 4)]));
            id.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        if !artifacts.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(ArtifactError::RandomUnavailable)
}

fn verify_media_type(
    mime_type: &str,
    bytes: &[u8],
) -> Result<(String, ArtifactMediaKind), ArtifactError> {
    let parsed = mime_type
        .parse::<Mime>()
        .map_err(|_| ArtifactError::UnsupportedMime(mime_type.to_owned()))?;
    let essence = parsed.essence_str().to_owned();
    if mime_type != essence {
        return Err(ArtifactError::UnsupportedMime(mime_type.to_owned()));
    }

    let (matches, media_kind) = match essence.as_str() {
        "image/png" => (
            bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            ArtifactMediaKind::Image(parsed),
        ),
        "image/jpeg" => (
            bytes.starts_with(&[0xff, 0xd8, 0xff]),
            ArtifactMediaKind::Image(parsed),
        ),
        "image/gif" => (
            bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            ArtifactMediaKind::Image(parsed),
        ),
        "image/webp" => (
            bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
            ArtifactMediaKind::Image(parsed),
        ),
        "audio/wav" => (
            bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
            ArtifactMediaKind::Audio(AudioFormat::Wav),
        ),
        "audio/mpeg" => (
            bytes.starts_with(b"ID3")
                || (bytes.len() >= 2
                    && bytes[0] == 0xff
                    && bytes[1] & 0xe0 == 0xe0
                    && bytes[1] & 0x06 != 0),
            ArtifactMediaKind::Audio(AudioFormat::Mp3),
        ),
        "audio/flac" => (
            bytes.starts_with(b"fLaC"),
            ArtifactMediaKind::Audio(AudioFormat::Flac),
        ),
        "audio/opus" => (
            bytes.starts_with(b"OggS")
                && bytes
                    .windows(b"OpusHead".len())
                    .take(64)
                    .any(|window| window == b"OpusHead"),
            ArtifactMediaKind::Audio(AudioFormat::Opus),
        ),
        "audio/aac" => (
            bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xf6 == 0xf0,
            ArtifactMediaKind::Audio(AudioFormat::Aac),
        ),
        "audio/mp4" => (
            bytes.len() >= 12 && &bytes[4..8] == b"ftyp",
            ArtifactMediaKind::Audio(AudioFormat::Aac),
        ),
        _ => return Err(ArtifactError::UnsupportedMime(mime_type.to_owned())),
    };
    if !matches {
        return Err(ArtifactError::MimeMismatch(mime_type.to_owned()));
    }
    Ok((essence, media_kind))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ygg_ai::{AudioPayload, ImageSource};

    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nverified-payload";

    fn publication(source: ArtifactSource, bytes: &[u8], mime_type: &str) -> ArtifactPublication {
        ArtifactPublication {
            source,
            mime_type: mime_type.to_owned(),
            size: bytes.len() as u64,
            sha256: content_hash(bytes),
        }
    }

    #[test]
    fn inline_artifact_is_opaque_and_resolves_to_native_media() {
        let store = ArtifactStore::new().unwrap();
        store.begin_generation(7).unwrap();
        let published = store
            .publish(
                7,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();

        assert!(published.id.as_str().starts_with("artifact_"));
        assert!(!published.id.as_str().contains(&published.sha256));
        let resolved = store.resolve_artifact(7, &published.id).unwrap();
        assert_eq!(resolved.artifact, published);
        let Media::Image(image) = resolved.media else {
            panic!("expected image");
        };
        assert!(matches!(image.source, ImageSource::Inline(ref bytes) if bytes.as_ref() == PNG));
        assert_eq!(image.media_type.unwrap().essence_str(), "image/png");
    }

    #[test]
    fn artifact_handle_is_scoped_to_its_host_derived_owner() {
        let store = ArtifactStore::new().unwrap();
        store.begin_generation(1).unwrap();
        let published = store
            .publish_for_owner(
                1,
                "session-a",
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();

        assert!(store
            .resolve_artifact_for_owner(1, "session-a", &published.id)
            .is_ok());
        assert!(matches!(
            store.resolve_artifact_for_owner(1, "session-b", &published.id),
            Err(ArtifactError::UnknownArtifact)
        ));
    }

    #[test]
    fn scratch_publication_uses_an_immutable_ingested_snapshot() {
        let store = ArtifactStore::new().unwrap();
        let scratch = store.begin_generation(1).unwrap();
        let path = scratch.join("screen.png");
        fs::write(&path, PNG).unwrap();
        let published = store
            .publish(
                1,
                publication(
                    ArtifactSource::ScratchPath(PathBuf::from("screen.png")),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();
        fs::write(path, b"changed after publication").unwrap();

        let Media::Image(image) = store.resolve_media(1, &published.id).unwrap() else {
            panic!("expected image");
        };
        assert!(matches!(image.source, ImageSource::Inline(ref bytes) if bytes.as_ref() == PNG));
    }

    #[test]
    fn size_digest_and_mime_claims_are_all_verified() {
        let store = ArtifactStore::new().unwrap();
        store.begin_generation(1).unwrap();

        let mut wrong_size = publication(
            ArtifactSource::Inline(Bytes::from_static(PNG)),
            PNG,
            "image/png",
        );
        wrong_size.size += 1;
        assert!(matches!(
            store.publish(1, wrong_size),
            Err(ArtifactError::SizeMismatch { .. })
        ));

        let mut wrong_digest = publication(
            ArtifactSource::Inline(Bytes::from_static(PNG)),
            PNG,
            "image/png",
        );
        wrong_digest.sha256 = "0".repeat(64);
        assert!(matches!(
            store.publish(1, wrong_digest),
            Err(ArtifactError::DigestMismatch)
        ));

        let wrong_mime = publication(
            ArtifactSource::Inline(Bytes::from_static(PNG)),
            PNG,
            "audio/wav",
        );
        assert!(matches!(
            store.publish(1, wrong_mime),
            Err(ArtifactError::MimeMismatch(_))
        ));
    }

    #[test]
    fn inline_and_generation_quotas_are_enforced() {
        let inline_limits = ArtifactStoreLimits {
            max_inline_bytes: PNG.len() - 1,
            max_artifact_bytes: PNG.len(),
            max_generation_bytes: PNG.len(),
            max_artifacts_per_generation: 1,
        };
        let store = ArtifactStore::with_limits(inline_limits).unwrap();
        store.begin_generation(1).unwrap();
        assert!(matches!(
            store.publish(
                1,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png"
                )
            ),
            Err(ArtifactError::InlineTooLarge { .. })
        ));

        let generation_limits = ArtifactStoreLimits {
            max_inline_bytes: PNG.len(),
            max_artifact_bytes: PNG.len(),
            max_generation_bytes: PNG.len(),
            max_artifacts_per_generation: 2,
        };
        let store = ArtifactStore::with_limits(generation_limits).unwrap();
        store.begin_generation(2).unwrap();
        store
            .publish(
                2,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();
        assert!(matches!(
            store.publish(
                2,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png"
                )
            ),
            Err(ArtifactError::GenerationBytesExceeded { .. })
        ));
    }

    #[test]
    fn removing_an_undeliverable_artifact_recovers_generation_quota() {
        let limits = ArtifactStoreLimits {
            max_inline_bytes: PNG.len(),
            max_artifact_bytes: PNG.len(),
            max_generation_bytes: PNG.len(),
            max_artifacts_per_generation: 1,
        };
        let store = ArtifactStore::with_limits(limits).unwrap();
        store.begin_generation(3).unwrap();
        let first = store
            .publish(
                3,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();
        assert!(matches!(
            store.publish(
                3,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            ),
            Err(ArtifactError::CountLimit(1))
        ));

        assert!(store.remove_artifact(3, &first.id).unwrap());
        assert!(matches!(
            store.resolve_artifact(3, &first.id),
            Err(ArtifactError::UnknownArtifact)
        ));
        store
            .publish(
                3,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .expect("rollback must recover count and byte quota");
    }

    #[test]
    fn traversal_absolute_and_link_paths_fail_closed() {
        let store = ArtifactStore::new().unwrap();
        let scratch = store.begin_generation(1).unwrap();
        for path in [
            PathBuf::from("../escape.png"),
            PathBuf::from("/tmp/escape.png"),
        ] {
            assert!(matches!(
                store.publish(
                    1,
                    publication(ArtifactSource::ScratchPath(path), PNG, "image/png")
                ),
                Err(ArtifactError::InvalidScratchPath)
            ));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempfile::NamedTempFile::new().unwrap();
            fs::write(outside.path(), PNG).unwrap();
            symlink(outside.path(), scratch.join("linked.png")).unwrap();
            assert!(matches!(
                store.publish(
                    1,
                    publication(
                        ArtifactSource::ScratchPath(PathBuf::from("linked.png")),
                        PNG,
                        "image/png"
                    )
                ),
                Err(ArtifactError::SecureFile(_))
            ));
        }
    }

    #[test]
    fn generation_settlement_invalidates_ids_and_cleans_scratch() {
        let store = ArtifactStore::new().unwrap();
        let scratch = store.begin_generation(4).unwrap();
        let published = store
            .publish(
                4,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();
        let settlement = store.settle_generation(4).unwrap();

        assert_eq!(settlement.artifacts, 1);
        assert_eq!(settlement.bytes, PNG.len());
        assert!(!scratch.exists());
        assert!(matches!(
            store.resolve_media(4, &published.id),
            Err(ArtifactError::StaleGeneration(4))
        ));
        assert!(matches!(
            store.publish(
                4,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png"
                )
            ),
            Err(ArtifactError::StaleGeneration(4))
        ));
    }

    #[test]
    fn dropping_the_last_store_owner_cleans_active_generations() {
        let scratch = {
            let store = ArtifactStore::new().unwrap();
            let scratch = store.begin_generation(9).unwrap();
            fs::write(scratch.join("unpublished.tmp"), b"temporary").unwrap();
            scratch
        };

        assert!(!scratch.exists());
    }

    #[test]
    fn artifact_ids_never_cross_generations() {
        let store = ArtifactStore::new().unwrap();
        store.begin_generation(1).unwrap();
        store.begin_generation(2).unwrap();
        let published = store
            .publish(
                1,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(PNG)),
                    PNG,
                    "image/png",
                ),
            )
            .unwrap();
        assert!(matches!(
            store.resolve_media(2, &published.id),
            Err(ArtifactError::UnknownArtifact)
        ));
    }

    #[test]
    fn verified_audio_resolves_without_provider_references() {
        let wav = b"RIFF\x04\x00\x00\x00WAVEdata";
        let store = ArtifactStore::new().unwrap();
        store.begin_generation(1).unwrap();
        let published = store
            .publish(
                1,
                publication(
                    ArtifactSource::Inline(Bytes::from_static(wav)),
                    wav,
                    "audio/wav",
                ),
            )
            .unwrap();
        let Media::Audio(audio) = store.resolve_media(1, &published.id).unwrap() else {
            panic!("expected audio");
        };
        assert!(matches!(audio.payload, AudioPayload::Inline(ref bytes) if bytes.as_ref() == wav));
        assert_eq!(audio.format, AudioFormat::Wav);
    }
}
