#![allow(missing_docs)]

use std::fmt;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::Deserialize;
use ygg_ai::ModelId;

use crate::app::bootstrap::resolve_model_id;
use crate::config::{
    self, ColorMode, CompactionMode, CompactionPolicy, Config, Mode, ResumeSelector, SandboxPolicy,
    ToolPolicy,
};
use crate::extension_package::ExtensionCommand;
use crate::migrate::MigrationCommand;
use crate::pi::PiCommand;
use crate::session_commands::SessionCommand;

#[derive(Clone, Debug, Subcommand)]
pub enum TopLevelCommand {
    /// Inspect and manage durable local sessions.
    Sessions {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Install and manage extension packages.
    Extension {
        #[command(subcommand)]
        command: ExtensionCommand,
    },
    /// Inspect another coding-agent setup and plan a bounded migration.
    Migrate {
        #[command(subcommand)]
        command: MigrationCommand,
    },
    /// Link existing Pi extensions through Ygg's compatibility host.
    Pi {
        #[command(subcommand)]
        command: PiCommand,
    },
    /// Check for, and install, a newer Ygg release.
    Update {
        /// Only report whether a newer release is available.
        #[arg(long)]
        check: bool,
    },
    /// Check local prerequisites, configured providers, and model visibility.
    Doctor,
    /// Launch the loopback-only Ygg Serve application.
    ///
    /// Default builds dispatch to the installed extension runtime; builds with
    /// the `serve` feature run the embedded implementation.
    Serve {
        /// Do not open the graphical client in the default browser.
        #[arg(long)]
        no_open: bool,
        /// Loopback TCP port. Zero asks the operating system for a free port.
        #[arg(long, default_value_t = 31415)]
        port: u16,
        /// Directory containing a development graphical shell.
        #[arg(long, value_name = "DIR")]
        web_root: Option<PathBuf>,
    },
}

/// Command-line launcher for `ygg`.
#[derive(Debug, Parser)]
#[command(
    name = "ygg",
    version,
    about = "A local-first coding agent",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<TopLevelCommand>,
    /// An initial prompt. In interactive mode it is submitted after startup.
    #[arg(value_name = "PROMPT")]
    pub message: Option<String>,
    /// Sign in to a subscription provider (e.g. `codex`) and exit.
    #[arg(long, value_name = "PROVIDER")]
    pub login: Option<String>,
    /// Sign out of a subscription provider (e.g. `codex`) and exit.
    #[arg(long, value_name = "PROVIDER")]
    pub logout: Option<String>,
    /// With `--login`, print the device URL/code without opening a browser.
    #[arg(long)]
    pub headless: bool,
    /// Frontend mode: interactive or rpc.
    #[arg(long, value_name = "MODE", conflicts_with = "print")]
    pub mode: Option<String>,
    /// Use headless print mode instead of the full-screen TUI.
    #[arg(long, short = 'p')]
    pub print: bool,
    /// Continue the newest session in this workspace.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub continue_: bool,
    /// Resume a session by id, or open the session picker interactively.
    #[arg(
        long,
        value_name = "ID",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with = "continue_"
    )]
    pub resume: Option<Option<String>>,
    /// Fork a session by id, or open the session picker when omitted.
    #[arg(
        long,
        value_name = "ID",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with_all = ["continue_", "resume"]
    )]
    pub fork: Option<Option<String>>,
    /// Model id override.
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning: off, minimal, low, medium, high, xhigh, max, ultra, or budget=N.
    #[arg(long)]
    pub reasoning: Option<String>,
    /// Deprecated persisted-session compatibility; Pro migrates to Ultra when V2 delegation is advertised.
    #[arg(long, value_name = "MODE", hide = true)]
    pub reasoning_mode: Option<String>,
    /// Prompt-cache retention: none, short, or long.
    #[arg(long, value_name = "POLICY")]
    pub cache_retention: Option<String>,
    /// Workspace root override.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Legacy TUI theme name; the current runtime always uses the compiled default.
    #[arg(long, value_name = "NAME", hide = true)]
    pub theme: Option<String>,
    /// Legacy theme directory option; the current runtime does not load custom themes.
    #[arg(long = "theme-dir", value_name = "DIR", hide = true)]
    pub theme_dirs: Vec<PathBuf>,
    /// Colour output policy: auto, always, or never.
    #[arg(long, value_name = "WHEN")]
    pub color: Option<String>,
    /// Use chronological ASCII output without cursor control.
    #[arg(long)]
    pub plain: bool,
    /// Mouse ownership: auto/terminal/off preserve native gestures; app
    /// captures wheel scrolling and drag selection for the semantic viewport.
    #[arg(long, value_name = "MODE")]
    pub mouse: Option<String>,
    /// Emit reasoning deltas in print mode.
    #[arg(long)]
    pub show_reasoning: bool,
    /// Maximum model turns in one run.
    #[arg(long)]
    pub max_turns: Option<u64>,
    /// Persistent session directory override.
    #[arg(long)]
    pub session_dir: Option<PathBuf>,
    /// Expand a named prompt template around the positional prompt.
    #[arg(long = "prompt", value_name = "NAME")]
    pub prompt_template: Option<String>,
    /// Print or display the fully expanded named prompt and its content hash.
    #[arg(long)]
    pub debug_prompt: bool,
    /// Append privacy-preserving run and tool metrics to a JSONL file.
    #[arg(long, value_name = "PATH")]
    pub telemetry: Option<PathBuf>,
    /// Explicit prompt-template file or directory (repeatable, Pi compatible).
    #[arg(long = "prompt-template", value_name = "PATH")]
    pub prompt_templates: Vec<PathBuf>,
    /// Override the composed system prompt. Use `--system-prompt` to clear it.
    #[arg(
        long = "system-prompt",
        value_name = "PROMPT",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub system_prompt: Option<String>,
    /// Additional directory paths to scan for agent skills.
    #[arg(long = "skill-dir", value_name = "DIR")]
    pub skill_dirs: Vec<PathBuf>,
    /// Additional directory paths to scan for executable extensions.
    #[arg(long = "extension-dir", value_name = "DIR")]
    pub extension_dirs: Vec<PathBuf>,
    /// Explicitly enable executable extensions by name (comma-separated).
    #[arg(
        long = "enable-extension",
        value_name = "NAMES",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub enable_extensions: Vec<String>,
    /// Trust the selected extension source for this invocation (comma-separated).
    #[arg(
        long = "trust-extension",
        value_name = "NAMES",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub trust_extensions: Vec<String>,
    /// Trust this workspace and load its project config, AGENTS.md, and skills.
    #[arg(long = "workspace-trusted", alias = "trust-workspace")]
    pub workspace_trusted: bool,
    /// Require approval for every bash call and keep host effects controlled.
    #[arg(long = "safe-mode", alias = "safe")]
    pub safe_mode: bool,
    /// Load only these tools (comma-separated).
    #[arg(long, value_name = "NAMES", value_delimiter = ',', num_args = 1..)]
    pub tools: Option<Vec<String>>,
    /// Remove tools from the active set (comma-separated).
    #[arg(long, value_name = "NAMES", value_delimiter = ',', num_args = 1..)]
    pub exclude_tools: Vec<String>,
    /// Disable every built-in and skill tool.
    #[arg(long, conflicts_with = "tools")]
    pub no_tools: bool,
    /// Disable both file mutation tools (`edit` and `write`).
    #[arg(long)]
    pub no_edit: bool,
    /// Disable full-file creation and replacement.
    #[arg(long)]
    pub no_write: bool,
    /// Disable all command execution.
    #[arg(long)]
    pub no_process: bool,
    /// Disable all command execution (process execution is shell-equivalent authority).
    #[arg(long)]
    pub no_shell: bool,
    /// Explicitly enable command execution (overrides a disabling user setting).
    #[arg(long)]
    pub allow_shell: bool,
    /// Allow `read` to fetch public HTTPS image/audio URLs.
    #[arg(long, conflicts_with = "offline")]
    pub allow_remote_read: bool,
    /// Bash-compatible shell executable used by the `bash` tool.
    #[arg(long, value_name = "PATH")]
    pub shell_path: Option<PathBuf>,
    /// Do not load global or workspace AGENTS.md files.
    #[arg(long)]
    pub no_context_files: bool,
    /// Disable optional provider/model discovery network requests at startup.
    #[arg(long)]
    pub offline: bool,
    /// Treat unknown configuration keys as startup errors instead of warnings.
    #[arg(long)]
    pub strict_config: bool,
    /// Maximum `bash` tool execution time in seconds.
    #[arg(long, alias = "exec-timeout-secs")]
    pub bash_timeout_secs: Option<u64>,
    /// Maximum persisted tool output size in bytes.
    #[arg(long)]
    pub max_output_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CompactionLayer {
    #[serde(alias = "policy")]
    mode: Option<String>,
    /// Deprecated boolean spelling retained for existing configuration.
    enabled: Option<bool>,
    threshold_fraction: Option<f64>,
    max_active_tokens: Option<u64>,
    keep_recent_tokens: Option<u64>,
    /// Deprecated turn-count retention, mapped to 1,000 tokens per turn.
    keep_recent_turns: Option<usize>,
    compact_model: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ConfigLayer {
    model: Option<String>,
    reasoning: Option<String>,
    reasoning_mode: Option<String>,
    cache_retention: Option<String>,
    theme: Option<String>,
    color: Option<String>,
    mouse: Option<String>,
    plain: Option<bool>,
    allow_external_paths: Option<bool>,
    allow_edit: Option<bool>,
    allow_write: Option<bool>,
    allow_process: Option<bool>,
    allow_shell: Option<bool>,
    allow_remote_read: Option<bool>,
    shell_path: Option<PathBuf>,
    #[serde(alias = "exec_timeout_secs")]
    bash_timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    session_dir: Option<PathBuf>,
    max_turns: Option<u64>,
    max_cost_microdollars: Option<u64>,
    cost_warning_microdollars: Option<u64>,
    context_files: Option<bool>,
    offline: Option<bool>,
    strict_config: Option<bool>,
    telemetry: Option<PathBuf>,
    enabled_extensions: Option<Vec<String>>,
    trusted_extensions: Option<Vec<String>>,
    system_prompt: Option<String>,
    compaction: Option<CompactionLayer>,
}

impl ConfigLayer {
    fn merge(&mut self, newer: Self) {
        macro_rules! override_some {
            ($field:ident) => {
                if newer.$field.is_some() {
                    self.$field = newer.$field;
                }
            };
        }
        override_some!(model);
        override_some!(reasoning);
        override_some!(reasoning_mode);
        override_some!(cache_retention);
        override_some!(theme);
        override_some!(color);
        override_some!(mouse);
        override_some!(plain);
        override_some!(allow_external_paths);
        override_some!(allow_edit);
        override_some!(allow_write);
        override_some!(allow_process);
        override_some!(allow_shell);
        override_some!(allow_remote_read);
        override_some!(shell_path);
        override_some!(bash_timeout_secs);
        override_some!(max_output_bytes);
        override_some!(session_dir);
        override_some!(max_turns);
        override_some!(max_cost_microdollars);
        override_some!(cost_warning_microdollars);
        override_some!(context_files);
        override_some!(offline);
        override_some!(strict_config);
        override_some!(telemetry);
        override_some!(enabled_extensions);
        override_some!(trusted_extensions);
        override_some!(system_prompt);
        match (self.compaction.as_mut(), newer.compaction) {
            (Some(current), Some(newer)) => {
                if newer.mode.is_some() {
                    current.mode = newer.mode;
                    current.enabled = None;
                } else if newer.enabled.is_some() {
                    current.enabled = newer.enabled;
                    current.mode = None;
                }
                if newer.threshold_fraction.is_some() {
                    current.threshold_fraction = newer.threshold_fraction;
                }
                if newer.max_active_tokens.is_some() {
                    current.max_active_tokens = newer.max_active_tokens;
                }
                if newer.keep_recent_tokens.is_some() {
                    current.keep_recent_tokens = newer.keep_recent_tokens;
                    current.keep_recent_turns = None;
                } else if newer.keep_recent_turns.is_some() {
                    current.keep_recent_turns = newer.keep_recent_turns;
                    current.keep_recent_tokens = None;
                }
                if newer.compact_model.is_some() {
                    current.compact_model = newer.compact_model;
                }
            }
            (None, Some(newer)) => self.compaction = Some(newer),
            _ => {}
        }
    }

    /// Merge a trusted project layer without allowing it to relax authority or
    /// resource floors established by the user's global configuration.
    fn merge_project(&mut self, mut project: Self) {
        fn tighten_bool(current: &mut Option<bool>, project: Option<bool>) {
            if let Some(project) = project {
                *current = Some(current.unwrap_or(true) && project);
            }
        }
        fn lower_u64(current: &mut Option<u64>, project: Option<u64>) {
            if let Some(project) = project {
                *current = Some(current.map_or(project, |current| current.min(project)));
            }
        }
        fn lower_usize(current: &mut Option<usize>, project: Option<usize>) {
            if let Some(project) = project {
                *current = Some(current.map_or(project, |current| current.min(project)));
            }
        }

        tighten_bool(
            &mut self.allow_external_paths,
            project.allow_external_paths.take(),
        );
        tighten_bool(&mut self.allow_edit, project.allow_edit.take());
        tighten_bool(&mut self.allow_write, project.allow_write.take());
        tighten_bool(&mut self.allow_process, project.allow_process.take());
        tighten_bool(&mut self.allow_shell, project.allow_shell.take());
        // Remote reads are opt-in. A project may revoke a user grant, but may
        // never create network authority when the user/global layer omitted it.
        if project.allow_remote_read.take() == Some(false) {
            self.allow_remote_read = Some(false);
        }
        tighten_bool(&mut self.context_files, project.context_files.take());
        lower_u64(
            &mut self.bash_timeout_secs,
            project.bash_timeout_secs.take(),
        );
        lower_usize(&mut self.max_output_bytes, project.max_output_bytes.take());
        lower_u64(&mut self.max_turns, project.max_turns.take());
        lower_u64(
            &mut self.max_cost_microdollars,
            project.max_cost_microdollars.take(),
        );
        lower_u64(
            &mut self.cost_warning_microdollars,
            project.cost_warning_microdollars.take(),
        );
        // Offline and strict diagnostics are one-way safety settings for
        // project configuration.
        self.offline =
            Some(self.offline.unwrap_or(false) || project.offline.take().unwrap_or(false));
        if project.strict_config.take() == Some(true) {
            self.strict_config = Some(true);
        }
        // A trusted project may suggest activation, but executable trust is a
        // user-level decision and can never be granted by project config.
        let trusted_extensions = self.trusted_extensions.clone();
        project.trusted_extensions = None;
        self.merge(project);
        self.trusted_extensions = trusted_extensions;
    }
}

pub fn global_config_path() -> Option<PathBuf> {
    global_config_path_from_home(dirs::home_dir())
}

fn global_config_path_from_home(home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|home| home.is_absolute())
        .map(|home| home.join(".ygg").join("config.toml"))
}

