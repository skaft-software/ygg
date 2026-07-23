#![allow(missing_docs)]

//! Presentation-level run lifecycle and deterministic tool summaries.
//!
//! The agent owns execution and persistence. This module owns the one answer the
//! frontends use for whether a run is active, what it is doing, how long the
//! current phase has lasted, and how it terminated. Events are applied with a
//! monotonically increasing [`RunId`] so a late event cannot mutate a newer run.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use ygg_agent::{AgentEvent, FinishReason, ToolError, ToolOutput};
use ygg_ai::{AssistantPart, ModelSpec, Pricing, TokenRate};

pub fn provider_status_name(canonical: &str) -> String {
    let canonical = canonical.trim();
    let friendly = match canonical.to_ascii_lowercase().as_str() {
        "openai-codex" | "codex" => Some("Codex"),
        "openai" => Some("OpenAI"),
        "anthropic" => Some("Anthropic"),
        "openrouter" => Some("OpenRouter"),
        "deepseek" => Some("DeepSeek"),
        "custom-openai" => Some("local endpoint"),
        _ => None,
    };
    friendly.unwrap_or(canonical).to_owned()
}

/// Render a token rate as dollars per million tokens.
pub fn format_token_rate(rate: TokenRate) -> String {
    format!("{}/M", format_token_rate_value(rate))
}

