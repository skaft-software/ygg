#![allow(missing_docs)]

pub mod bootstrap;

use std::path::PathBuf;
use std::sync::Arc;

use ygg_agent::{Agent, DurableGoalStore, GoalDriver};
use ygg_ai::{
    AgentDelegation, AiClient, Model, ModelCatalog, ModelId, OpenAiChatReasoningMode,
    ReasoningConfig, ReasoningControl, ReasoningEffort, ReasoningMode,
};

use crate::config::Config;
use crate::config::ThinkingLevel;
use crate::extensions::SUBAGENTS_EXTENSION_NAME;
use crate::prompts::PromptRegistry;
use crate::session_store::SessionStore;

/// Label suitable for status and durable provenance entries.
pub fn reasoning_label(reasoning: &ReasoningConfig) -> String {
    match reasoning {
        ReasoningConfig::Off => "off".to_owned(),
        ReasoningConfig::On => "on".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Minimal) => "minimal".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Low) => "low".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Medium) => "medium".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High) => "high".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Xhigh) => "xhigh".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Max) => "max".to_owned(),
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Ultra) => "ultra".to_owned(),
        ReasoningConfig::Budget(budget) => format!("budget={budget}"),
    }
}

/// Whether the selected route can provide complete Ultra semantics. Ultra is
/// more than a wire effort: it also requires the host-side V2 collaboration
/// runtime advertised by model metadata.
pub fn model_supports_ultra(model: &Model) -> bool {
    model
        .spec
        .capabilities
        .reasoning
        .as_ref()
        .is_some_and(|capability| {
            capability.control == ReasoningControl::Effort
                && capability.max_effort >= ReasoningEffort::Ultra
        })
        && model
            .spec
            .capabilities
            .agent_delegation
            .is_some_and(|version| {
                version == AgentDelegation::V2 && ygg_agent::delegation_runtime_supports(version)
            })
}

