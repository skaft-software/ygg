//! Conservative JSON repair for provider-generated tool arguments.
//!
//! Repairs lexical mistakes commonly produced by local/open-compatible models
//! without inventing missing values or closing truncated objects. This keeps
//! tool execution safe: a cut-off command is still rejected rather than being
//! completed speculatively.

use crate::error::DecodeError;
use std::collections::HashSet;

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

/// Parse and normalize a repaired JSON object without serializing it first.
pub(crate) fn normalize_json_object_value(input: &str) -> Result<serde_json::Value, DecodeError> {
    let value = parse_json_value(input)?;
    if !value.is_object() {
        return Err(DecodeError::Json(
            "Arguments must be a JSON object".to_string(),
        ));
    }
    Ok(value)
}

/// Normalize provider-generated tool arguments into a canonical JSON object.
pub(crate) fn normalize_json_object(input: &str) -> Result<String, DecodeError> {
    let value = normalize_json_object_value(input)?;
    serde_json::to_string(&value).map_err(|error| DecodeError::Json(error.to_string()))
}

// Tool schemas arrive as untrusted request data and provider arguments are
// untrusted response data. Validate the small JSON-Schema subset used by Ygg
// and reject unknown keywords instead of silently ignoring constraints. The
// node budget keeps malformed schemas and values from consuming unbounded CPU.
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 4_096;
const MAX_SCHEMA_TOOLS: usize = 1_024;
const MAX_SCHEMA_PROPERTY_NAME_BYTES: usize = 256;
const MAX_SCHEMA_ERROR_BYTES: usize = 512;
const MAX_SCHEMA_CONSTANT_BYTES: usize = 64 * 1024;
const SUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "$schema",
    "title",
    "description",
    "default",
    "examples",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "allOf",
    "anyOf",
    "oneOf",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

fn supported_json_type(name: &str) -> bool {
    matches!(
        name,
        "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
    )
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut bounded = value.to_owned();
    truncate_utf8(&mut bounded, max_bytes);
    bounded
}

fn schema_path(path: &str, component: &str) -> String {
    let mut child = format!(
        "{path}.{}",
        bounded_text(component, MAX_SCHEMA_PROPERTY_NAME_BYTES)
    );
    truncate_utf8(&mut child, MAX_SCHEMA_ERROR_BYTES);
    child
}

fn index_path(path: &str, index: usize) -> String {
    let mut child = format!("{path}[{index}]");
    truncate_utf8(&mut child, MAX_SCHEMA_ERROR_BYTES);
    child
}

fn bounded_error(prefix: &str, detail: impl Into<String>) -> DecodeError {
    let mut message = format!("{prefix}: {}", detail.into());
    truncate_utf8(&mut message, MAX_SCHEMA_ERROR_BYTES);
    DecodeError::Json(message)
}

struct ValidationBudget {
    remaining: usize,
}

impl Default for ValidationBudget {
    fn default() -> Self {
        Self {
            remaining: MAX_SCHEMA_NODES,
        }
    }
}

impl ValidationBudget {
    fn consume(&mut self, path: &str) -> Result<(), String> {
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            format!(
                "validation work limit exceeded at {}",
                bounded_text(path, MAX_SCHEMA_ERROR_BYTES)
            )
        })?;
        Ok(())
    }
}

/// Validate the shape of every active tool schema before sending it to a provider.
pub(crate) fn validate_tool_definitions(
    tools: &[crate::types::ToolDef],
) -> Result<(), DecodeError> {
    if tools.len() > MAX_SCHEMA_TOOLS {
        return Err(bounded_error(
            "tool argument schema validation failed",
            format!("tool count exceeds {MAX_SCHEMA_TOOLS}"),
        ));
    }
    let mut names = HashSet::new();
    let mut budget = ValidationBudget::default();
    for tool in tools {
        if tool.name.is_empty() || tool.name.len() > MAX_SCHEMA_PROPERTY_NAME_BYTES {
            return Err(bounded_error(
                "tool argument schema validation failed",
                "tool name must contain 1 to 256 bytes",
            ));
        }
        if !names.insert(tool.name.as_str()) {
            return Err(bounded_error(
                "tool argument schema validation failed",
                format!("duplicate tool name `{}`", bounded_text(&tool.name, 128)),
            ));
        }
        validate_schema(
            &tool.parameters,
            &format!("tool `{}`", bounded_text(&tool.name, 128)),
            0,
            &mut budget,
        )
        .map_err(|detail| bounded_error("tool argument schema validation failed", detail))?;
    }
    Ok(())
}

