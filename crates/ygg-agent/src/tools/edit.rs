//! Single-operation file mutation: create, replace, delete.

use std::path::Path;

use serde::Deserialize;
use ygg_ai::ToolDef;

use crate::tool::{content_hash, Tool, ToolContext, ToolError, ToolOutput};
use crate::tools::{clip_line, parse_args};

/// One edit call = one operation (tagged by `operation` on the wire).
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum EditArgs {
    Create {
        path: String,
        content: String,
        expected_hash: Option<String>,
    },
    Replace {
        path: String,
        old: String,
        new: String,
        expected_hash: Option<String>,
    },
    Delete {
        path: String,
        expected_hash: Option<String>,
    },
}

/// The built-in `edit` tool.
///
/// `expected_hash` (the hash returned by `read`) is optional on every
/// operation and provides optimistic concurrency:
///
/// * `create` — when the target already exists, `expected_hash` gates the
///   overwrite against that existing content; without it the caller accepts
///   last-write-wins.
/// * `replace` / `delete` — the edit is rejected as stale when the current
///   content no longer hashes to `expected_hash`.
///
/// Writes are atomic (temp file + rename in the target directory); a failed
/// edit never leaves partial content behind.
pub struct EditTool;

#[async_trait::async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit".to_string(),
            description: "Mutate one local file with one operation: create (write a file, \
                          creating parent directories), replace (substitute an exact unique \
                          `old` string with `new`), or delete. Relative paths resolve from the \
                          workspace; trusted-local hosts also accept absolute and `~/` paths. \
                          Pass expected_hash from a prior read to reject stale edits; omitting it \
                          accepts last-write-wins."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["create", "replace", "delete"],
                        "description": "The mutation to perform."
                    },
                    "path": {
                        "type": "string",
                        "description": "File path; relative to the workspace, or absolute/~/ when enabled."
                    },
                    "content": {
                        "type": "string",
                        "description": "create: the full file content to write."
                    },
                    "old": {
                        "type": "string",
                        "description": "replace: exact existing text; must occur exactly once."
                    },
                    "new": {
                        "type": "string",
                        "description": "replace: replacement text."
                    },
                    "expected_hash": {
                        "type": "string",
                        "description": "Optional hash from read; rejects the edit if the file changed."
                    }
                },
                "required": ["operation", "path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        if !ctx.sandbox.allow_edit {
            return Err(ToolError::new(
                "error not_permitted\nedit is disabled by sandbox policy (allow_edit=false)",
            ));
        }
        match parse_args::<EditArgs>(args)? {
            EditArgs::Create {
                path,
                content,
                expected_hash,
            } => create(ctx, &path, &content, expected_hash.as_deref()),
            EditArgs::Replace {
                path,
                old,
                new,
                expected_hash,
            } => replace(ctx, &path, &old, &new, expected_hash.as_deref()),
            EditArgs::Delete {
                path,
                expected_hash,
            } => delete(ctx, &path, expected_hash.as_deref()),
        }
    }
}

fn stale_error(path: &str, expected: &str, actual: &str) -> ToolError {
    ToolError::new(format!(
        "error stale_file\n{path}  expected hash={expected} actual={actual}\n\
         The file has changed since it was last read."
    ))
}

fn read_current(path_display: &str, target: &Path) -> Result<Vec<u8>, ToolError> {
    if target.is_dir() {
        return Err(ToolError::new(format!(
            "error is_directory\n{path_display}: is a directory"
        )));
    }
    std::fs::read(target).map_err(|e| ToolError::new(format!("error io\n{path_display}: {e}")))
}

/// Atomically writes `data` to `target` via a temp file + rename in the same
/// directory, so a crash or failure never leaves partial content at `target`.
fn write_atomic(path_display: &str, target: &Path, data: &[u8]) -> Result<(), ToolError> {
    let dir = target
        .parent()
        .ok_or_else(|| ToolError::new(format!("error io\n{path_display}: no parent directory")))?;
    let file_name = target
        .file_name()
        .ok_or_else(|| ToolError::new(format!("error io\n{path_display}: no file name")))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let write = std::fs::write(&tmp, data)
        .and_then(|()| std::fs::rename(&tmp, target))
        .map_err(|e| ToolError::new(format!("error io\n{path_display}: {e}")));
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

fn create(
    ctx: &ToolContext<'_>,
    path: &str,
    content: &str,
    expected_hash: Option<&str>,
) -> Result<ToolOutput, ToolError> {
    let target = ctx.resolve_create(path)?;
    let exists = target.symlink_metadata().is_ok();
    if exists {
        let current = read_current(path, &target)?;
        let actual = content_hash(&current);
        if let Some(expected) = expected_hash {
            if actual != expected {
                return Err(stale_error(path, expected, &actual));
            }
        }
        // Without expected_hash the caller accepts last-write-wins.
    } else if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError::new(format!("error io\n{path}: {e}")))?;
    }

    write_atomic(path, &target, content.as_bytes())?;
    let verb = if exists { "replaced" } else { "created" };
    let hash = content_hash(content.as_bytes());
    Ok(ToolOutput::new(format!("ok\n{path}  {verb} hash={hash}")))
}

