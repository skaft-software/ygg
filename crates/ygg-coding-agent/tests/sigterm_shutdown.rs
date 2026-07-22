#![cfg(unix)]

//! OS-boundary SIGTERM, process-tree, and terminal-restoration probes.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const EXIT_DEADLINE: Duration = Duration::from_secs(3);
const READY_DEADLINE: Duration = Duration::from_secs(15);
static PTY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct PtyYgg {
    child: Child,
    master: File,
    slave_probe: File,
    original_termios: libc::termios,
    output: Vec<u8>,
    terminal_control_expected: bool,
}

impl PtyYgg {
    fn spawn(root: &Path) -> Self {
        Self::spawn_with_args(root, &[])
    }

    fn spawn_with_args(root: &Path, extra_args: &[String]) -> Self {
        Self::spawn_with_mode(root, extra_args, true, true)
    }

    fn spawn_plain_with_args(root: &Path, extra_args: &[String]) -> Self {
        Self::spawn_with_mode(root, extra_args, false, true)
    }

    fn spawn_during_startup(root: &Path, extra_args: &[String]) -> Self {
        Self::spawn_with_mode(root, extra_args, true, false)
    }

    fn spawn_with_mode(
        root: &Path,
        extra_args: &[String],
        interactive: bool,
        wait_for_app: bool,
    ) -> Self {
        let home = root.join("home");
        let workspace = root.join("workspace");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(home.join(".ygg/credentials")).expect("credential directory");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let credential = home.join(".ygg/credentials/custom.json");
        std::fs::write(
            &credential,
            r#"{"base_url":"http://127.0.0.1:9/v1/","api_key":"","api_name":"probe","headers":[],"models":[],"auto_discover":false}"#,
        )
        .expect("credential");
        let mut credential_permissions = std::fs::metadata(&credential)
            .expect("credential metadata")
            .permissions();
        credential_permissions.set_mode(0o600);
        std::fs::set_permissions(&credential, credential_permissions)
            .expect("credential permissions");

        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut dimensions = libc::winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let opened = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dimensions,
            )
        };
        assert_eq!(
            opened,
            0,
            "openpty failed: {}",
            std::io::Error::last_os_error()
        );
        set_close_on_exec(master_fd);
        set_close_on_exec(slave_fd);
        set_nonblocking(master_fd);

        let master = unsafe { File::from_raw_fd(master_fd) };
        let slave_probe = unsafe { File::from_raw_fd(slave_fd) };
        let original_termios = terminal_attributes(slave_probe.as_raw_fd());
        let stdin = duplicate_stdio(slave_probe.as_raw_fd());
        let stdout = duplicate_stdio(slave_probe.as_raw_fd());
        let stderr = duplicate_stdio(slave_probe.as_raw_fd());
        let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        let mut command = Command::new(env!("CARGO_BIN_EXE_ygg"));
        command
            .args([
                "--offline",
                "--no-context-files",
                "--no-tools",
                "--allow-shell",
                "--mouse",
                "app",
                "--model",
                "custom/probe",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--session-dir")
            .arg(&sessions)
            .args(extra_args)
            .current_dir(&workspace)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", path)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("LANG", "C.UTF-8")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        let child = command.spawn().expect("spawn ygg");

        let mut process = Self {
            child,
            master,
            slave_probe,
            original_termios,
            output: Vec::new(),
            terminal_control_expected: interactive,
        };
        if interactive {
            process.wait_until(READY_DEADLINE, |output| {
                contains_bytes(output, b"\x1b[?2004h")
                    && contains_bytes(output, b"\x1b[?25l")
                    && (!wait_for_app || contains_bytes(output, b"custom/probe"))
            });
            let raw = terminal_attributes(process.slave_probe.as_raw_fd());
            assert_eq!(raw.c_lflag & (libc::ICANON | libc::ECHO), 0);
        } else {
            process.wait_until(READY_DEADLINE, |output| {
                contains_bytes(output, b"Workspace -")
            });
        }
        process
    }

    fn write_input(&mut self, input: &[u8]) {
        self.master.write_all(input).expect("write PTY input");
        self.master.flush().expect("flush PTY input");
    }

    fn submit_command(&mut self, command: &[u8]) {
        self.write_input(command);
        self.wait_until(READY_DEADLINE, |output| contains_bytes(output, command));
        // A bare slash command first accepts its autocomplete row; the second
        // Enter submits it. Commands whose popup already closed harmlessly
        // ignore the extra empty submission at the lifecycle boundary.
        self.write_input(b"\r\r");
    }

    fn wait_until(&mut self, timeout: Duration, predicate: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.read_available();
            if predicate(&self.output) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll ygg") {
                panic!(
                    "ygg exited before PTY condition ({status}); output: {}",
                    String::from_utf8_lossy(&self.output)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "PTY condition timed out; output: {}",
            String::from_utf8_lossy(&self.output)
        );
    }

    fn terminate(self) -> (ExitStatus, Duration, Vec<u8>) {
        let started = Instant::now();
        let signaled = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        assert_eq!(
            signaled,
            0,
            "SIGTERM failed: {}",
            std::io::Error::last_os_error()
        );
        self.wait_for_exit(started)
    }

    fn interrupt(mut self) -> (ExitStatus, Duration, Vec<u8>) {
        let started = Instant::now();
        self.write_input(&[3]);
        self.wait_for_exit(started)
    }

    fn wait_for_exit(mut self, started: Instant) -> (ExitStatus, Duration, Vec<u8>) {
        let status = loop {
            self.read_available();
            if let Some(status) = self.child.try_wait().expect("poll ygg shutdown") {
                break status;
            }
            if started.elapsed() >= EXIT_DEADLINE {
                unsafe {
                    let _ = libc::kill(self.child.id() as i32, libc::SIGKILL);
                }
                let _ = self.child.wait();
                panic!(
                    "ygg did not stop within {EXIT_DEADLINE:?}; output: {}",
                    String::from_utf8_lossy(&self.output)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        for _ in 0..5 {
            self.read_available();
            std::thread::sleep(Duration::from_millis(5));
        }
        let restored = terminal_attributes(self.slave_probe.as_raw_fd());
        let restored_mask = libc::ICANON | libc::ECHO;
        assert_eq!(
            restored.c_lflag & restored_mask,
            self.original_termios.c_lflag & restored_mask,
            "terminal canonical/echo flags were not restored"
        );
        if self.terminal_control_expected {
            assert_restoration_sequences(&self.output);
        }
        (status, started.elapsed(), std::mem::take(&mut self.output))
    }

    fn read_available(&mut self) {
        let mut buffer = [0u8; 8192];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => self.output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                // PTY masters commonly report EIO after the last slave closes.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
                Err(error) => panic!("read PTY: {error}"),
            }
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

#[test]
fn idle_interactive_sigterm_is_coordinated_and_restores_terminal() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let ygg = PtyYgg::spawn(directory.path());

    let (status, elapsed, output) = ygg.terminate();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(elapsed < EXIT_DEADLINE, "shutdown took {elapsed:?}");
    assert!(contains_bytes(&output, b"\x1b[?1003l"));
}

#[test]
fn sigterm_stops_running_shell_command_and_its_descendant() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let marker = directory.path().join("descendant.pid");
    let mut ygg = PtyYgg::spawn(directory.path());
    ygg.write_input(format!("!sleep 30 & echo $! > {}; wait\r", marker.display()).as_bytes());
    ygg.wait_until(READY_DEADLINE, |_| pid_marker_ready(&marker));
    let descendant = read_pid(&marker);
    assert!(process_exists(descendant), "shell descendant never started");

    let (status, elapsed, _) = ygg.terminate();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(elapsed < EXIT_DEADLINE, "shutdown took {elapsed:?}");
    let deadline = Instant::now() + Duration::from_millis(500);
    while process_exists(descendant) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_exists(descendant),
        "shell descendant {descendant} survived Ygg shutdown"
    );
}

#[test]
fn sigterm_stops_redirected_background_descendant_after_shell_leader_exits() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let marker = directory.path().join("redirected-descendant.pid");
    let mut ygg = PtyYgg::spawn(directory.path());
    ygg.write_input(
        format!(
            "!sleep 30 </dev/null >/dev/null 2>&1 & echo $! > {}\r",
            marker.display()
        )
        .as_bytes(),
    );
    ygg.wait_until(READY_DEADLINE, |_| pid_marker_ready(&marker));
    let descendant = read_pid(&marker);
    // Give the short-lived `sh -c` leader time to exit and transfer group
    // ownership to the centralized supervisor.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        process_exists(descendant),
        "redirected background descendant exited before shutdown"
    );

    let (status, elapsed, _) = ygg.terminate();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(elapsed < EXIT_DEADLINE, "shutdown took {elapsed:?}");
    let deadline = Instant::now() + Duration::from_millis(500);
    while process_exists(descendant) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survived = process_exists(descendant);
    if survived {
        unsafe {
            let _ = libc::kill(descendant, libc::SIGKILL);
        }
    }
    assert!(
        !survived,
        "redirected shell descendant {descendant} survived Ygg shutdown"
    );
}

