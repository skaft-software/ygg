//! Composer text wrapping, cursor geometry, and layout caching.

use sexy_tui_rs::visible_width;
use unicode_width::UnicodeWidthChar;

use super::ShellState;

#[derive(Clone, Debug)]
pub(crate) struct EditorVisualLine {
    /// Source range owned by this visual row. A soft-wrap separator can be
    /// included in `end` while omitted from display via `visible_end`.
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) visible_end: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct EditorLayout {
    pub(crate) lines: Vec<EditorVisualLine>,
    pub(crate) cursor_row: usize,
}

/// Cache key for the editor layout so we don't re-wrap on every animation
/// frame when the editor content hasn't changed.
#[derive(Clone, Debug)]
pub(super) struct EditorLayoutCache {
    width: u16,
    text_len: usize,
    cursor: usize,
    text_hash: u64,
    layout: EditorLayout,
}

impl ShellState {
    pub(crate) fn cached_editor_layout(
        &self,
        width: u16,
        editor: Option<&String>,
        cursor: Option<usize>,
    ) -> EditorLayout {
        let text = editor.map(String::as_str).unwrap_or("");
        let cursor = cursor.unwrap_or(0).min(text.len());
        // Only recompute when the input actually changed: text, cursor, or width.
        let cache = self.cached_layout.borrow();
        if let Some(ref cached) = *cache {
            if cached.width == width
                && cached.text_len == text.len()
                && cached.cursor == cursor
                && cached.text_hash == hash_str(text)
            {
                return cached.layout.clone();
            }
        }
        drop(cache);
        let layout = editor_layout(text, cursor, width);
        *self.cached_layout.borrow_mut() = Some(EditorLayoutCache {
            width,
            text_len: text.len(),
            cursor,
            text_hash: hash_str(text),
            layout: layout.clone(),
        });
        layout
    }
}

