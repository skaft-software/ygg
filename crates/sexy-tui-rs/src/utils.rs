//! Legacy ANSI-string helpers routed through the crate-wide width policy.
//!
//! New code should prefer semantic [`crate::rich_text`] values; these helpers
//! remain for compatibility with closure-styled widgets.

use unicode_segmentation::UnicodeSegmentation;

use crate::width::WidthPolicy;

/// Calculate visible display cells while ignoring complete terminal escape
/// sequences and C0/C1 controls.
pub fn visible_width(text: &str) -> usize {
    let policy = WidthPolicy::default();
    let mut width = 0usize;
    for token in terminal_tokens(text) {
        if let TerminalToken::Text(value) = token {
            for grapheme in value.graphemes(true) {
                if grapheme.contains(['\n', '\r']) {
                    continue;
                }
                width = width.saturating_add(policy.grapheme_width(grapheme, width));
            }
        }
    }
    width
}

/// Truncate without splitting grapheme clusters or terminal control sequences.
/// The indicator is included in `max_width`.
pub fn truncate_to_width(text: &str, max_width: usize, indicator: Option<&str>) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }

    let indicator = indicator.unwrap_or("…");
    let indicator = truncate_plain(indicator, max_width);
    let indicator_width = visible_width(&indicator);
    let available = max_width.saturating_sub(indicator_width);
    let policy = WidthPolicy::default();
    let mut output = String::new();
    let mut width = 0usize;
    'tokens: for token in terminal_tokens(text) {
        match token {
            TerminalToken::Escape(sequence) => output.push_str(sequence),
            TerminalToken::Text(value) => {
                for grapheme in value.graphemes(true) {
                    let grapheme_width = policy.grapheme_width(grapheme, width);
                    if width.saturating_add(grapheme_width) > available {
                        break 'tokens;
                    }
                    output.push_str(grapheme);
                    width = width.saturating_add(grapheme_width);
                }
            }
        }
    }
    output.push_str(&indicator);
    if output.contains('\x1b') {
        output.push_str("\x1b[0m\x1b]8;;\x1b\\");
    }
    output
}

/// Remove complete terminal escape sequences while retaining their visible
/// text. This accepts legacy trusted ANSI output; semantic content should use
/// the rich-text sanitizer instead.
pub fn strip_terminal_sequences(text: &str) -> String {
    if !text
        .chars()
        .any(|character| character == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&character))
    {
        return text.to_owned();
    }

    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let character = text[index..]
            .chars()
            .next()
            .expect("index remains on a character boundary");
        match character {
            '\x1b' => index = terminal_sequence_end(bytes, index),
            '\u{009b}' => index = c1_csi_end(text, index + character.len_utf8()),
            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                index = c1_string_end(text, index + character.len_utf8())
            }
            '\u{009c}' => index += character.len_utf8(),
            _ => {
                output.push(character);
                index += character.len_utf8();
            }
        }
    }
    output
}

fn c1_csi_end(text: &str, mut index: usize) -> usize {
    while index < text.len() {
        let character = text[index..]
            .chars()
            .next()
            .expect("index remains on a character boundary");
        index += character.len_utf8();
        if ('\u{0040}'..='\u{007e}').contains(&character) {
            return index;
        }
    }
    text.len()
}

fn c1_string_end(text: &str, mut index: usize) -> usize {
    while index < text.len() {
        let character = text[index..]
            .chars()
            .next()
            .expect("index remains on a character boundary");
        if matches!(character, '\x07' | '\u{009c}') {
            return index + character.len_utf8();
        }
        if character == '\x1b' && text.as_bytes().get(index + 1) == Some(&b'\\') {
            return (index + 2).min(text.len());
        }
        index += character.len_utf8();
    }
    text.len()
}

