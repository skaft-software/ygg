use std::path::Path;

use tree_sitter::Node;
use tree_sitter::Parser;
use tree_sitter::Tree;
use tree_sitter_bash::LANGUAGE as BASH_LANGUAGE;

const MAX_SAFE_COMMAND_WRAPPER_DEPTH: usize = 8;

/// Returns whether a full `bash` command string is conservatively safe.
///
/// This parser is intentionally strict: it accepts only command strings that can
/// be parsed as plain word-only bash commands joined by a conservative set of
/// shell operators (`&&`, `||`, `;`, `|`). It then requires each command in the
/// sequence to match the allowlist of read-only invocations.
pub(crate) fn is_known_safe_bash_command(command: &str) -> bool {
    let Some(commands) = parse_shell_script_into_commands(command) else {
        return false;
    };
    if commands.is_empty() {
        return false;
    }
    commands
        .iter()
        .all(|command| is_known_safe_shell_command(command))
}

fn is_known_safe_shell_command(command: &[String]) -> bool {
    is_known_safe_shell_command_with_depth(command, 0)
}

fn is_known_safe_shell_command_with_depth(command: &[String], depth: usize) -> bool {
    if depth > MAX_SAFE_COMMAND_WRAPPER_DEPTH {
        return false;
    }
    if command.is_empty() {
        return false;
    }

    let command_name = normalize_shell_command_name(command[0].as_str());

    if matches!(command_name.as_str(), "bash" | "zsh" | "sh") {
        if let Some(inner_commands) = parse_shell_lc_plain_commands(command) {
            return inner_commands
                .iter()
                .all(|command| is_known_safe_shell_command_with_depth(command, depth + 1));
        }
    }

    is_safe_to_call_with_exec(command_name.as_str(), command)
}

fn is_safe_to_call_with_exec(command_name: &str, command: &[String]) -> bool {
    if command.is_empty() {
        return false;
    }

    let command_name = command_name.to_owned();

    if cfg!(target_os = "linux") && matches!(command_name.as_str(), "numfmt" | "tac") {
        return true;
    }

    match command_name.as_str() {
        "cat" | "cd" | "cut" | "echo" | "expr" | "false" | "grep" | "head" | "id" | "ls" | "nl"
        | "paste" | "pwd" | "rev" | "seq" | "stat" | "tail" | "tr" | "true" | "uname" | "uniq"
        | "wc" | "which" | "whoami" => true,

        "base64" => !command.iter().skip(1).any(|argument| {
            argument == "-o"
                || argument == "--output"
                || argument.starts_with("--output=")
                || (argument.starts_with("-o") && argument.len() > 2)
        }),

        "find" => {
            const UNSAFE_FIND_OPTIONS: &[&str] = &[
                "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0",
                "-fprintf",
            ];
            !command
                .iter()
                .skip(1)
                .any(|argument| UNSAFE_FIND_OPTIONS.contains(&argument.as_str()))
        }

        "rg" => !command.iter().skip(1).any(|argument| {
            argument == "--pre"
                || argument == "--hostname-bin"
                || argument == "--search-zip"
                || argument == "-z"
                || argument.starts_with("--pre=")
                || argument.starts_with("--hostname-bin=")
        }),

        "sed" => {
            command.len() >= 3
                && command.len() <= 4
                && command.get(1).is_some_and(|argument| argument == "-n")
                && is_valid_sed_n_arg(command.get(2).map(String::as_str))
        }

        "git" => is_safe_git_command(command),
        _ => false,
    }
}

fn is_safe_git_command(command: &[String]) -> bool {
    let Some((subcommand_index, subcommand)) =
        find_git_subcommand(command, &["status", "log", "diff", "show", "branch"])
    else {
        return false;
    };

    let global_options = &command[1..subcommand_index];
    if git_has_unsafe_global_option(global_options) {
        return false;
    }

    let options = &command[subcommand_index + 1..];
    match subcommand {
        "status" | "log" | "diff" | "show" => git_subcommand_args_are_read_only(options),
        "branch" => git_subcommand_args_are_read_only(options) && git_branch_is_read_only(options),
        _ => false,
    }
}

