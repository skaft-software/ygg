#![allow(missing_docs)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ygg_agent::SandboxConfig;

pub use crate::tui::terminal::ColorMode;
use ygg_ai::{CacheRetention, ModelId, ReasoningConfig, ReasoningEffort, ReasoningEffortBudgets};

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

/// Mouse ownership policy. `Auto` and `Terminal` preserve the terminal's native
/// selection and scrollback; `App` explicitly opts into Ygg's semantic mouse
/// scrolling and selection compatibility mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    Auto,
    App,
    Terminal,
    Off,
}

impl MouseMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "app" => Ok(Self::App),
            "terminal" => Ok(Self::Terminal),
            "off" => Ok(Self::Off),
            _ => anyhow::bail!("invalid mouse mode {value:?}; use auto, app, terminal, or off"),
        }
    }

    /// Native selection and scrollback are the default. Application mouse
    /// ownership remains available only as an explicit compatibility choice.
    pub fn application_owned(self) -> bool {
        matches!(self, Self::App)
    }
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
    pub allow_external_paths: bool,
    pub allow_edit: bool,
    pub allow_write: bool,
    pub allow_process: bool,
    pub allow_shell: bool,
    pub exec_timeout_secs: u64,
    pub max_output_bytes: usize,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            // Ygg is a trusted local agent: explicit absolute, `~/`, and
            // parent-relative paths work by default. Users can opt into the
            // workspace-only accidental-path guard in configuration.
            allow_external_paths: true,
            allow_edit: true,
            allow_write: true,
            allow_process: true,
            allow_shell: true,
            exec_timeout_secs: 120,
            max_output_bytes: 16 * 1024,
        }
    }
}

/// Tool names understood by the v0.1 coding product.
pub const SUPPORTED_TOOL_NAMES: [&str; 8] = [
    "read",
    "search",
    "edit",
    "write",
    "exec",
    "search_skills",
    "load_skill",
    "read_skill_resource",
];

/// One authoritative model-visible tool allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPolicy {
    enabled: BTreeSet<String>,
    excluded: BTreeSet<String>,
    /// Original names supplied through an explicit allowlist. Keep this
    /// separate from `enabled`: later sandbox and exclusion gates may remove a
    /// requested name, and startup must still report that request as
    /// unavailable rather than silently accepting it.
    requested: Option<BTreeSet<String>>,
    allow_discovered_extensions: bool,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            enabled: SUPPORTED_TOOL_NAMES
                .into_iter()
                // `exec` already provides faster, composable discovery through
                // rg/find/ls. Keep the narrower search schema available for
                // explicit allowlists without charging every default request.
                .filter(|name| *name != "search")
                .map(str::to_owned)
                .collect(),
            excluded: BTreeSet::new(),
            requested: None,
            allow_discovered_extensions: true,
        }
    }
}

impl ToolPolicy {
    /// Build and validate an explicit allowlist.
    pub fn only(names: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut enabled = BTreeSet::new();
        for name in names {
            let name = name.trim().to_ascii_lowercase();
            if !valid_tool_name(&name) {
                anyhow::bail!(
                    "invalid tool name {name:?}; built-ins: {}",
                    SUPPORTED_TOOL_NAMES.join(", ")
                );
            }
            enabled.insert(name);
        }
        Ok(Self {
            requested: Some(enabled.clone()),
            enabled,
            excluded: BTreeSet::new(),
            allow_discovered_extensions: false,
        })
    }

    /// Remove one validated tool name.
    pub fn exclude(&mut self, name: &str) -> anyhow::Result<()> {
        let name = name.trim().to_ascii_lowercase();
        if !valid_tool_name(&name) {
            anyhow::bail!(
                "invalid tool name {name:?}; built-ins: {}",
                SUPPORTED_TOOL_NAMES.join(", ")
            );
        }
        self.enabled.remove(&name);
        self.excluded.insert(name);
        Ok(())
    }

