#![allow(missing_docs)]

//! Terminal-aware boundaries for human-facing command output.
//!
//! Human-facing output is always control-safe, including when it is piped into
//! another program that later writes to a terminal. TTYs use compact Unicode
//! control pictures; redirected streams use ASCII descriptions.

use std::borrow::Cow;
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};

use sexy_tui_rs::{sanitize_line, sanitize_text, SanitizeOptions};

macro_rules! stderr {
    ($($argument:tt)*) => {
        $crate::output::stderr_line(format!($($argument)*))
    };
}
pub(crate) use stderr;

pub(crate) fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

fn safe_line(value: &str, terminal: bool) -> Cow<'_, str> {
    sanitize_line(value, !terminal)
}

fn safe_table_line(value: &str, terminal: bool) -> Cow<'_, str> {
    sanitize_text(
        value,
        SanitizeOptions {
            controls: if terminal {
                sexy_tui_rs::ControlPictures::Unicode
            } else {
                sexy_tui_rs::ControlPictures::Ascii
            },
            preserve_newlines: false,
            preserve_tabs: true,
        },
    )
}

fn safe_multiline(value: &str, terminal: bool) -> Cow<'_, str> {
    sanitize_text(
        value,
        SanitizeOptions {
            controls: if terminal {
                sexy_tui_rs::ControlPictures::Unicode
            } else {
                sexy_tui_rs::ControlPictures::Ascii
            },
            ..SanitizeOptions::default()
        },
    )
}

/// Sanitize one untrusted table field without consuming trusted separators.
pub(crate) fn table_field(value: &str, terminal: bool) -> Cow<'_, str> {
    safe_line(value, terminal)
}

fn write_line(mut writer: impl Write, value: &str, terminal: bool) {
    let _ = writeln!(writer, "{}", safe_line(value, terminal));
}

fn write_multiline(mut writer: impl Write, value: &str, terminal: bool) {
    let value = safe_multiline(value, terminal);
    let _ = write!(writer, "{value}");
    if !value.ends_with('\n') {
        let _ = writeln!(writer);
    }
}

pub(crate) fn stdout_line(value: impl Display) {
    let terminal = stdout_is_terminal();
    write_line(io::stdout().lock(), &value.to_string(), terminal);
}

pub(crate) fn stdout_table_line(value: impl Display) {
    let terminal = stdout_is_terminal();
    let value = value.to_string();
    let value = safe_table_line(&value, terminal);
    let _ = writeln!(io::stdout().lock(), "{value}");
}

pub(crate) fn stderr_line(value: impl Display) {
    let terminal = io::stderr().is_terminal();
    write_line(io::stderr().lock(), &value.to_string(), terminal);
}

pub(crate) fn stdout_multiline(value: impl Display) {
    let terminal = stdout_is_terminal();
    write_multiline(io::stdout().lock(), &value.to_string(), terminal);
}

pub(crate) fn stderr_multiline(value: impl Display) {
    let terminal = io::stderr().is_terminal();
    write_multiline(io::stderr().lock(), &value.to_string(), terminal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_lines_neutralize_controls_and_line_injection() {
        let rendered = safe_line("name\x1b]52;c;YXR0YWNr\x07\nforged\tcolumn", true);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\t'));
        assert!(rendered.contains('␛'));
        assert!(rendered.contains('␇'));
    }

    #[test]
    fn redirected_lines_use_ascii_control_descriptions() {
        let rendered = safe_line("name\x1b]52;c;YXR0YWNr\x07\nsecond\tcolumn", false);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\t'));
        assert!(rendered.contains("^["));
        assert!(rendered.contains("<BEL>"));
    }

    #[test]
    fn terminal_table_fields_cannot_add_rows_or_columns() {
        let field = table_field("name\nforged\tcolumn\x1b", true);
        assert!(!field.contains('\n'));
        assert!(!field.contains('\t'));
        let row = format!("id\t{field}\ttag");
        assert_eq!(safe_table_line(&row, true).matches('\t').count(), 2);
        assert!(!row.contains('\x1b'));
    }

    #[test]
    fn terminal_multiline_preserves_layout_but_not_commands() {
        let rendered = safe_multiline("first\nsecond\tvalue\x1b]0;owned\x07", true);
        assert!(rendered.contains("first\nsecond\tvalue"));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
    }
}