#[test]
fn sigterm_delivers_graceful_shutdown_to_executable_extension() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let (args, started_marker, marker) = install_shutdown_probe(directory.path());
    let mut ygg = PtyYgg::spawn_with_args(directory.path(), &args);
    // The first terminal setup bytes precede `build_app`; wait for an
    // extension-owned marker so SIGTERM cannot race its initialize handshake.
    ygg.wait_until(READY_DEADLINE, |_| started_marker.exists());

    let (status, elapsed, output) = ygg.terminate();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(elapsed < EXIT_DEADLINE, "shutdown took {elapsed:?}");
    assert_graceful_marker(&marker, &output);
}

#[test]
fn idle_plain_tty_sigterm_gracefully_stops_extensions_without_the_watchdog() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let (mut args, started_marker, marker) = install_shutdown_probe(directory.path());
    args.push("--plain".to_owned());
    let mut ygg = PtyYgg::spawn_plain_with_args(directory.path(), &args);
    ygg.wait_until(READY_DEADLINE, |_| started_marker.exists());

    let (status, elapsed, output) = ygg.terminate();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(elapsed < EXIT_DEADLINE, "shutdown took {elapsed:?}");
    assert_graceful_marker(&marker, &output);
}

#[test]
fn sigterm_cancels_a_hung_extension_initialize_without_freezing_raw_terminal() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let (args, started_marker) = install_hung_initialize_probe(directory.path());
    let mut ygg = PtyYgg::spawn_during_startup(directory.path(), &args);
    ygg.wait_until(READY_DEADLINE, |_| started_marker.exists());

    let (status, elapsed, _) = ygg.terminate();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(elapsed < EXIT_DEADLINE, "shutdown took {elapsed:?}");
}