    /// Whether a schema and implementation may be registered.
    pub fn enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
            || (self.allow_discovered_extensions
                && !SUPPORTED_TOOL_NAMES.contains(&name)
                && valid_tool_name(name)
                && !self.excluded.contains(name))
    }

    /// Explicit requested names, when the user supplied `--tools`.
    pub fn explicit_names(&self) -> Option<impl Iterator<Item = &str>> {
        self.requested
            .as_ref()
            .map(|requested| requested.iter().map(String::as_str))
    }

    /// Deterministic enabled names for tests.
    #[cfg(test)]
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.enabled.iter().map(String::as_str)
    }
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-' | '.')
        })
}

impl SandboxPolicy {
    /// Whether this process is permitted to start *any* child process.
    ///
    /// Shell escapes, executable extensions, and the model `exec` tool all
    /// have process authority. Keep this gate shared so `--no-process` and
    /// `--no-shell` are truthful product-wide guarantees.
    pub fn process_execution_allowed(&self) -> bool {
        self.allow_process && self.allow_shell
    }

    /// Translate product settings to the frozen agent sandbox configuration.
    pub fn to_sandbox_config(&self, workspace: &Path) -> SandboxConfig {
        let mut sandbox = SandboxConfig::new(workspace);
        sandbox.allow_external_paths = self.allow_external_paths;
        sandbox.allow_edit = self.allow_edit;
        sandbox.allow_write = self.allow_write;
        sandbox.allow_process = self.allow_process;
        sandbox.allow_shell = self.allow_shell;
        sandbox.exec_timeout = Duration::from_secs(self.exec_timeout_secs);
        sandbox.max_output_bytes = self.max_output_bytes;
        sandbox
    }
}

/// Portable user-facing thinking levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    On,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            "minimal" | "min" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => anyhow::bail!(
                "invalid thinking level {value:?}; use off, on, minimal, low, medium, high, xhigh, or max"
            ),
        }
    }

    pub fn to_effort(self) -> ReasoningEffort {
        match self {
            Self::Minimal => ReasoningEffort::Minimal,
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::Xhigh => ReasoningEffort::Xhigh,
            Self::Max => ReasoningEffort::Max,
            Self::Off | Self::On => {
                unreachable!("binary thinking is not represented by ReasoningEffort")
            }
        }
    }

    pub fn pick_budget(self, budgets: &ReasoningEffortBudgets) -> u64 {
        match self {
            Self::Minimal => budgets.minimal,
            Self::Low => budgets.low,
            Self::Medium => budgets.medium,
            Self::High => budgets.high,
            Self::Xhigh => budgets.xhigh,
            Self::Max => budgets.max,
            Self::Off | Self::On => 0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Automatic compaction policy.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPolicy {
    pub threshold_fraction: f64,
    pub keep_recent_turns: usize,
    /// Optional model override for summary calls. When absent, bootstrap uses
    /// the active model; when present, bootstrap resolves this ID in the model
    /// catalog before constructing the agent.
    pub compact_model: Option<ModelId>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            threshold_fraction: 0.85,
            keep_recent_turns: 4,
            compact_model: None,
        }
    }
}