pub fn persist_model(model: &str) -> anyhow::Result<()> {
    let path = global_config_path().ok_or_else(|| {
        anyhow::anyhow!("cannot persist model: user home directory is unavailable")
    })?;
    persist_key_to_path("model", model, &path)
}

pub fn persist_reasoning(reasoning: &str) -> anyhow::Result<()> {
    let path = global_config_path().ok_or_else(|| {
        anyhow::anyhow!("cannot persist reasoning: user home directory is unavailable")
    })?;
    persist_key_to_path("reasoning", reasoning, &path)
}

pub fn persist_reasoning_mode(mode: ygg_ai::ReasoningMode) -> anyhow::Result<()> {
    let path = global_config_path().ok_or_else(|| {
        anyhow::anyhow!("cannot persist reasoning mode: user home directory is unavailable")
    })?;
    let value = match mode {
        ygg_ai::ReasoningMode::Standard => "standard",
        ygg_ai::ReasoningMode::Pro => "pro",
    };
    persist_key_to_path("reasoning_mode", value, &path)
}

/// Persist one executable extension's activation without copying merged
/// project, environment, or command-line activation into the user config.
pub fn persist_extension_enabled(name: &str, enabled: bool) -> anyhow::Result<Vec<String>> {
    let path = global_config_path().ok_or_else(|| {
        anyhow::anyhow!("cannot persist extension activation: user home directory is unavailable")
    })?;
    persist_extension_enabled_to_path(name, enabled, &path)
}

/// Revalidate that the user config remains the next-launch authority before an
/// interactive extension toggle mutates it. A newly added trusted-project layer
/// fails closed instead of making a global edit look durable when it is not.
pub fn extension_activation_menu_authoritative(config: &Config) -> anyhow::Result<bool> {
    if config.extension_activation_overridden {
        return Ok(false);
    }
    if !config.workspace_trusted {
        return Ok(true);
    }
    let project = read_layer(
        &project_config_path(&config.workspace),
        ConfigSourceKind::Project,
    )?;
    Ok(project.values.enabled_extensions.is_none())
}

fn persist_extension_enabled_to_path(
    name: &str,
    enabled: bool,
    path: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let name = normalize_extension_name(name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _config_lock = config_update_lock(path)?;

    let original = match std::fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let content = original.as_deref().unwrap_or_default();
    let mut document = if content.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        content.parse::<toml_edit::DocumentMut>().map_err(|error| {
            anyhow::anyhow!("cannot update invalid config {}: {error}", path.display())
        })?
    };
    let mut names = std::collections::BTreeSet::new();
    if let Some(item) = document.get("enabled_extensions") {
        let values = item.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot update config {}: enabled_extensions must be an array of names",
                path.display()
            )
        })?;
        for value in values {
            let value = value.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot update config {}: enabled_extensions must contain only strings",
                    path.display()
                )
            })?;
            names.insert(normalize_extension_name(value)?);
        }
    }
    if enabled {
        names.insert(name);
    } else {
        names.remove(&name);
    }

    let mut values = toml_edit::Array::new();
    for name in &names {
        values.push(name.as_str());
    }
    document["enabled_extensions"] = toml_edit::value(values);
    write_config_atomically(path, &document.to_string(), original.as_deref())?;
    Ok(names.into_iter().collect())
}

fn config_update_lock(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config path {} has no file name", path.display()))?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(&lock_path)?;
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
        anyhow::anyhow!(
            "another config update is in progress for {}: {error}",
            path.display()
        )
    })?;
    Ok(file)
}

fn write_config_atomically(
    path: &std::path::Path,
    content: &str,
    expected: Option<&str>,
) -> anyhow::Result<()> {
    const MAX_CONFIG_BYTES: usize = 1024 * 1024;
    ygg_agent::secure_fs::write_atomic_if_unchanged(
        path,
        expected.map(str::as_bytes),
        content.as_bytes(),
        MAX_CONFIG_BYTES,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "could not atomically update config {} without overwriting a concurrent edit: {error}",
            path.display()
        )
    })
}

fn persist_key_to_path(key: &str, value: &str, path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _config_lock = config_update_lock(path)?;

    let original = match std::fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let content = original.as_deref().unwrap_or_default();
    let mut document = if content.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        content.parse::<toml_edit::DocumentMut>().map_err(|error| {
            anyhow::anyhow!("cannot update invalid config {}: {error}", path.display())
        })?
    };

    // Structural TOML editing avoids partial-key matches and orphaned lines
    // from multiline values while retaining the user's comments and layout.
    document[key] = toml_edit::value(value);
    let new_content = document.to_string();

    // Atomic write: write to a unique sibling temp file then rename over the
    // real path so a crash or concurrent config writer cannot leave a partial
    // or collide with this writer's staging file.
    write_config_atomically(path, &new_content, original.as_deref())
}