/// Validate one normalized call against the exact request tool-definition snapshot.
pub(crate) fn validate_tool_arguments(
    tool_name: &str,
    arguments: &serde_json::Value,
    tools: &[crate::types::ToolDef],
) -> Result<(), DecodeError> {
    if tools.len() > MAX_SCHEMA_TOOLS {
        return Err(bounded_error(
            "tool argument schema validation failed",
            format!("tool count exceeds {MAX_SCHEMA_TOOLS}"),
        ));
    }
    let mut schema = None;
    for tool in tools {
        if tool.name == tool_name {
            if schema.is_some() {
                return Err(bounded_error(
                    "tool argument schema validation failed",
                    format!(
                        "duplicate schema for tool `{}`",
                        bounded_text(tool_name, 128)
                    ),
                ));
            }
            schema = Some(&tool.parameters);
        }
    }
    let Some(schema) = schema else {
        return Err(bounded_error(
            "tool argument schema validation failed",
            format!("no schema for tool `{}`", bounded_text(tool_name, 128)),
        ));
    };

    let mut schema_budget = ValidationBudget::default();
    validate_schema(
        schema,
        &format!("tool `{}`", bounded_text(tool_name, 128)),
        0,
        &mut schema_budget,
    )
    .map_err(|detail| bounded_error("tool argument schema validation failed", detail))?;
    let mut value_budget = ValidationBudget::default();
    validate_value(schema, arguments, "$", 0, &mut value_budget)
        .map_err(|detail| bounded_error("tool argument validation failed", detail))
}

fn validate_schema(
    schema: &serde_json::Value,
    path: &str,
    depth: usize,
    budget: &mut ValidationBudget,
) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!("schema nesting exceeds {MAX_SCHEMA_DEPTH}"));
    }
    budget.consume(path)?;
    let object = schema
        .as_object()
        .ok_or_else(|| "schema nodes must be objects".to_owned())?;
    for (keyword, value) in object {
        budget.consume(path)?;
        if !SUPPORTED_SCHEMA_KEYWORDS.contains(&keyword.as_str()) {
            return Err(format!(
                "unsupported JSON Schema keyword `{}` at {}",
                bounded_text(keyword, 128),
                bounded_text(path, MAX_SCHEMA_ERROR_BYTES)
            ));
        }
        match keyword.as_str() {
            "$schema" | "title" | "description" => {
                if !value.is_string() {
                    return Err(format!("{keyword} must be a string"));
                }
            }
            "default" | "examples" | "const" => {
                let size = serde_json::to_vec(value)
                    .map_err(|error| format!("{keyword} cannot be encoded: {error}"))?
                    .len();
                if size > MAX_SCHEMA_CONSTANT_BYTES {
                    return Err(format!(
                        "{keyword} exceeds {MAX_SCHEMA_CONSTANT_BYTES} bytes"
                    ));
                }
            }
            _ => {}
        }
    }

    if let Some(types) = object.get("type") {
        let valid = match types {
            serde_json::Value::String(name) => supported_json_type(name),
            serde_json::Value::Array(names) => {
                !names.is_empty()
                    && names
                        .iter()
                        .all(|name| name.as_str().is_some_and(supported_json_type))
            }
            _ => false,
        };
        if !valid {
            return Err(format!("type must name supported JSON types at {path}"));
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| "properties must be an object".to_owned())?;
        for (name, child) in properties {
            if name.len() > MAX_SCHEMA_PROPERTY_NAME_BYTES {
                return Err(format!(
                    "property name exceeds {MAX_SCHEMA_PROPERTY_NAME_BYTES} bytes"
                ));
            }
            validate_schema(child, &schema_path(path, name), depth + 1, budget)?;
        }
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| "required must be an array".to_owned())?;
        let mut names = HashSet::new();
        for name in required {
            budget.consume(path)?;
            let name = name
                .as_str()
                .ok_or_else(|| "required entries must be strings".to_owned())?;
            if !names.insert(name) {
                return Err(format!("duplicate required property `{name}`"));
            }
        }
    }
    if let Some(additional) = object.get("additionalProperties") {
        match additional {
            serde_json::Value::Bool(_) => {}
            serde_json::Value::Object(_) => validate_schema(
                additional,
                &schema_path(path, "additionalProperties"),
                depth + 1,
                budget,
            )?,
            _ => return Err("additionalProperties must be a boolean or schema object".to_owned()),
        }
    }
    if let Some(items) = object.get("items") {
        if !items.is_object() {
            return Err("items must be a schema object".to_owned());
        }
        validate_schema(items, &schema_path(path, "items"), depth + 1, budget)?;
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = object.get(keyword) {
            let branches = branches
                .as_array()
                .filter(|branches| !branches.is_empty())
                .ok_or_else(|| format!("{keyword} must be a non-empty array"))?;
            for (index, branch) in branches.iter().enumerate() {
                validate_schema(
                    branch,
                    &index_path(&schema_path(path, keyword), index),
                    depth + 1,
                    budget,
                )?;
            }
        }
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| "enum must be a non-empty array".to_owned())?;
        for value in values {
            budget.consume(path)?;
            if serde_json::to_vec(value)
                .map_err(|error| format!("enum cannot be encoded: {error}"))?
                .len()
                > MAX_SCHEMA_CONSTANT_BYTES
            {
                return Err(format!(
                    "enum value exceeds {MAX_SCHEMA_CONSTANT_BYTES} bytes"
                ));
            }
        }
    }
    for keyword in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if object.get(keyword).is_some_and(|value| !value.is_number()) {
            return Err(format!("{keyword} must be a number"));
        }
    }
    for keyword in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if object
            .get(keyword)
            .is_some_and(|value| value.as_u64().is_none())
        {
            return Err(format!("{keyword} must be a non-negative integer"));
        }
    }
    if object
        .get("uniqueItems")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("uniqueItems must be boolean".to_owned());
    }
    Ok(())
}

