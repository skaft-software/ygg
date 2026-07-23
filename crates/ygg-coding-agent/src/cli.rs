#![allow(missing_docs)]

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::Deserialize;
use ygg_ai::ModelId;

use crate::app::bootstrap::resolve_model_id;
use crate::config::{
    self, ColorMode, CompactionPolicy, Config, Mode, ResumeSelector, SandboxPolicy, ToolPolicy,
};
use crate::session_commands::SessionCommand;

#[derive(Clone, Debug, Subcommand)]
pub enum TopLevelCommand {
    /// Inspect and manage durable local sessions.
    Sessions {
        #[command(subcommand)]
        command: SessionCommand,
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
    /// Model id override.
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning: off, minimal, low, medium, high, xhigh, max, or budget=N.
    #[arg(long)]
    pub reasoning: Option<String>,
    /// Prompt-cache retention: none, short, or long.
    #[arg(long, value_name = "POLICY")]
    pub cache_retention: Option<String>,
    /// Workspace root override.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// TUI theme name.
    #[arg(long)]
    pub theme: Option<String>,
    /// Additional directory paths to scan for themes (repeatable).
    #[arg(long = "theme-dir", value_name = "DIR")]
    pub theme_dirs: Vec<PathBuf>,
    /// Colour output policy: auto, always, or never.
    #[arg(long, value_name = "WHEN")]
    pub color: Option<String>,
    /// Use chronological ASCII output without cursor control.
    #[arg(long)]
    pub plain: bool,
    /// Mouse ownership: auto/terminal/off preserve native selection; app captures the mouse.
    #[arg(long, value_name = "MODE")]
    pub mouse: Option<String>,
    /// Emit reasoning deltas in print mode.
    #[arg(long)]
    pub show_reasoning: bool,
    /// Deprecated compatibility flag; accumulated session cost is always shown when available.
    #[arg(long, hide = true)]
    pub show_turn_cost: bool,
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
    /// Explicit prompt-template file or directory (repeatable, Pi compatible).
    #[arg(long = "prompt-template", value_name = "PATH")]
    pub prompt_templates: Vec<PathBuf>,
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
    /// Do not load global or workspace AGENTS.md files.
    #[arg(long)]
    pub no_context_files: bool,
    /// Disable optional provider/model discovery network requests at startup.
    #[arg(long)]
    pub offline: bool,
    /// Maximum execution time in seconds.
    #[arg(long)]
    pub exec_timeout_secs: Option<u64>,
    /// Maximum persisted tool output size in bytes.
    #[arg(long)]
    pub max_output_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CompactionLayer {
    enabled: Option<bool>,
    threshold_fraction: Option<f64>,
    keep_recent_turns: Option<usize>,
    compact_model: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ConfigLayer {
    model: Option<String>,
    reasoning: Option<String>,
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
    exec_timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    session_dir: Option<PathBuf>,
    max_turns: Option<u64>,
    max_cost_microdollars: Option<u64>,
    cost_warning_microdollars: Option<u64>,
    show_turn_cost: Option<bool>,
    context_files: Option<bool>,
    offline: Option<bool>,
    enabled_extensions: Option<Vec<String>>,
    trusted_extensions: Option<Vec<String>>,
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
        override_some!(exec_timeout_secs);
        override_some!(max_output_bytes);
        override_some!(session_dir);
        override_some!(max_turns);
        override_some!(max_cost_microdollars);
        override_some!(cost_warning_microdollars);
        override_some!(show_turn_cost);
        override_some!(context_files);
        override_some!(offline);
        override_some!(enabled_extensions);
        override_some!(trusted_extensions);
        match (self.compaction.as_mut(), newer.compaction) {
            (Some(current), Some(newer)) => {
                if newer.enabled.is_some() {
                    current.enabled = newer.enabled;
                }
                if newer.threshold_fraction.is_some() {
                    current.threshold_fraction = newer.threshold_fraction;
                }
                if newer.keep_recent_turns.is_some() {
                    current.keep_recent_turns = newer.keep_recent_turns;
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
        tighten_bool(&mut self.context_files, project.context_files.take());
        lower_u64(
            &mut self.exec_timeout_secs,
            project.exec_timeout_secs.take(),
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
        // Offline=true is a one-way safety setting for project configuration.
        self.offline =
            Some(self.offline.unwrap_or(false) || project.offline.take().unwrap_or(false));
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

fn persist_key_to_path(key: &str, value: &str, path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    // Serialise the value through toml so special characters are
    // properly escaped rather than producing invalid config.
    let escaped = toml::Value::String(value.to_string());
    let replacement = format!("{key} = {escaped}");

    let mut found = false;
    let mut new_lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        // Never rewrite a commented-out line.
        if trimmed.starts_with('#') {
            new_lines.push(line.to_string());
            continue;
        }
        if trimmed.starts_with(key) {
            let after = trimmed.strip_prefix(key).unwrap();
            if after.trim_start().starts_with('=') {
                new_lines.push(replacement.clone());
                found = true;
                continue;
            }
        }
        new_lines.push(line.to_string());
    }

    if !found {
        if !new_lines.is_empty() && !new_lines.last().unwrap().is_empty() {
            new_lines.push(String::new());
        }
        new_lines.push(replacement);
    }

    let new_content = new_lines.join("\n") + "\n";
    // Atomic write: write to a sibling temp file then rename over the
    // real path so a crash mid-write cannot leave a truncated config.
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, &new_content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
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
/// project or explicit source uses `name@path/to/extension.toml`.
fn normalize_extension_trust_grants(
    grants: impl IntoIterator<Item = String>,
) -> anyhow::Result<Vec<String>> {
    let mut normalized = std::collections::BTreeSet::new();
    for grant in grants {
        let grant = grant.trim();
        let normalized_grant = if let Some((name, path)) = grant.split_once('@') {
            let name = normalize_extension_name(name)?;
            let path = path.trim();
            if path.is_empty()
                || path.len() > 8 * 1024
                || path.chars().any(char::is_control)
                || !Path::new(path).is_absolute()
            {
                anyhow::bail!(
                    "invalid extension trust path {path:?}; persistent source-bound grants require an absolute path to extension.toml"
                );
            }
            format!("{name}@{path}")
        } else {
            normalize_extension_name(grant)?
        };
        normalized.insert(normalized_grant);
    }
    Ok(normalized.into_iter().collect())
}

fn read_layer(path: &Path) -> anyhow::Result<ConfigLayer> {
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
            return Ok(ConfigLayer::default())
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
                return Ok(ConfigLayer::default())
            }
            Err(error) => anyhow::bail!("cannot read config {}: {error}", path.display()),
        };
    toml::from_str(&source)
        .map_err(|error| anyhow::anyhow!("invalid config {}: {error}", path.display()))
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
    let compaction_enabled = env_parse("YGG_AUTO_COMPACT")?;
    let threshold_fraction = env_parse("YGG_COMPACTION_THRESHOLD_FRACTION")?;
    let keep_recent_turns = env_parse("YGG_COMPACTION_KEEP_RECENT_TURNS")?;
    let compact_model = env_value("YGG_COMPACT_MODEL");
    Ok(ConfigLayer {
        model: env_value("YGG_MODEL"),
        reasoning: env_value("YGG_REASONING"),
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
        exec_timeout_secs: env_parse("YGG_EXEC_TIMEOUT_SECS")?,
        max_output_bytes: env_parse("YGG_MAX_OUTPUT_BYTES")?,
        session_dir: env_value("YGG_SESSION_DIR").map(PathBuf::from),
        max_turns: env_parse("YGG_MAX_TURNS")?,
        max_cost_microdollars: env_parse("YGG_MAX_COST_MICRODOLLARS")?,
        cost_warning_microdollars: env_parse("YGG_COST_WARNING_MICRODOLLARS")?,
        show_turn_cost: env_parse("YGG_SHOW_TURN_COST")?,
        context_files: env_parse("YGG_CONTEXT_FILES")?,
        offline: env_parse("YGG_OFFLINE")?,
        enabled_extensions: env_value("YGG_EXTENSIONS").map(split_names),
        trusted_extensions: env_value("YGG_TRUSTED_EXTENSIONS").map(split_names),
        compaction: (compaction_enabled.is_some()
            || threshold_fraction.is_some()
            || keep_recent_turns.is_some()
            || compact_model.is_some())
        .then_some(CompactionLayer {
            enabled: compaction_enabled,
            threshold_fraction,
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

    // A missing home directory disables global config. Never reinterpret the
    // invocation directory as user scope: that would let an untrusted project
    // smuggle executable trust through `./.ygg/config.toml`.
    let global = match global_path {
        Some(path) => read_layer(path)?,
        None => ConfigLayer::default(),
    };
    let project = if cli.workspace_trusted {
        read_layer(&project_config_path(&workspace))?
    } else {
        ConfigLayer::default()
    };
    let environment = environment_layer()?;
    let mut values = global.clone();
    values.merge_project(project);
    values.merge(environment);

    let model = resolve_model_id(
        cli.model.clone().map(ygg_ai::ModelId),
        values.model.clone().map(ygg_ai::ModelId),
        None,
    );
    let reasoning = match cli.reasoning.as_deref().or(values.reasoning.as_deref()) {
        Some(value) => config::parse_reasoning(value)?,
        None => ygg_ai::ReasoningConfig::Off,
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
    if let Some(value) = values.exec_timeout_secs {
        sandbox.exec_timeout_secs = value;
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
    if let Some(value) = cli.exec_timeout_secs {
        sandbox.exec_timeout_secs = value;
    }
    if let Some(value) = cli.max_output_bytes {
        sandbox.max_output_bytes = value;
    }
    sandbox.exec_timeout_secs = sandbox.exec_timeout_secs.clamp(1, 3_600);
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
        tools.exclude("exec")?;
    }
    if !sandbox.allow_edit {
        tools.exclude("edit")?;
    }
    if !sandbox.allow_write {
        tools.exclude("write")?;
    }
    if !(sandbox.allow_process && sandbox.allow_shell) {
        tools.exclude("exec")?;
    }

    let mut compaction = CompactionPolicy::default();
    if let Some(layer) = values.compaction {
        if let Some(value) = layer.enabled {
            compaction.enabled = value;
        }
        if let Some(value) = layer.threshold_fraction {
            if !value.is_finite() || value <= 0.0 || value > 1.0 {
                anyhow::bail!("compaction.threshold_fraction must be greater than 0 and at most 1");
            }
            compaction.threshold_fraction = value;
        }
        if let Some(value) = layer.keep_recent_turns {
            compaction.keep_recent_turns = value.max(1);
        }
        if let Some(value) = layer.compact_model {
            let value = value.trim();
            if value.is_empty() {
                anyhow::bail!("compaction.compact_model must not be empty");
            }
            compaction.compact_model = Some(ModelId(value.to_owned()));
        }
    }

    let mode = if cli.print {
        let prompt = cli.message.clone().unwrap_or_default();
        if prompt.is_empty() && cli.prompt_template.is_none() {
            anyhow::bail!("--print requires a prompt or --prompt <template>");
        }
        Mode::Print { prompt }
    } else {
        Mode::Interactive
    };
    let resume = if cli.continue_ {
        ResumeSelector::Continue
    } else if let Some(id) = cli.resume {
        ResumeSelector::Resume(id.and_then(|id| {
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
        invocation_cwd,
        model,
        model_explicit,
        reasoning,
        reasoning_explicit,
        cache_retention,
        sandbox,
        theme: cli.theme.or(values.theme),
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
        show_turn_cost: cli.show_turn_cost || values.show_turn_cost.unwrap_or(false),
        max_turns: {
            let raw = cli.max_turns.or(values.max_turns).unwrap_or(0);
            if raw == 0 {
                None
            } else {
                Some(raw.max(1))
            }
        },
        show_reasoning_in_print: cli.show_reasoning,
        initial_prompt: (!cli.print).then_some(cli.message).flatten(),
        prompt_template: cli.prompt_template,
        debug_prompt: cli.debug_prompt,
        prompt_paths: cli.prompt_templates,
        mode,
        resume,
        skill_paths: cli.skill_dirs,
        extension_paths: cli.extension_dirs,
        enabled_extensions,
        trusted_extensions,
        invocation_trusted_extensions,
        tools,
        context_files: !cli.no_context_files && values.context_files.unwrap_or(true),
        offline: cli.offline || values.offline.unwrap_or(false),
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
            print: false,
            continue_: false,
            resume: None,
            model: None,
            reasoning: None,
            cache_retention: None,
            workspace: None,
            theme: None,
            theme_dirs: vec![],
            color: None,
            mouse: None,
            plain: false,
            show_reasoning: false,
            show_turn_cost: false,
            max_turns: None,
            session_dir: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_templates: vec![],
            skill_dirs: vec![],
            extension_dirs: vec![],
            enable_extensions: vec![],
            trust_extensions: vec![],
            workspace_trusted: false,
            tools: None,
            exclude_tools: vec![],
            no_tools: false,
            no_edit: false,
            no_write: false,
            no_process: false,
            no_shell: false,
            allow_shell: false,
            no_context_files: false,
            offline: false,
            exec_timeout_secs: None,
            max_output_bytes: None,
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
    fn compact_footer_turn_cost_is_opt_in() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(!config.show_turn_cost);

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.show_turn_cost = true;
        let config = config_with_empty_global(cli, directory.path()).unwrap();
        assert!(config.show_turn_cost);
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
        cli.reasoning = Some("nonsense".into());
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
    fn untrusted_project_config_is_ignored_and_cannot_relax_global_policy() {
        let directory = cwd();
        let global = directory.path().join("global.toml");
        std::fs::write(
            &global,
            "model = 'global'\nallow_external_paths = false\nallow_edit = false\nallow_write = false\nallow_process = false\nallow_shell = false\nsession_dir = 'global-sessions'\n",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join(".ygg")).unwrap();
        std::fs::write(
            directory.path().join(".ygg/config.toml"),
            "model = 'project'\nallow_external_paths = true\nallow_edit = true\nallow_write = true\nallow_process = true\nallow_shell = true\nsession_dir = 'project-sessions'\n",
        )
        .unwrap();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        let config = build_config_with_global_path(cli, directory.path(), Some(&global)).unwrap();

        assert_eq!(config.model.unwrap().0, "global");
        assert!(!config.sandbox.allow_external_paths);
        assert!(!config.sandbox.allow_edit);
        assert!(!config.sandbox.allow_write);
        assert!(!config.sandbox.allow_process);
        assert!(!config.sandbox.allow_shell);
        assert_eq!(config.session_dir, PathBuf::from("global-sessions"));
        assert!(!config.tools.enabled("edit"));
        assert!(!config.tools.enabled("write"));
        assert!(!config.tools.enabled("exec"));
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
        assert_eq!(config.trusted_extensions, vec!["user-tool"]);
        assert!(config.invocation_trusted_extensions.is_empty());
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
    fn persistent_source_trust_rejects_relative_paths() {
        let error = normalize_extension_trust_grants([
            "git-tools@.ygg/extensions/git-tools/extension.toml".to_owned(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("absolute path"));
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
            "max_cost_microdollars = 100\ncost_warning_microdollars = 25\nshow_turn_cost = false\n[compaction]\nenabled = false\ncompact_model = 'cheap'",
        )
        .unwrap();
        let project: ConfigLayer = toml::from_str(
            "cost_warning_microdollars = 40\nshow_turn_cost = true\n[compaction]\nkeep_recent_turns = 2",
        )
        .unwrap();
        let mut merged = global;
        merged.merge(project);
        assert_eq!(merged.max_cost_microdollars, Some(100));
        assert_eq!(merged.cost_warning_microdollars, Some(40));
        assert_eq!(merged.show_turn_cost, Some(true));
        let compaction = merged.compaction.unwrap();
        assert_eq!(compaction.enabled, Some(false));
        assert_eq!(compaction.compact_model.as_deref(), Some("cheap"));
        assert_eq!(compaction.keep_recent_turns, Some(2));
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
        assert!(
            content.contains("# model = \"commented-out\""),
            "commented line preserved: {content}"
        );
        assert!(
            content.contains("model = \"active-model\""),
            "new entry appended: {content}"
        );
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
    fn debug_prompt_is_an_explicit_prompt_template_diagnostic() {
        let cli = Cli::try_parse_from(["ygg", "--print", "--prompt", "review", "--debug-prompt"])
            .unwrap();
        assert_eq!(cli.prompt_template.as_deref(), Some("review"));
        assert!(cli.debug_prompt);
    }
}
