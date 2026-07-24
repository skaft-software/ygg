#![allow(missing_docs)]

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sexy_tui_rs::theme::{capability::CapabilityTier, Theme as SexyTheme};
use sexy_tui_rs::widgets::SelectListTheme;
use sexy_tui_rs::{
    CapabilityOverrides, CodeOverflow, Color, RenderOptions, RichRenderer, SupportLevel, TextRole,
    TextStyle, UnorderedListMarker,
};
use ygg_ai::{Model, ModelSpec};

use crate::config::{ColorMode, Config};
use crate::resource_resolver::{ResourceKind, ResourceResolver};
use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
use crate::tui::theme_pack;
use crate::tui::theme_schema::{self, ParsedTheme, RoleStyleSpec, ThemeSurface};

#[allow(unused_imports)]
pub use crate::tui::theme_schema::{
    ResolvedThemeLayout, ResolvedThemeSurface, ThemeDensity, ThemeLayout, ThemeMetadata,
    ThemeSurfaceAlign, ThemeSurfaceChrome, ThemeSurfaceHeading, ThemeSurfaceWidth, MAX_THEME_BYTES,
};

/// Name accepted by `/theme` for the compiled-in Ygg theme.
pub const DEFAULT_THEME_NAME: &str = "default";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeSource {
    CompiledDefault,
    Bundled(&'static str),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ThemeSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: ThemeSource,
}

// Artificial Analysis (https://artificialanalysis.ai/) uses stable creator
// colors in comparison charts. Keep those source colors here, then rebalance
// their luminance for the terminal background instead of using web colors
// verbatim: OpenAI's near-black and DeepSeek's dark blue, for example, are
// unreadable on many dark terminal profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelLab {
    OpenAi,
    Anthropic,
    Google,
    XAi,
    Meta,
    Mistral,
    DeepSeek,
    Alibaba,
    MiniMax,
    Kimi,
    ZAi,
    Nvidia,
    Xiaomi,
    Cohere,
    Amazon,
    Microsoft,
    Ai21,
    ByteDance,
    Perplexity,
    Ibm,
    Baidu,
    Tencent,
    AllenAi,
    Unknown,
}

impl ModelLab {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::XAi => "xai",
            Self::Meta => "meta",
            Self::Mistral => "mistral",
            Self::DeepSeek => "deepseek",
            Self::Alibaba => "alibaba",
            Self::MiniMax => "minimax",
            Self::Kimi => "kimi",
            Self::ZAi => "zai",
            Self::Nvidia => "nvidia",
            Self::Xiaomi => "xiaomi",
            Self::Cohere => "cohere",
            Self::Amazon => "amazon",
            Self::Microsoft => "microsoft",
            Self::Ai21 => "ai21",
            Self::ByteDance => "bytedance",
            Self::Perplexity => "perplexity",
            Self::Ibm => "ibm",
            Self::Baidu => "baidu",
            Self::Tencent => "tencent",
            Self::AllenAi => "allenai",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Some(match key.trim().to_ascii_lowercase().as_str() {
            "openai" => Self::OpenAi,
            "anthropic" => Self::Anthropic,
            "google" => Self::Google,
            "xai" => Self::XAi,
            "meta" => Self::Meta,
            "mistral" => Self::Mistral,
            "deepseek" => Self::DeepSeek,
            "alibaba" => Self::Alibaba,
            "minimax" => Self::MiniMax,
            "kimi" => Self::Kimi,
            "zai" => Self::ZAi,
            "nvidia" => Self::Nvidia,
            "xiaomi" => Self::Xiaomi,
            "cohere" => Self::Cohere,
            "amazon" => Self::Amazon,
            "microsoft" => Self::Microsoft,
            "ai21" => Self::Ai21,
            "bytedance" => Self::ByteDance,
            "perplexity" => Self::Perplexity,
            "ibm" => Self::Ibm,
            "baidu" => Self::Baidu,
            "tencent" => Self::Tencent,
            "allenai" => Self::AllenAi,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    pub(crate) fn source_color(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("#1f1f1f"),
            Self::Anthropic => Some("#cc785c"),
            Self::Google => Some("#34a853"),
            Self::XAi => Some("#736cd3"),
            Self::Meta => Some("#0089f4"),
            Self::Mistral => Some("#fd6f00"),
            Self::DeepSeek => Some("#2243e6"),
            Self::Alibaba => Some("#ff7018"),
            Self::MiniMax => Some("#eb3568"),
            Self::Kimi => Some("#047afe"),
            Self::ZAi => Some("#1c7ff8"),
            Self::Nvidia => Some("#86b737"),
            Self::Xiaomi => Some("#ff6900"),
            Self::Cohere => Some("#d18ee2"),
            Self::Amazon => Some("#ff9900"),
            Self::Microsoft => Some("#0078d5"),
            Self::Ai21 => Some("#d63864"),
            Self::ByteDance => Some("#3c8bff"),
            Self::Perplexity => Some("#1b818e"),
            Self::Ibm => Some("#0f62fe"),
            Self::Baidu => Some("#2436d8"),
            Self::Tencent => Some("#5cb9ff"),
            Self::AllenAi => Some("#f0529c"),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalBackground {
    Dark,
    Light,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb {
    red: u8,
    green: u8,
    blue: u8,
}

/// Ygg-side styling boundary around sexy-tui's semantic token store. Ygg owns
/// model-family palette selection and contrast balancing; sexy-tui owns rich
/// text layout, sanitization, syntax highlighting, and semantic encoding.
#[derive(Clone, Debug)]
pub struct YggTheme {
    inner: SexyTheme,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
    semantic_styles: BTreeMap<String, TextStyle>,
    glyphs: BTreeMap<String, String>,
    ascii_glyphs: BTreeMap<String, String>,
    surfaces: BTreeMap<String, ThemeSurface>,
    layout: ThemeLayout,
    metadata: ThemeMetadata,
    source: ThemeSource,
}

/// Semantic roles rendered as thinking prose. Code, diff, and syntax roles
/// deliberately stay out of this list so technical output remains upright,
/// crisp, and more prominent than the surrounding stream.
const REASONING_PROSE_ROLES: &[TextRole] = &[
    TextRole::Text,
    TextRole::Accent,
    TextRole::Heading,
    TextRole::Emphasis,
    TextRole::Strong,
    TextRole::Quote,
    TextRole::Link,
    TextRole::ListMarker,
];

fn unicode_glyph(name: &str) -> &'static str {
    match name {
        "top_left" => "╭",
        "top_right" => "╮",
        "bottom_left" => "╰",
        "bottom_right" => "╯",
        "horizontal" => "─",
        "vertical" | "branch" | "rail" => "│",
        "last_branch" => "└",
        "prompt" => "›",
        "shell" => "$",
        "success" => "✓",
        "warning" | "note" => "◇",
        "error" => "×",
        "interrupt" => "■",
        "pending" | "reasoning" => "·",
        "collapsed" => "▸",
        "expanded" => "▾",
        "separator" => " · ",
        "ellipsis" => "…",
        "bullet" => "•",
        "wordmark" => "ygg",
        _ => "*",
    }
}

fn ascii_glyph(name: &str) -> &'static str {
    match name {
        "top_left" | "top_right" | "bottom_left" | "bottom_right" => "+",
        "horizontal" => "-",
        "vertical" | "rail" => "|",
        "branch" => "|-",
        "last_branch" => "`-",
        "prompt" => ">",
        "shell" => "$",
        "success" => "+",
        "warning" | "interrupt" => "!",
        "note" => "*",
        "error" => "x",
        "pending" | "reasoning" => ".",
        "collapsed" => "[+]",
        "expanded" => "[-]",
        "separator" => " - ",
        "ellipsis" => "...",
        "bullet" => "*",
        "wordmark" => "ygg",
        _ => "*",
    }
}

fn default_glyphs() -> BTreeMap<String, String> {
    [
        "top_left",
        "top_right",
        "bottom_left",
        "bottom_right",
        "horizontal",
        "vertical",
        "branch",
        "last_branch",
        "rail",
        "prompt",
        "shell",
        "success",
        "warning",
        "note",
        "error",
        "interrupt",
        "pending",
        "reasoning",
        "collapsed",
        "expanded",
        "separator",
        "ellipsis",
        "bullet",
        "wordmark",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), unicode_glyph(name).to_owned()))
    .collect()
}

fn default_ascii_glyphs() -> BTreeMap<String, String> {
    default_glyphs()
        .into_keys()
        .map(|name| {
            let glyph = ascii_glyph(&name).to_owned();
            (name, glyph)
        })
        .collect()
}

fn default_surfaces() -> BTreeMap<String, ThemeSurface> {
    [
        "user",
        "assistant",
        "reasoning",
        "tool",
        "notice",
        "outcome",
        "shell",
        "compaction",
    ]
    .into_iter()
    .map(|kind| (kind.to_owned(), ThemeSurface::default()))
    .collect()
}

#[allow(dead_code)]
fn valid_runtime_role_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn semantic_text_role(name: &str) -> Option<(TextRole, &'static str)> {
    Some(match name {
        "text" | "foreground" => (TextRole::Text, "foreground"),
        "muted" => (TextRole::Muted, "muted"),
        "subtle" | "dim" => (TextRole::Subtle, "dim"),
        "accent" => (TextRole::Accent, "accent"),
        "success" => (TextRole::Success, "success"),
        "warning" => (TextRole::Warning, "warning"),
        "error" => (TextRole::Error, "error"),
        "heading" | "md_heading" => (TextRole::Heading, "md_heading"),
        "emphasis" | "md_emphasis" => (TextRole::Emphasis, "md_emphasis"),
        "strong" | "md_strong" => (TextRole::Strong, "md_strong"),
        "inline_code" | "md_code" => (TextRole::InlineCode, "md_code"),
        "code" | "md_code_block" => (TextRole::Code, "md_code_block"),
        "quote" | "md_quote" => (TextRole::Quote, "md_quote"),
        "border" => (TextRole::Border, "border"),
        "link" | "md_link" => (TextRole::Link, "md_link"),
        "list_marker" | "md_list_bullet" => (TextRole::ListMarker, "md_list_bullet"),
        "diff_add" | "diff_added" => (TextRole::DiffAdd, "diff_added"),
        "diff_remove" | "diff_removed" => (TextRole::DiffRemove, "diff_removed"),
        "diff_context" => (TextRole::DiffContext, "diff_context"),
        "diff_hunk" => (TextRole::DiffHunk, "diff_hunk"),
        "diff_header" => (TextRole::DiffHeader, "diff_header"),
        "syntax_comment" => (TextRole::SyntaxComment, "syntax_comment"),
        "syntax_keyword" => (TextRole::SyntaxKeyword, "syntax_keyword"),
        "syntax_function" => (TextRole::SyntaxFunction, "syntax_function"),
        "syntax_variable" => (TextRole::SyntaxVariable, "syntax_variable"),
        "syntax_string" => (TextRole::SyntaxString, "syntax_string"),
        "syntax_number" => (TextRole::SyntaxNumber, "syntax_number"),
        "syntax_type" => (TextRole::SyntaxType, "syntax_type"),
        "syntax_operator" => (TextRole::SyntaxOperator, "syntax_operator"),
        "syntax_punctuation" => (TextRole::SyntaxPunctuation, "syntax_punctuation"),
        _ => return None,
    })
}

impl Default for YggTheme {
    fn default() -> Self {
        default_theme()
    }
}

impl YggTheme {
    fn new(
        mut inner: SexyTheme,
        capabilities: TerminalCapabilities,
        background: TerminalBackground,
    ) -> Self {
        inner.set_capabilities(rich_capabilities(capabilities));
        Self {
            inner,
            capabilities,
            background,
            semantic_styles: BTreeMap::new(),
            glyphs: default_glyphs(),
            ascii_glyphs: default_ascii_glyphs(),
            surfaces: default_surfaces(),
            layout: ThemeLayout::default(),
            metadata: ThemeMetadata {
                name: "Ygg Default".to_owned(),
                description: "Terminal-neutral compiled theme".to_owned(),
                author: "Ygg".to_owned(),
                ..ThemeMetadata::default()
            },
            source: ThemeSource::CompiledDefault,
        }
    }

