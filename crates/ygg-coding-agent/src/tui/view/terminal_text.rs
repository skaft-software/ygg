use sexy_tui_rs::{strip_terminal_sequences, WidthPolicy};
use unicode_segmentation::UnicodeSegmentation;

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

/// A grapheme-safe mapping from the editable source buffer to a safe layout
/// buffer. The layout buffer retains literal tabs so [`WidthPolicy`] can apply
/// tab stops at the visual row where the tab is painted;
/// [`Self::terminal_row_bounded`] expands those tabs only when materializing a
/// visible terminal row.
#[derive(Clone, Debug)]
pub(crate) struct EditorDisplayMap {
    layout_text: String,
    boundaries: Vec<EditorDisplayBoundary>,
}

#[derive(Clone, Copy, Debug)]
struct EditorDisplayBoundary {
    source: usize,
    display: usize,
}

impl EditorDisplayMap {
    /// Visualize controls without mutating the editable source buffer.
    #[must_use]
    pub(crate) fn from_source(source: &str) -> Self {
        let mut layout_text = String::with_capacity(source.len());
        let mut boundaries = Vec::with_capacity(source.graphemes(true).count().saturating_add(1));
        boundaries.push(EditorDisplayBoundary {
            source: 0,
            display: 0,
        });

        for (start, grapheme) in source.grapheme_indices(true) {
            append_visualized_editor_grapheme(&mut layout_text, grapheme);
            boundaries.push(EditorDisplayBoundary {
                source: start + grapheme.len(),
                display: layout_text.len(),
            });
        }

        // Replacing a control can make a following combining mark join the
        // visible control-picture grapheme. Map every source boundary forward
        // to a valid display boundary rather than placing a caret in that new
        // grapheme's byte interior. Multiple source boundaries may safely map
        // to one display boundary; reverse mapping chooses the latest source
        // boundary for that visible location. Both lists are monotonic, so this
        // normalization stays linear for large restored drafts.
        let mut display_boundaries = layout_text
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain(std::iter::once(layout_text.len()));
        let mut display_boundary = display_boundaries
            .next()
            .expect("a display buffer always has its zero boundary");
        for boundary in &mut boundaries {
            while display_boundary < boundary.display {
                display_boundary = display_boundaries
                    .next()
                    .expect("source transformation cannot exceed display length");
            }
            boundary.display = display_boundary;
        }

        Self {
            layout_text,
            boundaries,
        }
    }

    /// Safe text used for layout. It can contain literal tabs, so it must be
    /// materialized through [`Self::terminal_row_bounded`] before terminal
    /// output.
    #[must_use]
    pub(crate) fn layout_text(&self) -> &str {
        &self.layout_text
    }

    /// Map a source grapheme boundary to a display grapheme boundary.
    #[must_use]
    pub(crate) fn source_to_display(&self, source: usize) -> usize {
        let index = self
            .boundaries
            .partition_point(|boundary| boundary.source <= source)
            .saturating_sub(1);
        self.boundaries[index].display
    }

    /// Map a display grapheme boundary back to a source grapheme boundary.
    ///
    /// Intermediate tab cells map to the preceding source boundary. Exact
    /// display boundaries shared by transformed graphemes map to the latest
    /// source boundary, avoiding a stale cursor inside a source sequence.
    #[must_use]
    pub(crate) fn display_to_source(&self, display: usize) -> usize {
        let display = floor_to_grapheme_boundary(&self.layout_text, display);
        let index = self
            .boundaries
            .partition_point(|boundary| boundary.display <= display)
            .saturating_sub(1);
        self.boundaries[index].source
    }

    /// Materialize one row without splitting a grapheme or overflowing
    /// `max_columns` text cells.
    ///
    /// `cursor` is an optional layout-text byte offset in `start..=end`. The
    /// caller owns the trusted marker and focus policy; this method never
    /// derives a cursor from source content.
    ///
    /// The trusted cursor marker is zero-cell terminal chrome and is retained
    /// even when an oversized source grapheme cannot fit. In that case its
    /// location is clamped to the last materialized text boundary rather than
    /// being dropped by a later generic string truncator.
    #[must_use]
    pub(crate) fn terminal_row_bounded(
        &self,
        start: usize,
        end: usize,
        cursor: Option<usize>,
        cursor_marker: &str,
        max_columns: usize,
    ) -> String {
        let Some(row) = self.layout_text.get(start..end) else {
            return String::new();
        };
        let cursor = cursor.filter(|offset| *offset >= start && *offset <= end);
        let mut rendered = String::with_capacity(row.len().saturating_add(cursor_marker.len()));
        let mut column = 0usize;
        let mut inserted_cursor = false;
        if cursor == Some(start) {
            rendered.push_str(cursor_marker);
            inserted_cursor = true;
        }
        let policy = WidthPolicy::default();
        for (relative, grapheme) in row.grapheme_indices(true) {
            let offset = start + relative;
            let width = policy.grapheme_width(grapheme, column);
            if column.saturating_add(width) > max_columns {
                break;
            }
            if grapheme == "\t" {
                rendered.push_str(&" ".repeat(width));
            } else {
                rendered.push_str(grapheme);
            }
            column = column.saturating_add(width);
            if cursor == Some(offset + grapheme.len()) {
                rendered.push_str(cursor_marker);
                inserted_cursor = true;
            }
        }
        if cursor.is_some() && !inserted_cursor {
            // The cursor was after text that could not fit. Keep its trusted
            // token at the clipped row edge; an embedding reserves its cell.
            rendered.push_str(cursor_marker);
        }
        rendered
    }
}

