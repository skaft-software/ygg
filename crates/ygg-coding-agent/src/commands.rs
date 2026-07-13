#![allow(missing_docs)]

use crate::app::{reasoning_label, App, Reconfig};
use crate::compaction::{estimate_next_request_tokens, hard_input_budget};
use crate::session_store::active_branch_title;

/// Parsed in-TUI command. Commands are deliberately separate from shell CLI
/// options: only editor text beginning with `/` enters this grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Model(Option<String>),
    Thinking(Option<String>),
    Theme(Option<String>),
    Compact,
    New,
    Resume(Option<String>),
    Status,
    Help,
    Quit,
    Unknown(String),
}

/// Parse a slash command without interpreting models, paths, or capabilities.
pub fn parse(input: &str) -> Command {
    let input = input.trim();
    let Some(body) = input.strip_prefix('/') else {
        return Command::Unknown(input.to_owned());
    };
    let mut parts = body.split_whitespace();
    let name = parts.next().unwrap_or_default();
    let argument = parts.next().map(str::to_owned);
    if parts.next().is_some() {
        return Command::Unknown(input.to_owned());
    }

    match name {
        "model" => Command::Model(argument),
        "thinking" => Command::Thinking(argument),
        "theme" => Command::Theme(argument),
        "compact" if argument.is_none() => Command::Compact,
        "new" if argument.is_none() => Command::New,
        "resume" => Command::Resume(argument),
        "status" if argument.is_none() => Command::Status,
        "help" if argument.is_none() => Command::Help,
        "quit" if argument.is_none() => Command::Quit,
        _ => Command::Unknown(input.to_owned()),
    }
}

/// Detailed status text suitable for the `/status` overlay.
pub fn status_text(app: &App, queued: Option<&Reconfig>) -> String {
    let session = app.agent.session();
    let session_id = session
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("(unknown)");
    let queue = queued
        .map(|item| format!("{item:?}"))
        .unwrap_or_else(|| "none".to_owned());
    format!(
        "Model: {}\nThinking: {}\nWorkspace: {}\nSession: {} — {}\nContext estimate: ~{} / {} tokens\nSandbox: edit={} process={} shell={}\nQueued reconfiguration: {}",
        app.model.spec.id.0,
        reasoning_label(&app.reasoning),
        app.config.workspace.display(),
        session_id,
        active_branch_title(session),
        estimate_next_request_tokens(app, ""),
        hard_input_budget(&app.model),
        app.config.sandbox.allow_edit,
        app.config.sandbox.allow_process,
        app.config.sandbox.allow_shell,
        queue,
    )
}

/// Concrete interaction reference shown by `/help`.
pub fn help_text() -> String {
    [
        "Commands: /model [id], /thinking [level], /theme [name], /compact, /new, /resume [id], /status, /help, /quit",
        "Idle: Enter submits; Alt+Enter inserts a newline; Ctrl+C quits.",
        "Active: Enter queues a follow-up; Ctrl+S steers; Ctrl+C aborts.",
        "PageUp/PageDown scroll the transcript. Esc closes overlays.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_complete_v1_command_grammar() {
        assert_eq!(
            parse("/model gpt-4o-mini"),
            Command::Model(Some("gpt-4o-mini".into()))
        );
        assert_eq!(parse("/thinking"), Command::Thinking(None));
        assert_eq!(parse("/theme dusk"), Command::Theme(Some("dusk".into())));
        assert_eq!(parse("/compact"), Command::Compact);
        assert_eq!(parse("/new"), Command::New);
        assert_eq!(parse("/resume id"), Command::Resume(Some("id".into())));
        assert_eq!(parse("/status"), Command::Status);
        assert_eq!(parse("/help"), Command::Help);
        assert_eq!(parse("/quit"), Command::Quit);
    }

    #[test]
    fn rejects_unknown_or_malformed_commands() {
        assert!(matches!(parse("hello"), Command::Unknown(_)));
        assert!(matches!(parse("/new extra"), Command::Unknown(_)));
        assert!(matches!(parse("/checkout"), Command::Unknown(_)));
    }

    fn app_for_status() -> (tempfile::TempDir, App) {
        use crate::app::bootstrap::{bootstrap, build_app, LaunchSelection, SessionSelection};
        use crate::config::{CompactionPolicy, Config, Mode, ResumeSelector, SandboxPolicy};
        use ygg_ai::{ModelId, ReasoningConfig};

        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            workspace: directory.path().to_owned(),
            invocation_cwd: directory.path().to_owned(),
            model: Some(ModelId("gpt-4o-mini".into())),
            reasoning: ReasoningConfig::Off,
            sandbox: SandboxPolicy::default(),
            theme: None,
            session_dir: directory.path().join("sessions"),
            compaction: CompactionPolicy::default(),
            max_turns: 40,
            show_reasoning_in_print: false,
            initial_prompt: None,
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
        };
        let boot = bootstrap(config).unwrap();
        let app = build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::CreateNew(directory.path().join("session.jsonl")),
            },
            "system".into(),
        )
        .unwrap();
        (directory, app)
    }

    #[test]
    fn status_and_help_reference_real_runtime_features() {
        let (_directory, app) = app_for_status();
        let queued = Reconfig::NewSession;
        let status = status_text(&app, Some(&queued));
        for expected in [
            "gpt-4o-mini",
            "Thinking:",
            "Workspace:",
            "Session:",
            "Context estimate:",
            "edit=true process=true shell=false",
            "NewSession",
        ] {
            assert!(
                status.contains(expected),
                "missing {expected:?} in {status}"
            );
        }
        let help = help_text();
        for expected in [
            "Enter submits",
            "Enter queues a follow-up",
            "Ctrl+S steers",
            "Ctrl+C aborts",
            "PageUp/PageDown",
            "Esc closes",
            "Alt+Enter",
        ] {
            assert!(help.contains(expected), "missing {expected:?}");
        }
    }
}
