//! Single-shot command execution with timeout, bounded capture, and child
//! process-tree cleanup.
//!
//! Ygg internally decides between direct process spawning and `/bin/sh -c`
//! based on command syntax. Both transports require the same command-execution
//! authority because any arbitrary executable can itself be an interpreter.

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
const POST_KILL_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// Leave room for exit/capture metadata inside the per-tool result cap.
#[cfg(unix)]
const CAPTURE_ENVELOPE_RESERVE: usize = 256;
use bytes::Bytes;
use serde::Deserialize;
#[cfg(unix)]
use tokio::io::unix::AsyncFd;
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt};
use ygg_ai::ToolDef;

#[cfg(unix)]
use crate::extension_process::ProcessGroupGuard;
use crate::tool::{OutputStream, Tool, ToolContext, ToolError, ToolOutput, ToolProgressSink};
#[cfg(unix)]
use crate::tools::parse_args;

/// Flat execution request. Ygg chooses process vs shell internally.
#[derive(Deserialize)]
struct ExecArgs {
    /// The command line to run. Simple commands are spawned directly;
    /// commands containing shell operators (pipes, redirections, etc.)
    /// are routed through `/bin/sh -c`.
    command: String,
    /// Optional working directory. Relative paths use the workspace;
    /// trusted-local hosts also accept absolute and `~/` paths.
    cwd: Option<String>,
    /// Optional timeout in milliseconds.
    timeout_ms: Option<u64>,
}

/// The built-in `exec` tool.
///
/// Executes a command with bounded stdout/stderr capture and a timeout.
/// Simple commands (no shell operators) are spawned directly; commands
/// containing pipes, redirections, substitutions, or other shell syntax are
/// routed through `/bin/sh -c`. Both require unified command authority.
/// The child's entire process group is killed on timeout or cancellation.
///
/// **Unix-only in v0.1.** Process-tree cleanup requires unix process groups.
pub struct ExecTool;

#[async_trait::async_trait]
impl Tool for ExecTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "exec".to_string(),
            description: "Run a shell command. Simple commands are spawned directly; \
                          commands with pipes, redirections, or other shell operators \
                          use /bin/sh -c when permitted. Omit cwd to run at the \
                          workspace root. Output reports the exit status and bounded \
                          stdout/stderr. Complete streams end with complete_<stream>=true; \
                          truncated_<stream>=... means bytes were omitted."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command line to execute."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory relative to the workspace (default: workspace root)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional timeout in milliseconds."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        #[cfg(not(unix))]
        {
            let _ = (args, ctx);
            Err(ToolError::new(
                "error unsupported_platform\nexec is unavailable on this platform in v0.1: \
                 cancellation cleanup requires unix process groups",
            ))
        }
        #[cfg(unix)]
        {
            self.execute_unix(args, ctx).await
        }
    }
}

#[cfg(unix)]
impl ExecTool {
    async fn execute_unix(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let args: ExecArgs = parse_args(args)?;
        if args.command.is_empty() {
            return Err(ToolError::new(
                "error invalid_arguments\ncommand must be non-empty",
            ));
        }

        if !(ctx.sandbox.allow_process && ctx.sandbox.allow_shell) {
            return Err(ToolError::new(
                "error not_permitted\ncommand execution is disabled by sandbox policy; \
                 arbitrary process execution has shell-equivalent authority and requires \
                 both allow_process=true and allow_shell=true",
            ));
        }

        let needs_shell = shell_command_has_operators(&args.command);
        let interactive_sudo = !needs_shell && direct_sudo_needs_terminal(&args.command);
        let (mut command, cwd) = if needs_shell {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c").arg(&args.command);
            (c, args.cwd.as_ref())
        } else {
            let (program, argv) = shell_word_parse(&args.command);
            let mut c = tokio::process::Command::new(&program);
            c.args(&argv);
            (c, args.cwd.as_ref())
        };

        // Honour the per-call timeout when present, bounded by sandbox max.
        let effective_timeout = match args.timeout_ms {
            Some(ms) => Duration::from_millis(ms).min(ctx.sandbox.exec_timeout),
            None => ctx.sandbox.exec_timeout,
        };

        let workdir: PathBuf = match cwd {
            None => ctx.workspace.to_path_buf(),
            Some(rel) => {
                let display_path = ctx.display_path(rel);
                let dir = ctx.resolve_existing(rel)?;
                if !dir.is_dir() {
                    return Err(ToolError::new(format!(
                        "error invalid_cwd\n{display_path}: not a directory"
                    )));
                }
                dir
            }
        };

        if interactive_sudo {
            return execute_interactive_pty(command, &workdir, effective_timeout, ctx).await;
        }

        command
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Put the child in its own process group so cancellation and timeouts
        // can terminate the whole tree, not just the direct child.
        #[cfg(unix)]
        command.process_group(0);

        let start = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|e| ToolError::new(format!("error spawn\nfailed to start command: {e}")))?;
        let guard = ProcessGroupGuard::exec(child.id());

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        // Capture each stream up to the complete shared allowance first. Once
        // both byte counts are known, rebalance the retained bytes so an empty
        // or short peer cannot strand half of the advertised result budget.
        let capture_budget = ctx
            .sandbox
            .max_output_bytes
            .saturating_sub(CAPTURE_ENVELOPE_RESERVE);
        let stdout_progress = ctx.progress.clone();
        let stderr_progress = ctx.progress.clone();

