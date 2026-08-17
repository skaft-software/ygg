//! Executable extensions discovered from disk and connected over JSON lines.
//!
//! Native [`Extension`]s remain the lowest-overhead option for
//! built-ins. This module adds a language-neutral product boundary: a trusted,
//! explicitly enabled manifest launches one child process and exchanges typed
//! JSON-RPC 2.0 requests, responses, and notifications over stdin/stdout.
//! Capability declarations are consent metadata, not an operating-system
//! sandbox; executable extensions run with the current user's privileges.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, RwLock as StdRwLock, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Notify, Semaphore};
use ygg_ai::{Media, ToolDef};

use crate::artifact::{ArtifactId, ArtifactPublication, ArtifactSource, ArtifactStore};
use crate::delegation::ExtensionDelegationService;
use crate::events::AgentEvent;
use crate::extension::{
    DynamicToolRegistration, EventObserver, Extension, ExtensionHost, ToolCallHook,
};
use crate::extension_policy::{
    ExtensionActionIntent, ExtensionApprovalStore, ExtensionApprovalToken, ExtensionPolicyDecision,
};
use crate::extension_secret::{ExtensionSecretBroker, ExtensionSecretRequest};
use crate::tool::{
    CancellationToken, OutputStream, ReplaySafety, Tool, ToolContext, ToolError, ToolOutput,
    ToolOutputContentPart, ToolProgressSink,
};

/// The newest executable-extension API implemented by this Ygg release.
pub const EXTENSION_API_VERSION: &str = EXTENSION_API_VERSION_0_2;

/// Frozen compatibility version for simple, trusted text extensions.
pub const EXTENSION_API_VERSION_0_1: &str = "0.1";

/// Stateful extension protocol with cancellation, progress, and lifecycle.
pub const EXTENSION_API_VERSION_0_2: &str = "0.2";

/// API `0.2` cooperative request cancellation feature.
pub const EXTENSION_FEATURE_REQUEST_CANCELLATION: &str = "request_cancellation";
/// API `0.2` structured/media result feature.
pub const EXTENSION_FEATURE_CONTENT_PARTS: &str = "content_parts";
/// API `0.2` correlated progress feature.
pub const EXTENSION_FEATURE_REQUEST_PROGRESS: &str = "request_progress";
/// API `0.2` host-ingested artifact feature.
pub const EXTENSION_FEATURE_ARTIFACTS: &str = "artifacts";
/// API `0.2` observational lifecycle feature.
pub const EXTENSION_FEATURE_LIFECYCLE_EVENTS: &str = "lifecycle_events";
/// API `0.2` host-mediated action-intent feature.
pub const EXTENSION_FEATURE_POLICY_INTENTS: &str = "policy_intents";
/// API `0.2` live extension-owned tool catalog feature.
pub const EXTENSION_FEATURE_DYNAMIC_TOOLS: &str = "dynamic_tools";
/// API `0.2` host-owned child model-session service.
pub const EXTENSION_FEATURE_AGENT_SESSIONS: &str = "agent_sessions";
/// API `0.2` single-use host approval capability service.
pub const EXTENSION_FEATURE_APPROVALS: &str = "approvals";
/// API `0.2` owner-scoped host secret lookup service.
pub const EXTENSION_FEATURE_SECRETS: &str = "secrets";

const API_0_2_REQUIRED_FEATURES: &[&str] = &[
    EXTENSION_FEATURE_REQUEST_CANCELLATION,
    EXTENSION_FEATURE_CONTENT_PARTS,
];
const API_0_2_OPTIONAL_FEATURES: &[&str] = &[
    EXTENSION_FEATURE_REQUEST_PROGRESS,
    EXTENSION_FEATURE_ARTIFACTS,
    EXTENSION_FEATURE_LIFECYCLE_EVENTS,
    EXTENSION_FEATURE_POLICY_INTENTS,
    EXTENSION_FEATURE_DYNAMIC_TOOLS,
];

const MAX_EXTENSION_AGENT_WAIT_MS: u64 = 60_000;
const MAX_EXTENSION_SECRET_NAME_BYTES: usize = 64;

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
const DEFAULT_WRITER_QUEUE: usize = 128;
const DEFAULT_CANCELLATION_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_TOMBSTONE_TTL: Duration = Duration::from_secs(30);
const MAX_TOMBSTONES: usize = 512;
const MAX_CHILD_REQUESTS: usize = 128;
const MAX_CHILD_WORKERS: usize = 8;
const MAX_DYNAMIC_EXTENSION_TOOLS: usize = 256;
const DYNAMIC_CATALOG_QUEUE_CAPACITY: usize = 32;
const SUPERVISOR_BASE_BACKOFF: Duration = Duration::from_millis(250);
const SUPERVISOR_MAX_BACKOFF: Duration = Duration::from_secs(30);
const SUPERVISOR_MAX_RESTARTS: u32 = 8;
const SUPERVISOR_STABLE_READY: Duration = Duration::from_secs(30);
const SUPERVISOR_POLL: Duration = Duration::from_millis(100);
static NEXT_EXTENSION_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);
/// Maximum distinct extension-originated JSON-RPC IDs in one process
/// generation. IDs are never reusable inside the generation, preventing a
/// delayed frontend answer from targeting a later request with the same ID.
pub const MAX_EXTENSION_CHILD_REQUEST_IDS_PER_GENERATION: usize = 65_536;
const MAX_LIFECYCLE_REASON_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 prompt bytes admitted by an API `0.2` input request.
pub const MAX_EXTENSION_INPUT_PROMPT_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 answer bytes returned to an API `0.2` input request.
pub const MAX_EXTENSION_INPUT_VALUE_BYTES: usize = 256 * 1024;
/// Maximum ordered parts admitted from one API `0.2` tool result.
pub const MAX_EXTENSION_RESULT_CONTENT_PARTS: usize = 256;
/// Maximum aggregate referenced media bytes in one API `0.2` tool result.
/// Repeated references count repeatedly because downstream encoders may copy
/// each occurrence.
pub const MAX_EXTENSION_RESULT_MEDIA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCHEMA_VALIDATION_STEPS: usize = 65_536;
const EXTENSION_EVENT_CAPACITY: usize = 128;
// Retain every answered confirmation that can still be buffered for another
// event subscriber. Once this many newer confirmations have been answered, an
// older event has necessarily fallen outside the broadcast channel's window.
const ANSWERED_CONFIRMATION_CAPACITY: usize = EXTENSION_EVENT_CAPACITY;
const MAX_CONFIRMATION_REQUEST_ID_BYTES: usize = 256;

static HOST_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static HOST_SHUTDOWN_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Return the non-secret ambient environment permitted for model-controlled
/// subprocesses. Provider credentials, application tokens, dynamic-loader
/// controls, and arbitrary dotenv values are intentionally absent.
pub fn sanitized_subprocess_environment() -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "YGG_PACKAGE_DIR",
    ];
    ALLOWED
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| ((*name).into(), value)))
        .collect()
}

