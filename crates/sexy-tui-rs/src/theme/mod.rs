//! Three-layer semantic theme resolution.
//!
//! Built-in terminal-neutral defaults are overlaid by TOML values and then by
//! runtime overrides. The same theme quantizes itself for the active terminal
//! capability profile.

pub mod capability;
pub mod config;
pub mod palette;
pub mod tokens;

use std::collections::HashMap;

use crate::capabilities::{ColorDepth, SupportLevel, TerminalCapabilities};
use crate::style::{BlockRole, BlockStyle, Color, TextAttributes, TextRole, TextStyle};

/// Resolved semantic theme.
#[derive(Debug, Clone)]
pub struct Theme {
    values: HashMap<String, String>,
    overrides: HashMap<String, String>,
    style_overrides: HashMap<TextRole, TextStyle>,
    block_overrides: HashMap<BlockRole, BlockStyle>,
    config_path: Option<String>,
    tier: capability::CapabilityTier,
    capabilities: TerminalCapabilities,
    revision: u64,
}

impl Theme {
    /// Construct from a legacy capability tier.
    pub fn new(tier: capability::CapabilityTier) -> Self {
        Self::from_capabilities(tier.capabilities(), tier)
    }

    /// Construct directly from the central capability model.
    pub fn with_capabilities(capabilities: TerminalCapabilities) -> Self {
        let tier = if capabilities.color_depth == ColorDepth::TrueColor {
            capability::CapabilityTier::TrueColor
        } else {
            capability::CapabilityTier::Baseline
        };
        Self::from_capabilities(capabilities, tier)
    }

    fn from_capabilities(
        capabilities: TerminalCapabilities,
        tier: capability::CapabilityTier,
    ) -> Self {
        let mut values = HashMap::new();
        tokens::apply_defaults(&mut values);
        Self {
            values,
            overrides: HashMap::new(),
            style_overrides: HashMap::new(),
            block_overrides: HashMap::new(),
            config_path: None,
            tier,
            capabilities,
            revision: 0,
        }
    }

    /// Load built-ins plus a TOML file.
    pub fn load(config_path: Option<&str>, tier: capability::CapabilityTier) -> Self {
        let mut theme = Self::new(tier);
        theme.config_path = config_path.map(str::to_owned);
        if let Some(path) = config_path {
            config::load_toml(path, &mut theme.values);
        }
        theme
    }

    /// Load with an explicit terminal profile.
    pub fn load_with_capabilities(
        config_path: Option<&str>,
        capabilities: TerminalCapabilities,
    ) -> Self {
        let mut theme = Self::with_capabilities(capabilities);
        theme.config_path = config_path.map(str::to_owned);
        if let Some(path) = config_path {
            config::load_toml(path, &mut theme.values);
        }
        theme
    }

    pub const fn capabilities(&self) -> TerminalCapabilities {
        self.capabilities
    }

    pub fn set_capabilities(&mut self, capabilities: TerminalCapabilities) {
        self.capabilities = capabilities;
        self.tier = if capabilities.kitty_graphics && capabilities.synchronized_output {
            capability::CapabilityTier::KittyProtocol
        } else if capabilities.nerd_font {
            capability::CapabilityTier::NerdFont
        } else if capabilities.color_depth == ColorDepth::TrueColor {
            capability::CapabilityTier::TrueColor
        } else {
            capability::CapabilityTier::Baseline
        };
        self.revision = self.revision.saturating_add(1);
    }

    /// Reload the configured TOML layer while preserving runtime overrides.
    pub fn reload(&mut self) {
        self.values.clear();
        tokens::apply_defaults(&mut self.values);
        if let Some(path) = self.config_path.as_deref() {
            config::load_toml(path, &mut self.values);
        }
        self.revision = self.revision.saturating_add(1);
    }

