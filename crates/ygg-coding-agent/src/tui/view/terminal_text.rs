use sexy_tui_rs::strip_terminal_sequences;

const MAX_LIVE_PANEL_BYTES: usize = 64 * 1024;
const MAX_EXTENSION_RENDER_BYTES: usize = 64 * 1024;
const MAX_EXTENSION_TOOL_RENDER_SEGMENTS: usize = 128;
const ELISION_MARKER: &str = "\n… older tool output elided …\n";

/// Strip complete terminal sequences, replace remaining controls (except line
/// feeds), and normalize CRLF so raw tool/provider output cannot execute
/// terminal commands or leave color-protocol debris in the transcript.
///
/// NULL becomes `␀`, BEL becomes `␇`, and other C0/C1 controls become `·`.
pub(crate) fn sanitize_for_terminal(raw: &str) -> String {
    // Command output often carries color, OSC hyperlinks, or a charset reset.
    // Remove complete terminal sequences as units: exposing only their ESC
    // byte leaves artifacts such as `[32m` and `(B` in the transcript.
    let stripped;
    let raw = if raw
        .chars()
        .any(|character| character == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&character))
    {
        stripped = strip_terminal_sequences(raw);
        stripped.as_str()
    } else {
        raw
    };

    // Fast path: most tool output is clean text.
    if raw
        .chars()
        .all(|character| !character.is_control() || character == '\n')
    {
        return raw.to_owned();
    }

    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => out.push('\n'),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push('\n');
            }
            '\r' => out.push('␍'),
            '\t' => out.push_str("    "),
            '\x00' => out.push('␀'),
            '\x07' => out.push('␇'),
            '\x1b' => {
                // If the next char starts a CSI sequence (ESC [), swallow
                // until the final byte so the terminal never sees a live
                // escape. Render the whole thing as visible text.
                out.push('␛');
                if chars.peek() == Some(&'[') {
                    out.push('[');
                    chars.next();
                    // Consume parameter bytes (0x30-0x3F) and intermediate
                    // bytes (0x20-0x2F), then the final byte (0x40-0x7E).
                    while let Some(&next) = chars.peek() {
                        let b = next as u32;
                        if (0x30..=0x3F).contains(&b) || (0x20..=0x2F).contains(&b) {
                            out.push(next);
                            chars.next();
                        } else if (0x40..=0x7E).contains(&b) {
                            out.push(next);
                            chars.next();
                            break;
                        } else {
                            break;
                        }
                    }
                }
            }
            c if c.is_control() => out.push('·'),
            other => out.push(other),
        }
    }
    out
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(character) = chars.next() {
        if character.is_ascii() && (0x40..=0x7e).contains(&(character as u8)) {
            break;
        }
        if character == '\u{009c}' {
            break;
        }
        if character == '\x1b' {
            // A malformed CSI can contain another control introducer. Consume
            // that complete control too instead of allowing its bytes to fall
            // back into visible metadata.
            let _ = consume_escape_sequence(chars);
        }
    }
}

fn consume_control_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(character) = chars.next() {
        match character {
            // BEL is an OSC terminator. Treating it as a terminator for every
            // ECMA-48 string type is intentionally conservative: metadata
            // must never turn a malformed DCS/APC payload into terminal text.
            '\x07' | '\u{009c}' => break,
            '\x1b' if chars.peek() == Some(&'\\') => {
                chars.next();
                break;
            }
            '\x1b' => {
                let _ = consume_escape_sequence(chars);
            }
            _ => {}
        }
    }
}

