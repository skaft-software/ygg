//! Typed semantic styling. Text values never contain terminal escapes.

use std::fmt;

/// A terminal colour independent of a backend escape representation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    /// Keep the terminal's configured foreground/background.
    #[default]
    Default,
    /// One of the base sixteen ANSI colours (0..=15).
    Ansi16(u8),
    /// One of the xterm 256-colour entries.
    Indexed(u8),
    /// A 24-bit colour.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Parse `#RRGGBB`, a small set of ANSI names, `ansi:N`, or `index:N`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("default") || value.eq_ignore_ascii_case("none") {
            return Some(Self::Default);
        }
        if let Some(hex) = value.strip_prefix('#') {
            if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Some(Self::Rgb(
                    u8::from_str_radix(&hex[0..2], 16).ok()?,
                    u8::from_str_radix(&hex[2..4], 16).ok()?,
                    u8::from_str_radix(&hex[4..6], 16).ok()?,
                ));
            }
            return None;
        }
        if let Some(index) = value.strip_prefix("ansi:") {
            let index = index.parse::<u8>().ok()?;
            return (index < 16).then_some(Self::Ansi16(index));
        }
        if let Some(index) = value.strip_prefix("index:") {
            return index.parse::<u8>().ok().map(Self::Indexed);
        }
        let index = match value.to_ascii_lowercase().as_str() {
            "black" => 0,
            "red" => 1,
            "green" => 2,
            "yellow" => 3,
            "blue" => 4,
            "magenta" | "purple" => 5,
            "cyan" => 6,
            "white" => 7,
            "bright-black" | "gray" | "grey" => 8,
            "bright-red" => 9,
            "bright-green" => 10,
            "bright-yellow" => 11,
            "bright-blue" => 12,
            "bright-magenta" => 13,
            "bright-cyan" => 14,
            "bright-white" => 15,
            _ => return None,
        };
        Some(Self::Ansi16(index))
    }
}

impl fmt::Display for Color {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::Ansi16(index) => write!(formatter, "ansi:{index}"),
            Self::Indexed(index) => write!(formatter, "index:{index}"),
            Self::Rgb(red, green, blue) => write!(formatter, "#{red:02x}{green:02x}{blue:02x}"),
        }
    }
}

/// Text attributes. Unsupported attributes are ignored by the renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextAttributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
}

/// A complete typed text style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextStyle {
    pub foreground: Color,
    pub background: Color,
    pub attributes: TextAttributes,
}

impl TextStyle {
    pub const fn plain() -> Self {
        Self {
            foreground: Color::Default,
            background: Color::Default,
            attributes: TextAttributes {
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                strikethrough: false,
                inverse: false,
            },
        }
    }

    pub const fn foreground(mut self, color: Color) -> Self {
        self.foreground = color;
        self
    }

    pub const fn background(mut self, color: Color) -> Self {
        self.background = color;
        self
    }

    pub const fn bold(mut self) -> Self {
        self.attributes.bold = true;
        self
    }

    pub const fn italic(mut self) -> Self {
        self.attributes.italic = true;
        self
    }

    pub const fn underline(mut self) -> Self {
        self.attributes.underline = true;
        self
    }

    pub const fn strikethrough(mut self) -> Self {
        self.attributes.strikethrough = true;
        self
    }

    /// Merge an overlay style. Default colours do not replace explicit base
    /// colours; enabled attributes are additive.
    pub fn merge(self, overlay: Self) -> Self {
        Self {
            foreground: if overlay.foreground == Color::Default {
                self.foreground
            } else {
                overlay.foreground
            },
            background: if overlay.background == Color::Default {
                self.background
            } else {
                overlay.background
            },
            attributes: TextAttributes {
                bold: self.attributes.bold || overlay.attributes.bold,
                dim: self.attributes.dim || overlay.attributes.dim,
                italic: self.attributes.italic || overlay.attributes.italic,
                underline: self.attributes.underline || overlay.attributes.underline,
                strikethrough: self.attributes.strikethrough || overlay.attributes.strikethrough,
                inverse: self.attributes.inverse || overlay.attributes.inverse,
            },
        }
    }
}

/// Semantic roles shared by Markdown, diffs, widgets, and applications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextRole {
    Text,
    Muted,
    Subtle,
    Accent,
    Success,
    Warning,
    Error,
    Heading,
    Emphasis,
    Strong,
    InlineCode,
    Code,
    Quote,
    Border,
    Link,
    ListMarker,
    DiffAdd,
    DiffRemove,
    DiffContext,
    DiffHunk,
    DiffHeader,
    SyntaxComment,
    SyntaxKeyword,
    SyntaxFunction,
    SyntaxVariable,
    SyntaxString,
    SyntaxNumber,
    SyntaxType,
    SyntaxOperator,
    SyntaxPunctuation,
}

impl TextRole {
    /// Stable token name used by TOML themes and the compatibility API.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Text => "foreground",
            Self::Muted => "muted",
            Self::Subtle => "dim",
            Self::Accent => "accent",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Heading => "md_heading",
            Self::Emphasis => "md_emphasis",
            Self::Strong => "md_strong",
            Self::InlineCode => "md_code",
            Self::Code => "md_code_block",
            Self::Quote => "md_quote",
            Self::Border => "border",
            Self::Link => "md_link",
            Self::ListMarker => "md_list_bullet",
            Self::DiffAdd => "diff_added",
            Self::DiffRemove => "diff_removed",
            Self::DiffContext => "diff_context",
            Self::DiffHunk => "diff_hunk",
            Self::DiffHeader => "diff_header",
            Self::SyntaxComment => "syntax_comment",
            Self::SyntaxKeyword => "syntax_keyword",
            Self::SyntaxFunction => "syntax_function",
            Self::SyntaxVariable => "syntax_variable",
            Self::SyntaxString => "syntax_string",
            Self::SyntaxNumber => "syntax_number",
            Self::SyntaxType => "syntax_type",
            Self::SyntaxOperator => "syntax_operator",
            Self::SyntaxPunctuation => "syntax_punctuation",
        }
    }
}

/// Structural block styling. Backgrounds default to terminal-controlled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockStyle {
    pub text: TextStyle,
    pub border: TextStyle,
    pub background: Option<Color>,
    pub padding_left: u16,
    pub padding_right: u16,
    pub padding_top: u16,
    pub padding_bottom: u16,
}

/// Semantic block roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockRole {
    Code,
    Quote,
    Table,
    Detail,
}