fn is_valid_sed_n_arg(pattern: Option<&str>) -> bool {
    let Some(pattern) = pattern else {
        return false;
    };
    if !pattern.ends_with('p') {
        return false;
    }

    let range = pattern.strip_suffix('p').unwrap_or_default();
    if range.is_empty() {
        return false;
    }
    if range.contains(',') {
        let mut parts = range.split(',');
        let first = parts.next();
        let second = parts.next();
        if parts.next().is_some() {
            return false;
        }
        first.is_some_and(|first| {
            !first.is_empty() && first.chars().all(|character| character.is_ascii_digit())
        }) && second.is_some_and(|second| {
            !second.is_empty() && second.chars().all(|character| character.is_ascii_digit())
        })
    } else {
        !range.is_empty() && range.chars().all(|character| character.is_ascii_digit())
    }
}

#[derive(Clone, Copy)]
enum GitOptionPattern {
    Exact(&'static str),
    ShortWithInlineValue(&'static str),
    Prefix(&'static str),
}

const UNSAFE_GIT_GLOBAL_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::Exact("-C"),
    GitOptionPattern::ShortWithInlineValue("-C"),
    GitOptionPattern::Exact("-c"),
    GitOptionPattern::ShortWithInlineValue("-c"),
    GitOptionPattern::Exact("-p"),
    GitOptionPattern::Exact("--config-env"),
    GitOptionPattern::Prefix("--config-env="),
    GitOptionPattern::Exact("--exec-path"),
    GitOptionPattern::Prefix("--exec-path="),
    GitOptionPattern::Exact("--git-dir"),
    GitOptionPattern::Prefix("--git-dir="),
    GitOptionPattern::Exact("--namespace"),
    GitOptionPattern::Prefix("--namespace="),
    GitOptionPattern::Exact("--paginate"),
    GitOptionPattern::Prefix("--super-prefix"),
    GitOptionPattern::Prefix("--super-prefix="),
    GitOptionPattern::Exact("--work-tree"),
    GitOptionPattern::Prefix("--work-tree="),
];

const UNSAFE_GIT_SUBCOMMAND_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::Exact("--output"),
    GitOptionPattern::Prefix("--output="),
    GitOptionPattern::Exact("--ext-diff"),
    GitOptionPattern::Exact("--textconv"),
    GitOptionPattern::Exact("--exec"),
    GitOptionPattern::Prefix("--exec="),
];

fn find_git_subcommand<'a>(
    command: &'a [String],
    subcommands: &[&'a str],
) -> Option<(usize, &'a str)> {
    if command.is_empty() {
        return None;
    }

    let mut skip_next = false;
    for (index, arg) in command.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg = arg.as_str();

        if is_git_global_option_with_inline_value(arg) {
            continue;
        }

        if is_git_global_option_with_value(arg) {
            skip_next = true;
            continue;
        }

        if arg == "--" || arg.starts_with('-') {
            continue;
        }

        if subcommands.contains(&arg) {
            return Some((index, arg));
        }
        return None;
    }

    None
}

fn is_git_global_option_with_value(argument: &str) -> bool {
    matches!(
        argument,
        "-C" | "-c"
            | "--config-env"
            | "--exec-path"
            | "--git-dir"
            | "--namespace"
            | "--super-prefix"
            | "--work-tree"
    )
}

fn is_git_global_option_with_inline_value(argument: &str) -> bool {
    matches!(
        argument,
        s if s.starts_with("--config-env=")
            || s.starts_with("--exec-path=")
            || s.starts_with("--git-dir=")
            || s.starts_with("--namespace=")
            || s.starts_with("--super-prefix=")
            || s.starts_with("--work-tree=")
    ) || ((argument.starts_with("-C") || argument.starts_with("-c")) && argument.len() > 2)
}