/// Consume the remainder of a 7-bit escape sequence.
///
/// A non-ASCII follower is not an ECMA-48 final byte, so returning it lets the
/// ordinary-cell caller retain harmless user text after discarding the raw ESC.
fn consume_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
    let next = chars.next()?;
    match next {
        '[' => consume_csi(chars),
        '\u{009b}' => consume_csi(chars),
        // OSC, DCS, SOS, PM, and APC all carry arbitrary bytes until BEL/ST.
        ']' | 'P' | 'X' | '^' | '_' => consume_control_string(chars),
        '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
            consume_control_string(chars)
        }
        '\\' => {}
        intermediate
            if intermediate.is_ascii() && (0x20..=0x2f).contains(&(intermediate as u8)) =>
        {
            while chars.peek().is_some_and(|character| {
                character.is_ascii() && (0x20..=0x2f).contains(&(*character as u8))
            }) {
                chars.next();
            }
            if chars.peek().is_some_and(|character| {
                character.is_ascii() && (0x30..=0x7e).contains(&(*character as u8))
            }) {
                chars.next();
            }
        }
        final_byte if final_byte.is_ascii() => {}
        visible if !visible.is_control() => return Some(visible),
        _ => {}
    }
    None
}

fn push_ordinary_control(out: &mut String, character: char, unicode: bool) {
    let replacement = match character {
        '\x00' => {
            if unicode {
                "␀"
            } else {
                "[NUL]"
            }
        }
        '\x07' => {
            if unicode {
                "␇"
            } else {
                "[BEL]"
            }
        }
        '\r' => {
            if unicode {
                "␍"
            } else {
                "[CR]"
            }
        }
        '\x1b' => {
            if unicode {
                "␛"
            } else {
                "[ESC]"
            }
        }
        _ if unicode => "·",
        _ => "[CTL]",
    };
    out.push_str(replacement);
}

/// Sanitize untrusted ordinary-surface metadata into one display cell.
///
/// This parser consumes complete and malformed 7-bit/C1 CSI and string
/// controls before width measurement or theme styling. Newlines cannot inject
/// a row; visible control diagnostics use only ASCII fallbacks when Unicode is
/// disabled. It intentionally does not preserve ANSI because callers apply
/// trusted theme styling only after this boundary.
pub(crate) fn sanitize_ordinary_surface_cell(raw: &str, unicode: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\x1b' => {
                if let Some(visible) = consume_escape_sequence(&mut chars) {
                    out.push(visible);
                }
            }
            '\u{009b}' => consume_csi(&mut chars),
            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                consume_control_string(&mut chars)
            }
            '\n' => out.push(' '),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push(' ');
            }
            '\t' => out.push_str("    "),
            control if control.is_control() => push_ordinary_control(&mut out, control, unicode),
            visible => out.push(visible),
        }
    }
    out
}

/// Project carriage-return progress into the terminal-visible text state.
/// CRLF remains a newline; a bare CR replaces the current logical line instead
/// of retaining every intermediate progress frame and allowing it to wrap.
pub(crate) fn normalize_carriage_return_progress(raw: &str) -> String {
    if !raw.contains('\r') {
        return raw.to_owned();
    }

    let mut out = String::with_capacity(raw.len());
    let mut line_start = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push('\n');
                line_start = out.len();
            }
            '\r' => out.truncate(line_start),
            '\n' => {
                out.push('\n');
                line_start = out.len();
            }
            visible => out.push(visible),
        }
    }
    out
}

fn bounded_plain_prefix(mut text: String, byte_budget: usize) -> String {
    if text.len() <= byte_budget {
        return text;
    }
    let mut end = byte_budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

pub(super) fn sanitize_extension_tool_render_segments(
    segments: &[ygg_agent::extension_process::ToolRenderSegment],
) -> Vec<ygg_agent::extension_process::ToolRenderSegment> {
    let mut remaining = MAX_EXTENSION_RENDER_BYTES;
    let mut sanitized = Vec::new();
    for segment in segments.iter().take(MAX_EXTENSION_TOOL_RENDER_SEGMENTS) {
        if remaining == 0 {
            break;
        }
        let text = bounded_plain_prefix(sanitize_for_terminal(&segment.text), remaining);
        remaining = remaining.saturating_sub(text.len());
        if text.is_empty() {
            continue;
        }
        let style_role = segment.style_role.as_deref().and_then(|role| {
            let role = sanitize_for_terminal(role).replace('\n', " ");
            let role = bounded_plain_prefix(role.trim().to_owned(), 128);
            (!role.is_empty()).then_some(role)
        });
        sanitized.push(ygg_agent::extension_process::ToolRenderSegment { text, style_role });
    }
    sanitized
}

fn visualize_editor_controls(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\n' => out.push('\n'),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push('\n');
            }
            '\r' => out.push('␍'),
            '\t' => out.push_str("    "),
            '\x00' => out.push('␀'),
            '\x07' => out.push('␇'),
            '\x1b' => out.push('␛'),
            control if control.is_control() => out.push('·'),
            visible => out.push(visible),
        }
    }
    out
}