    pub fn capabilities(&self) -> TerminalCapabilities {
        self.capabilities
    }

    pub fn unicode(&self) -> bool {
        self.capabilities.unicode
    }

    #[allow(dead_code)]
    pub fn metadata(&self) -> &ThemeMetadata {
        &self.metadata
    }

    pub(crate) fn is_compiled_default(&self) -> bool {
        matches!(self.source, ThemeSource::CompiledDefault)
    }

    #[allow(dead_code)]
    pub fn source(&self) -> &ThemeSource {
        &self.source
    }

    #[allow(dead_code)]
    pub fn source_path(&self) -> Option<&Path> {
        match &self.source {
            ThemeSource::File(path) => Some(path),
            ThemeSource::CompiledDefault | ThemeSource::Bundled(_) => None,
        }
    }

    #[allow(dead_code)]
    pub fn layout(&self) -> &ThemeLayout {
        &self.layout
    }

    pub fn layout_for_width(&self, width: u16) -> ResolvedThemeLayout {
        self.layout.resolve(width)
    }

    pub fn surface_for_width(&self, kind: &str, width: u16) -> ResolvedThemeSurface<'_> {
        let narrow = self.layout_for_width(width).narrow;
        self.surfaces
            .get(kind)
            .expect("built-in transcript surface kind")
            .resolve(narrow)
    }

    #[allow(dead_code)]
    pub(crate) fn background(&self) -> TerminalBackground {
        self.background
    }

    /// Return a theme glyph with deterministic ASCII fallback. Theme files can
    /// change semantic marks, but cannot force Unicode into a conservative
    /// terminal profile.
    pub fn glyph<'a>(&'a self, name: &str) -> &'a str {
        if !self.unicode() {
            return self
                .ascii_glyphs
                .get(name)
                .map(String::as_str)
                .unwrap_or_else(|| ascii_glyph(name));
        }
        self.glyphs
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| unicode_glyph(name))
    }

    #[allow(dead_code)]
    pub fn semantic_role_names(&self) -> impl Iterator<Item = &str> {
        self.semantic_styles.keys().map(String::as_str)
    }

    pub fn has_semantic_role(&self, role: &str) -> bool {
        self.semantic_styles.contains_key(role)
    }

    #[allow(dead_code)]
    pub fn semantic_style(&self, role: &str) -> TextStyle {
        self.semantic_styles
            .get(role)
            .copied()
            .or_else(|| semantic_text_role(role).map(|(role, _)| self.inner.style(role)))
            .unwrap_or_else(|| {
                TextStyle::plain()
                    .foreground(self.inner.resolve_color(role).unwrap_or(Color::Default))
            })
    }

    /// Render a built-in or extension-defined semantic role. Extensions use
    /// stable role names and never need access to Ygg's private application
    /// state or raw terminal escape sequences.
    #[allow(dead_code)]
    pub fn apply_semantic_role(&self, role: &str, text: &str) -> String {
        self.inner.apply_style(self.semantic_style(role), text)
    }

    /// Apply an outer semantic layer while preserving it across trusted ANSI
    /// runs produced by the rich renderer. Theme data remains typed; only the
    /// renderer creates these reset/reopen sequences.
    pub(crate) fn apply_semantic_role_layered(&self, role: &str, text: &str) -> String {
        let wrapped_empty = self.inner.apply_style(self.semantic_style(role), "");
        let Some(opening) = wrapped_empty.strip_suffix("\x1b[0m") else {
            return text.to_owned();
        };
        if opening.is_empty() {
            return text.to_owned();
        }

        let mut layered = String::with_capacity(text.len().saturating_add(opening.len() * 2 + 4));
        layered.push_str(opening);
        let mut rest = text;
        while let Some(index) = rest.find("\x1b[0m") {
            let (before, after) = rest.split_at(index + 4);
            layered.push_str(before);
            layered.push_str(opening);
            rest = after;
        }
        layered.push_str(rest);
        layered.push_str("\x1b[0m");
        layered
    }

    #[allow(dead_code)]
    pub fn override_semantic_style(
        &mut self,
        role: impl Into<String>,
        style: TextStyle,
    ) -> anyhow::Result<()> {
        let role = role.into();
        if !valid_runtime_role_name(&role) {
            anyhow::bail!("invalid semantic role {role:?}");
        }
        if let Some((text_role, _)) = semantic_text_role(&role) {
            self.inner.override_style(text_role, style);
        }
        self.semantic_styles.insert(role, style);
        Ok(())
    }

    /// Reload the active file or bundled theme while preserving this terminal
    /// capability/background profile. Runtime model styling is reapplied by
    /// the shell when it swaps the returned theme in.
    #[allow(dead_code)]
    pub fn reload(&self) -> anyhow::Result<Self> {
        match &self.source {
            ThemeSource::CompiledDefault => {
                Ok(default_theme_for(self.background, self.capabilities))
            }
            ThemeSource::Bundled(name) => {
                load_bundled_theme_for(name, self.capabilities, self.background)
            }
            ThemeSource::File(path) => {
                load_theme_path_for(path, self.capabilities, self.background)
            }
        }
    }

    pub fn fg(&self, token: &str, text: &str) -> String {
        if let Some(style) = self.semantic_styles.get(token) {
            return self.inner.apply_style(*style, text);
        }
        let Some(color) = self.resolve_rgb(token) else {
            return text.to_owned();
        };
        self.color_text(color, text)
    }

    /// Resolve the accent colour of a specific model family. `None` uses the
    /// active theme token; a concrete lab remains stable across model switches.
    pub(crate) fn model_rgb(&self, lab: Option<ModelLab>) -> Option<(u8, u8, u8)> {
        let Some(lab) = lab else {
            return self.role_rgb("model_accent");
        };
        let configured_key = format!("model.{}", lab.key());
        let configured = self
            .resolve::<String>(&configured_key)
            .filter(|color| parse_hex_color(color).is_some());
        let use_lab_color =
            configured.is_some() || self.resolve::<bool>("model.use_lab_color").unwrap_or(false);
        let source = configured
            .or_else(|| {
                if use_lab_color {
                    lab.source_color().map(str::to_owned)
                } else {
                    None
                }
            })
            .or_else(|| {
                self.resolve::<String>("accent")
                    .filter(|color| parse_hex_color(color).is_some())
            })
            .unwrap_or_else(|| DEFAULT_ACCENT.to_owned());
        let color = parse_hex_color(&balance_foreground(&source, self.background))?;
        Some((color.red, color.green, color.blue))
    }

    /// Render `text` in the accent colour of a specific model family.
    /// When `lab` is `None` or has no source colour the global `model_accent`
    /// token is used instead.
    pub fn model_fg(&self, lab: Option<ModelLab>, text: &str) -> String {
        let Some((red, green, blue)) = self.model_rgb(lab) else {
            return text.to_owned();
        };
        self.color_text(Rgb { red, green, blue }, text)
    }

    /// Render a historical prompt cell using the exact model colour stored
    /// with that turn and a readable foreground chosen for that colour.
    pub(crate) fn prompt_color_cell(&self, color: Option<&str>, text: &str) -> String {
        let Some(color) = color.and_then(parse_hex_color) else {
            return text.to_owned();
        };
        let luminance =
            u32::from(color.red) * 299 + u32::from(color.green) * 587 + u32::from(color.blue) * 114;
        let foreground = if luminance >= 150_000 {
            Color::Rgb(0, 0, 0)
        } else {
            Color::Rgb(255, 255, 255)
        };
        self.inner.apply_style(
            TextStyle::plain()
                .foreground(foreground)
                .background(Color::Rgb(color.red, color.green, color.blue)),
            text,
        )
    }

    pub(crate) fn role_rgb(&self, token: &str) -> Option<(u8, u8, u8)> {
        self.resolve_rgb(token)
            .map(|color| (color.red, color.green, color.blue))
    }

    /// Resting composer chrome is a background-adjacent form of the model
    /// accent. Moving toward white on light profiles and black on dark ones
    /// keeps the outline quiet before the shimmer moves toward full accent.
    pub(crate) fn composer_idle_rgb(&self, accent: (u8, u8, u8)) -> (u8, u8, u8) {
        let source = Rgb {
            red: accent.0,
            green: accent.1,
            blue: accent.2,
        };
        let destination = match self.background {
            TerminalBackground::Light => Rgb {
                red: 255,
                green: 255,
                blue: 255,
            },
            // Preserve the conservative dark-terminal fallback when neither
            // YGG_COLOR_SCHEME nor COLORFGBG identifies the background.
            TerminalBackground::Dark | TerminalBackground::Unknown => Rgb {
                red: 0,
                green: 0,
                blue: 0,
            },
        };
        let idle = blend(source, destination, 0.88);
        (idle.red, idle.green, idle.blue)
    }

    pub(crate) fn rgb_fg(&self, color: (u8, u8, u8), text: &str) -> String {
        self.color_text(
            Rgb {
                red: color.0,
                green: color.1,
                blue: color.2,
            },
            text,
        )
    }

    /// Stable key for the foreground sequence `rgb_fg` will actually emit.
    /// Gradient renderers can use this to combine adjacent cells after colour
    /// quantization instead of reopening the same SGR sequence per glyph.
    pub(crate) fn rgb_fg_key(&self, color: (u8, u8, u8)) -> u32 {
        let color = Rgb {
            red: color.0,
            green: color.1,
            blue: color.2,
        };
        match self.capabilities.color {
            ColorDepth::None => 0,
            ColorDepth::Ansi16 => 0x0100_0000 | u32::from(nearest_ansi16_code(color)),
            ColorDepth::Ansi256 => 0x0200_0000 | u32::from(nearest_ansi256(color)),
            ColorDepth::TrueColor => {
                0x0300_0000
                    | (u32::from(color.red) << 16)
                    | (u32::from(color.green) << 8)
                    | u32::from(color.blue)
            }
        }
    }

    fn color_text(&self, color: Rgb, text: &str) -> String {
        match self.capabilities.color {
            ColorDepth::None => text.to_owned(),
            ColorDepth::TrueColor => format!(
                "\x1b[38;2;{};{};{}m{text}\x1b[39m",
                color.red, color.green, color.blue
            ),
            ColorDepth::Ansi256 => {
                format!("\x1b[38;5;{}m{text}\x1b[39m", nearest_ansi256(color))
            }
            ColorDepth::Ansi16 => {
                format!("\x1b[{}m{text}\x1b[39m", nearest_ansi16_code(color))
            }
        }
    }

    pub fn bold(&self, text: &str) -> String {
        if self.capabilities.color == ColorDepth::None {
            text.to_owned()
        } else {
            format!("\x1b[1m{text}\x1b[22m")
        }
    }

    /// Render secondary text using a real muted foreground rather than SGR
    /// faint. Terminal implementations disagree about SGR 2 (some make text
    /// look brighter or thinner), while a palette colour is predictable.
    pub fn dim(&self, text: &str) -> String {
        self.fg("muted", text)
    }

    pub(crate) fn settled_event_dot(&self, tone: &str, text: &str) -> String {
        let source = match tone {
            "success" => Rgb {
                red: 78,
                green: 170,
                blue: 106,
            },
            "error" => Rgb {
                red: 207,
                green: 77,
                blue: 77,
            },
            _ => self.resolve_rgb("muted").unwrap_or(Rgb {
                red: 119,
                green: 119,
                blue: 119,
            }),
        };
        let destination = match self.background {
            TerminalBackground::Light => Rgb {
                red: 255,
                green: 255,
                blue: 255,
            },
            TerminalBackground::Dark => Rgb {
                red: 0,
                green: 0,
                blue: 0,
            },
            TerminalBackground::Unknown => Rgb {
                red: 85,
                green: 85,
                blue: 85,
            },
        };
        let color = blend(source, destination, 0.62);
        self.color_text(color, text)
    }

    pub fn override_token(&mut self, key: &str, value: &str) {
        self.inner.override_token(key, value);
    }

    pub fn resolve<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.inner.resolve(key)
    }

    /// Build the persistent semantic renderer used by assistant transcript
    /// blocks. The renderer uses its own semantic colour roles (heading,
    /// code, link, syntax, etc.) so the model accent never bleeds into prose.
    pub fn rich_renderer(&self) -> RichRenderer {
        self.rich_renderer_with_inner(self.inner.clone())
    }

    /// Build the renderer used for model reasoning. Every semantic role keeps
    /// its own colour while prose receives a muted foreground, so the stream
    /// recedes behind the final response without changing font shape.
    pub fn reasoning_renderer(&self) -> RichRenderer {
        let mut theme = self.inner.clone();
        let reasoning_foreground = balance_foreground("#777777", self.background);
        let reasoning_foreground = parse_hex_color(&reasoning_foreground)
            .map(|color| Color::Rgb(color.red, color.green, color.blue));
        for role in REASONING_PROSE_ROLES {
            let mut style = theme.style(*role);
            if let Some(foreground) = reasoning_foreground {
                style.foreground = foreground;
            }
            // Do not use SGR faint here. A muted foreground gives the desired
            // hierarchy without changing the terminal's font weight/shape.
            style.attributes.dim = false;
            style.attributes.italic = false;
            theme.override_style(*role, style);
        }
        // Muted/subtle roles are used by code-frame labels and ellipses. Keep
        // those annotations quiet, but upright, so an entire code block never
        // inherits the thinking prose treatment.
        for role in [TextRole::Muted, TextRole::Subtle] {
            let mut style = theme.style(role);
            if let Some(foreground) = reasoning_foreground {
                style.foreground = foreground;
            }
            style.attributes.dim = false;
            style.attributes.italic = false;
            style.attributes.underline = false;
            theme.override_style(role, style);
        }
        self.rich_renderer_with_inner(theme)
    }

    fn rich_renderer_with_inner(&self, mut theme: SexyTheme) -> RichRenderer {
        let capabilities = rich_capabilities(self.capabilities);
        theme.set_capabilities(capabilities);
        RichRenderer::new(
            theme,
            capabilities,
            RenderOptions {
                // Transcript code has no horizontal viewport. Wrapping keeps
                // every model-emitted grapheme visible while sexy-tui retains
                // the original source as semantic copy text.
                code_overflow: CodeOverflow::Wrap,
                code_borders: true,
                syntax_highlighting: true,
                tables: true,
                unordered_list_marker: UnorderedListMarker::Dash,
                ..RenderOptions::default()
            },
        )
    }

    fn resolve_rgb(&self, token: &str) -> Option<Rgb> {
        let mut value = self.inner.resolve::<String>(token)?;
        for _ in 0..6 {
            if value.eq_ignore_ascii_case("default") || value.eq_ignore_ascii_case("none") {
                return None;
            }
            if let Some(color) = parse_hex_color(&value).or_else(|| named_color(&value)) {
                return Some(color);
            }
            let next = self.inner.resolve::<String>(&value)?;
            if next == value {
                return None;
            }
            value = next;
        }
        None
    }
}

