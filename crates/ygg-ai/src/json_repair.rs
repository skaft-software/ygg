//! Conservative JSON repair for provider-generated tool arguments.
//!
//! Repairs lexical mistakes commonly produced by local/open-compatible models
//! without inventing missing values or closing truncated objects. This keeps
//! tool execution safe: a cut-off command is still rejected rather than being
//! completed speculatively.

use crate::error::DecodeError;

const VALID_ESCAPES: &[u8] = b"\"\\/bfnrtu";

/// Parse a JSON value after applying conservative provider-output repairs.
pub(crate) fn parse_json_value(input: &str) -> Result<serde_json::Value, DecodeError> {
    let input = strip_json_fence(input.trim());
    if let Ok(value) = serde_json::from_str(input) {
        return Ok(value);
    }

    let repaired = repair_json(input);
    serde_json::from_str(&repaired).map_err(|error| DecodeError::Json(error.to_string()))
}

/// Normalize provider-generated tool arguments into a canonical JSON object.
pub(crate) fn normalize_json_object(input: &str) -> Result<String, DecodeError> {
    let value = parse_json_value(input)?;
    if !value.is_object() {
        return Err(DecodeError::Json(
            "Arguments must be a JSON object".to_string(),
        ));
    }
    serde_json::to_string(&value).map_err(|error| DecodeError::Json(error.to_string()))
}

fn strip_json_fence(input: &str) -> &str {
    let Some(after_open) = input.strip_prefix("```") else {
        return input;
    };
    let after_language = after_open
        .strip_prefix("json")
        .or_else(|| after_open.strip_prefix("JSON"))
        .unwrap_or(after_open);
    let after_language = after_language
        .strip_prefix("\r\n")
        .or_else(|| after_language.strip_prefix('\n'))
        .unwrap_or(after_language);
    after_language
        .strip_suffix("```")
        .map(str::trim_end)
        .unwrap_or(input)
}

fn repair_json(input: &str) -> String {
    let lexical = repair_string_literals(input);
    let keys = quote_unquoted_object_keys(&lexical);
    let python = replace_python_literals(&keys);
    remove_trailing_commas(&python)
}

/// Escape raw controls and invalid backslash escapes, and accept Python-style
/// single-quoted strings. This mirrors pi's invalid-escape repair while also
/// covering a frequent local-model dialect.
fn repair_string_literals(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len() + 8);
    let mut index = 0usize;
    let mut quote: Option<u8> = None;

    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii() {
            let character = input[index..].chars().next().expect("valid UTF-8 boundary");
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        match quote {
            None => {
                if byte == b'\'' {
                    quote = Some(b'\'');
                    output.push('"');
                } else {
                    output.push(byte as char);
                    if byte == b'"' {
                        quote = Some(b'"');
                    }
                }
                index += 1;
            }
            Some(active_quote) => {
                if byte == active_quote {
                    output.push('"');
                    quote = None;
                    index += 1;
                    continue;
                }
                if active_quote == b'\'' && byte == b'"' {
                    output.push_str("\\\"");
                    index += 1;
                    continue;
                }
                if byte == b'\\' {
                    let Some(&next) = bytes.get(index + 1) else {
                        output.push_str("\\\\");
                        index += 1;
                        continue;
                    };
                    if active_quote == b'\'' && next == b'\'' {
                        output.push('\'');
                        index += 2;
                        continue;
                    }
                    if next == b'u'
                        && bytes.get(index + 2..index + 6).is_some_and(|digits| {
                            digits.len() == 4
                                && digits.iter().all(|digit| digit.is_ascii_hexdigit())
                        })
                    {
                        output.push_str(std::str::from_utf8(&bytes[index..index + 6]).unwrap());
                        index += 6;
                        continue;
                    }
                    if VALID_ESCAPES.contains(&next) {
                        output.push('\\');
                        output.push(next as char);
                        index += 2;
                        continue;
                    }
                    // Preserve the literal backslash instead of allowing an
                    // invalid JSON escape such as a Windows `\U` path.
                    output.push_str("\\\\");
                    index += 1;
                    continue;
                }
                match byte {
                    b'\n' => output.push_str("\\n"),
                    b'\r' => output.push_str("\\r"),
                    b'\t' => output.push_str("\\t"),
                    0x08 => output.push_str("\\b"),
                    0x0c => output.push_str("\\f"),
                    0x00..=0x1f => {
                        use std::fmt::Write as _;
                        let _ = write!(output, "\\u{byte:04x}");
                    }
                    _ => output.push(byte as char),
                }
                index += 1;
            }
        }
    }

    // Do not synthesize a missing closing quote. Leaving it open makes the
    // final serde parse reject truncated arguments.
    output
}

