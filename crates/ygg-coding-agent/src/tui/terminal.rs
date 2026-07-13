#![allow(missing_docs)]

use std::io::{Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::{cursor, event, execute, terminal};

/// Shared dimensions reachable by both the boxed terminal and the shell.
pub type TerminalSize = Arc<Mutex<(u16, u16)>>;

static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

// sexy-tui wraps every frame in synchronized-output mode. Buffer the many
// per-line Terminal::write calls until this delimiter so one frame reaches the
// terminal in one flush rather than dozens of tiny writes.
const SYNC_OUTPUT_BEGIN: &str = "\x1b[?2026h";
const SYNC_OUTPUT_END: &str = "\x1b[?2026l";

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
    let raw_active = RAW_ACTIVE.swap(false, Ordering::SeqCst);
    let keyboard_enhancement_active = KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::SeqCst);
    if !raw_active && !keyboard_enhancement_active {
        return;
    }

    let mut out = std::io::stdout();
    if keyboard_enhancement_active {
        let _ = execute!(out, event::PopKeyboardEnhancementFlags);
    }
    if raw_active {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            out,
            event::DisableBracketedPaste,
            event::DisableMouseCapture,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
    }
    let _ = out.flush();
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
    pending: Vec<u8>,
    in_synchronized_frame: bool,
}

impl YggTerminal {
    /// Enter raw mode and the alternate screen, returning the shared size cell.
    #[allow(dead_code)] // Used by the separately compiled Gate-0 spike target.
    pub fn enter() -> Result<(Self, TerminalSize)> {
        let size = Arc::new(Mutex::new(terminal::size().unwrap_or((80, 24))));
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
            event::EnableBracketedPaste,
            cursor::Hide
        )?;
        // In compatible terminals, CSI-u makes Ctrl+Enter distinct from a
        // plain Enter. Unsupported terminals safely ignore this request; the
        // Alt+Enter binding remains the portable multiline fallback.
        if execute!(
            out,
            event::PushKeyboardEnhancementFlags(
                event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )
        .is_ok()
        {
            KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::SeqCst);
        }
        let detected_size = terminal::size()
            .unwrap_or_else(|_| *size.lock().expect("terminal size mutex poisoned"));
        *size.lock().expect("terminal size mutex poisoned") = detected_size;
        Ok(Self {
            out,
            size,
            last_was_cr: false,
            pending: Vec::with_capacity(16 * 1024),
            in_synchronized_frame: false,
        })
    }

    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let _ = self.out.write_all(&self.pending);
        self.pending.clear();
        let _ = self.out.flush();
    }

    fn flush_before_control_sequence(&mut self) {
        // Cursor and clear operations must occur after already-buffered cursor
        // movement, especially during sexy-tui's differential tail redraw.
        self.flush_pending();
    }
}

impl Drop for YggTerminal {
    fn drop(&mut self) {
        self.flush_pending();
        force_restore();
    }
}

impl sexy_tui_rs::Terminal for YggTerminal {
    fn start(&mut self, _on_input: Box<dyn FnMut(&str)>, _on_resize: Box<dyn FnMut()>) {
        unreachable!("YggTerminal::start is never called; input is driven by the select! loop");
    }

    fn stop(&mut self) {
        self.flush_pending();
        force_restore();
    }

    fn write(&mut self, data: &str) {
        // sexy-tui writes each rendered line and its `\n` separately. In raw
        // mode LF alone advances vertically but does not reliably return to
        // column zero, which corrupts differential frames. Normalize to CRLF
        // at this terminal boundary while preserving existing CRLF sequences.
        let normalized = normalize_line_endings(data, &mut self.last_was_cr);
        self.pending.extend_from_slice(normalized.as_bytes());
        if data.contains(SYNC_OUTPUT_BEGIN) {
            self.in_synchronized_frame = true;
        }
        if data.contains(SYNC_OUTPUT_END) {
            self.in_synchronized_frame = false;
            self.flush_pending();
        } else if !self.in_synchronized_frame {
            // Defensive fallback for a direct Terminal::write outside a TUI
            // frame; do not leave it buffered indefinitely.
            self.flush_pending();
        }
    }

    fn columns(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").0
    }

    fn rows(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").1
    }

    fn move_by(&mut self, lines: i16) {
        self.flush_before_control_sequence();
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
        self.flush_before_control_sequence();
        let _ = execute!(self.out, cursor::Hide);
    }

    fn show_cursor(&mut self) {
        self.flush_before_control_sequence();
        let _ = execute!(self.out, cursor::Show);
    }

    fn clear_line(&mut self) {
        self.flush_before_control_sequence();
        let _ = execute!(self.out, terminal::Clear(terminal::ClearType::CurrentLine));
    }

    fn clear_from_cursor(&mut self) {
        self.flush_before_control_sequence();
        let _ = execute!(
            self.out,
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
    }

    fn clear_screen(&mut self) {
        self.flush_before_control_sequence();
        let _ = execute!(self.out, terminal::Clear(terminal::ClearType::All));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_restore_is_idempotent_without_a_terminal() {
        RAW_ACTIVE.store(false, Ordering::SeqCst);
        KEYBOARD_ENHANCEMENT_ACTIVE.store(false, Ordering::SeqCst);
        force_restore();
        force_restore();
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
        assert!(!KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn synchronized_output_markers_delimit_a_render_frame() {
        assert!(SYNC_OUTPUT_BEGIN.starts_with("\x1b[?2026"));
        assert!(SYNC_OUTPUT_END.starts_with("\x1b[?2026"));
        assert_ne!(SYNC_OUTPUT_BEGIN, SYNC_OUTPUT_END);
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
        KEYBOARD_ENHANCEMENT_ACTIVE.store(false, Ordering::SeqCst);
        let result: Result<()> = Err(anyhow::anyhow!("simulated alternate-screen failure"));
        if result.is_err() {
            force_restore();
        }
        assert!(!RAW_ACTIVE.load(Ordering::SeqCst));
        assert!(!KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::SeqCst));
    }
}