/// Compact dollar value for side-by-side model price comparisons.
pub fn format_token_rate_value(rate: TokenRate) -> String {
    let dollars = rate.0 as f64 / 1_000_000.0;
    if dollars >= 1.0 {
        format!("${dollars:.2}")
    } else {
        let value = format!("{dollars:.4}");
        let value = value.trim_end_matches('0').trim_end_matches('.');
        format!("${value}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunId(u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PriceDisplay {
    #[default]
    Unknown,
    ExplicitZero,
    Priced,
}

impl PriceDisplay {
    pub fn from_pricing(pricing: Option<&Pricing>) -> Self {
        let Some(pricing) = pricing else {
            return Self::Unknown;
        };
        let base_is_zero = pricing.input.0 == 0
            && pricing.output.0 == 0
            && pricing.cache_read.0 == 0
            && pricing.cache_write_5m.0 == 0
            && pricing.cache_write_1h.is_none_or(|rate| rate.0 == 0)
            && pricing.reasoning.is_none_or(|rate| rate.0 == 0);
        let tiers_are_zero = pricing.tiers.iter().all(|tier| {
            tier.input.is_none_or(|rate| rate.0 == 0)
                && tier.output.is_none_or(|rate| rate.0 == 0)
                && tier.cache_read.is_none_or(|rate| rate.0 == 0)
                && tier.cache_write_5m.is_none_or(|rate| rate.0 == 0)
                && tier.cache_write_1h.is_none_or(|rate| rate.0 == 0)
                && tier.reasoning.is_none_or(|rate| rate.0 == 0)
        });
        if base_is_zero && tiers_are_zero {
            Self::ExplicitZero
        } else {
            Self::Priced
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelDisplayMetadata {
    pub name: String,
    pub compact_names: Vec<String>,
}

impl ModelDisplayMetadata {
    pub fn resolve(spec: &ModelSpec) -> Self {
        let name =
            resolve_model_display_name(spec.display_name.as_deref(), &spec.id.0, &spec.api_name);
        let compact_names = model_display_name_variants(&name);
        Self {
            name,
            compact_names,
        }
    }
}

/// Resolve the stable, human-facing model identity once when model metadata
/// changes. Renderers receive this result and never inspect provider IDs.
pub fn resolve_model_display_name(
    configured: Option<&str>,
    canonical_id: &str,
    provider_name: &str,
) -> String {
    if let Some(configured) = configured.map(str::trim).filter(|name| !name.is_empty()) {
        return configured.to_owned();
    }
    if let Some(registry_name) = ygg_ai::model_metadata::model_display_name(canonical_id)
        .or_else(|| ygg_ai::model_metadata::model_display_name(provider_name))
    {
        return registry_name.to_owned();
    }
    // Custom-endpoint configuration historically encoded an explicit label in
    // the canonical suffix. Honor it only when it is visibly a human label;
    // machine IDs and repository paths continue through the conservative
    // derivation below.
    if let Some(custom_name) = canonical_id.strip_prefix("custom/") {
        let custom_name = custom_name.trim();
        if custom_name.contains(char::is_whitespace)
            && !custom_name.contains('/')
            && custom_name != provider_name
        {
            return custom_name.to_owned();
        }
    }
    // A provider-supplied value containing ordinary words is likely a real
    // label. Machine IDs, paths, and artifact names go through the conservative
    // canonical derivation below instead.
    let provider_name = provider_name.trim();
    if provider_name.contains(char::is_whitespace)
        && !provider_name.contains('/')
        && !provider_name.eq_ignore_ascii_case(canonical_id)
    {
        return provider_name.to_owned();
    }
    derive_model_display_name(canonical_id)
}

/// Conservative fallback for canonical IDs. Only recognized model families
/// are normalized; unfamiliar IDs are returned byte-for-byte (apart from
/// surrounding whitespace) rather than guessed at.
pub fn derive_model_display_name(canonical_id: &str) -> String {
    let original = canonical_id.trim();
    if original.is_empty() {
        return "model".to_owned();
    }

    let mut candidate = original;
    let mut recognized_prefix = false;
    for prefix in [
        "custom/",
        "openai/",
        "anthropic/",
        "deepseek/",
        "openrouter/",
        "models/",
    ] {
        if let Some(rest) = candidate.strip_prefix(prefix) {
            candidate = rest;
            recognized_prefix = true;
            break;
        }
    }
    let leaf = candidate.rsplit('/').next().unwrap_or(candidate);
    let family = model_family(leaf);
    if family.is_none() {
        return original.to_owned();
    }
    if candidate.contains('/') && !recognized_prefix {
        // `owner/model` may itself be a meaningful unfamiliar registry ID.
        return original.to_owned();
    }

    let mut words = leaf
        .trim_end_matches(|character: char| character == '.' || character.is_whitespace())
        .split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    while words.last().is_some_and(|word| artifact_suffix(word)) {
        words.pop();
    }
    // Quantization tags such as `Q4_K_M` are split into several words by the
    // normal tokenizer. Remove the group only when it forms a recognized
    // trailing artifact suffix on an already-recognized model family.
    if let Some(group_start) = trailing_quantization_group(&words) {
        words.truncate(group_start);
    }
    if words
        .last()
        .is_some_and(|word| word.eq_ignore_ascii_case("mtp"))
        && leaf.to_ascii_lowercase().contains("gguf")
    {
        words.pop();
    }
    if words.is_empty() {
        return original.to_owned();
    }

    match family.expect("checked above") {
        ModelFamily::Gpt => format_gpt(&words),
        ModelFamily::Qwen => format_known_words("Qwen", &words, true),
        ModelFamily::Claude => format_claude(&words),
        ModelFamily::DeepSeek => format_known_words("DeepSeek", &words, false),
        ModelFamily::Llama => format_known_words("Llama", &words, true),
        ModelFamily::Gemini => format_known_words("Gemini", &words, false),
        ModelFamily::Mistral => format_known_words("Mistral", &words, true),
    }
}

#[derive(Clone, Copy)]
enum ModelFamily {
    Gpt,
    Qwen,
    Claude,
    DeepSeek,
    Llama,
    Gemini,
    Mistral,
}

fn model_family(value: &str) -> Option<ModelFamily> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("gpt-") || lower.starts_with("gpt_") {
        Some(ModelFamily::Gpt)
    } else if lower.starts_with("qwen") {
        Some(ModelFamily::Qwen)
    } else if lower.starts_with("claude-") || lower.starts_with("claude_") {
        Some(ModelFamily::Claude)
    } else if lower.starts_with("deepseek") {
        Some(ModelFamily::DeepSeek)
    } else if lower.starts_with("llama") {
        Some(ModelFamily::Llama)
    } else if lower.starts_with("gemini") {
        Some(ModelFamily::Gemini)
    } else if lower.starts_with("mistral") || lower.starts_with("mixtral") {
        Some(ModelFamily::Mistral)
    } else {
        None
    }
}

fn artifact_suffix(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower == "gguf"
        || lower == "awq"
        || lower == "gptq"
        || lower == "safetensors"
        || lower == "autoround"
        || lower == "onnx"
        || lower == "mlx"
        || lower == "instruct"
        || lower == "chat"
        || lower == "responses"
        || precision_suffix(&lower)
        || (lower.len() == 8
            && (lower.starts_with("19") || lower.starts_with("20"))
            && lower.chars().all(|character| character.is_ascii_digit()))
        || (lower.starts_with('q')
            && lower[1..].chars().all(|character| {
                character.is_ascii_digit() || matches!(character, 'k' | 'm' | 's' | 'l' | '_')
            }))
}

fn precision_suffix(value: &str) -> bool {
    ["int", "uint", "fp", "bf", "f"].iter().any(|prefix| {
        value
            .strip_prefix(prefix)
            .is_some_and(|bits| !bits.is_empty() && bits.chars().all(|c| c.is_ascii_digit()))
    })
}

fn trailing_quantization_group(words: &[String]) -> Option<usize> {
    let start = words.iter().rposition(|word| {
        let lower = word.to_ascii_lowercase();
        let digits = lower.strip_prefix('q').or_else(|| lower.strip_prefix("iq"));
        digits.is_some_and(|digits| {
            !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
        })
    })?;
    let suffix = &words[start + 1..];
    (!suffix.is_empty()
        && suffix.len() <= 3
        && suffix.iter().all(|word| {
            let lower = word.to_ascii_lowercase();
            lower.chars().all(|character| character.is_ascii_digit())
                || matches!(lower.as_str(), "k" | "m" | "s" | "l")
        }))
    .then_some(start)
}

fn format_gpt(words: &[String]) -> String {
    let Some(version) = words.get(1) else {
        return "GPT".to_owned();
    };
    let mut output = format!("GPT-{version}");
    for word in words.iter().skip(2) {
        output.push(' ');
        output.push_str(&display_word(word));
    }
    output
}

fn format_claude(words: &[String]) -> String {
    let mut output = vec!["Claude".to_owned()];
    let mut index = 1;
    while index < words.len() {
        if index + 1 < words.len()
            && words[index]
                .chars()
                .all(|character| character.is_ascii_digit())
            && words[index + 1]
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            output.push(format!("{}.{}", words[index], words[index + 1]));
            index += 2;
        } else {
            output.push(display_word(&words[index]));
            index += 1;
        }
    }
    output.join(" ")
}

fn format_known_words(label: &str, words: &[String], drop_artifacts: bool) -> String {
    let mut output = Vec::with_capacity(words.len());
    let first = &words[0];
    let lower_label = label.to_ascii_lowercase();
    if first.to_ascii_lowercase() == lower_label {
        output.push(label.to_owned());
    } else if first.to_ascii_lowercase().starts_with(&lower_label) {
        output.push(format!("{label}{}", &first[label.len()..]));
    } else {
        output.push(display_word(first));
    }
    for word in words.iter().skip(1) {
        if drop_artifacts && artifact_suffix(word) {
            continue;
        }
        output.push(display_word(word));
    }
    output.join(" ")
}

fn display_word(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if lower.len() > 1
        && lower.ends_with('b')
        && lower[..lower.len() - 1]
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        return format!("{}B", &word[..word.len() - 1]);
    }
    if lower.starts_with('a')
        && lower.ends_with('b')
        && lower[1..lower.len() - 1]
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return word.to_ascii_uppercase();
    }
    if lower.starts_with('v')
        && lower[1..]
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        return format!("V{}", &word[1..]);
    }
    let mut characters = word.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    format!("{}{}", first.to_uppercase(), characters.as_str())
}

