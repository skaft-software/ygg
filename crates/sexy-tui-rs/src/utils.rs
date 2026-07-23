//! ANSI string algorithms compatible with Pi TUI v0.81.1.
//!
//! These are core compositing primitives, not generic terminal sanitizers.
//! In particular Pi assigns every visible tab three cells and carries exact
//! style state across wrapping and overlay boundaries.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalToken<'a> {
    Text(&'a str),
    Escape(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnsiCode<'a> {
    pub code: &'a str,
    pub length: usize,
}

/// Extract one of the control strings understood by Pi's compositor.
pub fn extract_ansi_code(text: &str, position: usize) -> Option<AnsiCode<'_>> {
    if text.as_bytes().get(position) != Some(&0x1b) {
        return None;
    }
    match text.as_bytes().get(position + 1).copied()? {
        b'[' => {
            let relative = text.as_bytes()[position + 2..]
                .iter()
                .position(|byte| matches!(byte, b'm' | b'G' | b'K' | b'H' | b'J'))?;
            let end = position + 2 + relative + 1;
            Some(AnsiCode {
                code: &text[position..end],
                length: end - position,
            })
        }
        b']' | b'_' => {
            let mut cursor = position + 2;
            while cursor < text.len() {
                if text.as_bytes()[cursor] == 0x07 {
                    let end = cursor + 1;
                    return Some(AnsiCode {
                        code: &text[position..end],
                        length: end - position,
                    });
                }
                if text.as_bytes()[cursor] == 0x1b
                    && text.as_bytes().get(cursor + 1) == Some(&b'\\')
                {
                    let end = cursor + 2;
                    return Some(AnsiCode {
                        code: &text[position..end],
                        length: end - position,
                    });
                }
                cursor += 1;
            }
            None
        }
        _ => None,
    }
}

fn grapheme_width(grapheme: &str) -> usize {
    if grapheme == "\t" {
        return 3;
    }
    if grapheme
        .chars()
        .all(|character| character.is_control() || is_zero_width(character))
    {
        return 0;
    }
    let first = grapheme
        .chars()
        .find(|character| !character.is_control() && !is_zero_width(*character));
    if first.is_some_and(|character| ('\u{1f1e6}'..='\u{1f1ff}').contains(&character)) {
        return 2;
    }
    UnicodeWidthStr::width(grapheme)
}

fn is_zero_width(character: char) -> bool {
    matches!(character,
        '\u{00ad}'|'\u{034f}'|'\u{061c}'|'\u{115f}'|'\u{1160}'|'\u{17b4}'|'\u{17b5}'|
        '\u{180b}'..='\u{180f}'|'\u{200b}'..='\u{200f}'|'\u{202a}'..='\u{202e}'|
        '\u{2060}'..='\u{206f}'|'\u{3164}'|'\u{fe00}'..='\u{fe0f}'|'\u{feff}'|
        '\u{ffa0}'|'\u{1bca0}'..='\u{1bca3}'|'\u{1d173}'..='\u{1d17a}'|
        '\u{e0000}'..='\u{e0fff}'
    )
}

pub fn visible_width(text: &str) -> usize {
    if text.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return text.len();
    }
    let mut clean = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        if let Some(ansi) = extract_ansi_code(text, cursor) {
            cursor += ansi.length;
            continue;
        }
        let character = text[cursor..].chars().next().expect("character boundary");
        if character == '\t' {
            clean.push_str("   ");
        } else {
            clean.push(character);
        }
        cursor += character.len_utf8();
    }
    clean.graphemes(true).map(grapheme_width).sum()
}

/// Normalize only physical terminal output; editor content remains unchanged.
pub fn normalize_terminal_output(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        if let Some(ansi) = extract_ansi_code(text, cursor) {
            output.push_str(ansi.code);
            cursor += ansi.length;
            continue;
        }
        let character = text[cursor..].chars().next().expect("character boundary");
        match character {
            '\u{0e33}' => output.push_str("\u{0e4d}\u{0e32}"),
            '\u{0eb3}' => output.push_str("\u{0ecd}\u{0eb2}"),
            '\t' => output.push_str("   "),
            value => output.push(value),
        }
        cursor += character.len_utf8();
    }
    output
}

