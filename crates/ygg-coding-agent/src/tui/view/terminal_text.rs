use sexy_tui_rs::strip_terminal_sequences;

const MAX_PANEL_BYTES: usize = 64 * 1024;
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
    let mut remaining = MAX_PANEL_BYTES;
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

/// Append display output while retaining only the newest 64 KiB.
pub fn bounded_append(existing: &mut String, additional: &str) {
    let safe = sanitize_for_terminal(additional);
    if existing.len().saturating_add(safe.len()) <= MAX_PANEL_BYTES {
        existing.push_str(&safe);
        return;
    }

    // Retain the newest bytes in place. The old implementation allocated a
    // second combined String on every overflow event, which is a hot path for
    // noisy tools; reserve once and shift only the retained tail.
    let tail_budget = MAX_PANEL_BYTES.saturating_sub(ELISION_MARKER.len());
    let mut additional_start = if safe.len() >= tail_budget {
        safe.len() - tail_budget
    } else {
        0
    };
    while additional_start < safe.len() && !safe.is_char_boundary(additional_start) {
        additional_start += 1;
    }
    let existing_budget = tail_budget.saturating_sub(safe.len() - additional_start);
    let mut existing_start = existing.len().saturating_sub(existing_budget);
    while existing_start < existing.len() && !existing.is_char_boundary(existing_start) {
        existing_start += 1;
    }

    let final_len = ELISION_MARKER.len()
        + existing.len().saturating_sub(existing_start)
        + safe.len().saturating_sub(additional_start);
    existing.replace_range(..existing_start, "");
    existing.reserve(final_len.saturating_sub(existing.len()));
    existing.insert_str(0, ELISION_MARKER);
    existing.push_str(&safe[additional_start..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_append_retains_a_tail_and_marks_elision() {
        let mut output = "prefix".repeat(20_000);
        bounded_append(&mut output, "THE-TAIL");
        assert!(output.len() <= MAX_PANEL_BYTES);
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
    fn bounded_append_keeps_valid_utf8_at_the_cut_boundary() {
        let mut output = "é".repeat(40_000);
        bounded_append(&mut output, " tail");
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with(" tail"));
    }
}