/// Resolved configuration for one process.
#[derive(Clone, Debug)]
pub struct Config {
    pub workspace: PathBuf,
    pub invocation_cwd: PathBuf,
    pub model: Option<ModelId>,
    /// True when `model` came from an explicit command-line override rather
    /// than defaults that a resumed session may supersede.
    pub model_explicit: bool,
    pub reasoning: ReasoningConfig,
    /// True when `reasoning` came from an explicit command-line override.
    pub reasoning_explicit: bool,
    pub cache_retention: CacheRetention,
    pub sandbox: SandboxPolicy,
    pub theme: Option<String>,
    /// Explicit theme directories, in precedence order after global/project.
    pub theme_paths: Vec<PathBuf>,
    pub color: ColorMode,
    /// Whether Ygg owns mouse scrolling and cross-viewport selection.
    pub mouse: MouseMode,
    /// Force the chronological ASCII frontend even on a capable TTY.
    pub plain: bool,
    pub session_dir: PathBuf,
    pub compaction: CompactionPolicy,
    /// Optional cumulative session spend limit in microdollars.
    pub max_cost_microdollars: Option<u64>,
    /// Optional warning threshold for one provider turn in microdollars.
    pub cost_warning_microdollars: Option<u64>,
    /// Show the current provider turn's cost in the compact TUI footer.
    /// Detailed cost diagnostics remain available regardless of this setting.
    pub show_turn_cost: bool,
    pub max_turns: Option<u64>,
    pub show_reasoning_in_print: bool,
    /// Prompt passed positionally for interactive startup, if any.
    pub initial_prompt: Option<String>,
    /// Named prompt template selected with `--prompt`.
    pub prompt_template: Option<String>,
    /// Expose the deterministic final expansion before provider submission.
    pub debug_prompt: bool,
    /// Explicit prompt-template files or directories, in precedence order.
    pub prompt_paths: Vec<PathBuf>,
    pub mode: Mode,
    pub resume: ResumeSelector,
    /// Additional directory paths to scan for agent skills.
    pub skill_paths: Vec<PathBuf>,
    /// Explicit executable-extension directories, in precedence order.
    pub extension_paths: Vec<PathBuf>,
    /// Executable extensions selected for activation.
    pub enabled_extensions: Vec<String>,
    /// Persistent executable trust grants: global names or `name@manifest-path`.
    pub trusted_extensions: Vec<String>,
    /// One-shot extension names trusted only for this process invocation.
    pub invocation_trusted_extensions: Vec<String>,
    /// One authoritative allowlist used for schema and implementation registration.
    pub tools: ToolPolicy,
    /// Load bounded `AGENTS.md` context files. Workspace files additionally
    /// require `workspace_trusted`.
    pub context_files: bool,
    /// Disable all optional startup discovery network calls.
    pub offline: bool,
    /// Trust the workspace and load project config, context, and skills.
    pub workspace_trusted: bool,
}

impl Config {
    /// Whether a tool will survive both the model-visible allowlist and the
    /// matching execution capability gate for this process.
    pub fn tool_available(&self, name: &str) -> bool {
        self.tools.enabled(name)
            && match name {
                "edit" => self.sandbox.allow_edit,
                "write" => self.sandbox.allow_write,
                // Process mode deliberately has shell-equivalent authority.
                "exec" => self.sandbox.process_execution_allowed(),
                _ => true,
            }
    }
}

