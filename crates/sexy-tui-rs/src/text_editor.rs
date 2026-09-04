//! A small, terminal-cell-aware multiline text model.
//!
//! [`TextEditor`] deliberately owns only editable text, cursor placement, and
//! visual-row geometry. It does not translate terminal events, draw borders,
//! sanitize untrusted terminal output, or decide focus policy. Those concerns
//! stay with the application that embeds it.

use std::cell::RefCell;
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::width::WidthPolicy;

/// A text mutation understood by [`TextEditor`].
///
/// Event/key translation is intentionally outside this enum. Applications map
/// their input backend into these semantic editing actions and supply the
/// usable text width to [`TextEditor::apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextEditAction {
    /// Insert one Unicode scalar at the cursor.
    Char(char),
    /// Insert a bracketed-paste payload as text, normalizing CRLF and CR to LF.
    Paste(String),
    /// Delete the preceding extended grapheme cluster.
    Backspace,
    /// Delete the following extended grapheme cluster.
    Delete,
    /// Insert one hard line feed.
    Newline,
    /// Move to the preceding extended grapheme cluster.
    Left,
    /// Move to the following extended grapheme cluster.
    Right,
    /// Move to the preceding visual row.
    Up,
    /// Move to the following visual row.
    Down,
    /// Move to the start of the current visual row.
    Home,
    /// Move to the visible end of the current visual row.
    End,
}

/// One visual row in a [`TextEditorLayout`].
///
/// `end` includes source whitespace consumed at a soft wrap while
/// `visible_end` excludes that whitespace. This lets an editor keep every byte
/// editable without rendering a leading or trailing separator row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEditorVisualLine {
    start: usize,
    end: usize,
    visible_end: usize,
}

impl TextEditorVisualLine {
    /// Byte offset at which this visual row starts.
    #[must_use]
    pub fn start(&self) -> usize {
        self.start
    }

    /// Exclusive byte offset owned by this visual row.
    #[must_use]
    pub fn end(&self) -> usize {
        self.end
    }

    /// Exclusive byte offset that is visible on this row.
    #[must_use]
    pub fn visible_end(&self) -> usize {
        self.visible_end
    }
}

/// Visual wrapping and cursor placement for a [`TextEditor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEditorLayout {
    lines: Vec<TextEditorVisualLine>,
    cursor_row: usize,
    wrap_width: usize,
}

impl TextEditorLayout {
    /// Visual rows in source order. Every offset is a grapheme boundary.
    #[must_use]
    pub fn lines(&self) -> &[TextEditorVisualLine] {
        &self.lines
    }

    /// Index of the visual row owning the cursor.
    #[must_use]
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// Effective text-cell width used to create this layout.
    ///
    /// A requested width of zero is normalized to one so the model can always
    /// make progress without splitting a grapheme.
    #[must_use]
    pub fn wrap_width(&self) -> usize {
        self.wrap_width
    }
}

/// Plain visual rows prepared for an application's renderer.
///
/// The row at [`Self::cursor_row`] contains the supplied cursor marker exactly
/// once when that marker is non-empty. The marker is treated as zero-width by
/// callers such as the retained TUI; this type itself does not emit terminal
/// escapes or impose focus policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEditorProjection {
    lines: Vec<String>,
    cursor_row: usize,
}

impl TextEditorProjection {
    /// Projected visual rows in source order.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Index of the row containing the inserted cursor marker.
    #[must_use]
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// Consume the projection and return its rendered rows.
    #[must_use]
    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }
}

#[derive(Clone, Debug)]
struct LayoutCache {
    wrap_width: usize,
    layout: TextEditorLayout,
}

/// An owned UTF-8 multiline buffer with a grapheme-safe cursor.
///
/// The model stores byte offsets because Rust strings are UTF-8, but it accepts
/// only extended-grapheme boundaries for cursor positions and edits. Layout
/// uses the crate's default [`WidthPolicy`], so combining sequences, CJK text,
/// and emoji use terminal display cells rather than byte or scalar counts.
///
/// ```
/// use sexy_tui_rs::{TextEditAction, TextEditor};
///
/// let mut editor = TextEditor::with_text("one\r\ntwo");
/// editor.apply(TextEditAction::Paste("\r\nthree".into()), 12);
/// assert_eq!(editor.text(), "one\r\ntwo\nthree");
///
/// editor.set_text("alpha beta");
/// editor.apply(TextEditAction::Home, 6);
/// assert_eq!(editor.cursor(), 6); // start of the second soft-wrapped row
/// ```
#[derive(Clone, Debug)]
pub struct TextEditor {
    text: String,
    cursor: usize,
    /// The column selected before a vertical move into a shorter row. It stays
    /// sticky for repeated vertical moves and is reset by non-vertical edits.
    preferred_column: Option<usize>,
    cached_layout: RefCell<Option<LayoutCache>>,
}

