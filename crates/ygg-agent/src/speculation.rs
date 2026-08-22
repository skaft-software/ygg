//! Conservative classifier for speculatively executable bash reconnaissance.
//!
//! A "recon" command is a shallow, read-only observation (`ls`, `cat`, `rg`,
//! `git status`, …) whose result is very likely to be requested verbatim by
//! the model. Running such commands while the provider response is still
//! streaming hides their latency inside generation time. The classifier is a
//! latency heuristic, not a security boundary: speculative execution always
//! flows through the same sandbox, effect broker, hooks, and cancellation as
//! serial execution, and any mismatch between the speculated arguments and the
//! authoritative tool call discards the speculative result and re-executes
//! serially.
//!
//! Known limitation: environment-driven configuration such as
//! `RIPGREP_CONFIG_PATH` can extend a listed command with extra behavior that
//! this static check cannot see. That does not add authority (the bash tool
//! already permits arbitrary processes) and does not change session-visible
//! ordering, so it is accepted here.

/// Commands that are pure or read-only observations in their overwhelmingly
/// common forms. The first whitespace-delimited token must match exactly;
/// absolute paths and `env`-style prefixes are deliberately not admitted.
const RECON_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "rg", "grep", "find", "du", "df", "file", "stat",
    "which", "realpath", "basename", "dirname", "git",
];

/// `git` is only admitted for read-only inspection subcommands invoked without
/// global options (so `git -c key=value …` and `--exec`-style plumbing are
/// rejected).
const GIT_RECON_SUBCOMMANDS: &[&str] = &["status", "log", "diff", "show", "rev-parse"];

/// Substrings that make an otherwise read-looking command mutating or
/// process-spawning. Matched case-sensitively against each token; every entry
/// is distinctive enough that legitimate recon arguments do not contain them.
const DENYLIST_TOKEN_SUBSTRINGS: &[&str] = &[
    // find / grep family: execute, delete, or write to files.
    "-exec",
    "-ok",
    "-delete",
    "-fprint",
    "-fls",
    // ripgrep: run a program over each match; custom matcher binaries.
    "--pre",
    "--rr",
    "--hostname-bin",
    "--search-zip",
    "--zbin",
    // git: custom diffs and config overrides can execute configured programs.
    "--ext-diff",
    "--textconv",
    "-c",
    // generic: config files and output redirection to a program.
    "--config",
    "--output",
];

/// Whether a fully assembled bash `arguments` object describes a shallow
/// read-only command that is safe to speculate under the conservative rules
/// above.
pub(crate) fn is_speculatable_recon_bash(arguments: &serde_json::Value) -> bool {
    let Some(object) = arguments.as_object() else {
        return false;
    };
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "command" | "cwd" | "timeout_ms"))
    {
        return false;
    }
    let Some(command) = object.get("command").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if command.is_empty() {
        return false;
    }

    // Single simple command only: reject every metacharacter that could
    // chain, redirect, substitute, quote, or escape, including newlines.
    if command.chars().any(|c| {
        matches!(
            c,
            '|' | '&'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '{'
                | '}'
                | '$'
                | '`'
                | '\\'
                | '\''
                | '"'
                | '\n'
                | '\r'
        )
    }) {
        return false;
    }

    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    if !RECON_COMMANDS.contains(first) {
        return false;
    }
    if *first == "git" {
        let Some(subcommand) = tokens.get(1) else {
            return false;
        };
        // Reject global options (`git -c …`, `git --no-pager …`) and
        // non-inspection subcommands alike.
        if subcommand.starts_with('-') || !GIT_RECON_SUBCOMMANDS.contains(subcommand) {
            return false;
        }
    }
    !tokens.iter().any(|token| {
        DENYLIST_TOKEN_SUBSTRINGS
            .iter()
            .any(|deny| token.contains(deny))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn speculatable(command: &str) -> bool {
        is_speculatable_recon_bash(&json!({ "command": command }))
    }

    #[test]
    fn admits_shallow_read_only_commands() {
        assert!(speculatable("ls"));
        assert!(speculatable("ls -la src"));
        assert!(speculatable("cat README.md"));
        assert!(speculatable("rg --files -g *.rs"));
        assert!(speculatable("grep -rn TODO crates"));
        assert!(speculatable("wc -l src/lib.rs"));
        assert!(speculatable("find . -name *.rs -maxdepth 2"));
        assert!(speculatable("git status --short"));
        assert!(speculatable("git log --oneline -5"));
        assert!(speculatable("git diff HEAD~1"));
        assert!(speculatable("git rev-parse HEAD"));
        assert!(speculatable("du -sh target"));
        assert!(is_speculatable_recon_bash(&json!({
            "command": "ls src",
            "cwd": "crates",
            "timeout_ms": 1000
        })));
    }

    #[test]
    fn rejects_mutating_or_composite_commands() {
        assert!(!speculatable("ls | wc -l"));
        assert!(!speculatable("cat a > b"));
        assert!(!speculatable("echo hi && ls"));
        assert!(!speculatable("ls; git status"));
        assert!(!speculatable("$(git status)"));
        assert!(!speculatable("cat `ls`"));
        assert!(!speculatable("ls \n git status"));
        assert!(!speculatable("cat 'my file'"));
        assert!(!speculatable("cat \"my file\""));
        assert!(!speculatable("ls --color && cat x"));
        assert!(!speculatable("git status; git push"));
    }

    #[test]
    fn rejects_non_recon_programs() {
        assert!(!speculatable("cargo build"));
        assert!(!speculatable("sudo ls"));
        assert!(!speculatable("/bin/ls"));
        assert!(!speculatable("env ls"));
        assert!(!speculatable("rm -rf ."));
        assert!(!speculatable(""));
    }

    #[test]
    fn rejects_dangerous_options_on_listed_commands() {
        assert!(!speculatable("rg --pre 'gunzip' pattern"));
        assert!(!speculatable("find . -name '*.tmp' -delete"));
        assert!(!speculatable("find . -exec rm {} \\;"));
        assert!(!speculatable("find . -fprint output.txt"));
        assert!(!speculatable("git -c core.pager=cat status"));
        assert!(!speculatable("git diff --ext-diff HEAD~1"));
        assert!(!speculatable("git log --textconv"));
        assert!(!speculatable("git push"));
        assert!(!speculatable("grep --output=file pattern"));
    }

    #[test]
    fn rejects_malformed_arguments() {
        assert!(!is_speculatable_recon_bash(&json!({})));
        assert!(!is_speculatable_recon_bash(&json!({ "command": 42 })));
        assert!(!is_speculatable_recon_bash(&json!({ "command": "" })));
        assert!(!is_speculatable_recon_bash(&json!(
            { "command": "ls", "extra": true }
        )));
        assert!(!is_speculatable_recon_bash(&json!("ls")));
    }
}
