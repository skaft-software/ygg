//! Governed process-fleet ownership for executable extensions.
//!
//! A catalog is static and content-bound: loading it never launches an
//! executable. A manager owns the resulting process fleet, while each product
//! session receives an [`ExtensionSessionBinding`] that may attach eligible
//! runtimes. Shared processes require all of canonical workspace, explicit
//! trust domain, explicit manifest sharing policy, and content digest to match.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak};
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, Notify, Semaphore};

use crate::extension_process::{
    DiscoveredExtension, ExtensionLifecycleProfile, ExtensionProcess, ExtensionReloadReport,
    ExtensionRuntimeConfig, ExtensionRuntimeError as ProcessRuntimeError, ExtensionRuntimeSharing,
    ExtensionTrust, EXTENSION_API_VERSION_0_1,
};
use crate::secure_fs::read_regular_file_bounded;

const MAX_CATALOG_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const ESTIMATED_PROCESS_FDS: usize = 4;
const SUPERVISOR_POLL: Duration = Duration::from_millis(100);
fn lock<T>(value: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read<T>(value: &StdRwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    value
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(value: &StdRwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    value
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sha256_hex(domain: &[u8], bytes: impl AsRef<[u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes.as_ref());
    format!("{:x}", digest.finalize())
}

/// Error while canonicalizing a workspace or defining an explicit trust domain.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionRuntimeDomainError {
    /// The workspace could not be canonicalized into an existing directory.
    #[error("extension runtime workspace is unavailable")]
    WorkspaceUnavailable,
    /// A trust-domain label was empty or contained unsupported characters.
    #[error("extension runtime trust domain is invalid")]
    InvalidTrustDomain,
    /// A persisted content identity was not a lowercase SHA-256 digest.
    #[error("extension runtime content digest is invalid")]
    InvalidContentDigest,
}

/// Canonical workspace identity used by one extension runtime domain.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CanonicalWorkspace {
    path: PathBuf,
    digest: String,
}

impl CanonicalWorkspace {
    /// Resolves an existing workspace directory once for domain ownership.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ExtensionRuntimeDomainError> {
        let path = path
            .as_ref()
            .canonicalize()
            .map_err(|_| ExtensionRuntimeDomainError::WorkspaceUnavailable)?;
        if !path.is_dir() {
            return Err(ExtensionRuntimeDomainError::WorkspaceUnavailable);
        }
        let digest = sha256_hex(
            b"ygg-extension-workspace-v1\0",
            path.to_string_lossy().as_bytes(),
        );
        Ok(Self { path, digest })
    }

    /// Returns the canonical local path used as the child working directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a path-free stable fingerprint for diagnostics and provenance.
    pub fn fingerprint(&self) -> &str {
        &self.digest
    }
}

/// Explicit trust partition for a runtime domain.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExtensionTrustDomain {
    fingerprint: String,
}

impl ExtensionTrustDomain {
    /// Creates an explicit trust partition from a bounded stable label.
    ///
    /// The original label is deliberately not retained or exposed by runtime
    /// status, so diagnostics never need to reveal a Serve principal or other
    /// trust-routing input.
    pub fn new(label: impl AsRef<str>) -> Result<Self, ExtensionRuntimeDomainError> {
        let label = label.as_ref();
        if label.is_empty()
            || label.len() > 128
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ExtensionRuntimeDomainError::InvalidTrustDomain);
        }
        Ok(Self {
            fingerprint: sha256_hex(b"ygg-extension-trust-domain-v1\0", label),
        })
    }

    /// Returns the ordinary local-host trust partition.
    pub fn ordinary() -> Self {
        // This literal is validated above and cannot fail.
        Self::new("ordinary").expect("ordinary trust domain is valid")
    }

    /// Returns the path-free trust partition fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// Host family owning a runtime domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionRuntimeHostKind {
    /// The terminal, print, RPC, or native local host.
    Ordinary,
    /// A Serve host. It always uses a separately supplied trust partition.
    Serve,
}

/// Canonical workspace plus explicit trust partition for one manager.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExtensionRuntimeDomain {
    workspace: CanonicalWorkspace,
    trust_domain: ExtensionTrustDomain,
    host_kind: ExtensionRuntimeHostKind,
    fingerprint: String,
}

impl ExtensionRuntimeDomain {
    /// Creates the ordinary local-host domain for an existing workspace.
    pub fn ordinary(workspace: impl AsRef<Path>) -> Result<Self, ExtensionRuntimeDomainError> {
        Self::new(
            CanonicalWorkspace::new(workspace)?,
            ExtensionTrustDomain::ordinary(),
            ExtensionRuntimeHostKind::Ordinary,
        )
    }

    /// Creates a Serve domain. Callers must supply the project/session trust
    /// partition explicitly; Serve and ordinary hosts never share by accident.
    pub fn serve(
        workspace: impl AsRef<Path>,
        trust_domain: ExtensionTrustDomain,
    ) -> Result<Self, ExtensionRuntimeDomainError> {
        Self::new(
            CanonicalWorkspace::new(workspace)?,
            trust_domain,
            ExtensionRuntimeHostKind::Serve,
        )
    }

    /// Creates a domain from already canonical workspace identity and an
    /// explicit trust partition.
    pub fn new(
        workspace: CanonicalWorkspace,
        trust_domain: ExtensionTrustDomain,
        host_kind: ExtensionRuntimeHostKind,
    ) -> Result<Self, ExtensionRuntimeDomainError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(workspace.fingerprint().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(trust_domain.fingerprint().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(match host_kind {
            ExtensionRuntimeHostKind::Ordinary => b"ordinary",
            ExtensionRuntimeHostKind::Serve => b"serve",
        });
        Ok(Self {
            workspace,
            trust_domain,
            host_kind,
            fingerprint: sha256_hex(b"ygg-extension-runtime-domain-v1\0", bytes),
        })
    }

    /// Returns the canonical workspace identity.
    pub fn workspace(&self) -> &CanonicalWorkspace {
        &self.workspace
    }

    /// Returns the explicit trust-domain identity.
    pub fn trust_domain(&self) -> &ExtensionTrustDomain {
        &self.trust_domain
    }

    /// Returns whether this is an ordinary or Serve runtime partition.
    pub fn host_kind(&self) -> ExtensionRuntimeHostKind {
        self.host_kind
    }

    /// Returns the complete path-free domain fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// SHA-256 content identity used for explicit runtime sharing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExtensionContentDigest(String);

impl ExtensionContentDigest {
    /// Parses an externally stored lowercase SHA-256 digest.
    pub fn parse(value: impl Into<String>) -> Result<Self, ExtensionRuntimeDomainError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ExtensionRuntimeDomainError::InvalidContentDigest);
        }
        Ok(Self(value))
    }

    /// Returns the lowercase digest text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Non-fatal static-catalog diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionRuntimeCatalogDiagnostic {
    /// Manifest-selected extension name, when it was parseable.
    pub extension: String,
    /// Bounded path-free diagnostic category.
    pub message: String,
}

/// One static catalog entry. Constructing this value never launches a process.
#[derive(Clone, Debug)]
pub struct ExtensionRuntimeCatalogEntry {
    /// The validated selected manifest and explicit activation policy.
    pub descriptor: DiscoveredExtension,
    /// Digest of the manifest and resolved local entrypoint content.
    pub content_digest: ExtensionContentDigest,
    /// Whether the entrypoint content was directly verified. Workspace sharing
    /// requires this to be true; isolated legacy execution remains compatible
    /// with PATH-resolved commands.
    pub source_verified: bool,
}

impl ExtensionRuntimeCatalogEntry {
    /// Returns the selected lifecycle profile.
    pub fn lifecycle(&self) -> ExtensionLifecycleProfile {
        self.descriptor.manifest.runtime.lifecycle
    }

    /// Returns the explicit sharing selection.
    pub fn sharing(&self) -> ExtensionRuntimeSharing {
        self.descriptor.manifest.runtime.sharing
    }

    fn current_digest(&self) -> Result<(ExtensionContentDigest, bool), String> {
        catalog_content_digest(&self.descriptor)
    }
}

/// Static content-bound catalog used by a runtime manager.
#[derive(Clone, Debug, Default)]
pub struct ExtensionRuntimeCatalog {
    entries: BTreeMap<String, ExtensionRuntimeCatalogEntry>,
    diagnostics: Vec<ExtensionRuntimeCatalogDiagnostic>,
}

impl ExtensionRuntimeCatalog {
    /// Builds a static catalog from already discovered descriptors.
    ///
    /// This reads only bounded regular files needed for digesting and never
    /// executes an extension. Invalid source fingerprints remain inspectable
    /// diagnostics instead of making unrelated entries disappear.
    pub fn from_descriptors(descriptors: impl IntoIterator<Item = DiscoveredExtension>) -> Self {
        let mut catalog = Self::default();
        for descriptor in descriptors {
            let name = descriptor.manifest.name.clone();
            if catalog.entries.contains_key(&name) {
                catalog.diagnostics.push(ExtensionRuntimeCatalogDiagnostic {
                    extension: name,
                    message: "duplicate selected extension name".into(),
                });
                continue;
            }
            let (content_digest, source_verified) = match catalog_content_digest(&descriptor) {
                Ok(value) => value,
                Err(message) => {
                    catalog.diagnostics.push(ExtensionRuntimeCatalogDiagnostic {
                        extension: name.clone(),
                        message,
                    });
                    // Keep isolated legacy behavior available while preventing
                    // unverified source sharing. The fallback remains stable for
                    // the parsed manifest and cannot collide with verified
                    // content because it has a separate domain tag.
                    let encoded = serde_json::to_vec(&descriptor.manifest).unwrap_or_default();
                    (
                        ExtensionContentDigest(sha256_hex(
                            b"ygg-extension-unverified-catalog-v1\0",
                            encoded,
                        )),
                        false,
                    )
                }
            };
            catalog.entries.insert(
                name,
                ExtensionRuntimeCatalogEntry {
                    descriptor,
                    content_digest,
                    source_verified,
                },
            );
        }
        catalog
    }

    /// Returns an entry by selected manifest name.
    pub fn get(&self, name: &str) -> Option<&ExtensionRuntimeCatalogEntry> {
        self.entries.get(name)
    }

