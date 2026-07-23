#![allow(missing_docs)]

use std::io::{IsTerminal, Stdout, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{cursor, event, execute, queue, terminal};
use sexy_tui_rs::{
    ColorDepth as SexyColorDepth, SupportLevel as SexySupportLevel,
    TerminalCapabilities as SexyTerminalCapabilities, TerminalSize as SexyTerminalSize,
};

/// Shared dimensions reachable by both the boxed terminal and the shell.
pub type TerminalSize = Arc<Mutex<(u16, u16)>>;

/// ANSI colour policy. Structural glyph and cursor capabilities are detected
/// separately, so forcing colour never forces an alternate-screen TUI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "always" | "on" => Ok(Self::Always),
            "never" | "off" => Ok(Self::Never),
            _ => anyhow::bail!("invalid colour mode {value:?}; use auto, always, or never"),
        }
    }
}

/// Colour precision that can be emitted without changing the information
/// architecture. `None` also suppresses non-colour SGR attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// Capabilities selected before any terminal control sequence is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// Safe to enter raw mode and use differential cursor rendering.
    pub interactive: bool,
    /// Safe Unicode is expected to render as single-cell glyphs.
    pub unicode: bool,
    /// Selected foreground-colour precision.
    pub color: ColorDepth,
    /// Optional italic SGR is likely to work.
    pub italics: bool,
    /// OSC 8 links are supported by the detected terminal family. Markdown
    /// destinations remain visible text when this is false.
    pub hyperlinks: bool,
    /// Cursor-rewritten animation is allowed. Ygg's elapsed clock does not
    /// depend on this flag for comprehension.
    pub animation: bool,
}

#[derive(Clone, Debug)]
struct CapabilityProbe {
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<String>,
    term_program: Option<String>,
    colorterm: Option<String>,
    locale: Option<String>,
    no_color: bool,
    explicit_plain: bool,
}

fn known_terminal(term: &str) -> bool {
    [
        "xterm",
        "screen",
        "tmux",
        "rxvt",
        "vt",
        "ansi",
        "linux",
        "cygwin",
        "cons",
        "eterm",
        "konsole",
        "gnome",
        "putty",
        "st",
        "alacritty",
        "kitty",
        "foot",
        "wezterm",
        "ghostty",
        "contour",
        "rio",
        "mlterm",
        "terminator",
        "fbterm",
        "iterm",
    ]
    .iter()
    .any(|family| {
        term == *family
            || term.strip_prefix(family).is_some_and(|suffix| {
                suffix.chars().next().is_some_and(|character| {
                    matches!(character, '-' | '_') || character.is_ascii_digit()
                })
            })
    })
}

