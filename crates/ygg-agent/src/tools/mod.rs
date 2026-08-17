//! The built-in tools (`read`, `edit`, `write`, `bash`, `search`) and the [`CoreTools`]
//! extension that registers them.
//!
//! Core tools are not special: they implement the same [`Tool`](crate::Tool)
//! trait and register through the same [`ExtensionHost::tool`] method as any
//! third-party tool.

mod bash;
mod edit;
mod read;
mod search;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::ReadTool;
pub use search::SearchTool;
pub use write::WriteTool;

use crate::extension::{Extension, ExtensionHost};
use crate::tool::ToolError;

/// Hard cap for one file loaded by read/edit/write preview and conflict checks.
pub(crate) const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;

/// Hard cap for one local path spelling accepted at the tool boundary.
pub(crate) const MAX_TOOL_PATH_BYTES: usize = 32 * 1024;

/// Validate only lexical path properties before effect admission. This must not
/// query filesystem state: classification must remain deterministic and must
/// not leak host-path metadata before policy authorization.
pub(crate) fn validate_effect_path(
    path: &str,
    allow_external_paths: bool,
) -> Result<(), ToolError> {
    if path.is_empty() {
        return Err(ToolError::new(
            "invalid arguments: `path` must be non-empty",
        ));
    }
    if path.len() > MAX_TOOL_PATH_BYTES {
        return Err(ToolError::new(format!(
            "invalid arguments: `path` is {} bytes (limit {MAX_TOOL_PATH_BYTES})",
            path.len()
        )));
    }
    if path.as_bytes().contains(&0) {
        return Err(ToolError::new(
            "invalid arguments: `path` must not contain NUL",
        ));
    }
    if allow_external_paths {
        return Ok(());
    }

    let path_value = std::path::Path::new(path);
    if path_value.is_absolute()
        || path_value.components().any(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(ToolError::new(format!(
            "invalid arguments: absolute paths are not allowed: `{path}`"
        )));
    }
    if path_value
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(ToolError::new(format!(
            "invalid arguments: parent (`..`) path components are not allowed: `{path}`"
        )));
    }
    if matches!(path, "~") || path.starts_with("~/") || path.starts_with("~\\") {
        return Err(ToolError::new(format!(
            "invalid arguments: home-relative paths are not allowed: `{path}`"
        )));
    }
    Ok(())
}

