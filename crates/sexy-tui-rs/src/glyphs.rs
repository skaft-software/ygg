//! Conservative structural glyph sets with deterministic ASCII fallback.

use crate::capabilities::TerminalCapabilities;

/// Structural glyphs shared by lists, trees, quotes, borders, and truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphSet {
    pub vertical: &'static str,
    pub branch: &'static str,
    pub last_branch: &'static str,
    pub horizontal: &'static str,
    pub chevron: &'static str,
    pub bullet: &'static str,
    pub ellipsis: &'static str,
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub success: &'static str,
    pub error: &'static str,
    pub warning: &'static str,
    pub pending: &'static str,
    pub detail_collapsed: &'static str,
    pub detail_expanded: &'static str,
}

impl GlyphSet {
    pub const UNICODE: Self = Self {
        vertical: "│",
        branch: "├",
        last_branch: "└",
        horizontal: "─",
        chevron: "›",
        bullet: "•",
        ellipsis: "…",
        top_left: "┌",
        top_right: "┐",
        bottom_left: "└",
        bottom_right: "┘",
        success: "✓",
        error: "×",
        warning: "!",
        pending: "·",
        detail_collapsed: "▸",
        detail_expanded: "▾",
    };

    pub const ASCII: Self = Self {
        vertical: "|",
        branch: "|-",
        last_branch: "`-",
        horizontal: "-",
        chevron: ">",
        bullet: "*",
        ellipsis: "...",
        top_left: "+",
        top_right: "+",
        bottom_left: "+",
        bottom_right: "+",
        success: "+",
        error: "x",
        warning: "!",
        pending: ".",
        detail_collapsed: "[+]",
        detail_expanded: "[-]",
    };

    pub const fn for_capabilities(capabilities: TerminalCapabilities) -> Self {
        if capabilities.unicode && !capabilities.plain {
            Self::UNICODE
        } else {
            Self::ASCII
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_mode_never_requires_unicode_or_icon_fonts() {
        let glyphs = GlyphSet::for_capabilities(TerminalCapabilities::plain());
        assert_eq!(glyphs.vertical, "|");
        assert_eq!(glyphs.branch, "|-");
        assert!(glyphs.ellipsis.is_ascii());
    }
}
