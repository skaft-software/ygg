#![allow(missing_docs)]

pub mod bootstrap;

use ygg_agent::Agent;
use ygg_ai::{AiClient, Model, ModelCatalog, ReasoningConfig};

use crate::config::Config;
use crate::session_store::SessionStore;

/// Mode-agnostic application state. TUI state and themes stay outside this type.
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
