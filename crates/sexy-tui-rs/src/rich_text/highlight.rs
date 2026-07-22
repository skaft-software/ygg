//! Optional syntect adapter that maps syntax scopes to semantic theme roles.

use std::sync::OnceLock;

use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::style::TextRole;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HighlightedRegion {
    pub text: String,
    pub role: Option<TextRole>,
}

pub(crate) type HighlightedLine = Vec<HighlightedRegion>;

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

pub(crate) fn highlight(code: &str, language: &str) -> Option<Vec<HighlightedLine>> {
    let syntaxes = syntax_set();
    let token = language_token(language);
    let syntax = syntaxes.find_syntax_by_token(token)?;
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut output = Vec::new();

    if code.is_empty() {
        return Some(vec![Vec::new()]);
    }

    for source_line in LinesWithEndings::from(code) {
        let operations = parser.parse_line(source_line, syntaxes).ok()?;
        let mut line = Vec::<HighlightedRegion>::new();
        for (region, operation) in ScopeRegionIterator::new(&operations, source_line) {
            stack.apply(operation).ok()?;
            let region = region.strip_suffix('\n').unwrap_or(region);
            let region = region.strip_suffix('\r').unwrap_or(region);
            if region.is_empty() {
                continue;
            }
            let role = classify_scopes(&stack);
            if let Some(previous) = line.last_mut().filter(|previous| previous.role == role) {
                previous.text.push_str(region);
            } else {
                line.push(HighlightedRegion {
                    text: region.to_owned(),
                    role,
                });
            }
        }
        output.push(line);
    }
    if code.ends_with('\n') {
        output.push(Vec::new());
    }
    Some(output)
}

fn language_token(language: &str) -> &str {
    match language.trim().to_ascii_lowercase().as_str() {
        "rust" => "rs",
        // Syntect's compact default set omits TypeScript and TOML. Nearby
        // grammars still provide useful conservative tokenization.
        "typescript" | "tsx" | "ts" => "js",
        "javascript" | "jsx" => "js",
        "jsonc" => "json",
        "toml" => "properties",
        "shell" | "bash" | "zsh" => "sh",
        "python" => "py",
        "c++" | "cxx" | "cc" => "cpp",
        "markdown" => "md",
        "yml" => "yaml",
        _ => {
            // Known aliases return static tokens; unknown values retain the
            // caller-owned token and therefore the correct lifetime.
            language
        }
    }
}

fn classify_scopes(stack: &ScopeStack) -> Option<TextRole> {
    let mut joined = String::new();
    for scope in stack.as_slice() {
        use std::fmt::Write;
        let _ = write!(joined, " {scope}");
    }
    if joined.contains("comment") {
        Some(TextRole::SyntaxComment)
    } else if joined.contains("string") {
        Some(TextRole::SyntaxString)
    } else if joined.contains("constant.numeric") {
        Some(TextRole::SyntaxNumber)
    } else if joined.contains("entity.name.function") || joined.contains("support.function") {
        Some(TextRole::SyntaxFunction)
    } else if joined.contains("keyword.operator") {
        Some(TextRole::SyntaxOperator)
    } else if joined.contains("keyword")
        || joined.contains("storage.modifier")
        || joined.contains("storage.type")
    {
        Some(TextRole::SyntaxKeyword)
    } else if joined.contains("entity.name.type")
        || joined.contains("support.type")
        || joined.contains("entity.name.class")
    {
        Some(TextRole::SyntaxType)
    } else if joined.contains("variable") || joined.contains("entity.name") {
        Some(TextRole::SyntaxVariable)
    } else if joined.contains("punctuation") {
        Some(TextRole::SyntaxPunctuation)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_languages_load_and_unknown_is_plain() {
        for language in [
            "rust",
            "typescript",
            "javascript",
            "json",
            "toml",
            "yaml",
            "markdown",
            "bash",
            "python",
            "c",
            "c++",
            "diff",
        ] {
            assert!(
                highlight("let value = 1;\n", language).is_some(),
                "{language}"
            );
        }
        assert!(highlight("text", "definitely-unknown-language").is_none());
    }

    #[test]
    fn rust_scopes_map_to_semantic_roles() {
        let lines = highlight("// note\nlet value = \"text\";\n", "rust").unwrap();
        assert!(lines
            .iter()
            .flatten()
            .any(|region| region.role == Some(TextRole::SyntaxComment)));
        assert!(lines
            .iter()
            .flatten()
            .any(|region| region.role == Some(TextRole::SyntaxKeyword)));
        assert!(lines
            .iter()
            .flatten()
            .any(|region| region.role == Some(TextRole::SyntaxString)));
    }
}
