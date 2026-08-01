//! Bounded, inert theme projections for graphical clients.
//!
//! The trusted host resolves Ygg theme resources. Clients receive data tokens,
//! never CSS, JavaScript, URLs, font files, or filesystem paths.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    validate_public_text, ProtocolValidation, ThemeId, ValidationError, MAX_PUBLIC_TEXT_BYTES,
};

const MAX_THEME_NAME_BYTES: usize = 128;
const MAX_THEME_TOKENS: usize = 256;
const MAX_SEMANTIC_ROLE_BYTES: usize = 96;

/// Provenance of a host-parsed theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeSourceClass {
    /// Compiled Ygg fallback.
    Bundled,
    /// User-level Ygg theme resource.
    Global,
    /// Trusted project theme resource.
    Project,
    /// Explicit trusted theme directory.
    Explicit,
}

/// Host-observed light/dark variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ColorScheme {
    /// Light canvas.
    Light,
    /// Dark canvas.
    Dark,
    /// Unknown canvas; clients use their documented accessible fallback.
    Unknown,
}

/// Theme layout density.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeDensity {
    /// Dense operational layout.
    Compact,
    /// Default layout.
    Comfortable,
    /// More editorial whitespace.
    Airy,
}

/// Bounded motion preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeMotion {
    /// Normal state transitions.
    Full,
    /// Essential state changes only.
    Reduced,
    /// No theme-requested motion.
    None,
}

/// A resolved color value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ThemeColor {
    /// Renderer-owned accessible fallback.
    Default,
    /// Exact sRGB color.
    Rgb {
        /// Red channel.
        red: u8,
        /// Green channel.
        green: u8,
        /// Blue channel.
        blue: u8,
    },
    /// ANSI palette index retained for clients with that palette.
    Ansi {
        /// Palette entry.
        index: u8,
    },
}

/// Validated semantic role identifier, including namespaced `extension.*`
/// roles. Unknown roles remain inert and render with a neutral fallback.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SemanticRole(String);

impl SemanticRole {
    /// Constructs a bounded lowercase semantic role.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_SEMANTIC_ROLE_BYTES {
            return Err(format!(
                "semantic role must contain 1..={MAX_SEMANTIC_ROLE_BYTES} bytes"
            ));
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        }) {
            return Err("semantic role must be lowercase ASCII".into());
        }
        Ok(Self(value))
    }

    /// Returns the role spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SemanticRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Resolved style for one semantic role.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ThemeRoleStyle {
    /// Foreground color-token name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    /// Background color-token name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Strong emphasis.
    pub bold: bool,
    /// Quiet emphasis.
    pub dim: bool,
    /// Italic emphasis.
    pub italic: bool,
    /// Underline emphasis.
    pub underline: bool,
    /// Strike-through emphasis.
    pub strikethrough: bool,
}

/// Safe typography tokens.
///
/// Families are host-selected identifiers or installed-family display names,
/// never CSS, URLs, or file paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThemeTypography {
    /// Body family identifier.
    pub body_family: String,
    /// Monospace family identifier.
    pub mono_family: String,
    /// Base body size in logical pixels before accessibility scaling.
    pub body_size: u16,
    /// Display/body ratio in thousandths.
    pub display_ratio_milli: u16,
}

/// Complete bounded theme emitted by the trusted Rust host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThemeDto {
    /// Display name.
    pub name: String,
    /// Resource provenance.
    pub source: ThemeSourceClass,
    /// Monotonic host theme revision.
    pub revision: u64,
    /// Resolved variant.
    pub scheme: ColorScheme,
    /// Layout density.
    pub density: ThemeDensity,
    /// Motion level.
    pub motion: ThemeMotion,
    /// Shared typography tokens.
    pub typography: ThemeTypography,
    /// Resolved named colors.
    pub colors: BTreeMap<String, ThemeColor>,
    /// Resolved semantic styles.
    pub roles: BTreeMap<SemanticRole, ThemeRoleStyle>,
}

/// Selectable theme catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThemeOption {
    /// Stable theme identity.
    pub id: ThemeId,
    /// Complete safe theme projection.
    pub theme: ThemeDto,
}

impl ProtocolValidation for ThemeOption {
    fn validate(&self) -> Result<(), ValidationError> {
        self.theme.validate()
    }
}

impl ProtocolValidation for ThemeDto {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("theme.name", &self.name, MAX_THEME_NAME_BYTES, false)?;
        if self.name.trim().is_empty() {
            return Err(ValidationError::new(
                "theme.name",
                "must not be empty or whitespace",
            ));
        }
        if self.colors.len() > MAX_THEME_TOKENS || self.roles.len() > MAX_THEME_TOKENS {
            return Err(ValidationError::new(
                "theme",
                format!("exceeds the {MAX_THEME_TOKENS}-entry token limit"),
            ));
        }
        validate_family("theme.typography.body_family", &self.typography.body_family)?;
        validate_family("theme.typography.mono_family", &self.typography.mono_family)?;
        if !(12..=32).contains(&self.typography.body_size) {
            return Err(ValidationError::new(
                "theme.typography.body_size",
                "must be between 12 and 32 logical pixels",
            ));
        }
        if !(1000..=1600).contains(&self.typography.display_ratio_milli) {
            return Err(ValidationError::new(
                "theme.typography.display_ratio_milli",
                "must be between 1000 and 1600",
            ));
        }
        for token in self.colors.keys() {
            validate_token_name(token)?;
        }
        for style in self.roles.values() {
            if let Some(token) = &style.foreground {
                validate_token_reference("theme.role.foreground", token, &self.colors)?;
            }
            if let Some(token) = &style.background {
                validate_token_reference("theme.role.background", token, &self.colors)?;
            }
        }
        Ok(())
    }
}

fn validate_family(field: &'static str, value: &str) -> Result<(), ValidationError> {
    validate_public_text(field, value, 128, false)?;
    let lower = value.to_ascii_lowercase();
    if value.trim().is_empty()
        || lower.contains("url(")
        || lower.contains("data:")
        || lower.contains("javascript:")
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(ValidationError::new(
            field,
            "must be a safe family identifier, not a URL or path",
        ));
    }
    Ok(())
}

fn validate_token_name(token: &str) -> Result<(), ValidationError> {
    validate_public_text("theme.color.name", token, 64, false)?;
    if token.is_empty()
        || !token.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(ValidationError::new(
            "theme.color.name",
            "must be a lowercase semantic token",
        ));
    }
    Ok(())
}

fn validate_token_reference(
    field: &'static str,
    token: &str,
    colors: &BTreeMap<String, ThemeColor>,
) -> Result<(), ValidationError> {
    validate_public_text(field, token, MAX_PUBLIC_TEXT_BYTES.min(64), false)?;
    if token != "default" && !colors.contains_key(token) {
        return Err(ValidationError::new(
            field,
            format!("references unknown color token {token:?}"),
        ));
    }
    Ok(())
}