    /// Returns all entries in deterministic selected-name order.
    pub fn entries(&self) -> impl Iterator<Item = &ExtensionRuntimeCatalogEntry> {
        self.entries.values()
    }

    /// Returns non-fatal source/deduplication diagnostics.
    pub fn diagnostics(&self) -> &[ExtensionRuntimeCatalogDiagnostic] {
        &self.diagnostics
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        std::path::absolute(path).map_err(|_| "source path cannot be resolved".into())
    }
}

fn catalog_content_digest(
    descriptor: &DiscoveredExtension,
) -> Result<(ExtensionContentDigest, bool), String> {
    let manifest_path = absolute_path(&descriptor.manifest_path)?;
    let manifest = read_regular_file_bounded(&manifest_path, 64 * 1024)
        .map_err(|_| "manifest content cannot be verified".to_owned())?;
    let mut hasher = Sha256::new();
    hasher.update(b"ygg-extension-runtime-content-v1\0manifest\0");
    hasher.update(&manifest);
    hasher.update(b"\0entrypoint\0");
    let entrypoint = serde_json::to_vec(&descriptor.manifest.entrypoint)
        .map_err(|_| "entrypoint metadata cannot be encoded".to_owned())?;
    hasher.update(entrypoint);

    let configured = PathBuf::from(&descriptor.manifest.entrypoint.command);
    let local = if configured.is_absolute() {
        Some(configured)
    } else {
        descriptor
            .manifest_path
            .parent()
            .map(|directory| directory.join(configured))
    };
    let Some(local) = local else {
        return Err("entrypoint source cannot be located".into());
    };
    let local = absolute_path(&local)?;
    match read_regular_file_bounded(&local, MAX_CATALOG_SOURCE_BYTES) {
        Ok(bytes) => {
            hasher.update(b"\0source\0");
            hasher.update(bytes);
            Ok((
                ExtensionContentDigest(format!("{:x}", hasher.finalize())),
                true,
            ))
        }
        Err(_) => {
            // A PATH-resolved executable is a compatible legacy launch form,
            // but it is intentionally not eligible for content-digested
            // sharing because the manager cannot bind its bytes safely here.
            hasher.update(b"\0unverified-source\0");
            Ok((
                ExtensionContentDigest(format!("{:x}", hasher.finalize())),
                false,
            ))
        }
    }
}

/// Aggregate process-fleet limits enforced by a runtime manager.
#[derive(Clone, Debug)]
pub struct ExtensionRuntimeBudget {
    /// Maximum simultaneously owned child processes.
    pub max_processes: usize,
    /// Maximum conservatively reserved extension file descriptors.
    pub max_file_descriptors: usize,
    /// Maximum conservatively reserved buffered protocol bytes.
    pub max_buffered_bytes: usize,
    /// Maximum launches/handshakes in progress at once.
    pub max_concurrent_startups: usize,
    /// Maximum wait for a startup permit and one launch/handshake.
    pub startup_timeout: Duration,
    /// Maximum manual reloads in one restart window per runtime.
    pub max_reloads_per_window: usize,
    /// Maximum crash restarts in one restart window per runtime.
    pub max_restarts_per_window: usize,
    /// Rolling window for reload and restart-storm governance.
    pub restart_window: Duration,
    /// Base delay before a manager-supervised crash restart retry.
    pub restart_backoff: Duration,
}

impl Default for ExtensionRuntimeBudget {
    fn default() -> Self {
        Self {
            max_processes: 64,
            max_file_descriptors: 256,
            max_buffered_bytes: 16 * 1024 * 1024 * 1024,
            max_concurrent_startups: 4,
            startup_timeout: Duration::from_secs(30),
            max_reloads_per_window: 16,
            max_restarts_per_window: 8,
            restart_window: Duration::from_secs(60),
            restart_backoff: Duration::from_millis(250),
        }
    }
}

impl ExtensionRuntimeBudget {
    fn validate(&self) -> Result<(), ExtensionRuntimeManagerError> {
        if self.max_processes == 0
            || self.max_file_descriptors == 0
            || self.max_buffered_bytes == 0
            || self.max_concurrent_startups == 0
            || self.startup_timeout.is_zero()
            || self.max_reloads_per_window == 0
            || self.max_restarts_per_window == 0
            || self.restart_window.is_zero()
        {
            return Err(ExtensionRuntimeManagerError::InvalidBudget);
        }
        Ok(())
    }
}

/// Governed resource kind that was exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionRuntimeResource {
    /// Child-process count.
    Processes,
    /// Conservatively reserved standard-stream/process descriptors.
    FileDescriptors,
    /// Conservatively reserved protocol buffering.
    BufferedBytes,
    /// Concurrent launch or handshake slots.
    StartupConcurrency,
    /// Launch or handshake wall-clock time.
    StartupTime,
    /// Explicit reload-rate budget.
    Reloads,
    /// Automatic crash-restart storm budget.
    RestartStorm,
}

/// Secret-safe runtime provenance included in visible governance outcomes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionRuntimeProvenance {
    /// Manifest-selected extension name.
    pub extension: String,
    /// Content-bound runtime digest.
    pub content_digest: String,
    /// Manifest-selected lifecycle profile.
    pub lifecycle: ExtensionLifecycleProfile,
}

/// Typed, visible aggregate resource exhaustion outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionResourceExhausted {
    /// Exhausted aggregate resource.
    pub resource: ExtensionRuntimeResource,
    /// Configured hard limit.
    pub limit: u64,
    /// New reservation or event request.
    pub requested: u64,
    /// Usage retained before the rejected request.
    pub in_use: u64,
    /// Secret-safe ownership/provenance.
    pub provenance: ExtensionRuntimeProvenance,
}

impl std::fmt::Display for ExtensionResourceExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} budget exhausted ({} used + {} requested; limit {}) for {}",
            self.resource, self.in_use, self.requested, self.limit, self.provenance.extension
        )
    }
}

impl std::error::Error for ExtensionResourceExhausted {}

impl ExtensionResourceExhausted {
    /// API 0.3's stable JSON-RPC code for this outcome.
    pub const JSON_RPC_CODE: i64 = -32012;

    /// Returns API 0.3's stable error name.
    pub const fn api_error_name(&self) -> &'static str {
        "resource_exhausted"
    }
}

/// Public classification of a process launch/reload failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionRuntimeFailure {
    /// The manifest was disabled or untrusted.
    NotEligible,
    /// Source content changed or was no longer verifiable for a shared runtime.
    StaleSource,
    /// Launching the child failed.
    Launch,
    /// Initialization/negotiation rejected the child.
    Protocol,
    /// The manager startup timer elapsed.
    StartupTimeout,
    /// A runtime was shut down while a start was pending.
    ManagerClosed,
}

/// Runtime-manager operation error.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionRuntimeManagerError {
    /// The configured aggregate budget is invalid.
    #[error("invalid extension runtime budget")]
    InvalidBudget,
    /// A caller asked for an entry absent from the static catalog.
    #[error("extension runtime entry is unavailable")]
    UnknownExtension,
    /// A process is disabled, untrusted, or blocked by the caller's gate.
    #[error("extension runtime is not eligible")]
    NotEligible,
    /// A shared profile lacks a directly verified content source.
    #[error("extension runtime source cannot be content-bound for sharing")]
    UnverifiedSharedSource,
    /// A static source changed before start/reload and was retired fail-closed.
    #[error("extension runtime source changed")]
    StaleSource,
    /// A session binding was already released.
    #[error("extension runtime session binding is closed")]
    BindingClosed,
    /// The manager was shut down.
    #[error("extension runtime manager is shut down")]
    ManagerClosed,
    /// A caller supplied a config for another workspace domain.
    #[error("extension runtime configuration belongs to another workspace")]
    WorkspaceMismatch,
    /// A shared runtime requested session-specific reverse services.
    #[error("shared extension runtime cannot expose session-specific host services")]
    SharedServiceUnsupported,
    /// API 0.1 cannot carry resource-owner fences required for sharing.
    #[error("shared extension runtime requires API 0.2 or newer")]
    SharedApiUnsupported,
    /// One aggregate resource was exhausted.
    #[error(transparent)]
    ResourceExhausted(#[from] ExtensionResourceExhausted),
    /// Process start/reload failed without exposing arbitrary child diagnostics.
    #[error("extension runtime {failure:?}")]
    Failed {
        /// Bounded secret-safe failure class.
        failure: ExtensionRuntimeFailure,
    },
}

/// Current externally observable manager state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionManagedRuntimeState {
    /// The static entry is eligible but has not been activated.
    Eligible,
    /// The entry is disabled or lacks explicit executable trust.
    Inactive,
    /// A launch/handshake owns a bounded startup slot.
    Starting,
    /// A resident process is ready for use.
    Ready,
    /// The manager is delaying an automatic restart.
    Backoff,
    /// A runtime was retired because its bound source changed.
    StaleSource,
    /// A governed resource prevented activation/reload.
    ResourceExhausted,
    /// Restart policy parked the runtime after a terminal failure.
    Parked,
    /// The runtime was deliberately stopped.
    Stopped,
}

/// Resource usage charged to one active runtime or the aggregate manager.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionRuntimeUsage {
    /// Process count.
    pub processes: usize,
    /// Conservatively reserved file descriptors.
    pub file_descriptors: usize,
    /// Conservatively reserved buffered bytes.
    pub buffered_bytes: usize,
}

/// Inspectable status for a static entry or durable active runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionRuntimeStatus {
    /// Secret-safe static ownership identity.
    pub provenance: ExtensionRuntimeProvenance,
    /// Current lifecycle state.
    pub state: ExtensionManagedRuntimeState,
    /// Number of current session bindings attached to the process.
    pub bindings: usize,
    /// Charged resource usage while active.
    pub usage: ExtensionRuntimeUsage,
    /// Most recent typed resource rejection, when one occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_exhausted: Option<ExtensionResourceExhausted>,
    /// Bounded public process failure class, without child stderr or paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ExtensionRuntimeFailure>,
}

/// Result of activating one static entry through a session binding.
#[derive(Clone)]
pub struct ExtensionRuntimeLease {
    process: ExtensionProcess,
    provenance: ExtensionRuntimeProvenance,
    shared: bool,
    one_shot: bool,
}