impl TerminalCapabilities {
    /// Detect the frontend tier. Unknown, dumb, redirected, and explicitly
    /// plain environments never enter alternate-screen mode.
    pub fn detect(color_mode: ColorMode, explicit_plain: bool) -> Self {
        let locale = std::env::var("LC_ALL")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::env::var("LC_CTYPE")
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| std::env::var("LANG").ok().filter(|value| !value.is_empty()));
        Self::from_probe(
            color_mode,
            CapabilityProbe {
                stdin_tty: std::io::stdin().is_terminal(),
                stdout_tty: std::io::stdout().is_terminal(),
                term: std::env::var("TERM").ok().filter(|value| !value.is_empty()),
                term_program: std::env::var("TERM_PROGRAM")
                    .ok()
                    .filter(|value| !value.is_empty()),
                colorterm: std::env::var("COLORTERM")
                    .ok()
                    .filter(|value| !value.is_empty()),
                locale,
                no_color: std::env::var_os("NO_COLOR").is_some(),
                explicit_plain,
            },
        )
    }

    fn from_probe(color_mode: ColorMode, probe: CapabilityProbe) -> Self {
        let term = probe
            .term
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let known = known_terminal(&term);
        let interactive = probe.stdin_tty && probe.stdout_tty && known && !probe.explicit_plain;
        let unicode = !probe.explicit_plain
            && known
            && probe.locale.as_deref().is_some_and(|locale| {
                let locale = locale.to_ascii_lowercase();
                locale.contains("utf-8") || locale.contains("utf8")
            });

        let ansi_allowed = !probe.no_color
            && term != "dumb"
            && !probe.explicit_plain
            && match color_mode {
                ColorMode::Auto => probe.stdout_tty && known,
                ColorMode::Always => true,
                ColorMode::Never => false,
            };
        let color = if !ansi_allowed {
            ColorDepth::None
        } else if probe.colorterm.as_deref().is_some_and(|value| {
            value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
        }) || term.contains("ghostty")
            || term.contains("kitty")
            || term.contains("wezterm")
        {
            ColorDepth::TrueColor
        } else if term.contains("256color") {
            ColorDepth::Ansi256
        } else {
            ColorDepth::Ansi16
        };

        let rich_terminal = term.contains("ghostty")
            || term.contains("kitty")
            || term.contains("wezterm")
            || probe.term_program.as_deref().is_some_and(|program| {
                program == "iTerm.app" || program == "WezTerm" || program == "ghostty"
            });
        // Apple Terminal supports SGR italics even though it is not an OSC 8
        // hyperlink target. Keep the capabilities separate: otherwise the
        // rich renderer's italic fallback becomes underline, making every
        // thinking row look like a link.
        let apple_terminal = probe.term_program.as_deref().is_some_and(|program| {
            matches!(
                program.to_ascii_lowercase().as_str(),
                "apple_terminal" | "apple terminal"
            )
        });
        Self {
            interactive,
            unicode,
            color,
            italics: interactive && (rich_terminal || apple_terminal) && color != ColorDepth::None,
            hyperlinks: interactive && rich_terminal,
            animation: interactive && !probe.explicit_plain,
        }
    }

    #[cfg(test)]
    pub fn test(interactive: bool, unicode: bool, color: ColorDepth) -> Self {
        Self {
            interactive,
            unicode,
            color,
            italics: interactive && color == ColorDepth::TrueColor,
            hyperlinks: interactive && color == ColorDepth::TrueColor,
            animation: interactive,
        }
    }
}

/// Convert Ygg's negotiated terminal profile into the profile consumed by
/// `sexy-tui-rs`.  The latter's synchronized-output flag doubles as the only
/// frame-boundary callback exposed by its `Terminal` trait.  Ygg's backend
/// uses those delimiters to batch a complete differential frame into a few
/// writes; terminals that do not implement CSI 2026 safely ignore the private
/// mode while still receiving the same ordered bytes in one batch.
fn sexy_terminal_capabilities(
    capabilities: TerminalCapabilities,
    dimensions: (u16, u16),
) -> SexyTerminalCapabilities {
    if !capabilities.interactive {
        return SexyTerminalCapabilities::plain();
    }

    let color_depth = match capabilities.color {
        ColorDepth::None => SexyColorDepth::None,
        ColorDepth::Ansi16 => SexyColorDepth::Ansi16,
        ColorDepth::Ansi256 => SexyColorDepth::Ansi256,
        ColorDepth::TrueColor => SexyColorDepth::TrueColor,
    };
    let mut rendered = SexyTerminalCapabilities::interactive(color_depth, capabilities.unicode);
    rendered.italics = if capabilities.italics {
        SexySupportLevel::Supported
    } else {
        SexySupportLevel::Unsupported
    };
    rendered.hyperlinks = capabilities.hyperlinks;
    rendered.animation = capabilities.animation;
    rendered.dimensions = Some(SexyTerminalSize {
        columns: dimensions.0,
        rows: dimensions.1,
    });

    // This is both a progressive terminal feature and Ygg's render-frame
    // delimiter. The TUI otherwise calls `Terminal::write` once per row and
    // the adapter must flush every call because it has no end-of-frame signal.
    // CSI private modes are ignored by terminals without synchronized output,
    // so using the delimiter remains safe while the adapter still gains one
    // write/flush for the frame.
    rendered.synchronized_output = true;
    rendered.sync_output = true;
    rendered
}

