//! Executable extensions discovered from disk and connected over JSON lines.
//!
//! Native [`Extension`](crate::Extension)s remain the lowest-overhead option for
//! built-ins. This module adds a language-neutral product boundary: a trusted,
//! explicitly enabled manifest launches one child process and exchanges typed
//! JSON-RPC 2.0 requests, responses, and notifications over stdin/stdout.
//! Capability declarations are consent metadata, not an operating-system
//! sandbox; executable extensions run with the current user's privileges.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, oneshot, Mutex, Notify, Semaphore};
use ygg_ai::ToolDef;

use crate::extension::{Extension, ExtensionHost, ToolCallHook};
use crate::tool::{CancellationToken, ReplaySafety, Tool, ToolContext, ToolError, ToolOutput};

/// The executable-extension API implemented by this Ygg release.
pub const EXTENSION_API_VERSION: &str = "0.1";

/// The manifest filename inside every extension directory.
pub const EXTENSION_MANIFEST_FILENAME: &str = "extension.toml";

/// Default maximum manifest size (64 KiB).
pub const DEFAULT_EXTENSION_MANIFEST_BYTES: u64 = 64 * 1024;

/// Default maximum size of one JSON protocol message (1 MiB).
pub const DEFAULT_EXTENSION_MESSAGE_BYTES: usize = 1024 * 1024;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const CONFIRMATION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_PENDING_REQUESTS: usize = 64;
const EXTENSION_EVENT_CAPACITY: usize = 128;
// Retain every answered confirmation that can still be buffered for another
// event subscriber. Once this many newer confirmations have been answered, an
// older event has necessarily fallen outside the broadcast channel's window.
const ANSWERED_CONFIRMATION_CAPACITY: usize = EXTENSION_EVENT_CAPACITY;
const MAX_CONFIRMATION_REQUEST_ID_BYTES: usize = 256;

static HOST_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static HOST_SHUTDOWN_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Marks the host as shutting down and cancels ordinary extension RPC work.
///
/// The flag is level-triggered so calls which start after the signal cannot
/// miss it. Protocol shutdown requests use a separate path and remain allowed.
pub fn begin_host_shutdown() {
    HOST_SHUTDOWN_REQUESTED.store(true, Ordering::Release);
    HOST_SHUTDOWN_NOTIFY.notify_waiters();
    #[cfg(unix)]
    if let Some(reaper) = LazyLock::force(&DETACHED_EXEC_REAPER) {
        reaper.unpark();
    }
}

async fn host_shutdown_requested() {
    loop {
        let notified = HOST_SHUTDOWN_NOTIFY.notified();
        if HOST_SHUTDOWN_REQUESTED.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisteredProcessKind {
    Exec,
    Extension,
}

#[cfg(unix)]
struct DetachedExecSupervision {
    deadline: Instant,
    cancellation: CancellationToken,
}

#[cfg(unix)]
struct RegisteredProcessGroup {
    kind: RegisteredProcessKind,
    registration_id: u64,
    detached_exec: Option<DetachedExecSupervision>,
}

#[cfg(unix)]
static REGISTERED_PROCESS_GROUPS: LazyLock<StdMutex<BTreeMap<i32, RegisteredProcessGroup>>> =
    LazyLock::new(|| StdMutex::new(BTreeMap::new()));
static NEXT_PROCESS_GROUP_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
const DETACHED_EXEC_REAPER_POLL: Duration = Duration::from_millis(25);

/// One process-wide reaper owns successful `exec` groups whose direct leader
/// has exited while background descendants remain. A standard thread keeps the
/// cleanup boundary alive during async-runtime teardown without spawning one
/// task per command.
#[cfg(unix)]
static DETACHED_EXEC_REAPER: LazyLock<Option<std::thread::Thread>> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("ygg-exec-group-reaper".into())
        .spawn(detached_exec_reaper_loop)
        .ok()
        .map(|handle| handle.thread().clone())
});

fn valid_process_group_id(process_group_id: u64) -> Option<i32> {
    i32::try_from(process_group_id)
        .ok()
        .filter(|process_group_id| *process_group_id > 0)
}

fn register_process_group(process_group_id: u64, kind: RegisteredProcessKind) -> u64 {
    let registration_id = NEXT_PROCESS_GROUP_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed);
    #[cfg(unix)]
    if let Some(process_group_id) = valid_process_group_id(process_group_id) {
        lock_std_mutex(&REGISTERED_PROCESS_GROUPS).insert(
            process_group_id,
            RegisteredProcessGroup {
                kind,
                registration_id,
                detached_exec: None,
            },
        );
    }
    #[cfg(not(unix))]
    let _ = (process_group_id, kind);
    registration_id
}

fn unregister_process_group(process_group_id: u64, registration_id: u64) -> bool {
    #[cfg(unix)]
    if let Some(process_group_id) = valid_process_group_id(process_group_id) {
        let mut registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
        if registered
            .get(&process_group_id)
            .is_some_and(|entry| entry.registration_id == registration_id)
        {
            registered.remove(&process_group_id);
            return true;
        }
    }
    #[cfg(not(unix))]
    let _ = process_group_id;
    false
}

/// RAII ownership for a child placed in its own process group.
///
/// Dropping an armed guard force-terminates the whole group. Call
/// [`ProcessGroupGuard::disarm`] only after the direct child has been waited
/// and all captured output pipes have closed.
pub struct ProcessGroupGuard {
    process_group_id: AtomicU64,
    registration_id: u64,
}

impl ProcessGroupGuard {
    /// Registers a shell or built-in `exec` child process group.
    pub fn exec(pid: Option<u32>) -> Self {
        Self::new(pid.map(u64::from).unwrap_or(0), RegisteredProcessKind::Exec)
    }

    fn extension(process_group_id: u64) -> Self {
        Self::new(process_group_id, RegisteredProcessKind::Extension)
    }

    fn new(process_group_id: u64, kind: RegisteredProcessKind) -> Self {
        let registration_id = register_process_group(process_group_id, kind);
        Self {
            process_group_id: AtomicU64::new(process_group_id),
            registration_id,
        }
    }

    /// Immediately force-terminates the owned process group.
    pub fn terminate_now(&self) {
        let process_group_id = self.process_group_id.swap(0, Ordering::AcqRel);
        unregister_process_group(process_group_id, self.registration_id);
        kill_process_group(process_group_id);
    }

    /// Releases the group after its child and output pipes have fully settled.
    pub fn disarm(&self) {
        let process_group_id = self.process_group_id.swap(0, Ordering::AcqRel);
        unregister_process_group(process_group_id, self.registration_id);
    }