fn rich_capabilities(capabilities: TerminalCapabilities) -> sexy_tui_rs::TerminalCapabilities {
    if !capabilities.interactive {
        return sexy_tui_rs::TerminalCapabilities::plain();
    }
    let color_depth = match capabilities.color {
        ColorDepth::None => sexy_tui_rs::ColorDepth::None,
        ColorDepth::Ansi16 => sexy_tui_rs::ColorDepth::Ansi16,
        ColorDepth::Ansi256 => sexy_tui_rs::ColorDepth::Ansi256,
        ColorDepth::TrueColor => sexy_tui_rs::ColorDepth::TrueColor,
    };
    sexy_tui_rs::TerminalCapabilities::interactive(color_depth, capabilities.unicode)
        .with_overrides(&CapabilityOverrides {
            italics: Some(if capabilities.italics {
                SupportLevel::Supported
            } else {
                SupportLevel::Unsupported
            }),
            hyperlinks: Some(capabilities.hyperlinks),
            animation: Some(capabilities.animation),
            ..CapabilityOverrides::default()
        })
}

fn named_color(value: &str) -> Option<Rgb> {
    let (red, green, blue) = match value.trim().to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "red" => (205, 49, 49),
        "green" => (13, 188, 121),
        "yellow" => (229, 229, 16),
        "blue" => (36, 114, 200),
        "magenta" | "purple" => (188, 63, 188),
        "cyan" => (17, 168, 205),
        "white" => (229, 229, 229),
        "gray" | "grey" => (102, 102, 102),
        _ => return None,
    };
    Some(Rgb { red, green, blue })
}

const ANSI16: [(Rgb, u8); 16] = [
    (
        Rgb {
            red: 0,
            green: 0,
            blue: 0,
        },
        30,
    ),
    (
        Rgb {
            red: 205,
            green: 49,
            blue: 49,
        },
        31,
    ),
    (
        Rgb {
            red: 13,
            green: 188,
            blue: 121,
        },
        32,
    ),
    (
        Rgb {
            red: 229,
            green: 229,
            blue: 16,
        },
        33,
    ),
    (
        Rgb {
            red: 36,
            green: 114,
            blue: 200,
        },
        34,
    ),
    (
        Rgb {
            red: 188,
            green: 63,
            blue: 188,
        },
        35,
    ),
    (
        Rgb {
            red: 17,
            green: 168,
            blue: 205,
        },
        36,
    ),
    (
        Rgb {
            red: 229,
            green: 229,
            blue: 229,
        },
        37,
    ),
    (
        Rgb {
            red: 102,
            green: 102,
            blue: 102,
        },
        90,
    ),
    (
        Rgb {
            red: 241,
            green: 76,
            blue: 76,
        },
        91,
    ),
    (
        Rgb {
            red: 35,
            green: 209,
            blue: 139,
        },
        92,
    ),
    (
        Rgb {
            red: 245,
            green: 245,
            blue: 67,
        },
        93,
    ),
    (
        Rgb {
            red: 59,
            green: 142,
            blue: 234,
        },
        94,
    ),
    (
        Rgb {
            red: 214,
            green: 112,
            blue: 214,
        },
        95,
    ),
    (
        Rgb {
            red: 41,
            green: 184,
            blue: 219,
        },
        96,
    ),
    (
        Rgb {
            red: 255,
            green: 255,
            blue: 255,
        },
        97,
    ),
];

fn color_distance(left: Rgb, right: Rgb) -> u32 {
    let red = i32::from(left.red) - i32::from(right.red);
    let green = i32::from(left.green) - i32::from(right.green);
    let blue = i32::from(left.blue) - i32::from(right.blue);
    (red * red + green * green + blue * blue) as u32
}

fn nearest_ansi16_code(color: Rgb) -> u8 {
    ANSI16
        .iter()
        .min_by_key(|(candidate, _)| color_distance(color, *candidate))
        .map_or(37, |(_, code)| *code)
}

fn ansi256_rgb(index: u8) -> Rgb {
    if index < 16 {
        return ANSI16[usize::from(index)].0;
    }
    if index < 232 {
        let value = index - 16;
        let component = |part: u8| if part == 0 { 0 } else { 55 + part * 40 };
        return Rgb {
            red: component(value / 36),
            green: component((value % 36) / 6),
            blue: component(value % 6),
        };
    }
    let gray = 8 + (index - 232) * 10;
    Rgb {
        red: gray,
        green: gray,
        blue: gray,
    }
}

fn nearest_ansi256(color: Rgb) -> u8 {
    (0u8..=255)
        .min_by_key(|index| color_distance(color, ansi256_rgb(*index)))
        .unwrap_or(7)
}

const DEFAULT_ACCENT: &str = "#16876d";
// 0.27 gives ~5.6:1 against the test-dark reference (and ~5:1 against a
// typical #1e1e1e terminal).  We stay well below the old AAA target of
// 0.32 so foreground colours keep their saturation instead of washing out.
const DARK_TARGET_LUMINANCE: f64 = 0.27;
const LIGHT_TARGET_LUMINANCE: f64 = 0.11;
// Symmetric midpoint: ~4.58:1 against both pure black and pure white.
// Light-terminal users can set YGG_COLOR_SCHEME=light for a 0.11 target.
const UNIVERSAL_TARGET_LUMINANCE: f64 = 0.179;

// Tokens that receive terminal-background-aware luminance balancing.
// These are semantic UI signals (errors, warnings, model accent) whose
// source colours may be unreadable on dark or light terminals without
// adjustment.  The compiled default and bundled themes additionally receive
// the standard technical code/diff palette below; user file themes keep their
// configured code colours unless they opt into their own role overrides.
const BALANCED_FOREGROUNDS: &[(&str, &str)] = &[
    ("muted", "#777777"),
    ("dim", "#777777"),
    ("accent", DEFAULT_ACCENT),
    ("error", "#c74747"),
    ("warning", "#9a6700"),
    ("border_focused", DEFAULT_ACCENT),
];

/// Foreground tokens applied verbatim — no luminance balancing.
/// "default" means the terminal's own foreground colour.
const VERBATIM_FOREGROUNDS: &[(&str, &str)] = &[
    ("foreground", "default"),
    ("success", "default"),
    ("info", "default"),
    ("border", "default"),
    ("border_idle", "default"),
    ("user_msg_text", "default"),
    ("assistant_msg_text", "default"),
    ("tool_title", "default"),
    ("tool_output", "default"),
    // Diff semantics are carried by row surfaces. Source text keeps its normal
    // syntax foregrounds (or the terminal foreground when no syntax applies).
    ("diff_added", "default"),
    ("diff_removed", "default"),
    ("diff_context", "default"),
    // --- Markdown chrome ------------------------------------------------
    ("md_heading", "default"),
    ("md_link", "default"),
    ("md_code", "#78a9b0"),
    ("md_code_block", "default"),
    ("md_code_border", "default"),
    ("md_quote", "default"),
    ("md_quote_border", "default"),
    ("md_hr", "default"),
    ("md_list_bullet", "default"),
    // --- syntax highlighting --------------------------------------------
    ("syntax_comment", "default"),
    ("syntax_keyword", "#815ac0"),
    ("syntax_function", "#287fb8"),
    ("syntax_variable", "#68737d"),
    ("syntax_string", "#00b847"),
    ("syntax_number", "#b26a00"),
    ("syntax_type", "#9b6500"),
    ("syntax_operator", "#b14d7d"),
    ("syntax_punctuation", "#68737d"),
];

/// Subtle terminal-background-aware surfaces. These retain their semantic hue
/// without replacing syntax foregrounds or looking like terminal selection.
const DEFAULT_BACKGROUNDS: &[(&str, &str)] = &[("user_msg_bg", DEFAULT_ACCENT)];

// Standard technical palette for code and diffs. Bundled themes may keep their
// distinctive chrome, but source code needs one predictable language-neutral
// grammar: syntax owns foregrounds, diff owns quiet row surfaces, and the +/-
// marker carries the high-salience add/remove hue.
const STANDARD_SYNTAX_COLORS: &[(&str, &str, &str)] = &[
    ("syntax_comment", "#9da8b5", "#505c68"),
    ("syntax_keyword", "#f29e74", "#813d00"),
    ("syntax_type", "#76c7c0", "#005c5e"),
    ("syntax_function", "#a8c7fa", "#2456a6"),
    ("syntax_variable", "#d6dee8", "#1f2933"),
    ("syntax_string", "#a8d279", "#335e00"),
    ("syntax_number", "#d6a6e8", "#7d3c98"),
    ("syntax_operator", "#aab4c0", "#4d5966"),
    ("syntax_punctuation", "#aab4c0", "#4d5966"),
    ("diff_hunk", "#8ab4f8", "#355f9e"),
];

const STANDARD_DIFF_COLORS: &[(&str, &str, &str)] = &[
    ("diff_added_marker", "#67d391", "#087a45"),
    ("diff_removed_marker", "#ff7d8a", "#b4233a"),
];

