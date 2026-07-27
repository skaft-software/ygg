//! Path-free repository and folder-instruction context for a trusted project.
//!
//! This module deliberately accepts only an opaque project ID plus the
//! authoritative private [`ProjectRegistry`]. It resolves a server-owned
//! [`ProjectRoot`], revalidates trust after refresh, and never accepts or
//! serializes a browser-supplied host path.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::project_registry::{ProjectId, ProjectRegistry, ProjectRegistryError, ProjectRoot};

/// Maximum bytes read from one `AGENTS.md`, matching Ygg's runtime loader.
pub const MAX_INSTRUCTION_FILE_BYTES: u64 = 256 * 1024;
/// Maximum aggregate bytes accepted from discovered instruction files.
pub const MAX_INSTRUCTION_TOTAL_BYTES: u64 = 512 * 1024;
/// Maximum instruction bytes exposed to a client from one file.
pub const MAX_VISIBLE_INSTRUCTION_FILE_BYTES: usize = 32 * 1024;
/// Maximum aggregate instruction bytes exposed to a client.
pub const MAX_VISIBLE_INSTRUCTION_TOTAL_BYTES: usize = 128 * 1024;
/// Maximum discovered instruction files returned by one refresh.
pub const MAX_INSTRUCTION_FILES: usize = 128;
/// Maximum safe load errors returned by one refresh.
pub const MAX_INSTRUCTION_ERRORS: usize = 128;
/// Maximum filesystem entries inspected while discovering instructions.
pub const MAX_INSTRUCTION_WALK_ENTRIES: usize = 50_000;
/// Maximum folder depth inspected below the project root.
pub const MAX_INSTRUCTION_WALK_DEPTH: usize = 32;
/// Maximum project-relative origin length exposed to a client.
pub const MAX_PUBLIC_ORIGIN_BYTES: usize = 2_048;
/// Maximum characters in one instruction summary.
pub const MAX_INSTRUCTION_SUMMARY_CHARS: usize = 240;
/// Maximum output retained from `git status`.
pub const MAX_GIT_STATUS_BYTES: usize = 1024 * 1024;
/// Default hard deadline for each read-only Git subprocess.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(3);

const MAX_GIT_ROOT_BYTES: usize = 16 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 16 * 1024;
const MAX_PUBLIC_BRANCH_BYTES: usize = 512;
const AGENTS_FILE_NAME: &str = "AGENTS.md";

/// Explicit authorization represented by a successful context snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryTrust {
    /// The private registry verified an active, unchanged, trusted root.
    Verified,
}

/// Coarse result of one bounded refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextRefreshState {
    /// All requested context was refreshed within its bounds.
    Current,
    /// Safe partial context is available, with truncation or load errors.
    Partial,
    /// The context does not apply to this project.
    NotApplicable,
    /// The context source was unavailable or failed validation.
    Unavailable,
    /// The context source exceeded its execution deadline.
    TimedOut,
}

/// Bounded, path-free refresh metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRefreshStatus {
    /// Refresh outcome.
    pub state: ContextRefreshState,
    /// Wall-clock time at which refresh completed.
    pub refreshed_at_unix_ms: u64,
    /// Bounded elapsed refresh time.
    pub duration_ms: u64,
    /// Whether a discovery, content, or output bound omitted information.
    pub truncated: bool,
}

/// Server implementation used to obtain Git state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryStateSource {
    /// A bounded, read-only `git status --porcelain=v2` subprocess.
    GitStatusPorcelainV2,
}

/// Safe worktree classification without a host path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GitWorktreeState {
    /// The exact trusted project root is a Git worktree root.
    Present,
    /// The exact trusted project root has no repository metadata.
    NotRepository,
    /// Repository metadata exists but could not be safely validated.
    Unknown,
}

/// Safe branch classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GitBranchState {
    /// A bounded branch name is available.
    Named,
    /// `HEAD` is detached.
    Detached,
    /// The named branch has no commit yet.
    Unborn,
    /// Branch state could not be refreshed.
    Unknown,
}

