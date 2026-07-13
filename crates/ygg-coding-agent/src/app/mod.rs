#![allow(missing_docs)]

pub mod bootstrap;

use std::path::PathBuf;
use ygg_agent::Agent;

use ygg_ai::{AiClient, Model, ModelCatalog, ModelId, ReasoningConfig};

use crate::config::Config;
use crate::session_store::SessionStore;

/// Mode-agnostic application state. TUI state and themes stay outside this type.
/// Label suitable for status and durable provenance entries.
pub fn reasoning_label(reasoning: &ReasoningConfig) -> String {
    match reasoning {
        ReasoningConfig::Off => "off".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Minimal) => "minimal".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Low) => "low".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Medium) => "medium".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High) => "high".to_owned(),
        ReasoningConfig::Budget(budget) => format!("budget={budget}"),
    }
}

/// An Agent-owning runtime transition. These are valid only while idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconfig {
    Model(ModelId),
    Thinking(ReasoningConfig),
    NewSession,
    Resume(PathBuf),
}

/// Apply one consuming configuration transition at an idle boundary.
pub fn apply_reconfig(app: App, reconfig: Reconfig) -> anyhow::Result<App> {
    match reconfig {
        Reconfig::Model(id) => {
            let model = app.catalog.resolve(&id)?;
            bootstrap::rebuild_app(app, Some(model), None, None)
        }
        Reconfig::Thinking(reasoning) => bootstrap::rebuild_app(app, None, Some(reasoning), None),
        Reconfig::NewSession => {
            let path = app.sessions.new_path(&crate::modes::timestamp());
            bootstrap::rebuild_app(
                app,
                None,
                None,
                Some(bootstrap::SessionSelection::CreateNew(path)),
            )
        }
        Reconfig::Resume(path) => bootstrap::rebuild_app(
            app,
            None,
            None,
            Some(bootstrap::SessionSelection::OpenExisting(path)),
        ),
    }
}

pub struct App {
    pub agent: Agent,
    pub model: Model,
    pub client: AiClient,
    pub config: Config,
    pub catalog: ModelCatalog,
    pub sessions: SessionStore,
    pub reasoning: ReasoningConfig,
    pub system: String,
    pub system_tokens: u64,
    pub tool_schema_tokens: u64,
}
