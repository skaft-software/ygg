//! Bounded, line-addressable file reading.

use serde::Deserialize;
use ygg_ai::ToolDef;

use crate::tool::{content_hash, Tool, ToolContext, ToolError, ToolOutput};
use crate::tools::{clip_line, parse_args};

/// Hard cap on the file size `read` will load (the whole file is read to
/// compute the content hash even when only a window is returned).
const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
/// Display cap for a single line.
const MAX_LINE_CHARS: usize = 2000;
/// Default number of lines returned when `limit` is omitted.
const DEFAULT_LIMIT: usize = 500;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// The built-in `read` tool: bounded file reads with stable line numbers, a
/// whole-file content hash for optimistic edit checks, and explicit
/// continuation metadata.
pub struct ReadTool;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read".to_string(),
            description: "Read a local file with line numbers. Relative paths resolve from the \
                          workspace; trusted-local hosts also accept absolute and `~/` paths. \
                          Returns a header `path:start-end/total hash=…` (the hash covers the \
                          whole file and is used for edit's expected_hash), numbered lines, and \
                          `next_offset=N` when more lines remain."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path; relative to the workspace, or absolute/~/ when enabled."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-indexed line to start from (default 1)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum lines to return (default 500)."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse_args(args)?;
        let target = ctx.resolve_existing(&args.path)?;
        if target.is_dir() {
            return Err(ToolError::new(format!("{}: is a directory", args.path)));
        }
        let metadata = std::fs::metadata(&target)
            .map_err(|e| ToolError::new(format!("{}: {e}", args.path)))?;
        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(ToolError::new(format!(
                "{}: file is too large to read ({} bytes, limit {})",
                args.path,
                metadata.len(),
                MAX_FILE_BYTES
            )));
        }

        let bytes =
            std::fs::read(&target).map_err(|e| ToolError::new(format!("{}: {e}", args.path)))?;
        let hash = content_hash(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);

        if total == 0 {
            return Ok(ToolOutput::new(format!(
                "{}:0-0/0 hash={hash}\n(empty file)\ntruncated=false",
                args.path
            )));
        }
        if offset > total {
            return Err(ToolError::new(format!(
                "{}: offset {offset} is beyond the end of the file ({total} lines)",
                args.path
            )));
        }

        // Reserve some budget for the header/footer lines.
        let byte_budget = ctx.sandbox.max_output_bytes.saturating_sub(256).max(1024);
        let requested_end = (offset + limit - 1).min(total);

        let mut body = String::new();
        let mut end = offset - 1; // last included line
        let mut truncated = false;
        for (i, line) in lines
            .iter()
            .enumerate()
            .take(requested_end)
            .skip(offset - 1)
        {
            let rendered = format!("{}: {}\n", i + 1, clip_line(line, MAX_LINE_CHARS));
            if !body.is_empty() && body.len() + rendered.len() > byte_budget {
                truncated = true;
                break;
            }
            body.push_str(&rendered);
            end = i + 1;
        }

        let header = format!("{}:{offset}-{end}/{total} hash={hash}", args.path);
        let footer = if end < total {
            format!("next_offset={} truncated={truncated}", end + 1)
        } else {
            format!("truncated={truncated}")
        };
        Ok(ToolOutput::new(format!("{header}\n{body}{footer}")))
    }
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
                progress: ToolProgressSink::null(),
            }
        }
    }

    #[tokio::test]
    async fn reads_with_line_numbers_and_hash() {
        let f = fixture();
        std::fs::write(f.workspace.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();

        let out = ReadTool
            .execute(json!({"path": "a.txt"}), &f.ctx())
            .await
            .unwrap();
        let expected_hash = content_hash(b"alpha\nbeta\ngamma\n");
        assert_eq!(
            out.text,
            format!(
                "a.txt:1-3/3 hash={expected_hash}\n1: alpha\n2: beta\n3: gamma\ntruncated=false"
            )
        );
    }

    #[tokio::test]
    async fn offset_and_limit_report_continuation() {
        let f = fixture();
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(f.workspace.join("b.txt"), &content).unwrap();

        let out = ReadTool
            .execute(json!({"path": "b.txt", "offset": 3, "limit": 4}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.starts_with("b.txt:3-6/10 hash="), "{}", out.text);
        assert!(out.text.contains("3: line3\n"));
        assert!(out.text.contains("6: line6\n"));
        assert!(!out.text.contains("7: line7"));
        assert!(out.text.ends_with("next_offset=7 truncated=false"));
    }

    #[tokio::test]
    async fn byte_budget_truncates_with_marker() {
        let f = fixture();
        let content: String = (1..=2000).map(|i| format!("line number {i}\n")).collect();
        std::fs::write(f.workspace.join("big.txt"), &content).unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.max_output_bytes = 2048;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            progress: ToolProgressSink::null(),
        };

        let out = ReadTool
            .execute(json!({"path": "big.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.text.len() < 4096);
        assert!(out.text.contains("truncated=true"), "{}", out.text);
        assert!(out.text.contains("next_offset="), "{}", out.text);
    }

    #[tokio::test]
    async fn directory_missing_and_escaping_paths_fail() {
        let f = fixture();
        std::fs::create_dir(f.workspace.join("sub")).unwrap();

        let err = ReadTool
            .execute(json!({"path": "sub"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("directory"), "{err}");

        let err = ReadTool
            .execute(json!({"path": "missing.txt"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("missing.txt"), "{err}");

        let err = ReadTool
            .execute(json!({"path": "../outside.txt"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");
    }

    #[tokio::test]
    async fn trusted_local_mode_reads_an_absolute_path() {
        let f = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside\n").unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_external_paths = true;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            progress: ToolProgressSink::null(),
        };

        let out = ReadTool
            .execute(json!({"path": outside.path().to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(out.text.contains("1: outside"), "{}", out.text);
    }

    #[tokio::test]
    async fn offset_beyond_end_is_an_error() {
        let f = fixture();
        std::fs::write(f.workspace.join("s.txt"), "only\n").unwrap();
        let err = ReadTool
            .execute(json!({"path": "s.txt", "offset": 5}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("beyond the end"), "{err}");
    }

    #[tokio::test]
    async fn empty_file_reads_cleanly() {
        let f = fixture();
        std::fs::write(f.workspace.join("e.txt"), "").unwrap();
        let out = ReadTool
            .execute(json!({"path": "e.txt"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("e.txt:0-0/0 hash="), "{}", out.text);
        assert!(out.text.contains("(empty file)"));
    }

    #[tokio::test]
    async fn invalid_args_are_a_tool_error() {
        let f = fixture();
        let err = ReadTool
            .execute(json!({"offset": 1}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("invalid arguments"), "{err}");
    }
}