pub fn terminal_tokens(text: &str) -> Vec<TerminalToken<'_>> {
    let mut result = Vec::new();
    let mut cursor = 0;
    let mut text_start = 0;
    while cursor < text.len() {
        if let Some(ansi) = extract_ansi_code(text, cursor) {
            if text_start < cursor {
                result.push(TerminalToken::Text(&text[text_start..cursor]));
            }
            result.push(TerminalToken::Escape(ansi.code));
            cursor += ansi.length;
            text_start = cursor;
        } else {
            cursor += text[cursor..]
                .chars()
                .next()
                .expect("character boundary")
                .len_utf8();
        }
    }
    if text_start < text.len() {
        result.push(TerminalToken::Text(&text[text_start..]));
    }
    result
}

pub fn strip_terminal_sequences(text: &str) -> String {
    let mut output = String::new();
    for token in terminal_tokens(text) {
        if let TerminalToken::Text(value) = token {
            output.push_str(value);
        }
    }
    output
}

#[derive(Clone, Debug, Default)]
struct Hyperlink {
    params: String,
    url: String,
    terminator: String,
}

#[derive(Clone, Debug, Default)]
struct AnsiState {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strike: bool,
    foreground: Option<String>,
    background: Option<String>,
    hyperlink: Option<Hyperlink>,
}

impl AnsiState {
    fn process(&mut self, code: &str) {
        if let Some(body) = code.strip_prefix("\x1b]8;") {
            let (body, terminator) = if let Some(body) = body.strip_suffix('\x07') {
                (body, "\x07")
            } else if let Some(body) = body.strip_suffix("\x1b\\") {
                (body, "\x1b\\")
            } else {
                return;
            };
            let Some((params, url)) = body.split_once(';') else {
                return;
            };
            self.hyperlink = (!url.is_empty()).then(|| Hyperlink {
                params: params.into(),
                url: url.into(),
                terminator: terminator.into(),
            });
            return;
        }
        let Some(parameters) = code
            .strip_prefix("\x1b[")
            .and_then(|value| value.strip_suffix('m'))
        else {
            return;
        };
        if parameters.is_empty() || parameters == "0" {
            self.reset_sgr();
            return;
        }
        let parts: Vec<&str> = parameters.split(';').collect();
        let mut index = 0;
        while index < parts.len() {
            let Ok(value) = parts[index].parse::<u16>() else {
                index += 1;
                continue;
            };
            if matches!(value, 38 | 48) {
                let consumed =
                    if parts.get(index + 1) == Some(&"5") && parts.get(index + 2).is_some() {
                        3
                    } else if parts.get(index + 1) == Some(&"2") && parts.get(index + 4).is_some() {
                        5
                    } else {
                        0
                    };
                if consumed > 0 {
                    let color = parts[index..index + consumed].join(";");
                    if value == 38 {
                        self.foreground = Some(color)
                    } else {
                        self.background = Some(color)
                    }
                    index += consumed;
                    continue;
                }
            }
            match value {
                0 => self.reset_sgr(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strike = true,
                21 => self.bold = false,
                22 => {
                    self.bold = false;
                    self.dim = false
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strike = false,
                39 => self.foreground = None,
                49 => self.background = None,
                30..=37 | 90..=97 => self.foreground = Some(value.to_string()),
                40..=47 | 100..=107 => self.background = Some(value.to_string()),
                _ => {}
            }
            index += 1;
        }
    }
    fn reset_sgr(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strike = false;
        self.foreground = None;
        self.background = None;
    }
    fn active_codes(&self) -> String {
        let mut values = Vec::new();
        if self.bold {
            values.push("1".into())
        }
        if self.dim {
            values.push("2".into())
        }
        if self.italic {
            values.push("3".into())
        }
        if self.underline {
            values.push("4".into())
        }
        if self.blink {
            values.push("5".into())
        }
        if self.inverse {
            values.push("7".into())
        }
        if self.hidden {
            values.push("8".into())
        }
        if self.strike {
            values.push("9".into())
        }
        if let Some(value) = &self.foreground {
            values.push(value.clone())
        }
        if let Some(value) = &self.background {
            values.push(value.clone())
        }
        let mut result = if values.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", values.join(";"))
        };
        if let Some(link) = &self.hyperlink {
            result.push_str(&format!(
                "\x1b]8;{};{}{}",
                link.params, link.url, link.terminator
            ));
        }
        result
    }
    fn line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m")
        }
        if let Some(link) = &self.hyperlink {
            result.push_str(&format!("\x1b]8;;{}", link.terminator))
        }
        result
    }
}

fn update_state(text: &str, state: &mut AnsiState) {
    let mut cursor = 0;
    while cursor < text.len() {
        if let Some(ansi) = extract_ansi_code(text, cursor) {
            state.process(ansi.code);
            cursor += ansi.length
        } else {
            cursor += text[cursor..].chars().next().unwrap().len_utf8()
        }
    }
}
fn is_cjk(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|c| matches!(c,'\u{2e80}'..='\u{9fff}'|'\u{ac00}'..='\u{d7af}'))
}

