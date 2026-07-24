//! Bash-compatible command execution with timeout, bounded capture, and child
//! process-tree cleanup.
//!
//! Like Pi, Ygg always gives the complete command string to one selected shell
//! with `-c`. On Unix the default selection order is `/bin/bash`, `bash` on
//! `PATH`, then `sh`; an explicit host-configured shell path takes precedence.

#[cfg(unix)]
use std::path::{Path, PathBuf};
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
use tokio::io::{AsyncRead, AsyncReadExt};
use ygg_ai::ToolDef;

#[cfg(unix)]
use crate::extension_process::ProcessGroupGuard;
use crate::tool::{OutputStream, Tool, ToolContext, ToolError, ToolOutput, ToolProgressSink};
#[cfg(unix)]
use crate::tools::parse_args;

/// One Bash-compatible shell request.
#[derive(Deserialize)]
struct BashArgs {
    /// The complete command string passed to the selected shell with `-c`.
    command: String,
    /// Optional working directory. Relative paths use the workspace;
    /// trusted-local hosts also accept absolute and `~/` paths.
    cwd: Option<String>,
    /// Optional timeout in milliseconds.
    timeout_ms: Option<u64>,
}

/// The built-in `bash` tool.
///
/// Executes the complete command through a Bash-compatible shell with bounded
/// stdout/stderr capture and a timeout.
/// The child's entire process group is killed on timeout or cancellation.
///
/// **Unix-only in v0.1.** Process-tree cleanup requires unix process groups.
pub struct BashTool;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_string(),
            description: "Run a command through the configured Bash-compatible shell. \
                          Omit cwd to run at the workspace root. Output reports the exit \
                          status and bounded stdout/stderr. Complete streams end with \
                          complete_<stream>=true; truncated_<stream>=... means bytes \
                          were omitted."
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
                "error unsupported_platform\nbash is unavailable on this platform in v0.1: \
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
impl BashTool {
    async fn execute_unix(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = parse_args(args)?;
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

        let shell = resolve_shell(ctx.sandbox.shell_path.as_deref());
        let mut command = tokio::process::Command::new(&shell);
        command.arg("-c").arg(&args.command);

        // Honour the per-call timeout when present, bounded by sandbox max.
        let effective_timeout = match args.timeout_ms {
            Some(ms) => Duration::from_millis(ms).min(ctx.sandbox.bash_timeout),
            None => ctx.sandbox.bash_timeout,
        };

        let workdir: PathBuf = match args.cwd.as_ref() {
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
        let mut child = command.spawn().map_err(|e| {
            ToolError::new(format!(
                "error spawn\nfailed to start shell {}: {e}",
                shell.display()
            ))
        })?;
        let guard = ProcessGroupGuard::bash(child.id());

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
                guard.supervise_bash_descendants(
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

#[cfg(unix)]
fn resolve_shell(configured: Option<&Path>) -> PathBuf {
    if let Some(configured) = configured {
        return configured.to_path_buf();
    }
    let system_bash = PathBuf::from("/bin/bash");
    if is_executable_file(&system_bash) {
        return system_bash;
    }
    find_on_path("bash").unwrap_or_else(|| PathBuf::from("sh"))
}

#[cfg(unix)]
fn find_on_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| is_executable_file(candidate))
    })
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

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
        sandbox.bash_timeout = Duration::from_secs(10);
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
                execution_scope: "bash-test",
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

    #[tokio::test]
    async fn every_command_uses_bash_semantics() {
        let f = fixture();
        let out = BashTool
            .execute(
                json!({"command": "printf '%s\\n' brace-{one,two} \"$BASH_VERSION\""}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("exit=0"), "{}", out.text);
        assert!(out.text.contains("brace-one"), "{}", out.text);
        assert!(out.text.contains("brace-two"), "{}", out.text);
        assert!(
            out.text
                .lines()
                .any(|line| line.chars().next().is_some_and(|ch| ch.is_ascii_digit())),
            "BASH_VERSION was empty: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn explicit_shell_path_takes_precedence() {
        use std::os::unix::fs::PermissionsExt;

        let mut f = fixture();
        let shell = f.workspace.join("custom-shell");
        std::fs::write(
            &shell,
            concat!(
                "#!/bin/sh\n",
                "printf 'custom-shell\\n'\n",
                "exec /bin/sh \"$@\"\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        f.sandbox.shell_path = Some(shell);
        let out = BashTool
            .execute(json!({"command": "printf 'command-output\\n'"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("custom-shell"), "{}", out.text);
        assert!(out.text.contains("command-output"), "{}", out.text);
    }

    #[tokio::test]
    async fn successful_command_without_descendants_releases_its_registry_entry() {
        let f = fixture();
        BashTool
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
        sandbox.bash_timeout = Duration::from_secs(2);
        let cancellation = crate::tool::CancellationToken::default();
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: cancellation.clone(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "bash-detached-descendant-test",
        };

        let output = BashTool
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
            execution_scope: "bash-permission-test",
        };
        for command in [
            "true",
            "true | false",
            "/bin/sh -c 'printf bypass'",
            "python3 -c 'print(1)'",
            "env /bin/sh -c true",
        ] {
            let err = BashTool
                .execute(json!({"command": command}), &ctx)
                .await
                .unwrap_err();
            assert!(err.message.contains("shell-equivalent"), "{command}: {err}");
        }
    }

    #[tokio::test]
    async fn bash_rejected_when_allow_process_false() {
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
            execution_scope: "bash-permission-test",
        };
        let err = BashTool
            .execute(json!({"command": "true"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("allow_process"), "{err}");
    }

    #[tokio::test]
    async fn nonzero_exit_and_stderr_are_reported_as_an_error() {
        let f = fixture();
        let error = BashTool
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
            execution_scope: "bash-shared-budget-test",
        };
        let out = BashTool
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
        let out = BashTool
            .execute(json!({"command": "pwd", "cwd": "sub"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("/sub"), "{}", out.text);

        let err = BashTool
            .execute(json!({"command": "pwd", "cwd": "../"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");

        let err = BashTool
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
            execution_scope: "bash-external-path-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let out = BashTool
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
            execution_scope: "bash-output-test",
        };
        let out = BashTool
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
        sandbox.bash_timeout = Duration::from_millis(200);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "bash-timeout-test",
        };
        let started = std::time::Instant::now();
        let err = BashTool
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
        sandbox.bash_timeout = Duration::from_secs(30);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "bash-per-call-timeout-test",
        };
        let started = std::time::Instant::now();
        let err = BashTool
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
        sandbox.bash_timeout = Duration::from_millis(100);
        let ctx = ToolContext {
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "bash-escaped-pipe-test",
        };
        let started = std::time::Instant::now();
        let error = BashTool
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
            let bash = BashTool.execute(args, &ctx);
            tokio::pin!(bash);
            let _ = tokio::time::timeout(Duration::from_millis(500), &mut bash).await;
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