/// Wrap a trusted ANSI-styled string at word boundaries. Escape sequences are
/// indivisible, SGR/hyperlink state is closed on every row and reopened on the
/// next, and long words fall back to grapheme-boundary wrapping. Semantic
/// Markdown uses typed runs instead of this compatibility helper.
pub fn wrap_text_with_ansi(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }

    let policy = WidthPolicy::default();
    let mut lines = vec![String::new()];
    let mut width = 0usize;
    let mut state = WrappedAnsiState::default();
    let mut word = Vec::new();
    let mut separator = Vec::new();

    for token in terminal_tokens(text) {
        match token {
            TerminalToken::Escape(sequence) => word.push(WrappedPiece::Escape(sequence)),
            TerminalToken::Text(value) => {
                for grapheme in value.graphemes(true) {
                    if matches!(grapheme, "\n" | "\r\n" | "\r") {
                        flush_wrapped_word(
                            &mut word,
                            &mut separator,
                            &mut lines,
                            &mut width,
                            &mut state,
                            max_width,
                            policy,
                        );
                        // Trailing whitespace is not useful terminal content
                        // and must not create a whitespace-only wrapped row.
                        separator.clear();
                        begin_wrapped_line(&mut lines, &mut width, &state);
                    } else if grapheme.chars().all(char::is_whitespace) {
                        flush_wrapped_word(
                            &mut word,
                            &mut separator,
                            &mut lines,
                            &mut width,
                            &mut state,
                            max_width,
                            policy,
                        );
                        separator.push(WrappedPiece::Grapheme(grapheme));
                    } else {
                        word.push(WrappedPiece::Grapheme(grapheme));
                    }
                }
            }
        }
    }

    flush_wrapped_word(
        &mut word,
        &mut separator,
        &mut lines,
        &mut width,
        &mut state,
        max_width,
        policy,
    );
    // Do not retain trailing spaces, but do emit trailing state changes (for
    // example a final foreground reset) that were buffered with the word.
    separator.clear();
    state.close_line(lines.last_mut().expect("one output line"));
    lines
}