/// Public Git context for one trusted project root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitRepositoryContext {
    /// Provenance of this context.
    pub source: RepositoryStateSource,
    /// Refresh outcome and freshness.
    pub refresh: ContextRefreshStatus,
    /// Whether the exact project root is a worktree.
    pub worktree: GitWorktreeState,
    /// Validated hexadecimal `HEAD` object ID, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Branch classification.
    pub branch_state: GitBranchState,
    /// Bounded branch name for named or unborn branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Whether tracked or untracked changes exist; unknown on failed refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Commits ahead of the configured upstream; absent without an upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    /// Commits behind the configured upstream; absent without an upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
}

/// Server implementation used to discover folder instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstructionStateSource {
    /// Root-confined `AGENTS.md` discovery, ordered shallowest to deepest.
    ProjectAgentsMdV1,
}

/// Safe project-relative origin for an instruction file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionOrigin {
    /// Project-relative file name using `/` separators.
    pub relative_path: String,
    /// Folder scope affected by the file, or `.` for the project root.
    pub scope: String,
}

/// One safely loaded, bounded folder instruction file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FolderInstructionFile {
    /// Project-relative source.
    pub origin: InstructionOrigin,
    /// Root-first stable order. Deeper scopes override ancestors in descendants.
    pub precedence: u16,
    /// Exact accepted source byte length.
    pub byte_len: u64,
    /// Lowercase SHA-256 of the exact accepted source bytes.
    pub sha256: String,
    /// Bounded first non-empty line.
    pub summary: String,
    /// Bounded UTF-8 content suitable for an explicit inspector view.
    pub visible_content: String,
    /// Whether the inspector view omits the remainder of this file.
    pub content_truncated: bool,
}

/// Safe instruction-load failure classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstructionLoadErrorCode {
    /// A directory could not be listed.
    DirectoryUnavailable,
    /// A path component could not be represented safely.
    UnsupportedName,
    /// A symbolic link was rejected.
    SymlinkRejected,
    /// An `AGENTS.md` entry was not a regular file.
    NotRegularFile,
    /// A multiply-linked file was rejected to avoid reading outside authority.
    HardLinkRejected,
    /// A file exceeded its per-file byte bound.
    FileTooLarge,
    /// The aggregate source byte bound was reached.
    AggregateLimitReached,
    /// The file changed during validation or reading.
    ChangedDuringRead,
    /// The file was not UTF-8.
    InvalidUtf8,
    /// The file contained binary control characters.
    BinaryContent,
    /// A traversal bound stopped discovery.
    DiscoveryLimitReached,
}

/// One path-safe instruction load error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionLoadError {
    /// Project-relative origin when it can be represented safely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<InstructionOrigin>,
    /// Stable machine-readable classification.
    pub code: InstructionLoadErrorCode,
}

/// Public folder-instruction discovery result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FolderInstructionContext {
    /// Provenance of this context.
    pub source: InstructionStateSource,
    /// Refresh outcome and freshness.
    pub refresh: ContextRefreshStatus,
    /// Root-first, depth-aware instruction files.
    pub files: Vec<FolderInstructionFile>,
    /// Bounded load errors with no host paths or operating-system text.
    pub errors: Vec<InstructionLoadError>,
    /// Errors omitted after the public error limit.
    pub omitted_errors: usize,
    /// Aggregate bytes of successfully loaded source files.
    pub loaded_bytes: u64,
}

/// One path-free repository context snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryContextSnapshot {
    /// Opaque registry-owned project identity.
    pub project_id: ProjectId,
    /// Explicit trust verification represented by this snapshot.
    pub trust: RepositoryTrust,
    /// Git worktree state.
    pub repository: GitRepositoryContext,
    /// Loaded folder instruction state.
    pub instructions: FolderInstructionContext,
}

/// Trust or root-integrity failure before a snapshot can be returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum RepositoryContextError {
    /// The project is absent, archived, or not explicitly trusted.
    #[error("trusted project access is required")]
    TrustRequired,
    /// The registered project root disappeared or changed identity.
    #[error("the trusted project root changed")]
    RootChanged,
}

