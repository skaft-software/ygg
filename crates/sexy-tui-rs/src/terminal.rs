//! Terminal abstraction layer.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, ClearType};

use crate::keys::{is_kitty_protocol_active, set_kitty_protocol_active};
use crate::terminal_colors::is_osc11_background_color_response;
use crate::terminal_image::get_capabilities;

/// Poll interval for the input loop shutdown check.
const POLL_TIMEOUT_MS: u64 = 50;
const KITTY_NEGOTIATION_QUERY: &str = "\x1b[>7u\x1b[?u\x1b[c";
const MODIFY_OTHER_KEYS_ENABLE: &str = "\x1b[>4;2m";
const MODIFY_OTHER_KEYS_DISABLE: &str = "\x1b[>4;0m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    KittyFlags(u16),
    DeviceAttributes,
}

/// Parse exactly the startup replies consumed by Pi's keyboard negotiation.
pub fn parse_keyboard_protocol_negotiation_sequence(
    sequence: &str,
) -> Option<KeyboardProtocolNegotiationSequence> {
    if let Some(flags) = sequence
        .strip_prefix("\x1b[?")
        .and_then(|value| value.strip_suffix('u'))
        .and_then(|value| value.parse::<u16>().ok())
    {
        return Some(KeyboardProtocolNegotiationSequence::KittyFlags(flags));
    }
    let attributes = sequence
        .strip_prefix("\x1b[")
        .and_then(|value| value.strip_suffix('c'))?;
    attributes
        .strip_prefix('?')
        .is_some_and(|value| {
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b';')
        })
        .then_some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
}

pub fn is_apple_terminal_session() -> bool {
    cfg!(target_os = "macos")
        && std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "Apple_Terminal")
}

pub fn normalize_apple_terminal_input(
    data: &str,
    apple_terminal: bool,
    shift_pressed: bool,
) -> String {
    if data == "\r" && apple_terminal && shift_pressed {
        "\x1b[13;2u".into()
    } else {
        data.into()
    }
}

/// Terminal-produced text for a key event.
///
/// This deliberately trusts the character selected by the terminal/protocol,
/// rather than deriving text from a physical base key and its modifiers.  Alt
/// and Ctrl+Alt remain text-capable so Option and AltGr input survives; plain
/// Ctrl and system modifiers remain shortcut space.
pub fn key_text(key: &KeyEvent) -> Option<char> {
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    if character.is_control()
        || key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
    {
        return None;
    }
    Some(character)
}

/// A semantic terminal input event.
///
/// `Text` and `Paste` intentionally remain distinct.  Converting a paste into
/// synthetic key bytes loses multiline boundaries and makes a CSI-u printable
/// key indistinguishable from a terminal control sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInput {
    Text(String),
    Paste(String),
    Key(KeyEvent),
}

impl TerminalInput {
    /// Compatibility representation for older string-only consumers.
    ///
    /// New code should use [`TUI::handle_terminal_input`](crate::tui::TUI::handle_terminal_input)
    /// so paste and textual input keep their semantics.
    fn legacy_data(&self) -> Option<String> {
        match self {
            Self::Text(text) | Self::Paste(text) => Some(text.clone()),
            Self::Key(key) => key_to_string(key),
        }
    }
}

fn forwards_key_event(key: &KeyEvent) -> bool {
    match key.kind {
        KeyEventKind::Press => true,
        KeyEventKind::Release => false,
        KeyEventKind::Repeat => match key.code {
            KeyCode::Char(_) => key_text(key).is_some(),
            KeyCode::Backspace => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER),
            KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => key.modifiers.is_empty(),
            _ => false,
        },
    }
}

fn input_from_key(key: KeyEvent) -> TerminalInput {
    match key_text(&key) {
        Some(character) => TerminalInput::Text(character.to_string()),
        None => TerminalInput::Key(key),
    }
}

/// Trait for terminal I/O implementations.
pub trait Terminal {
    /// Start the terminal with semantic input and resize handlers.
    fn start_events(
        &mut self,
        on_input: Box<dyn FnMut(TerminalInput)>,
        on_resize: Box<dyn FnMut()>,
    );

    /// Start with the legacy string-only callback.
    ///
    /// This adapter remains for compatibility. New consumers should use
    /// [`Terminal::start_events`] so bracketed paste and text cannot be
    /// confused with encoded key-control sequences.
    fn start(&mut self, mut on_input: Box<dyn FnMut(&str)>, on_resize: Box<dyn FnMut()>) {
        self.start_events(
            Box::new(move |event| {
                if let Some(data) = event.legacy_data() {
                    on_input(&data);
                }
            }),
            on_resize,
        );
    }

