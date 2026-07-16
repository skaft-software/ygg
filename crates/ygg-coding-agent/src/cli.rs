#![allow(missing_docs)]

use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;

use crate::app::bootstrap::resolve_model_id;
use crate::config::{self, CompactionPolicy, Config, Mode, ResumeSelector, SandboxPolicy};

/// Command-line launcher for `ygg`.
#[derive(Debug, Parser)]
#[command(name = "ygg", about = "A local-first coding agent")]
pub struct Cli {
    /// An initial prompt. In interactive mode it is submitted after startup.
    pub prompt: Option<String>,
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
    /// Reasoning: off, minimal, low, medium, high, or budget=N.
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
    /// Emit reasoning deltas in print mode.
    #[arg(long)]
    pub show_reasoning: bool,
    /// Maximum model turns in one run.
    #[arg(long)]
    pub max_turns: Option<u64>,
    /// Persistent session directory override.
    #[arg(long)]
    pub session_dir: Option<PathBuf>,
    /// Disable file editing tools.
    #[arg(long)]
    pub no_edit: bool,
    /// Disable structured process execution.
    #[arg(long)]
    pub no_process: bool,
    /// Enable shell execution (overrides a disabling configuration setting).
    #[arg(long)]
    pub allow_shell: bool,
    /// Maximum execution time in seconds.
    #[arg(long)]
    pub exec_timeout_secs: Option<u64>,
    /// Maximum persisted tool output size in bytes.
    #[arg(long)]
    pub max_output_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CompactionLayer {
    threshold_fraction: Option<f64>,
    keep_recent_turns: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ConfigLayer {
    model: Option<String>,
    reasoning: Option<String>,
    cache_retention: Option<String>,
    theme: Option<String>,
    allow_external_paths: Option<bool>,
    allow_edit: Option<bool>,
    allow_process: Option<bool>,
    allow_shell: Option<bool>,
    exec_timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    session_dir: Option<PathBuf>,
    max_turns: Option<u64>,
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
        override_some!(allow_external_paths);
        override_some!(allow_edit);
        override_some!(allow_process);
        override_some!(allow_shell);
        override_some!(exec_timeout_secs);
        override_some!(max_output_bytes);
        override_some!(session_dir);
        override_some!(max_turns);
        match (self.compaction.as_mut(), newer.compaction) {
            (Some(current), Some(newer)) => {
                if newer.threshold_fraction.is_some() {
                    current.threshold_fraction = newer.threshold_fraction;
                }
                if newer.keep_recent_turns.is_some() {
                    current.keep_recent_turns = newer.keep_recent_turns;
                }
            }
            (None, Some(newer)) => self.compaction = Some(newer),
            _ => {}
        }
    }
}

pub fn global_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("config.toml")
}

pub fn persist_model(model: &str) -> anyhow::Result<()> {
    persist_model_to_path(model, &global_config_path())
}