/// Stateless bounded context refresher.
#[derive(Clone)]
pub struct RepositoryContextLoader {
    git_timeout: Duration,
}

impl fmt::Debug for RepositoryContextLoader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryContextLoader")
            .field("git_timeout", &self.git_timeout)
            .finish()
    }
}

impl Default for RepositoryContextLoader {
    fn default() -> Self {
        Self {
            git_timeout: DEFAULT_GIT_TIMEOUT,
        }
    }
}

impl RepositoryContextLoader {
    /// Creates a loader with the production deadline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Refreshes Git and folder-instruction context for an authoritative project.
    ///
    /// The caller never supplies a path. Trust and the canonical root are
    /// resolved from `registry`, and trust/root identity are rechecked after
    /// the bounded reads complete.
    pub fn refresh(
        &self,
        registry: &ProjectRegistry,
        project_id: &ProjectId,
    ) -> Result<RepositoryContextSnapshot, RepositoryContextError> {
        let root = trusted_root(registry, project_id)?;
        let repository = refresh_git(root.as_path(), self.git_timeout);
        let instructions = refresh_instructions(root.as_path());
        let current = trusted_root(registry, project_id)?;
        if current.as_path() != root.as_path() {
            return Err(RepositoryContextError::RootChanged);
        }
        Ok(RepositoryContextSnapshot {
            project_id: project_id.clone(),
            trust: RepositoryTrust::Verified,
            repository,
            instructions,
        })
    }
}

/// Refreshes a trusted project's path-free repository context.
pub fn refresh_repository_context(
    registry: &ProjectRegistry,
    project_id: &ProjectId,
) -> Result<RepositoryContextSnapshot, RepositoryContextError> {
    RepositoryContextLoader::new().refresh(registry, project_id)
}

fn trusted_root(
    registry: &ProjectRegistry,
    project_id: &ProjectId,
) -> Result<ProjectRoot, RepositoryContextError> {
    registry
        .resolve_trusted_root(project_id)
        .map_err(|error| match error {
            ProjectRegistryError::ProjectNotFound
            | ProjectRegistryError::ProjectArchived
            | ProjectRegistryError::ProjectUntrusted => RepositoryContextError::TrustRequired,
            _ => RepositoryContextError::RootChanged,
        })
}

fn refresh_git(root: &Path, timeout: Duration) -> GitRepositoryContext {
    let started = Instant::now();
    let metadata = match root.join(".git").symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return unavailable_git(
                started,
                ContextRefreshState::NotApplicable,
                GitWorktreeState::NotRepository,
            );
        }
        Err(_) => {
            return unavailable_git(
                started,
                ContextRefreshState::Unavailable,
                GitWorktreeState::Unknown,
            );
        }
    };
    if metadata.file_type().is_symlink()
        || (!metadata.file_type().is_dir() && !metadata.file_type().is_file())
    {
        return unavailable_git(
            started,
            ContextRefreshState::Unavailable,
            GitWorktreeState::Unknown,
        );
    }

    let top_level = run_git(
        root,
        &["-c", "core.fsmonitor=false", "rev-parse", "--show-toplevel"],
        timeout,
        MAX_GIT_ROOT_BYTES,
    );
    let top_level = match top_level {
        GitRunResult::Finished(output) if output.status.success() && !output.truncated => output,
        GitRunResult::TimedOut => {
            return unavailable_git(
                started,
                ContextRefreshState::TimedOut,
                GitWorktreeState::Unknown,
            );
        }
        GitRunResult::Unavailable | GitRunResult::Finished(_) => {
            return unavailable_git(
                started,
                ContextRefreshState::Unavailable,
                GitWorktreeState::Unknown,
            );
        }
    };
    if !top_level_matches(root, &top_level.stdout) {
        return unavailable_git(
            started,
            ContextRefreshState::Unavailable,
            GitWorktreeState::Unknown,
        );
    }

    let status = run_git(
        root,
        &[
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "submodule.recurse=false",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=all",
        ],
        timeout,
        MAX_GIT_STATUS_BYTES,
    );
    match status {
        GitRunResult::TimedOut => unavailable_git(
            started,
            ContextRefreshState::TimedOut,
            GitWorktreeState::Present,
        ),
        GitRunResult::Unavailable => unavailable_git(
            started,
            ContextRefreshState::Unavailable,
            GitWorktreeState::Present,
        ),
        GitRunResult::Finished(output) if !output.status.success() => unavailable_git(
            started,
            ContextRefreshState::Unavailable,
            GitWorktreeState::Present,
        ),
        GitRunResult::Finished(output) => {
            match parse_git_status(&output.stdout, output.truncated) {
                Some(parsed) => GitRepositoryContext {
                    source: RepositoryStateSource::GitStatusPorcelainV2,
                    refresh: refresh_status(
                        started,
                        if output.truncated {
                            ContextRefreshState::Partial
                        } else {
                            ContextRefreshState::Current
                        },
                        output.truncated,
                    ),
                    worktree: GitWorktreeState::Present,
                    head: parsed.head,
                    branch_state: parsed.branch_state,
                    branch: parsed.branch,
                    dirty: Some(parsed.dirty),
                    ahead: parsed.ahead,
                    behind: parsed.behind,
                },
                None => unavailable_git(
                    started,
                    ContextRefreshState::Unavailable,
                    GitWorktreeState::Present,
                ),
            }
        }
    }
}