static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
static SIGNAL_NUMBER: AtomicI32 = AtomicI32::new(0);
static SIGNAL_NOTIFY: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
const SIGNAL_WATCHDOG_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Keep ordinary text in the terminal's normal text path while asking Kitty
/// protocol terminals to include the layout-resolved alternate character for
/// modified keys.  `REPORT_ALL_KEYS_AS_ESCAPE_CODES` intentionally does not
/// belong here: it turns every printable key into a physical/base-key event,
/// and crossterm cannot recover associated IME/dead-key text from that form.
fn keyboard_enhancement_flags() -> event::KeyboardEnhancementFlags {
    event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

// sexy-tui wraps every frame in synchronized-output mode. Buffer the many
// per-line Terminal::write calls until this delimiter so one frame reaches the
// terminal in one flush rather than dozens of tiny writes.
const SYNC_OUTPUT_BEGIN: &str = "\x1b[?2026h";
const SYNC_OUTPUT_END: &str = "\x1b[?2026l";
const OSC11_BACKGROUND_QUERY: &str = "\x1b]11;?\x1b\\";

fn parse_osc11_background_from_buffer(
    buffer: &str,
) -> Option<sexy_tui_rs::terminal_colors::RgbColor> {
    for (start, _) in buffer.match_indices("\x1b]11;") {
        let tail = &buffer[start..];
        if let Some(end) = tail.find('\x07') {
            if let Some(color) =
                sexy_tui_rs::terminal_colors::parse_osc11_background_color(&tail[..=end])
            {
                return Some(color);
            }
        }
        if let Some(end) = tail.find("\x1b\\") {
            if let Some(color) =
                sexy_tui_rs::terminal_colors::parse_osc11_background_color(&tail[..end + 2])
            {
                return Some(color);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn append_event_text(buffer: &mut String, terminal_event: event::Event) {
    match terminal_event {
        event::Event::Key(key) => match key.code {
            event::KeyCode::Char(character) => buffer.push(character),
            event::KeyCode::Esc => buffer.push('\x1b'),
            _ => {}
        },
        event::Event::Paste(text) => buffer.push_str(&text),
        _ => {}
    }
}

#[cfg(unix)]
fn read_osc11_response(timeout: Duration) -> Option<sexy_tui_rs::terminal_colors::RgbColor> {
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buffer = String::with_capacity(128);
    loop {
        if let Some(color) = parse_osc11_background_from_buffer(&buffer) {
            return Some(color);
        }
        let now = Instant::now();
        if now >= deadline || buffer.len() > 512 {
            return None;
        }
        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .min(i32::MAX as u128) as i32;
        let mut fds = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `fds` points to one valid pollfd and lives for the call.
        let ready = unsafe { libc::poll(&mut fds, 1, remaining_ms) };
        if ready <= 0 || (fds.revents & libc::POLLIN) == 0 {
            return None;
        }
        let mut chunk = [0u8; 128];
        // SAFETY: `chunk` is a valid writable byte buffer and `fd` is stdin.
        let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if read <= 0 {
            return None;
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read as usize]));
    }
}

#[cfg(not(unix))]
fn read_osc11_response(timeout: Duration) -> Option<sexy_tui_rs::terminal_colors::RgbColor> {
    let deadline = Instant::now() + timeout;
    let mut buffer = String::with_capacity(128);
    loop {
        if let Some(color) = parse_osc11_background_from_buffer(&buffer) {
            return Some(color);
        }
        let now = Instant::now();
        if now >= deadline || buffer.len() > 512 {
            return None;
        }
        let remaining = deadline.saturating_duration_since(now);
        if !event::poll(remaining).ok()? {
            return None;
        }
        append_event_text(&mut buffer, event::read().ok()?);
    }
}

/// Query the terminal's default background via OSC 11 while raw mode is active.
/// This is intentionally a best-effort startup probe: environment/config wins,
/// unsupported terminals are ignored, and a timeout falls back to Unknown.
pub(crate) fn query_terminal_background_color(timeout: Duration) -> Option<(u8, u8, u8)> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    if !terminal::is_raw_mode_enabled().ok()? {
        return None;
    }

    let mut out = std::io::stdout();
    write!(out, "{OSC11_BACKGROUND_QUERY}").ok()?;
    out.flush().ok()?;

    let color = read_osc11_response(timeout)?;
    Some((color.r, color.g, color.b))
}

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
            cursor::SetCursorStyle::DefaultUserShape,
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

/// Returns the first Unix termination signal received by the process.
pub async fn wait_for_shutdown_signal() -> i32 {
    loop {
        let notified = SIGNAL_NOTIFY.notified();
        let signal = SIGNAL_NUMBER.load(Ordering::Acquire);
        if signal != 0 {
            return signal;
        }
        notified.await;
    }
}

/// Returns the pending Unix termination signal, if coordinated shutdown began.
pub fn received_shutdown_signal() -> Option<i32> {
    let signal = SIGNAL_NUMBER.load(Ordering::Acquire);
    (signal != 0).then_some(signal)
}

fn conventional_signal_exit_code(signal: i32) -> i32 {
    128i32.saturating_add(signal)
}

fn emergency_signal_exit(signal: i32) -> ! {
    ygg_agent::extension_process::force_kill_registered_process_groups();
    force_restore();
    std::process::exit(conventional_signal_exit_code(signal));
}

/// Begin the same coordinated shutdown path used by the Unix signal thread.
///
/// Raw terminal input turns Ctrl-C into a key event instead of a kernel
/// signal. Long-running lifecycle boundaries use this helper so that key is
/// still an immediate, level-triggered cancellation request with the same
/// bounded cleanup watchdog and conventional exit status as SIGINT.
pub fn request_coordinated_shutdown(signal: i32) -> std::io::Result<()> {
    if signal <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shutdown signal must be positive",
        ));
    }
    if SIGNAL_NUMBER
        .compare_exchange(0, signal, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        emergency_signal_exit(SIGNAL_NUMBER.load(Ordering::Acquire));
    }

    ygg_agent::extension_process::begin_host_shutdown();
    SIGNAL_NOTIFY.notify_waiters();
    std::thread::Builder::new()
        .name("ygg-signal-watchdog".into())
        .spawn(move || {
            std::thread::sleep(SIGNAL_WATCHDOG_TIMEOUT);
            emergency_signal_exit(signal);
        })?;
    Ok(())
}

