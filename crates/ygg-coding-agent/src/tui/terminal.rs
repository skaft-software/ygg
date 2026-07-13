#![allow(missing_docs)]

use std::cell::Cell;
use std::io::{Stdout, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{cursor, event, execute, terminal};

/// Shared dimensions reachable by both the boxed terminal and the shell.
pub type TerminalSize = Rc<Cell<(u16, u16)>>;

static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);

fn normalize_line_endings(data: &str, last_was_cr: &mut bool) -> String {
    let mut normalized = String::with_capacity(data.len().saturating_add(8));
    for character in data.chars() {
        if character == '\n' && !*last_was_cr {
            normalized.push('\r');
        }
        normalized.push(character);
        *last_was_cr = character == '\r';
    }
    normalized
}

/// Restore the process terminal state. Repeated calls are harmless.
pub fn force_restore() {
    if RAW_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            event::DisableMouseCapture,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        let _ = out.flush();
    }
}

/// Install a panic hook which restores the terminal before delegating to the
/// hook that was installed by the caller (or by the standard library).
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        force_restore();
        previous(info);
    }));
}

/// Render-only terminal adapter used by sexy-tui.
///
/// Input is deliberately driven by the application's async crossterm stream;
/// sexy-tui's blocking `Terminal::start` is never called.
pub struct YggTerminal {
    out: Stdout,
    size: TerminalSize,
    last_was_cr: bool,
}

impl YggTerminal {
    /// Enter raw mode and the alternate screen, returning the shared size cell.
    #[allow(dead_code)] // Used by the separately compiled Gate-0 spike target.
    pub fn enter() -> Result<(Self, TerminalSize)> {
        let size = Rc::new(Cell::new(terminal::size().unwrap_or((80, 24))));
        let terminal = Self::enter_with_size(size.clone())?;
        Ok((terminal, size))
    }

    /// Enter using a caller-owned shared dimensions cell. This lets the shell
    /// update dimensions after resize while the terminal is boxed in the TUI.
    pub fn enter_with_size(size: TerminalSize) -> Result<Self> {
        terminal::enable_raw_mode()?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);

        let result = Self::enter_inner(size);
        if result.is_err() {
            force_restore();
        }
        result
    }

    fn enter_inner(size: TerminalSize) -> Result<Self> {
        let mut out = std::io::stdout();
        execute!(
            out,
            terminal::EnterAlternateScreen,
            event::EnableMouseCapture,
            cursor::Hide
        )?;
        size.set(terminal::size().unwrap_or(size.get()));
        Ok(Self {
            out,
            size,
            last_was_cr: false,
        })
    }
}

impl Drop for YggTerminal {
    fn drop(&mut self) {
        force_restore();
    }
}

impl sexy_tui_rs::Terminal for YggTerminal {
    fn start(&mut self, _on_input: Box<dyn FnMut(&str)>, _on_resize: Box<dyn FnMut()>) {
        unreachable!("YggTerminal::start is never called; input is driven by the select! loop");
    }

    fn stop(&mut self) {
        force_restore();
    }

    fn write(&mut self, data: &str) {
        // sexy-tui writes each rendered line and its `\n` separately. In raw
        // mode LF alone advances vertically but does not reliably return to
        // column zero, which corrupts differential frames. Normalize to CRLF
        // at this terminal boundary while preserving existing CRLF sequences.
        let normalized = normalize_line_endings(data, &mut self.last_was_cr);
        let _ = self.out.write_all(normalized.as_bytes());
        let _ = self.out.flush();
    }

    fn columns(&self) -> u16 {
        self.size.get().0
    }

    fn rows(&self) -> u16 {
        self.size.get().1
    }

    fn move_by(&mut self, lines: i16) {
        let result = if lines > 0 {
            execute!(self.out, cursor::MoveDown(lines as u16))
        } else if lines < 0 {
            execute!(self.out, cursor::MoveUp((-lines) as u16))
        } else {
            Ok(())
        };
        let _ = result;
    }

    fn hide_cursor(&mut self) {
        let _ = execute!(self.out, cursor::Hide);
    }

    fn show_cursor(&mut self) {
        let _ = execute!(self.out, cursor::Show);
    }

    fn clear_line(&mut self) {
        let _ = execute!(self.out, terminal::Clear(terminal::ClearType::CurrentLine));
    }

    fn clear_from_cursor(&mut self) {
        let _ = execute!(
            self.out,
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
    }

    fn clear_screen(&mut self) {
        let _ = execute!(self.out, terminal::Clear(terminal::ClearType::All));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_restore_is_idempotent_without_a_terminal() {
        RAW_ACTIVE.store(false, Ordering::SeqCst);
        force_restore();
        force_restore();
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn output_newlines_return_to_column_zero_across_write_calls() {
        let mut last_was_cr = false;
        assert_eq!(normalize_line_endings("line", &mut last_was_cr), "line");
        assert_eq!(
            normalize_line_endings("\nnext", &mut last_was_cr),
            "\r\nnext"
        );
        assert_eq!(normalize_line_endings("\r", &mut last_was_cr), "\r");
        assert_eq!(normalize_line_endings("\n", &mut last_was_cr), "\n");
    }

    #[test]
    fn setup_failure_disarms_the_restore_guard() {
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        let result: Result<()> = Err(anyhow::anyhow!("simulated alternate-screen failure"));
        if result.is_err() {
            force_restore();
        }
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
    }
}