fn unavailable_git(
    started: Instant,
    state: ContextRefreshState,
    worktree: GitWorktreeState,
) -> GitRepositoryContext {
    GitRepositoryContext {
        source: RepositoryStateSource::GitStatusPorcelainV2,
        refresh: refresh_status(started, state, false),
        worktree,
        head: None,
        branch_state: GitBranchState::Unknown,
        branch: None,
        dirty: None,
        ahead: None,
        behind: None,
    }
}

#[derive(Debug)]
struct ParsedGitStatus {
    head: Option<String>,
    branch_state: GitBranchState,
    branch: Option<String>,
    dirty: bool,
    ahead: Option<u32>,
    behind: Option<u32>,
}

fn parse_git_status(bytes: &[u8], truncated: bool) -> Option<ParsedGitStatus> {
    let mut oid_seen = false;
    let mut head = None;
    let mut branch_seen = false;
    let mut branch_state = GitBranchState::Unknown;
    let mut branch = None;
    let mut dirty = truncated;
    let mut ahead = None;
    let mut behind = None;

    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            oid_seen = true;
            if value != b"(initial)" {
                let value = std::str::from_utf8(value).ok()?;
                if !valid_object_id(value) {
                    return None;
                }
                head = Some(value.to_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.head ") {
            branch_seen = true;
            if value == b"(detached)" {
                branch_state = GitBranchState::Detached;
            } else {
                let value = std::str::from_utf8(value).ok()?;
                if !safe_branch(value) {
                    return None;
                }
                branch_state = GitBranchState::Named;
                branch = Some(value.to_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let value = std::str::from_utf8(value).ok()?;
            let mut parts = value.split_ascii_whitespace();
            ahead = parse_signed_count(parts.next()?, '+');
            behind = parse_signed_count(parts.next()?, '-');
            if ahead.is_none() || behind.is_none() || parts.next().is_some() {
                return None;
            }
        } else if record.starts_with(b"1 ")
            || record.starts_with(b"2 ")
            || record.starts_with(b"u ")
            || record.starts_with(b"? ")
        {
            dirty = true;
        }
    }
    if !oid_seen || !branch_seen {
        return None;
    }
    if head.is_none() && branch_state == GitBranchState::Named {
        branch_state = GitBranchState::Unborn;
    }
    Some(ParsedGitStatus {
        head,
        branch_state,
        branch,
        dirty,
        ahead,
        behind,
    })
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_branch(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PUBLIC_BRANCH_BYTES
        && !value.chars().any(char::is_control)
}

fn parse_signed_count(value: &str, sign: char) -> Option<u32> {
    value.strip_prefix(sign)?.parse().ok()
}

fn top_level_matches(root: &Path, output: &[u8]) -> bool {
    let output = match std::str::from_utf8(output) {
        Ok(output) => output.trim_end_matches(['\r', '\n']),
        Err(_) => return false,
    };
    if output.is_empty() || output.len() > MAX_GIT_ROOT_BYTES {
        return false;
    }
    Path::new(output)
        .canonicalize()
        .is_ok_and(|top_level| top_level == root)
}

enum GitRunResult {
    Finished(BoundedProcessOutput),
    TimedOut,
    Unavailable,
}

struct BoundedProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    truncated: bool,
}

fn run_git(root: &Path, args: &[&str], timeout: Duration, stdout_limit: usize) -> GitRunResult {
    let Some(git_executable) = safe_git_executable(root) else {
        return GitRunResult::Unavailable;
    };
    let mut command = Command::new(git_executable);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    #[cfg(windows)]
    for name in ["SystemRoot", "ComSpec", "PATHEXT"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C");

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return GitRunResult::Unavailable,
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return GitRunResult::Unavailable;
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        return GitRunResult::Unavailable;
    };
    let stdout_reader = thread::spawn(move || read_bounded(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, MAX_GIT_STDERR_BYTES));
    let started = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                break (child.wait().ok(), true);
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                let _ = child.kill();
                break (child.wait().ok(), false);
            }
        }
    };
    let stdout = stdout_reader.join().ok();
    let _ = stderr_reader.join();
    if timed_out {
        return GitRunResult::TimedOut;
    }
    match (status, stdout) {
        (Some(status), Some((stdout, truncated))) => GitRunResult::Finished(BoundedProcessOutput {
            status,
            stdout,
            truncated,
        }),
        _ => GitRunResult::Unavailable,
    }
}