fn append_visualized_editor_grapheme(out: &mut String, grapheme: &str) {
    if grapheme == "\r\n" {
        out.push('\n');
        return;
    }
    for character in grapheme.chars() {
        match character {
            '\n' => out.push('\n'),
            // A bare CR is visible source text in the composer. CRLF is
            // handled above as one source grapheme and one hard line feed.
            '\r' => out.push('␍'),
            // Keep tabs for width-aware visual layout; expand them only in a
            // row with a known starting column.
            '\t' => out.push('\t'),
            '\x00' => out.push('␀'),
            '\x07' => out.push('␇'),
            '\x1b' => out.push('␛'),
            control if control.is_control() => out.push('·'),
            visible => out.push(visible),
        }
    }
}

fn floor_to_grapheme_boundary(text: &str, offset: usize) -> usize {
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
    fn composer_display_map_preserves_the_cursor_without_mutating_input() {
        let raw = "before \x1b[31m after";
        let cursor = "before \x1b".len();
        let map = EditorDisplayMap::from_source(raw);
        let safe = map.layout_text();
        let safe_cursor = map.source_to_display(cursor);
        assert_eq!(raw, "before \x1b[31m after");
        assert_eq!(&safe[..safe_cursor], "before ␛");
        assert_eq!(safe, "before ␛[31m after");
        assert!(safe
            .grapheme_indices(true)
            .any(|(offset, _)| offset == safe_cursor));
        assert_eq!(
            map.terminal_row_bounded(0, safe.len(), Some(safe_cursor), "<cursor>", usize::MAX),
            "before ␛<cursor>[31m after"
        );
    }

    #[test]
    fn composer_display_map_never_maps_a_cursor_into_joined_display_graphemes() {
        // The source ESC and following combining mark are separate source
        // boundaries, but visualizing ESC as U+241B makes them one displayed
        // grapheme. Mapping must advance rather than split it.
        let raw = "\x1b\u{0301}x";
        let map = EditorDisplayMap::from_source(raw);
        let display_cursor = map.source_to_display("\x1b".len());
        assert_eq!(map.layout_text(), "␛\u{0301}x");
        assert_eq!(display_cursor, "␛\u{0301}".len());
        assert!(map
            .layout_text()
            .grapheme_indices(true)
            .any(|(offset, _)| offset == display_cursor));
        let source_cursor = map.display_to_source(display_cursor);
        assert!(raw
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain(std::iter::once(raw.len()))
            .any(|offset| offset == source_cursor));
    }

    #[test]
    fn composer_display_map_keeps_complex_grapheme_boundaries_safe() {
        let source = "e\u{301}👩‍💻❤\u{fe0f}🇦🇧界\t\x1b\u{0301}\r\n";
        let source_boundaries = source
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain(std::iter::once(source.len()))
            .collect::<Vec<_>>();
        let map = EditorDisplayMap::from_source(source);
        let display_boundaries = map
            .layout_text()
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain(std::iter::once(map.layout_text().len()))
            .collect::<Vec<_>>();

        let mut previous_display = 0;
        for source in source_boundaries.iter().copied() {
            let display = map.source_to_display(source);
            assert!(display_boundaries.contains(&display));
            assert!(display >= previous_display);
            assert!(source_boundaries.contains(&map.display_to_source(display)));
            previous_display = display;
        }
    }

    #[test]
    fn composer_display_map_expands_tabs_only_in_the_visible_row() {
        let map = EditorDisplayMap::from_source("ab\tc");
        assert_eq!(map.layout_text(), "ab\tc");
        assert_eq!(
            map.terminal_row_bounded(0, 4, None, "", usize::MAX),
            "ab  c"
        );

        let map = EditorDisplayMap::from_source("x\t");
        assert_eq!(
            map.terminal_row_bounded(0, 2, Some(2), "<cursor>", usize::MAX),
            "x   <cursor>"
        );
    }

    #[test]
    fn bounded_composer_rows_clip_before_wide_graphemes_and_keep_the_cursor() {
        let map = EditorDisplayMap::from_source("a界b");
        let cursor = "a界".len();
        let row = map.terminal_row_bounded(
            0,
            map.layout_text().len(),
            Some(cursor),
            sexy_tui_rs::CURSOR_MARKER,
            2,
        );
        assert_eq!(row, format!("a{}", sexy_tui_rs::CURSOR_MARKER));
        assert_eq!(
            sexy_tui_rs::visible_width(&row.replace(sexy_tui_rs::CURSOR_MARKER, "")),
            1
        );

        let map = EditorDisplayMap::from_source("界a");
        let row = map.terminal_row_bounded(
            0,
            map.layout_text().len(),
            Some("界".len()),
            sexy_tui_rs::CURSOR_MARKER,
            2,
        );
        assert_eq!(row, format!("界{}", sexy_tui_rs::CURSOR_MARKER));
    }

    #[test]
    fn composer_display_map_cannot_confuse_source_with_the_trusted_cursor_token() {
        let source = format!("source {} token", sexy_tui_rs::CURSOR_MARKER);
        let map = EditorDisplayMap::from_source(&source);
        assert!(!map.layout_text().contains(sexy_tui_rs::CURSOR_MARKER));
        let row = map.terminal_row_bounded(
            0,
            map.layout_text().len(),
            Some(map.layout_text().len()),
            sexy_tui_rs::CURSOR_MARKER,
            usize::MAX,
        );
        assert_eq!(row.matches(sexy_tui_rs::CURSOR_MARKER).count(), 1);
        assert!(row.contains("source ␛_pi:c␇ token"));
    }

    #[test]
    fn bounded_live_append_keeps_valid_utf8_at_the_cut_boundary() {
        let mut output = "é".repeat(40_000);
        bounded_live_append(&mut output, " tail");
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with(" tail"));
    }
}
