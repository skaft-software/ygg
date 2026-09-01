// This adapter is intentionally staged before theme discovery and selection.
#![allow(dead_code, missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context};
use serde::Deserialize;
use sexy_tui_rs::Color;

use super::theme_schema::{
    ParsedTheme, RoleStyleSpec, ThemeLayout, ThemeMetadata, MAX_THEME_BYTES,
};

/// Exact upstream compatibility target for this data-only adapter.
pub(crate) const PI_THEME_VERSION: &str = "0.84.4";
pub(crate) const PI_THEME_REVISION: &str = "b79e4cc834970cca69daebffab7df1da7d1e52c4";

const MAX_VARIABLES: usize = 256;
const MAX_VARIABLE_NAME_BYTES: usize = 128;
const MAX_COLOR_VALUE_BYTES: usize = 512;
const MAX_THEME_NAME_BYTES: usize = 80;
const MAX_SCHEMA_BYTES: usize = 512;

const PI_SCHEMA_MAIN: &str = "https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/src/modes/interactive/theme/theme-schema.json";
const PI_SCHEMA_TAG: &str = "https://raw.githubusercontent.com/earendil-works/pi/v0.84.4/packages/coding-agent/src/modes/interactive/theme/theme-schema.json";
const PI_SCHEMA_REVISION: &str = "https://raw.githubusercontent.com/earendil-works/pi/b79e4cc834970cca69daebffab7df1da7d1e52c4/packages/coding-agent/src/modes/interactive/theme/theme-schema.json";

// Pi validates the result of JSON.parse, so decimal/exponent spellings with
// integer values satisfy its integer schema just like ordinary JSON integers.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum PiColorValue {
    Text(String),
    Indexed(serde_json::Number),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiThemeDocument {
    #[serde(
        default,
        rename = "$schema",
        deserialize_with = "deserialize_present_option"
    )]
    schema: Option<String>,
    name: String,
    #[serde(default)]
    vars: BTreeMap<String, PiColorValue>,
    colors: BTreeMap<String, PiColorValue>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    export: Option<BTreeMap<String, PiColorValue>>,
}

// Serde normally maps both a missing field and explicit JSON null to `None`.
// Pi's schema permits omission but not null, so present values deserialize as
// their concrete type and only a genuinely absent field uses `Option::default`.
fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug)]
struct ColorMapping {
    pi: &'static str,
    ygg: &'static str,
    fallback: Option<&'static str>,
}

// Pi's complete 0.84.4 `colors` vocabulary. Optional Pi tokens are materialized
// with the same fallbacks as `withThemeColorFallbacks()` in upstream theme.ts.
const COLOR_MAPPINGS: &[ColorMapping] = &[
    ColorMapping {
        pi: "accent",
        ygg: "accent",
        fallback: None,
    },
    ColorMapping {
        pi: "border",
        ygg: "border",
        fallback: None,
    },
    ColorMapping {
        pi: "borderAccent",
        ygg: "border_focused",
        fallback: None,
    },
    ColorMapping {
        pi: "borderMuted",
        ygg: "border_idle",
        fallback: None,
    },
    ColorMapping {
        pi: "success",
        ygg: "success",
        fallback: None,
    },
    ColorMapping {
        pi: "error",
        ygg: "error",
        fallback: None,
    },
    ColorMapping {
        pi: "warning",
        ygg: "warning",
        fallback: None,
    },
    ColorMapping {
        pi: "muted",
        ygg: "muted",
        fallback: None,
    },
    ColorMapping {
        pi: "dim",
        ygg: "dim",
        fallback: None,
    },
    ColorMapping {
        pi: "text",
        ygg: "foreground",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingText",
        ygg: "thinking_text",
        fallback: None,
    },
    ColorMapping {
        pi: "selectedBg",
        ygg: "selected_bg",
        fallback: None,
    },
    ColorMapping {
        pi: "scrollbarThumb",
        ygg: "scrollbar_thumb",
        fallback: Some("selectedBg"),
    },
    ColorMapping {
        pi: "searchMatchBg",
        ygg: "search_match_bg",
        fallback: Some("selectedBg"),
    },
    ColorMapping {
        pi: "searchMatchText",
        ygg: "search_match_text",
        fallback: Some("text"),
    },
    ColorMapping {
        pi: "userMessageBg",
        ygg: "user_msg_bg",
        fallback: None,
    },
    ColorMapping {
        pi: "userMessageText",
        ygg: "user_msg_text",
        fallback: None,
    },
    ColorMapping {
        pi: "customMessageBg",
        ygg: "custom_msg_bg",
        fallback: None,
    },
    ColorMapping {
        pi: "customMessageText",
        ygg: "custom_msg_text",
        fallback: None,
    },
    ColorMapping {
        pi: "customMessageLabel",
        ygg: "custom_msg_label",
        fallback: None,
    },
    ColorMapping {
        pi: "toolPendingBg",
        ygg: "tool_pending_bg",
        fallback: None,
    },
    ColorMapping {
        pi: "toolSuccessBg",
        ygg: "tool_success_bg",
        fallback: None,
    },
    ColorMapping {
        pi: "toolErrorBg",
        ygg: "tool_error_bg",
        fallback: None,
    },
    ColorMapping {
        pi: "toolTitle",
        ygg: "tool_title",
        fallback: None,
    },
    ColorMapping {
        pi: "toolOutput",
        ygg: "tool_output",
        fallback: None,
    },
    ColorMapping {
        pi: "mdHeading",
        ygg: "md_heading",
        fallback: None,
    },
    ColorMapping {
        pi: "mdLink",
        ygg: "md_link",
        fallback: None,
    },
    ColorMapping {
        pi: "mdLinkUrl",
        ygg: "md_link_url",
        fallback: None,
    },
    ColorMapping {
        pi: "mdCode",
        ygg: "md_code",
        fallback: None,
    },
    ColorMapping {
        pi: "mdCodeBlock",
        ygg: "md_code_block",
        fallback: None,
    },
    ColorMapping {
        pi: "mdCodeBlockBorder",
        ygg: "md_code_border",
        fallback: None,
    },
    ColorMapping {
        pi: "mdQuote",
        ygg: "md_quote",
        fallback: None,
    },
    ColorMapping {
        pi: "mdQuoteBorder",
        ygg: "md_quote_border",
        fallback: None,
    },
    ColorMapping {
        pi: "mdHr",
        ygg: "md_hr",
        fallback: None,
    },
    ColorMapping {
        pi: "mdListBullet",
        ygg: "md_list_bullet",
        fallback: None,
    },
    ColorMapping {
        // Ygg carries diff status on the marker while source text retains its
        // syntax foreground, rather than painting every source grapheme green.
        pi: "toolDiffAdded",
        ygg: "diff_added_marker",
        fallback: None,
    },
    ColorMapping {
        pi: "toolDiffRemoved",
        ygg: "diff_removed_marker",
        fallback: None,
    },
    ColorMapping {
        pi: "toolDiffContext",
        ygg: "diff_context",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxComment",
        ygg: "syntax_comment",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxKeyword",
        ygg: "syntax_keyword",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxFunction",
        ygg: "syntax_function",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxVariable",
        ygg: "syntax_variable",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxString",
        ygg: "syntax_string",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxNumber",
        ygg: "syntax_number",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxType",
        ygg: "syntax_type",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxOperator",
        ygg: "syntax_operator",
        fallback: None,
    },
    ColorMapping {
        pi: "syntaxPunctuation",
        ygg: "syntax_punctuation",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingOff",
        ygg: "thinking_off",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingMinimal",
        ygg: "thinking_minimal",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingLow",
        ygg: "thinking_low",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingMedium",
        ygg: "thinking_medium",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingHigh",
        ygg: "thinking_high",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingXhigh",
        ygg: "thinking_xhigh",
        fallback: None,
    },
    ColorMapping {
        pi: "thinkingMax",
        ygg: "thinking_max",
        fallback: Some("thinkingXhigh"),
    },
    ColorMapping {
        pi: "bashMode",
        ygg: "bash_mode",
        fallback: None,
    },
];