impl Default for TextEditor {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            preferred_column: None,
            cached_layout: RefCell::new(None),
        }
    }
}

impl TextEditor {
    /// Construct an empty editor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an editor whose cursor starts at the end of `text`.
    #[must_use]
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            preferred_column: None,
            cached_layout: RefCell::new(None),
        }
    }

    /// Borrow the owned text buffer.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return the cursor's UTF-8 byte offset.
    ///
    /// The offset is always an extended-grapheme boundary or the end of the
    /// buffer.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Return whether the buffer has no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Return whether the cursor satisfies this model's grapheme invariant.
    #[must_use]
    pub fn cursor_is_valid(&self) -> bool {
        is_grapheme_boundary(&self.text, self.cursor)
    }

    /// Replace the complete buffer and place the cursor at its end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.preferred_column = None;
        self.invalidate_layout();
    }

    /// Clamp `cursor` to the preceding grapheme boundary and select it.
    ///
    /// Supplying an offset in the middle of a UTF-8 scalar or extended
    /// grapheme never creates an invalid editor state.
    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = clamp_to_grapheme_boundary(&self.text, cursor);
        self.preferred_column = None;
        self.invalidate_layout();
    }

    /// Move the cursor to the end of the buffer.
    pub fn move_to_end(&mut self) {
        self.set_cursor(self.text.len());
    }

    /// Clear the buffer and reset its cursor.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.preferred_column = None;
        self.invalidate_layout();
    }

    /// Take the owned buffer, leaving an empty editor with cursor zero.
    #[must_use]
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        self.preferred_column = None;
        self.invalidate_layout();
        std::mem::take(&mut self.text)
    }

    /// Replace a complete-grapheme byte range without exposing mutable text.
    ///
    /// Returns `false` and leaves the model unchanged when either range endpoint
    /// is not a grapheme boundary. The cursor follows text after the range and
    /// moves to the replacement's end when it was inside the range.
    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) -> bool {
        if range.start > range.end
            || !is_grapheme_boundary(&self.text, range.start)
            || !is_grapheme_boundary(&self.text, range.end)
        {
            return false;
        }

        let removed = range.end - range.start;
        let inserted = replacement.len();
        let next_cursor = if self.cursor <= range.start {
            self.cursor
        } else if self.cursor >= range.end {
            self.cursor - removed + inserted
        } else {
            range.start + inserted
        };
        self.text.replace_range(range, replacement);
        self.finish_non_vertical_change(next_cursor);
        true
    }

    /// Normalize terminal paste line endings without changing other text.
    #[must_use]
    pub fn normalize_paste(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// Apply one semantic edit action.
    ///
    /// `wrap_width` is the usable text width in terminal cells, after any
    /// application-owned prompt, border, or padding columns are reserved. A
    /// width of zero behaves as one cell. Only vertical, Home, and End actions
    /// consult it; it is accepted for every action so input loops have one API.
    /// Returns whether the buffer or cursor changed.
    pub fn apply(&mut self, action: TextEditAction, wrap_width: usize) -> bool {
        match action {
            TextEditAction::Char(character) => self.insert_text(&character.to_string()),
            TextEditAction::Paste(text) => self.insert_text(&Self::normalize_paste(&text)),
            TextEditAction::Backspace => self.backspace(),
            TextEditAction::Delete => self.delete(),
            TextEditAction::Newline => self.insert_text("\n"),
            TextEditAction::Left => self.move_left(),
            TextEditAction::Right => self.move_right(),
            TextEditAction::Up => self.move_vertical(VerticalDirection::Up, wrap_width),
            TextEditAction::Down => self.move_vertical(VerticalDirection::Down, wrap_width),
            TextEditAction::Home => self.move_to_visual_edge(false, wrap_width),
            TextEditAction::End => self.move_to_visual_edge(true, wrap_width),
        }
    }

    /// Return the visual layout for the current text and cursor.
    #[must_use]
    pub fn layout(&self, wrap_width: usize) -> TextEditorLayout {
        let wrap_width = normalize_wrap_width(wrap_width);
        if let Some(cache) = self.cached_layout.borrow().as_ref() {
            if cache.wrap_width == wrap_width {
                return cache.layout.clone();
            }
        }

        let layout = Self::layout_for(&self.text, self.cursor, wrap_width);
        *self.cached_layout.borrow_mut() = Some(LayoutCache {
            wrap_width,
            layout: layout.clone(),
        });
        layout
    }

    /// Layout arbitrary safe display text with the same editor rules.
    ///
    /// This exists for applications that retain raw editable text but render a
    /// separately sanitized projection. It clamps `cursor` to a grapheme
    /// boundary and does not mutate any [`TextEditor`] instance.
    #[must_use]
    pub fn layout_for(text: &str, cursor: usize, wrap_width: usize) -> TextEditorLayout {
        let wrap_width = normalize_wrap_width(wrap_width);
        let cursor = clamp_to_grapheme_boundary(text, cursor);
        let lines = visual_lines(text, wrap_width);
        let cursor_row = cursor_row(&lines, cursor);
        TextEditorLayout {
            lines,
            cursor_row,
            wrap_width,
        }
    }

    /// Build plain visual rows with `cursor_marker` inserted at the cursor.
    #[must_use]
    pub fn render_projection(
        &self,
        wrap_width: usize,
        cursor_marker: &str,
    ) -> TextEditorProjection {
        let layout = self.layout(wrap_width);
        projection_for(&self.text, self.cursor, &layout, cursor_marker)
    }

    /// Build a cursor-marker projection for arbitrary safe display text.
    ///
    /// Like [`Self::layout_for`], this is useful when an application transforms
    /// untrusted source text only at its render boundary.
    #[must_use]
    pub fn render_projection_for(
        text: &str,
        cursor: usize,
        wrap_width: usize,
        cursor_marker: &str,
    ) -> TextEditorProjection {
        let cursor = clamp_to_grapheme_boundary(text, cursor);
        let layout = Self::layout_for(text, cursor, wrap_width);
        projection_for(text, cursor, &layout, cursor_marker)
    }

    fn insert_text(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        self.text.insert_str(self.cursor, text);
        self.finish_non_vertical_change(self.cursor + text.len());
        true
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let previous = previous_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(previous..self.cursor, "");
        self.finish_non_vertical_change(previous);
        true
    }

    fn delete(&mut self) -> bool {
        if self.cursor == self.text.len() {
            return false;
        }
        let next = next_grapheme_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        self.finish_non_vertical_change(self.cursor);
        true
    }

    fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let cursor = previous_grapheme_boundary(&self.text, self.cursor);
        self.finish_non_vertical_change(cursor);
        true
    }

    fn move_right(&mut self) -> bool {
        if self.cursor == self.text.len() {
            return false;
        }
        let cursor = next_grapheme_boundary(&self.text, self.cursor);
        self.finish_non_vertical_change(cursor);
        true
    }

    fn move_vertical(&mut self, direction: VerticalDirection, wrap_width: usize) -> bool {
        let layout = self.layout(wrap_width);
        let line = &layout.lines[layout.cursor_row];
        let target_column = self
            .preferred_column
            .unwrap_or_else(|| column_at(&self.text, line, self.cursor));
        self.preferred_column = Some(target_column);

        let last_row = layout.lines.len().saturating_sub(1);
        let cursor = match direction {
            VerticalDirection::Up if layout.cursor_row == 0 => 0,
            VerticalDirection::Down if layout.cursor_row == last_row => self.text.len(),
            VerticalDirection::Up => offset_at_column(
                &self.text,
                &layout.lines[layout.cursor_row - 1],
                target_column,
            ),
            VerticalDirection::Down => offset_at_column(
                &self.text,
                &layout.lines[layout.cursor_row + 1],
                target_column,
            ),
        };
        if cursor == self.cursor {
            return false;
        }
        self.cursor = cursor;
        self.invalidate_layout();
        true
    }

    fn move_to_visual_edge(&mut self, end: bool, wrap_width: usize) -> bool {
        let layout = self.layout(wrap_width);
        let line = &layout.lines[layout.cursor_row];
        let cursor = if end { line.visible_end } else { line.start };
        let changed = cursor != self.cursor;
        self.finish_non_vertical_change(cursor);
        changed
    }

    fn finish_non_vertical_change(&mut self, cursor: usize) {
        self.cursor = clamp_to_grapheme_boundary(&self.text, cursor);
        self.preferred_column = None;
        self.invalidate_layout();
    }

    fn invalidate_layout(&mut self) {
        self.cached_layout.get_mut().take();
    }
}

