//! Tool capability settings and local-path resolution.
//!
//! Ygg is a trusted local agent, not an OS sandbox. This configuration carries
//! capability gates, resource limits, and an optional workspace-only path mode
//! for hosts that want accidental-path protection. The narrow
//! `allow_remote_read` gate controls only direct HTTPS media requests made by
//! the built-in `read` tool. It is not a process network sandbox: an enabled
//! child process may still open sockets because this crate does not ship an OS
//! sandbox backend in v0.1.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Default maximum bytes retained for each model-visible tool result.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// Default total number of attempts for one remote media read, including the
/// initial request.
pub const DEFAULT_REMOTE_READ_MAX_ATTEMPTS: usize = 3;
/// Default delay before the first remote media retry.
pub const DEFAULT_REMOTE_READ_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
/// Default upper bound for exponential remote media retry delays.
pub const DEFAULT_REMOTE_READ_MAX_BACKOFF: Duration = Duration::from_secs(2);
/// Hard upper bound for the total number of remote media attempts, including
/// the initial request.
pub const MAX_REMOTE_READ_ATTEMPTS: usize = 8;
/// Hard upper bound for one remote media retry delay.
pub const MAX_REMOTE_READ_BACKOFF: Duration = Duration::from_secs(30);

/// Bounded retry behavior for idempotent remote media reads.
///
/// `max_attempts` includes the initial request. Transport failures, response
/// timeouts, and HTTP 408, 429, or 5xx responses may consume the remaining
/// attempts; validation, content, and other HTTP errors do not. A zero
/// `max_attempts` value is treated as one attempt. Setting either backoff to
/// zero disables the corresponding delay without disabling retries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteReadRetryPolicy {
    /// Total requests allowed for one remote media read, including the first.
    pub max_attempts: usize,
    /// Delay before the first retry; later delays double up to `max_backoff`.
    pub initial_backoff: Duration,
    /// Maximum delay between remote media attempts.
    pub max_backoff: Duration,
}

impl Default for RemoteReadRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_REMOTE_READ_MAX_ATTEMPTS,
            initial_backoff: DEFAULT_REMOTE_READ_INITIAL_BACKOFF,
            max_backoff: DEFAULT_REMOTE_READ_MAX_BACKOFF,
        }
    }
}

impl RemoteReadRetryPolicy {
    /// Normalize a caller-provided attempt count at the execution boundary.
    pub(crate) fn attempts(&self) -> usize {
        self.max_attempts.clamp(1, MAX_REMOTE_READ_ATTEMPTS)
    }

    /// Return the deterministic capped exponential delay for a retry numbered
    /// from one. Saturating arithmetic keeps malformed host configuration from
    /// overflowing a duration.
    pub(crate) fn backoff_for_retry(&self, retry_number: usize) -> Duration {
        let cap = self.max_backoff.min(MAX_REMOTE_READ_BACKOFF);
        if retry_number == 0 || self.initial_backoff.is_zero() || cap.is_zero() {
            return Duration::ZERO;
        }
        let mut delay = self.initial_backoff.min(cap);
        for _ in 1..retry_number.min(64) {
            delay = delay.saturating_mul(2);
        }
        delay.min(cap)
    }
}