fn reasoning_effort_ceiling(model: &Model, advertised: ReasoningEffort) -> ReasoningEffort {
    if advertised == ReasoningEffort::Ultra && !model_supports_ultra(model) {
        ReasoningEffort::Max
    } else {
        advertised
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
    if capability.control == ReasoningControl::AlwaysOn {
        return Ok(ReasoningConfig::On);
    }
    if level == ThinkingLevel::Off {
        return Ok(ReasoningConfig::Off);
    }
    if capability.control == ReasoningControl::Toggle {
        return Ok(ReasoningConfig::On);
    }
    // Clamp the requested tier down to the model's advertised ceiling so we
    // never emit an effort the backend would reject (mirrors pi's
    // `clampThinkingLevel`).  Also raise it to the model's floor: a request
    // below what the model distinguishes is silently upgraded rather than
    // rejected.
    let requested = if level == ThinkingLevel::On {
        capability.min_effort
    } else {
        level.to_effort()
    };
    let ceiling = reasoning_effort_ceiling(model, capability.max_effort);
    let effort = clamp_effort(raise_effort(requested, capability.min_effort), ceiling);
    let effort = match &capability.openai_chat_mode {
        OpenAiChatReasoningMode::ProviderValues { values, .. }
            if capability.control == ReasoningControl::Effort =>
        {
            let supported = values
                .iter()
                .filter_map(|value| match value.trim().to_ascii_lowercase().as_str() {
                    "minimal" | "min" => Some(ReasoningEffort::Minimal),
                    "low" => Some(ReasoningEffort::Low),
                    "medium" | "med" => Some(ReasoningEffort::Medium),
                    "high" => Some(ReasoningEffort::High),
                    "xhigh" | "x-high" | "extra_high" => Some(ReasoningEffort::Xhigh),
                    "max" => Some(ReasoningEffort::Max),
                    "ultra" => Some(ReasoningEffort::Ultra),
                    _ => None,
                })
                .filter(|supported| *supported <= ceiling)
                .collect::<Vec<_>>();
            supported
                .iter()
                .copied()
                .filter(|supported| *supported <= effort)
                .max()
                .or_else(|| supported.iter().copied().min())
                .unwrap_or(effort)
        }
        _ => effort,
    };
    match capability.control {
        ReasoningControl::AlwaysOn => Ok(ReasoningConfig::On),
        ReasoningControl::Effort => Ok(ReasoningConfig::Effort(effort)),
        ReasoningControl::Toggle => unreachable!("toggle handled above"),
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

/// Translate a thinking selection while enforcing the product's subagent
/// observability boundary.
pub fn thinking_to_reasoning_with_subagents(
    level: ThinkingLevel,
    model: &Model,
    subagents_available: bool,
) -> anyhow::Result<ReasoningConfig> {
    let reasoning = thinking_to_reasoning(level, model)?;
    if !subagents_available && reasoning == ReasoningConfig::Effort(ReasoningEffort::Ultra) {
        let fallback = thinking_to_reasoning(ThinkingLevel::Max, model)?;
        return Ok(
            if fallback != ReasoningConfig::Effort(ReasoningEffort::Ultra) {
                fallback
            } else {
                ReasoningConfig::Off
            },
        );
    }
    Ok(reasoning)
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
        ReasoningEffort::Ultra => ThinkingLevel::Ultra,
    }
}

/// Normalize a CLI/config reasoning selection against the resolved model.
pub fn normalize_reasoning_for_model(
    reasoning: &ReasoningConfig,
    model: &Model,
) -> anyhow::Result<ReasoningConfig> {
    if model
        .spec
        .capabilities
        .reasoning
        .as_ref()
        .is_some_and(|capability| capability.control == ReasoningControl::AlwaysOn)
    {
        return Ok(ReasoningConfig::On);
    }
    match reasoning {
        ReasoningConfig::Off => Ok(ReasoningConfig::Off),
        ReasoningConfig::On => thinking_to_reasoning(ThinkingLevel::On, model),
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

/// Migrate the obsolete Pro execution bit at the product boundary.
///
/// Persisted Pro selections become Ultra only when live metadata advertises
/// both Ultra effort and V2 delegation. Otherwise the wire mode is removed and
/// the independently selected effort is normalized normally.
#[allow(dead_code)]
pub fn normalize_reasoning_selection_for_model(
    reasoning: &ReasoningConfig,
    mode: ReasoningMode,
    model: &Model,
) -> anyhow::Result<(ReasoningConfig, ReasoningMode, Option<String>)> {
    normalize_reasoning_selection_for_model_with_subagents(reasoning, mode, model, true)
}

/// Normalize reasoning while enforcing the product's subagent observability
/// boundary. Ultra is not a safe standalone model tier: the active first-party
/// subagents extension must provide the owner-bound observation surface before
/// Ygg can select it.
pub fn normalize_reasoning_selection_for_model_with_subagents(
    reasoning: &ReasoningConfig,
    mode: ReasoningMode,
    model: &Model,
    subagents_available: bool,
) -> anyhow::Result<(ReasoningConfig, ReasoningMode, Option<String>)> {
    let normalized = normalize_reasoning_for_model(reasoning, model)?;
    if !subagents_available
        && (normalized == ReasoningConfig::Effort(ReasoningEffort::Ultra)
            || (mode == ReasoningMode::Pro && model_supports_ultra(model)))
    {
        let fallback =
            normalize_reasoning_for_model(&ReasoningConfig::Effort(ReasoningEffort::Max), model)?;
        let fallback = if fallback != ReasoningConfig::Effort(ReasoningEffort::Ultra) {
            fallback
        } else {
            ReasoningConfig::Off
        };
        return Ok((
            fallback,
            ReasoningMode::Standard,
            Some(format!(
                "Ultra is disabled until the trusted {SUBAGENTS_EXTENSION_NAME} extension is active; using standard reasoning for {}",
                model.spec.id.0
            )),
        ));
    }
    if mode == ReasoningMode::Standard {
        return Ok((normalized, ReasoningMode::Standard, None));
    }

    if model_supports_ultra(model) {
        return Ok((
            ReasoningConfig::Effort(ReasoningEffort::Ultra),
            ReasoningMode::Standard,
            Some(format!(
                "legacy reasoning_mode=pro migrated to reasoning=ultra with V2 delegation for {}",
                model.spec.id.0
            )),
        ));
    }

    Ok((
        normalized.clone(),
        ReasoningMode::Standard,
        Some(format!(
            "legacy reasoning_mode=pro is obsolete; {} does not advertise Ultra with V2 delegation, using standard mode with reasoning={}",
            model.spec.id.0,
            reasoning_label(&normalized)
        )),
    ))
}

/// Convert a current model-specific reasoning setting back to a portable level
/// before switching models. Custom token budgets cannot be safely translated.
pub fn level_from_reasoning(
    reasoning: &ReasoningConfig,
    model: &Model,
) -> anyhow::Result<ThinkingLevel> {
    match reasoning {
        ReasoningConfig::Off
            if model
                .spec
                .capabilities
                .reasoning
                .as_ref()
                .is_some_and(|capability| capability.control == ReasoningControl::AlwaysOn) =>
        {
            Ok(ThinkingLevel::On)
        }
        ReasoningConfig::Off => Ok(ThinkingLevel::Off),
        ReasoningConfig::On => Ok(ThinkingLevel::On),
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

fn supported_levels_for_model(model: &Model) -> Vec<ThinkingLevel> {
    let Some(capability) = &model.spec.capabilities.reasoning else {
        return vec![ThinkingLevel::Off];
    };
    if capability.control == ReasoningControl::AlwaysOn {
        return vec![ThinkingLevel::On];
    }
    if let OpenAiChatReasoningMode::ProviderValues { values, .. } = &capability.openai_chat_mode {
        let mut levels = Vec::new();
        for value in values {
            let level = match value.trim().to_ascii_lowercase().as_str() {
                "none" | "off" | "disabled" => Some(ThinkingLevel::Off),
                "default" | "on" | "enabled" => Some(ThinkingLevel::On),
                "minimal" | "min" => Some(ThinkingLevel::Minimal),
                "low" => Some(ThinkingLevel::Low),
                "medium" | "med" => Some(ThinkingLevel::Medium),
                "high" => Some(ThinkingLevel::High),
                "xhigh" | "x-high" | "extra_high" => Some(ThinkingLevel::Xhigh),
                "max" => Some(ThinkingLevel::Max),
                "ultra" if model_supports_ultra(model) => Some(ThinkingLevel::Ultra),
                _ => None,
            };
            let level = match (capability.control, level) {
                (ReasoningControl::Toggle, Some(ThinkingLevel::Off | ThinkingLevel::On)) => level,
                (ReasoningControl::Effort, Some(level)) if !matches!(level, ThinkingLevel::On) => {
                    Some(level)
                }
                _ => None,
            };
            if let Some(level) = level.filter(|level| !levels.contains(level)) {
                levels.push(level);
            }
        }
        if !levels.is_empty() {
            return levels;
        }
    }
    if capability.control == ReasoningControl::Toggle {
        return vec![ThinkingLevel::Off, ThinkingLevel::On];
    }
    let mut levels = vec![ThinkingLevel::Off];
    levels.extend(
        [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
            ThinkingLevel::Ultra,
        ]
        .into_iter()
        .filter(|level| {
            let effort = level.to_effort();
            effort >= capability.min_effort
                && effort <= reasoning_effort_ceiling(model, capability.max_effort)
        }),
    );
    levels
}

/// Returns the model's portable thinking levels after applying the product's
/// subagent observability gate.
pub fn supported_levels_with_subagents(
    model: &Model,
    subagents_available: bool,
) -> Vec<ThinkingLevel> {
    supported_levels_for_model(model)
        .into_iter()
        .filter(|level| *level != ThinkingLevel::Ultra || subagents_available)
        .collect()
}

/// An Agent-owning runtime transition. These are valid only while idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconfig {
    Model(ModelId),
    Thinking(ReasoningConfig),
    ThinkingMode {
        mode: ReasoningMode,
        reasoning: ReasoningConfig,
    },
    NewSession,
    Resume(PathBuf),
}

/// Apply one consuming configuration transition at an idle boundary.
pub fn apply_reconfig(app: App, reconfig: Reconfig) -> anyhow::Result<App> {
    match reconfig {
        Reconfig::Model(id) => {
            let model = app.catalog.resolve(&id)?;
            bootstrap::rebuild_app(app, Some(model), None, None, None)
        }
        Reconfig::Thinking(reasoning) => {
            bootstrap::rebuild_app(app, None, Some(reasoning), None, None)
        }
        Reconfig::ThinkingMode { mode, reasoning } => {
            bootstrap::rebuild_app(app, None, Some(reasoning), Some(mode), None)
        }
        Reconfig::NewSession => {
            let path = app.sessions.new_path(&crate::modes::timestamp());
            bootstrap::rebuild_app(
                app,
                None,
                None,
                None,
                Some(bootstrap::SessionSelection::CreateNew(path)),
            )
        }
        Reconfig::Resume(path) => bootstrap::rebuild_app(
            app,
            None,
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
    pub reasoning_mode: ReasoningMode,
    pub system: String,
    pub system_tokens: u64,
    pub skills: Arc<dyn ygg_agent::skills::SkillRegistry>,
    pub prompts: Arc<PromptRegistry>,
    pub executable_extensions: crate::extensions::ExecutableExtensions,
    pub goal_store: Arc<DurableGoalStore>,
    pub goal_driver: GoalDriver,
    pub goal_session_id: String,
}

impl App {
    /// Whether this application has the owner-bound subagent observer needed
    /// before Ultra may be selected or submitted.
    pub fn subagents_available(&self) -> bool {
        self.model.spec.capabilities.tools
            && self
                .agent
                .registered_tool_names()
                .iter()
                .any(|name| name == "subagent_spawn")
            && self.executable_extensions.has_agent_session_service()
    }

    /// Current provider-visible tool schema reserve, including live extension
    /// catalog changes published after application bootstrap.
    pub fn current_tool_schema_tokens(&self) -> u64 {
        crate::app::bootstrap::tool_schema_reserve(&self.agent.registered_tool_definitions())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // The catalog is disposable: shutdown must never fail because this
        // best-effort projection could not be refreshed.
        let _ = self
            .sessions
            .refresh_catalog_for_open_session(self.agent.session());
    }
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
    fn legacy_pro_migrates_only_when_ultra_and_v2_are_advertised() {
        let unsupported = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: true,
            effort_budgets: None,
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ReasoningEffort::Minimal,
            max_effort: ReasoningEffort::Ultra,
        }));
        let (reasoning, mode, diagnostic) = normalize_reasoning_selection_for_model(
            &ReasoningConfig::Effort(ReasoningEffort::Max),
            ReasoningMode::Pro,
            &unsupported,
        )
        .unwrap();
        assert_eq!(reasoning, ReasoningConfig::Effort(ReasoningEffort::Max));
        assert_eq!(mode, ReasoningMode::Standard);
        assert!(diagnostic
            .unwrap()
            .contains("does not advertise Ultra with V2"));

        let mut spec = (*unsupported.spec).clone();
        spec.capabilities.agent_delegation = Some(AgentDelegation::V2);
        let supported = Model {
            spec: Arc::new(spec),
            endpoint: unsupported.endpoint,
        };
        let (reasoning, mode, diagnostic) = normalize_reasoning_selection_for_model(
            &ReasoningConfig::Effort(ReasoningEffort::Max),
            ReasoningMode::Pro,
            &supported,
        )
        .unwrap();
        assert_eq!(reasoning, ReasoningConfig::Effort(ReasoningEffort::Ultra));
        assert_eq!(mode, ReasoningMode::Standard);
        assert!(diagnostic.unwrap().contains("migrated"));
    }

    #[test]
    fn ultra_requires_the_observing_subagents_extension() {
        let base = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: true,
            effort_budgets: None,
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ReasoningEffort::Minimal,
            max_effort: ReasoningEffort::Ultra,
        }));
        let mut spec = (*base.spec).clone();
        spec.capabilities.agent_delegation = Some(AgentDelegation::V2);
        let model = Model {
            spec: Arc::new(spec),
            endpoint: base.endpoint,
        };
        assert!(!supported_levels_with_subagents(&model, false).contains(&ThinkingLevel::Ultra));
        assert!(supported_levels_with_subagents(&model, true).contains(&ThinkingLevel::Ultra));

        let (reasoning, mode, diagnostic) = normalize_reasoning_selection_for_model_with_subagents(
            &ReasoningConfig::Effort(ReasoningEffort::Ultra),
            ReasoningMode::Standard,
            &model,
            false,
        )
        .unwrap();
        assert_eq!(reasoning, ReasoningConfig::Effort(ReasoningEffort::Max));
        assert_eq!(mode, ReasoningMode::Standard);
        assert!(diagnostic.unwrap().contains("ygg-subagents"));

        let (reasoning, _, _) = normalize_reasoning_selection_for_model_with_subagents(
            &ReasoningConfig::Effort(ReasoningEffort::Ultra),
            ReasoningMode::Standard,
            &model,
            true,
        )
        .unwrap();
        assert_eq!(reasoning, ReasoningConfig::Effort(ReasoningEffort::Ultra));
    }
    #[test]
    fn ultra_floor_cannot_override_the_effective_runtime_ceiling() {
        let model = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: true,
            effort_budgets: None,
            openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
            min_effort: ReasoningEffort::Ultra,
            max_effort: ReasoningEffort::Ultra,
        }));

        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Ultra, &model).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Max)
        );

        let provider_values = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: true,
            effort_budgets: None,
            openai_chat_mode: OpenAiChatReasoningMode::ProviderValues {
                values: vec!["ultra".into()],
                default: Some("ultra".into()),
                system_message: true,
            },
            min_effort: ReasoningEffort::Ultra,
            max_effort: ReasoningEffort::Ultra,
        }));
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Ultra, &provider_values).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Max)
        );
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
        assert!(!supported_levels_with_subagents(&high, false).contains(&ThinkingLevel::Xhigh));
        assert!(!supported_levels_with_subagents(&high, false).contains(&ThinkingLevel::Max));

        let xhigh = effort_model(ReasoningEffort::Xhigh);
        assert!(supported_levels_with_subagents(&xhigh, false).contains(&ThinkingLevel::Xhigh));
        assert!(!supported_levels_with_subagents(&xhigh, false).contains(&ThinkingLevel::Max));

        let max = effort_model(ReasoningEffort::Max);
        assert!(supported_levels_with_subagents(&max, false).contains(&ThinkingLevel::Xhigh));
        assert!(supported_levels_with_subagents(&max, false).contains(&ThinkingLevel::Max));
    }

    #[test]
    fn supported_levels_respect_the_model_floor() {
        let mut model = effort_model(ReasoningEffort::Max);
        let mut spec = (*model.spec).clone();
        spec.capabilities.reasoning.as_mut().unwrap().min_effort = ReasoningEffort::Medium;
        model.spec = Arc::new(spec);

        assert_eq!(
            supported_levels_with_subagents(&model, false),
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
    fn provider_reported_toggle_and_effort_values_are_exact() {
        let toggle = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Toggle,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: None,
            openai_chat_mode: OpenAiChatReasoningMode::ProviderValues {
                values: vec!["none".into(), "default".into()],
                default: Some("default".into()),
                system_message: true,
            },
            min_effort: ReasoningEffort::Minimal,
            max_effort: ReasoningEffort::High,
        }));
        assert_eq!(
            supported_levels_with_subagents(&toggle, false),
            vec![ThinkingLevel::Off, ThinkingLevel::On]
        );
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::On, &toggle).unwrap(),
            ReasoningConfig::On
        );
        assert_eq!(
            normalize_reasoning_for_model(&ReasoningConfig::Effort(ReasoningEffort::High), &toggle)
                .unwrap(),
            ReasoningConfig::On
        );

        let levels = model_with(Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: None,
            openai_chat_mode: OpenAiChatReasoningMode::ProviderValues {
                values: vec!["none".into(), "low".into(), "high".into()],
                default: Some("low".into()),
                system_message: true,
            },
            min_effort: ReasoningEffort::Low,
            max_effort: ReasoningEffort::High,
        }));
        assert_eq!(
            supported_levels_with_subagents(&levels, false),
            vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High]
        );
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Medium, &levels).unwrap(),
            ReasoningConfig::Effort(ReasoningEffort::Low)
        );
    }

    #[test]
    fn always_on_reasoning_exposes_only_on_and_normalizes_stale_off() {
        let model = model_with(Some(ReasoningCapability {
            control: ReasoningControl::AlwaysOn,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: None,
            openai_chat_mode: OpenAiChatReasoningMode::SystemMessage,
            min_effort: ReasoningEffort::Minimal,
            max_effort: ReasoningEffort::High,
        }));
        assert_eq!(supported_levels_with_subagents(&model, false), vec![ThinkingLevel::On]);
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::Off, &model).unwrap(),
            ReasoningConfig::On
        );
        assert_eq!(
            thinking_to_reasoning(ThinkingLevel::High, &model).unwrap(),
            ReasoningConfig::On
        );
        assert_eq!(
            normalize_reasoning_for_model(&ReasoningConfig::Off, &model).unwrap(),
            ReasoningConfig::On
        );
        assert_eq!(
            level_from_reasoning(&ReasoningConfig::Off, &model).unwrap(),
            ThinkingLevel::On
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
        assert_eq!(supported_levels_with_subagents(&model, false), vec![ThinkingLevel::Off]);
    }
}