fn safe_git_executable(root: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        #[cfg(windows)]
        let names = ["git.exe", "git"];
        #[cfg(not(windows))]
        let names = ["git"];
        for name in names {
            let Ok(candidate) = directory.join(name).canonicalize() else {
                continue;
            };
            if candidate.starts_with(root) {
                continue;
            }
            let Ok(metadata) = candidate.symlink_metadata() else {
                continue;
            };
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            return Some(candidate);
        }
    }
    None
}

fn read_bounded(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut retained = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(retained.len());
                let keep = remaining.min(read);
                retained.extend_from_slice(&buffer[..keep]);
                truncated |= keep < read;
            }
        }
    }
    (retained, truncated)
}

#[cfg(unix)]
fn null_device() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(any(unix, windows)))]
fn null_device() -> &'static str {
    "/dev/null"
}

fn refresh_instructions(root: &Path) -> FolderInstructionContext {
    let started = Instant::now();
    let mut discovered = Vec::new();
    let mut errors = Vec::new();
    let mut omitted_errors = 0usize;
    let mut walked_entries = 0usize;
    let mut traversal_truncated = false;
    let mut root_unavailable = false;
    let mut queue = VecDeque::from([(PathBuf::new(), 0usize)]);

    while let Some((relative_directory, depth)) = queue.pop_front() {
        if walked_entries >= MAX_INSTRUCTION_WALK_ENTRIES
            || discovered.len() >= MAX_INSTRUCTION_FILES
        {
            traversal_truncated = true;
            break;
        }
        let directory = root.join(&relative_directory);
        let entries = match read_directory_bounded(
            &directory,
            MAX_INSTRUCTION_WALK_ENTRIES.saturating_sub(walked_entries),
        ) {
            Ok(entries) => entries,
            Err(_) => {
                if relative_directory.as_os_str().is_empty() {
                    root_unavailable = true;
                }
                push_instruction_error(
                    &mut errors,
                    &mut omitted_errors,
                    InstructionLoadError {
                        origin: directory_origin(&relative_directory),
                        code: InstructionLoadErrorCode::DirectoryUnavailable,
                    },
                );
                continue;
            }
        };
        if entries.truncated {
            traversal_truncated = true;
        }
        for entry in entries.entries {
            walked_entries = walked_entries.saturating_add(1);
            if walked_entries > MAX_INSTRUCTION_WALK_ENTRIES {
                traversal_truncated = true;
                break;
            }
            let name = match entry.file_name().into_string() {
                Ok(name) if safe_component(&name) => name,
                _ => {
                    push_instruction_error(
                        &mut errors,
                        &mut omitted_errors,
                        InstructionLoadError {
                            origin: None,
                            code: InstructionLoadErrorCode::UnsupportedName,
                        },
                    );
                    continue;
                }
            };
            let relative_path = relative_directory.join(&name);
            let metadata = match entry.path().symlink_metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    if name == AGENTS_FILE_NAME {
                        push_instruction_error(
                            &mut errors,
                            &mut omitted_errors,
                            InstructionLoadError {
                                origin: instruction_origin(&relative_path),
                                code: InstructionLoadErrorCode::ChangedDuringRead,
                            },
                        );
                    }
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                if name == AGENTS_FILE_NAME {
                    push_instruction_error(
                        &mut errors,
                        &mut omitted_errors,
                        InstructionLoadError {
                            origin: instruction_origin(&relative_path),
                            code: InstructionLoadErrorCode::SymlinkRejected,
                        },
                    );
                }
                continue;
            }
            if metadata.file_type().is_dir() {
                if depth >= MAX_INSTRUCTION_WALK_DEPTH {
                    traversal_truncated = true;
                } else if !ignored_directory(&name) {
                    queue.push_back((relative_path, depth + 1));
                }
                continue;
            }
            if name != AGENTS_FILE_NAME {
                continue;
            }
            let Some(origin) = instruction_origin(&relative_path) else {
                push_instruction_error(
                    &mut errors,
                    &mut omitted_errors,
                    InstructionLoadError {
                        origin: None,
                        code: InstructionLoadErrorCode::UnsupportedName,
                    },
                );
                continue;
            };
            if !metadata.file_type().is_file() {
                push_instruction_error(
                    &mut errors,
                    &mut omitted_errors,
                    InstructionLoadError {
                        origin: Some(origin),
                        code: InstructionLoadErrorCode::NotRegularFile,
                    },
                );
                continue;
            }
            if hard_link_count(&metadata) > 1 {
                push_instruction_error(
                    &mut errors,
                    &mut omitted_errors,
                    InstructionLoadError {
                        origin: Some(origin),
                        code: InstructionLoadErrorCode::HardLinkRejected,
                    },
                );
                continue;
            }
            if metadata.len() > MAX_INSTRUCTION_FILE_BYTES {
                push_instruction_error(
                    &mut errors,
                    &mut omitted_errors,
                    InstructionLoadError {
                        origin: Some(origin),
                        code: InstructionLoadErrorCode::FileTooLarge,
                    },
                );
                continue;
            }
            if discovered.len() >= MAX_INSTRUCTION_FILES {
                traversal_truncated = true;
                break;
            }
            match read_instruction(root, &relative_path, &metadata) {
                Ok(raw) => discovered.push((origin, depth, raw)),
                Err(code) => push_instruction_error(
                    &mut errors,
                    &mut omitted_errors,
                    InstructionLoadError {
                        origin: Some(origin),
                        code,
                    },
                ),
            }
        }
        if entries.truncated {
            break;
        }
    }

    if traversal_truncated {
        push_instruction_error(
            &mut errors,
            &mut omitted_errors,
            InstructionLoadError {
                origin: None,
                code: InstructionLoadErrorCode::DiscoveryLimitReached,
            },
        );
    }
    discovered.sort_by(
        |(left_origin, left_depth, _), (right_origin, right_depth, _)| {
            left_depth
                .cmp(right_depth)
                .then_with(|| left_origin.relative_path.cmp(&right_origin.relative_path))
        },
    );

    let mut loaded_bytes = 0u64;
    let mut visible_bytes = 0usize;
    let mut files = Vec::new();
    for (origin, _, raw) in discovered {
        if loaded_bytes
            .checked_add(raw.bytes.len() as u64)
            .is_none_or(|total| total > MAX_INSTRUCTION_TOTAL_BYTES)
        {
            traversal_truncated = true;
            push_instruction_error(
                &mut errors,
                &mut omitted_errors,
                InstructionLoadError {
                    origin: Some(origin),
                    code: InstructionLoadErrorCode::AggregateLimitReached,
                },
            );
            continue;
        }
        loaded_bytes += raw.bytes.len() as u64;
        let available = MAX_VISIBLE_INSTRUCTION_TOTAL_BYTES.saturating_sub(visible_bytes);
        let visible_limit = available.min(MAX_VISIBLE_INSTRUCTION_FILE_BYTES);
        let visible_content = truncate_utf8(&raw.text, visible_limit).to_owned();
        visible_bytes = visible_bytes.saturating_add(visible_content.len());
        let content_truncated = visible_content.len() < raw.text.len();
        let precedence = u16::try_from(files.len()).unwrap_or(u16::MAX);
        files.push(FolderInstructionFile {
            origin,
            precedence,
            byte_len: raw.bytes.len() as u64,
            sha256: sha256_hex(&raw.bytes),
            summary: summarize_instruction(&raw.text),
            visible_content,
            content_truncated,
        });
    }
    let truncated = traversal_truncated
        || omitted_errors > 0
        || files.iter().any(|file| file.content_truncated);
    let state = if root_unavailable {
        ContextRefreshState::Unavailable
    } else if !errors.is_empty() || truncated {
        ContextRefreshState::Partial
    } else {
        ContextRefreshState::Current
    };
    FolderInstructionContext {
        source: InstructionStateSource::ProjectAgentsMdV1,
        refresh: refresh_status(started, state, truncated),
        files,
        errors,
        omitted_errors,
        loaded_bytes,
    }
}

