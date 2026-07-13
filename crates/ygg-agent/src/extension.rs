//! The extension spine: one trait, one registration boundary, two hooks.
//!
//! Deliberately small. There are no context providers, commands, UI widgets,
//! service containers, or lifecycle callbacks — those can be added as new
//! `ExtensionHost` methods later without breaking the [`Extension`] trait.

use std::sync::Arc;

use crate::events::AgentEvent;
use crate::tool::Tool;

/// Implemented by any extension the agent loads.
///
/// The core tools register through this same boundary (see
/// [`CoreTools`](crate::tools::CoreTools)); extensions are first-class from
/// day one.
pub trait Extension: Send + Sync {
    /// Registers the extension's tools and observers with the host.
    fn register(&self, host: &mut ExtensionHost);
}

/// Observes agent events without modifying them.
///
/// Observers are invoked synchronously, in registration order, immediately
/// before each event is delivered to the run's consumer.
pub trait EventObserver: Send + Sync {
    /// Called for every [`AgentEvent`] the run emits.
    fn on_event(&self, event: &AgentEvent);
}

/// Registry of tools and event observers, filled by extensions and consumed
/// by [`Agent::new`](crate::Agent::new).
#[derive(Default)]
pub struct ExtensionHost {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) observers: Vec<Arc<dyn EventObserver>>,
    pub(crate) duplicate_tools: Vec<String>,
}

impl ExtensionHost {
    /// Creates an empty host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool. Core tools use this same method.
    ///
    /// Duplicate tool names are rejected deterministically: the first
    /// registration wins, and [`Agent::new`](crate::Agent::new) fails with
    /// [`AgentError::DuplicateTool`](crate::AgentError::DuplicateTool) if any
    /// duplicate was registered.
    pub fn tool(&mut self, tool: impl Tool + 'static) {
        let name = tool.definition().name;
        if self.tools.iter().any(|t| t.definition().name == name) {
            self.duplicate_tools.push(name);
        } else {
            self.tools.push(Arc::new(tool));
        }
    }

    /// Registers an event observer.
    pub fn observe(&mut self, observer: impl EventObserver + 'static) {
        self.observers.push(Arc::new(observer));
    }

    /// Loads an extension by letting it register against this host.
    pub fn load(&mut self, extension: &dyn Extension) {
        extension.register(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolContext, ToolError, ToolOutput};
    use ygg_ai::ToolDef;

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: self.0.to_string(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::new("ok"))
        }
    }

    #[test]
    fn duplicate_tool_names_are_recorded() {
        let mut host = ExtensionHost::new();
        host.tool(NamedTool("read"));
        host.tool(NamedTool("search"));
        host.tool(NamedTool("read"));
        assert_eq!(host.tools.len(), 2);
        assert_eq!(host.duplicate_tools, vec!["read".to_string()]);
    }
}
