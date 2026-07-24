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

/// Build a minimal unified diff showing the replacement with surrounding
/// context lines so the rendered output is scannable at a glance.
pub(crate) fn format_unified_diff(path: &str, old: &str, new: &str, full_text: &str) -> String {
    let match_offset = full_text.find(old).unwrap_or(0);
    let change_line = full_text[..match_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let full_lines = full_text.lines().collect::<Vec<_>>();
    let old_count = old.lines().count();
    let new_count = new.lines().count();
    let context_start = change_line.saturating_sub(3);
    let context_before_count = change_line.saturating_sub(context_start);
    let after_start = change_line.saturating_add(old_count).min(full_lines.len());
    let after_end = after_start.saturating_add(3).min(full_lines.len());
    let context_after_count = after_end.saturating_sub(after_start);
    let hunk_start = context_start + 1;
    let old_hunk_count = context_before_count + old_count + context_after_count;
    let new_hunk_count = context_before_count + new_count + context_after_count;

    let mut diff = String::new();
    diff.push_str(&format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{hunk_start},{old_hunk_count} +{hunk_start},{new_hunk_count} @@\n"
    ));

    for line in &full_lines[context_start..change_line.min(full_lines.len())] {
        diff.push_str(&format!(" {line}\n"));
    }
    for line in old.lines() {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in new.lines() {
        diff.push_str(&format!("+{line}\n"));
    }
    for line in &full_lines[after_start..after_end] {
        diff.push_str(&format!(" {line}\n"));
    }
    diff
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