        let work = async {
            let (out, err, status) = tokio::join!(
                read_bounded_with_progress(
                    &mut stdout_pipe,
                    capture_budget,
                    &stdout_progress,
                    OutputStream::Stdout
                ),
                read_bounded_with_progress(
                    &mut stderr_pipe,
                    capture_budget,
                    &stderr_progress,
                    OutputStream::Stderr
                ),
                child.wait(),
            );
            (out, err, status)
        };
        tokio::pin!(work);

        match tokio::time::timeout(effective_timeout, &mut work).await {
            Err(_elapsed) => {
                guard.terminate_now();
                // Preserve final output when ordinary descendants close the
                // pipes promptly, but never let an escaped descendant retain a
                // capture descriptor and defeat the execution deadline. When
                // this bounded drain expires, returning drops `work`, the pipe
                // readers, and the kill-on-drop child handle immediately.
                let drained = tokio::time::timeout(POST_KILL_DRAIN_TIMEOUT, &mut work).await;
                let mut message = format!(
                    "error timeout\ncommand exceeded the {:.0}s execution limit and was killed",
                    effective_timeout.as_secs_f64()
                );
                match drained {
                    Ok((mut out, mut err, status)) => {
                        rebalance_captures(&mut out, &mut err, capture_budget);
                        guard.disarm();
                        if out.total_bytes > 0 {
                            message.push('\n');
                            message.push_str(&out.render("stdout"));
                        }
                        if err.total_bytes > 0 {
                            message.push('\n');
                            message.push_str(&err.render("stderr"));
                        }
                        if let Ok(status) = status {
                            use std::os::unix::process::ExitStatusExt;
                            let exit = match (status.code(), status.signal()) {
                                (Some(code), _) => format!("exit={code}"),
                                (None, Some(sig)) => format!("exit=signal:{sig}"),
                                (None, None) => "exit=unknown".to_string(),
                            };
                            message.push_str(&format!("\n{exit}"));
                        }
                    }
                    Err(_) => message.push_str(
                        "\noutput drain abandoned after escaped descendants kept capture pipes open",
                    ),
                }
                Err(ToolError::new(message))
            }
            Ok((mut out, mut err, status)) => {
                rebalance_captures(&mut out, &mut err, capture_budget);
                let status = status.map_err(|e| {
                    ToolError::new(format!("error io\nfailed to wait for command: {e}"))
                })?;
                // The direct child and capture pipes are finished, but a
                // background descendant may have deliberately redirected both
                // streams and remained in the same process group. Transfer the
                // group to the centralized reaper for the rest of this tool's
                // original deadline instead of silently unregistering it.
                guard.supervise_exec_descendants(
                    effective_timeout.saturating_sub(start.elapsed()),
                    ctx.cancellation.clone(),
                );
                let duration = start.elapsed();

                let exit = {
                    use std::os::unix::process::ExitStatusExt;
                    match (status.code(), status.signal()) {
                        (Some(code), _) => format!("exit={code}"),
                        (None, Some(sig)) => format!("exit=signal:{sig}"),
                        (None, None) => "exit=unknown".to_string(),
                    }
                };

                let mut text = format!("{exit} duration={:.2}s", duration.as_secs_f64());
                if out.total_bytes == 0 && err.total_bytes == 0 {
                    text.push_str("\n(no output)");
                } else {
                    if out.total_bytes > 0 {
                        text.push('\n');
                        text.push_str(&out.render("stdout"));
                    }
                    if err.total_bytes > 0 {
                        text.push('\n');
                        text.push_str(&err.render("stderr"));
                    }
                }
                if status.success() {
                    Ok(ToolOutput::new(text))
                } else {
                    Err(ToolError::new(format!("error nonzero_exit\n{text}")))
                }
            }
        }
    }
}

