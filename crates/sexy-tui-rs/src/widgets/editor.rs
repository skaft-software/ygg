use unicode_segmentation::UnicodeSegmentation;

use crate::editor_component::EditorComponent;
use crate::sanitize::{sanitize_text, ControlPictures, SanitizeOptions};
use crate::theme::Theme;
use crate::tui::{Component, Focusable, CURSOR_MARKER};
use crate::width::WidthPolicy;

pub struct EditorTheme {
    pub border_color: Box<dyn Fn(&str) -> String>,
}

impl EditorTheme {
    pub fn new(theme: &Theme) -> Self {
        let theme = theme.clone();
        Self {
            border_color: Box::new(move |text| theme.fg("accent", text)),
        }
    }
}

#[derive(Default)]
pub struct EditorOptions {
    pub padding_x: u16,
}

/// Multi-line text editor widget with byte-boundary-safe grapheme movement.
pub struct Editor {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    focused: bool,
    theme: EditorTheme,
    options: EditorOptions,
    unicode: bool,
}

impl Editor {
    pub fn new(theme: EditorTheme, options: EditorOptions) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            focused: false,
            theme,
            options,
            unicode: crate::terminal_image::get_capabilities().unicode,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    pub fn get_text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_capabilities(&mut self, capabilities: crate::TerminalCapabilities) {
        self.unicode = capabilities.unicode && !capabilities.plain;
    }

    fn move_vertical(&mut self, row: usize) {
        let policy = WidthPolicy::default();
        let current = &self.lines[self.cursor_row][..self.cursor_col];
        let target_cells = policy.line_width(&safe_editor_text(current, self.unicode));
        self.cursor_row = row;
        self.cursor_col = byte_at_cell(&self.lines[row], target_cells, policy, self.unicode);
    }

    fn insert_paste(&mut self, data: &str) {
        // Bracketed paste is text, not a replay of individual key events. Keep
        // hard line breaks while rejecting every other terminal control byte.
        let normalized = data.replace("\r\n", "\n").replace('\r', "\n");
        if normalized
            .chars()
            .any(|character| character.is_control() && character != '\n')
        {
            return;
        }

        let tail = self.lines[self.cursor_row].split_off(self.cursor_col);
        let mut lines = normalized.split('\n');
        let first = lines.next().unwrap_or_default();
        self.lines[self.cursor_row].push_str(first);
        let mut row = self.cursor_row;
        for line in lines {
            row += 1;
            self.lines.insert(row, line.to_owned());
        }
        self.cursor_row = row;
        self.cursor_col = self.lines[row].len();
        self.lines[row].push_str(&tail);
    }
}

impl Component for Editor {
    fn render(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return vec![String::new()];
        }
        if width == 1 {
            let glyph = if self.unicode { "│" } else { "|" };
            return vec![glyph.to_owned(); self.lines.len().saturating_add(2)];
        }
        let inside = width.saturating_sub(2);
        let padding = self.options.padding_x.min(inside / 2);
        let text_width = inside.saturating_sub(padding.saturating_mul(2));
        let (horizontal, vertical, top_left, top_right, bottom_left, bottom_right) = if self.unicode
        {
            ("─", "│", "┌", "┐", "└", "┘")
        } else {
            ("-", "|", "+", "+", "+", "+")
        };
        let border = (self.theme.border_color)(&horizontal.repeat(usize::from(inside)));
        let top = format!("{top_left}{border}{top_right}");
        let bottom = format!("{bottom_left}{border}{bottom_right}");
        let spacer = " ".repeat(usize::from(padding));
        let mut output = vec![top];
        for (row, line) in self.lines.iter().enumerate() {
            let content = editor_view(
                line,
                if row == self.cursor_row && self.focused {
                    Some(self.cursor_col)
                } else {
                    None
                },
                usize::from(text_width),
                self.unicode,
            );
            let used = crate::utils::visible_width(&content);
            output.push(format!(
                "{vertical}{spacer}{content}{}{spacer}{vertical}",
                " ".repeat(usize::from(text_width).saturating_sub(used))
            ));
        }
        output.push(bottom);
        output
    }

    fn handle_input(&mut self, data: &str) {
        use crate::keys::{matches_key, Key};
        if matches_key(data, Key::enter) {
            let rest = self.lines[self.cursor_row][self.cursor_col..].to_owned();
            self.lines[self.cursor_row].truncate(self.cursor_col);
            self.lines.insert(self.cursor_row + 1, rest);
            self.cursor_row += 1;
            self.cursor_col = 0;
        } else if matches_key(data, Key::backspace) {
            if self.cursor_col > 0 {
                let start = self.lines[self.cursor_row][..self.cursor_col]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(index, _)| index);
                self.lines[self.cursor_row].replace_range(start..self.cursor_col, "");
                self.cursor_col = start;
            } else if self.cursor_row > 0 {
                let rest = self.lines.remove(self.cursor_row);
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].len();
                self.lines[self.cursor_row].push_str(&rest);
            }
        } else if matches_key(data, Key::up) && self.cursor_row > 0 {
            self.move_vertical(self.cursor_row - 1);
        } else if matches_key(data, Key::down) && self.cursor_row + 1 < self.lines.len() {
            self.move_vertical(self.cursor_row + 1);
        } else if matches_key(data, Key::left) && self.cursor_col > 0 {
            self.cursor_col = self.lines[self.cursor_row][..self.cursor_col]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index);
        } else if matches_key(data, Key::right)
            && self.cursor_col < self.lines[self.cursor_row].len()
        {
            self.cursor_col += self.lines[self.cursor_row][self.cursor_col..]
                .graphemes(true)
                .next()
                .map_or(0, str::len);
        } else if let Some(character) = crate::keys::decode_kitty_text(data) {
            self.lines[self.cursor_row].insert(self.cursor_col, character);
            self.cursor_col += character.len_utf8();
        } else if !data.is_empty()
            && !data.starts_with('\x1b')
            && data.chars().all(|character| !character.is_control())
        {
            self.lines[self.cursor_row].insert_str(self.cursor_col, data);
            self.cursor_col += data.len();
        }
        self.invalidate();
    }

    fn handle_paste(&mut self, data: &str) {
        self.insert_paste(data);
        self.invalidate();
    }

    fn invalidate(&mut self) {}
}