impl ExtensionRuntimeLease {
    /// Returns the active extension process handle.
    pub fn process(&self) -> &ExtensionProcess {
        &self.process
    }

    /// Returns the secret-safe runtime identity.
    pub fn provenance(&self) -> &ExtensionRuntimeProvenance {
        &self.provenance
    }

    /// Returns whether this activation attached to a pre-existing shared
    /// process rather than starting a new child.
    pub fn shared(&self) -> bool {
        self.shared
    }

    /// Returns whether the binding must settle this lease at the operation
    /// boundary to stop its one-shot process.
    pub fn is_one_shot(&self) -> bool {
        self.one_shot
    }
}

/// Deterministic report for eager profile activation.
#[derive(Clone)]
pub struct ExtensionRuntimeActivation {
    /// Selected manifest name.
    pub extension: String,
    /// Secret-safe identity, when catalog construction succeeded.
    pub provenance: Option<ExtensionRuntimeProvenance>,
    /// Successful process handle, if activation was admitted.
    pub process: Option<ExtensionProcess>,
    /// Whether a successful activation reused an existing shared process.
    pub shared: bool,
    /// Typed visible outcome.
    pub outcome: ExtensionRuntimeActivationOutcome,
}

/// Typed eager activation outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionRuntimeActivationOutcome {
    /// The runtime is ready.
    Ready,
    /// The entry was not eligible under the caller's explicit activation gate.
    Inactive,
    /// A source change was rejected fail-closed.
    StaleSource,
    /// A typed aggregate budget was exhausted.
    ResourceExhausted(ExtensionResourceExhausted),
    /// A bounded startup failure occurred.
    Failed(ExtensionRuntimeFailure),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RuntimeScope {
    Shared,
    Binding(u64),
    OneShot(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RuntimeKey {
    domain_digest: String,
    extension: String,
    content_digest: String,
    scope: RuntimeScope,
}

struct ManagedRuntime {
    descriptor: DiscoveredExtension,
    provenance: ExtensionRuntimeProvenance,
    process: ExtensionProcess,
    usage: ExtensionRuntimeUsage,
    bindings: BTreeSet<u64>,
    lifecycle: ExtensionLifecycleProfile,
    sharing: ExtensionRuntimeSharing,
    state: ExtensionManagedRuntimeState,
    reloads: VecDeque<Instant>,
    restarts: VecDeque<Instant>,
    restart_attempt: u32,
    next_restart: Option<Instant>,
    gate: Arc<Mutex<()>>,
}

struct RecentStatus {
    provenance: ExtensionRuntimeProvenance,
    state: ExtensionManagedRuntimeState,
    resource_exhausted: Option<ExtensionResourceExhausted>,
    failure: Option<ExtensionRuntimeFailure>,
}

/// A launch reservation that is visible while a process is being initialized.
///
/// Keeping provenance and its charge here makes startup wait and budget state
/// inspectable without exposing the child command, workspace path, or stderr.
struct StartingRuntime {
    notify: Arc<Notify>,
    provenance: ExtensionRuntimeProvenance,
    usage: ExtensionRuntimeUsage,
}

#[derive(Default)]
struct ManagerState {
    active: BTreeMap<RuntimeKey, ManagedRuntime>,
    starting: BTreeMap<RuntimeKey, StartingRuntime>,
    usage: ExtensionRuntimeUsage,
    recent: BTreeMap<String, RecentStatus>,
}

struct ManagerInner {
    domain: ExtensionRuntimeDomain,
    budget: ExtensionRuntimeBudget,
    catalog: StdRwLock<ExtensionRuntimeCatalog>,
    state: StdMutex<ManagerState>,
    startup_slots: Arc<Semaphore>,
    shutdown: AtomicBool,
    monitor_started: AtomicBool,
    next_binding: AtomicU64,
    next_one_shot: AtomicU64,
}

/// One durable owner of an ordinary-host or explicit Serve-partition process fleet.
#[derive(Clone)]
pub struct ExtensionRuntimeManager {
    inner: Arc<ManagerInner>,
}

impl ExtensionRuntimeManager {
    /// Creates a manager with an empty static catalog and default governance.
    pub fn new(domain: ExtensionRuntimeDomain) -> Self {
        Self::with_budget(domain, ExtensionRuntimeBudget::default())
            .expect("default extension runtime budget is valid")
    }

    /// Creates a manager with explicit aggregate governance limits.
    pub fn with_budget(
        domain: ExtensionRuntimeDomain,
        budget: ExtensionRuntimeBudget,
    ) -> Result<Self, ExtensionRuntimeManagerError> {
        budget.validate()?;
        Ok(Self {
            inner: Arc::new(ManagerInner {
                domain,
                startup_slots: Arc::new(Semaphore::new(budget.max_concurrent_startups)),
                budget,
                catalog: StdRwLock::new(ExtensionRuntimeCatalog::default()),
                state: StdMutex::new(ManagerState::default()),
                shutdown: AtomicBool::new(false),
                monitor_started: AtomicBool::new(false),
                next_binding: AtomicU64::new(1),
                next_one_shot: AtomicU64::new(1),
            }),
        })
    }

    /// Returns the immutable canonical workspace/trust domain.
    pub fn domain(&self) -> &ExtensionRuntimeDomain {
        &self.inner.domain
    }

    /// Replaces the static catalog without launching any process.
    ///
    /// Runtimes whose selected source is replaced or removed are stopped before
    /// the method returns. This preserves one durable owner and prevents an old
    /// process from being silently attached under a new content identity.
    pub async fn replace_catalog(&self, catalog: ExtensionRuntimeCatalog) {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let changed = {
            let mut current = write(&self.inner.catalog);
            let mut changed = Vec::new();
            let state = lock(&self.inner.state);
            for (key, runtime) in &state.active {
                let replacement = match catalog.get(&runtime.descriptor.manifest.name) {
                    None => Some(ExtensionManagedRuntimeState::Stopped),
                    Some(entry)
                        if !Self::entry_is_eligible(entry)
                            || entry.lifecycle() != runtime.lifecycle
                            || entry.sharing() != runtime.sharing =>
                    {
                        Some(ExtensionManagedRuntimeState::Inactive)
                    }
                    Some(entry)
                        if entry.content_digest.as_str() != key.content_digest
                            || (entry.sharing() == ExtensionRuntimeSharing::Workspace
                                && !entry.source_verified) =>
                    {
                        Some(ExtensionManagedRuntimeState::StaleSource)
                    }
                    Some(_) => None,
                };
                if let Some(replacement) = replacement {
                    changed.push((key.clone(), replacement));
                }
            }
            *current = catalog;
            changed
        };
        for (key, replacement) in changed {
            self.stop_key(&key, Some(replacement)).await;
        }
    }

    /// Returns a static catalog snapshot. Reading this has no activation side effects.
    pub fn catalog(&self) -> ExtensionRuntimeCatalog {
        read(&self.inner.catalog).clone()
    }

    /// Creates a session binding. The opaque owner is only hashed for runtime
    /// keys and never emitted in provenance/status.
    pub fn bind_session(
        &self,
        session_owner: impl AsRef<str>,
    ) -> Result<ExtensionSessionBinding, ExtensionRuntimeManagerError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeManagerError::ManagerClosed);
        }
        let session_owner = session_owner.as_ref();
        if session_owner.is_empty() || session_owner.len() > 512 {
            return Err(ExtensionRuntimeManagerError::BindingClosed);
        }
        let id = self.inner.next_binding.fetch_add(1, Ordering::Relaxed);
        Ok(ExtensionSessionBinding {
            manager: self.clone(),
            id,
            session_digest: sha256_hex(b"ygg-extension-session-binding-v1\0", session_owner),
            active: Arc::new(StdMutex::new(BTreeSet::new())),
            released: Arc::new(AtomicBool::new(false)),
            // Activation fans out cloned handles. Only the final binding handle
            // may perform Drop-based cleanup; a completed activation must not
            // release the session attachment that owns the returned process.
            owners: Arc::new(()),
        })
    }

    /// Returns static and active runtime status without starting eligible entries.
    pub fn statuses(&self) -> Vec<ExtensionRuntimeStatus> {
        let catalog = self.catalog();
        let state = lock(&self.inner.state);
        let mut statuses = BTreeMap::<(String, String), ExtensionRuntimeStatus>::new();
        for entry in catalog.entries() {
            let provenance = self.provenance(entry);
            let state_value = if entry.descriptor.activation.enabled
                && entry.descriptor.activation.trust == ExtensionTrust::Trusted
            {
                ExtensionManagedRuntimeState::Eligible
            } else {
                ExtensionManagedRuntimeState::Inactive
            };
            statuses.insert(
                (
                    provenance.extension.clone(),
                    provenance.content_digest.clone(),
                ),
                ExtensionRuntimeStatus {
                    provenance,
                    state: state_value,
                    bindings: 0,
                    usage: ExtensionRuntimeUsage::default(),
                    resource_exhausted: None,
                    failure: None,
                },
            );
        }
        let is_selected_identity = |provenance: &ExtensionRuntimeProvenance| {
            catalog.get(&provenance.extension).is_some_and(|entry| {
                entry.content_digest.as_str() == provenance.content_digest
                    && entry.lifecycle() == provenance.lifecycle
            })
        };
        for starting in state.starting.values() {
            // A replaced entry can still be winding down its startup future.
            // It is never presented as the newly selected catalog entry.
            if !is_selected_identity(&starting.provenance) {
                continue;
            }
            statuses.insert(
                (
                    starting.provenance.extension.clone(),
                    starting.provenance.content_digest.clone(),
                ),
                ExtensionRuntimeStatus {
                    provenance: starting.provenance.clone(),
                    state: ExtensionManagedRuntimeState::Starting,
                    bindings: 0,
                    usage: starting.usage,
                    resource_exhausted: None,
                    failure: None,
                },
            );
        }
        for runtime in state.active.values() {
            // `replace_catalog` removes changed runtimes before waiting for
            // child shutdown, but retain this filter for observers racing the
            // catalog transition.
            if !is_selected_identity(&runtime.provenance) {
                continue;
            }
            statuses.insert(
                (
                    runtime.provenance.extension.clone(),
                    runtime.provenance.content_digest.clone(),
                ),
                ExtensionRuntimeStatus {
                    provenance: runtime.provenance.clone(),
                    state: runtime.state,
                    bindings: runtime.bindings.len(),
                    usage: runtime.usage,
                    resource_exhausted: None,
                    failure: None,
                },
            );
        }
        for recent in state.recent.values() {
            // A status belongs only to the catalog identity that produced it.
            // Do not let a prior source's stale/exhausted status overwrite a
            // newly selected manifest with the same display name.
            if !is_selected_identity(&recent.provenance) {
                continue;
            }
            let key = (
                recent.provenance.extension.clone(),
                recent.provenance.content_digest.clone(),
            );
            if let Some(status) = statuses.get_mut(&key) {
                status.state = recent.state;
                status.resource_exhausted = recent.resource_exhausted.clone();
                status.failure = recent.failure;
            }
        }
        statuses.into_values().collect()
    }

    /// Returns aggregate resource usage currently charged to the fleet.
    pub fn usage(&self) -> ExtensionRuntimeUsage {
        lock(&self.inner.state).usage
    }

    /// Reloads every currently active runtime with this manifest-selected name.
    ///
    /// Candidate-first reload uses a transient second reservation while the old
    /// process is still alive, so it fails visibly rather than oversubscribing
    /// file descriptors, process count, or protocol buffering.
    pub async fn reload(
        &self,
        extension: &str,
    ) -> Vec<Result<ExtensionReloadReport, ExtensionRuntimeManagerError>> {
        let keys = {
            let state = lock(&self.inner.state);
            state
                .active
                .iter()
                .filter(|(_, runtime)| runtime.descriptor.manifest.name == extension)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>()
        };
        join_all(keys.into_iter().map(|key| self.reload_key(key, false))).await
    }

    /// Stops every fleet process after cancelling/draining its protocol work.
    pub async fn shutdown(&self) {
        if self.inner.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        // Terminal shutdown closes pending permit acquisition immediately;
        // start reservations then release their charges and wake coalesced
        // activation callers instead of waiting for the startup timeout.
        self.inner.startup_slots.close();
        let (runtimes, starters) = {
            let mut state = lock(&self.inner.state);
            state.usage = ExtensionRuntimeUsage::default();
            let starters = std::mem::take(&mut state.starting)
                .into_values()
                .map(|starting| starting.notify)
                .collect::<Vec<_>>();
            let runtimes = std::mem::take(&mut state.active)
                .into_values()
                .map(|runtime| runtime.process)
                .collect::<Vec<_>>();
            (runtimes, starters)
        };
        for starter in starters {
            starter.notify_waiters();
        }
        let _ = join_all(runtimes.iter().map(ExtensionProcess::shutdown)).await;
    }

    fn entry_is_eligible(entry: &ExtensionRuntimeCatalogEntry) -> bool {
        entry.descriptor.activation.enabled
            && entry.descriptor.activation.trust == ExtensionTrust::Trusted
    }

    fn validate_catalog_identity(
        key: &RuntimeKey,
        expected: &ExtensionRuntimeCatalogEntry,
        selected: Option<&ExtensionRuntimeCatalogEntry>,
    ) -> Result<(), ExtensionRuntimeManagerError> {
        let entry = selected.ok_or(ExtensionRuntimeManagerError::StaleSource)?;
        if entry.content_digest != expected.content_digest
            || entry.content_digest.as_str() != key.content_digest
            || entry.lifecycle() != expected.lifecycle()
            || entry.sharing() != expected.sharing()
        {
            return Err(ExtensionRuntimeManagerError::StaleSource);
        }
        if !Self::entry_is_eligible(entry) {
            return Err(ExtensionRuntimeManagerError::NotEligible);
        }
        Ok(())
    }

    /// Rechecks the selected catalog and source immediately before a freshly
    /// started child becomes attachable. This closes the gap between the first
    /// preflight fingerprint and a slow launch or concurrent catalog reload.
    fn validate_current_entry(
        &self,
        key: &RuntimeKey,
        expected: &ExtensionRuntimeCatalogEntry,
    ) -> Result<(), ExtensionRuntimeManagerError> {
        let entry = read(&self.inner.catalog)
            .get(&expected.descriptor.manifest.name)
            .cloned();
        Self::validate_catalog_identity(key, expected, entry.as_ref())?;
        let entry = entry.expect("validated catalog entry is present");
        let (current_digest, source_verified) = entry
            .current_digest()
            .map_err(|_| ExtensionRuntimeManagerError::StaleSource)?;
        if current_digest != entry.content_digest
            || (entry.sharing() == ExtensionRuntimeSharing::Workspace && !source_verified)
        {
            return Err(ExtensionRuntimeManagerError::StaleSource);
        }
        Ok(())
    }

    fn provenance(&self, entry: &ExtensionRuntimeCatalogEntry) -> ExtensionRuntimeProvenance {
        ExtensionRuntimeProvenance {
            extension: entry.descriptor.manifest.name.clone(),
            content_digest: entry.content_digest.as_str().to_owned(),
            lifecycle: entry.lifecycle(),
        }
    }

    fn runtime_key(
        &self,
        entry: &ExtensionRuntimeCatalogEntry,
        binding: &ExtensionSessionBinding,
    ) -> RuntimeKey {
        let scope = match entry.sharing() {
            ExtensionRuntimeSharing::Workspace => RuntimeScope::Shared,
            ExtensionRuntimeSharing::Isolated
                if entry.lifecycle() == ExtensionLifecycleProfile::OneShot =>
            {
                RuntimeScope::OneShot(self.inner.next_one_shot.fetch_add(1, Ordering::Relaxed))
            }
            ExtensionRuntimeSharing::Isolated => RuntimeScope::Binding(binding.id),
        };
        RuntimeKey {
            domain_digest: self.inner.domain.fingerprint().to_owned(),
            extension: entry.descriptor.manifest.name.clone(),
            content_digest: entry.content_digest.as_str().to_owned(),
            scope,
        }
    }

    fn estimated_usage(config: &ExtensionRuntimeConfig) -> ExtensionRuntimeUsage {
        let queue_items = config
            .writer_queue_capacity
            .saturating_add(config.max_pending_requests)
            .saturating_add(2);
        ExtensionRuntimeUsage {
            processes: 1,
            file_descriptors: ESTIMATED_PROCESS_FDS,
            buffered_bytes: config.max_message_bytes.saturating_mul(queue_items),
        }
    }

    fn reserve(
        &self,
        state: &mut ManagerState,
        requested: ExtensionRuntimeUsage,
        provenance: &ExtensionRuntimeProvenance,
    ) -> Result<(), ExtensionRuntimeManagerError> {
        let check =
            |resource: ExtensionRuntimeResource, limit: usize, in_use: usize, request: usize| {
                if in_use.saturating_add(request) > limit {
                    Err(ExtensionRuntimeManagerError::ResourceExhausted(
                        ExtensionResourceExhausted {
                            resource,
                            limit: limit as u64,
                            requested: request as u64,
                            in_use: in_use as u64,
                            provenance: provenance.clone(),
                        },
                    ))
                } else {
                    Ok(())
                }
            };
        check(
            ExtensionRuntimeResource::Processes,
            self.inner.budget.max_processes,
            state.usage.processes,
            requested.processes,
        )?;
        check(
            ExtensionRuntimeResource::FileDescriptors,
            self.inner.budget.max_file_descriptors,
            state.usage.file_descriptors,
            requested.file_descriptors,
        )?;
        check(
            ExtensionRuntimeResource::BufferedBytes,
            self.inner.budget.max_buffered_bytes,
            state.usage.buffered_bytes,
            requested.buffered_bytes,
        )?;
        state.usage.processes = state.usage.processes.saturating_add(requested.processes);
        state.usage.file_descriptors = state
            .usage
            .file_descriptors
            .saturating_add(requested.file_descriptors);
        state.usage.buffered_bytes = state
            .usage
            .buffered_bytes
            .saturating_add(requested.buffered_bytes);
        Ok(())
    }

    fn record_recent(
        &self,
        provenance: ExtensionRuntimeProvenance,
        state_value: ExtensionManagedRuntimeState,
        resource_exhausted: Option<ExtensionResourceExhausted>,
        failure: Option<ExtensionRuntimeFailure>,
    ) {
        lock(&self.inner.state).recent.insert(
            provenance.extension.clone(),
            RecentStatus {
                provenance,
                state: state_value,
                resource_exhausted,
                failure,
            },
        );
    }

    fn record_entry_validation_failure(
        &self,
        provenance: &ExtensionRuntimeProvenance,
        error: &ExtensionRuntimeManagerError,
    ) {
        match error {
            ExtensionRuntimeManagerError::NotEligible => self.record_recent(
                provenance.clone(),
                ExtensionManagedRuntimeState::Inactive,
                None,
                Some(ExtensionRuntimeFailure::NotEligible),
            ),
            ExtensionRuntimeManagerError::StaleSource => self.record_recent(
                provenance.clone(),
                ExtensionManagedRuntimeState::StaleSource,
                None,
                Some(ExtensionRuntimeFailure::StaleSource),
            ),
            _ => {}
        }
    }

    fn ensure_monitor(&self) {
        if self.inner.monitor_started.swap(true, Ordering::AcqRel)
            || self.inner.shutdown.load(Ordering::Acquire)
        {
            return;
        }
        let Ok(handle) = Handle::try_current() else {
            self.inner.monitor_started.store(false, Ordering::Release);
            return;
        };
        let manager = self.clone();
        handle.spawn(async move { manager.monitor().await });
    }

    async fn monitor(&self) {
        loop {
            if self.inner.shutdown.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            let keys = {
                let state = lock(&self.inner.state);
                state
                    .active
                    .iter()
                    .filter(|(_, runtime)| {
                        runtime.lifecycle != ExtensionLifecycleProfile::OneShot
                            && !runtime.process.is_running()
                            && matches!(
                                runtime.state,
                                ExtensionManagedRuntimeState::Ready
                                    | ExtensionManagedRuntimeState::Backoff
                            )
                            && runtime.next_restart.is_none_or(|next| next <= now)
                    })
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>()
            };
            for key in keys {
                let _ = self.reload_key(key, true).await;
            }
            tokio::time::sleep(SUPERVISOR_POLL).await;
        }
    }

    async fn stop_key(&self, key: &RuntimeKey, replacement: Option<ExtensionManagedRuntimeState>) {
        let runtime = {
            let mut state = lock(&self.inner.state);
            let runtime = state.active.remove(key);
            if let Some(runtime) = &runtime {
                state.usage.processes = state
                    .usage
                    .processes
                    .saturating_sub(runtime.usage.processes);
                state.usage.file_descriptors = state
                    .usage
                    .file_descriptors
                    .saturating_sub(runtime.usage.file_descriptors);
                state.usage.buffered_bytes = state
                    .usage
                    .buffered_bytes
                    .saturating_sub(runtime.usage.buffered_bytes);
                if let Some(replacement) = replacement {
                    state.recent.insert(
                        runtime.provenance.extension.clone(),
                        RecentStatus {
                            provenance: runtime.provenance.clone(),
                            state: replacement,
                            resource_exhausted: None,
                            failure: (replacement == ExtensionManagedRuntimeState::StaleSource)
                                .then_some(ExtensionRuntimeFailure::StaleSource),
                        },
                    );
                }
            }
            runtime
        };
        if let Some(runtime) = runtime {
            let _ = runtime.process.shutdown().await;
        }
    }

    async fn detach_binding(&self, binding_id: u64, keys: BTreeSet<RuntimeKey>) {
        let mut stop = Vec::new();
        {
            let mut state = lock(&self.inner.state);
            for key in keys {
                let should_stop = state.active.get_mut(&key).is_some_and(|runtime| {
                    runtime.bindings.remove(&binding_id);
                    runtime.bindings.is_empty()
                        && runtime.sharing == ExtensionRuntimeSharing::Isolated
                        && runtime.lifecycle != ExtensionLifecycleProfile::Always
                });
                if should_stop {
                    if let Some(runtime) = state.active.remove(&key) {
                        state.usage.processes = state
                            .usage
                            .processes
                            .saturating_sub(runtime.usage.processes);
                        state.usage.file_descriptors = state
                            .usage
                            .file_descriptors
                            .saturating_sub(runtime.usage.file_descriptors);
                        state.usage.buffered_bytes = state
                            .usage
                            .buffered_bytes
                            .saturating_sub(runtime.usage.buffered_bytes);
                        stop.push(runtime.process);
                    }
                }
            }
        }
        let _ = join_all(stop.iter().map(ExtensionProcess::shutdown)).await;
    }

    async fn settle_one_shots(&self, binding_id: u64, keys: BTreeSet<RuntimeKey>) {
        let targets = {
            let state = lock(&self.inner.state);
            keys.into_iter()
                .filter(|key| {
                    state.active.get(key).is_some_and(|runtime| {
                        runtime.lifecycle == ExtensionLifecycleProfile::OneShot
                            && runtime.bindings.contains(&binding_id)
                    })
                })
                .collect::<Vec<_>>()
        };
        for key in targets {
            self.stop_key(&key, Some(ExtensionManagedRuntimeState::Stopped))
                .await;
        }
    }

    async fn reload_key(
        &self,
        key: RuntimeKey,
        automatic: bool,
    ) -> Result<ExtensionReloadReport, ExtensionRuntimeManagerError> {
        let gate = {
            let state = lock(&self.inner.state);
            state
                .active
                .get(&key)
                .map(|runtime| Arc::clone(&runtime.gate))
        };
        let Some(gate) = gate else {
            return Err(ExtensionRuntimeManagerError::UnknownExtension);
        };
        let _gate = gate.lock().await;
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeManagerError::ManagerClosed);
        }
        let (descriptor, provenance, process, usage, was_running) = {
            let mut state = lock(&self.inner.state);
            let Some(runtime) = state.active.get_mut(&key) else {
                return Err(ExtensionRuntimeManagerError::UnknownExtension);
            };
            let now = Instant::now();
            let history = if automatic {
                &mut runtime.restarts
            } else {
                &mut runtime.reloads
            };
            while history
                .front()
                .is_some_and(|at| now.duration_since(*at) > self.inner.budget.restart_window)
            {
                history.pop_front();
            }
            let limit = if automatic {
                self.inner.budget.max_restarts_per_window
            } else {
                self.inner.budget.max_reloads_per_window
            };
            if history.len() >= limit {
                let resource = if automatic {
                    ExtensionRuntimeResource::RestartStorm
                } else {
                    ExtensionRuntimeResource::Reloads
                };
                let exhausted = ExtensionResourceExhausted {
                    resource,
                    limit: limit as u64,
                    requested: 1,
                    in_use: history.len() as u64,
                    provenance: runtime.provenance.clone(),
                };
                let runtime_state = if automatic {
                    ExtensionManagedRuntimeState::Parked
                } else {
                    ExtensionManagedRuntimeState::ResourceExhausted
                };
                runtime.state = runtime_state;
                let extension = runtime.provenance.extension.clone();
                let recent = RecentStatus {
                    provenance: runtime.provenance.clone(),
                    state: runtime_state,
                    resource_exhausted: Some(exhausted.clone()),
                    failure: None,
                };
                // Do not retain the runtime borrow while updating the separate
                // bounded public diagnostic map.
                let _ = runtime;
                state.recent.insert(extension, recent);
                return Err(exhausted.into());
            }
            history.push_back(now);
            runtime.state = ExtensionManagedRuntimeState::Starting;
            (
                runtime.descriptor.clone(),
                runtime.provenance.clone(),
                runtime.process.clone(),
                runtime.usage,
                runtime.process.is_running(),
            )
        };

        let entry = read(&self.inner.catalog)
            .get(&descriptor.manifest.name)
            .cloned();
        let Some(entry) = entry else {
            self.stop_key(&key, Some(ExtensionManagedRuntimeState::StaleSource))
                .await;
            return Err(ExtensionRuntimeManagerError::StaleSource);
        };
        if let Err(error) = self.validate_current_entry(&key, &entry) {
            let replacement = if matches!(&error, ExtensionRuntimeManagerError::NotEligible) {
                ExtensionManagedRuntimeState::Inactive
            } else {
                ExtensionManagedRuntimeState::StaleSource
            };
            self.stop_key(&key, Some(replacement)).await;
            self.record_entry_validation_failure(&provenance, &error);
            return Err(error);
        }

        let transient = if was_running {
            let mut state = lock(&self.inner.state);
            match self.reserve(&mut state, usage, &provenance) {
                Ok(()) => Some(UsageReservation {
                    manager: Arc::downgrade(&self.inner),
                    usage,
                    armed: true,
                }),
                Err(error) => {
                    if let ExtensionRuntimeManagerError::ResourceExhausted(exhausted) = &error {
                        state.recent.insert(
                            provenance.extension.clone(),
                            RecentStatus {
                                provenance: provenance.clone(),
                                state: ExtensionManagedRuntimeState::ResourceExhausted,
                                resource_exhausted: Some(exhausted.clone()),
                                failure: None,
                            },
                        );
                    }
                    if let Some(runtime) = state.active.get_mut(&key) {
                        runtime.state = ExtensionManagedRuntimeState::Ready;
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };

        let permit = match self.acquire_startup(&provenance).await {
            Ok(permit) => permit,
            Err(error) => {
                if let ExtensionRuntimeManagerError::ResourceExhausted(exhausted) = &error {
                    self.record_reload_exhaustion(&key, automatic, exhausted.clone());
                }
                return Err(error);
            }
        };
        let result =
            tokio::time::timeout(self.inner.budget.startup_timeout, process.reload()).await;
        drop(permit);
        drop(transient);
        match result {
            Ok(Ok(report)) => {
                if self.inner.shutdown.load(Ordering::Acquire) {
                    return Err(ExtensionRuntimeManagerError::ManagerClosed);
                }
                if let Err(error) = self.validate_current_entry(&key, &entry) {
                    let replacement = if matches!(&error, ExtensionRuntimeManagerError::NotEligible)
                    {
                        ExtensionManagedRuntimeState::Inactive
                    } else {
                        ExtensionManagedRuntimeState::StaleSource
                    };
                    self.stop_key(&key, Some(replacement)).await;
                    self.record_entry_validation_failure(&provenance, &error);
                    return Err(error);
                }
                let mut state = lock(&self.inner.state);
                if let Some(runtime) = state.active.get_mut(&key) {
                    runtime.state = ExtensionManagedRuntimeState::Ready;
                    runtime.restart_attempt = 0;
                    runtime.next_restart = None;
                }
                Ok(report)
            }
            Ok(Err(error)) => {
                self.record_reload_failure(&key, automatic, &provenance, &error);
                Err(ExtensionRuntimeManagerError::Failed {
                    failure: classify_process_failure(&error),
                })
            }
            Err(_) => {
                let exhausted = ExtensionResourceExhausted {
                    resource: ExtensionRuntimeResource::StartupTime,
                    limit: self.inner.budget.startup_timeout.as_millis() as u64,
                    requested: self.inner.budget.startup_timeout.as_millis() as u64,
                    in_use: 0,
                    provenance: provenance.clone(),
                };
                self.record_reload_exhaustion(&key, automatic, exhausted.clone());
                Err(exhausted.into())
            }
        }
    }

    fn record_reload_failure(
        &self,
        key: &RuntimeKey,
        automatic: bool,
        provenance: &ExtensionRuntimeProvenance,
        error: &ProcessRuntimeError,
    ) {
        let failure = classify_process_failure(error);
        let mut state = lock(&self.inner.state);
        if let Some(runtime) = state.active.get_mut(key) {
            runtime.state = if automatic {
                runtime.restart_attempt = runtime.restart_attempt.saturating_add(1);
                let multiplier = 1_u32 << runtime.restart_attempt.saturating_sub(1).min(8);
                runtime.next_restart = Some(
                    Instant::now() + self.inner.budget.restart_backoff.saturating_mul(multiplier),
                );
                ExtensionManagedRuntimeState::Backoff
            } else if runtime.process.is_running() {
                ExtensionManagedRuntimeState::Ready
            } else {
                ExtensionManagedRuntimeState::Parked
            };
        }
        state.recent.insert(
            provenance.extension.clone(),
            RecentStatus {
                provenance: provenance.clone(),
                state: if automatic {
                    ExtensionManagedRuntimeState::Backoff
                } else {
                    ExtensionManagedRuntimeState::Parked
                },
                resource_exhausted: None,
                failure: Some(failure),
            },
        );
    }

    fn record_reload_exhaustion(
        &self,
        key: &RuntimeKey,
        automatic: bool,
        exhausted: ExtensionResourceExhausted,
    ) {
        let mut state = lock(&self.inner.state);
        if let Some(runtime) = state.active.get_mut(key) {
            runtime.state = if automatic {
                runtime.restart_attempt = runtime.restart_attempt.saturating_add(1);
                runtime.next_restart = Some(Instant::now() + self.inner.budget.restart_backoff);
                ExtensionManagedRuntimeState::Backoff
            } else {
                ExtensionManagedRuntimeState::Ready
            };
        }
        state.recent.insert(
            exhausted.provenance.extension.clone(),
            RecentStatus {
                provenance: exhausted.provenance.clone(),
                state: if automatic {
                    ExtensionManagedRuntimeState::Backoff
                } else {
                    ExtensionManagedRuntimeState::ResourceExhausted
                },
                resource_exhausted: Some(exhausted),
                failure: None,
            },
        );
    }

    async fn acquire_startup(
        &self,
        provenance: &ExtensionRuntimeProvenance,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ExtensionRuntimeManagerError> {
        match tokio::time::timeout(
            self.inner.budget.startup_timeout,
            Arc::clone(&self.inner.startup_slots).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(ExtensionRuntimeManagerError::ManagerClosed),
            Err(_) => Err(ExtensionResourceExhausted {
                resource: ExtensionRuntimeResource::StartupConcurrency,
                limit: self.inner.budget.max_concurrent_startups as u64,
                requested: 1,
                in_use: self
                    .inner
                    .budget
                    .max_concurrent_startups
                    .saturating_sub(self.inner.startup_slots.available_permits())
                    as u64,
                provenance: provenance.clone(),
            }
            .into()),
        }
    }
}

fn classify_process_failure(error: &ProcessRuntimeError) -> ExtensionRuntimeFailure {
    match error {
        ProcessRuntimeError::Disabled(_) | ProcessRuntimeError::Untrusted(_) => {
            ExtensionRuntimeFailure::NotEligible
        }
        ProcessRuntimeError::Timeout { .. } => ExtensionRuntimeFailure::StartupTimeout,
        ProcessRuntimeError::Protocol(_) | ProcessRuntimeError::UnsupportedApiVersion { .. } => {
            ExtensionRuntimeFailure::Protocol
        }
        _ => ExtensionRuntimeFailure::Launch,
    }
}

struct UsageReservation {
    manager: Weak<ManagerInner>,
    usage: ExtensionRuntimeUsage,
    armed: bool,
}

impl Drop for UsageReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let mut state = lock(&manager.state);
        state.usage.processes = state.usage.processes.saturating_sub(self.usage.processes);
        state.usage.file_descriptors = state
            .usage
            .file_descriptors
            .saturating_sub(self.usage.file_descriptors);
        state.usage.buffered_bytes = state
            .usage
            .buffered_bytes
            .saturating_sub(self.usage.buffered_bytes);
    }
}