impl From<String> for TextEditor {
    fn from(text: String) -> Self {
        Self::with_text(text)
    }
}

#[derive(Clone, Copy)]
enum VerticalDirection {
    Up,
    Down,
}

fn normalize_wrap_width(wrap_width: usize) -> usize {
    wrap_width.max(1)
}

fn is_whitespace_grapheme(grapheme: &str) -> bool {
    grapheme.chars().all(char::is_whitespace)
}

fn visual_lines(text: &str, wrap_width: usize) -> Vec<TextEditorVisualLine> {
    let mut lines = Vec::new();
    let mut logical_start = 0;
    let policy = WidthPolicy::default();

    // Newlines are hard boundaries and intentionally do not belong to either
    // adjacent source range. A final empty row remains editable after one.
    for logical in text.split_inclusive('\n') {
        // CRLF is one extended grapheme. Keep it entirely outside adjacent
        // visual rows so every exported offset remains a grapheme boundary
        // even when callers set raw text rather than using Paste.
        let content_len = logical
            .strip_suffix("\r\n")
            .or_else(|| logical.strip_suffix('\n'))
            .map_or(logical.len(), str::len);
        let logical_end = logical_start + content_len;
        let mut start = logical_start;

        if start == logical_end {
            lines.push(TextEditorVisualLine {
                start,
                end: logical_end,
                visible_end: logical_end,
            });
        } else {
            while start < logical_end {
                let mut columns = 0usize;
                let mut hard_end = logical_end;
                let mut overflow_is_whitespace = false;
                // Latest separator run after visible text. It is consumed by
                // the previous row but omitted from its visual projection.
                let mut word_break: Option<(usize, usize)> = None;
                let mut saw_non_whitespace = false;
                let mut consumed_any = false;

                for (relative, grapheme) in text[start..logical_end].grapheme_indices(true) {
                    let offset = start + relative;
                    let grapheme_width = policy.grapheme_width(grapheme, columns);
                    if consumed_any && columns.saturating_add(grapheme_width) > wrap_width {
                        hard_end = offset;
                        overflow_is_whitespace = is_whitespace_grapheme(grapheme);
                        break;
                    }
                    columns = columns.saturating_add(grapheme_width);
                    consumed_any = true;
                    if is_whitespace_grapheme(grapheme) {
                        if saw_non_whitespace {
                            let separator_end = offset + grapheme.len();
                            match word_break.as_mut() {
                                Some((_, end)) if *end == offset => *end = separator_end,
                                _ => word_break = Some((offset, separator_end)),
                            }
                        }
                    } else {
                        saw_non_whitespace = true;
                    }
                }

                if hard_end == logical_end {
                    lines.push(TextEditorVisualLine {
                        start,
                        end: logical_end,
                        visible_end: logical_end,
                    });
                    break;
                }

                if overflow_is_whitespace {
                    let mut separator_end = hard_end;
                    for (relative, grapheme) in text[hard_end..logical_end].grapheme_indices(true) {
                        if !is_whitespace_grapheme(grapheme) {
                            break;
                        }
                        separator_end = hard_end + relative + grapheme.len();
                    }
                    lines.push(TextEditorVisualLine {
                        start,
                        end: separator_end,
                        visible_end: hard_end,
                    });
                    start = separator_end;
                } else if let Some((separator_start, separator_end)) = word_break {
                    lines.push(TextEditorVisualLine {
                        start,
                        end: separator_end,
                        visible_end: separator_start,
                    });
                    start = separator_end;
                } else {
                    // Never split an oversized grapheme merely to satisfy a
                    // narrow viewport. The next loop still makes progress.
                    lines.push(TextEditorVisualLine {
                        start,
                        end: hard_end,
                        visible_end: hard_end,
                    });
                    start = hard_end;
                }
            }
        }

        logical_start += logical.len();
    }

    if text.is_empty() || text.ends_with('\n') {
        lines.push(TextEditorVisualLine {
            start: text.len(),
            end: text.len(),
            visible_end: text.len(),
        });
    }
    lines
}