/// Parse a reasoning override such as `high` or `budget=2048`.
pub fn parse_reasoning(value: &str) -> anyhow::Result<ReasoningConfig> {
    let config = match value.trim().to_ascii_lowercase().as_str() {
        "off" => ReasoningConfig::Off,
        "on" => ReasoningConfig::On,
        "minimal" | "min" => ReasoningConfig::Effort(ReasoningEffort::Minimal),
        "low" => ReasoningConfig::Effort(ReasoningEffort::Low),
        "medium" | "med" => ReasoningConfig::Effort(ReasoningEffort::Medium),
        "high" => ReasoningConfig::Effort(ReasoningEffort::High),
        "xhigh" | "x-high" => ReasoningConfig::Effort(ReasoningEffort::Xhigh),
        "max" => ReasoningConfig::Effort(ReasoningEffort::Max),
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

/// Parse a prompt-cache retention policy.
pub fn parse_cache_retention(value: &str) -> anyhow::Result<CacheRetention> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "disabled" => Ok(CacheRetention::None),
        "short" => Ok(CacheRetention::Short),
        "long" => Ok(CacheRetention::Long),
        _ => anyhow::bail!("invalid cache retention {value:?}; use none, short, or long"),
    }
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
    fn mouse_mode_defaults_to_native_selection_and_scrollback() {
        assert_eq!(MouseMode::parse("app").unwrap(), MouseMode::App);
        assert_eq!(MouseMode::parse("terminal").unwrap(), MouseMode::Terminal);
        assert_eq!(MouseMode::parse("off").unwrap(), MouseMode::Off);
        assert!(!MouseMode::Auto.application_owned());
        assert!(MouseMode::App.application_owned());
        assert!(!MouseMode::Terminal.application_owned());
        assert!(!MouseMode::Off.application_owned());
        assert!(MouseMode::parse("sometimes").is_err());
    }

    #[test]
    fn colour_mode_accepts_the_three_portable_policies() {
        assert_eq!(ColorMode::parse("auto").unwrap(), ColorMode::Auto);
        assert_eq!(ColorMode::parse("always").unwrap(), ColorMode::Always);
        assert_eq!(ColorMode::parse("never").unwrap(), ColorMode::Never);
        assert!(ColorMode::parse("sometimes").is_err());
    }

    #[test]
    fn cache_retention_accepts_disable_and_rejects_unknown_values() {
        assert_eq!(parse_cache_retention("none").unwrap(), CacheRetention::None);
        assert_eq!(
            parse_cache_retention("short").unwrap(),
            CacheRetention::Short
        );
        assert_eq!(parse_cache_retention("long").unwrap(), CacheRetention::Long);
        assert!(parse_cache_retention("sometimes").is_err());
    }

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
    fn trusted_local_paths_are_the_product_default() {
        let directory = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::default();
        assert!(policy.allow_external_paths);
        assert!(policy.allow_edit);
        assert!(policy.allow_write);
        assert!(policy.allow_process);
        assert!(policy.allow_shell);
        let sandbox = policy.to_sandbox_config(directory.path());
        assert!(sandbox.allow_external_paths);
    }

    #[test]
    fn tool_policy_validates_and_filters_names() {
        let mut policy = ToolPolicy::only(["read".to_owned(), "search".to_owned()]).unwrap();
        policy.exclude("search").unwrap();
        assert_eq!(policy.names().collect::<Vec<_>>(), vec!["read"]);
        assert_eq!(
            policy.explicit_names().unwrap().collect::<Vec<_>>(),
            vec!["read", "search"],
            "startup diagnostics must retain explicitly requested names removed by later gates"
        );

        let extension_policy = ToolPolicy::only(["git_status".to_owned()]).unwrap();
        assert!(extension_policy.enabled("git_status"));
        assert!(!extension_policy.enabled("another_extension_tool"));
        assert!(ToolPolicy::only(["not a tool".to_owned()]).is_err());
    }

    #[test]
    fn default_tool_policy_uses_exec_instead_of_a_redundant_search_schema() {
        let policy = ToolPolicy::default();
        for name in ["read", "edit", "write", "exec"] {
            assert!(policy.enabled(name), "{name}");
        }
        assert!(!policy.enabled("search"));
        assert!(ToolPolicy::only(["search".to_owned()])
            .unwrap()
            .enabled("search"));
    }

    #[test]
    fn thinking_levels_parse_short_and_full_names() {
        assert_eq!(ThinkingLevel::parse("off").unwrap(), ThinkingLevel::Off);
        assert_eq!(ThinkingLevel::parse("on").unwrap(), ThinkingLevel::On);
        assert_eq!(ThinkingLevel::parse("min").unwrap(), ThinkingLevel::Minimal);
        assert_eq!(ThinkingLevel::parse("high").unwrap(), ThinkingLevel::High);
        assert_eq!(ThinkingLevel::parse("xhigh").unwrap(), ThinkingLevel::Xhigh);
        assert_eq!(
            ThinkingLevel::parse("x-high").unwrap(),
            ThinkingLevel::Xhigh
        );
        assert_eq!(ThinkingLevel::parse("max").unwrap(), ThinkingLevel::Max);
        assert!(ThinkingLevel::parse("budget=2048").is_err());
    }

    #[test]
    fn thinking_level_xhigh_and_max_round_trip_labels_and_efforts() {
        for level in [ThinkingLevel::Xhigh, ThinkingLevel::Max] {
            assert_eq!(ThinkingLevel::parse(level.label()).unwrap(), level);
        }
        assert_eq!(ThinkingLevel::Xhigh.to_effort(), ReasoningEffort::Xhigh);
        assert_eq!(ThinkingLevel::Max.to_effort(), ReasoningEffort::Max);
    }

    #[test]
    fn reasoning_parser_accepts_effort_and_budget_values() {
        assert_eq!(parse_reasoning("off").unwrap(), ReasoningConfig::Off);
        assert_eq!(parse_reasoning("on").unwrap(), ReasoningConfig::On);
        assert_eq!(
            parse_reasoning("high").unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::High)
        );
        assert_eq!(
            parse_reasoning("xhigh").unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Xhigh)
        );
        assert_eq!(
            parse_reasoning("max").unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Max)
        );
        assert_eq!(
            parse_reasoning("budget=2048").unwrap(),
            ReasoningConfig::Budget(2048)
        );
        assert!(parse_reasoning("nonsense").is_err());
    }
}