pub(crate) fn sanitized_editor(raw: &str, cursor: usize) -> (String, usize) {
    let mut cursor = cursor.min(raw.len());
    while cursor > 0 && !raw.is_char_boundary(cursor) {
        cursor -= 1;
    }
    // Composer input remains authoritative and editable. Unlike command logs,
    // controls are visualized rather than removed so cursor offsets can map to
    // every source byte without executing it.
    let before = visualize_editor_controls(&raw[..cursor]);
    let safe_cursor = before.len();
    let after = visualize_editor_controls(&raw[cursor..]);
    (before + &after, safe_cursor)
}

/// Append ephemeral live display output while retaining only the newest 64 KiB.
/// Final tool results replace this buffer instead of passing through it.
pub fn bounded_live_append(existing: &mut String, additional: &str) {
    // Keep raw display bytes (already lossily decoded at the event boundary)
    // until rendering so split terminal/progress sequences are interpreted as
    // one retained stream rather than independently per chunk.
    if existing.len().saturating_add(additional.len()) <= MAX_LIVE_PANEL_BYTES {
        existing.push_str(additional);
        return;
    }

    // Retain the newest bytes in place. The old implementation allocated a
    // second combined String on every overflow event, which is a hot path for
    // noisy tools; reserve once and shift only the retained tail.
    let tail_budget = MAX_LIVE_PANEL_BYTES.saturating_sub(ELISION_MARKER.len());
    let mut additional_start = if additional.len() >= tail_budget {
        additional.len() - tail_budget
    } else {
        0
    };
    while additional_start < additional.len() && !additional.is_char_boundary(additional_start) {
        additional_start += 1;
    }
    let existing_budget = tail_budget.saturating_sub(additional.len() - additional_start);
    let mut existing_start = existing.len().saturating_sub(existing_budget);
    while existing_start < existing.len() && !existing.is_char_boundary(existing_start) {
        existing_start += 1;
    }

    let final_len = ELISION_MARKER.len()
        + existing.len().saturating_sub(existing_start)
        + additional.len().saturating_sub(additional_start);
    existing.replace_range(..existing_start, "");
    existing.reserve(final_len.saturating_sub(existing.len()));
    existing.insert_str(0, ELISION_MARKER);
    existing.push_str(&additional[additional_start..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_live_append_retains_a_tail_and_marks_elision() {
        let mut output = "prefix".repeat(20_000);
        bounded_live_append(&mut output, "THE-TAIL");
        assert!(output.len() <= MAX_LIVE_PANEL_BYTES);
        assert!(output.contains("elided"));
        assert!(output.ends_with("THE-TAIL"));
    }

    #[test]
    fn sanitize_for_terminal_strips_sequences_without_leaving_protocol_debris() {
        // Clean text passes through unchanged.
        assert_eq!(sanitize_for_terminal("hello world\n"), "hello world\n");
        // NULL, BEL, and remaining C0 controls are still visible diagnostics.
        assert_eq!(sanitize_for_terminal("a\x00b\x07c\x01e"), "a␀b␇c·e");
        assert_eq!(sanitize_for_terminal("a\r\nb\rc\td"), "a\nb␍c    d");
        // Valid color, hyperlink, and charset sequences disappear as units;
        // their printable payload remains.
        assert_eq!(sanitize_for_terminal("\x1b[31mRED\x1b[0m"), "RED");
        assert_eq!(sanitize_for_terminal("\x1b(B\x1b[m\x1b[32m+"), "+");
        assert_eq!(
            sanitize_for_terminal("\x1b]8;;https://example.com\x1b\\docs\x1b]8;;\x1b\\"),
            "docs"
        );
        // Incomplete sequences are dropped rather than exposed as `[38;5`.
        assert_eq!(sanitize_for_terminal("before\x1b[38;5"), "before");
        // C1 forms are stripped with their parameters too.
        assert_eq!(sanitize_for_terminal("a\u{009b}31m"), "a");
    }

    #[test]
    fn ordinary_surface_cells_consume_complete_and_malformed_terminal_controls() {
        let complete_sequences = [
            ("before\x1b[31mred\x1b[0mafter", "beforeredafter"),
            ("before\u{009b}31mred\u{009b}0mafter", "beforeredafter"),
            ("before\x1b\u{009b}31mredafter", "beforeredafter"),
            ("before\x1b]0;forged-title\x07after", "beforeafter"),
            ("before\u{009d}0;forged-title\u{009c}after", "beforeafter"),
            ("before\x1bPprivate-data\x1b\\after", "beforeafter"),
            ("before\u{0090}private-data\u{009c}after", "beforeafter"),
            ("before\x1bXignored\x1b\\after", "beforeafter"),
            ("before\x1b^ignored\x1b\\after", "beforeafter"),
            ("before\x1b_ignored\x1b\\after", "beforeafter"),
            ("before\u{0098}ignored\u{009c}after", "beforeafter"),
            ("before\u{009e}ignored\u{009c}after", "beforeafter"),
            ("before\u{009f}ignored\u{009c}after", "beforeafter"),
        ];
        for (raw, expected) in complete_sequences {
            for unicode in [true, false] {
                let sanitized = sanitize_ordinary_surface_cell(raw, unicode);
                assert_eq!(sanitized, expected, "{raw:?}, unicode={unicode}");
                assert!(
                    !sanitized.chars().any(char::is_control),
                    "control survived: {raw:?} -> {sanitized:?}"
                );
                assert!(
                    !sanitized.contains('\x1b'),
                    "escape survived: {raw:?} -> {sanitized:?}"
                );
                if !unicode {
                    assert!(
                        sanitized.is_ascii(),
                        "ASCII cell leaked Unicode: {sanitized:?}"
                    );
                }
            }
        }

        // An unterminated introducer consumes the remainder rather than letting
        // parameter or string bytes regain terminal meaning in an ordinary row.
        for raw in [
            "before\x1b[38;5",
            "before\u{009b}?25",
            "before\x1b]8;;https://example.invalid",
            "before\x1bPpayload",
            "before\u{009d}payload",
        ] {
            for unicode in [true, false] {
                assert_eq!(
                    sanitize_ordinary_surface_cell(raw, unicode),
                    "before",
                    "{raw:?}"
                );
            }
        }

        let unicode = sanitize_ordinary_surface_cell("a\x07b\nc\r\nd\rex\tz\x00q\x01e", true);
        assert_eq!(unicode, "a␇b c d␍ex    z␀q·e");
        let ascii = sanitize_ordinary_surface_cell("a\x07b\nc\r\nd\rex\tz\x00q\x01e", false);
        assert_eq!(ascii, "a[BEL]b c d[CR]ex    z[NUL]q[CTL]e");
        assert!(ascii.is_ascii());
    }

    #[test]
    fn carriage_return_progress_replaces_the_current_logical_line() {
        assert_eq!(
            normalize_carriage_return_progress("phase\n0%\r10%\r100%\r\ndone"),
            "phase\n100%\ndone"
        );
        assert_eq!(normalize_carriage_return_progress("plain"), "plain");
    }

    #[test]
    fn composer_sanitization_preserves_the_cursor_without_mutating_input() {
        let raw = "before \x1b[31m after";
        let cursor = "before \x1b".len();
        let (safe, safe_cursor) = sanitized_editor(raw, cursor);
        assert_eq!(raw, "before \x1b[31m after");
        assert_eq!(&safe[..safe_cursor], "before ␛");
        assert_eq!(safe, "before ␛[31m after");
        assert!(safe.is_char_boundary(safe_cursor));
    }

    #[test]
    fn bounded_live_append_keeps_valid_utf8_at_the_cut_boundary() {
        let mut output = "é".repeat(40_000);
        bounded_live_append(&mut output, " tail");
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with(" tail"));
    }
}