fn cursor_row(lines: &[TextEditorVisualLine], cursor: usize) -> usize {
    lines
        .iter()
        .position(|line| {
            (line.start == line.end && cursor == line.start)
                || (cursor >= line.start && cursor < line.end)
        })
        .or_else(|| lines.iter().rposition(|line| cursor == line.end))
        .unwrap_or(0)
}

fn column_at(text: &str, line: &TextEditorVisualLine, cursor: usize) -> usize {
    let cursor = cursor.clamp(line.start, line.visible_end);
    WidthPolicy::default().line_width(&text[line.start..cursor])
}

fn offset_at_column(text: &str, line: &TextEditorVisualLine, target: usize) -> usize {
    let mut offset = line.start;
    let mut column = 0usize;
    let policy = WidthPolicy::default();
    for (relative, grapheme) in text[line.start..line.visible_end].grapheme_indices(true) {
        let width = policy.grapheme_width(grapheme, column);
        if column.saturating_add(width) > target {
            break;
        }
        column = column.saturating_add(width);
        offset = line.start + relative + grapheme.len();
    }
    offset
}

fn projection_for(
    text: &str,
    cursor: usize,
    layout: &TextEditorLayout,
    cursor_marker: &str,
) -> TextEditorProjection {
    let mut lines = Vec::with_capacity(layout.lines.len());
    for (index, line) in layout.lines.iter().enumerate() {
        if index == layout.cursor_row {
            let cursor = cursor.clamp(line.start, line.visible_end);
            lines.push(format!(
                "{}{cursor_marker}{}",
                &text[line.start..cursor],
                &text[cursor..line.visible_end]
            ));
        } else {
            lines.push(text[line.start..line.visible_end].to_owned());
        }
    }
    TextEditorProjection {
        lines,
        cursor_row: layout.cursor_row,
    }
}