const EXPORT_MAPPINGS: &[(&str, &str)] = &[
    ("pageBg", "export.page_bg"),
    ("cardBg", "export.card_bg"),
    ("infoBg", "export.info_bg"),
];

// Roles without a Pi analogue remain terminal-controlled or alias a compatible
// semantic foreground. In particular, no synthetic RGB surface is introduced.
const SAFE_YGG_TOKENS: &[(&str, &str)] = &[
    ("surface", "default"),
    ("overlay", "default"),
    ("raised", "default"),
    ("info", "accent"),
    ("assistant_msg_text", "foreground"),
    ("assistant_msg_bg", "default"),
    ("md_emphasis", "foreground"),
    ("md_strong", "foreground"),
    ("md_code_bg", "default"),
    ("md_code_inline_bg", "default"),
    ("md_quote_bg", "default"),
    ("diff_added", "default"),
    ("diff_removed", "default"),
    ("diff_added_bg", "default"),
    ("diff_removed_bg", "default"),
    ("diff_hunk", "border_focused"),
    ("diff_header", "foreground"),
    ("composer_border", "border_focused"),
    ("composer_bg", "default"),
    ("splash", "accent"),
    ("splash_box", "border"),
    // A selected Pi palette owns current chrome. Historical prompt provenance
    // remains independent because Ygg stores and renders the exact turn color.
    ("model.use_lab_color", "false"),
    ("model_accent", "accent"),
    ("model_assistant", "assistant_msg_text"),
];

/// Parse bounded Pi JSON bytes into Ygg's typed, non-executable theme schema.
///
/// This function performs no filesystem access. The caller remains responsible
/// for regular-file, no-follow, trust, and descriptor traversal guarantees.
pub(crate) fn parse_pi_theme_bytes(
    source: &[u8],
    source_name: &str,
) -> anyhow::Result<ParsedTheme> {
    check_size(source.len(), source_name)?;
    let source = std::str::from_utf8(source)
        .with_context(|| format!("Pi theme {source_name} is not UTF-8"))?;
    parse_pi_theme_str_bounded(source, source_name)
}

/// Parse a bounded Pi JSON string into Ygg's typed, non-executable theme schema.
pub(crate) fn parse_pi_theme_str(source: &str, source_name: &str) -> anyhow::Result<ParsedTheme> {
    check_size(source.len(), source_name)?;
    parse_pi_theme_str_bounded(source, source_name)
}

fn check_size(size: usize, source_name: &str) -> anyhow::Result<()> {
    if size as u64 > MAX_THEME_BYTES {
        bail!("Pi theme {source_name} is {size} bytes; the limit is {MAX_THEME_BYTES}");
    }
    Ok(())
}