struct BoundedDirectory {
    entries: Vec<std::fs::DirEntry>,
    truncated: bool,
}

fn read_directory_bounded(path: &Path, limit: usize) -> std::io::Result<BoundedDirectory> {
    let mut entries = Vec::with_capacity(limit.min(1_024));
    let mut truncated = false;
    for entry in std::fs::read_dir(path)? {
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        entries.push(entry?);
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(BoundedDirectory { entries, truncated })
}

struct RawInstruction {
    bytes: Vec<u8>,
    text: String,
}

fn read_instruction(
    root: &Path,
    relative_path: &Path,
    expected: &std::fs::Metadata,
) -> Result<RawInstruction, InstructionLoadErrorCode> {
    let expected_identity = capture_identity(expected);
    let path = validate_confined_instruction(root, relative_path, expected_identity)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| InstructionLoadErrorCode::ChangedDuringRead)?;
    if !matches_expected_file(&file, expected_identity) {
        return Err(InstructionLoadErrorCode::ChangedDuringRead);
    }
    let mut bytes = Vec::with_capacity(expected.len().min(MAX_INSTRUCTION_FILE_BYTES) as usize);
    Read::by_ref(&mut file)
        .take(MAX_INSTRUCTION_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| InstructionLoadErrorCode::ChangedDuringRead)?;
    if bytes.len() as u64 != expected.len() || bytes.len() as u64 > MAX_INSTRUCTION_FILE_BYTES {
        return Err(InstructionLoadErrorCode::ChangedDuringRead);
    }
    if !matches_expected_file(&file, expected_identity) {
        return Err(InstructionLoadErrorCode::ChangedDuringRead);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| InstructionLoadErrorCode::InvalidUtf8)?
        .to_owned();
    if text.chars().any(binary_control) {
        return Err(InstructionLoadErrorCode::BinaryContent);
    }
    Ok(RawInstruction { bytes, text })
}