    /// Transfers a successfully reaped direct `exec` child to the centralized
    /// descendant supervisor. The registry entry remains live until the group
    /// disappears naturally, the run is cancelled, the original execution
    /// deadline expires, or host shutdown begins.
    pub fn supervise_exec_descendants(self, lifetime: Duration, cancellation: CancellationToken) {
        #[cfg(unix)]
        {
            let process_group_id = self.process_group_id.load(Ordering::Acquire);
            let Some(process_group_id_i32) = valid_process_group_id(process_group_id) else {
                self.disarm();
                return;
            };
            if !process_group_is_alive(process_group_id_i32) {
                self.disarm();
                return;
            }
            if lifetime.is_zero()
                || cancellation.is_cancelled()
                || HOST_SHUTDOWN_REQUESTED.load(Ordering::Acquire)
            {
                self.terminate_now();
                return;
            }
            let now = Instant::now();
            let Some(deadline) = now.checked_add(lifetime) else {
                self.terminate_now();
                return;
            };
            let Some(reaper) = LazyLock::force(&DETACHED_EXEC_REAPER) else {
                self.terminate_now();
                return;
            };

            let transferred = {
                let mut registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
                let Some(entry) = registered.get_mut(&process_group_id_i32) else {
                    return;
                };
                if entry.registration_id != self.registration_id
                    || entry.kind != RegisteredProcessKind::Exec
                {
                    return;
                }
                entry.detached_exec = Some(DetachedExecSupervision {
                    deadline,
                    cancellation,
                });
                // Transfer ownership while holding the registry lock. The
                // reaper cannot observe the detached state before Drop becomes
                // harmless, avoiding a stale post-reap signal after PGID reuse.
                self.process_group_id.store(0, Ordering::Release);
                true
            };
            if transferred {
                reaper.unpark();
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (lifetime, cancellation);
            self.disarm();
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate_now();
    }
}

#[cfg(unix)]
fn registered_process_groups(kind: Option<RegisteredProcessKind>) -> Vec<i32> {
    lock_std_mutex(&REGISTERED_PROCESS_GROUPS)
        .iter()
        .filter_map(|(process_group_id, registered)| {
            kind.is_none_or(|kind| kind == registered.kind)
                .then_some(*process_group_id)
        })
        .collect()
}

#[cfg(unix)]
fn detached_exec_process_groups() -> Vec<(i32, u64, Instant, CancellationToken)> {
    lock_std_mutex(&REGISTERED_PROCESS_GROUPS)
        .iter()
        .filter_map(|(process_group_id, registered)| {
            registered.detached_exec.as_ref().map(|supervision| {
                (
                    *process_group_id,
                    registered.registration_id,
                    supervision.deadline,
                    supervision.cancellation.clone(),
                )
            })
        })
        .collect()
}

#[cfg(unix)]
fn detached_exec_reaper_loop() {
    loop {
        let supervised = detached_exec_process_groups();
        if supervised.is_empty() {
            std::thread::park();
            continue;
        }

        let now = Instant::now();
        let host_shutdown = HOST_SHUTDOWN_REQUESTED.load(Ordering::Acquire);
        let mut next_poll = DETACHED_EXEC_REAPER_POLL;
        for (process_group_id, registration_id, deadline, cancellation) in supervised {
            let alive = process_group_is_alive(process_group_id);
            let terminate = host_shutdown || cancellation.is_cancelled() || now >= deadline;
            if !alive || terminate {
                if unregister_process_group(process_group_id as u64, registration_id) && terminate {
                    kill_process_group(process_group_id as u64);
                }
                continue;
            }
            next_poll = next_poll.min(deadline.saturating_duration_since(now));
        }
        if next_poll.is_zero() {
            std::thread::yield_now();
        } else {
            std::thread::park_timeout(next_poll);
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) fn process_group_registered_for_test(process_group_id: i32) -> bool {
    lock_std_mutex(&REGISTERED_PROCESS_GROUPS).contains_key(&process_group_id)
}

#[cfg(unix)]
fn signal_process_groups(process_group_ids: &[i32], signal: i32) {
    for process_group_id in process_group_ids {
        // SAFETY: every ID is registered only after its child was spawned with
        // `process_group(0)`, so a negative target cannot reach Ygg's caller.
        unsafe {
            let _ = libc::kill(-*process_group_id, signal);
        }
    }
}

#[cfg(unix)]
fn process_group_is_alive(process_group_id: i32) -> bool {
    // Signal zero performs existence/permission checking without changing the
    // target. EPERM still means that the group exists.
    let result = unsafe { libc::kill(-process_group_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Gracefully terminates registered shell/`exec` groups, then force-kills and
/// waits for survivors, all within the supplied total timeout.
pub async fn terminate_exec_process_groups(timeout: Duration) {
    #[cfg(unix)]
    {
        let process_group_ids = registered_process_groups(Some(RegisteredProcessKind::Exec));
        if process_group_ids.is_empty() {
            return;
        }
        let started = Instant::now();
        let graceful_deadline = started + timeout / 2;
        let final_deadline = started + timeout;
        signal_process_groups(&process_group_ids, libc::SIGTERM);

        let mut survivors = process_group_ids;
        while Instant::now() < graceful_deadline {
            survivors.retain(|process_group_id| process_group_is_alive(*process_group_id));
            if survivors.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        signal_process_groups(&survivors, libc::SIGKILL);
        while Instant::now() < final_deadline {
            survivors.retain(|process_group_id| process_group_is_alive(*process_group_id));
            if survivors.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[cfg(not(unix))]
    let _ = timeout;
}

/// Force-kills every registered shell, `exec`, and extension process group.
///
/// This is the last-resort watchdog path after coordinated cleanup times out.
pub fn force_kill_registered_process_groups() {
    #[cfg(unix)]
    signal_process_groups(&registered_process_groups(None), libc::SIGKILL);
}

/// Parsed `extension.toml` metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    /// Stable lowercase extension identifier.
    pub name: String,
    /// Semantic extension version.
    pub version: String,
    /// Ygg extension API required by the extension.
    pub api_version: String,
    /// Optional human-readable summary.
    #[serde(default)]
    pub description: Option<String>,
    /// Process launch configuration.
    pub entrypoint: ExtensionEntrypoint,
    /// Privileges requested by the extension.
    #[serde(default)]
    pub capabilities: ExtensionCapabilities,
    /// Typed contribution points declared by the extension.
    #[serde(default)]
    pub contributes: ManifestContributions,
}

impl ExtensionManifest {
    /// Parses and validates a TOML manifest string.
    pub fn parse(source: &str) -> Result<Self, ExtensionRuntimeError> {
        let manifest: Self = toml::from_str(source)
            .map_err(|error| ExtensionRuntimeError::ManifestParse(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reads and validates a manifest with the default 64 KiB bound.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ExtensionRuntimeError> {
        Self::load_bounded(path, DEFAULT_EXTENSION_MANIFEST_BYTES)
    }

    /// Reads and validates a manifest without ever buffering more than
    /// `max_bytes + 1` bytes.
    pub fn load_bounded(
        path: impl AsRef<Path>,
        max_bytes: u64,
    ) -> Result<Self, ExtensionRuntimeError> {
        let path = path.as_ref();
        let metadata =
            std::fs::metadata(path).map_err(|error| ExtensionRuntimeError::ManifestIo {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        if metadata.len() > max_bytes {
            return Err(ExtensionRuntimeError::ManifestTooLarge {
                path: path.to_path_buf(),
                bytes: metadata.len(),
                limit: max_bytes,
            });
        }

        let file = File::open(path).map_err(|error| ExtensionRuntimeError::ManifestIo {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| ExtensionRuntimeError::ManifestIo {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(ExtensionRuntimeError::ManifestTooLarge {
                path: path.to_path_buf(),
                bytes: bytes.len() as u64,
                limit: max_bytes,
            });
        }
        let source = std::str::from_utf8(&bytes).map_err(|_| {
            ExtensionRuntimeError::InvalidManifest("manifest is not valid UTF-8".into())
        })?;
        Self::parse(source)
    }

    /// Validates identifiers, versions, launch data, and contribution lists.
    pub fn validate(&self) -> Result<(), ExtensionRuntimeError> {
        validate_identifier("extension name", &self.name, false)?;
        semver::Version::parse(&self.version).map_err(|error| {
            ExtensionRuntimeError::InvalidManifest(format!(
                "version `{}` is not semantic versioning: {error}",
                self.version
            ))
        })?;
        if self.api_version != EXTENSION_API_VERSION {
            return Err(ExtensionRuntimeError::UnsupportedApiVersion {
                extension: self.api_version.clone(),
                host: EXTENSION_API_VERSION.to_owned(),
            });
        }
        if self.entrypoint.command.trim().is_empty()
            || self.entrypoint.command.chars().any(char::is_control)
        {
            return Err(ExtensionRuntimeError::InvalidManifest(
                "entrypoint.command must be non-empty and contain no control characters".into(),
            ));
        }
        for argument in &self.entrypoint.args {
            if argument.contains('\0') {
                return Err(ExtensionRuntimeError::InvalidManifest(
                    "entrypoint arguments cannot contain NUL".into(),
                ));
            }
        }
        for (name, value) in &self.entrypoint.env {
            if !valid_environment_name(name) || value.contains('\0') {
                return Err(ExtensionRuntimeError::InvalidManifest(format!(
                    "invalid entrypoint environment variable `{name}`"
                )));
            }
        }
        validate_identifiers("tool", &self.contributes.tools, true)?;
        validate_identifiers("command", &self.contributes.commands, true)?;
        validate_identifiers("tool renderer", &self.contributes.tool_renderers, true)?;
        validate_unique("hook", &self.contributes.hooks)?;
        validate_unique("UI contribution", &self.contributes.ui)?;
        Ok(())
    }
}

/// Process launch configuration from an extension manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEntrypoint {
    /// Executable name or path. A bare name found beside the manifest wins
    /// over `PATH`, which makes self-contained extension folders convenient.
    pub command: String,
    /// Arguments passed directly without shell interpretation.
    #[serde(default)]
    pub args: Vec<String>,
    /// Additional environment variables for this child only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Privileges declared by an executable extension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionCapabilities {
    /// Filesystem scope requested by the extension.
    #[serde(default)]
    pub filesystem: ExtensionFilesystemAccess,
    /// Whether the extension intends to launch additional processes.
    #[serde(default)]
    pub process: bool,
    /// Whether the extension intends to access the network.
    #[serde(default)]
    pub network: bool,
}

/// Filesystem access declared by an extension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionFilesystemAccess {
    /// The extension declares no filesystem access.
    #[default]
    None,
    /// The extension needs files under the active workspace.
    Workspace,
    /// The extension asks for unrestricted user-level filesystem access.
    Unrestricted,
}

/// Contribution names declared in `extension.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestContributions {
    /// Model-callable tool names.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Slash-command names.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Agent lifecycle hooks.
    #[serde(default)]
    pub hooks: Vec<ExtensionHook>,
    /// Semantic terminal surfaces.
    #[serde(default)]
    pub ui: Vec<ExtensionUiSurface>,
    /// Whether the extension can contribute prompt context.
    #[serde(default)]
    pub context: bool,
    /// Tool names for which the extension supplies semantic render output.
    #[serde(default)]
    pub tool_renderers: Vec<String>,
    /// Whether the process may emit user-visible notifications.
    #[serde(default)]
    pub notifications: bool,
    /// Whether the process may request interactive confirmation.
    #[serde(default)]
    pub confirmations: bool,
}

/// Supported extension lifecycle hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionHook {
    /// Runs immediately before prompt composition.
    BeforePrompt,
    /// Runs after a complete assistant response.
    AfterResponse,
    /// Runs before a tool is dispatched.
    BeforeToolCall,
    /// Runs after a tool result is available.
    AfterToolCall,
}

/// Semantic terminal surfaces an extension may populate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionUiSurface {
    /// A compact status item.
    Status,
    /// The semantic header region.
    Header,
    /// The semantic footer region.
    Footer,
}

/// Where an extension manifest came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSource {
    /// `.ygg/extensions/` under the active workspace.
    Project,
    /// `~/.ygg/extensions/` under the user's home directory.
    Global,
    /// A directory supplied explicitly by the caller.
    Explicit,
}

/// One extension search root. Roots are consulted in caller-provided order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRoot {
    /// Directory whose direct children are extension directories.
    pub directory: PathBuf,
    /// Provenance attached to discovered manifests.
    pub source: ExtensionSource,
}

/// Returns the conventional project-first extension roots.
pub fn default_extension_roots(workspace: &Path, home: Option<&Path>) -> Vec<ExtensionRoot> {
    let mut roots = vec![ExtensionRoot {
        directory: workspace.join(".ygg/extensions"),
        source: ExtensionSource::Project,
    }];
    if let Some(home) = home {
        roots.push(ExtensionRoot {
            directory: home.join(".ygg/extensions"),
            source: ExtensionSource::Global,
        });
    }
    roots
}

/// A resolved manifest path ready for bounded loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionManifestInput {
    /// Exact `extension.toml` path.
    pub path: PathBuf,
    /// Discovery provenance.
    pub source: ExtensionSource,
}

/// Severity of a non-fatal catalog diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionDiagnosticLevel {
    /// The entry was loaded but deserves attention.
    Warning,
    /// The entry could not be loaded.
    Error,
}

/// A path-scoped extension discovery or loading diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionDiagnostic {
    /// Severity of the diagnostic.
    pub level: ExtensionDiagnosticLevel,
    /// Path involved, when known.
    pub path: PathBuf,
    /// Human-readable explanation.
    pub message: String,
}

/// Scans direct child directories for [`EXTENSION_MANIFEST_FILENAME`].
/// Missing roots are normal and produce no diagnostic.
pub fn discover_extension_manifests(
    roots: &[ExtensionRoot],
) -> (Vec<ExtensionManifestInput>, Vec<ExtensionDiagnostic>) {
    let mut manifests = Vec::new();
    let mut diagnostics = Vec::new();
    for root in roots {
        let entries = match std::fs::read_dir(&root.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(ExtensionDiagnostic {
                    level: ExtensionDiagnosticLevel::Error,
                    path: root.directory.clone(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let manifest = entry.path().join(EXTENSION_MANIFEST_FILENAME);
                    if manifest.is_file() {
                        paths.push(manifest);
                    }
                }
                Err(error) => diagnostics.push(ExtensionDiagnostic {
                    level: ExtensionDiagnosticLevel::Warning,
                    path: root.directory.clone(),
                    message: error.to_string(),
                }),
            }
        }
        paths.sort();
        manifests.extend(paths.into_iter().map(|path| ExtensionManifestInput {
            path,
            source: root.source,
        }));
    }
    (manifests, diagnostics)
}

/// Trust state required before an executable manifest may launch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTrust {
    /// Discovery alone never grants code-execution permission.
    #[default]
    Untrusted,
    /// The user explicitly trusted this extension identifier.
    Trusted,
}

/// Explicit activation state for one discovered extension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionActivation {
    /// Whether this extension is enabled.
    pub enabled: bool,
    /// Whether launching its executable is trusted.
    pub trust: ExtensionTrust,
}

/// Explicit enablement plus source-bound executable trust. Persistent
/// name-only grants intentionally apply only to the user's global extension
/// directory; project and explicit code must match an exact manifest path or
/// receive a one-invocation grant from the frontend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionPolicy {
    enabled: BTreeSet<String>,
    trusted_global: BTreeSet<String>,
    trusted_sources: BTreeSet<(String, PathBuf)>,
    trusted_for_invocation: BTreeSet<String>,
}

impl ExtensionPolicy {
    /// Explicitly enables an extension name without implicitly trusting it.
    pub fn enable(&mut self, name: impl Into<String>) {
        self.enabled.insert(name.into());
    }

    /// Persistently trusts a name from the user's global extension directory.
    /// This grant never transfers to a project or explicit manifest with the
    /// same name.
    pub fn trust(&mut self, name: impl Into<String>) {
        self.trusted_global.insert(name.into());
    }

    /// Persistently trusts one exact, normalized manifest path.
    pub fn trust_source(&mut self, name: impl Into<String>, manifest_path: impl Into<PathBuf>) {
        self.trusted_sources
            .insert((name.into(), manifest_path.into()));
    }

    /// Trusts whichever descriptor with this name was selected for the
    /// current process invocation. Frontends should expose this only through
    /// an explicit one-shot CLI/action boundary, never persistent config.
    pub fn trust_for_invocation(&mut self, name: impl Into<String>) {
        self.trusted_for_invocation.insert(name.into());
    }

    /// Removes an extension from the enabled set.
    pub fn disable(&mut self, name: &str) {
        self.enabled.remove(name);
    }

    /// Revokes an extension's executable trust grant.
    pub fn revoke_trust(&mut self, name: &str) {
        self.trusted_global.remove(name);
        self.trusted_for_invocation.remove(name);
        self.trusted_sources
            .retain(|(trusted_name, _)| trusted_name != name);
    }

    /// Returns the two independent decisions for one selected source.
    pub fn activation(
        &self,
        name: &str,
        manifest_path: &Path,
        source: ExtensionSource,
    ) -> ExtensionActivation {
        let source_bound = self
            .trusted_sources
            .contains(&(name.to_owned(), manifest_path.to_owned()));
        let trusted = self.trusted_for_invocation.contains(name)
            || source_bound
            || (source == ExtensionSource::Global && self.trusted_global.contains(name));
        ExtensionActivation {
            enabled: self.enabled.contains(name),
            trust: if trusted {
                ExtensionTrust::Trusted
            } else {
                ExtensionTrust::Untrusted
            },
        }
    }
}

/// A valid manifest plus its provenance and activation decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredExtension {
    /// Validated manifest.
    pub manifest: ExtensionManifest,
    /// Exact manifest file used.
    pub manifest_path: PathBuf,
    /// Resource provenance.
    pub source: ExtensionSource,
    /// Explicit enablement and trust state.
    pub activation: ExtensionActivation,
}

impl DiscoveredExtension {
    fn ensure_startable(&self) -> Result<(), ExtensionRuntimeError> {
        if !self.activation.enabled {
            return Err(ExtensionRuntimeError::Disabled(self.manifest.name.clone()));
        }
        if self.activation.trust != ExtensionTrust::Trusted {
            return Err(ExtensionRuntimeError::Untrusted(self.manifest.name.clone()));
        }
        Ok(())
    }
}

/// Loaded extension catalog. Invalid or shadowed entries are diagnostics,
/// allowing one bad tinkerer extension to leave the rest usable.
#[derive(Clone, Debug, Default)]
pub struct ExtensionCatalog {
    /// First valid manifest for each name, preserving input precedence.
    pub extensions: Vec<DiscoveredExtension>,
    /// Non-fatal load, validation, and duplicate diagnostics.
    pub diagnostics: Vec<ExtensionDiagnostic>,
}

impl ExtensionCatalog {
    /// Loads caller-resolved paths in order. The first manifest for a name
    /// wins, so a shared resource resolver can authoritatively set precedence.
    pub fn load_resolved(
        inputs: impl IntoIterator<Item = ExtensionManifestInput>,
        policy: &ExtensionPolicy,
        max_manifest_bytes: u64,
    ) -> Self {
        let mut catalog = Self::default();
        let mut names = BTreeMap::<String, PathBuf>::new();
        for input in inputs {
            match ExtensionManifest::load_bounded(&input.path, max_manifest_bytes) {
                Ok(manifest) => {
                    if let Some(first) = names.get(&manifest.name) {
                        catalog.diagnostics.push(ExtensionDiagnostic {
                            level: ExtensionDiagnosticLevel::Warning,
                            path: input.path,
                            message: format!(
                                "extension `{}` is shadowed by {}",
                                manifest.name,
                                first.display()
                            ),
                        });
                        continue;
                    }
                    names.insert(manifest.name.clone(), input.path.clone());
                    let activation = policy.activation(&manifest.name, &input.path, input.source);
                    catalog.extensions.push(DiscoveredExtension {
                        activation,
                        manifest,
                        manifest_path: input.path,
                        source: input.source,
                    });
                }
                Err(error) => catalog.diagnostics.push(ExtensionDiagnostic {
                    level: ExtensionDiagnosticLevel::Error,
                    path: input.path,
                    message: error.to_string(),
                }),
            }
        }
        catalog
    }
}

/// Convenience loader for manifest paths already resolved by another resource
/// system. Paths are tagged as [`ExtensionSource::Explicit`].
pub fn load_extension_manifest_paths<I, P>(
    paths: I,
    policy: &ExtensionPolicy,
    max_manifest_bytes: u64,
) -> ExtensionCatalog
where
    I: IntoIterator<Item = P>,
    P: Into<PathBuf>,
{
    ExtensionCatalog::load_resolved(
        paths.into_iter().map(|path| ExtensionManifestInput {
            path: path.into(),
            source: ExtensionSource::Explicit,
        }),
        policy,
        max_manifest_bytes,
    )
}

/// A tool schema supplied during the initialize handshake.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    /// Manifest-declared tool name.
    pub name: String,
    /// Model-facing description.
    pub description: String,
    /// JSON Schema for tool arguments.
    pub parameters: serde_json::Value,
}

/// A slash-command definition supplied during initialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDefinition {
    /// Manifest-declared command name, without a leading slash.
    pub name: String,
    /// User-facing summary.
    pub description: String,
    /// Optional compact usage string.
    #[serde(default)]
    pub usage: Option<String>,
}

/// Fully negotiated contributions for a running process.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContributions {
    /// Model-callable tools and their schemas.
    pub tools: Vec<ToolDefinition>,
    /// Interactive commands and their help metadata.
    pub commands: Vec<CommandDefinition>,
    /// Lifecycle hooks declared in the manifest.
    pub hooks: Vec<ExtensionHook>,
    /// Whether context requests are supported.
    pub context: bool,
    /// Semantic TUI surfaces declared in the manifest.
    pub ui: Vec<ExtensionUiSurface>,
    /// Tool names with semantic renderers.
    pub tool_renderers: Vec<String>,
    /// Whether notifications may arrive from the process.
    pub notifications: bool,
    /// Whether confirmation requests may arrive from the process.
    pub confirmations: bool,
}