#[cfg(test)]
fn persist_model_to_path(model: &str, path: &std::path::Path) -> anyhow::Result<()> {
    persist_key_to_path("model", model, path)
}

fn project_config_path(workspace: &Path) -> PathBuf {
    workspace.join(".ygg").join("config.toml")
}

fn split_names(value: String) -> Vec<String> {
    value.split(',').map(str::to_owned).collect()
}

fn normalize_extension_names(
    names: impl IntoIterator<Item = String>,
) -> anyhow::Result<Vec<String>> {
    let mut normalized = std::collections::BTreeSet::new();
    for name in names {
        normalized.insert(normalize_extension_name(&name)?);
    }
    Ok(normalized.into_iter().collect())
}

fn normalize_extension_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim().to_ascii_lowercase();
    let mut characters = name.chars();
    let valid = name.len() <= 64
        && characters
            .next()
            .is_some_and(|character| character.is_ascii_lowercase())
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        });
    if !valid {
        anyhow::bail!(
            "invalid extension name {name:?}; use a lowercase letter followed by lowercase letters, digits, or '-' (64 bytes maximum)"
        );
    }
    Ok(name)
}

/// Normalize persistent executable trust grants without erasing their source
/// binding. A bare name applies only to the global extension directory. A
/// project or explicit source uses `name@path/to/extension.toml`; an installed
/// aggregate can additionally bind its principal as `name@path@sha256:<hex>`.
fn normalize_extension_trust_grants(
    grants: impl IntoIterator<Item = String>,
) -> anyhow::Result<Vec<String>> {
    let mut normalized = std::collections::BTreeSet::new();
    for grant in grants {
        let grant = grant.trim();
        let (source, digest) = match grant.rsplit_once("@sha256:") {
            Some((source, digest)) => {
                if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    anyhow::bail!(
                        "invalid extension trust digest {digest:?}; SHA-256 must contain exactly 64 hexadecimal characters"
                    );
                }
                (source, Some(digest.to_ascii_lowercase()))
            }
            None => (grant, None),
        };
        let normalized_grant = if let Some((name, path)) = source.split_once('@') {
            let name = normalize_extension_name(name)?;
            let path = path.trim();
            if path.is_empty()
                || path.len() > 8 * 1024
                || path.chars().any(char::is_control)
                || !Path::new(path).is_absolute()
                || Path::new(path).file_name().and_then(|name| name.to_str())
                    != Some(ygg_agent::extension_process::EXTENSION_MANIFEST_FILENAME)
            {
                anyhow::bail!(
                    "invalid extension trust path {path:?}; persistent source-bound grants require an absolute path to extension.toml"
                );
            }
            match digest {
                Some(digest) => format!("{name}@{path}@sha256:{digest}"),
                None => format!("{name}@{path}"),
            }
        } else if digest.is_some() {
            anyhow::bail!(
                "invalid identity-bound extension trust grant {grant:?}; an absolute manifest path is required before @sha256:"
            );
        } else {
            normalize_extension_name(source)?
        };
        normalized.insert(normalized_grant);
    }
    Ok(normalized.into_iter().collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigSourceKind {
    Global,
    Project,
}

impl fmt::Display for ConfigSourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Global => "global",
            Self::Project => "project",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConfigDiagnostic {
    source_kind: ConfigSourceKind,
    path: PathBuf,
    key: String,
    line: usize,
    column: usize,
    suggestion: Option<&'static str>,
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} config {}:{}:{}: unknown configuration key {:?}",
            self.source_kind,
            self.path.display(),
            self.line,
            self.column,
            self.key
        )?;
        if let Some(suggestion) = self.suggestion {
            write!(formatter, "; did you mean {suggestion:?}?")?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct LoadedConfigLayer {
    values: ConfigLayer,
    diagnostics: Vec<ConfigDiagnostic>,
}

const CONFIG_KEYS: &[&str] = &[
    "model",
    "reasoning",
    "reasoning_mode",
    "cache_retention",
    "theme",
    "color",
    "mouse",
    "plain",
    "allow_external_paths",
    "allow_edit",
    "allow_write",
    "allow_process",
    "allow_shell",
    "allow_remote_read",
    "shell_path",
    "bash_timeout_secs",
    "exec_timeout_secs",
    "max_output_bytes",
    "session_dir",
    "max_turns",
    "max_cost_microdollars",
    "cost_warning_microdollars",
    "context_files",
    "offline",
    "strict_config",
    "telemetry",
    "enabled_extensions",
    "trusted_extensions",
    "system_prompt",
    "compaction",
];

/// Legacy configuration keys accepted and ignored for backward
/// compatibility. Removed settings must be listed here so older configs keep
/// loading without unknown-key warnings or strict-mode rejections.
const IGNORED_CONFIG_KEYS: &[&str] = &["show_turn_cost"];

const COMPACTION_KEYS: &[&str] = &[
    "mode",
    "policy",
    "enabled",
    "threshold_fraction",
    "max_active_tokens",
    "keep_recent_tokens",
    "keep_recent_turns",
    "compact_model",
];

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    let mut current = vec![0; previous.len()];
    for (left_index, left_character) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_character) in right.chars().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_character != right_character));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.chars().count()]
}

fn config_key_suggestion(key: &str) -> Option<&'static str> {
    let (prefix, leaf, candidates) = match key.rsplit_once('.') {
        Some(("compaction", leaf)) => ("compaction.", leaf, COMPACTION_KEYS),
        Some(_) => return None,
        None => ("", key, CONFIG_KEYS),
    };
    let (candidate, distance) = candidates
        .iter()
        .map(|candidate| (*candidate, edit_distance(leaf, candidate)))
        .min_by_key(|(_, distance)| *distance)?;
    let threshold = 2.max(leaf.chars().count() / 3);
    (distance <= threshold).then(|| {
        if prefix.is_empty() {
            candidate
        } else {
            match candidate {
                "mode" => "compaction.mode",
                "policy" => "compaction.policy",
                "enabled" => "compaction.enabled",
                "threshold_fraction" => "compaction.threshold_fraction",
                "max_active_tokens" => "compaction.max_active_tokens",
                "keep_recent_tokens" => "compaction.keep_recent_tokens",
                "keep_recent_turns" => "compaction.keep_recent_turns",
                "compact_model" => "compaction.compact_model",
                _ => unreachable!("compaction suggestion came from the fixed schema"),
            }
        }
    })
}

fn table_key_offset(table: &toml_edit::Table, segments: &[&str]) -> Option<usize> {
    let (segment, remaining) = segments.split_first()?;
    let key = table.key(segment)?;
    if remaining.is_empty() {
        return key.span().map(|span| span.start);
    }
    let item = table.get(segment)?;
    if let Some(table) = item.as_table() {
        table_key_offset(table, remaining)
    } else {
        inline_table_key_offset(item.as_inline_table()?, remaining)
    }
}

fn inline_table_key_offset(table: &toml_edit::InlineTable, segments: &[&str]) -> Option<usize> {
    let (segment, remaining) = segments.split_first()?;
    let key = table.key(segment)?;
    if remaining.is_empty() {
        return key.span().map(|span| span.start);
    }
    inline_table_key_offset(table.get(segment)?.as_inline_table()?, remaining)
}

fn ignored_config_path(path: &serde_ignored::Path<'_>, segments: &mut Vec<String>) {
    match path {
        serde_ignored::Path::Root => {}
        serde_ignored::Path::Map { parent, key } => {
            ignored_config_path(parent, segments);
            segments.push(key.clone());
        }
        serde_ignored::Path::Seq { parent, index } => {
            ignored_config_path(parent, segments);
            segments.push(index.to_string());
        }
        serde_ignored::Path::Some { parent }
        | serde_ignored::Path::NewtypeStruct { parent }
        | serde_ignored::Path::NewtypeVariant { parent } => {
            ignored_config_path(parent, segments);
        }
    }
}

fn config_key_location(source: &str, segments: &[String]) -> (usize, usize) {
    let offset = toml_edit::ImDocument::parse(source.to_owned())
        .ok()
        .and_then(|document| {
            let segments = segments.iter().map(String::as_str).collect::<Vec<_>>();
            table_key_offset(document.as_table(), &segments)
        })
        .unwrap_or(0);
    let prefix = &source[..offset.min(source.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, tail)| tail)
        .chars()
        .count()
        + 1;
    (line, column)
}

fn report_config_diagnostics(diagnostics: &[ConfigDiagnostic], strict: bool) -> anyhow::Result<()> {
    if diagnostics.is_empty() {
        return Ok(());
    }
    if strict {
        let details = diagnostics
            .iter()
            .map(|diagnostic| format!("  - {diagnostic}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("strict configuration rejected unknown keys:\n{details}");
    }
    for diagnostic in diagnostics {
        crate::output::stderr_line(format!("warning: {diagnostic}"));
    }
    Ok(())
}

fn read_layer(path: &Path, source_kind: ConfigSourceKind) -> anyhow::Result<LoadedConfigLayer> {
    const MAX_CONFIG_BYTES: usize = 1024 * 1024;
    let Some(name) = path.file_name() else {
        anyhow::bail!("config path {} has no file name", path.display());
    };
    let Some(parent) = path.parent() else {
        anyhow::bail!("config path {} has no parent", path.display());
    };
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedConfigLayer::default())
        }
        Err(error) => return Err(error.into()),
    };
    let source =
        match ygg_agent::secure_fs::read_regular_file_bounded(&parent.join(name), MAX_CONFIG_BYTES)
        {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("config {} is not valid UTF-8", path.display()))?,
            Err(ygg_agent::secure_fs::SecureFileError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(LoadedConfigLayer::default())
            }
            Err(error) => anyhow::bail!("cannot read config {}: {error}", path.display()),
        };

    let mut unknown_keys = Vec::new();
    let deserializer = toml::Deserializer::new(&source);
    let values = serde_ignored::deserialize(deserializer, |path| {
        let mut segments = Vec::new();
        ignored_config_path(&path, &mut segments);
        unknown_keys.push(segments);
    })
    .map_err(|error| anyhow::anyhow!("invalid config {}: {error}", path.display()))?;
    unknown_keys.retain(|segments| {
        segments.len() != 1 || !IGNORED_CONFIG_KEYS.contains(&segments[0].as_str())
    });
    unknown_keys.sort();
    unknown_keys.dedup();
    let diagnostics = unknown_keys
        .into_iter()
        .map(|segments| {
            let (line, column) = config_key_location(&source, &segments);
            let key = segments.join(".");
            ConfigDiagnostic {
                source_kind,
                path: path.to_path_buf(),
                suggestion: config_key_suggestion(&key),
                key,
                line,
                column,
            }
        })
        .collect();
    Ok(LoadedConfigLayer {
        values,
        diagnostics,
    })
}