    /// Stop the terminal and restore state.
    fn stop(&mut self);

    /// Write data to the terminal.
    fn write(&mut self, data: &str);

    /// Get current terminal width in columns.
    fn columns(&self) -> u16;

    /// Get current terminal height in rows.
    fn rows(&self) -> u16;

    /// Move cursor by N lines (negative = up).
    fn move_by(&mut self, lines: i16);

    /// Hide the terminal cursor.
    fn hide_cursor(&mut self);

    /// Show the terminal cursor.
    fn show_cursor(&mut self);

    /// Clear the current line.
    fn clear_line(&mut self);

    /// Clear from cursor position to end of screen.
    fn clear_from_cursor(&mut self);

    /// Clear the entire screen.
    fn clear_screen(&mut self);

    /// Whether Kitty progressive keyboard enhancement was negotiated.
    fn kitty_protocol_active(&self) -> bool {
        is_kitty_protocol_active()
    }

    /// Drain late key releases before returning control to a parent shell.
    fn drain_input(&mut self, max_ms: u64, idle_ms: u64) {
        let started = std::time::Instant::now();
        loop {
            if started.elapsed() >= Duration::from_millis(max_ms) {
                break;
            }
            match event::poll(Duration::from_millis(idle_ms)) {
                Ok(true) => {
                    let _ = event::read();
                }
                _ => break,
            }
        }
    }

    fn set_title(&mut self, title: &str) {
        let title: String = title
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        self.write(&format!("\x1b]2;{title}\x07"));
    }

    fn set_progress(&mut self, active: bool) {
        self.write(if active {
            "\x1b]9;4;1;0\x1b\\"
        } else {
            "\x1b]9;4;0\x1b\\"
        });
    }

    /// Backend capability profile. Custom terminals should override this when
    /// they have negotiated more precise support than environment detection.
    fn capabilities(&self) -> crate::capabilities::TerminalCapabilities {
        crate::capabilities::TerminalCapabilities::detect()
    }
}

/// Production terminal implementation using crossterm.
pub struct ProcessTerminal {
    stdout: io::Stdout,
    columns: u16,
    rows: u16,
    raw_mode: bool,
    keyboard_enhancement_active: bool,
    modify_other_keys_active: bool,
    shutdown: Arc<AtomicBool>,
    input_thread: Option<JoinHandle<()>>,
}

impl ProcessTerminal {
    pub fn new() -> io::Result<Self> {
        let (cols, rows) = terminal::size()?;
        Ok(ProcessTerminal {
            stdout: io::stdout(),
            columns: cols,
            rows,
            raw_mode: false,
            keyboard_enhancement_active: false,
            modify_other_keys_active: false,
            shutdown: Arc::new(AtomicBool::new(false)),
            input_thread: None,
        })
    }

    /// Restore raw-mode and progressive-keyboard state.
    fn restore(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        execute!(self.stdout, cursor::Show).ok();
        execute!(self.stdout, crossterm::style::Print("\x1b[?2004l")).ok();
        if self.keyboard_enhancement_active {
            let _ = self.stdout.write_all(b"\x1b[<u");
            self.keyboard_enhancement_active = false;
        }
        if self.modify_other_keys_active {
            let _ = self.stdout.write_all(MODIFY_OTHER_KEYS_DISABLE.as_bytes());
            self.modify_other_keys_active = false;
        }
        set_kitty_protocol_active(false);
        if self.raw_mode {
            terminal::disable_raw_mode().ok();
            self.raw_mode = false;
        }
        if let Some(handle) = self.input_thread.take() {
            let _ = handle.join();
        }
    }

    /// Ask capable terminals for unambiguous modified controls and
    /// layout-resolved alternate text, while leaving ordinary text in the
    /// normal terminal path. Unsupported terminals ignore the request.
    fn enable_keyboard_enhancements(&mut self) {
        // Pi asks for flags 1, 2 and 4 before querying. The DA request is the
        // sentinel used to select modifyOtherKeys on terminals without Kitty.
        let _ = self.stdout.write_all(KITTY_NEGOTIATION_QUERY.as_bytes());
        let kitty_supported = get_capabilities().kitty_keyboard;
        if kitty_supported {
            // The first sequence in KITTY_NEGOTIATION_QUERY already pushes
            // flags 1|2|4; do not push them a second time via crossterm.
            self.keyboard_enhancement_active = true;
            set_kitty_protocol_active(true);
        } else {
            let _ = self.stdout.write_all(MODIFY_OTHER_KEYS_ENABLE.as_bytes());
            self.modify_other_keys_active = true;
            set_kitty_protocol_active(false);
        }
        let _ = self.stdout.flush();
    }
}