impl Focusable for Editor {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

impl EditorComponent for Editor {
    fn get_text(&self) -> String {
        self.get_text()
    }

    fn set_text(&mut self, text: &str) {
        self.set_text(text);
    }

    fn on_submit(&mut self, _text: &str) {}

    fn on_change(&mut self, _text: &str) {}
}

fn editor_view(line: &str, cursor: Option<usize>, width: usize, unicode: bool) -> String {
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
    let (safe, cursor) = if let Some(cursor) = cursor.map(|cursor| cursor.min(line.len())) {
        let before = sanitize_text(&line[..cursor], options);
        let after = sanitize_text(&line[cursor..], options);
        let safe_cursor = before.len();
        (format!("{before}{after}"), Some(safe_cursor))
    } else {
        (sanitize_text(line, options).into_owned(), None)
    };

    let policy = WidthPolicy::default();
    let mut start = cursor.unwrap_or(0);
    if let Some(cursor) = cursor {
        let mut cells = 0usize;
        for (index, grapheme) in safe[..cursor].grapheme_indices(true).rev() {
            let grapheme_width = policy.grapheme_width(grapheme, 0);
            if cells.saturating_add(grapheme_width) > width.saturating_sub(1) {
                break;
            }
            cells = cells.saturating_add(grapheme_width);
            start = index;
        }
    }

    let mut output = String::new();
    let mut cells = 0usize;
    let mut marker_written = false;
    let cursor = cursor.unwrap_or(usize::MAX);
    for (relative, grapheme) in safe[start..].grapheme_indices(true) {
        let index = start + relative;
        if !marker_written && index == cursor {
            output.push_str(CURSOR_MARKER);
            marker_written = true;
        }
        let grapheme_width = policy.grapheme_width(grapheme, cells);
        if cells.saturating_add(grapheme_width) > width {
            break;
        }
        output.push_str(grapheme);
        cells = cells.saturating_add(grapheme_width);
    }
    if !marker_written && cursor <= safe.len() && cells <= width {
        output.push_str(CURSOR_MARKER);
    }
    output
}

fn safe_editor_text(text: &str, unicode: bool) -> String {
    sanitize_text(
        text,
        SanitizeOptions {
            controls: if unicode {
                ControlPictures::Unicode
            } else {
                ControlPictures::Ascii
            },
            preserve_newlines: false,
            preserve_tabs: false,
        },
    )
    .into_owned()
}

fn byte_at_cell(line: &str, target: usize, policy: WidthPolicy, unicode: bool) -> usize {
    let mut cells = 0usize;
    for (index, grapheme) in line.grapheme_indices(true) {
        let safe = safe_editor_text(grapheme, unicode);
        let width = policy.line_width_from(&safe, cells);
        if cells.saturating_add(width) > target {
            return index;
        }
        cells = cells.saturating_add(width);
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_navigation_and_backspace_keep_utf8_boundaries() {
        let capabilities = crate::capabilities::TerminalCapabilities::plain();
        let theme = Theme::with_capabilities(capabilities);
        let mut editor = Editor::new(EditorTheme::new(&theme), EditorOptions::default());
        editor.set_focused(true);
        editor.handle_input("👩‍💻");
        editor.handle_input("e\u{301}");
        editor.handle_input("\x7f");
        assert_eq!(editor.get_text(), "👩‍💻");
        editor.handle_input("\x7f");
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn semantic_paste_preserves_multiline_text_and_csi_u_text() {
        let capabilities = crate::capabilities::TerminalCapabilities::plain();
        let theme = Theme::with_capabilities(capabilities);
        let mut editor = Editor::new(EditorTheme::new(&theme), EditorOptions::default());
        editor.handle_input("\x1b[97:65;2u");
        editor.handle_input("\x1b[49:33;2u");
        editor.handle_paste(" first\r\nsecond\rthird");
        assert_eq!(editor.get_text(), "A! first\nsecond\nthird");
    }

    #[test]
    fn cursor_marker_is_cell_accurate_and_untrusted_escape_is_visible() {
        let view = editor_view("界e\u{301}\x1b[31m", Some("界e\u{301}".len()), 10, false);
        assert!(view.contains(CURSOR_MARKER));
        assert!(!view.contains("\x1b[31m"));
        assert!(crate::utils::visible_width(&view) <= 10);
    }
}
