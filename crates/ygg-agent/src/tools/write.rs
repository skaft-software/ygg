//! Single-file create or full overwrite.

use serde::Deserialize;
use ygg_ai::ToolDef;

use crate::secure_fs::{PreparedMutation, SecureFileError};
use crate::tool::{content_hash, Tool, ToolContext, ToolError, ToolOutput};
use crate::tools::{format_unified_diff, parse_args, MAX_FILE_BYTES};

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    /// Optional hash from a prior `read`; rejects the write if the existing
    /// file content no longer matches.
    expected_hash: Option<String>,
}

/// The built-in `write` tool.
///
/// Creates a new file (and missing parent directories) or completely replaces
/// an existing file.  `expected_hash` from a prior `read` gates the overwrite
/// against that existing content; without it the caller accepts
/// last-write-wins.  Writes are atomic per-file (temp file + rename).
pub struct WriteTool;

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write".to_string(),
            description: "Create or fully replace one file. Creates missing parent \
                          directories. Pass expected_hash from a prior read to reject \
                          stale writes; omitting it accepts last-write-wins. Prefer \
                          paths relative to the workspace."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path; relative to the workspace, or absolute/~/ when enabled."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full file content to write."
                    },
                    "expected_hash": {
                        "type": "string",
                        "description": "Optional hash from read; rejects the write if the file changed."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        if !ctx.sandbox.allow_write {
            return Err(ToolError::new(
                "error not_permitted\nwrite is disabled by sandbox policy (allow_write=false)",
            ));
        }
        let args: WriteArgs = parse_args(args)?;
        let display_path = ctx.display_path(&args.path);
        let target = ctx.resolve_create(&args.path)?;
        let cancellation = ctx.cancellation.clone();
        tokio::task::spawn_blocking(move || {
            create_or_replace(
                &display_path,
                &target,
                &args.path,
                &args.content,
                args.expected_hash.as_deref(),
                &cancellation,
            )
        })
        .await
        .map_err(|error| ToolError::new(format!("error internal\nwrite worker failed: {error}")))?
    }
}

fn stale_error(path: &str, expected: &str, actual: &str) -> ToolError {
    ToolError::new(format!(
        "error stale_file\n{path}  expected hash={expected} actual={actual}\n\
         The file has changed since it was last read."
    ))
}

fn file_error(path_display: &str, error: SecureFileError) -> ToolError {
    match error {
        SecureFileError::NotRegular => ToolError::new(format!(
            "error is_directory\n{path_display}: target is not a regular file"
        )),
        SecureFileError::Changed => ToolError::new(format!(
            "error stale_file\n{path_display}: changed while the write was in progress; retry from a fresh read"
        )),
        SecureFileError::Cancelled => {
            ToolError::new(format!("error cancelled\n{path_display}: write cancelled"))
        }
        other => ToolError::new(format!("error io\n{path_display}: {other}")),
    }
}