const STANDARD_DIFF_SURFACES: &[(&str, &str, &str)] = &[
    ("diff_added_bg", "#10261e", "#e8f6ee"),
    ("diff_removed_bg", "#2a171b", "#fcebed"),
];

fn standard_foreground(dark: &str, light: &str, background: TerminalBackground) -> String {
    match background {
        TerminalBackground::Dark => dark.to_owned(),
        TerminalBackground::Light => light.to_owned(),
        TerminalBackground::Unknown => balance_foreground(light, TerminalBackground::Unknown),
    }
}

fn standard_surface(dark: &str, light: &str, background: TerminalBackground) -> String {
    match background {
        TerminalBackground::Dark => dark.to_owned(),
        TerminalBackground::Light => light.to_owned(),
        // Unknown terminal backgrounds cannot safely receive absolute RGB row
        // surfaces. Preserve diff semantics through +/- text and marker colour.
        TerminalBackground::Unknown => "default".to_owned(),
    }
}

fn apply_standard_technical_palette(theme: &mut YggTheme, background: TerminalBackground) {
    theme.override_token("diff_added", "default");
    theme.override_token("diff_removed", "default");
    theme.override_token("diff_context", "default");
    for &(token, dark, light) in STANDARD_SYNTAX_COLORS {
        theme.override_token(token, &standard_foreground(dark, light, background));
    }
    for &(token, dark, light) in STANDARD_DIFF_COLORS {
        theme.override_token(token, &standard_foreground(dark, light, background));
    }
    for &(token, dark, light) in STANDARD_DIFF_SURFACES {
        theme.override_token(token, &standard_surface(dark, light, background));
    }
}

fn parse_hex_color(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Rgb {
        red: u8::from_str_radix(&hex[0..2], 16).ok()?,
        green: u8::from_str_radix(&hex[2..4], 16).ok()?,
        blue: u8::from_str_radix(&hex[4..6], 16).ok()?,
    })
}

fn hex_color(color: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red, color.green, color.blue)
}