/// Exit with the shell-conventional `128 + signal` status after cleanup.
pub fn exit_if_signaled() {
    if let Some(signal) = received_shutdown_signal() {
        emergency_signal_exit(signal);
    }
}

/// Installs level-triggered Unix termination handling.
///
/// The signal thread only announces shutdown. Async mode owners then stop
/// input, abort active runs, await process groups, stop extensions, and restore
/// the terminal. A short watchdog force-cleans children and terminal state if
/// an owner stalls; a repeated signal takes that emergency path immediately.
#[cfg(unix)]
pub fn install_signal_restore() -> std::io::Result<()> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGHUP, SIGINT, SIGQUIT, SIGTERM])?;
    std::thread::Builder::new()
        .name("ygg-signal-restore".into())
        .spawn(move || {
            for signal in signals.forever() {
                request_coordinated_shutdown(signal)?;
            }
            Ok::<(), std::io::Error>(())
        })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn install_signal_restore() -> std::io::Result<()> {
    Ok(())
}

/// Render-only terminal adapter used by sexy-tui.
///
/// Input is deliberately driven by the application's async crossterm stream;
/// sexy-tui's blocking `Terminal::start` is never called.
pub struct YggTerminal<W: Write = Stdout> {
    out: W,
    size: TerminalSize,
    last_was_cr: bool,
    pending: Vec<u8>,
    in_synchronized_frame: bool,
}

