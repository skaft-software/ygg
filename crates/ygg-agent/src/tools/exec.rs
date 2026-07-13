//! Program and shell execution with timeout, bounded capture, and child
//! process-tree cleanup.

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Instant;

use bytes::Bytes;
use serde::Deserialize;
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt};
use ygg_ai::ToolDef;

use crate::tool::{OutputStream, Tool, ToolContext, ToolError, ToolOutput, ToolProgressSink};
#[cfg(unix)]
use crate::tools::parse_args;

/// Execution request (tagged by `mode` on the wire).
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum ExecArgs {
    /// Structured execution: no shell interpretation of program or args.
    Process {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        cwd: Option<String>,
    },
    /// Explicit higher-risk mode: the command line is interpreted by `sh -c`.
    Shell {
        command: String,
        cwd: Option<String>,
    },
}

/// The built-in `exec` tool.
///
/// `Process` mode is the default structured path (gated by `allow_process`);
/// `Shell` mode is independently gated by `allow_shell` — enabling process
/// execution does not open a shell backdoor. Relative working directories use
/// the workspace; trusted-local hosts may also use absolute and `~/` paths.
/// Output capture and duration are bounded by
/// [`SandboxConfig::max_output_bytes`](crate::SandboxConfig) and
/// [`SandboxConfig::exec_timeout`](crate::SandboxConfig). On timeout or
/// cancellation the child's whole process group is killed, not just the
/// direct child.
///
/// **Unix-only in v0.1.** Process-tree cleanup requires unix process groups;
/// rather than silently weakening cancellation on other platforms, `execute`
/// returns a clear `unsupported_platform` tool error there.
pub struct ExecTool;

