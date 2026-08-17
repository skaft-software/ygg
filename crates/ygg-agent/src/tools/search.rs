//! Bounded repository content search backed by `rg --json`.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use ygg_ai::ToolDef;

use crate::effect::ToolEffect;
use crate::tool::{ReplaySafety, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput};
use crate::tools::{clip_line, parse_args, validate_effect_path};

/// Display cap for a single match line.
const MAX_LINE_CHARS: usize = 300;
/// Default result cap when `max_results` is omitted.
const DEFAULT_MAX_RESULTS: usize = 50;
/// Hard cap for one structured `rg --json` record before parsing.
const MAX_RG_EVENT_BYTES: usize = 256 * 1024;
const RG_MAX_COLUMNS: &str = "1024";
const RG_MAX_FILESIZE: &str = "32M";
const MAX_SEARCH_PATTERN_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    mode: SearchMode,
    max_results: Option<usize>,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SearchMode {
    #[default]
    Literal,
    Regex,
}

/// The built-in `search` tool.
///
/// Read-only. Shells out to ripgrep with `--json` (structured output, no
/// shell interpolation of the query — every value is passed as its own
/// argument after `--`) and reformats matches into compact
/// `path:line  text` lines. Results are sorted by path for deterministic
/// ordering, capped by `max_results` and by the sandbox output-byte limit,
/// with explicit truncation metadata. "No matches" is a successful output,
/// not an error.
///
/// Cleanup note: unlike `bash`, ripgrep is cancelled via `kill_on_drop` on
/// the direct child only — `rg` spawns no subprocess tree, so no process-group
/// handling is needed.
pub struct SearchTool;