fn ansi_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut pending = String::new();
    let mut kind: Option<bool> = None;
    let mut cursor = 0;
    let flush = |tokens: &mut Vec<String>, current: &mut String, kind: &mut Option<bool>| {
        if !current.is_empty() {
            tokens.push(std::mem::take(current));
            *kind = None
        }
    };
    while cursor < text.len() {
        if let Some(ansi) = extract_ansi_code(text, cursor) {
            pending.push_str(ansi.code);
            cursor += ansi.length;
            continue;
        }
        let end = (cursor + 1..=text.len())
            .find(|offset| {
                text.is_char_boundary(*offset) && extract_ansi_code(text, *offset).is_some()
            })
            .unwrap_or(text.len());
        for grapheme in text[cursor..end].graphemes(true) {
            let space = grapheme == " ";
            if !space && is_cjk(grapheme) {
                flush(&mut tokens, &mut current, &mut kind);
                tokens.push(format!("{pending}{grapheme}"));
                pending.clear();
                continue;
            }
            if !current.is_empty() && kind != Some(space) {
                flush(&mut tokens, &mut current, &mut kind)
            }
            current.push_str(&pending);
            pending.clear();
            kind = Some(space);
            current.push_str(grapheme);
        }
        cursor = end;
    }
    if !pending.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending)
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending)
        } else {
            current = pending
        }
    }
    flush(&mut tokens, &mut current, &mut kind);
    tokens
}

fn break_long_word(word: &str, width: usize, state: &mut AnsiState) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = state.active_codes();
    let mut used = 0;
    let mut cursor = 0;
    while cursor < word.len() {
        if let Some(ansi) = extract_ansi_code(word, cursor) {
            line.push_str(ansi.code);
            state.process(ansi.code);
            cursor += ansi.length;
            continue;
        }
        let end = (cursor + 1..=word.len())
            .find(|offset| {
                word.is_char_boundary(*offset) && extract_ansi_code(word, *offset).is_some()
            })
            .unwrap_or(word.len());
        for grapheme in word[cursor..end].graphemes(true) {
            let cells = grapheme_width(grapheme);
            if used + cells > width {
                line.push_str(&state.line_end_reset());
                lines.push(line);
                line = state.active_codes();
                used = 0
            }
            line.push_str(grapheme);
            used += cells;
        }
        cursor = end;
    }
    if !line.is_empty() {
        lines.push(line)
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if visible_width(line) <= width {
        return vec![line.into()];
    }
    let mut wrapped = Vec::new();
    let mut state = AnsiState::default();
    let mut current = String::new();
    let mut used = 0;
    for token in ansi_tokens(line) {
        let token_width = visible_width(&token);
        let whitespace = token.trim().is_empty();
        if token_width > width && !whitespace {
            if !current.is_empty() {
                current.push_str(&state.line_end_reset());
                wrapped.push(current)
            }
            let broken = break_long_word(&token, width, &mut state);
            let last = broken.len().saturating_sub(1);
            for value in broken[..last].iter().cloned() {
                wrapped.push(value)
            }
            current = broken[last].clone();
            used = visible_width(&current);
            continue;
        }
        if used + token_width > width && used > 0 {
            let mut finished = current.trim_end().to_owned();
            finished.push_str(&state.line_end_reset());
            wrapped.push(finished);
            if whitespace {
                current = state.active_codes();
                used = 0
            } else {
                current = state.active_codes() + &token;
                used = token_width
            }
        } else {
            current.push_str(&token);
            used += token_width
        }
        update_state(&token, &mut state);
    }
    if !current.is_empty() {
        wrapped.push(current)
    }
    if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped
            .into_iter()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }
}

pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    let mut state = AnsiState::default();
    for input in logical_lines(text) {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            state.active_codes()
        };
        result.extend(wrap_single_line(&(prefix + input), width));
        update_state(input, &mut state);
    }
    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

// `str::lines` drops a final empty line and does not recognize bare CR.
fn logical_lines(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if matches!(bytes[cursor], b'\r' | b'\n') {
            result.push(&text[start..cursor]);
            if bytes[cursor] == b'\r' && bytes.get(cursor + 1) == Some(&b'\n') {
                cursor += 1
            }
            start = cursor + 1
        }
        cursor += 1
    }
    result.push(&text[start..]);
    result
}