impl YggTerminal<Stdout> {
    /// Enter raw mode on the primary screen, returning the shared size cell.
    #[allow(dead_code)] // Used by the separately compiled Gate-0 spike target.
    pub fn enter() -> Result<(Self, TerminalSize)> {
        let size = Arc::new(Mutex::new(terminal::size().unwrap_or((80, 24))));
        let terminal = Self::enter_with_size(size.clone())?;
        Ok((terminal, size))
    }

    /// Enter using a caller-owned shared dimensions cell. This lets the shell
    /// update dimensions after resize while the terminal is boxed in the TUI.
    pub fn enter_with_size(size: TerminalSize) -> Result<Self> {
        Self::enter_with_mouse(size, false)
    }

    /// Enter the primary screen with optional SGR mouse reporting. Existing
    /// shell scrollback and the terminal's native selection remain available;
    /// Ygg virtualizes transcript history in its own semantic viewport.
    pub fn enter_with_mouse(size: TerminalSize, capture_mouse: bool) -> Result<Self> {
        terminal::enable_raw_mode()?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);

        let result = Self::enter_inner(size, capture_mouse);
        if result.is_err() {
            force_restore();
        }
        result
    }

    fn enter_inner(size: TerminalSize, capture_mouse: bool) -> Result<Self> {
        let mut out = std::io::stdout();
        execute!(
            out,
            event::EnableBracketedPaste,
            cursor::SetCursorStyle::SteadyBlock,
            cursor::Hide
        )?;
        if capture_mouse {
            execute!(out, event::EnableMouseCapture)?;
        }
        // Preserve ordinary text as terminal text. Modified keys still use
        // CSI-u, and REPORT_ALTERNATE_KEYS supplies their layout-resolved
        // character (for example `a:A` and `1:!`) to crossterm. This keeps
        // Ctrl+Enter distinct without asking terminals to report every typed
        // character as an ambiguous physical/base key.
        // Unsupported terminals safely ignore this request; Alt+Enter remains
        // the portable multiline fallback.
        if execute!(
            out,
            event::PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
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
}

impl<W: Write> YggTerminal<W> {
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let _ = self.out.write_all(&self.pending);
        self.pending.clear();
        let _ = self.out.flush();
    }

    fn flush_if_outside_frame(&mut self) {
        if !self.in_synchronized_frame {
            self.flush_pending();
        }
    }

    /// Clear operations erase using the terminal's current rendition. Reset it
    /// first: a stale background attribute must not turn an erased
    /// differential-render tail into a colored band.
    fn reset_rendition_before_clear(&mut self) {
        self.pending.extend_from_slice(b"\x1b[0m");
    }
}

impl<W: Write> Drop for YggTerminal<W> {
    fn drop(&mut self) {
        self.flush_pending();
        force_restore();
    }
}

impl<W: Write> sexy_tui_rs::Terminal for YggTerminal<W> {
    fn start_events(
        &mut self,
        _on_input: Box<dyn FnMut(sexy_tui_rs::TerminalInput)>,
        _on_resize: Box<dyn FnMut()>,
    ) {
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
        let result = if lines > 0 {
            queue!(self.pending, cursor::MoveDown(lines as u16))
        } else if lines < 0 {
            queue!(self.pending, cursor::MoveUp((-lines) as u16))
        } else {
            Ok(())
        };
        let _ = result;
        self.flush_if_outside_frame();
    }

    fn hide_cursor(&mut self) {
        let _ = queue!(self.pending, cursor::Hide);
        self.flush_if_outside_frame();
    }

