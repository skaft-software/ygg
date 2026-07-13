#![allow(missing_docs)]

use sexy_tui_rs::theme::{capability::CapabilityTier, Theme};
use sexy_tui_rs::widgets::{MarkdownTheme, SelectListTheme};

use crate::config::Config;

fn foreground(theme: &Theme, token: &'static str) -> Box<dyn Fn(&str) -> String> {
    let theme = theme.clone();
    Box::new(move |text| theme.fg(token, text))
}

fn bold_foreground(theme: &Theme, token: &'static str) -> Box<dyn Fn(&str) -> String> {
    let theme = theme.clone();
    Box::new(move |text| theme.bold(&theme.fg(token, text)))
}

/// Load a configured theme without allowing a malformed theme to abort startup.
pub fn load_theme(config: &Config) -> Theme {
    let path = config.theme.as_deref();
    Theme::load(path, CapabilityTier::Baseline)
}

/// Build sexy-tui's markdown closures from a resolved theme.
pub fn markdown_theme(theme: &Theme) -> MarkdownTheme {
    MarkdownTheme {
        heading: bold_foreground(theme, "accent"),
        bold: bold_foreground(theme, "accent"),
        code: foreground(theme, "muted"),
        code_block_border: foreground(theme, "accent"),
    }
}

/// Build sexy-tui's select-list closures from a resolved theme.
pub fn select_list_theme(theme: &Theme) -> SelectListTheme {
    SelectListTheme {
        selected_prefix: foreground(theme, "accent"),
        selected_text: bold_foreground(theme, "accent"),
        description: foreground(theme, "muted"),
        scroll_info: foreground(theme, "muted"),
        no_match: foreground(theme, "error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_theme_builds_and_preserves_text() {
        let theme = markdown_theme(&Theme::default());
        assert!((theme.bold)("x").contains('x'));
    }

    #[test]
    fn select_list_theme_builds_and_preserves_text() {
        let theme = select_list_theme(&Theme::default());
        assert!((theme.selected_text)("x").contains('x'));
        assert!((theme.no_match)("x").contains('x'));
    }
}