/// Direct sudo owns a private PTY unless the caller explicitly selected a
/// non-interactive or stdin/askpass transport. This prevents sudo from opening
/// Ygg's controlling terminal and racing the composer's event reader.
#[cfg(unix)]
fn direct_sudo_needs_terminal(command: &str) -> bool {
    let (program, arguments) = shell_word_parse(command);
    let is_sudo = std::path::Path::new(&program)
        .file_name()
        .is_some_and(|name| name == "sudo");
    is_sudo
        && !arguments.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "-n" | "--non-interactive" | "-S" | "--stdin" | "-A" | "--askpass"
            )
        })
}

#[cfg(unix)]
async fn execute_interactive_pty(
    mut command: tokio::process::Command,
    workdir: &std::path::Path,
    timeout: Duration,
    ctx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolError> {
    let (master_fd, slave_fd) = open_private_pty()?;
    // Own both descriptors immediately so every clone/spawn/setup error closes
    // the complete private terminal rather than leaking its raw master fd.
    let master = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
    let slave_fd_for_child = slave.as_raw_fd();
    command
        .current_dir(workdir)
        .stdin(Stdio::from(slave.try_clone().map_err(pty_error)?))
        .stdout(Stdio::from(slave.try_clone().map_err(pty_error)?))
        .stderr(Stdio::from(slave.try_clone().map_err(pty_error)?))
        .kill_on_drop(true);
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd_for_child, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::new(format!("error pty\nfailed to start command: {error}")))?;
    let guard = ProcessGroupGuard::exec(child.id());
    drop(slave);
    set_nonblocking(master.as_raw_fd())?;
    let master =
        AsyncFd::new(master).map_err(|error| ToolError::new(format!("error pty\n{error}")))?;

    let capture_budget = ctx
        .sandbox
        .max_output_bytes
        .saturating_sub(CAPTURE_ENVELOPE_RESERVE);
    let mut capture = Capture::empty();
    let mut was_echo_enabled = true;
    let mut request_issued = false;
    let mut input_request = None;
    let mut pty_open = true;
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let status = loop {
        tokio::select! {
            biased;
            _ = &mut deadline => {
                guard.terminate_now();
                let _ = tokio::time::timeout(POST_KILL_DRAIN_TIMEOUT, child.wait()).await;
                return Err(ToolError::new(format!(
                    "error timeout\ncommand exceeded the {:.0}s execution limit and was killed",
                    timeout.as_secs_f64()
                )));
            }
            response = futures_util::future::OptionFuture::from(
                input_request.as_mut().map(|request: &mut std::pin::Pin<Box<dyn std::future::Future<Output = Option<crate::tool::ToolInputResponse>> + Send>>| request.as_mut())
            ), if input_request.is_some() => {
                input_request = None;
                let Some(response) = response.flatten() else {
                    guard.terminate_now();
                    let _ = tokio::time::timeout(POST_KILL_DRAIN_TIMEOUT, child.wait()).await;
                    return Err(ToolError::new("error input_cancelled\ninteractive command input was cancelled"));
                };
                pty_write_all(&master, response.as_bytes()).await?;
                pty_write_all(&master, b"\n").await?;
            }
            ready = master.readable(), if pty_open => {
                let mut ready = ready.map_err(pty_error)?;
                match ready.try_io(|inner| {
                    let mut bytes = [0u8; 8192];
                    inner.get_ref().read(&mut bytes).map(|count| bytes[..count].to_vec())
                }) {
                    Ok(Ok(bytes)) if !bytes.is_empty() => {
                        capture.push_bytes(&bytes, capture_budget);
                        ctx.progress.output(OutputStream::Stdout, Bytes::from(bytes));
                        let echo_enabled = pty_echo_enabled(master.get_ref().as_raw_fd())?;
                        if was_echo_enabled && !echo_enabled && !request_issued {
                            request_issued = true;
                            let progress = ctx.progress.clone();
                            input_request = Some(Box::pin(async move {
                                progress.input("Password:".to_owned(), true).await
                            }));
                        } else if echo_enabled {
                            request_issued = false;
                        }
                        was_echo_enabled = echo_enabled;
                    }
                    Ok(Ok(_)) => pty_open = false,
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => {
                        pty_open = false;
                    }
                    Ok(Err(error)) => return Err(pty_error(error)),
                    Err(_) => {}
                }
            }
            _ = poll.tick() => {
                if let Some(status) = child.try_wait().map_err(pty_error)? {
                    break status;
                }
            }
        }
    };

    // Capture bytes already available when the child closed its slave.
    drain_pty(&master, &mut capture, capture_budget, &ctx.progress)?;
    guard.disarm();
    capture.fit_to_budget(capture_budget);
    let duration = start.elapsed();
    let exit = match status.code() {
        Some(code) => format!("exit={code}"),
        None => "exit=signal".to_owned(),
    };
    let mut text = format!("{exit} duration={:.2}s", duration.as_secs_f64());
    if capture.total_bytes == 0 {
        text.push_str("\n(no output)");
    } else {
        text.push('\n');
        text.push_str(&capture.render("terminal"));
    }
    if status.success() {
        Ok(ToolOutput::new(text))
    } else {
        Err(ToolError::new(format!("error nonzero_exit\n{text}")))
    }
}