/// Session and model facts exposed to an extension through typed requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtensionHostState {
    /// Stable session identifier, when a frontend has one.
    #[serde(default)]
    pub session_id: Option<String>,
    /// User-assigned session name, when present.
    #[serde(default)]
    pub session_name: Option<String>,
    /// Canonical current model identifier.
    #[serde(default)]
    pub model: Option<String>,
    /// Inspectably serialized reasoning configuration.
    #[serde(default)]
    pub reasoning: Option<serde_json::Value>,
    /// Skills explicitly active at this boundary.
    #[serde(default)]
    pub active_skills: Vec<ExtensionActiveSkill>,
}

/// Compact skill metadata sent to executable extensions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionActiveSkill {
    /// Stable skill identifier.
    pub id: String,
    /// Human-readable skill name.
    pub name: String,
    /// Optional skill version.
    #[serde(default)]
    pub version: Option<String>,
}

/// Ambient metadata supplied with commands, hooks, tools, and contributions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtensionExecutionContext {
    /// Active workspace root.
    pub workspace: PathBuf,
    /// Unique process-local tool execution scope, when invoked as a model tool.
    #[serde(default)]
    pub execution_scope: Option<String>,
    /// Current host state.
    pub host: ExtensionHostState,
}

/// Result returned by an executable tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallOutput {
    /// Compact model-visible result text.
    pub content: String,
    /// Whether the result represents a tool failure.
    #[serde(default)]
    pub is_error: bool,
    /// Optional structured data retained for frontend or renderer use.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Output from an extension slash command.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandOutput {
    /// Text to display to the user.
    #[serde(default)]
    pub text: String,
    /// Notifications emitted by the command.
    #[serde(default)]
    pub notifications: Vec<ExtensionNotification>,
    /// Optional context that should be considered by prompt composition.
    #[serde(default)]
    pub context: Vec<ContextContribution>,
}

/// Where a context contribution is inserted by prompt composition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPlacement {
    /// Before the host system prompt.
    SystemPrefix,
    /// After the host system prompt.
    SystemSuffix,
    /// Before the immediate user prompt.
    PromptPrefix,
    /// After the immediate user prompt.
    #[default]
    PromptSuffix,
}

/// Text contributed to prompt composition by an extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContribution {
    /// Stable label shown in context inspection.
    pub label: String,
    /// Plain text sent to the model after host-side size enforcement.
    pub content: String,
    /// Semantic insertion point.
    #[serde(default)]
    pub placement: ContextPlacement,
}

/// A lifecycle hook's disposition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ExtensionHookDisposition {
    /// Continue the normal operation.
    #[default]
    Continue,
    /// Deny an interceptable operation such as `before_tool_call`.
    Deny {
        /// Inspectable reason presented to the user and model.
        reason: String,
    },
}

/// Typed hook response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtensionHookOutput {
    /// Whether the intercepted operation should proceed.
    #[serde(default)]
    pub disposition: ExtensionHookDisposition,
    /// Additional prompt context produced at this boundary.
    #[serde(default)]
    pub context: Vec<ContextContribution>,
    /// User-visible notifications produced at this boundary.
    #[serde(default)]
    pub notifications: Vec<ExtensionNotification>,
}

/// A semantic status/header/footer contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionStatusContribution {
    /// Target semantic surface.
    pub surface: ExtensionUiSurface,
    /// Plain display text; terminal escape sequences are not interpreted.
    pub text: String,
    /// Optional semantic theme role, for example `extension.git.clean`.
    #[serde(default)]
    pub style_role: Option<String>,
    /// Higher values are retained first when space is constrained.
    #[serde(default)]
    pub priority: i32,
}

/// Notification severity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionNotificationLevel {
    /// Informational message.
    #[default]
    Info,
    /// Successful operation.
    Success,
    /// Recoverable warning.
    Warning,
    /// Failure requiring attention.
    Error,
}

/// User-visible notification emitted by an extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionNotification {
    /// Semantic severity.
    #[serde(default)]
    pub level: ExtensionNotificationLevel,
    /// Optional concise title.
    #[serde(default)]
    pub title: Option<String>,
    /// Plain notification body.
    pub message: String,
}

/// A confirmation prompt requested by an extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationRequest {
    /// Short action-oriented question.
    pub prompt: String,
    /// Optional additional consequence or scope detail.
    #[serde(default)]
    pub detail: Option<String>,
    /// Marks a potentially destructive action for stronger UI treatment.
    #[serde(default)]
    pub destructive: bool,
    /// Suggested default when a frontend supports one.
    #[serde(default)]
    pub default: bool,
}

/// Host answer to a confirmation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmationResponse {
    /// Whether the user approved the operation.
    pub confirmed: bool,
}

/// One semantic segment returned by a tool renderer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRenderSegment {
    /// Plain text content.
    pub text: String,
    /// Optional semantic role resolved through the active theme.
    #[serde(default)]
    pub style_role: Option<String>,
}

/// Semantic renderer output for one tool call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedToolCall {
    /// Ordered semantic segments. Newlines remain explicit in segment text.
    #[serde(default)]
    pub segments: Vec<ToolRenderSegment>,
}

/// JSON-RPC identifier used by a process-originated request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtensionRequestId {
    /// Numeric identifier.
    Number(u64),
    /// String identifier.
    String(String),
}

impl ExtensionRequestId {
    fn validate_confirmation_id(&self) -> Result<(), String> {
        let Self::String(id) = self else {
            return Ok(());
        };
        if id.len() > MAX_CONFIRMATION_REQUEST_ID_BYTES {
            return Err(format!(
                "confirmation request id is {} bytes; limit is {MAX_CONFIRMATION_REQUEST_ID_BYTES}",
                id.len()
            ));
        }
        Ok(())
    }
}

/// Asynchronous process-to-host event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionEvent {
    /// User-visible notification.
    Notification {
        /// Notification content.
        notification: ExtensionNotification,
    },
    /// Interactive confirmation request. The generation prevents a stale
    /// request from being answered after a reload.
    ConfirmationRequested {
        /// Process-originated JSON-RPC ID.
        request_id: ExtensionRequestId,
        /// Process generation that originated the request.
        generation: u64,
        /// Confirmation content.
        request: ConfirmationRequest,
    },
    /// Unsolicited prompt context contribution.
    ContextContributed {
        /// Context content.
        contribution: ContextContribution,
    },
    /// Unsolicited semantic TUI contribution.
    StatusContributed {
        /// Status/header/footer content.
        contribution: ExtensionStatusContribution,
    },
    /// Bounded stderr or protocol diagnostic.
    Diagnostic {
        /// Human-readable diagnostic text.
        message: String,
    },
}

/// Runtime knobs for one executable extension process.
#[derive(Clone, Debug)]
pub struct ExtensionRuntimeConfig {
    /// Workspace used as the child working directory and execution context.
    pub workspace: PathBuf,
    /// Initial session/model/skill state.
    pub host_state: ExtensionHostState,
    /// Maximum duration of one request.
    pub request_timeout: Duration,
    /// Grace period before a child is killed during shutdown/reload.
    pub shutdown_timeout: Duration,
    /// Maximum serialized JSON line size.
    pub max_message_bytes: usize,
    /// Maximum concurrent requests to one extension.
    pub max_pending_requests: usize,
}

impl ExtensionRuntimeConfig {
    /// Creates a runtime configuration with conservative bounded defaults.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            host_state: ExtensionHostState::default(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            max_message_bytes: DEFAULT_EXTENSION_MESSAGE_BYTES,
            max_pending_requests: DEFAULT_PENDING_REQUESTS,
        }
    }
}

/// Outcome of a successful extension reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionReloadReport {
    /// Newly active process generation.
    pub generation: u64,
    /// Whether the prior process acknowledged shutdown and exited in time.
    pub previous_shutdown_graceful: bool,
}