/// Quote JavaScript/Python-style bare object keys. A key is changed only when
/// it follows an object boundary (`{` or `,`) and is immediately followed by a
/// colon after optional whitespace, so values and truncated structure are not
/// guessed or completed.
fn quote_unquoted_object_keys(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len() + 8);
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii() {
            let character = input[index..].chars().next().expect("valid UTF-8 boundary");
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        output.push(byte as char);
        index += 1;
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte == b'"' {
            in_string = true;
            continue;
        }
        if !matches!(byte, b'{' | b',') {
            continue;
        }

        let whitespace_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        output.push_str(&input[whitespace_start..index]);
        let key_start = index;
        if !bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            continue;
        }
        index += 1;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.'))
        {
            index += 1;
        }
        let key_end = index;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index) == Some(&b':') {
            output.push('"');
            output.push_str(&input[key_start..key_end]);
            output.push('"');
            output.push_str(&input[key_end..=index]);
            index += 1;
        } else {
            output.push_str(&input[key_start..index]);
        }
    }
    output
}

fn replace_python_literals(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii() {
            let character = input[index..].chars().next().expect("valid UTF-8 boundary");
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        if in_string {
            output.push(byte as char);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push('"');
            index += 1;
            continue;
        }

        let rest = &input[index..];
        let replacement = [("None", "null"), ("True", "true"), ("False", "false")]
            .into_iter()
            .find(|(token, _)| {
                rest.starts_with(token)
                    && token_boundary(bytes.get(index.wrapping_sub(1)).copied())
                    && token_boundary(bytes.get(index + token.len()).copied())
            });
        if let Some((token, value)) = replacement {
            output.push_str(value);
            index += token.len();
        } else {
            output.push(byte as char);
            index += 1;
        }
    }
    output
}

fn token_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
}

fn remove_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii() {
            let character = input[index..].chars().next().expect("valid UTF-8 boundary");
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        if in_string {
            output.push(byte as char);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push('"');
            index += 1;
            continue;
        }
        if byte == b',' {
            let next = bytes[index + 1..]
                .iter()
                .copied()
                .find(|candidate| !candidate.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                index += 1;
                continue;
            }
        }
        output.push(byte as char);
        index += 1;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_json_is_canonicalized_without_semantic_changes() {
        assert_eq!(
            normalize_json_object(r#"{ "path": "src/main.rs", "n": 2 }"#).unwrap(),
            r#"{"n":2,"path":"src/main.rs"}"#
        );
    }

    #[test]
    fn repairs_controls_invalid_escapes_trailing_commas_and_python_literals() {
        let raw = "{path:'C:\\Users\\example', lines:['a\nb',], ok:True, none:None,}";
        let value = parse_json_value(raw).unwrap();
        assert_eq!(value["path"], r"C:\Users\example");
        assert_eq!(value["lines"][0], "a\nb");
        assert_eq!(value["ok"], true);
        assert!(value["none"].is_null());
    }

    #[test]
    fn accepts_json_code_fences() {
        assert_eq!(
            normalize_json_object("```json\n{\"path\":\"README.md\"}\n```").unwrap(),
            r#"{"path":"README.md"}"#
        );
        assert_eq!(
            normalize_json_object("{'message':'你好 🌲'}").unwrap(),
            r#"{"message":"你好 🌲"}"#
        );
    }

    #[test]
    fn never_completes_truncated_json() {
        for raw in [r#"{"command":"rm -r"#, r#"{"path":"src"#, "{'path':'src"] {
            assert!(normalize_json_object(raw).is_err(), "accepted {raw:?}");
        }
    }
}
