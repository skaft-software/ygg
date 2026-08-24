//! Terminal-owned scrollback reset and replay policy.

use crate::terminal::Terminal;
use crate::tui::delete_all_kitty_images;

/// Destructively replace the terminal's screen and saved lines, then replay the
/// application-owned rows. The caller brackets this operation in synchronized
/// output when the terminal supports it.
pub(crate) fn reset_and_replay<'a>(
    terminal: &mut dyn Terminal,
    delete_kitty_images: bool,
    lines: impl IntoIterator<Item = &'a str>,
) {
    if delete_kitty_images {
        terminal.write(&delete_all_kitty_images());
    }

    // ED 2 can move the old grid into a multiplexer's history. Clear saved
    // lines only after it, matching Pi's terminal-independent resize policy.
    terminal.clear_screen();
    terminal.write("\x1b[H\x1b[3J");

    let mut lines = lines.into_iter().peekable();
    while let Some(line) = lines.next() {
        terminal.write(line);
        if lines.peek().is_some() {
            terminal.write("\n");
        }
    }
}