/// Same signature as the original Rust port; `None` selects Pi's `"..."`.
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: Option<&str>) -> String {
    truncate_to_width_padded(text, max_width, ellipsis.unwrap_or("..."), false)
}

pub fn truncate_to_width_padded(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return if pad {
            " ".repeat(max_width)
        } else {
            String::new()
        };
    }
    let text_width = visible_width(text);
    let mut ellipsis = ellipsis.to_owned();
    let mut ellipsis_width = visible_width(&ellipsis);
    if ellipsis_width >= max_width {
        if text_width <= max_width {
            return if pad {
                format!("{text}{}", " ".repeat(max_width - text_width))
            } else {
                text.into()
            };
        }
        let (clipped, width) = truncate_fragment(&ellipsis, max_width);
        ellipsis = clipped;
        ellipsis_width = width;
        if width == 0 {
            return if pad {
                " ".repeat(max_width)
            } else {
                String::new()
            };
        }
        return finalize_truncated("", 0, &ellipsis, ellipsis_width, max_width, pad);
    }
    if text_width <= max_width {
        return if pad {
            format!("{text}{}", " ".repeat(max_width - text_width))
        } else {
            text.into()
        };
    }
    let target = max_width - ellipsis_width;
    let mut prefix = String::new();
    let mut kept = 0;
    let mut contiguous = true;
    let mut pending = String::new();
    for token in terminal_tokens(text) {
        match token {
            TerminalToken::Escape(code) => pending.push_str(code),
            TerminalToken::Text(value) => {
                for grapheme in value.graphemes(true) {
                    let width = grapheme_width(grapheme);
                    if contiguous && kept + width <= target {
                        prefix.push_str(&pending);
                        pending.clear();
                        prefix.push_str(grapheme);
                        kept += width
                    } else {
                        contiguous = false;
                        pending.clear()
                    }
                }
            }
        }
    }
    finalize_truncated(&prefix, kept, &ellipsis, ellipsis_width, max_width, pad)
}