fn git_matches_option_pattern(argument: &str, patterns: &[GitOptionPattern]) -> bool {
    patterns.iter().any(|pattern| match pattern {
        GitOptionPattern::Exact(option) => argument == *option,
        GitOptionPattern::ShortWithInlineValue(option) => {
            argument.starts_with(option) && argument.len() > option.len()
        }
        GitOptionPattern::Prefix(prefix) => argument.starts_with(prefix),
    })
}

fn git_has_unsafe_global_option(global_options: &[String]) -> bool {
    global_options
        .iter()
        .any(|argument| git_matches_option_pattern(argument, UNSAFE_GIT_GLOBAL_OPTIONS))
}

fn git_subcommand_args_are_read_only(args: &[String]) -> bool {
    !args
        .iter()
        .any(|argument| git_matches_option_pattern(argument, UNSAFE_GIT_SUBCOMMAND_OPTIONS))
}

fn git_branch_is_read_only(options: &[String]) -> bool {
    if options.is_empty() {
        return true;
    }

    let mut has_read_only_flag = false;
    for argument in options {
        match argument.as_str() {
            "--list" | "-l" | "--show-current" | "-a" | "--all" | "-r" | "--remotes" | "-v"
            | "-vv" | "--verbose" => {
                has_read_only_flag = true;
            }
            argument if argument.starts_with("--format=") => {
                has_read_only_flag = true;
            }
            _ => {
                return false;
            }
        }
    }
    has_read_only_flag
}

fn parse_shell_script_into_commands(script: &str) -> Option<Vec<Vec<String>>> {
    let tree = try_parse_shell(script)?;
    try_parse_word_only_commands_sequence(&tree, script)
}

fn try_parse_shell(shell_lc_arg: &str) -> Option<Tree> {
    let lang = BASH_LANGUAGE.into();
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return None;
    }
    let old_tree: Option<&Tree> = None;
    parser.parse(shell_lc_arg, old_tree)
}

fn try_parse_word_only_commands_sequence(tree: &Tree, src: &str) -> Option<Vec<Vec<String>>> {
    if tree.root_node().has_error() {
        return None;
    }

    const ALLOWED_KINDS: &[&str] = &[
        "program",
        "list",
        "pipeline",
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
    ];
    const ALLOWED_PUNCT_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut command_nodes = Vec::new();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                command_nodes.push(node);
            }
        } else if kind.chars().any(|character| "&;|".contains(character))
            && !ALLOWED_PUNCT_TOKENS.contains(&kind)
        {
            return None;
        } else if !(ALLOWED_PUNCT_TOKENS.contains(&kind) || kind.trim().is_empty()) {
            return None;
        }

        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }

    command_nodes.sort_by_key(Node::start_byte);

    let mut commands = Vec::new();
    for node in command_nodes {
        if let Some(words) = parse_plain_command_from_node(node, src) {
            commands.push(words);
        } else {
            return None;
        }
    }
    Some(commands)
}

fn parse_shell_lc_plain_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    if command.len() != 3 {
        return None;
    }

    let flag = &command[1];
    let script = &command[2];
    if !matches!(flag.as_str(), "-lc" | "-c") {
        return None;
    }

    let shell_name = normalize_shell_command_name(&command[0]);
    if !matches!(shell_name.as_str(), "bash" | "zsh" | "sh") {
        return None;
    }

    parse_shell_script_into_commands(script)
}