fn value_matches_type(value: &serde_json::Value, expected: &str) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "string" => value.is_string(),
        _ => false,
    }
}

fn validate_value(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
    depth: usize,
    budget: &mut ValidationBudget,
) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!("validation depth exceeds {MAX_SCHEMA_DEPTH}"));
    }
    budget.consume(path)?;
    let object = schema
        .as_object()
        .ok_or_else(|| "validated schema node must be an object".to_owned())?;
    if let Some(expected) = object.get("type") {
        let matches = match expected {
            serde_json::Value::String(name) => value_matches_type(value, name),
            serde_json::Value::Array(names) => names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|name| value_matches_type(value, name)),
            _ => false,
        };
        if !matches {
            return Err(format!("{path} does not match declared type"));
        }
    }
    if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
        let mut matches = false;
        for candidate in values {
            budget.consume(path)?;
            if candidate == value {
                matches = true;
                break;
            }
        }
        if !matches {
            return Err(format!("{path} is not one of the declared enum values"));
        }
    }
    if object
        .get("const")
        .is_some_and(|constant| constant != value)
    {
        return Err(format!("{path} does not match const"));
    }
    if let Some(branches) = object.get("allOf").and_then(serde_json::Value::as_array) {
        for branch in branches {
            validate_value(branch, value, path, depth + 1, budget)?;
        }
    }
    if let Some(branches) = object.get("anyOf").and_then(serde_json::Value::as_array) {
        let mut matches = false;
        for branch in branches {
            if validate_value(branch, value, path, depth + 1, budget).is_ok() {
                matches = true;
                break;
            }
        }
        if !matches {
            return Err(format!("{path} does not match anyOf"));
        }
    }
    if let Some(branches) = object.get("oneOf").and_then(serde_json::Value::as_array) {
        let mut matches = 0_u8;
        for branch in branches {
            if validate_value(branch, value, path, depth + 1, budget).is_ok() {
                matches = matches.saturating_add(1);
                if matches > 1 {
                    break;
                }
            }
        }
        if matches != 1 {
            return Err(format!("{path} does not match exactly one oneOf branch"));
        }
    }
    if let Some(value_object) = value.as_object() {
        if let Some(required) = object.get("required").and_then(serde_json::Value::as_array) {
            for name in required {
                budget.consume(path)?;
                let name = name
                    .as_str()
                    .ok_or_else(|| "validated required entry must be a string".to_owned())?;
                if !value_object.contains_key(name) {
                    return Err(format!("{}.{} is required", path, bounded_text(name, 128)));
                }
            }
        }
        let properties = object
            .get("properties")
            .and_then(serde_json::Value::as_object);
        for (name, child) in value_object {
            budget.consume(path)?;
            let child_path = schema_path(path, name);
            if let Some(child_schema) = properties.and_then(|properties| properties.get(name)) {
                validate_value(child_schema, child, &child_path, depth + 1, budget)?;
            } else if let Some(additional) = object.get("additionalProperties") {
                match additional {
                    serde_json::Value::Bool(false) => {
                        return Err(format!("{child_path} is not allowed"))
                    }
                    serde_json::Value::Bool(true) => {}
                    serde_json::Value::Object(_) => {
                        validate_value(additional, child, &child_path, depth + 1, budget)?;
                    }
                    _ => return Err("validated additionalProperties is malformed".to_owned()),
                }
            }
        }
        let count = value_object.len() as u64;
        if object
            .get("minProperties")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            return Err(format!("{path} has too few properties"));
        }
        if object
            .get("maxProperties")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            return Err(format!("{path} has too many properties"));
        }
    }
    if let Some(value_array) = value.as_array() {
        if let Some(items) = object.get("items") {
            for (index, child) in value_array.iter().enumerate() {
                validate_value(items, child, &index_path(path, index), depth + 1, budget)?;
            }
        }
        let count = value_array.len() as u64;
        if object
            .get("minItems")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            return Err(format!("{path} has too few items"));
        }
        if object
            .get("maxItems")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            return Err(format!("{path} has too many items"));
        }
        if object
            .get("uniqueItems")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let mut unique = HashSet::new();
            for item in value_array {
                budget.consume(path)?;
                let encoded = serde_json::to_vec(item)
                    .map_err(|error| format!("cannot encode {path} item: {error}"))?;
                if !unique.insert(encoded) {
                    return Err(format!("{path} contains duplicate items"));
                }
            }
        }
    }
    if let Some(string) = value.as_str() {
        let count = string.chars().count() as u64;
        if object
            .get("minLength")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            return Err(format!("{path} is shorter than minLength"));
        }
        if object
            .get("maxLength")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            return Err(format!("{path} is longer than maxLength"));
        }
    }
    if let Some(number) = value.as_f64() {
        if object
            .get("minimum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|minimum| number < minimum)
        {
            return Err(format!("{path} violates minimum"));
        }
        if object
            .get("maximum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|maximum| number > maximum)
        {
            return Err(format!("{path} violates maximum"));
        }
        if object
            .get("exclusiveMinimum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|minimum| number <= minimum)
        {
            return Err(format!("{path} violates exclusiveMinimum"));
        }
        if object
            .get("exclusiveMaximum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|maximum| number >= maximum)
        {
            return Err(format!("{path} violates exclusiveMaximum"));
        }
    }
    Ok(())
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

    #[test]
    fn validates_repaired_arguments_against_tool_schema() {
        let tools = vec![crate::types::ToolDef {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }];
        validate_tool_definitions(&tools).unwrap();
        let arguments = normalize_json_object_value("{path:'README.md', offset:1}").unwrap();
        validate_tool_arguments("read", &arguments, &tools).unwrap();

        for invalid in [
            serde_json::json!({"offset": 1}),
            serde_json::json!({"path": "README.md", "offset": 0}),
            serde_json::json!({"path": "README.md", "unexpected": true}),
            serde_json::json!({"path": 7}),
        ] {
            assert!(validate_tool_arguments("read", &invalid, &tools).is_err());
        }
        assert!(validate_tool_arguments("write", &arguments, &tools).is_err());
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_tool_schemas() {
        let duplicate = vec![
            crate::types::ToolDef {
                name: "same".to_owned(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            },
            crate::types::ToolDef {
                name: "same".to_owned(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];
        assert!(validate_tool_definitions(&duplicate).is_err());
        assert!(validate_tool_definitions(&[crate::types::ToolDef {
            name: "unsupported".to_owned(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string", "pattern": "secret"}}
            }),
        }])
        .is_err());
    }

    #[test]
    fn bounds_schema_validation_work_and_error_text() {
        let mut schema = serde_json::json!({"type": "object"});
        for _ in 0..=MAX_SCHEMA_DEPTH {
            schema = serde_json::json!({"type": "object", "properties": {"next": schema}});
        }
        let error = validate_tool_definitions(&[crate::types::ToolDef {
            name: "deep".to_owned(),
            description: String::new(),
            parameters: schema,
        }])
        .unwrap_err();
        let message = error.to_string();
        assert!(message.len() <= MAX_SCHEMA_ERROR_BYTES);
        assert!(message.contains("nesting") || message.contains("work limit"));
    }
}