#[cfg(not(test))]
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(not(test))]
fn env_parse<T>(name: &str) -> anyhow::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    env_value(name)
        .map(|value| {
            value
                .parse::<T>()
                .map_err(|error| anyhow::anyhow!("invalid {name}={value:?}: {error}"))
        })
        .transpose()
}

#[cfg(not(test))]
fn environment_layer() -> anyhow::Result<ConfigLayer> {
    let compaction_mode = env_value("YGG_COMPACTION_MODE");
    let compaction_enabled = env_parse("YGG_AUTO_COMPACT")?;
    let threshold_fraction = env_parse("YGG_COMPACTION_THRESHOLD_FRACTION")?;
    let max_active_tokens = env_parse("YGG_COMPACTION_MAX_ACTIVE_TOKENS")?;
    let keep_recent_tokens = env_parse("YGG_COMPACTION_KEEP_RECENT_TOKENS")?;
    let keep_recent_turns = env_parse("YGG_COMPACTION_KEEP_RECENT_TURNS")?;
    let compact_model = env_value("YGG_COMPACT_MODEL");
    Ok(ConfigLayer {
        model: env_value("YGG_MODEL"),
        reasoning: env_value("YGG_REASONING"),
        reasoning_mode: env_value("YGG_REASONING_MODE"),
        cache_retention: env_value("YGG_CACHE_RETENTION")
            .or_else(|| env_value("PI_CACHE_RETENTION")),
        theme: env_value("YGG_THEME"),
        color: env_value("YGG_COLOR"),
        mouse: env_value("YGG_MOUSE"),
        plain: env_parse("YGG_PLAIN")?,
        allow_external_paths: env_parse("YGG_ALLOW_EXTERNAL_PATHS")?,
        allow_edit: env_parse("YGG_ALLOW_EDIT")?,
        allow_write: env_parse("YGG_ALLOW_WRITE")?,
        allow_process: env_parse("YGG_ALLOW_PROCESS")?,
        allow_shell: env_parse("YGG_ALLOW_SHELL")?,
        allow_remote_read: env_parse("YGG_ALLOW_REMOTE_READ")?,
        shell_path: env_value("YGG_SHELL_PATH").map(PathBuf::from),
        bash_timeout_secs: env_parse("YGG_BASH_TIMEOUT_SECS")?
            .or(env_parse("YGG_EXEC_TIMEOUT_SECS")?),
        max_output_bytes: env_parse("YGG_MAX_OUTPUT_BYTES")?,
        session_dir: env_value("YGG_SESSION_DIR").map(PathBuf::from),
        max_turns: env_parse("YGG_MAX_TURNS")?,
        max_cost_microdollars: env_parse("YGG_MAX_COST_MICRODOLLARS")?,
        cost_warning_microdollars: env_parse("YGG_COST_WARNING_MICRODOLLARS")?,
        context_files: env_parse("YGG_CONTEXT_FILES")?,
        offline: env_parse("YGG_OFFLINE")?,
        strict_config: env_parse("YGG_STRICT_CONFIG")?,
        telemetry: env_value("YGG_TELEMETRY").map(PathBuf::from),
        enabled_extensions: env_value("YGG_EXTENSIONS").map(split_names),
        trusted_extensions: env_value("YGG_TRUSTED_EXTENSIONS").map(split_names),
        system_prompt: env_value("YGG_SYSTEM_PROMPT"),
        compaction: (compaction_mode.is_some()
            || compaction_enabled.is_some()
            || threshold_fraction.is_some()
            || max_active_tokens.is_some()
            || keep_recent_tokens.is_some()
            || keep_recent_turns.is_some()
            || compact_model.is_some())
        .then_some(CompactionLayer {
            mode: compaction_mode,
            enabled: compaction_enabled,
            threshold_fraction,
            max_active_tokens,
            keep_recent_tokens,
            keep_recent_turns,
            compact_model,
        }),
    })
}

#[cfg(test)]
fn environment_layer() -> anyhow::Result<ConfigLayer> {
    // Unit tests must never inherit provider, credential, session, or policy
    // state from the developer's real process environment.
    Ok(ConfigLayer::default())
}

