#![allow(missing_docs)]

pub mod bootstrap;

use std::path::PathBuf;
use std::sync::Arc;

use ygg_agent::Agent;
use ygg_ai::{
    AiClient, Model, ModelCatalog, ModelId, ReasoningConfig, ReasoningControl, ReasoningEffort,
};

use crate::config::Config;
use crate::config::ThinkingLevel;
use crate::prompts::PromptRegistry;
use crate::session_store::SessionStore;

/// Label suitable for status and durable provenance entries.
pub fn reasoning_label(reasoning: &ReasoningConfig) -> String {
    match reasoning {
        ReasoningConfig::Off => "off".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Minimal) => "minimal".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Low) => "low".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Medium) => "medium".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High) => "high".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Xhigh) => "xhigh".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Max) => "max".to_owned(),
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
        None => {
            // Model doesn't support thinking — fall back to Off rather than
            // crashing, so a stale persisted thinking config doesn't lock
            // the user out after switching to a simpler model.
            return Ok(ReasoningConfig::Off);
        }
    };
    if level == ThinkingLevel::Off {
        return Ok(ReasoningConfig::Off);
    }
    // Clamp the requested tier down to the model's advertised ceiling so we
    // never emit an effort the backend would reject (mirrors pi's
    // `clampThinkingLevel`).  Also raise it to the model's floor: a request
    // below what the model distinguishes is silently upgraded rather than
    // rejected.
    let effort = raise_effort(
        clamp_effort(level.to_effort(), capability.max_effort),
        capability.min_effort,
    );
    match capability.control {
        ReasoningControl::Effort => Ok(ReasoningConfig::Effort(effort)),
        ReasoningControl::TokenBudget => {
            let budgets = capability
                .effort_budgets
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{} has no reasoning budgets", model.spec.id.0))?;
            Ok(ReasoningConfig::Budget(
                effort_level(effort).pick_budget(budgets),
            ))
        }
    }
}

/// Clamp a requested effort down to the model's highest supported tier.
fn clamp_effort(effort: ReasoningEffort, ceiling: ReasoningEffort) -> ReasoningEffort {
    effort.min(ceiling)
}

/// Raise a requested effort up to the model's lowest meaningfully distinct tier.
fn raise_effort(effort: ReasoningEffort, floor: ReasoningEffort) -> ReasoningEffort {
    effort.max(floor)
}

fn effort_level(effort: ReasoningEffort) -> ThinkingLevel {
    match effort {
        ReasoningEffort::Minimal => ThinkingLevel::Minimal,
        ReasoningEffort::Low => ThinkingLevel::Low,
        ReasoningEffort::Medium => ThinkingLevel::Medium,
        ReasoningEffort::High => ThinkingLevel::High,
        ReasoningEffort::Xhigh => ThinkingLevel::Xhigh,
        ReasoningEffort::Max => ThinkingLevel::Max,
    }
}

/// Normalize a CLI/config reasoning selection against the resolved model.
pub fn normalize_reasoning_for_model(
    reasoning: &ReasoningConfig,
    model: &Model,
) -> anyhow::Result<ReasoningConfig> {
    match reasoning {
        ReasoningConfig::Off => Ok(ReasoningConfig::Off),
        ReasoningConfig::Effort(effort) => thinking_to_reasoning(effort_level(*effort), model),
        ReasoningConfig::Budget(budget) => match &model.spec.capabilities.reasoning {
            Some(capability) if capability.control == ReasoningControl::TokenBudget => {
                if *budget < 1024 || *budget > model.spec.limits.max_output_tokens {
                    anyhow::bail!(
                        "reasoning budget {budget} must be between 1024 and {} for {}",
                        model.spec.limits.max_output_tokens,
                        model.spec.id.0
                    );
                }
                Ok(ReasoningConfig::Budget(*budget))
            }
            Some(_) => anyhow::bail!(
                "{} uses effort-based thinking; use --reasoning high/medium/low/minimal instead of budget={budget}",
                model.spec.id.0
            ),
            None => {
                // Model doesn't support thinking — fall back to Off.
                Ok(ReasoningConfig::Off)
            }
        },
    }
}

/// Convert a current model-specific reasoning setting back to a portable level
/// before switching models. Custom token budgets cannot be safely translated.
pub fn level_from_reasoning(
    reasoning: &ReasoningConfig,
    model: &Model,
) -> anyhow::Result<ThinkingLevel> {
    match reasoning {
        ReasoningConfig::Off => Ok(ThinkingLevel::Off),
        ReasoningConfig::Effort(effort) => Ok(effort_level(*effort)),
        ReasoningConfig::Budget(budget) => {
            let Some(capability) = &model.spec.capabilities.reasoning else {
                // Model doesn't support thinking — fall back to Off.
                return Ok(ThinkingLevel::Off);
            };
            let Some(budgets) = capability.effort_budgets else {
                anyhow::bail!("{} has no portable thinking budgets", model.spec.id.0);
            };
            match *budget {
                value if value == budgets.minimal => Ok(ThinkingLevel::Minimal),
                value if value == budgets.low => Ok(ThinkingLevel::Low),
                value if value == budgets.medium => Ok(ThinkingLevel::Medium),
                value if value == budgets.high => Ok(ThinkingLevel::High),
                value if value == budgets.xhigh => Ok(ThinkingLevel::Xhigh),
                value if value == budgets.max => Ok(ThinkingLevel::Max),
                _ => anyhow::bail!(
                    "budget={budget} cannot be translated while switching models; choose /thinking explicitly"
                ),
            }
        }
    }
}

