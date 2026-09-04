#![cfg(unix)]

//! Deterministic PTY/frame regression coverage for primary-screen startup.
//!
//! The real binary is run against a disposable HOME, workspace, session store,
//! and inert local custom-provider record. No prompt is submitted, so the test
//! exercises terminal lifecycle only and never needs credentials or a network.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use sexy_tui_rs::{ColorDepth, Component, Terminal, TerminalCapabilities, TerminalInput, TUI};
use tempfile::TempDir;

const INITIAL_COLUMNS: u16 = 96;
const INITIAL_ROWS: u16 = 18;
const RESIZED_COLUMNS: u16 = 64;
const RESIZED_ROWS: u16 = 12;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DRAIN_TIME: Duration = Duration::from_millis(35);
const FRAME_BEGIN: &[u8] = b"\x1b[?2026h";
const FRAME_END: &[u8] = b"\x1b[?2026l";
const STALE_MARKER: &str = "YGG_PTY_STALE_STARTUP";
const READY_MARKER: &[u8] = b"custom/probe";
const PREEXISTING_STARTUP: &[u8] =
    include_bytes!("fixtures/startup-frame-pty/preexisting-startup.txt");
const INLINE_STARTUP: &str = include_str!("fixtures/startup-frame-pty/inline-startup.txt");
const INLINE_READY: &str = include_str!("fixtures/startup-frame-pty/inline-ready.txt");

fn pty_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseMode {
    Auto,
    App,
}

impl MouseMode {
    const fn as_arg(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::App => "app",
        }
    }
}

/// PTY master/slave pair with a nonblocking transcript of bytes seen by the
/// terminal. The slave stays open in the parent so terminal attributes can be
/// checked after the child exits.
struct Pty {
    master: File,
    slave: File,
    original_termios: libc::termios,
    output: Vec<u8>,
}

impl Pty {
    fn open(columns: u16, rows: u16) -> Self {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut dimensions = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let opened = unsafe {
            // macOS declares `winp` mutable while Linux declares it const.
            #[allow(clippy::unnecessary_mut_passed)]
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dimensions,
            )
        };
        assert_eq!(opened, 0, "openpty failed: {}", io::Error::last_os_error());
        set_close_on_exec(master_fd);
        set_close_on_exec(slave_fd);
        set_nonblocking(master_fd);

        let master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let original_termios = terminal_attributes(slave.as_raw_fd());
        Self {
            master,
            slave,
            original_termios,
            output: Vec::new(),
        }
    }

    fn seed_startup_rows(&mut self) {
        self.slave
            .write_all(PREEXISTING_STARTUP)
            .expect("write preexisting PTY rows");
        self.slave.flush().expect("flush preexisting PTY rows");
        self.wait_for_bytes(Duration::from_secs(1), b"YGG_PTY_STALE_STARTUP_B");
    }

    fn write_input(&mut self, input: &[u8]) {
        self.master.write_all(input).expect("write PTY input");
        self.master.flush().expect("flush PTY input");
    }

    fn set_size(&self, columns: u16, rows: u16) {
        let dimensions = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe {
            libc::ioctl(
                self.slave.as_raw_fd(),
                libc::TIOCSWINSZ,
                &dimensions as *const libc::winsize,
            )
        };
        assert_eq!(
            result,
            0,
            "TIOCSWINSZ failed: {}",
            io::Error::last_os_error()
        );
    }

    fn duplicate_slave(&self) -> File {
        let duplicated = unsafe { libc::dup(self.slave.as_raw_fd()) };
        assert!(
            duplicated >= 0,
            "dup PTY slave failed: {}",
            io::Error::last_os_error()
        );
        unsafe { File::from_raw_fd(duplicated) }
    }

    fn wait_for_bytes(&mut self, timeout: Duration, needle: &[u8]) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.read_available();
            if contains_bytes(&self.output, needle) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "PTY did not receive {needle:?}; transcript: {}",
            visible_bytes(&self.output)
        );
    }

    fn drain_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.read_available();
            thread::sleep(Duration::from_millis(2));
        }
        self.read_available();
    }

    fn read_available(&mut self) {
        let mut buffer = [0u8; 8192];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => self.output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                // PTY masters commonly report EIO after the last slave closes.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
                Err(error) => panic!("read PTY: {error}"),
            }
        }
    }
}