#[test]
fn ctrl_c_stops_a_hung_explicit_extension_reload_without_freezing_input() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let (args, reload_marker) = install_reload_probe(directory.path());
    let mut ygg = PtyYgg::spawn_with_args(directory.path(), &args);
    ygg.submit_command(b"/extensions reload");
    ygg.wait_until(READY_DEADLINE, |_| reload_marker.exists());

    let (status, elapsed, _) = ygg.interrupt();
    assert_eq!(status.code(), Some(128 + libc::SIGINT));
    assert!(elapsed < EXIT_DEADLINE, "Ctrl-C shutdown took {elapsed:?}");
}

#[test]
fn ctrl_c_stops_a_hung_resource_rebuild_without_freezing_input() {
    let _guard = PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().expect("tempdir");
    let (args, reload_marker) = install_reload_probe(directory.path());
    let mut ygg = PtyYgg::spawn_with_args(directory.path(), &args);
    ygg.submit_command(b"/reload");
    ygg.wait_until(READY_DEADLINE, |_| reload_marker.exists());

    let (status, elapsed, _) = ygg.interrupt();
    assert_eq!(status.code(), Some(128 + libc::SIGINT));
    assert!(elapsed < EXIT_DEADLINE, "Ctrl-C shutdown took {elapsed:?}");
}

fn install_shutdown_probe(root: &Path) -> (Vec<String>, PathBuf, PathBuf) {
    let extension_root = root.join("extensions");
    let extension = extension_root.join("shutdown-probe");
    std::fs::create_dir_all(&extension).expect("extension directory");
    std::fs::write(
        extension.join("extension.toml"),
        r#"
name = "shutdown-probe"
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "probe.sh"
"#,
    )
    .expect("extension manifest");
    let script = extension.join("probe.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' started > "$YGG_WORKSPACE/extension-started.marker"
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[]}}'
IFS= read -r shutdown
printf '%s\n' graceful > "$YGG_WORKSPACE/extension-shutdown.marker"
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
"#,
    )
    .expect("extension script");
    let mut permissions = std::fs::metadata(&script)
        .expect("extension script metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).expect("extension script permissions");
    let args = vec![
        "--extension-dir".to_owned(),
        extension_root.display().to_string(),
        "--enable-extension".to_owned(),
        "shutdown-probe".to_owned(),
        "--trust-extension".to_owned(),
        "shutdown-probe".to_owned(),
    ];
    let started_marker = root.join("workspace/extension-started.marker");
    let marker = root.join("workspace/extension-shutdown.marker");
    (args, started_marker, marker)
}