fn parse_plain_command_from_node(cmd: Node<'_>, src: &str) -> Option<Vec<String>> {
    if cmd.kind() != "command" {
        return None;
    }

    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if word_node.kind() != "word" {
                    return None;
                }
                words.push(word_node.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "word" | "number" => {
                words.push(child.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "string" => {
                let parsed = parse_double_quoted_string(child, src)?;
                words.push(parsed);
            }
            "raw_string" => {
                let parsed = parse_raw_string(child, src)?;
                words.push(parsed);
            }
            "concatenation" => {
                let mut concatenated = String::new();
                let mut concat_cursor = child.walk();
                for part in child.named_children(&mut concat_cursor) {
                    match part.kind() {
                        "word" | "number" => {
                            concatenated
                                .push_str(part.utf8_text(src.as_bytes()).ok()?.to_owned().as_str());
                        }
                        "string" => {
                            let parsed = parse_double_quoted_string(part, src)?;
                            concatenated.push_str(&parsed);
                        }
                        "raw_string" => {
                            let parsed = parse_raw_string(part, src)?;
                            concatenated.push_str(&parsed);
                        }
                        _ => return None,
                    }
                }
                if concatenated.is_empty() {
                    return None;
                }
                words.push(concatenated);
            }
            _ => return None,
        }
    }
    Some(words)
}

fn parse_double_quoted_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }

    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        if part.kind() != "string_content" {
            return None;
        }
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))?;
    Some(stripped.to_string())
}

fn parse_raw_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "raw_string" {
        return None;
    }

    let raw_string = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw_string
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''));
    stripped.map(str::to_owned)
}

fn normalize_shell_command_name(raw: &str) -> String {
    let normalized = Path::new(raw)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(raw)
        .to_ascii_lowercase();
    if normalized == "zsh" {
        "bash".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn parse_seq(src: &str) -> Option<Vec<Vec<String>>> {
        parse_shell_script_into_commands(src)
    }

    fn vec_str(args: &[&str]) -> Vec<String> {
        args.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn safe_parse_accepts_simple_chain() {
        let cmds = parse_seq("ls && pwd; echo 'hi' | wc -l").unwrap();
        assert_eq!(
            cmds,
            vec![
                vec_str(&["ls"]),
                vec_str(&["pwd"]),
                vec_str(&["echo", "hi"]),
                vec_str(&["wc", "-l"]),
            ]
        );
    }

    #[test]
    fn safe_bash_command_accepts_allowed_composite_shell_sequence() {
        assert!(is_known_safe_bash_command("ls && pwd; echo 'hi' | wc -l"));
    }

    #[test]
    fn safe_bash_command_accepts_bash_lc_wrapper_chain() {
        assert!(is_known_safe_bash_command("bash -lc \"ls && pwd\""));
    }

    #[test]
    fn safe_bash_command_rejects_redirected_commands() {
        assert!(!is_known_safe_bash_command(
            "printf 'owned' > /tmp/owned.txt"
        ));
    }

    #[test]
    fn safe_bash_command_rejects_parens_and_subshells() {
        assert!(!is_known_safe_bash_command("(ls)"));
        assert!(!is_known_safe_bash_command("ls || (pwd && echo hi)"));
    }

    #[test]
    fn safe_bash_command_rejects_unsafe_find_and_base64_output() {
        assert!(!is_known_safe_bash_command("find . -name file.txt -delete"));
        assert!(!is_known_safe_bash_command(
            "base64 --output=/tmp/out.bin Cargo.toml"
        ));
    }

    #[test]
    fn safe_bash_command_rejects_unsafe_git_branch_creation() {
        assert!(!is_known_safe_bash_command("git branch new-branch"));
    }

    #[test]
    fn parse_shell_lc_plain_commands_accepts_zsh_alias() {
        let command = vec!["zsh".to_string(), "-lc".to_string(), "ls".to_string()];
        assert_eq!(
            parse_shell_lc_plain_commands(&command).unwrap(),
            vec![vec!["ls".to_string()]]
        );
    }

    #[test]
    fn known_safe_bash_command_rejects_non_lc_wrapper_shape() {
        assert!(!is_known_safe_bash_command("bash -ic 'ls'"));
    }
}