#[cfg(unix)]
fn pty_error(error: std::io::Error) -> ToolError {
    ToolError::new(format!("error pty\n{error}"))
}

#[cfg(unix)]
fn open_private_pty() -> Result<(libc::c_int, libc::c_int), ToolError> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } == -1
    {
        return Err(pty_error(std::io::Error::last_os_error()));
    }
    Ok((master, slave))
}

#[cfg(unix)]
fn set_nonblocking(fd: libc::c_int) -> Result<(), ToolError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(pty_error(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(unix)]
fn pty_echo_enabled(fd: libc::c_int) -> Result<bool, ToolError> {
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) } == -1 {
        return Err(pty_error(std::io::Error::last_os_error()));
    }
    let attributes = unsafe { attributes.assume_init() };
    Ok(attributes.c_lflag & libc::ECHO != 0)
}

#[cfg(unix)]
async fn pty_write_all(master: &AsyncFd<std::fs::File>, bytes: &[u8]) -> Result<(), ToolError> {
    let mut written = 0;
    while written < bytes.len() {
        let mut ready = master.writable().await.map_err(pty_error)?;
        match ready.try_io(|inner| inner.get_ref().write(&bytes[written..])) {
            Ok(Ok(0)) => return Err(ToolError::new("error pty\nmaster closed")),
            Ok(Ok(count)) => written += count,
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(Err(error)) => return Err(pty_error(error)),
            Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(unix)]
fn drain_pty(
    master: &AsyncFd<std::fs::File>,
    capture: &mut Capture,
    budget: usize,
    progress: &ToolProgressSink,
) -> Result<(), ToolError> {
    loop {
        let mut bytes = [0u8; 8192];
        match master.get_ref().read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                capture.push_bytes(&bytes[..count], budget);
                progress.output(
                    OutputStream::Stdout,
                    Bytes::copy_from_slice(&bytes[..count]),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
            Err(error) => return Err(pty_error(error)),
        }
    }
}

/// Returns true when `command` contains shell operators that require
/// `/bin/sh -c` instead of direct process spawning.
#[cfg(unix)]
fn shell_command_has_operators(command: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;

    for ch in command.chars() {
        if prev_backslash {
            prev_backslash = false;
            continue;
        }
        if ch == '\\' {
            prev_backslash = true;
            continue;
        }
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '|' | '>' | '<' | '&' | ';' | '$' | '`' if !in_single && !in_double => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Parses a simple command line (no shell operators) into a program and
/// argument vector using POSIX shell word-splitting rules (quoting, but
/// no expansions, substitutions, or globs).
///
/// Returns `(program, [arg, ...])`. Quoted strings are unquoted; bare
/// words are kept verbatim. Does not expand `$VAR`, `*`, or backticks.
#[cfg(unix)]
fn shell_word_parse(command: &str) -> (String, Vec<String>) {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;

    for ch in command.chars() {
        // Backslash inside double quotes only escapes `"`, `\\`, `$`, `` ` ``.
        if prev_backslash {
            prev_backslash = false;
            match ch {
                '"' | '\\' | '$' | '`' if in_double => current.push(ch),
                _ if in_double => {
                    current.push('\\');
                    current.push(ch);
                }
                _ => current.push(ch),
            }
            continue;
        }

        if ch == '\\' && !in_single {
            prev_backslash = true;
            continue;
        }

        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }

    let program = words.first().cloned().unwrap_or_default();
    let argv = if words.len() > 1 {
        words[1..].to_vec()
    } else {
        Vec::new()
    };
    (program, argv)
}

/// No persistent terminal sessions are exposed by the v0.1 tool schema.
#[cfg(unix)]
pub(crate) fn cleanup_pty_scope(_execution_scope: &str) {}

/// Byte-bounded stream capture keeping the head and tail halves of the budget.
#[cfg(unix)]
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total_bytes: usize,
    truncated: bool,
}

#[cfg(unix)]
impl Capture {
    fn empty() -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            total_bytes: 0,
            truncated: false,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8], budget: usize) {
        let head_cap = budget / 2;
        let tail_cap = budget.saturating_sub(head_cap);
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        let mut remaining = bytes;
        if self.head.len() < head_cap {
            let take = remaining.len().min(head_cap - self.head.len());
            self.head.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
        }
        if !remaining.is_empty() && tail_cap > 0 {
            self.tail.extend_from_slice(remaining);
            if self.tail.len() > tail_cap {
                self.tail.drain(..self.tail.len() - tail_cap);
            }
        }
        self.truncated = self.total_bytes > budget;
    }

    /// Finalize a capture recorded with an equal-or-larger provisional
    /// allowance. If every byte fits, restore the original stream exactly;
    /// otherwise retain balanced head/tail evidence within `budget`.
    fn fit_to_budget(&mut self, budget: usize) {
        if self.total_bytes <= budget {
            self.head.append(&mut self.tail);
            self.truncated = false;
            return;
        }

        let head_cap = budget / 2;
        let tail_cap = budget.saturating_sub(head_cap);
        self.head.truncate(head_cap);
        if self.tail.len() > tail_cap {
            self.tail.drain(..self.tail.len() - tail_cap);
        }
        self.truncated = true;
    }

    /// Renders one output section:
    ///
    /// ```text
    /// stdout: 12 lines
    /// <lines>
    /// complete_stdout=true
    /// ```
    ///
    /// or, when the byte budget was exceeded:
    ///
    /// ```text
    /// stdout: 5210240 bytes, showing first N and last M lines
    /// <head lines>
    /// ...
    /// <tail lines>
    /// truncated_stdout=head:N tail:M omitted_bytes:K
    /// ```
    fn render(&self, name: &str) -> String {
        if !self.truncated {
            let text = String::from_utf8_lossy(&self.head);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let lines = if text.is_empty() {
                0
            } else {
                text.lines().count()
            };
            format!("{name}: {lines} lines\n{text}\ncomplete_{name}=true")
        } else {
            let head = String::from_utf8_lossy(&self.head);
            let tail = String::from_utf8_lossy(&self.tail);
            // Drop the partial line at each cut so the output stays line-oriented.
            let head = head.rsplit_once('\n').map(|(kept, _)| kept).unwrap_or("");
            let tail = tail.split_once('\n').map(|(_, kept)| kept).unwrap_or("");
            let tail = tail.strip_suffix('\n').unwrap_or(tail);
            let head_lines = if head.is_empty() {
                0
            } else {
                head.lines().count()
            };
            let tail_lines = if tail.is_empty() {
                0
            } else {
                tail.lines().count()
            };
            let omitted = self.total_bytes - self.head.len() - self.tail.len();
            format!(
                "{name}: {} bytes, showing first {head_lines} and last {tail_lines} lines\n\
                 {head}\n...\n{tail}\n\
                 truncated_{name}=head:{head_lines} tail:{tail_lines} omitted_bytes:{omitted}",
                self.total_bytes
            )
        }
    }
}

/// Reads a pipe to EOF keeping at most `budget` bytes: the first half of the
/// budget verbatim plus a rolling tail of the second half. Forwards every
/// chunk to `progress` so the consumer sees live output.
#[cfg(unix)]
async fn read_bounded_with_progress<R: AsyncRead + Unpin>(
    reader: &mut Option<R>,
    budget: usize,
    progress: &ToolProgressSink,
    stream: OutputStream,
) -> Capture {
    let Some(reader) = reader.as_mut() else {
        return Capture::empty();
    };
    let head_cap = budget / 2;
    let tail_cap = budget.saturating_sub(head_cap);

    let mut capture = Capture::empty();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                progress.output(stream, Bytes::copy_from_slice(&buf[..n]));
                capture.total_bytes += n;
                let mut chunk = &buf[..n];
                if capture.head.len() < head_cap {
                    let take = chunk.len().min(head_cap - capture.head.len());
                    capture.head.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                }
                if !chunk.is_empty() && tail_cap > 0 {
                    capture.tail.extend_from_slice(chunk);
                    if capture.tail.len() > tail_cap {
                        let excess = capture.tail.len() - tail_cap;
                        capture.tail.drain(..excess);
                    }
                }
            }
        }
    }
    capture
}