#[async_trait::async_trait]
impl Tool for ExecTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "exec".to_string(),
            description: "Run a local command. mode=process runs a program with an argument \
                          list and no shell interpretation (preferred). mode=shell runs a command \
                          line via `sh -c` and must be permitted. Relative cwd values use the \
                          workspace; trusted-local hosts also accept absolute and `~/` cwd paths. \
                          Output reports the exit status and bounded stdout/stderr."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["process", "shell"],
                        "description": "process: structured program + args. shell: sh -c command line."
                    },
                    "program": {
                        "type": "string",
                        "description": "process: the program to run."
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "process: arguments passed verbatim (no shell expansion)."
                    },
                    "command": {
                        "type": "string",
                        "description": "shell: the command line for sh -c."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory; relative to workspace, or absolute/~/ when enabled (default: workspace root)."
                    }
                },
                "required": ["mode"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        // Fail clearly rather than silently weakening cleanup: without unix
        // process groups a cancelled command's descendants would be orphaned,
        // so v0.1 does not offer exec at all off unix.
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
        let (mut command, cwd) = match &args {
            ExecArgs::Process { program, args, cwd } => {
                if !ctx.sandbox.allow_process {
                    return Err(ToolError::new(
                        "error not_permitted\nprocess execution is disabled by sandbox policy \
                         (allow_process=false)",
                    ));
                }
                let mut c = tokio::process::Command::new(program);
                c.args(args);
                (c, cwd)
            }
            ExecArgs::Shell { command, cwd } => {
                if !ctx.sandbox.allow_shell {
                    return Err(ToolError::new(
                        "error not_permitted\nshell execution is disabled by sandbox policy \
                         (allow_shell=false)",
                    ));
                }
                let mut c = tokio::process::Command::new("/bin/sh");
                c.arg("-c").arg(command);
                (c, cwd)
            }
        };

        let workdir: PathBuf = match cwd {
            None => ctx.workspace.to_path_buf(),
            Some(rel) => {
                let dir = ctx.resolve_existing(rel)?;
                if !dir.is_dir() {
                    return Err(ToolError::new(format!(
                        "error invalid_cwd\n{rel}: not a directory"
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
        let mut child = command
            .spawn()
            .map_err(|e| ToolError::new(format!("error spawn\nfailed to start command: {e}")))?;
        let mut guard = GroupKillGuard::new(child.id());

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let budget = ctx.sandbox.max_output_bytes;
        let stdout_progress = ctx.progress.clone();
        let stderr_progress = ctx.progress.clone();

        let waited = {
            let work = async {
                let (out, err, status) = tokio::join!(
                    read_bounded_with_progress(
                        &mut stdout_pipe,
                        budget,
                        &stdout_progress,
                        OutputStream::Stdout
                    ),
                    read_bounded_with_progress(
                        &mut stderr_pipe,
                        budget,
                        &stderr_progress,
                        OutputStream::Stderr
                    ),
                    child.wait(),
                );
                (out, err, status)
            };
            tokio::pin!(work);
            tokio::time::timeout(ctx.sandbox.exec_timeout, work).await
        };

        match waited {
            Err(_elapsed) => {
                guard.kill_now();
                let _ = child.wait().await; // reap the killed child
                Err(ToolError::new(format!(
                    "error timeout\ncommand exceeded the {:.0}s execution limit and was killed",
                    ctx.sandbox.exec_timeout.as_secs_f64()
                )))
            }
            Ok((out, err, status)) => {
                guard.disarm();
                let status = status.map_err(|e| {
                    ToolError::new(format!("error io\nfailed to wait for command: {e}"))
                })?;
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
                Ok(ToolOutput::new(text))
            }
        }
    }
}

/// Kills the child's process group on drop unless disarmed, so a cancelled or
/// timed-out `exec` never orphans the process tree. `kill_on_drop(true)`
/// remains as a redundant backstop for the direct child, and tokio reaps
/// dropped children in the background, so no zombie survives either path.
#[cfg(unix)]
struct GroupKillGuard {
    pgid: Option<u32>,
}

#[cfg(unix)]
impl GroupKillGuard {
    fn new(pid: Option<u32>) -> Self {
        // With `process_group(0)` the child's pgid equals its pid.
        Self { pgid: pid }
    }

    fn kill_now(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: kill(2) with a negative pgid signals the process group.
            // The pgid is our freshly spawned child's own group (created via
            // process_group(0)), so this cannot signal unrelated processes.
            unsafe {
                let _ = libc::kill(-(pgid as i32), libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

#[cfg(unix)]
impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
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

    /// Renders one output section:
    ///
    /// ```text
    /// stdout: 12 lines
    /// <lines>
    /// truncated_stdout=false
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
            format!("{name}: {lines} lines\n{text}\ntruncated_{name}=false")
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
    let head_cap = (budget / 2).max(1);
    let tail_cap = (budget - budget / 2).max(1);

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
                if !chunk.is_empty() {
                    capture.truncated = true;
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
                progress: ToolProgressSink::null(),
            }
        }
    }

    #[tokio::test]
    async fn process_mode_runs_without_shell_interpretation() {
        let f = fixture();
        let out = ExecTool
            .execute(
                json!({"mode": "process", "program": "echo", "args": ["$HOME", "&&", "ls"]}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("exit=0"), "{}", out.text);
        // The literal tokens survive: nothing expanded `$HOME` or parsed `&&`.
        assert!(out.text.contains("$HOME && ls"), "{}", out.text);
    }

    #[tokio::test]
    async fn shell_mode_interprets_the_command() {
        let f = fixture();
        let out = ExecTool
            .execute(
                json!({"mode": "shell", "command": "echo $((21 * 2))"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("42"), "{}", out.text);
    }

    #[tokio::test]
    async fn permission_gates_are_independent() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_shell = false;
        let ctx = ToolContext {
            progress: ToolProgressSink::null(),
            workspace: &f.workspace,
            sandbox: &sandbox,
        };
        // Process still allowed…
        assert!(ExecTool
            .execute(json!({"mode": "process", "program": "true"}), &ctx)
            .await
            .is_ok());
        // …while shell is independently denied.
        let err = ExecTool
            .execute(json!({"mode": "shell", "command": "true"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("allow_shell"), "{err}");

        let mut sandbox = f.sandbox.clone();
        sandbox.allow_process = false;
        let ctx = ToolContext {
            progress: ToolProgressSink::null(),
            workspace: &f.workspace,
            sandbox: &sandbox,
        };
        let err = ExecTool
            .execute(json!({"mode": "process", "program": "true"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("allow_process"), "{err}");
    }

    #[tokio::test]
    async fn nonzero_exit_and_stderr_are_reported_as_output() {
        let f = fixture();
        let out = ExecTool
            .execute(
                json!({"mode": "shell", "command": "echo oops >&2; exit 3"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("exit=3"), "{}", out.text);
        assert!(out.text.contains("stderr: 1 lines\noops"), "{}", out.text);
        assert!(out.text.contains("truncated_stderr=false"), "{}", out.text);
    }

    #[tokio::test]
    async fn cwd_is_workspace_bounded() {
        let f = fixture();
        std::fs::create_dir(f.workspace.join("sub")).unwrap();
        let out = ExecTool
            .execute(
                json!({"mode": "process", "program": "pwd", "cwd": "sub"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("/sub"), "{}", out.text);

        let err = ExecTool
            .execute(
                json!({"mode": "process", "program": "pwd", "cwd": "../"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");

        let err = ExecTool
            .execute(
                json!({"mode": "process", "program": "pwd", "cwd": "missing"}),
                &f.ctx(),
            )
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
            progress: ToolProgressSink::null(),
        };

        let out = ExecTool
            .execute(
                json!({
                    "mode": "process",
                    "program": "pwd",
                    "cwd": outside.path().to_string_lossy()
                }),
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
            progress: ToolProgressSink::null(),
            workspace: &f.workspace,
            sandbox: &sandbox,
        };
        let out = ExecTool
            .execute(
                json!({"mode": "shell", "command": "i=0; while [ $i -lt 2000 ]; do echo \"line $i\"; i=$((i+1)); done"}),
                &ctx,
            )
            .await
            .unwrap();
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
            progress: ToolProgressSink::null(),
            workspace: &f.workspace,
            sandbox: &sandbox,
        };
        let started = std::time::Instant::now();
        let err = ExecTool
            .execute(
                json!({"mode": "process", "program": "sleep", "args": ["30"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must not wait for the child's natural exit"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_the_child_process_tree() {
        let f = fixture();
        // A uniquely identifiable process tree: sh spawns a grandchild sleep.
        let marker = format!("986{}", std::process::id());
        let args = json!({
            "mode": "shell",
            "command": format!("sleep {marker} & wait")
        });

        {
            let ctx = f.ctx();
            let exec = ExecTool.execute(args, &ctx);
            tokio::pin!(exec);
            // Poll long enough for the child to spawn, then cancel by drop.
            let _ = tokio::time::timeout(Duration::from_millis(500), &mut exec).await;
        } // exec dropped here → GroupKillGuard kills the process group

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
