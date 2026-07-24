use unicode_segmentation::UnicodeSegmentation;

use crate::keybindings::get_keybindings;
use crate::kill_ring::{KillRing, PushOptions};
use crate::sanitize::{sanitize_text, ControlPictures, SanitizeOptions};
use crate::tui::{Component, Focusable, CURSOR_MARKER};
use crate::undo_stack::UndoStack;
use crate::utils::{slice_by_column, visible_width};
use crate::word_navigation::{find_word_backward, find_word_forward};

#[derive(Clone)]
struct InputState {
    value: String,
    cursor: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

pub type InputSubmitHandler = Box<dyn FnMut(&str)>;

/// Pi-compatible single-line input with horizontal scrolling, kill/yank, and undo.
pub struct Input {
    value: String,
    cursor: usize,
    pub focused: bool,
    pub on_submit: Option<InputSubmitHandler>,
    pub on_escape: Option<Box<dyn FnMut()>>,
    paste_buffer: String,
    in_paste: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: UndoStack<InputState>,
    capabilities: crate::TerminalCapabilities,
}

impl Input {
    pub fn new() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            focused: false,
            on_submit: None,
            on_escape: None,
            paste_buffer: String::new(),
            in_paste: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
            capabilities: crate::terminal_image::get_capabilities(),
        }
    }

    pub fn set_value(&mut self, value: &str) {
        self.value = value.into();
        self.cursor = self.cursor.min(self.value.len());
    }

    pub fn get_value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_capabilities(&mut self, capabilities: crate::TerminalCapabilities) {
        self.capabilities = capabilities;
        self.invalidate();
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(&InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    fn undo(&mut self) {
        if let Some(state) = self.undo_stack.pop() {
            self.value = state.value;
            self.cursor = state.cursor;
            self.last_action = None;
        }
    }

    fn insert_text(&mut self, text: &str) {
        if text.chars().all(char::is_whitespace) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn backspace(&mut self) {
        self.last_action = None;
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let start = self.value[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_forward(&mut self) {
        self.last_action = None;
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let length = self.value[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(1, str::len);
        self.value
            .replace_range(self.cursor..self.cursor + length, "");
    }

    fn kill(&mut self, text: &str, prepend: bool, accumulate: bool) {
        self.kill_ring.push(
            text,
            &PushOptions {
                prepend,
                accumulate,
            },
        );
        self.last_action = Some(LastAction::Kill);
    }

    fn delete_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted = self.value[..self.cursor].to_owned();
        let accumulate = self.last_action == Some(LastAction::Kill);
        self.value.replace_range(..self.cursor, "");
        self.cursor = 0;
        self.kill(&deleted, true, accumulate);
    }

    fn delete_to_end(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let deleted = self.value[self.cursor..].to_owned();
        let accumulate = self.last_action == Some(LastAction::Kill);
        self.value.truncate(self.cursor);
        self.kill(&deleted, false, accumulate);
    }

    fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let accumulate = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let start = find_word_backward(&self.value, self.cursor, None);
        let deleted = self.value[start..self.cursor].to_owned();
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.kill(&deleted, true, accumulate);
    }

    fn delete_word_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let accumulate = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let end = find_word_forward(&self.value, self.cursor, None);
        let deleted = self.value[self.cursor..end].to_owned();
        self.value.replace_range(self.cursor..end, "");
        self.kill(&deleted, false, accumulate);
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        self.push_undo();
        self.value.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        let old = self.kill_ring.peek().unwrap_or_default().to_owned();
        let start = self.cursor.saturating_sub(old.len());
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default();
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.last_action = Some(LastAction::Yank);
    }

    fn move_left(&mut self) {
        self.last_action = None;
        if self.cursor > 0 {
            self.cursor = self.value[..self.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index);
        }
    }

    fn move_right(&mut self) {
        self.last_action = None;
        if self.cursor < self.value.len() {
            self.cursor += self.value[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(1, str::len);
        }
    }

    fn insert_paste(&mut self, pasted: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = pasted
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");
        self.value.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    fn sanitized_value(&self) -> String {
        sanitize_text(
            &self.value,
            SanitizeOptions {
                controls: if self.capabilities.unicode {
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
}

impl Component for Input {
    fn render(&self, width: u16) -> Vec<String> {
        let prompt = "> ";
        let available = usize::from(width).saturating_sub(visible_width(prompt));
        if available == 0 {
            return vec![crate::utils::truncate_to_width(
                prompt,
                usize::from(width),
                Some(""),
            )];
        }

        let value = self.sanitized_value();
        // Accepted input contains no controls, so sanitizing does not normally
        // change offsets. Clamp defensively for programmatic values.
        let cursor = self.cursor.min(value.len());
        let total_width = visible_width(&value);
        let (visible, cursor_display) = if total_width < available {
            (value.clone(), cursor)
        } else {
            let scroll_width = if cursor == value.len() {
                available.saturating_sub(1)
            } else {
                available
            };
            if scroll_width == 0 {
                (String::new(), 0)
            } else {
                let cursor_col = visible_width(&value[..cursor]);
                let half = scroll_width / 2;
                let start_col = if cursor_col < half {
                    0
                } else if cursor_col > total_width.saturating_sub(half) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half)
                };
                let visible = slice_by_column(&value, start_col, scroll_width, true);
                let before = slice_by_column(
                    &value,
                    start_col,
                    cursor_col.saturating_sub(start_col),
                    true,
                );
                (visible, before.len())
            }
        };

        let cursor_display = cursor_display.min(visible.len());
        let at_cursor = visible[cursor_display..]
            .graphemes(true)
            .next()
            .unwrap_or(" ");
        let after_start = cursor_display + at_cursor.len();
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let content = format!(
            "{}{marker}\x1b[7m{at_cursor}\x1b[27m{}",
            &visible[..cursor_display],
            &visible[after_start..]
        );
        let padding = " ".repeat(available.saturating_sub(visible_width(&content)));
        vec![format!("{prompt}{content}{padding}")]
    }

    fn handle_input(&mut self, mut data: &str) {
        if let Some(start) = data.find("\x1b[200~") {
            self.in_paste = true;
            self.paste_buffer.clear();
            data = &data[start + 6..];
        }
        if self.in_paste {
            self.paste_buffer.push_str(data);
            if let Some(end) = self.paste_buffer.find("\x1b[201~") {
                let pasted = self.paste_buffer[..end].to_owned();
                let remaining = self.paste_buffer[end + 6..].to_owned();
                self.paste_buffer.clear();
                self.in_paste = false;
                self.insert_paste(&pasted);
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
            }
            return;
        }

        let bindings = get_keybindings();
        if bindings.matches(data, "tui.select.cancel") {
            if let Some(callback) = &mut self.on_escape {
                callback();
            }
        } else if bindings.matches(data, "tui.editor.undo") {
            self.undo();
        } else if bindings.matches(data, "tui.input.submit") || data == "\n" {
            if let Some(callback) = &mut self.on_submit {
                callback(&self.value);
            }
        } else if bindings.matches(data, "tui.editor.deleteCharBackward") {
            self.backspace();
        } else if bindings.matches(data, "tui.editor.deleteCharForward") {
            self.delete_forward();
        } else if bindings.matches(data, "tui.editor.deleteWordBackward") {
            self.delete_word_backward();
        } else if bindings.matches(data, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
        } else if bindings.matches(data, "tui.editor.deleteToLineStart") {
            self.delete_to_start();
        } else if bindings.matches(data, "tui.editor.deleteToLineEnd") {
            self.delete_to_end();
        } else if bindings.matches(data, "tui.editor.yank") {
            self.yank();
        } else if bindings.matches(data, "tui.editor.yankPop") {
            self.yank_pop();
        } else if bindings.matches(data, "tui.editor.cursorLeft") {
            self.move_left();
        } else if bindings.matches(data, "tui.editor.cursorRight") {
            self.move_right();
        } else if bindings.matches(data, "tui.editor.cursorLineStart") {
            self.last_action = None;
            self.cursor = 0;
        } else if bindings.matches(data, "tui.editor.cursorLineEnd") {
            self.last_action = None;
            self.cursor = self.value.len();
        } else if bindings.matches(data, "tui.editor.cursorWordLeft") {
            self.last_action = None;
            self.cursor = find_word_backward(&self.value, self.cursor, None);
        } else if bindings.matches(data, "tui.editor.cursorWordRight") {
            self.last_action = None;
            self.cursor = find_word_forward(&self.value, self.cursor, None);
        } else if let Some(character) = crate::keys::decode_kitty_text(data) {
            self.insert_text(&character.to_string());
        } else if !data.is_empty()
            && data.chars().all(|character| {
                let code = u32::from(character);
                code >= 32 && code != 0x7f && !(0x80..=0x9f).contains(&code)
            })
        {
            self.insert_text(data);
        }
        self.invalidate();
    }

    fn handle_paste(&mut self, data: &str) {
        self.insert_paste(data);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(input: &mut Input, data: &str) {
        input.handle_input(data);
    }

    #[test]
    fn pi_single_line_backslash_submits_unchanged() {
        let submitted = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let result = submitted.clone();
        let mut input = Input::new();
        input.on_submit = Some(Box::new(move |value| {
            *result.borrow_mut() = Some(value.into())
        }));
        for value in ["h", "e", "l", "l", "o", "\\", "\r"] {
            key(&mut input, value);
        }
        assert_eq!(submitted.borrow().as_deref(), Some("hello\\"));
    }

    #[test]
    fn pi_kill_yank_and_yank_pop() {
        let mut input = Input::new();
        for value in [
            "first", "\x05", "\x17", "second", "\x05", "\x17", "third", "\x05", "\x17",
        ] {
            key(&mut input, value);
        }
        key(&mut input, "\x19");
        assert_eq!(input.get_value(), "third");
        key(&mut input, "\x1by");
        assert_eq!(input.get_value(), "second");
        key(&mut input, "\x1by");
        assert_eq!(input.get_value(), "first");
    }

    #[test]
    fn pi_consecutive_typing_and_spaces_are_undo_units() {
        let mut input = Input::new();
        for value in ["h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d"] {
            key(&mut input, value);
        }
        key(&mut input, "\x1b[45;5u");
        assert_eq!(input.get_value(), "hello");
        key(&mut input, "\x1b[45;5u");
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn pi_paste_is_cleaned_and_undone_atomically() {
        let mut input = Input::new();
        input.set_value("hello world");
        key(&mut input, "\x01");
        for _ in 0..5 {
            key(&mut input, "\x1b[C");
        }
        key(&mut input, "\x1b[200~beep\n\tboop\x1b[201~");
        assert_eq!(input.get_value(), "hellobeep    boop world");
        key(&mut input, "\x1b[45;5u");
        assert_eq!(input.get_value(), "hello world");
    }

    #[test]
    fn pi_render_stays_bounded_for_wide_text_and_keeps_cursor_marker() {
        let mut input = Input::new();
        input.set_value("가나다라마바사아자차카타파하");
        input.focused = true;
        key(&mut input, "\x01");
        for _ in 0..5 {
            key(&mut input, "\x1b[C");
        }
        let line = input.render(20).remove(0);
        assert!(visible_width(&line) <= 20);
        assert!(line.contains(CURSOR_MARKER));
    }

    #[test]
    fn rust_controls_are_visible_not_executed_for_programmatic_values() {
        let mut input = Input::new();
        input.set_value("safe\x1b]52;c;bad\x07");
        let line = input.render(80).remove(0);
        assert!(!line.contains("\x1b]52"));
    }
}
