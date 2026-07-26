//! Shared validation and sanitization limits for protocol v1.

use serde_json::Value;

/// Maximum serialized command envelope size (512 KiB).
pub const MAX_COMMAND_BYTES: usize = 512 * 1024;
/// Maximum serialized event envelope size (1 MiB).
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// Maximum serialized selected-session snapshot size (8 MiB).
pub const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum serialized host bootstrap size (12 MiB).
pub const MAX_BOOTSTRAP_BYTES: usize = 12 * 1024 * 1024;
/// Maximum submitted prompt text size (256 KiB).
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;
/// Maximum cumulative text retained in one transcript item (512 KiB).
pub const MAX_ITEM_TEXT_BYTES: usize = 512 * 1024;
/// Maximum ordinary public text field size (64 KiB).
pub const MAX_PUBLIC_TEXT_BYTES: usize = 64 * 1024;
/// Maximum one public diagnostic (8 KiB).
pub const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
/// Maximum JSON nesting accepted in inert public values.
pub const MAX_JSON_DEPTH: usize = 32;

/// A public-boundary value failed explicit validation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {message}")]
pub struct ValidationError {
    /// Stable field path.
    pub field: &'static str,
    /// Sanitized description.
    pub message: String,
}

impl ValidationError {
    pub(crate) fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// Explicit validation required before a DTO crosses the trust boundary.
pub trait ProtocolValidation {
    /// Validates all bounded and semantic invariants.
    fn validate(&self) -> Result<(), ValidationError>;
}

/// Validates a bounded UTF-8 field and rejects unsafe control characters.
pub fn validate_public_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    multiline: bool,
) -> Result<(), ValidationError> {
    if value.len() > max_bytes {
        return Err(ValidationError::new(
            field,
            format!("exceeds the {max_bytes}-byte limit"),
        ));
    }
    if value.chars().any(|character| {
        character.is_control() && !(multiline && matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(ValidationError::new(
            field,
            "contains a disallowed control character",
        ));
    }
    Ok(())
}

/// Produces bounded, control-safe text from an internal diagnostic.
pub fn sanitize_public_text(value: &str, max_bytes: usize, multiline: bool) -> String {
    const MARKER: &str = "\n[… truncated …]";
    let mut sanitized = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars() {
        let allowed_whitespace = multiline && matches!(character, '\n' | '\r' | '\t');
        let unsafe_directional = matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        );
        if (character.is_control() && !allowed_whitespace) || unsafe_directional {
            sanitized.push('\u{fffd}');
        } else {
            sanitized.push(character);
        }
    }
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let marker = if multiline && max_bytes >= MARKER.len() {
        MARKER
    } else {
        "…"
    };
    let mut keep = max_bytes.saturating_sub(marker.len());
    while keep > 0 && !sanitized.is_char_boundary(keep) {
        keep -= 1;
    }
    sanitized.truncate(keep);
    sanitized.push_str(marker);
    sanitized
}

/// Validates both serialized size and nesting of inert JSON.
pub fn validate_json(
    field: &'static str,
    value: &Value,
    max_bytes: usize,
) -> Result<(), ValidationError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| ValidationError::new(field, "cannot be serialized as JSON"))?;
    if bytes.len() > max_bytes {
        return Err(ValidationError::new(
            field,
            format!("exceeds the {max_bytes}-byte limit"),
        ));
    }

    fn visit(field: &'static str, value: &Value, depth: usize) -> Result<(), ValidationError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ValidationError::new(
                field,
                format!("exceeds the {MAX_JSON_DEPTH}-level nesting limit"),
            ));
        }
        match value {
            Value::String(text) => validate_public_text(field, text, MAX_PUBLIC_TEXT_BYTES, true),
            Value::Array(items) => {
                for item in items {
                    visit(field, item, depth + 1)?;
                }
                Ok(())
            }
            Value::Object(entries) => {
                for (key, item) in entries {
                    validate_public_text(field, key, 256, false)?;
                    visit(field, item, depth + 1)?;
                }
                Ok(())
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        }
    }

    visit(field, value, 0)
}

pub(crate) fn validate_serialized_size<T: serde::Serialize>(
    field: &'static str,
    value: &T,
    limit: usize,
) -> Result<(), ValidationError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| ValidationError::new(field, "cannot be serialized as JSON"))?;
    if bytes.len() > limit {
        return Err(ValidationError::new(
            field,
            format!("serialized value exceeds the {limit}-byte limit"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitization_removes_terminal_and_directional_controls() {
        let text = sanitize_public_text("safe\u{1b}[31m\u{202e}evil", 128, true);
        assert_eq!(text, "safe�[31m�evil");
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn deeply_nested_json_is_rejected() {
        let mut value = Value::Null;
        for _ in 0..=MAX_JSON_DEPTH {
            value = serde_json::json!([value]);
        }
        assert!(validate_json("payload", &value, 4096).is_err());
    }
}
