//! Bounded repository content search backed by `rg --json`.

use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use ygg_ai::ToolDef;

use crate::tool::{ReplaySafety, Tool, ToolContext, ToolError, ToolOutput};
use crate::tools::{clip_line, parse_args};

/// Display cap for a single match line.
const MAX_LINE_CHARS: usize = 300;
/// Default result cap when `max_results` is omitted.
const DEFAULT_MAX_RESULTS: usize = 50;

#[derive(Deserialize)]
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

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Safe
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let args: SearchArgs = parse_args(args)?;
        if args.query.is_empty() {
            return Err(ToolError::new("invalid arguments: query must be non-empty"));
        }
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
        command.args(["--json", "--sort", "path", "--no-config"]);
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
            .current_dir(ctx.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
            let mut lines = BufReader::new(stdout).lines();
            let mut results: Vec<String> = Vec::new();
            let mut body_bytes = 0usize;
            let mut truncated = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(rendered) = render_match(&line) else {
                    continue;
                };
                if results.len() == max_results || body_bytes + rendered.len() > byte_budget {
                    truncated = true;
                    break;
                }
                body_bytes += rendered.len();
                results.push(rendered);
            }
            (results, truncated)
        };
        let (results, truncated) = tokio::time::timeout(ctx.sandbox.bash_timeout, collect)
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "search exceeded the {:.0}s execution limit",
                    ctx.sandbox.bash_timeout.as_secs_f64()
                ))
            })?;

        if truncated {
            // Enough results — stop ripgrep instead of draining it.
            let _ = child.start_kill();
            let _ = child.wait().await;
        } else {
            let status = child
                .wait()
                .await
                .map_err(|e| ToolError::new(format!("failed to wait for ripgrep: {e}")))?;
            // rg exits 0 on matches, 1 on no matches, 2 on real errors.
            if status.code() == Some(2) && results.is_empty() {
                return Err(ToolError::new(
                    "search failed: ripgrep reported an error (check the query/glob syntax)",
                ));
            }
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
        let sandbox = SandboxConfig::new(&workspace);
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