fn replace(
    ctx: &ToolContext<'_>,
    path: &str,
    old: &str,
    new: &str,
    expected_hash: Option<&str>,
) -> Result<ToolOutput, ToolError> {
    if old.is_empty() {
        return Err(ToolError::new(
            "error invalid_arguments\n`old` must be non-empty",
        ));
    }
    let target = ctx.resolve_existing(path)?;
    let current = read_current(path, &target)?;
    let actual = content_hash(&current);
    if let Some(expected) = expected_hash {
        if actual != expected {
            return Err(stale_error(path, expected, &actual));
        }
    }

    let text = String::from_utf8_lossy(&current).into_owned();
    match text.matches(old).count() {
        0 => Err(no_match_error(path, old, &text)),
        1 => {
            let updated = text.replacen(old, new, 1);
            write_atomic(path, &target, updated.as_bytes())?;
            let added = new.lines().count();
            let removed = old.lines().count();
            let hash = content_hash(updated.as_bytes());
            Ok(ToolOutput::new(format!(
                "ok modified=1\n{path}  +{added} -{removed} hash={hash}"
            )))
        }
        n => Err(ToolError::new(format!(
            "error ambiguous\n{path}\n\"{}\" matches {n} locations. \
             Include more surrounding context to make it unique.",
            clip_line(old, 80)
        ))),
    }
}

/// Builds the `no_match` error, suggesting nearby lines that resemble the
/// first line of the missing `old` text.
fn no_match_error(path: &str, old: &str, text: &str) -> ToolError {
    let mut message = format!(
        "error no_match\n{path}\n\"{}\" not found in file.",
        clip_line(old, 80)
    );
    let needle = old.lines().find(|l| !l.trim().is_empty()).map(str::trim);
    if let Some(needle) = needle {
        let suggestions: Vec<String> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(needle) || line.trim() == needle)
            .take(3)
            .map(|(i, line)| format!("    {}: {}", i + 1, clip_line(line.trim_end(), 120)))
            .collect();
        if !suggestions.is_empty() {
            message.push_str(" Did you mean:\n");
            message.push_str(&suggestions.join("\n"));
        }
    }
    ToolError::new(message)
}