fn is_grapheme_boundary(text: &str, offset: usize) -> bool {
    offset <= text.len()
        && (offset == text.len()
            || text
                .grapheme_indices(true)
                .any(|(index, _)| index == offset))
}

fn clamp_to_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == text.len() {
        return offset;
    }
    text.grapheme_indices(true)
        .take_while(|(index, _)| *index <= offset)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0)
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(offset, _)| offset)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |grapheme| cursor + grapheme.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_layout_invariants(text: &str, layout: &TextEditorLayout) {
        assert!(!layout.lines().is_empty());
        assert!(layout.cursor_row() < layout.lines().len());
        for line in layout.lines() {
            assert!(line.start() <= line.visible_end());
            assert!(line.visible_end() <= line.end());
            assert!(line.end() <= text.len());
            for offset in [line.start(), line.visible_end(), line.end()] {
                assert!(
                    is_grapheme_boundary(text, offset),
                    "{text:?} had non-grapheme layout offset {offset}"
                );
            }
        }
    }

    #[test]
    fn empty_text_and_boundary_actions_keep_a_valid_cursor() {
        let mut editor = TextEditor::new();
        for action in [
            TextEditAction::Backspace,
            TextEditAction::Delete,
            TextEditAction::Left,
            TextEditAction::Right,
            TextEditAction::Up,
            TextEditAction::Down,
            TextEditAction::Home,
            TextEditAction::End,
        ] {
            assert!(!editor.apply(action, 0));
            assert!(editor.cursor_is_valid());
            assert_eq!(editor.cursor(), 0);
        }
        let projection = editor.render_projection(0, "<cursor>");
        assert_eq!(projection.lines(), ["<cursor>"]);
        assert_eq!(projection.cursor_row(), 0);
    }

    #[test]
    fn paste_normalizes_crlf_and_newline_is_an_editable_hard_boundary() {
        let mut editor = TextEditor::with_text("a");
        editor.apply(TextEditAction::Paste("b\r\nc\rd".into()), 80);
        editor.apply(TextEditAction::Newline, 80);
        assert_eq!(editor.text(), "ab\nc\nd\n");
        assert_eq!(editor.cursor(), editor.text().len());
        assert_eq!(
            editor
                .layout(80)
                .lines()
                .iter()
                .map(|line| &editor.text()[line.start()..line.visible_end()])
                .collect::<Vec<_>>(),
            vec!["ab", "c", "d", ""]
        );
    }

    #[test]
    fn grapheme_navigation_and_deletion_do_not_split_combining_or_emoji_text() {
        let text = "a e\u{301} 👩‍💻 界";
        let mut editor = TextEditor::with_text(text);
        let original_len = editor.text().len();

        editor.apply(TextEditAction::Left, 80);
        assert_eq!(&editor.text()[editor.cursor()..], "界");
        editor.apply(TextEditAction::Delete, 80);
        assert_eq!(editor.text(), "a e\u{301} 👩‍💻 ");
        assert!(editor.cursor_is_valid());

        editor.apply(TextEditAction::Backspace, 80);
        editor.apply(TextEditAction::Backspace, 80);
        assert_eq!(editor.text(), "a e\u{301} ");
        assert!(editor.cursor_is_valid());
        assert!(editor.text().len() < original_len);

        editor.set_text("e\u{301}");
        editor.apply(TextEditAction::Backspace, 80);
        assert!(editor.is_empty());
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn visual_layout_wraps_words_without_losing_source_ranges() {
        let editor = TextEditor::with_text("alpha beta gamma");
        let layout = editor.layout(10);
        let visible = layout
            .lines()
            .iter()
            .map(|line| &editor.text()[line.start()..line.visible_end()])
            .collect::<Vec<_>>();
        assert_eq!(visible, vec!["alpha beta", "gamma"]);
        assert_eq!(
            layout
                .lines()
                .iter()
                .map(|line| &editor.text()[line.start()..line.end()])
                .collect::<String>(),
            editor.text()
        );
        assert_layout_invariants(editor.text(), &layout);
    }

    #[test]
    fn layout_and_projection_handle_zeroish_widths_wide_cells_and_unique_cursor_markers() {
        let mut editor = TextEditor::with_text("e\u{301}界👩‍💻 alpha");
        editor.set_cursor("e\u{301}界".len());
        for width in 0..=4 {
            let layout = editor.layout(width);
            assert_layout_invariants(editor.text(), &layout);
            let projection = editor.render_projection(width, "<cursor>");
            assert_eq!(
                projection
                    .lines()
                    .iter()
                    .map(|line| line.matches("<cursor>").count())
                    .sum::<usize>(),
                1,
                "width {width}: {projection:?}"
            );
            assert_eq!(projection.cursor_row(), layout.cursor_row());
        }
    }

    #[test]
    fn vertical_movement_keeps_the_preferred_cell_column_across_short_rows() {
        let mut editor = TextEditor::with_text("012345\nxy\nabcdefghi");
        editor.set_cursor(5);
        editor.apply(TextEditAction::Down, 80);
        assert_eq!(editor.cursor(), "012345\nxy".len());
        editor.apply(TextEditAction::Down, 80);
        assert_eq!(editor.cursor(), "012345\nxy\nabcde".len());

        let mut cells = TextEditor::with_text("界界a\nq\n界界abc");
        cells.set_cursor("界界".len());
        cells.apply(TextEditAction::Down, 80);
        assert_eq!(cells.cursor(), "界界a\nq".len());
        cells.apply(TextEditAction::Down, 80);
        assert_eq!(cells.cursor(), "界界a\nq\n界界".len());
    }

    #[test]
    fn soft_wraps_and_resize_keep_the_same_source_cursor_affinity() {
        let mut editor = TextEditor::with_text("abcdefghij");
        editor.set_cursor(7);
        assert_eq!(editor.layout(3).cursor_row(), 2);
        assert_eq!(editor.layout(5).cursor_row(), 1);

        // A vertical motion establishes a preferred visual column. Reflowing
        // before the next motion must preserve that column, not a stale row.
        editor.apply(TextEditAction::Up, 3);
        assert_eq!(editor.cursor(), 4);
        editor.apply(TextEditAction::Down, 5);
        assert_eq!(editor.cursor(), 6);
    }

    #[test]
    fn replacement_and_cursor_setters_cannot_create_invalid_positions() {
        let mut editor = TextEditor::with_text("e\u{301}界");
        editor.set_cursor(1);
        assert_eq!(editor.cursor(), 0);
        editor.set_cursor(usize::MAX);
        assert_eq!(editor.cursor(), editor.text().len());
        assert!(!editor.replace_range(1..2, "x"));
        assert!(editor.replace_range(0.."e\u{301}".len(), "z"));
        assert_eq!(editor.text(), "z界");
        assert!(editor.cursor_is_valid());
    }

    #[test]
    fn home_end_and_soft_wrap_separators_follow_visual_rows() {
        let mut editor = TextEditor::with_text("alpha beta");
        let layout = editor.layout(6);
        assert_eq!(
            layout
                .lines()
                .iter()
                .map(|line| &editor.text()[line.start()..line.visible_end()])
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );

        editor.apply(TextEditAction::Home, 6);
        assert_eq!(editor.cursor(), "alpha ".len());
        editor.apply(TextEditAction::End, 6);
        assert_eq!(editor.cursor(), editor.text().len());

        editor.set_cursor("alpha".len());
        editor.apply(TextEditAction::Right, 6);
        assert_eq!(editor.cursor(), "alpha ".len());
        assert!(editor.cursor_is_valid());
    }

    #[test]
    fn raw_crlf_text_keeps_cursor_and_layout_on_grapheme_boundaries() {
        let mut editor = TextEditor::with_text("a\r\nb");
        let layout = editor.layout(8);
        assert_eq!(
            layout
                .lines()
                .iter()
                .map(|line| &editor.text()[line.start()..line.visible_end()])
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_layout_invariants(editor.text(), &layout);

        // CRLF is a single extended grapheme. It is not painted as source text,
        // but motion can still cross it without creating a split cursor state.
        editor.set_cursor(1);
        editor.apply(TextEditAction::Right, 8);
        assert_eq!(editor.cursor(), 3);
        editor.apply(TextEditAction::Left, 8);
        assert_eq!(editor.cursor(), 1);
        assert!(editor.cursor_is_valid());
    }

    #[test]
    fn static_projection_clamps_untrusted_offsets_without_mutating_source() {
        let text = "e\u{301}界";
        let projection = TextEditor::render_projection_for(text, 1, 1, "<cursor>");
        assert_eq!(
            projection
                .lines()
                .iter()
                .map(|line| line.matches("<cursor>").count())
                .sum::<usize>(),
            1
        );
        assert_eq!(text, "e\u{301}界");
        let layout = TextEditor::layout_for(text, 1, 1);
        assert_layout_invariants(text, &layout);
    }

    #[test]
    fn deterministic_action_matrix_preserves_cursor_and_layout_invariants() {
        let sources = [
            "",
            "ascii",
            "e\u{301}",
            "界界",
            "👩‍💻x",
            "one two three",
            "one\ntwo\n",
            "\r\n",
            "a\tb",
        ];
        let widths = [0, 1, 2, 3, 7, 80];
        for source in sources {
            for width in widths {
                let mut editor = TextEditor::with_text(source);
                for action in [
                    TextEditAction::Left,
                    TextEditAction::Right,
                    TextEditAction::Up,
                    TextEditAction::Down,
                    TextEditAction::Home,
                    TextEditAction::End,
                    TextEditAction::Backspace,
                    TextEditAction::Delete,
                    TextEditAction::Char('界'),
                    TextEditAction::Paste("\r\ne\u{301}👩‍💻".into()),
                    TextEditAction::Newline,
                ] {
                    editor.apply(action, width);
                    assert!(editor.cursor_is_valid(), "{source:?} at width {width}");
                    let layout = editor.layout(width);
                    assert_layout_invariants(editor.text(), &layout);
                    let projection = editor.render_projection(width, "<cursor>");
                    assert_eq!(
                        projection
                            .lines()
                            .iter()
                            .map(|line| line.matches("<cursor>").count())
                            .sum::<usize>(),
                        1,
                        "{source:?} at width {width}: {projection:?}"
                    );
                }
            }
        }
    }
}