fn build_config_with_global_path(
    cli: Cli,
    cwd: &Path,
    global_path: Option<&Path>,
) -> anyhow::Result<Config> {
    let invocation_cwd = cwd.canonicalize()?;
    let workspace = config::resolve_workspace(cli.workspace.as_deref(), &invocation_cwd)?;
    if !invocation_cwd.starts_with(&workspace) {
        anyhow::bail!(
            "invocation directory {} is outside workspace {}",
            invocation_cwd.display(),
            workspace.display()
        );
    }

    let model_explicit = cli.model.is_some();
    let reasoning_explicit = cli.reasoning.is_some();
    let reasoning_mode_explicit = cli.reasoning_mode.is_some();

    // A missing home directory disables global config. Never reinterpret the
    // invocation directory as user scope: that would let an untrusted project
    // smuggle executable trust through `./.ygg/config.toml`.
    let global = match global_path {
        Some(path) => read_layer(path, ConfigSourceKind::Global)?,
        None => LoadedConfigLayer::default(),
    };
    let project = if cli.workspace_trusted {
        read_layer(&project_config_path(&workspace), ConfigSourceKind::Project)?
    } else {
        LoadedConfigLayer::default()
    };
    let environment = environment_layer()?;
    let extension_activation_overridden = project.values.enabled_extensions.is_some()
        || environment.enabled_extensions.is_some()
        || !cli.enable_extensions.is_empty();
    let mut diagnostics = global.diagnostics;
    diagnostics.extend(project.diagnostics);
    let mut values = global.values;
    values.merge_project(project.values);
    values.merge(environment);
    report_config_diagnostics(
        &diagnostics,
        cli.strict_config || values.strict_config.unwrap_or(false),
    )?;

    let model = resolve_model_id(
        cli.model.clone().map(ygg_ai::ModelId),
        values.model.clone().map(ygg_ai::ModelId),
        None,
    );
    let reasoning = match cli.reasoning.as_deref().or(values.reasoning.as_deref()) {
        Some(value) => config::parse_reasoning(value)?,
        None => ygg_ai::ReasoningConfig::Off,
    };
    let reasoning_mode = match cli
        .reasoning_mode
        .as_deref()
        .or(values.reasoning_mode.as_deref())
    {
        Some(value) => config::parse_reasoning_mode(value)?,
        None => ygg_ai::ReasoningMode::Standard,
    };
    let cache_retention = match cli
        .cache_retention
        .as_deref()
        .or(values.cache_retention.as_deref())
    {
        Some(value) => config::parse_cache_retention(value)?,
        None => ygg_ai::CacheRetention::Short,
    };
    let color = match cli.color.as_deref().or(values.color.as_deref()) {
        Some(value) => ColorMode::parse(value)?,
        None => ColorMode::Auto,
    };
    let mouse = match cli.mouse.as_deref().or(values.mouse.as_deref()) {
        Some(value) => config::MouseMode::parse(value)?,
        None => config::MouseMode::Auto,
    };
    let system_prompt = cli.system_prompt.or(values.system_prompt);
    let effect_policy = if cli.safe_mode {
        ygg_agent::EffectPolicy::ControlledBashApproval
    } else {
        ygg_agent::EffectPolicy::UnsafeHost
    };

    let mut sandbox = SandboxPolicy::default();
    if let Some(value) = values.allow_external_paths {
        sandbox.allow_external_paths = value;
    }
    if let Some(value) = values.allow_edit {
        sandbox.allow_edit = value;
    }
    if let Some(value) = values.allow_write {
        sandbox.allow_write = value;
    }
    if let Some(value) = values.allow_process {
        sandbox.allow_process = value;
    }
    if let Some(value) = values.allow_shell {
        sandbox.allow_shell = value;
    }
    if let Some(value) = values.allow_remote_read {
        sandbox.allow_remote_read = value;
    }
    if let Some(value) = values.shell_path {
        sandbox.shell_path = Some(value);
    }
    if let Some(value) = values.bash_timeout_secs {
        sandbox.bash_timeout_secs = value;
    }
    if let Some(value) = values.max_output_bytes {
        sandbox.max_output_bytes = value;
    }
    if cli.no_edit {
        sandbox.allow_edit = false;
        sandbox.allow_write = false;
    }
    if cli.no_write {
        sandbox.allow_write = false;
    }
    if cli.no_process || cli.no_shell {
        // Arbitrary process execution has shell-equivalent authority; these
        // flags are aliases at the enforcement boundary.
        sandbox.allow_process = false;
        sandbox.allow_shell = false;
    }
    if cli.allow_shell {
        sandbox.allow_process = true;
        sandbox.allow_shell = true;
    }
    if cli.allow_remote_read {
        sandbox.allow_remote_read = true;
    }
    if let Some(value) = cli.shell_path {
        sandbox.shell_path = Some(value);
    }
    if let Some(value) = cli.bash_timeout_secs {
        sandbox.bash_timeout_secs = value;
    }
    if let Some(value) = cli.max_output_bytes {
        sandbox.max_output_bytes = value;
    }
    let offline = cli.offline || values.offline.unwrap_or(false);
    if offline {
        sandbox.allow_remote_read = false;
    }
    if effect_policy != ygg_agent::EffectPolicy::UnsafeHost {
        // External-path classification cannot remain stable between admission
        // and execution. Keep controlled operations workspace-relative so the
        // broker's workspace/host distinction fails closed.
        sandbox.allow_external_paths = false;
    }
    sandbox.bash_timeout_secs = sandbox.bash_timeout_secs.clamp(1, 3_600);
    sandbox.max_output_bytes = sandbox.max_output_bytes.clamp(1_024, 1024 * 1024);

    let mut tools = match cli.tools {
        Some(names) => ToolPolicy::only(names)?,
        None if cli.no_tools => ToolPolicy::only(Vec::new())?,
        None => ToolPolicy::default(),
    };
    for name in &cli.exclude_tools {
        tools.exclude(name)?;
    }
    if cli.no_edit {
        tools.exclude("edit")?;
        tools.exclude("write")?;
    }
    if cli.no_write {
        tools.exclude("write")?;
    }
    if cli.no_process || cli.no_shell {
        tools.exclude("bash")?;
    }
    if !sandbox.allow_edit {
        tools.exclude("edit")?;
    }
    if !sandbox.allow_write {
        tools.exclude("write")?;
    }
    if !(sandbox.allow_process && sandbox.allow_shell) {
        tools.exclude("bash")?;
    }

    let mut compaction = CompactionPolicy::default();
    if let Some(layer) = values.compaction {
        compaction.mode = match (layer.mode, layer.enabled) {
            (Some(value), _) => CompactionMode::parse(&value)?,
            (None, Some(true)) => CompactionMode::Local,
            (None, Some(false)) => CompactionMode::Disabled,
            (None, None) => compaction.mode,
        };
        if let Some(value) = layer.threshold_fraction {
            if !value.is_finite() || value <= 0.0 || value > 1.0 {
                anyhow::bail!("compaction.threshold_fraction must be greater than 0 and at most 1");
            }
            compaction.threshold_fraction = value;
        }
        if let Some(value) = layer.max_active_tokens {
            compaction.max_active_tokens = Some(value);
        }
        if let Some(value) = layer.keep_recent_tokens {
            compaction.keep_recent_tokens = value.max(1);
        } else if let Some(value) = layer.keep_recent_turns {
            const LEGACY_TOKENS_PER_TURN: u64 = 1_000;
            compaction.keep_recent_tokens = u64::try_from(value)
                .unwrap_or(u64::MAX)
                .saturating_mul(LEGACY_TOKENS_PER_TURN)
                .max(1);
        }
        if let Some(value) = layer.compact_model {
            let value = value.trim();
            if value.is_empty() {
                anyhow::bail!("compaction.compact_model must not be empty");
            }
            compaction.compact_model = Some(ModelId(value.to_owned()));
        }
    }

    let mode = match cli.mode.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("rpc") => Mode::Rpc,
        Some(value) if value.eq_ignore_ascii_case("interactive") => Mode::Interactive,
        Some(value) => {
            anyhow::bail!("invalid frontend mode {value:?}; use interactive or rpc (or --print)")
        }
        None if cli.print => {
            let prompt = cli.message.clone().unwrap_or_default();
            if prompt.is_empty() && cli.prompt_template.is_none() {
                anyhow::bail!("--print requires a prompt or --prompt <template>");
            }
            Mode::Print { prompt }
        }
        None => Mode::Interactive,
    };
    let resume = if cli.continue_ {
        ResumeSelector::Continue
    } else if let Some(id) = cli.resume {
        ResumeSelector::Resume(id.and_then(|id| {
            let id = id.trim().to_owned();
            (!id.is_empty()).then_some(id)
        }))
    } else if let Some(id) = cli.fork {
        ResumeSelector::Fork(id.and_then(|id| {
            let id = id.trim().to_owned();
            (!id.is_empty()).then_some(id)
        }))
    } else {
        ResumeSelector::New
    };

    let mut enabled_extensions = values.enabled_extensions.unwrap_or_default();
    enabled_extensions.extend(cli.enable_extensions);
    let enabled_extensions = normalize_extension_names(enabled_extensions)?;
    let trusted_extensions =
        normalize_extension_trust_grants(values.trusted_extensions.unwrap_or_default())?;
    let invocation_trusted_extensions = normalize_extension_names(cli.trust_extensions)?;

    Ok(Config {
        workspace,
        invocation_cwd: invocation_cwd.clone(),
        model,
        model_explicit,
        reasoning,
        reasoning_explicit,
        reasoning_mode,
        reasoning_mode_explicit,
        cache_retention,
        effect_policy,
        sandbox,
        theme: cli.theme.or(values.theme),
        system_prompt,
        theme_paths: cli.theme_dirs,
        color,
        mouse,
        plain: cli.plain || values.plain.unwrap_or(false),
        session_dir: cli
            .session_dir
            .or(values.session_dir)
            .unwrap_or_else(config::default_session_dir),
        compaction,
        max_cost_microdollars: values.max_cost_microdollars,
        cost_warning_microdollars: values.cost_warning_microdollars,
        max_turns: {
            let raw = cli.max_turns.or(values.max_turns).unwrap_or(0);
            if raw == 0 {
                None
            } else {
                Some(raw.max(1))
            }
        },
        show_reasoning_in_print: cli.show_reasoning,
        initial_prompt: matches!(mode, Mode::Interactive)
            .then_some(cli.message)
            .flatten(),
        prompt_template: cli.prompt_template,
        debug_prompt: cli.debug_prompt,
        prompt_paths: cli.prompt_templates,
        mode,
        resume,
        skill_paths: cli.skill_dirs,
        extension_paths: cli.extension_dirs,
        enabled_extensions,
        extension_activation_overridden,
        trusted_extensions,
        invocation_trusted_extensions,
        tools,
        telemetry: cli.telemetry.or(values.telemetry).map(|path| {
            if path.is_absolute() {
                path
            } else {
                invocation_cwd.join(path)
            }
        }),
        context_files: !cli.no_context_files && values.context_files.unwrap_or(true),
        offline,
        workspace_trusted: cli.workspace_trusted,
    })
}