/// Capability gates and resource limits enforced by the agent's tools.
///
/// `workspace` is canonicalized once by [`Agent::new`](crate::Agent::new) and
/// remains the base for relative paths and default working directories. When
/// [`allow_external_paths`](Self::allow_external_paths) is false, explicit
/// paths are additionally constrained to that workspace. This is an optional
/// accidental-path guard, not an OS-level sandbox.
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// The workspace root used for relative paths and default working directories.
    pub workspace: PathBuf,
    /// Permit absolute paths, `~` paths, `..`, and symlinks outside the workspace.
    ///
    /// When false, built-in file tools and `bash` working directories are
    /// workspace-only. Spawned processes are never filesystem-confined by this
    /// flag.
    pub allow_external_paths: bool,
    /// Allow the `edit` tool to mutate files.
    pub allow_edit: bool,
    /// Allow the `write` tool to create or replace files.
    pub allow_write: bool,
    /// First half of the unified command-execution gate.
    ///
    /// Arbitrary process execution has shell-equivalent authority; `bash`
    /// requires this and `allow_shell` together.
    pub allow_process: bool,
    /// Second half of the unified command-execution gate. Keeping both fields
    /// preserves configuration compatibility at the authority boundary.
    pub allow_shell: bool,
    /// Allow the built-in `read` tool to fetch public HTTPS image/audio URLs.
    ///
    /// This narrow authority is independent of process execution and defaults
    /// off so conservative library hosts never gain network access implicitly.
    pub allow_remote_read: bool,
    /// Bounded retry and backoff policy for transient remote media failures.
    pub remote_read_retry: RemoteReadRetryPolicy,
    /// Explicit Bash-compatible shell executable. When unset on Unix, Ygg
    /// follows Pi's order: `/bin/bash`, `bash` on `PATH`, then `sh`.
    pub shell_path: Option<PathBuf>,
    /// Maximum duration for a `bash` call (also bounds `search`).
    pub bash_timeout: Duration,
    /// Maximum bytes of tool output before truncation.
    pub max_output_bytes: usize,
}