fn validate_confined_instruction(
    root: &Path,
    relative_path: &Path,
    expected_identity: FileIdentity,
) -> Result<PathBuf, InstructionLoadErrorCode> {
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(InstructionLoadErrorCode::ChangedDuringRead);
    }
    let mut current = root.to_owned();
    let component_count = relative_path.components().count();
    for (index, component) in relative_path.components().enumerate() {
        let Component::Normal(component) = component else {
            return Err(InstructionLoadErrorCode::ChangedDuringRead);
        };
        current.push(component);
        let metadata = current
            .symlink_metadata()
            .map_err(|_| InstructionLoadErrorCode::ChangedDuringRead)?;
        if metadata.file_type().is_symlink() {
            return Err(InstructionLoadErrorCode::SymlinkRejected);
        }
        if index + 1 == component_count {
            if !metadata.file_type().is_file()
                || capture_identity(&metadata) != expected_identity
                || hard_link_count(&metadata) > 1
            {
                return Err(InstructionLoadErrorCode::ChangedDuringRead);
            }
        } else if !metadata.file_type().is_dir() {
            return Err(InstructionLoadErrorCode::ChangedDuringRead);
        }
    }
    let canonical = current
        .canonicalize()
        .map_err(|_| InstructionLoadErrorCode::ChangedDuringRead)?;
    if canonical == root || !canonical.starts_with(root) {
        return Err(InstructionLoadErrorCode::ChangedDuringRead);
    }
    Ok(current)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: Option<u64>,
    inode: Option<u64>,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