pub fn model_display_name_variants(name: &str) -> Vec<String> {
    let words = name.split_whitespace().collect::<Vec<_>>();
    let mut variants = Vec::with_capacity(3);
    for count in [words.len(), words.len().min(2), words.len().min(1)] {
        if count == 0 {
            continue;
        }
        let candidate = words[..count].join(" ");
        if variants.last() != Some(&candidate) {
            variants.push(candidate);
        }
    }
    if variants.is_empty() {
        variants.push("model".to_owned());
    }
    variants
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub files_changed: usize,
    pub tool_calls: usize,
    pub warnings: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed {
        elapsed: Duration,
        summary: RunSummary,
    },
    CompletedWithWarnings {
        elapsed: Duration,
        warnings: usize,
        summary: RunSummary,
    },
    Failed {
        elapsed: Duration,
        reason: String,
    },
    Interrupted {
        elapsed: Duration,
    },
    NeedsInput {
        prompt: String,
    },
    #[allow(dead_code)]
    Cancelled {
        elapsed: Duration,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunPhase {
    Preparing {
        summary: String,
    },
    AwaitingProvider {
        provider: String,
    },
    Thinking,
    StreamingResponse,
    PreparingToolCall,
    RunningTool {
        summary: String,
    },
    #[allow(dead_code)]
    AwaitingApproval {
        prompt: String,
    },
    Finished(RunOutcome),
}

impl RunPhase {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Finished(_))
    }
}

#[derive(Clone, Debug)]
struct TrackedTool {
    name: String,
    args: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct RunPresentation {
    id: RunId,
    provider: String,
    started_at: Instant,
    phase_started_at: Instant,
    phase: RunPhase,
    pending_tools: BTreeSet<String>,
    tools: HashMap<String, TrackedTool>,
    changed_files: BTreeSet<String>,
    tool_calls: usize,
    warnings: usize,
}

impl RunPresentation {
    pub fn id(&self) -> RunId {
        self.id
    }

    pub fn phase(&self) -> &RunPhase {
        &self.phase
    }

    pub fn is_active(&self) -> bool {
        self.phase.is_active()
    }

    pub fn phase_elapsed_at(&self, now: Instant) -> Duration {
        match &self.phase {
            RunPhase::Finished(_) => Duration::ZERO,
            _ => now.saturating_duration_since(self.phase_started_at),
        }
    }

    /// Total run time. Terminal outcomes carry their frozen elapsed value, so
    /// completion telemetry can never resume ticking after the run settles.
    pub fn elapsed_at(&self, now: Instant) -> Duration {
        match &self.phase {
            RunPhase::Finished(RunOutcome::Completed { elapsed, .. })
            | RunPhase::Finished(RunOutcome::CompletedWithWarnings { elapsed, .. })
            | RunPhase::Finished(RunOutcome::Failed { elapsed, .. })
            | RunPhase::Finished(RunOutcome::Interrupted { elapsed })
            | RunPhase::Finished(RunOutcome::Cancelled { elapsed }) => *elapsed,
            RunPhase::Finished(RunOutcome::NeedsInput { .. }) => self
                .phase_started_at
                .saturating_duration_since(self.started_at),
            _ => now.saturating_duration_since(self.started_at),
        }
    }

    fn summary(&self) -> RunSummary {
        RunSummary {
            files_changed: self.changed_files.len(),
            tool_calls: self.tool_calls,
            warnings: self.warnings,
        }
    }

    fn transition(&mut self, phase: RunPhase, now: Instant) {
        if self.phase == phase {
            return;
        }
        self.phase = phase;
        self.phase_started_at = now;
    }