/// Marks the host as shutting down and cancels ordinary extension RPC work.
///
/// The flag is level-triggered so calls which start after the signal cannot
/// miss it. Protocol shutdown requests use a separate path and remain allowed.
pub fn begin_host_shutdown() {
    HOST_SHUTDOWN_REQUESTED.store(true, Ordering::Release);
    HOST_SHUTDOWN_NOTIFY.notify_waiters();
    #[cfg(unix)]
    if let Some(reaper) = LazyLock::force(&PROCESS_REAPER) {
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
    Bash,
    Extension,
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
const PROCESS_IDENTITY_TRACKING_AVAILABLE: bool = true;
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
const PROCESS_IDENTITY_TRACKING_AVAILABLE: bool = false;

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: i32,
    start_time: u128,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct ProcessSnapshot {
    identity: ProcessIdentity,
    parent_pid: i32,
    process_group_id: i32,
}

#[cfg(unix)]
struct DetachedBashSupervision {
    deadline: Instant,
    cancellation: CancellationToken,
}

#[cfg(unix)]
struct RegisteredProcessGroup {
    kind: RegisteredProcessKind,
    registration_id: u64,
    root: Option<ProcessIdentity>,
    original_group_active: bool,
    descendants: BTreeMap<i32, ProcessIdentity>,
    detached_bash: Option<DetachedBashSupervision>,
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct RegisteredProcessScanState {
    registration_id: u64,
    direct_bash_child_owned: bool,
}

#[cfg(unix)]
static REGISTERED_PROCESS_GROUPS: LazyLock<StdMutex<BTreeMap<i32, RegisteredProcessGroup>>> =
    LazyLock::new(|| StdMutex::new(BTreeMap::new()));
/// Keep process scan/application order monotonic. Otherwise an older scan can
/// finish last and erase descendants recorded by a newer scan.
#[cfg(unix)]
static PROCESS_SNAPSHOT_REFRESH: LazyLock<StdMutex<()>> = LazyLock::new(|| StdMutex::new(()));
static NEXT_PROCESS_GROUP_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
const PROCESS_REAPER_POLL: Duration = Duration::from_millis(25);

/// One process-wide supervisor records PID/start-time identities for descendants
/// and reaps successful `bash` groups with residual background work. A standard
/// thread keeps the cleanup boundary alive during async-runtime teardown.
#[cfg(unix)]
static PROCESS_REAPER: LazyLock<Option<std::thread::Thread>> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("ygg-process-reaper".into())
        .spawn(process_reaper_loop)
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
        let root = process_identity(process_group_id);
        lock_std_mutex(&REGISTERED_PROCESS_GROUPS).insert(
            process_group_id,
            RegisteredProcessGroup {
                kind,
                registration_id,
                root,
                original_group_active: root.is_some(),
                descendants: BTreeMap::new(),
                detached_bash: None,
            },
        );
        if let Some(reaper) = LazyLock::force(&PROCESS_REAPER) {
            reaper.unpark();
        }
    }
    #[cfg(not(unix))]
    let _ = (process_group_id, kind);
    registration_id
}

#[cfg(unix)]
fn remove_registered_process_group(
    process_group_id: i32,
    registration_id: u64,
) -> Option<RegisteredProcessGroup> {
    let mut registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
    if registered
        .get(&process_group_id)
        .is_some_and(|entry| entry.registration_id == registration_id)
    {
        registered.remove(&process_group_id)
    } else {
        None
    }
}

fn unregister_process_group(process_group_id: u64, registration_id: u64) -> bool {
    #[cfg(unix)]
    if let Some(process_group_id) = valid_process_group_id(process_group_id) {
        return remove_registered_process_group(process_group_id, registration_id).is_some();
    }
    #[cfg(not(unix))]
    let _ = (process_group_id, registration_id);
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

#[derive(Clone, Copy)]
struct ProcessTerminationHandle {
    process_group_id: u64,
    registration_id: u64,
}

impl ProcessTerminationHandle {
    fn terminate(self) {
        terminate_registered_process_group(
            self.process_group_id,
            self.registration_id,
            libc_sigkill(),
        );
    }
}

impl ProcessGroupGuard {
    /// Registers a shell or built-in `bash` child process group.
    pub fn bash(pid: Option<u32>) -> Self {
        Self::new(pid.map(u64::from).unwrap_or(0), RegisteredProcessKind::Bash)
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

    fn termination_handle(&self) -> ProcessTerminationHandle {
        ProcessTerminationHandle {
            process_group_id: self.process_group_id.load(Ordering::Acquire),
            registration_id: self.registration_id,
        }
    }

    /// Immediately force-terminates the owned process group.
    pub fn terminate_now(&self) {
        let process_group_id = self.process_group_id.swap(0, Ordering::AcqRel);
        terminate_registered_process_group(process_group_id, self.registration_id, libc_sigkill());
    }

    /// Releases the group after its child and output pipes have fully settled.
    pub fn disarm(&self) {
        let process_group_id = self.process_group_id.swap(0, Ordering::AcqRel);
        unregister_process_group(process_group_id, self.registration_id);
    }

    /// Transfers a successfully reaped direct `bash` child to the centralized
    /// descendant supervisor. The registry entry remains live until the group
    /// disappears naturally, the run is cancelled, the original execution
    /// deadline expires, or host shutdown begins.
    pub fn supervise_bash_descendants(self, lifetime: Duration, cancellation: CancellationToken) {
        #[cfg(unix)]
        {
            let process_group_id = self.process_group_id.load(Ordering::Acquire);
            let Some(process_group_id_i32) = valid_process_group_id(process_group_id) else {
                self.disarm();
                return;
            };
            // A whole-process-table scan can race the shell's final exit and
            // miss a background child that has already inherited its group.
            // Retry the bounded handoff while the freshly created group still
            // exists; stable PID/start-time identities remain mandatory.
            let mut descendants_found = false;
            for _ in 0..3 {
                refresh_registered_descendants();
                let handoff_is_bound = if PROCESS_IDENTITY_TRACKING_AVAILABLE {
                    registered_process_has_live_identity(process_group_id_i32, self.registration_id)
                } else {
                    registered_process_is_alive(process_group_id_i32, self.registration_id)
                };
                if handoff_is_bound {
                    descendants_found = true;
                    break;
                }
                if !process_group_is_alive(process_group_id_i32) {
                    break;
                }
                std::thread::yield_now();
            }
            if !descendants_found {
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
            let Some(reaper) = LazyLock::force(&PROCESS_REAPER) else {
                self.terminate_now();
                return;
            };

            let transferred = {
                let mut registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
                let Some(entry) = registered.get_mut(&process_group_id_i32) else {
                    return;
                };
                if entry.registration_id != self.registration_id
                    || entry.kind != RegisteredProcessKind::Bash
                {
                    return;
                }
                entry.detached_bash = Some(DetachedBashSupervision {
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

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_process_snapshot(pid: i32) -> Option<ProcessSnapshot> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat.get(stat.rfind(") ")?.saturating_add(2)..)?;
    let fields = fields.split_ascii_whitespace().collect::<Vec<_>>();
    let parent_pid = fields.get(1)?.parse().ok()?;
    let process_group_id = fields.get(2)?.parse().ok()?;
    let start_time = fields.get(19)?.parse::<u128>().ok()?;
    Some(ProcessSnapshot {
        identity: ProcessIdentity { pid, start_time },
        parent_pid,
        process_group_id,
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_snapshots() -> Vec<ProcessSnapshot> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(linux_process_snapshot)
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apple_process_snapshot(pid: i32) -> Option<ProcessSnapshot> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_i32 = i32::try_from(size).ok()?;
    // SAFETY: `info` points to exactly `size_i32` writable bytes and
    // PROC_PIDTBSDINFO initializes the complete proc_bsdinfo on success.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_i32,
        )
    };
    if read != size_i32 {
        return None;
    }
    // SAFETY: the exact-size success check above proves initialization.
    let info = unsafe { info.assume_init() };
    if i32::try_from(info.pbi_pid).ok()? != pid {
        return None;
    }
    let parent_pid = i32::try_from(info.pbi_ppid).ok()?;
    let process_group_id = i32::try_from(info.pbi_pgid).ok()?;
    let start_time = u128::from(info.pbi_start_tvsec)
        .saturating_mul(1_000_000)
        .saturating_add(u128::from(info.pbi_start_tvusec));
    Some(ProcessSnapshot {
        identity: ProcessIdentity { pid, start_time },
        parent_pid,
        process_group_id,
    })
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apple_related_pids(process_id: i32, children: bool) -> Vec<i32> {
    // SAFETY: null/zero queries ask libproc only for the required PID count.
    let count = unsafe {
        if children {
            libc::proc_listchildpids(process_id, std::ptr::null_mut(), 0)
        } else {
            libc::proc_listpgrppids(process_id, std::ptr::null_mut(), 0)
        }
    };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    let capacity = count.saturating_add(16);
    let mut pids = vec![0_i32; capacity];
    let Ok(bytes) = i32::try_from(capacity.saturating_mul(std::mem::size_of::<i32>())) else {
        return Vec::new();
    };
    // SAFETY: `pids` exposes `bytes` writable bytes and libproc returns no more
    // PID entries than fit in the supplied buffer.
    let listed = unsafe {
        if children {
            libc::proc_listchildpids(process_id, pids.as_mut_ptr().cast(), bytes)
        } else {
            libc::proc_listpgrppids(process_id, pids.as_mut_ptr().cast(), bytes)
        }
    };
    let Ok(listed) = usize::try_from(listed) else {
        return Vec::new();
    };
    pids.truncate(listed.min(pids.len()));
    pids.retain(|pid| *pid > 0);
    pids
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn process_snapshots() -> Vec<ProcessSnapshot> {
    let (mut pending, active_groups) = {
        let registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
        let mut pending = BTreeSet::new();
        let mut active_groups = Vec::new();
        for (process_group_id, entry) in registered.iter() {
            pending.extend(entry.root.into_iter().map(|identity| identity.pid));
            pending.extend(entry.descendants.keys().copied());
            if entry.original_group_active {
                active_groups.push(*process_group_id);
            }
        }
        (pending, active_groups)
    };
    for process_group_id in active_groups {
        pending.extend(apple_related_pids(process_group_id, false));
    }

    let mut discovered = BTreeSet::new();
    let mut snapshots = Vec::new();
    while let Some(pid) = pending.pop_first() {
        if !discovered.insert(pid) {
            continue;
        }
        let Some(snapshot) = apple_process_snapshot(pid) else {
            continue;
        };
        pending.extend(apple_related_pids(pid, true));
        snapshots.push(snapshot);
    }
    snapshots
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn process_snapshots() -> Vec<ProcessSnapshot> {
    Vec::new()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_snapshot(pid: i32) -> Option<ProcessSnapshot> {
    linux_process_snapshot(pid)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn process_snapshot(pid: i32) -> Option<ProcessSnapshot> {
    apple_process_snapshot(pid)
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn process_snapshot(pid: i32) -> Option<ProcessSnapshot> {
    process_snapshots()
        .into_iter()
        .find(|snapshot| snapshot.identity.pid == pid)
}

#[cfg(unix)]
fn process_identity(pid: i32) -> Option<ProcessIdentity> {
    process_snapshot(pid).map(|snapshot| snapshot.identity)
}

#[cfg(unix)]
fn process_identity_is_alive(identity: ProcessIdentity) -> bool {
    process_identity(identity.pid) == Some(identity)
}

#[cfg(unix)]
fn refresh_registered_descendants() {
    let _refresh_guard = lock_std_mutex(&PROCESS_SNAPSHOT_REFRESH);
    // Process discovery runs without the registry lock. Record which
    // registrations and directly-owned state the scan can describe so a group
    // inserted, replaced, or handed off while the scan is in flight is never
    // invalidated by stale observations.
    let (registration_states_at_snapshot_start, known_identities) = {
        let registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
        let registration_states = registered
            .iter()
            .map(|(process_group_id, entry)| {
                (
                    *process_group_id,
                    RegisteredProcessScanState {
                        registration_id: entry.registration_id,
                        direct_bash_child_owned: entry.kind == RegisteredProcessKind::Bash
                            && entry.detached_bash.is_none(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let identities = registered
            .values()
            .flat_map(|entry| {
                entry
                    .root
                    .into_iter()
                    .chain(entry.descendants.values().copied())
            })
            .collect::<Vec<_>>();
        (registration_states, identities)
    };
    let snapshots = process_snapshots();
    if snapshots.is_empty() {
        return;
    }
    let mut snapshots_by_pid = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.identity.pid, snapshot))
        .collect::<BTreeMap<_, _>>();
    // Whole-system enumeration is not atomic and platform APIs may briefly
    // omit a live process. Re-read every missing identity directly before
    // treating it as dead and severing ownership of its descendants.
    for identity in known_identities {
        if snapshots_by_pid.contains_key(&identity.pid) {
            continue;
        }
        let Some(snapshot) = process_snapshot(identity.pid) else {
            continue;
        };
        snapshots_by_pid.insert(identity.pid, snapshot);
    }
    let mut registered = lock_std_mutex(&REGISTERED_PROCESS_GROUPS);
    apply_process_snapshots(
        &mut registered,
        &registration_states_at_snapshot_start,
        &snapshots_by_pid,
    );
}

#[cfg(unix)]
fn apply_process_snapshots(
    registered: &mut BTreeMap<i32, RegisteredProcessGroup>,
    registration_states_at_snapshot_start: &BTreeMap<i32, RegisteredProcessScanState>,
    snapshots_by_pid: &BTreeMap<i32, ProcessSnapshot>,
) {
    for (process_group_id, entry) in registered.iter_mut() {
        let Some(scan_state) = registration_states_at_snapshot_start.get(process_group_id) else {
            continue;
        };
        if scan_state.registration_id != entry.registration_id {
            continue;
        }
        entry.descendants.retain(|pid, identity| {
            snapshots_by_pid
                .get(pid)
                .is_some_and(|snapshot| snapshot.identity == *identity)
        });

        let root_alive = entry.root.and_then(|identity| {
            snapshots_by_pid
                .get(&identity.pid)
                .filter(|snapshot| snapshot.identity == identity)
                .copied()
        });
        let mut owned = entry.descendants.clone();
        if let Some(root) = root_alive {
            owned.insert(root.identity.pid, root.identity);
        }
        let group_has_member = snapshots_by_pid
            .values()
            .any(|snapshot| snapshot.process_group_id == *process_group_id);
        // A scan may straddle the direct child's exit and its descendant's
        // creation. Keep the freshly owned group bound until Bash hands it to
        // detached supervision; the handoff performs fresh scans below.
        let group_is_bound =
            entry.original_group_active && (group_has_member || scan_state.direct_bash_child_owned);

        loop {
            let mut changed = false;
            for snapshot in snapshots_by_pid.values() {
                if owned.contains_key(&snapshot.identity.pid) {
                    continue;
                }
                let child_of_owned = owned.contains_key(&snapshot.parent_pid);
                let original_group_member =
                    group_is_bound && snapshot.process_group_id == *process_group_id;
                if child_of_owned || original_group_member {
                    owned.insert(snapshot.identity.pid, snapshot.identity);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        if let Some(root) = entry.root {
            owned.remove(&root.pid);
        }
        entry.original_group_active = group_is_bound;
        entry.descendants = owned;
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct RegisteredProcessTargets {
    process_group_id: i32,
    root: Option<ProcessIdentity>,
    descendants: Vec<ProcessIdentity>,
}

#[cfg(unix)]
impl RegisteredProcessTargets {
    fn identities(&self) -> impl Iterator<Item = ProcessIdentity> + '_ {
        self.root
            .into_iter()
            .chain(self.descendants.iter().copied())
    }
}

#[cfg(unix)]
fn targets_from_registered(
    process_group_id: i32,
    registered: &RegisteredProcessGroup,
) -> RegisteredProcessTargets {
    RegisteredProcessTargets {
        process_group_id,
        root: registered.root,
        descendants: registered.descendants.values().copied().collect(),
    }
}

#[cfg(unix)]
fn registered_process_keys(kind: Option<RegisteredProcessKind>) -> Vec<(i32, u64)> {
    lock_std_mutex(&REGISTERED_PROCESS_GROUPS)
        .iter()
        .filter_map(|(process_group_id, registered)| {
            kind.is_none_or(|kind| kind == registered.kind)
                .then_some((*process_group_id, registered.registration_id))
        })
        .collect()
}

#[cfg(unix)]
fn registered_targets(
    process_group_id: i32,
    registration_id: u64,
) -> Option<RegisteredProcessTargets> {
    lock_std_mutex(&REGISTERED_PROCESS_GROUPS)
        .get(&process_group_id)
        .filter(|registered| registered.registration_id == registration_id)
        .map(|registered| targets_from_registered(process_group_id, registered))
}

#[cfg(unix)]
fn detached_bash_process_groups() -> Vec<(i32, u64, Instant, CancellationToken)> {
    lock_std_mutex(&REGISTERED_PROCESS_GROUPS)
        .iter()
        .filter_map(|(process_group_id, registered)| {
            registered.detached_bash.as_ref().map(|supervision| {
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
fn signal_identity(identity: ProcessIdentity, signal: i32) {
    if process_identity(identity.pid) != Some(identity) {
        return;
    }
    // SAFETY: the immediately preceding start-time check binds this PID to the
    // process recorded while it was a descendant of Ygg's registered child.
    unsafe {
        let _ = libc::kill(identity.pid, signal);
    }
}

#[cfg(unix)]
fn signal_registered_targets(targets: &RegisteredProcessTargets, signal: i32) {
    let group_is_bound = targets.identities().any(|identity| {
        process_snapshot(identity.pid).is_some_and(|snapshot| {
            snapshot.identity == identity && snapshot.process_group_id == targets.process_group_id
        })
    });
    if group_is_bound
        || (!PROCESS_IDENTITY_TRACKING_AVAILABLE
            && process_group_is_alive(targets.process_group_id))
    {
        // SAFETY: supported platforms require a matching PID/start-time
        // identity in the group, so its ID cannot name an unrelated group.
        // Other Unix targets retain the legacy best-effort group cleanup.
        unsafe {
            let _ = libc::kill(-targets.process_group_id, signal);
        }
    }
    for identity in targets.identities() {
        signal_identity(identity, signal);
    }
}

#[cfg(unix)]
fn registered_process_has_live_identity(process_group_id: i32, registration_id: u64) -> bool {
    registered_targets(process_group_id, registration_id)
        .is_some_and(|targets| targets.identities().any(process_identity_is_alive))
}

#[cfg(unix)]
fn registered_process_is_alive(process_group_id: i32, registration_id: u64) -> bool {
    let Some(targets) = registered_targets(process_group_id, registration_id) else {
        return false;
    };
    let mut identities = targets.identities().peekable();
    if identities.peek().is_none() {
        return process_group_is_alive(process_group_id);
    }
    identities.any(process_identity_is_alive)
}

fn libc_sigkill() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGKILL
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn terminate_registered_process_group(process_group_id: u64, registration_id: u64, signal: i32) {
    #[cfg(unix)]
    {
        refresh_registered_descendants();
        let Some(process_group_id) = valid_process_group_id(process_group_id) else {
            return;
        };
        if let Some(registered) = remove_registered_process_group(process_group_id, registration_id)
        {
            let targets = targets_from_registered(process_group_id, &registered);
            signal_registered_targets(&targets, signal);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (registration_id, signal);
        kill_process_group(process_group_id);
    }
}

#[cfg(unix)]
fn process_reaper_loop() {
    loop {
        refresh_registered_descendants();
        let has_registered = !lock_std_mutex(&REGISTERED_PROCESS_GROUPS).is_empty();
        if !has_registered {
            std::thread::park();
            continue;
        }

        let now = Instant::now();
        let host_shutdown = HOST_SHUTDOWN_REQUESTED.load(Ordering::Acquire);
        let mut next_poll = PROCESS_REAPER_POLL;
        for (process_group_id, registration_id, deadline, cancellation) in
            detached_bash_process_groups()
        {
            let alive = registered_process_is_alive(process_group_id, registration_id);
            let terminate = host_shutdown || cancellation.is_cancelled() || now >= deadline;
            if !alive {
                unregister_process_group(process_group_id as u64, registration_id);
                continue;
            }
            if terminate {
                terminate_registered_process_group(
                    process_group_id as u64,
                    registration_id,
                    libc::SIGKILL,
                );
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
fn process_group_is_alive(process_group_id: i32) -> bool {
    // Signal zero performs existence/permission checking without changing the
    // target. EPERM still means that the group exists.
    let result = unsafe { libc::kill(-process_group_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn signal_registered_processes(keys: &[(i32, u64)], signal: i32) {
    refresh_registered_descendants();
    for (process_group_id, registration_id) in keys {
        if let Some(targets) = registered_targets(*process_group_id, *registration_id) {
            signal_registered_targets(&targets, signal);
        }
    }
}

/// Gracefully terminates registered shell/`bash` process trees, then force-kills
/// and waits for survivors, all within the supplied total timeout.
pub async fn terminate_bash_process_groups(timeout: Duration) {
    #[cfg(unix)]
    {
        let process_keys = registered_process_keys(Some(RegisteredProcessKind::Bash));
        if process_keys.is_empty() {
            return;
        }
        let started = Instant::now();
        let graceful_deadline = started + timeout / 2;
        let final_deadline = started + timeout;
        signal_registered_processes(&process_keys, libc::SIGTERM);

        let mut survivors = process_keys;
        while Instant::now() < graceful_deadline {
            refresh_registered_descendants();
            survivors.retain(|(process_group_id, registration_id)| {
                registered_process_is_alive(*process_group_id, *registration_id)
            });
            if survivors.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        signal_registered_processes(&survivors, libc::SIGKILL);
        while Instant::now() < final_deadline {
            refresh_registered_descendants();
            survivors.retain(|(process_group_id, registration_id)| {
                registered_process_is_alive(*process_group_id, *registration_id)
            });
            if survivors.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[cfg(not(unix))]
    let _ = timeout;
}

/// Force-kills every registered shell, `bash`, and extension process tree.
///
/// This is the last-resort watchdog path after coordinated cleanup times out.
pub fn force_kill_registered_process_groups() {
    #[cfg(unix)]
    {
        let process_keys = registered_process_keys(None);
        signal_registered_processes(&process_keys, libc::SIGKILL);
    }
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
        if !matches!(
            self.api_version.as_str(),
            EXTENSION_API_VERSION_0_1 | EXTENSION_API_VERSION_0_2
        ) {
            return Err(ExtensionRuntimeError::UnsupportedApiVersion {
                extension: self.api_version.clone(),
                host: format!("{EXTENSION_API_VERSION_0_1} or {EXTENSION_API_VERSION_0_2}"),
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
        validate_identifiers("secret", &self.capabilities.secrets, true)?;
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
    /// Exact logical secret names this extension may request from a configured
    /// host broker. An empty list disables secret negotiation for the process.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
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
    /// Optional API `0.2` JSON Schema for `structured_content`.
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
}

/// API `0.2` request to add or replace extension-owned tools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRegistrationRequest {
    /// Complete definitions to merge into the current extension catalog.
    pub tools: Vec<ToolDefinition>,
}

/// API `0.2` request to remove extension-owned tools by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUnregistrationRequest {
    /// Tool names to remove. Missing names are ignored, making retries safe.
    pub names: Vec<String>,
}

/// Host acknowledgement for a live tool catalog mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalogUpdateResponse {
    /// Monotonic catalog epoch within this subprocess generation. It starts at
    /// zero after initialize/reload and increments for each accepted mutation.
    pub revision: u64,
    /// Complete active tool-name set for this extension.
    pub tools: Vec<String>,
}

/// API `0.2` request to create an isolated child model session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionSpawnRequest {
    /// Active host request that supplies the authoritative resource owner.
    pub parent_request_id: u64,
    /// Unique task label under the calling owner.
    pub task_name: String,
    /// Initial task delivered to the child model session.
    pub message: String,
    /// Retry key scoped to this extension and resource owner.
    pub idempotency_key: String,
}

/// API `0.2` request carrying a child-session target and message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionMessageRequest {
    /// Active host request that supplies the authoritative resource owner.
    pub parent_request_id: u64,
    /// Agent ID or path returned by `agent/spawn`.
    pub target: String,
    /// Message or follow-up task to deliver.
    pub message: String,
}

/// API `0.2` request carrying only a child-session target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionTargetRequest {
    /// Active host request that supplies the authoritative resource owner.
    pub parent_request_id: u64,
    /// Agent ID or path returned by `agent/spawn`.
    pub target: String,
}

/// API `0.2` request to list sessions owned by the calling extension owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionListRequest {
    /// Active host request that supplies the authoritative resource owner.
    pub parent_request_id: u64,
}

/// API `0.2` request to wait for owned child-session state changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionWaitRequest {
    /// Active host request that supplies the authoritative resource owner.
    pub parent_request_id: u64,
    /// Bounded wait duration. Defaults to 30 seconds and is capped at 60.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionResourceOwner {
    /// Durable host-derived session identity. Extensions must treat this as
    /// the namespace for browser tabs, MCP/LSP state, and other handles.
    pub session_id: String,
    /// Host-created extension-process instance fence. It changes across a
    /// complete process-host rebuild even when generation numbering restarts.
    pub extension_instance_id: String,
    /// Process generation fence for rejecting stale resource operations after
    /// a restart or reload.
    pub process_generation: u64,
}

/// Ambient metadata supplied with commands, hooks, tools, and contributions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtensionExecutionContext {
    /// Active workspace root.
    pub workspace: PathBuf,
    /// Unique process-local tool execution scope, when invoked as a model tool.
    #[serde(default)]
    pub execution_scope: Option<String>,
    /// Durable extension-resource owner. Frozen API `0.1` omits this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_owner: Option<ExtensionResourceOwner>,
    /// Current host state.
    pub host: ExtensionHostState,
}

/// Result returned by an executable tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallOutput {
    /// Compact model-visible result text.
    pub content: String,
    /// Whether the result represents a tool failure.
    #[serde(default)]
    pub is_error: bool,
    /// Optional non-model metadata. Frozen API `0.1` returns it to direct API
    /// callers but discards it at the native tool/session bridge; negotiated
    /// API `0.2` validates and retains it.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Optional machine-readable API `0.2` result, validated against the
    /// tool's declared output schema before this value is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
    /// Canonical native result preserving ordered text/media/details.
    #[serde(skip)]
    native_output: Option<ToolOutput>,
}

impl PartialEq for ToolCallOutput {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
            && self.is_error == other.is_error
            && self.metadata == other.metadata
            && self.structured_content == other.structured_content
    }
}

impl ToolCallOutput {
    fn into_native(self) -> Result<ToolOutput, ExtensionRuntimeError> {
        if let Some(output) = self.native_output {
            return Ok(output);
        }
        ToolOutput::new(self.content)
            .try_with_details(self.structured_content, Some(self.metadata))
            .map_err(|error| {
                ExtensionRuntimeError::Protocol(format!("invalid tool output details: {error}"))
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallOutputWire {
    content: serde_json::Value,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    structured_content: PresentJsonValue,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Default)]
enum PresentJsonValue {
    #[default]
    Missing,
    Present(serde_json::Value),
}

impl PresentJsonValue {
    fn into_option(self) -> Option<serde_json::Value> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }
}

impl<'de> Deserialize<'de> for PresentJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ExtensionToolContentPart {
    Text {
        text: String,
    },
    Image {
        artifact_id: String,
        mime_type: String,
        #[serde(default, rename = "alt")]
        _alt: Option<String>,
    },
    Audio {
        artifact_id: String,
        mime_type: String,
        #[serde(default)]
        transcript: Option<String>,
    },
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
    /// Originating host request. Required for API `0.2` operation-scoped
    /// confirmations and absent from frozen API `0.1` frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<u64>,
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

/// Ephemeral input requested by an API `0.2` extension operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInputRequest {
    /// Originating host request whose cancellation owns this prompt.
    pub parent_request_id: u64,
    /// Short frontend-visible prompt. Secret answers never appear here.
    pub prompt: String,
    /// Whether the frontend should suppress echo and ordinary editor handling.
    #[serde(default)]
    pub secret: bool,
}

/// Host answer to an API `0.2` input request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionInputResponse {
    /// UTF-8 answer, or `null` when cancelled/unavailable.
    pub value: Option<String>,
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

/// Host-advertised limits for the API `0.2` initialization handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProtocolLimits {
    /// Maximum number of concurrently admitted host requests.
    pub max_concurrent_requests: usize,
}

/// Additive feature negotiation sent by an API `0.2` host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProtocolRequest {
    /// Exact framing version selected by the manifest.
    pub version: String,
    /// Features without which the extension cannot be registered.
    pub required_features: Vec<String>,
    /// Supported features the extension may elect to use.
    pub optional_features: Vec<String>,
    /// Host-capped transport limits.
    pub limits: ExtensionProtocolLimits,
}

/// Feature subset and accepted limits returned by an API `0.2` extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionProtocolResponse {
    /// Exact negotiated framing version.
    pub version: String,
    /// Advertised subset of the host's required and optional features.
    #[serde(default)]
    pub features: Vec<String>,
    /// Extension-accepted limits, still capped by the host.
    pub limits: ExtensionProtocolLimits,
    /// Lifecycle methods the extension wants to observe. Omitting the field
    /// while negotiating `lifecycle_events` subscribes to every lifecycle
    /// method for compatibility with minimal SDKs.
    #[serde(default)]
    pub lifecycle_events: Vec<String>,
}

/// Immutable protocol facts negotiated for one process generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionNegotiatedProtocol {
    /// Exact manifest-selected API version.
    pub version: String,
    /// Additive features supported by both peers.
    pub features: BTreeSet<String>,
    /// Effective host-capped concurrency limit.
    pub max_concurrent_requests: usize,
    /// Subscribed lifecycle wire methods.
    pub lifecycle_events: BTreeSet<String>,
}

impl ExtensionNegotiatedProtocol {
    fn api_0_1(limit: usize) -> Self {
        Self {
            version: EXTENSION_API_VERSION_0_1.to_owned(),
            features: BTreeSet::new(),
            max_concurrent_requests: limit,
            lifecycle_events: BTreeSet::new(),
        }
    }

    fn supports(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }
}

/// Request-scoped progress payload emitted by an API `0.2` extension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionProgressEvent {
    /// Human-readable progress with optional determinate units.
    Status {
        /// Bounded status text.
        message: String,
        /// Completed units, when known.
        #[serde(default)]
        current: Option<u64>,
        /// Total units, when known.
        #[serde(default)]
        total: Option<u64>,
        /// Unit label, for example `results`.
        #[serde(default)]
        unit: Option<String>,
    },
    /// Ephemeral stdout or stderr bytes.
    Output {
        /// Source stream.
        stream: ExtensionProgressStream,
        /// Text or binary encoding.
        encoding: ExtensionProgressEncoding,
        /// UTF-8 text or base64 data.
        data: String,
    },
}

/// Stream named by an extension progress event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionProgressStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Encoding of an extension progress output event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionProgressEncoding {
    /// Direct Unicode text.
    Utf8,
    /// RFC 4648 base64 bytes.
    Base64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionProgressNotification {
    request_id: u64,
    sequence: u64,
    event: ExtensionProgressEvent,
}

/// Outcome attached to settled API `0.2` lifecycle notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionLifecycleOutcome {
    /// Normal successful completion.
    Completed,
    /// Execution failed.
    Failed,
    /// User or parent cancellation won.
    Cancelled,
    /// Execution was interrupted without a cancellation acknowledgement.
    Interrupted,
    /// The owning frontend disconnected.
    FrontendDisconnected,
    /// Host shutdown settled the operation.
    Shutdown,
    /// A configured turn or resource limit was reached.
    LimitReached,
}

/// Observational lifecycle event sent best-effort to negotiated API `0.2`
/// subscribers. These notifications never replace host-owned finalizers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionLifecycleEvent {
    /// A host session became active.
    SessionStarted {
        /// Stable session ID.
        session_id: String,
        /// Stable run ID, when one already exists.
        #[serde(default)]
        run_id: Option<String>,
    },
    /// A host session reached its terminal boundary.
    SessionSettled {
        /// Stable session ID.
        session_id: String,
        /// Stable run ID, when applicable.
        #[serde(default)]
        run_id: Option<String>,
        /// Terminal outcome.
        outcome: ExtensionLifecycleOutcome,
        /// Elapsed duration in milliseconds.
        duration_ms: u64,
        /// Bounded inspectable reason.
        #[serde(default)]
        reason: Option<String>,
    },
    /// A model turn was admitted.
    TurnStarted {
        /// Stable session ID.
        session_id: String,
        /// Stable run ID.
        run_id: String,
        /// Stable turn ID.
        turn_id: String,
    },
    /// An admitted model turn settled exactly once.
    TurnSettled {
        /// Stable session ID.
        session_id: String,
        /// Stable run ID.
        run_id: String,
        /// Stable turn ID.
        turn_id: String,
        /// Terminal outcome.
        outcome: ExtensionLifecycleOutcome,
        /// Elapsed duration in milliseconds.
        duration_ms: u64,
        /// Bounded inspectable reason.
        #[serde(default)]
        reason: Option<String>,
    },
    /// A globally observed tool call started.
    ToolStarted {
        /// Stable session ID.
        session_id: String,
        /// Stable run ID.
        run_id: String,
        /// Stable turn ID.
        turn_id: String,
        /// Stable tool-call ID.
        tool_call_id: String,
        /// Registered tool name.
        tool_name: String,
    },
    /// A globally observed tool call settled.
    ToolSettled {
        /// Stable session ID.
        session_id: String,
        /// Stable run ID.
        run_id: String,
        /// Stable turn ID.
        turn_id: String,
        /// Stable tool-call ID.
        tool_call_id: String,
        /// Registered tool name.
        tool_name: String,
        /// Terminal outcome.
        outcome: ExtensionLifecycleOutcome,
        /// Elapsed duration in milliseconds.
        duration_ms: u64,
        /// Bounded inspectable reason.
        #[serde(default)]
        reason: Option<String>,
    },
}

impl ExtensionLifecycleEvent {
    fn method(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => methods::SESSION_STARTED,
            Self::SessionSettled { .. } => methods::SESSION_SETTLED,
            Self::TurnStarted { .. } => methods::TURN_STARTED,
            Self::TurnSettled { .. } => methods::TURN_SETTLED,
            Self::ToolStarted { .. } => methods::TOOL_STARTED,
            Self::ToolSettled { .. } => methods::TOOL_SETTLED,
        }
    }

    fn params(&self) -> Result<serde_json::Value, ExtensionRuntimeError> {
        let mut value = serde_json::to_value(self)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        let object = value.as_object_mut().ok_or_else(|| {
            ExtensionRuntimeError::Protocol("lifecycle event must serialize as an object".into())
        })?;
        object.remove("type");
        if object
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| reason.len() > MAX_LIFECYCLE_REASON_BYTES)
        {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "lifecycle reason exceeded {MAX_LIFECYCLE_REASON_BYTES} bytes"
            )));
        }
        Ok(value)
    }
}

/// Extension-originated request for host policy classification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPolicyEvaluationRequest {
    /// Originating host request whose lifetime owns this decision.
    pub parent_request_id: u64,
    /// Structured, non-authoritative proposed action.
    pub intent: ExtensionActionIntent,
    /// Single-use capability returned by a prior approved evaluation. Tokens
    /// are consumed against this exact intent, process generation, and parent.
    #[serde(default)]
    pub approval_token: Option<ExtensionApprovalToken>,
}

/// Host answer to a policy evaluation request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPolicyEvaluationResponse {
    /// Host-authoritative decision.
    pub decision: ExtensionPolicyDecision,
    /// Optional single-use, generation- and intent-bound approval capability.
    #[serde(default)]
    pub approval_token: Option<ExtensionApprovalToken>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionSecretGetRequest {
    parent_request_id: u64,
    name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactPublishRequest {
    parent_request_id: u64,
    mime_type: String,
    size: u64,
    sha256: String,
    #[serde(default)]
    data: Option<ArtifactInlineData>,
    #[serde(default)]
    path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactInlineData {
    encoding: ExtensionProgressEncoding,
    data: String,
}

/// Observable health state for one executable-extension process generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionHealthState {
    /// Process spawn is underway.
    Starting,
    /// The initialize handshake is underway.
    Initializing,
    /// New operations are accepted.
    Ready,
    /// New operations are rejected while admitted work settles.
    Draining,
    /// Graceful or forced shutdown completed.
    Stopped,
    /// A recoverable service failure occurred.
    Degraded,
    /// The child or transport failed unexpectedly.
    Crashed,
    /// A manager-imposed restart delay is active.
    Backoff,
    /// Permanent configuration, authorization, or protocol failure.
    Parked,
}

/// Bounded machine-readable health snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionHealthSnapshot {
    /// Current state.
    pub state: ExtensionHealthState,
    /// Owning process generation.
    pub generation: u64,
    /// Host requests which have not yet settled.
    pub pending_requests: usize,
    /// Last bounded transport or protocol error.
    #[serde(default)]
    pub last_error: Option<String>,
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
        /// Originating host request for API `0.2` correlation.
        #[serde(default)]
        parent_request_id: Option<u64>,
        /// Confirmation content.
        request: ConfirmationRequest,
    },
    /// Host-policy classification requested by an API `0.2` extension.
    PolicyEvaluationRequested {
        /// Process-originated JSON-RPC ID.
        request_id: ExtensionRequestId,
        /// Process generation that originated the request.
        generation: u64,
        /// Originating host request.
        parent_request_id: u64,
        /// Structured action intent.
        intent: ExtensionActionIntent,
    },
    /// Ephemeral frontend input requested by an API `0.2` extension.
    InputRequested {
        /// Process-originated JSON-RPC ID.
        request_id: ExtensionRequestId,
        /// Process generation that originated the request.
        generation: u64,
        /// Originating host request.
        parent_request_id: u64,
        /// Frontend-visible request without any answer value.
        request: ExtensionInputRequest,
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

/// Host-owned identity for one admitted extension operation.
///
/// JSON-RPC request IDs restart in every process generation, so consumers must
/// match both fields before handling an operation-scoped child request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionOperationToken {
    /// Process generation that admitted the operation.
    pub generation: u64,
    /// Host JSON-RPC request ID within that generation.
    pub parent_request_id: u64,
}

impl ExtensionOperationToken {
    /// Returns whether a child event belongs to this exact operation.
    pub fn owns(self, generation: u64, parent_request_id: u64) -> bool {
        self.generation == generation && self.parent_request_id == parent_request_id
    }
}

/// Runtime knobs for one executable extension process.
#[derive(Clone)]
pub struct ExtensionRuntimeConfig {
    /// Workspace used as the child working directory and execution context.
    pub workspace: PathBuf,
    /// Initial session/model/skill state.
    pub host_state: ExtensionHostState,
    /// Offer the optional host-owned child model-session service. The product
    /// must bind an enabled delegation runtime before the service is usable.
    pub agent_sessions: bool,
    /// Offer single-use approval redemption. A trusted frontend can issue a
    /// capability with [`ExtensionProcess::respond_to_policy_approval`].
    pub approvals: bool,
    /// Optional owner-scoped secret provider. The `secrets` feature is offered
    /// only when this is configured and the manifest declares secret names.
    /// The broker must not strongly retain this extension process.
    pub secret_broker: Option<Arc<dyn ExtensionSecretBroker>>,
    /// Maximum duration of one request.
    pub request_timeout: Duration,
    /// Per-stage shutdown timeout, applied once to the shutdown request/ack and
    /// again while waiting for the child to exit during shutdown or reload.
    pub shutdown_timeout: Duration,
    /// Maximum serialized JSON line size.
    pub max_message_bytes: usize,
    /// Maximum concurrent requests to one extension.
    pub max_pending_requests: usize,
    /// Maximum complete frames awaiting the dedicated serialized writer.
    pub writer_queue_capacity: usize,
    /// Grace allowed after cooperative cancellation before a non-responsive
    /// extension generation is force-terminated.
    pub cancellation_grace: Duration,
    /// Retention window for cancelled request IDs and late-reply diagnosis.
    pub tombstone_ttl: Duration,
}

impl std::fmt::Debug for ExtensionRuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionRuntimeConfig")
            .field("workspace", &self.workspace)
            .field("host_state", &self.host_state)
            .field("agent_sessions", &self.agent_sessions)
            .field("approvals", &self.approvals)
            .field("secret_broker_configured", &self.secret_broker.is_some())
            .field("request_timeout", &self.request_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("max_message_bytes", &self.max_message_bytes)
            .field("max_pending_requests", &self.max_pending_requests)
            .field("writer_queue_capacity", &self.writer_queue_capacity)
            .field("cancellation_grace", &self.cancellation_grace)
            .field("tombstone_ttl", &self.tombstone_ttl)
            .finish()
    }
}

impl ExtensionRuntimeConfig {
    /// Creates a runtime configuration with conservative bounded defaults.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            host_state: ExtensionHostState::default(),
            agent_sessions: false,
            approvals: false,
            secret_broker: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            max_message_bytes: DEFAULT_EXTENSION_MESSAGE_BYTES,
            max_pending_requests: DEFAULT_PENDING_REQUESTS,
            writer_queue_capacity: DEFAULT_WRITER_QUEUE,
            cancellation_grace: DEFAULT_CANCELLATION_GRACE,
            tombstone_ttl: DEFAULT_TOMBSTONE_TTL,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct OfferedHostServices {
    agent_sessions: bool,
    approvals: bool,
    secrets: bool,
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
    /// A request was cooperatively cancelled before a terminal response.
    #[error("extension request `{method}` cancelled: {reason}")]
    Cancelled {
        /// JSON-RPC method.
        method: String,
        /// Inspectable terminal reason.
        reason: String,
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
    /// Idempotent cancellation of a host or extension-originated request.
    pub const CANCEL_REQUEST: &str = "$/cancelRequest";
    /// Request-scoped ephemeral progress.
    pub const PROGRESS: &str = "$/progress";
    /// Extension-to-host user notification.
    pub const NOTIFICATION: &str = "notification";
    /// Extension-to-host interactive confirmation request.
    pub const CONFIRMATION_REQUEST: &str = "confirmation/request";
    /// Extension-to-host unsolicited prompt context.
    pub const CONTEXT_CONTRIBUTION: &str = "context/contribution";
    /// Extension-to-host unsolicited semantic UI contribution.
    pub const STATUS_CONTRIBUTION: &str = "status/contribution";
    /// Extension-to-host structured policy intent.
    pub const POLICY_EVALUATE: &str = "policy/evaluate";
    /// Extension-to-host ephemeral input request.
    pub const INPUT_REQUEST: &str = "input/request";
    /// Extension-to-host bounded artifact ingestion request.
    pub const ARTIFACT_PUBLISH: &str = "artifact/publish";
    /// Extension-to-host owner-scoped secret lookup.
    pub const SECRET_GET: &str = "secret/get";
    /// Extension-to-host live tool registration request.
    pub const TOOLS_REGISTER: &str = "tools/register";
    /// Extension-to-host live tool removal request.
    pub const TOOLS_UNREGISTER: &str = "tools/unregister";
    /// Extension request to create one host-owned child model session.
    pub const AGENT_SPAWN: &str = "agent/spawn";
    /// Extension request to send steering input to an owned child session.
    pub const AGENT_MESSAGE: &str = "agent/message";
    /// Extension request to queue a follow-up task on an owned child session.
    pub const AGENT_FOLLOW_UP: &str = "agent/follow_up";
    /// Extension request to inspect owned child sessions.
    pub const AGENT_LIST: &str = "agent/list";
    /// Extension request to wait for owned child-session state changes.
    pub const AGENT_WAIT: &str = "agent/wait";
    /// Extension request to interrupt an owned child-session tree.
    pub const AGENT_INTERRUPT: &str = "agent/interrupt";
    /// Observational session start.
    pub const SESSION_STARTED: &str = "session/started";
    /// Observational session terminal boundary.
    pub const SESSION_SETTLED: &str = "session/settled";
    /// Observational turn start.
    pub const TURN_STARTED: &str = "turn/started";
    /// Observational turn terminal boundary.
    pub const TURN_SETTLED: &str = "turn/settled";
    /// Observational global tool start.
    pub const TOOL_STARTED: &str = "tool/started";
    /// Observational global tool terminal boundary.
    pub const TOOL_SETTLED: &str = "tool/settled";
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
    /// Additive API `0.2` feature and limit negotiation. Frozen API `0.1`
    /// initialization omits this field byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ExtensionProtocolRequest>,
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
    /// Negotiated API `0.2` features and limits. API `0.1` must omit it.
    #[serde(default)]
    pub protocol: Option<ExtensionProtocolResponse>,
}

/// Host-to-extension tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Tool name.
    pub name: String,
    /// Model-produced arguments.
    pub arguments: serde_json::Value,
    /// Frozen live-catalog revision used to resolve the handler. Present only
    /// for API `0.2` extensions that negotiated `dynamic_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<u64>,
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

enum CatalogMutation {
    Register(Vec<ToolDefinition>),
    Unregister(Vec<String>),
}

struct CatalogUpdateRequest {
    request_id: ExtensionRequestId,
    generation: u64,
    mutation: CatalogMutation,
    catalog: Arc<StdRwLock<Vec<ToolDefinition>>>,
    writer: mpsc::Sender<WriterFrame>,
    child_requests: ChildRequests,
    max_message_bytes: usize,
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

struct AnsweredConfirmationReservation<'a> {
    answered: &'a StdMutex<AnsweredConfirmations>,
    generation: u64,
    request_id: ExtensionRequestId,
    committed: bool,
}

impl AnsweredConfirmationReservation<'_> {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for AnsweredConfirmationReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            lock_std_mutex(self.answered).remove(self.generation, &self.request_id);
        }
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
    next_generation: AtomicU64,
    instance_id: String,
    generation_changed: Arc<Notify>,
    reload_guard: Mutex<()>,
    supervisor_cancelled: AtomicBool,
    artifact_store: ArtifactStore,
    approval_store: Arc<ExtensionApprovalStore>,
    lifecycle: StdMutex<ActiveLifecycleState>,
    dynamic_tool_registration: StdMutex<Option<DynamicToolRegistration>>,
    dynamic_tool_registration_ready: Notify,
    delegation_service: Arc<StdRwLock<Option<ExtensionDelegationService>>>,
    catalog_updates: mpsc::Sender<CatalogUpdateRequest>,
}

/// Stable IDs attached to global tool lifecycle observations for the active
/// model turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionLifecycleTurnContext {
    /// Stable session ID.
    pub session_id: String,
    /// Stable run ID.
    pub run_id: String,
    /// Stable turn ID.
    pub turn_id: String,
}

#[derive(Default)]
struct ActiveLifecycleState {
    sessions: HashMap<String, ActiveLifecycleSession>,
    turns: HashMap<String, ActiveLifecycleTurn>,
    tools: HashMap<(String, String), ActiveLifecycleTool>,
}

#[derive(Clone)]
struct LifecycleEndpoint {
    generation: u64,
    connection: Arc<ProcessConnection>,
}

#[derive(Clone)]
struct ActiveLifecycleTurn {
    context: ExtensionLifecycleTurnContext,
    started_at: Instant,
    endpoint: LifecycleEndpoint,
    start_queued: bool,
}

struct ActiveLifecycleTool {
    name: String,
    started_at: Instant,
    context: ExtensionLifecycleTurnContext,
    endpoint: LifecycleEndpoint,
}

#[derive(Clone)]
struct ActiveLifecycleSession {
    session_id: String,
    run_id: Option<String>,
    started_at: Instant,
    endpoint: LifecycleEndpoint,
}

fn candidate_event_requires_host_response(event: &ExtensionEvent) -> bool {
    matches!(
        event,
        ExtensionEvent::ConfirmationRequested { .. }
            | ExtensionEvent::PolicyEvaluationRequested { .. }
            | ExtensionEvent::InputRequested { .. }
    )
}

async fn forward_candidate_events(
    inner: Weak<ExtensionProcessInner>,
    generation: u64,
    candidate: Weak<ProcessConnection>,
    mut events: broadcast::Receiver<ExtensionEvent>,
) {
    let mut deferred_requests = VecDeque::new();
    loop {
        let Some(current) = inner.upgrade() else {
            return;
        };
        let generation_changed = Arc::clone(&current.generation_changed);
        let public_events = current.events.clone();
        drop(current);
        let generation_changed = generation_changed.notified();
        tokio::pin!(generation_changed);
        generation_changed.as_mut().enable();
        let active = inner
            .upgrade()
            .is_some_and(|current| current.generation.load(Ordering::Acquire) == generation);
        if active {
            while let Some(event) = deferred_requests.pop_front() {
                let _ = public_events.send(event);
            }
        }
        let received = tokio::select! {
            event = events.recv() => Some(event),
            _ = &mut generation_changed => None,
        };
        let Some(received) = received else {
            continue;
        };
        let active = inner
            .upgrade()
            .is_some_and(|current| current.generation.load(Ordering::Acquire) == generation);
        match received {
            Ok(event) if active => {
                let _ = public_events.send(event);
            }
            Ok(event) if candidate_event_requires_host_response(&event) => {
                if deferred_requests.len() >= MAX_CHILD_WORKERS {
                    if let Some(candidate) = candidate.upgrade() {
                        candidate.terminate().await;
                    }
                    return;
                }
                deferred_requests.push_back(event);
            }
            Ok(_) => {
                // Candidate notifications, UI/context contributions, and
                // diagnostics are not observable until generation cutover.
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                if active {
                    let _ = public_events.send(ExtensionEvent::Diagnostic {
                        message: format!(
                            "extension event stream dropped {count} candidate event(s)"
                        ),
                    });
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn clear_matching_lifecycle_turn(
    lifecycle: &mut ActiveLifecycleState,
    resource_owner: Option<&str>,
    turn_id: &str,
) {
    lifecycle.turns.retain(|owner, turn| {
        resource_owner.is_some_and(|expected| expected != owner) || turn.context.turn_id != turn_id
    });
    lifecycle.tools.retain(|(owner, _), tool| {
        resource_owner.is_some_and(|expected| expected != owner) || tool.context.turn_id != turn_id
    });
}

impl ExtensionProcess {
    /// Launches, initializes, and validates an explicitly enabled and trusted
    /// executable extension.
    pub async fn start(
        descriptor: DiscoveredExtension,
        config: ExtensionRuntimeConfig,
    ) -> Result<Self, ExtensionRuntimeError> {
        descriptor.ensure_startable()?;
        if config.max_message_bytes == 0
            || config.max_pending_requests == 0
            || config.writer_queue_capacity == 0
            || config.cancellation_grace.is_zero()
            || config.tombstone_ttl.is_zero()
        {
            return Err(ExtensionRuntimeError::Protocol(
                "message, request, writer, cancellation, and tombstone limits must be greater than zero"
                    .into(),
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
        let artifact_store =
            ArtifactStore::new().map_err(|error| ExtensionRuntimeError::Spawn {
                extension: descriptor.manifest.name.clone(),
                message: format!("cannot create artifact store: {error}"),
            })?;
        let approval_store = Arc::new(ExtensionApprovalStore::new());
        let (catalog_updates, catalog_update_rx) = mpsc::channel(DYNAMIC_CATALOG_QUEUE_CAPACITY);
        let delegation_service = Arc::new(StdRwLock::new(None));
        let generation = 1;
        let (connection, contributions) = spawn_connection(
            &descriptor,
            &config,
            config.host_state.clone(),
            generation,
            events.clone(),
            artifact_store.clone(),
            catalog_updates.clone(),
            Arc::clone(&delegation_service),
            Arc::clone(&approval_store),
        )
        .await?;
        let process = Self {
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
                next_generation: AtomicU64::new(generation.saturating_add(1)),
                instance_id: new_extension_instance_id(),
                generation_changed: Arc::new(Notify::new()),
                reload_guard: Mutex::new(()),
                supervisor_cancelled: AtomicBool::new(false),
                artifact_store,
                approval_store,
                lifecycle: StdMutex::new(ActiveLifecycleState::default()),
                dynamic_tool_registration: StdMutex::new(None),
                dynamic_tool_registration_ready: Notify::new(),
                delegation_service,
                catalog_updates,
            }),
        };
        tokio::spawn(run_catalog_updates(
            Arc::downgrade(&process.inner),
            catalog_update_rx,
        ));
        tokio::spawn(supervise_extension(Arc::downgrade(&process.inner)));
        Ok(process)
    }

    /// Returns the discovered manifest and activation metadata.
    pub fn descriptor(&self) -> &DiscoveredExtension {
        &self.inner.descriptor
    }

    /// Returns the contributions negotiated during initialization.
    pub fn contributions(&self) -> &ExtensionContributions {
        &self.inner.contributions
    }

    /// Returns the active generation's complete live tool catalog.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let connection = read_std_lock(&self.inner.connection);
        let _catalog = read_std_lock(&connection.catalog_guard);
        let tools = read_std_lock(&connection.tool_catalog).clone();
        tools
    }

    /// Returns the exact manifest-selected protocol version.
    pub fn api_version(&self) -> &str {
        &self.inner.descriptor.manifest.api_version
    }

    /// Returns immutable feature, limit, and lifecycle negotiation for the
    /// active process generation.
    pub fn negotiated_protocol(&self) -> ExtensionNegotiatedProtocol {
        let connection = read_std_lock(&self.inner.connection);
        let protocol = read_std_lock(&connection.protocol).clone();
        protocol
    }

    /// Returns the active generation's negotiated additive feature set.
    pub fn negotiated_features(&self) -> BTreeSet<String> {
        self.negotiated_protocol().features
    }

    pub(crate) fn bind_agent_session_service(
        &self,
        service: ExtensionDelegationService,
    ) -> Result<(), ExtensionRuntimeError> {
        if !self
            .negotiated_protocol()
            .supports(EXTENSION_FEATURE_AGENT_SESSIONS)
        {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "extension `{}` did not negotiate `{EXTENSION_FEATURE_AGENT_SESSIONS}`",
                self.inner.descriptor.manifest.name
            )));
        }
        *write_std_lock(&self.inner.delegation_service) = Some(service);
        Ok(())
    }

    pub(crate) fn agent_session_principal(&self) -> String {
        format!(
            "{}@{}",
            self.inner.descriptor.manifest.name,
            self.inner.descriptor.manifest_path.display()
        )
    }

    /// Returns an inspectable bounded health snapshot for the active process.
    pub fn health_snapshot(&self) -> ExtensionHealthSnapshot {
        let connection = read_std_lock(&self.inner.connection);
        let health = read_std_lock(&connection.health);
        let pending_requests = lock_std_mutex(&connection.pending).len();
        ExtensionHealthSnapshot {
            state: health.state,
            generation: connection.generation,
            pending_requests,
            last_error: health.last_error.clone(),
        }
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
        mut context: ExtensionExecutionContext,
    ) -> Result<ToolCallOutput, ExtensionRuntimeError> {
        let name = name.into();
        let connection = read_std_lock(&self.inner.connection).clone();
        let _catalog = read_std_lock(&connection.catalog_guard);
        let definition = self.require_tool(&connection, &name)?;
        let catalog_revision = read_std_lock(&connection.protocol)
            .supports(EXTENSION_FEATURE_DYNAMIC_TOOLS)
            .then(|| connection.catalog_revision.load(Ordering::Acquire));
        context.resource_owner = context.resource_owner.map(|owner| ExtensionResourceOwner {
            session_id: owner.session_id,
            extension_instance_id: self.inner.instance_id.clone(),
            process_generation: connection.generation,
        });
        let resource_owner = context.resource_owner.clone();
        let artifact_owner = resource_owner
            .as_ref()
            .map(|owner| owner.session_id.clone());
        let params = serde_json::to_value(ToolCallRequest {
            name,
            arguments,
            catalog_revision,
            context,
        })
        .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        drop(_catalog);
        let _artifact_lease = connection.acquire_artifact_lease();
        let result = connection
            .request_with_resource_owner(
                methods::TOOL_CALL,
                params,
                self.inner.config.request_timeout,
                resource_owner,
            )
            .await?;
        decode_tool_call_output(&connection, &definition, artifact_owner.as_deref(), result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_tool_controlled(
        &self,
        connection: Arc<ProcessConnection>,
        definition: ToolDefinition,
        catalog_revision: u64,
        arguments: serde_json::Value,
        mut context: ExtensionExecutionContext,
        cancellation: CancellationToken,
        progress: ToolProgressSink,
        request_started: oneshot::Sender<ExtensionOperationToken>,
    ) -> Result<ToolCallOutput, ExtensionRuntimeError> {
        let catalog_revision = read_std_lock(&connection.protocol)
            .supports(EXTENSION_FEATURE_DYNAMIC_TOOLS)
            .then_some(catalog_revision);
        context.resource_owner = context.resource_owner.map(|owner| ExtensionResourceOwner {
            session_id: owner.session_id,
            extension_instance_id: self.inner.instance_id.clone(),
            process_generation: connection.generation,
        });
        let resource_owner = context.resource_owner.clone();
        let artifact_owner = resource_owner
            .as_ref()
            .map(|owner| owner.session_id.clone());
        let params = serde_json::to_value(ToolCallRequest {
            name: definition.name.clone(),
            arguments,
            catalog_revision,
            context,
        })
        .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        let _artifact_lease = connection.acquire_artifact_lease();
        let result = connection
            .request_with_cancellation(
                methods::TOOL_CALL,
                params,
                self.inner.config.request_timeout,
                cancellation,
                progress,
                resource_owner,
                request_started,
            )
            .await?;
        decode_tool_call_output(&connection, &definition, artifact_owner.as_deref(), result)
    }

    /// Invokes a manifest-declared slash command.
    pub async fn execute_command(
        &self,
        name: impl Into<String>,
        arguments: Vec<String>,
        context: ExtensionExecutionContext,
    ) -> Result<CommandOutput, ExtensionRuntimeError> {
        self.execute_command_inner(name.into(), arguments, context, None)
            .await
    }

    /// Invokes a manifest-declared slash command and reports the exact
    /// generation-scoped operation identity once the request is admitted.
    pub async fn execute_command_controlled(
        &self,
        name: impl Into<String>,
        arguments: Vec<String>,
        context: ExtensionExecutionContext,
        request_started: oneshot::Sender<ExtensionOperationToken>,
    ) -> Result<CommandOutput, ExtensionRuntimeError> {
        self.execute_command_inner(name.into(), arguments, context, Some(request_started))
            .await
    }

    async fn execute_command_inner(
        &self,
        name: String,
        arguments: Vec<String>,
        context: ExtensionExecutionContext,
        request_started: Option<oneshot::Sender<ExtensionOperationToken>>,
    ) -> Result<CommandOutput, ExtensionRuntimeError> {
        if !self
            .inner
            .contributions
            .commands
            .iter()
            .any(|command| command.name == name)
        {
            return Err(self.undeclared("command", name));
        }
        self.request_typed_controlled(
            methods::COMMAND_EXECUTE,
            &CommandRequest {
                name,
                arguments,
                context,
            },
            request_started,
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
        let connection = read_std_lock(&self.inner.connection).clone();
        if generation != connection.generation {
            return Err(ExtensionRuntimeError::Closed(format!(
                "confirmation belongs to stale generation {generation}; current generation is {}",
                connection.generation
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
        let mut reservation = AnsweredConfirmationReservation {
            answered: &self.inner.answered_confirmations,
            generation,
            request_id: request_id.clone(),
            committed: false,
        };
        connection
            .send_child_response(request_id.clone(), &response)
            .await?;
        reservation.commit();
        Ok(())
    }

    /// Whether a frontend or tool-progress consumer already answered this
    /// request. Product event drains use this to avoid duplicate UI/actions.
    pub fn confirmation_answered(&self, request_id: &ExtensionRequestId, generation: u64) -> bool {
        lock_std_mutex(&self.inner.answered_confirmations).contains(generation, request_id)
    }

    /// Answers an extension-originated API `0.2` policy evaluation request.
    /// Classification and approval issuance remain host-owned; this method
    /// only sends the already-decided typed result to the matching generation.
    pub async fn respond_to_policy_evaluation(
        &self,
        request_id: ExtensionRequestId,
        generation: u64,
        response: ExtensionPolicyEvaluationResponse,
    ) -> Result<(), ExtensionRuntimeError> {
        if response.approval_token.is_some() {
            return Err(ExtensionRuntimeError::Protocol(
                "approval capabilities must be issued by respond_to_policy_approval".into(),
            ));
        }
        let connection = read_std_lock(&self.inner.connection).clone();
        if generation != connection.generation {
            return Err(ExtensionRuntimeError::Closed(format!(
                "policy request belongs to stale generation {generation}; current generation is {}",
                connection.generation
            )));
        }
        if !read_std_lock(&connection.protocol).supports("policy_intents") {
            return Err(ExtensionRuntimeError::Protocol(
                "extension did not negotiate policy_intents".into(),
            ));
        }
        connection.send_child_response(request_id, &response).await
    }

    /// Resolves an interactive policy prompt and, when approved, issues a
    /// short-lived single-use capability bound to the exact intent, process
    /// generation, and originating parent request.
    ///
    /// An approved response remains `ask` and carries the capability. The
    /// extension must repeat `policy/evaluate` with that token; the host then
    /// atomically consumes it and returns `allow`. This keeps approval
    /// consumption on the operation boundary instead of treating a UI answer
    /// as authority forever.
    pub async fn respond_to_policy_approval(
        &self,
        request_id: ExtensionRequestId,
        generation: u64,
        parent_request_id: u64,
        approved: bool,
        ttl: Duration,
    ) -> Result<(), ExtensionRuntimeError> {
        let connection = read_std_lock(&self.inner.connection).clone();
        if generation != connection.generation {
            return Err(ExtensionRuntimeError::Closed(format!(
                "policy request belongs to stale generation {generation}; current generation is {}",
                connection.generation
            )));
        }
        let supports_approvals = {
            let protocol = read_std_lock(&connection.protocol);
            protocol.supports(EXTENSION_FEATURE_POLICY_INTENTS)
                && protocol.supports(EXTENSION_FEATURE_APPROVALS)
        };
        if !supports_approvals {
            return Err(ExtensionRuntimeError::Protocol(
                "extension did not negotiate policy_intents and approvals".into(),
            ));
        }
        let requested_intent = {
            let children = lock_std_mutex(&connection.child_requests);
            children
                .get(&request_id)
                .filter(|child| {
                    child.parent_request_id == parent_request_id
                        && child.response_state.state.load(Ordering::Acquire) == CHILD_ACTIVE
                })
                .and_then(|child| child.policy_intent.clone())
        };
        let parent_has_owner = {
            let pending = lock_std_mutex(&connection.pending);
            pending
                .get(&parent_request_id)
                .is_some_and(|parent| parent.resource_owner.is_some())
        };
        let Some(intent) = requested_intent else {
            return Err(ExtensionRuntimeError::Closed(
                "policy approval no longer belongs to its original active intent".into(),
            ));
        };
        if !parent_has_owner {
            return Err(ExtensionRuntimeError::Closed(
                "policy approval no longer belongs to an active owner-scoped request".into(),
            ));
        }
        if !approved {
            return connection
                .send_child_response(
                    request_id,
                    &ExtensionPolicyEvaluationResponse {
                        decision: ExtensionPolicyDecision::Deny,
                        approval_token: None,
                    },
                )
                .await;
        }
        let parent = ExtensionRequestId::Number(parent_request_id);
        let token = self
            .inner
            .approval_store
            .issue(&intent, generation, parent.clone(), ttl)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        let response = ExtensionPolicyEvaluationResponse {
            decision: ExtensionPolicyDecision::Ask,
            approval_token: Some(token.clone()),
        };
        let sent = connection
            .send_child_response_admitted(request_id, &response)
            .await;
        if !matches!(sent, Ok(ChildResponseAdmission::Queued)) {
            let _ = self
                .inner
                .approval_store
                .consume(&token, &intent, generation, &parent);
        }
        sent.map(|_| ())
    }

    /// Answers an extension-originated API `0.2` ephemeral input request.
    /// A `None` value is the deterministic cancellation/no-frontend answer.
    pub async fn respond_to_input(
        &self,
        request_id: ExtensionRequestId,
        generation: u64,
        response: ExtensionInputResponse,
    ) -> Result<(), ExtensionRuntimeError> {
        request_id
            .validate_confirmation_id()
            .map_err(ExtensionRuntimeError::Protocol)?;
        if response
            .value
            .as_ref()
            .is_some_and(|value| value.len() > MAX_EXTENSION_INPUT_VALUE_BYTES)
        {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "input response exceeded {MAX_EXTENSION_INPUT_VALUE_BYTES} UTF-8 bytes"
            )));
        }
        let connection = read_std_lock(&self.inner.connection).clone();
        if generation != connection.generation {
            return Err(ExtensionRuntimeError::Closed(format!(
                "input request belongs to stale generation {generation}; current generation is {}",
                connection.generation
            )));
        }
        if read_std_lock(&connection.protocol).version != EXTENSION_API_VERSION_0_2 {
            return Err(ExtensionRuntimeError::Protocol(
                "input/request requires API 0.2".into(),
            ));
        }
        connection.send_child_response(request_id, &response).await
    }

    /// Sends a non-veto lifecycle observation to a subscribed API `0.2`
    /// extension. Non-negotiated events are successful no-ops.
    pub async fn notify_lifecycle(
        &self,
        event: &ExtensionLifecycleEvent,
    ) -> Result<(), ExtensionRuntimeError> {
        let connection = read_std_lock(&self.inner.connection).clone();
        let current = LifecycleEndpoint {
            generation: connection.generation,
            connection,
        };
        let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
        if current.connection.draining.load(Ordering::Acquire)
            && matches!(
                event,
                ExtensionLifecycleEvent::SessionStarted { .. }
                    | ExtensionLifecycleEvent::TurnStarted { .. }
            )
        {
            return Ok(());
        }
        if current.connection.draining.load(Ordering::Acquire)
            && lifecycle.sessions.is_empty()
            && lifecycle.turns.is_empty()
            && lifecycle.tools.is_empty()
        {
            return Ok(());
        }
        let mut skip_delivery = false;
        let endpoint = match event {
            ExtensionLifecycleEvent::SessionSettled { session_id, .. } => lifecycle
                .sessions
                .get(session_id)
                .map(|session| session.endpoint.clone())
                .unwrap_or_else(|| current.clone()),
            ExtensionLifecycleEvent::TurnStarted { turn_id, .. } => {
                if let Some(turn) = lifecycle
                    .turns
                    .values()
                    .find(|turn| turn.context.turn_id == *turn_id)
                {
                    if turn.start_queued {
                        return Ok(());
                    }
                    turn.endpoint.clone()
                } else {
                    current.clone()
                }
            }
            ExtensionLifecycleEvent::TurnSettled { turn_id, .. } => {
                if let Some(turn) = lifecycle
                    .turns
                    .values()
                    .find(|turn| turn.context.turn_id == *turn_id)
                {
                    if !turn.start_queued {
                        skip_delivery = true;
                    }
                    turn.endpoint.clone()
                } else {
                    current.clone()
                }
            }
            _ => current,
        };
        let delivery = if skip_delivery {
            Ok(false)
        } else {
            Self::queue_lifecycle_observation(&endpoint, event.clone())
        };
        let queued = matches!(delivery, Ok(true));
        match event {
            ExtensionLifecycleEvent::SessionStarted { session_id, run_id } if queued => {
                lifecycle.sessions.insert(
                    session_id.clone(),
                    ActiveLifecycleSession {
                        session_id: session_id.clone(),
                        run_id: run_id.clone(),
                        started_at: Instant::now(),
                        endpoint,
                    },
                );
            }
            ExtensionLifecycleEvent::SessionSettled { session_id, .. } => {
                lifecycle.sessions.remove(session_id);
            }
            ExtensionLifecycleEvent::TurnStarted { turn_id, .. } if queued => {
                if let Some(turn) = lifecycle
                    .turns
                    .values_mut()
                    .find(|turn| turn.context.turn_id == *turn_id)
                {
                    turn.start_queued = true;
                }
            }
            ExtensionLifecycleEvent::TurnSettled { turn_id, .. } => {
                clear_matching_lifecycle_turn(&mut lifecycle, None, turn_id);
            }
            _ => {}
        }
        delivery.map(|_| ())
    }

    /// Sets the stable turn IDs used by synchronous global tool observation.
    /// Product code must clear this at the same terminal boundary that emits
    /// `turn/settled`.
    pub fn set_active_lifecycle_turn(
        &self,
        resource_owner: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) {
        let connection = read_std_lock(&self.inner.connection).clone();
        if connection.draining.load(Ordering::Acquire) {
            return;
        }
        let endpoint = LifecycleEndpoint {
            generation: connection.generation,
            connection,
        };
        let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
        if endpoint.connection.draining.load(Ordering::Acquire) {
            return;
        }
        let resource_owner = resource_owner.into();
        lifecycle.turns.insert(
            resource_owner.clone(),
            ActiveLifecycleTurn {
                context: ExtensionLifecycleTurnContext {
                    session_id: session_id.into(),
                    run_id: run_id.into(),
                    turn_id: turn_id.into(),
                },
                started_at: Instant::now(),
                endpoint,
                start_queued: false,
            },
        );
        lifecycle
            .tools
            .retain(|(owner, _), _| owner != &resource_owner);
    }

    /// Clears active turn IDs and any unmatched observed tool starts.
    pub fn clear_active_lifecycle_turn(&self, resource_owner: &str, turn_id: &str) {
        let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
        clear_matching_lifecycle_turn(&mut lifecycle, Some(resource_owner), turn_id);
    }

    fn queue_lifecycle_observation(
        endpoint: &LifecycleEndpoint,
        event: ExtensionLifecycleEvent,
    ) -> Result<bool, ExtensionRuntimeError> {
        let method = event.method();
        let protocol = read_std_lock(&endpoint.connection.protocol).clone();
        if !protocol.supports(EXTENSION_FEATURE_LIFECYCLE_EVENTS)
            || !protocol.lifecycle_events.contains(method)
        {
            return Ok(false);
        }
        let params = event.params()?;
        if !endpoint.connection.queue_notification(method, params) {
            return Err(ExtensionRuntimeError::Closed(format!(
                "unable to queue lifecycle notification `{method}` for generation {}",
                endpoint.generation
            )));
        }
        Ok(true)
    }

    /// Marks the active generation draining and rejects all new dispatch.
    /// Returns `true` only for the transition which won.
    pub fn begin_drain(&self) -> bool {
        read_std_lock(&self.inner.connection).begin_drain()
    }

    /// Allows admitted operations to settle until `deadline`, then cancels
    /// any remainder without replaying them.
    pub async fn drain(&self, deadline: Duration) -> bool {
        let connection = read_std_lock(&self.inner.connection).clone();
        connection.drain(deadline).await
    }

    /// Restarts the process and atomically swaps it in after a successful
    /// handshake. API `0.2` extensions negotiating `dynamic_tools` may publish
    /// a different tool catalog; frozen/static contribution surfaces must
    /// remain compatible.
    pub async fn reload(&self) -> Result<ExtensionReloadReport, ExtensionRuntimeError> {
        let _guard = self.inner.reload_guard.lock().await;
        self.reload_locked(None).await
    }

    async fn reload_locked(
        &self,
        expected_generation: Option<u64>,
    ) -> Result<ExtensionReloadReport, ExtensionRuntimeError> {
        if self.inner.supervisor_cancelled.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeError::Closed(format!(
                "extension `{}` is shutting down",
                self.inner.descriptor.manifest.name
            )));
        }
        let active_generation = read_std_lock(&self.inner.connection).generation;
        if expected_generation.is_some_and(|expected| expected != active_generation) {
            return Err(ExtensionRuntimeError::Closed(format!(
                "extension generation changed before restart (expected {}, active {active_generation})",
                expected_generation.unwrap_or_default()
            )));
        }
        let generation = self
            .inner
            .next_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(1)
            })
            .map_err(|_| {
                ExtensionRuntimeError::Protocol(
                    "extension process generation space is exhausted".to_owned(),
                )
            })?;
        let host_state = read_std_lock(&self.inner.host_state).clone();
        let (candidate_events, candidate_event_rx) = broadcast::channel(EXTENSION_EVENT_CAPACITY);
        let (replacement, contributions) = spawn_connection(
            &self.inner.descriptor,
            &self.inner.config,
            host_state,
            generation,
            candidate_events,
            self.inner.artifact_store.clone(),
            self.inner.catalog_updates.clone(),
            Arc::clone(&self.inner.delegation_service),
            Arc::clone(&self.inner.approval_store),
        )
        .await?;
        tokio::spawn(forward_candidate_events(
            Arc::downgrade(&self.inner),
            generation,
            Arc::downgrade(&replacement),
            candidate_event_rx,
        ));
        let current = read_std_lock(&self.inner.connection).clone();
        let current_dynamic_tools =
            read_std_lock(&current.protocol).supports(EXTENSION_FEATURE_DYNAMIC_TOOLS);
        let replacement_dynamic_tools =
            read_std_lock(&replacement.protocol).supports(EXTENSION_FEATURE_DYNAMIC_TOOLS);
        if current_dynamic_tools != replacement_dynamic_tools {
            replacement.terminate().await;
            let _ = self.inner.artifact_store.settle_generation(generation);
            return Err(ExtensionRuntimeError::ReloadRequiresReregistration {
                extension: self.inner.descriptor.manifest.name.clone(),
            });
        }
        let dynamic_tools = current_dynamic_tools;
        if !contributions_compatible(&self.inner.contributions, &contributions, dynamic_tools) {
            replacement.terminate().await;
            let _ = self.inner.artifact_store.settle_generation(generation);
            return Err(ExtensionRuntimeError::ReloadRequiresReregistration {
                extension: self.inner.descriptor.manifest.name.clone(),
            });
        }
        let replacement_tools = read_std_lock(&replacement.tool_catalog).clone();
        let dynamic_registration = lock_std_mutex(&self.inner.dynamic_tool_registration).clone();
        let mut replacement_reservation = if let Some(registration) = &dynamic_registration {
            let replacement_process_tools =
                self.process_tools(Arc::clone(&replacement), &replacement_tools);
            let revision = Arc::clone(&replacement_process_tools.revision);
            match registration.reserve(replacement_process_tools.tools) {
                Ok(reservation) => Some((reservation, revision)),
                Err(message) => {
                    replacement.terminate().await;
                    let _ = self.inner.artifact_store.settle_generation(generation);
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "replacement tool catalog was rejected: {message}"
                    )));
                }
            }
        } else {
            None
        };
        if !connection_is_usable(&replacement) {
            replacement.terminate().await;
            let _ = self.inner.artifact_store.settle_generation(generation);
            return Err(ExtensionRuntimeError::Closed(format!(
                "replacement generation {generation} exited before reload cutover"
            )));
        }

        let previous = {
            let active = write_std_lock(&self.inner.connection);
            let previous = Arc::clone(&active);
            previous.begin_drain();
            previous
        };

        // Give already-admitted requests their bounded natural settlement
        // window before synthesizing interruption for lifecycle records that
        // remain active.
        let previous_quiesced = previous.quiesce(self.inner.config.shutdown_timeout).await;
        if !connection_is_usable(&replacement) {
            if previous_quiesced {
                previous.resume_after_failed_drain();
            } else {
                previous.cancel_all_pending("replacement exited during reload drain");
                if let Some(registration) = &dynamic_registration {
                    registration.remove();
                }
                let _ = previous.shutdown().await;
            }
            replacement.terminate().await;
            let _ = self.inner.artifact_store.settle_generation(generation);
            return Err(ExtensionRuntimeError::Closed(format!(
                "replacement generation {generation} exited while the active generation drained"
            )));
        }

        let (old_turns, old_sessions) = {
            let _active = write_std_lock(&self.inner.connection);
            let previous_generation = previous.generation;
            let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
            let reason = Some("extension generation reloaded".to_owned());

            let mut old_tools = Vec::new();
            let mut retained_tools = HashMap::new();
            for (tool_key, tool) in std::mem::take(&mut lifecycle.tools) {
                if tool.endpoint.generation == previous_generation {
                    old_tools.push((tool_key, tool));
                } else {
                    retained_tools.insert(tool_key, tool);
                }
            }
            old_tools.sort_by(|left, right| left.0.cmp(&right.0));
            lifecycle.tools = retained_tools;

            let mut old_turns = Vec::new();
            lifecycle.turns.retain(|owner, turn| {
                if turn.endpoint.generation == previous_generation {
                    old_turns.push((owner.clone(), turn.clone()));
                    false
                } else {
                    true
                }
            });
            old_turns.sort_by(|left, right| left.0.cmp(&right.0));
            let mut old_sessions = Vec::new();
            lifecycle.sessions.retain(|session_id, session| {
                if session.endpoint.generation == previous_generation {
                    old_sessions.push((session_id.clone(), session.clone()));
                    false
                } else {
                    true
                }
            });
            old_sessions.sort_by(|left, right| left.0.cmp(&right.0));

            for ((_, tool_call_id), tool) in old_tools {
                let _ = Self::queue_lifecycle_observation(
                    &tool.endpoint,
                    ExtensionLifecycleEvent::ToolSettled {
                        session_id: tool.context.session_id,
                        run_id: tool.context.run_id,
                        turn_id: tool.context.turn_id,
                        tool_call_id,
                        tool_name: tool.name,
                        outcome: ExtensionLifecycleOutcome::Interrupted,
                        duration_ms: u64::try_from(tool.started_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        reason: reason.clone(),
                    },
                );
            }
            for (_, turn) in old_turns.iter().filter(|(_, turn)| turn.start_queued) {
                let _ = Self::queue_lifecycle_observation(
                    &turn.endpoint,
                    ExtensionLifecycleEvent::TurnSettled {
                        session_id: turn.context.session_id.clone(),
                        run_id: turn.context.run_id.clone(),
                        turn_id: turn.context.turn_id.clone(),
                        outcome: ExtensionLifecycleOutcome::Interrupted,
                        duration_ms: u64::try_from(turn.started_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        reason: reason.clone(),
                    },
                );
            }
            for (_, session) in &old_sessions {
                let _ = Self::queue_lifecycle_observation(
                    &session.endpoint,
                    ExtensionLifecycleEvent::SessionSettled {
                        session_id: session.session_id.clone(),
                        run_id: session.run_id.clone(),
                        outcome: ExtensionLifecycleOutcome::Interrupted,
                        duration_ms: u64::try_from(session.started_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        reason: reason.clone(),
                    },
                );
            }

            (old_turns, old_sessions)
        };

        // No await is allowed between the final candidate liveness check,
        // lifecycle transfer, connection swap, and catalog publication. This
        // keeps the cutover admission boundary synchronous. The old process is
        // shut down immediately after the new generation becomes authoritative.
        {
            let mut active = write_std_lock(&self.inner.connection);
            let replacement_endpoint = LifecycleEndpoint {
                generation,
                connection: Arc::clone(&replacement),
            };
            let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
            for (session_key, session) in old_sessions {
                let event = ExtensionLifecycleEvent::SessionStarted {
                    session_id: session.session_id.clone(),
                    run_id: session.run_id.clone(),
                };
                if Self::queue_lifecycle_observation(&replacement_endpoint, event).unwrap_or(false)
                {
                    lifecycle.sessions.insert(
                        session_key,
                        ActiveLifecycleSession {
                            session_id: session.session_id,
                            run_id: session.run_id,
                            started_at: Instant::now(),
                            endpoint: replacement_endpoint.clone(),
                        },
                    );
                }
            }
            for (owner, turn) in old_turns {
                let start_queued = Self::queue_lifecycle_observation(
                    &replacement_endpoint,
                    ExtensionLifecycleEvent::TurnStarted {
                        session_id: turn.context.session_id.clone(),
                        run_id: turn.context.run_id.clone(),
                        turn_id: turn.context.turn_id.clone(),
                    },
                )
                .unwrap_or(false);
                lifecycle.turns.insert(
                    owner,
                    ActiveLifecycleTurn {
                        context: turn.context,
                        started_at: Instant::now(),
                        endpoint: replacement_endpoint.clone(),
                        start_queued,
                    },
                );
            }

            *active = Arc::clone(&replacement);
            self.inner.generation.store(generation, Ordering::Release);
            self.inner.generation_changed.notify_waiters();
            let catalog_publication = replacement_reservation
                .take()
                .map(|(reservation, revision)| {
                    reservation.commit_with(|_, published| {
                        // Wire catalog epochs are local to one subprocess.
                        // A replacement starts at the SDK's initial epoch 0;
                        // the host-wide registry revision remains internal.
                        revision.store(0, Ordering::Release);
                        let _catalog = write_std_lock(&replacement.catalog_guard);
                        write_std_lock(&replacement.tool_catalog)
                            .retain(|definition| published.contains(&definition.name));
                        replacement.catalog_revision.store(0, Ordering::Release);
                    })
                })
                .unwrap_or_else(|| {
                    Ok((
                        0,
                        replacement_tools
                            .iter()
                            .map(|definition| definition.name.clone())
                            .collect(),
                    ))
                });
            if let Err(message) = &catalog_publication {
                let _catalog = write_std_lock(&replacement.catalog_guard);
                write_std_lock(&replacement.tool_catalog).clear();
                let _ = self.inner.events.send(ExtensionEvent::Diagnostic {
                    message: format!(
                        "replacement tool catalog could not be published after cutover: {message}"
                    ),
                });
            }
        }
        let previous_shutdown_graceful = previous.shutdown().await;
        self.inner
            .approval_store
            .invalidate_generation(previous.generation);
        lock_std_mutex(&self.inner.answered_confirmations).retain_generation(generation);
        Ok(ExtensionReloadReport {
            generation,
            previous_shutdown_graceful,
        })
    }

    /// Requests graceful shutdown using the configured per-stage timeout, waits
    /// for child exit using that timeout again, then kills it if needed. Returns
    /// whether it acknowledged and exited within their respective stages.
    pub async fn shutdown(&self) -> bool {
        self.inner
            .supervisor_cancelled
            .store(true, Ordering::Release);
        self.inner.generation_changed.notify_waiters();
        let _guard = self.inner.reload_guard.lock().await;
        let connection = read_std_lock(&self.inner.connection).clone();
        let _ = connection.drain(self.inner.config.shutdown_timeout).await;
        let graceful = connection.shutdown().await;
        self.inner
            .approval_store
            .invalidate_generation(connection.generation);
        if let Some(registration) = lock_std_mutex(&self.inner.dynamic_tool_registration).as_ref() {
            registration.remove();
        }
        if let Some(service) = write_std_lock(&self.inner.delegation_service).take() {
            service.shutdown_owned();
        }
        graceful
    }

    fn execution_context(&self) -> ExtensionExecutionContext {
        ExtensionExecutionContext {
            workspace: self.inner.config.workspace.clone(),
            execution_scope: None,
            resource_owner: None,
            host: read_std_lock(&self.inner.host_state).clone(),
        }
    }

    fn require_tool(
        &self,
        connection: &ProcessConnection,
        name: &str,
    ) -> Result<ToolDefinition, ExtensionRuntimeError> {
        read_std_lock(&connection.tool_catalog)
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
            .ok_or_else(|| self.undeclared("tool", name.to_owned()))
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
        self.request_typed_controlled(method, params, None).await
    }

    async fn request_typed_controlled<P, R>(
        &self,
        method: &'static str,
        params: &P,
        request_started: Option<oneshot::Sender<ExtensionOperationToken>>,
    ) -> Result<R, ExtensionRuntimeError>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let params = serde_json::to_value(params)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        let connection = read_std_lock(&self.inner.connection).clone();
        let result = match request_started {
            Some(request_started) => {
                connection
                    .request_with_operation(
                        method,
                        params,
                        self.inner.config.request_timeout,
                        request_started,
                    )
                    .await?
            }
            None => {
                connection
                    .request(method, params, self.inner.config.request_timeout)
                    .await?
            }
        };
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
        self.register_dynamic_tool_catalog(host);
        host.observe(self.clone());
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

impl ExtensionProcess {
    /// Attaches the live tool catalog before the process's ordered observer and
    /// hook registration. Product startup uses this narrow first phase so a
    /// fast child can publish while a slower sibling is still initializing;
    /// the later full [`Extension`] registration remains deterministic.
    pub fn register_dynamic_tool_catalog(&self, host: &mut ExtensionHost) {
        if lock_std_mutex(&self.inner.dynamic_tool_registration).is_some() {
            return;
        }
        let definitions = self.tool_definitions();
        let owner = format!(
            "{}@{}",
            self.inner.descriptor.manifest.name,
            self.inner.descriptor.manifest_path.display()
        );
        let connection = read_std_lock(&self.inner.connection).clone();
        let process_tools = self.process_tools(Arc::clone(&connection), &definitions);
        let published_connection = Arc::clone(&connection);
        match host.dynamic_tools_with(owner, process_tools.tools, move |_, published| {
            let _catalog = write_std_lock(&published_connection.catalog_guard);
            write_std_lock(&published_connection.tool_catalog)
                .retain(|definition| published.contains(&definition.name));
            published_connection
                .catalog_revision
                .store(0, Ordering::Release);
        }) {
            Ok(registration) => {
                *lock_std_mutex(&self.inner.dynamic_tool_registration) = Some(registration);
                self.inner.dynamic_tool_registration_ready.notify_waiters();
            }
            Err(error) => {
                host.duplicate_tools.push(error);
            }
        }
    }
}

impl ExtensionProcess {
    fn observe_agent_event(&self, event: &AgentEvent, resource_owner: Option<&str>) {
        match event {
            AgentEvent::ToolStarted { id, name, .. } => {
                let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
                let owner = resource_owner.map(str::to_owned).or_else(|| {
                    (lifecycle.turns.len() == 1)
                        .then(|| lifecycle.turns.keys().next().cloned())
                        .flatten()
                });
                let Some(owner) = owner else {
                    return;
                };
                let Some(turn) = lifecycle.turns.get(&owner).cloned() else {
                    return;
                };
                let active = ActiveLifecycleTool {
                    name: name.clone(),
                    started_at: Instant::now(),
                    context: turn.context.clone(),
                    endpoint: turn.endpoint.clone(),
                };
                let _ = Self::queue_lifecycle_observation(
                    &active.endpoint,
                    ExtensionLifecycleEvent::ToolStarted {
                        session_id: active.context.session_id.clone(),
                        run_id: active.context.run_id.clone(),
                        turn_id: active.context.turn_id.clone(),
                        tool_call_id: id.0.clone(),
                        tool_name: name.clone(),
                    },
                );
                lifecycle.tools.insert((owner, id.0.clone()), active);
            }
            AgentEvent::ToolFinished { id, result } => {
                let mut lifecycle = lock_std_mutex(&self.inner.lifecycle);
                let owner = resource_owner.map(str::to_owned).or_else(|| {
                    let mut owners = lifecycle
                        .tools
                        .keys()
                        .filter(|(_, tool_call_id)| tool_call_id == &id.0)
                        .map(|(owner, _)| owner.clone());
                    let first = owners.next();
                    (owners.next().is_none()).then_some(first).flatten()
                });
                let Some(owner) = owner else {
                    return;
                };
                let active = lifecycle.tools.remove(&(owner, id.0.clone()));
                let Some(active) = active else {
                    return;
                };
                let (outcome, reason) = match result {
                    Ok(output) if output.is_error() => {
                        (ExtensionLifecycleOutcome::Failed, Some(output.text.clone()))
                    }
                    Ok(_) => (ExtensionLifecycleOutcome::Completed, None),
                    Err(error) => (
                        ExtensionLifecycleOutcome::Failed,
                        Some(error.message.clone()),
                    ),
                };
                let reason = reason.map(|mut reason| {
                    truncate_utf8(&mut reason, MAX_LIFECYCLE_REASON_BYTES);
                    reason
                });
                let duration_ms =
                    u64::try_from(active.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                let _ = Self::queue_lifecycle_observation(
                    &active.endpoint,
                    ExtensionLifecycleEvent::ToolSettled {
                        session_id: active.context.session_id,
                        run_id: active.context.run_id,
                        turn_id: active.context.turn_id,
                        tool_call_id: id.0.clone(),
                        tool_name: active.name,
                        outcome,
                        duration_ms,
                        reason,
                    },
                );
            }
            _ => {}
        }
    }
}

impl EventObserver for ExtensionProcess {
    fn on_event(&self, event: &AgentEvent) {
        self.observe_agent_event(event, None);
    }

    fn on_event_for_owner(&self, event: &AgentEvent, resource_owner: &str) {
        self.observe_agent_event(event, Some(resource_owner));
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
                self.tool_execution_context(context, {
                    let connection = read_std_lock(&self.inner.connection);
                    connection.generation
                }),
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
                self.tool_execution_context(context, {
                    let connection = read_std_lock(&self.inner.connection);
                    connection.generation
                }),
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
    fn tool_execution_context(
        &self,
        context: &ToolContext<'_>,
        process_generation: u64,
    ) -> ExtensionExecutionContext {
        let mut execution = self.execution_context();
        execution.execution_scope = Some(context.execution_scope.to_owned());
        if self.api_version() == EXTENSION_API_VERSION_0_2 {
            execution.resource_owner = Some(ExtensionResourceOwner {
                session_id: context.resource_owner.to_owned(),
                extension_instance_id: self.inner.instance_id.clone(),
                process_generation,
            });
        }
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

    fn process_tools(
        &self,
        connection: Arc<ProcessConnection>,
        definitions: &[ToolDefinition],
    ) -> ProcessToolSet {
        let revision = Arc::new(AtomicU64::new(0));
        let tools = definitions
            .iter()
            .cloned()
            .map(|definition| {
                Arc::new(ProcessTool {
                    process: self.clone(),
                    connection: Arc::clone(&connection),
                    definition,
                    catalog_revision: Arc::clone(&revision),
                }) as Arc<dyn Tool>
            })
            .collect();
        ProcessToolSet { tools, revision }
    }
}

async fn wait_for_dynamic_registration(
    inner: &ExtensionProcessInner,
) -> Result<DynamicToolRegistration, String> {
    let timeout = inner.config.request_timeout;
    let registration = tokio::time::timeout(timeout, async {
        loop {
            let notified = inner.dynamic_tool_registration_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(registration) = lock_std_mutex(&inner.dynamic_tool_registration).clone() {
                return registration;
            }
            notified.await;
        }
    })
    .await
    .map_err(|_| "dynamic tool process was not registered with its host in time".to_owned())?;
    registration.wait_until_ready(timeout).await?;
    Ok(registration)
}

async fn run_catalog_updates(
    inner: Weak<ExtensionProcessInner>,
    mut updates: mpsc::Receiver<CatalogUpdateRequest>,
) {
    while let Some(update) = updates.recv().await {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let mut committed_catalog = None;
        let result = match wait_for_dynamic_registration(&inner).await {
            Err(message) => Err(message),
            Ok(registration) => {
                let _reload = inner.reload_guard.lock().await;
                let process = ExtensionProcess {
                    inner: Arc::clone(&inner),
                };
                let active = read_std_lock(&inner.connection).clone();
                let active_request = active.generation == update.generation
                    && Arc::ptr_eq(&active.tool_catalog, &update.catalog)
                    && !active.draining.load(Ordering::Acquire);
                if !active_request {
                    Err(
                        "tool catalog request belongs to an inactive extension generation"
                            .to_owned(),
                    )
                } else {
                    let mut next = read_std_lock(&update.catalog).clone();
                    match update.mutation {
                        CatalogMutation::Register(definitions) => {
                            for definition in definitions {
                                if let Some(existing) = next
                                    .iter_mut()
                                    .find(|existing| existing.name == definition.name)
                                {
                                    *existing = definition;
                                } else {
                                    next.push(definition);
                                }
                            }
                        }
                        CatalogMutation::Unregister(names) => {
                            let removed = names.into_iter().collect::<BTreeSet<_>>();
                            next.retain(|definition| !removed.contains(&definition.name));
                        }
                    }
                    validate_tool_definitions(&next, EXTENSION_API_VERSION_0_2)
                        .map_err(|error| error.to_string())
                        .and_then(|()| {
                            let process_tools = process.process_tools(Arc::clone(&active), &next);
                            let revision_stamp = Arc::clone(&process_tools.revision);
                            let reservation = registration.reserve(process_tools.tools)?;
                            let revision = active
                                .catalog_revision
                                .load(Ordering::Acquire)
                                .saturating_add(1);
                            let (_, published) = reservation.commit_with(|_, published| {
                                revision_stamp.store(revision, Ordering::Release);
                                let _catalog = write_std_lock(&active.catalog_guard);
                                *write_std_lock(&update.catalog) = next
                                    .iter()
                                    .filter(|definition| published.contains(&definition.name))
                                    .cloned()
                                    .collect();
                                active.catalog_revision.store(revision, Ordering::Release);
                            })?;
                            next.retain(|definition| published.contains(&definition.name));
                            *write_std_lock(&update.catalog) = next.clone();
                            committed_catalog = Some((registration.clone(), Arc::clone(&active)));
                            Ok(ToolCatalogUpdateResponse {
                                revision,
                                tools: next.into_iter().map(|definition| definition.name).collect(),
                            })
                        })
                }
            }
        };
        let response = match result {
            Ok(result) => serde_json::json!({
                "jsonrpc":"2.0",
                "id":update.request_id,
                "result":result,
            }),
            Err(message) => serde_json::json!({
                "jsonrpc":"2.0",
                "id":update.request_id,
                "error":{"code":-32602,"message":message},
            }),
        };
        let delivery = try_queue_child_response(
            &update.child_requests,
            &update.request_id,
            &update.writer,
            update.max_message_bytes,
            response,
        );
        if !matches!(delivery, Ok(ChildResponseAdmission::Queued)) {
            if let Some((registration, connection)) = committed_catalog {
                registration.remove();
                {
                    let _catalog = write_std_lock(&connection.catalog_guard);
                    write_std_lock(&connection.tool_catalog).clear();
                }
                update_health(
                    &connection.health,
                    ExtensionHealthState::Crashed,
                    Some("dynamic tool catalog acknowledgement was not delivered".to_owned()),
                );
                connection.terminate().await;
            }
        }
        if let Err(message) = delivery {
            let _ = inner.events.send(ExtensionEvent::Diagnostic { message });
            settle_child_request(&update.child_requests, &update.request_id);
        }
    }
}

fn new_extension_instance_id() -> String {
    let mut random = [0_u8; 16];
    if getrandom::fill(&mut random).is_ok() {
        return random.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    let sequence = NEXT_EXTENSION_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
    format!("local-{}-{sequence}", std::process::id())
}

fn supervisor_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(20);
    let base_ms = u64::try_from(SUPERVISOR_BASE_BACKOFF.as_millis()).unwrap_or(u64::MAX);
    let cap_ms = u64::try_from(SUPERVISOR_MAX_BACKOFF.as_millis()).unwrap_or(u64::MAX);
    let exponential = base_ms.saturating_mul(1_u64 << shift).min(cap_ms);
    let mut random = [0_u8; 8];
    let jitter = if getrandom::fill(&mut random).is_ok() {
        u64::from_le_bytes(random) % exponential.saturating_add(1)
    } else {
        exponential
    };
    Duration::from_millis(jitter)
}

fn permanent_supervisor_error(error: &ExtensionRuntimeError) -> bool {
    matches!(
        error,
        ExtensionRuntimeError::InvalidManifest(_)
            | ExtensionRuntimeError::UnsupportedApiVersion { .. }
            | ExtensionRuntimeError::ReloadRequiresReregistration { .. }
    )
}

fn supervisor_is_stopping(inner: &ExtensionProcessInner) -> bool {
    HOST_SHUTDOWN_REQUESTED.load(Ordering::Acquire)
        || inner.supervisor_cancelled.load(Ordering::Acquire)
}

async fn wait_for_supervisor_revival(
    inner: &Weak<ExtensionProcessInner>,
    parked_generation: u64,
) -> bool {
    loop {
        let Some(current_inner) = inner.upgrade() else {
            return false;
        };
        if supervisor_is_stopping(&current_inner) {
            return false;
        }
        let active_generation = read_std_lock(&current_inner.connection).generation;
        drop(current_inner);
        if active_generation != parked_generation {
            return true;
        }
        tokio::time::sleep(SUPERVISOR_POLL).await;
    }
}

async fn supervise_extension(inner: Weak<ExtensionProcessInner>) {
    let mut restart_attempts = 0_u32;
    loop {
        let Some(current_inner) = inner.upgrade() else {
            return;
        };
        if supervisor_is_stopping(&current_inner) {
            return;
        }
        let connection = read_std_lock(&current_inner.connection).clone();
        let generation = connection.generation;
        drop(current_inner);
        let ready_since = Instant::now();
        let mut reset_attempts = false;
        loop {
            let Some(current_inner) = inner.upgrade() else {
                return;
            };
            if supervisor_is_stopping(&current_inner) {
                return;
            }
            let active = read_std_lock(&current_inner.connection).clone();
            drop(current_inner);
            if active.generation != generation {
                break;
            }
            if ready_since.elapsed() >= SUPERVISOR_STABLE_READY {
                reset_attempts = true;
            }
            if connection.closed.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(SUPERVISOR_POLL).await;
        }
        let Some(current_inner) = inner.upgrade() else {
            return;
        };
        if supervisor_is_stopping(&current_inner) {
            return;
        }
        if read_std_lock(&current_inner.connection).generation != generation {
            continue;
        }
        if let Some(registration) =
            lock_std_mutex(&current_inner.dynamic_tool_registration).as_ref()
        {
            // A frozen provider turn retains its pinned failing endpoint, but
            // subsequent turns must not keep advertising a dead process.
            registration.remove();
        }
        if reset_attempts {
            restart_attempts = 0;
        }
        restart_attempts = restart_attempts.saturating_add(1);
        if restart_attempts > SUPERVISOR_MAX_RESTARTS {
            update_health(
                &connection.health,
                ExtensionHealthState::Parked,
                Some(format!(
                    "extension restart budget exhausted after {SUPERVISOR_MAX_RESTARTS} attempts"
                )),
            );
            if let Some(registration) =
                lock_std_mutex(&current_inner.dynamic_tool_registration).as_ref()
            {
                registration.remove();
            }
            let _ = current_inner.events.send(ExtensionEvent::Diagnostic {
                message: format!(
                    "extension `{}` parked after repeated crashes",
                    current_inner.descriptor.manifest.name
                ),
            });
            drop(current_inner);
            if wait_for_supervisor_revival(&inner, generation).await {
                restart_attempts = 0;
                continue;
            }
            return;
        }
        update_health(
            &connection.health,
            ExtensionHealthState::Backoff,
            Some(format!(
                "unexpected exit; restart attempt {restart_attempts}/{SUPERVISOR_MAX_RESTARTS}"
            )),
        );
        let delay = supervisor_backoff(restart_attempts);
        drop(current_inner);
        tokio::time::sleep(delay).await;

        let Some(current_inner) = inner.upgrade() else {
            return;
        };
        if supervisor_is_stopping(&current_inner) {
            return;
        }
        let reload_guard = current_inner.reload_guard.lock().await;
        if supervisor_is_stopping(&current_inner) {
            return;
        }
        let active = read_std_lock(&current_inner.connection).clone();
        if active.generation != generation || !active.closed.load(Ordering::Acquire) {
            continue;
        }
        let process = ExtensionProcess {
            inner: Arc::clone(&current_inner),
        };
        let result = process.reload_locked(Some(generation)).await;
        drop(reload_guard);
        drop(current_inner);
        if let Err(error) = result {
            let Some(current_inner) = inner.upgrade() else {
                return;
            };
            let active = read_std_lock(&current_inner.connection).clone();
            update_health(
                &active.health,
                if permanent_supervisor_error(&error) {
                    ExtensionHealthState::Parked
                } else {
                    ExtensionHealthState::Crashed
                },
                Some(error.to_string()),
            );
            if permanent_supervisor_error(&error) {
                if let Some(registration) =
                    lock_std_mutex(&current_inner.dynamic_tool_registration).as_ref()
                {
                    registration.remove();
                }
                let _ = current_inner.events.send(ExtensionEvent::Diagnostic {
                    message: format!(
                        "extension `{}` parked after a permanent restart error: {error}",
                        current_inner.descriptor.manifest.name
                    ),
                });
                drop(current_inner);
                if wait_for_supervisor_revival(&inner, generation).await {
                    restart_attempts = 0;
                    continue;
                }
                return;
            }
        }
    }
}

struct ProcessToolSet {
    tools: Vec<Arc<dyn Tool>>,
    revision: Arc<AtomicU64>,
}

struct ProcessTool {
    process: ExtensionProcess,
    connection: Arc<ProcessConnection>,
    definition: ToolDefinition,
    catalog_revision: Arc<AtomicU64>,
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
        let context = self
            .process
            .tool_execution_context(ctx, self.connection.generation);
        let mut events = self.process.subscribe();
        let mut events_open = true;
        let legacy_uncorrelated = self.process.api_version() == EXTENSION_API_VERSION_0_1;
        let (request_started, started) = oneshot::channel();
        let call = self.process.call_tool_controlled(
            Arc::clone(&self.connection),
            self.definition.clone(),
            self.catalog_revision.load(Ordering::Acquire),
            args,
            context,
            ctx.cancellation.clone(),
            ctx.progress.clone(),
            request_started,
        );
        tokio::pin!(call);
        let operation = tokio::select! {
            output = &mut call => return lower_process_tool_output(output, legacy_uncorrelated),
            started = started => match started {
                Ok(operation) => operation,
                Err(_) => return lower_process_tool_output(call.await, legacy_uncorrelated),
            },
        };
        let output = loop {
            tokio::select! {
                output = &mut call => break output,
                event = events.recv(), if events_open => match event {
                    Ok(ExtensionEvent::ConfirmationRequested {
                        request_id,
                        generation,
                        parent_request_id: event_parent,
                        request,
                    }) if event_parent.is_some_and(|parent| operation.owns(generation, parent))
                        || (legacy_uncorrelated
                            && generation == operation.generation
                            && event_parent.is_none()) => {
                        let confirmation = ctx.progress.confirmation(
                                request.prompt,
                                request.detail,
                                request.destructive,
                                request.default,
                            );
                        tokio::pin!(confirmation);
                        let confirmed = tokio::select! {
                            confirmed = &mut confirmation => confirmed,
                            _ = ctx.cancellation.cancelled() => false,
                        };
                        self.process.respond_to_confirmation(
                            request_id,
                            generation,
                            ConfirmationResponse { confirmed },
                        ).await.map_err(|error| ToolError::new(error.to_string()))?;
                    }
                    Ok(ExtensionEvent::ConfirmationRequested { .. }) => {}
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
                    Ok(ExtensionEvent::PolicyEvaluationRequested {
                        ..
                    }) => {}
                    Ok(ExtensionEvent::InputRequested {
                        request_id,
                        generation,
                        parent_request_id: event_parent,
                        request,
                    }) if operation.owns(generation, event_parent) => {
                        let input = ctx.progress.input(request.prompt, request.secret);
                        tokio::pin!(input);
                        let value = tokio::select! {
                            answer = &mut input => answer.and_then(|answer| {
                                let bytes = answer.as_bytes();
                                (bytes.len() <= MAX_EXTENSION_INPUT_VALUE_BYTES)
                                    .then(|| std::str::from_utf8(bytes).ok().map(str::to_owned))
                                    .flatten()
                            }),
                            _ = ctx.cancellation.cancelled() => None,
                        };
                        self.process.respond_to_input(
                            request_id,
                            generation,
                            ExtensionInputResponse { value },
                        ).await.map_err(|error| ToolError::new(error.to_string()))?;
                    }
                    Ok(ExtensionEvent::InputRequested { .. }) => {}
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        ctx.progress.status(format!(
                            "extension event stream dropped {count} event(s)"
                        ));
                    }
                    Err(broadcast::error::RecvError::Closed) => events_open = false,
                }
            }
        };
        lower_process_tool_output(output, legacy_uncorrelated)
    }
}

fn lower_process_tool_output(
    output: Result<ToolCallOutput, ExtensionRuntimeError>,
    api_0_1: bool,
) -> Result<ToolOutput, ToolError> {
    match output {
        Ok(output) if api_0_1 && output.is_error => Err(ToolError::new(output.content)),
        Ok(output) => {
            let is_error = output.is_error;
            output
                .into_native()
                .map(|output| output.with_is_error(is_error))
                .map_err(|error| ToolError::new(error.to_string()))
        }
        Err(error) => Err(ToolError::new(error.to_string())),
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

const FRAME_QUEUED: u8 = 0;
const FRAME_WRITING: u8 = 1;
const FRAME_WRITTEN: u8 = 2;
const FRAME_SKIPPED: u8 = 3;
const REQUEST_ACTIVE: u8 = 0;
const REQUEST_COMPLETED: u8 = 1;
const REQUEST_CANCELLED: u8 = 2;
const JSON_RPC_REQUEST_CANCELLED: i64 = -32800;

struct ProcessConnection {
    writer: mpsc::Sender<WriterFrame>,
    child: Arc<Mutex<Child>>,
    pending: PendingRequests,
    pending_changed: Arc<Notify>,
    child_requests: ChildRequests,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    active_admissions: AtomicU64,
    slots: StdRwLock<Arc<Semaphore>>,
    max_message_bytes: usize,
    shutdown_timeout: Duration,
    cancellation_grace: Duration,
    tombstone_ttl: Duration,
    tombstones: Arc<StdMutex<RequestTombstones>>,
    protocol: Arc<StdRwLock<ExtensionNegotiatedProtocol>>,
    catalog_guard: StdRwLock<()>,
    tool_catalog: Arc<StdRwLock<Vec<ToolDefinition>>>,
    catalog_revision: AtomicU64,
    health: Arc<StdRwLock<ConnectionHealth>>,
    events: broadcast::Sender<ExtensionEvent>,
    generation: u64,
    artifact_store: ArtifactStore,
    artifact_leases: AtomicU64,
    artifact_leases_changed: Notify,
    artifacts_settled: AtomicBool,
    process_group: ProcessGroupGuard,
}

fn connection_is_usable(connection: &ProcessConnection) -> bool {
    if connection.closed.load(Ordering::Acquire) {
        return false;
    }
    matches!(
        read_std_lock(&connection.health).state,
        ExtensionHealthState::Ready | ExtensionHealthState::Degraded
    )
}

#[derive(Clone, Debug)]
enum PendingError {
    Closed(String),
    Protocol(String),
    Cancelled(String),
    Remote {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },
}

type PendingReply = Result<serde_json::Value, PendingError>;
type PendingSender = oneshot::Sender<PendingReply>;

struct PendingRequest {
    sender: PendingSender,
    terminal: Arc<AtomicU8>,
    frame_state: Arc<AtomicU8>,
    cancellation_sent: Arc<AtomicBool>,
    progress: Option<ToolProgressSink>,
    resource_owner: Option<ExtensionResourceOwner>,
    last_progress_sequence: Option<u64>,
}

type PendingRequests = Arc<StdMutex<HashMap<u64, PendingRequest>>>;
const CHILD_ACTIVE: u8 = 0;
const CHILD_RESPONDING: u8 = 1;
const CHILD_SETTLED: u8 = 2;

struct ChildRequest {
    parent_request_id: u64,
    response_state: Arc<ChildResponseState>,
    policy_intent: Option<ExtensionActionIntent>,
}

struct RegisteredChildRequest {
    parent_request_id: Option<u64>,
    progress: Option<ToolProgressSink>,
    resource_owner: Option<ExtensionResourceOwner>,
    response_state: Arc<ChildResponseState>,
}

struct ChildResponseState {
    state: AtomicU8,
    changed: Notify,
    cancel_on_response_abort: StdMutex<Option<String>>,
}

struct ChildResponseClaim {
    child_requests: ChildRequests,
    id: ExtensionRequestId,
    response_state: Arc<ChildResponseState>,
    admitted: bool,
    abort_cancel: Option<(mpsc::Sender<WriterFrame>, usize)>,
}

impl ChildResponseClaim {
    fn mark_admitted(&mut self) {
        self.response_state
            .state
            .store(CHILD_SETTLED, Ordering::Release);
        let mut children = lock_std_mutex(&self.child_requests);
        if children
            .get(&self.id)
            .is_some_and(|child| Arc::ptr_eq(&child.response_state, &self.response_state))
        {
            children.remove(&self.id);
        }
        self.admitted = true;
        self.response_state.changed.notify_waiters();
    }
}

impl Drop for ChildResponseClaim {
    fn drop(&mut self) {
        if self.admitted {
            return;
        }
        let deferred_cancel = {
            let mut children = lock_std_mutex(&self.child_requests);
            if !children
                .get(&self.id)
                .is_some_and(|child| Arc::ptr_eq(&child.response_state, &self.response_state))
            {
                return;
            }
            let deferred_cancel =
                lock_std_mutex(&self.response_state.cancel_on_response_abort).take();
            if deferred_cancel.is_some() {
                self.response_state
                    .state
                    .store(CHILD_SETTLED, Ordering::Release);
                children.remove(&self.id);
            } else {
                let _ = self.response_state.state.compare_exchange(
                    CHILD_RESPONDING,
                    CHILD_ACTIVE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            self.response_state.changed.notify_waiters();
            deferred_cancel
        };
        if let (Some(reason), Some((writer, max_message_bytes))) =
            (deferred_cancel, self.abort_cancel.take())
        {
            let _ = queue_writer_value(
                &writer,
                max_message_bytes,
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "method":methods::CANCEL_REQUEST,
                    "params":{"id":self.id,"reason":reason},
                }),
            );
        }
    }
}

type ChildRequests = Arc<StdMutex<HashMap<ExtensionRequestId, ChildRequest>>>;

struct PendingRegistration {
    connection: Weak<ProcessConnection>,
    id: u64,
    cancellation_reason: Arc<StdMutex<String>>,
    armed: bool,
}

impl PendingRegistration {
    fn new(
        connection: &Arc<ProcessConnection>,
        id: u64,
        cancellation_reason: Arc<StdMutex<String>>,
    ) -> Self {
        Self {
            connection: Arc::downgrade(connection),
            id,
            cancellation_reason,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(connection) = self.connection.upgrade() else {
            return;
        };
        let reason = lock_std_mutex(&self.cancellation_reason).clone();
        connection.cancel_request(self.id, &reason);
    }
}

struct WriterFrame {
    line: Vec<u8>,
    state: Arc<AtomicU8>,
    completion: Option<oneshot::Sender<Result<(), PendingError>>>,
}

struct ZeroizingBytes(Vec<u8>);

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Serialize)]
struct ChildSuccessResponse<'a, T: ?Sized> {
    jsonrpc: &'static str,
    id: &'a ExtensionRequestId,
    result: &'a T,
}

impl Drop for WriterFrame {
    fn drop(&mut self) {
        // Some API 0.2 frames carry approval capabilities or secret values.
        // Erasing every bounded frame avoids a fragile sensitive/non-sensitive
        // distinction in the shared writer queue.
        self.line.fill(0);
    }
}

struct ArtifactDecodeLease {
    connection: Arc<ProcessConnection>,
}

struct RequestAdmissionLease {
    connection: Arc<ProcessConnection>,
}

impl Drop for RequestAdmissionLease {
    fn drop(&mut self) {
        self.connection
            .active_admissions
            .fetch_sub(1, Ordering::AcqRel);
        self.connection.pending_changed.notify_waiters();
    }
}

impl Drop for ArtifactDecodeLease {
    fn drop(&mut self) {
        self.connection
            .artifact_leases
            .fetch_sub(1, Ordering::AcqRel);
        self.connection.artifact_leases_changed.notify_waiters();
        self.connection.pending_changed.notify_waiters();
    }
}

#[derive(Default)]
struct RequestTombstones {
    entries: VecDeque<(u64, Instant)>,
}

impl RequestTombstones {
    fn insert(&mut self, id: u64, ttl: Duration) {
        self.purge();
        self.remove(id);
        while self.entries.len() >= MAX_TOMBSTONES {
            self.entries.pop_front();
        }
        self.entries.push_back((
            id,
            Instant::now().checked_add(ttl).unwrap_or_else(Instant::now),
        ));
    }

    fn remove(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(entry_id, _)| *entry_id != id);
        before != self.entries.len()
    }

    fn contains(&mut self, id: u64) -> bool {
        self.purge();
        self.entries.iter().any(|(entry_id, _)| *entry_id == id)
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.entries.retain(|(_, expires)| *expires > now);
    }
}

struct ConnectionHealth {
    state: ExtensionHealthState,
    last_error: Option<String>,
}

fn update_health(
    health: &StdRwLock<ConnectionHealth>,
    state: ExtensionHealthState,
    error: Option<String>,
) {
    let mut health = write_std_lock(health);
    health.state = state;
    if let Some(mut error) = error {
        truncate_utf8(&mut error, MAX_LIFECYCLE_REASON_BYTES);
        health.last_error = Some(error);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_protocol_writer(
    mut stdin: ChildStdin,
    mut frames: mpsc::Receiver<WriterFrame>,
    closed: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    pending: PendingRequests,
    pending_changed: Arc<Notify>,
    health: Arc<StdRwLock<ConnectionHealth>>,
    events: broadcast::Sender<ExtensionEvent>,
    child: Arc<Mutex<Child>>,
    termination: ProcessTerminationHandle,
) {
    while let Some(mut frame) = frames.recv().await {
        if frame
            .state
            .compare_exchange(
                FRAME_QUEUED,
                FRAME_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            if let Some(completion) = frame.completion.take() {
                let _ = completion.send(Err(PendingError::Cancelled(
                    "frame cancelled before write".into(),
                )));
            }
            continue;
        }

        let result = async {
            stdin
                .write_all(&frame.line)
                .await
                .map_err(|error| error.to_string())?;
            stdin.flush().await.map_err(|error| error.to_string())
        }
        .await;
        match result {
            Ok(()) => {
                frame.state.store(FRAME_WRITTEN, Ordering::Release);
                if let Some(completion) = frame.completion.take() {
                    let _ = completion.send(Ok(()));
                }
            }
            Err(message) => {
                closed.store(true, Ordering::Release);
                update_health(
                    &health,
                    ExtensionHealthState::Crashed,
                    Some(format!("stdin write failed: {message}")),
                );
                let error = PendingError::Closed(format!("stdin write failed: {message}"));
                if let Some(completion) = frame.completion.take() {
                    let _ = completion.send(Err(error.clone()));
                }
                fail_all_pending(&pending, &pending_changed, error);
                let _ = events.send(ExtensionEvent::Diagnostic {
                    message: format!("extension stdin write failed: {message}"),
                });
                if !draining.load(Ordering::Acquire) {
                    reap_failed_extension(child, termination).await;
                }
                return;
            }
        }
    }

    if !closed.swap(true, Ordering::AcqRel) {
        let coordinated = draining.load(Ordering::Acquire);
        let state = if coordinated {
            ExtensionHealthState::Stopped
        } else {
            ExtensionHealthState::Crashed
        };
        update_health(
            &health,
            state,
            (!draining.load(Ordering::Acquire)).then(|| "extension writer closed".into()),
        );
        fail_all_pending(
            &pending,
            &pending_changed,
            PendingError::Closed("extension writer closed".into()),
        );
        if !coordinated {
            reap_failed_extension(child, termination).await;
        }
    }
}

async fn reap_failed_extension(child: Arc<Mutex<Child>>, termination: ProcessTerminationHandle) {
    termination.terminate();
    let mut child = child.lock().await;
    if tokio::time::timeout(DEFAULT_SHUTDOWN_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

impl ProcessConnection {
    fn acquire_artifact_lease(self: &Arc<Self>) -> ArtifactDecodeLease {
        self.artifact_leases.fetch_add(1, Ordering::AcqRel);
        ArtifactDecodeLease {
            connection: Arc::clone(self),
        }
    }

    fn settle_artifacts(&self) {
        if !self.artifacts_settled.swap(true, Ordering::AcqRel) {
            let _ = self.artifact_store.settle_generation(self.generation);
        }
    }
    async fn request(
        self: &Arc<Self>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(method, params, timeout, true, true, None, None, None, None)
            .await
    }

    async fn request_with_resource_owner(
        self: &Arc<Self>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        resource_owner: Option<ExtensionResourceOwner>,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(
            method,
            params,
            timeout,
            true,
            true,
            None,
            None,
            resource_owner,
            None,
        )
        .await
    }

    async fn request_with_operation(
        self: &Arc<Self>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        request_started: oneshot::Sender<ExtensionOperationToken>,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(
            method,
            params,
            timeout,
            true,
            true,
            None,
            None,
            None,
            Some(request_started),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_with_cancellation(
        self: &Arc<Self>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        cancellation: CancellationToken,
        progress: ToolProgressSink,
        resource_owner: Option<ExtensionResourceOwner>,
        request_started: oneshot::Sender<ExtensionOperationToken>,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(
            method,
            params,
            timeout,
            true,
            true,
            Some(cancellation),
            Some(progress),
            resource_owner,
            Some(request_started),
        )
        .await
    }

    async fn request_during_shutdown(
        self: &Arc<Self>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        self.request_inner(
            method, params, timeout, false, false, None, None, None, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_inner(
        self: &Arc<Self>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        cancel_on_host_shutdown: bool,
        use_request_slot: bool,
        cancellation: Option<CancellationToken>,
        progress: Option<ToolProgressSink>,
        resource_owner: Option<ExtensionResourceOwner>,
        request_started: Option<oneshot::Sender<ExtensionOperationToken>>,
    ) -> Result<serde_json::Value, ExtensionRuntimeError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeError::Closed("stdout is closed".into()));
        }
        if use_request_slot && self.draining.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeError::Closed(
                "extension generation is draining".into(),
            ));
        }

        let cancellation_reason = Arc::new(StdMutex::new("request dropped".to_owned()));
        let operation_reason = Arc::clone(&cancellation_reason);
        let connection = Arc::clone(self);
        let operation = async move {
            let _admission = if use_request_slot {
                Some(connection.acquire_request_admission()?)
            } else {
                None
            };
            let _slot =
                if use_request_slot {
                    let slots = read_std_lock(&connection.slots).clone();
                    Some(slots.acquire_owned().await.map_err(|_| {
                        ExtensionRuntimeError::Closed("request queue is closed".into())
                    })?)
                } else {
                    None
                };
            if use_request_slot && connection.draining.load(Ordering::Acquire) {
                return Err(ExtensionRuntimeError::Closed(
                    "extension generation is draining".into(),
                ));
            }
            let id = connection.next_id.fetch_add(1, Ordering::Relaxed);
            let message = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });
            let line = connection.serialize_message(&message)?;
            let (reply_tx, reply_rx) = oneshot::channel();
            let terminal = Arc::new(AtomicU8::new(REQUEST_ACTIVE));
            let frame_state = Arc::new(AtomicU8::new(FRAME_QUEUED));
            let cancellation_sent = Arc::new(AtomicBool::new(false));
            lock_std_mutex(&connection.pending).insert(
                id,
                PendingRequest {
                    sender: reply_tx,
                    terminal,
                    frame_state: Arc::clone(&frame_state),
                    cancellation_sent,
                    progress,
                    resource_owner,
                    last_progress_sequence: None,
                },
            );
            if let Some(request_started) = request_started {
                let _ = request_started.send(ExtensionOperationToken {
                    generation: connection.generation,
                    parent_request_id: id,
                });
            }
            let mut registration =
                PendingRegistration::new(&connection, id, Arc::clone(&operation_reason));
            connection
                .writer
                .send(WriterFrame {
                    line,
                    state: frame_state,
                    completion: None,
                })
                .await
                .map_err(|_| ExtensionRuntimeError::Closed("extension writer closed".into()))?;

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

        let wait_for_cancellation = async {
            if let Some(cancellation) = cancellation {
                cancellation.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(wait_for_cancellation);

        if cancel_on_host_shutdown {
            tokio::select! {
                biased;
                _ = host_shutdown_requested() => {
                    *lock_std_mutex(&cancellation_reason) = "host shutdown".into();
                    Err(ExtensionRuntimeError::Closed("host is shutting down".into()))
                },
                _ = &mut wait_for_cancellation => {
                    *lock_std_mutex(&cancellation_reason) = "user".into();
                    Err(ExtensionRuntimeError::Cancelled {
                        method: method.to_owned(),
                        reason: "user".into(),
                    })
                },
                result = &mut timed => match result {
                    Ok(result) => result,
                    Err(_) => {
                        *lock_std_mutex(&cancellation_reason) = "timeout".into();
                        Err(ExtensionRuntimeError::Timeout { method: method.to_owned() })
                    }
                },
            }
        } else {
            match timed.await {
                Ok(result) => result,
                Err(_) => {
                    *lock_std_mutex(&cancellation_reason) = "timeout".into();
                    Err(ExtensionRuntimeError::Timeout {
                        method: method.to_owned(),
                    })
                }
            }
        }
    }

    fn serialize_message(
        &self,
        message: &serde_json::Value,
    ) -> Result<Vec<u8>, ExtensionRuntimeError> {
        let mut line = serde_json::to_vec(message)
            .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        line.push(b'\n');
        if line.len() > self.max_message_bytes {
            return Err(ExtensionRuntimeError::MessageTooLarge {
                limit: self.max_message_bytes,
            });
        }
        Ok(line)
    }

    fn queue_notification(&self, method: &str, params: serde_json::Value) -> bool {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let Ok(line) = self.serialize_message(&message) else {
            return false;
        };
        let queued = self
            .writer
            .try_send(WriterFrame {
                line,
                state: Arc::new(AtomicU8::new(FRAME_QUEUED)),
                completion: None,
            })
            .is_ok();
        if !queued {
            update_health(
                &self.health,
                ExtensionHealthState::Degraded,
                Some(format!(
                    "bounded writer queue rejected `{method}` notification"
                )),
            );
            let _ = self.events.send(ExtensionEvent::Diagnostic {
                message: format!("dropped `{method}` because the extension writer queue is full"),
            });
        }
        queued
    }

    fn cancel_request(self: &Arc<Self>, id: u64, reason: &str) {
        let request = {
            let mut pending = lock_std_mutex(&self.pending);
            let Some(request) = pending.get(&id) else {
                return;
            };
            if request
                .terminal
                .compare_exchange(
                    REQUEST_ACTIVE,
                    REQUEST_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return;
            }
            pending.remove(&id)
        };
        let Some(request) = request else {
            return;
        };
        self.pending_changed.notify_waiters();
        let _ = request
            .sender
            .send(Err(PendingError::Cancelled(reason.to_owned())));
        lock_std_mutex(&self.tombstones).insert(id, self.tombstone_ttl);
        self.cancel_children(id, reason);

        let frame_was_admitted = request
            .frame_state
            .compare_exchange(
                FRAME_QUEUED,
                FRAME_SKIPPED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err();
        let cancellation_supported = read_std_lock(&self.protocol).supports("request_cancellation");
        if frame_was_admitted
            && cancellation_supported
            && !request.cancellation_sent.swap(true, Ordering::AcqRel)
        {
            let _ = self.queue_notification(
                methods::CANCEL_REQUEST,
                serde_json::json!({"id": id, "reason": reason}),
            );
            self.schedule_cancellation_escalation(id);
        }
    }

    fn cancel_children(&self, parent_request_id: u64, reason: &str) {
        let child_ids = cancel_active_children(&self.child_requests, parent_request_id, reason);
        for id in child_ids {
            let _ = self.queue_notification(
                methods::CANCEL_REQUEST,
                serde_json::json!({"id": id, "reason": reason}),
            );
        }
    }

    fn schedule_cancellation_escalation(self: &Arc<Self>, id: u64) {
        let connection = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(connection.cancellation_grace).await;
            let unresolved = lock_std_mutex(&connection.tombstones).contains(id);
            if unresolved && !connection.closed.load(Ordering::Acquire) {
                update_health(
                    &connection.health,
                    ExtensionHealthState::Degraded,
                    Some(format!(
                        "request {id} did not acknowledge cancellation within {:?}",
                        connection.cancellation_grace
                    )),
                );
                connection.terminate().await;
            }
        });
    }

    fn cancel_all_pending(self: &Arc<Self>, reason: &str) {
        let ids = lock_std_mutex(&self.pending)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            self.cancel_request(id, reason);
        }
    }

    async fn send_child_response<T: Serialize + ?Sized>(
        &self,
        id: ExtensionRequestId,
        result: &T,
    ) -> Result<(), ExtensionRuntimeError> {
        self.send_child_response_admitted(id, result)
            .await
            .map(|_| ())
    }

    async fn send_child_response_admitted<T: Serialize + ?Sized>(
        &self,
        id: ExtensionRequestId,
        result: &T,
    ) -> Result<ChildResponseAdmission, ExtensionRuntimeError> {
        let mut line = serde_json::to_vec(&ChildSuccessResponse {
            jsonrpc: "2.0",
            id: &id,
            result,
        })
        .map_err(|error| ExtensionRuntimeError::Protocol(error.to_string()))?;
        line.push(b'\n');
        if line.len() > self.max_message_bytes {
            line.fill(0);
            return Err(ExtensionRuntimeError::MessageTooLarge {
                limit: self.max_message_bytes,
            });
        }
        let line = ZeroizingBytes(line);
        loop {
            let response_state = {
                let children = lock_std_mutex(&self.child_requests);
                let Some(child) = children.get(&id) else {
                    return Ok(ChildResponseAdmission::AlreadySettled);
                };
                Arc::clone(&child.response_state)
            };
            match response_state.state.compare_exchange(
                CHILD_ACTIVE,
                CHILD_RESPONDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let mut claim = ChildResponseClaim {
                        child_requests: Arc::clone(&self.child_requests),
                        id: id.clone(),
                        response_state,
                        admitted: false,
                        abort_cancel: Some((self.writer.clone(), self.max_message_bytes)),
                    };
                    let (completed, completion) = oneshot::channel();
                    let admission = self.writer.send(WriterFrame {
                        line: line.0.clone(),
                        state: Arc::new(AtomicU8::new(FRAME_QUEUED)),
                        completion: Some(completed),
                    });
                    tokio::pin!(admission);
                    tokio::select! {
                        biased;
                        _ = host_shutdown_requested() => return Err(
                            ExtensionRuntimeError::Closed("host is shutting down".into())
                        ),
                        result = tokio::time::timeout(
                            CONFIRMATION_RESPONSE_TIMEOUT,
                            &mut admission,
                        ) => result.map_err(|_| ExtensionRuntimeError::Timeout {
                            method: "extension/response admission".to_owned(),
                        })?.map_err(|_| ExtensionRuntimeError::Closed(
                            "extension writer closed".into()
                        ))?,
                    };
                    // Writer admission is the sole terminal outcome boundary:
                    // after this non-awaiting step cancellation cannot enqueue
                    // a competing $/cancelRequest for the same child request.
                    claim.mark_admitted();
                    let completed = tokio::select! {
                        biased;
                        _ = host_shutdown_requested() => return Err(
                            ExtensionRuntimeError::Closed("host is shutting down".into())
                        ),
                        result = tokio::time::timeout(
                            CONFIRMATION_RESPONSE_TIMEOUT,
                            completion,
                        ) => result.map_err(|_| ExtensionRuntimeError::Timeout {
                            method: "extension/response write".to_owned(),
                        })?,
                    };
                    completed
                        .map_err(|_| {
                            ExtensionRuntimeError::Closed("extension writer closed".into())
                        })?
                        .map_err(pending_error)?;
                    return Ok(ChildResponseAdmission::Queued);
                }
                Err(CHILD_RESPONDING) => {
                    let changed = response_state.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if response_state.state.load(Ordering::Acquire) == CHILD_RESPONDING {
                        changed.await;
                    }
                }
                Err(CHILD_SETTLED) => return Ok(ChildResponseAdmission::AlreadySettled),
                Err(_) => continue,
            }
        }
    }

    fn begin_drain(&self) -> bool {
        if self.draining.swap(true, Ordering::AcqRel) {
            return false;
        }
        read_std_lock(&self.slots).close();
        update_health(&self.health, ExtensionHealthState::Draining, None);
        true
    }

    fn resume_after_failed_drain(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let limit = read_std_lock(&self.protocol).max_concurrent_requests;
        *write_std_lock(&self.slots) = Arc::new(Semaphore::new(limit));
        self.draining.store(false, Ordering::Release);
        update_health(&self.health, ExtensionHealthState::Ready, None);
        true
    }

    fn acquire_request_admission(
        self: &Arc<Self>,
    ) -> Result<RequestAdmissionLease, ExtensionRuntimeError> {
        if self.draining.load(Ordering::Acquire) {
            return Err(ExtensionRuntimeError::Closed(
                "extension generation is draining".into(),
            ));
        }
        self.active_admissions.fetch_add(1, Ordering::AcqRel);
        if self.draining.load(Ordering::Acquire) {
            self.active_admissions.fetch_sub(1, Ordering::AcqRel);
            self.pending_changed.notify_waiters();
            return Err(ExtensionRuntimeError::Closed(
                "extension generation is draining".into(),
            ));
        }
        Ok(RequestAdmissionLease {
            connection: Arc::clone(self),
        })
    }

    async fn drain(self: &Arc<Self>, deadline: Duration) -> bool {
        let settled = self.quiesce(deadline).await;
        if !settled {
            self.cancel_all_pending("reload drain deadline");
        } else {
            self.settle_artifacts();
        }
        settled
    }

    async fn quiesce(self: &Arc<Self>, deadline: Duration) -> bool {
        self.begin_drain();
        tokio::time::timeout(deadline, async {
            loop {
                let changed = self.pending_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if lock_std_mutex(&self.pending).is_empty()
                    && self.active_admissions.load(Ordering::Acquire) == 0
                    && self.artifact_leases.load(Ordering::Acquire) == 0
                {
                    break;
                }
                changed.await;
            }
        })
        .await
        .is_ok()
    }

    async fn shutdown(self: &Arc<Self>) -> bool {
        self.begin_drain();
        self.cancel_all_pending("shutdown");
        let quiescent = tokio::time::timeout(self.shutdown_timeout, async {
            loop {
                let changed = self.artifact_leases_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.artifact_leases.load(Ordering::Acquire) == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await
        .is_ok();
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
        update_health(&self.health, ExtensionHealthState::Stopped, None);
        if quiescent {
            self.settle_artifacts();
        }
        acknowledged && exited
    }

    async fn terminate(&self) {
        self.draining.store(true, Ordering::Release);
        self.kill_process_group();
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        self.closed.store(true, Ordering::Release);
        let current = read_std_lock(&self.health).state;
        if !matches!(
            current,
            ExtensionHealthState::Degraded
                | ExtensionHealthState::Crashed
                | ExtensionHealthState::Parked
        ) {
            update_health(&self.health, ExtensionHealthState::Stopped, None);
        }
    }

    fn kill_process_group(&self) {
        self.process_group.terminate_now();
    }
}

impl Drop for ProcessConnection {
    fn drop(&mut self) {
        self.process_group.terminate_now();
        self.settle_artifacts();
    }
}

struct ArtifactGenerationGuard {
    store: ArtifactStore,
    generation: u64,
    armed: bool,
}

impl ArtifactGenerationGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ArtifactGenerationGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.store.settle_generation(self.generation);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn spawn_connection(
    descriptor: &DiscoveredExtension,
    config: &ExtensionRuntimeConfig,
    host_state: ExtensionHostState,
    generation: u64,
    events: broadcast::Sender<ExtensionEvent>,
    artifact_store: ArtifactStore,
    catalog_updates: mpsc::Sender<CatalogUpdateRequest>,
    delegation_service: Arc<StdRwLock<Option<ExtensionDelegationService>>>,
    approval_store: Arc<ExtensionApprovalStore>,
) -> Result<(Arc<ProcessConnection>, ExtensionContributions), ExtensionRuntimeError> {
    let extension_dir =
        descriptor
            .manifest_path
            .parent()
            .ok_or_else(|| ExtensionRuntimeError::Spawn {
                extension: descriptor.manifest.name.clone(),
                message: "manifest has no parent directory".into(),
            })?;
    let resolved_entrypoint =
        resolve_entrypoint_command(extension_dir, &descriptor.manifest.entrypoint).map_err(
            |error| ExtensionRuntimeError::Spawn {
                extension: descriptor.manifest.name.clone(),
                message: error.to_string(),
            },
        )?;
    let scratch_directory = artifact_store
        .begin_generation(generation)
        .map_err(|error| ExtensionRuntimeError::Spawn {
            extension: descriptor.manifest.name.clone(),
            message: format!("cannot create generation scratch directory: {error}"),
        })?;
    let mut artifact_guard = ArtifactGenerationGuard {
        store: artifact_store.clone(),
        generation,
        armed: true,
    };
    let mut command = Command::new(&resolved_entrypoint.command);
    command
        .args(&descriptor.manifest.entrypoint.args)
        .current_dir(&config.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear()
        .envs(sanitized_subprocess_environment())
        .envs(&descriptor.manifest.entrypoint.env)
        .env(
            "YGG_EXTENSION_API_VERSION",
            &descriptor.manifest.api_version,
        )
        .env("YGG_EXTENSION_NAME", &descriptor.manifest.name)
        .env("YGG_EXTENSION_DIR", extension_dir)
        .env("YGG_EXTENSION_MANIFEST", &descriptor.manifest_path)
        .env("YGG_WORKSPACE", &config.workspace)
        .env("YGG_EXTENSION_SCRATCH", &scratch_directory);
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
    let termination = process_group.termination_handle();
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
    let child = Arc::new(Mutex::new(child));

    let pending = Arc::new(StdMutex::new(HashMap::new()));
    let pending_changed = Arc::new(Notify::new());
    let child_requests = Arc::new(StdMutex::new(HashMap::new()));
    let child_work_slots = Arc::new(Semaphore::new(MAX_CHILD_WORKERS));
    let closed = Arc::new(AtomicBool::new(false));
    let draining = Arc::new(AtomicBool::new(false));
    let tombstones = Arc::new(StdMutex::new(RequestTombstones::default()));
    let protocol = Arc::new(StdRwLock::new(ExtensionNegotiatedProtocol {
        version: descriptor.manifest.api_version.clone(),
        features: BTreeSet::new(),
        max_concurrent_requests: config.max_pending_requests,
        lifecycle_events: BTreeSet::new(),
    }));
    let tool_catalog = Arc::new(StdRwLock::new(Vec::new()));
    let health = Arc::new(StdRwLock::new(ConnectionHealth {
        state: ExtensionHealthState::Initializing,
        last_error: None,
    }));
    let initialization_complete = Arc::new(AtomicBool::new(false));
    let initialization_changed = Arc::new(Notify::new());
    let (writer, writer_frames) = mpsc::channel(config.writer_queue_capacity);
    tokio::spawn(run_protocol_writer(
        stdin,
        writer_frames,
        Arc::clone(&closed),
        Arc::clone(&draining),
        Arc::clone(&pending),
        Arc::clone(&pending_changed),
        Arc::clone(&health),
        events.clone(),
        Arc::clone(&child),
        termination,
    ));
    tokio::spawn(read_protocol_stdout(
        stdout,
        Arc::clone(&pending),
        Arc::clone(&pending_changed),
        Arc::clone(&closed),
        Arc::clone(&draining),
        events.clone(),
        generation,
        config.max_message_bytes,
        descriptor.manifest.contributes.clone(),
        writer.clone(),
        Arc::clone(&child_requests),
        child_work_slots,
        Arc::clone(&tombstones),
        Arc::clone(&protocol),
        Arc::clone(&tool_catalog),
        catalog_updates,
        delegation_service,
        approval_store,
        config.secret_broker.clone(),
        ExtensionIdentity {
            name: descriptor.manifest.name.clone(),
            version: descriptor.manifest.version.clone(),
            manifest_path: descriptor.manifest_path.clone(),
            source: descriptor.source,
        },
        Arc::new(
            descriptor
                .manifest
                .capabilities
                .secrets
                .iter()
                .cloned()
                .collect(),
        ),
        Arc::clone(&health),
        Arc::clone(&initialization_complete),
        Arc::clone(&initialization_changed),
        artifact_store.clone(),
        Some(Arc::clone(&child)),
        Some(termination),
    ));
    tokio::spawn(read_extension_stderr(
        stderr,
        events.clone(),
        config.max_message_bytes,
    ));

    let connection = Arc::new(ProcessConnection {
        writer,
        child,
        pending,
        pending_changed,
        child_requests,
        next_id: AtomicU64::new(1),
        closed,
        draining,
        active_admissions: AtomicU64::new(0),
        slots: StdRwLock::new(Arc::new(Semaphore::new(config.max_pending_requests))),
        max_message_bytes: config.max_message_bytes,
        shutdown_timeout: config.shutdown_timeout,
        cancellation_grace: config.cancellation_grace,
        tombstone_ttl: config.tombstone_ttl,
        tombstones,
        protocol,
        catalog_guard: StdRwLock::new(()),
        tool_catalog,
        catalog_revision: AtomicU64::new(0),
        health,
        events: events.clone(),
        generation,
        artifact_store,
        artifact_leases: AtomicU64::new(0),
        artifact_leases_changed: Notify::new(),
        artifacts_settled: AtomicBool::new(false),
        process_group,
    });
    artifact_guard.disarm();
    let offered_host_services = OfferedHostServices {
        agent_sessions: config.agent_sessions,
        approvals: config.approvals,
        secrets: config.secret_broker.is_some()
            && !descriptor.manifest.capabilities.secrets.is_empty(),
    };
    let mut optional_features = API_0_2_OPTIONAL_FEATURES
        .iter()
        .map(|feature| (*feature).to_owned())
        .collect::<Vec<_>>();
    if offered_host_services.agent_sessions {
        optional_features.push(EXTENSION_FEATURE_AGENT_SESSIONS.to_owned());
    }
    if offered_host_services.approvals {
        optional_features.push(EXTENSION_FEATURE_APPROVALS.to_owned());
    }
    if offered_host_services.secrets {
        optional_features.push(EXTENSION_FEATURE_SECRETS.to_owned());
    }
    let initialize = InitializeRequest {
        api_version: descriptor.manifest.api_version.clone(),
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
        protocol: (descriptor.manifest.api_version == EXTENSION_API_VERSION_0_2).then(|| {
            ExtensionProtocolRequest {
                version: EXTENSION_API_VERSION_0_2.to_owned(),
                required_features: API_0_2_REQUIRED_FEATURES
                    .iter()
                    .map(|feature| (*feature).to_owned())
                    .collect(),
                optional_features,
                limits: ExtensionProtocolLimits {
                    max_concurrent_requests: config.max_pending_requests,
                },
            }
        }),
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
            initialization_complete.store(true, Ordering::Release);
            initialization_changed.notify_waiters();
            connection.terminate().await;
            return Err(error);
        }
    };
    let (contributions, negotiated) = match negotiate_contributions_with_host_services(
        &descriptor.manifest,
        response,
        config.max_pending_requests,
        offered_host_services,
    ) {
        Ok(negotiated) => negotiated,
        Err(error) => {
            initialization_complete.store(true, Ordering::Release);
            initialization_changed.notify_waiters();
            connection.terminate().await;
            return Err(error);
        }
    };
    *write_std_lock(&connection.slots) =
        Arc::new(Semaphore::new(negotiated.max_concurrent_requests));
    *write_std_lock(&connection.protocol) = negotiated;
    *write_std_lock(&connection.tool_catalog) = contributions.tools.clone();
    initialization_complete.store(true, Ordering::Release);
    initialization_changed.notify_waiters();
    update_health(&connection.health, ExtensionHealthState::Ready, None);
    Ok((connection, contributions))
}

const MAX_STAGED_ENTRYPOINT_BYTES: u64 = 64 * 1024 * 1024;

struct ResolvedEntrypoint {
    command: PathBuf,
    _staging: Option<tempfile::TempDir>,
}

fn stage_entrypoint(path: &Path) -> std::io::Result<Option<ResolvedEntrypoint>> {
    let mut source = match crate::secure_fs::open_regular_file_for_read(path) {
        Ok(source) => source,
        Err(crate::secure_fs::SecureFileError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => return Err(std::io::Error::other(error)),
    };
    let metadata = source.metadata()?;
    if metadata.len() > MAX_STAGED_ENTRYPOINT_BYTES {
        return Err(std::io::Error::other(
            "extension entrypoint exceeds the 64 MiB staging limit",
        ));
    }
    let temporary = tempfile::Builder::new()
        .prefix("ygg-extension-entrypoint-")
        .tempdir()?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("extension entrypoint has no file name"))?;
    let staged = temporary.path().join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o700);
    }
    let mut destination = options.open(&staged)?;
    let copied = std::io::copy(
        &mut Read::by_ref(&mut source).take(MAX_STAGED_ENTRYPOINT_BYTES + 1),
        &mut destination,
    )?;
    if copied > MAX_STAGED_ENTRYPOINT_BYTES {
        return Err(std::io::Error::other(
            "extension entrypoint grew beyond the 64 MiB staging limit",
        ));
    }
    destination.flush()?;
    destination.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        destination.set_permissions(std::fs::Permissions::from_mode(if executable {
            0o700
        } else {
            0o600
        }))?;
        destination.sync_all()?;
    }
    Ok(Some(ResolvedEntrypoint {
        command: staged,
        _staging: Some(temporary),
    }))
}

fn resolve_entrypoint_command(
    directory: &Path,
    entrypoint: &ExtensionEntrypoint,
) -> std::io::Result<ResolvedEntrypoint> {
    let configured = PathBuf::from(&entrypoint.command);
    if configured.is_absolute() {
        return stage_entrypoint(&configured)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "extension entrypoint is missing",
            )
        });
    }

    let local = directory.join(&configured);
    if let Some(staged) = stage_entrypoint(&local)? {
        return Ok(staged);
    }

    if configured.components().count() == 1 {
        if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                let candidate = directory.join(&configured);
                let resolved = match candidate.canonicalize() {
                    Ok(resolved) => resolved,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if let Some(staged) = stage_entrypoint(&resolved)? {
                    return Ok(staged);
                }
            }
        }
    }

    Ok(ResolvedEntrypoint {
        command: configured,
        _staging: None,
    })
}

fn negotiate_contributions_with_host_services(
    manifest: &ExtensionManifest,
    response: InitializeResponse,
    host_max_concurrent_requests: usize,
    offered_host_services: OfferedHostServices,
) -> Result<(ExtensionContributions, ExtensionNegotiatedProtocol), ExtensionRuntimeError> {
    if response.api_version != manifest.api_version {
        return Err(ExtensionRuntimeError::UnsupportedApiVersion {
            extension: response.api_version,
            host: manifest.api_version.clone(),
        });
    }

    let protocol = match manifest.api_version.as_str() {
        EXTENSION_API_VERSION_0_1 => {
            if response.protocol.is_some() {
                return Err(ExtensionRuntimeError::Protocol(
                    "API 0.1 initialize response must not include protocol negotiation".into(),
                ));
            }
            ExtensionNegotiatedProtocol::api_0_1(host_max_concurrent_requests)
        }
        EXTENSION_API_VERSION_0_2 => {
            let negotiated = response.protocol.clone().ok_or_else(|| {
                ExtensionRuntimeError::Protocol(
                    "API 0.2 initialize response requires protocol negotiation".into(),
                )
            })?;
            if negotiated.version != EXTENSION_API_VERSION_0_2 {
                return Err(ExtensionRuntimeError::UnsupportedApiVersion {
                    extension: negotiated.version,
                    host: EXTENSION_API_VERSION_0_2.to_owned(),
                });
            }
            if negotiated.limits.max_concurrent_requests == 0 {
                return Err(ExtensionRuntimeError::Protocol(
                    "negotiated max_concurrent_requests must be greater than zero".into(),
                ));
            }
            let features = negotiated.features.into_iter().collect::<BTreeSet<_>>();
            if features.len()
                != response
                    .protocol
                    .as_ref()
                    .map_or(0, |protocol| protocol.features.len())
            {
                return Err(ExtensionRuntimeError::Protocol(
                    "negotiated features contain duplicates".into(),
                ));
            }
            let mut allowed = API_0_2_REQUIRED_FEATURES
                .iter()
                .chain(API_0_2_OPTIONAL_FEATURES)
                .copied()
                .collect::<BTreeSet<_>>();
            if offered_host_services.agent_sessions {
                allowed.insert(EXTENSION_FEATURE_AGENT_SESSIONS);
            }
            if offered_host_services.approvals {
                allowed.insert(EXTENSION_FEATURE_APPROVALS);
            }
            if offered_host_services.secrets {
                allowed.insert(EXTENSION_FEATURE_SECRETS);
            }
            if let Some(feature) = features
                .iter()
                .find(|feature| !allowed.contains(feature.as_str()))
            {
                return Err(ExtensionRuntimeError::Protocol(format!(
                    "extension advertised unknown feature `{feature}`"
                )));
            }
            if let Some(feature) = API_0_2_REQUIRED_FEATURES
                .iter()
                .find(|feature| !features.contains(**feature))
            {
                return Err(ExtensionRuntimeError::Protocol(format!(
                    "extension is missing required feature `{feature}`"
                )));
            }
            if features.contains(EXTENSION_FEATURE_APPROVALS)
                && !features.contains(EXTENSION_FEATURE_POLICY_INTENTS)
            {
                return Err(ExtensionRuntimeError::Protocol(
                    "approvals negotiation requires policy_intents".into(),
                ));
            }
            let all_lifecycle = [
                methods::SESSION_STARTED,
                methods::SESSION_SETTLED,
                methods::TURN_STARTED,
                methods::TURN_SETTLED,
                methods::TOOL_STARTED,
                methods::TOOL_SETTLED,
            ]
            .into_iter()
            .collect::<BTreeSet<_>>();
            let lifecycle_events = if features.contains(EXTENSION_FEATURE_LIFECYCLE_EVENTS) {
                if negotiated.lifecycle_events.is_empty() {
                    all_lifecycle
                        .iter()
                        .map(|method| (*method).to_owned())
                        .collect()
                } else {
                    let subscribed = negotiated
                        .lifecycle_events
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                    if let Some(method) = subscribed
                        .iter()
                        .find(|method| !all_lifecycle.contains(method.as_str()))
                    {
                        return Err(ExtensionRuntimeError::Protocol(format!(
                            "unknown lifecycle subscription `{method}`"
                        )));
                    }
                    subscribed
                }
            } else {
                if !negotiated.lifecycle_events.is_empty() {
                    return Err(ExtensionRuntimeError::Protocol(
                        "lifecycle subscriptions require lifecycle_events".into(),
                    ));
                }
                BTreeSet::new()
            };
            ExtensionNegotiatedProtocol {
                version: EXTENSION_API_VERSION_0_2.to_owned(),
                features,
                max_concurrent_requests: negotiated
                    .limits
                    .max_concurrent_requests
                    .min(host_max_concurrent_requests),
                lifecycle_events,
            }
        }
        _ => unreachable!("manifest validation accepts only API 0.1 or 0.2"),
    };

    let tool_names = response
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    if !protocol.supports(EXTENSION_FEATURE_DYNAMIC_TOOLS) {
        ensure_same_contributions("tools", &manifest.contributes.tools, &tool_names)?;
    }
    validate_tool_definitions(&response.tools, &manifest.api_version)?;

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

    Ok((
        ExtensionContributions {
            tools: response.tools,
            commands: response.commands,
            hooks: manifest.contributes.hooks.clone(),
            context: manifest.contributes.context,
            ui: manifest.contributes.ui.clone(),
            tool_renderers: manifest.contributes.tool_renderers.clone(),
            notifications: manifest.contributes.notifications,
            confirmations: manifest.contributes.confirmations,
        },
        protocol,
    ))
}

fn validate_tool_definitions(
    tools: &[ToolDefinition],
    api_version: &str,
) -> Result<(), ExtensionRuntimeError> {
    if tools.len() > MAX_DYNAMIC_EXTENSION_TOOLS {
        return Err(ExtensionRuntimeError::Protocol(format!(
            "tool catalog contains {} tools; limit is {MAX_DYNAMIC_EXTENSION_TOOLS}",
            tools.len()
        )));
    }
    let mut names = BTreeSet::new();
    for tool in tools {
        if !names.insert(tool.name.clone()) {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "tool catalog contains duplicate `{}`",
                tool.name
            )));
        }
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
        if let Some(schema) = &tool.output_schema {
            if api_version != EXTENSION_API_VERSION_0_2 {
                return Err(ExtensionRuntimeError::Protocol(format!(
                    "API 0.1 tool `{}` cannot declare output_schema",
                    tool.name
                )));
            }
            validate_output_schema_definition(schema).map_err(|message| {
                ExtensionRuntimeError::Protocol(format!(
                    "tool `{}` has invalid output_schema: {message}",
                    tool.name
                ))
            })?;
        }
    }
    Ok(())
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

fn contributions_compatible(
    established: &ExtensionContributions,
    replacement: &ExtensionContributions,
    dynamic_tools: bool,
) -> bool {
    (dynamic_tools || established.tools == replacement.tools)
        && established.commands == replacement.commands
        && established.hooks == replacement.hooks
        && established.context == replacement.context
        && established.ui == replacement.ui
        && established.tool_renderers == replacement.tool_renderers
        && established.notifications == replacement.notifications
        && established.confirmations == replacement.confirmations
}

fn validate_output_schema_definition(schema: &serde_json::Value) -> Result<(), String> {
    fn visit(schema: &serde_json::Value, depth: usize) -> Result<(), String> {
        if depth > 32 {
            return Err("schema nesting exceeds 32 levels".into());
        }
        let object = schema
            .as_object()
            .ok_or_else(|| "schema nodes must be objects".to_owned())?;
        const SUPPORTED: &[&str] = &[
            "$schema",
            "title",
            "description",
            "default",
            "examples",
            "type",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "const",
            "allOf",
            "anyOf",
            "oneOf",
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
            "uniqueItems",
            "minProperties",
            "maxProperties",
        ];
        if let Some(keyword) = object
            .keys()
            .find(|keyword| !SUPPORTED.contains(&keyword.as_str()))
        {
            return Err(format!("unsupported JSON Schema keyword `{keyword}`"));
        }
        if let Some(types) = object.get("type") {
            let valid_type = |name: &str| {
                matches!(
                    name,
                    "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
                )
            };
            match types {
                serde_json::Value::String(name) if valid_type(name) => {}
                serde_json::Value::Array(names)
                    if !names.is_empty()
                        && names
                            .iter()
                            .all(|name| name.as_str().is_some_and(valid_type)) => {}
                _ => return Err("type must name one or more supported JSON types".into()),
            }
        }
        if let Some(properties) = object.get("properties") {
            for (name, property) in properties
                .as_object()
                .ok_or_else(|| "properties must be an object".to_owned())?
            {
                if name.len() > 256 {
                    return Err("property name exceeds 256 bytes".into());
                }
                visit(property, depth + 1)?;
            }
        }
        if let Some(required) = object.get("required") {
            let required = required
                .as_array()
                .ok_or_else(|| "required must be an array".to_owned())?;
            if !required.iter().all(serde_json::Value::is_string) {
                return Err("required entries must be strings".into());
            }
        }
        if let Some(additional) = object.get("additionalProperties") {
            if !additional.is_boolean() {
                visit(additional, depth + 1)?;
            }
        }
        if let Some(items) = object.get("items") {
            visit(items, depth + 1)?;
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(branches) = object.get(keyword) {
                let branches = branches
                    .as_array()
                    .filter(|branches| !branches.is_empty())
                    .ok_or_else(|| format!("{keyword} must be a non-empty array"))?;
                for branch in branches {
                    visit(branch, depth + 1)?;
                }
            }
        }
        if object.get("enum").is_some_and(|values| {
            !values.is_array() || values.as_array().is_some_and(Vec::is_empty)
        }) {
            return Err("enum must be a non-empty array".into());
        }
        for keyword in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
            if object.get(keyword).is_some_and(|value| !value.is_number()) {
                return Err(format!("{keyword} must be a number"));
            }
        }
        for keyword in [
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
            "minProperties",
            "maxProperties",
        ] {
            if object
                .get(keyword)
                .is_some_and(|value| value.as_u64().is_none())
            {
                return Err(format!("{keyword} must be a non-negative integer"));
            }
        }
        if object
            .get("uniqueItems")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err("uniqueItems must be boolean".into());
        }
        Ok(())
    }
    visit(schema, 0)
}

fn validate_structured_content(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), String> {
    struct ValidationBudget {
        remaining: usize,
    }

    impl ValidationBudget {
        fn consume(&mut self) -> Result<(), String> {
            self.remaining = self
                .remaining
                .checked_sub(1)
                .ok_or_else(|| "structured output validation budget exceeded".to_owned())?;
            Ok(())
        }
    }

    fn matches_type(value: &serde_json::Value, expected: &str) -> bool {
        match expected {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "string" => value.is_string(),
            _ => false,
        }
    }

    fn visit(
        schema: &serde_json::Value,
        value: &serde_json::Value,
        path: &str,
        depth: usize,
        budget: &mut ValidationBudget,
    ) -> Result<(), String> {
        budget.consume()?;
        if depth > 32 {
            return Err(format!("{path} exceeds validation depth"));
        }
        let object = schema
            .as_object()
            .ok_or_else(|| "validated schema node is not an object".to_owned())?;
        if let Some(expected) = object.get("type") {
            let accepted = match expected {
                serde_json::Value::String(expected) => matches_type(value, expected),
                serde_json::Value::Array(expected) => expected
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(|expected| matches_type(value, expected)),
                _ => false,
            };
            if !accepted {
                return Err(format!("{path} does not match declared type"));
            }
        }
        if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
            let mut matched = false;
            for candidate in values {
                budget.consume()?;
                if candidate == value {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(format!("{path} is not one of the declared enum values"));
            }
        }
        if object
            .get("const")
            .is_some_and(|constant| constant != value)
        {
            return Err(format!("{path} does not match const"));
        }
        if let Some(branches) = object.get("allOf").and_then(serde_json::Value::as_array) {
            for branch in branches {
                visit(branch, value, path, depth + 1, budget)?;
            }
        }
        if let Some(branches) = object.get("anyOf").and_then(serde_json::Value::as_array) {
            let mut matched = false;
            for branch in branches {
                if visit(branch, value, path, depth + 1, budget).is_ok() {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(format!("{path} does not match anyOf"));
            }
        }
        if let Some(branches) = object.get("oneOf").and_then(serde_json::Value::as_array) {
            let mut matches = 0_u8;
            for branch in branches {
                if visit(branch, value, path, depth + 1, budget).is_ok() {
                    matches = matches.saturating_add(1);
                    if matches > 1 {
                        break;
                    }
                }
            }
            if matches != 1 {
                return Err(format!("{path} does not match exactly one oneOf branch"));
            }
        }
        if let Some(value) = value.as_object() {
            let properties = object
                .get("properties")
                .and_then(serde_json::Value::as_object);
            if let Some(required) = object.get("required").and_then(serde_json::Value::as_array) {
                for required in required.iter().filter_map(serde_json::Value::as_str) {
                    budget.consume()?;
                    if !value.contains_key(required) {
                        return Err(format!("{path}.{required} is required"));
                    }
                }
            }
            for (name, child) in value {
                budget.consume()?;
                if let Some(schema) = properties.and_then(|properties| properties.get(name)) {
                    visit(schema, child, &format!("{path}.{name}"), depth + 1, budget)?;
                } else if let Some(additional) = object.get("additionalProperties") {
                    match additional {
                        serde_json::Value::Bool(false) => {
                            return Err(format!("{path}.{name} is not allowed"));
                        }
                        serde_json::Value::Object(_) => {
                            visit(
                                additional,
                                child,
                                &format!("{path}.{name}"),
                                depth + 1,
                                budget,
                            )?;
                        }
                        _ => {}
                    }
                }
            }
            let count = value.len() as u64;
            if object
                .get("minProperties")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|minimum| count < minimum)
            {
                return Err(format!("{path} has too few properties"));
            }
            if object
                .get("maxProperties")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|maximum| count > maximum)
            {
                return Err(format!("{path} has too many properties"));
            }
        }
        if let Some(value) = value.as_array() {
            if let Some(items) = object.get("items") {
                for (index, child) in value.iter().enumerate() {
                    visit(items, child, &format!("{path}[{index}]"), depth + 1, budget)?;
                }
            }
            let count = value.len() as u64;
            if object
                .get("minItems")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|minimum| count < minimum)
            {
                return Err(format!("{path} has too few items"));
            }
            if object
                .get("maxItems")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|maximum| count > maximum)
            {
                return Err(format!("{path} has too many items"));
            }
            if object
                .get("uniqueItems")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                let mut unique = HashSet::with_capacity(value.len());
                for item in value {
                    budget.consume()?;
                    let canonical = serde_json::to_vec(item)
                        .map_err(|error| format!("cannot canonicalize {path} item: {error}"))?;
                    if !unique.insert(canonical) {
                        return Err(format!("{path} contains duplicate items"));
                    }
                }
            }
        }
        if let Some(value) = value.as_str() {
            let count = value.chars().count() as u64;
            if object
                .get("minLength")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|minimum| count < minimum)
            {
                return Err(format!("{path} is shorter than minLength"));
            }
            if object
                .get("maxLength")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|maximum| count > maximum)
            {
                return Err(format!("{path} is longer than maxLength"));
            }
        }
        if let Some(number) = value.as_f64() {
            for (keyword, predicate) in [
                (
                    "minimum",
                    number
                        < object
                            .get("minimum")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(number),
                ),
                (
                    "maximum",
                    number
                        > object
                            .get("maximum")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(number),
                ),
                (
                    "exclusiveMinimum",
                    number
                        <= object
                            .get("exclusiveMinimum")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(number - 1.0),
                ),
                (
                    "exclusiveMaximum",
                    number
                        >= object
                            .get("exclusiveMaximum")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(number + 1.0),
                ),
            ] {
                if object.contains_key(keyword) && predicate {
                    return Err(format!("{path} violates {keyword}"));
                }
            }
        }
        Ok(())
    }
    let mut budget = ValidationBudget {
        remaining: MAX_SCHEMA_VALIDATION_STEPS,
    };
    visit(schema, value, "$", 0, &mut budget)
}

fn decode_tool_call_output(
    connection: &ProcessConnection,
    definition: &ToolDefinition,
    artifact_owner: Option<&str>,
    value: serde_json::Value,
) -> Result<ToolCallOutput, ExtensionRuntimeError> {
    let wire: ToolCallOutputWire = serde_json::from_value(value).map_err(|error| {
        ExtensionRuntimeError::Protocol(format!(
            "invalid `{}` response for tool `{}`: {error}",
            methods::TOOL_CALL,
            definition.name
        ))
    })?;
    let structured_content = wire.structured_content.into_option();
    let protocol = read_std_lock(&connection.protocol).clone();
    if protocol.version == EXTENSION_API_VERSION_0_1 {
        if structured_content
            .as_ref()
            .is_some_and(|structured| !structured.is_null())
        {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "API 0.1 tool `{}` returned unsupported structured_content",
                definition.name
            )));
        }
        let content = wire.content.as_str().ok_or_else(|| {
            ExtensionRuntimeError::Protocol(format!(
                "API 0.1 tool `{}` content must be a string",
                definition.name
            ))
        })?;
        let native = ToolOutput::new(content);
        return Ok(ToolCallOutput {
            content: content.to_owned(),
            is_error: wire.is_error,
            metadata: wire.metadata,
            structured_content: None,
            native_output: Some(native),
        });
    }

    if !protocol.supports(EXTENSION_FEATURE_CONTENT_PARTS) {
        return Err(ExtensionRuntimeError::Protocol(
            "API 0.2 tool result arrived without content_parts negotiation".into(),
        ));
    }
    // Admit the machine-readable fields through the native byte/depth/node
    // bounds before running extension-supplied schema logic over them.
    ToolOutput::new("")
        .try_with_details(structured_content.clone(), Some(wire.metadata.clone()))
        .map_err(|error| {
            ExtensionRuntimeError::Protocol(format!(
                "tool `{}` returned invalid output details: {error}",
                definition.name
            ))
        })?;
    let parts: Vec<ExtensionToolContentPart> =
        serde_json::from_value(wire.content).map_err(|error| {
            ExtensionRuntimeError::Protocol(format!(
                "API 0.2 tool `{}` content must be an array of typed parts: {error}",
                definition.name
            ))
        })?;
    if parts.is_empty() {
        return Err(ExtensionRuntimeError::Protocol(format!(
            "API 0.2 tool `{}` returned no content parts",
            definition.name
        )));
    }
    if parts.len() > MAX_EXTENSION_RESULT_CONTENT_PARTS {
        return Err(ExtensionRuntimeError::Protocol(format!(
            "API 0.2 tool `{}` returned {} content parts; limit is {MAX_EXTENSION_RESULT_CONTENT_PARTS}",
            definition.name,
            parts.len()
        )));
    }

    let mut native_parts = Vec::with_capacity(parts.len());
    let mut saw_text = false;
    let mut referenced_media_bytes = 0_u64;
    for part in parts {
        match part {
            ExtensionToolContentPart::Text { text } => {
                saw_text = true;
                native_parts.push(ToolOutputContentPart::Text(text));
            }
            ExtensionToolContentPart::Image {
                artifact_id,
                mime_type,
                _alt: _,
            } => {
                require_artifact_feature(&protocol, &definition.name)?;
                let artifact_owner = artifact_owner.ok_or_else(|| {
                    ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` returned an artifact without a host-owned session context",
                        definition.name
                    ))
                })?;
                let artifact_id: ArtifactId = serde_json::from_value(serde_json::Value::String(
                    artifact_id,
                ))
                .map_err(|error| {
                    ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` returned invalid artifact ID: {error}",
                        definition.name
                    ))
                })?;
                let resolved = connection
                    .artifact_store
                    .resolve_artifact_for_owner(connection.generation, artifact_owner, &artifact_id)
                    .map_err(|error| {
                        ExtensionRuntimeError::Protocol(format!(
                            "tool `{}` returned unavailable artifact: {error}",
                            definition.name
                        ))
                    })?;
                if resolved.artifact.mime_type != mime_type {
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` artifact MIME `{mime_type}` does not match verified `{}`",
                        definition.name, resolved.artifact.mime_type
                    )));
                }
                if !matches!(resolved.media, Media::Image(_)) {
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` declared a non-image artifact as image",
                        definition.name
                    )));
                }
                referenced_media_bytes = referenced_media_bytes
                    .checked_add(resolved.artifact.size)
                    .ok_or_else(|| {
                        ExtensionRuntimeError::Protocol(format!(
                            "tool `{}` media reference byte count overflowed",
                            definition.name
                        ))
                    })?;
                if referenced_media_bytes > MAX_EXTENSION_RESULT_MEDIA_BYTES {
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` referenced {referenced_media_bytes} aggregate media bytes; limit is {MAX_EXTENSION_RESULT_MEDIA_BYTES}",
                        definition.name
                    )));
                }
                native_parts.push(ToolOutputContentPart::Media(resolved.media));
            }
            ExtensionToolContentPart::Audio {
                artifact_id,
                mime_type,
                transcript,
            } => {
                require_artifact_feature(&protocol, &definition.name)?;
                let artifact_owner = artifact_owner.ok_or_else(|| {
                    ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` returned an artifact without a host-owned session context",
                        definition.name
                    ))
                })?;
                let artifact_id: ArtifactId = serde_json::from_value(serde_json::Value::String(
                    artifact_id,
                ))
                .map_err(|error| {
                    ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` returned invalid artifact ID: {error}",
                        definition.name
                    ))
                })?;
                let mut resolved = connection
                    .artifact_store
                    .resolve_artifact_for_owner(connection.generation, artifact_owner, &artifact_id)
                    .map_err(|error| {
                        ExtensionRuntimeError::Protocol(format!(
                            "tool `{}` returned unavailable artifact: {error}",
                            definition.name
                        ))
                    })?;
                if resolved.artifact.mime_type != mime_type {
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` artifact MIME `{mime_type}` does not match verified `{}`",
                        definition.name, resolved.artifact.mime_type
                    )));
                }
                let Media::Audio(audio) = &mut resolved.media else {
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` declared a non-audio artifact as audio",
                        definition.name
                    )));
                };
                referenced_media_bytes = referenced_media_bytes
                    .checked_add(resolved.artifact.size)
                    .ok_or_else(|| {
                        ExtensionRuntimeError::Protocol(format!(
                            "tool `{}` media reference byte count overflowed",
                            definition.name
                        ))
                    })?;
                if referenced_media_bytes > MAX_EXTENSION_RESULT_MEDIA_BYTES {
                    return Err(ExtensionRuntimeError::Protocol(format!(
                        "tool `{}` referenced {referenced_media_bytes} aggregate media bytes; limit is {MAX_EXTENSION_RESULT_MEDIA_BYTES}",
                        definition.name
                    )));
                }
                audio.transcript = transcript;
                native_parts.push(ToolOutputContentPart::Media(resolved.media));
            }
        }
    }
    if !saw_text {
        return Err(ExtensionRuntimeError::Protocol(format!(
            "API 0.2 tool `{}` must include an explicit compact text part",
            definition.name
        )));
    }

    match (&definition.output_schema, &structured_content) {
        (Some(schema), Some(structured)) => {
            validate_structured_content(schema, structured).map_err(|message| {
                ExtensionRuntimeError::Protocol(format!(
                    "tool `{}` structured_content failed output_schema: {message}",
                    definition.name
                ))
            })?;
        }
        (Some(_), None) if !wire.is_error => {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "tool `{}` declared output_schema but omitted structured_content",
                definition.name
            )));
        }
        (Some(_), None) => {}
        (None, Some(_)) => {
            return Err(ExtensionRuntimeError::Protocol(format!(
                "tool `{}` returned structured_content without output_schema",
                definition.name
            )));
        }
        (None, None) => {}
    }
    let native = ToolOutput::from_content_parts(native_parts)
        .try_with_details(structured_content.clone(), Some(wire.metadata.clone()))
        .map_err(|error| {
            ExtensionRuntimeError::Protocol(format!(
                "tool `{}` returned invalid output details: {error}",
                definition.name
            ))
        })?;
    Ok(ToolCallOutput {
        content: native.text.clone(),
        is_error: wire.is_error,
        metadata: wire.metadata,
        structured_content,
        native_output: Some(native),
    })
}

fn require_artifact_feature(
    protocol: &ExtensionNegotiatedProtocol,
    tool_name: &str,
) -> Result<(), ExtensionRuntimeError> {
    if protocol.supports(EXTENSION_FEATURE_ARTIFACTS) {
        Ok(())
    } else {
        Err(ExtensionRuntimeError::Protocol(format!(
            "tool `{tool_name}` returned an artifact without artifacts negotiation"
        )))
    }
}

struct ProtocolReadState {
    pending: PendingRequests,
    pending_changed: Arc<Notify>,
    closed: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    events: broadcast::Sender<ExtensionEvent>,
    generation: u64,
    max_message_bytes: usize,
    declared: ManifestContributions,
    writer: mpsc::Sender<WriterFrame>,
    child_requests: ChildRequests,
    seen_child_request_ids: StdMutex<HashSet<ExtensionRequestId>>,
    child_work_slots: Arc<Semaphore>,
    tombstones: Arc<StdMutex<RequestTombstones>>,
    protocol: Arc<StdRwLock<ExtensionNegotiatedProtocol>>,
    tool_catalog: Arc<StdRwLock<Vec<ToolDefinition>>>,
    catalog_updates: mpsc::Sender<CatalogUpdateRequest>,
    delegation_service: Arc<StdRwLock<Option<ExtensionDelegationService>>>,
    approval_store: Arc<ExtensionApprovalStore>,
    secret_broker: Option<Arc<dyn ExtensionSecretBroker>>,
    extension_identity: ExtensionIdentity,
    allowed_secrets: Arc<BTreeSet<String>>,
    health: Arc<StdRwLock<ConnectionHealth>>,
    artifact_store: ArtifactStore,
    child: Option<Arc<Mutex<Child>>>,
    termination: Option<ProcessTerminationHandle>,
}

enum AgentSessionOperation {
    Spawn {
        task_name: String,
        message: String,
        idempotency_key: String,
    },
    Message {
        target: String,
        message: String,
    },
    FollowUp {
        target: String,
        message: String,
    },
    List,
    Wait {
        timeout: Duration,
    },
    Interrupt {
        target: String,
    },
}

async fn execute_agent_session_operation(
    service: ExtensionDelegationService,
    resource_owner: String,
    operation: AgentSessionOperation,
    cancellation: CancellationToken,
) -> Result<serde_json::Value, String> {
    match operation {
        AgentSessionOperation::Spawn {
            task_name,
            message,
            idempotency_key,
        } => service.spawn(&resource_owner, task_name, message, idempotency_key),
        AgentSessionOperation::Message { target, message } => {
            service
                .send_message(&resource_owner, &target, message)
                .await
        }
        AgentSessionOperation::FollowUp { target, message } => {
            service.follow_up(&resource_owner, &target, message).await
        }
        AgentSessionOperation::List => service.list(&resource_owner),
        AgentSessionOperation::Wait { timeout } => {
            service.wait(&resource_owner, timeout, &cancellation).await
        }
        AgentSessionOperation::Interrupt { target } => {
            service.interrupt(&resource_owner, &target).await
        }
    }
}

fn queue_agent_session_operation(
    state: &ProtocolReadState,
    request_id: ExtensionRequestId,
    parent_request_id: u64,
    method: &'static str,
    operation: AgentSessionOperation,
) -> Result<(), String> {
    let Some(registered) =
        register_child_request(state, request_id.clone(), Some(parent_request_id), method)?
    else {
        return Ok(());
    };
    let response_state = registered.response_state;
    let resource_owner = registered.resource_owner.map(|owner| owner.session_id);
    let service = read_std_lock(&state.delegation_service).clone();
    let worker = match state.child_work_slots.clone().try_acquire_owned() {
        Ok(worker) => worker,
        Err(_) => {
            let delivery = try_queue_child_response(
                &state.child_requests,
                &request_id,
                &state.writer,
                state.max_message_bytes,
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":request_id,
                    "error":{
                        "code":-32000,
                        "message":format!("extension child worker limit {MAX_CHILD_WORKERS} exceeded"),
                    },
                }),
            );
            if delivery.is_err() {
                settle_child_request(&state.child_requests, &request_id);
            }
            return Ok(());
        }
    };
    let writer = state.writer.clone();
    let child_requests = Arc::clone(&state.child_requests);
    let health = Arc::clone(&state.health);
    let events = state.events.clone();
    let max_message_bytes = state.max_message_bytes;
    tokio::spawn(async move {
        let cancellation = CancellationToken::default();
        let result = if let (Some(service), Some(resource_owner)) = (service, resource_owner) {
            tokio::select! {
                result = execute_agent_session_operation(
                    service,
                    resource_owner,
                    operation,
                    cancellation.clone(),
                ) => result,
                _ = child_response_settled(Arc::clone(&response_state)) => {
                    cancellation.cancel();
                    drop(worker);
                    return;
                }
            }
        } else {
            Err("agent session service is not bound to this host-owned resource owner".to_owned())
        };
        let response = match result {
            Ok(result) => serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "result":result,
            }),
            Err(message) => serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "error":{"code":-32002,"message":message},
            }),
        };
        let delivery = try_queue_child_response(
            &child_requests,
            &request_id,
            &writer,
            max_message_bytes,
            response,
        );
        if let Err(message) = delivery {
            update_health(
                &health,
                ExtensionHealthState::Degraded,
                Some(message.clone()),
            );
            let _ = events.send(ExtensionEvent::Diagnostic { message });
            settle_child_request(&child_requests, &request_id);
        }
        drop(worker);
    });
    Ok(())
}

#[derive(Serialize)]
struct SecretResult<'a> {
    value: &'a str,
}

fn queue_secret_lookup(
    state: &ProtocolReadState,
    request_id: ExtensionRequestId,
    request: ExtensionSecretGetRequest,
) -> Result<(), String> {
    let Some(registered) = register_child_request(
        state,
        request_id.clone(),
        Some(request.parent_request_id),
        methods::SECRET_GET,
    )?
    else {
        return Ok(());
    };
    if request.name.len() > MAX_EXTENSION_SECRET_NAME_BYTES
        || !state.allowed_secrets.contains(&request.name)
    {
        try_queue_child_response(
            &state.child_requests,
            &request_id,
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "error":{"code":-32602,"message":"secret name is not declared by this extension"},
            }),
        )?;
        return Ok(());
    }
    let Some(resource_owner) = registered.resource_owner else {
        try_queue_child_response(
            &state.child_requests,
            &request_id,
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "error":{
                    "code":-32002,
                    "message":"secret lookup requires a host-owned session context",
                },
            }),
        )?;
        return Ok(());
    };
    let Some(broker) = state.secret_broker.clone() else {
        try_queue_child_response(
            &state.child_requests,
            &request_id,
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "error":{"code":-32002,"message":"secret service is unavailable"},
            }),
        )?;
        return Ok(());
    };
    let worker = match state.child_work_slots.clone().try_acquire_owned() {
        Ok(worker) => worker,
        Err(_) => {
            try_queue_child_response(
                &state.child_requests,
                &request_id,
                &state.writer,
                state.max_message_bytes,
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":request_id,
                    "error":{
                        "code":-32000,
                        "message":format!("extension child worker limit {MAX_CHILD_WORKERS} exceeded"),
                    },
                }),
            )?;
            return Ok(());
        }
    };
    let lookup = ExtensionSecretRequest {
        extension: state.extension_identity.clone(),
        resource_owner,
        parent_request_id: request.parent_request_id,
        name: request.name,
    };
    let response_state = registered.response_state;
    let child_requests = Arc::clone(&state.child_requests);
    let writer = state.writer.clone();
    let health = Arc::clone(&state.health);
    let events = state.events.clone();
    let max_message_bytes = state.max_message_bytes;
    tokio::spawn(async move {
        let result = tokio::select! {
            result = broker.get_secret(lookup) => Some(result),
            _ = child_response_settled(response_state) => None,
        };
        let Some(result) = result else {
            drop(worker);
            return;
        };
        let delivery = match result {
            Ok(Some(secret)) => {
                let result = SecretResult {
                    value: secret.as_str(),
                };
                let line = serde_json::to_vec(&ChildSuccessResponse {
                    jsonrpc: "2.0",
                    id: &request_id,
                    result: &result,
                })
                .map_err(|error| error.to_string());
                match line {
                    Ok(line) => try_queue_child_response_line(
                        &child_requests,
                        &request_id,
                        &writer,
                        max_message_bytes,
                        line,
                    ),
                    Err(error) => Err(error),
                }
            }
            Ok(None) => try_queue_child_response(
                &child_requests,
                &request_id,
                &writer,
                max_message_bytes,
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":request_id,
                    "error":{"code":-32004,"message":"secret is unavailable"},
                }),
            ),
            Err(_) => {
                let _ = events.send(ExtensionEvent::Diagnostic {
                    message: "configured extension secret broker failed a lookup".into(),
                });
                try_queue_child_response(
                    &child_requests,
                    &request_id,
                    &writer,
                    max_message_bytes,
                    serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":request_id,
                        "error":{"code":-32004,"message":"secret is unavailable"},
                    }),
                )
            }
        };
        if let Err(message) = delivery {
            update_health(
                &health,
                ExtensionHealthState::Degraded,
                Some(message.clone()),
            );
            let _ = events.send(ExtensionEvent::Diagnostic { message });
            settle_child_request(&child_requests, &request_id);
        }
        drop(worker);
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn read_protocol_stdout<R>(
    mut stdout: R,
    pending: PendingRequests,
    pending_changed: Arc<Notify>,
    closed: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    events: broadcast::Sender<ExtensionEvent>,
    generation: u64,
    max_message_bytes: usize,
    declared: ManifestContributions,
    writer: mpsc::Sender<WriterFrame>,
    child_requests: ChildRequests,
    child_work_slots: Arc<Semaphore>,
    tombstones: Arc<StdMutex<RequestTombstones>>,
    protocol: Arc<StdRwLock<ExtensionNegotiatedProtocol>>,
    tool_catalog: Arc<StdRwLock<Vec<ToolDefinition>>>,
    catalog_updates: mpsc::Sender<CatalogUpdateRequest>,
    delegation_service: Arc<StdRwLock<Option<ExtensionDelegationService>>>,
    approval_store: Arc<ExtensionApprovalStore>,
    secret_broker: Option<Arc<dyn ExtensionSecretBroker>>,
    extension_identity: ExtensionIdentity,
    allowed_secrets: Arc<BTreeSet<String>>,
    health: Arc<StdRwLock<ConnectionHealth>>,
    initialization_complete: Arc<AtomicBool>,
    initialization_changed: Arc<Notify>,
    artifact_store: ArtifactStore,
    child: Option<Arc<Mutex<Child>>>,
    termination: Option<ProcessTerminationHandle>,
) where
    R: AsyncRead + Unpin,
{
    let state = ProtocolReadState {
        pending,
        pending_changed,
        closed,
        draining,
        events,
        generation,
        max_message_bytes,
        declared,
        writer,
        child_requests,
        seen_child_request_ids: StdMutex::new(HashSet::new()),
        child_work_slots,
        tombstones,
        protocol,
        tool_catalog,
        catalog_updates,
        delegation_service,
        approval_store,
        secret_broker,
        extension_identity,
        allowed_secrets,
        health,
        artifact_store,
        child,
        termination,
    };
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
                if let Err(error) = handle_protocol_line(&line, &state) {
                    break 'stream Err(error);
                }
                line.fill(0);
                line.clear();
                while !initialization_complete.load(Ordering::Acquire) {
                    let changed = initialization_changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if initialization_complete.load(Ordering::Acquire) {
                        break;
                    }
                    changed.await;
                }
            } else {
                line.push(*byte);
                if line.len() >= state.max_message_bytes {
                    break 'stream Err(format!(
                        "stdout message exceeded {} bytes",
                        state.max_message_bytes
                    ));
                }
            }
        }
    };

    state.closed.store(true, Ordering::Release);
    let message = match result {
        Ok(()) => "extension stdout closed".to_owned(),
        Err(message) => {
            let _ = state.events.send(ExtensionEvent::Diagnostic {
                message: message.clone(),
            });
            message
        }
    };
    let health_state = if state.draining.load(Ordering::Acquire) {
        ExtensionHealthState::Stopped
    } else {
        ExtensionHealthState::Crashed
    };
    update_health(
        &state.health,
        health_state,
        (health_state == ExtensionHealthState::Crashed).then(|| message.clone()),
    );
    fail_all_pending(
        &state.pending,
        &state.pending_changed,
        PendingError::Closed(message),
    );
    if health_state == ExtensionHealthState::Crashed {
        if let (Some(child), Some(termination)) = (state.child, state.termination) {
            reap_failed_extension(child, termination).await;
        }
    }
}

fn handle_protocol_line(line: &[u8], state: &ProtocolReadState) -> Result<(), String> {
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
                require_declared(state.declared.notifications, "notifications")?;
                let notification = serde_json::from_value(params)
                    .map_err(|error| format!("invalid notification: {error}"))?;
                let _ = state
                    .events
                    .send(ExtensionEvent::Notification { notification });
            }
            methods::CONFIRMATION_REQUEST => {
                require_declared(state.declared.confirmations, "confirmations")?;
                let id = object
                    .get("id")
                    .cloned()
                    .ok_or_else(|| "confirmation request requires an id".to_owned())?;
                let request_id: ExtensionRequestId = serde_json::from_value(id)
                    .map_err(|error| format!("invalid confirmation request id: {error}"))?;
                request_id
                    .validate_confirmation_id()
                    .map_err(|error| format!("invalid confirmation request id: {error}"))?;
                let request: ConfirmationRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid confirmation request: {error}"))?;
                let Some(registered) = register_child_request(
                    state,
                    request_id.clone(),
                    request.parent_request_id,
                    methods::CONFIRMATION_REQUEST,
                )?
                else {
                    return Ok(());
                };
                let parent_request_id = registered.parent_request_id;
                let progress = registered.progress;
                let response_state = registered.response_state;
                let worker = if progress.is_some() {
                    Some(
                        state
                            .child_work_slots
                            .clone()
                            .try_acquire_owned()
                            .map_err(|_| {
                                settle_child_request(&state.child_requests, &request_id);
                                format!("extension child worker limit {MAX_CHILD_WORKERS} exceeded")
                            })?,
                    )
                } else {
                    None
                };
                if let (Some(progress), Some(_), Some(worker)) =
                    (progress, parent_request_id, worker)
                {
                    let writer = state.writer.clone();
                    let child_requests = Arc::clone(&state.child_requests);
                    let health = Arc::clone(&state.health);
                    let events = state.events.clone();
                    let max_message_bytes = state.max_message_bytes;
                    let response_id = request_id;
                    tokio::spawn(async move {
                        let confirmation = progress.confirmation(
                            request.prompt,
                            request.detail,
                            request.destructive,
                            request.default,
                        );
                        tokio::pin!(confirmation);
                        let confirmed = tokio::select! {
                            confirmed = &mut confirmation => confirmed,
                            _ = child_response_settled(Arc::clone(&response_state)) => return,
                        };
                        let response = try_queue_child_response(
                            &child_requests,
                            &response_id,
                            &writer,
                            max_message_bytes,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": response_id,
                                "result": {"confirmed": confirmed},
                            }),
                        );
                        if let Err(error) = response {
                            update_health(
                                &health,
                                ExtensionHealthState::Degraded,
                                Some(error.clone()),
                            );
                            let _ = events.send(ExtensionEvent::Diagnostic { message: error });
                            settle_child_request(&child_requests, &response_id);
                        }
                        drop(worker);
                    });
                } else {
                    state
                        .events
                        .send(ExtensionEvent::ConfirmationRequested {
                            request_id,
                            generation: state.generation,
                            parent_request_id,
                            request,
                        })
                        .map_err(|_| {
                            "confirmation request arrived without an active event subscriber"
                                .to_owned()
                        })?;
                }
            }
            methods::CONTEXT_CONTRIBUTION => {
                require_declared(state.declared.context, "context contributions")?;
                let contribution = serde_json::from_value(params)
                    .map_err(|error| format!("invalid context contribution: {error}"))?;
                let _ = state
                    .events
                    .send(ExtensionEvent::ContextContributed { contribution });
            }
            methods::STATUS_CONTRIBUTION => {
                let contribution = serde_json::from_value(params)
                    .map_err(|error| format!("invalid status contribution: {error}"))?;
                let ExtensionStatusContribution { surface, .. } = &contribution;
                require_declared(state.declared.ui.contains(surface), "UI contributions")?;
                let _ = state
                    .events
                    .send(ExtensionEvent::StatusContributed { contribution });
            }
            methods::PROGRESS => {
                require_feature(state, EXTENSION_FEATURE_REQUEST_PROGRESS)?;
                let progress: ExtensionProgressNotification = serde_json::from_value(params)
                    .map_err(|error| format!("invalid progress notification: {error}"))?;
                dispatch_progress(state, progress)?;
            }
            methods::CANCEL_REQUEST => {
                let id = params
                    .get("id")
                    .cloned()
                    .ok_or_else(|| "cancel request requires id".to_owned())?;
                let request_id: ExtensionRequestId = serde_json::from_value(id)
                    .map_err(|error| format!("invalid cancel request id: {error}"))?;
                settle_child_request(&state.child_requests, &request_id);
            }
            methods::POLICY_EVALUATE => {
                require_feature(state, EXTENSION_FEATURE_POLICY_INTENTS)?;
                let id = parse_child_request_id(object, methods::POLICY_EVALUATE)?;
                let request: ExtensionPolicyEvaluationRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid policy evaluation request: {error}"))?;
                request
                    .intent
                    .canonical_hash()
                    .map_err(|error| format!("invalid policy action intent: {error}"))?;
                let Some(registered) = register_child_request(
                    state,
                    id.clone(),
                    Some(request.parent_request_id),
                    methods::POLICY_EVALUATE,
                )?
                else {
                    return Ok(());
                };
                let parent = registered
                    .parent_request_id
                    .expect("API 0.2 child registration returns a parent ID");
                if let Some(token) = request.approval_token {
                    if !read_std_lock(&state.protocol).supports(EXTENSION_FEATURE_APPROVALS) {
                        try_queue_child_response(
                            &state.child_requests,
                            &id,
                            &state.writer,
                            state.max_message_bytes,
                            serde_json::json!({
                                "jsonrpc":"2.0",
                                "id":id,
                                "error":{
                                    "code":-32602,
                                    "message":"approval token requires negotiated approvals",
                                },
                            }),
                        )?;
                        return Ok(());
                    }
                    let approved = state
                        .approval_store
                        .consume(
                            &token,
                            &request.intent,
                            state.generation,
                            &ExtensionRequestId::Number(parent),
                        )
                        .map_err(|error| format!("invalid policy action intent: {error}"))?;
                    try_queue_child_response(
                        &state.child_requests,
                        &id,
                        &state.writer,
                        state.max_message_bytes,
                        serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":id,
                            "result":ExtensionPolicyEvaluationResponse {
                                decision: if approved {
                                    ExtensionPolicyDecision::Allow
                                } else {
                                    ExtensionPolicyDecision::Deny
                                },
                                approval_token: None,
                            },
                        }),
                    )?;
                    return Ok(());
                }
                let intent = request.intent;
                if let Some(child) = lock_std_mutex(&state.child_requests).get_mut(&id) {
                    child.policy_intent = Some(intent.clone());
                }
                state
                    .events
                    .send(ExtensionEvent::PolicyEvaluationRequested {
                        request_id: id,
                        generation: state.generation,
                        parent_request_id: parent,
                        intent,
                    })
                    .map_err(|_| {
                        "policy evaluation arrived without an active event subscriber".to_owned()
                    })?;
            }
            methods::INPUT_REQUEST => {
                if read_std_lock(&state.protocol).version != EXTENSION_API_VERSION_0_2 {
                    return Err("input/request requires API 0.2".into());
                }
                let id = parse_child_request_id(object, methods::INPUT_REQUEST)?;
                let request: ExtensionInputRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid input request: {error}"))?;
                if request.prompt.trim().is_empty() {
                    return Err("input prompt must not be empty".into());
                }
                if request.prompt.len() > MAX_EXTENSION_INPUT_PROMPT_BYTES {
                    return Err(format!(
                        "input prompt exceeded {MAX_EXTENSION_INPUT_PROMPT_BYTES} UTF-8 bytes"
                    ));
                }
                let Some(registered) = register_child_request(
                    state,
                    id.clone(),
                    Some(request.parent_request_id),
                    methods::INPUT_REQUEST,
                )?
                else {
                    return Ok(());
                };
                let progress = registered.progress;
                let response_state = registered.response_state;
                let worker = if progress.is_some() {
                    Some(
                        state
                            .child_work_slots
                            .clone()
                            .try_acquire_owned()
                            .map_err(|_| {
                                settle_child_request(&state.child_requests, &id);
                                format!("extension child worker limit {MAX_CHILD_WORKERS} exceeded")
                            })?,
                    )
                } else {
                    None
                };
                if let (Some(progress), Some(worker)) = (progress, worker) {
                    let writer = state.writer.clone();
                    let child_requests = Arc::clone(&state.child_requests);
                    let health = Arc::clone(&state.health);
                    let events = state.events.clone();
                    let max_message_bytes = state.max_message_bytes;
                    let response_id = id;
                    tokio::spawn(async move {
                        let input = progress.input(request.prompt, request.secret);
                        tokio::pin!(input);
                        let value = tokio::select! {
                            answer = &mut input => answer.and_then(|answer| {
                                let bytes = answer.as_bytes();
                                (bytes.len() <= MAX_EXTENSION_INPUT_VALUE_BYTES)
                                    .then(|| std::str::from_utf8(bytes).ok().map(str::to_owned))
                                    .flatten()
                            }),
                            _ = child_response_settled(Arc::clone(&response_state)) => return,
                        };
                        let response = try_queue_child_response(
                            &child_requests,
                            &response_id,
                            &writer,
                            max_message_bytes,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": response_id,
                                "result": {"value": value},
                            }),
                        );
                        if let Err(error) = response {
                            update_health(
                                &health,
                                ExtensionHealthState::Degraded,
                                Some(error.clone()),
                            );
                            let _ = events.send(ExtensionEvent::Diagnostic { message: error });
                            settle_child_request(&child_requests, &response_id);
                        }
                        drop(worker);
                    });
                } else {
                    try_queue_child_response(
                        &state.child_requests,
                        &id,
                        &state.writer,
                        state.max_message_bytes,
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"value": serde_json::Value::Null},
                        }),
                    )?;
                }
            }
            methods::TOOLS_REGISTER => {
                require_feature(state, EXTENSION_FEATURE_DYNAMIC_TOOLS)?;
                let id = parse_child_request_id(object, methods::TOOLS_REGISTER)?;
                let request: ToolRegistrationRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid tool registration: {error}"),
                        )
                    }
                };
                if let Err(error) =
                    validate_tool_definitions(&request.tools, EXTENSION_API_VERSION_0_2)
                {
                    return reject_unparented_child_request(state, id, error.to_string());
                }
                queue_catalog_update(state, id, CatalogMutation::Register(request.tools))?;
            }
            methods::TOOLS_UNREGISTER => {
                require_feature(state, EXTENSION_FEATURE_DYNAMIC_TOOLS)?;
                let id = parse_child_request_id(object, methods::TOOLS_UNREGISTER)?;
                let request: ToolUnregistrationRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid tool unregistration: {error}"),
                        )
                    }
                };
                if request.names.len() > MAX_DYNAMIC_EXTENSION_TOOLS {
                    return reject_unparented_child_request(
                        state,
                        id,
                        format!(
                            "tool unregistration contains {} names; limit is {MAX_DYNAMIC_EXTENSION_TOOLS}",
                            request.names.len()
                        ),
                    );
                }
                if let Err(error) = validate_identifiers("tool", &request.names, true) {
                    return reject_unparented_child_request(state, id, error.to_string());
                }
                queue_catalog_update(state, id, CatalogMutation::Unregister(request.names))?;
            }
            methods::AGENT_SPAWN => {
                require_feature(state, EXTENSION_FEATURE_AGENT_SESSIONS)?;
                let id = parse_child_request_id(object, methods::AGENT_SPAWN)?;
                let request: AgentSessionSpawnRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid agent spawn request: {error}"),
                        )
                    }
                };
                queue_agent_session_operation(
                    state,
                    id,
                    request.parent_request_id,
                    methods::AGENT_SPAWN,
                    AgentSessionOperation::Spawn {
                        task_name: request.task_name,
                        message: request.message,
                        idempotency_key: request.idempotency_key,
                    },
                )?;
            }
            methods::AGENT_MESSAGE => {
                require_feature(state, EXTENSION_FEATURE_AGENT_SESSIONS)?;
                let id = parse_child_request_id(object, methods::AGENT_MESSAGE)?;
                let request: AgentSessionMessageRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid agent message request: {error}"),
                        )
                    }
                };
                queue_agent_session_operation(
                    state,
                    id,
                    request.parent_request_id,
                    methods::AGENT_MESSAGE,
                    AgentSessionOperation::Message {
                        target: request.target,
                        message: request.message,
                    },
                )?;
            }
            methods::AGENT_FOLLOW_UP => {
                require_feature(state, EXTENSION_FEATURE_AGENT_SESSIONS)?;
                let id = parse_child_request_id(object, methods::AGENT_FOLLOW_UP)?;
                let request: AgentSessionMessageRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid agent follow-up request: {error}"),
                        )
                    }
                };
                queue_agent_session_operation(
                    state,
                    id,
                    request.parent_request_id,
                    methods::AGENT_FOLLOW_UP,
                    AgentSessionOperation::FollowUp {
                        target: request.target,
                        message: request.message,
                    },
                )?;
            }
            methods::AGENT_LIST => {
                require_feature(state, EXTENSION_FEATURE_AGENT_SESSIONS)?;
                let id = parse_child_request_id(object, methods::AGENT_LIST)?;
                let request: AgentSessionListRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid agent list request: {error}"),
                        )
                    }
                };
                queue_agent_session_operation(
                    state,
                    id,
                    request.parent_request_id,
                    methods::AGENT_LIST,
                    AgentSessionOperation::List,
                )?;
            }
            methods::AGENT_WAIT => {
                require_feature(state, EXTENSION_FEATURE_AGENT_SESSIONS)?;
                let id = parse_child_request_id(object, methods::AGENT_WAIT)?;
                let request: AgentSessionWaitRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid agent wait request: {error}"),
                        )
                    }
                };
                let timeout = Duration::from_millis(
                    request
                        .timeout_ms
                        .unwrap_or(30_000)
                        .clamp(1, MAX_EXTENSION_AGENT_WAIT_MS),
                );
                queue_agent_session_operation(
                    state,
                    id,
                    request.parent_request_id,
                    methods::AGENT_WAIT,
                    AgentSessionOperation::Wait { timeout },
                )?;
            }
            methods::AGENT_INTERRUPT => {
                require_feature(state, EXTENSION_FEATURE_AGENT_SESSIONS)?;
                let id = parse_child_request_id(object, methods::AGENT_INTERRUPT)?;
                let request: AgentSessionTargetRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid agent interrupt request: {error}"),
                        )
                    }
                };
                queue_agent_session_operation(
                    state,
                    id,
                    request.parent_request_id,
                    methods::AGENT_INTERRUPT,
                    AgentSessionOperation::Interrupt {
                        target: request.target,
                    },
                )?;
            }
            methods::SECRET_GET => {
                require_feature(state, EXTENSION_FEATURE_SECRETS)?;
                let id = parse_child_request_id(object, methods::SECRET_GET)?;
                let request: ExtensionSecretGetRequest = match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return reject_unparented_child_request(
                            state,
                            id,
                            format!("invalid secret lookup request: {error}"),
                        )
                    }
                };
                queue_secret_lookup(state, id, request)?;
            }
            methods::ARTIFACT_PUBLISH => {
                require_feature(state, EXTENSION_FEATURE_ARTIFACTS)?;
                let id = parse_child_request_id(object, methods::ARTIFACT_PUBLISH)?;
                let request: ArtifactPublishRequest = serde_json::from_value(params)
                    .map_err(|error| format!("invalid artifact publication: {error}"))?;
                let parent_request_id = request.parent_request_id;
                match artifact_publication(request) {
                    Ok(publication) => {
                        let worker =
                            state
                                .child_work_slots
                                .clone()
                                .try_acquire_owned()
                                .map_err(|_| {
                                    format!(
                                        "extension child worker limit {MAX_CHILD_WORKERS} exceeded"
                                    )
                                })?;
                        let Some(registered) = register_child_request(
                            state,
                            id.clone(),
                            Some(parent_request_id),
                            methods::ARTIFACT_PUBLISH,
                        )?
                        else {
                            return Ok(());
                        };
                        let Some(resource_owner) = registered.resource_owner else {
                            let delivery = try_queue_child_response(
                                &state.child_requests,
                                &id,
                                &state.writer,
                                state.max_message_bytes,
                                serde_json::json!({
                                    "jsonrpc":"2.0",
                                    "id":id,
                                    "error":{
                                        "code":-32002,
                                        "message":"artifact publication requires a host-owned session context",
                                    },
                                }),
                            );
                            drop(worker);
                            return delivery.map(|_| ());
                        };
                        let resource_owner = resource_owner.session_id;
                        let store = state.artifact_store.clone();
                        let writer = state.writer.clone();
                        let child_requests = Arc::clone(&state.child_requests);
                        let health = Arc::clone(&state.health);
                        let events = state.events.clone();
                        let response_id = id;
                        let max_message_bytes = state.max_message_bytes;
                        let generation = state.generation;
                        tokio::spawn(async move {
                            let publication = store
                                .publish_async_for_owner(generation, resource_owner, publication)
                                .await;
                            let published_id = publication
                                .as_ref()
                                .ok()
                                .map(|published| published.id.clone());
                            let value = match publication {
                                Ok(published) => serde_json::json!({
                                    "jsonrpc":"2.0",
                                    "id":response_id,
                                    "result":{"artifact_id":published.id.to_string()},
                                }),
                                Err(error) => serde_json::json!({
                                    "jsonrpc":"2.0",
                                    "id":response_id,
                                    "error":{
                                        "code":-32602,
                                        "message":format!("artifact publication rejected: {error}"),
                                    },
                                }),
                            };
                            let delivery = try_queue_child_response(
                                &child_requests,
                                &response_id,
                                &writer,
                                max_message_bytes,
                                value,
                            );
                            rollback_undelivered_artifact(
                                &store,
                                generation,
                                published_id.as_ref(),
                                &delivery,
                            );
                            if let Err(error) = delivery {
                                update_health(
                                    &health,
                                    ExtensionHealthState::Degraded,
                                    Some(error.clone()),
                                );
                                let _ = events.send(ExtensionEvent::Diagnostic { message: error });
                                settle_child_request(&child_requests, &response_id);
                            }
                            drop(worker);
                        });
                    }
                    Err(message) => {
                        let Some(_registered) = register_child_request(
                            state,
                            id.clone(),
                            Some(parent_request_id),
                            methods::ARTIFACT_PUBLISH,
                        )?
                        else {
                            return Ok(());
                        };
                        try_queue_child_response(
                            &state.child_requests,
                            &id,
                            &state.writer,
                            state.max_message_bytes,
                            serde_json::json!({
                                "jsonrpc":"2.0",
                                "id":id,
                                "error":{"code":-32602,"message":message},
                            }),
                        )?;
                    }
                }
            }
            _ => {
                if let Some(id) = object.get("id").cloned() {
                    let id: ExtensionRequestId = serde_json::from_value(id)
                        .map_err(|error| format!("invalid unknown-method request id: {error}"))?;
                    queue_writer_value(
                        &state.writer,
                        state.max_message_bytes,
                        serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":id,
                            "error":{
                                "code":-32601,
                                "message":format!("method not found: {method}"),
                            },
                        }),
                    )?;
                } else {
                    let _ = state.events.send(ExtensionEvent::Diagnostic {
                        message: format!("ignored unknown extension notification `{method}`"),
                    });
                }
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
    let request = {
        let mut pending = lock_std_mutex(&state.pending);
        let completed = pending.get(&id).is_some_and(|request| {
            request
                .terminal
                .compare_exchange(
                    REQUEST_ACTIVE,
                    REQUEST_COMPLETED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        });
        if completed {
            pending.remove(&id)
        } else {
            None
        }
    };
    if let Some(request) = request {
        state.pending_changed.notify_waiters();
        cancel_children_from_reader(state, id, "parent settled");
        let _ = request.sender.send(reply);
    } else if lock_std_mutex(&state.tombstones).remove(id) {
        let _ = state.events.send(ExtensionEvent::Diagnostic {
            message: format!("ignored late response for cancelled request {id}"),
        });
    } else {
        let _ = state.events.send(ExtensionEvent::Diagnostic {
            message: format!("ignored response for unknown request {id}"),
        });
    }
    Ok(())
}

fn parse_child_request_id(
    object: &serde_json::Map<String, serde_json::Value>,
    method: &str,
) -> Result<ExtensionRequestId, String> {
    let id = object
        .get("id")
        .cloned()
        .ok_or_else(|| format!("{method} requires an id"))?;
    let id = serde_json::from_value(id)
        .map_err(|error| format!("invalid {method} request id: {error}"))?;
    ExtensionRequestId::validate_confirmation_id(&id)
        .map_err(|error| format!("invalid {method} request id: {error}"))?;
    Ok(id)
}

fn require_feature(state: &ProtocolReadState, feature: &str) -> Result<(), String> {
    if read_std_lock(&state.protocol).supports(feature) {
        Ok(())
    } else {
        Err(format!("extension did not negotiate `{feature}`"))
    }
}

fn queue_catalog_update(
    state: &ProtocolReadState,
    request_id: ExtensionRequestId,
    mutation: CatalogMutation,
) -> Result<(), String> {
    let _registered = insert_child_request(state, request_id.clone(), None, None)?;
    let request = CatalogUpdateRequest {
        request_id: request_id.clone(),
        generation: state.generation,
        mutation,
        catalog: Arc::clone(&state.tool_catalog),
        writer: state.writer.clone(),
        child_requests: Arc::clone(&state.child_requests),
        max_message_bytes: state.max_message_bytes,
    };
    if state.catalog_updates.try_send(request).is_err() {
        settle_child_request(&state.child_requests, &request_id);
        queue_writer_value(
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":request_id,
                "error":{
                    "code":-32000,
                    "message":"host tool catalog update queue is full",
                },
            }),
        )?;
    }
    Ok(())
}

fn reject_unparented_child_request(
    state: &ProtocolReadState,
    request_id: ExtensionRequestId,
    message: impl Into<String>,
) -> Result<(), String> {
    let message = message.into();
    let _registered = insert_child_request(state, request_id.clone(), None, None)?;
    let delivery = try_queue_child_response(
        &state.child_requests,
        &request_id,
        &state.writer,
        state.max_message_bytes,
        serde_json::json!({
            "jsonrpc":"2.0",
            "id":request_id,
            "error":{"code":-32602,"message":message},
        }),
    );
    if delivery.is_err() {
        settle_child_request(&state.child_requests, &request_id);
    }
    delivery.map(|_| ())
}

fn register_child_request(
    state: &ProtocolReadState,
    id: ExtensionRequestId,
    parent_request_id: Option<u64>,
    method: &str,
) -> Result<Option<RegisteredChildRequest>, String> {
    if read_std_lock(&state.protocol).version == EXTENSION_API_VERSION_0_1 {
        return insert_child_request(state, id, None, None).map(Some);
    }
    let parent = parent_request_id
        .ok_or_else(|| format!("{method} requires parent_request_id in API 0.2"))?;
    let pending = lock_std_mutex(&state.pending);
    let Some(parent_request) = pending
        .get(&parent)
        .filter(|pending| pending.terminal.load(Ordering::Acquire) == REQUEST_ACTIVE)
    else {
        reserve_child_request_id(state, &id)?;
        drop(pending);
        // Parent settlement can legitimately win the wire race with an
        // extension-originated child request. Consume the child ID for this
        // generation and terminalize it when possible; this is not a fatal
        // framing or protocol violation.
        let _ = queue_writer_value(
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{
                    "code":JSON_RPC_REQUEST_CANCELLED,
                    "message":"parent request is no longer active",
                    "data":{"parent_request_id":parent},
                },
            }),
        );
        return Ok(None);
    };
    let progress = parent_request.progress.clone();
    let resource_owner = parent_request.resource_owner.clone();
    let mut registered = insert_child_request(state, id, Some(parent), progress)?;
    registered.resource_owner = resource_owner;
    drop(pending);
    Ok(Some(registered))
}

fn reserve_child_request_id(
    state: &ProtocolReadState,
    id: &ExtensionRequestId,
) -> Result<(), String> {
    let mut seen = lock_std_mutex(&state.seen_child_request_ids);
    if seen.len() >= MAX_EXTENSION_CHILD_REQUEST_IDS_PER_GENERATION {
        return Err(format!(
            "extension-originated request ID limit {MAX_EXTENSION_CHILD_REQUEST_IDS_PER_GENERATION} exceeded"
        ));
    }
    if !seen.insert(id.clone()) {
        return Err("reused extension-originated request id".into());
    }
    Ok(())
}

fn insert_child_request(
    state: &ProtocolReadState,
    id: ExtensionRequestId,
    parent_request_id: Option<u64>,
    progress: Option<ToolProgressSink>,
) -> Result<RegisteredChildRequest, String> {
    let parent = parent_request_id.unwrap_or(0);
    let mut children = lock_std_mutex(&state.child_requests);
    if children.len() >= MAX_CHILD_REQUESTS {
        return Err(format!(
            "extension-originated request limit {MAX_CHILD_REQUESTS} exceeded"
        ));
    }
    reserve_child_request_id(state, &id)?;
    let response_state = Arc::new(ChildResponseState {
        state: AtomicU8::new(CHILD_ACTIVE),
        changed: Notify::new(),
        cancel_on_response_abort: StdMutex::new(None),
    });
    match children.entry(id) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(ChildRequest {
                parent_request_id: parent,
                response_state: Arc::clone(&response_state),
                policy_intent: None,
            });
        }
        std::collections::hash_map::Entry::Occupied(_) => {
            unreachable!("seen request IDs make an occupied child entry impossible");
        }
    }
    Ok(RegisteredChildRequest {
        parent_request_id,
        progress,
        resource_owner: None,
        response_state,
    })
}

fn settle_child_request(child_requests: &ChildRequests, id: &ExtensionRequestId) -> bool {
    let child = lock_std_mutex(child_requests).remove(id);
    if let Some(child) = child {
        child
            .response_state
            .state
            .store(CHILD_SETTLED, Ordering::Release);
        child.response_state.changed.notify_waiters();
        true
    } else {
        false
    }
}

async fn child_response_settled(state: Arc<ChildResponseState>) {
    loop {
        let changed = state.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if state.state.load(Ordering::Acquire) != CHILD_ACTIVE {
            return;
        }
        changed.await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildResponseAdmission {
    Queued,
    AlreadySettled,
}

fn rollback_undelivered_artifact(
    store: &ArtifactStore,
    generation: u64,
    artifact_id: Option<&ArtifactId>,
    delivery: &Result<ChildResponseAdmission, String>,
) {
    if !matches!(delivery, Ok(ChildResponseAdmission::Queued)) {
        if let Some(artifact_id) = artifact_id {
            let _ = store.remove_artifact(generation, artifact_id);
        }
    }
}

fn try_queue_child_response(
    child_requests: &ChildRequests,
    id: &ExtensionRequestId,
    writer: &mpsc::Sender<WriterFrame>,
    max_message_bytes: usize,
    value: serde_json::Value,
) -> Result<ChildResponseAdmission, String> {
    let line = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    try_queue_child_response_line(child_requests, id, writer, max_message_bytes, line)
}

fn try_queue_child_response_line(
    child_requests: &ChildRequests,
    id: &ExtensionRequestId,
    writer: &mpsc::Sender<WriterFrame>,
    max_message_bytes: usize,
    line: Vec<u8>,
) -> Result<ChildResponseAdmission, String> {
    // A secret lookup can reach either early-settled branch below. Keep the
    // serialized response zeroizing until writer ownership is established.
    let mut line = ZeroizingBytes(line);
    let response_state = {
        let children = lock_std_mutex(child_requests);
        let Some(child) = children.get(id) else {
            return Ok(ChildResponseAdmission::AlreadySettled);
        };
        Arc::clone(&child.response_state)
    };
    if response_state
        .state
        .compare_exchange(
            CHILD_ACTIVE,
            CHILD_RESPONDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return Ok(ChildResponseAdmission::AlreadySettled);
    }
    match queue_writer_line(writer, max_message_bytes, std::mem::take(&mut line.0)) {
        Ok(()) => {
            response_state.state.store(CHILD_SETTLED, Ordering::Release);
            let mut children = lock_std_mutex(child_requests);
            if children
                .get(id)
                .is_some_and(|child| Arc::ptr_eq(&child.response_state, &response_state))
            {
                children.remove(id);
            }
            response_state.changed.notify_waiters();
            Ok(ChildResponseAdmission::Queued)
        }
        Err(error) => {
            response_state.state.store(CHILD_ACTIVE, Ordering::Release);
            response_state.changed.notify_waiters();
            Err(error)
        }
    }
}

fn cancel_children_from_reader(state: &ProtocolReadState, parent_request_id: u64, reason: &str) {
    let child_ids = cancel_active_children(&state.child_requests, parent_request_id, reason);
    for id in child_ids {
        let _ = queue_writer_value(
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":methods::CANCEL_REQUEST,
                "params":{"id":id,"reason":reason},
            }),
        );
    }
}

fn cancel_active_children(
    child_requests: &ChildRequests,
    parent_request_id: u64,
    reason: &str,
) -> Vec<ExtensionRequestId> {
    let mut children = lock_std_mutex(child_requests);
    let matching = children
        .iter()
        .filter_map(|(id, child)| {
            (child.parent_request_id == parent_request_id)
                .then_some((id.clone(), Arc::clone(&child.response_state)))
        })
        .collect::<Vec<_>>();
    let mut settled = Vec::new();
    for (id, response_state) in matching {
        match response_state.state.load(Ordering::Acquire) {
            CHILD_ACTIVE
                if response_state
                    .state
                    .compare_exchange(
                        CHILD_ACTIVE,
                        CHILD_SETTLED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok() =>
            {
                settled.push((id, response_state));
            }
            CHILD_RESPONDING => {
                *lock_std_mutex(&response_state.cancel_on_response_abort) = Some(reason.to_owned());
            }
            _ => {}
        }
    }
    for (id, response_state) in &settled {
        if children
            .get(id)
            .is_some_and(|child| Arc::ptr_eq(&child.response_state, response_state))
        {
            children.remove(id);
        }
        response_state.changed.notify_waiters();
    }
    settled.into_iter().map(|(id, _)| id).collect()
}

fn dispatch_progress(
    state: &ProtocolReadState,
    notification: ExtensionProgressNotification,
) -> Result<(), String> {
    let sink = {
        let mut pending = lock_std_mutex(&state.pending);
        let Some(request) = pending.get_mut(&notification.request_id) else {
            let _ = state.events.send(ExtensionEvent::Diagnostic {
                message: format!(
                    "ignored progress for inactive request {}",
                    notification.request_id
                ),
            });
            return Ok(());
        };
        if request
            .last_progress_sequence
            .is_some_and(|previous| notification.sequence <= previous)
        {
            let _ = state.events.send(ExtensionEvent::Diagnostic {
                message: format!(
                    "ignored non-monotonic progress sequence {} for request {}",
                    notification.sequence, notification.request_id
                ),
            });
            return Ok(());
        }
        request.last_progress_sequence = Some(notification.sequence);
        request.progress.clone()
    };
    let Some(sink) = sink else {
        return Ok(());
    };
    match notification.event {
        ExtensionProgressEvent::Status {
            mut message,
            current,
            total,
            unit,
        } => {
            if current.is_some() || total.is_some() || unit.is_some() {
                use std::fmt::Write as _;
                message.push_str(" [");
                match (current, total) {
                    (Some(current), Some(total)) => {
                        let _ = write!(message, "{current}/{total}");
                    }
                    (Some(current), None) => {
                        let _ = write!(message, "{current}");
                    }
                    (None, Some(total)) => {
                        let _ = write!(message, "total {total}");
                    }
                    (None, None) => {}
                }
                if let Some(unit) = unit {
                    if current.is_some() || total.is_some() {
                        message.push(' ');
                    }
                    message.push_str(&unit);
                }
                message.push(']');
            }
            sink.status(message);
        }
        ExtensionProgressEvent::Output {
            stream,
            encoding,
            data,
        } => {
            let bytes = match encoding {
                ExtensionProgressEncoding::Utf8 => data.into_bytes(),
                ExtensionProgressEncoding::Base64 => base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|error| format!("invalid base64 progress output: {error}"))?,
            };
            sink.output(
                match stream {
                    ExtensionProgressStream::Stdout => OutputStream::Stdout,
                    ExtensionProgressStream::Stderr => OutputStream::Stderr,
                },
                bytes,
            );
        }
    }
    Ok(())
}

fn artifact_publication(request: ArtifactPublishRequest) -> Result<ArtifactPublication, String> {
    let source = match (request.data, request.path) {
        (Some(data), None) if data.encoding == ExtensionProgressEncoding::Base64 => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.data)
                .map_err(|error| format!("invalid base64 artifact data: {error}"))?;
            ArtifactSource::Inline(bytes.into())
        }
        (Some(_), None) => return Err("inline artifact encoding must be base64".into()),
        (None, Some(path)) => ArtifactSource::ScratchPath(path),
        _ => return Err("artifact publication requires exactly one of data or path".into()),
    };
    Ok(ArtifactPublication {
        source,
        mime_type: request.mime_type,
        size: request.size,
        sha256: request.sha256,
    })
}

fn queue_writer_value(
    writer: &mpsc::Sender<WriterFrame>,
    max_message_bytes: usize,
    value: serde_json::Value,
) -> Result<(), String> {
    let line = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    queue_writer_line(writer, max_message_bytes, line)
}

fn queue_writer_line(
    writer: &mpsc::Sender<WriterFrame>,
    max_message_bytes: usize,
    mut line: Vec<u8>,
) -> Result<(), String> {
    line.push(b'\n');
    if line.len() > max_message_bytes {
        line.fill(0);
        return Err(format!("writer frame exceeded {max_message_bytes} bytes"));
    }
    writer
        .try_send(WriterFrame {
            line,
            state: Arc::new(AtomicU8::new(FRAME_QUEUED)),
            completion: None,
        })
        .map_err(|error| format!("bounded extension writer rejected frame: {error}"))
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

fn fail_all_pending(pending: &PendingRequests, pending_changed: &Notify, error: PendingError) {
    let mut pending = lock_std_mutex(pending);
    for (_, request) in pending.drain() {
        request.terminal.store(REQUEST_COMPLETED, Ordering::Release);
        let _ = request.sender.send(Err(error.clone()));
    }
    drop(pending);
    pending_changed.notify_waiters();
}

fn pending_error(error: PendingError) -> ExtensionRuntimeError {
    match error {
        PendingError::Closed(message) => ExtensionRuntimeError::Closed(message),
        PendingError::Protocol(message) => ExtensionRuntimeError::Protocol(message),
        PendingError::Cancelled(reason) => ExtensionRuntimeError::Cancelled {
            method: "request".into(),
            reason,
        },
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

    fn protocol_read_state_for_test(
        declared: ManifestContributions,
        events: broadcast::Sender<ExtensionEvent>,
    ) -> (ProtocolReadState, mpsc::Receiver<WriterFrame>) {
        let artifact_store = ArtifactStore::new().expect("artifact store");
        artifact_store.begin_generation(1).expect("generation");
        let (writer, frames) = mpsc::channel(8);
        let (catalog_updates, _catalog_update_requests) = mpsc::channel(8);
        (
            ProtocolReadState {
                pending: Arc::new(StdMutex::new(HashMap::new())),
                pending_changed: Arc::new(Notify::new()),
                closed: Arc::new(AtomicBool::new(false)),
                draining: Arc::new(AtomicBool::new(false)),
                events,
                generation: 1,
                max_message_bytes: DEFAULT_EXTENSION_MESSAGE_BYTES,
                declared,
                writer,
                child_requests: Arc::new(StdMutex::new(HashMap::new())),
                seen_child_request_ids: StdMutex::new(HashSet::new()),
                child_work_slots: Arc::new(Semaphore::new(MAX_CHILD_WORKERS)),
                tombstones: Arc::new(StdMutex::new(RequestTombstones::default())),
                protocol: Arc::new(StdRwLock::new(ExtensionNegotiatedProtocol::api_0_1(
                    DEFAULT_PENDING_REQUESTS,
                ))),
                tool_catalog: Arc::new(StdRwLock::new(Vec::new())),
                catalog_updates,
                delegation_service: Arc::new(StdRwLock::new(None)),
                approval_store: Arc::new(ExtensionApprovalStore::new()),
                secret_broker: None,
                extension_identity: ExtensionIdentity {
                    name: "test-extension".into(),
                    version: "0.1.0".into(),
                    manifest_path: PathBuf::from("/test/extension.toml"),
                    source: ExtensionSource::Explicit,
                },
                allowed_secrets: Arc::new(BTreeSet::new()),
                health: Arc::new(StdRwLock::new(ConnectionHealth {
                    state: ExtensionHealthState::Ready,
                    last_error: None,
                })),
                artifact_store,
                child: None,
                termination: None,
            },
            frames,
        )
    }

    fn insert_test_parent(
        state: &ProtocolReadState,
        id: u64,
        resource_owner: Option<ExtensionResourceOwner>,
    ) {
        let (reply, _reply_rx) = oneshot::channel();
        lock_std_mutex(&state.pending).insert(
            id,
            PendingRequest {
                sender: reply,
                terminal: Arc::new(AtomicU8::new(REQUEST_ACTIVE)),
                frame_state: Arc::new(AtomicU8::new(FRAME_WRITTEN)),
                cancellation_sent: Arc::new(AtomicBool::new(false)),
                progress: None,
                resource_owner,
                last_progress_sequence: None,
            },
        );
    }

    fn test_resource_owner(session_id: &str) -> ExtensionResourceOwner {
        ExtensionResourceOwner {
            session_id: session_id.into(),
            extension_instance_id: "instance-test".into(),
            process_generation: 1,
        }
    }

    #[derive(Clone)]
    struct RecordingSecretBroker {
        requests: Arc<StdMutex<Vec<ExtensionSecretRequest>>>,
    }

    struct UnavailableSecretBroker {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl ExtensionSecretBroker for UnavailableSecretBroker {
        async fn get_secret(
            &self,
            _request: ExtensionSecretRequest,
        ) -> Result<
            Option<crate::extension_secret::ExtensionSecretValue>,
            crate::extension_secret::ExtensionSecretError,
        > {
            if self.fail {
                Err(crate::extension_secret::ExtensionSecretError::Provider(
                    "provider detail must not cross the wire".into(),
                ))
            } else {
                Ok(None)
            }
        }
    }

    #[async_trait::async_trait]
    impl ExtensionSecretBroker for RecordingSecretBroker {
        async fn get_secret(
            &self,
            request: ExtensionSecretRequest,
        ) -> Result<
            Option<crate::extension_secret::ExtensionSecretValue>,
            crate::extension_secret::ExtensionSecretError,
        > {
            lock_std_mutex(&self.requests).push(request);
            Ok(Some(crate::extension_secret::ExtensionSecretValue::new(
                "host-secret",
            )?))
        }
    }

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
    fn old_operation_rejects_reused_parent_id_from_replacement_generation() {
        let old_operation = ExtensionOperationToken {
            generation: 4,
            parent_request_id: 2,
        };
        let replacement_event = ExtensionEvent::InputRequested {
            request_id: ExtensionRequestId::String("replacement-input".into()),
            generation: 5,
            parent_request_id: 2,
            request: ExtensionInputRequest {
                parent_request_id: 2,
                prompt: "replacement prompt".into(),
                secret: false,
            },
        };

        let ExtensionEvent::InputRequested {
            generation,
            parent_request_id,
            ..
        } = replacement_event
        else {
            unreachable!();
        };
        assert!(old_operation.owns(4, 2));
        assert!(!old_operation.owns(generation, parent_request_id));
    }

    fn child_request(parent_request_id: u64, state: u8) -> ChildRequest {
        ChildRequest {
            parent_request_id,
            response_state: Arc::new(ChildResponseState {
                state: AtomicU8::new(state),
                changed: Notify::new(),
                cancel_on_response_abort: StdMutex::new(None),
            }),
            policy_intent: None,
        }
    }

    #[test]
    fn parent_settlement_cancels_only_active_child_requests() {
        let children: ChildRequests = Arc::new(StdMutex::new(HashMap::new()));
        let active_id = ExtensionRequestId::String("active".into());
        let responding_id = ExtensionRequestId::String("responding".into());
        let unrelated_id = ExtensionRequestId::String("unrelated".into());
        lock_std_mutex(&children).insert(active_id.clone(), child_request(7, CHILD_ACTIVE));
        lock_std_mutex(&children).insert(responding_id.clone(), child_request(7, CHILD_RESPONDING));
        lock_std_mutex(&children).insert(unrelated_id.clone(), child_request(8, CHILD_ACTIVE));

        assert_eq!(
            cancel_active_children(&children, 7, "parent settled"),
            vec![active_id.clone()]
        );
        let children = lock_std_mutex(&children);
        assert!(!children.contains_key(&active_id));
        assert_eq!(
            children[&responding_id]
                .response_state
                .state
                .load(Ordering::Acquire),
            CHILD_RESPONDING
        );
        assert!(children.contains_key(&unrelated_id));
    }

    #[test]
    fn child_response_claim_restores_before_admission_and_settles_after() {
        let children: ChildRequests = Arc::new(StdMutex::new(HashMap::new()));
        let id = ExtensionRequestId::String("claim".into());
        let child = child_request(7, CHILD_RESPONDING);
        let response_state = Arc::clone(&child.response_state);
        lock_std_mutex(&children).insert(id.clone(), child);
        drop(ChildResponseClaim {
            child_requests: Arc::clone(&children),
            id: id.clone(),
            response_state: Arc::clone(&response_state),
            admitted: false,
            abort_cancel: None,
        });
        assert_eq!(response_state.state.load(Ordering::Acquire), CHILD_ACTIVE);
        assert!(lock_std_mutex(&children).contains_key(&id));

        response_state
            .state
            .store(CHILD_RESPONDING, Ordering::Release);
        let mut claim = ChildResponseClaim {
            child_requests: Arc::clone(&children),
            id: id.clone(),
            response_state: Arc::clone(&response_state),
            admitted: false,
            abort_cancel: None,
        };
        claim.mark_admitted();
        drop(claim);
        assert_eq!(response_state.state.load(Ordering::Acquire), CHILD_SETTLED);
        assert!(!lock_std_mutex(&children).contains_key(&id));
    }

    #[test]
    fn parent_cancel_during_response_claim_is_deferred_until_abort() {
        let children: ChildRequests = Arc::new(StdMutex::new(HashMap::new()));
        let id = ExtensionRequestId::String("deferred".into());
        let child = child_request(7, CHILD_RESPONDING);
        let response_state = Arc::clone(&child.response_state);
        lock_std_mutex(&children).insert(id.clone(), child);
        let (writer, mut frames) = mpsc::channel(1);
        let claim = ChildResponseClaim {
            child_requests: Arc::clone(&children),
            id: id.clone(),
            response_state: Arc::clone(&response_state),
            admitted: false,
            abort_cancel: Some((writer, DEFAULT_EXTENSION_MESSAGE_BYTES)),
        };

        assert!(cancel_active_children(&children, 7, "parent settled").is_empty());
        assert_eq!(
            response_state.state.load(Ordering::Acquire),
            CHILD_RESPONDING
        );
        drop(claim);

        assert_eq!(response_state.state.load(Ordering::Acquire), CHILD_SETTLED);
        assert!(!lock_std_mutex(&children).contains_key(&id));
        let frame = frames.try_recv().expect("deferred cancel frame");
        let cancel: serde_json::Value = serde_json::from_slice(&frame.line).expect("JSON");
        assert_eq!(cancel["method"], methods::CANCEL_REQUEST);
        assert_eq!(cancel["params"]["id"], "deferred");
    }

    #[test]
    fn cancelled_artifact_child_rolls_back_published_bytes() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrollback-fixture";
        let (events, _events_rx) = broadcast::channel(4);
        let (state, _frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        let id = ExtensionRequestId::String("artifact:cancelled".into());
        insert_child_request(&state, id.clone(), Some(7), None).expect("register child");
        let published = state
            .artifact_store
            .publish(
                1,
                ArtifactPublication {
                    source: ArtifactSource::Inline(bytes::Bytes::from_static(PNG)),
                    mime_type: "image/png".into(),
                    size: PNG.len() as u64,
                    sha256: crate::tool::content_hash(PNG),
                },
            )
            .expect("publish before cancellation wins");

        assert_eq!(
            cancel_active_children(&state.child_requests, 7, "parent cancelled"),
            vec![id.clone()]
        );
        let delivery = try_queue_child_response(
            &state.child_requests,
            &id,
            &state.writer,
            state.max_message_bytes,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "result":{"artifact_id":published.id.to_string()},
            }),
        );
        assert_eq!(delivery, Ok(ChildResponseAdmission::AlreadySettled));
        rollback_undelivered_artifact(&state.artifact_store, 1, Some(&published.id), &delivery);
        assert!(matches!(
            state.artifact_store.resolve_artifact(1, &published.id),
            Err(crate::artifact::ArtifactError::UnknownArtifact)
        ));
        state
            .artifact_store
            .publish(
                1,
                ArtifactPublication {
                    source: ArtifactSource::Inline(bytes::Bytes::from_static(PNG)),
                    mime_type: "image/png".into(),
                    size: PNG.len() as u64,
                    sha256: crate::tool::content_hash(PNG),
                },
            )
            .expect("rollback recovers publication capacity");
    }

    #[test]
    fn approval_token_retry_is_single_use_and_emits_no_second_policy_event() {
        let (events, mut events_rx) = broadcast::channel(8);
        let (state, mut frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .copied()
                .chain([
                    EXTENSION_FEATURE_POLICY_INTENTS,
                    EXTENSION_FEATURE_APPROVALS,
                ])
                .map(str::to_owned)
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        insert_test_parent(&state, 7, Some(test_resource_owner("owner-a")));
        let intent = ExtensionActionIntent {
            kind: "external_side_effect".into(),
            operation: "browser.submit_form".into(),
            target: serde_json::json!({"origin":"https://example.com"}),
            data_classes: vec!["user_text".into()],
            adapter_hints: Default::default(),
        };
        let token = state
            .approval_store
            .issue(
                &intent,
                1,
                ExtensionRequestId::Number(7),
                Duration::from_secs(30),
            )
            .expect("issue token");
        for (id, expected) in [("approved", "allow"), ("reused", "deny")] {
            let line = serde_json::to_vec(&serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":methods::POLICY_EVALUATE,
                "params":{
                    "parent_request_id":7,
                    "intent":intent,
                    "approval_token":token,
                },
            }))
            .unwrap();
            handle_protocol_line(&line, &state).expect("token retry is handled");
            let frame = frames.try_recv().expect("policy response");
            let response: serde_json::Value =
                serde_json::from_slice(&frame.line).expect("JSON response");
            assert_eq!(response["result"]["decision"], expected);
        }
        assert!(matches!(
            events_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn secret_lookup_forwards_host_owner_and_requires_owner() {
        let (events, _events_rx) = broadcast::channel(8);
        let (mut state, mut frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .copied()
                .chain([EXTENSION_FEATURE_SECRETS])
                .map(str::to_owned)
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        let requests = Arc::new(StdMutex::new(Vec::new()));
        state.secret_broker = Some(Arc::new(RecordingSecretBroker {
            requests: Arc::clone(&requests),
        }));
        state.allowed_secrets = Arc::new(BTreeSet::from(["browser.api_token".into()]));
        let owner = test_resource_owner("owner-a");
        insert_test_parent(&state, 7, Some(owner.clone()));
        let line = serde_json::to_vec(&serde_json::json!({
            "jsonrpc":"2.0",
            "id":"secret-1",
            "method":methods::SECRET_GET,
            "params":{"parent_request_id":7,"name":"browser.api_token"},
        }))
        .unwrap();
        handle_protocol_line(&line, &state).expect("secret request accepted");
        let frame = tokio::time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .expect("secret response timeout")
            .expect("secret response frame");
        let response: serde_json::Value =
            serde_json::from_slice(&frame.line).expect("JSON response");
        assert_eq!(response["result"]["value"], "host-secret");
        assert_eq!(
            lock_std_mutex(&requests).as_slice(),
            &[ExtensionSecretRequest {
                extension: state.extension_identity.clone(),
                resource_owner: owner,
                parent_request_id: 7,
                name: "browser.api_token".into(),
            }]
        );

        insert_test_parent(&state, 8, None);
        let ownerless = serde_json::to_vec(&serde_json::json!({
            "jsonrpc":"2.0",
            "id":"secret-ownerless",
            "method":methods::SECRET_GET,
            "params":{"parent_request_id":8,"name":"browser.api_token"},
        }))
        .unwrap();
        handle_protocol_line(&ownerless, &state).expect("ownerless request receives an error");
        let frame = frames.recv().await.expect("ownerless response");
        let response: serde_json::Value = serde_json::from_slice(&frame.line).expect("JSON");
        assert_eq!(response["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn unavailable_and_failed_secret_lookups_are_wire_indistinguishable() {
        let (events, _events_rx) = broadcast::channel(8);
        let (mut state, mut frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .copied()
                .chain([EXTENSION_FEATURE_SECRETS])
                .map(str::to_owned)
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        state.allowed_secrets = Arc::new(BTreeSet::from(["browser.api_token".into()]));
        let mut errors = Vec::new();
        for (parent, id, fail) in [(7, "missing", false), (8, "failed", true)] {
            insert_test_parent(&state, parent, Some(test_resource_owner("owner-a")));
            state.secret_broker = Some(Arc::new(UnavailableSecretBroker { fail }));
            let line = serde_json::to_vec(&serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":methods::SECRET_GET,
                "params":{"parent_request_id":parent,"name":"browser.api_token"},
            }))
            .unwrap();
            handle_protocol_line(&line, &state).expect("lookup receives bounded response");
            let frame = tokio::time::timeout(Duration::from_secs(1), frames.recv())
                .await
                .expect("secret error timeout")
                .expect("secret error frame");
            let response: serde_json::Value =
                serde_json::from_slice(&frame.line).expect("JSON response");
            errors.push(response["error"].clone());
        }
        assert_eq!(errors[0], errors[1]);
        assert_eq!(errors[0]["code"], -32004);
        assert_eq!(errors[0]["message"], "secret is unavailable");
    }

    #[tokio::test]
    async fn artifact_publication_requires_and_preserves_the_host_owner() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\nowner-fixture";
        let (events, _events_rx) = broadcast::channel(8);
        let (state, mut frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .copied()
                .chain([EXTENSION_FEATURE_ARTIFACTS])
                .map(str::to_owned)
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        let publication = |id: &str, parent_request_id| {
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":methods::ARTIFACT_PUBLISH,
                "params":{
                    "parent_request_id":parent_request_id,
                    "mime_type":"image/png",
                    "size":PNG.len(),
                    "sha256":crate::tool::content_hash(PNG),
                    "data":{
                        "encoding":"base64",
                        "data":base64::engine::general_purpose::STANDARD.encode(PNG),
                    },
                },
            }))
            .unwrap()
        };

        insert_test_parent(&state, 7, Some(test_resource_owner("owner-a")));
        handle_protocol_line(&publication("artifact-a", 7), &state)
            .expect("owner-scoped publication accepted");
        let frame = tokio::time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .expect("artifact response timeout")
            .expect("artifact response frame");
        let response: serde_json::Value = serde_json::from_slice(&frame.line).expect("JSON");
        let artifact_id: ArtifactId =
            serde_json::from_value(response["result"]["artifact_id"].clone()).expect("artifact id");
        assert!(state
            .artifact_store
            .resolve_artifact_for_owner(1, "owner-a", &artifact_id)
            .is_ok());
        assert!(matches!(
            state
                .artifact_store
                .resolve_artifact_for_owner(1, "owner-b", &artifact_id),
            Err(crate::artifact::ArtifactError::UnknownArtifact)
        ));

        insert_test_parent(&state, 8, None);
        handle_protocol_line(&publication("artifact-ownerless", 8), &state)
            .expect("ownerless publication receives an error");
        let frame = frames.recv().await.expect("ownerless artifact response");
        let response: serde_json::Value = serde_json::from_slice(&frame.line).expect("JSON");
        assert_eq!(response["error"]["code"], -32002);
    }

    #[test]
    fn child_request_ids_are_unique_for_the_process_generation() {
        let (events, _events_rx) = broadcast::channel(4);
        let (state, _frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        let id = ExtensionRequestId::String("py:1".into());
        register_child_request(&state, id.clone(), Some(1), methods::INPUT_REQUEST)
            .expect("first ID");
        assert!(settle_child_request(&state.child_requests, &id));
        let error = register_child_request(&state, id, Some(1), methods::INPUT_REQUEST)
            .err()
            .expect("ID reuse must fail");
        assert_eq!(error, "reused extension-originated request id");
    }

    #[test]
    fn child_arriving_after_parent_cancellation_is_terminal_not_fatal() {
        let (events, _events_rx) = broadcast::channel(4);
        let (state, mut frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        let (reply, _reply_rx) = oneshot::channel();
        lock_std_mutex(&state.pending).insert(
            7,
            PendingRequest {
                sender: reply,
                terminal: Arc::new(AtomicU8::new(REQUEST_ACTIVE)),
                frame_state: Arc::new(AtomicU8::new(FRAME_WRITTEN)),
                cancellation_sent: Arc::new(AtomicBool::new(false)),
                progress: None,
                resource_owner: None,
                last_progress_sequence: None,
            },
        );
        lock_std_mutex(&state.pending).remove(&7);

        let line = br#"{"jsonrpc":"2.0","id":"late-input","method":"input/request","params":{"parent_request_id":7,"prompt":"Too late","secret":false}}"#;
        handle_protocol_line(line, &state).expect("late child is a normal cancellation race");
        let frame = frames.try_recv().expect("terminal child response");
        let response: serde_json::Value = serde_json::from_slice(&frame.line).expect("JSON");
        assert_eq!(response["id"], "late-input");
        assert_eq!(response["error"]["code"], JSON_RPC_REQUEST_CANCELLED);
        assert!(lock_std_mutex(&state.child_requests).is_empty());
        assert!(lock_std_mutex(&state.seen_child_request_ids)
            .contains(&ExtensionRequestId::String("late-input".into())));

        let reuse = handle_protocol_line(line, &state)
            .expect_err("terminal child IDs remain consumed for the generation");
        assert_eq!(reuse, "reused extension-originated request id");
    }

    #[test]
    fn parent_settlement_cannot_overtake_child_registration() {
        let (events, _events_rx) = broadcast::channel(4);
        let (state, _frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        let (reply, _reply_rx) = oneshot::channel();
        lock_std_mutex(&state.pending).insert(
            7,
            PendingRequest {
                sender: reply,
                terminal: Arc::new(AtomicU8::new(REQUEST_ACTIVE)),
                frame_state: Arc::new(AtomicU8::new(FRAME_WRITTEN)),
                cancellation_sent: Arc::new(AtomicBool::new(false)),
                progress: None,
                resource_owner: None,
                last_progress_sequence: None,
            },
        );
        let state = Arc::new(state);
        let id = ExtensionRequestId::String("input:cancel-race".into());
        let child_map = lock_std_mutex(&state.child_requests);
        let register_state = Arc::clone(&state);
        let register_id = id.clone();
        let registration = std::thread::spawn(move || {
            register_child_request(
                &register_state,
                register_id,
                Some(7),
                methods::INPUT_REQUEST,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match state.pending.try_lock() {
                Err(std::sync::TryLockError::WouldBlock) => break,
                Err(std::sync::TryLockError::Poisoned(_)) => panic!("pending lock poisoned"),
                Ok(pending) => drop(pending),
            }
            assert!(
                Instant::now() < deadline,
                "registration did not hold the parent lock while awaiting child insertion"
            );
            std::thread::yield_now();
        }

        let cancel_state = Arc::clone(&state);
        let cancellation = std::thread::spawn(move || {
            let mut pending = lock_std_mutex(&cancel_state.pending);
            pending.remove(&7);
            drop(pending);
            cancel_active_children(&cancel_state.child_requests, 7, "parent settled")
        });
        drop(child_map);

        let response_state = registration
            .join()
            .expect("registration thread")
            .expect("register child")
            .expect("parent remains active through registration")
            .response_state;
        let cancelled = cancellation.join().expect("cancellation thread");

        assert_eq!(cancelled, vec![id.clone()]);
        assert!(!lock_std_mutex(&state.child_requests).contains_key(&id));
        assert_eq!(response_state.state.load(Ordering::Acquire), CHILD_SETTLED);
    }

    #[test]
    fn non_tool_input_fails_closed_without_an_event_consumer() {
        let (events, _events_rx) = broadcast::channel(4);
        let (state, mut frames) =
            protocol_read_state_for_test(ManifestContributions::default(), events);
        *write_std_lock(&state.protocol) = ExtensionNegotiatedProtocol {
            version: EXTENSION_API_VERSION_0_2.into(),
            features: API_0_2_REQUIRED_FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            max_concurrent_requests: 1,
            lifecycle_events: BTreeSet::new(),
        };
        let (reply, _reply_rx) = oneshot::channel();
        lock_std_mutex(&state.pending).insert(
            7,
            PendingRequest {
                sender: reply,
                terminal: Arc::new(AtomicU8::new(REQUEST_ACTIVE)),
                frame_state: Arc::new(AtomicU8::new(FRAME_WRITTEN)),
                cancellation_sent: Arc::new(AtomicBool::new(false)),
                progress: None,
                resource_owner: None,
                last_progress_sequence: None,
            },
        );
        handle_protocol_line(
            br#"{"jsonrpc":"2.0","id":"py:1","method":"input/request","params":{"parent_request_id":7,"prompt":"Token?","secret":true}}"#,
            &state,
        )
        .expect("fail-closed input response");
        let frame = frames.try_recv().expect("null response frame");
        let response: serde_json::Value = serde_json::from_slice(&frame.line).expect("JSON");
        assert_eq!(response["id"], "py:1");
        assert!(response["result"]["value"].is_null());
        assert!(lock_std_mutex(&state.child_requests).is_empty());
    }

    #[test]
    fn structured_validation_has_a_total_operation_budget() {
        let schema = serde_json::json!({
            "allOf": vec![serde_json::json!({}); MAX_SCHEMA_VALIDATION_STEPS + 1]
        });
        assert!(
            validate_structured_content(&schema, &serde_json::Value::Null)
                .expect_err("validation must be bounded")
                .contains("budget")
        );
    }

    #[test]
    fn lifecycle_reasons_are_clipped_on_a_utf8_boundary() {
        let mut reason = "🦀".repeat(MAX_LIFECYCLE_REASON_BYTES);
        truncate_utf8(&mut reason, MAX_LIFECYCLE_REASON_BYTES);
        assert!(reason.len() <= MAX_LIFECYCLE_REASON_BYTES);
        assert!(reason.is_char_boundary(reason.len()));

        let health = StdRwLock::new(ConnectionHealth {
            state: ExtensionHealthState::Ready,
            last_error: None,
        });
        update_health(
            &health,
            ExtensionHealthState::Degraded,
            Some("🦀".repeat(MAX_LIFECYCLE_REASON_BYTES)),
        );
        let health = read_std_lock(&health);
        let last_error = health.last_error.as_deref().expect("last error");
        assert!(last_error.len() <= MAX_LIFECYCLE_REASON_BYTES);
        assert!(last_error.is_char_boundary(last_error.len()));
    }

    #[test]
    fn confirmation_request_string_ids_are_bounded_before_event_delivery() {
        let (events, mut receiver) = broadcast::channel(2);
        let declared = ManifestContributions {
            confirmations: true,
            ..ManifestContributions::default()
        };
        let accepted_id = "x".repeat(MAX_CONFIRMATION_REQUEST_ID_BYTES);
        let (state, _writer_frames) = protocol_read_state_for_test(declared, events);
        let accepted = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": accepted_id,
            "method": methods::CONFIRMATION_REQUEST,
            "params": {"prompt": "Continue?"},
        }))
        .expect("serialize accepted confirmation");
        handle_protocol_line(&accepted, &state)
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
        let error = handle_protocol_line(&oversized, &state)
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
            api_version: manifest.api_version.clone(),
            tools: vec![ToolDefinition {
                name: "surprise".into(),
                description: "Undeclared".into(),
                parameters: serde_json::json!({"type": "object"}),
                output_schema: None,
            }],
            commands: vec![CommandDefinition {
                name: "checkpoint".into(),
                description: "Checkpoint".into(),
                usage: None,
            }],
            protocol: None,
        };
        assert!(matches!(
            negotiate_contributions_with_host_services(
                &manifest,
                response,
                DEFAULT_PENDING_REQUESTS,
                OfferedHostServices::default(),
            ),
            Err(ExtensionRuntimeError::Protocol(message)) if message.contains("do not match")
        ));
    }

    #[test]
    fn agent_sessions_must_be_explicitly_offered_by_the_host() {
        let manifest = ExtensionManifest::parse(
            r#"name = "agent-service"
version = "0.1.0"
api_version = "0.2"
[entrypoint]
command = "agent-service"
"#,
        )
        .unwrap();
        let response = || InitializeResponse {
            api_version: EXTENSION_API_VERSION_0_2.into(),
            tools: Vec::new(),
            commands: Vec::new(),
            protocol: Some(ExtensionProtocolResponse {
                version: EXTENSION_API_VERSION_0_2.into(),
                features: API_0_2_REQUIRED_FEATURES
                    .iter()
                    .copied()
                    .chain([EXTENSION_FEATURE_AGENT_SESSIONS])
                    .map(str::to_owned)
                    .collect(),
                limits: ExtensionProtocolLimits {
                    max_concurrent_requests: 1,
                },
                lifecycle_events: Vec::new(),
            }),
        };
        assert!(matches!(
            negotiate_contributions_with_host_services(
                &manifest,
                response(),
                DEFAULT_PENDING_REQUESTS,
                OfferedHostServices::default(),
            ),
            Err(ExtensionRuntimeError::Protocol(message)) if message.contains("agent_sessions")
        ));
        let (_, protocol) = negotiate_contributions_with_host_services(
            &manifest,
            response(),
            DEFAULT_PENDING_REQUESTS,
            OfferedHostServices {
                agent_sessions: true,
                ..OfferedHostServices::default()
            },
        )
        .unwrap();
        assert!(protocol.supports(EXTENSION_FEATURE_AGENT_SESSIONS));
    }

    #[test]
    fn approvals_and_secrets_must_be_explicitly_offered() {
        let manifest = ExtensionManifest::parse(
            r#"name = "host-services"
version = "0.1.0"
api_version = "0.2"
[entrypoint]
command = "host-services"
[capabilities]
secrets = ["browser.api_token"]
"#,
        )
        .unwrap();
        let response = |features: &[&str]| InitializeResponse {
            api_version: EXTENSION_API_VERSION_0_2.into(),
            tools: Vec::new(),
            commands: Vec::new(),
            protocol: Some(ExtensionProtocolResponse {
                version: EXTENSION_API_VERSION_0_2.into(),
                features: API_0_2_REQUIRED_FEATURES
                    .iter()
                    .copied()
                    .chain(features.iter().copied())
                    .map(str::to_owned)
                    .collect(),
                limits: ExtensionProtocolLimits {
                    max_concurrent_requests: 1,
                },
                lifecycle_events: Vec::new(),
            }),
        };
        assert!(matches!(
            negotiate_contributions_with_host_services(
                &manifest,
                response(&[EXTENSION_FEATURE_APPROVALS, EXTENSION_FEATURE_POLICY_INTENTS]),
                DEFAULT_PENDING_REQUESTS,
                OfferedHostServices::default(),
            ),
            Err(ExtensionRuntimeError::Protocol(message)) if message.contains("approvals")
        ));
        assert!(matches!(
            negotiate_contributions_with_host_services(
                &manifest,
                response(&[EXTENSION_FEATURE_SECRETS]),
                DEFAULT_PENDING_REQUESTS,
                OfferedHostServices::default(),
            ),
            Err(ExtensionRuntimeError::Protocol(message)) if message.contains("secrets")
        ));
        assert!(matches!(
            negotiate_contributions_with_host_services(
                &manifest,
                response(&[EXTENSION_FEATURE_APPROVALS]),
                DEFAULT_PENDING_REQUESTS,
                OfferedHostServices {
                    approvals: true,
                    ..OfferedHostServices::default()
                },
            ),
            Err(ExtensionRuntimeError::Protocol(message)) if message.contains("requires policy_intents")
        ));

        let (_, protocol) = negotiate_contributions_with_host_services(
            &manifest,
            response(&[
                EXTENSION_FEATURE_POLICY_INTENTS,
                EXTENSION_FEATURE_APPROVALS,
                EXTENSION_FEATURE_SECRETS,
            ]),
            DEFAULT_PENDING_REQUESTS,
            OfferedHostServices {
                approvals: true,
                secrets: true,
                ..OfferedHostServices::default()
            },
        )
        .unwrap();
        assert!(protocol.supports(EXTENSION_FEATURE_APPROVALS));
        assert!(protocol.supports(EXTENSION_FEATURE_SECRETS));
    }

    #[cfg(unix)]
    async fn lifecycle_v02_fixture(temp: &TempDir) -> (ExtensionProcess, PathBuf) {
        let script_path = temp.path().join("lifecycle-v02.py");
        let log_path = temp.path().join("lifecycle-v02.jsonl");
        write_executable_script(
            &script_path,
            r#"#!/usr/bin/env python3
import json
import os
import sys

log_path = os.path.join(os.environ["YGG_WORKSPACE"], "lifecycle-v02.jsonl")
lifecycle = {
    "session/started", "session/settled", "turn/started", "turn/settled",
    "tool/started", "tool/settled",
}

def send(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def record(message):
    value = {
        "pid": os.getpid(),
        "method": message["method"],
        "params": message.get("params"),
    }
    line = (json.dumps(value, separators=(",", ":")) + "\n").encode()
    descriptor = os.open(log_path, os.O_APPEND | os.O_CREAT | os.O_WRONLY, 0o600)
    try:
        os.write(descriptor, line)
    finally:
        os.close(descriptor)

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {
                "api_version": "0.2",
                "tools": [],
                "commands": [],
                "protocol": {
                    "version": "0.2",
                    "features": ["request_cancellation", "content_parts", "lifecycle_events"],
                    "limits": {"max_concurrent_requests": 1},
                    "lifecycle_events": sorted(lifecycle),
                },
            },
        })
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
        break
    elif method in lifecycle:
        record(message)
"#,
        );
        let manifest = ExtensionManifest::parse(
            r#"
name = "lifecycle-v02"
version = "0.2.0"
api_version = "0.2"
[entrypoint]
command = "lifecycle-v02.py"
"#,
        )
        .expect("manifest");
        let process = ExtensionProcess::start(
            trusted_descriptor(temp.path(), manifest),
            ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .expect("start lifecycle fixture");
        (process, log_path)
    }

    #[cfg(unix)]
    fn lifecycle_records(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("lifecycle JSON"))
            .collect()
    }

    #[cfg(unix)]
    fn lifecycle_methods_for_pid(records: &[serde_json::Value], pid: u32) -> Vec<&str> {
        records
            .iter()
            .filter(|record| record["pid"].as_u64() == Some(u64::from(pid)))
            .map(|record| record["method"].as_str().expect("lifecycle method"))
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_preserves_each_generation_lifecycle_order() {
        let temp = TempDir::new().expect("tempdir");
        let (process, log_path) = lifecycle_v02_fixture(&temp).await;
        let old_connection = read_std_lock(&process.inner.connection).clone();
        let old_pid = old_connection.child.lock().await.id().expect("old pid");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::SessionStarted {
                session_id: "session".into(),
                run_id: Some("run".into()),
            })
            .await
            .expect("session start");
        process.set_active_lifecycle_turn("owner", "session", "run", "turn");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::TurnStarted {
                session_id: "session".into(),
                run_id: "run".into(),
                turn_id: "turn".into(),
            })
            .await
            .expect("turn start");
        process.on_event(&AgentEvent::ToolStarted {
            id: ygg_ai::ToolCallId("tool-call".into()),
            name: "observed".into(),
            args: serde_json::json!({}),
        });

        let admission = old_connection
            .acquire_request_admission()
            .expect("hold admitted request in drain window");
        let reload_process = process.clone();
        let reload = tokio::spawn(async move { reload_process.reload().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !old_connection.draining.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reload never began drain");
        process.on_event(&AgentEvent::ToolFinished {
            id: ygg_ai::ToolCallId("tool-call".into()),
            result: Ok(ToolOutput::new("natural completion during drain")),
        });
        drop(admission);
        tokio::time::timeout(Duration::from_secs(3), reload)
            .await
            .expect("reload timed out")
            .expect("reload task failed")
            .expect("reload");
        let new_connection = read_std_lock(&process.inner.connection).clone();
        let new_pid = new_connection.child.lock().await.id().expect("new pid");
        assert_ne!(old_pid, new_pid);
        process.on_event(&AgentEvent::ToolFinished {
            id: ygg_ai::ToolCallId("tool-call".into()),
            result: Ok(ToolOutput::new("duplicate late completion")),
        });
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::TurnSettled {
                session_id: "session".into(),
                run_id: "run".into(),
                turn_id: "turn".into(),
                outcome: ExtensionLifecycleOutcome::Completed,
                duration_ms: 1,
                reason: None,
            })
            .await
            .expect("replacement turn terminal");
        process.clear_active_lifecycle_turn("owner", "turn");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::SessionSettled {
                session_id: "session".into(),
                run_id: Some("run".into()),
                outcome: ExtensionLifecycleOutcome::Completed,
                duration_ms: 2,
                reason: None,
            })
            .await
            .expect("replacement session terminal");
        assert!(process.shutdown().await);

        let records = lifecycle_records(&log_path);
        assert_eq!(
            lifecycle_methods_for_pid(&records, old_pid),
            [
                "session/started",
                "turn/started",
                "tool/started",
                "tool/settled",
                "turn/settled",
                "session/settled",
            ]
        );
        assert_eq!(
            lifecycle_methods_for_pid(&records, new_pid),
            [
                "session/started",
                "turn/started",
                "turn/settled",
                "session/settled",
            ]
        );
        let old_terminal_outcomes = records
            .iter()
            .filter(|record| {
                record["pid"].as_u64() == Some(u64::from(old_pid))
                    && record["method"]
                        .as_str()
                        .is_some_and(|method| method.ends_with("/settled"))
            })
            .map(|record| record["params"]["outcome"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            old_terminal_outcomes,
            [Some("completed"), Some("interrupted"), Some("interrupted")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delayed_turn_start_after_reload_is_delivered_once_to_replacement() {
        let temp = TempDir::new().expect("tempdir");
        let (process, log_path) = lifecycle_v02_fixture(&temp).await;
        let old_connection = read_std_lock(&process.inner.connection).clone();
        let old_pid = old_connection.child.lock().await.id().expect("old pid");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::SessionStarted {
                session_id: "session".into(),
                run_id: Some("run".into()),
            })
            .await
            .expect("session start");
        process.set_active_lifecycle_turn("owner", "session", "run", "delayed-turn");

        process
            .reload()
            .await
            .expect("reload before start delivery");
        let new_connection = read_std_lock(&process.inner.connection).clone();
        let new_pid = new_connection.child.lock().await.id().expect("new pid");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::TurnStarted {
                session_id: "session".into(),
                run_id: "run".into(),
                turn_id: "delayed-turn".into(),
            })
            .await
            .expect("delayed start is once-suppressed");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::TurnSettled {
                session_id: "session".into(),
                run_id: "run".into(),
                turn_id: "delayed-turn".into(),
                outcome: ExtensionLifecycleOutcome::Completed,
                duration_ms: 1,
                reason: None,
            })
            .await
            .expect("replacement turn terminal");
        process.clear_active_lifecycle_turn("owner", "delayed-turn");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::SessionSettled {
                session_id: "session".into(),
                run_id: Some("run".into()),
                outcome: ExtensionLifecycleOutcome::Completed,
                duration_ms: 2,
                reason: None,
            })
            .await
            .expect("replacement session terminal");
        assert!(process.shutdown().await);

        let records = lifecycle_records(&log_path);
        assert_eq!(
            lifecycle_methods_for_pid(&records, old_pid),
            ["session/started", "session/settled"],
            "old generation must not receive an unmatched turn terminal"
        );
        assert_eq!(
            lifecycle_methods_for_pid(&records, new_pid),
            [
                "session/started",
                "turn/started",
                "turn/settled",
                "session/settled",
            ],
            "replacement must receive exactly one turn start"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn settled_turn_is_consumed_before_reload_can_handoff_lifecycle() {
        let temp = TempDir::new().expect("tempdir");
        let (process, log_path) = lifecycle_v02_fixture(&temp).await;
        let old_connection = read_std_lock(&process.inner.connection).clone();
        let old_pid = old_connection.child.lock().await.id().expect("old pid");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::SessionStarted {
                session_id: "session".into(),
                run_id: Some("run".into()),
            })
            .await
            .expect("session start");
        process.set_active_lifecycle_turn("owner", "session", "run", "settled-turn");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::TurnStarted {
                session_id: "session".into(),
                run_id: "run".into(),
                turn_id: "settled-turn".into(),
            })
            .await
            .expect("turn start");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::TurnSettled {
                session_id: "session".into(),
                run_id: "run".into(),
                turn_id: "settled-turn".into(),
                outcome: ExtensionLifecycleOutcome::Completed,
                duration_ms: 1,
                reason: None,
            })
            .await
            .expect("turn terminal");

        process.reload().await.expect("reload after terminal");
        let new_connection = read_std_lock(&process.inner.connection).clone();
        let new_pid = new_connection.child.lock().await.id().expect("new pid");
        process.clear_active_lifecycle_turn("owner", "settled-turn");
        process
            .notify_lifecycle(&ExtensionLifecycleEvent::SessionSettled {
                session_id: "session".into(),
                run_id: Some("run".into()),
                outcome: ExtensionLifecycleOutcome::Completed,
                duration_ms: 2,
                reason: None,
            })
            .await
            .expect("replacement session terminal");
        assert!(process.shutdown().await);

        let records = lifecycle_records(&log_path);
        assert_eq!(
            lifecycle_methods_for_pid(&records, old_pid),
            [
                "session/started",
                "turn/started",
                "turn/settled",
                "session/settled",
            ]
        );
        assert_eq!(
            lifecycle_methods_for_pid(&records, new_pid),
            ["session/started", "session/settled"],
            "reload must not duplicate or resurrect a settled turn"
        );
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
                ..
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
    async fn explicit_null_structured_content_is_validated_and_preserved() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("null-structured.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.2","tools":[{"name":"null_result","description":"Return explicit null","parameters":{"type":"object"},"output_schema":{"type":"null"}}],"commands":[],"protocol":{"version":"0.2","features":["request_cancellation","content_parts"],"limits":{"max_concurrent_requests":1}}}}'
IFS= read -r tool_call
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"explicit null"}],"is_error":false,"structured_content":null,"metadata":null}}'
IFS= read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{}}'
"#,
        );
        let manifest = ExtensionManifest::parse(
            r#"
name = "null-structured"
version = "0.2.0"
api_version = "0.2"
[entrypoint]
command = "null-structured.sh"
[contributes]
tools = ["null_result"]
"#,
        )
        .expect("manifest");
        let process = ExtensionProcess::start(
            trusted_descriptor(temp.path(), manifest),
            ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .expect("start process");

        let output = process
            .call_tool(
                "null_result",
                serde_json::json!({}),
                process.current_context(),
            )
            .await
            .expect("null output satisfies type:null schema");
        assert_eq!(output.structured_content, Some(serde_json::Value::Null));
        assert!(process.shutdown().await);
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
    async fn drain_waits_for_request_between_admission_gate_and_pending_insert() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("admission-drain.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
IFS= read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
"#,
        );
        let descriptor = trusted_descriptor(
            temp.path(),
            minimal_manifest("admission-drain", "admission-drain.sh"),
        );
        let process = ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
            .await
            .expect("start process");
        let connection = read_std_lock(&process.inner.connection).clone();
        let admission = connection
            .acquire_request_admission()
            .expect("admit before drain");
        let drain_connection = Arc::clone(&connection);
        let mut drain =
            tokio::spawn(async move { drain_connection.drain(Duration::from_secs(1)).await });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut drain)
                .await
                .is_err(),
            "drain returned while an admitted request had not inserted pending state"
        );
        drop(admission);
        assert!(tokio::time::timeout(Duration::from_secs(1), drain)
            .await
            .expect("drain did not observe admission release")
            .expect("drain task failed"));
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
    async fn timed_out_framed_write_does_not_corrupt_the_connection() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("blocked-stdin.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
sleep 30
"#,
        );
        let descriptor = trusted_descriptor(
            temp.path(),
            minimal_manifest("blocked-stdin", "blocked-stdin.sh"),
        );
        let mut config = ExtensionRuntimeConfig::new(temp.path());
        config.max_message_bytes = 5 * 1024 * 1024;
        config.shutdown_timeout = Duration::from_millis(50);
        let process = ExtensionProcess::start(descriptor, config)
            .await
            .expect("start process");
        let connection = read_std_lock(&process.inner.connection).clone();

        let error = connection
            .request(
                "probe/large",
                serde_json::json!({"payload": "x".repeat(4 * 1024 * 1024)}),
                Duration::from_millis(50),
            )
            .await
            .expect_err("blocked framed write must time out");
        assert!(matches!(error, ExtensionRuntimeError::Timeout { .. }));
        assert!(!connection.closed.load(Ordering::Acquire));
        assert!(lock_std_mutex(&connection.pending).is_empty());
        connection.terminate().await;
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
    #[test]
    fn process_scan_does_not_mutate_a_later_registration() {
        let process_group_id = 42;
        let root = ProcessIdentity {
            pid: process_group_id,
            start_time: 200,
        };
        let descendant = ProcessIdentity {
            pid: 43,
            start_time: 201,
        };
        let mut registered = BTreeMap::from([(
            process_group_id,
            RegisteredProcessGroup {
                kind: RegisteredProcessKind::Bash,
                registration_id: 8,
                root: Some(root),
                original_group_active: true,
                descendants: BTreeMap::from([(descendant.pid, descendant)]),
                detached_bash: None,
            },
        )]);
        let registrations_at_snapshot_start = BTreeMap::from([(
            process_group_id,
            RegisteredProcessScanState {
                registration_id: 7,
                direct_bash_child_owned: true,
            },
        )]);
        let unrelated = ProcessSnapshot {
            identity: ProcessIdentity {
                pid: 900,
                start_time: 1,
            },
            parent_pid: 1,
            process_group_id: 900,
        };
        let snapshots_by_pid = BTreeMap::from([(unrelated.identity.pid, unrelated)]);

        apply_process_snapshots(
            &mut registered,
            &registrations_at_snapshot_start,
            &snapshots_by_pid,
        );

        let entry = registered.get(&process_group_id).expect("registration");
        assert!(entry.original_group_active);
        assert_eq!(entry.root, Some(root));
        assert_eq!(entry.descendants.get(&descendant.pid), Some(&descendant));
    }

    #[cfg(unix)]
    #[test]
    fn process_scan_keeps_a_directly_owned_group_bound_during_handoff() {
        let process_group_id = 42;
        let root = ProcessIdentity {
            pid: process_group_id,
            start_time: 200,
        };
        let mut registered = BTreeMap::from([(
            process_group_id,
            RegisteredProcessGroup {
                kind: RegisteredProcessKind::Bash,
                registration_id: 8,
                root: Some(root),
                original_group_active: true,
                descendants: BTreeMap::new(),
                detached_bash: Some(DetachedBashSupervision {
                    deadline: Instant::now() + Duration::from_secs(1),
                    cancellation: CancellationToken::default(),
                }),
            },
        )]);
        let registrations_at_snapshot_start = BTreeMap::from([(
            process_group_id,
            RegisteredProcessScanState {
                registration_id: 8,
                direct_bash_child_owned: true,
            },
        )]);
        let unrelated = ProcessSnapshot {
            identity: ProcessIdentity {
                pid: 900,
                start_time: 1,
            },
            parent_pid: 1,
            process_group_id: 900,
        };
        let snapshots_by_pid = BTreeMap::from([(unrelated.identity.pid, unrelated)]);

        apply_process_snapshots(
            &mut registered,
            &registrations_at_snapshot_start,
            &snapshots_by_pid,
        );

        let entry = registered.get(&process_group_id).expect("registration");
        assert!(entry.original_group_active);
    }

    #[cfg(unix)]
    #[test]
    fn process_scan_does_not_keep_an_extension_group_bound_without_a_member() {
        let process_group_id = 42;
        let root = ProcessIdentity {
            pid: process_group_id,
            start_time: 200,
        };
        let mut registered = BTreeMap::from([(
            process_group_id,
            RegisteredProcessGroup {
                kind: RegisteredProcessKind::Extension,
                registration_id: 8,
                root: Some(root),
                original_group_active: true,
                descendants: BTreeMap::new(),
                detached_bash: None,
            },
        )]);
        let registrations_at_snapshot_start = BTreeMap::from([(
            process_group_id,
            RegisteredProcessScanState {
                registration_id: 8,
                direct_bash_child_owned: false,
            },
        )]);
        let unrelated = ProcessSnapshot {
            identity: ProcessIdentity {
                pid: 900,
                start_time: 1,
            },
            parent_pid: 1,
            process_group_id: 900,
        };
        let snapshots_by_pid = BTreeMap::from([(unrelated.identity.pid, unrelated)]);

        apply_process_snapshots(
            &mut registered,
            &registrations_at_snapshot_start,
            &snapshots_by_pid,
        );

        let entry = registered.get(&process_group_id).expect("registration");
        assert!(!entry.original_group_active);
        assert!(entry.descendants.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pid_start_time_prevents_signaling_a_different_process_identity() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn identity fixture");
        let pid = i32::try_from(child.id()).expect("fixture pid");
        let identity = process_identity(pid).expect("read fixture identity");
        let stale_identity = ProcessIdentity {
            start_time: identity.start_time.saturating_add(1),
            ..identity
        };

        signal_identity(stale_identity, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            child.try_wait().expect("inspect fixture").is_none(),
            "a stale PID identity signaled the replacement process"
        );

        signal_identity(identity, libc::SIGKILL);
        child.wait().expect("reap fixture");
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
descendant_marker="$YGG_WORKSPACE/graceful-descendant.pid"
python3 -c 'import os,sys,time; os.setsid(); open(sys.argv[1], "w").write(str(os.getpid())); time.sleep(30)' "$descendant_marker" &
attempts=0
while [ ! -s "$descendant_marker" ]; do
  attempts=$((attempts + 1))
  [ "$attempts" -lt 100 ] || exit 24
  sleep 0.01
done
sleep 0.1
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

    #[cfg(unix)]
    #[tokio::test]
    async fn fatal_stdout_protocol_error_terminates_and_reaps_the_child_group() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("fatal-stdout.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
sleep 0.05
printf '%s\n' 'not-json'
sleep 30
"#,
        );
        let descriptor = trusted_descriptor(
            temp.path(),
            minimal_manifest("fatal-stdout", "fatal-stdout.sh"),
        );
        let process = ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
            .await
            .expect("initial handshake completes before fatal frame");
        let connection = read_std_lock(&process.inner.connection).clone();
        let pid = connection.child.lock().await.id().expect("child pid");

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let reaped = connection
                    .child
                    .lock()
                    .await
                    .try_wait()
                    .expect("inspect child")
                    .is_some();
                if reaped {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fatal child was not reaped");
        assert!(connection.closed.load(Ordering::Acquire));
        assert!(!process_group_registered_for_test(pid as i32));
        assert!(!process_id_exists(pid as i32));
        process.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_restarts_after_a_crash_and_explicit_shutdown_stops_revival() {
        let temp = TempDir::new().expect("tempdir");
        let script_path = temp.path().join("restart-once.sh");
        write_executable_script(
            &script_path,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
marker="$YGG_WORKSPACE/restarted.marker"
if [ ! -f "$marker" ]; then
  printf '%s\n' first > "$marker"
  sleep 0.05
  exit 17
fi
IFS= read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
"#,
        );
        let descriptor = trusted_descriptor(
            temp.path(),
            minimal_manifest("restart-once", "restart-once.sh"),
        );
        let process = ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
            .await
            .expect("initial generation");
        let first = read_std_lock(&process.inner.connection).clone();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let health = process.health_snapshot();
                if health.generation >= 2 && health.state == ExtensionHealthState::Ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervisor did not activate a replacement");
        assert!(first.closed.load(Ordering::Acquire));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("restarted.marker")).unwrap(),
            "first\n"
        );

        assert!(process.shutdown().await);
        let stopped_generation = process.health_snapshot().generation;
        tokio::time::sleep(SUPERVISOR_BASE_BACKOFF + SUPERVISOR_POLL).await;
        assert_eq!(process.health_snapshot().generation, stopped_generation);
        assert_eq!(
            process.health_snapshot().state,
            ExtensionHealthState::Stopped
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_subprocess_catalog_registers_replaces_and_unregisters_while_host_is_idle() {
        let temp = TempDir::new().unwrap();
        let script_path = temp.path().join("dynamic.py");
        write_executable_script(
            &script_path,
            r#"#!/usr/bin/env python3
import json
import sys

def receive():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit(90)
    return json.loads(line)

def send(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

initialize = receive()
send({
    "jsonrpc": "2.0",
    "id": initialize["id"],
    "result": {
        "api_version": "0.2",
        "tools": [],
        "commands": [],
        "protocol": {
            "version": "0.2",
            "features": ["request_cancellation", "content_parts", "dynamic_tools"],
            "limits": {"max_concurrent_requests": 1},
        },
    },
})

def definition(description):
    return {
        "name": "live_echo",
        "description": description,
        "parameters": {"type": "object", "additionalProperties": False},
    }

send({
    "jsonrpc": "2.0",
    "id": "catalog-1",
    "method": "tools/register",
    "params": {"tools": [definition("revision one")]},
})
ack = receive()
assert ack["result"] == {"revision": 1, "tools": ["live_echo"]}, ack

first = receive()
assert first["method"] == "tool/call", first
assert first["params"]["catalog_revision"] == 1, first
send({
    "jsonrpc": "2.0",
    "id": first["id"],
    "result": {
        "content": [{"type": "text", "text": "revision one"}],
        "is_error": False,
        "metadata": {},
    },
})

send({
    "jsonrpc": "2.0",
    "id": "catalog-2",
    "method": "tools/register",
    "params": {"tools": [definition("revision two")]},
})
ack = receive()
assert ack["result"] == {"revision": 2, "tools": ["live_echo"]}, ack

second = receive()
assert second["method"] == "tool/call", second
assert second["params"]["catalog_revision"] == 2, second
send({
    "jsonrpc": "2.0",
    "id": second["id"],
    "result": {
        "content": [{"type": "text", "text": "revision two"}],
        "is_error": False,
        "metadata": {},
    },
})

send({
    "jsonrpc": "2.0",
    "id": "catalog-3",
    "method": "tools/unregister",
    "params": {"names": ["live_echo"]},
})
ack = receive()
assert ack["result"] == {"revision": 3, "tools": []}, ack

shutdown = receive()
assert shutdown["method"] == "shutdown", shutdown
send({"jsonrpc": "2.0", "id": shutdown["id"], "result": {}})
"#,
        );
        let manifest = ExtensionManifest::parse(
            r#"name = "dynamic"
version = "0.2.0"
api_version = "0.2"
[entrypoint]
command = "dynamic.py"
"#,
        )
        .unwrap();
        let process = ExtensionProcess::start(
            trusted_descriptor(temp.path(), manifest),
            ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .unwrap();
        let mut host = ExtensionHost::new();
        host.load(&process);
        host.finalize_tool_surface();

        let initial_publication = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let definitions = host.tool_definitions();
                if definitions
                    .iter()
                    .any(|definition| definition.description == "revision one")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if initial_publication.is_err() {
            let mut events = process.subscribe();
            let mut diagnostics = Vec::new();
            while let Ok(event) = events.try_recv() {
                diagnostics.push(format!("{event:?}"));
            }
            panic!(
                "initial live catalog did not publish while the host was idle; health={:?}; events={diagnostics:?}",
                process.health_snapshot()
            );
        }
        assert_eq!(
            process
                .call_tool(
                    "live_echo",
                    serde_json::json!({}),
                    process.current_context()
                )
                .await
                .unwrap()
                .content,
            "revision one"
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let definitions = host.tool_definitions();
                if definitions
                    .iter()
                    .any(|definition| definition.description == "revision two")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement live catalog did not publish");
        assert_eq!(
            process
                .call_tool(
                    "live_echo",
                    serde_json::json!({}),
                    process.current_context()
                )
                .await
                .unwrap()
                .content,
            "revision two"
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            while !host.tool_definitions().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("live tool did not unregister");
        assert!(process.tool_definitions().is_empty());
        assert!(process.shutdown().await);
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
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":"from extension","is_error":false,"metadata":null,"structured_content":null}}'
IFS= read -r confirmation_response
IFS= read -r shutdown
case "$shutdown" in
  *'"method":"shutdown"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{}}' ;;
  *) exit 23 ;;
esac
"#
    }
}
