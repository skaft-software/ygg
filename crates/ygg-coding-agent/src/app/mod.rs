#![allow(missing_docs)]

pub mod bootstrap;

use std::path::PathBuf;

use ygg_agent::Agent;
use ygg_ai::{AiClient, Model, ModelCatalog, ModelId, ReasoningConfig, ReasoningControl};

use crate::config::Config;
use crate::config::ThinkingLevel;
use crate::session_store::SessionStore;

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

/// Translate a portable thinking selection to the target model's advertised
/// reasoning control mechanism.
pub fn thinking_to_reasoning(
    level: ThinkingLevel,
    model: &Model,
) -> anyhow::Result<ReasoningConfig> {
    let capability = match &model.spec.capabilities.reasoning {
        Some(capability) => capability,
        None if level == ThinkingLevel::Off => return Ok(ReasoningConfig::Off),
        None => anyhow::bail!("{} has no thinking support", model.spec.id.0),
    };
    if level == ThinkingLevel::Off {
        return Ok(ReasoningConfig::Off);
    }
    match capability.control {
        ReasoningControl::Effort => Ok(ReasoningConfig::Effort(level.to_effort())),
        ReasoningControl::TokenBudget => {
            let budgets = capability
                .effort_budgets
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{} has no reasoning budgets", model.spec.id.0))?;
            Ok(ReasoningConfig::Budget(level.pick_budget(budgets)))
        }
    }
}

/// Levels the current model can safely offer in the thinking picker.
pub fn supported_levels(model: &Model) -> Vec<ThinkingLevel> {
    if model.spec.capabilities.reasoning.is_some() {
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ]
    } else {
        vec![ThinkingLevel::Off]
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ygg_ai::{ReasoningCapability, ReasoningEffort, ReasoningEffortBudgets};

    fn model_with(capability: Option<ReasoningCapability>) -> Model {
        let catalog = ModelCatalog::builtin().unwrap();
        let base = catalog
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        let mut spec = (*base.spec).clone();
        spec.capabilities.reasoning = capability;
        Model {
            spec: Arc::new(spec),
            endpoint: base.endpoint,
        }
    }

    #[test]
    fn maps_effort_and_token_budget_thinking() {
        let effort = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: None,
        }));
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::High, &effort).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::High)
        );

        let budget = model_with(Some(ReasoningCapability {
            control: ReasoningControl::TokenBudget,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: Some(ReasoningEffortBudgets {
                minimal: 1024,
                low: 2048,
                medium: 4096,
                high: 8192,
            }),
        }));
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::High, &budget).unwrap(),
            ReasoningConfig::Budget(8192)
        );
    }

    #[test]
    fn unsupported_model_allows_only_off() {
        let model = model_with(None);
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Off, &model).unwrap(),
            ReasoningConfig::Off
        );
        assert!(thinking_to_reasoning(ThinkingLevel::High, &model).is_err());
        assert_eq!(supported_levels(&model), vec![ThinkingLevel::Off]);
    }
}
