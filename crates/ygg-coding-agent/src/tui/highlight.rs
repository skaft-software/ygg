use regex::Regex;
use sexy_tui_rs::theme::Theme;
use std::sync::OnceLock;

struct Rule {
    token: &'static str,
    regex: Regex,
}

struct LangRules {
    rules: Vec<Rule>,
}

fn get_rules(lang: &str) -> Option<&'static LangRules> {
    static RUST_RULES: OnceLock<LangRules> = OnceLock::new();
    static PYTHON_RULES: OnceLock<LangRules> = OnceLock::new();
    static JS_RULES: OnceLock<LangRules> = OnceLock::new();
    static SHELL_RULES: OnceLock<LangRules> = OnceLock::new();
    static JSON_RULES: OnceLock<LangRules> = OnceLock::new();

    match lang {
        "rust" | "rs" => Some(RUST_RULES.get_or_init(|| LangRules {
            rules: vec![
                Rule { token: "syntax_comment", regex: Regex::new(r"//.*|/\*(?s:.*?)\*/").unwrap() },
                Rule { token: "syntax_string", regex: Regex::new(r#""(\\.|[^"\\])*"|'(\\.|[^'\\])'|r#"[^"]*""#).unwrap() },
                Rule { token: "syntax_keyword", regex: Regex::new(r"\b(fn|let|mut|const|static|struct|enum|union|trait|impl|pub|use|mod|as|crate|self|Self|return|if|else|match|loop|while|for|in|break|continue|type|where|unsafe|async|await|dyn|move|ref)\b").unwrap() },
                Rule { token: "syntax_type", regex: Regex::new(r"\b(u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize|f32|f64|bool|char|str|String|Option|Result|Vec|Box|Rc|Arc|Cell|RefCell|HashMap|BTreeMap|BTreeSet|HashSet)\b").unwrap() },
                Rule { token: "syntax_number", regex: Regex::new(r"\b(0x[0-9a-fA-F_]+|0b[01_]+|0o[0-7_]+|[0-9_]+(\.[0-9_]+)?([eE][+-]?[0-9_]+)?)\b").unwrap() },
                Rule { token: "syntax_punctuation", regex: Regex::new(r"[{}()\[\].,;:]").unwrap() },
                Rule { token: "syntax_operator", regex: Regex::new(r"[+\-*/%&|^!=<>?~@]").unwrap() },
            ]
        })),
        "python" | "py" => Some(PYTHON_RULES.get_or_init(|| LangRules {
            rules: vec![
                Rule { token: "syntax_comment", regex: Regex::new(r"#.*").unwrap() },
                Rule { token: "syntax_string", regex: Regex::new(r#""{3}(?s:.*?)"{3}|'{3}(?s:.*?)'{3}|"(\\.|[^"\\])*"|'(\\.|[^'\\])*'"#).unwrap() },
                Rule { token: "syntax_keyword", regex: Regex::new(r"\b(def|class|return|if|elif|else|for|while|in|is|and|or|not|import|from|as|try|except|finally|raise|assert|with|yield|lambda|pass|break|continue|global|nonlocal|async|await|None|True|False)\b").unwrap() },
                Rule { token: "syntax_type", regex: Regex::new(r"\b(int|float|str|bool|list|tuple|dict|set|bytes|object|type)\b").unwrap() },
                Rule { token: "syntax_number", regex: Regex::new(r"\b([0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?)\b").unwrap() },
                Rule { token: "syntax_punctuation", regex: Regex::new(r"[{}()\[\].,;:]").unwrap() },
                Rule { token: "syntax_operator", regex: Regex::new(r"[+\-*/%&|^!=<>?]").unwrap() },
            ]
        })),
        "javascript" | "js" | "typescript" | "ts" => Some(JS_RULES.get_or_init(|| LangRules {
            rules: vec![
                Rule { token: "syntax_comment", regex: Regex::new(r"//.*|/\*(?s:.*?)\*/").unwrap() },
                Rule { token: "syntax_string", regex: Regex::new(r#""(\\.|[^"\\])*"|'(\\.|[^'\\])*'|`[^`]*`"#).unwrap() },
                Rule { token: "syntax_keyword", regex: Regex::new(r"\b(const|let|var|function|return|if|else|for|while|do|switch|case|break|continue|import|export|from|default|class|extends|new|this|typeof|instanceof|in|of|try|catch|finally|throw|async|await|interface|type|public|private|protected|readonly|any|string|number|boolean|void|null|undefined|true|false)\b").unwrap() },
                Rule { token: "syntax_number", regex: Regex::new(r"\b([0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?)\b").unwrap() },
                Rule { token: "syntax_punctuation", regex: Regex::new(r"[{}()\[\].,;:]").unwrap() },
                Rule { token: "syntax_operator", regex: Regex::new(r"[+\-*/%&|^!=<>?~]").unwrap() },
            ]
        })),
        "bash" | "sh" | "zsh" | "shell" => Some(SHELL_RULES.get_or_init(|| LangRules {
            rules: vec![
                Rule { token: "syntax_comment", regex: Regex::new(r"#.*").unwrap() },
                Rule { token: "syntax_string", regex: Regex::new(r#""(\\.|[^"\\])*"|'[^']*'"#).unwrap() },
                Rule { token: "syntax_keyword", regex: Regex::new(r"\b(if|then|elif|else|fi|case|esac|for|while|until|do|done|in|function|local|return|exit|echo|printf|cd|pwd|ls|grep|sed|awk)\b").unwrap() },
                Rule { token: "syntax_operator", regex: Regex::new(r"[=|&;<>+\-*/%]").unwrap() },
            ]
        })),
        "json" | "yaml" | "yml" => Some(JSON_RULES.get_or_init(|| LangRules {
            rules: vec![
                Rule { token: "syntax_string", regex: Regex::new(r#""(\\.|[^"\\])*"|'(\\.|[^'\\])*'"#).unwrap() },
                Rule { token: "syntax_keyword", regex: Regex::new(r"\b(true|false|null)\b").unwrap() },
                Rule { token: "syntax_number", regex: Regex::new(r"\b([0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?)\b").unwrap() },
                Rule { token: "syntax_punctuation", regex: Regex::new(r"[{}()\[\].,;:]").unwrap() },
            ]
        })),
        _ => None,
    }
}

pub fn highlight_line(line: &str, lang: &str, theme: &Theme) -> String {
    let rules = match get_rules(lang) {
        Some(r) => &r.rules,
        None => return line.to_string(),
    };

    let mut matches = Vec::new();
    for rule in rules {
        for mat in rule.regex.find_iter(line) {
            matches.push((mat.start(), mat.end(), rule.token));
        }
    }

    // Sort matches: first by start index, then reverse by length (longest match first)
    matches.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1))
    });

    let mut result = String::new();
    let mut last_idx = 0;

    for (start, end, token) in matches {
        if start < last_idx {
            continue; // Overlapping match, skip
        }
        if start > last_idx {
            result.push_str(&line[last_idx..start]);
        }
        let text = &line[start..end];
        result.push_str(&theme.fg(token, text));
        last_idx = end;
    }

    if last_idx < line.len() {
        result.push_str(&line[last_idx..]);
    }

    result
}