fn parse_pi_theme_str_bounded(source: &str, source_name: &str) -> anyhow::Result<ParsedTheme> {
    // Pi's stripBom() accepts the ordinary UTF-8 BOM before strict JSON.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let document: PiThemeDocument = serde_json::from_str(source)
        .with_context(|| format!("invalid Pi {PI_THEME_VERSION} theme {source_name}"))?;
    convert_document(document, source_name)
}

fn convert_document(document: PiThemeDocument, source_name: &str) -> anyhow::Result<ParsedTheme> {
    validate_name(&document.name, source_name)?;
    validate_schema(document.schema.as_deref(), source_name)?;
    validate_variable_names(&document.vars, source_name)?;
    validate_color_names(&document.colors, source_name)?;
    if let Some(export) = &document.export {
        validate_export_names(export, source_name)?;
    }

    let mut resolver = VariableResolver::new(&document.vars, source_name);
    resolver.resolve_all_variables()?;

    let mut tokens = BTreeMap::new();
    for mapping in COLOR_MAPPINGS {
        let value = document
            .colors
            .get(mapping.pi)
            .or_else(|| {
                mapping
                    .fallback
                    .and_then(|fallback| document.colors.get(fallback))
            })
            .expect("required colors and optional fallbacks were validated");
        let resolved = resolver
            .resolve(value)
            .with_context(|| format!("Pi theme {source_name} colors.{}", mapping.pi))?;
        ensure_typed_color(&resolved, source_name, mapping.pi)?;
        tokens.insert(mapping.ygg.to_owned(), resolved);
    }

    if let Some(export) = &document.export {
        for &(pi, ygg) in EXPORT_MAPPINGS {
            if let Some(value) = export.get(pi) {
                let resolved = resolver
                    .resolve(value)
                    .with_context(|| format!("Pi theme {source_name} export.{pi}"))?;
                ensure_typed_color(&resolved, source_name, pi)?;
                tokens.insert(ygg.to_owned(), resolved);
            }
        }
    }

    for &(token, value) in SAFE_YGG_TOKENS {
        tokens.insert(token.to_owned(), value.to_owned());
    }

    Ok(ParsedTheme {
        metadata: ThemeMetadata {
            name: document.name,
            description: format!("Converted from Pi {PI_THEME_VERSION} JSON theme"),
            author: String::new(),
            version: "1".to_owned(),
            terminal: "any".to_owned(),
            // Pi dark/light palettes are already concrete. Adapting their RGB
            // values again would alter the selected compatible theme.
            adaptive: false,
        },
        tokens,
        roles: safe_semantic_roles(),
        glyphs: BTreeMap::new(),
        ascii_glyphs: BTreeMap::new(),
        surfaces: BTreeMap::new(),
        layout: ThemeLayout::default(),
    })
}

fn validate_name(name: &str, source_name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > MAX_THEME_NAME_BYTES {
        bail!("Pi theme {source_name} name must be 1 to {MAX_THEME_NAME_BYTES} bytes");
    }
    if name.contains('/') {
        bail!("Pi theme {source_name} name cannot contain '/'");
    }
    if name.chars().any(char::is_control) {
        bail!("Pi theme {source_name} name contains a control character");
    }
    Ok(())
}

fn validate_schema(schema: Option<&str>, source_name: &str) -> anyhow::Result<()> {
    let Some(schema) = schema else {
        return Ok(());
    };
    if schema.len() > MAX_SCHEMA_BYTES || schema.chars().any(char::is_control) {
        bail!("Pi theme {source_name} has an invalid $schema value");
    }
    if !matches!(schema, PI_SCHEMA_MAIN | PI_SCHEMA_TAG | PI_SCHEMA_REVISION) {
        bail!(
            "Pi theme {source_name} declares unsupported schema {schema:?}; expected Pi {PI_THEME_VERSION}"
        );
    }
    Ok(())
}

fn validate_variable_names(
    vars: &BTreeMap<String, PiColorValue>,
    source_name: &str,
) -> anyhow::Result<()> {
    if vars.len() > MAX_VARIABLES {
        bail!(
            "Pi theme {source_name} has {} variables; the limit is {MAX_VARIABLES}",
            vars.len()
        );
    }
    for name in vars.keys() {
        if name.is_empty()
            || name.len() > MAX_VARIABLE_NAME_BYTES
            || name.chars().any(char::is_control)
        {
            bail!("Pi theme {source_name} has invalid variable name {name:?}");
        }
    }
    Ok(())
}

fn validate_color_names(
    colors: &BTreeMap<String, PiColorValue>,
    source_name: &str,
) -> anyhow::Result<()> {
    let known = COLOR_MAPPINGS
        .iter()
        .map(|mapping| mapping.pi)
        .collect::<BTreeSet<_>>();
    let unknown = colors
        .keys()
        .filter(|name| !known.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        bail!(
            "Pi theme {source_name} has unknown Pi {PI_THEME_VERSION} color tokens: {}",
            quoted_list(&unknown)
        );
    }

    let missing = COLOR_MAPPINGS
        .iter()
        .filter(|mapping| mapping.fallback.is_none() && !colors.contains_key(mapping.pi))
        .map(|mapping| mapping.pi.to_owned())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "Pi theme {source_name} is missing required Pi {PI_THEME_VERSION} color tokens: {}",
            quoted_list(&missing)
        );
    }
    Ok(())
}

fn validate_export_names(
    export: &BTreeMap<String, PiColorValue>,
    source_name: &str,
) -> anyhow::Result<()> {
    let known = EXPORT_MAPPINGS
        .iter()
        .map(|(name, _)| *name)
        .collect::<BTreeSet<_>>();
    let unknown = export
        .keys()
        .filter(|name| !known.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        bail!(
            "Pi theme {source_name} has unknown export color tokens: {}",
            quoted_list(&unknown)
        );
    }
    Ok(())
}

fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn ensure_typed_color(value: &str, source_name: &str, token: &str) -> anyhow::Result<()> {
    if Color::parse(value).is_none() {
        bail!("Pi theme {source_name} token {token:?} did not resolve to a typed terminal color");
    }
    Ok(())
}

struct VariableResolver<'a> {
    vars: &'a BTreeMap<String, PiColorValue>,
    resolved: BTreeMap<String, String>,
    stack: Vec<String>,
    source_name: &'a str,
}

impl<'a> VariableResolver<'a> {
    fn new(vars: &'a BTreeMap<String, PiColorValue>, source_name: &'a str) -> Self {
        Self {
            vars,
            resolved: BTreeMap::new(),
            stack: Vec::new(),
            source_name,
        }
    }

    fn resolve_all_variables(&mut self) -> anyhow::Result<()> {
        for name in self.vars.keys() {
            self.resolve_variable(name)?;
        }
        Ok(())
    }

    fn resolve(&mut self, value: &PiColorValue) -> anyhow::Result<String> {
        match value {
            PiColorValue::Indexed(index) => {
                let Some(index) = index.as_f64().filter(|index| {
                    index.is_finite()
                        && index.fract() == 0.0
                        && (0.0..=f64::from(u8::MAX)).contains(index)
                }) else {
                    bail!(
                        "Pi theme {} palette indices must be integers from 0 through 255",
                        self.source_name
                    );
                };
                Ok(format!("index:{}", index as u8))
            }
            PiColorValue::Text(value) => self.resolve_text(value),
        }
    }

    fn resolve_text(&mut self, value: &str) -> anyhow::Result<String> {
        if value.len() > MAX_COLOR_VALUE_BYTES || value.chars().any(char::is_control) {
            bail!(
                "Pi theme {} has an invalid or oversized color value",
                self.source_name
            );
        }
        if value.is_empty() {
            return Ok("default".to_owned());
        }
        if let Some(hex) = value.strip_prefix('#') {
            if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!(
                    "Pi theme {} has invalid hex color {value:?}; expected #RRGGBB",
                    self.source_name
                );
            }
            return Ok(format!("#{}", hex.to_ascii_lowercase()));
        }
        self.resolve_variable(value)
    }

    fn resolve_variable(&mut self, name: &str) -> anyhow::Result<String> {
        if let Some(value) = self.resolved.get(name) {
            return Ok(value.clone());
        }
        if let Some(start) = self.stack.iter().position(|entry| entry == name) {
            let mut cycle = self.stack[start..].to_vec();
            cycle.push(name.to_owned());
            bail!(
                "Pi theme {} has a variable reference cycle: {}",
                self.source_name,
                cycle.join(" -> ")
            );
        }
        let value = self.vars.get(name).cloned().with_context(|| {
            format!(
                "Pi theme {} variable reference {name:?} was not found",
                self.source_name
            )
        })?;
        self.stack.push(name.to_owned());
        let result = self.resolve(&value);
        self.stack.pop();
        let result = result?;
        self.resolved.insert(name.to_owned(), result.clone());
        Ok(result)
    }
}

fn role(
    foreground: Option<&str>,
    background: Option<&str>,
    bold: bool,
    italic: bool,
) -> RoleStyleSpec {
    RoleStyleSpec {
        foreground: foreground.map(str::to_owned),
        background: background.map(str::to_owned),
        bold: bold.then_some(true),
        italic: italic.then_some(true),
        ..RoleStyleSpec::default()
    }
}