/// Manifest, policy, transport, and remote-protocol errors.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionRuntimeError {
    /// Manifest file I/O failed.
    #[error("cannot read extension manifest {}: {message}", path.display())]
    ManifestIo {
        /// Exact manifest path.
        path: PathBuf,
        /// Underlying I/O message.
        message: String,
    },
    /// Manifest exceeded the configured read bound.
    #[error("extension manifest {} is {bytes} bytes; limit is {limit}", path.display())]
    ManifestTooLarge {
        /// Exact manifest path.
        path: PathBuf,
        /// Observed size.
        bytes: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// TOML decoding failed.
    #[error("invalid extension TOML: {0}")]
    ManifestParse(String),
    /// Parsed manifest failed semantic validation.
    #[error("invalid extension manifest: {0}")]
    InvalidManifest(String),
    /// The extension asks for an unsupported API version.
    #[error("extension API {extension} is unsupported; host implements {host}")]
    UnsupportedApiVersion {
        /// Requested API version.
        extension: String,
        /// Host API version.
        host: String,
    },
    /// The extension has not been explicitly enabled.
    #[error("extension `{0}` is not enabled")]
    Disabled(String),
    /// The extension executable has not been explicitly trusted.
    #[error("extension `{0}` is not trusted")]
    Untrusted(String),
    /// Child process launch failed.
    #[error("failed to launch extension `{extension}`: {message}")]
    Spawn {
        /// Extension name.
        extension: String,
        /// Underlying process error.
        message: String,
    },
    /// JSON serialization or protocol validation failed.
    #[error("extension protocol error: {0}")]
    Protocol(String),
    /// A serialized or received message exceeded the configured bound.
    #[error("extension message exceeded {limit} bytes")]
    MessageTooLarge {
        /// Configured maximum.
        limit: usize,
    },
    /// An extension did not answer in time.
    #[error("extension request `{method}` timed out")]
    Timeout {
        /// JSON-RPC method.
        method: String,
    },
    /// The child process or protocol stream is no longer available.
    #[error("extension process closed: {0}")]
    Closed(String),
    /// The remote extension returned a JSON-RPC error.
    #[error("extension RPC error {code}: {message}")]
    Remote {
        /// JSON-RPC error code.
        code: i64,
        /// Remote message.
        message: String,
        /// Optional remote structured data.
        data: Option<serde_json::Value>,
    },
    /// The requested contribution was not declared in the manifest.
    #[error("extension `{extension}` did not declare {kind} `{name}`")]
    UndeclaredContribution {
        /// Extension name.
        extension: String,
        /// Contribution kind.
        kind: &'static str,
        /// Requested contribution name.
        name: String,
    },
    /// Reload produced tool or command registrations requiring a host rebuild.
    #[error(
        "extension `{extension}` changed contributions during reload; rebuild the ExtensionHost"
    )]
    ReloadRequiresReregistration {
        /// Extension name.
        extension: String,
    },
}

/// Stable JSON-RPC method names for executable-extension SDKs.
pub mod methods {
    /// Host-to-extension initialization handshake.
    pub const INITIALIZE: &str = "initialize";
    /// Host-to-extension tool invocation.
    pub const TOOL_CALL: &str = "tool/call";
    /// Host-to-extension slash-command invocation.
    pub const COMMAND_EXECUTE: &str = "command/execute";
    /// Host-to-extension lifecycle hook invocation.
    pub const HOOK_RUN: &str = "hook/run";
    /// Host request for prompt context.
    pub const CONTEXT_COLLECT: &str = "context/collect";
    /// Host request for a semantic status/header/footer contribution.
    pub const STATUS_COLLECT: &str = "status/collect";
    /// Host request for semantic tool-renderer output.
    pub const TOOL_RENDER: &str = "tool/render";
    /// Graceful lifecycle shutdown request.
    pub const SHUTDOWN: &str = "shutdown";
    /// Extension-to-host user notification.
    pub const NOTIFICATION: &str = "notification";
    /// Extension-to-host interactive confirmation request.
    pub const CONFIRMATION_REQUEST: &str = "confirmation/request";
    /// Extension-to-host unsolicited prompt context.
    pub const CONTEXT_CONTRIBUTION: &str = "context/contribution";
    /// Extension-to-host unsolicited semantic UI contribution.
    pub const STATUS_CONTRIBUTION: &str = "status/contribution";
}

/// Extension identity sent during initialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionIdentity {
    /// Stable extension name.
    pub name: String,
    /// Extension semantic version.
    pub version: String,
    /// Manifest file used to launch this process.
    pub manifest_path: PathBuf,
    /// Resource provenance.
    pub source: ExtensionSource,
}

/// Host-to-extension initialize parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitializeRequest {
    /// API version the host expects.
    pub api_version: String,
    /// Ygg crate version.
    pub ygg_version: String,
    /// Extension identity and provenance.
    pub extension: ExtensionIdentity,
    /// Active workspace.
    pub workspace: PathBuf,
    /// Manifest-declared privileges.
    pub capabilities: ExtensionCapabilities,
    /// Manifest-declared contribution names.
    pub contributes: ManifestContributions,
    /// Initial session/model/skill state.
    pub host: ExtensionHostState,
}

/// Extension-to-host initialize result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeResponse {
    /// API version implemented by the child.
    pub api_version: String,
    /// Complete schemas for manifest-declared tools.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// Complete metadata for manifest-declared commands.
    #[serde(default)]
    pub commands: Vec<CommandDefinition>,
}

/// Host-to-extension tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Tool name.
    pub name: String,
    /// Model-produced arguments.
    pub arguments: serde_json::Value,
    /// Current execution metadata.
    pub context: ExtensionExecutionContext,
}

/// Host-to-extension slash-command call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandRequest {
    /// Command name without a leading slash.
    pub name: String,
    /// Tokenized user arguments.
    pub arguments: Vec<String>,
    /// Current execution metadata.
    pub context: ExtensionExecutionContext,
}

/// Host-to-extension lifecycle hook call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookRequest {
    /// Hook boundary.
    pub hook: ExtensionHook,
    /// Boundary-specific semantic payload.
    pub payload: serde_json::Value,
    /// Current execution metadata.
    pub context: ExtensionExecutionContext,
}

/// Host request for extension-provided prompt context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextRequest {
    /// Immediate prompt before extension context is composed, when available.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Current execution metadata.
    pub context: ExtensionExecutionContext,
}

/// Host request for one semantic UI surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusRequest {
    /// Surface to populate.
    pub surface: ExtensionUiSurface,
    /// Current execution metadata.
    pub context: ExtensionExecutionContext,
}

/// Host request for semantic tool render output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRenderRequest {
    /// Tool whose lifecycle/result is being rendered.
    pub name: String,
    /// Tool arguments.
    pub arguments: serde_json::Value,
    /// Completed result text, when available.
    #[serde(default)]
    pub output: Option<String>,
    /// Whether the completed result is an error.
    #[serde(default)]
    pub is_error: bool,
    /// Current execution metadata.
    pub context: ExtensionExecutionContext,
}

/// A running executable extension. Clones share the same supervised child and
/// can be registered through the existing native [`ExtensionHost`].
#[derive(Clone)]
pub struct ExtensionProcess {
    inner: Arc<ExtensionProcessInner>,
}

#[derive(Default)]
struct AnsweredConfirmations {
    recent: VecDeque<(u64, ExtensionRequestId)>,
}

impl AnsweredConfirmations {
    fn insert(&mut self, generation: u64, request_id: ExtensionRequestId) -> bool {
        if self.contains(generation, &request_id) {
            return false;
        }
        if self.recent.len() == ANSWERED_CONFIRMATION_CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back((generation, request_id));
        true
    }

    fn remove(&mut self, generation: u64, request_id: &ExtensionRequestId) {
        if let Some(index) = self.recent.iter().position(|(entry_generation, entry_id)| {
            *entry_generation == generation && entry_id == request_id
        }) {
            self.recent.remove(index);
        }
    }

    fn contains(&self, generation: u64, request_id: &ExtensionRequestId) -> bool {
        self.recent.iter().any(|(entry_generation, entry_id)| {
            *entry_generation == generation && entry_id == request_id
        })
    }

    fn retain_generation(&mut self, generation: u64) {
        self.recent
            .retain(|(entry_generation, _)| *entry_generation == generation);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.recent.len()
    }
}

struct ExtensionProcessInner {
    descriptor: DiscoveredExtension,
    config: ExtensionRuntimeConfig,
    host_state: StdRwLock<ExtensionHostState>,
    contributions: ExtensionContributions,
    connection: StdRwLock<Arc<ProcessConnection>>,
    events: broadcast::Sender<ExtensionEvent>,
    initial_events: StdMutex<Option<broadcast::Receiver<ExtensionEvent>>>,
    answered_confirmations: StdMutex<AnsweredConfirmations>,
    generation: AtomicU64,
    reload_guard: Mutex<()>,
}

impl ExtensionProcess {
    /// Launches, initializes, and validates an explicitly enabled and trusted
    /// executable extension.
    pub async fn start(
        descriptor: DiscoveredExtension,
        config: ExtensionRuntimeConfig,
    ) -> Result<Self, ExtensionRuntimeError> {
        descriptor.ensure_startable()?;
        if config.max_message_bytes == 0 || config.max_pending_requests == 0 {
            return Err(ExtensionRuntimeError::Protocol(
                "message and pending-request limits must be greater than zero".into(),
            ));
        }
        if !config.workspace.is_dir() {
            return Err(ExtensionRuntimeError::Spawn {
                extension: descriptor.manifest.name.clone(),
                message: format!(
                    "workspace {} is not a directory",
                    config.workspace.display()
                ),
            });
        }

        // Retain the first receiver across initialization so an extension
        // cannot race a startup notification or confirmation ahead of the
        // product's first `subscribe` call.
        let (events, initial_events) = broadcast::channel(EXTENSION_EVENT_CAPACITY);
        let generation = 1;
        let (connection, contributions) = spawn_connection(
            &descriptor,
            &config,
            config.host_state.clone(),
            generation,
            events.clone(),
        )
        .await?;
        Ok(Self {
            inner: Arc::new(ExtensionProcessInner {
                host_state: StdRwLock::new(config.host_state.clone()),
                descriptor,
                config,
                contributions,
                connection: StdRwLock::new(connection),
                events,
                initial_events: StdMutex::new(Some(initial_events)),
                answered_confirmations: StdMutex::new(AnsweredConfirmations::default()),
                generation: AtomicU64::new(generation),
                reload_guard: Mutex::new(()),
            }),
        })
    }

    /// Returns the discovered manifest and activation metadata.
    pub fn descriptor(&self) -> &DiscoveredExtension {
        &self.inner.descriptor
    }

    /// Returns the contributions negotiated during initialization.
    pub fn contributions(&self) -> &ExtensionContributions {
        &self.inner.contributions
    }

    /// Subscribes to notifications, confirmations, contributions, and bounded
    /// stderr/protocol diagnostics. The first subscriber also receives events
    /// buffered during initialization. Slow receivers may observe a lag error.
    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionEvent> {
        lock_std_mutex(&self.inner.initial_events)
            .take()
            .unwrap_or_else(|| self.inner.events.subscribe())
    }

    /// Updates the session/model/skill snapshot attached to future calls and
    /// future reload initialization. Existing child state changes only when a
    /// typed request is made or the process reloads.
    pub fn set_host_state(&self, state: ExtensionHostState) {
        *write_std_lock(&self.inner.host_state) = state;
    }

    /// Returns whether the current process transport is open.
    pub fn is_running(&self) -> bool {
        !read_std_lock(&self.inner.connection)
            .closed
            .load(Ordering::Acquire)
    }

    /// Builds the current ambient execution context for command, hook,
    /// context, status, and renderer calls.
    pub fn current_context(&self) -> ExtensionExecutionContext {
        self.execution_context()
    }

    /// Invokes a manifest-declared model tool.
    pub async fn call_tool(
        &self,
        name: impl Into<String>,
        arguments: serde_json::Value,
        context: ExtensionExecutionContext,
    ) -> Result<ToolCallOutput, ExtensionRuntimeError> {
        let name = name.into();
        self.require_tool(&name)?;
        self.request_typed(
            methods::TOOL_CALL,
            &ToolCallRequest {
                name,
                arguments,
                context,
            },
        )
        .await
    }

    /// Invokes a manifest-declared slash command.
    pub async fn execute_command(
        &self,
        name: impl Into<String>,
        arguments: Vec<String>,
        context: ExtensionExecutionContext,
    ) -> Result<CommandOutput, ExtensionRuntimeError> {
        let name = name.into();
        if !self
            .inner
            .contributions
            .commands
            .iter()
            .any(|command| command.name == name)
        {
            return Err(self.undeclared("command", name));
        }
        self.request_typed(
            methods::COMMAND_EXECUTE,
            &CommandRequest {
                name,
                arguments,
                context,
            },
        )
        .await
    }

