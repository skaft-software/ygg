//! Safety boundary for untrusted terminal and Markdown text.

use std::borrow::Cow;

/// How control bytes become visible copyable text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlPictures {
    /// Unicode control pictures such as `␛`.
    Unicode,
    /// ASCII descriptions such as `^[` and `<BEL>`.
    Ascii,
    /// Remove controls rather than visualizing them.
    Strip,
}

/// Sanitization policy for a component's text semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SanitizeOptions {
    pub controls: ControlPictures,
    pub preserve_newlines: bool,
    pub preserve_tabs: bool,
}

impl Default for SanitizeOptions {
    fn default() -> Self {
        Self {
            controls: ControlPictures::Unicode,
            preserve_newlines: true,
            preserve_tabs: true,
        }
    }
}

/// Sanitize untrusted terminal text. No ESC, C0/C1 command, OSC, CSI,
/// clipboard, title, cursor, or query sequence survives this function.
pub fn sanitize_text(input: &str, options: SanitizeOptions) -> Cow<'_, str> {
    if input.chars().all(|character| is_clean(character, options)) {
        return Cow::Borrowed(input);
    }

    let mut output = String::with_capacity(input.len());
    let mut previous_was_cr = false;
    for character in input.chars() {
        match character {
            '\n' if options.preserve_newlines => {
                if !previous_was_cr {
                    output.push('\n');
                }
                previous_was_cr = false;
            }
            '\r' if options.preserve_newlines => {
                // Normalize CR and CRLF at the semantic boundary.
                output.push('\n');
                previous_was_cr = true;
            }
            '\t' if options.preserve_tabs => output.push('\t'),
            '\x1b' => push_control(&mut output, "␛", "^[", "ESC", options.controls),
            '\x00' => push_control(&mut output, "␀", "<NUL>", "NUL", options.controls),
            '\x07' => push_control(&mut output, "␇", "<BEL>", "BEL", options.controls),
            '\x08' => push_control(&mut output, "␈", "<BS>", "BS", options.controls),
            '\x7f' => push_control(&mut output, "␡", "<DEL>", "DEL", options.controls),
            character if is_c0_or_c1(character) => {
                let ascii = format!("<U+{:04X}>", character as u32);
                push_control(&mut output, "·", &ascii, "", options.controls);
            }
            character if is_bidi_override(character) => {
                let ascii = format!("<U+{:04X}>", character as u32);
                match options.controls {
                    ControlPictures::Strip => {}
                    _ => output.push_str(&ascii),
                }
            }
            character => output.push(character),
        }
        if character != '\r' && character != '\n' {
            previous_was_cr = false;
        }
    }
    Cow::Owned(output)
}

fn is_clean(character: char, options: SanitizeOptions) -> bool {
    if character == '\n' {
        return options.preserve_newlines;
    }
    if character == '\t' {
        return options.preserve_tabs;
    }
    character != '\r'
        && !is_c0_or_c1(character)
        && !is_bidi_override(character)
        && character != '\x7f'
}

fn is_c0_or_c1(character: char) -> bool {
    let value = character as u32;
    value < 0x20 || (0x80..=0x9f).contains(&value)
}

fn is_bidi_override(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn push_control(
    output: &mut String,
    unicode: &str,
    ascii: &str,
    _name: &str,
    policy: ControlPictures,
) {
    match policy {
        ControlPictures::Unicode => output.push_str(unicode),
        ControlPictures::Ascii => output.push_str(ascii),
        ControlPictures::Strip => {}
    }
}

/// Sanitization suitable for a logical single-line label.
pub fn sanitize_line(input: &str, ascii: bool) -> Cow<'_, str> {
    sanitize_text(
        input,
        SanitizeOptions {
            controls: if ascii {
                ControlPictures::Ascii
            } else {
                ControlPictures::Unicode
            },
            preserve_newlines: false,
            preserve_tabs: false,
        },
    )
}

/// A destination that is safe to place inside an OSC 8 payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeUrl(String);

impl SafeUrl {
    /// Validate a caller-provided URL. Network, mail, and local-file schemes are
    /// accepted; active-content schemes are rejected.
    pub fn parse(target: &str) -> Option<Self> {
        let target = target.trim();
        if target.is_empty()
            || target.chars().any(|character| {
                character.is_control()
                    || character == '\x1b'
                    || character == '\u{009c}'
                    || is_bidi_override(character)
            })
        {
            return None;
        }
        let colon = target.find(':')?;
        let scheme = &target[..colon];
        if scheme.is_empty()
            || !scheme.chars().enumerate().all(|(index, character)| {
                if index == 0 {
                    character.is_ascii_alphabetic()
                } else {
                    character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
                }
            })
        {
            return None;
        }
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "mailto" | "file" | "ssh" | "git"
        ) {
            return None;
        }

        let mut escaped = String::with_capacity(target.len());
        for byte in target.bytes() {
            if byte <= 0x20 || byte >= 0x7f || matches!(byte, b'\\' | b'\x1b' | b'\x07') {
                use std::fmt::Write;
                let _ = write!(escaped, "%{byte:02X}");
            } else {
                escaped.push(char::from(byte));
            }
        }
        Some(Self(escaped))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Create an OSC 8 hyperlink only from validated data. The visible target is
/// intentionally not hidden: when label and target differ the target is shown.
pub fn safe_hyperlink(label: &str, target: &str, enabled: bool, ascii: bool) -> String {
    let label = sanitize_line(label, ascii);
    let target_label = sanitize_line(target, ascii);
    let visible = if label.as_ref() == target_label.as_ref() || label.is_empty() {
        target_label.into_owned()
    } else {
        format!("{} ({})", label, target_label)
    };
    if !enabled {
        return visible;
    }
    let Some(target) = SafeUrl::parse(target) else {
        return visible;
    };
    format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", target.as_str(), visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_terminal_controls_are_neutralized() {
        let input = concat!(
            "ok\x1b[31mred\x1b[0m",
            "\x1b]0;title\x07",
            "\x1b]52;c;Y2xpcA==\x07",
            "\x1b[2J\x1b[6n"
        );
        let safe = sanitize_text(input, SanitizeOptions::default());
        assert!(!safe.contains('\x1b'));
        assert!(!safe.contains('\x07'));
        assert!(safe.contains('␛'));
        assert!(safe.contains("title"));
    }

    #[test]
    fn tabs_and_newlines_follow_component_policy() {
        let safe = sanitize_text("a\tb\r\nc", SanitizeOptions::default());
        assert_eq!(safe, "a\tb\nc");
        assert_eq!(sanitize_line("a\tb\nc", true), "a<U+0009>b<U+000A>c");
    }

    #[test]
    fn hyperlinks_show_the_destination_and_reject_active_content() {
        let link = safe_hyperlink("docs", "https://example.com/a b", true, false);
        assert!(link.contains("docs (https://example.com/a b)"));
        assert!(link.contains("a%20b"));
        let malicious = safe_hyperlink("click", "javascript:alert(1)", true, false);
        assert_eq!(malicious, "click (javascript:alert(1))");
        assert!(!malicious.contains('\x1b'));
        let unicode = SafeUrl::parse("https://example.com/界").unwrap();
        assert_eq!(unicode.as_str(), "https://example.com/%E7%95%8C");
    }
}