#[async_trait::async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "search".to_string(),
            description: "Search local file contents. Prefer paths relative to the workspace; \
                          trusted-local hosts also accept absolute and `~/` paths for intentional \
                          external searches. Matches are literal by default; set mode=regex for \
                          regular expressions. Returns `path:line  text` lines with a match count and \
                          truncation flag."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text (or regex when mode=regex) to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search; relative to workspace, or absolute/~/ when enabled (default: workspace root)."
                    },
                    "glob": {
                        "type": "string",
                        "description": "File pattern filter, e.g. \"*.rs\"."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["literal", "regex"],
                        "description": "Matching mode (default literal)."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum matches to return (default 50)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(
        &self,
        arguments: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolEffect, ToolError> {
        if !(ctx.sandbox.allow_process && ctx.sandbox.allow_shell) {
            return Err(ToolError::new(
                "error not_permitted\nsearch requires command execution \
                 (allow_process=true and allow_shell=true)",
            ));
        }
        let arguments = arguments
            .as_object()
            .ok_or_else(|| ToolError::new("invalid arguments: expected an object"))?;
        if arguments.len() > 5
            || arguments.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "query" | "path" | "glob" | "mode" | "max_results"
                )
            })
        {
            return Err(ToolError::new("invalid arguments: unknown property"));
        }
        let query = arguments
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::new("invalid arguments: `query` must be a string"))?;
        if query.is_empty() {
            return Err(ToolError::new("invalid arguments: query must be non-empty"));
        }
        if query.len() > MAX_SEARCH_PATTERN_BYTES {
            return Err(ToolError::new(format!(
                "invalid arguments: query is {} bytes (limit {MAX_SEARCH_PATTERN_BYTES})",
                query.len()
            )));
        }
        for name in ["path", "glob"] {
            if arguments.get(name).is_some_and(|value| !value.is_string()) {
                return Err(ToolError::new(format!(
                    "invalid arguments: `{name}` must be a string"
                )));
            }
        }
        if let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str) {
            validate_effect_path(path, ctx.sandbox.allow_external_paths)?;
        }
        if arguments
            .get("glob")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|glob| glob.len() > MAX_SEARCH_PATTERN_BYTES)
        {
            return Err(ToolError::new(format!(
                "invalid arguments: `glob` exceeds {MAX_SEARCH_PATTERN_BYTES} bytes"
            )));
        }
        if arguments
            .get("mode")
            .is_some_and(|value| !matches!(value.as_str(), Some("literal" | "regex")))
        {
            return Err(ToolError::new(
                "invalid arguments: `mode` must be `literal` or `regex`",
            ));
        }
        if arguments.get("max_results").is_some_and(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .is_none_or(|value| value == 0)
        }) {
            return Err(ToolError::new(
                "invalid arguments: `max_results` must be a positive integer",
            ));
        }
        // Search currently executes `rg` from PATH as a native child. Treat it
        // as process authority even though its argument construction is fixed.
        Ok(ToolEffect::HostProcess)
    }

    fn replay_safety(&self) -> ReplaySafety {
        // `rg` is resolved afresh from ambient PATH. Recovery must not assume
        // the resulting native executable is idempotent.
        ReplaySafety::Unsafe
    }

    fn concurrency(&self) -> ToolConcurrency {
        // Native process effects remain ordered unless an isolated backend can
        // prove independence and enforce aggregate resource bounds.
        ToolConcurrency::Sequential
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        self.effect(&args, ctx)?;
        let args: SearchArgs = parse_args(args)?;
        let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS).max(1);

        // Resolve explicit paths through the host policy. `rg` keeps relative
        // display paths for workspace targets and receives an absolute target
        // for trusted-local paths outside the workspace.
        let search_path = args
            .path
            .as_deref()
            .map(|path| ctx.resolve_existing(path))
            .transpose()?;

        let mut command = tokio::process::Command::new("rg");
        command.args([
            "--json",
            "--sort",
            "path",
            "--no-config",
            "--max-columns",
            RG_MAX_COLUMNS,
            "--max-columns-preview",
            "--max-filesize",
            RG_MAX_FILESIZE,
        ]);
        if args.mode == SearchMode::Literal {
            command.arg("--fixed-strings");
        }
        if let Some(glob) = &args.glob {
            command.args(["--glob", glob]);
        }
        // `--` terminates flags: the model's query and path are data, never
        // options, and no shell is involved at any point.
        command.arg("--").arg(&args.query);
        if let Some(path) = &search_path {
            if let Ok(relative) = path.strip_prefix(ctx.workspace) {
                command.arg(if relative.as_os_str().is_empty() {
                    std::path::Path::new(".")
                } else {
                    relative
                });
            } else {
                command.arg(path);
            }
        }
        command
            .env_clear()
            .envs(crate::extension_process::sanitized_subprocess_environment())
            .current_dir(ctx.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Error text is not surfaced (it can contain host paths), so never
            // create an unread pipe that a child can fill and deadlock on.
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::new("search is unavailable: ripgrep (rg) was not found on PATH")
            } else {
                ToolError::new(format!("failed to start ripgrep: {e}"))
            }
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::new("failed to capture ripgrep output"))?;

        let byte_budget = ctx.sandbox.max_output_bytes.saturating_sub(128).max(1024);
        let collect = async {
            let (results, truncated) = collect_rg_stdout(stdout, max_results, byte_budget).await?;

            let status = if truncated {
                // Enough results — stop ripgrep instead of draining it.
                let _ = child.start_kill();
                child
                    .wait()
                    .await
                    .map_err(|error| ToolError::new(format!("failed to reap ripgrep: {error}")))?;
                None
            } else {
                Some(child.wait().await.map_err(|error| {
                    ToolError::new(format!("failed to wait for ripgrep: {error}"))
                })?)
            };
            Ok::<_, ToolError>((results, truncated, status))
        };
        let (results, truncated, status) =
            match tokio::time::timeout(ctx.sandbox.bash_timeout, collect).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                    return Err(error);
                }
                Err(_) => {
                    let _ = child.start_kill();
                    // Bound cleanup separately; `kill_on_drop` remains the final
                    // backstop if an unusual platform does not reap promptly.
                    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
                    return Err(ToolError::new(format!(
                        "search exceeded the {:.0}s execution limit",
                        ctx.sandbox.bash_timeout.as_secs_f64()
                    )));
                }
            };

        // rg exits 0 on matches, 1 on no matches, 2 on real errors.
        if status.is_some_and(|status| status.code() == Some(2)) && results.is_empty() {
            return Err(ToolError::new(
                "search failed: ripgrep reported an error (check the query/glob syntax)",
            ));
        }

        if results.is_empty() {
            return Ok(ToolOutput::new("no matches"));
        }
        let count_line = if truncated {
            format!("{}+ matches", results.len())
        } else if results.len() == 1 {
            "1 match".to_string()
        } else {
            format!("{} matches", results.len())
        };
        Ok(ToolOutput::new(format!(
            "{count_line}\n{}\ntruncated={truncated}",
            results.join("\n")
        )))
    }
}