    /// Runs a manifest-declared lifecycle hook. Product code decides where an
    /// interceptable hook is applied; private agent state is never exposed.
    pub async fn run_hook(
        &self,
        hook: ExtensionHook,
        payload: serde_json::Value,
        context: ExtensionExecutionContext,
    ) -> Result<ExtensionHookOutput, ExtensionRuntimeError> {
        if !self.inner.contributions.hooks.contains(&hook) {
            return Err(self.undeclared("hook", format!("{hook:?}").to_ascii_lowercase()));
        }
        self.request_typed(
            methods::HOOK_RUN,
            &HookRequest {
                hook,
                payload,
                context,
            },
        )
        .await
    }

    /// Collects prompt context through the typed context contribution point.
    pub async fn collect_context(
        &self,
        prompt: Option<String>,
        context: ExtensionExecutionContext,
    ) -> Result<Vec<ContextContribution>, ExtensionRuntimeError> {
        if !self.inner.contributions.context {
            return Err(self.undeclared("context contribution", "context".into()));
        }
        self.request_typed(
            methods::CONTEXT_COLLECT,
            &ContextRequest { prompt, context },
        )
        .await
    }

    /// Collects a semantic status, header, or footer contribution.
    pub async fn collect_status(
        &self,
        surface: ExtensionUiSurface,
        context: ExtensionExecutionContext,
    ) -> Result<Option<ExtensionStatusContribution>, ExtensionRuntimeError> {
        if !self.inner.contributions.ui.contains(&surface) {
            return Err(self.undeclared("UI surface", format!("{surface:?}").to_ascii_lowercase()));
        }
        self.request_typed(methods::STATUS_COLLECT, &StatusRequest { surface, context })
            .await
    }

    /// Asks an extension to semantically render a declared tool lifecycle.
    pub async fn render_tool(
        &self,
        request: ToolRenderRequest,
    ) -> Result<RenderedToolCall, ExtensionRuntimeError> {
        if !self
            .inner
            .contributions
            .tool_renderers
            .iter()
            .any(|name| name == &request.name)
        {
            return Err(self.undeclared("tool renderer", request.name));
        }
        self.request_typed(methods::TOOL_RENDER, &request).await
    }

    /// Answers a process-originated confirmation request. Requests from a
    /// previous process generation are rejected after reload.
    pub async fn respond_to_confirmation(
        &self,
        request_id: ExtensionRequestId,
        generation: u64,
        response: ConfirmationResponse,
    ) -> Result<(), ExtensionRuntimeError> {
        request_id
            .validate_confirmation_id()
            .map_err(ExtensionRuntimeError::Protocol)?;
        let current_generation = self.inner.generation.load(Ordering::Acquire);
        if generation != current_generation {
            return Err(ExtensionRuntimeError::Closed(format!(
                "confirmation belongs to stale generation {generation}; current generation is {current_generation}"
            )));
        }
        if !self.inner.contributions.confirmations {
            return Err(self.undeclared("confirmation capability", "confirmations".into()));
        }
        {
            let mut answered = lock_std_mutex(&self.inner.answered_confirmations);
            if !answered.insert(generation, request_id.clone()) {
                return Ok(());
            }
        }
        let connection = read_std_lock(&self.inner.connection).clone();
        if let Err(error) = connection
            .send_response(request_id.clone(), &response)
            .await
        {
            lock_std_mutex(&self.inner.answered_confirmations).remove(generation, &request_id);
            return Err(error);
        }
        Ok(())
    }

    /// Whether a frontend or tool-progress consumer already answered this
    /// request. Product event drains use this to avoid duplicate UI/actions.
    pub fn confirmation_answered(&self, request_id: &ExtensionRequestId, generation: u64) -> bool {
        lock_std_mutex(&self.inner.answered_confirmations).contains(generation, request_id)
    }

    /// Restarts the process and atomically swaps it in after a successful
    /// handshake. Tool/command schemas must remain identical because the
    /// existing `ExtensionHost` has already registered them; changed schemas
    /// return `ReloadRequiresReregistration` and leave the old process active.
    pub async fn reload(&self) -> Result<ExtensionReloadReport, ExtensionRuntimeError> {
        let _guard = self.inner.reload_guard.lock().await;
        let generation = self
            .inner
            .generation
            .load(Ordering::Acquire)
            .saturating_add(1);
        let host_state = read_std_lock(&self.inner.host_state).clone();
        let (replacement, contributions) = spawn_connection(
            &self.inner.descriptor,
            &self.inner.config,
            host_state,
            generation,
            self.inner.events.clone(),
        )
        .await?;
        if contributions != self.inner.contributions {
            replacement.terminate().await;
            return Err(ExtensionRuntimeError::ReloadRequiresReregistration {
                extension: self.inner.descriptor.manifest.name.clone(),
            });
        }

        let previous = {
            let mut active = write_std_lock(&self.inner.connection);
            std::mem::replace(&mut *active, replacement)
        };
        self.inner.generation.store(generation, Ordering::Release);
        lock_std_mutex(&self.inner.answered_confirmations).retain_generation(generation);
        let previous_shutdown_graceful = previous.shutdown().await;
        Ok(ExtensionReloadReport {
            generation,
            previous_shutdown_graceful,
        })
    }

    /// Requests graceful shutdown, then kills the child after the configured
    /// grace period. Returns whether it acknowledged and exited gracefully.
    pub async fn shutdown(&self) -> bool {
        let _guard = self.inner.reload_guard.lock().await;
        let connection = read_std_lock(&self.inner.connection).clone();
        connection.shutdown().await
    }

    fn execution_context(&self) -> ExtensionExecutionContext {
        ExtensionExecutionContext {
            workspace: self.inner.config.workspace.clone(),
            execution_scope: None,
            host: read_std_lock(&self.inner.host_state).clone(),
        }
    }

    fn require_tool(&self, name: &str) -> Result<(), ExtensionRuntimeError> {
        if self
            .inner
            .contributions
            .tools
            .iter()
            .any(|tool| tool.name == name)
        {
            Ok(())
        } else {
            Err(self.undeclared("tool", name.to_owned()))
        }
    }

    fn undeclared(&self, kind: &'static str, name: String) -> ExtensionRuntimeError {
        ExtensionRuntimeError::UndeclaredContribution {
            extension: self.inner.descriptor.manifest.name.clone(),
            kind,
            name,
        }
    }

    async fn request_typed<P, R>(
        &self,
        method: &'static str,
        params: &P,
    ) -> Result<R, ExtensionRuntimeError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let params = serde_json::to_value(params)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        let connection = read_std_lock(&self.inner.connection).clone();
        let result = connection
            .request(method, params, self.inner.config.request_timeout)
            .await?;
        serde_json::from_value(result).map_err(|error| {
            ExtensionRuntimeError::Protocol(format!(
                "invalid `{method}` response from `{}`: {error}",
                self.inner.descriptor.manifest.name
            ))
        })
    }
}

impl Extension for ExtensionProcess {
    fn register(&self, host: &mut ExtensionHost) {
        for definition in &self.inner.contributions.tools {
            host.tool(ProcessTool {
                process: self.clone(),
                definition: definition.clone(),
            });
        }
        if self.inner.contributions.hooks.iter().any(|hook| {
            matches!(
                hook,
                ExtensionHook::BeforeToolCall | ExtensionHook::AfterToolCall
            )
        }) {
            host.tool_call_hook(self.clone());
        }
    }
}

#[async_trait::async_trait]
impl ToolCallHook for ExtensionProcess {
    async fn before_tool_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> Result<(), ToolError> {
        if !self
            .inner
            .contributions
            .hooks
            .contains(&ExtensionHook::BeforeToolCall)
        {
            return Ok(());
        }
        let output = self
            .run_hook(
                ExtensionHook::BeforeToolCall,
                serde_json::json!({ "name": name, "arguments": arguments }),
                self.tool_execution_context(context),
            )
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        self.publish_hook_output(&output);
        match output.disposition {
            ExtensionHookDisposition::Continue => Ok(()),
            ExtensionHookDisposition::Deny { reason } => Err(ToolError::new(format!(
                "extension `{}` denied tool `{name}`: {reason}",
                self.inner.descriptor.manifest.name
            ))),
        }
    }

    async fn after_tool_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        output: &str,
        is_error: bool,
        context: &ToolContext<'_>,
    ) {
        if !self
            .inner
            .contributions
            .hooks
            .contains(&ExtensionHook::AfterToolCall)
        {
            return;
        }
        match self
            .run_hook(
                ExtensionHook::AfterToolCall,
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                    "output": output,
                    "is_error": is_error,
                }),
                self.tool_execution_context(context),
            )
            .await
        {
            Ok(output) => self.publish_hook_output(&output),
            Err(error) => {
                let _ = self.inner.events.send(ExtensionEvent::Diagnostic {
                    message: format!("after_tool_call hook failed: {error}"),
                });
            }
        }
    }
}

impl ExtensionProcess {
    fn tool_execution_context(&self, context: &ToolContext<'_>) -> ExtensionExecutionContext {
        let mut execution = self.execution_context();
        execution.execution_scope = Some(context.execution_scope.to_owned());
        execution.host.active_skills = context
            .active_skills
            .iter()
            .map(|skill| ExtensionActiveSkill {
                id: skill.descriptor.id.clone(),
                name: skill.descriptor.name.clone(),
                version: skill.descriptor.version.clone(),
            })
            .collect();
        execution
    }

    fn publish_hook_output(&self, output: &ExtensionHookOutput) {
        for notification in &output.notifications {
            let _ = self.inner.events.send(ExtensionEvent::Notification {
                notification: notification.clone(),
            });
        }
        for contribution in &output.context {
            let _ = self.inner.events.send(ExtensionEvent::ContextContributed {
                contribution: contribution.clone(),
            });
        }
    }
}

struct ProcessTool {
    process: ExtensionProcess,
    definition: ToolDefinition,
}

#[async_trait::async_trait]
impl Tool for ProcessTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.definition.name.clone(),
            description: self.definition.description.clone(),
            parameters: self.definition.parameters.clone(),
        }
    }

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Unsafe
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let context = self.process.tool_execution_context(ctx);
        let mut events = self.process.subscribe();
        let mut events_open = true;
        let call = self
            .process
            .call_tool(self.definition.name.clone(), args, context);
        tokio::pin!(call);
        let output = loop {
            tokio::select! {
                output = &mut call => break output,
                event = events.recv(), if events_open => match event {
                    Ok(ExtensionEvent::ConfirmationRequested {
                        request_id,
                        generation,
                        request,
                    }) => {
                        let confirmed = ctx.progress.confirmation(
                            request.prompt,
                            request.detail,
                            request.destructive,
                            request.default,
                        ).await;
                        self.process.respond_to_confirmation(
                            request_id,
                            generation,
                            ConfirmationResponse { confirmed },
                        ).await.map_err(|error| ToolError::new(error.to_string()))?;
                    }
                    Ok(ExtensionEvent::Notification { notification }) => {
                        ctx.progress.status(format!(
                            "extension notification: {}",
                            notification.message
                        ));
                    }
                    Ok(ExtensionEvent::Diagnostic { message }) => {
                        ctx.progress.status(format!("extension diagnostic: {message}"));
                    }
                    Ok(ExtensionEvent::StatusContributed { contribution }) => {
                        ctx.progress.status(contribution.text);
                    }
                    Ok(ExtensionEvent::ContextContributed { .. }) => {}
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        ctx.progress.status(format!(
                            "extension event stream dropped {count} event(s)"
                        ));
                    }
                    Err(broadcast::error::RecvError::Closed) => events_open = false,
                }
            }
        };
        match output {
            Ok(output) if output.is_error => Err(ToolError::new(output.content)),
            Ok(output) => Ok(ToolOutput::new(output.content)),
            Err(error) => Err(ToolError::new(error.to_string())),
        }
    }
}

struct ProcessConnection {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: PendingRequests,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    slots: Arc<Semaphore>,
    max_message_bytes: usize,
    shutdown_timeout: Duration,
    process_group: ProcessGroupGuard,
}

#[derive(Clone, Debug)]
enum PendingError {
    Closed(String),
    Protocol(String),
    Remote {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },
}

