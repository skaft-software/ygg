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

/// One command shown in the prompt's live slash-command suggestions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlashCommandSuggestion {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    accepts_argument: bool,
}

const SLASH_COMMANDS: &[SlashCommandSuggestion] = &[
    SlashCommandSuggestion {
        name: "model",
        usage: "/model [id]",
        description: "select or change the model",
        accepts_argument: true,
    },
    SlashCommandSuggestion {
        name: "thinking",
        usage: "/thinking [level]",
        description: "set thinking level",
        accepts_argument: true,
    },
    SlashCommandSuggestion {
        name: "theme",
        usage: "/theme [name]",
        description: "select or change theme",
        accepts_argument: true,
    },
    SlashCommandSuggestion {
        name: "compact",
        usage: "/compact",
        description: "compact conversation context",
        accepts_argument: false,
    },
    SlashCommandSuggestion {
        name: "new",
        usage: "/new",
        description: "start a new session",
        accepts_argument: false,
    },
    SlashCommandSuggestion {
        name: "resume",
        usage: "/resume [id]",
        description: "resume a saved session",
        accepts_argument: true,
    },
    SlashCommandSuggestion {
        name: "status",
        usage: "/status",
        description: "show session and capability status",
        accepts_argument: false,
    },
    SlashCommandSuggestion {
        name: "help",
        usage: "/help",
        description: "show commands and key bindings",
        accepts_argument: false,
    },
    SlashCommandSuggestion {
        name: "quit",
        usage: "/quit",
        description: "quit Ygg",
        accepts_argument: false,
    },
];

/// Suggestions for an editor value while its first token is a slash command.
pub fn slash_suggestions(input: &str) -> Vec<&'static SlashCommandSuggestion> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) || query.contains('\n') {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(query))
        .collect()
}

/// Complete a unique command-name prefix. Argument-taking commands receive a
/// trailing space so the next keystroke naturally begins their argument.
pub fn complete_slash_command(input: &str) -> Option<String> {
    let suggestions = slash_suggestions(input);
    let [command] = suggestions.as_slice() else {
        return None;
    };
    Some(format!(
        "/{}{}",
        command.name,
        if command.accepts_argument { " " } else { "" }
    ))
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

/// Render a capability gate as an explicit enabled/disabled word rather than a
/// bare boolean, so `/status` reads as a security report.
fn gate(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

/// Detailed status text suitable for the `/status` overlay.
///
/// The security block states Ygg's model plainly: it is a trusted local agent,
/// not an OS sandbox. The workspace path guard only validates explicit path
/// arguments to built-in tools — it never confines a spawned process — so the
/// block never implies that enabled process/shell execution is contained.
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
    let sandbox = &app.config.sandbox;
    format!(
        "Model: {}\nThinking: {}\nWorkspace: {}\nSession: {} — {}\nContext estimate: ~{} / {} tokens\n\
         Security model: trusted local agent\nWorkspace path guard (built-in tools): enabled\nFile edits: {}\n\
         Process execution: {}\nShell execution: {}\nOS isolation: none\n\
         Process privileges: current user\nRepository trust: user-managed\nQueued reconfiguration: {}",
        app.model.spec.id.0,
        reasoning_label(&app.reasoning),
        app.config.workspace.display(),
        session_id,
        active_branch_title(session),
        estimate_next_request_tokens(app, ""),
        hard_input_budget(&app.model),
        gate(sandbox.allow_edit),
        gate(sandbox.allow_process),
        gate(sandbox.allow_shell),
        queue,
    )
}

/// Concrete interaction reference shown by `/help`.
pub fn help_text() -> String {
    [
        "Commands: /model [id], /thinking [level], /theme [name], /compact, /new, /resume [id], /status, /help, /quit",
        "Idle: Enter submits; Alt+Enter inserts a newline; Ctrl+C quits. Type / for commands; Tab completes a unique match.",
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
    fn slash_suggestions_filter_and_tab_complete_unique_prefixes() {
        assert_eq!(slash_suggestions("/").len(), 9);
        assert_eq!(slash_suggestions("/mod")[0].usage, "/model [id]");
        assert_eq!(slash_suggestions("/th").len(), 2);
        assert!(slash_suggestions("/model ").is_empty());
        assert_eq!(complete_slash_command("/mod"), Some("/model ".to_owned()));
        assert_eq!(complete_slash_command("/th"), None);
        assert_eq!(
            complete_slash_command("/status"),
            Some("/status".to_owned())
        );
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
            // The security block reports the trusted-local-agent model with each
            // capability gate as an explicit enabled/disabled word, and never
            // claims OS isolation (defaults: edit + process on, shell off).
            "Security model: trusted local agent",
            "Workspace path guard (built-in tools): enabled",
            "File edits: enabled",
            "Process execution: enabled",
            "Shell execution: disabled",
            "OS isolation: none",
            "Process privileges: current user",
            "Repository trust: user-managed",
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
            "Tab completes",
        ] {
            assert!(help.contains(expected), "missing {expected:?}");
        }
    }
}