    fn finish(&mut self, outcome: RunOutcome, now: Instant) -> Option<RunOutcome> {
        if !self.is_active() {
            return None;
        }
        self.phase = RunPhase::Finished(outcome.clone());
        self.phase_started_at = now;
        Some(outcome)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunTracker {
    next_id: u64,
    current: Option<RunPresentation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunUpdate {
    pub accepted: bool,
    pub outcome: Option<RunOutcome>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RunTracker {
    pub fn current(&self) -> Option<&RunPresentation> {
        self.current.as_ref()
    }

    pub fn current_id(&self) -> Option<RunId> {
        self.current.as_ref().map(RunPresentation::id)
    }

    pub fn is_active(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(RunPresentation::is_active)
    }

    /// Clear session-local presentation without reusing an earlier run ID.
    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn begin(&mut self, provider: impl Into<String>) -> Result<RunId, &'static str> {
        self.begin_at(provider, Instant::now())
    }

    pub fn begin_at(
        &mut self,
        provider: impl Into<String>,
        now: Instant,
    ) -> Result<RunId, &'static str> {
        if self.is_active() {
            return Err("a run is already active");
        }
        self.next_id = self.next_id.saturating_add(1);
        let id = RunId(self.next_id);
        self.current = Some(RunPresentation {
            id,
            provider: provider.into(),
            started_at: now,
            phase_started_at: now,
            phase: RunPhase::Preparing {
                summary: "checking context".into(),
            },
            pending_tools: BTreeSet::new(),
            tools: HashMap::new(),
            changed_files: BTreeSet::new(),
            tool_calls: 0,
            warnings: 0,
        });
        Ok(id)
    }

    pub fn set_preparing(&mut self, id: RunId, summary: impl Into<String>) -> bool {
        self.set_phase(
            id,
            RunPhase::Preparing {
                summary: summary.into(),
            },
        )
    }

    pub fn awaiting_provider(&mut self, id: RunId) -> bool {
        let Some(run) = self.active_mut(id) else {
            return false;
        };
        let provider = run.provider.clone();
        run.transition(RunPhase::AwaitingProvider { provider }, Instant::now());
        true
    }

    pub fn awaiting_approval(&mut self, id: RunId, prompt: impl Into<String>) -> bool {
        self.set_phase(
            id,
            RunPhase::AwaitingApproval {
                prompt: prompt.into(),
            },
        )
    }

    pub fn set_phase(&mut self, id: RunId, phase: RunPhase) -> bool {
        self.set_phase_at(id, phase, Instant::now())
    }

    pub fn set_phase_at(&mut self, id: RunId, phase: RunPhase, now: Instant) -> bool {
        if matches!(phase, RunPhase::Finished(_)) {
            return false;
        }
        let Some(run) = self.active_mut(id) else {
            return false;
        };
        run.transition(phase, now);
        true
    }

    pub fn interrupt(&mut self, id: RunId) -> Option<RunOutcome> {
        self.interrupt_at(id, Instant::now())
    }

    pub fn interrupt_at(&mut self, id: RunId, now: Instant) -> Option<RunOutcome> {
        let run = self.active_mut(id)?;
        let elapsed = now.saturating_duration_since(run.started_at);
        run.finish(RunOutcome::Interrupted { elapsed }, now)
    }

    pub fn cancel_at(&mut self, id: RunId, now: Instant) -> Option<RunOutcome> {
        let run = self.active_mut(id)?;
        let elapsed = now.saturating_duration_since(run.started_at);
        run.finish(RunOutcome::Cancelled { elapsed }, now)
    }

    pub fn fail(&mut self, id: RunId, reason: impl Into<String>) -> Option<RunOutcome> {
        self.fail_at(id, reason, Instant::now())
    }

    pub fn fail_at(
        &mut self,
        id: RunId,
        reason: impl Into<String>,
        now: Instant,
    ) -> Option<RunOutcome> {
        let run = self.active_mut(id)?;
        let elapsed = now.saturating_duration_since(run.started_at);
        run.finish(
            RunOutcome::Failed {
                elapsed,
                reason: reason.into(),
            },
            now,
        )
    }

    pub fn needs_input(&mut self, id: RunId, prompt: impl Into<String>) -> Option<RunOutcome> {
        let run = self.active_mut(id)?;
        run.finish(
            RunOutcome::NeedsInput {
                prompt: prompt.into(),
            },
            Instant::now(),
        )
    }

    pub fn apply_event(&mut self, id: RunId, event: &AgentEvent) -> RunUpdate {
        self.apply_event_at(id, event, Instant::now())
    }

    pub fn apply_event_at(&mut self, id: RunId, event: &AgentEvent, now: Instant) -> RunUpdate {
        let Some(run) = self.active_mut(id) else {
            return RunUpdate::default();
        };

        let mut outcome = None;
        match event {
            AgentEvent::OutputDelta { channel, .. } => {
                let phase = match channel {
                    ygg_agent::OutputChannel::Reasoning => RunPhase::Thinking,
                    ygg_agent::OutputChannel::Text => RunPhase::StreamingResponse,
                };
                run.transition(phase, now);
            }
            AgentEvent::ProviderRetry { .. } => {
                let provider = run.provider.clone();
                run.transition(RunPhase::AwaitingProvider { provider }, now);
            }
            AgentEvent::SteeringDelivered { .. } => {
                let provider = run.provider.clone();
                run.transition(RunPhase::AwaitingProvider { provider }, now);
            }
            AgentEvent::CompactionStarted { .. } => {
                run.transition(
                    RunPhase::Preparing {
                        summary: "compacting".into(),
                    },
                    now,
                );
            }
            AgentEvent::CompactionFinished { .. } => {
                let provider = run.provider.clone();
                run.transition(RunPhase::AwaitingProvider { provider }, now);
            }
            AgentEvent::TurnFinished { message, .. } => {
                run.pending_tools.clear();
                for part in &message.content {
                    if let AssistantPart::ToolCall(call) = part {
                        run.pending_tools.insert(call.id.0.clone());
                    }
                }
                if !run.pending_tools.is_empty() {
                    run.transition(RunPhase::PreparingToolCall, now);
                }
            }
            AgentEvent::ToolStarted { id, name, args } => {
                run.tool_calls = run.tool_calls.saturating_add(1);
                run.pending_tools.insert(id.0.clone());
                run.tools.insert(
                    id.0.clone(),
                    TrackedTool {
                        name: name.clone(),
                        args: args.clone(),
                    },
                );
                run.transition(
                    RunPhase::RunningTool {
                        // The pinned status line has no workspace context, so
                        // keep its tool summary concise rather than leaking a
                        // host-specific absolute path.
                        summary: summarize_tool(name, args).compact_active,
                    },
                    now,
                );
            }
            AgentEvent::ToolProgress { .. } => {}
            AgentEvent::ToolFinished { id, result } => {
                run.pending_tools.remove(&id.0);
                if let Some(tool) = run.tools.get(&id.0) {
                    if tool_result_is_failure(&tool.name, result) {
                        run.warnings = run.warnings.saturating_add(1);
                    } else if tool.name == "edit" {
                        if let Some(path) = tool.args.get("path").and_then(|value| value.as_str()) {
                            run.changed_files.insert(path.to_owned());
                        }
                    }
                } else if result.is_err() {
                    run.warnings = run.warnings.saturating_add(1);
                }
                if run.pending_tools.is_empty() {
                    let provider = run.provider.clone();
                    run.transition(RunPhase::AwaitingProvider { provider }, now);
                } else {
                    run.transition(RunPhase::PreparingToolCall, now);
                }
            }
            AgentEvent::RunFinished { reason, .. } => {
                let elapsed = now.saturating_duration_since(run.started_at);
                let terminal = match reason {
                    FinishReason::Completed if run.warnings > 0 => {
                        RunOutcome::CompletedWithWarnings {
                            elapsed,
                            warnings: run.warnings,
                            summary: run.summary(),
                        }
                    }
                    FinishReason::Completed => RunOutcome::Completed {
                        elapsed,
                        summary: run.summary(),
                    },
                    FinishReason::Aborted => RunOutcome::Interrupted { elapsed },
                    FinishReason::Failed(error) => RunOutcome::Failed {
                        elapsed,
                        reason: error.to_string(),
                    },
                    FinishReason::MaxTurns => RunOutcome::Failed {
                        elapsed,
                        reason: "maximum turns reached".into(),
                    },
                };
                outcome = run.finish(terminal, now);
            }
        }

        RunUpdate {
            accepted: true,
            outcome,
        }
    }

    fn active_mut(&mut self, id: RunId) -> Option<&mut RunPresentation> {
        self.current
            .as_mut()
            .filter(|run| run.id == id && run.is_active())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDisplay {
    pub active: String,
    pub success: String,
    pub failure: String,
    pub compact_active: String,
    pub compact_success: String,
    pub compact_failure: String,
    pub plain_tag: &'static str,
    /// Stable user-facing label, independent of the protocol tool identifier.
    pub label: String,
    /// Presentation-only shell command. The internal tool remains `exec`.
    pub shell_command: Option<String>,
    pub changed_path: Option<String>,
}

pub fn summarize_tool(name: &str, args: &serde_json::Value) -> ToolDisplay {
    summarize_tool_with_workspace(name, args, None)
}

/// Build a tool summary while rendering paths inside `workspace` relatively.
/// The raw arguments remain authoritative for execution and audit history.
pub fn summarize_tool_with_workspace(
    name: &str,
    args: &serde_json::Value,
    workspace: Option<&Path>,
) -> ToolDisplay {
    match name {
        "read" => summarize_read(args, workspace),
        "search" => {
            let path = display_path(string_arg(args, "path").unwrap_or("workspace"), workspace);
            let query = string_arg(args, "query").unwrap_or("pattern");
            let full = format!("searching {path} for {query}");
            let compact_path = compact_path(&path);
            let compact = format!("searching {compact_path}");
            ToolDisplay {
                active: full.clone(),
                success: format!("searched {path} for {query}"),
                failure: full,
                compact_active: compact.clone(),
                compact_success: format!("searched {compact_path}"),
                compact_failure: compact,
                plain_tag: "search",
                label: "search".to_owned(),
                shell_command: None,
                changed_path: None,
            }
        }
        "edit" => path_tool(args, "updating", "updated", "updating", "edit", workspace),
        "write" => path_tool(args, "writing", "wrote", "writing", "write", workspace),
        "exec" => summarize_exec(args, workspace),
        other => {
            let readable = other.replace(['_', '-'], " ");
            ToolDisplay {
                active: format!("running {readable}"),
                success: format!("finished {readable}"),
                failure: format!("{readable} failed"),
                compact_active: format!("running {readable}"),
                compact_success: format!("finished {readable}"),
                compact_failure: format!("{readable} failed"),
                plain_tag: "tool",
                label: readable.clone(),
                shell_command: None,
                changed_path: None,
            }
        }
    }
}

fn path_tool(
    args: &serde_json::Value,
    active_verb: &str,
    success_verb: &str,
    failure_verb: &str,
    tag: &'static str,
    workspace: Option<&Path>,
) -> ToolDisplay {
    let path = display_path(string_arg(args, "path").unwrap_or("file"), workspace);
    let compact = compact_path(&path);
    ToolDisplay {
        active: format!("{active_verb} {path}"),
        success: format!("{success_verb} {path}"),
        failure: format!("{failure_verb} {path}"),
        compact_active: format!("{active_verb} {compact}"),
        compact_success: format!("{success_verb} {compact}"),
        compact_failure: format!("{failure_verb} {compact}"),
        plain_tag: tag,
        label: tag.to_owned(),
        shell_command: None,
        changed_path: (tag == "edit").then_some(path),
    }
}

fn summarize_read(args: &serde_json::Value, workspace: Option<&Path>) -> ToolDisplay {
    let path = display_path(string_arg(args, "path").unwrap_or("file"), workspace);
    let offset = args.get("offset").and_then(|v| v.as_u64());
    let limit = args.get("limit").and_then(|v| v.as_u64());
    let range = match (offset, limit) {
        (Some(start), Some(count)) => {
            let end = start + count - 1;
            format!("{path}:{start}-{end}")
        }
        (Some(start), None) => format!("{path}:{start}+"),
        _ => path.clone(),
    };
    let compact = compact_path(&path);
    ToolDisplay {
        active: format!("reading {range}"),
        success: format!("read {range}"),
        failure: format!("reading {range}"),
        compact_active: format!("reading {compact}"),
        compact_success: format!("read {compact}"),
        compact_failure: format!("reading {compact}"),
        plain_tag: "read",
        label: "read".to_owned(),
        shell_command: None,
        changed_path: None,
    }
}

fn summarize_exec(args: &serde_json::Value, workspace: Option<&Path>) -> ToolDisplay {
    let command =
        normalize_shell_command(string_arg(args, "command").unwrap_or("command"), workspace);
    let program = command
        .split_whitespace()
        .next()
        .and_then(|value| std::path::Path::new(value).file_name())
        .and_then(|value| value.to_str())
        .unwrap_or("command");

    ToolDisplay {
        active: format!("running {}", command),
        success: format!("ran {}", command),
        failure: format!("failed: {}", command),
        compact_active: format!("running {program}"),
        compact_success: format!("ran {program}"),
        compact_failure: format!("{program} failed"),
        plain_tag: "run",
        label: "shell".to_owned(),
        shell_command: Some(command),
        changed_path: None,
    }
}

/// Normalize a path only for presentation. Execution retains the raw model
/// argument, while paths inside the configured workspace lose the host-specific
/// absolute prefix.
fn display_path(path: &str, workspace: Option<&Path>) -> String {
    let Some(workspace) = workspace else {
        return path.to_owned();
    };
    if path == "~" || path.starts_with("~/") {
        return path.to_owned();
    }
    let workspace_root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let source = Path::new(path);
    let candidate = if source.is_absolute() {
        source.to_path_buf()
    } else {
        workspace_root.join(source)
    };
    let candidate = candidate.canonicalize().unwrap_or(candidate);
    match candidate.strip_prefix(&workspace_root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.to_owned(),
    }
}

/// Remove a redundant shell `cd` to the workspace from the human-facing
/// summary. The command sent to the child process is never rewritten.
fn normalize_shell_command(command: &str, workspace: Option<&Path>) -> String {
    let Some(workspace) = workspace else {
        return command.to_owned();
    };
    let root = workspace.to_string_lossy();
    for prefix in [
        format!("cd {root} &&"),
        format!("cd '{root}' &&"),
        format!("cd \"{root}\" &&"),
    ] {
        if let Some(rest) = command.strip_prefix(&prefix) {
            return rest.trim_start().to_owned();
        }
    }
    command.to_owned()
}

fn string_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|value| value.as_str())
}

pub fn compact_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
        .to_owned()
}

pub fn tool_result_is_failure(name: &str, result: &Result<ToolOutput, ToolError>) -> bool {
    match result {
        Err(_) => true,
        Ok(output) if name == "exec" => exec_exit_reason(&output.text).is_some(),
        Ok(_) => false,
    }
}

pub fn tool_failure_reason(name: &str, result: &Result<ToolOutput, ToolError>) -> Option<String> {
    match result {
        Err(error) => Some(error_reason(&error.message)),
        Ok(output) if name == "exec" => exec_exit_reason(&output.text),
        Ok(_) => None,
    }
}

fn exec_exit_reason(output: &str) -> Option<String> {
    let first = output.lines().next()?.trim();
    let exit = first
        .split_whitespace()
        .find_map(|part| part.strip_prefix("exit="))?;
    match exit {
        "0" => None,
        value if value.starts_with("signal:") => {
            Some(format!("command stopped by {}", value.replace(':', " ")))
        }
        "unknown" => Some("command exit status unknown".into()),
        code => Some(format!("command exited {code}")),
    }
}

fn error_reason(message: &str) -> String {
    let mut lines = message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or("tool failed");
    if let Some(code) = first.strip_prefix("error ") {
        lines
            .find(|line| !is_hidden_tool_detail(line))
            .map(concise_line)
            .unwrap_or_else(|| code.replace('_', " "))
    } else if is_hidden_tool_detail(first) {
        "tool failed".into()
    } else {
        concise_line(first)
    }
}

/// Protocol/concurrency details remain available in verbose mode but are not
/// part of the default intent-level transcript.
pub fn is_hidden_tool_detail(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains("hash=")
        || line.contains("expected_hash")
        || line.contains("actual_hash")
        || line.contains("tool_call_id")
}

pub fn concise_line(value: &str) -> String {
    const LIMIT: usize = 160;
    let line = value
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("unknown error");
    let line = line.trim();
    if line.chars().count() <= LIMIT {
        return line.to_owned();
    }
    let mut result = line
        .chars()
        .take(LIMIT.saturating_sub(1))
        .collect::<String>();
    result.push('…');
    result
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let minutes = duration.as_secs() / 60;
        let remainder = duration.as_secs() % 60;
        format!("{minutes}m{remainder:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rates_keep_useful_sub_dollar_precision() {
        assert_eq!(format_token_rate_value(TokenRate(1_000_000)), "$1.00");
        assert_eq!(format_token_rate_value(TokenRate(435_000)), "$0.435");
        assert_eq!(format_token_rate_value(TokenRate(50_000)), "$0.05");
        assert_eq!(format_token_rate_value(TokenRate(2_800)), "$0.0028");
    }
    use ygg_agent::{AgentError, EntryId, FinishReason, OutputChannel};
    use ygg_ai::{AssistantMessage, ModelId, Protocol, ToolCall, ToolCallId, Usage};

    fn text_event(channel: OutputChannel) -> AgentEvent {
        AgentEvent::OutputDelta {
            channel,
            text: "x".into(),
        }
    }

    fn turn_with_tool(id: &str) -> AgentEvent {
        AgentEvent::TurnFinished {
            message: AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId(id.into()),
                    name: "read".into(),
                    arguments_json: "{\"path\":\"src/lib.rs\"}".into(),
                })],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            },
            turn_usage: Usage::default(),
            usage: Usage::default(),
            session_cost_microdollars: None,
            run_cost_microdollars: 0,
        }
    }

    fn finished(reason: FinishReason) -> AgentEvent {
        AgentEvent::RunFinished {
            head: EntryId("head".into()),
            reason,
        }
    }

    #[test]
    fn provider_status_names_hide_endpoint_implementation_noise() {
        assert_eq!(provider_status_name("openai-codex"), "Codex");
        assert_eq!(provider_status_name("custom-openai"), "local endpoint");
        assert_eq!(provider_status_name("private-relay-v2"), "private-relay-v2");
    }

    #[test]
    fn model_names_are_friendly_only_for_known_families() {
        assert_eq!(
            derive_model_display_name("custom/unsloth/Qwen3.6-35B-A3B-MTP-GGUF"),
            "Qwen3.6 35B A3B"
        );
        assert_eq!(
            derive_model_display_name("custom/Qwen3.6-35B-AWQ"),
            "Qwen3.6 35B"
        );
        assert_eq!(
            derive_model_display_name("custom/Intel/Qwen3.6-27B-int4-AutoRound"),
            "Qwen3.6 27B"
        );
        assert_eq!(
            derive_model_display_name("custom/Intel/Qwen3.6-27B-Q4_K_M-GGUF"),
            "Qwen3.6 27B"
        );
        assert_eq!(
            derive_model_display_name("anthropic/claude-3-5-sonnet-20241022"),
            "Claude 3.5 Sonnet"
        );
        assert_eq!(
            derive_model_display_name("openai/gpt-4.1-mini"),
            "GPT-4.1 Mini"
        );
        assert_eq!(
            derive_model_display_name("custom/acme/unknown-model-Q4_K_M-GGUF"),
            "custom/acme/unknown-model-Q4_K_M-GGUF"
        );
    }

    #[test]
    fn explicit_registry_and_custom_display_names_follow_precedence() {
        assert_eq!(
            resolve_model_display_name(Some("My Fast Model"), "openai/gpt-4.1", "gpt-4.1"),
            "My Fast Model"
        );
        let registry = ygg_ai::model_metadata::model_display_name("openai/gpt-4o-mini").unwrap();
        assert_eq!(
            resolve_model_display_name(None, "openai/gpt-4o-mini", "gpt-4o-mini"),
            registry
        );
        assert_eq!(
            resolve_model_display_name(None, "custom/Local Coder", "org/raw-model"),
            "Local Coder"
        );
        assert_eq!(
            resolve_model_display_name(
                None,
                "custom/Intel/Qwen3.6-27B-int4-AutoRound",
                "Intel/Qwen3.6-27B-int4-AutoRound",
            ),
            "Qwen3.6 27B"
        );
    }

    #[test]
    fn submission_immediately_enters_an_active_state() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("anthropic", now).unwrap();
        assert_eq!(tracker.current_id(), Some(id));
        assert!(tracker.is_active());
        assert!(matches!(
            tracker.current().unwrap().phase(),
            RunPhase::Preparing { .. }
        ));
    }

    #[test]
    fn provider_wait_remains_active_without_streamed_tokens() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        tracker.set_phase_at(
            id,
            RunPhase::AwaitingProvider {
                provider: "openai".into(),
            },
            now,
        );
        let later = now + Duration::from_secs(3);
        assert!(tracker.is_active());
        assert_eq!(
            tracker.current().unwrap().phase_elapsed_at(later),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn autonomous_compaction_has_an_explicit_phase_then_returns_to_provider_wait() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        tracker.apply_event_at(
            id,
            &AgentEvent::CompactionStarted {
                reason: ygg_agent::CompactionReason::Threshold,
            },
            now,
        );
        assert_eq!(
            tracker.current().unwrap().phase(),
            &RunPhase::Preparing {
                summary: "compacting".into()
            }
        );

        tracker.apply_event_at(
            id,
            &AgentEvent::CompactionFinished {
                reason: ygg_agent::CompactionReason::Threshold,
                result: Err("summary unavailable".into()),
            },
            now + Duration::from_secs(1),
        );
        assert_eq!(
            tracker.current().unwrap().phase(),
            &RunPhase::AwaitingProvider {
                provider: "openai".into()
            }
        );
    }

    #[test]
    fn thinking_transitions_to_response_streaming() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        tracker.apply_event_at(id, &text_event(OutputChannel::Reasoning), now);
        assert_eq!(tracker.current().unwrap().phase(), &RunPhase::Thinking);
        tracker.apply_event_at(id, &text_event(OutputChannel::Text), now);
        assert_eq!(
            tracker.current().unwrap().phase(),
            &RunPhase::StreamingResponse
        );
    }

    #[test]
    fn model_streaming_moves_to_tool_execution_and_back_to_provider_wait() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        tracker.apply_event_at(id, &text_event(OutputChannel::Text), now);
        tracker.apply_event_at(id, &turn_with_tool("call"), now);
        assert_eq!(
            tracker.current().unwrap().phase(),
            &RunPhase::PreparingToolCall
        );
        tracker.apply_event_at(
            id,
            &AgentEvent::ToolStarted {
                id: ToolCallId("call".into()),
                name: "read".into(),
                args: serde_json::json!({"path":"src/lib.rs"}),
            },
            now,
        );
        assert!(matches!(
            tracker.current().unwrap().phase(),
            RunPhase::RunningTool { .. }
        ));
        tracker.apply_event_at(
            id,
            &AgentEvent::ToolFinished {
                id: ToolCallId("call".into()),
                result: Ok(ToolOutput::new("ok")),
            },
            now,
        );
        assert_eq!(
            tracker.current().unwrap().phase(),
            &RunPhase::AwaitingProvider {
                provider: "openai".into()
            }
        );
    }

    #[test]
    fn completion_and_failure_emit_exactly_one_terminal_outcome() {
        let now = Instant::now();
        let mut completed = RunTracker::default();
        let id = completed.begin_at("openai", now).unwrap();
        let first = completed.apply_event_at(id, &finished(FinishReason::Completed), now);
        let second = completed.apply_event_at(id, &finished(FinishReason::Completed), now);
        assert!(matches!(first.outcome, Some(RunOutcome::Completed { .. })));
        assert_eq!(second, RunUpdate::default());

        let mut failed = RunTracker::default();
        let id = failed.begin_at("openai", now).unwrap();
        let update = failed.apply_event_at(
            id,
            &finished(FinishReason::Failed(AgentError::RunEnded)),
            now,
        );
        assert!(matches!(update.outcome, Some(RunOutcome::Failed { .. })));
    }

    #[test]
    fn recovered_tool_failure_completes_with_warnings() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        tracker.apply_event_at(
            id,
            &AgentEvent::ToolStarted {
                id: ToolCallId("call".into()),
                name: "exec".into(),
                args: serde_json::json!({"mode":"process","program":"cargo","args":["test"]}),
            },
            now,
        );
        tracker.apply_event_at(
            id,
            &AgentEvent::ToolFinished {
                id: ToolCallId("call".into()),
                result: Ok(ToolOutput::new("exit=1 duration=0.1s")),
            },
            now,
        );
        let update = tracker.apply_event_at(id, &finished(FinishReason::Completed), now);
        assert!(matches!(
            update.outcome,
            Some(RunOutcome::CompletedWithWarnings { warnings: 1, .. })
        ));
    }

    #[test]
    fn interruption_is_immediate_and_cannot_later_become_completed() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        assert!(matches!(
            tracker.interrupt_at(id, now),
            Some(RunOutcome::Interrupted { .. })
        ));
        let update = tracker.apply_event_at(id, &finished(FinishReason::Completed), now);
        assert_eq!(update, RunUpdate::default());
        assert!(matches!(
            tracker.current().unwrap().phase(),
            RunPhase::Finished(RunOutcome::Interrupted { .. })
        ));
    }

    #[test]
    fn cancellation_cannot_later_become_completed() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        assert!(matches!(
            tracker.cancel_at(id, now),
            Some(RunOutcome::Cancelled { .. })
        ));
        assert_eq!(
            tracker.apply_event_at(id, &finished(FinishReason::Completed), now),
            RunUpdate::default()
        );
    }

    #[test]
    fn approval_and_input_wait_are_not_completed() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let id = tracker.begin_at("openai", now).unwrap();
        tracker.awaiting_approval(id, "allow edit");
        assert!(tracker.is_active());
        let outcome = tracker.needs_input(id, "choose an implementation").unwrap();
        assert_eq!(
            outcome,
            RunOutcome::NeedsInput {
                prompt: "choose an implementation".into()
            }
        );
    }

    #[test]
    fn clearing_session_presentation_preserves_monotonic_run_ids() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let old = tracker.begin_at("openai", now).unwrap();
        tracker.apply_event_at(old, &finished(FinishReason::Completed), now);
        tracker.clear();
        let current = tracker.begin_at("anthropic", now).unwrap();
        assert_ne!(current, old);
    }

    #[test]
    fn stale_events_from_a_previous_run_cannot_mutate_the_current_run() {
        let now = Instant::now();
        let mut tracker = RunTracker::default();
        let old = tracker.begin_at("openai", now).unwrap();
        tracker.apply_event_at(old, &finished(FinishReason::Completed), now);
        let current = tracker.begin_at("anthropic", now).unwrap();
        let update = tracker.apply_event_at(old, &text_event(OutputChannel::Text), now);
        assert_eq!(update, RunUpdate::default());
        assert_eq!(tracker.current_id(), Some(current));
        assert!(matches!(
            tracker.current().unwrap().phase(),
            RunPhase::Preparing { .. }
        ));
    }

    #[test]
    fn tool_summaries_show_real_commands() {
        let read = summarize_tool("read", &serde_json::json!({"path":"crates/ygg/src/lib.rs"}));
        assert_eq!(read.active, "reading crates/ygg/src/lib.rs");
        assert_eq!(read.compact_active, "reading lib.rs");

        let tests = summarize_tool(
            "exec",
            &serde_json::json!({"command":"cargo test --workspace"}),
        );
        assert_eq!(tests.active, "running cargo test --workspace");
        assert_eq!(tests.plain_tag, "run");
    }

    #[test]
    fn workspace_paths_and_redundant_cd_are_normalized_for_display() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let read = summarize_tool_with_workspace(
            "read",
            &serde_json::json!({"path": file}),
            Some(dir.path()),
        );
        assert_eq!(read.active, "reading src/main.rs");

        let command = format!("cd {} && printf ok", dir.path().display());
        let exec = summarize_tool_with_workspace(
            "exec",
            &serde_json::json!({"command":command}),
            Some(dir.path()),
        );
        assert_eq!(exec.active, "running printf ok");
    }

    #[test]
    fn stale_edit_hashes_are_diagnostic_only() {
        let result = Err(ToolError::new(
            "error stale_file\nsrc/lib.rs expected hash=aaa actual=bbb\nThe file changed",
        ));
        assert_eq!(
            tool_failure_reason("edit", &result).as_deref(),
            Some("The file changed")
        );
        assert!(is_hidden_tool_detail(
            "src/lib.rs expected hash=aaa actual=bbb"
        ));
    }

    #[test]
    fn nonzero_exec_is_a_specific_warning() {
        let result = Ok(ToolOutput::new("exit=1 duration=0.20s\nstderr:\nfailed"));
        assert!(tool_result_is_failure("exec", &result));
        assert_eq!(
            tool_failure_reason("exec", &result).as_deref(),
            Some("command exited 1")
        );
    }
}