type PendingReply = Result<serde_json::Value, PendingError>;
type PendingSender = oneshot::Sender<PendingReply>;
type PendingRequests = Arc<StdMutex<HashMap<u64, PendingSender>>>;

struct PendingRegistration {
    pending: PendingRequests,
    id: u64,
    armed: bool,
}

impl PendingRegistration {
    fn new(pending: PendingRequests, id: u64) -> Self {
        Self {
            pending,
            id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if self.armed {
            lock_std_mutex(&self.pending).remove(&self.id);
        }
    }
}

impl ProcessConnection {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(method, params, timeout, true).await
    }

    async fn request_during_shutdown(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(method, params, timeout, false).await
    }

    async fn request_inner(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        cancel_on_host_shutdown: bool,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeError::Closed("stdout is closed".into()));
        }
        let operation =
            async {
                let _slot =
                    self.slots.clone().acquire_owned().await.map_err(|_| {
                        ExtensionRuntimeError::Closed("request queue is closed".into())
                    })?;
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let (reply_tx, reply_rx) = oneshot::channel();
                lock_std_mutex(&self.pending).insert(id, reply_tx);
                let mut registration = PendingRegistration::new(Arc::clone(&self.pending), id);
                let message = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                });
                self.write_message(&message).await?;

                let reply = match reply_rx.await {
                    Ok(reply) => reply,
                    Err(_) => Err(PendingError::Closed("response channel closed".into())),
                };
                registration.disarm();
                reply.map_err(pending_error)
            };
        tokio::pin!(operation);
        let timed = tokio::time::timeout(timeout, &mut operation);
        tokio::pin!(timed);
        if cancel_on_host_shutdown {
            tokio::select! {
                biased;
                _ = host_shutdown_requested() => Err(ExtensionRuntimeError::Closed(
                    "host is shutting down".into(),
                )),
                result = &mut timed => result.map_err(|_| ExtensionRuntimeError::Timeout {
                    method: method.to_owned(),
                })?,
            }
        } else {
            timed.await.map_err(|_| ExtensionRuntimeError::Timeout {
                method: method.to_owned(),
            })?
        }
    }

    async fn send_response<T: Serialize + ?Sized>(
        &self,
        id: ExtensionRequestId,
        result: &T,
    ) -> Result<(), ExtensionRuntimeError> {
        let result = serde_json::to_value(result)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let write = self.write_message(&message);
        tokio::pin!(write);
        let timed = tokio::time::timeout(CONFIRMATION_RESPONSE_TIMEOUT, &mut write);
        tokio::pin!(timed);
        tokio::select! {
            biased;
            _ = host_shutdown_requested() => Err(ExtensionRuntimeError::Closed(
                "host is shutting down".into(),
            )),
            result = &mut timed => result.map_err(|_| ExtensionRuntimeError::Timeout {
                method: "confirmation/response".to_owned(),
            })?,
        }
    }

    async fn write_message(
        &self,
        message: &serde_json::Value,
    ) -> Result<(), ExtensionRuntimeError> {
        let mut line = serde_json::to_vec(message)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        line.push(b'\n');
        if line.len() > self.max_message_bytes {
            return Err(ExtensionRuntimeError::MessageTooLarge {
                limit: self.max_message_bytes,
            });
        }
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .map_err(|error| ExtensionRuntimeError::Closed(error.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|error| ExtensionRuntimeError::Closed(error.to_string()))
    }

    async fn shutdown(&self) -> bool {
        let acknowledged = if self.closed.load(Ordering::Acquire) {
            false
        } else {
            self.request_during_shutdown(
                methods::SHUTDOWN,
                serde_json::json!({}),
                self.shutdown_timeout,
            )
            .await
            .is_ok()
        };

        let exited = {
            let mut child = self.child.lock().await;
            match tokio::time::timeout(self.shutdown_timeout, child.wait()).await {
                Ok(Ok(_)) => {
                    // The direct child acknowledged and exited, but a child it
                    // spawned may still own the group's pipes. Reap the leader,
                    // then force-clean any residual members before releasing
                    // process-group ownership.
                    self.process_group.terminate_now();
                    true
                }
                Ok(Err(_)) => {
                    self.kill_process_group();
                    false
                }
                Err(_) => {
                    self.kill_process_group();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    false
                }
            }
        };
        self.closed.store(true, Ordering::Release);
        acknowledged && exited
    }

    async fn terminate(&self) {
        self.kill_process_group();
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        self.closed.store(true, Ordering::Release);
    }

    fn kill_process_group(&self) {
        self.process_group.terminate_now();
    }
}

impl Drop for ProcessConnection {
    fn drop(&mut self) {
        self.process_group.terminate_now();
    }
}

async fn spawn_connection(
    descriptor: &DiscoveredExtension,
    config: &ExtensionRuntimeConfig,
    host_state: ExtensionHostState,
    generation: u64,
    events: broadcast::Sender<ExtensionEvent>,
) -> Result<(Arc<ProcessConnection>, ExtensionContributions), ExtensionRuntimeError> {
    let extension_dir =
        descriptor
            .manifest_path
            .parent()
            .ok_or_else(|| ExtensionRuntimeError::Spawn {
                extension: descriptor.manifest.name.clone(),
                message: "manifest has no parent directory".into(),
            })?;
    let command_path = resolve_entrypoint_command(extension_dir, &descriptor.manifest.entrypoint);
    let mut command = Command::new(command_path);
    command
        .args(&descriptor.manifest.entrypoint.args)
        .current_dir(&config.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .envs(&descriptor.manifest.entrypoint.env)
        .env("YGG_EXTENSION_API_VERSION", EXTENSION_API_VERSION)
        .env("YGG_EXTENSION_NAME", &descriptor.manifest.name)
        .env("YGG_EXTENSION_DIR", extension_dir)
        .env("YGG_EXTENSION_MANIFEST", &descriptor.manifest_path)
        .env("YGG_WORKSPACE", &config.workspace);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| ExtensionRuntimeError::Spawn {
            extension: descriptor.manifest.name.clone(),
            message: error.to_string(),
        })?;
    let process_group_id = extension_process_group_id(&child);
    let process_group = ProcessGroupGuard::extension(process_group_id);
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ExtensionRuntimeError::Spawn {
            extension: descriptor.manifest.name.clone(),
            message: "child stdin was not piped".into(),
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExtensionRuntimeError::Spawn {
            extension: descriptor.manifest.name.clone(),
            message: "child stdout was not piped".into(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExtensionRuntimeError::Spawn {
            extension: descriptor.manifest.name.clone(),
            message: "child stderr was not piped".into(),
        })?;

    let pending = Arc::new(StdMutex::new(HashMap::new()));
    let closed = Arc::new(AtomicBool::new(false));
    tokio::spawn(read_protocol_stdout(
        stdout,
        Arc::clone(&pending),
        Arc::clone(&closed),
        events.clone(),
        generation,
        config.max_message_bytes,
        descriptor.manifest.contributes.clone(),
    ));
    tokio::spawn(read_extension_stderr(
        stderr,
        events,
        config.max_message_bytes,
    ));

    let connection = Arc::new(ProcessConnection {
        stdin: Mutex::new(stdin),
        child: Mutex::new(child),
        pending,
        next_id: AtomicU64::new(1),
        closed,
        slots: Arc::new(Semaphore::new(config.max_pending_requests)),
        max_message_bytes: config.max_message_bytes,
        shutdown_timeout: config.shutdown_timeout,
        process_group,
    });
    let initialize = InitializeRequest {
        api_version: EXTENSION_API_VERSION.to_owned(),
        ygg_version: env!("CARGO_PKG_VERSION").to_owned(),
        extension: ExtensionIdentity {
            name: descriptor.manifest.name.clone(),
            version: descriptor.manifest.version.clone(),
            manifest_path: descriptor.manifest_path.clone(),
            source: descriptor.source,
        },
        workspace: config.workspace.clone(),
        capabilities: descriptor.manifest.capabilities.clone(),
        contributes: descriptor.manifest.contributes.clone(),
        host: host_state,
    };
    let initialize_value = serde_json::to_value(initialize)
        .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
    let response = match connection
        .request(
            methods::INITIALIZE,
            initialize_value,
            config.request_timeout,
        )
        .await
    {
        Ok(value) => serde_json::from_value::<InitializeResponse>(value).map_err(|error| {
            ExtensionRuntimeError::Protocol(format!("invalid initialize response: {error}"))
        }),
        Err(error) => Err(error),
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            connection.terminate().await;
            return Err(error);
        }
    };
    let contributions = match negotiate_contributions(&descriptor.manifest, response) {
        Ok(contributions) => contributions,
        Err(error) => {
            connection.terminate().await;
            return Err(error);
        }
    };
    Ok((connection, contributions))
}

fn resolve_entrypoint_command(directory: &Path, entrypoint: &ExtensionEntrypoint) -> PathBuf {
    let configured = PathBuf::from(&entrypoint.command);
    if configured.is_absolute() {
        return configured;
    }
    let local = directory.join(&configured);
    if local.is_file() {
        local
    } else {
        configured
    }
}

fn negotiate_contributions(
    manifest: &ExtensionManifest,
    response: InitializeResponse,
) -> Result<ExtensionContributions, ExtensionRuntimeError> {
    if response.api_version != EXTENSION_API_VERSION {
        return Err(ExtensionRuntimeError::UnsupportedApiVersion {
            extension: response.api_version,
            host: EXTENSION_API_VERSION.to_owned(),
        });
    }

    let tool_names = response
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    ensure_same_contributions("tools", &manifest.contributes.tools, &tool_names)?;
    for tool in &response.tools {
        validate_identifier("tool", &tool.name, true)?;
        if tool.description.trim().is_empty() {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "tool `{}` has an empty description",
                tool.name
            )));
        }
        if !tool.parameters.is_object() {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "tool `{}` parameters must be a JSON Schema object",
                tool.name
            )));
        }
    }

    let command_names = response
        .commands
        .iter()
        .map(|command| command.name.clone())
        .collect::<Vec<_>>();
    ensure_same_contributions("commands", &manifest.contributes.commands, &command_names)?;
    for command in &response.commands {
        validate_identifier("command", &command.name, true)?;
        if command.description.trim().is_empty() {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "command `{}` has an empty description",
                command.name
            )));
        }
    }

    Ok(ExtensionContributions {
        tools: response.tools,
        commands: response.commands,
        hooks: manifest.contributes.hooks.clone(),
        context: manifest.contributes.context,
        ui: manifest.contributes.ui.clone(),
        tool_renderers: manifest.contributes.tool_renderers.clone(),
        notifications: manifest.contributes.notifications,
        confirmations: manifest.contributes.confirmations,
    })
}

fn ensure_same_contributions(
    kind: &str,
    declared: &[String],
    initialized: &[String],
) -> Result<(), ExtensionRuntimeError> {
    let declared_set = declared.iter().collect::<BTreeSet<_>>();
    let initialized_set = initialized.iter().collect::<BTreeSet<_>>();
    if declared_set == initialized_set
        && declared_set.len() == declared.len()
        && initialized_set.len() == initialized.len()
    {
        Ok(())
    } else {
        Err(ExtensionRuntimeError::Protocol(format!(
            "initialized {kind} do not match manifest declarations"
        )))
    }
}

async fn read_protocol_stdout<R>(
    mut stdout: R,
    pending: PendingRequests,
    closed: Arc<AtomicBool>,
    events: broadcast::Sender<ExtensionEvent>,
    generation: u64,
    max_message_bytes: usize,
    declared: ManifestContributions,
) where
    R: AsyncRead + Unpin,
{
    let mut read_buffer = [0_u8; 8192];
    let mut line = Vec::new();
    let result = 'stream: loop {
        let count = match stdout.read(&mut read_buffer).await {
            Ok(0) => {
                if line.is_empty() {
                    break 'stream Ok(());
                }
                break 'stream Err("stdout ended with an unterminated JSON message".into());
            }
            Ok(count) => count,
            Err(error) => break 'stream Err(error.to_string()),
        };
        for byte in &read_buffer[..count] {
            if *byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                if let Err(error) =
                    handle_protocol_line(&line, &pending, &events, generation, &declared)
                {
                    break 'stream Err(error);
                }
                line.clear();
            } else {
                line.push(*byte);
                if line.len() >= max_message_bytes {
                    break 'stream Err(format!(
                        "stdout message exceeded {max_message_bytes} bytes"
                    ));
                }
            }
        }
    };

    closed.store(true, Ordering::Release);
    let message = match result {
        Ok(()) => "extension stdout closed".to_owned(),
        Err(message) => {
            let _ = events.send(ExtensionEvent::Diagnostic {
                message: message.clone(),
            });
            message
        }
    };
    fail_all_pending(&pending, PendingError::Closed(message));
}

