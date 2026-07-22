use unicode_segmentation::UnicodeSegmentation;

use crate::sanitize::{sanitize_text, ControlPictures, SanitizeOptions};
use crate::tui::{Component, Focusable, CURSOR_MARKER};
use crate::width::WidthPolicy;

/// Single-line text input widget with grapheme-safe editing.
pub struct Input {
    text: String,
    cursor: usize,
    focused: bool,
    capabilities: crate::TerminalCapabilities,
}

impl Input {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            focused: false,
            capabilities: crate::terminal_image::get_capabilities(),
        }
    }

    pub fn set_value(&mut self, value: &str) {
        self.text.clear();
        self.text.push_str(value);
        self.cursor = self.text.len();
        self.invalidate();
    }

    pub fn get_value(&self) -> &str {
        &self.text
    }

    pub fn set_capabilities(&mut self, capabilities: crate::TerminalCapabilities) {
        self.capabilities = capabilities;
        self.invalidate();
    }
}

impl Component for Input {
    fn render(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        let prefix = if width >= 2 { "> " } else { ">" };
        let available = usize::from(width).saturating_sub(prefix.len());
        let value = input_view(
            &self.text,
            self.cursor,
            available,
            self.focused,
            self.capabilities.unicode,
        );
        vec![format!("{prefix}{value}")]
    }

    fn handle_input(&mut self, data: &str) {
        use crate::keys::{matches_key, Key};
        if matches_key(data, Key::backspace) && self.cursor > 0 {
            let start = self.text[..self.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index);
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        } else if matches_key(data, Key::left) && self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index);
        } else if matches_key(data, Key::right) && self.cursor < self.text.len() {
            self.cursor += self.text[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len);
        } else if let Some(character) = crate::keys::decode_kitty_text(data) {
            self.text.insert(self.cursor, character);
            self.cursor += character.len_utf8();
        } else if !data.is_empty()
            && !data.starts_with('\x1b')
            && data.chars().all(|character| !character.is_control())
        {
            self.text.insert_str(self.cursor, data);
            self.cursor += data.len();
        }
        self.invalidate();
    }

    fn invalidate(&mut self) {}
}

impl Focusable for Input {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

fn input_view(text: &str, cursor: usize, width: usize, focused: bool, unicode: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let options = SanitizeOptions {
        controls: if unicode {
            ControlPictures::Unicode
        } else {
            ControlPictures::Ascii
        },
        preserve_newlines: false,
        preserve_tabs: false,
    };
    let cursor = cursor.min(text.len());
    let safe_before = sanitize_text(&text[..cursor], options);
    let safe_after = sanitize_text(&text[cursor..], options);
    let safe_cursor = safe_before.len();
    let safe = format!("{safe_before}{safe_after}");

    let policy = WidthPolicy::default();
    let mut start = safe_cursor;
    let mut before_cells = 0usize;
    for (index, grapheme) in safe[..safe_cursor].grapheme_indices(true).rev() {
        let cells = policy.grapheme_width(grapheme, 0);
        if before_cells.saturating_add(cells) > width.saturating_sub(1) {
            break;
        }
        before_cells = before_cells.saturating_add(cells);
        start = index;
    }

    let mut end = start;
    let mut cells = 0usize;
    for (relative, grapheme) in safe[start..].grapheme_indices(true) {
        let grapheme_cells = policy.grapheme_width(grapheme, cells);
        if cells.saturating_add(grapheme_cells) > width {
            break;
        }
        cells = cells.saturating_add(grapheme_cells);
        end = start + relative + grapheme.len();
    }
    end = end.max(safe_cursor);
    if focused {
        format!(
            "{}{CURSOR_MARKER}{}",
            &safe[start..safe_cursor],
            &safe[safe_cursor..end]
        )
    } else {
        safe[start..end].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_emoji_and_combining_clusters_is_atomic() {
        let mut input = Input::new();
        input.handle_input("👩‍💻");
        input.handle_input("e\u{301}");
        input.handle_input("\x7f");
        assert_eq!(input.get_value(), "👩‍💻");
        input.handle_input("\x7f");
        assert!(input.get_value().is_empty());
    }

    #[test]
    fn inserts_layout_resolved_csi_u_text_without_accepting_control_sequences() {
        let mut input = Input::new();
        input.handle_input("\x1b[97:65;2u"); // Shift+A, base `a`, text `A`
        input.handle_input("\x1b[49:33;2u"); // Shift+1, base `1`, text `!`
        input.handle_input("\x1b[8364;7u"); // Ctrl+Alt/AltGr + €
        input.handle_input("\x1b[99;5u"); // Ctrl+C is not text
        assert_eq!(input.get_value(), "A!€");
    }

    #[test]
    fn escape_sequences_are_visible_not_executed() {
        let view = input_view("safe\x1b]52;c;bad\x07", 4, 80, false, false);
        assert!(!view.contains('\x1b'));
        assert!(!view.contains('\x07'));
        let narrow = input_view("a\x1b]52;c;bad\x07", 2, 5, true, false);
        assert!(crate::utils::visible_width(&narrow) <= 5);
        assert!(narrow.contains(CURSOR_MARKER));
    }
}
