#![allow(missing_docs)]

use std::path::{Path, PathBuf};

use clap::Parser;

use crate::config::{self, Config, Mode, ResumeSelector, SandboxPolicy};

/// Command-line launcher for `ygg`.
#[derive(Debug, Parser)]
#[command(name = "ygg", about = "A local-first coding agent")]
pub struct Cli {
    /// An initial prompt. In interactive mode it is submitted after startup.
    pub prompt: Option<String>,
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
    #[arg(long, default_value_t = 40)]
    pub max_turns: u64,
    /// Persistent session directory override.
    #[arg(long)]
    pub session_dir: Option<PathBuf>,
    /// Disable file editing tools.
    #[arg(long)]
    pub no_edit: bool,
    /// Disable structured process execution.
    #[arg(long)]
    pub no_process: bool,
    /// Enable shell execution (structured process mode remains enabled unless disabled).
    #[arg(long)]
    pub allow_shell: bool,
    /// Maximum execution time in seconds.
    #[arg(long, default_value_t = 120)]
    pub exec_timeout_secs: u64,
    /// Maximum persisted tool output size in bytes.
    #[arg(long, default_value_t = 64 * 1024)]
    pub max_output_bytes: usize,
}

/// Convert parsed CLI arguments into the process configuration.
pub fn build_config(cli: Cli, cwd: &Path) -> anyhow::Result<Config> {
    let invocation_cwd = cwd.canonicalize()?;
    let workspace = config::resolve_workspace(cli.workspace.as_deref(), &invocation_cwd)?;
    if !invocation_cwd.starts_with(&workspace) {
        anyhow::bail!(
            "invocation directory {} is outside workspace {}",
            invocation_cwd.display(),
            workspace.display()
        );
    }

    let reasoning = cli
        .reasoning
        .as_deref()
        .map(config::parse_reasoning)
        .transpose()?
        .unwrap_or_default();

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
            let id = id.trim().to_string();
            (!id.is_empty()).then_some(id)
        }))
    } else {
        ResumeSelector::New
    };

    let sandbox = SandboxPolicy {
        allow_edit: !cli.no_edit,
        allow_process: !cli.no_process,
        allow_shell: cli.allow_shell,
        exec_timeout_secs: cli.exec_timeout_secs,
        max_output_bytes: cli.max_output_bytes,
    };

    Ok(Config {
        workspace,
        invocation_cwd,
        model: cli.model.map(ygg_ai::ModelId),
        reasoning,
        sandbox,
        theme: cli.theme,
        session_dir: cli.session_dir.unwrap_or_else(config::default_session_dir),
        compaction: config::CompactionPolicy::default(),
        max_turns: cli.max_turns.max(1),
        show_reasoning_in_print: cli.show_reasoning,
        initial_prompt: (!cli.print).then_some(cli.prompt).flatten(),
        mode,
        resume,
    })
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
            print: false,
            continue_: false,
            resume: None,
            model: None,
            reasoning: None,
            workspace: None,
            theme: None,
            show_reasoning: false,
            max_turns: 40,
            session_dir: None,
            no_edit: false,
            no_process: false,
            allow_shell: false,
            exec_timeout_secs: 120,
            max_output_bytes: 64 * 1024,
        }
    }

    #[test]
    fn print_mode_requires_prompt_text() {
        let directory = cwd();
        let mut cli = base();
        cli.print = true;
        cli.model = Some("m".into());
        cli.workspace = Some(directory.path().into());
        assert!(build_config(cli, directory.path()).is_err());
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
        let config = build_config(cli, directory.path()).unwrap();
        assert!(matches!(config.mode, Mode::Print { prompt } if prompt == "hi"));
        assert!(config.show_reasoning_in_print);
    }

    #[test]
    fn continue_sets_resume_selector_and_interactive_mode() {
        let directory = cwd();
        let mut cli = base();
        cli.continue_ = true;
        cli.workspace = Some(directory.path().into());
        let config = build_config(cli, directory.path()).unwrap();
        assert!(matches!(config.resume, ResumeSelector::Continue));
        assert!(matches!(config.mode, Mode::Interactive));
    }

    #[test]
    fn reasoning_is_parsed_and_invalid_values_fail() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning = Some("off".into());
        assert!(build_config(cli, directory.path()).is_ok());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning = Some("budget=2048".into());
        assert!(build_config(cli, directory.path()).is_ok());

        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.reasoning = Some("nonsense".into());
        assert!(build_config(cli, directory.path()).is_err());
    }

    #[test]
    fn resume_without_an_id_is_distinct_from_resume_by_id() {
        let directory = cwd();
        let mut cli = base();
        cli.workspace = Some(directory.path().into());
        cli.resume = Some(None);
        assert!(matches!(
            build_config(cli, directory.path()).unwrap().resume,
            ResumeSelector::Resume(None)
        ));
    }
}