fn handle_protocol_line(
    line: &[u8],
    pending: &PendingRequests,
    events: &broadcast::Sender<ExtensionEvent>,
    generation: u64,
    declared: &ManifestContributions,
) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|error| format!("invalid JSON on stdout: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "protocol message must be a JSON object".to_owned())?;
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err("protocol message must set jsonrpc to 2.0".into());
    }

    if let Some(method) = object.get("method").and_then(serde_json::Value::as_str) {
        let params = object
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match method {
            methods::NOTIFICATION => {
                require_declared(declared.notifications, "notifications")?;
                let notification = serde_json::from_value(params)
                    .map_err(|error| format!("invalid notification: {error}"))?;
                let _ = events.send(ExtensionEvent::Notification { notification });
            }
            methods::CONFIRMATION_REQUEST => {
                require_declared(declared.confirmations, "confirmations")?;
                let id = object
                    .get("id")
                    .cloned()
                    .ok_or_else(|| "confirmation request requires an id".to_owned())?;
                let request_id: ExtensionRequestId = serde_json::from_value(id)
                    .map_err(|error| format!("invalid confirmation request id: {error}"))?;
                request_id
                    .validate_confirmation_id()
                    .map_err(|error| format!("invalid confirmation request id: {error}"))?;
                let request = serde_json::from_value(params)
                    .map_err(|error| format!("invalid confirmation request: {error}"))?;
                events
                    .send(ExtensionEvent::ConfirmationRequested {
                        request_id,
                        generation,
                        request,
                    })
                    .map_err(|_| {
                        "confirmation request arrived without an active event subscriber".to_owned()
                    })?;
            }
            methods::CONTEXT_CONTRIBUTION => {
                require_declared(declared.context, "context contributions")?;
                let contribution = serde_json::from_value(params)
                    .map_err(|error| format!("invalid context contribution: {error}"))?;
                let _ = events.send(ExtensionEvent::ContextContributed { contribution });
            }
            methods::STATUS_CONTRIBUTION => {
                let contribution = serde_json::from_value(params)
                    .map_err(|error| format!("invalid status contribution: {error}"))?;
                let ExtensionStatusContribution { surface, .. } = &contribution;
                require_declared(declared.ui.contains(surface), "UI contributions")?;
                let _ = events.send(ExtensionEvent::StatusContributed { contribution });
            }
            _ => {
                let _ = events.send(ExtensionEvent::Diagnostic {
                    message: format!("ignored unknown extension method `{method}`"),
                });
            }
        }
        return Ok(());
    }

    let id = object
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "response requires a numeric id".to_owned())?;
    let reply = if let Some(error) = object.get("error") {
        let error: RpcErrorObject = serde_json::from_value(error.clone())
            .map_err(|decode| format!("invalid JSON-RPC error: {decode}"))?;
        Err(PendingError::Remote {
            code: error.code,
            message: error.message,
            data: error.data,
        })
    } else if let Some(result) = object.get("result") {
        Ok(result.clone())
    } else {
        Err(PendingError::Protocol(
            "response requires result or error".into(),
        ))
    };
    let sender = lock_std_mutex(pending).remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(reply);
    } else {
        let _ = events.send(ExtensionEvent::Diagnostic {
            message: format!("ignored response for unknown request {id}"),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct RpcErrorObject {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

async fn read_extension_stderr<R>(
    stderr: R,
    events: broadcast::Sender<ExtensionEvent>,
    max_message_bytes: usize,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut bytes = vec![0_u8; max_message_bytes.clamp(1, 8192)];
    let mut buffered = Vec::new();
    loop {
        let count = match reader.read(&mut bytes).await {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => {
                let _ = events.send(ExtensionEvent::Diagnostic {
                    message: format!("extension stderr read failed: {error}"),
                });
                break;
            }
        };
        for byte in &bytes[..count] {
            if *byte == b'\n' || buffered.len() >= max_message_bytes.saturating_sub(1) {
                if !buffered.is_empty() {
                    let _ = events.send(ExtensionEvent::Diagnostic {
                        message: format!(
                            "extension stderr: {}",
                            String::from_utf8_lossy(&buffered)
                        ),
                    });
                    buffered.clear();
                }
            } else if *byte != b'\r' {
                buffered.push(*byte);
            }
        }
    }
    if !buffered.is_empty() {
        let _ = events.send(ExtensionEvent::Diagnostic {
            message: format!("extension stderr: {}", String::from_utf8_lossy(&buffered)),
        });
    }
}

fn fail_all_pending(pending: &PendingRequests, error: PendingError) {
    let mut pending = lock_std_mutex(pending);
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn pending_error(error: PendingError) -> ExtensionRuntimeError {
    match error {
        PendingError::Closed(message) => ExtensionRuntimeError::Closed(message),
        PendingError::Protocol(message) => ExtensionRuntimeError::Protocol(message),
        PendingError::Remote {
            code,
            message,
            data,
        } => ExtensionRuntimeError::Remote {
            code,
            message,
            data,
        },
    }
}

fn read_std_lock<T>(lock: &StdRwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_std_lock<T>(lock: &StdRwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_std_mutex<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn require_declared(declared: bool, contribution: &str) -> Result<(), String> {
    if declared {
        Ok(())
    } else {
        Err(format!(
            "extension emitted undeclared {contribution} capability"
        ))
    }
}

#[cfg(unix)]
fn extension_process_group_id(child: &Child) -> u64 {
    child.id().map(u64::from).unwrap_or(0)
}

#[cfg(not(unix))]
fn extension_process_group_id(_child: &Child) -> u64 {
    0
}

#[cfg(unix)]
fn kill_process_group(process_group_id: u64) {
    if let Ok(process_group_id) = i32::try_from(process_group_id) {
        if process_group_id > 0 {
            // SAFETY: the child was placed into a fresh process group whose
            // ID is its PID, so the negative kill target cannot name an
            // unrelated process group while this connection owns the child.
            unsafe {
                let _ = libc::kill(-process_group_id, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_process_group_id: u64) {}

fn validate_identifiers(
    kind: &str,
    values: &[String],
    extended: bool,
) -> Result<(), ExtensionRuntimeError> {
    for value in values {
        validate_identifier(kind, value, extended)?;
    }
    validate_unique(kind, values)
}

fn validate_identifier(
    kind: &str,
    value: &str,
    extended: bool,
) -> Result<(), ExtensionRuntimeError> {
    let mut characters = value.chars();
    let first = characters.next();
    let first_valid = if extended {
        first.is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
    } else {
        first.is_some_and(|character| character.is_ascii_lowercase())
    };
    let rest_valid = characters.all(|character| {
        if extended {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        } else {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }
    });
    if value.len() > 64 || !first_valid || !rest_valid {
        return Err(ExtensionRuntimeError::InvalidManifest(format!(
            "invalid {kind} identifier `{value}`"
        )));
    }
    Ok(())
}

fn validate_unique<T>(kind: &str, values: &[T]) -> Result<(), ExtensionRuntimeError>
where
    T: Ord + std::fmt::Debug,
{
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(ExtensionRuntimeError::InvalidManifest(format!(
                "duplicate {kind} `{value:?}`"
            )));
        }
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    const VALID_MANIFEST: &str = r#"
name = "git-tools"
version = "0.1.0"
api_version = "0.1"
description = "Local git helpers"

[entrypoint]
command = "git-tools"
args = ["--stdio"]

[capabilities]
filesystem = "workspace"
process = true
network = false

[contributes]
tools = ["git_status"]
commands = ["checkpoint"]
hooks = ["after_tool_call"]
ui = ["status"]
context = true
tool_renderers = ["git_status"]
notifications = true
confirmations = true
"#;

    #[test]
    fn answered_confirmations_cover_exactly_the_buffered_event_window() {
        let generation = 7;
        let mut answered = AnsweredConfirmations::default();

        for id in 0..ANSWERED_CONFIRMATION_CAPACITY {
            assert!(answered.insert(generation, ExtensionRequestId::Number(id as u64)));
        }
        assert_eq!(answered.len(), EXTENSION_EVENT_CAPACITY);
        assert!(answered.contains(generation, &ExtensionRequestId::Number(0)));

        assert!(!answered.insert(generation, ExtensionRequestId::Number(0)));
        assert_eq!(answered.len(), ANSWERED_CONFIRMATION_CAPACITY);

        assert!(answered.insert(
            generation,
            ExtensionRequestId::Number(ANSWERED_CONFIRMATION_CAPACITY as u64)
        ));
        assert_eq!(answered.len(), ANSWERED_CONFIRMATION_CAPACITY);
        assert!(!answered.contains(generation, &ExtensionRequestId::Number(0)));
        assert!(answered.contains(generation, &ExtensionRequestId::Number(1)));

        answered.retain_generation(generation + 1);
        assert_eq!(answered.len(), 0);
        assert!(answered.insert(generation + 1, ExtensionRequestId::Number(0)));
    }

    #[test]
    fn confirmation_request_string_ids_are_bounded_before_event_delivery() {
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let (events, mut receiver) = broadcast::channel(2);
        let declared = ManifestContributions {
            confirmations: true,
            ..ManifestContributions::default()
        };
        let accepted_id = "x".repeat(MAX_CONFIRMATION_REQUEST_ID_BYTES);
        let accepted = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": accepted_id,
            "method": methods::CONFIRMATION_REQUEST,
            "params": {"prompt": "Continue?"},
        }))
        .expect("serialize accepted confirmation");
        handle_protocol_line(&accepted, &pending, &events, 1, &declared)
            .expect("maximum-size confirmation id should be accepted");
        assert!(matches!(
            receiver.try_recv(),
            Ok(ExtensionEvent::ConfirmationRequested {
                request_id: ExtensionRequestId::String(id),
                generation: 1,
                ..
            }) if id.len() == MAX_CONFIRMATION_REQUEST_ID_BYTES
        ));

        let oversized = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "x".repeat(MAX_CONFIRMATION_REQUEST_ID_BYTES + 1),
            "method": methods::CONFIRMATION_REQUEST,
            "params": {"prompt": "Continue?"},
        }))
        .expect("serialize oversized confirmation");
        let error = handle_protocol_line(&oversized, &pending, &events, 1, &declared)
            .expect_err("oversized confirmation id should be rejected");
        assert!(error.contains(&format!("limit is {MAX_CONFIRMATION_REQUEST_ID_BYTES}")));
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn manifest_parses_the_minimum_product_boundary() {
        let manifest = ExtensionManifest::parse(VALID_MANIFEST).expect("valid manifest");
        assert_eq!(manifest.name, "git-tools");
        assert_eq!(
            manifest.capabilities.filesystem,
            ExtensionFilesystemAccess::Workspace
        );
        assert_eq!(manifest.contributes.tools, vec!["git_status"]);
        assert_eq!(
            manifest.contributes.hooks,
            vec![ExtensionHook::AfterToolCall]
        );
        assert_eq!(manifest.contributes.ui, vec![ExtensionUiSurface::Status]);
        assert!(manifest.contributes.context);
        assert!(manifest.contributes.confirmations);
    }

    #[test]
    fn manifest_rejects_api_mismatch_duplicate_names_and_unknown_keys() {
        let mismatch = VALID_MANIFEST.replace("api_version = \"0.1\"", "api_version = \"9\"");
        assert!(matches!(
            ExtensionManifest::parse(&mismatch),
            Err(ExtensionRuntimeError::UnsupportedApiVersion { .. })
        ));

        let duplicate = VALID_MANIFEST.replace(
            "tools = [\"git_status\"]",
            "tools = [\"git_status\", \"git_status\"]",
        );
        assert!(matches!(
            ExtensionManifest::parse(&duplicate),
            Err(ExtensionRuntimeError::InvalidManifest(message)) if message.contains("duplicate tool")
        ));

        let unknown = VALID_MANIFEST.replace("network = false", "network = false\nshell = true");
        assert!(matches!(
            ExtensionManifest::parse(&unknown),
            Err(ExtensionRuntimeError::ManifestParse(_))
        ));
    }

    #[test]
    fn bounded_manifest_load_rejects_oversized_files() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(EXTENSION_MANIFEST_FILENAME);
        std::fs::write(&path, VALID_MANIFEST).expect("write manifest");
        let error = ExtensionManifest::load_bounded(&path, 10).expect_err("must be bounded");
        assert!(matches!(
            error,
            ExtensionRuntimeError::ManifestTooLarge { limit: 10, .. }
        ));
    }

    #[test]
    fn discovery_is_sorted_and_catalog_precedence_is_caller_owned() {
        let temp = TempDir::new().expect("tempdir");
        let project = temp.path().join("project");
        let global = temp.path().join("home/.ygg/extensions");
        write_manifest(&project.join(".ygg/extensions/z-last"), "z-last", "z");
        write_manifest(
            &project.join(".ygg/extensions/git-tools"),
            "git-tools",
            "project",
        );
        write_manifest(&global.join("git-tools"), "git-tools", "global");

        let roots = default_extension_roots(&project, Some(&temp.path().join("home")));
        let (inputs, diagnostics) = discover_extension_manifests(&roots);
        assert!(diagnostics.is_empty());
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].source, ExtensionSource::Project);
        assert!(inputs[0].path.to_string_lossy().contains("git-tools"));

        let mut policy = ExtensionPolicy::default();
        policy.enable("git-tools");
        policy.trust("git-tools");
        let catalog = ExtensionCatalog::load_resolved(inputs, &policy, 64 * 1024);
        assert_eq!(catalog.extensions.len(), 2);
        assert_eq!(catalog.extensions[0].manifest.name, "git-tools");
        assert_eq!(
            catalog.extensions[0].manifest.description.as_deref(),
            Some("project")
        );
        assert_eq!(
            catalog.extensions[0].activation.trust,
            ExtensionTrust::Untrusted
        );
        assert!(catalog.extensions[0].activation.enabled);
        assert_eq!(catalog.diagnostics.len(), 1);
        assert!(catalog.diagnostics[0].message.contains("shadowed"));
    }

    #[test]
    fn executable_trust_is_bound_to_global_or_exact_selected_source() {
        let project = PathBuf::from("/workspace/.ygg/extensions/git-tools/extension.toml");
        let global = PathBuf::from("/home/user/.ygg/extensions/git-tools/extension.toml");
        let mut policy = ExtensionPolicy::default();
        policy.enable("git-tools");
        policy.trust("git-tools");

        assert_eq!(
            policy.activation("git-tools", &global, ExtensionSource::Global),
            ExtensionActivation {
                enabled: true,
                trust: ExtensionTrust::Trusted,
            }
        );
        assert_eq!(
            policy.activation("git-tools", &project, ExtensionSource::Project),
            ExtensionActivation {
                enabled: true,
                trust: ExtensionTrust::Untrusted,
            }
        );

        policy.trust_source("git-tools", project.clone());
        assert_eq!(
            policy
                .activation("git-tools", &project, ExtensionSource::Project)
                .trust,
            ExtensionTrust::Trusted
        );
        assert_eq!(
            policy
                .activation(
                    "git-tools",
                    Path::new("/other/project/extension.toml"),
                    ExtensionSource::Project,
                )
                .trust,
            ExtensionTrust::Untrusted
        );

        policy.revoke_trust("git-tools");
        policy.trust_for_invocation("git-tools");
        assert_eq!(
            policy
                .activation(
                    "git-tools",
                    Path::new("/one-shot/extension.toml"),
                    ExtensionSource::Explicit,
                )
                .trust,
            ExtensionTrust::Trusted
        );
    }

    #[tokio::test]
    async fn launch_requires_both_enablement_and_trust() {
        let temp = TempDir::new().expect("tempdir");
        let manifest = minimal_manifest("policy-test", "does-not-exist");
        let descriptor = DiscoveredExtension {
            manifest,
            manifest_path: temp.path().join(EXTENSION_MANIFEST_FILENAME),
            source: ExtensionSource::Explicit,
            activation: ExtensionActivation {
                enabled: true,
                trust: ExtensionTrust::Untrusted,
            },
        };
        let error =
            match ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
                .await
            {
                Ok(_) => panic!("untrusted process unexpectedly started"),
                Err(error) => error,
            };
        assert!(matches!(error, ExtensionRuntimeError::Untrusted(name) if name == "policy-test"));
    }

    #[test]
    fn handshake_must_exactly_match_manifest_contribution_names() {
        let manifest = ExtensionManifest::parse(VALID_MANIFEST).expect("valid manifest");
        let response = InitializeResponse {
            api_version: EXTENSION_API_VERSION.into(),
            tools: vec![ToolDefinition {
                name: "surprise".into(),
                description: "Undeclared".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            commands: vec![CommandDefinition {
                name: "checkpoint".into(),
                description: "Checkpoint".into(),
                usage: None,
            }],
        };
        assert!(matches!(
            negotiate_contributions(&manifest, response),
            Err(ExtensionRuntimeError::Protocol(message)) if message.contains("do not match")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_transport_registers_tools_and_routes_events_and_confirmation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("fixture.sh");
        std::fs::write(&script_path, protocol_fixture_script()).expect("write fixture");
        let mut permissions = std::fs::metadata(&script_path)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script_path, permissions).expect("chmod");

        let manifest = ExtensionManifest::parse(
            r#"
name = "fixture"
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "fixture.sh"
[contributes]
tools = ["echo"]
notifications = true
confirmations = true
"#,
        )
        .expect("manifest");
        let descriptor = trusted_descriptor(temp.path(), manifest);
        let process = ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
            .await
            .expect("start process");

        let mut host = ExtensionHost::new();
        host.load(&process);
        assert_eq!(host.tool_definitions()[0].name, "echo");

        let mut events = process.subscribe();
        let result = process
            .call_tool(
                "echo",
                serde_json::json!({"text": "hello"}),
                process.current_context(),
            )
            .await
            .expect("tool result");
        assert_eq!(result.content, "from extension");
        assert!(!result.is_error);

        let notification = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("notification timeout")
            .expect("notification event");
        assert!(matches!(
            notification,
            ExtensionEvent::Notification { notification }
                if notification.message == "tool called"
        ));
        let confirmation = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("confirmation timeout")
            .expect("confirmation event");
        let (request_id, generation) = match confirmation {
            ExtensionEvent::ConfirmationRequested {
                request_id,
                generation,
                request,
            } => {
                assert_eq!(request.prompt, "Continue?");
                (request_id, generation)
            }
            event => panic!("unexpected event: {event:?}"),
        };
        process
            .respond_to_confirmation(
                request_id.clone(),
                generation,
                ConfirmationResponse { confirmed: true },
            )
            .await
            .expect("confirmation response");
        assert!(process.confirmation_answered(&request_id, generation));
        process
            .respond_to_confirmation(
                request_id,
                generation,
                ConfirmationResponse { confirmed: false },
            )
            .await
            .expect("duplicate confirmation response is suppressed");
        assert!(process.shutdown().await);
        assert!(!process.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_swaps_only_after_a_compatible_handshake() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("reload.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
IFS= read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
"#,
        )
        .expect("write fixture");
        let mut permissions = std::fs::metadata(&script_path)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script_path, permissions).expect("chmod");

        let descriptor =
            trusted_descriptor(temp.path(), minimal_manifest("reloadable", "reload.sh"));
        let process = ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
            .await
            .expect("start process");
        let report = process.reload().await.expect("reload");
        assert_eq!(report.generation, 2);
        assert!(report.previous_shutdown_graceful);
        assert!(process.is_running());
        assert!(process.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_process_rpc_is_bounded_and_removes_its_pending_slot() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("hung-rpc.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
IFS= read -r request
sleep 30
"#,
        );
        let descriptor =
            trusted_descriptor(temp.path(), minimal_manifest("hung-rpc", "hung-rpc.sh"));
        let mut config = ExtensionRuntimeConfig::new(temp.path());
        config.shutdown_timeout = Duration::from_millis(50);
        let process = ExtensionProcess::start(descriptor, config)
            .await
            .expect("start process");
        let connection = read_std_lock(&process.inner.connection).clone();

        let started = Instant::now();
        let error = connection
            .request(
                "probe/hang",
                serde_json::json!({}),
                Duration::from_millis(100),
            )
            .await
            .expect_err("hung request must time out");
        assert!(
            matches!(error, ExtensionRuntimeError::Timeout { ref method }
                if method == "probe/hang"),
            "{error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "request exceeded its bounded deadline: {:?}",
            started.elapsed()
        );
        assert!(lock_std_mutex(&connection.pending).is_empty());
        assert!(!process.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_process_rpc_releases_its_pending_slot() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("dropped-rpc.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
IFS= read -r request
sleep 30
"#,
        );
        let descriptor = trusted_descriptor(
            temp.path(),
            minimal_manifest("dropped-rpc", "dropped-rpc.sh"),
        );
        let mut config = ExtensionRuntimeConfig::new(temp.path());
        config.shutdown_timeout = Duration::from_millis(50);
        let process = ExtensionProcess::start(descriptor, config)
            .await
            .expect("start process");
        let connection = read_std_lock(&process.inner.connection).clone();
        let request_connection = Arc::clone(&connection);
        let request = tokio::spawn(async move {
            request_connection
                .request("probe/drop", serde_json::json!({}), Duration::from_secs(5))
                .await
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if lock_std_mutex(&connection.pending).len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request never registered");
        request.abort();
        let _ = request.await;
        assert!(lock_std_mutex(&connection.pending).is_empty());
        connection.terminate().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graceful_shutdown_request_reaches_extension_before_exit() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("graceful.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
IFS= read -r shutdown
printf '%s\n' graceful > "$YGG_WORKSPACE/graceful.marker"
sleep 30 &
printf '%s\n' "$!" > "$YGG_WORKSPACE/graceful-descendant.pid"
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
"#,
        );
        let descriptor =
            trusted_descriptor(temp.path(), minimal_manifest("graceful", "graceful.sh"));
        let process = ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
            .await
            .expect("start process");

        assert!(process.shutdown().await);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("graceful.marker")).expect("shutdown marker"),
            "graceful\n"
        );
        let descendant = std::fs::read_to_string(temp.path().join("graceful-descendant.pid"))
            .expect("descendant marker")
            .trim()
            .parse::<i32>()
            .expect("descendant pid");
        let deadline = Instant::now() + Duration::from_millis(500);
        while process_id_exists(descendant) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !process_id_exists(descendant),
            "extension descendant survived graceful shutdown"
        );
    }

    fn write_manifest(directory: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(directory).expect("create extension directory");
        std::fs::write(
            directory.join(EXTENSION_MANIFEST_FILENAME),
            format!(
                r#"name = "{name}"
version = "0.1.0"
api_version = "0.1"
description = "{description}"
[entrypoint]
command = "test"
"#
            ),
        )
        .expect("write manifest");
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, source: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, source).expect("write fixture");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("chmod");
    }

    #[cfg(unix)]
    fn process_id_exists(pid: i32) -> bool {
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn minimal_manifest(name: &str, command: &str) -> ExtensionManifest {
        ExtensionManifest::parse(&format!(
            r#"name = "{name}"
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "{command}"
"#
        ))
        .expect("minimal manifest")
    }

    fn trusted_descriptor(directory: &Path, manifest: ExtensionManifest) -> DiscoveredExtension {
        DiscoveredExtension {
            manifest,
            manifest_path: directory.join(EXTENSION_MANIFEST_FILENAME),
            source: ExtensionSource::Explicit,
            activation: ExtensionActivation {
                enabled: true,
                trust: ExtensionTrust::Trusted,
            },
        }
    }

    fn protocol_fixture_script() -> &'static str {
        r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[{"name":"echo","description":"Echo a value","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}],"commands":[]}}'
IFS= read -r tool_call
printf '%s\n' '{"jsonrpc":"2.0","method":"notification","params":{"level":"info","message":"tool called"}}'
printf '%s\n' '{"jsonrpc":"2.0","id":"confirm-1","method":"confirmation/request","params":{"prompt":"Continue?","destructive":false,"default":false}}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":"from extension","is_error":false,"metadata":null}}'
IFS= read -r confirmation_response
IFS= read -r shutdown
case "$shutdown" in
  *'"method":"shutdown"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{}}' ;;
  *) exit 23 ;;
esac
"#
    }
}