/// Quick FNV-1a hash for cache-key purposes; we only need to detect changes.
fn hash_str(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn prompt_content_width(width: u16) -> usize {
    // Prompt marker + one separating space. Continuation rows use two spaces.
    usize::from(width).saturating_sub(2)
}

fn editor_wrap_width(width: u16) -> usize {
    // Reserve one cell for the rendered cursor.
    prompt_content_width(width).saturating_sub(1).max(1)
}

/// Normalize terminal paste line endings before placing them in the editor.
/// Bracketed paste must never submit the prompt or turn CRLF into visual `\r`
/// characters in a multi-line editor.
pub(super) fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn editor_visual_lines(text: &str, wrap_width: usize) -> Vec<EditorVisualLine> {
    let mut lines = Vec::new();
    let mut logical_start = 0;

    // Newlines are hard boundaries and are intentionally excluded from the
    // adjacent visual rows. `split_inclusive` preserves an empty row after a
    // trailing newline below.
    for logical in text.split_inclusive('\n') {
        let content_len = logical.strip_suffix('\n').map_or(logical.len(), str::len);
        let logical_end = logical_start + content_len;
        let mut start = logical_start;

        if start == logical_end {
            lines.push(EditorVisualLine {
                start,
                end: logical_end,
                visible_end: logical_end,
            });
        } else {
            while start < logical_end {
                let mut columns = 0usize;
                let mut hard_end = logical_end;
                let mut overflow_is_whitespace = false;
                // Byte range of the latest separator run after visible text.
                let mut word_break: Option<(usize, usize)> = None;
                let mut saw_non_whitespace = false;
                let mut consumed_any = false;

                for (relative, character) in text[start..logical_end].char_indices() {
                    let offset = start + relative;
                    let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                    if consumed_any && columns.saturating_add(character_width) > wrap_width {
                        hard_end = offset;
                        overflow_is_whitespace = character.is_whitespace();
                        break;
                    }
                    columns = columns.saturating_add(character_width);
                    consumed_any = true;
                    if character.is_whitespace() {
                        // A leading indentation is not a word boundary. Once a
                        // word has appeared, retain the whole separator run so
                        // it can be consumed (but visually trimmed) at a wrap.
                        if saw_non_whitespace {
                            let separator_end = offset + character.len_utf8();
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
                    lines.push(EditorVisualLine {
                        start,
                        end: logical_end,
                        visible_end: logical_end,
                    });
                    break;
                }

                if overflow_is_whitespace {
                    // A word exactly filled the row. Consume and visually trim
                    // the separator run rather than creating a leading-space
                    // or whitespace-only row before the next word.
                    let mut separator_end = hard_end;
                    for character in text[hard_end..logical_end].chars() {
                        if !character.is_whitespace() {
                            break;
                        }
                        separator_end += character.len_utf8();
                    }
                    lines.push(EditorVisualLine {
                        start,
                        end: separator_end,
                        visible_end: hard_end,
                    });
                    start = separator_end;
                } else if let Some((separator_start, separator_end)) = word_break {
                    // Move the entire word that overflowed to the next row and
                    // hide only its separating whitespace. Source offsets stay
                    // owned by a row, so cursor motion remains lossless.
                    lines.push(EditorVisualLine {
                        start,
                        end: separator_end,
                        visible_end: separator_start,
                    });
                    start = separator_end;
                } else {
                    // A single word is wider than the composer: hard-wrap it.
                    lines.push(EditorVisualLine {
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

    // `split_inclusive` does not produce a final empty item. Keep an editable
    // row for an empty editor and after a trailing hard newline.
    if text.is_empty() || text.ends_with('\n') {
        lines.push(EditorVisualLine {
            start: text.len(),
            end: text.len(),
            visible_end: text.len(),
        });
    }
    lines
}

pub(super) fn editor_layout(text: &str, cursor: usize, width: u16) -> EditorLayout {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }

    let lines = editor_visual_lines(text, editor_wrap_width(width));

    let cursor_row = lines
        .iter()
        .position(|line| {
            (line.start == line.end && cursor == line.start)
                || (cursor >= line.start && cursor < line.end)
        })
        .or_else(|| lines.iter().rposition(|line| cursor == line.end))
        .unwrap_or(0);

    EditorLayout { lines, cursor_row }
}

pub(super) fn editor_column(text: &str, line: &EditorVisualLine, cursor: usize) -> usize {
    visible_width(&text[line.start..cursor.clamp(line.start, line.visible_end)])
}

pub(super) fn editor_offset_at_column(text: &str, line: &EditorVisualLine, target: usize) -> usize {
    let mut offset = line.start;
    let mut column: usize = 0;
    for (relative, character) in text[line.start..line.visible_end].char_indices() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if column.saturating_add(width) > target {
            break;
        }
        column = column.saturating_add(width);
        offset = line.start + relative + character.len_utf8();
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_wraps_at_word_boundaries_and_hard_wraps_long_words() {
        let text = "alpha beta gamma";
        let lines = editor_visual_lines(text, 10);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["alpha beta", "gamma"]);
        assert_eq!(
            lines
                .iter()
                .map(|line| &text[line.start..line.end])
                .collect::<String>(),
            text,
            "soft wrapping must retain every source byte for cursor editing"
        );

        // A word that exactly follows a full row must not create a
        // whitespace-only row or split despite fitting on its own.
        let text = "one two";
        let lines = editor_visual_lines(text, 3);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["one", "two"]);

        let text = "supercalifragilistic";
        let lines = editor_visual_lines(text, 5);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["super", "calif", "ragil", "istic"]);
    }

    #[test]
    fn word_wrap_preserves_explicit_newlines() {
        let text = "one two\nthree four\n";
        let lines = editor_visual_lines(text, 6);
        let wrapped: Vec<_> = lines
            .iter()
            .map(|line| &text[line.start..line.visible_end])
            .collect();
        assert_eq!(wrapped, vec!["one", "two", "three", "four", ""]);
    }
}