struct StartReservation {
    manager: Weak<ManagerInner>,
    key: RuntimeKey,
    usage: ExtensionRuntimeUsage,
    notify: Arc<Notify>,
    armed: bool,
}

impl StartReservation {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let mut state = lock(&manager.state);
        state.starting.remove(&self.key);
        state.usage.processes = state.usage.processes.saturating_sub(self.usage.processes);
        state.usage.file_descriptors = state
            .usage
            .file_descriptors
            .saturating_sub(self.usage.file_descriptors);
        state.usage.buffered_bytes = state
            .usage
            .buffered_bytes
            .saturating_sub(self.usage.buffered_bytes);
        self.notify.notify_waiters();
    }
}

/// Session-scoped attachment to a durable runtime manager.
#[derive(Clone)]
pub struct ExtensionSessionBinding {
    manager: ExtensionRuntimeManager,
    id: u64,
    session_digest: String,
    active: Arc<StdMutex<BTreeSet<RuntimeKey>>>,
    released: Arc<AtomicBool>,
    owners: Arc<()>,
}

impl ExtensionSessionBinding {
    /// Returns the path-free session-binding fingerprint.
    pub fn session_fingerprint(&self) -> &str {
        &self.session_digest
    }

    /// Explicitly activates one static catalog entry.
    pub async fn activate(
        &self,
        extension: &str,
        mut config: ExtensionRuntimeConfig,
    ) -> Result<ExtensionRuntimeLease, ExtensionRuntimeManagerError> {
        if self.released.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeManagerError::BindingClosed);
        }
        if self.manager.inner.shutdown.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeManagerError::ManagerClosed);
        }
        let entry = read(&self.manager.inner.catalog)
            .get(extension)
            .cloned()
            .ok_or(ExtensionRuntimeManagerError::UnknownExtension)?;
        let provenance = self.manager.provenance(&entry);
        let key = self.manager.runtime_key(&entry, self);
        if !ExtensionRuntimeManager::entry_is_eligible(&entry) {
            self.manager.record_recent(
                provenance,
                ExtensionManagedRuntimeState::Inactive,
                None,
                Some(ExtensionRuntimeFailure::NotEligible),
            );
            return Err(ExtensionRuntimeManagerError::NotEligible);
        }
        if entry.sharing() == ExtensionRuntimeSharing::Workspace {
            if entry.descriptor.manifest.api_version == EXTENSION_API_VERSION_0_1 {
                return Err(ExtensionRuntimeManagerError::SharedApiUnsupported);
            }
            if !entry.source_verified {
                self.manager
                    .stop_key(&key, Some(ExtensionManagedRuntimeState::StaleSource))
                    .await;
                self.manager.record_recent(
                    provenance,
                    ExtensionManagedRuntimeState::StaleSource,
                    None,
                    Some(ExtensionRuntimeFailure::StaleSource),
                );
                return Err(ExtensionRuntimeManagerError::UnverifiedSharedSource);
            }
            if config.agent_sessions
                || config.session_lifecycle.is_some()
                || config.approvals
                || config.secret_broker.is_some()
            {
                return Err(ExtensionRuntimeManagerError::SharedServiceUnsupported);
            }
            // Shared process initialization must not cache the first binding's
            // session/model/skill snapshot. Every operation still carries its
            // owner-scoped execution context.
            config.host_state = Default::default();
        }
        let configured_workspace = CanonicalWorkspace::new(&config.workspace)
            .map_err(|_| ExtensionRuntimeManagerError::WorkspaceMismatch)?;
        if configured_workspace != *self.manager.inner.domain.workspace() {
            return Err(ExtensionRuntimeManagerError::WorkspaceMismatch);
        }
        config.workspace = self.manager.inner.domain.workspace().path().to_owned();
        config.supervise = false;

        let (current_digest, source_verified) = entry
            .current_digest()
            .map_err(|_| ExtensionRuntimeManagerError::StaleSource)?;
        if current_digest != entry.content_digest
            || (entry.sharing() == ExtensionRuntimeSharing::Workspace && !source_verified)
        {
            self.manager
                .stop_key(&key, Some(ExtensionManagedRuntimeState::StaleSource))
                .await;
            self.manager.record_recent(
                provenance,
                ExtensionManagedRuntimeState::StaleSource,
                None,
                Some(ExtensionRuntimeFailure::StaleSource),
            );
            return Err(ExtensionRuntimeManagerError::StaleSource);
        }

        loop {
            let (wait, reservation) = {
                let mut state = lock(&self.manager.inner.state);
                if self.manager.inner.shutdown.load(Ordering::Acquire) {
                    return Err(ExtensionRuntimeManagerError::ManagerClosed);
                }
                if let Some(runtime) = state.active.get_mut(&key) {
                    runtime.bindings.insert(self.id);
                    lock(&self.active).insert(key.clone());
                    return Ok(ExtensionRuntimeLease {
                        process: runtime.process.clone(),
                        provenance: runtime.provenance.clone(),
                        shared: runtime.sharing == ExtensionRuntimeSharing::Workspace,
                        one_shot: runtime.lifecycle == ExtensionLifecycleProfile::OneShot,
                    });
                }
                if let Some(wait) = state.starting.get(&key) {
                    (Some(Arc::clone(&wait.notify)), None)
                } else {
                    let usage = ExtensionRuntimeManager::estimated_usage(&config);
                    match self.manager.reserve(&mut state, usage, &provenance) {
                        Ok(()) => {
                            let notify = Arc::new(Notify::new());
                            state.starting.insert(
                                key.clone(),
                                StartingRuntime {
                                    notify: Arc::clone(&notify),
                                    provenance: provenance.clone(),
                                    usage,
                                },
                            );
                            (
                                None,
                                Some(StartReservation {
                                    manager: Arc::downgrade(&self.manager.inner),
                                    key: key.clone(),
                                    usage,
                                    notify,
                                    armed: true,
                                }),
                            )
                        }
                        Err(error) => {
                            if let ExtensionRuntimeManagerError::ResourceExhausted(exhausted) =
                                &error
                            {
                                state.recent.insert(
                                    provenance.extension.clone(),
                                    RecentStatus {
                                        provenance: provenance.clone(),
                                        state: ExtensionManagedRuntimeState::ResourceExhausted,
                                        resource_exhausted: Some(exhausted.clone()),
                                        failure: None,
                                    },
                                );
                            }
                            return Err(error);
                        }
                    }
                }
            };
            if let Some(reservation) = reservation {
                return self
                    .start_new(entry, provenance, key, config, reservation)
                    .await;
            }
            let wait = wait.expect("a non-starting activation waits for its owner");
            wait.notified().await;
            if self.released.load(Ordering::Acquire) {
                return Err(ExtensionRuntimeManagerError::BindingClosed);
            }
        }
    }

    async fn start_new(
        &self,
        entry: ExtensionRuntimeCatalogEntry,
        provenance: ExtensionRuntimeProvenance,
        key: RuntimeKey,
        config: ExtensionRuntimeConfig,
        mut reservation: StartReservation,
    ) -> Result<ExtensionRuntimeLease, ExtensionRuntimeManagerError> {
        self.manager.ensure_monitor();
        let permit = match self.manager.acquire_startup(&provenance).await {
            Ok(permit) => permit,
            Err(error) => {
                if let ExtensionRuntimeManagerError::ResourceExhausted(exhausted) = &error {
                    self.manager.record_recent(
                        provenance.clone(),
                        ExtensionManagedRuntimeState::ResourceExhausted,
                        Some(exhausted.clone()),
                        None,
                    );
                }
                return Err(error);
            }
        };
        let started = tokio::time::timeout(
            self.manager.inner.budget.startup_timeout,
            ExtensionProcess::start(entry.descriptor.clone(), config),
        )
        .await;
        drop(permit);
        let process = match started {
            Ok(Ok(process)) if !self.manager.inner.shutdown.load(Ordering::Acquire) => process,
            Ok(Ok(process)) => {
                drop(reservation);
                let _ = process.shutdown().await;
                return Err(ExtensionRuntimeManagerError::ManagerClosed);
            }
            Ok(Err(error)) => {
                let failure = classify_process_failure(&error);
                self.manager.record_recent(
                    provenance,
                    ExtensionManagedRuntimeState::Parked,
                    None,
                    Some(failure),
                );
                return Err(ExtensionRuntimeManagerError::Failed { failure });
            }
            Err(_) => {
                let exhausted = ExtensionResourceExhausted {
                    resource: ExtensionRuntimeResource::StartupTime,
                    limit: self.manager.inner.budget.startup_timeout.as_millis() as u64,
                    requested: self.manager.inner.budget.startup_timeout.as_millis() as u64,
                    in_use: 0,
                    provenance: provenance.clone(),
                };
                self.manager.record_recent(
                    provenance,
                    ExtensionManagedRuntimeState::ResourceExhausted,
                    Some(exhausted.clone()),
                    None,
                );
                return Err(exhausted.into());
            }
        };
        if let Err(error) = self.manager.validate_current_entry(&key, &entry) {
            let _ = process.shutdown().await;
            self.manager
                .record_entry_validation_failure(&provenance, &error);
            return Err(error);
        }
        // The start reservation owns the exact caller configuration charge,
        // including queue and pending-request bounds.
        let usage = reservation.usage;
        let lifecycle = entry.lifecycle();
        let sharing = entry.sharing();
        let one_shot = lifecycle == ExtensionLifecycleProfile::OneShot;
        // Hold the catalog read lock through the state transition. Catalog
        // replacement takes the same catalog-then-state order, so it cannot
        // select a new entry between post-start validation and this commit.
        let committed = {
            let catalog = read(&self.manager.inner.catalog);
            match ExtensionRuntimeManager::validate_catalog_identity(
                &key,
                &entry,
                catalog.get(&entry.descriptor.manifest.name),
            ) {
                Err(error) => Err(error),
                Ok(()) => {
                    let mut state = lock(&self.manager.inner.state);
                    if self.manager.inner.shutdown.load(Ordering::Acquire) {
                        Err(ExtensionRuntimeManagerError::ManagerClosed)
                    } else {
                        state.starting.remove(&key);
                        state.recent.remove(&provenance.extension);
                        state.active.insert(
                            key.clone(),
                            ManagedRuntime {
                                descriptor: entry.descriptor,
                                provenance: provenance.clone(),
                                process: process.clone(),
                                usage,
                                bindings: BTreeSet::from([self.id]),
                                lifecycle,
                                sharing,
                                state: ExtensionManagedRuntimeState::Ready,
                                reloads: VecDeque::new(),
                                restarts: VecDeque::new(),
                                restart_attempt: 0,
                                next_restart: None,
                                gate: Arc::new(Mutex::new(())),
                            },
                        );
                        Ok(())
                    }
                }
            }
        };
        if let Err(error) = committed {
            let _ = process.shutdown().await;
            self.manager
                .record_entry_validation_failure(&provenance, &error);
            return Err(error);
        }
        lock(&self.active).insert(key.clone());
        reservation.disarm();
        reservation.notify.notify_waiters();
        if self.manager.inner.shutdown.load(Ordering::Acquire) {
            lock(&self.active).remove(&key);
            return Err(ExtensionRuntimeManagerError::ManagerClosed);
        }
        if self.released.load(Ordering::Acquire) {
            let keys = {
                let mut active = lock(&self.active);
                std::mem::take(&mut *active)
            };
            self.manager.detach_binding(self.id, keys).await;
            return Err(ExtensionRuntimeManagerError::BindingClosed);
        }
        Ok(ExtensionRuntimeLease {
            process,
            provenance,
            shared: false,
            one_shot,
        })
    }

    /// Activates only eager lifecycle profiles from an explicit eligible-name set.
    ///
    /// The caller retains policy ownership (safe mode, process gate, and
    /// enable/trust diagnostics); the manager retains resource accounting.
    pub async fn activate_eager<F>(
        &self,
        eligible_names: impl IntoIterator<Item = String>,
        mut config_for: F,
    ) -> Vec<ExtensionRuntimeActivation>
    where
        F: FnMut(&ExtensionRuntimeCatalogEntry) -> ExtensionRuntimeConfig,
    {
        let eligible = eligible_names.into_iter().collect::<BTreeSet<_>>();
        let entries = self
            .manager
            .catalog()
            .entries()
            .filter(|entry| {
                eligible.contains(&entry.descriptor.manifest.name)
                    && matches!(
                        entry.lifecycle(),
                        ExtensionLifecycleProfile::LegacyResident
                            | ExtensionLifecycleProfile::Session
                            | ExtensionLifecycleProfile::WorkspaceService
                            | ExtensionLifecycleProfile::Always
                            | ExtensionLifecycleProfile::PiAggregate
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        let requests = entries
            .into_iter()
            .map(|entry| {
                let name = entry.descriptor.manifest.name.clone();
                let provenance = self.manager.provenance(&entry);
                let config = config_for(&entry);
                let binding = self.clone();
                async move {
                    match binding.activate(&name, config).await {
                        Ok(lease) => ExtensionRuntimeActivation {
                            extension: name,
                            provenance: Some(lease.provenance.clone()),
                            process: Some(lease.process),
                            shared: lease.shared,
                            outcome: ExtensionRuntimeActivationOutcome::Ready,
                        },
                        Err(error) => ExtensionRuntimeActivation {
                            extension: name,
                            provenance: Some(provenance),
                            process: None,
                            shared: false,
                            outcome: activation_outcome(error),
                        },
                    }
                }
            })
            .collect::<Vec<_>>();
        join_all(requests).await
    }

    /// Returns active process handles attached to this binding in deterministic order.
    pub fn processes(&self) -> Vec<ExtensionProcess> {
        let keys = lock(&self.active).clone();
        let state = lock(&self.manager.inner.state);
        keys.into_iter()
            .filter_map(|key| {
                state
                    .active
                    .get(&key)
                    .map(|runtime| runtime.process.clone())
            })
            .collect()
    }

    /// Stops one-shot processes owned by this binding after their operation settles.
    pub async fn settle_one_shots(&self) {
        let keys = lock(&self.active).clone();
        self.manager.settle_one_shots(self.id, keys).await;
        let active = lock(&self.manager.inner.state)
            .active
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        lock(&self.active).retain(|key| active.contains(key));
    }

    /// Releases the session binding without shutting down workspace-shared or
    /// always-owned runtimes. Call [`ExtensionRuntimeManager::shutdown`] when
    /// the host itself is ending.
    pub async fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let keys = std::mem::take(&mut *lock(&self.active));
        self.manager.detach_binding(self.id, keys).await;
    }
}

impl Drop for ExtensionSessionBinding {
    fn drop(&mut self) {
        // `activate_eager` and callers may clone a binding for concurrent work.
        // A temporary clone cannot tear down the attachment owned by the
        // original binding.
        if Arc::strong_count(&self.owners) != 1 || self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let keys = std::mem::take(&mut *lock(&self.active));
        if keys.is_empty() {
            return;
        }
        if let Ok(handle) = Handle::try_current() {
            let manager = self.manager.clone();
            let id = self.id;
            handle.spawn(async move { manager.detach_binding(id, keys).await });
        }
    }
}

fn activation_outcome(error: ExtensionRuntimeManagerError) -> ExtensionRuntimeActivationOutcome {
    match error {
        ExtensionRuntimeManagerError::NotEligible => ExtensionRuntimeActivationOutcome::Inactive,
        ExtensionRuntimeManagerError::StaleSource
        | ExtensionRuntimeManagerError::UnverifiedSharedSource => {
            ExtensionRuntimeActivationOutcome::StaleSource
        }
        ExtensionRuntimeManagerError::ResourceExhausted(exhausted) => {
            ExtensionRuntimeActivationOutcome::ResourceExhausted(exhausted)
        }
        ExtensionRuntimeManagerError::Failed { failure } => {
            ExtensionRuntimeActivationOutcome::Failed(failure)
        }
        ExtensionRuntimeManagerError::ManagerClosed => {
            ExtensionRuntimeActivationOutcome::Failed(ExtensionRuntimeFailure::ManagerClosed)
        }
        _ => ExtensionRuntimeActivationOutcome::Failed(ExtensionRuntimeFailure::Launch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;

    use crate::extension_process::{
        ExtensionActivation, ExtensionEntrypoint, ExtensionManifest, ExtensionSource,
        ManifestContributions,
    };

    #[cfg(unix)]
    fn write_script(path: &Path, source: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    fn descriptor(
        root: &Path,
        name: &str,
        lifecycle: ExtensionLifecycleProfile,
    ) -> DiscoveredExtension {
        let directory = root.join(name);
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("runner.sh");
        write_script(
            &script,
            r#"#!/bin/sh
printf started >> "$YGG_WORKSPACE/starts"
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.2","tools":[],"commands":[],"protocol":{"version":"0.2","features":["request_cancellation","content_parts"],"limits":{"max_concurrent_requests":1}}}}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"shutdown"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'; exit 0 ;;
  esac
done
"#,
        );
        let manifest_path = directory.join("extension.toml");
        let sharing = match lifecycle {
            ExtensionLifecycleProfile::WorkspaceService
            | ExtensionLifecycleProfile::Always
            | ExtensionLifecycleProfile::PiAggregate => ExtensionRuntimeSharing::Workspace,
            _ => ExtensionRuntimeSharing::Isolated,
        };
        let manifest = ExtensionManifest {
            name: name.into(),
            version: "0.1.0".into(),
            api_version: "0.2".into(),
            requires_ygg: None,
            description: None,
            entrypoint: ExtensionEntrypoint {
                command: "runner.sh".into(),
                args: Vec::new(),
                env: Default::default(),
            },
            capabilities: Default::default(),
            contributes: ManifestContributions::default(),
            runtime: crate::extension_process::ExtensionRuntimeSettings { lifecycle, sharing },
        };
        fs::write(&manifest_path, toml::to_string(&manifest).unwrap()).unwrap();
        DiscoveredExtension {
            manifest,
            manifest_path,
            source: ExtensionSource::Explicit,
            activation: ExtensionActivation {
                enabled: true,
                trust: ExtensionTrust::Trusted,
            },
        }
    }

    #[test]
    fn dropping_a_temporary_binding_does_not_release_its_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let manager = ExtensionRuntimeManager::new(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
        );
        let binding = manager.bind_session("session-a").unwrap();
        drop(binding.clone());
        assert!(!binding.released.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[test]
    fn static_lazy_catalog_never_starts_eligible_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let descriptors = (0..100)
            .map(|index| {
                descriptor(
                    temporary.path(),
                    &format!("lazy-{index}"),
                    ExtensionLifecycleProfile::LazyResident,
                )
            })
            .collect::<Vec<_>>();
        let catalog = ExtensionRuntimeCatalog::from_descriptors(descriptors);
        let domain = ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap();
        let manager = ExtensionRuntimeManager::new(domain);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(manager.replace_catalog(catalog));
        assert_eq!(manager.statuses().len(), 100);
        assert!(manager
            .statuses()
            .iter()
            .all(|status| status.state == ExtensionManagedRuntimeState::Eligible));
        assert_eq!(manager.usage(), ExtensionRuntimeUsage::default());
        assert!(!temporary.path().join("starts").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_wakes_startup_waiters_without_launching_a_child() {
        let temporary = tempfile::tempdir().unwrap();
        let descriptor = descriptor(
            temporary.path(),
            "lazy-runtime",
            ExtensionLifecycleProfile::LazyResident,
        );
        let manager = ExtensionRuntimeManager::with_budget(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
            ExtensionRuntimeBudget {
                max_processes: 1,
                max_file_descriptors: ESTIMATED_PROCESS_FDS,
                max_buffered_bytes: usize::MAX,
                max_concurrent_startups: 1,
                startup_timeout: Duration::from_secs(5),
                max_reloads_per_window: 1,
                max_restarts_per_window: 1,
                restart_window: Duration::from_secs(1),
                restart_backoff: Duration::from_millis(1),
            },
        )
        .unwrap();
        manager
            .replace_catalog(ExtensionRuntimeCatalog::from_descriptors([descriptor]))
            .await;
        let permit = Arc::clone(&manager.inner.startup_slots)
            .acquire_owned()
            .await
            .unwrap();
        let binding = manager.bind_session("session-a").unwrap();
        let workspace = temporary.path().to_owned();
        let pending = tokio::spawn({
            let binding = binding.clone();
            async move {
                binding
                    .activate("lazy-runtime", ExtensionRuntimeConfig::new(workspace))
                    .await
            }
        });
        for _ in 0..16 {
            if manager
                .statuses()
                .iter()
                .any(|status| status.state == ExtensionManagedRuntimeState::Starting)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(manager
            .statuses()
            .iter()
            .any(|status| status.state == ExtensionManagedRuntimeState::Starting));

        manager.shutdown().await;
        let result = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("shutdown must wake an activation waiting for a startup slot")
            .unwrap();
        assert!(matches!(
            result,
            Err(ExtensionRuntimeManagerError::ManagerClosed)
        ));
        drop(permit);
        assert!(!temporary.path().join("starts").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compatible_binding_reuses_content_bound_workspace_service() {
        let temporary = tempfile::tempdir().unwrap();
        let descriptor = descriptor(
            temporary.path(),
            "workspace-service",
            ExtensionLifecycleProfile::WorkspaceService,
        );
        let catalog = ExtensionRuntimeCatalog::from_descriptors([descriptor]);
        let manager = ExtensionRuntimeManager::new(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
        );
        manager.replace_catalog(catalog).await;
        let first = manager.bind_session("session-a").unwrap();
        let first_process = first
            .activate(
                "workspace-service",
                ExtensionRuntimeConfig::new(temporary.path()),
            )
            .await
            .unwrap()
            .process()
            .extension_instance_id()
            .to_owned();
        first.release().await;
        let second = manager.bind_session("session-b").unwrap();
        let second_process = second
            .activate(
                "workspace-service",
                ExtensionRuntimeConfig::new(temporary.path()),
            )
            .await
            .unwrap()
            .process()
            .extension_instance_id()
            .to_owned();
        assert_eq!(first_process, second_process);
        assert_eq!(
            fs::read_to_string(temporary.path().join("starts")).unwrap(),
            "started"
        );
        second.release().await;
        manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shared_runtime_rejects_active_session_lifecycle_service() {
        let temporary = tempfile::tempdir().unwrap();
        let descriptor = descriptor(
            temporary.path(),
            "workspace-service",
            ExtensionLifecycleProfile::WorkspaceService,
        );
        let catalog = ExtensionRuntimeCatalog::from_descriptors([descriptor]);
        let manager = ExtensionRuntimeManager::new(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
        );
        manager.replace_catalog(catalog).await;
        let binding = manager.bind_session("session-a").unwrap();
        let (service, _receiver) =
            crate::extension_process::ExtensionSessionLifecycleService::channel(1).unwrap();
        let mut config = ExtensionRuntimeConfig::new(temporary.path());
        config.session_lifecycle = Some(service);

        assert!(matches!(
            binding.activate("workspace-service", config).await,
            Err(ExtensionRuntimeManagerError::SharedServiceUnsupported)
        ));
        assert!(!temporary.path().join("starts").exists());
        binding.release().await;
        manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_change_is_retired_fail_closed_and_releases_governance() {
        let temporary = tempfile::tempdir().unwrap();
        let descriptor = descriptor(
            temporary.path(),
            "workspace-service",
            ExtensionLifecycleProfile::WorkspaceService,
        );
        let script = descriptor.manifest_path.parent().unwrap().join("runner.sh");
        let catalog = ExtensionRuntimeCatalog::from_descriptors([descriptor]);
        let manager = ExtensionRuntimeManager::new(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
        );
        manager.replace_catalog(catalog).await;
        let binding = manager.bind_session("session-a").unwrap();
        binding
            .activate(
                "workspace-service",
                ExtensionRuntimeConfig::new(temporary.path()),
            )
            .await
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(script)
            .unwrap()
            .write_all(b"\n# changed\n")
            .unwrap();
        let result = manager.reload("workspace-service").await;
        assert!(matches!(
            result.as_slice(),
            [Err(ExtensionRuntimeManagerError::StaleSource)]
        ));
        assert_eq!(manager.usage(), ExtensionRuntimeUsage::default());
        assert!(manager.statuses().iter().any(|status| {
            status.state == ExtensionManagedRuntimeState::StaleSource
                && status.failure == Some(ExtensionRuntimeFailure::StaleSource)
        }));
        binding.release().await;
        manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn catalog_replacement_retires_old_content_without_overwriting_new_status() {
        let temporary = tempfile::tempdir().unwrap();
        let descriptor = descriptor(
            temporary.path(),
            "workspace-service",
            ExtensionLifecycleProfile::WorkspaceService,
        );
        let replacement = descriptor.clone();
        let script = descriptor.manifest_path.parent().unwrap().join("runner.sh");
        let manager = ExtensionRuntimeManager::new(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
        );
        manager
            .replace_catalog(ExtensionRuntimeCatalog::from_descriptors([descriptor]))
            .await;
        let first = manager.bind_session("session-a").unwrap();
        first
            .activate(
                "workspace-service",
                ExtensionRuntimeConfig::new(temporary.path()),
            )
            .await
            .unwrap();

        fs::OpenOptions::new()
            .append(true)
            .open(script)
            .unwrap()
            .write_all(b"\n# replacement\n")
            .unwrap();
        manager
            .replace_catalog(ExtensionRuntimeCatalog::from_descriptors([replacement]))
            .await;

        assert_eq!(manager.usage(), ExtensionRuntimeUsage::default());
        assert!(matches!(
            manager.statuses().as_slice(),
            [ExtensionRuntimeStatus {
                state: ExtensionManagedRuntimeState::Eligible,
                resource_exhausted: None,
                failure: None,
                ..
            }]
        ));

        let second = manager.bind_session("session-b").unwrap();
        second
            .activate(
                "workspace-service",
                ExtensionRuntimeConfig::new(temporary.path()),
            )
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(temporary.path().join("starts")).unwrap(),
            "startedstarted"
        );
        first.release().await;
        second.release().await;
        manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resource_exhaustion_is_typed_and_does_not_erase_other_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let first = descriptor(
            temporary.path(),
            "first",
            ExtensionLifecycleProfile::LazyResident,
        );
        let second = descriptor(
            temporary.path(),
            "second",
            ExtensionLifecycleProfile::LazyResident,
        );
        let manager = ExtensionRuntimeManager::with_budget(
            ExtensionRuntimeDomain::ordinary(temporary.path()).unwrap(),
            ExtensionRuntimeBudget {
                max_processes: 1,
                max_file_descriptors: 8,
                max_buffered_bytes: usize::MAX,
                max_concurrent_startups: 1,
                startup_timeout: Duration::from_secs(2),
                max_reloads_per_window: 2,
                max_restarts_per_window: 2,
                restart_window: Duration::from_secs(2),
                restart_backoff: Duration::from_millis(1),
            },
        )
        .unwrap();
        manager
            .replace_catalog(ExtensionRuntimeCatalog::from_descriptors([first, second]))
            .await;
        let binding = manager.bind_session("session-a").unwrap();
        binding
            .activate("first", ExtensionRuntimeConfig::new(temporary.path()))
            .await
            .unwrap();
        let error = match binding
            .activate("second", ExtensionRuntimeConfig::new(temporary.path()))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("second process should exceed the process budget"),
        };
        assert!(matches!(
            error,
            ExtensionRuntimeManagerError::ResourceExhausted(ExtensionResourceExhausted {
                resource: ExtensionRuntimeResource::Processes,
                ..
            })
        ));
        assert_eq!(manager.statuses().len(), 2);
        binding.release().await;
        manager.shutdown().await;
    }
}