fn create_or_replace(
    display_path: &str,
    target: &std::path::Path,
    path: &str,
    content: &str,
    expected_hash: Option<&str>,
    cancellation: &crate::tool::CancellationToken,
) -> Result<ToolOutput, ToolError> {
    if content.len() > MAX_FILE_BYTES {
        return Err(ToolError::new(format!(
            "error too_large\n{path}: content is {} bytes (limit {MAX_FILE_BYTES})",
            content.len()
        )));
    }
    let prepared = PreparedMutation::prepare(target, true, MAX_FILE_BYTES)
        .map_err(|error| file_error(display_path, error))?;
    let old_content = prepared.original().map(<[u8]>::to_vec);
    let exists = old_content.is_some();

    // Hash gate against existing content.
    if let Some(ref current) = old_content {
        if let Some(expected) = expected_hash {
            let actual = content_hash(current);
            if actual != expected {
                return Err(stale_error(display_path, expected, &actual));
            }
        }
    }

    // Generate a diff for every content-changing write. Creation previews are
    // bounded, but still carry a real hunk header so the TUI recognizes and
    // renders them through the same diff path as replacements.
    let detail = if let Some(ref current) = old_content {
        let old_text = String::from_utf8_lossy(current).into_owned();
        if old_text == content {
            String::from("(no change)")
        } else {
            format_unified_diff(path, &old_text, content, &old_text)
        }
    } else {
        let preview_lines: Vec<&str> = content.lines().take(10).collect();
        let total = content.lines().count();
        let mut preview = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{total} @@\n");
        for line in &preview_lines {
            preview.push_str(&format!("+{line}\n"));
        }
        if total > preview_lines.len() {
            preview.push_str(&format!(
                "… {} more line{}\n",
                total - preview_lines.len(),
                if total - preview_lines.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        preview
    };

    prepared
        .commit_if(content.as_bytes(), || cancellation.is_cancelled())
        .map_err(|error| file_error(display_path, error))?;
    let verb = if exists { "replaced" } else { "created" };
    let hash = content_hash(content.as_bytes());
    Ok(ToolOutput::new(format!(
        "ok\n{display_path}  {verb} hash={hash}\n{detail}"
    )))
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
        let mut sandbox = SandboxConfig::new(&workspace);
        sandbox.allow_write = true;
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
                execution_scope: "write-test",
                active_skills: &[],
                registered_tools: &[],
                progress: ToolProgressSink::null(),
                cancellation: Default::default(),
            }
        }
    }

    #[tokio::test]
    async fn creates_file_and_parent_dirs() {
        let f = fixture();
        let out = WriteTool
            .execute(
                json!({"path": "src/new/mod.rs", "content": "pub fn x() {}\n"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        let expected_hash = content_hash(b"pub fn x() {}\n");
        assert!(
            out.text.starts_with(&format!(
                "ok\nsrc/new/mod.rs  created hash={expected_hash}\n"
            )),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("--- /dev/null"),
            "missing diff header: {}",
            out.text
        );
        assert!(
            out.text.contains("@@ -0,0 +1,1 @@"),
            "missing diff hunk: {}",
            out.text
        );
        assert!(
            out.text.contains("+pub fn x() {}"),
            "missing preview line: {}",
            out.text
        );
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("src/new/mod.rs")).unwrap(),
            "pub fn x() {}\n"
        );
    }

    #[tokio::test]
    async fn overwrite_existing_gated_by_expected_hash() {
        let f = fixture();
        std::fs::write(f.workspace.join("a.txt"), "old content").unwrap();
        let good = content_hash(b"old content");

        // Wrong hash: rejected, file preserved.
        let err = WriteTool
            .execute(
                json!({"path": "a.txt", "content": "new", "expected_hash": "0000000000000000"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("stale_file"), "{err}");
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("a.txt")).unwrap(),
            "old content"
        );

        // Matching hash: replacement proceeds.
        let out = WriteTool
            .execute(
                json!({"path": "a.txt", "content": "new", "expected_hash": good}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("replaced"), "{}", out.text);

        // No hash at all: last-write-wins overwrite.
        let out = WriteTool
            .execute(json!({"path": "a.txt", "content": "newest"}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("replaced"), "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("a.txt")).unwrap(),
            "newest"
        );
    }

    #[tokio::test]
    async fn empty_content_creates_empty_file_not_deletes() {
        let f = fixture();
        let out = WriteTool
            .execute(json!({"path": "empty.txt", "content": ""}), &f.ctx())
            .await
            .unwrap();
        assert!(out.text.contains("created"), "{}", out.text);
        assert!(f.workspace.join("empty.txt").exists());
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("empty.txt")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn requires_allow_write() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_write = false;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            execution_scope: "write-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation: Default::default(),
        };
        let err = WriteTool
            .execute(json!({"path": "x.txt", "content": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.message.contains("not_permitted"), "{err}");
        assert!(!f.workspace.join("x.txt").exists());
    }

    #[tokio::test]
    async fn cancellation_prevents_the_rename_commit() {
        let f = fixture();
        let cancellation = crate::CancellationToken::default();
        cancellation.cancel();
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &f.sandbox,
            execution_scope: "write-cancel-test",
            active_skills: &[],
            registered_tools: &[],
            progress: ToolProgressSink::null(),
            cancellation,
        };
        let error = WriteTool
            .execute(
                json!({"path": "cancelled.txt", "content": "must not commit"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("cancelled"), "{error}");
        assert!(!f.workspace.join("cancelled.txt").exists());
    }

    #[tokio::test]
    async fn rejects_directory_and_escaping_paths() {
        let f = fixture();
        std::fs::create_dir(f.workspace.join("sub")).unwrap();

        let err = WriteTool
            .execute(json!({"path": "sub", "content": "x"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains("is_directory"), "{err}");

        let err = WriteTool
            .execute(json!({"path": "../evil.txt", "content": "x"}), &f.ctx())
            .await
            .unwrap_err();
        assert!(err.message.contains(".."), "{err}");
    }
}