fn install_hung_initialize_probe(root: &Path) -> (Vec<String>, PathBuf) {
    let extension_root = root.join("extensions");
    let extension = extension_root.join("hung-initialize");
    std::fs::create_dir_all(&extension).expect("extension directory");
    std::fs::write(
        extension.join("extension.toml"),
        r#"
name = "hung-initialize"
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "probe.sh"
"#,
    )
    .expect("extension manifest");
    let script = extension.join("probe.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' started > "$YGG_WORKSPACE/extension-started.marker"
IFS= read -r initialize
while :; do sleep 1; done
"#,
    )
    .expect("extension script");
    make_executable(&script);
    (
        extension_args(&extension_root, "hung-initialize"),
        root.join("workspace/extension-started.marker"),
    )
}

fn install_reload_probe(root: &Path) -> (Vec<String>, PathBuf) {
    let extension_root = root.join("extensions");
    let extension = extension_root.join("reload-probe");
    std::fs::create_dir_all(&extension).expect("extension directory");
    std::fs::write(
        extension.join("extension.toml"),
        r#"
name = "reload-probe"
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "probe.sh"
"#,
    )
    .expect("extension manifest");
    let script = extension.join("probe.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
count_file="$YGG_WORKSPACE/reload-count"
count=0
if [ -f "$count_file" ]; then IFS= read -r count < "$count_file"; fi
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
IFS= read -r initialize
if [ "$count" -gt 1 ]; then
  printf '%s\n' started > "$YGG_WORKSPACE/reload-started.marker"
  while :; do sleep 1; done
fi
id=$(printf '%s\n' "$initialize" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[]}}\n' "$id"
while IFS= read -r request; do
  case "$request" in
    *'"method":"shutdown"'*)
      id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      exit 0
      ;;
  esac
done
"#,
    )
    .expect("extension script");
    make_executable(&script);
    (
        extension_args(&extension_root, "reload-probe"),
        root.join("workspace/reload-started.marker"),
    )
}

fn extension_args(extension_root: &Path, name: &str) -> Vec<String> {
    vec![
        "--extension-dir".to_owned(),
        extension_root.display().to_string(),
        "--enable-extension".to_owned(),
        name.to_owned(),
        "--trust-extension".to_owned(),
        name.to_owned(),
    ]
}

fn make_executable(path: &Path) {
    let mut permissions = std::fs::metadata(path)
        .expect("extension script metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).expect("extension script permissions");
}

fn assert_graceful_marker(marker: &Path, output: &[u8]) {
    assert_eq!(
        std::fs::read_to_string(marker).unwrap_or_else(|error| panic!(
            "graceful extension marker {}: {error}; PTY output: {}",
            marker.display(),
            String::from_utf8_lossy(output)
        )),
        "graceful\n"
    );
}

fn duplicate_stdio(fd: RawFd) -> Stdio {
    let duplicated = unsafe { libc::dup(fd) };
    assert!(
        duplicated >= 0,
        "dup failed: {}",
        std::io::Error::last_os_error()
    );
    unsafe { Stdio::from_raw_fd(duplicated) }
}

fn set_close_on_exec(fd: RawFd) {
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    assert_eq!(result, 0, "fcntl(FD_CLOEXEC) failed");
}

fn set_nonblocking(fd: RawFd) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0, "fcntl(F_GETFL) failed");
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(result, 0, "fcntl(O_NONBLOCK) failed");
}

fn terminal_attributes(fd: RawFd) -> libc::termios {
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    let result = unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) };
    assert_eq!(
        result,
        0,
        "tcgetattr failed: {}",
        std::io::Error::last_os_error()
    );
    unsafe { attributes.assume_init() }
}

fn assert_restoration_sequences(output: &[u8]) {
    assert!(
        contains_bytes(output, b"\x1b[?2004l"),
        "bracketed paste was not disabled"
    );
    assert!(
        contains_bytes(output, b"\x1b[?1000l"),
        "mouse capture was not disabled"
    );
    assert!(
        contains_bytes(output, b"\x1b[?25h"),
        "cursor was not restored"
    );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn read_pid(path: &PathBuf) -> i32 {
    std::fs::read_to_string(path)
        .expect("descendant pid marker")
        .trim()
        .parse()
        .expect("descendant pid")
}

fn pid_marker_ready(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
        .is_some()
}

fn process_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