fn linear_channel(channel: u8) -> f64 {
    let channel = f64::from(channel) / 255.0;
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn relative_luminance(color: Rgb) -> f64 {
    0.2126 * linear_channel(color.red)
        + 0.7152 * linear_channel(color.green)
        + 0.0722 * linear_channel(color.blue)
}

fn blend_channel(source: u8, destination: u8, amount: f64) -> u8 {
    (f64::from(source) + (f64::from(destination) - f64::from(source)) * amount)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn blend(source: Rgb, destination: Rgb, amount: f64) -> Rgb {
    Rgb {
        red: blend_channel(source.red, destination.red, amount),
        green: blend_channel(source.green, destination.green, amount),
        blue: blend_channel(source.blue, destination.blue, amount),
    }
}

/// Move a web color toward black or white until all lab colors share a useful
/// perceived brightness. Equalizing luminance avoids a near-black OpenAI accent
/// beside a neon-orange Amazon accent while preserving their recognizable hue.
/// Move a colour toward the terminal background so it reads as a subtle
/// surface tint rather than a painted slab. Used for diff-add/diff-remove
/// backgrounds so they adapt to the user's terminal profile.
pub(crate) fn balance_background(source: &str, background: TerminalBackground) -> String {
    let Some(source) = parse_hex_color(source) else {
        return source.to_owned();
    };
    // Most terminals do not export COLORFGBG (Ghostty included), so treating an
    // unknown profile as "no surface" silently removes diff semantics. Use the
    // universal midpoint already used for unknown-profile foregrounds: it
    // retains the surface while remaining readable with either a black or
    // white terminal-default foreground.
    let target_luminance = match background {
        TerminalBackground::Dark => 0.025,
        TerminalBackground::Light => 0.95,
        TerminalBackground::Unknown => UNIVERSAL_TARGET_LUMINANCE,
    };
    let source_luminance = relative_luminance(source);
    if (source_luminance - target_luminance).abs() <= 0.002 {
        return hex_color(source);
    }
    let lighten = source_luminance < target_luminance;
    let destination = if lighten {
        Rgb {
            red: 255,
            green: 255,
            blue: 255,
        }
    } else {
        Rgb {
            red: 0,
            green: 0,
            blue: 0,
        }
    };
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..20 {
        let amount = (low + high) / 2.0;
        let candidate = blend(source, destination, amount);
        let reached = if lighten {
            relative_luminance(candidate) >= target_luminance
        } else {
            relative_luminance(candidate) <= target_luminance
        };
        if reached {
            high = amount;
        } else {
            low = amount;
        }
    }
    hex_color(blend(source, destination, high))
}

fn balance_foreground(source: &str, background: TerminalBackground) -> String {
    let Some(source) = parse_hex_color(source) else {
        return source.to_owned();
    };
    let target = match background {
        TerminalBackground::Dark => DARK_TARGET_LUMINANCE,
        TerminalBackground::Light => LIGHT_TARGET_LUMINANCE,
        TerminalBackground::Unknown => UNIVERSAL_TARGET_LUMINANCE,
    };
    let source_luminance = relative_luminance(source);
    if (source_luminance - target).abs() <= 0.002 {
        return hex_color(source);
    }

    let lighten = source_luminance < target;
    let destination = if lighten {
        Rgb {
            red: 255,
            green: 255,
            blue: 255,
        }
    } else {
        Rgb {
            red: 0,
            green: 0,
            blue: 0,
        }
    };
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..20 {
        let amount = (low + high) / 2.0;
        let candidate = blend(source, destination, amount);
        let reached = if lighten {
            relative_luminance(candidate) >= target
        } else {
            relative_luminance(candidate) <= target
        };
        if reached {
            high = amount;
        } else {
            low = amount;
        }
    }
    hex_color(blend(source, destination, high))
}

fn background_from_colorfgbg(value: &str) -> Option<TerminalBackground> {
    // COLORFGBG conventionally ends in the ANSI background index, e.g. 15;0
    // for light-on-dark and 0;15 for dark-on-light.
    let index = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    match index {
        0..=6 | 8 => Some(TerminalBackground::Dark),
        7 | 9..=15 => Some(TerminalBackground::Light),
        _ => None,
    }
}

fn background_from_override(value: &str) -> Option<TerminalBackground> {
    match value.trim().to_ascii_lowercase().as_str() {
        "dark" => Some(TerminalBackground::Dark),
        "light" => Some(TerminalBackground::Light),
        "universal" | "unknown" => Some(TerminalBackground::Unknown),
        // Returning None lets terminal_background continue to COLORFGBG.
        "auto" => None,
        _ => None,
    }
}

pub(crate) fn background_from_terminal_rgb(red: u8, green: u8, blue: u8) -> TerminalBackground {
    let background = Rgb { red, green, blue };
    let luminance = relative_luminance(background);
    let contrast_with_black = (luminance + 0.05) / 0.05;
    let contrast_with_white = 1.05 / (luminance + 0.05);
    if contrast_with_black >= contrast_with_white {
        TerminalBackground::Light
    } else {
        TerminalBackground::Dark
    }
}

fn terminal_background() -> TerminalBackground {
    std::env::var("YGG_COLOR_SCHEME")
        .ok()
        .as_deref()
        .and_then(background_from_override)
        .or_else(|| {
            std::env::var("COLORFGBG")
                .ok()
                .as_deref()
                .and_then(background_from_colorfgbg)
        })
        .unwrap_or(TerminalBackground::Unknown)
}

fn sexy_tier(capabilities: TerminalCapabilities) -> CapabilityTier {
    if capabilities.color == ColorDepth::TrueColor {
        CapabilityTier::TrueColor
    } else {
        CapabilityTier::Baseline
    }
}

fn apply_required_surfaces(theme: &mut YggTheme, background: TerminalBackground) {
    // Diff status belongs to the row surface, never to source foregrounds.
    theme.override_token("diff_added", "default");
    theme.override_token("diff_removed", "default");
    for &(token, source) in DEFAULT_BACKGROUNDS {
        if theme.inner.resolve_color(token).unwrap_or_default() == Color::Default {
            theme.override_token(token, &balance_background(source, background));
        }
    }
}

fn default_theme_for(
    background: TerminalBackground,
    capabilities: TerminalCapabilities,
) -> YggTheme {
    let mut theme = YggTheme::new(
        SexyTheme::load(None, sexy_tier(capabilities)),
        capabilities,
        background,
    );
    // Semantic UI signals get luminance-balanced; everything else is verbatim.
    for &(token, source) in BALANCED_FOREGROUNDS {
        theme.override_token(token, &balance_foreground(source, background));
    }
    for &(token, source) in VERBATIM_FOREGROUNDS {
        theme.override_token(token, source);
    }
    // Code is identified by compact borders, indentation, and syntax roles.
    // Painting fixed surfaces looks like terminal selection and cannot be
    // contrast-safe across unknown light/dark profiles. Named themes may opt in.
    theme.override_token("md_code_bg", "default");
    theme.override_token("md_code_inline_bg", "default");
    apply_required_surfaces(&mut theme, background);
    apply_standard_technical_palette(&mut theme, background);
    // There is no model before the startup picker. Use Ygg green until the
    // selected model's lab is known.
    let neutral_model_accent = balance_foreground(DEFAULT_ACCENT, background);
    theme.override_token("model.use_lab_color", "true");
    theme.override_token("model_accent", &neutral_model_accent);
    theme.override_token("model_assistant", "default");
    theme
}

/// Build Ygg's compiled-in, terminal-balanced theme.
pub fn default_theme() -> YggTheme {
    default_theme_for(
        terminal_background(),
        TerminalCapabilities::detect(ColorMode::Auto, false),
    )
}

#[cfg(test)]
pub(crate) fn test_theme() -> YggTheme {
    default_theme_for(
        TerminalBackground::Unknown,
        TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
    )
}

#[cfg(test)]
pub(crate) fn test_theme_with(capabilities: TerminalCapabilities) -> YggTheme {
    default_theme_for(TerminalBackground::Unknown, capabilities)
}

#[cfg(test)]
pub(crate) fn test_theme_from_source(source: &str) -> YggTheme {
    test_theme_source_with(
        source,
        TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
        TerminalBackground::Unknown,
    )
}

#[cfg(test)]
pub(crate) fn test_theme_source_with(
    source: &str,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> YggTheme {
    load_theme_source_for(
        source,
        "renderer-test",
        ThemeSource::CompiledDefault,
        "Renderer test",
        capabilities,
        background,
    )
    .expect("renderer test theme should compile")
}

#[cfg(test)]
pub(crate) fn test_bundled_theme_with(
    name: &str,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> YggTheme {
    load_bundled_theme_for(name, capabilities, background)
        .expect("bundled renderer test theme should compile")
}

#[cfg(test)]
fn foreground(theme: &YggTheme, token: &'static str) -> Box<dyn Fn(&str) -> String> {
    let theme = theme.clone();
    Box::new(move |text| theme.fg(token, text))
}

#[cfg(test)]
fn bold_foreground(theme: &YggTheme, token: &'static str) -> Box<dyn Fn(&str) -> String> {
    let theme = theme.clone();
    Box::new(move |text| theme.bold(&theme.fg(token, text)))
}

#[cfg(test)]
fn project_theme_dir(config: &Config) -> PathBuf {
    config.workspace.join(".ygg").join("themes")
}

fn theme_file_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || Path::new(name).components().count() != 1
        || name.contains(std::path::MAIN_SEPARATOR)
    {
        return None;
    }
    Some(if name.ends_with(".toml") {
        name.to_owned()
    } else {
        format!("{name}.toml")
    })
}

/// Resolve a theme by name, preferring the workspace theme directory.
#[cfg(test)]
pub fn theme_path(name: &str, config: &Config) -> Option<PathBuf> {
    let file_name = theme_file_name(name)?;
    let resource_name = file_name.strip_suffix(".toml").unwrap_or(&file_name);
    let resolver = ResourceResolver::new(config.workspace.clone(), config.workspace_trusted);
    resolver
        .discover(ResourceKind::Theme, &config.theme_paths)
        .get(resource_name)
        .map(|resource| resource.path.clone())
}

fn read_theme_file_bounded(path: &Path) -> anyhow::Result<String> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("theme {} has no parent", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("theme {} has no file name", path.display()))?;
    // Reloads use the same no-follow, regular-file boundary as initial shared
    // resource reads. A trusted theme cannot be swapped for a symlink or FIFO
    // between discovery and `/theme reload`.
    let opened_path = parent.canonicalize()?.join(name);
    let bytes =
        ygg_agent::secure_fs::read_regular_file_bounded(&opened_path, MAX_THEME_BYTES as usize)?;
    String::from_utf8(bytes)
        .map_err(|error| anyhow::anyhow!("theme {} is not UTF-8: {error}", path.display()))
}

fn background_token(token: &str) -> bool {
    token.ends_with("_bg") || matches!(token, "surface" | "overlay" | "raised" | "background")
}

fn adaptive_color_value(token: &str, value: &str, background: TerminalBackground) -> String {
    if parse_hex_color(value).is_none() {
        return value.to_owned();
    }
    if background_token(token) {
        balance_background(value, background)
    } else {
        balance_foreground(value, background)
    }
}

fn resolve_role_color(
    theme: &YggTheme,
    token: &str,
    adaptive: bool,
    surface: bool,
) -> anyhow::Result<Color> {
    let value = if adaptive && parse_hex_color(token).is_some() {
        if surface {
            balance_background(token, theme.background)
        } else {
            balance_foreground(token, theme.background)
        }
    } else {
        token.to_owned()
    };
    Color::parse(&value)
        .or_else(|| theme.inner.resolve_color(&value))
        .ok_or_else(|| anyhow::anyhow!("unknown theme color or token {token:?}"))
}

fn apply_role_style(
    theme: &mut YggTheme,
    name: &str,
    spec: &RoleStyleSpec,
    adaptive_by_default: bool,
) -> anyhow::Result<()> {
    let mapped = semantic_text_role(name);
    let token = mapped.map_or(name, |(_, token)| token);
    let adaptive = spec.adaptive.unwrap_or(adaptive_by_default);

    if let Some(foreground) = spec
        .foreground
        .as_deref()
        .filter(|foreground| *foreground != token)
    {
        let value = if adaptive {
            adaptive_color_value(token, foreground, theme.background)
        } else {
            foreground.to_owned()
        };
        theme.override_token(token, &value);
    }

    let mut style = mapped.map_or_else(
        || {
            TextStyle::plain()
                .foreground(theme.inner.resolve_color(token).unwrap_or(Color::Default))
        },
        |(role, _)| theme.inner.style(role),
    );
    if let Some(foreground) = spec.foreground.as_deref() {
        style.foreground = resolve_role_color(theme, foreground, adaptive, false)?;
    }
    if let Some(background) = spec.background.as_deref() {
        style.background = resolve_role_color(theme, background, adaptive, true)?;
    }
    if let Some(value) = spec.bold {
        style.attributes.bold = value;
    }
    if let Some(value) = spec.dim {
        style.attributes.dim = value;
    }
    if let Some(value) = spec.italic {
        style.attributes.italic = value;
    }
    if let Some(value) = spec.underline {
        style.attributes.underline = value;
    }
    if let Some(value) = spec.strikethrough {
        style.attributes.strikethrough = value;
    }
    if let Some(value) = spec.inverse {
        style.attributes.inverse = value;
    }

    if let Some((role, canonical)) = mapped {
        theme.inner.override_style(role, style);
        theme.semantic_styles.insert(canonical.to_owned(), style);
    }
    theme.semantic_styles.insert(name.to_owned(), style);
    Ok(())
}

fn build_parsed_theme(
    parsed: ParsedTheme,
    source: ThemeSource,
    fallback_name: &str,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> anyhow::Result<YggTheme> {
    let mut theme = YggTheme::new(
        SexyTheme::load(None, sexy_tier(capabilities)),
        capabilities,
        background,
    );
    let adaptive = parsed.metadata.adaptive;
    for (token, value) in &parsed.tokens {
        let value = if adaptive {
            adaptive_color_value(token, value, background)
        } else {
            value.clone()
        };
        theme.override_token(token, &value);
    }
    apply_required_surfaces(&mut theme, background);
    if matches!(source, ThemeSource::Bundled(_)) {
        apply_standard_technical_palette(&mut theme, background);
    }
    for (name, style) in &parsed.roles {
        apply_role_style(&mut theme, name, style, adaptive)?;
    }
    theme.glyphs.extend(parsed.glyphs);
    theme.ascii_glyphs.extend(parsed.ascii_glyphs);
    theme.surfaces.extend(parsed.surfaces);
    theme.layout = parsed.layout;
    theme.metadata = parsed.metadata;
    if theme.metadata.name.trim().is_empty() {
        theme.metadata.name = fallback_name.to_owned();
    }
    theme.source = source;
    // The picker can render before a model is selected. Seed model-aware
    // chrome from the theme's own accent until the model family is known.
    apply_model_lab(&mut theme, ModelLab::Unknown);
    Ok(theme)
}

fn load_theme_source_for(
    source_text: &str,
    source_name: &str,
    source: ThemeSource,
    fallback_name: &str,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> anyhow::Result<YggTheme> {
    let parsed = theme_schema::parse_theme(source_text, source_name, background)?;
    build_parsed_theme(parsed, source, fallback_name, capabilities, background)
}

fn load_theme_path_for(
    path: &Path,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> anyhow::Result<YggTheme> {
    let source_text = read_theme_file_bounded(path)?;
    let fallback_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Custom theme");
    load_theme_source_for(
        &source_text,
        &path.display().to_string(),
        ThemeSource::File(path.to_owned()),
        fallback_name,
        capabilities,
        background,
    )
}

fn load_bundled_theme_for(
    name: &str,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> anyhow::Result<YggTheme> {
    let bundled = theme_pack::find(name)
        .ok_or_else(|| anyhow::anyhow!("bundled theme {name:?} was not found"))?;
    load_theme_source_for(
        bundled.source,
        bundled.id,
        ThemeSource::Bundled(bundled.id),
        bundled.id,
        capabilities,
        background,
    )
}

/// Load a theme from a resolver-selected path. The shared resource resolver
/// owns precedence and trust; this boundary owns bounded reads, schema
/// validation, terminal adaptation, and semantic compilation.
#[allow(dead_code)]
pub fn load_theme_path(path: &Path, config: &Config) -> anyhow::Result<YggTheme> {
    load_theme_path_for(
        path,
        TerminalCapabilities::detect(config.color, config.plain),
        terminal_background(),
    )
}

fn load_resolved_theme_for(
    path: &Path,
    source_text: &str,
    capabilities: TerminalCapabilities,
    background: TerminalBackground,
) -> anyhow::Result<YggTheme> {
    let fallback_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Custom theme");
    load_theme_source_for(
        source_text,
        &path.display().to_string(),
        ThemeSource::File(path.to_owned()),
        fallback_name,
        capabilities,
        background,
    )
}

/// Compile text already read by the shared resource resolver. This keeps
/// secure descriptor traversal, trust, diagnostics, and precedence in the
/// shared layer while retaining the resolved path for inspectability/reload.
#[allow(dead_code)]
pub fn load_resolved_theme(
    path: &Path,
    source_text: &str,
    config: &Config,
) -> anyhow::Result<YggTheme> {
    load_resolved_theme_for(
        path,
        source_text,
        TerminalCapabilities::detect(config.color, config.plain),
        terminal_background(),
    )
}

#[allow(dead_code)]
pub fn bundled_theme_summaries() -> Vec<ThemeSummary> {
    theme_pack::THEMES
        .iter()
        .filter_map(|bundled| {
            theme_schema::parse_theme(bundled.source, bundled.id, TerminalBackground::Unknown)
                .ok()
                .map(|parsed| ThemeSummary {
                    id: bundled.id.to_owned(),
                    name: if parsed.metadata.name.is_empty() {
                        bundled.id.to_owned()
                    } else {
                        parsed.metadata.name
                    },
                    description: parsed.metadata.description,
                    source: ThemeSource::Bundled(bundled.id),
                })
        })
        .collect()
}

/// Load a named theme or return an error without altering the current theme.
pub(crate) fn load_named_theme_for_background(
    name: &str,
    config: &Config,
    background: TerminalBackground,
) -> anyhow::Result<YggTheme> {
    let capabilities = TerminalCapabilities::detect(config.color, config.plain);
    if name.trim().eq_ignore_ascii_case(DEFAULT_THEME_NAME) {
        return Ok(default_theme_for(background, capabilities));
    }
    let file_name =
        theme_file_name(name).ok_or_else(|| anyhow::anyhow!("invalid theme name {name:?}"))?;
    let resource_name = file_name.strip_suffix(".toml").unwrap_or(&file_name);
    let resolver = ResourceResolver::new(config.workspace.clone(), config.workspace_trusted);
    let snapshot = resolver.discover(ResourceKind::Theme, &config.theme_paths);
    if let Some(resource) = snapshot.get(resource_name) {
        let source = resolver.read_text(resource)?;
        return load_resolved_theme_for(&resource.path, &source, capabilities, background);
    }
    load_bundled_theme_for(name, capabilities, background)
}

/// Load a named theme or return an error without altering the current theme.
#[allow(dead_code)]
pub fn load_named_theme(name: &str, config: &Config) -> anyhow::Result<YggTheme> {
    load_named_theme_for_background(name, config, terminal_background())
}

/// Load the startup theme for an already-detected terminal background. Missing
/// or malformed files intentionally fall back to Ygg's default token set instead
/// of affecting launch/print mode.
pub(crate) fn load_theme_for_background(
    config: &Config,
    background: TerminalBackground,
) -> YggTheme {
    match config
        .theme
        .as_deref()
        .map(|name| load_named_theme_for_background(name, config, background))
    {
        Some(Ok(theme)) => theme,
        _ => default_theme_for(
            background,
            TerminalCapabilities::detect(config.color, config.plain),
        ),
    }
}

/// Load the startup theme. Missing or malformed files intentionally fall back
/// to Ygg's default token set instead of affecting launch/print mode.
pub fn load_theme(config: &Config) -> YggTheme {
    load_theme_for_background(config, terminal_background())
}

#[cfg(test)]
fn available_themes_from_dirs(global: &Path, project: &Path) -> Vec<String> {
    let mut names = BTreeSet::new();
    for directory in [global, project] {
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
                    if let Some(name) = path.file_stem().and_then(|name| name.to_str()) {
                        names.insert(name.to_owned());
                    }
                }
            }
        }
    }
    names.into_iter().collect()
}

/// List the built-in default plus global and project theme names.
pub fn available_themes(config: &Config) -> Vec<String> {
    let resolver = ResourceResolver::new(config.workspace.clone(), config.workspace_trusted);
    let snapshot = resolver.discover(ResourceKind::Theme, &config.theme_paths);
    let mut names = snapshot
        .resources()
        .iter()
        .map(|resource| resource.name.clone())
        .collect::<Vec<_>>();
    names.extend(theme_pack::THEMES.iter().map(|theme| theme.id.to_owned()));
    names.sort();
    names.dedup();
    names.retain(|name| !name.eq_ignore_ascii_case(DEFAULT_THEME_NAME));
    names.insert(0, DEFAULT_THEME_NAME.to_owned());
    names
}

fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

pub(crate) fn classify_model_text(text: &str) -> Option<ModelLab> {
    if contains_any(text, &["claude", "anthropic"]) {
        Some(ModelLab::Anthropic)
    } else if text.contains("deepseek") {
        Some(ModelLab::DeepSeek)
    } else if contains_any(text, &["gemini", "gemma", "google"]) {
        Some(ModelLab::Google)
    } else if contains_any(text, &["grok", "x.ai", "x-ai", "spacexai"]) {
        Some(ModelLab::XAi)
    } else if contains_any(text, &["llama", "meta-ai", "meta/"]) {
        Some(ModelLab::Meta)
    } else if contains_any(
        text,
        &["mistral", "mixtral", "codestral", "ministral", "devstral"],
    ) {
        Some(ModelLab::Mistral)
    } else if contains_any(text, &["qwen", "qwq", "alibaba", "dashscope"]) {
        Some(ModelLab::Alibaba)
    } else if text.contains("minimax") {
        Some(ModelLab::MiniMax)
    } else if contains_any(text, &["kimi", "moonshot"]) {
        Some(ModelLab::Kimi)
    } else if contains_any(text, &["z-ai", "zhipu", "chatglm", "glm-"]) {
        Some(ModelLab::ZAi)
    } else if contains_any(text, &["nvidia", "nemotron"]) {
        Some(ModelLab::Nvidia)
    } else if contains_any(text, &["xiaomi", "mimo-"]) {
        Some(ModelLab::Xiaomi)
    } else if contains_any(text, &["cohere", "command-r", "command-a"]) {
        Some(ModelLab::Cohere)
    } else if contains_any(text, &["amazon", "bedrock", "nova-"]) {
        Some(ModelLab::Amazon)
    } else if contains_any(text, &["microsoft", "azure", "phi-", "mai-"]) {
        Some(ModelLab::Microsoft)
    } else if contains_any(text, &["ai21", "jamba"]) {
        Some(ModelLab::Ai21)
    } else if contains_any(text, &["bytedance", "doubao", "seed-"]) {
        Some(ModelLab::ByteDance)
    } else if contains_any(text, &["perplexity", "sonar-"]) {
        Some(ModelLab::Perplexity)
    } else if contains_any(text, &["ibm", "granite"]) {
        Some(ModelLab::Ibm)
    } else if contains_any(text, &["baidu", "ernie"]) {
        Some(ModelLab::Baidu)
    } else if contains_any(text, &["tencent", "hunyuan"]) {
        Some(ModelLab::Tencent)
    } else if contains_any(text, &["allenai", "allen-ai", "olmo"]) {
        Some(ModelLab::AllenAi)
    } else if contains_any(text, &["openai", "chatgpt", "codex", "gpt-"])
        || text.starts_with("o1")
        || text.starts_with("o3")
        || text.starts_with("o4")
    {
        Some(ModelLab::OpenAi)
    } else {
        None
    }
}

fn classify_model_identity(id: &str, api_name: &str, endpoint: &str) -> ModelLab {
    let model_text = format!(
        "{} {}",
        id.to_ascii_lowercase(),
        api_name.to_ascii_lowercase()
    );
    if let Some(lab) = classify_model_text(&model_text) {
        return lab;
    }

    let endpoint = endpoint.trim().to_ascii_lowercase();
    match endpoint.as_str() {
        "xai" | "x-ai" => ModelLab::XAi,
        "meta" | "meta-ai" => ModelLab::Meta,
        "zai" | "z-ai" | "zhipu" => ModelLab::ZAi,
        "aws" | "amazon-bedrock" => ModelLab::Amazon,
        "ai2" | "allen-ai" => ModelLab::AllenAi,
        _ => classify_model_text(&endpoint).unwrap_or(ModelLab::Unknown),
    }
}

pub(crate) fn model_spec_lab(model: &ModelSpec) -> ModelLab {
    classify_model_identity(&model.id.0, &model.api_name, &model.endpoint.0)
}

pub(crate) fn model_lab(model: &Model) -> ModelLab {
    model_spec_lab(&model.spec)
}

/// Version-stable lab-to-colour assignment used only when a prompt is first
/// appended (and as a legacy replay fallback). The exact result is persisted,
/// so future palette changes cannot recolour existing prompts.
fn prompt_color_for_lab(lab: ModelLab) -> String {
    let source = lab.source_color().unwrap_or(DEFAULT_ACCENT);
    balance_foreground(source, TerminalBackground::Unknown)
}

pub(crate) fn prompt_color_for_model_id(model_id: &str) -> String {
    let normalized = model_id.trim().to_ascii_lowercase();
    prompt_color_for_lab(classify_model_identity(&normalized, "", ""))
}

pub(crate) fn prompt_color_for_model(model: &Model) -> String {
    prompt_color_for_lab(model_lab(model))
}

/// Install the active lab color into dedicated chrome/assistant tokens. A
/// custom theme keeps its existing accent roles unless it sets
/// `use_lab_color = true` or a lab source such as `anthropic = "#..."` under
/// `[model]`; model colors are always balanced for the terminal background.
fn apply_model_lab_for(theme: &mut YggTheme, lab: ModelLab, background: TerminalBackground) {
    let configured_key = format!("model.{}", lab.key());
    let configured = theme
        .resolve::<String>(&configured_key)
        .filter(|color| parse_hex_color(color).is_some());
    let use_lab_color = configured.is_some()
        || theme
            .resolve::<bool>("model.use_lab_color")
            .unwrap_or(false);
    let source = configured
        .or_else(|| {
            if use_lab_color {
                lab.source_color().map(str::to_owned)
            } else {
                None
            }
        })
        .or_else(|| {
            theme
                .resolve::<String>("accent")
                .filter(|color| parse_hex_color(color).is_some())
        })
        .unwrap_or_else(|| DEFAULT_ACCENT.to_owned());
    let assistant_source = theme
        .resolve::<String>("assistant_msg_text")
        .unwrap_or_else(|| "default".into());
    theme.override_token("model_accent", &balance_foreground(&source, background));
    theme.override_token(
        "model_assistant",
        &balance_foreground(&assistant_source, background),
    );
}

pub(crate) fn apply_model_lab(theme: &mut YggTheme, lab: ModelLab) {
    let background = theme.background;
    apply_model_lab_for(theme, lab, background);
}

/// Build sexy-tui's select-list closures from the current model theme.
#[cfg(test)]
pub fn select_list_theme(theme: &YggTheme) -> SelectListTheme {
    SelectListTheme {
        selected_prefix: foreground(theme, "model_accent"),
        selected_text: bold_foreground(theme, "model_accent"),
        description: foreground(theme, "muted"),
        scroll_info: foreground(theme, "muted"),
        no_match: foreground(theme, "error"),
    }
}

#[allow(dead_code)]
fn shared_foreground(
    theme: Arc<Mutex<YggTheme>>,
    token: &'static str,
) -> Box<dyn Fn(&str) -> String> {
    Box::new(move |text| {
        theme
            .lock()
            .expect("picker theme mutex poisoned")
            .fg(token, text)
    })
}

#[allow(dead_code)]
fn shared_bold_foreground(
    theme: Arc<Mutex<YggTheme>>,
    token: &'static str,
) -> Box<dyn Fn(&str) -> String> {
    Box::new(move |text| {
        let theme = theme.lock().expect("picker theme mutex poisoned");
        theme.bold(&theme.fg(token, text))
    })
}

/// A picker theme whose model accent can change as selection moves.
#[allow(dead_code)]
pub fn dynamic_select_list_theme(theme: Arc<Mutex<YggTheme>>) -> SelectListTheme {
    SelectListTheme {
        selected_prefix: shared_foreground(theme.clone(), "model_accent"),
        selected_text: shared_bold_foreground(theme.clone(), "model_accent"),
        description: shared_foreground(theme.clone(), "muted"),
        scroll_info: shared_foreground(theme.clone(), "muted"),
        no_match: shared_foreground(theme, "error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompactionPolicy, Mode, ResumeSelector, SandboxPolicy};

    fn config(workspace: PathBuf) -> Config {
        Config {
            workspace: workspace.clone(),
            invocation_cwd: workspace,
            model: None,
            model_explicit: false,
            reasoning: ygg_ai::ReasoningConfig::Off,
            reasoning_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: SandboxPolicy::default(),
            theme: None,
            theme_paths: vec![],
            color: crate::config::ColorMode::Auto,
            mouse: crate::config::MouseMode::Auto,
            plain: false,
            session_dir: PathBuf::from("sessions"),
            compaction: CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(40),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: vec![],
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
            skill_paths: vec![],
            extension_paths: vec![],
            enabled_extensions: vec![],
            trusted_extensions: vec![],
            invocation_trusted_extensions: vec![],
            tools: crate::config::ToolPolicy::default(),
            context_files: true,
            offline: true,
            workspace_trusted: true,
        }
    }

    fn contrast(left: Rgb, right: Rgb) -> f64 {
        let (dark, light) = if relative_luminance(left) < relative_luminance(right) {
            (left, right)
        } else {
            (right, left)
        };
        (relative_luminance(light) + 0.05) / (relative_luminance(dark) + 0.05)
    }

    #[test]
    fn select_list_theme_builds_and_preserves_text() {
        let theme = select_list_theme(&test_theme());
        assert!((theme.selected_text)("x").contains('x'));
        assert!((theme.no_match)("x").contains('x'));
    }

    #[test]
    fn project_theme_wins_and_available_themes_deduplicate() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path().to_owned());
        let project = project_theme_dir(&config);
        let global = directory.path().join("global-themes");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(project.join("project.toml"), "accent = 'blue'").unwrap();
        std::fs::write(project.join("shared.toml"), "accent = 'green'").unwrap();
        std::fs::write(global.join("global.toml"), "accent = 'red'").unwrap();
        std::fs::write(global.join("shared.toml"), "accent = 'red'").unwrap();
        assert_eq!(
            theme_path("shared", &config),
            Some(project.join("shared.toml").canonicalize().unwrap())
        );
        assert_eq!(
            available_themes_from_dirs(&global, &project),
            vec![
                "global".to_owned(),
                "project".to_owned(),
                "shared".to_owned()
            ]
        );
    }

    #[test]
    fn builtin_default_is_always_a_theme_choice() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path().to_owned());
        let names = available_themes(&config);
        assert_eq!(names.first().map(String::as_str), Some(DEFAULT_THEME_NAME));
        assert!(load_named_theme(DEFAULT_THEME_NAME, &config).is_ok());
    }

    #[test]
    fn ten_bundled_themes_load_for_every_terminal_profile() {
        assert_eq!(theme_pack::THEMES.len(), 10);
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        let black = Rgb {
            red: 0,
            green: 0,
            blue: 0,
        };
        let white = Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };
        for background in [
            TerminalBackground::Dark,
            TerminalBackground::Light,
            TerminalBackground::Unknown,
        ] {
            for bundled in theme_pack::THEMES {
                let theme = load_bundled_theme_for(bundled.id, capabilities, background)
                    .unwrap_or_else(|error| panic!("{} ({background:?}): {error}", bundled.id));
                assert!(!theme.metadata().name.is_empty(), "{}", bundled.id);
                assert_eq!(theme.source(), &ThemeSource::Bundled(bundled.id));
                assert_eq!(
                    theme.resolve::<String>("surface").as_deref(),
                    Some("default")
                );
                assert_eq!(
                    theme.resolve::<String>("overlay").as_deref(),
                    Some("default")
                );
                for glyph in [
                    "top_left",
                    "top_right",
                    "bottom_left",
                    "bottom_right",
                    "horizontal",
                    "vertical",
                ] {
                    assert_eq!(sexy_tui_rs::display_width(theme.glyph(glyph)), 1);
                }
                let accent = theme
                    .resolve::<String>("accent")
                    .and_then(|value| parse_hex_color(&value))
                    .expect("adaptive bundled accent");
                match background {
                    TerminalBackground::Dark => {
                        assert!(contrast(accent, black) >= 5.5, "{} dark", bundled.id)
                    }
                    TerminalBackground::Light => {
                        assert!(contrast(accent, white) >= 5.5, "{} light", bundled.id)
                    }
                    TerminalBackground::Unknown => {
                        assert!(contrast(accent, black) >= 4.5, "{} black", bundled.id);
                        assert!(contrast(accent, white) >= 4.5, "{} white", bundled.id);
                        for token in ["diff_added_bg", "diff_removed_bg"] {
                            assert_eq!(
                                theme.resolve::<String>(token).as_deref(),
                                Some("default"),
                                "{} {token} should not paint unknown terminal backgrounds",
                                bundled.id
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bundled_themes_have_distinct_design_signatures() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        let themes = theme_pack::THEMES
            .iter()
            .map(|bundled| {
                load_bundled_theme_for(bundled.id, capabilities, TerminalBackground::Unknown)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let signatures = themes
            .iter()
            .map(|theme| {
                format!(
                    "{}|{}|{}|{}|{}|{:?}|{}",
                    theme.resolve::<String>("accent").unwrap(),
                    theme.glyph("wordmark"),
                    theme.glyph("prompt"),
                    theme.glyph("separator"),
                    theme.glyph("top_left"),
                    theme.layout().density,
                    theme.layout().transcript_inset,
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(signatures.len(), 10);
        assert_eq!(
            themes
                .iter()
                .map(|theme| theme.glyph("wordmark"))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            10
        );
        assert!(
            themes
                .iter()
                .map(|theme| theme.glyph("prompt"))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                >= 8
        );
        assert!(
            themes
                .iter()
                .map(|theme| theme.glyph("top_left"))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                >= 5
        );
        assert_eq!(
            themes
                .iter()
                .map(|theme| format!("{:?}", theme.layout().density))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert_eq!(bundled_theme_summaries().len(), 10);
    }

    #[test]
    fn bundled_themes_never_inherit_model_lab_colors() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        for bundled in theme_pack::THEMES {
            let theme =
                load_bundled_theme_for(bundled.id, capabilities, TerminalBackground::Unknown)
                    .unwrap_or_else(|error| panic!("{} failed to load: {error}", bundled.id));
            assert_eq!(
                theme.resolve::<bool>("model.use_lab_color"),
                Some(false),
                "{} leaked the default theme's model palette",
                bundled.id
            );
        }
        assert_eq!(
            default_theme_for(TerminalBackground::Unknown, capabilities)
                .resolve::<bool>("model.use_lab_color"),
            Some(true)
        );
    }

    #[test]
    fn every_bundled_theme_degrades_across_color_unicode_and_narrow_profiles() {
        for bundled in theme_pack::THEMES {
            for depth in [
                ColorDepth::Ansi16,
                ColorDepth::Ansi256,
                ColorDepth::TrueColor,
            ] {
                let capabilities = TerminalCapabilities::test(true, true, depth);
                let theme =
                    load_bundled_theme_for(bundled.id, capabilities, TerminalBackground::Unknown)
                        .unwrap();
                let accent = theme.fg("accent", "accent-probe");
                assert_eq!(
                    sexy_tui_rs::strip_terminal_sequences(&accent),
                    "accent-probe",
                    "{} {depth:?}",
                    bundled.id
                );
                assert!(!accent.contains("48;"), "{} {depth:?}", bundled.id);
                match depth {
                    ColorDepth::Ansi16 => {
                        assert!(!accent.contains("38;5;") && !accent.contains("38;2;"))
                    }
                    ColorDepth::Ansi256 => assert!(accent.contains("38;5;")),
                    ColorDepth::TrueColor => assert!(accent.contains("38;2;")),
                    ColorDepth::None => unreachable!(),
                }

                assert!(theme
                    .semantic_role_names()
                    .any(|role| role == "extension.status"));
                assert!(theme
                    .semantic_role_names()
                    .any(|role| role == "extension.header"));
                let extension = theme.apply_semantic_role("extension.status", "extension-probe");
                assert_eq!(
                    sexy_tui_rs::strip_terminal_sequences(&extension),
                    "extension-probe"
                );
                assert!(!extension.contains("48;"), "{} {depth:?}", bundled.id);
                let header = theme.apply_semantic_role("extension.header", "header-probe");
                assert_eq!(
                    sexy_tui_rs::strip_terminal_sequences(&header),
                    "header-probe"
                );
                assert!(!header.contains("48;"), "{} {depth:?}", bundled.id);

                let breakpoint = theme.layout().narrow_breakpoint;
                assert!(theme.layout_for_width(breakpoint.saturating_sub(1)).narrow);
                assert!(!theme.layout_for_width(breakpoint).narrow);
            }

            let plain = load_bundled_theme_for(
                bundled.id,
                TerminalCapabilities::test(false, false, ColorDepth::None),
                TerminalBackground::Unknown,
            )
            .unwrap();
            assert_eq!(plain.fg("accent", "plain-probe"), "plain-probe");
            assert_eq!(
                plain.apply_semantic_role("extension.status", "extension-probe"),
                "extension-probe"
            );
            assert_eq!(
                plain.apply_semantic_role("extension.header", "header-probe"),
                "header-probe"
            );
            assert_eq!(plain.glyph("top_left"), "+");
            assert_eq!(plain.glyph("prompt"), ">");
        }
    }

    #[test]
    fn semantic_roles_and_glyphs_degrade_without_losing_text() {
        let truecolor = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        let theme =
            load_bundled_theme_for("bone-machine", truecolor, TerminalBackground::Unknown).unwrap();
        let confirmation = theme.apply_semantic_role("confirmation", "continue?");
        assert!(confirmation.contains("continue?"));
        assert!(confirmation.contains("\x1b[1m"));
        assert!(confirmation.contains("\x1b[7m"));
        assert_eq!(theme.glyph("prompt"), "»");

        let plain = TerminalCapabilities::test(false, false, ColorDepth::None);
        let theme =
            load_bundled_theme_for("bone-machine", plain, TerminalBackground::Unknown).unwrap();
        assert_eq!(
            theme.apply_semantic_role("confirmation", "continue?"),
            "continue?"
        );
        assert_eq!(theme.glyph("prompt"), ">");
        assert_eq!(theme.glyph("horizontal"), "=");
    }

    #[test]
    fn resolver_selected_theme_paths_are_bounded_validated_and_reloadable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom.toml");
        std::fs::write(
            &path,
            r##"
                [metadata]
                name = "Custom"
                [colors]
                accent = "#456789"
                [roles."extension.custom"]
                foreground = "accent"
                bold = true
                [glyphs]
                prompt = ":"
            "##,
        )
        .unwrap();
        let config = config(directory.path().to_owned());
        let theme = load_theme_path_for(
            &path,
            TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
            TerminalBackground::Unknown,
        )
        .unwrap();
        assert_eq!(theme.metadata().name, "Custom");
        assert_eq!(theme.source_path(), Some(path.as_path()));
        assert_eq!(theme.glyph("prompt"), ":");
        assert!(theme
            .apply_semantic_role("extension.custom", "custom")
            .contains("custom"));

        std::fs::write(
            &path,
            "[metadata]\nname = 'Reloaded'\n[glyphs]\nprompt = '#'\n",
        )
        .unwrap();
        let reloaded = theme.reload().unwrap();
        assert_eq!(reloaded.metadata().name, "Reloaded");
        assert_eq!(reloaded.glyph("prompt"), "#");

        let oversized = directory.path().join("oversized.toml");
        std::fs::write(&oversized, vec![b' '; MAX_THEME_BYTES as usize + 1]).unwrap();
        let error = load_theme_path(&oversized, &config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("too large"));
    }

    #[cfg(unix)]
    #[test]
    fn file_theme_reload_rejects_a_path_swapped_to_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.toml");
        let replacement = directory.path().join("replacement.toml");
        std::fs::write(&path, "[metadata]\nname = 'Initial'\n").unwrap();
        std::fs::write(&replacement, "[metadata]\nname = 'Replacement'\n").unwrap();
        let theme = load_theme_path_for(
            &path,
            TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
            TerminalBackground::Unknown,
        )
        .unwrap();

        std::fs::remove_file(&path).unwrap();
        symlink(&replacement, &path).unwrap();
        assert!(theme.reload().is_err());
    }

    #[test]
    fn model_identity_prefers_creator_markers_over_compatible_endpoint() {
        assert_eq!(
            classify_model_identity("claude-sonnet-4-5", "claude-sonnet", "openai"),
            ModelLab::Anthropic
        );
        assert_eq!(
            classify_model_identity("qwen3-coder", "qwen3-coder", "openai-compatible"),
            ModelLab::Alibaba
        );
        assert_eq!(
            classify_model_identity("gpt-5.4", "gpt-5.4", "openai-codex"),
            ModelLab::OpenAi
        );
        assert_eq!(
            classify_model_identity("deepseek-v4-pro", "deepseek-v4-pro", "deepseek"),
            ModelLab::DeepSeek
        );
        assert_eq!(
            classify_model_identity("custom", "custom", "xai"),
            ModelLab::XAi
        );
        assert_eq!(
            classify_model_identity("custom", "custom", "meta"),
            ModelLab::Meta
        );
    }

    #[test]
    fn source_colors_track_recognizable_lab_chart_colors() {
        assert_eq!(ModelLab::OpenAi.source_color(), Some("#1f1f1f"));
        assert_eq!(ModelLab::Anthropic.source_color(), Some("#cc785c"));
        assert_eq!(ModelLab::Google.source_color(), Some("#34a853"));
        assert_eq!(ModelLab::DeepSeek.source_color(), Some("#2243e6"));
        assert_eq!(ModelLab::Mistral.source_color(), Some("#fd6f00"));
    }

    #[test]
    fn diff_rows_use_standard_background_surfaces_and_normal_foregrounds() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        for (background, expected_add, expected_remove) in [
            (TerminalBackground::Dark, "#10261e", "#2a171b"),
            (TerminalBackground::Light, "#e8f6ee", "#fcebed"),
        ] {
            let theme = default_theme_for(background, capabilities);
            assert_eq!(
                theme.resolve::<String>("diff_added").as_deref(),
                Some("default")
            );
            assert_eq!(
                theme.resolve::<String>("diff_removed").as_deref(),
                Some("default")
            );
            assert_eq!(
                theme.resolve::<String>("diff_added_bg").as_deref(),
                Some(expected_add)
            );
            assert_eq!(
                theme.resolve::<String>("diff_removed_bg").as_deref(),
                Some(expected_remove)
            );
        }

        let unknown = default_theme_for(TerminalBackground::Unknown, capabilities);
        assert_eq!(
            unknown.resolve::<String>("diff_added_bg").as_deref(),
            Some("default")
        );
        assert_eq!(
            unknown.resolve::<String>("diff_removed_bg").as_deref(),
            Some("default")
        );
    }

    fn required_rgb_token(theme: &YggTheme, token: &str) -> Rgb {
        let value = theme
            .resolve::<String>(token)
            .unwrap_or_else(|| panic!("missing token {token}"));
        parse_hex_color(&value).unwrap_or_else(|| panic!("{token} was not RGB: {value}"))
    }

    #[test]
    fn standard_syntax_palette_contrasts_with_diff_surfaces() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        let syntax_tokens = STANDARD_SYNTAX_COLORS
            .iter()
            .map(|(token, _, _)| *token)
            .chain(["diff_added_marker", "diff_removed_marker"]);

        for background in [TerminalBackground::Dark, TerminalBackground::Light] {
            let mut themes = vec![default_theme_for(background, capabilities)];
            themes.extend(theme_pack::THEMES.iter().map(|bundled| {
                load_bundled_theme_for(bundled.id, capabilities, background)
                    .unwrap_or_else(|error| panic!("{}: {error}", bundled.id))
            }));

            for theme in themes {
                for surface in ["diff_added_bg", "diff_removed_bg"] {
                    let surface_color = required_rgb_token(&theme, surface);
                    for token in syntax_tokens.clone() {
                        let foreground = required_rgb_token(&theme, token);
                        assert!(
                            contrast(foreground, surface_color) >= 4.5,
                            "{:?} {:?} {token} on {surface}",
                            theme.source(),
                            background
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_background_uses_no_fixed_diff_surfaces_and_universal_syntax() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        let theme = default_theme_for(TerminalBackground::Unknown, capabilities);
        let black = Rgb {
            red: 0,
            green: 0,
            blue: 0,
        };
        let white = Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };

        for surface in ["diff_added_bg", "diff_removed_bg"] {
            assert_eq!(theme.resolve::<String>(surface).as_deref(), Some("default"));
        }
        for token in STANDARD_SYNTAX_COLORS
            .iter()
            .map(|(token, _, _)| *token)
            .chain(["diff_added_marker", "diff_removed_marker"])
        {
            let foreground = required_rgb_token(&theme, token);
            assert!(contrast(foreground, black) >= 4.5, "{token} on black");
            assert!(contrast(foreground, white) >= 4.5, "{token} on white");
        }
    }

    #[test]
    fn composer_idle_border_moves_toward_the_terminal_background() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
        let accent = (96, 80, 64);
        let dark = default_theme_for(TerminalBackground::Dark, capabilities);
        let light = default_theme_for(TerminalBackground::Light, capabilities);
        let unknown = default_theme_for(TerminalBackground::Unknown, capabilities);

        assert_eq!(dark.composer_idle_rgb(accent), (12, 10, 8));
        assert_eq!(light.composer_idle_rgb(accent), (236, 234, 232));
        assert_eq!(unknown.composer_idle_rgb(accent), (12, 10, 8));
    }

    #[test]
    fn openai_idle_border_does_not_quantize_to_black_on_light_profiles() {
        for color in [
            ColorDepth::Ansi16,
            ColorDepth::Ansi256,
            ColorDepth::TrueColor,
        ] {
            let capabilities = TerminalCapabilities::test(true, true, color);
            let mut theme = default_theme_for(TerminalBackground::Light, capabilities);
            apply_model_lab_for(&mut theme, ModelLab::OpenAi, TerminalBackground::Light);
            let accent = theme.role_rgb("model_accent").expect("model accent");
            let idle = theme.composer_idle_rgb(accent);
            assert!(idle.0 > accent.0 && idle.1 > accent.1 && idle.2 > accent.2);

            let rendered = theme.rgb_fg(idle, "─");
            assert!(!rendered.contains("\x1b[30m"), "{color:?}: {rendered:?}");
            assert!(!rendered.contains("38;5;0m"), "{color:?}: {rendered:?}");
        }
    }

    #[test]
    fn active_lab_populates_the_dedicated_model_token() {
        let mut theme = test_theme();
        apply_model_lab_for(&mut theme, ModelLab::OpenAi, TerminalBackground::Unknown);
        assert_eq!(
            theme.resolve::<String>("model_accent").as_deref(),
            Some("#767676")
        );
        apply_model_lab_for(&mut theme, ModelLab::Anthropic, TerminalBackground::Unknown);
        assert_eq!(
            theme.resolve::<String>("model_accent").as_deref(),
            Some("#a9634c")
        );
        assert_eq!(
            theme.resolve::<String>("model_assistant").as_deref(),
            Some("default")
        );
    }

    #[test]
    fn rich_text_roles_are_semantic_not_model_accent() {
        let mut theme = test_theme();
        apply_model_lab_for(&mut theme, ModelLab::Anthropic, TerminalBackground::Unknown);
        let renderer = theme.rich_renderer();
        let accent = sexy_tui_rs::Color::Rgb(169, 99, 76);
        // Semantic roles in the rich renderer use their own colour tokens,
        // NOT the model accent. Only Ygg's structural chrome uses model colors.
        for role in [
            TextRole::Heading,
            TextRole::ListMarker,
            TextRole::InlineCode,
            TextRole::Border,
            TextRole::Code,
            TextRole::Link,
            TextRole::Emphasis,
            TextRole::Strong,
        ] {
            assert_ne!(
                renderer.theme().style(role).foreground,
                accent,
                "{role:?} must not use the model accent"
            );
        }
    }

    #[test]
    fn named_theme_roles_survive_without_model_palette_opt_in() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::Ansi16);
        let mut theme = YggTheme::new(
            SexyTheme::load(None, CapabilityTier::Baseline),
            capabilities,
            TerminalBackground::Unknown,
        );
        theme.override_token("accent", "#005f5f");
        theme.override_token("assistant_msg_text", "#7a3e65");
        apply_model_lab_for(&mut theme, ModelLab::Anthropic, TerminalBackground::Unknown);
        assert_eq!(
            theme.resolve::<String>("model_accent"),
            Some(balance_foreground("#005f5f", TerminalBackground::Unknown))
        );
        assert_eq!(
            theme.resolve::<String>("model_assistant"),
            Some(balance_foreground("#7a3e65", TerminalBackground::Unknown))
        );
    }

    #[test]
    fn balanced_lab_colors_have_terminal_safe_contrast() {
        let labs = [
            ModelLab::OpenAi,
            ModelLab::Anthropic,
            ModelLab::Google,
            ModelLab::XAi,
            ModelLab::Meta,
            ModelLab::Mistral,
            ModelLab::DeepSeek,
            ModelLab::Alibaba,
            ModelLab::MiniMax,
            ModelLab::Kimi,
            ModelLab::ZAi,
            ModelLab::Nvidia,
            ModelLab::Xiaomi,
            ModelLab::Cohere,
            ModelLab::Amazon,
            ModelLab::Microsoft,
            ModelLab::Ai21,
            ModelLab::ByteDance,
            ModelLab::Perplexity,
            ModelLab::Ibm,
            ModelLab::Baidu,
            ModelLab::Tencent,
            ModelLab::AllenAi,
        ];
        let black = Rgb {
            red: 0,
            green: 0,
            blue: 0,
        };
        let white = Rgb {
            red: 255,
            green: 255,
            blue: 255,
        };
        let dark = Rgb {
            red: 18,
            green: 20,
            blue: 22,
        };
        let light = Rgb {
            red: 250,
            green: 250,
            blue: 250,
        };

        for lab in labs {
            let source = lab.source_color().unwrap();
            let universal =
                parse_hex_color(&balance_foreground(source, TerminalBackground::Unknown)).unwrap();
            assert!(contrast(universal, black) >= 4.5, "{lab:?} on black");
            assert!(contrast(universal, white) >= 4.5, "{lab:?} on white");

            let dark_color =
                parse_hex_color(&balance_foreground(source, TerminalBackground::Dark)).unwrap();
            assert!(contrast(dark_color, dark) >= 5.5, "{lab:?} on dark");

            let light_color =
                parse_hex_color(&balance_foreground(source, TerminalBackground::Light)).unwrap();
            assert!(contrast(light_color, light) >= 5.5, "{lab:?} on light");
        }
    }

    #[test]
    fn styling_degrades_without_changing_text() {
        let plain = test_theme_with(TerminalCapabilities::test(false, false, ColorDepth::None));
        assert_eq!(plain.fg("model_accent", "ygg"), "ygg");
        assert_eq!(plain.bold("ygg"), "ygg");
        assert_eq!(plain.dim("ygg"), "ygg");

        let ansi16 = test_theme_with(TerminalCapabilities::test(true, true, ColorDepth::Ansi16));
        let styled = ansi16.fg("model_accent", "ygg");
        assert!(styled.starts_with("\x1b["));
        let dimmed = ansi16.dim("ygg");
        assert_ne!(dimmed, "ygg");
        assert!(!dimmed.contains("\x1b[2m"));
        assert!(!styled.contains("38;2;"));
        assert!(!styled.contains("38;5;"));

        let ansi256 = test_theme_with(TerminalCapabilities::test(true, true, ColorDepth::Ansi256));
        assert!(ansi256.fg("model_accent", "ygg").contains("38;5;"));

        let truecolor = test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::TrueColor,
        ));
        assert!(truecolor.fg("model_accent", "ygg").contains("38;2;"));
    }

    #[test]
    fn prompt_colors_follow_the_lab_palette_instead_of_exact_model_aliases() {
        let openai = prompt_color_for_lab(ModelLab::OpenAi);
        assert_eq!(openai, prompt_color_for_model_id(" GPT-5.6 "));
        assert_eq!(openai, prompt_color_for_model_id("gpt-5.6-sol"));
        assert_eq!(openai, prompt_color_for_model_id("openai/codex-mini"));

        let deepseek = prompt_color_for_lab(ModelLab::DeepSeek);
        assert_eq!(
            deepseek,
            prompt_color_for_model_id("opencode/deepseek-v4-flash-free")
        );
        assert_eq!(deepseek, prompt_color_for_model_id("deepseek-v4-pro"));
        assert_ne!(openai, deepseek);

        for color in [openai, deepseek] {
            assert_eq!(color.len(), 7);
            assert!(color.starts_with('#'));
            assert!(color[1..].bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn every_recognized_model_family_uses_its_lab_prompt_color() {
        for (model_id, lab) in [
            ("gpt-5.6-terra", ModelLab::OpenAi),
            ("claude-opus-4.5", ModelLab::Anthropic),
            ("gemini-3-pro", ModelLab::Google),
            ("grok-4", ModelLab::XAi),
            ("llama-4", ModelLab::Meta),
            ("mistral-large", ModelLab::Mistral),
            ("deepseek-v4", ModelLab::DeepSeek),
            ("qwen3-coder", ModelLab::Alibaba),
            ("minimax-m2", ModelLab::MiniMax),
            ("kimi-k2", ModelLab::Kimi),
            ("glm-5", ModelLab::ZAi),
            ("nemotron-4", ModelLab::Nvidia),
            ("mimo-v2", ModelLab::Xiaomi),
            ("command-r-plus", ModelLab::Cohere),
            ("nova-pro", ModelLab::Amazon),
            ("phi-4", ModelLab::Microsoft),
            ("jamba-large", ModelLab::Ai21),
            ("doubao-pro", ModelLab::ByteDance),
            ("sonar-pro", ModelLab::Perplexity),
            ("granite-4", ModelLab::Ibm),
            ("ernie-5", ModelLab::Baidu),
            ("hunyuan-t1", ModelLab::Tencent),
            ("olmo-3", ModelLab::AllenAi),
        ] {
            assert_eq!(
                prompt_color_for_model_id(model_id),
                prompt_color_for_lab(lab),
                "{model_id} did not use the {lab:?} prompt color"
            );
        }
    }

    #[test]
    fn exact_prompt_background_degrades_without_emitting_unsafe_data() {
        let plain = test_theme_with(TerminalCapabilities::test(false, false, ColorDepth::None));
        assert_eq!(plain.prompt_color_cell(Some("#123456"), "> "), "> ");

        let ansi16 = test_theme_with(TerminalCapabilities::test(true, true, ColorDepth::Ansi16));
        let rendered = ansi16.prompt_color_cell(Some("#123456"), "> ");
        assert!(rendered.contains("> "));
        assert!(rendered.contains("\x1b["));
        assert!(!rendered.contains("48;2;") && !rendered.contains("48;5;"));

        let ansi256 = test_theme_with(TerminalCapabilities::test(true, true, ColorDepth::Ansi256));
        assert!(ansi256
            .prompt_color_cell(Some("#123456"), "> ")
            .contains("48;5;"));

        let truecolor = test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::TrueColor,
        ));
        let rendered = truecolor.prompt_color_cell(Some("#123456"), "> ");
        assert!(rendered.contains("48;2;18;52;86m"), "{rendered:?}");
        assert_eq!(
            truecolor.prompt_color_cell(Some("#12\u{1b}3456"), "> "),
            "> "
        );
    }

    #[test]
    fn ansi16_light_and_dark_balancing_never_paints_a_background() {
        let capabilities = TerminalCapabilities::test(true, true, ColorDepth::Ansi16);
        for background in [TerminalBackground::Light, TerminalBackground::Dark] {
            let mut theme = default_theme_for(background, capabilities);
            apply_model_lab_for(&mut theme, ModelLab::Anthropic, background);
            let rendered = theme.fg("model_accent", "model");
            assert!(rendered.contains("model"));
            assert!(!rendered.contains("48;"));
        }
    }

    #[test]
    fn colorfgbg_explicit_override_and_osc_rgb_detect_backgrounds() {
        assert_eq!(
            background_from_colorfgbg("15;0"),
            Some(TerminalBackground::Dark)
        );
        assert_eq!(
            background_from_colorfgbg("0;15"),
            Some(TerminalBackground::Light)
        );
        assert_eq!(
            background_from_override("universal"),
            Some(TerminalBackground::Unknown)
        );
        assert_eq!(
            background_from_terminal_rgb(12, 18, 24),
            TerminalBackground::Dark
        );
        assert_eq!(
            background_from_terminal_rgb(240, 240, 240),
            TerminalBackground::Light
        );
    }
}
