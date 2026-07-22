//! One display-cell measurement policy for all semantic rendering.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Treatment of East Asian Ambiguous characters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AmbiguousWidth {
    /// Match most Western terminal profiles.
    #[default]
    Narrow,
    /// Match terminals configured for CJK ambiguous-width rendering.
    Wide,
}

/// Width and tab policy used throughout layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidthPolicy {
    pub ambiguous: AmbiguousWidth,
    /// Tab stops, in cells. Zero is normalized to one.
    pub tab_stop: usize,
}

impl Default for WidthPolicy {
    fn default() -> Self {
        Self {
            ambiguous: AmbiguousWidth::Narrow,
            tab_stop: 4,
        }
    }
}

impl WidthPolicy {
    /// Width of one grapheme at the supplied current column.
    pub fn grapheme_width(self, grapheme: &str, column: usize) -> usize {
        if grapheme == "\t" {
            let stop = self.tab_stop.max(1);
            return stop - column % stop;
        }
        if grapheme == "\n" || grapheme == "\r" {
            return 0;
        }
        match self.ambiguous {
            AmbiguousWidth::Narrow => UnicodeWidthStr::width(grapheme),
            AmbiguousWidth::Wide => UnicodeWidthStr::width_cjk(grapheme),
        }
    }

    /// Width of a single logical line.
    pub fn line_width(self, text: &str) -> usize {
        self.line_width_from(text, 0)
    }

    /// Width of a line beginning at `start_column`.
    pub fn line_width_from(self, text: &str, start_column: usize) -> usize {
        let mut column = start_column;
        for grapheme in text.graphemes(true) {
            if grapheme == "\n" || grapheme == "\r" {
                break;
            }
            column = column.saturating_add(self.grapheme_width(grapheme, column));
        }
        column.saturating_sub(start_column)
    }

    /// Maximum line width in a possibly multiline value.
    pub fn max_width(self, text: &str) -> usize {
        text.split('\n')
            .map(|line| self.line_width(line.trim_end_matches('\r')))
            .max()
            .unwrap_or(0)
    }

    /// Expand tabs according to cell position. Returns borrowed text when no
    /// expansion is needed.
    pub fn expand_tabs(self, text: &str, start_column: usize) -> Cow<'_, str> {
        if !text.contains('\t') {
            return Cow::Borrowed(text);
        }
        let mut expanded = String::with_capacity(text.len());
        let mut column = start_column;
        for grapheme in text.graphemes(true) {
            match grapheme {
                "\t" => {
                    let width = self.grapheme_width(grapheme, column);
                    expanded.push_str(&" ".repeat(width));
                    column += width;
                }
                "\n" => {
                    expanded.push('\n');
                    column = 0;
                }
                _ => {
                    expanded.push_str(grapheme);
                    column += self.grapheme_width(grapheme, column);
                }
            }
        }
        Cow::Owned(expanded)
    }

    /// Return a cell-bounded prefix without splitting a grapheme.
    pub fn prefix(self, text: &str, max_width: usize) -> (&str, usize) {
        if max_width == 0 {
            return ("", 0);
        }
        let mut end = 0;
        let mut column = 0usize;
        for (offset, grapheme) in text.grapheme_indices(true) {
            if grapheme == "\n" || grapheme == "\r" {
                break;
            }
            let width = self.grapheme_width(grapheme, column);
            if column.saturating_add(width) > max_width {
                break;
            }
            column += width;
            end = offset + grapheme.len();
        }
        (&text[..end], column)
    }

    /// Truncate plain text and optionally append an indicator.
    pub fn truncate(self, text: &str, max_width: usize, indicator: &str) -> String {
        if self.line_width(text) <= max_width {
            return text.to_owned();
        }
        if max_width == 0 {
            return String::new();
        }
        let indicator_width = self.line_width(indicator);
        let indicator = if indicator_width <= max_width {
            indicator
        } else {
            ""
        };
        let available = max_width.saturating_sub(self.line_width(indicator));
        let (prefix, _) = self.prefix(text, available);
        let mut result = String::with_capacity(prefix.len() + indicator.len());
        result.push_str(prefix);
        result.push_str(indicator);
        result
    }

    /// Wrap plain text using whitespace opportunities and grapheme-safe hard
    /// breaks. Returned lines never exceed `width` (except when `width == 0`,
    /// where one empty line is returned).
    pub fn wrap(self, text: &str, width: usize) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        let mut output = Vec::new();
        for logical in text.split('\n') {
            self.wrap_line(logical.trim_end_matches('\r'), width, &mut output);
        }
        if output.is_empty() {
            output.push(String::new());
        }
        output
    }

    fn wrap_line(self, line: &str, width: usize, output: &mut Vec<String>) {
        if line.is_empty() {
            output.push(String::new());
            return;
        }

        let expanded = self.expand_tabs(line, 0);
        let mut remaining = expanded.as_ref();
        while self.line_width(remaining) > width {
            let (hard_prefix, _) = self.prefix(remaining, width);
            if hard_prefix.is_empty() {
                // A grapheme wider than the viewport cannot be split. Keep it
                // as one unit; callers at width 1 can still render safely.
                let grapheme = remaining.graphemes(true).next().unwrap_or_default();
                output.push(grapheme.to_owned());
                remaining = &remaining[grapheme.len()..];
                continue;
            }

            let split = hard_prefix
                .char_indices()
                .rev()
                .find(|(_, character)| character.is_whitespace())
                .map(|(offset, _)| offset)
                .filter(|offset| *offset > 0)
                .unwrap_or(hard_prefix.len());
            let rendered = remaining[..split].trim_end_matches(char::is_whitespace);
            output.push(rendered.to_owned());
            remaining = remaining[split..].trim_start_matches(char::is_whitespace);
        }
        output.push(remaining.to_owned());
    }
}

/// Width under the default policy.
pub fn display_width(text: &str) -> usize {
    WidthPolicy::default().line_width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_combining_and_wide_widths_are_cell_correct() {
        let policy = WidthPolicy::default();
        assert_eq!(policy.line_width("e\u{301}"), 1);
        assert_eq!(policy.line_width("界"), 2);
        assert_eq!(policy.line_width("a界b"), 4);
        assert_eq!(policy.line_width("👩‍💻"), 2);
    }

    #[test]
    fn tabs_follow_current_column() {
        let policy = WidthPolicy::default();
        assert_eq!(policy.line_width("\t"), 4);
        assert_eq!(policy.line_width("a\tb"), 5);
        assert_eq!(policy.expand_tabs("a\tb", 0), "a   b");
    }

    #[test]
    fn zero_and_one_column_layout_do_not_panic() {
        let policy = WidthPolicy::default();
        assert_eq!(policy.wrap("abc", 0), vec![""]);
        assert_eq!(policy.wrap("abc", 1), vec!["a", "b", "c"]);
        assert_eq!(policy.truncate("界", 1, "…"), "…");
    }

    #[test]
    fn wrapping_prefers_words_then_breaks_long_identifiers() {
        let policy = WidthPolicy::default();
        assert_eq!(policy.wrap("alpha beta", 6), vec!["alpha", "beta"]);
        assert_eq!(policy.wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }
}