fn persist_model_to_path(model: &str, path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    // Serialise the model id through toml so special characters are
    // properly escaped rather than producing invalid config.
    let escaped = toml::Value::String(model.to_string());
    let replacement = format!("model = {escaped}");

    let mut found = false;
    let mut new_lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        // Never rewrite a commented-out model line.
        if trimmed.starts_with('#') {
            new_lines.push(line.to_string());
            continue;
        }
        if trimmed.starts_with("model") {
            let after = trimmed.strip_prefix("model").unwrap();
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

fn project_config_path(workspace: &Path) -> PathBuf {
    workspace.join(".ygg").join("config.toml")
}

fn read_layer(path: &Path) -> anyhow::Result<ConfigLayer> {
    if !path.exists() {
        return Ok(ConfigLayer::default());
    }
    let source = std::fs::read_to_string(path)?;
    toml::from_str(&source)
        .map_err(|error| anyhow::anyhow!("invalid config {}: {error}", path.display()))
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

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

fn environment_layer() -> anyhow::Result<ConfigLayer> {
    let threshold_fraction = env_parse("YGG_COMPACTION_THRESHOLD_FRACTION")?;
    let keep_recent_turns = env_parse("YGG_COMPACTION_KEEP_RECENT_TURNS")?;
    Ok(ConfigLayer {
        model: env_value("YGG_MODEL"),
        reasoning: env_value("YGG_REASONING"),
        cache_retention: env_value("YGG_CACHE_RETENTION")
            .or_else(|| env_value("PI_CACHE_RETENTION")),
        theme: env_value("YGG_THEME"),
        allow_external_paths: env_parse("YGG_ALLOW_EXTERNAL_PATHS")?,
        allow_edit: env_parse("YGG_ALLOW_EDIT")?,
        allow_process: env_parse("YGG_ALLOW_PROCESS")?,
        allow_shell: env_parse("YGG_ALLOW_SHELL")?,
        exec_timeout_secs: env_parse("YGG_EXEC_TIMEOUT_SECS")?,
        max_output_bytes: env_parse("YGG_MAX_OUTPUT_BYTES")?,
        session_dir: env_value("YGG_SESSION_DIR").map(PathBuf::from),
        max_turns: env_parse("YGG_MAX_TURNS")?,
        compaction: (threshold_fraction.is_some() || keep_recent_turns.is_some()).then_some(
            CompactionLayer {
                threshold_fraction,
                keep_recent_turns,
            },
        ),
    })
}

fn build_config_with_global_path(
    cli: Cli,
    cwd: &Path,
    global_path: &Path,
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

    let global = read_layer(global_path)?;
    let project = read_layer(&project_config_path(&workspace))?;
    let environment = environment_layer()?;
    let mut values = global.clone();
    values.merge(project.clone());
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

    let mut sandbox = SandboxPolicy::default();
    if let Some(value) = values.allow_external_paths {
        sandbox.allow_external_paths = value;
    }
    if let Some(value) = values.allow_edit {
        sandbox.allow_edit = value;
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
    }
    if cli.no_process {
        sandbox.allow_process = false;
    }
    if cli.allow_shell {
        sandbox.allow_shell = true;
    }
    if let Some(value) = cli.exec_timeout_secs {
        sandbox.exec_timeout_secs = value;
    }
    if let Some(value) = cli.max_output_bytes {
        sandbox.max_output_bytes = value;
    }

    let mut compaction = CompactionPolicy::default();
    if let Some(layer) = values.compaction {
        if let Some(value) = layer.threshold_fraction {
            if !(0.0..=1.0).contains(&value) {
                anyhow::bail!("compaction.threshold_fraction must be between 0 and 1");
            }
            compaction.threshold_fraction = value;
        }
        if let Some(value) = layer.keep_recent_turns {
            compaction.keep_recent_turns = value.max(1);
        }
    }

    let mode = if cli.print {
        let prompt = cli.prompt.clone().ok_or_else(|| {
            anyhow::anyhow!("--print requires a prompt, for example: ygg --print \"...\"")
        })?;
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

    Ok(Config {
        workspace,
        invocation_cwd,
        model,
        reasoning,
        cache_retention,
        sandbox,
        theme: cli.theme.or(values.theme),
        session_dir: cli
            .session_dir
            .or(values.session_dir)
            .unwrap_or_else(config::default_session_dir),
        compaction,
        max_turns: cli.max_turns.or(values.max_turns).unwrap_or(40).max(1),
        show_reasoning_in_print: cli.show_reasoning,
        initial_prompt: (!cli.print).then_some(cli.prompt).flatten(),
        mode,
        resume,
    })
}

/// Convert parsed CLI arguments into layered process configuration.
pub fn build_config(cli: Cli, cwd: &Path) -> anyhow::Result<Config> {
    build_config_with_global_path(cli, cwd, &global_config_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn base() -> Cli {
        Cli {
            prompt: None,
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
            show_reasoning: false,
            max_turns: None,
            session_dir: None,
            no_edit: false,
            no_process: false,
            allow_shell: false,
            exec_timeout_secs: None,
            max_output_bytes: None,
        }
    }

    fn config_with_empty_global(cli: Cli, directory: &Path) -> anyhow::Result<Config> {
        build_config_with_global_path(cli, directory, &directory.join("missing-global.toml"))
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
        cli.prompt = Some("hi".into());
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
        cli.model = Some("cli".into());
        cli.max_turns = Some(11);
        let config = build_config_with_global_path(cli, directory.path(), &global).unwrap();
        assert_eq!(config.model.unwrap().0, "cli");
        assert_eq!(config.theme.as_deref(), Some("project-theme"));
        assert_eq!(config.max_turns, 11);
        assert!(!config.sandbox.allow_external_paths);
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
}