fn capture_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        FileIdentity {
            device: Some(metadata.dev()),
            inode: Some(metadata.ino()),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
        FileIdentity {
            device: None,
            inode: None,
            size: metadata.len(),
            modified_seconds: modified
                .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
                .unwrap_or_default(),
            modified_nanoseconds: modified
                .map(|duration| duration.subsec_nanos() as i64)
                .unwrap_or_default(),
        }
    }
}

fn hard_link_count(metadata: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        1
    }
}

fn matches_expected_file(file: &File, expected_identity: FileIdentity) -> bool {
    file.metadata().is_ok_and(|metadata| {
        metadata.file_type().is_file()
            && hard_link_count(&metadata) <= 1
            && capture_identity(&metadata) == expected_identity
    })
}

fn instruction_origin(relative_path: &Path) -> Option<InstructionOrigin> {
    let relative_path = public_relative_path(relative_path)?;
    let scope = match relative_path.rsplit_once('/') {
        Some((scope, _)) => scope.to_owned(),
        None => ".".to_owned(),
    };
    Some(InstructionOrigin {
        relative_path,
        scope,
    })
}

fn directory_origin(relative_directory: &Path) -> Option<InstructionOrigin> {
    if relative_directory.as_os_str().is_empty() {
        return Some(InstructionOrigin {
            relative_path: ".".to_owned(),
            scope: ".".to_owned(),
        });
    }
    let relative_path = public_relative_path(relative_directory)?;
    Some(InstructionOrigin {
        relative_path: relative_path.clone(),
        scope: relative_path,
    })
}

fn public_relative_path(relative_path: &Path) -> Option<String> {
    if relative_path.is_absolute() {
        return None;
    }
    let mut output = String::new();
    for component in relative_path.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        let component = component.to_str()?;
        if !safe_component(component) {
            return None;
        }
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(component);
        if output.len() > MAX_PUBLIC_ORIGIN_BYTES {
            return None;
        }
    }
    (!output.is_empty()).then_some(output)
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
}

fn ignored_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | ".jj"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | "__pycache__"
    )
}

fn push_instruction_error(
    errors: &mut Vec<InstructionLoadError>,
    omitted_errors: &mut usize,
    error: InstructionLoadError,
) {
    if errors.len() < MAX_INSTRUCTION_ERRORS {
        errors.push(error);
    } else {
        *omitted_errors = omitted_errors.saturating_add(1);
    }
}

fn summarize_instruction(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut summary = String::new();
    for character in line.chars().take(MAX_INSTRUCTION_SUMMARY_CHARS) {
        if character.is_control() {
            summary.push(' ');
        } else {
            summary.push(character);
        }
    }
    summary
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

fn binary_control(character: char) -> bool {
    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn refresh_status(
    started: Instant,
    state: ContextRefreshState,
    truncated: bool,
) -> ContextRefreshStatus {
    ContextRefreshStatus {
        state,
        refreshed_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        truncated,
    }
}