/// Levels the current model can safely offer in the thinking picker. `xhigh`
/// and `max` appear only when the model advertises them via `max_effort`.
pub fn supported_levels(model: &Model) -> Vec<ThinkingLevel> {
    let Some(capability) = &model.spec.capabilities.reasoning else {
        return vec![ThinkingLevel::Off];
    };
    let mut levels = vec![ThinkingLevel::Off];
    levels.extend(
        [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ]
        .into_iter()
        .filter(|level| {
            let effort = level.to_effort();
            effort >= capability.min_effort && effort <= capability.max_effort
        }),
    );
    levels
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
    pub skills: Arc<dyn ygg_agent::skills::SkillRegistry>,
    pub prompts: Arc<PromptRegistry>,
    pub executable_extensions: crate::extensions::ExecutableExtensions,
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
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ygg_ai::ReasoningEffort::Minimal,
            max_effort: ygg_ai::ReasoningEffort::Max,
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
                xhigh: 16384,
                max: 32768,
            }),
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ygg_ai::ReasoningEffort::Minimal,
            max_effort: ygg_ai::ReasoningEffort::Max,
        }));
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::High, &budget).unwrap(),
            ReasoningConfig::Budget(8192)
        );
        assert_eq!(
            normalize_reasoning_for_model(&ReasoningConfig::Effort(ReasoningEffort::High), &budget)
                .unwrap(),
            ReasoningConfig::Budget(8192)
        );
        assert!(normalize_reasoning_for_model(&ReasoningConfig::Budget(2048), &effort).is_err());
    }

    fn effort_model(max_effort: ReasoningEffort) -> Model {
        model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: None,
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ReasoningEffort::Minimal,
            max_effort,
        }))
    }

    #[test]
    fn clamps_effort_to_model_ceiling() {
        // A High-ceiling model clamps a Max request down to High.
        let high = effort_model(ReasoningEffort::High);
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Max, &high).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::High)
        );

        // A Max-ceiling model passes Max and Xhigh through unchanged.
        let max = effort_model(ReasoningEffort::Max);
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Max, &max).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Max)
        );
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Xhigh, &max).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Xhigh)
        );
    }

    #[test]
    fn supported_levels_gate_on_ceiling() {
        let high = effort_model(ReasoningEffort::High);
        assert!(!supported_levels(&high).contains(&ThinkingLevel::Xhigh));
        assert!(!supported_levels(&high).contains(&ThinkingLevel::Max));

        let xhigh = effort_model(ReasoningEffort::Xhigh);
        assert!(supported_levels(&xhigh).contains(&ThinkingLevel::Xhigh));
        assert!(!supported_levels(&xhigh).contains(&ThinkingLevel::Max));

        let max = effort_model(ReasoningEffort::Max);
        assert!(supported_levels(&max).contains(&ThinkingLevel::Xhigh));
        assert!(supported_levels(&max).contains(&ThinkingLevel::Max));
    }

    #[test]
    fn supported_levels_respect_the_model_floor() {
        let mut model = effort_model(ReasoningEffort::Max);
        let mut spec = (*model.spec).clone();
        spec.capabilities.reasoning.as_mut().unwrap().min_effort = ReasoningEffort::Medium;
        model.spec = Arc::new(spec);

        assert_eq!(
            supported_levels(&model),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
                ThinkingLevel::Max,
            ]
        );
    }

    #[test]
    fn token_budget_maps_xhigh_and_max() {
        let budget = model_with(Some(ReasoningCapability {
            control: ReasoningControl::TokenBudget,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: Some(ReasoningEffortBudgets {
                minimal: 1024,
                low: 2048,
                medium: 4096,
                high: 8192,
                xhigh: 16384,
                max: 32768,
            }),
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ReasoningEffort::Minimal,
            max_effort: ReasoningEffort::Max,
        }));
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Xhigh, &budget).unwrap(),
            ReasoningConfig::Budget(16384)
        );
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Max, &budget).unwrap(),
            ReasoningConfig::Budget(32768)
        );
    }

    #[test]
    fn unsupported_model_allows_only_off() {
        let model = model_with(None);
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Off, &model).unwrap(),
            ReasoningConfig::Off
        );
        // When a model lacks thinking support, all levels silently fall back
        // to Off rather than crashing, so a stale persisted thinking config
        // doesn't lock the user out after switching models.
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::High, &model).unwrap(),
            ReasoningConfig::Off
        );
        assert_eq!(supported_levels(&model), vec![ThinkingLevel::Off]);
    }
}