async fn collect_rg_stdout<R: tokio::io::AsyncRead + Unpin>(
    mut stdout: R,
    max_results: usize,
    byte_budget: usize,
) -> Result<(Vec<String>, bool), ToolError> {
    let mut results = Vec::new();
    let mut body_bytes = 0usize;
    let mut event = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];

    loop {
        let read = stdout
            .read(&mut chunk)
            .await
            .map_err(|error| ToolError::new(format!("failed to read ripgrep output: {error}")))?;
        if read == 0 {
            if !event.is_empty()
                && record_rg_event(
                    &event,
                    &mut results,
                    &mut body_bytes,
                    max_results,
                    byte_budget,
                )
            {
                return Ok((results, true));
            }
            return Ok((results, false));
        }

        let mut cursor = 0;
        while cursor < read {
            let remainder = &chunk[cursor..read];
            let newline = remainder.iter().position(|byte| *byte == b'\n');
            let end = newline.map_or(read, |offset| cursor + offset);
            let segment = &chunk[cursor..end];
            if event.len().saturating_add(segment.len()) > MAX_RG_EVENT_BYTES {
                return Err(ToolError::new(format!(
                    "search output record exceeded the {MAX_RG_EVENT_BYTES}-byte limit"
                )));
            }
            event.extend_from_slice(segment);
            let Some(_) = newline else {
                break;
            };
            if record_rg_event(
                &event,
                &mut results,
                &mut body_bytes,
                max_results,
                byte_budget,
            ) {
                return Ok((results, true));
            }
            event.clear();
            cursor = end + 1;
        }
    }
}

fn record_rg_event(
    event: &[u8],
    results: &mut Vec<String>,
    body_bytes: &mut usize,
    max_results: usize,
    byte_budget: usize,
) -> bool {
    let Ok(event) = std::str::from_utf8(event) else {
        return false;
    };
    let Some(rendered) = render_match(event) else {
        return false;
    };
    if results.len() == max_results || body_bytes.saturating_add(rendered.len()) > byte_budget {
        return true;
    }
    *body_bytes += rendered.len();
    results.push(rendered);
    false
}