pub(crate) fn validate_expected_hash(value: Option<&serde_json::Value>) -> Result<(), ToolError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(value) = value.as_str() else {
        return Err(ToolError::new(
            "invalid arguments: `expected_hash` must be a string",
        ));
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ToolError::new(
            "invalid arguments: `expected_hash` must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

/// Extension registering the built-in tools through the public registration
/// boundary.
pub struct CoreTools;

impl Extension for CoreTools {
    fn register(&self, host: &mut ExtensionHost) {
        host.tool(ReadTool);
        host.tool(EditTool);
        host.tool(WriteTool);
        host.tool(BashTool);
        // The coding product disables this redundant schema by default, while
        // keeping it available to embedders and explicit tool allowlists.
        host.tool(SearchTool);
    }
}

/// Deserializes model-provided arguments into a typed argument struct,
/// converting schema mismatches into a clear tool error for the model.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::new(format!("invalid arguments: {e}")))
}

const MAX_UNIFIED_DIFF_BYTES: usize = 16 * 1024;
const UNIFIED_DIFF_TRUNCATION_MARKER: &str =
    "\n... unified diff truncated; remaining content omitted ...\n";

struct UnifiedDiffWriter {
    output: String,
    truncated: bool,
}

impl UnifiedDiffWriter {
    fn new() -> Self {
        Self {
            output: String::with_capacity(MAX_UNIFIED_DIFF_BYTES),
            truncated: false,
        }
    }

    fn push_bounded(&mut self, value: &str) {
        if self.truncated {
            return;
        }
        if value.len() <= MAX_UNIFIED_DIFF_BYTES.saturating_sub(self.output.len()) {
            self.output.push_str(value);
            return;
        }

        let content_limit = MAX_UNIFIED_DIFF_BYTES - UNIFIED_DIFF_TRUNCATION_MARKER.len();
        truncate_utf8(&mut self.output, content_limit);
        let mut keep = content_limit
            .saturating_sub(self.output.len())
            .min(value.len());
        while keep > 0 && !value.is_char_boundary(keep) {
            keep -= 1;
        }
        self.output.push_str(&value[..keep]);
        self.output.push_str(UNIFIED_DIFF_TRUNCATION_MARKER);
        self.truncated = true;
    }

    fn push_line(&mut self, prefix: &str, line: &str) -> bool {
        self.push_bounded(prefix);
        self.push_bounded(line);
        self.push_bounded("\n");
        !self.truncated
    }

    fn finish(self) -> String {
        self.output
    }
}

impl std::fmt::Write for UnifiedDiffWriter {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.push_bounded(value);
        Ok(())
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    let mut keep = value.len().min(max_bytes);
    while keep > 0 && !value.is_char_boundary(keep) {
        keep -= 1;
    }
    value.truncate(keep);
}

/// Build a minimal, bounded unified diff showing the replacement with
/// surrounding context lines so the rendered output is scannable at a glance.
/// Hunk counts always describe the complete replacement, even when the body is
/// truncated to the normal tool-output budget.
pub(crate) fn format_unified_diff(path: &str, old: &str, new: &str, full_text: &str) -> String {
    use std::fmt::Write as _;

    let match_offset = full_text.find(old).unwrap_or(0);
    let change_line = full_text[..match_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    // Do not collect every line here. A valid 32 MiB file made entirely of
    // newlines would otherwise allocate hundreds of MiB just to retain
    // borrowed line slices while constructing a three-line context window.
    let full_line_count = full_text.lines().count();
    let old_count = old.lines().count();
    let new_count = new.lines().count();
    let context_start = change_line.saturating_sub(3);
    let context_before_count = change_line
        .min(full_line_count)
        .saturating_sub(context_start);
    let after_start = change_line.saturating_add(old_count).min(full_line_count);
    let after_end = after_start.saturating_add(3).min(full_line_count);
    let context_after_count = after_end.saturating_sub(after_start);
    let hunk_start = context_start + 1;
    let old_hunk_count = context_before_count + old_count + context_after_count;
    let new_hunk_count = context_before_count + new_count + context_after_count;

    let mut diff = UnifiedDiffWriter::new();
    write!(
        diff,
        "--- a/{path}\n+++ b/{path}\n@@ -{hunk_start},{old_hunk_count} +{hunk_start},{new_hunk_count} @@\n"
    )
    .expect("bounded unified-diff formatting cannot fail");
    if diff.truncated {
        return diff.finish();
    }

    for line in full_text
        .lines()
        .skip(context_start)
        .take(context_before_count)
    {
        if !diff.push_line(" ", line) {
            return diff.finish();
        }
    }
    for line in old.lines() {
        if !diff.push_line("-", line) {
            return diff.finish();
        }
    }
    for line in new.lines() {
        if !diff.push_line("+", line) {
            return diff.finish();
        }
    }
    for line in full_text
        .lines()
        .skip(after_start)
        .take(context_after_count)
    {
        if !diff.push_line(" ", line) {
            return diff.finish();
        }
    }
    diff.finish()
}

/// Build the bounded creation form of a unified diff. At most ten content
/// lines are previewed, matching the historical output without ever copying a
/// payload-sized line into an intermediate or final string.
pub(crate) fn format_unified_creation_diff(path: &str, content: &str) -> String {
    use std::fmt::Write as _;

    let total = content.lines().count();
    let mut diff = UnifiedDiffWriter::new();
    write!(diff, "--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{total} @@\n")
        .expect("bounded unified-diff formatting cannot fail");
    if diff.truncated {
        return diff.finish();
    }

    let mut shown = 0usize;
    for line in content.lines().take(10) {
        shown += 1;
        if !diff.push_line("+", line) {
            return diff.finish();
        }
    }
    if total > shown {
        write!(
            diff,
            "… {} more line{}\n",
            total - shown,
            if total - shown == 1 { "" } else { "s" }
        )
        .expect("bounded unified-diff formatting cannot fail");
    }
    diff.finish()
}

#[cfg(test)]
mod unified_diff_tests {
    use super::*;

    #[test]
    fn small_unified_diff_output_is_unchanged() {
        let diff = format_unified_diff("file.txt", "old", "new", "before\nold\nafter\n");
        assert_eq!(
            diff,
            "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n before\n-old\n+new\n after\n"
        );
    }

    #[test]
    fn small_creation_diff_output_is_unchanged() {
        let content = (1..=11)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = format_unified_creation_diff("file.txt", &content);
        assert_eq!(
            diff,
            "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,11 @@\n+line-1\n+line-2\n+line-3\n+line-4\n+line-5\n+line-6\n+line-7\n+line-8\n+line-9\n+line-10\n… 1 more line\n"
        );
    }

    #[test]
    fn multi_megabyte_single_line_diffs_are_bounded_and_utf8_safe() {
        let old = "🙂".repeat(600_000);
        let new = "界".repeat(800_000);

        let replacement = format_unified_diff("large.txt", &old, &new, &old);
        let creation = format_unified_creation_diff("large.txt", &new);
        assert!(replacement.contains("@@ -1,1 +1,1 @@"), "{replacement}");
        assert!(creation.contains("@@ -0,0 +1,1 @@"), "{creation}");

        for diff in [replacement, creation] {
            assert!(diff.len() <= MAX_UNIFIED_DIFF_BYTES, "{}", diff.len());
            assert!(std::str::from_utf8(diff.as_bytes()).is_ok());
            assert!(
                diff.contains(UNIFIED_DIFF_TRUNCATION_MARKER.trim()),
                "{diff}"
            );
        }
    }
}

/// Truncates a display line to `max` characters, appending an ellipsis when cut.
pub(crate) fn clip_line(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let clipped: String = line.chars().take(max).collect();
        format!("{clipped}…")
    }
}