impl SandboxConfig {
    /// Creates a conservative library configuration rooted at `workspace`:
    /// workspace-only paths, no edits, no process, shell, or remote-read
    /// authority, a 120s execution timeout, and a 50 KiB per-tool output cap.
    /// Hosts may enable capabilities or trusted-local path access through the
    /// public fields.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            allow_external_paths: false,
            allow_edit: false,
            allow_write: false,
            allow_process: false,
            allow_shell: false,
            allow_remote_read: false,
            remote_read_retry: RemoteReadRetryPolicy::default(),
            shell_path: None,
            bash_timeout: Duration::from_secs(120),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Reject path shapes that are invalid in workspace-only mode.
fn check_workspace_shape(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path is empty".to_string());
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(format!(
            "{}: absolute paths are not allowed while workspace-only paths are enabled",
            path.display()
        ));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(format!(
                    "{}: `..` components are not allowed",
                    path.display()
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!(
                    "{}: absolute paths are not allowed while workspace-only paths are enabled",
                    path.display()
                ));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot expand `~`: HOME or USERPROFILE is not set".to_string())
}

/// Expand the current user's `~` or `~/…` spelling. Other shell expansions are
/// deliberately not performed: tool arguments are data, not shell input.
fn expand_tilde(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    #[cfg(windows)]
    if let Some(rest) = path.strip_prefix("~\\") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn candidate_path(
    workspace: &Path,
    path: &str,
    allow_external_paths: bool,
) -> Result<PathBuf, String> {
    if allow_external_paths {
        if path.is_empty() {
            return Err("path is empty".to_string());
        }
        let expanded = expand_tilde(path)?;
        return Ok(if expanded.is_absolute() {
            expanded
        } else {
            workspace.join(expanded)
        });
    }

    check_workspace_shape(path)?;
    Ok(workspace.join(path))
}

/// Resolves a path that must already exist.
///
/// Relative paths are based at `workspace`. With `allow_external_paths`,
/// absolute paths, `~/…`, parent components, and external symlink targets are
/// intentionally accepted under Ygg's trusted-local-agent model.
pub(crate) fn resolve_existing(
    workspace: &Path,
    path: &str,
    allow_external_paths: bool,
) -> Result<PathBuf, String> {
    let joined = candidate_path(workspace, path, allow_external_paths)?;
    let canonical = joined
        .canonicalize()
        .map_err(|error| format!("{path}: {error}"))?;
    if !allow_external_paths && !canonical.starts_with(workspace) {
        return Err(format!("{path}: path escapes the workspace"));
    }
    Ok(canonical)
}

/// Resolves a path for creation. The final components may not exist yet; every
/// existing ancestor is canonicalized. In workspace-only mode that ancestor
/// must remain below `workspace`; in trusted-local mode it may be anywhere the
/// current user can access.
pub(crate) fn resolve_create(
    workspace: &Path,
    path: &str,
    allow_external_paths: bool,
) -> Result<PathBuf, String> {
    let joined = candidate_path(workspace, path, allow_external_paths)?;

    if joined.symlink_metadata().is_ok() {
        return resolve_existing(workspace, path, allow_external_paths);
    }

    let mut ancestor = joined.as_path();
    loop {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("{path}: path has no valid parent"))?;
        if ancestor.symlink_metadata().is_ok() {
            break;
        }
    }
    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|error| format!("{path}: {error}"))?;
    if !allow_external_paths && !canonical_ancestor.starts_with(workspace) {
        return Err(format!("{path}: path escapes the workspace"));
    }
    let remainder = joined
        .strip_prefix(ancestor)
        .expect("ancestor is a prefix of the joined path");
    Ok(canonical_ancestor.join(remainder))
}

/// Return a stable, human/model-facing spelling for a path without changing
/// how it is resolved. Paths inside the workspace are displayed relative to
/// that workspace; intentional external paths retain the caller's spelling.
pub(crate) fn display_path(workspace: &Path, path: &str, allow_external_paths: bool) -> String {
    let expanded = if allow_external_paths {
        expand_tilde(path).unwrap_or_else(|_| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    let workspace_root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        workspace_root.join(expanded)
    };
    let candidate = candidate.canonicalize().unwrap_or(candidate);
    if let Ok(relative) = candidate.strip_prefix(&workspace_root) {
        if relative.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            relative.display().to_string()
        }
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        (dir, canonical)
    }

    #[test]
    fn remote_read_retry_policy_is_bounded_and_exponential() {
        let policy = RemoteReadRetryPolicy::default();
        assert_eq!(policy.attempts(), DEFAULT_REMOTE_READ_MAX_ATTEMPTS);
        assert_eq!(policy.backoff_for_retry(0), Duration::ZERO);
        assert_eq!(policy.backoff_for_retry(1), Duration::from_millis(100));
        assert_eq!(policy.backoff_for_retry(2), Duration::from_millis(200));
        assert_eq!(policy.backoff_for_retry(5), Duration::from_millis(1600));
        assert_eq!(policy.backoff_for_retry(6), Duration::from_secs(2));

        let untrusted = RemoteReadRetryPolicy {
            max_attempts: usize::MAX,
            initial_backoff: Duration::from_secs(60),
            max_backoff: Duration::from_secs(60),
        };
        assert_eq!(untrusted.attempts(), MAX_REMOTE_READ_ATTEMPTS);
        assert_eq!(untrusted.backoff_for_retry(1), MAX_REMOTE_READ_BACKOFF);
    }

    #[test]
    fn default_tool_output_limit_is_50_kib() {
        let (_dir, ws) = workspace();
        assert_eq!(SandboxConfig::new(ws).max_output_bytes, 50 * 1024);
    }

    #[test]
    fn normal_relative_access_resolves() {
        let (_dir, ws) = workspace();
        fs::create_dir(ws.join("src")).unwrap();
        fs::write(ws.join("src/main.rs"), "fn main() {}").unwrap();
        let resolved = resolve_existing(&ws, "src/main.rs", false).unwrap();
        assert_eq!(resolved, ws.join("src/main.rs"));
    }

    #[test]
    fn parent_dir_components_rejected_in_workspace_only_mode() {
        let (_dir, ws) = workspace();
        let err = resolve_existing(&ws, "../etc/passwd", false).unwrap_err();
        assert!(err.contains(".."), "{err}");
        // `..` is rejected even when it would stay inside the workspace.
        fs::create_dir(ws.join("src")).unwrap();
        fs::write(ws.join("ok.txt"), "x").unwrap();
        assert!(resolve_existing(&ws, "src/../ok.txt", false).is_err());
    }

    #[test]
    fn absolute_paths_rejected_in_workspace_only_mode() {
        let (_dir, ws) = workspace();
        let err = resolve_existing(&ws, "/etc/passwd", false).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        assert!(resolve_create(&ws, "/tmp/new.txt", false).is_err());
    }

    #[test]
    fn empty_path_rejected() {
        let (_dir, ws) = workspace();
        assert!(resolve_existing(&ws, "", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_file_escape_rejected() {
        let (_dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();
        std::os::unix::fs::symlink(&secret, ws.join("link.txt")).unwrap();

        let err = resolve_existing(&ws, "link.txt", false).unwrap_err();
        assert!(err.contains("escapes"), "{err}");
        // Creation through the same link is equally rejected.
        assert!(resolve_create(&ws, "link.txt", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_escape_rejected() {
        let (_dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("data.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join("subdir")).unwrap();

        let err = resolve_existing(&ws, "subdir/data.txt", false).unwrap_err();
        assert!(err.contains("escapes"), "{err}");
        // A create through a symlinked-out ancestor is rejected too, even for
        // a target that does not exist yet.
        let err = resolve_create(&ws, "subdir/new/file.txt", false).unwrap_err();
        assert!(err.contains("escapes"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_workspace_resolves_to_real_path() {
        let (_dir, ws) = workspace();
        fs::create_dir(ws.join("real")).unwrap();
        fs::write(ws.join("real/f.txt"), "x").unwrap();
        std::os::unix::fs::symlink(ws.join("real"), ws.join("alias")).unwrap();
        let resolved = resolve_existing(&ws, "alias/f.txt", false).unwrap();
        assert_eq!(resolved, ws.join("real/f.txt"));
    }

    #[test]
    fn create_through_nonexistent_final_path_resolves() {
        let (_dir, ws) = workspace();
        let resolved = resolve_create(&ws, "new/deep/file.txt", false).unwrap();
        assert_eq!(resolved, ws.join("new/deep/file.txt"));
    }

    #[test]
    fn create_of_missing_path_never_escapes() {
        let (_dir, ws) = workspace();
        assert!(resolve_create(&ws, "../outside.txt", false).is_err());
    }

    #[test]
    fn trusted_local_paths_allow_absolute_and_parent_access() {
        let (_dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let existing = outside.path().join("outside.txt");
        fs::write(&existing, "outside").unwrap();

        assert_eq!(
            resolve_existing(&ws, existing.to_str().unwrap(), true).unwrap(),
            existing.canonicalize().unwrap()
        );
        let created = outside.path().join("new/deep/file.txt");
        assert_eq!(
            resolve_create(&ws, created.to_str().unwrap(), true).unwrap(),
            outside
                .path()
                .canonicalize()
                .unwrap()
                .join("new/deep/file.txt")
        );
    }

    #[test]
    fn display_path_prefers_workspace_relative_spelling() {
        let (_dir, ws) = workspace();
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/main.rs"), "fn main() {}").unwrap();
        let absolute = ws.join("src/main.rs");

        assert_eq!(display_path(&ws, "./src/main.rs", true), "src/main.rs");
        assert_eq!(
            display_path(&ws, absolute.to_str().unwrap(), true),
            "src/main.rs"
        );

        let outside = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            display_path(&ws, outside.path().to_str().unwrap(), true),
            outside.path().to_str().unwrap()
        );
    }

    #[test]
    fn trusted_local_relative_parent_paths_are_workspace_based() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let sibling = root.path().join("sibling");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let existing = sibling.join("outside.txt");
        fs::write(&existing, "outside").unwrap();

        assert_eq!(
            resolve_existing(&workspace, "../sibling/outside.txt", true).unwrap(),
            existing.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_create(&workspace, "../sibling/new/deep.txt", true).unwrap(),
            sibling.canonicalize().unwrap().join("new/deep.txt")
        );
    }

    #[test]
    fn tilde_expands_to_the_current_users_home_directory() {
        let home = home_dir().unwrap();
        assert_eq!(expand_tilde("~").unwrap(), home);
        assert_eq!(
            expand_tilde("~/.ygg/config.toml").unwrap(),
            home.join(".ygg/config.toml")
        );
    }
}
