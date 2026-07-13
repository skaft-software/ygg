#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use ygg_agent::SandboxConfig;
use ygg_ai::{ModelId, ReasoningConfig, ReasoningEffort};

/// Resolve the workspace root: an explicit path, the nearest `.git` ancestor,
/// or the current directory. The returned path is canonicalized.
pub fn resolve_workspace(explicit: Option<&Path>, cwd: &Path) -> std::io::Result<PathBuf> {
    if let Some(path) = explicit {
        return path.canonicalize();
    }

    let mut current = Some(cwd);
    while let Some(directory) = current {
        if directory.join(".git").exists() {
            return directory.canonicalize();
        }
        current = directory.parent();
    }
    cwd.canonicalize()
}

/// Frontend selected for this invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Interactive,
    Print { prompt: String },
}

/// Session selected at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeSelector {
    New,
    Continue,
    Resume(Option<String>),
}

/// Product-level sandbox settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub allow_edit: bool,
    pub allow_process: bool,
    pub allow_shell: bool,
    pub exec_timeout_secs: u64,
    pub max_output_bytes: usize,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            allow_edit: true,
            allow_process: true,
            allow_shell: false,
            exec_timeout_secs: 120,
            max_output_bytes: 64 * 1024,
        }
    }
}

impl SandboxPolicy {
    /// Translate product settings to the frozen agent sandbox configuration.
    pub fn to_sandbox_config(&self, workspace: &Path) -> SandboxConfig {
        let mut sandbox = SandboxConfig::new(workspace);
        sandbox.allow_edit = self.allow_edit;
        sandbox.allow_process = self.allow_process;
        sandbox.allow_shell = self.allow_shell;
        sandbox.exec_timeout = Duration::from_secs(self.exec_timeout_secs);
        sandbox.max_output_bytes = self.max_output_bytes;
        sandbox
    }
}

/// Automatic compaction policy.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPolicy {
    pub threshold_fraction: f64,
    pub keep_recent_turns: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            threshold_fraction: 0.85,
            keep_recent_turns: 4,
        }
    }
}

/// Resolved configuration for one process.
#[derive(Clone, Debug)]
pub struct Config {
    pub workspace: PathBuf,
    pub invocation_cwd: PathBuf,
    pub model: Option<ModelId>,
    pub reasoning: ReasoningConfig,
    pub sandbox: SandboxPolicy,
    pub theme: Option<String>,
    pub session_dir: PathBuf,
    pub compaction: CompactionPolicy,
    pub max_turns: u64,
    pub show_reasoning_in_print: bool,
    /// Prompt passed positionally for interactive startup, if any.
    pub initial_prompt: Option<String>,
    pub mode: Mode,
    pub resume: ResumeSelector,
}

/// Parse a reasoning override such as `high` or `budget=2048`.
pub fn parse_reasoning(value: &str) -> anyhow::Result<ReasoningConfig> {
    let config = match value.trim().to_ascii_lowercase().as_str() {
        "off" => ReasoningConfig::Off,
        "minimal" | "min" => ReasoningConfig::Effort(ReasoningEffort::Minimal),
        "low" => ReasoningConfig::Effort(ReasoningEffort::Low),
        "medium" | "med" => ReasoningConfig::Effort(ReasoningEffort::Medium),
        "high" => ReasoningConfig::Effort(ReasoningEffort::High),
        other => {
            let budget = other
                .strip_prefix("budget=")
                .and_then(|raw| raw.parse::<u64>().ok())
                .filter(|budget| *budget > 0)
                .ok_or_else(|| anyhow::anyhow!("invalid reasoning setting {value:?}"))?;
            ReasoningConfig::Budget(budget)
        }
    };
    Ok(config)
}

/// Default location for persistent sessions.
pub fn default_session_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn explicit_workspace_wins_and_is_canonicalized() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = resolve_workspace(Some(directory.path()), Path::new("/")).unwrap();
        assert_eq!(workspace, directory.path().canonicalize().unwrap());
    }

    #[test]
    fn finds_nearest_git_ancestor() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        let nested = directory.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let workspace = resolve_workspace(None, &nested).unwrap();
        assert_eq!(workspace, directory.path().canonicalize().unwrap());
    }

    #[test]
    fn falls_back_to_cwd_without_git() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = resolve_workspace(None, directory.path()).unwrap();
        assert_eq!(workspace, directory.path().canonicalize().unwrap());
    }

    #[test]
    fn reasoning_parser_accepts_effort_and_budget_values() {
        assert_eq!(parse_reasoning("off").unwrap(), ReasoningConfig::Off);
        assert_eq!(
            parse_reasoning("high").unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::High)
        );
        assert_eq!(
            parse_reasoning("budget=2048").unwrap(),
            ReasoningConfig::Budget(2048)
        );
        assert!(parse_reasoning("nonsense").is_err());
    }
}
