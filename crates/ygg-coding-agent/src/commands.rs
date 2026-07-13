#![allow(missing_docs)]

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
}