    pub fn config_path(&self) -> Option<&str> {
        self.config_path.as_deref()
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    fn resolve_value(&self, token: &str) -> Option<String> {
        self.overrides
            .get(token)
            .or_else(|| self.values.get(token))
            .cloned()
    }

    /// Resolve token aliases with a bounded cycle guard.
    pub fn resolve_color(&self, token: &str) -> Option<Color> {
        let mut value = self
            .resolve_value(token)
            .unwrap_or_else(|| token.to_owned());
        for _ in 0..8 {
            if let Some(color) = Color::parse(&value) {
                return Some(color);
            }
            let next = self.resolve_value(&value)?;
            if next == value {
                return None;
            }
            value = next;
        }
        None
    }

    /// Resolve a semantic text role.
    pub fn style(&self, role: TextRole) -> TextStyle {
        if let Some(style) = self.style_overrides.get(&role) {
            return *style;
        }
        let foreground = self.resolve_color(role.token()).unwrap_or_default();
        let background = match role {
            TextRole::DiffAdd => self.resolve_color("diff_added_bg"),
            TextRole::DiffRemove => self.resolve_color("diff_removed_bg"),
            _ => None,
        }
        .filter(|color| *color != Color::Default)
        .unwrap_or_default();

        let mut attributes = TextAttributes::default();
        match role {
            TextRole::Heading | TextRole::Strong | TextRole::DiffHeader => attributes.bold = true,
            TextRole::Muted | TextRole::Subtle | TextRole::SyntaxComment => attributes.dim = true,
            TextRole::Emphasis | TextRole::Quote => attributes.italic = true,
            TextRole::Link => attributes.underline = true,
            _ => {}
        }
        TextStyle {
            foreground,
            background,
            attributes,
        }
    }

    /// Resolve a semantic block role.
    pub fn block_style(&self, role: BlockRole) -> BlockStyle {
        if let Some(style) = self.block_overrides.get(&role) {
            return *style;
        }
        match role {
            BlockRole::Code => {
                let mut border = self.style(TextRole::Border);
                if let Some(color) = self.resolve_color("md_code_border") {
                    border.foreground = color;
                }
                BlockStyle {
                    text: self.style(TextRole::Code),
                    border,
                    background: self
                        .resolve_color("md_code_bg")
                        .filter(|color| *color != Color::Default),
                    padding_left: 2,
                    padding_right: 1,
                    padding_top: 0,
                    padding_bottom: 0,
                }
            }
            BlockRole::Quote => {
                let mut border = self.style(TextRole::Border);
                if let Some(color) = self.resolve_color("md_quote_border") {
                    border.foreground = color;
                }
                BlockStyle {
                    text: self.style(TextRole::Quote),
                    border,
                    background: self
                        .resolve_color("md_quote_bg")
                        .filter(|color| *color != Color::Default),
                    padding_left: 1,
                    padding_right: 0,
                    padding_top: 0,
                    padding_bottom: 0,
                }
            }
            BlockRole::Table | BlockRole::Detail => BlockStyle {
                text: self.style(TextRole::Text),
                border: self.style(TextRole::Border),
                background: None,
                padding_left: 1,
                padding_right: 1,
                padding_top: 0,
                padding_bottom: 0,
            },
        }
    }

    /// Apply a typed style. Plain mode returns the input byte-for-byte.
    pub fn apply_style(&self, style: TextStyle, text: &str) -> String {
        if self.capabilities.plain {
            return text.to_owned();
        }
        let mut opening = String::new();
        let attrs = style.attributes;
        if attrs.bold {
            opening.push_str("\x1b[1m");
        }
        if attrs.dim {
            opening.push_str("\x1b[2m");
        }
        if attrs.italic && self.capabilities.italics == SupportLevel::Supported {
            opening.push_str("\x1b[3m");
        }
        // Never emulate italics with underline. Underline is a distinct
        // semantic primitive (links/explicit underline), and the fallback
        // makes ordinary emphasized or reasoning text look like a hyperlink.
        if attrs.underline && self.capabilities.color_depth != ColorDepth::None {
            opening.push_str("\x1b[4m");
        }
        if attrs.strikethrough && self.capabilities.color_depth != ColorDepth::None {
            opening.push_str("\x1b[9m");
        }
        if attrs.inverse && self.capabilities.color_depth != ColorDepth::None {
            opening.push_str("\x1b[7m");
        }
        if let Some(sequence) =
            palette::foreground_sequence(style.foreground, self.capabilities.color_depth)
        {
            opening.push_str(&sequence);
        }
        if let Some(sequence) =
            palette::background_sequence(style.background, self.capabilities.color_depth)
        {
            opening.push_str(&sequence);
        }
        if opening.is_empty() {
            text.to_owned()
        } else {
            format!("{opening}{text}\x1b[0m")
        }
    }

    pub fn apply_role(&self, role: TextRole, text: &str) -> String {
        self.apply_style(self.style(role), text)
    }

    /// Compatibility token foreground helper.
    pub fn fg(&self, token: &str, text: &str) -> String {
        let style = TextStyle::plain().foreground(self.resolve_color(token).unwrap_or_default());
        self.apply_style(style, text)
    }

    /// Compatibility token background helper.
    pub fn bg(&self, token: &str, text: &str) -> String {
        let style = TextStyle::plain().background(self.resolve_color(token).unwrap_or_default());
        self.apply_style(style, text)
    }

    pub fn bold(&self, text: &str) -> String {
        self.apply_style(TextStyle::plain().bold(), text)
    }

    /// Dim is capability-aware and remains a no-op without SGR styling.
    pub fn dim(&self, text: &str) -> String {
        let mut style = TextStyle::plain();
        style.attributes.dim = true;
        self.apply_style(style, text)
    }

    pub fn italic(&self, text: &str) -> String {
        self.apply_style(TextStyle::plain().italic(), text)
    }

    pub fn underline(&self, text: &str) -> String {
        self.apply_style(TextStyle::plain().underline(), text)
    }

    pub fn strikethrough(&self, text: &str) -> String {
        self.apply_style(TextStyle::plain().strikethrough(), text)
    }

    /// Resolve a legacy icon token. New components should use `GlyphSet`.
    pub fn icon(&self, token: &str) -> String {
        if !self.capabilities.nerd_font {
            return tokens::ascii_fallback(token).to_owned();
        }
        let value = self
            .resolve_value(token)
            .unwrap_or_else(|| tokens::ascii_fallback(token).to_owned());
        crate::sanitize::sanitize_line(&value, !self.capabilities.unicode).into_owned()
    }

    pub const fn capability_tier(&self) -> capability::CapabilityTier {
        self.tier
    }

    /// Override a string token at runtime (highest precedence).
    pub fn override_token(&mut self, key: &str, value: &str) {
        self.overrides.insert(key.to_owned(), value.to_owned());
        self.revision = self.revision.saturating_add(1);
    }

    /// Inject an application accent without rebuilding the theme.
    pub fn set_accent(&mut self, color: Color) {
        self.override_token("accent", &color.to_string());
    }

    pub fn override_style(&mut self, role: TextRole, style: TextStyle) {
        self.style_overrides.insert(role, style);
        self.revision = self.revision.saturating_add(1);
    }

    pub fn override_block_style(&mut self, role: BlockRole, style: BlockStyle) {
        self.block_overrides.insert(role, style);
        self.revision = self.revision.saturating_add(1);
    }

    pub fn clear_override(&mut self, key: &str) {
        self.overrides.remove(key);
        self.revision = self.revision.saturating_add(1);
    }

    pub fn clear_all_overrides(&mut self) {
        self.overrides.clear();
        self.style_overrides.clear();
        self.block_overrides.clear();
        self.revision = self.revision.saturating_add(1);
    }

    pub fn keys(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }

    pub fn resolve<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.resolve_value(key).and_then(|value| value.parse().ok())
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::with_capabilities(TerminalCapabilities::detect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_quantizes_and_plain_mode_preserves_text() {
        for depth in [
            ColorDepth::Ansi16,
            ColorDepth::Ansi256,
            ColorDepth::TrueColor,
        ] {
            let theme = Theme::with_capabilities(TerminalCapabilities::interactive(depth, true));
            let styled = theme.apply_role(TextRole::Accent, "accent");
            assert!(styled.contains("accent"));
            match depth {
                ColorDepth::Ansi16 => {
                    assert!(!styled.contains("38;2;") && !styled.contains("38;5;"))
                }
                ColorDepth::Ansi256 => assert!(styled.contains("38;5;")),
                ColorDepth::TrueColor => assert!(styled.contains("38;2;")),
                ColorDepth::None => unreachable!(),
            }
        }
        let theme = Theme::with_capabilities(TerminalCapabilities::plain());
        assert_eq!(theme.apply_role(TextRole::Error, "error"), "error");
    }

    #[test]
    fn layered_toml_aliases_and_runtime_overrides_reload_predictably() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("theme.toml");
        std::fs::write(
            &path,
            "[colors]\naccent = \"#010203\"\n[spacing]\nsm = 3\n[icons]\nsuccess = \"ok\"\n",
        )
        .unwrap();
        let capabilities = TerminalCapabilities::interactive(ColorDepth::TrueColor, true);
        let mut theme = Theme::load_with_capabilities(path.to_str(), capabilities);
        assert_eq!(theme.resolve_color("accent"), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(theme.resolve::<u8>("spacing_sm"), Some(3));
        theme.override_token("accent", "#040506");
        std::fs::write(&path, "[colors]\naccent = \"#ffffff\"\n").unwrap();
        theme.reload();
        assert_eq!(theme.resolve_color("accent"), Some(Color::Rgb(4, 5, 6)));
    }

    #[test]
    fn configured_icons_cannot_inject_terminal_controls() {
        let capabilities = TerminalCapabilities::interactive(ColorDepth::TrueColor, true)
            .with_overrides(&crate::CapabilityOverrides {
                nerd_font: Some(true),
                ..crate::CapabilityOverrides::default()
            });
        let mut theme = Theme::with_capabilities(capabilities);
        theme.override_token("icon_success", "ok\x1b]52;c;bad\x07");
        let icon = theme.icon("icon_success");
        assert!(!icon.contains('\x1b'));
        assert!(!icon.contains('\x07'));
    }

    #[test]
    fn accent_injection_changes_only_the_semantic_accent() {
        let mut theme = Theme::with_capabilities(TerminalCapabilities::interactive(
            ColorDepth::TrueColor,
            true,
        ));
        let heading = theme.style(TextRole::Heading);
        theme.set_accent(Color::Rgb(1, 2, 3));
        assert_eq!(
            theme.style(TextRole::Accent).foreground,
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(theme.style(TextRole::Heading), heading);
    }
}
