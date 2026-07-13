//! The built-in tools (`read`, `search`, `edit`, `exec`) and the
//! [`CoreTools`] extension that registers them.
//!
//! Core tools are not special: they implement the same [`Tool`](crate::Tool)
//! trait and register through the same [`ExtensionHost::tool`] method as any
//! third-party tool.

mod edit;
mod exec;
mod read;
mod search;

pub use edit::EditTool;
pub use exec::ExecTool;
pub use read::ReadTool;
pub use search::SearchTool;

use crate::extension::{Extension, ExtensionHost};
use crate::tool::ToolError;

/// Extension registering the four built-in tools through the public
/// registration boundary.
pub struct CoreTools;

impl Extension for CoreTools {
    fn register(&self, host: &mut ExtensionHost) {
        host.tool(ReadTool);
        host.tool(SearchTool);
        host.tool(EditTool);
        host.tool(ExecTool);
    }
}

/// Deserializes model-provided arguments into a typed argument struct,
/// converting schema mismatches into a clear tool error for the model.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::new(format!("invalid arguments: {e}")))
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