fn safe_semantic_roles() -> BTreeMap<String, RoleStyleSpec> {
    let mut roles = BTreeMap::new();
    let mut insert = |name: &str, style| {
        roles.insert(name.to_owned(), style);
    };

    insert(
        "notification",
        role(Some("custom_msg_label"), None, false, true),
    );
    insert("confirmation", role(Some("accent"), None, true, false));
    insert("extension.sparkle", role(Some("accent"), None, false, true));
    insert("extension.status", role(Some("muted"), None, false, true));
    insert("extension.header", role(Some("accent"), None, true, false));

    insert(
        "surface.user",
        role(Some("user_msg_text"), Some("user_msg_bg"), false, false),
    );
    insert(
        "surface.user.border",
        role(Some("border"), None, false, false),
    );
    insert(
        "surface.user.label",
        role(Some("accent"), None, true, false),
    );

    insert(
        "surface.assistant",
        role(Some("assistant_msg_text"), None, false, false),
    );
    insert(
        "surface.assistant.border",
        role(Some("border_idle"), None, false, false),
    );
    insert(
        "surface.assistant.label",
        role(Some("accent"), None, true, false),
    );

    insert(
        "surface.reasoning",
        role(Some("thinking_text"), None, false, true),
    );
    insert(
        "surface.reasoning.border",
        role(Some("border_idle"), None, false, false),
    );
    insert(
        "surface.reasoning.label",
        role(Some("muted"), None, false, true),
    );

    // Pi has three status-specific tool backgrounds. A single static Ygg
    // surface cannot safely choose one, so status colors remain typed tokens
    // and the generic tool surface stays on the terminal canvas.
    insert(
        "surface.tool",
        role(Some("tool_output"), None, false, false),
    );
    insert(
        "surface.tool.border",
        role(Some("border"), None, false, false),
    );
    insert(
        "surface.tool.label",
        role(Some("tool_title"), None, true, false),
    );

    insert(
        "surface.shell",
        role(Some("foreground"), None, false, false),
    );
    insert(
        "surface.shell.border",
        role(Some("bash_mode"), None, false, false),
    );
    insert(
        "surface.shell.label",
        role(Some("bash_mode"), None, true, false),
    );

    insert(
        "surface.notice",
        role(Some("custom_msg_text"), Some("custom_msg_bg"), false, false),
    );
    insert(
        "surface.notice.border",
        role(Some("custom_msg_label"), None, false, false),
    );
    insert(
        "surface.notice.label",
        role(Some("custom_msg_label"), None, true, false),
    );

    insert("surface.outcome", role(Some("muted"), None, false, false));
    insert(
        "surface.outcome.border",
        role(Some("border"), None, false, false),
    );
    insert(
        "surface.outcome.label",
        role(Some("accent"), None, true, false),
    );

    insert("surface.compaction", role(Some("muted"), None, false, true));
    insert(
        "surface.compaction.border",
        role(Some("border_idle"), None, false, false),
    );
    insert(
        "surface.compaction.label",
        role(Some("muted"), None, false, true),
    );

    roles
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dark and light are copied verbatim from the pinned Pi revision. The custom
    // fixture is the pinned documentation example with alias/index/default
    // cases made explicit. Keeping them local makes the oracle hermetic.
    const PI_DARK_0_84_4: &str = r###"{
	"$schema": "https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/src/modes/interactive/theme/theme-schema.json",
	"name": "dark",
	"vars": {
		"cyan": "#00d7ff",
		"blue": "#5f87ff",
		"green": "#b5bd68",
		"red": "#cc6666",
		"yellow": "#ffff00",
		"text": "#d4d4d4",
		"gray": "#808080",
		"dimGray": "#666666",
		"darkGray": "#505050",
		"accent": "#8abeb7",
		"selectedBg": "#3a3a4a",
		"userMsgBg": "#343541",
		"toolPendingBg": "#282832",
		"toolSuccessBg": "#283228",
		"toolErrorBg": "#3c2828",
		"customMsgBg": "#2d2838"
	},
	"colors": {
		"accent": "accent",
		"border": "blue",
		"borderAccent": "cyan",
		"borderMuted": "darkGray",
		"success": "green",
		"error": "red",
		"warning": "yellow",
		"muted": "gray",
		"dim": "dimGray",
		"text": "text",
		"thinkingText": "gray",

		"selectedBg": "selectedBg",
		"scrollbarThumb": "selectedBg",
		"searchMatchBg": "selectedBg",
		"searchMatchText": "text",
		"userMessageBg": "userMsgBg",
		"userMessageText": "text",
		"customMessageBg": "customMsgBg",
		"customMessageText": "text",
		"customMessageLabel": "#9575cd",
		"toolPendingBg": "toolPendingBg",
		"toolSuccessBg": "toolSuccessBg",
		"toolErrorBg": "toolErrorBg",
		"toolTitle": "text",
		"toolOutput": "gray",

		"mdHeading": "#f0c674",
		"mdLink": "#81a2be",
		"mdLinkUrl": "dimGray",
		"mdCode": "accent",
		"mdCodeBlock": "green",
		"mdCodeBlockBorder": "gray",
		"mdQuote": "gray",
		"mdQuoteBorder": "gray",
		"mdHr": "gray",
		"mdListBullet": "accent",

		"toolDiffAdded": "green",
		"toolDiffRemoved": "red",
		"toolDiffContext": "gray",

		"syntaxComment": "#6A9955",
		"syntaxKeyword": "#569CD6",
		"syntaxFunction": "#DCDCAA",
		"syntaxVariable": "#9CDCFE",
		"syntaxString": "#CE9178",
		"syntaxNumber": "#B5CEA8",
		"syntaxType": "#4EC9B0",
		"syntaxOperator": "#D4D4D4",
		"syntaxPunctuation": "#D4D4D4",

		"thinkingOff": "darkGray",
		"thinkingMinimal": "#6e6e6e",
		"thinkingLow": "#5f87af",
		"thinkingMedium": "#81a2be",
		"thinkingHigh": "#b294bb",
		"thinkingXhigh": "#d183e8",
		"thinkingMax": "#ff5fff",

		"bashMode": "green"
	},
	"export": {
		"pageBg": "#18181e",
		"cardBg": "#1e1e24",
		"infoBg": "#3c3728"
	}
}"###;
    const PI_LIGHT_0_84_4: &str = r###"{
	"$schema": "https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/src/modes/interactive/theme/theme-schema.json",
	"name": "light",
	"vars": {
		"teal": "#5a8080",
		"blue": "#547da7",
		"green": "#588458",
		"red": "#aa5555",
		"yellow": "#9a7326",
		"text": "#1f2328",
		"mediumGray": "#6c6c6c",
		"dimGray": "#767676",
		"lightGray": "#b0b0b0",
		"selectedBg": "#d0d0e0",
		"userMsgBg": "#e8e8e8",
		"toolPendingBg": "#e8e8f0",
		"toolSuccessBg": "#e8f0e8",
		"toolErrorBg": "#f0e8e8",
		"customMsgBg": "#ede7f6"
	},
	"colors": {
		"accent": "teal",
		"border": "blue",
		"borderAccent": "teal",
		"borderMuted": "lightGray",
		"success": "green",
		"error": "red",
		"warning": "yellow",
		"muted": "mediumGray",
		"dim": "dimGray",
		"text": "text",
		"thinkingText": "mediumGray",

		"selectedBg": "selectedBg",
		"scrollbarThumb": "selectedBg",
		"searchMatchBg": "selectedBg",
		"searchMatchText": "text",
		"userMessageBg": "userMsgBg",
		"userMessageText": "text",
		"customMessageBg": "customMsgBg",
		"customMessageText": "text",
		"customMessageLabel": "#7e57c2",
		"toolPendingBg": "toolPendingBg",
		"toolSuccessBg": "toolSuccessBg",
		"toolErrorBg": "toolErrorBg",
		"toolTitle": "text",
		"toolOutput": "mediumGray",

		"mdHeading": "yellow",
		"mdLink": "blue",
		"mdLinkUrl": "dimGray",
		"mdCode": "teal",
		"mdCodeBlock": "green",
		"mdCodeBlockBorder": "mediumGray",
		"mdQuote": "mediumGray",
		"mdQuoteBorder": "mediumGray",
		"mdHr": "mediumGray",
		"mdListBullet": "green",

		"toolDiffAdded": "green",
		"toolDiffRemoved": "red",
		"toolDiffContext": "mediumGray",

		"syntaxComment": "#008000",
		"syntaxKeyword": "#0000FF",
		"syntaxFunction": "#795E26",
		"syntaxVariable": "#001080",
		"syntaxString": "#A31515",
		"syntaxNumber": "#098658",
		"syntaxType": "#267F99",
		"syntaxOperator": "#000000",
		"syntaxPunctuation": "#000000",

		"thinkingOff": "lightGray",
		"thinkingMinimal": "#767676",
		"thinkingLow": "blue",
		"thinkingMedium": "teal",
		"thinkingHigh": "#875f87",
		"thinkingXhigh": "#8b008b",
		"thinkingMax": "#af005f",

		"bashMode": "green"
	},
	"export": {
		"pageBg": "#f8f8f8",
		"cardBg": "#ffffff",
		"infoBg": "#fffae6"
	}
}"###;
    const PI_CUSTOM_0_84_4: &str = r###"{
	"$schema": "https://raw.githubusercontent.com/earendil-works/pi/b79e4cc834970cca69daebffab7df1da7d1e52c4/packages/coding-agent/src/modes/interactive/theme/theme-schema.json",
	"name": "adapter-custom",
	"vars": {
		"primary": "secondary",
		"secondary": "paletteIndex",
		"paletteIndex": 39,
		"selected": "#222233",
		"page": "#101010",
		"card": 236
	},
	"colors": {
		"accent": "primary",
		"border": "primary",
		"borderAccent": "primary",
		"borderMuted": "secondary",
		"success": "#00ff00",
		"error": "#ff0000",
		"warning": "#ffff00",
		"muted": "secondary",
		"dim": 240,
		"text": "",
		"thinkingText": "secondary",
		"selectedBg": "selected",
		"userMessageBg": "#2d2d30",
		"userMessageText": "",
		"customMessageBg": "#2d2d30",
		"customMessageText": "",
		"customMessageLabel": "primary",
		"toolPendingBg": "#1e1e2e",
		"toolSuccessBg": "#1e2e1e",
		"toolErrorBg": "#2e1e1e",
		"toolTitle": "primary",
		"toolOutput": "",
		"mdHeading": "#ffaa00",
		"mdLink": "primary",
		"mdLinkUrl": "secondary",
		"mdCode": "#00ffff",
		"mdCodeBlock": "",
		"mdCodeBlockBorder": "secondary",
		"mdQuote": "secondary",
		"mdQuoteBorder": "secondary",
		"mdHr": "secondary",
		"mdListBullet": "#00ffff",
		"toolDiffAdded": "#00ff00",
		"toolDiffRemoved": "#ff0000",
		"toolDiffContext": "secondary",
		"syntaxComment": "secondary",
		"syntaxKeyword": "primary",
		"syntaxFunction": "#00aaff",
		"syntaxVariable": "#ffaa00",
		"syntaxString": "#00ff00",
		"syntaxNumber": "#ff00ff",
		"syntaxType": "#00aaff",
		"syntaxOperator": "primary",
		"syntaxPunctuation": "secondary",
		"thinkingOff": "secondary",
		"thinkingMinimal": "primary",
		"thinkingLow": "#00aaff",
		"thinkingMedium": "#00ffff",
		"thinkingHigh": "#ff00ff",
		"thinkingXhigh": "#ff0088",
		"bashMode": "#ffaa00"
	},
	"export": {
		"pageBg": "page",
		"cardBg": "card",
		"infoBg": ""
	}
}"###;

    fn mutate_dark(mutator: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value: serde_json::Value = serde_json::from_str(PI_DARK_0_84_4).unwrap();
        mutator(&mut value);
        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn pinned_default_dark_fixture_maps_every_pi_color_and_safe_ygg_roles() {
        let parsed = parse_pi_theme_bytes(PI_DARK_0_84_4.as_bytes(), "dark.json").unwrap();
        assert_eq!(parsed.metadata.name, "dark");
        assert_eq!(parsed.metadata.terminal, "any");
        assert!(!parsed.metadata.adaptive);
        assert_eq!(parsed.tokens["accent"], "#8abeb7");
        assert_eq!(parsed.tokens["border"], "#5f87ff");
        assert_eq!(parsed.tokens["border_focused"], "#00d7ff");
        assert_eq!(parsed.tokens["user_msg_bg"], "#343541");
        assert_eq!(parsed.tokens["diff_added_marker"], "#b5bd68");
        assert_eq!(parsed.tokens["scrollbar_thumb"], "#3a3a4a");
        assert_eq!(parsed.tokens["thinking_max"], "#ff5fff");
        assert_eq!(parsed.tokens["model.use_lab_color"], "false");
        assert_eq!(parsed.tokens["model_accent"], "accent");
        assert_eq!(
            parsed.roles["surface.user"].background.as_deref(),
            Some("user_msg_bg")
        );
        for mapping in COLOR_MAPPINGS {
            assert!(
                parsed.tokens.contains_key(mapping.ygg),
                "missing mapped token for {}",
                mapping.pi
            );
            assert!(Color::parse(&parsed.tokens[mapping.ygg]).is_some());
        }
        assert!(parsed.glyphs.is_empty());
        assert!(parsed.surfaces.is_empty());
        assert_eq!(parsed.layout, ThemeLayout::default());
    }

    #[test]
    fn pinned_light_fixture_keeps_its_resolved_palette() {
        let parsed = parse_pi_theme_str(PI_LIGHT_0_84_4, "light.json").unwrap();
        assert_eq!(parsed.metadata.name, "light");
        assert_eq!(parsed.tokens["accent"], "#5a8080");
        assert_eq!(parsed.tokens["foreground"], "#1f2328");
        assert_eq!(parsed.tokens["user_msg_bg"], "#e8e8e8");
        assert_eq!(parsed.tokens["md_heading"], "#9a7326");
        assert_eq!(parsed.tokens["syntax_keyword"], "#0000ff");
        assert_eq!(parsed.tokens["search_match_text"], "#1f2328");
    }

    #[test]
    fn pinned_custom_fixture_resolves_chains_indices_defaults_and_export_colors() {
        let parsed = parse_pi_theme_str(PI_CUSTOM_0_84_4, "custom.json").unwrap();
        assert_eq!(parsed.metadata.name, "adapter-custom");
        assert_eq!(parsed.tokens["accent"], "index:39");
        assert_eq!(parsed.tokens["border_focused"], "index:39");
        assert_eq!(parsed.tokens["foreground"], "default");
        assert_eq!(parsed.tokens["tool_output"], "default");
        assert_eq!(parsed.tokens["scrollbar_thumb"], "#222233");
        assert_eq!(parsed.tokens["search_match_bg"], "#222233");
        assert_eq!(parsed.tokens["search_match_text"], "default");
        assert_eq!(parsed.tokens["thinking_max"], "#ff0088");
        assert_eq!(parsed.tokens["export.page_bg"], "#101010");
        assert_eq!(parsed.tokens["export.card_bg"], "index:236");
        assert_eq!(parsed.tokens["export.info_bg"], "default");
        assert_eq!(parsed.tokens["assistant_msg_bg"], "default");
        assert_eq!(parsed.tokens["diff_added_bg"], "default");
    }

    #[test]
    fn integer_valued_json_numbers_match_pis_integer_schema() {
        for (label, number) in [
            ("decimal", "39.0"),
            ("exponent", "39e0"),
            ("negative-zero", "-0.0"),
        ] {
            let number = serde_json::from_str(number).unwrap();
            let source = mutate_dark(|value| value["colors"]["accent"] = number);
            let parsed = parse_pi_theme_str(&source, label).unwrap();
            let expected = if label == "negative-zero" {
                "index:0"
            } else {
                "index:39"
            };
            assert_eq!(parsed.tokens["accent"], expected);
        }
    }

    #[test]
    fn exact_vocabulary_has_51_required_and_four_compatible_optional_tokens() {
        assert_eq!(COLOR_MAPPINGS.len(), 55);
        assert_eq!(
            COLOR_MAPPINGS
                .iter()
                .filter(|mapping| mapping.fallback.is_none())
                .count(),
            51
        );
        let pi = COLOR_MAPPINGS
            .iter()
            .map(|mapping| mapping.pi)
            .collect::<BTreeSet<_>>();
        let ygg = COLOR_MAPPINGS
            .iter()
            .map(|mapping| mapping.ygg)
            .collect::<BTreeSet<_>>();
        assert_eq!(pi.len(), COLOR_MAPPINGS.len());
        assert_eq!(ygg.len(), COLOR_MAPPINGS.len());
        assert_eq!(
            COLOR_MAPPINGS
                .iter()
                .filter_map(|mapping| mapping.fallback)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["selectedBg", "text", "thinkingXhigh"])
        );
    }

    #[test]
    fn malformed_unknown_and_future_json_fail_closed() {
        let malformed = parse_pi_theme_str("{not json", "malformed.json")
            .unwrap_err()
            .to_string();
        assert!(malformed.contains("invalid Pi 0.84.4 theme"), "{malformed}");

        let unknown_root = mutate_dark(|value| {
            value["futureField"] = serde_json::json!(true);
        });
        let error = format!(
            "{:#}",
            parse_pi_theme_str(&unknown_root, "future-root.json").unwrap_err()
        );
        assert!(error.contains("unknown field"), "{error}");

        let unknown_color = mutate_dark(|value| {
            value["colors"]["futureAccent"] = serde_json::json!("#010203");
        });
        let error = parse_pi_theme_str(&unknown_color, "future-color.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown Pi 0.84.4 color tokens"), "{error}");

        let future_schema = mutate_dark(|value| {
            value["$schema"] = serde_json::json!(
                "https://raw.githubusercontent.com/earendil-works/pi/v0.85.0/packages/coding-agent/src/modes/interactive/theme/theme-schema.json"
            );
        });
        let error = parse_pi_theme_str(&future_schema, "future-schema.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported schema"), "{error}");

        for field in ["$schema", "export"] {
            let null_field = mutate_dark(|value| value[field] = serde_json::Value::Null);
            let error = format!(
                "{:#}",
                parse_pi_theme_str(&null_field, "null-optional-field.json").unwrap_err()
            );
            assert!(error.contains("invalid type: null"), "{field}: {error}");
        }
    }

    #[test]
    fn malformed_color_forms_and_references_are_rejected() {
        for (label, replacement, expected) in [
            ("short-hex", serde_json::json!("#abc"), "expected #RRGGBB"),
            ("bad-hex", serde_json::json!("#12zz00"), "expected #RRGGBB"),
            (
                "large-index",
                serde_json::json!(256),
                "integers from 0 through 255",
            ),
            (
                "float-index",
                serde_json::json!(2.5),
                "integers from 0 through 255",
            ),
            ("boolean", serde_json::json!(true), "did not match"),
        ] {
            let source = mutate_dark(|value| value["colors"]["accent"] = replacement);
            let error = format!("{:#}", parse_pi_theme_str(&source, label).unwrap_err());
            assert!(error.contains(expected), "{label}: {error}");
        }

        let missing_reference = mutate_dark(|value| {
            value["colors"]["accent"] = serde_json::json!("doesNotExist");
        });
        let error = format!(
            "{:#}",
            parse_pi_theme_str(&missing_reference, "missing-reference.json").unwrap_err()
        );
        assert!(error.contains("was not found"), "{error}");
    }

    #[test]
    fn variable_cycles_are_rejected_even_when_unused() {
        let source = mutate_dark(|value| {
            value["vars"]["cycleA"] = serde_json::json!("cycleB");
            value["vars"]["cycleB"] = serde_json::json!("cycleA");
        });
        let error = parse_pi_theme_str(&source, "cycle.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("variable reference cycle"), "{error}");
        assert!(error.contains("cycleA -> cycleB -> cycleA"), "{error}");
    }

    #[test]
    fn oversized_and_non_utf8_byte_inputs_are_rejected_before_json() {
        let oversized = vec![b' '; MAX_THEME_BYTES as usize + 1];
        let error = parse_pi_theme_bytes(&oversized, "oversized.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("the limit is 262144"), "{error}");

        let error = parse_pi_theme_bytes(&[0xff], "non-utf8.json")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not UTF-8"), "{error}");
    }

    #[test]
    fn missing_required_tokens_are_reported_deterministically() {
        let source = mutate_dark(|value| {
            value["colors"].as_object_mut().unwrap().remove("bashMode");
            value["colors"].as_object_mut().unwrap().remove("accent");
        });
        let error = parse_pi_theme_str(&source, "missing.json")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("missing required Pi 0.84.4 color tokens"),
            "{error}"
        );
        assert!(error.contains("\"accent\""), "{error}");
        assert!(error.contains("\"bashMode\""), "{error}");
    }

    #[test]
    fn optional_tokens_use_exact_pi_0_84_4_fallbacks() {
        let source = mutate_dark(|value| {
            let colors = value["colors"].as_object_mut().unwrap();
            colors.remove("scrollbarThumb");
            colors.remove("searchMatchBg");
            colors.remove("searchMatchText");
            colors.remove("thinkingMax");
        });
        let parsed = parse_pi_theme_str(&source, "legacy-compatible.json").unwrap();
        assert_eq!(
            parsed.tokens["scrollbar_thumb"],
            parsed.tokens["selected_bg"]
        );
        assert_eq!(
            parsed.tokens["search_match_bg"],
            parsed.tokens["selected_bg"]
        );
        assert_eq!(
            parsed.tokens["search_match_text"],
            parsed.tokens["foreground"]
        );
        assert_eq!(
            parsed.tokens["thinking_max"],
            parsed.tokens["thinking_xhigh"]
        );
    }

    #[test]
    fn resolved_palette_preserves_sexy_tui_no_color_fallbacks() {
        let parsed = parse_pi_theme_str(PI_CUSTOM_0_84_4, "plain-custom.json").unwrap();
        let mut compiled = sexy_tui_rs::theme::Theme::load_with_capabilities(
            None,
            sexy_tui_rs::TerminalCapabilities::plain(),
        );
        for (token, value) in &parsed.tokens {
            compiled.override_token(token, value);
        }
        for token in parsed
            .tokens
            .keys()
            .filter(|token| token.as_str() != "model.use_lab_color")
        {
            assert!(
                compiled.resolve_color(token).is_some(),
                "unresolvable converted color token {token:?}"
            );
        }
        assert_eq!(compiled.resolve::<bool>("model.use_lab_color"), Some(false));
        assert_eq!(compiled.fg("accent", "accent probe"), "accent probe");
        assert_eq!(compiled.bg("user_msg_bg", "surface probe"), "surface probe");
        assert_eq!(compiled.fg("foreground", "default probe"), "default probe");
    }

    #[test]
    fn ordinary_utf8_bom_is_accepted_like_pis_strip_bom() {
        let mut source = vec![0xef, 0xbb, 0xbf];
        source.extend_from_slice(PI_DARK_0_84_4.as_bytes());
        let parsed = parse_pi_theme_bytes(&source, "bom-dark.json").unwrap();
        assert_eq!(parsed.metadata.name, "dark");
    }
}