#[derive(Clone, Copy)]
enum WrappedPiece<'a> {
    Escape(&'a str),
    Grapheme(&'a str),
}

#[derive(Default)]
struct WrappedAnsiState {
    /// Exact SGR history since the latest full reset. Replaying the history is
    /// safer than splitting parameters: RGB channels such as 107 must never be
    /// mistaken for standalone background codes.
    sgr: String,
    hyperlink: Option<String>,
}

impl WrappedAnsiState {
    fn apply(&mut self, sequence: &str) {
        if sequence.starts_with("\x1b[") && sequence.ends_with('m') {
            if matches!(sequence, "\x1b[m" | "\x1b[0m") {
                self.sgr.clear();
            } else {
                self.sgr.push_str(sequence);
            }
            return;
        }

        let Some(body) = sequence.strip_prefix("\x1b]8;") else {
            return;
        };
        let body = body
            .strip_suffix("\x1b\\")
            .or_else(|| body.strip_suffix('\x07'))
            .unwrap_or(body);
        let Some((_, target)) = body.split_once(';') else {
            return;
        };
        self.hyperlink = (!target.is_empty()).then(|| sequence.to_owned());
    }

    fn close_line(&self, line: &mut String) {
        if self.hyperlink.is_some() {
            line.push_str("\x1b]8;;\x1b\\");
        }
        if !self.sgr.is_empty() {
            line.push_str("\x1b[0m");
        }
    }

    fn reopen(&self) -> String {
        let mut opening = self.sgr.clone();
        if let Some(hyperlink) = &self.hyperlink {
            opening.push_str(hyperlink);
        }
        opening
    }
}

fn wrapped_pieces_width(pieces: &[WrappedPiece<'_>], start: usize, policy: WidthPolicy) -> usize {
    pieces.iter().fold(0usize, |width, piece| match piece {
        WrappedPiece::Escape(_) => width,
        WrappedPiece::Grapheme(grapheme) => {
            width.saturating_add(policy.grapheme_width(grapheme, start.saturating_add(width)))
        }
    })
}

fn begin_wrapped_line(lines: &mut Vec<String>, width: &mut usize, state: &WrappedAnsiState) {
    state.close_line(lines.last_mut().expect("one output line"));
    lines.push(state.reopen());
    *width = 0;
}

fn emit_wrapped_pieces(
    pieces: &mut Vec<WrappedPiece<'_>>,
    lines: &mut Vec<String>,
    width: &mut usize,
    state: &mut WrappedAnsiState,
    max_width: usize,
    policy: WidthPolicy,
) {
    for piece in pieces.drain(..) {
        match piece {
            WrappedPiece::Escape(sequence) => {
                lines
                    .last_mut()
                    .expect("one output line")
                    .push_str(sequence);
                state.apply(sequence);
            }
            WrappedPiece::Grapheme(grapheme) => {
                let grapheme_width = policy.grapheme_width(grapheme, *width);
                if *width > 0 && width.saturating_add(grapheme_width) > max_width {
                    begin_wrapped_line(lines, width, state);
                }
                if grapheme_width > max_width {
                    lines.last_mut().expect("one output line").push('?');
                    *width = 1;
                } else {
                    lines
                        .last_mut()
                        .expect("one output line")
                        .push_str(grapheme);
                    *width = width.saturating_add(grapheme_width);
                }
            }
        }
    }
}

fn flush_wrapped_word(
    word: &mut Vec<WrappedPiece<'_>>,
    separator: &mut Vec<WrappedPiece<'_>>,
    lines: &mut Vec<String>,
    width: &mut usize,
    state: &mut WrappedAnsiState,
    max_width: usize,
    policy: WidthPolicy,
) {
    if word.is_empty() {
        return;
    }
    let separator_width = wrapped_pieces_width(separator, *width, policy);
    let word_width = wrapped_pieces_width(word, width.saturating_add(separator_width), policy);
    if *width > 0
        && width
            .saturating_add(separator_width)
            .saturating_add(word_width)
            > max_width
    {
        begin_wrapped_line(lines, width, state);
        separator.clear();
    }
    emit_wrapped_pieces(separator, lines, width, state, max_width, policy);
    emit_wrapped_pieces(word, lines, width, state, max_width, policy);
}

fn truncate_plain(text: &str, max_width: usize) -> String {
    let policy = WidthPolicy::default();
    let mut output = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = policy.grapheme_width(grapheme, width);
        if width.saturating_add(grapheme_width) > max_width {
            break;
        }
        if !grapheme.chars().any(char::is_control) {
            output.push_str(grapheme);
            width = width.saturating_add(grapheme_width);
        }
    }
    output
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalToken<'a> {
    Text(&'a str),
    Escape(&'a str),
}

pub fn terminal_tokens(text: &str) -> Vec<TerminalToken<'_>> {
    let bytes = text.as_bytes();
    let mut output = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let control = bytes[index] == 0x1b;
        if !control {
            index += 1;
            continue;
        }
        if start < index {
            output.push(TerminalToken::Text(&text[start..index]));
        }
        let end = terminal_sequence_end(bytes, index);
        output.push(TerminalToken::Escape(&text[index..end]));
        index = end;
        start = end;
    }
    if start < text.len() {
        output.push(TerminalToken::Text(&text[start..]));
    }
    output
}

fn terminal_sequence_end(bytes: &[u8], start: usize) -> usize {
    if bytes[start] != 0x1b {
        return (start + 1).min(bytes.len());
    }
    let Some(&kind) = bytes.get(start + 1) else {
        return bytes.len();
    };
    match kind {
        b'[' => {
            let mut index = start + 2;
            while index < bytes.len() {
                if (0x40..=0x7e).contains(&bytes[index]) {
                    return index + 1;
                }
                index += 1;
            }
            bytes.len()
        }
        b']' => string_sequence_end(bytes, start + 2, true),
        // Internal cursor marker: ESC _ \\. A real APC uses ST (ESC \\),
        // so this short form is unambiguous inside trusted widget output.
        b'_' if bytes.get(start + 2) == Some(&b'\\') => (start + 3).min(bytes.len()),
        b'P' | b'_' | b'^' | b'X' => string_sequence_end(bytes, start + 2, false),
        // ESC intermediates (for example ESC ( B, emitted by some command
        // color libraries when restoring the ASCII character set) are one
        // indivisible sequence, not an ESC pair followed by visible garbage.
        0x20..=0x2f => {
            let mut index = start + 1;
            while bytes
                .get(index)
                .is_some_and(|byte| (0x20..=0x2f).contains(byte))
            {
                index += 1;
            }
            if bytes
                .get(index)
                .is_some_and(|byte| (0x30..=0x7e).contains(byte))
            {
                index + 1
            } else {
                bytes.len()
            }
        }
        // A non-ASCII scalar cannot be the final byte of a two-byte ESC
        // sequence. Drop only ESC and leave the Unicode text on a valid UTF-8
        // boundary instead of slicing through its first code unit.
        0x80..=0xff => start + 1,
        _ => (start + 2).min(bytes.len()),
    }
}

fn string_sequence_end(bytes: &[u8], mut index: usize, bell_terminated: bool) -> usize {
    while index < bytes.len() {
        if bell_terminated && bytes[index] == 0x07 {
            return index + 1;
        }
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_handles_sgr_osc_cjk_combining_and_emoji() {
        assert_eq!(visible_width("\x1b[31mred\x1b[0m"), 3);
        assert_eq!(
            visible_width("\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"),
            4
        );
        assert_eq!(visible_width("界e\u{301}👩‍💻"), 5);
    }

    #[test]
    fn truncation_does_not_split_graphemes_or_leak_styles() {
        let result = truncate_to_width("\x1b[31m界界界", 5, Some("…"));
        assert_eq!(visible_width(&result), 5);
        assert!(result.ends_with("\x1b[0m\x1b]8;;\x1b\\"));
        assert!(result.contains("界界…"));
    }

    #[test]
    fn wrapping_preserves_graphemes_and_bounds() {
        let lines = wrap_text_with_ansi("a界e\u{301}👩‍💻z", 3);
        assert!(lines.iter().all(|line| visible_width(line) <= 3));
        assert_eq!(lines.concat(), "a界e\u{301}👩‍💻z");
    }

    #[test]
    fn wrapping_breaks_at_word_boundaries() {
        // Words should not be chopped mid-word across lines.
        let lines = wrap_text_with_ansi("hello world alpha beta", 10);
        assert!(lines.iter().all(|line| visible_width(line) <= 10));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].trim(), "hello");
        assert_eq!(lines[1].trim(), "world");
        assert_eq!(lines[2].trim(), "alpha beta");
    }

    #[test]
    fn wrapping_hard_breaks_words_longer_than_width() {
        let lines = wrap_text_with_ansi("abcdefghij", 4);
        assert!(lines.iter().all(|line| visible_width(line) <= 4));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].trim(), "abcd");
        assert_eq!(lines[1].trim(), "efgh");
        assert_eq!(lines[2].trim(), "ij");
    }

    #[test]
    fn wrapping_preserves_ansi_across_word_breaks() {
        let input = "hello \x1b[31mred world\x1b[0m foo";
        let lines = wrap_text_with_ansi(input, 10);
        assert!(lines.iter().all(|line| visible_width(line) <= 10));
        let visible = strip_terminal_sequences(&lines.concat());
        for word in ["hello", "red", "world", "foo"] {
            assert!(visible.contains(word), "missing {word:?}: {visible:?}");
        }
        assert!(lines.concat().contains("\x1b[31m"));
        assert!(lines.concat().contains("\x1b[0m"));
    }

    #[test]
    fn wrapping_never_splits_truecolor_sequences_inside_long_words() {
        let input = "\x1b[38;2;22;132;107mabcdefghijklmnop\x1b[39m";
        let lines = wrap_text_with_ansi(input, 5);
        assert!(lines.iter().all(|line| visible_width(line) <= 5));
        assert_eq!(
            strip_terminal_sequences(&lines.concat()),
            "abcdefghijklmnop"
        );
        let encoded = lines.concat();
        assert!(!encoded.contains("\x1b[107m"), "{encoded:?}");
        assert!(!encoded.contains("\x1b[38;2m"), "{encoded:?}");
        assert!(encoded.contains("\x1b[38;2;22;132;107m"));
    }

    #[test]
    fn stripping_terminal_sequences_removes_csi_osc_and_charset_resets() {
        let input = concat!(
            "\x1b(B\x1b[m\x1b[32m+\x1b[0m ",
            "\x1b]8;;https://example.com\x1b\\docs\x1b]8;;\x1b\\"
        );
        assert_eq!(strip_terminal_sequences(input), "+ docs");
        assert_eq!(strip_terminal_sequences("a\x1b💡z"), "a💡z");
        assert_eq!(
            strip_terminal_sequences("a\u{009b}31mred\u{009b}0mz"),
            "aredz"
        );
        assert_eq!(strip_terminal_sequences("a\u{009d}0;title\u{009c}z"), "az");
    }

    #[test]
    fn wrapped_hyperlinks_close_and_reopen_at_each_row() {
        let input = "\x1b]8;;https://example.com\x1b\\documentation\x1b]8;;\x1b\\";
        let lines = wrap_text_with_ansi(input, 4);
        assert_eq!(strip_terminal_sequences(&lines.concat()), "documentation");
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| {
            line.contains("\x1b]8;;https://example.com\x1b\\") && line.contains("\x1b]8;;\x1b\\")
        }));
    }
}