impl Drop for ProcessTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

impl Terminal for ProcessTerminal {
    fn start_events(
        &mut self,
        mut on_input: Box<dyn FnMut(TerminalInput)>,
        mut on_resize: Box<dyn FnMut()>,
    ) {
        self.shutdown.store(false, Ordering::Release);
        // Enable raw mode
        terminal::enable_raw_mode().expect("Failed to enable raw mode");
        self.raw_mode = true;

        // Hide cursor
        execute!(self.stdout, cursor::Hide).ok();

        // Enable bracketed paste
        execute!(self.stdout, crossterm::style::Print("\x1b[?2004h")).ok();

        // Preserve ordinary terminal text while making modified controls and
        // layout-resolved alternate key text unambiguous.
        self.enable_keyboard_enhancements();

        enum ProcessEvent {
            Input(TerminalInput),
            Resize(u16, u16),
        }

        // Spawn input reader thread with shutdown signalling
        let (tx, rx) = mpsc::channel();
        let tx_for_thread = tx.clone(); // clone for the thread; keep tx for drop signalling
        let shutdown_flag = Arc::clone(&self.shutdown);

        let handle = thread::spawn(move || {
            loop {
                // Check shutdown flag before blocking on event::read
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }
                // Poll with timeout so we can check the shutdown flag periodically
                if let Ok(true) = event::poll(Duration::from_millis(POLL_TIMEOUT_MS)) {
                    if let Ok(event) = event::read() {
                        match event {
                            Event::Key(key_event) if forwards_key_event(&key_event) => {
                                if tx_for_thread
                                    .send(ProcessEvent::Input(input_from_key(key_event)))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Event::Paste(text) => {
                                if tx_for_thread
                                    .send(ProcessEvent::Input(TerminalInput::Paste(text)))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Event::Resize(columns, rows)
                                if tx_for_thread
                                    .send(ProcessEvent::Resize(columns, rows))
                                    .is_err() =>
                            {
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
        self.input_thread = Some(handle);

        // Drop our sender clone so the receiver loop can detect
        // when the input thread has stopped.
        drop(tx);

        // Process input events in a loop that checks the shutdown flag.
        // The receiver will yield None when all senders are dropped
        // (i.e. the input thread exited).
        loop {
            // Check shutdown flag so we don't block forever if the
            // input thread is still running but we've been told to stop.
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(POLL_TIMEOUT_MS)) {
                Ok(ProcessEvent::Resize(columns, rows)) => {
                    self.columns = columns;
                    self.rows = rows;
                    on_resize();
                }
                Ok(ProcessEvent::Input(input)) => {
                    // OSC 11 replies are terminal metadata, never user text.
                    // A bracketed paste remains a Paste event and therefore
                    // cannot be mistaken for one of these replies.
                    if matches!(&input, TerminalInput::Text(text) if is_osc11_background_color_response(text))
                    {
                        continue;
                    }
                    on_input(input);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No event yet — loop back to check shutdown flag
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Input thread exited — clean shutdown
                    break;
                }
            }
        }
    }

    fn stop(&mut self) {
        self.restore();
    }

    fn write(&mut self, data: &str) {
        let _ = self.stdout.write_all(data.as_bytes());
        let _ = self.stdout.flush();
    }

    fn columns(&self) -> u16 {
        self.columns
    }
    fn rows(&self) -> u16 {
        self.rows
    }

    fn move_by(&mut self, lines: i16) {
        if lines < 0 {
            execute!(self.stdout, cursor::MoveUp((-lines) as u16)).ok();
        } else {
            execute!(self.stdout, cursor::MoveDown(lines as u16)).ok();
        }
    }

    fn hide_cursor(&mut self) {
        execute!(self.stdout, cursor::Hide).ok();
    }

    fn show_cursor(&mut self) {
        execute!(self.stdout, cursor::Show).ok();
    }

    fn clear_line(&mut self) {
        execute!(
            self.stdout,
            crossterm::terminal::Clear(ClearType::CurrentLine)
        )
        .ok();
    }

    fn clear_from_cursor(&mut self) {
        execute!(
            self.stdout,
            crossterm::terminal::Clear(ClearType::FromCursorDown)
        )
        .ok();
    }

    fn clear_screen(&mut self) {
        execute!(self.stdout, crossterm::terminal::Clear(ClearType::All)).ok();
    }

    fn kitty_protocol_active(&self) -> bool {
        self.keyboard_enhancement_active
    }

    fn capabilities(&self) -> crate::capabilities::TerminalCapabilities {
        crate::terminal_image::get_capabilities()
    }
}

/// Convert a non-text crossterm KeyEvent to the legacy control representation.
///
/// Printable text never reaches this encoder on the semantic input path. It is
/// delivered as [`TerminalInput::Text`] instead, so CSI-u cannot be mistaken
/// for user text by a string-only widget.
pub(crate) fn key_to_string(event: &event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};

    let mut result = String::new();

    // Kitty protocol format: ESC [ codepoint ; modifier u
    if event.modifiers.contains(KeyModifiers::CONTROL)
        || event.modifiers.contains(KeyModifiers::ALT)
        || event.modifiers.contains(KeyModifiers::SUPER)
        || event.modifiers.contains(KeyModifiers::SHIFT)
    {
        let mut mod_val = 1u8; // 1-indexed
        if event.modifiers.contains(KeyModifiers::SHIFT) {
            mod_val += 1;
        }
        if event.modifiers.contains(KeyModifiers::ALT) {
            mod_val += 2;
        }
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            mod_val += 4;
        }
        if event.modifiers.contains(KeyModifiers::SUPER) {
            mod_val += 8;
        }

        match event.code {
            KeyCode::Char(c) => {
                return Some(format!("\x1b[{};{}u", c as u32, mod_val));
            }
            KeyCode::Enter => return Some(format!("\x1b[13;{}u", mod_val)),
            KeyCode::Tab => return Some(format!("\x1b[9;{}u", mod_val)),
            KeyCode::Backspace => return Some(format!("\x1b[127;{}u", mod_val)),
            KeyCode::Esc => return Some(format!("\x1b[27;{}u", mod_val)),
            _ => {}
        }
    }

    // Plain key events
    match event.code {
        KeyCode::Char(c) => result.push(c),
        KeyCode::Enter => result.push('\r'),
        KeyCode::Tab => result.push('\t'),
        KeyCode::Backspace => result.push('\x7f'),
        KeyCode::Esc => result.push('\x1b'),
        KeyCode::Up => result.push_str("\x1b[A"),
        KeyCode::Down => result.push_str("\x1b[B"),
        KeyCode::Left => result.push_str("\x1b[D"),
        KeyCode::Right => result.push_str("\x1b[C"),
        KeyCode::Home => result.push_str("\x1b[H"),
        KeyCode::End => result.push_str("\x1b[F"),
        KeyCode::Delete => result.push_str("\x1b[3~"),
        KeyCode::Insert => result.push_str("\x1b[2~"),
        KeyCode::PageUp => result.push_str("\x1b[5~"),
        KeyCode::PageDown => result.push_str("\x1b[6~"),
        KeyCode::F(n) if n <= 12 => {
            result.push_str(&format!(
                "\x1b[{}~",
                match n {
                    1 => "11",
                    2 => "12",
                    3 => "13",
                    4 => "14",
                    5 => "15",
                    6 => "17",
                    7 => "18",
                    8 => "19",
                    9 => "20",
                    10 => "21",
                    11 => "23",
                    12 => "24",
                    _ => unreachable!(),
                }
            ));
        }
        _ => {}
    }

    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_keyboard_negotiation_and_apple_return_normalization() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?7u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags(7))
        );
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?62;4;52c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
        assert_eq!(
            normalize_apple_terminal_input("\r", true, true),
            "\x1b[13;2u"
        );
        assert_eq!(normalize_apple_terminal_input("\r", true, false), "\r");
        assert_eq!(normalize_apple_terminal_input("a", true, true), "a");
    }

    #[test]
    fn process_terminal_emits_text_and_paste_semantically() {
        assert_eq!(
            input_from_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)),
            TerminalInput::Text("A".into())
        );
        assert_eq!(
            input_from_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::ALT)),
            TerminalInput::Text("é".into())
        );
        assert_eq!(
            input_from_key(KeyEvent::new(
                KeyCode::Char('€'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            TerminalInput::Text("€".into())
        );
        assert_eq!(
            input_from_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            TerminalInput::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            TerminalInput::Paste("one\r\ntwo".into()).legacy_data(),
            Some("one\r\ntwo".into())
        );
    }

    #[test]
    fn process_terminal_repeats_only_editing_and_navigation_keys() {
        let key = |code, modifiers, kind| KeyEvent::new_with_kind(code, modifiers, kind);
        assert!(forwards_key_event(&key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        )));
        assert!(forwards_key_event(&key(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )));
        assert!(forwards_key_event(&key(
            KeyCode::Left,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )));
        assert!(!forwards_key_event(&key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )));
        assert!(!forwards_key_event(&key(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
        assert!(!forwards_key_event(&key(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )));
    }
}