    fn show_cursor(&mut self) {
        let _ = queue!(self.pending, cursor::Show);
        self.flush_if_outside_frame();
    }

    fn clear_line(&mut self) {
        self.reset_rendition_before_clear();
        let _ = queue!(
            self.pending,
            terminal::Clear(terminal::ClearType::CurrentLine)
        );
        self.flush_if_outside_frame();
    }

    fn clear_from_cursor(&mut self) {
        self.reset_rendition_before_clear();
        let _ = queue!(
            self.pending,
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
        self.flush_if_outside_frame();
    }

    fn clear_screen(&mut self) {
        self.reset_rendition_before_clear();
        let _ = queue!(self.pending, terminal::Clear(terminal::ClearType::All));
        self.flush_if_outside_frame();
    }

    fn capabilities(&self) -> SexyTerminalCapabilities {
        let dimensions = *self.size.lock().expect("terminal size mutex poisoned");
        sexy_terminal_capabilities(
            TerminalCapabilities::detect(ColorMode::Auto, false),
            dimensions,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct RecordingWriter {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        flushes: Arc<Mutex<usize>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes.lock().unwrap().push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let mut flushes = self.flushes.lock().unwrap();
            *flushes += 1;
            Ok(())
        }
    }

    // These tests intentionally mutate the process-global restoration flags.
    // Rust runs unit tests concurrently, so serialize that shared state.
    static TERMINAL_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn probe(term: Option<&str>) -> CapabilityProbe {
        CapabilityProbe {
            stdin_tty: true,
            stdout_tty: true,
            term: term.map(str::to_owned),
            term_program: None,
            colorterm: None,
            locale: Some("en_US.UTF-8".into()),
            no_color: false,
            explicit_plain: false,
        }
    }

    #[test]
    fn osc11_background_response_can_be_extracted_from_input_noise() {
        let color = parse_osc11_background_from_buffer("abc\x1b]11;rgb:12/34/56\x1b\\def")
            .expect("OSC 11 response should parse");
        assert_eq!((color.r, color.g, color.b), (18, 52, 86));

        let color = parse_osc11_background_from_buffer("\x1b]11;#f0f0f0\x07")
            .expect("BEL-terminated OSC 11 response should parse");
        assert_eq!((color.r, color.g, color.b), (240, 240, 240));
    }

    #[test]
    fn keyboard_enhancements_preserve_text_and_request_alternate_keys() {
        let flags = keyboard_enhancement_flags();
        assert!(flags.contains(event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }

    #[test]
    fn redirected_dumb_and_unknown_terminals_are_plain() {
        let mut redirected = probe(Some("xterm-256color"));
        redirected.stdout_tty = false;
        let caps = TerminalCapabilities::from_probe(ColorMode::Auto, redirected);
        assert!(!caps.interactive);
        assert_eq!(caps.color, ColorDepth::None);

        let dumb = TerminalCapabilities::from_probe(ColorMode::Always, probe(Some("dumb")));
        assert!(!dumb.interactive);
        assert!(!dumb.unicode);
        assert_eq!(dumb.color, ColorDepth::None);

        let unknown = TerminalCapabilities::from_probe(ColorMode::Auto, probe(None));
        assert!(!unknown.interactive);
        assert!(!unknown.unicode);
        assert_eq!(unknown.color, ColorDepth::None);

        for name in ["unknown", "mystery-terminal"] {
            let unknown = TerminalCapabilities::from_probe(ColorMode::Auto, probe(Some(name)));
            assert!(!unknown.interactive, "{name}");
            assert!(!unknown.unicode, "{name}");
            assert_eq!(unknown.color, ColorDepth::None, "{name}");
        }
    }

    #[test]
    fn apple_terminal_supports_italics_without_hyperlinks() {
        let mut apple = probe(Some("xterm-256color"));
        apple.term_program = Some("Apple_Terminal".into());
        let caps = TerminalCapabilities::from_probe(ColorMode::Auto, apple);
        assert!(caps.italics);
        assert!(!caps.hyperlinks);
    }

    #[test]
    fn capability_detection_degrades_truecolour_to_256_and_16() {
        let mut rich = probe(Some("xterm-256color"));
        rich.colorterm = Some("truecolor".into());
        assert_eq!(
            TerminalCapabilities::from_probe(ColorMode::Auto, rich).color,
            ColorDepth::TrueColor
        );
        assert_eq!(
            TerminalCapabilities::from_probe(ColorMode::Auto, probe(Some("screen-256color"))).color,
            ColorDepth::Ansi256
        );
        assert_eq!(
            TerminalCapabilities::from_probe(ColorMode::Auto, probe(Some("xterm"))).color,
            ColorDepth::Ansi16
        );
    }

    #[test]
    fn no_color_and_explicit_plain_override_forced_colour() {
        let mut no_color = probe(Some("xterm-256color"));
        no_color.no_color = true;
        assert_eq!(
            TerminalCapabilities::from_probe(ColorMode::Always, no_color).color,
            ColorDepth::None
        );
        let mut plain = probe(Some("xterm-256color"));
        plain.explicit_plain = true;
        let caps = TerminalCapabilities::from_probe(ColorMode::Always, plain);
        assert!(!caps.interactive);
        assert!(!caps.unicode);
        assert_eq!(caps.color, ColorDepth::None);
    }

    #[test]
    fn force_restore_is_idempotent_without_a_terminal() {
        let _guard = TERMINAL_STATE_TEST_LOCK.lock().unwrap();
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
    fn synchronized_frame_is_one_atomic_backend_write_even_without_csi_2026_support() {
        let writer = RecordingWriter::default();
        let writes = writer.writes.clone();
        let flushes = writer.flushes.clone();
        let mut terminal = YggTerminal {
            out: writer,
            size: Arc::new(Mutex::new((80, 24))),
            last_was_cr: false,
            pending: Vec::new(),
            in_synchronized_frame: false,
        };

        sexy_tui_rs::Terminal::write(&mut terminal, SYNC_OUTPUT_BEGIN);
        sexy_tui_rs::Terminal::write(&mut terminal, "\x1b[4;1H");
        sexy_tui_rs::Terminal::clear_from_cursor(&mut terminal);
        sexy_tui_rs::Terminal::write(&mut terminal, "replacement");
        assert!(writes.lock().unwrap().is_empty());
        assert_eq!(*flushes.lock().unwrap(), 0);

        sexy_tui_rs::Terminal::write(&mut terminal, SYNC_OUTPUT_END);
        let writes = writes.lock().unwrap();
        assert_eq!(
            writes.len(),
            1,
            "one frame must reach the backend atomically"
        );
        let output = String::from_utf8_lossy(&writes[0]);
        assert_eq!(
            output,
            format!("{SYNC_OUTPUT_BEGIN}\x1b[4;1H\x1b[0m\x1b[Jreplacement{SYNC_OUTPUT_END}")
        );
        assert_eq!(*flushes.lock().unwrap(), 1);
    }

    #[test]
    fn renderer_profile_uses_frame_delimiters_and_ygg_capabilities() {
        let renderer = sexy_terminal_capabilities(
            TerminalCapabilities::test(true, true, ColorDepth::Ansi256),
            (132, 47),
        );
        assert!(renderer.interactive);
        assert!(!renderer.plain);
        assert_eq!(renderer.color_depth, SexyColorDepth::Ansi256);
        assert!(renderer.cursor_addressing);
        assert!(renderer.line_clearing);
        assert!(renderer.synchronized_output);
        assert!(renderer.sync_output);
        assert_eq!(
            renderer.dimensions,
            Some(SexyTerminalSize {
                columns: 132,
                rows: 47,
            })
        );
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
        let _guard = TERMINAL_STATE_TEST_LOCK.lock().unwrap();
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