/// Converts one `rg --json` event line into a `path:line  text` result, or
/// `None` for non-match events (begin/end/summary).
fn render_match(json_line: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(json_line).ok()?;
    if event.get("type")?.as_str()? != "match" {
        return None;
    }
    let data = event.get("data")?;
    let path = data.get("path")?.get("text")?.as_str()?;
    let line_number = data.get("line_number")?.as_u64()?;
    let text = data
        .get("lines")
        .and_then(|l| l.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_end();
    Some(format!(
        "{path}:{line_number}  {}",
        clip_line(text, MAX_LINE_CHARS)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxConfig;
    use crate::ToolProgressSink;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        sandbox: SandboxConfig,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("src/api.rs"),
            "pub enum AudioPayload {\n    Inline,\n}\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("src/chat.rs"),
            "use AudioPayload;\nfn f() { let _ = AudioPayload::Inline; }\n",
        )
        .unwrap();
        let mut sandbox = SandboxConfig::new(&workspace);
        sandbox.allow_process = true;
        sandbox.allow_shell = true;
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
                execution_scope: "search-test",
                resource_owner: "search-test",
                active_skills: &[],
                registered_tools: &[],
                progress: ToolProgressSink::null(),
                cancellation: Default::default(),
            }
        }
    }

    fn rg_available() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn effect_requires_process_authority_and_metadata_fails_closed() {
        let mut fixture = fixture();
        assert_eq!(
            SearchTool
                .effect(&json!({"query": "needle"}), &fixture.ctx())
                .unwrap(),
            ToolEffect::HostProcess
        );
        assert_eq!(SearchTool.replay_safety(), ReplaySafety::Unsafe);
        assert_eq!(SearchTool.concurrency(), ToolConcurrency::Sequential);

        fixture.sandbox.allow_process = false;
        assert!(SearchTool
            .effect(&json!({"query": "needle"}), &fixture.ctx())
            .unwrap_err()
            .to_string()
            .contains("allow_process=true"));
    }

    #[test]
    fn effect_rejects_malicious_argument_shapes_before_spawning() {
        let f = fixture();
        for arguments in [
            json!({"query": "needle", "path": "../outside"}),
            json!({"query": "needle", "path": "/tmp/outside"}),
            json!({"query": "needle", "unexpected": true}),
            json!({"query": ""}),
            json!({"query": "needle", "max_results": 0}),
        ] {
            assert!(
                SearchTool.effect(&arguments, &f.ctx()).is_err(),
                "{arguments}"
            );
        }
        assert!(SearchTool
            .effect(
                &json!({"query": "x".repeat(MAX_SEARCH_PATTERN_BYTES + 1)}),
                &f.ctx(),
            )
            .is_err());
    }

    fn match_event(path: &str, line: u64, text: &str) -> String {
        serde_json::json!({
            "type": "match",
            "data": {
                "path": {"text": path},
                "line_number": line,
                "lines": {"text": text}
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn bounded_rg_framing_handles_fragmentation_final_records_and_limits() {
        let first = match_event("a.rs", 1, "first\n");
        let second = match_event("b.rs", 2, "second\n");
        let input = format!("{first}\n{{not json}}\n{second}");
        let reader =
            tokio::io::BufReader::with_capacity(3, std::io::Cursor::new(input.into_bytes()));
        let (results, truncated) = collect_rg_stdout(reader, 10, 4 * 1024).await.unwrap();
        assert_eq!(results, vec!["a.rs:1  first", "b.rs:2  second"]);
        assert!(!truncated);

        let input = format!("{first}\n{second}\n");
        let (results, truncated) =
            collect_rg_stdout(std::io::Cursor::new(input.into_bytes()), 1, 4 * 1024)
                .await
                .unwrap();
        assert_eq!(results, vec!["a.rs:1  first"]);
        assert!(truncated);

        let input = format!("{first}\n{second}\n");
        let (results, truncated) = collect_rg_stdout(
            std::io::Cursor::new(input.into_bytes()),
            10,
            "a.rs:1  first".len(),
        )
        .await
        .unwrap();
        assert_eq!(results, vec!["a.rs:1  first"]);
        assert!(truncated);

        let oversized = vec![b'x'; MAX_RG_EVENT_BYTES + 1];
        let error = collect_rg_stdout(std::io::Cursor::new(oversized), 10, 4 * 1024)
            .await
            .unwrap_err();
        assert!(error.message.contains("record exceeded"), "{error}");
    }

    #[tokio::test]
    async fn literal_matches_are_formatted_and_sorted() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        let out = SearchTool
            .execute(json!({"query": "AudioPayload"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.starts_with("3 matches\n"), "{}", out.text);
        assert!(
            out.text.contains("src/api.rs:1  pub enum AudioPayload {"),
            "{}",
            out.text
        );
        assert!(out.text.contains("src/chat.rs:1  use AudioPayload;"));
        assert!(out.text.ends_with("truncated=false"));
        // Deterministic path ordering: api.rs before chat.rs.
        let api = out.text.find("src/api.rs").unwrap();
        let chat = out.text.find("src/chat.rs").unwrap();
        assert!(api < chat);
    }

    #[tokio::test]
    async fn no_matches_is_successful_output() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        let out = SearchTool
            .execute(json!({"query": "NoSuchSymbolAnywhere"}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(out.text, "no matches");
    }

    #[tokio::test]
    async fn max_results_truncates_explicitly() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        let out = SearchTool
            .execute(json!({"query": "AudioPayload", "max_results": 1}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.starts_with("1+ matches\n"), "{}", out.text);
        assert!(out.text.ends_with("truncated=true"), "{}", out.text);
    }

    #[tokio::test]
    async fn regex_mode_and_glob_filter() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        let out = SearchTool
            .execute(
                json!({"query": "enum \\w+Payload", "mode": "regex", "glob": "api.rs"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("1 match\n"), "{}", out.text);
        assert!(out.text.contains("src/api.rs:1"), "{}", out.text);

        // The same pattern is inert in literal mode.
        let out = SearchTool
            .execute(json!({"query": "enum \\w+Payload"}), &f.ctx())
            .await
            .unwrap();
        assert_eq!(out.text, "no matches");
    }

    #[tokio::test]
    async fn scoped_path_is_validated_and_used() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        let out = SearchTool
            .execute(
                json!({"query": "AudioPayload", "path": "src/api.rs"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("1 match\n"), "{}", out.text);

        let err = SearchTool
            .execute(json!({"query": "x", "path": "../"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");
    }

    #[tokio::test]
    async fn trusted_local_mode_searches_an_absolute_path() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("outside.txt");
        std::fs::write(&file, "needle outside workspace\n").unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_external_paths = true;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "search-test",
            resource_owner: "search-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };

        let out = SearchTool
            .execute(
                json!({"query": "needle", "path": outside.path().to_string_lossy()}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.text.contains("outside.txt:1"), "{}", out.text);
    }

    #[tokio::test]
    async fn dashed_query_is_not_treated_as_a_flag() {
        if !rg_available() {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let f = fixture();
        std::fs::write(f.workspace.join("notes.txt"), "--force is dangerous\n").unwrap();
        let out = SearchTool
            .execute(json!({"query": "--force"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("notes.txt:1"), "{}", out.text);
    }
}