struct PtyYgg {
    child: Child,
    pty: Pty,
    _root: TempDir,
}

impl PtyYgg {
    fn spawn(binary: &Path, mode: MouseMode) -> Self {
        let root = tempfile::tempdir().expect("PTY test tempdir");
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let sessions = root.path().join("sessions");
        create_inert_environment(&home, &workspace, &sessions);

        let mut pty = Pty::open(INITIAL_COLUMNS, INITIAL_ROWS);
        // Rows exist before the child is exec'd, exactly as stale shell output
        // does at an interactive startup boundary.
        pty.seed_startup_rows();

        let stdin = duplicate_stdio(pty.slave.as_raw_fd());
        let stdout = duplicate_stdio(pty.slave.as_raw_fd());
        let stderr = duplicate_stdio(pty.slave.as_raw_fd());
        let tty_fd = pty.slave.as_raw_fd();
        let mut command = Command::new(binary);
        command
            .args([
                "--offline",
                "--no-context-files",
                "--no-tools",
                "--color",
                "never",
                "--mouse",
                mode.as_arg(),
                "--model",
                "custom/probe",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--session-dir")
            .arg(&sessions)
            .current_dir(&workspace)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("PWD", &workspace)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .env("YGG_COLOR_SCHEME", "dark")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);

        // `openpty` alone does not make the slave a controlling terminal. A
        // session/controlling TTY makes the resize path match a real shell.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(tty_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn ygg under PTY");
        Self {
            child,
            pty,
            _root: root,
        }
    }

    fn wait_until(&mut self, timeout: Duration, predicate: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pty.read_available();
            if predicate(&self.pty.output) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll ygg") {
                panic!(
                    "ygg exited before PTY condition ({status}); transcript: {}",
                    visible_bytes(&self.pty.output)
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "PTY condition timed out; transcript: {}",
            visible_bytes(&self.pty.output)
        );
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        self.pty.set_size(columns, rows);
        let signaled = unsafe { libc::kill(self.child.id() as i32, libc::SIGWINCH) };
        assert_eq!(
            signaled,
            0,
            "SIGWINCH failed: {}",
            io::Error::last_os_error()
        );
    }

    fn shutdown(mut self) -> ShutdownCapture {
        let shutdown_start = self.pty.output.len();
        self.pty.write_input(&[4]); // Ctrl-D
        let started = Instant::now();
        let status = loop {
            self.pty.read_available();
            if let Some(status) = self.child.try_wait().expect("poll Ctrl-D shutdown") {
                break status;
            }
            if started.elapsed() >= SHUTDOWN_TIMEOUT {
                unsafe {
                    let _ = libc::kill(self.child.id() as i32, libc::SIGKILL);
                }
                let _ = self.child.wait();
                panic!(
                    "ygg did not stop after Ctrl-D within {SHUTDOWN_TIMEOUT:?}; transcript: {}",
                    visible_bytes(&self.pty.output)
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        self.pty.drain_for(DRAIN_TIME);
        // A controlling-terminal session exit revokes the parent-held slave on
        // macOS (tcgetattr returns ENOTTY), while the PTY master continues to
        // expose the same terminal mode state on both macOS and Linux.
        let restored = terminal_modes_equal(
            &self.pty.original_termios,
            &terminal_attributes(self.pty.master.as_raw_fd()),
        );
        ShutdownCapture {
            output: std::mem::take(&mut self.pty.output),
            shutdown_start,
            status,
            termios_restored: restored,
        }
    }
}

impl Drop for PtyYgg {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe {
                let _ = libc::kill(self.child.id() as i32, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }
}

struct ShutdownCapture {
    output: Vec<u8>,
    shutdown_start: usize,
    status: ExitStatus,
    termios_restored: bool,
}

#[derive(Clone, Debug)]
struct PrimaryTrace {
    mode: MouseMode,
    startup_clear_screen: bool,
    first_frame_synchronized: bool,
    first_frame_stale_rows: bool,
    ready_frame_stale_rows: bool,
    resize_redraw_synchronized: bool,
    resize_clear_screen: bool,
    resize_clear_saved_lines: bool,
    resize_stale_rows: bool,
    shutdown_cursor_visible: bool,
    shutdown_bracketed_paste_disabled: bool,
    shutdown_termios_restored: bool,
    shutdown_cursor_position_valid: bool,
    shutdown_mouse_restored: bool,
    alternate_screen_used: bool,
    frames_balanced: bool,
    mouse_capture: bool,
    first_screen: String,
    ready_screen: String,
    resize_screen: String,
    shutdown_screen: String,
    output_len: usize,
}

impl PrimaryTrace {
    fn normalized(&self) -> String {
        format!(
            "mode={}\nstartup.clear_screen={}\nstartup.first_full_frame.synchronized={}\nstartup.first_full_frame.stale_rows={}\nstartup.ready_frame.stale_rows={}\nresize.redraw.synchronized={}\nresize.clear_screen={}\nresize.clear_saved_lines={}\nresize.stale_rows={}\nshutdown.cursor_visible={}\nshutdown.bracketed_paste_disabled={}\nshutdown.termios_restored={}\nshutdown.cursor_position_valid={}\nshutdown.mouse_restored={}\nalternate_screen.used={}\nframes.balanced={}\nmouse.capture={}\n",
            self.mode.as_arg(),
            self.startup_clear_screen,
            self.first_frame_synchronized,
            self.first_frame_stale_rows,
            self.ready_frame_stale_rows,
            self.resize_redraw_synchronized,
            self.resize_clear_screen,
            self.resize_clear_saved_lines,
            self.resize_stale_rows,
            self.shutdown_cursor_visible,
            self.shutdown_bracketed_paste_disabled,
            self.shutdown_termios_restored,
            self.shutdown_cursor_position_valid,
            self.shutdown_mouse_restored,
            self.alternate_screen_used,
            self.frames_balanced,
            self.mouse_capture,
        )
    }

    fn debug_report(&self) -> String {
        format!(
            "normalized:\n{}bytes={}\nfirst screen:\n{}\nready screen:\n{}\nresize screen:\n{}\nshutdown screen:\n{}",
            self.normalized(),
            self.output_len,
            self.first_screen,
            self.ready_screen,
            self.resize_screen,
            self.shutdown_screen,
        )
    }
}

fn run_primary(binary: &Path, mode: MouseMode) -> PrimaryTrace {
    let mut ygg = PtyYgg::spawn(binary, mode);
    ygg.wait_until(STARTUP_TIMEOUT, |output| nth_frame_end(output, 1).is_some());
    let first_end = nth_frame_end(&ygg.pty.output, 1).expect("first synchronized frame");
    ygg.wait_until(STARTUP_TIMEOUT, |output| {
        synchronized_frame_end_containing(output, READY_MARKER).is_some()
    });
    let ready_end = synchronized_frame_end_containing(&ygg.pty.output, READY_MARKER)
        .expect("ready synchronized frame");
    let raw = terminal_attributes(ygg.pty.slave.as_raw_fd());
    assert_eq!(
        raw.c_lflag & (libc::ICANON | libc::ECHO),
        0,
        "ygg did not enter raw terminal mode"
    );

    let mut parser = vt100::Parser::new(INITIAL_ROWS, INITIAL_COLUMNS, 512);
    parser.process(&ygg.pty.output[..first_end]);
    let first_screen = screen_text(&parser, INITIAL_COLUMNS);
    parser.process(&ygg.pty.output[first_end..ready_end]);
    let ready_screen = screen_text(&parser, INITIAL_COLUMNS);

    ygg.pty.drain_for(DRAIN_TIME);
    let resize_start = ygg.pty.output.len();
    ygg.resize(RESIZED_COLUMNS, RESIZED_ROWS);
    ygg.wait_until(STARTUP_TIMEOUT, |output| {
        output
            .get(resize_start..)
            .and_then(|bytes| synchronized_frame_end_containing(bytes, b"\x1b[2J"))
            .is_some()
    });
    let resize_end = resize_start
        + synchronized_frame_end_containing(&ygg.pty.output[resize_start..], b"\x1b[2J")
            .expect("resize redraw frame");
    let (resize_redraw_synchronized, resize_clear_screen, resize_clear_saved_lines) = {
        let resize_frame = &ygg.pty.output[resize_start..resize_end];
        parser.process(&ygg.pty.output[ready_end..resize_start]);
        parser.set_size(RESIZED_ROWS, RESIZED_COLUMNS);
        parser.process(resize_frame);
        (
            contains_bytes(resize_frame, FRAME_BEGIN) && contains_bytes(resize_frame, FRAME_END),
            contains_bytes(resize_frame, b"\x1b[2J"),
            contains_bytes(resize_frame, b"\x1b[3J"),
        )
    };
    let resize_screen = screen_text(&parser, RESIZED_COLUMNS);

    let shutdown = ygg.shutdown();
    assert_eq!(shutdown.status.code(), Some(0), "Ctrl-D exit status");
    parser.process(&shutdown.output[resize_end..shutdown.shutdown_start]);
    parser.process(&shutdown.output[shutdown.shutdown_start..]);
    let shutdown_screen = screen_text(&parser, RESIZED_COLUMNS);
    let restore = &shutdown.output[shutdown.shutdown_start..];
    let cursor = parser.screen().cursor_position();

    PrimaryTrace {
        mode,
        startup_clear_screen: contains_bytes(&shutdown.output[..first_end], b"\x1b[2J"),
        first_frame_synchronized: nth_frame_end(&shutdown.output, 1).is_some(),
        first_frame_stale_rows: first_screen.contains(STALE_MARKER),
        ready_frame_stale_rows: ready_screen.contains(STALE_MARKER),
        resize_redraw_synchronized,
        resize_clear_screen,
        resize_clear_saved_lines,
        resize_stale_rows: resize_screen.contains(STALE_MARKER),
        shutdown_cursor_visible: contains_bytes(restore, b"\x1b[?25h")
            && !parser.screen().hide_cursor(),
        shutdown_bracketed_paste_disabled: contains_bytes(restore, b"\x1b[?2004l")
            && !parser.screen().bracketed_paste(),
        shutdown_termios_restored: shutdown.termios_restored,
        shutdown_cursor_position_valid: cursor.0 < RESIZED_ROWS && cursor.1 < RESIZED_COLUMNS,
        shutdown_mouse_restored: contains_bytes(restore, b"\x1b[?1000l")
            && contains_bytes(restore, b"\x1b[?1006l"),
        alternate_screen_used: uses_alternate_screen(&shutdown.output)
            || parser.screen().alternate_screen(),
        frames_balanced: count_bytes(&shutdown.output, FRAME_BEGIN)
            == count_bytes(&shutdown.output, FRAME_END),
        mouse_capture: contains_bytes(&shutdown.output, b"\x1b[?1000h")
            && contains_bytes(&shutdown.output, b"\x1b[?1006h"),
        first_screen,
        ready_screen,
        resize_screen,
        shutdown_screen,
        output_len: shutdown.output.len(),
    }
}

/// A direct `sexy-tui-rs` PTY backend covers the explicit legacy inline
/// compatibility path. Ygg's real shell intentionally uses the primary-screen
/// Pi renderer, so this is the narrowest faithful way to keep inline behavior
/// in the same byte-level lane.
struct InlinePtyTerminal {
    writer: File,
    capabilities: TerminalCapabilities,
    columns: u16,
    rows: u16,
}

impl InlinePtyTerminal {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .expect("write inline PTY bytes");
        self.writer.flush().expect("flush inline PTY bytes");
    }
}

impl Terminal for InlinePtyTerminal {
    fn start_events(
        &mut self,
        _on_input: Box<dyn FnMut(TerminalInput)>,
        _on_resize: Box<dyn FnMut()>,
    ) {
    }

    fn stop(&mut self) {}

    fn write(&mut self, data: &str) {
        self.write_bytes(data.as_bytes());
    }

    fn columns(&self) -> u16 {
        self.columns
    }

    fn rows(&self) -> u16 {
        self.rows
    }

    fn move_by(&mut self, lines: i16) {
        match lines.cmp(&0) {
            std::cmp::Ordering::Less => {
                self.write_bytes(format!("\x1b[{}A", lines.unsigned_abs()).as_bytes())
            }
            std::cmp::Ordering::Greater => self.write_bytes(format!("\x1b[{lines}B").as_bytes()),
            std::cmp::Ordering::Equal => {}
        }
    }

    fn hide_cursor(&mut self) {
        self.write_bytes(b"\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.write_bytes(b"\x1b[?25h");
    }

    fn clear_line(&mut self) {
        self.write_bytes(b"\x1b[0m\x1b[2K");
    }

    fn clear_from_cursor(&mut self) {
        self.write_bytes(b"\x1b[0m\x1b[J");
    }

    fn clear_screen(&mut self) {
        self.write_bytes(b"\x1b[0m\x1b[2J");
    }

    fn capabilities(&self) -> TerminalCapabilities {
        self.capabilities
    }
}

struct MutableFixtureLines {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Component for MutableFixtureLines {
    fn render(&self, _width: u16) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn invalidate(&mut self) {}
}

#[derive(Debug)]
struct InlineTrace {
    startup_synchronized: bool,
    preexisting_rows_preserved: bool,
    shrink_stale_startup_rows: bool,
    shutdown_cursor_visible: bool,
    alternate_screen_used: bool,
    startup_screen: String,
    shrink_screen: String,
}

impl InlineTrace {
    fn normalized(&self) -> String {
        format!(
            "mode=legacy-inline\nstartup.synchronized={}\nstartup.preexisting_rows={}\nshrink.stale_startup_rows={}\nshutdown.cursor_visible={}\nalternate_screen.used={}\n",
            self.startup_synchronized,
            if self.preexisting_rows_preserved {
                "preserved"
            } else {
                "discarded"
            },
            self.shrink_stale_startup_rows,
            self.shutdown_cursor_visible,
            self.alternate_screen_used,
        )
    }

    fn debug_report(&self) -> String {
        format!(
            "normalized:\n{}startup screen:\n{}\nshrink screen:\n{}",
            self.normalized(),
            self.startup_screen,
            self.shrink_screen,
        )
    }
}

fn run_inline() -> InlineTrace {
    let mut pty = Pty::open(INITIAL_COLUMNS, INITIAL_ROWS);
    pty.seed_startup_rows();
    let lines = Arc::new(Mutex::new(fixture_lines(INLINE_STARTUP)));
    let mut capabilities = TerminalCapabilities::interactive(ColorDepth::None, true);
    capabilities.synchronized_output = true;
    capabilities.sync_output = true;
    capabilities.animation = false;
    let terminal = InlinePtyTerminal {
        writer: pty.duplicate_slave(),
        capabilities,
        columns: INITIAL_COLUMNS,
        rows: INITIAL_ROWS,
    };
    let mut tui = TUI::new(Box::new(terminal));
    tui.set_inline_scrollback(true);
    tui.add_child(Box::new(MutableFixtureLines {
        lines: lines.clone(),
    }));
    tui.start();
    pty.drain_for(DRAIN_TIME);
    let startup_end = pty.output.len();

    *lines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = fixture_lines(INLINE_READY);
    tui.request_render();
    pty.drain_for(DRAIN_TIME);
    let shrink_end = pty.output.len();
    tui.stop();
    pty.drain_for(DRAIN_TIME);

    let mut parser = vt100::Parser::new(INITIAL_ROWS, INITIAL_COLUMNS, 512);
    parser.process(&pty.output[..startup_end]);
    let startup_screen = screen_text(&parser, INITIAL_COLUMNS);
    parser.process(&pty.output[startup_end..shrink_end]);
    let shrink_screen = screen_text(&parser, INITIAL_COLUMNS);
    parser.process(&pty.output[shrink_end..]);
    let first_clear = find_subsequence(&pty.output[..startup_end], b"\x1b[2J", 0)
        .expect("inline first paint clears its viewport");
    let before_first_clear = &pty.output[..first_clear];

    InlineTrace {
        startup_synchronized: !frame_ranges(&pty.output[..startup_end]).is_empty(),
        // Inline first paint scrolls the old viewport into native history before
        // clearing the mutable viewport. It deliberately must not send ED 3.
        preexisting_rows_preserved: !contains_bytes(before_first_clear, b"\x1b[3J")
            && before_first_clear
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count()
                >= usize::from(INITIAL_ROWS),
        shrink_stale_startup_rows: shrink_screen.contains("YGG_PTY_INLINE_STARTUP"),
        shutdown_cursor_visible: contains_bytes(&pty.output[shrink_end..], b"\x1b[?25h")
            && !parser.screen().hide_cursor(),
        alternate_screen_used: uses_alternate_screen(&pty.output)
            || parser.screen().alternate_screen(),
        startup_screen,
        shrink_screen,
    }
}

#[test]
fn real_ygg_startup_frame_pty_contract() {
    let _guard = pty_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = Path::new(env!("CARGO_BIN_EXE_ygg"));
    let current_auto = run_primary(current, MouseMode::Auto);
    let current_app = run_primary(current, MouseMode::App);

    if let Some(baseline) = std::env::var_os("YGG_STARTUP_FRAME_BASELINE") {
        let baseline = PathBuf::from(baseline);
        let version = baseline_version(&baseline);
        let baseline_auto = run_primary(&baseline, MouseMode::Auto);
        let baseline_app = run_primary(&baseline, MouseMode::App);
        for (mode, current, baseline) in [
            ("auto", &current_auto, &baseline_auto),
            ("app", &current_app, &baseline_app),
        ] {
            assert_baseline_structural_compatibility(mode, current, baseline);
            eprintln!(
                "startup-frame-pty v0.6.7 comparison ({mode}, {version}):\n{}",
                normalized_delta(current, baseline)
            );
        }
    }

    assert_trace_fixture(
        "primary auto",
        &current_auto.normalized(),
        include_str!("fixtures/startup-frame-pty/primary-auto.trace"),
        &current_auto.debug_report(),
    );
    assert_trace_fixture(
        "primary app",
        &current_app.normalized(),
        include_str!("fixtures/startup-frame-pty/primary-app.trace"),
        &current_app.debug_report(),
    );
}

#[test]
fn legacy_inline_startup_frame_pty_contract() {
    let _guard = pty_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let trace = run_inline();
    assert_trace_fixture(
        "legacy inline",
        &trace.normalized(),
        include_str!("fixtures/startup-frame-pty/legacy-inline.trace"),
        &trace.debug_report(),
    );
}

fn create_inert_environment(home: &Path, workspace: &Path, sessions: &Path) {
    fs::create_dir_all(home.join(".ygg/credentials")).expect("credential directory");
    fs::create_dir_all(workspace).expect("workspace directory");
    fs::create_dir_all(sessions).expect("session directory");
    let credential = home.join(".ygg/credentials/custom.json");
    fs::write(
        &credential,
        r#"{"base_url":"http://127.0.0.1:9/v1/","api_key":"","api_name":"probe","headers":[],"models":[],"auto_discover":false}"#,
    )
    .expect("inert custom-provider fixture");
    let mut permissions = fs::metadata(&credential)
        .expect("credential fixture metadata")
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&credential, permissions).expect("credential fixture permissions");
}

fn fixture_lines(fixture: &str) -> Vec<String> {
    fixture.lines().map(str::to_owned).collect()
}

fn baseline_version(binary: &Path) -> String {
    assert!(
        binary.is_file(),
        "baseline is not a file: {}",
        binary.display()
    );
    let output = Command::new(binary)
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run explicit baseline --version");
    assert!(
        output.status.success(),
        "baseline --version failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(
        version.contains("0.6.7"),
        "YGG_STARTUP_FRAME_BASELINE must be a v0.6.7 binary, got {version:?}"
    );
    version
}

fn assert_baseline_structural_compatibility(
    mode: &str,
    current: &PrimaryTrace,
    baseline: &PrimaryTrace,
) {
    let differences = [
        (
            "first synchronized frame",
            current.first_frame_synchronized,
            baseline.first_frame_synchronized,
        ),
        (
            "resize synchronized redraw",
            current.resize_redraw_synchronized,
            baseline.resize_redraw_synchronized,
        ),
        (
            "resize screen clear",
            current.resize_clear_screen,
            baseline.resize_clear_screen,
        ),
        (
            "resize saved-line clear",
            current.resize_clear_saved_lines,
            baseline.resize_clear_saved_lines,
        ),
        (
            "shutdown cursor restoration",
            current.shutdown_cursor_visible,
            baseline.shutdown_cursor_visible,
        ),
        (
            "shutdown bracketed-paste restoration",
            current.shutdown_bracketed_paste_disabled,
            baseline.shutdown_bracketed_paste_disabled,
        ),
        (
            "shutdown terminal mode restoration",
            current.shutdown_termios_restored,
            baseline.shutdown_termios_restored,
        ),
        (
            "alternate-screen policy",
            current.alternate_screen_used,
            baseline.alternate_screen_used,
        ),
        (
            "mouse capture",
            current.mouse_capture,
            baseline.mouse_capture,
        ),
    ]
    .into_iter()
    .filter_map(|(name, current, baseline)| (current != baseline).then_some(name))
    .collect::<Vec<_>>();
    assert!(
        differences.is_empty(),
        "current changed v0.6.7 structural behavior in {mode}: {differences:?}\ncurrent:\n{}\nbaseline:\n{}",
        current.normalized(),
        baseline.normalized(),
    );
}

fn normalized_delta(current: &PrimaryTrace, baseline: &PrimaryTrace) -> String {
    let changes = current
        .normalized()
        .lines()
        .zip(baseline.normalized().lines())
        .filter(|(current, baseline)| current != baseline)
        .map(|(current, baseline)| format!("- {baseline}\n+ {current}"))
        .collect::<Vec<_>>();
    if changes.is_empty() {
        "no normalized delta".to_owned()
    } else {
        changes.join("\n")
    }
}

fn assert_trace_fixture(name: &str, actual: &str, expected: &str, diagnostics: &str) {
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "{name} PTY/frame contract changed\n{diagnostics}"
    );
}

fn frame_ranges(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(frame_start) = find_subsequence(bytes, FRAME_BEGIN, start) {
        let frame_body = frame_start + FRAME_BEGIN.len();
        let Some(frame_end_start) = find_subsequence(bytes, FRAME_END, frame_body) else {
            break;
        };
        let frame_end = frame_end_start + FRAME_END.len();
        ranges.push(frame_start..frame_end);
        start = frame_end;
    }
    ranges
}

fn nth_frame_end(bytes: &[u8], n: usize) -> Option<usize> {
    frame_ranges(bytes)
        .get(n.saturating_sub(1))
        .map(|range| range.end)
}

fn synchronized_frame_end_containing(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    frame_ranges(bytes)
        .into_iter()
        .find(|range| contains_bytes(&bytes[range.clone()], needle))
        .map(|range| range.end)
}

fn find_subsequence(bytes: &[u8], needle: &[u8], offset: usize) -> Option<usize> {
    (!needle.is_empty()).then_some(())?;
    bytes
        .get(offset..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| offset + position)
}

fn contains_bytes(bytes: &[u8], needle: &[u8]) -> bool {
    find_subsequence(bytes, needle, 0).is_some()
}

fn count_bytes(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn uses_alternate_screen(bytes: &[u8]) -> bool {
    [
        b"\x1b[?47h".as_slice(),
        b"\x1b[?47l",
        b"\x1b[?1047h",
        b"\x1b[?1047l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
    ]
    .iter()
    .any(|sequence| contains_bytes(bytes, sequence))
}

fn screen_text(parser: &vt100::Parser, columns: u16) -> String {
    parser
        .screen()
        .rows(0, columns)
        .map(|row| row.trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn duplicate_stdio(fd: RawFd) -> Stdio {
    let duplicated = unsafe { libc::dup(fd) };
    assert!(
        duplicated >= 0,
        "dup PTY slave failed: {}",
        io::Error::last_os_error()
    );
    let file = unsafe { File::from_raw_fd(duplicated) };
    Stdio::from(file)
}

fn set_close_on_exec(fd: RawFd) {
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    assert_eq!(
        result,
        0,
        "fcntl(FD_CLOEXEC) failed: {}",
        io::Error::last_os_error()
    );
}

fn set_nonblocking(fd: RawFd) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(
        flags >= 0,
        "fcntl(F_GETFL) failed: {}",
        io::Error::last_os_error()
    );
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(
        result,
        0,
        "fcntl(O_NONBLOCK) failed: {}",
        io::Error::last_os_error()
    );
}

fn terminal_attributes(fd: RawFd) -> libc::termios {
    let mut attributes = MaybeUninit::<libc::termios>::uninit();
    let result = unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) };
    assert_eq!(
        result,
        0,
        "tcgetattr failed: {}",
        io::Error::last_os_error()
    );
    unsafe { attributes.assume_init() }
}

fn terminal_modes_equal(before: &libc::termios, after: &libc::termios) -> bool {
    before.c_iflag == after.c_iflag
        && before.c_oflag == after.c_oflag
        && before.c_cflag == after.c_cflag
        && before.c_lflag == after.c_lflag
        && before.c_cc == after.c_cc
}

fn visible_bytes(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 4_096;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
    let escaped = text.escape_default().to_string();
    if bytes.len() > MAX_BYTES {
        format!("{escaped}… ({} bytes total)", bytes.len())
    } else {
        escaped
    }
}