fn delete(
    ctx: &ToolContext<'_>,
    path: &str,
    expected_hash: Option<&str>,
) -> Result<ToolOutput, ToolError> {
    let target = ctx.resolve_existing(path)?;
    let current = read_current(path, &target)?;
    if let Some(expected) = expected_hash {
        let actual = content_hash(&current);
        if actual != expected {
            return Err(stale_error(path, expected, &actual));
        }
    }
    std::fs::remove_file(&target).map_err(|e| ToolError::new(format!("error io\n{path}: {e}")))?;
    Ok(ToolOutput::new(format!("ok\n{path}  deleted")))
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
        sandbox.allow_edit = true;
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
    async fn create_writes_file_and_parent_dirs() {
        let f = fixture();
        let out = EditTool
            .execute(
                json!({"operation": "create", "path": "src/new/mod.rs", "content": "pub fn x() {}\n"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        let expected_hash = content_hash(b"pub fn x() {}\n");
        assert_eq!(
            out.text,
            format!("ok\nsrc/new/mod.rs  created hash={expected_hash}")
        );
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("src/new/mod.rs")).unwrap(),
            "pub fn x() {}\n"
        );
    }

    #[tokio::test]
    async fn create_over_existing_gated_by_expected_hash() {
        let f = fixture();
        std::fs::write(f.workspace.join("a.txt"), "old content").unwrap();
        let good = content_hash(b"old content");

        // Wrong hash: rejected, file preserved.
        let err = EditTool
            .execute(
                json!({"operation": "create", "path": "a.txt", "content": "new", "expected_hash": "0000000000000000"}),
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
        let out = EditTool
            .execute(
                json!({"operation": "create", "path": "a.txt", "content": "new", "expected_hash": good}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("replaced"), "{}", out.text);

        // No hash at all: last-write-wins overwrite.
        let out = EditTool
            .execute(
                json!({"operation": "create", "path": "a.txt", "content": "newest"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("replaced"), "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("a.txt")).unwrap(),
            "newest"
        );
    }

    #[tokio::test]
    async fn replace_exact_unique_match() {
        let f = fixture();
        std::fs::write(f.workspace.join("m.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let out = EditTool
            .execute(
                json!({"operation": "replace", "path": "m.rs", "old": "fn b() {}", "new": "fn b() -> u8 { 1 }"}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("ok modified=1\nm.rs  +1 -1 hash="));
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("m.rs")).unwrap(),
            "fn a() {}\nfn b() -> u8 { 1 }\n"
        );
    }

    #[tokio::test]
    async fn replace_rejects_stale_missing_and_ambiguous() {
        let f = fixture();
        let original = "let x = 1;\nlet x = 1;\nlet y = 2;\n";
        std::fs::write(f.workspace.join("m.rs"), original).unwrap();

        let err = EditTool
            .execute(
                json!({"operation": "replace", "path": "m.rs", "old": "let y = 2;", "new": "z", "expected_hash": "deadbeefdeadbeef"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("stale_file"), "{err}");

        let err = EditTool
            .execute(
                json!({"operation": "replace", "path": "m.rs", "old": "let q = 9;", "new": "z"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("no_match"), "{err}");

        let err = EditTool
            .execute(
                json!({"operation": "replace", "path": "m.rs", "old": "let x = 1;", "new": "z"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("ambiguous"), "{err}");
        assert!(err.message.contains("2 locations"), "{err}");

        // Every failure preserved the original content (atomicity).
        assert_eq!(
            std::fs::read_to_string(f.workspace.join("m.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn no_match_suggests_similar_lines() {
        let f = fixture();
        std::fs::write(f.workspace.join("m.rs"), "    let value = compute();\n").unwrap();
        let err = EditTool
            .execute(
                json!({"operation": "replace", "path": "m.rs", "old": "let value = compute();\nreturn value;", "new": "z"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("Did you mean"), "{err}");
        assert!(err.message.contains("1: "), "{err}");
    }

    #[tokio::test]
    async fn delete_with_and_without_hash() {
        let f = fixture();
        std::fs::write(f.workspace.join("gone.txt"), "bye").unwrap();

        let err = EditTool
            .execute(
                json!({"operation": "delete", "path": "gone.txt", "expected_hash": "1111111111111111"}),
                &f.ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("stale_file"), "{err}");
        assert!(f.workspace.join("gone.txt").exists());

        let good = content_hash(b"bye");
        let out = EditTool
            .execute(
                json!({"operation": "delete", "path": "gone.txt", "expected_hash": good}),
                &f.ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "ok\ngone.txt  deleted");
        assert!(!f.workspace.join("gone.txt").exists());
    }

    #[tokio::test]
    async fn edit_requires_allow_edit() {
        let f = fixture();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_edit = false;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            progress: ToolProgressSink::null(),
        };
        let err = EditTool
            .execute(
                json!({"operation": "create", "path": "x.txt", "content": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("not_permitted"), "{err}");
        assert!(!f.workspace.join("x.txt").exists());
    }

    #[tokio::test]
    async fn trusted_local_mode_edits_an_absolute_path() {
        let f = fixture();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "old").unwrap();
        let mut sandbox = f.sandbox.clone();
        sandbox.allow_external_paths = true;
        let ctx = ToolContext {
            workspace: &f.workspace,
            sandbox: &sandbox,
            progress: ToolProgressSink::null(),
        };

        EditTool
            .execute(
                json!({
                    "operation": "replace",
                    "path": outside.path().to_string_lossy(),
                    "old": "old",
                    "new": "new"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(outside.path()).unwrap(), "new");
    }

    #[tokio::test]
    async fn edit_rejects_escaping_paths() {
        let f = fixture();
        for op in [
            json!({"operation": "create", "path": "../evil.txt", "content": "x"}),
            json!({"operation": "replace", "path": "/etc/hosts", "old": "a", "new": "b"}),
            json!({"operation": "delete", "path": "../../etc/passwd"}),
        ] {
            let err = EditTool.execute(op, &f.ctx()).await.unwrap_err();
            assert!(
                err.message.contains("..") || err.message.contains("absolute"),
                "{err}"
            );
        }
    }
}