/// Convert parsed CLI arguments into layered process configuration.
pub fn build_config(cli: Cli, cwd: &Path) -> anyhow::Result<Config> {
    let global = global_config_path();
    build_config_with_global_path(cli, cwd, global.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn base() -> Cli {
        Cli {
            command: None,
            message: None,
            login: None,
            logout: None,
            headless: false,
            mode: None,
            print: false,
            continue_: false,
            resume: None,
            fork: None,
            model: None,
            reasoning: None,
            reasoning_mode: None,
            cache_retention: None,
            workspace: None,
            theme: None,
            theme_dirs: vec![],
            color: None,
            mouse: None,
            plain: false,
            show_reasoning: false,
            max_turns: None,
            session_dir: None,
            prompt_template: None,
            debug_prompt: false,
            telemetry: None,
            prompt_templates: vec![],
            skill_dirs: vec![],
            extension_dirs: vec![],
            enable_extensions: vec![],
            trust_extensions: vec![],
            workspace_trusted: false,
            safe_mode: false,
            tools: None,
            exclude_tools: vec![],
            no_tools: false,
            no_edit: false,
            no_write: false,
            no_process: false,
            no_shell: false,
            allow_shell: false,
            allow_remote_read: false,
            shell_path: None,
            no_context_files: false,
            offline: false,
            strict_config: false,
            bash_timeout_secs: None,
            max_output_bytes: None,
            system_prompt: None,
        }
    }

    fn config_with_empty_global(cli: Cli, directory: &Path) -> anyhow::Result<Config> {
        build_config_with_global_path(cli, directory, Some(&directory.join("missing-global.toml")))
    }

    #[test]
    fn cache_retention_can_disable_prompt_caching() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.cache_retention = Some("none".into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(config.cache_retention, ygg_ai::CacheRetention::None);
    }

    #[test]
    fn colour_policy_resolves_from_cli() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.color = Some("never".into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(config.color, ColorMode::Never);
    }

    #[test]
    fn mouse_policy_defaults_to_terminal_ownership() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(config.mouse, config::MouseMode::Auto);
        assert!(!config.mouse.application_owned());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.mouse = Some("app".into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(config.mouse, config::MouseMode::App);
        assert!(config.mouse.application_owned());
    }

    #[test]
    fn shell_path_and_bash_timeout_resolve_from_cli() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.shell_path = Some(PathBuf::from("/opt/homebrew/bin/bash"));
        cli.bash_timeout_secs = Some(45);
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(
            config.sandbox.shell_path,
            Some(PathBuf::from("/opt/homebrew/bin/bash"))
        );
        assert_eq!(config.sandbox.bash_timeout_secs, 45);
    }

    #[test]
    fn telemetry_path_resolves_relative_to_invocation_directory() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.telemetry = Some(PathBuf::from("metrics/run.jsonl"));
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(
            config.telemetry,
            Some(
                directory
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("metrics/run.jsonl")
            )
        );
    }

    #[test]
    fn remote_reads_are_default_off_and_require_user_level_opt_in() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(!config.sandbox.allow_remote_read);

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.allow_remote_read = true;
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(config.sandbox.allow_remote_read);

        let global = directory.path().join("global.toml");
        std::fs::write(&global, "allow_remote_read = true\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert!(config.sandbox.allow_remote_read);
    }

    #[test]
    fn project_config_cannot_grant_remote_network_authority_and_offline_revokes_it() {
        let directory = cwd();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "allow_remote_read = true\n",
        )
        .unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(!config.sandbox.allow_remote_read);

        let global = directory.path().join("global.toml");
        std::fs::write(&global, "allow_remote_read = true\noffline = true\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert!(config.offline);
        assert!(!config.sandbox.allow_remote_read);
    }

    #[test]
    fn effect_policy_is_full_access_by_default_and_yolo_is_removed() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(config.effect_policy, ygg_agent::EffectPolicy::UnsafeHost);
        assert!(config.sandbox.allow_external_paths);
        assert!(Cli::try_parse_from(["ygg", "--yolo"]).is_err());
    }

    #[test]
    fn safe_mode_uses_the_controlled_approval_profile() {
        let directory = cwd();

        let mut cli = Cli::try_parse_from(["ygg", "--safe-mode"]).unwrap();
        assert!(cli.safe_mode);
        cli.workspace = Some(directory.path().into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(
            config.effect_policy,
            ygg_agent::EffectPolicy::ControlledBashApproval
        );

        let cli = Cli::try_parse_from(["ygg", "--safe"]).unwrap();
        assert!(cli.safe_mode);
    }

    #[test]
    fn safe_mode_forces_workspace_only_paths() {
        let directory = cwd();
        assert!(SandboxPolicy::default().allow_external_paths);

        let global = directory.path().join("global.toml");
        std::fs::write(&global, "allow_external_paths = true\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.effect_policy, ygg_agent::EffectPolicy::UnsafeHost);
        assert!(config.sandbox.allow_external_paths);

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.safe_mode = true;
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(
            config.effect_policy,
            ygg_agent::EffectPolicy::ControlledBashApproval
        );
        assert!(!config.sandbox.allow_external_paths);
    }

    #[test]
    fn legacy_host_authority_config_does_not_select_a_policy() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "unsafe_host_effects = false\n").unwrap();

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.effect_policy, ygg_agent::EffectPolicy::UnsafeHost);
    }

    #[test]
    fn print_mode_requires_prompt_text() {
        let directory = cwd();
        let mut cli = base();
        cli.print = true;
        cli.model = Some("m".into());
        cli.workspace = Some(directory.path().into());
        assert!(config_with_empty_global(cli, directory.path()).is_err());
    }

    #[test]
    fn print_mode_builds_print_config() {
        let directory = cwd();
        let mut cli = base();
        cli.message = Some("hi".into());
        cli.print = true;
        cli.model = Some("m".into());
        cli.workspace = Some(directory.path().into());
        cli.show_reasoning = true;
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(matches!(config.mode, Mode::Print { prompt } if prompt == "hi"));
        assert!(config.show_reasoning_in_print);
    }

    #[test]
    fn continue_sets_resume_selector_and_interactive_mode() {
        let directory = cwd();
        let mut cli = base();
        cli.continue_ = true;
        cli.workspace = Some(directory.path().into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(matches!(config.resume, ResumeSelector::Continue));
        assert!(matches!(config.mode, Mode::Interactive));
    }

    #[test]
    fn clap_parses_fork_and_rejects_resume_conflicts() {
        let parsed = Cli::try_parse_from(["ygg", "--fork", "source-id"]).unwrap();
        assert_eq!(parsed.fork, Some(Some("source-id".into())));
        assert!(Cli::try_parse_from(["ygg", "--fork", "--resume"]).is_err());
        assert!(Cli::try_parse_from(["ygg", "--fork", "--continue"]).is_err());
    }

    #[test]
    fn fork_without_an_id_is_distinct_from_fork_by_id() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.fork = Some(None);
        assert!(matches!(
            config_with_empty_global(cli, directory.path())
                .unwrap()
                .resume,
            ResumeSelector::Fork(None)
        ));

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.fork = Some(Some("session-id".into()));
        assert!(matches!(
            config_with_empty_global(cli, directory.path())
                .unwrap()
                .resume,
            ResumeSelector::Fork(Some(id)) if id == "session-id"
        ));
    }

    #[test]
    fn reasoning_is_parsed_and_invalid_values_fail() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning = Some("off".into());
        assert!(config_with_empty_global(cli, directory.path()).is_ok());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning = Some("budget=2048".into());
        assert!(config_with_empty_global(cli, directory.path()).is_ok());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning_mode = Some("pro".into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(config.reasoning_mode, ygg_ai::ReasoningMode::Pro);
        assert!(config.reasoning_mode_explicit);

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning = Some("nonsense".into());
        assert!(config_with_empty_global(cli, directory.path()).is_err());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning_mode = Some("turbo".into());
        assert!(config_with_empty_global(cli, directory.path()).is_err());
    }

    #[test]
    fn resume_without_an_id_is_distinct_from_resume_by_id() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.resume = Some(None);
        assert!(matches!(
            config_with_empty_global(cli, directory.path())
                .unwrap()
                .resume,
            ResumeSelector::Resume(None)
        ));
    }

    #[test]
    fn unknown_config_keys_report_source_location_and_suggestion() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(
            &global,
            "model = 'known'\nmodle = 'ignored'\n[compaction]\nkeep_recent_turn = 2\n",
        )
        .unwrap();

        let loaded = read_layer(&global, ConfigSourceKind::Global).unwrap();

        assert_eq!(loaded.values.model.as_deref(), Some("known"));
        assert_eq!(loaded.diagnostics.len(), 2);
        assert_eq!(loaded.diagnostics[0].key, "compaction.keep_recent_turn");
        assert_eq!(loaded.diagnostics[0].line, 4);
        assert_eq!(loaded.diagnostics[0].column, 1);
        assert_eq!(
            loaded.diagnostics[0].suggestion,
            Some("compaction.keep_recent_turns")
        );
        assert_eq!(loaded.diagnostics[1].key, "modle");
        assert_eq!(loaded.diagnostics[1].line, 2);
        assert_eq!(loaded.diagnostics[1].column, 1);
        assert_eq!(loaded.diagnostics[1].suggestion, Some("model"));
        assert_eq!(loaded.diagnostics[1].source_kind, ConfigSourceKind::Global);
        assert_eq!(loaded.diagnostics[1].path, global);
    }

    #[test]
    fn unknown_config_keys_warn_by_default_and_fail_in_cli_strict_mode() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "modle = 'ignored'\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        assert!(build_config_with_global_path(cli, directory.path(), Some(&global)).is_ok());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.strict_config = true;
        let error = build_config_with_global_path(cli, directory.path(), Some(&global))
            .unwrap_err()
            .to_string();
        assert!(error.contains("strict configuration rejected unknown keys"));
        assert!(error.contains("global config"));
        assert!(error.contains(&format!("{}:1:1", global.display())));
        assert!(error.contains("unknown configuration key \"modle\""));
        assert!(error.contains("did you mean \"model\"?"));
    }

    #[test]
    fn project_config_can_opt_into_strict_diagnostics() {
        let directory = cwd();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        let project = directory.path().join(".ygg/config.toml");
        std::fs::write(&project, "strict_config = true\nthemee = 'ignored'\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;

        let error = config_with_empty_global(cli, directory.path())
            .unwrap_err()
            .to_string();

        assert!(error.contains("project config"));
        assert!(error.contains(&format!("{}:2:1", project.display())));
        assert!(error.contains("themee"));
    }

    #[test]
    fn strict_config_flag_is_parsed() {
        let cli = Cli::try_parse_from(["ygg", "--strict-config"]).unwrap();
        assert!(cli.strict_config);
    }

    #[test]
    fn accepted_config_aliases_do_not_emit_unknown_key_diagnostics() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(
            &global,
            "exec_timeout_secs = 30\n[compaction]\npolicy = 'local'\n",
        )
        .unwrap();

        let loaded = read_layer(&global, ConfigSourceKind::Global).unwrap();

        assert!(loaded.diagnostics.is_empty());
        assert_eq!(loaded.values.bash_timeout_secs, Some(30));
        assert_eq!(
            loaded.values.compaction.unwrap().mode.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn cli_overrides_project_which_overrides_global() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(
            &global,
            "model = 'global'\ntheme = 'global-theme'\nmax_turns = 7\n",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "model = 'project'\ntheme = 'project-theme'\nmax_turns = 9\nallow_external_paths = false\n",
        )
        .unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        cli.model = Some("cli".into());
        cli.max_turns = Some(11);
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.model.as_ref().unwrap().0, "cli");
        assert!(config.model_explicit);
        assert!(!config.reasoning_explicit);
        assert_eq!(config.theme.as_deref(), Some("project-theme"));
        assert_eq!(config.max_turns, Some(11));
        assert!(!config.sandbox.allow_external_paths);
    }

    #[test]
    fn telemetry_uses_cli_then_project_then_global_precedence() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "telemetry = 'global.jsonl'\n").unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "telemetry = 'project.jsonl'\n",
        )
        .unwrap();

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        cli.telemetry = Some("cli.jsonl".into());
        let canonical = directory.path().canonicalize().unwrap();
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.telemetry, Some(canonical.join("cli.jsonl")));

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.telemetry, Some(canonical.join("project.jsonl")));

        std::fs::remove_file(directory.path().join(".ygg/config.toml")).unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.telemetry, Some(canonical.join("global.jsonl")));
    }

    #[test]
    fn system_prompt_layered_precedence_prefers_cli_over_project_then_global() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "system_prompt = 'global'\n").unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "system_prompt = 'project'\n",
        )
        .unwrap();

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        cli.system_prompt = Some("cli".into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.system_prompt.as_deref(), Some("cli"));

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.system_prompt.as_deref(), Some("project"));
    }

    #[test]
    fn system_prompt_explicit_empty_cli_value_is_preserved() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "system_prompt = 'global'\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.system_prompt = Some("".into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.system_prompt.as_deref(), Some(""));
    }

    #[test]
    fn parse_system_prompt_flag_without_value() {
        let cli = Cli::try_parse_from(["ygg", "--system-prompt", "--print", "--prompt", "review"])
            .unwrap();
        assert!(cli.system_prompt.is_some());
        assert_eq!(cli.system_prompt.as_deref(), Some(""));
    }

    #[test]
    fn trusted_project_may_tighten_but_never_relax_global_authority() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "allow_write = false\nallow_edit = true\n").unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "allow_write = true\nallow_edit = false\n",
        )
        .unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert!(!config.sandbox.allow_write);
        assert!(!config.sandbox.allow_edit);
    }

    #[test]
    fn trusted_project_may_enable_but_cannot_trust_an_executable_extension() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(
            &global,
            "enabled_extensions = ['user-tool']\ntrusted_extensions = ['user-tool']\n",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "enabled_extensions = ['project-tool']\ntrusted_extensions = ['project-tool']\n",
        )
        .unwrap();

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();

        assert_eq!(config.enabled_extensions, vec!["project-tool"]);
        assert!(config.extension_activation_overridden);
        assert_eq!(config.trusted_extensions, vec!["user-tool"]);
        assert!(config.invocation_trusted_extensions.is_empty());
    }

    #[test]
    fn activation_menu_revalidates_a_project_override_added_after_startup() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "enabled_extensions = ['global-tool']\n").unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.workspace_trusted = true;
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert!(!config.extension_activation_overridden);
        assert!(extension_activation_menu_authoritative(&config).unwrap());

        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "enabled_extensions = ['project-tool']\n",
        )
        .unwrap();
        assert!(!extension_activation_menu_authoritative(&config).unwrap());

        std::fs::write(directory.path().join(".ygg/config.toml"), "not valid = [\n").unwrap();
        assert!(extension_activation_menu_authoritative(&config).is_err());
    }

    #[test]
    fn unavailable_home_never_loads_project_config_as_global_config() {
        let directory = cwd();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "enabled_extensions = ['project-tool']\ntrusted_extensions = ['project-tool']\n",
        )
        .unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());

        let config = build_config_with_global_path(cli, directory.path(), None).unwrap();

        assert!(config.enabled_extensions.is_empty());
        assert!(!config.extension_activation_overridden);
        assert!(config.trusted_extensions.is_empty());
        assert!(config.invocation_trusted_extensions.is_empty());
    }

    #[test]
    fn relative_home_is_not_a_global_config_root() {
        assert_eq!(global_config_path_from_home(None), None);
        assert_eq!(global_config_path_from_home(Some(".".into())), None);
        let absolute_home = std::env::temp_dir().join("ygg-home");
        assert_eq!(
            global_config_path_from_home(Some(absolute_home.clone())),
            Some(absolute_home.join(".ygg/config.toml"))
        );
    }

    #[test]
    fn cli_activation_marks_the_interactive_user_config_menu_non_authoritative() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "enabled_extensions = ['global-tool']\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.enable_extensions.push("cli-tool".into());

        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();

        assert_eq!(config.enabled_extensions, ["cli-tool", "global-tool"]);
        assert!(config.extension_activation_overridden);
    }

    #[test]
    fn extension_name_lists_are_normalized_and_deduplicated() {
        let names =
            normalize_extension_names(split_names("Git-Tools, local-model,git-tools".to_owned()))
                .unwrap();
        assert_eq!(names, vec!["git-tools", "local-model"]);
    }

    #[test]
    fn persistent_extension_trust_grants_preserve_exact_source_paths() {
        let grants = normalize_extension_trust_grants([
            "Git-Tools".to_owned(),
            "git-tools@/workspace/.ygg/extensions/git-tools/extension.toml".to_owned(),
            "git-tools@/Volumes/dev@home/git-tools/extension.toml".to_owned(),
            " Git-Tools ".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            grants,
            vec![
                "git-tools",
                "git-tools@/Volumes/dev@home/git-tools/extension.toml",
                "git-tools@/workspace/.ygg/extensions/git-tools/extension.toml",
            ]
        );
    }

    #[test]
    fn persistent_identity_trust_normalizes_and_validates_sha256() {
        let digest = "A".repeat(64);
        let grants = normalize_extension_trust_grants([format!(
            "Pi-Aggregate@/Volumes/dev@home/pi/extension.toml@sha256:{digest}"
        )])
        .unwrap();
        assert_eq!(
            grants,
            [format!(
                "pi-aggregate@/Volumes/dev@home/pi/extension.toml@sha256:{}",
                "a".repeat(64)
            )]
        );

        let error = normalize_extension_trust_grants([
            "pi-aggregate@/tmp/pi/extension.toml@sha256:abcd".to_owned(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("exactly 64"));

        let error =
            normalize_extension_trust_grants([format!("pi-aggregate@sha256:{}", "a".repeat(64))])
                .unwrap_err();
        assert!(error.to_string().contains("manifest path"));
    }

    #[test]
    fn persistent_source_trust_rejects_relative_paths() {
        let error = normalize_extension_trust_grants([
            "git-tools@.ygg/extensions/git-tools/extension.toml".to_owned(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("absolute path"));

        let error =
            normalize_extension_trust_grants(["git-tools@/tmp/git-tools/other.toml".to_owned()])
                .unwrap_err();
        assert!(error.to_string().contains("extension.toml"));
    }

    #[test]
    fn cli_extension_trust_is_kept_one_shot() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "trusted_extensions = ['global-tool']\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.trust_extensions = vec!["Project-Tool".into()];

        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();

        assert_eq!(config.trusted_extensions, vec!["global-tool"]);
        assert_eq!(config.invocation_trusted_extensions, vec!["project-tool"]);
    }

    #[test]
    fn no_edit_and_explicit_allowlists_match_the_provider_tool_surface() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.no_edit = true;
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(!config.sandbox.allow_edit);
        assert!(!config.sandbox.allow_write);
        assert!(!config.tools.enabled("edit"));
        assert!(!config.tools.enabled("write"));

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.tools = Some(vec!["read".into(), "search".into()]);
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert_eq!(
            config.tools.names().collect::<Vec<_>>(),
            vec!["read", "search"]
        );
    }

    #[test]
    fn cost_and_compaction_settings_merge_from_layered_toml() {
        let global: ConfigLayer = toml::from_str(
            "max_cost_microdollars = 100\ncost_warning_microdollars = 25\n[compaction]\nenabled = false\nmax_active_tokens = 272000\ncompact_model = 'cheap'",
        )
        .unwrap();
        let project: ConfigLayer = toml::from_str(
            "cost_warning_microdollars = 40\n[compaction]\nmax_active_tokens = 200000\nkeep_recent_tokens = 2",
        )
        .unwrap();
        let mut merged = global;
        merged.merge(project);
        assert_eq!(merged.max_cost_microdollars, Some(100));
        assert_eq!(merged.cost_warning_microdollars, Some(40));
        let compaction = merged.compaction.unwrap();
        assert_eq!(compaction.enabled, Some(false));
        assert_eq!(compaction.compact_model.as_deref(), Some("cheap"));
        assert_eq!(compaction.max_active_tokens, Some(200_000));
        assert_eq!(compaction.keep_recent_tokens, Some(2));
    }

    #[test]
    fn explicit_compaction_mode_and_legacy_enabled_map_without_silent_fallback() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(
            &global,
            "[compaction]\nmode = 'native-responses'\nmax_active_tokens = 0\n",
        )
        .unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.compaction.mode, CompactionMode::NativeResponses);
        assert_eq!(config.compaction.max_active_tokens, Some(0));

        std::fs::write(&global, "[compaction]\nenabled = true\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.compaction.mode, CompactionMode::Local);
        assert_eq!(config.compaction.max_active_tokens, Some(272_000));

        std::fs::write(&global, "[compaction]\nenabled = false\n").unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();
        assert_eq!(config.compaction.mode, CompactionMode::Disabled);
    }

    // --- extension activation persistence ---

    #[test]
    fn persist_extension_activation_changes_only_the_selected_user_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# keep this comment\nenabled_extensions = [\"ygg-browse\", \"ygg-ssh\"]\ntrusted_extensions = [\"ygg-browse\", \"ygg-ssh\"]\n",
        )
        .unwrap();

        assert_eq!(
            persist_extension_enabled_to_path("ygg-web-search", true, &path).unwrap(),
            vec!["ygg-browse", "ygg-ssh", "ygg-web-search"]
        );
        assert_eq!(
            persist_extension_enabled_to_path("ygg-browse", false, &path).unwrap(),
            vec!["ygg-ssh", "ygg-web-search"]
        );

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        let enabled = parsed["enabled_extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(enabled, ["ygg-ssh", "ygg-web-search"]);
        assert_eq!(
            parsed["trusted_extensions"].as_array().unwrap().len(),
            2,
            "trust is an independent decision"
        );
        assert!(content.contains("# keep this comment"), "{content}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_config_update_preserves_existing_permissions_and_uses_private_new_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.toml");
        std::fs::write(&existing, "enabled_extensions = []\n").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o640)).unwrap();
        persist_extension_enabled_to_path("ygg-ssh", true, &existing).unwrap();
        assert_eq!(
            std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let new = dir.path().join("new.toml");
        persist_extension_enabled_to_path("ygg-ssh", true, &new).unwrap();
        assert_eq!(
            std::fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let staging_files = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(staging_files, 0, "atomic staging files must be removed");
    }

    #[test]
    fn atomic_config_publish_rejects_a_non_locking_external_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "enabled_extensions = [\"ygg-browse\"]\n";
        let external = "enabled_extensions = [\"ygg-ssh\"]\ntrusted_extensions = [\"ygg-ssh\"]\n";
        std::fs::write(&path, original).unwrap();
        let expected = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, external).unwrap();

        let error = write_config_atomically(&path, "enabled_extensions = []\n", Some(&expected))
            .unwrap_err();

        assert!(error.to_string().contains("changed"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            !name.contains(".tmp-") && !name.contains(".ygg-tmp-")
        }));
    }

    #[test]
    fn concurrent_config_update_fails_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "enabled_extensions = [\"ygg-browse\"]\n";
        std::fs::write(&path, original).unwrap();
        let lock = config_update_lock(&path).unwrap();

        let error = persist_extension_enabled_to_path("ygg-ssh", true, &path).unwrap_err();
        assert!(
            error.to_string().contains("another config update"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(lock);

        assert_eq!(
            persist_extension_enabled_to_path("ygg-ssh", true, &path).unwrap(),
            ["ygg-browse", "ygg-ssh"]
        );
    }

    #[test]
    fn persist_extension_activation_rejects_a_non_array_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let invalid = "enabled_extensions = \"ygg-browse\"\n";
        std::fs::write(&path, invalid).unwrap();

        assert!(persist_extension_enabled_to_path("ygg-ssh", true, &path).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), invalid);
    }

    // --- persist_model_to_path ---

    fn read_model_from_config(path: &std::path::Path) -> Option<String> {
        let source = std::fs::read_to_string(path).unwrap();
        for line in source.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some(after) = trimmed.strip_prefix("model") {
                let after = after.trim_start();
                if let Some(val) = after.strip_prefix('=') {
                    return Some(val.trim().trim_matches('"').to_string());
                }
            }
        }
        None
    }

    #[test]
    fn persist_model_creates_file_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_model_to_path("gpt-4o-mini", &path).unwrap();
        assert_eq!(
            read_model_from_config(&path).as_deref(),
            Some("gpt-4o-mini")
        );
    }

    #[test]
    fn persist_model_updates_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"old-model\"\ntheme = \"dusk\"\n").unwrap();
        persist_model_to_path("new-model", &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("model = \"new-model\""), "{content}");
        assert!(
            content.contains("theme = \"dusk\""),
            "theme line preserved: {content}"
        );
    }

    #[test]
    fn persist_model_appends_when_no_model_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"dusk\"\n").unwrap();
        persist_model_to_path("gpt-4o-mini", &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("model = \"gpt-4o-mini\""), "{content}");
    }

    #[test]
    fn persist_model_skips_commented_model_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# model = \"commented-out\"\ntheme = \"dusk\"\n").unwrap();
        persist_model_to_path("active-model", &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // The TOML-based parser does not preserve comments since they are
        // not part of the parsed representation. The commented line is
        // intentionally dropped in exchange for structurally correct updates
        // that never corrupt multi-line values or cause partial-key collisions.
        assert!(
            content.contains("model = \"active-model\""),
            "new entry set: {content}"
        );
        assert!(
            content.contains("theme = \"dusk\""),
            "existing key preserved: {content}"
        );
    }

    #[test]
    fn persist_model_preserves_multiline_values_and_partial_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "model_alias = \"keep\"\nnotes = [\n  \"first\",\n  \"second\",\n]\n[compaction]\nkeep_recent_tokens = 4\n",
        )
        .unwrap();

        persist_model_to_path("active-model", &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(parsed["model"].as_str(), Some("active-model"));
        assert_eq!(parsed["model_alias"].as_str(), Some("keep"));
        assert_eq!(parsed["notes"].as_array().unwrap().len(), 2);
        assert_eq!(
            parsed["compaction"]["keep_recent_tokens"].as_integer(),
            Some(4)
        );
    }

    #[test]
    fn persist_model_rejects_invalid_toml_without_rewriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let invalid = "model = [\n";
        std::fs::write(&path, invalid).unwrap();

        assert!(persist_model_to_path("active-model", &path).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), invalid);
    }

    #[test]
    fn persist_model_escapes_special_characters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Backslash and double-quote must be escaped in TOML basic strings.
        persist_model_to_path("model\\with\"quotes", &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("model = "), "{content}");
        // Round-trip: the written TOML must parse back to the original id.
        let parsed: std::collections::BTreeMap<String, toml::Value> =
            toml::from_str(&content).unwrap();
        assert_eq!(
            parsed.get("model").unwrap().as_str().unwrap(),
            "model\\with\"quotes"
        );
    }

    #[test]
    fn doctor_command_parses_without_a_prompt() {
        let cli = Cli::try_parse_from(["ygg", "--offline", "doctor"]).unwrap();
        assert!(cli.message.is_none());
        assert!(matches!(cli.command, Some(TopLevelCommand::Doctor)));
        assert!(cli.offline);
    }

    #[test]
    fn sessions_subcommands_do_not_consume_the_positional_prompt() {
        let cli = Cli::try_parse_from(["ygg", "sessions", "inspect", "abc-123"]).unwrap();
        assert!(cli.message.is_none());
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Sessions {
                command: SessionCommand::Inspect { ref id }
            }) if id == "abc-123"
        ));
    }

    #[test]
    fn extension_package_commands_parse_without_a_prompt() {
        let cli = Cli::try_parse_from(["ygg", "extension", "install", "ygg-serve"]).unwrap();
        assert!(cli.message.is_none());
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Extension {
                command: ExtensionCommand::Install {
                    name: Some(ref name),
                    path: None,
                }
            }) if name == "ygg-serve"
        ));

        let cli = Cli::try_parse_from(["ygg", "extension", "install", "--path", "./serve.tar.gz"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Extension {
                command: ExtensionCommand::Install {
                    name: None,
                    path: Some(_),
                }
            })
        ));

        let cli = Cli::try_parse_from(["ygg", "extension", "update", "ygg-web-search"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Extension {
                command: ExtensionCommand::Update {
                    name: Some(ref name),
                    path: None,
                }
            }) if name == "ygg-web-search"
        ));
        let cli = Cli::try_parse_from(["ygg", "extension", "update", "--path", "./bundle.tar.gz"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Extension {
                command: ExtensionCommand::Update {
                    name: None,
                    path: Some(_),
                }
            })
        ));
    }

    #[test]
    fn pi_migration_dry_run_parses_without_a_prompt() {
        let cli = Cli::try_parse_from([
            "ygg",
            "migrate",
            "pi",
            "--dry-run",
            "--json",
            "--project",
            "./workspace",
        ])
        .unwrap();
        assert!(cli.message.is_none());
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Migrate {
                command: MigrationCommand::Pi {
                    dry_run: true,
                    json: true,
                    project: Some(_),
                    ..
                }
            })
        ));
    }

    #[test]
    fn pi_migration_plan_and_apply_forms_are_explicit_and_exclusive() {
        let plan = Cli::try_parse_from([
            "ygg",
            "migrate",
            "pi",
            "--dry-run",
            "--plan-out",
            "./pi-plan.json",
            "--pi-package",
            "./pi-package",
            "--extension-root",
            "./extensions",
            "--name",
            "pi-reviewed",
        ])
        .unwrap();
        assert!(matches!(
            plan.command,
            Some(TopLevelCommand::Migrate {
                command: MigrationCommand::Pi {
                    dry_run: true,
                    plan_out: Some(_),
                    apply: None,
                    yes: false,
                    name: Some(ref name),
                    ..
                }
            }) if name == "pi-reviewed"
        ));

        let apply =
            Cli::try_parse_from(["ygg", "migrate", "pi", "--apply", "./pi-plan.json", "--yes"])
                .unwrap();
        assert!(matches!(
            apply.command,
            Some(TopLevelCommand::Migrate {
                command: MigrationCommand::Pi {
                    apply: Some(_),
                    yes: true,
                    ..
                }
            })
        ));

        assert!(Cli::try_parse_from([
            "ygg",
            "migrate",
            "pi",
            "--dry-run",
            "--apply",
            "./pi-plan.json",
        ])
        .is_err());
        assert!(
            Cli::try_parse_from(["ygg", "migrate", "pi", "--pi-package", "./pi-package",]).is_err()
        );
    }

    #[test]
    fn pi_install_parses_without_a_prompt() {
        let cli = Cli::try_parse_from([
            "ygg",
            "pi",
            "install",
            "./private-extension.ts",
            "--pi-home",
            "./pi/agent",
            "--pi-package",
            "./node_modules/@earendil-works/pi-coding-agent",
        ])
        .unwrap();
        assert!(cli.message.is_none());
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Pi {
                command: PiCommand::Install {
                    pi_home: Some(_),
                    pi_package: Some(_),
                    ..
                }
            })
        ));
    }

    #[test]
    fn pi_install_preserves_explicit_aggregate_source_order() {
        let cli = Cli::try_parse_from([
            "ygg",
            "pi",
            "install",
            "./first.ts",
            "--with",
            "./second.ts",
            "--with",
            "./third-package",
        ])
        .unwrap();
        let Some(TopLevelCommand::Pi {
            command:
                PiCommand::Install {
                    source,
                    additional_sources,
                    ..
                },
        }) = cli.command
        else {
            panic!("expected pi install command");
        };
        assert_eq!(source, PathBuf::from("./first.ts"));
        assert_eq!(
            additional_sources,
            [
                PathBuf::from("./second.ts"),
                PathBuf::from("./third-package"),
            ]
        );
    }

    #[test]
    fn serve_command_parses_forwarded_loopback_options() {
        let cli = Cli::try_parse_from([
            "ygg",
            "serve",
            "--no-open",
            "--port",
            "0",
            "--web-root",
            "./web",
        ])
        .unwrap();
        assert!(cli.message.is_none());
        assert!(matches!(
            cli.command,
            Some(TopLevelCommand::Serve {
                no_open: true,
                port: 0,
                web_root: Some(_),
            })
        ));
    }

    #[test]
    fn debug_prompt_is_an_explicit_prompt_template_diagnostic() {
        let cli = Cli::try_parse_from(["ygg", "--print", "--prompt", "review", "--debug-prompt"])
            .unwrap();
        assert_eq!(cli.prompt_template.as_deref(), Some("review"));
        assert!(cli.debug_prompt);
    }
}