fn truncate_fragment(text: &str, max: usize) -> (String, usize) {
    let mut result = String::new();
    let mut width = 0;
    for token in terminal_tokens(text) {
        match token {
            TerminalToken::Escape(code) => result.push_str(code),
            TerminalToken::Text(value) => {
                for grapheme in value.graphemes(true) {
                    let cells = grapheme_width(grapheme);
                    if width + cells > max {
                        return (result, width);
                    }
                    result.push_str(grapheme);
                    width += cells
                }
            }
        }
    }
    (result, width)
}
fn finalize_truncated(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max: usize,
    pad: bool,
) -> String {
    let mut result = if ellipsis.is_empty() {
        format!("{prefix}\x1b[0m")
    } else {
        format!("{prefix}\x1b[0m{ellipsis}\x1b[0m")
    };
    if pad {
        result.push_str(&" ".repeat(max.saturating_sub(prefix_width + ellipsis_width)))
    }
    result
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ColumnSlice {
    pub text: String,
    pub width: usize,
}

pub fn slice_by_column(line: &str, start: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start, length, strict).text
}
pub fn slice_with_width(line: &str, start: usize, length: usize, strict: bool) -> ColumnSlice {
    if length == 0 {
        return ColumnSlice::default();
    }
    let end = start + length;
    let mut result = String::new();
    let mut width = 0;
    let mut column = 0;
    let mut cursor = 0;
    let mut pending = String::new();
    while cursor < line.len() && column < end {
        if let Some(ansi) = extract_ansi_code(line, cursor) {
            if column >= start {
                result.push_str(ansi.code)
            } else {
                pending.push_str(ansi.code)
            }
            cursor += ansi.length;
            continue;
        }
        let next = (cursor + 1..=line.len())
            .find(|offset| {
                line.is_char_boundary(*offset) && extract_ansi_code(line, *offset).is_some()
            })
            .unwrap_or(line.len());
        for grapheme in line[cursor..next].graphemes(true) {
            let cells = grapheme_width(grapheme);
            if column >= start && column < end && (!strict || column + cells <= end) {
                result.push_str(&pending);
                pending.clear();
                result.push_str(grapheme);
                width += cells
            }
            column += cells;
            if column >= end {
                break;
            }
        }
        cursor = next
    }
    ColumnSlice {
        text: result,
        width,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractedSegments {
    pub before: String,
    pub before_width: usize,
    pub after: String,
    pub after_width: usize,
}
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> ExtractedSegments {
    let after_end = after_start + after_len;
    let mut result = ExtractedSegments::default();
    let mut state = AnsiState::default();
    let mut column = 0;
    let mut cursor = 0;
    let mut pending_before = String::new();
    let mut after_started = false;
    while cursor < line.len() {
        if let Some(ansi) = extract_ansi_code(line, cursor) {
            state.process(ansi.code);
            if column < before_end {
                pending_before.push_str(ansi.code)
            } else if column >= after_start && column < after_end && after_started {
                result.after.push_str(ansi.code)
            }
            cursor += ansi.length;
            continue;
        }
        let next = (cursor + 1..=line.len())
            .find(|offset| {
                line.is_char_boundary(*offset) && extract_ansi_code(line, *offset).is_some()
            })
            .unwrap_or(line.len());
        for grapheme in line[cursor..next].graphemes(true) {
            let cells = grapheme_width(grapheme);
            if column < before_end && column + cells <= before_end {
                result.before.push_str(&pending_before);
                pending_before.clear();
                result.before.push_str(grapheme);
                result.before_width += cells
            } else if column >= after_start
                && column < after_end
                && (!strict_after || column + cells <= after_end)
            {
                if !after_started {
                    result.after.push_str(&state.active_codes());
                    after_started = true
                }
                result.after.push_str(grapheme);
                result.after_width += cells
            }
            column += cells;
            if if after_len == 0 {
                column >= before_end
            } else {
                column >= after_end
            } {
                break;
            }
        }
        cursor = next;
        if if after_len == 0 {
            column >= before_end
        } else {
            column >= after_end
        } {
            break;
        }
    }
    result
}

pub fn apply_background_to_line<F>(line: &str, width: usize, background: F) -> String
where
    F: FnOnce(&str) -> String,
{
    let mut padded = line.to_owned();
    padded.push_str(&" ".repeat(width.saturating_sub(visible_width(line))));
    background(&padded)
}
pub fn is_whitespace_char(character: char) -> bool {
    character.is_whitespace()
}
pub fn is_punctuation_char(character: char) -> bool {
    matches!(
        character,
        '(' | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '<'
            | '>'
            | '.'
            | ','
            | ';'
            | ':'
            | '\''
            | '"'
            | '!'
            | '?'
            | '+'
            | '-'
            | '='
            | '*'
            | '/'
            | '\\'
            | '|'
            | '&'
            | '%'
            | '^'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pi_width_and_normalization() {
        assert_eq!(visible_width("\t\x1b[31m界\x1b[0m"), 5);
        assert_eq!(visible_width("🇨"), 2);
        assert_eq!(normalize_terminal_output("ำຳ\t"), "ําໍາ   ");
        assert_eq!(
            normalize_terminal_output("\x1b]0;a\tb\x1b\\x\t"),
            "\x1b]0;a\tb\x1b\\x   "
        )
    }
    #[test]
    fn pi_truncation_resets_styles() {
        assert_eq!(
            truncate_to_width_padded("abcdef", 2, "🙂", false),
            "\x1b[0m🙂\x1b[0m"
        );
        assert_eq!(truncate_to_width_padded("abcdef", 1, "🙂", false), "");
        let value = truncate_to_width_padded("\x1b[31mhello hello", 8, "…", false);
        assert!(value.ends_with("\x1b[0m…\x1b[0m"));
        assert!(visible_width(&value) <= 8)
    }
    #[test]
    fn pi_column_slices_and_segments_handle_tabs() {
        let value = "out 192M\t.pi/skill-tests/results-ha";
        assert_eq!(
            slice_with_width(value, 0, 10, true),
            ColumnSlice {
                text: "out 192M".into(),
                width: 8
            }
        );
        let segments = extract_segments(value, 11, 13, 10, true);
        assert_eq!(segments.before, "out 192M\t");
        assert_eq!(segments.before_width, 11)
    }
    #[test]
    fn pi_wrap_preserves_specific_style_state() {
        let lines = wrap_text_with_ansi(
            "\x1b[44mhello world this is blue background text\x1b[0m",
            15,
        );
        assert!(lines.iter().all(|line| line.contains("\x1b[44m")));
        assert!(lines[..lines.len() - 1]
            .iter()
            .all(|line| !line.ends_with("\x1b[0m")));
        let underlined = wrap_text_with_ansi(
            "prefix \x1b[4mhttps://example.com/very/long/path\x1b[24m",
            18,
        );
        assert!(underlined
            .iter()
            .skip(1)
            .any(|line| line.starts_with("\x1b[4m")))
    }
    #[test]
    fn pi_logical_line_endings() {
        assert_eq!(
            logical_lines("first\nsecond\r\nthird\rfourth"),
            vec!["first", "second", "third", "fourth"]
        )
    }
}