#[cfg(unix)]
fn shared_capture_budgets(
    stdout_bytes: usize,
    stderr_bytes: usize,
    budget: usize,
) -> (usize, usize) {
    let stdout_floor = budget / 2;
    let stderr_floor = budget.saturating_sub(stdout_floor);
    let mut stdout_budget = stdout_bytes.min(stdout_floor);
    let mut stderr_budget = stderr_bytes.min(stderr_floor);
    let mut remaining = budget.saturating_sub(stdout_budget.saturating_add(stderr_budget));

    let stdout_extra = stdout_bytes.saturating_sub(stdout_budget).min(remaining);
    stdout_budget = stdout_budget.saturating_add(stdout_extra);
    remaining -= stdout_extra;

    let stderr_extra = stderr_bytes.saturating_sub(stderr_budget).min(remaining);
    stderr_budget = stderr_budget.saturating_add(stderr_extra);
    (stdout_budget, stderr_budget)
}

#[cfg(unix)]
fn rebalance_captures(stdout: &mut Capture, stderr: &mut Capture, budget: usize) {
    let (stdout_budget, stderr_budget) =
        shared_capture_budgets(stdout.total_bytes, stderr.total_bytes, budget);
    stdout.fit_to_budget(stdout_budget);
    stderr.fit_to_budget(stderr_budget);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sandbox::SandboxConfig;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    struct Fixture {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        sandbox: SandboxConfig,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let mut sandbox = SandboxConfig::new(&workspace);
        sandbox.allow_process = true;
        sandbox.allow_shell = true;
        sandbox.exec_timeout = Duration::from_secs(10);
        Fixture {
            _dir: dir,
            workspace,
            sandbox,
        }
    }

    impl Fixture {
        fn ctx(&self) -> ToolContext<'_> {
            ToolContext {
                workspace: &self.workspace,
                sandbox: &self.sandbox,
                execution_scope: "exec-test",
                active_skills: &[],
                registered_tools: &[],
                progress: ToolProgressSink::null(),
                cancellation: Default::default(),
            }
        }
    }

    fn process_is_alive(pid: i32) -> bool {
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    async fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
        let started = std::time::Instant::now();
        while process_is_alive(pid) && started.elapsed() < timeout {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        !process_is_alive(pid)
    }

    // ----------- shell-word parser -----------

    #[test]
    fn simple_command_is_direct_spawn() {
        assert!(!shell_command_has_operators("cargo test --workspace"));
        assert!(!shell_command_has_operators("echo hello world"));
        assert!(!shell_command_has_operators("git status"));
    }

    #[test]
    fn operators_in_quotes_are_not_operators() {
        assert!(!shell_command_has_operators("echo 'hello | world'"));
        assert!(!shell_command_has_operators("echo \"$HOME\""));
        assert!(!shell_command_has_operators("grep 'a|b' file"));
    }

    #[test]
    fn bare_operators_require_shell() {
        assert!(shell_command_has_operators("cat file | head"));
        assert!(shell_command_has_operators("echo hello > out"));
        assert!(shell_command_has_operators("echo $HOME"));
        assert!(shell_command_has_operators("cat < in"));
        assert!(shell_command_has_operators("cmd1 && cmd2"));
        assert!(shell_command_has_operators("cmd1; cmd2"));
        assert!(shell_command_has_operators("echo `date`"));
    }

    #[test]
    fn shell_word_parse_splits_words() {
        let (prog, argv) = shell_word_parse("cargo test --workspace");
        assert_eq!(prog, "cargo");
        assert_eq!(argv, vec!["test", "--workspace"]);
    }

    #[test]
    fn shell_word_parse_handles_quoting() {
        let (prog, argv) = shell_word_parse("echo 'hello world' \"foo bar\"");
        assert_eq!(prog, "echo");
        assert_eq!(argv, vec!["hello world", "foo bar"]);
    }

    #[test]
    fn shell_word_parse_preserves_literal_dollar() {
        let (_prog, argv) = shell_word_parse("echo $HOME");
        assert_eq!(argv, vec!["$HOME"]);
    }

    #[test]
    fn shell_word_parse_single_argument_produces_empty_argv() {
        let (prog, argv) = shell_word_parse("pwd");
        assert_eq!(prog, "pwd");
        assert!(argv.is_empty());
    }

    // ----------- execution tests -----------

    #[tokio::test]
    async fn simple_command_runs_directly_without_shell() {
        let f = fixture();
        // Quoted arguments hide operators from the shell-detection parser.
        let out = ExecTool
            .execute(json!({"command": "echo '$HOME' '&&' ls"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.starts_with("exit=0"), "{}", out.text);
        // Direct spawn: quotes are stripped, literal tokens preserved.
        assert!(out.text.contains("$HOME && ls"), "{}", out.text);
    }

    #[tokio::test]
    async fn interactive_sudo_uses_private_pty_secret_input_without_echo() {
        use std::os::unix::fs::PermissionsExt;

        let f = fixture();
        let sudo = f.workspace.join("sudo");
        std::fs::write(
            &sudo,
            concat!(
                "#!/bin/sh\n",
                "stty -echo\n",
                "printf 'Password:'\n",
                "IFS= read -r answer\n",
                "stty echo\n",
                "printf '\\naccepted=%s\\n' \"$(test \"$answer\" = swordfish && echo yes || echo no)\"\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o700)).unwrap();

        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::channel(crate::tool::PROGRESS_CHANNEL_CAPACITY);
        let progress = ToolProgressSink::live(progress_tx);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress,
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &f.sandbox,
            execution_scope: "interactive-sudo-test",
        };
        let command = sudo.display().to_string();
        let execution = ExecTool.execute(json!({"command": command}), &ctx);
        tokio::pin!(execution);

        let output = loop {
            tokio::select! {
                result = &mut execution => break result.unwrap(),
                progress = progress_rx.recv() => match progress.expect("progress channel") {
                    crate::tool::ToolProgress::Input(request) => {
                        assert!(request.secret);
                        assert_eq!(request.prompt, "Password:");
                        request.respond(b"swordfish".to_vec());
                    }
                    crate::tool::ToolProgress::Output { bytes, .. } => {
                        assert!(
                            !String::from_utf8_lossy(&bytes).contains("swordfish"),
                            "secret was echoed in progress"
                        );
                    }
                    _ => {}
                }
            }
        };

        assert!(output.text.contains("Password:"), "{}", output.text);
        assert!(output.text.contains("accepted=yes"), "{}", output.text);
        assert!(!output.text.contains("swordfish"), "{}", output.text);
    }

    #[test]
    fn explicit_sudo_input_modes_do_not_claim_a_private_pty() {
        assert!(direct_sudo_needs_terminal("sudo id"));
        assert!(direct_sudo_needs_terminal("/usr/bin/sudo id"));
        for command in ["sudo -n id", "sudo -S id", "sudo --askpass id"] {
            assert!(!direct_sudo_needs_terminal(command), "{command}");
        }
    }

    #[tokio::test]
    async fn piped_command_uses_shell() {
        let f = fixture();
        let out = ExecTool
            .execute(json!({"command": "echo $((3 * 14))"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("42"), "{}", out.text);
    }

    #[tokio::test]
    async fn successful_command_without_descendants_releases_its_registry_entry() {
        let f = fixture();
        ExecTool
            .execute(json!({"command": "printf '%s' $$ > leader.pid"}), &f.ctx())
            .await
            .unwrap();
        let leader = std::fs::read_to_string(f.workspace.join("leader.pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();

        assert!(
            !crate::extension_process::process_group_registered_for_test(leader),
            "a completed process group without descendants remained registered"
        );
    }

    #[tokio::test]
    async fn redirected_background_descendant_remains_supervised_after_leader_exit() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.exec_timeout = Duration::from_secs(2);
        let cancellation = crate::tool::CancellationToken::default();
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: cancellation.clone(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-detached-descendant-test",
        };

        let output = ExecTool
            .execute(
                json!({
                    "command": "printf '%s' $$ > leader.pid; sleep 30 </dev/null >/dev/null 2>&1 & printf '%s' $! > descendant.pid"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let leader = std::fs::read_to_string(f.workspace.join("leader.pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let descendant = std::fs::read_to_string(f.workspace.join("descendant.pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let descendant_was_alive = process_is_alive(descendant);
        let group_was_registered =
            crate::extension_process::process_group_registered_for_test(leader);

        cancellation.cancel();
        let descendant_exited = wait_for_process_exit(descendant, Duration::from_secs(2)).await;
        if !descendant_exited {
            unsafe {
                let _ = libc::kill(descendant, libc::SIGKILL);
            }
        }

        assert!(output.text.starts_with("exit=0"), "{}", output.text);
        assert!(
            descendant_was_alive,
            "background descendant exited before supervision could be verified"
        );
        assert!(
            group_was_registered,
            "leader completion prematurely unregistered the descendant process group"
        );
        assert!(
            descendant_exited,
            "background descendant survived cancellation"
        );
        assert!(
            !crate::extension_process::process_group_registered_for_test(leader),
            "cancelled descendant process group remained registered"
        );
    }

    #[tokio::test]
    async fn all_commands_are_rejected_when_unified_shell_authority_is_false() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_shell = false;
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-permission-test",
        };
        for command in [
            "true",
            "true | false",
            "/bin/sh -c 'printf bypass'",
            "python3 -c 'print(1)'",
            "env /bin/sh -c true",
        ] {
            let err = ExecTool
                .execute(json!({"command": command}), &ctx)
                .await
                .unwrap_err();
            assert!(err.message.contains("shell-equivalent"), "{command}: {err}");
        }
    }

    #[tokio::test]
    async fn direct_spawn_rejected_when_allow_process_false() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_process = false;
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-permission-test",
        };
        let err = ExecTool
            .execute(json!({"command": "true"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("allow_process"), "{err}");
    }

    #[tokio::test]
    async fn nonzero_exit_and_stderr_are_reported_as_an_error() {
        let f = fixture();
        let error = ExecTool
            .execute(json!({"command": "echo oops >&2; exit 3"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(error.message.contains("error nonzero_exit"), "{error}");
        assert!(error.message.contains("exit=3"), "{error}");
        assert!(error.message.contains("stderr: 1 lines\noops"), "{error}");
        assert!(error.message.contains("complete_stderr=true"), "{error}");
        assert!(!error.message.contains("truncated_stderr=false"), "{error}");
    }

    #[tokio::test]
    async fn stdout_uses_the_unused_stderr_capture_budget() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.max_output_bytes = 2048;
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-shared-budget-test",
        };
        let out = ExecTool
            .execute(
                json!({"command": "i=0; while [ $i -lt 150 ]; do printf 'abcdefghij\\n'; i=$((i+1)); done"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.text.contains("stdout: 150 lines"), "{}", out.text);
        assert!(out.text.contains("complete_stdout=true"), "{}", out.text);
        assert!(!out.text.contains("truncated_stdout"), "{}", out.text);
    }

    #[tokio::test]
    async fn cwd_is_workspace_bounded() {
        let f = fixture();
        std::fs::create_dir(f.workspace.join("sub")).unwrap();
        let out = ExecTool
            .execute(json!({"command": "pwd", "cwd": "sub"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("/sub"), "{}", out.text);

        let err = ExecTool
            .execute(json!({"command": "pwd", "cwd": "../"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");

        let err = ExecTool
            .execute(json!({"command": "pwd", "cwd": "missing"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("missing"), "{err}");
    }

    #[tokio::test]
    async fn trusted_local_mode_accepts_an_absolute_cwd() {
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_external_paths = true;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-external-path-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let out = ExecTool
            .execute(
                json!({"command": "pwd", "cwd": outside.path().to_string_lossy()}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.text
                .contains(&outside.path().canonicalize().unwrap().display().to_string()),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn output_is_bounded_with_head_tail_truncation() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.max_output_bytes = 2048;
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-output-test",
        };
        let out = ExecTool
            .execute(
                json!({"command": "i=0; while [ $i -lt 2000 ]; do echo \"line $i\"; i=$((i+1)); done"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.text.len() <= sandbox.max_output_bytes,
            "result exceeded the configured cap: {} bytes",
            out.text.len()
        );
        assert!(out.text.len() < 8192, "output must stay bounded");
        assert!(out.text.contains("truncated_stdout=head:"), "{}", out.text);
        assert!(out.text.contains("omitted_bytes:"), "{}", out.text);
        assert!(out.text.contains("line 0"), "head preserved: {}", out.text);
        assert!(
            out.text.contains("line 1999"),
            "tail preserved: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.exec_timeout = Duration::from_millis(200);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-timeout-test",
        };
        let started = std::time::Instant::now();
        let err = ExecTool
            .execute(
                json!({"command": "printf 'partial-before-timeout\\n'; sleep 30"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("timeout"), "{err}");
        assert!(
            err.message.contains("partial-before-timeout"),
            "timeout diagnostics must retain partial output: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must not wait for the child's natural exit"
        );
    }

    #[tokio::test]
    async fn per_call_timeout_overrides_sandbox() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.exec_timeout = Duration::from_secs(30);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-per-call-timeout-test",
        };
        let started = std::time::Instant::now();
        let err = ExecTool
            .execute(json!({"command": "sleep 30", "timeout_ms": 200}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "per-call timeout must fire before sandbox timeout"
        );
    }

    #[tokio::test]
    async fn timeout_drain_is_bounded_when_escaped_descendant_holds_pipes() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.exec_timeout = Duration::from_millis(100);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "exec-escaped-pipe-test",
        };
        let started = std::time::Instant::now();
        let error = ExecTool
            .execute(
                json!({"command": "python3 -c 'import os,time; os.setsid(); open(\"escaped.pid\", \"w\").write(str(os.getpid())); time.sleep(30)' & sleep 30"}),
                &ctx,
            )
            .await
            .unwrap_err();

        if let Ok(pid) = std::fs::read_to_string(f.workspace.join("escaped.pid")) {
            if let Ok(pid) = pid.parse::<i32>() {
                unsafe {
                    let _ = libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        // Either behaviour is acceptable: the escaped descendant may exit
        // quickly enough that the drain succeeds, or it may hold pipes open.
        assert!(
            error.message.contains("output drain abandoned") || error.message.contains("timeout"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "escaped pipe holder defeated the deadline"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_the_child_process_tree() {
        let f = fixture();
        let marker = format!("986{}", std::process::id());
        let args = json!({
            "command": format!("sleep {marker} & wait")
        });

        {
            let ctx = f.ctx();
            let exec = ExecTool.execute(args, &ctx);
            tokio::pin!(exec);
            let _ = tokio::time::timeout(Duration::from_millis(500), &mut exec).await;
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
        let check = tokio::process::Command::new("pgrep")
            .args(["-f", &format!("sleep {marker}")])
            .output()
            .await
            .unwrap();
        assert!(
            check.stdout.is_empty(),
            "grandchild survived cancellation: {}",
            String::from_utf8_lossy(&check.stdout)
        );
    }
}
