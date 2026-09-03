//! The agent: configuration, the procedural run loop, and run control.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_core::Stream;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use ygg_ai::{
    AiClient, AiError, AssistantMessage, AssistantPart, AudioPayload, CacheRetention,
    CompatibilityMode, Cost, DecodeError, ImageSource, Media, Message, Model, OutputFormat,
    OutputModalities, Protocol, ReasoningConfig, ReasoningMode, Request, ResponsesCompactRequest,
    ResponsesInput, ResponsesOptions, ResponsesReplayItem, StopReason, StreamEvent, ToolCall,
    ToolCallArgumentError, ToolChoice, ToolDef, ToolResult, ToolResultPart, Usage, UserMessage,
    UserPart, PICODOLLARS_PER_MICRODOLLAR,
};

use crate::compaction::{
    build_handoff_message, build_turn_prefix_handoff_message, choose_first_kept_by_tokens,
    finish_handoff, prepare_handoff, HandoffPreparation, DEFAULT_KEEP_RECENT_TOKENS,
    SUMMARIZATION_SYSTEM_PROMPT, SUMMARY_OUTPUT_TOKENS, TURN_PREFIX_OUTPUT_TOKENS,
};
use crate::context::{ContextBreakdown, ContextSnapshot, ContextTracker};
use crate::delegation::{
    enable_root_delegation, DelegationBinding, DelegationConfig, DelegationError,
    DelegationRuntimeSettings, DelegationTemplate,
};
use crate::effect::{EffectBroker, EffectIntent, EffectReservation, ToolEffect};
use crate::events::{
    AgentEvent, CompactionInfo, CompactionKind, CompactionReason, Control,
    DelegationTelemetrySnapshot, FinishReason, OutputChannel, QueueDeliveryMode,
};
use crate::extension::{EventObserver, ExtensionHost, ToolCallHook};
use crate::extension_process::{ExtensionProcess, EXTENSION_FEATURE_AGENT_SESSIONS};
use crate::input::UserInput;
use crate::sandbox::SandboxConfig;
use crate::session::{
    DelegatedUsage, EntryId, EntryMetadata, EntryValue, Session, SessionError, SessionRunOutcome,
    UsageRecordKind,
};
use crate::speculation::is_speculatable_recon_bash;
use crate::tool::{
    content_hash, CancellationToken, ReplaySafety, Tool, ToolConcurrency, ToolContext, ToolError,
    ToolOutput, ToolOutputContentPart, ToolOutputDetails, ToolOutputMediaKind, ToolProgress,
    ToolProgressSink, PROGRESS_CHANNEL_CAPACITY,
};

/// Errors surfaced by [`Agent`] APIs.
///
/// Before a run starts these are returned directly (from [`Agent::new`],
/// [`Agent::prompt`], [`RunControl::steer`]…). Once a run has started, every
/// failure is delivered as the single terminal
/// [`AgentEvent::RunFinished`]`{ reason: FinishReason::Failed(..) }` event —
/// there is no second asynchronous error channel.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Session persistence failed.
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    /// The inference layer failed.
    #[error("ai error: {0}")]
    Ai(#[from] AiError),
    /// Repeated non-timeout network failures exhausted automatic recovery.
    #[error(
        "network connection failed after {retries} retries. Are you connected to the internet? ({detail})"
    )]
    NetworkUnavailable {
        /// Number of replacement attempts made after the initial request.
        retries: usize,
        /// Bounded phase-only detail for diagnostics.
        detail: String,
    },
    /// Two tools were registered under the same name.
    #[error("duplicate tool name registered: {0}")]
    DuplicateTool(String),
    /// The configured collaboration runtime could not start an owning run.
    #[error("delegation error: {0}")]
    Delegation(String),
    /// The configured workspace root is unusable.
    #[error("invalid workspace: {0}")]
    Workspace(String),
    /// The provider ended a response without a normal completion signal.
    #[error("model response did not complete normally: {stop_reason}")]
    IncompleteResponse {
        /// Provider termination reason.
        stop_reason: String,
    },
    /// The next billable request's conservative token reservation would cross
    /// the configured session token ceiling.
    #[error(
        "session token limit would be exceeded: current {current} + reserved {reserved} tokens > limit {limit}"
    )]
    TokenLimit {
        /// Durable session token usage before the request.
        current: u64,
        /// Conservative input plus maximum-output reservation.
        reserved: u64,
        /// Configured ceiling.
        limit: u64,
    },
    /// The next billable request's conservative reservation would cross the
    /// configured session spend ceiling.
    #[error(
        "session cost limit would be exceeded: current {current} µUSD + reserved {reserved} µUSD > limit {limit} µUSD"
    )]
    CostLimit {
        /// Durable session cost before the request.
        current: u64,
        /// Conservative worst-case request reservation.
        reserved: u64,
        /// Configured ceiling.
        limit: u64,
    },
    /// A spend ceiling was requested but the selected model has no trusted
    /// pricing from which the host can reserve the next request.
    #[error(
        "session cost limit cannot be enforced because model pricing is unavailable (limit {limit} µUSD)"
    )]
    CostUnavailable {
        /// Configured ceiling that cannot be enforced.
        limit: u64,
    },
    /// The request would exceed the model's context budget after compaction.
    #[error(
        "request context is too large: approximately {estimate} tokens exceeds the {budget}-token input budget"
    )]
    ContextExceeded {
        /// Estimated request size.
        estimate: u64,
        /// Maximum input size after reserving output capacity.
        budget: u64,
    },
    /// The configured autonomous compaction policy is invalid.
    #[error("invalid compaction policy: {0}")]
    InvalidCompactionPolicy(String),
    /// Internal autonomous work was cancelled before its commit point.
    #[error("operation cancelled")]
    Cancelled,
    /// A control message was sent after the run finished.
    #[error("the run has already finished")]
    RunEnded,
}

/// Format an agent failure for a public frontend.
///
/// Provider failures retain operationally useful, bounded metadata: the route,
/// execution phase, HTTP status/class, provider error code, safe provider
/// message, retry hint, terminal stop reason, and request ID when supplied.
/// Request bodies, URLs,
/// credentials, and arbitrary response payloads are never copied into this
/// string. The AI client redacts at the transport boundary; this function
/// applies the final field allow-list and byte bound for UI/RPC consumers.
pub fn public_error_diagnostic(error: &AgentError, endpoint: &str, model: &str) -> String {
    match error {
        AgentError::Ai(error) => public_ai_error_diagnostic(error, endpoint, model),
        AgentError::IncompleteResponse { stop_reason } => {
            let mut diagnostic = provider_phase_diagnostic(endpoint, model, "response completion");
            append_provider_field(&mut diagnostic, "reason", Some(stop_reason));
            truncate_public_diagnostic(&mut diagnostic);
            diagnostic
        }
        _ => match provider_failure_phase(error) {
            Some(phase) => provider_phase_diagnostic(endpoint, model, phase),
            None => error.to_string(),
        },
    }
}

/// Format an inference-layer error for a user-facing retry or terminal event.
/// The same allow-list is used for both paths so retry messages cannot expose
/// more provider data than the final failure message.
fn public_ai_error_diagnostic(error: &AiError, endpoint: &str, model: &str) -> String {
    let context = |phase| provider_phase_diagnostic(endpoint, model, phase);
    match error {
        AiError::Http(http) => format_http_diagnostic(&context("HTTP response"), http),
        AiError::Provider(provider) => {
            let mut diagnostic = context("response body (provider error)");
            append_provider_field(&mut diagnostic, "code", provider.code.as_deref());
            append_provider_field(&mut diagnostic, "kind", provider.kind.as_deref());
            append_provider_field(&mut diagnostic, "detail", Some(&provider.message));
            append_provider_field(
                &mut diagnostic,
                "request_id",
                provider.request_id.as_deref(),
            );
            truncate_public_diagnostic(&mut diagnostic);
            diagnostic
        }
        AiError::Transport(transport) => {
            let phase = match (transport.phase, transport.timeout) {
                (ygg_ai::TransportPhase::Connect, false) => "connection",
                (ygg_ai::TransportPhase::Connect, true) => "connection timeout",
                (ygg_ai::TransportPhase::ResponseHeaders, false) => "response headers",
                (ygg_ai::TransportPhase::ResponseHeaders, true) => "response headers timeout",
                (ygg_ai::TransportPhase::Body, false) => "response body",
                (ygg_ai::TransportPhase::Body, true) => "response body timeout",
            };
            let mut diagnostic = context(phase);
            append_provider_field(&mut diagnostic, "detail", Some(&transport.message));
            truncate_public_diagnostic(&mut diagnostic);
            diagnostic
        }
        // A mid-stream failure is annotated with the wire progress that
        // distinguishes "died after 400 frames" from "never started": raw
        // provider frames, decoded events, retained content bytes, and
        // elapsed time. The inner diagnostic is first bounded to leave room
        // for this compact field, so a verbose inner detail can never push
        // the progress out of the truncation window.
        AiError::StreamFailure { inner, progress } => {
            let progress = format_stream_progress(progress);
            let mut diagnostic = public_ai_error_diagnostic(inner, endpoint, model);
            let budget = MAX_PUBLIC_PROVIDER_DIAGNOSTIC_BYTES
                .saturating_sub(progress.len())
                .saturating_sub("stream_progress=".len() + 1);
            truncate_public_diagnostic_to(&mut diagnostic, budget);
            append_provider_field(&mut diagnostic, "stream_progress", Some(&progress));
            truncate_public_diagnostic(&mut diagnostic);
            diagnostic
        }
        // The remaining variants previously surfaced as a bare phase label
        // with their message discarded. Their text is already
        // credential-redacted at the request boundary, so a bounded
        // `detail` field is safe to show.
        AiError::Config(error) => detail_diagnostic(&context("request preparation"), error),
        AiError::Auth(error) => detail_diagnostic(&context("authentication"), error),
        AiError::Validation(error) => detail_diagnostic(&context("request preparation"), error),
        AiError::Unsupported(error) => detail_diagnostic(&context("request preparation"), error),
        AiError::Decode(error) => detail_diagnostic(&context("response decoding"), error),
        AiError::Pricing(error) => detail_diagnostic(&context("usage accounting"), error),
        AiError::StreamProtocol(error) => detail_diagnostic(&context("stream protocol"), error),
        AiError::Canceled => context("request cancellation"),
    }
}

/// One-line enrichment for an error variant: the phase context plus the
/// (already credential-redacted) error text as a bounded `detail` field.
fn detail_diagnostic(prefix: &str, error: &impl std::fmt::Display) -> String {
    let mut diagnostic = prefix.to_string();
    append_provider_field(&mut diagnostic, "detail", Some(&error.to_string()));
    truncate_public_diagnostic(&mut diagnostic);
    diagnostic
}

/// Compact, greppable rendering of mid-stream progress counters.
fn format_stream_progress(progress: &ygg_ai::StreamProgress) -> String {
    let first_byte = if progress.first_body_seen {
        "seen"
    } else {
        "none"
    };
    let last_event = progress
        .last_event_ms
        .map_or_else(|| "none".to_owned(), |elapsed| format!("{elapsed}ms"));
    format!(
        "frames={} events={} content={}B buffered={}B first_byte={} elapsed={}ms last_event={}",
        progress.provider_events,
        progress.decoded_events,
        progress.content_bytes,
        progress.buffered_bytes,
        first_byte,
        progress.elapsed_ms,
        last_event,
    )
}

const MAX_PUBLIC_PROVIDER_DIAGNOSTIC_BYTES: usize = 2 * 1024;
const MAX_PUBLIC_PROVIDER_FIELD_BYTES: usize = 512;

fn format_http_diagnostic(prefix: &str, error: &ygg_ai::HttpError) -> String {
    let mut diagnostic = format!("{prefix} status={}", error.status.as_u16());
    if let Some(summary) = http_status_summary(error.status.as_u16()) {
        diagnostic.push_str(" (");
        diagnostic.push_str(summary);
        diagnostic.push(')');
    }
    append_provider_field(&mut diagnostic, "code", error.provider_code.as_deref());
    if let Some(message) = provider_error_message(error.body_snippet.as_deref()) {
        append_provider_field(&mut diagnostic, "detail", Some(&message));
    }
    if let Some(delay) = error.retry_after {
        append_provider_field(
            &mut diagnostic,
            "retry_after",
            Some(&format!("{}s", delay.as_secs())),
        );
    }
    append_provider_field(&mut diagnostic, "request_id", error.request_id.as_deref());
    truncate_public_diagnostic(&mut diagnostic);
    diagnostic
}

fn provider_error_message(body: Option<&str>) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body?).ok()?;
    let error = value.get("error").unwrap_or(&value);
    ["message", "detail", "description"]
        .iter()
        .find_map(|field| error.get(*field).and_then(serde_json::Value::as_str))
        .map(ToOwned::to_owned)
        .filter(|message| !message.trim().is_empty())
}

fn append_provider_field(output: &mut String, name: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    let value = compact_public_provider_field(value);
    if value.is_empty() {
        return;
    }
    output.push(' ');
    output.push_str(name);
    output.push('=');
    output.push_str(&value);
}

fn compact_public_provider_field(value: &str) -> String {
    let value = redact_common_secret_patterns(value);
    let mut output = String::with_capacity(value.len().min(MAX_PUBLIC_PROVIDER_FIELD_BYTES));
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            continue;
        }
        if output
            .len()
            .saturating_add(character.len_utf8())
            .saturating_add('…'.len_utf8())
            > MAX_PUBLIC_PROVIDER_FIELD_BYTES
        {
            output.push('…');
            break;
        }
        output.push(character);
    }
    output
}

/// Defense in depth for callers that construct `AgentError` values without
/// passing through ygg-ai's credential redactor. The normal transport path
/// performs exact credential redaction; these common bearer/key forms prevent
/// accidental leakage from provider-authentication messages at the UI boundary.
fn redact_common_secret_patterns(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while !rest.is_empty() {
        let lower = rest.to_ascii_lowercase();
        let Some((offset, marker)) = ["sk-", "bearer ", "api_key=", "https://", "http://"]
            .iter()
            .filter_map(|marker| lower.find(marker).map(|offset| (offset, *marker)))
            .min_by_key(|(offset, _)| *offset)
        else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..offset]);
        output.push_str(match marker {
            "bearer " => "Bearer [REDACTED]",
            "https://" | "http://" => "[URL]",
            _ => "[REDACTED]",
        });
        let token_start = offset + marker.len();
        let token_end = rest[token_start..]
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, '"' | '\'' | ')' | ']' | '}' | ',' | ';')
            })
            .map_or(rest.len(), |end| token_start + end);
        rest = &rest[token_end..];
    }
    output
}

fn truncate_public_diagnostic(diagnostic: &mut String) {
    truncate_public_diagnostic_to(diagnostic, MAX_PUBLIC_PROVIDER_DIAGNOSTIC_BYTES)
}

fn truncate_public_diagnostic_to(diagnostic: &mut String, budget: usize) {
    if diagnostic.len() <= budget {
        return;
    }
    let mut end = budget.saturating_sub('…'.len_utf8());
    while end > 0 && !diagnostic.is_char_boundary(end) {
        end -= 1;
    }
    diagnostic.truncate(end);
    diagnostic.push('…');
}

fn http_status_summary(status: u16) -> Option<&'static str> {
    Some(match status {
        400 => "bad request",
        401 => "authentication failed",
        402 => "payment or credits required",
        403 => "forbidden",
        404 => "route or model not found",
        408 => "request timeout",
        409 => "conflict",
        413 => "request too large",
        422 => "request rejected",
        429 => "rate limited",
        500..=599 => "provider unavailable",
        _ => return None,
    })
}

fn provider_failure_phase(error: &AgentError) -> Option<&'static str> {
    match error {
        AgentError::Ai(error) => Some(ai_error_phase(error)),
        AgentError::NetworkUnavailable { .. } => Some("connection"),
        // Handled with an extra `reason=` field by `public_error_diagnostic`;
        // kept here so the phase table stays exhaustive.
        AgentError::IncompleteResponse { .. } => Some("response completion"),
        AgentError::Session(_)
        | AgentError::DuplicateTool(_)
        | AgentError::Delegation(_)
        | AgentError::Workspace(_)
        | AgentError::TokenLimit { .. }
        | AgentError::CostLimit { .. }
        | AgentError::CostUnavailable { .. }
        | AgentError::ContextExceeded { .. }
        | AgentError::InvalidCompactionPolicy(_)
        | AgentError::Cancelled
        | AgentError::RunEnded => None,
    }
}

fn ai_error_phase(error: &AiError) -> &'static str {
    match error {
        AiError::Config(_) | AiError::Validation(_) | AiError::Unsupported(_) => {
            "request preparation"
        }
        AiError::Auth(_) => "authentication",
        AiError::Http(_) => "HTTP response",
        AiError::Transport(error) => match (error.phase, error.timeout) {
            (ygg_ai::TransportPhase::Connect, false) => "connection",
            (ygg_ai::TransportPhase::Connect, true) => "connection timeout",
            (ygg_ai::TransportPhase::ResponseHeaders, false) => "response headers",
            (ygg_ai::TransportPhase::ResponseHeaders, true) => "response headers timeout",
            (ygg_ai::TransportPhase::Body, false) => "response body",
            (ygg_ai::TransportPhase::Body, true) => "response body timeout",
        },
        AiError::Provider(_) => "response body (provider error)",
        AiError::Decode(_) => "response decoding",
        AiError::Pricing(_) => "usage accounting",
        AiError::StreamProtocol(_) => "stream protocol",
        // A mid-stream wrapper reports the phase of the failure that
        // actually ended the stream, so the UI shows e.g. "response
        // decoding", not an uninformative wrapper label.
        AiError::StreamFailure { inner, .. } => ai_error_phase(inner),
        AiError::Canceled => "request cancellation",
    }
}

fn provider_phase_diagnostic(endpoint: &str, model: &str, phase: &str) -> String {
    format!("provider={endpoint} model={model} phase={phase}")
}

/// How an agent decides that a natural no-tool response is complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// Accept the first normal no-tool response.
    #[default]
    Natural,
    /// Treat a normal no-tool response as a candidate and ask an isolated,
    /// one-token evidence gate whether control should return to the user.
    TerminalGate,
}

/// Autonomous context-reduction strategy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentCompactionMode {
    /// Disable autonomous compaction.
    Disabled,
    /// Generate a provider-independent summary and retain a canonical tail.
    #[default]
    Local,
    /// Use OpenAI Responses native opaque compaction on the active route.
    NativeResponses,
}

/// Configuration for [`Agent::new`].
pub struct AgentConfig {
    /// The inference client.
    pub client: AiClient,
    /// The resolved model to converse with.
    pub model: Model,
    /// The session holding (and persisting) conversation history.
    pub session: Session,
    /// The system prompt (empty string for none).
    pub system: String,
    /// Capability gates and limits for tool execution.
    pub sandbox: SandboxConfig,
    /// Mandatory deterministic broker for every model-requested tool effect.
    pub effect_broker: EffectBroker,
    /// Registered tools and event observers. Register [`CoreTools`](crate::tools::CoreTools)
    /// here for the built-in `read`/`edit`/`write`/`bash`/`search` tools.
    pub extensions: ExtensionHost,
    /// Maximum model turns per run; exceeding it finishes the run with
    /// [`FinishReason::MaxTurns`].  `None` disables the limit.
    pub max_turns: Option<u64>,
    /// Reasoning configuration applied to every model request in this agent's
    /// runs. Use [`ReasoningConfig::Off`] to disable reasoning (the historical
    /// default). Unsupported configurations are rejected by `ygg-ai`'s
    /// validation when the run opens its stream, surfacing as
    /// [`FinishReason::Failed`].
    pub reasoning: ReasoningConfig,
    /// Reasoning execution mode applied independently from effort.
    pub reasoning_mode: ReasoningMode,
    /// Prompt-cache retention policy for model turns. Defaults to short in
    /// application configuration, matching pi.
    pub cache_retention: CacheRetention,
    /// Optional explicit cache-affinity ID. When absent, the stable session
    /// path-derived key is used.
    pub session_id: Option<String>,
}

struct RunLifecycle {
    finished: AtomicBool,
    dropped: AtomicBool,
}

/// Owns the session borrow inside the generated run stream. Rust drops stream
/// locals when [`Run`] is dropped, so this guard is the only place that can
/// durably pair unresolved calls before the mutable session borrow is released.
struct RunSessionGuard<'a> {
    session: &'a mut Session,
    lifecycle: Arc<RunLifecycle>,
}

impl std::ops::Deref for RunSessionGuard<'_> {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        self.session
    }
}

impl std::ops::DerefMut for RunSessionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.session
    }
}

impl Drop for RunSessionGuard<'_> {
    fn drop(&mut self) {
        if !self.lifecycle.finished.load(Ordering::Acquire) {
            // Drop cannot report an I/O error, but attempting the append here
            // closes the old gap where a deliberate stream drop followed by a
            // process crash was mistaken for an unclean tool interruption.
            let _ = persist_pending_cancellations(self.session);
        }
    }
}

/// A stateful agent: one session, one model, one authoritative head.
///
/// The agent owns its [`Session`]; runs borrow the agent mutably
/// (`&mut self`), so there is exactly one mutable head and no cloned or
/// detached conversation state.
pub struct Agent {
    client: AiClient,
    model: Model,
    session: Session,
    extensions: ExtensionHost,
    sandbox: SandboxConfig,
    effect_broker: EffectBroker,
    system: String,
    max_turns: Option<u64>,
    reasoning: ReasoningConfig,
    reasoning_mode: ReasoningMode,
    cache_retention: CacheRetention,
    /// Optional provider route used for autonomous context summaries.
    /// Defaults to the active model when unset.
    compaction_model: Option<Model>,
    auto_compaction_mode: AgentCompactionMode,
    compaction_threshold_fraction: f64,
    compaction_keep_recent_tokens: u64,
    session_id: String,
    resource_owner: String,
    tool_scope: String,
    completion_policy: CompletionPolicy,
    output_modalities: OutputModalities,
    max_output_tokens: u64,
    /// Stable semantic source key persisted with user-submitted prompts.
    prompt_model_source: Option<String>,
    prompt_color: Option<String>,
    /// One-shot user-visible text for the next prompt. Model-only context is
    /// persisted in the message body for exact replay instead.
    prompt_display_text: Option<String>,
    max_session_tokens: Option<u64>,
    max_session_cost_microdollars: Option<u64>,
    provider_retries_enabled: bool,
    /// Child sessions owned by the delegation manager are observed by their
    /// parent even when they do not carry a nested delegation binding.
    ultra_observation_managed: bool,
    delegation: Option<DelegationBinding>,
    last_run_lifecycle: Option<Arc<RunLifecycle>>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        if self
            .last_run_lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.dropped.load(Ordering::Acquire))
        {
            // Persist cancellation before the session owner disappears. This
            // makes dropping a run safe even when the next agent is reopened
            // from the same session file rather than reusing this Agent.
            let _ = persist_pending_cancellations(&mut self.session);
        }

        if let Some(delegation) = &self.delegation {
            delegation.request_shutdown();
        }

        // Tool process groups are owned by per-call RAII guards. There are no
        // persistent shell sessions to clean up when the agent is dropped.
    }
}

/// Aggregate result of [`Agent::complete`].
#[derive(Debug)]
pub struct RunOutput {
    /// Concatenated visible text from all turns.
    pub text: String,
    /// Completed generated media from committed turns, in event order.
    pub media: Vec<Media>,
    /// Total token usage across the run.
    pub usage: Usage,
    /// Total microdollar cost accrued during this run.
    pub cost_microdollars: u64,
    /// Session entry ID after the run.
    pub head: EntryId,
    /// How the run ended (never [`FinishReason::Failed`]; failures are
    /// returned as `Err` instead).
    pub reason: FinishReason,
}

/// Conservative estimate of the model-visible input for the next request.
///
/// `structural_tokens` comes from Ygg's request serializer. When available,
/// `provider_tokens` is the latest tokenizer measurement for the same route
/// and model after the latest compaction, plus structurally estimated trailing
/// messages. `input_tokens` is the larger of those two values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestContextEstimate {
    /// Structural estimate of the complete provider request.
    pub structural_tokens: u64,
    /// Provider-authoritative prefix measurement reconciled to the current head.
    pub provider_tokens: Option<u64>,
    /// Conservative input estimate used by autonomous capacity checks.
    pub input_tokens: u64,
}

/// A streaming agent run: the event stream plus a clonable control handle.
///
/// The run is driven by the caller — poll it with [`Run::next`] (or as a
/// [`Stream`]), typically inside `tokio::select!` alongside user input.
/// Dropping the run cancels the in-flight model stream and any running tool
/// (child processes included).
pub struct Run<'a> {
    stream: Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'a>>,
    control: RunControl,
    lifecycle: Arc<RunLifecycle>,
    context: Arc<ContextTracker>,
    delegation: Option<DelegationBinding>,
}

impl Run<'_> {
    /// Open an extension-negotiated child session by its opaque presentation
    /// reference while this run is active.
    ///
    /// Mirrors [`Agent::open_delegated_session_reference`] so live UI (for
    /// example the mid-run `/subagents` transcript drill-in) can read a worker
    /// transcript read-only without owning the root session. The delegation
    /// manager state lock is only taken to resolve the reference to a path.
    pub fn open_delegated_session_reference(
        &self,
        extension_principal: &str,
        reference: &str,
    ) -> Result<Option<Session>, AgentError> {
        let Some(binding) = self.delegation.as_ref() else {
            return Ok(None);
        };
        binding.open_session_reference(extension_principal, reference)
    }

    /// Returns a clonable handle for sending control messages while the run's
    /// event stream is being consumed.
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }

    /// Returns an owned snapshot of incrementally tracked response,
    /// tool-boundary, and provider token-usage state.
    pub fn context_snapshot(&self) -> ContextSnapshot {
        self.context.snapshot()
    }

    /// Consumes the run and returns its settled context snapshot.
    ///
    /// An unfinished run is first marked as dropped, matching the normal
    /// cancellation semantics of [`Drop`]. A run that already delivered its
    /// terminal event retains that terminal state.
    pub fn into_context_snapshot(self) -> ContextSnapshot {
        let context = Arc::clone(&self.context);
        drop(self);
        context.snapshot()
    }

    /// Returns the next event, or `None` after the terminal
    /// [`AgentEvent::RunFinished`] has been delivered.
    pub async fn next(&mut self) -> Option<AgentEvent> {
        self.stream.next().await
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        if !self.lifecycle.finished.load(Ordering::Acquire) {
            self.lifecycle.dropped.store(true, Ordering::Release);
            self.context.run_dropped();
            if let Some(delegation) = &self.delegation {
                delegation.request_shutdown();
            }
        }
    }
}

impl Stream for Run<'_> {
    type Item = AgentEvent;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

/// Clonable control handle for an active [`Run`].
#[derive(Clone)]
pub struct RunControl {
    tx: mpsc::Sender<Control>,
    abort: Arc<AbortFlag>,
}

impl RunControl {
    /// Injects input into the conversation at the next model-turn boundary of
    /// the active run (persisted to the session when applied).
    pub async fn steer(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .send(Control::Steer(input.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Attempts to enqueue steering without allowing a producer to wait behind
    /// the run's bounded control queue.
    pub(crate) fn try_steer(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .try_send(Control::Steer(input.into()))
            .map_err(|_| AgentError::RunEnded)
    }

    /// Queues input for after the current run settles: when the model completes
    /// a turn without tool calls, the run continues with this input instead of
    /// finishing.
    pub async fn follow_up(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .send(Control::FollowUp(input.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Requests a final answer at the next safe turn boundary. The supplied
    /// input is persisted like steering, but subsequent requests in this run
    /// expose no tools.
    pub async fn finish_now(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .send(Control::FinishNow(input.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Attempts to enqueue a follow-up without allowing a producer to wait
    /// behind the run's bounded control queue.
    pub(crate) fn try_follow_up(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .try_send(Control::FollowUp(input.into()))
            .map_err(|_| AgentError::RunEnded)
    }

    /// Changes how pending steering messages are delivered.
    pub async fn set_steering_mode(&self, mode: QueueDeliveryMode) -> Result<(), AgentError> {
        self.tx
            .send(Control::SetSteeringMode(mode))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Changes how pending follow-up messages are delivered.
    pub async fn set_follow_up_mode(&self, mode: QueueDeliveryMode) -> Result<(), AgentError> {
        self.tx
            .send(Control::SetFollowUpMode(mode))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Aborts the run at the next safe boundary: the in-flight model stream is
    /// dropped (cancelling the request) or the running tool is cancelled (child
    /// processes killed). All already-completed session entries are preserved
    /// and the run finishes with exactly one
    /// [`AgentEvent::RunFinished`]`{ reason: FinishReason::Aborted }`.
    pub fn abort(&self) {
        self.abort.set();
    }
}

/// Level-triggered abort signal: reliable regardless of channel capacity and
/// observable both by polling (`is_set`) and awaiting (`wait`).
#[derive(Default)]
struct AbortFlag {
    set: AtomicBool,
    notify: tokio::sync::Notify,
    cancellation: CancellationToken,
}

impl AbortFlag {
    fn set(&self) {
        self.set.store(true, Ordering::Release);
        self.cancellation.cancel();
        self.notify.notify_waiters();
    }

    fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

fn user_message(input: UserInput) -> EntryValue {
    EntryValue::Message(Message::User(UserMessage {
        content: input.into_user_parts(),
    }))
}

struct ObserverDispatch {
    observers: Vec<Arc<dyn EventObserver>>,
    resource_owner: String,
}

fn notify_observers(observers: &ObserverDispatch, event: &AgentEvent) {
    for observer in &observers.observers {
        observer.on_event_for_owner(event, &observers.resource_owner);
    }
}

/// Minimum context headroom retained for an ordinary coding turn. This is a
/// compaction policy, not the provider request's output ceiling.
const DEFAULT_COMPACTION_RESERVE_TOKENS: u64 = 16 * 1024;
/// Leave room for a visible answer after token-budget reasoning when the model
/// advertises enough output capacity.
const REASONING_ANSWER_RESERVE: u64 = 1024;
/// Bound actual tool executions emitted in one assistant turn. Every excess
/// call still receives a compact error result so provider pairing remains valid.
const MAX_TOOL_CALLS_PER_TURN: usize = 32;
/// Number of recent identical calls retained for the generic no-progress hint.
const MAX_RECENT_TOOL_CALLS: usize = 16;
/// Do not distract the model for the first two legitimate repeated probes.
const REPEATED_TOOL_CALL_THRESHOLD: usize = 2;
const FAILED_TURN_CONTEXT_MARKER: &str = "The previous assistant turn failed before completion. Do not continue that request unless the user asks again.";
const TOOL_TRUNCATION_MARKER: &str = "\n[tool output truncated]\n";
/// Maximum retries for a transient provider failure. A replacement attempt is
/// safe even after deltas were received: streamed output is provisional, the
/// assistant message is persisted only after `Finished`, and tools are not
/// executed until that point.
const MAX_PROVIDER_RETRIES: usize = 3;
/// Non-timeout network failures are usually short-lived connection loss. Five
/// visible, cancellable replacement attempts give the connection time to
/// recover without charging usage or consuming an autonomous model turn.
const MAX_NETWORK_RETRIES: usize = 5;
const TERMINAL_GATE_SYSTEM: &str = r#"You gate control flow for a coding agent. Output R when the candidate is a valid response to return to the user now: a substantiated completion, an answer or plan based on supplied text or general knowledge, a necessary clarification, an honest blocker or uncertainty, or a justified refusal. Output C when autonomous work should continue: promised next action, unsupported claim about current state, or requested repository or external action not substantiated by relevant successful action evidence. Do not treat an irrelevant or failed action as evidence. Respect explicit requests not to use tools or to guess. Output exactly R or C."#;
const TERMINAL_GATE_CORRECTION: &str = "The candidate response was not returnable: requested current-state or action work is not supported by relevant successful tool evidence. Continue the work using the available tools; do not repeat the rejected candidate.";
const TERMINAL_GATE_ATTEMPTS: usize = 2;
const TERMINAL_GATE_TEXT_LIMIT: usize = 3_000;
const TERMINAL_GATE_RECEIPT_LIMIT: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalGateDecision {
    Return,
    Continue,
}

#[derive(Debug)]
struct TerminalActionReceipt {
    tool: String,
    arguments: String,
    status: &'static str,
    result: String,
}

struct CompletedToolExecution {
    result: Result<ToolOutput, ToolError>,
    /// Wall time taken for the call.
    duration: std::time::Duration,
    /// Unix milliseconds just before the tool's effects were admitted
    /// (`None` when the call never reached the effect gate).
    started_unix_ms: Option<u64>,
    /// Unix milliseconds when the call's outcome was finalized.
    finished_unix_ms: Option<u64>,
    progress_rx: mpsc::Receiver<ToolProgress>,
    progress_sink: ToolProgressSink,
    cancellation_won: bool,
}

/// Synthetic, secret-safe result for a normalized call rejected by the exact
/// request schema. Keep this static: provider arguments may contain secrets.
const SCHEMA_MISMATCH_TOOL_ERROR: &str =
    "tool call was not executed because its arguments do not satisfy the advertised schema; correct the arguments and try again";

fn rejected_argument_tool_error(error: ToolCallArgumentError) -> ToolError {
    let message = match error {
        ToolCallArgumentError::SchemaMismatch => SCHEMA_MISMATCH_TOOL_ERROR,
    };
    ToolError::new(message)
}

fn rejected_argument_tool_execution(error: ToolCallArgumentError) -> CompletedToolExecution {
    let (progress_tx, progress_rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
    CompletedToolExecution {
        result: Err(rejected_argument_tool_error(error)),
        duration: std::time::Duration::ZERO,
        started_unix_ms: None,
        finished_unix_ms: Some(crate::session::now_unix_millis()),
        progress_rx,
        progress_sink: ToolProgressSink::live(progress_tx),
        cancellation_won: false,
    }
}

fn effect_is_repeatable_observation(effect: ToolEffect) -> bool {
    matches!(effect, ToolEffect::Pure | ToolEffect::WorkspaceRead)
}

#[allow(clippy::too_many_arguments)]
async fn reserve_tool_effect(
    broker: &EffectBroker,
    tool: &dyn Tool,
    name: &str,
    arguments: &serde_json::Value,
    context: &ToolContext<'_>,
    principal: &str,
    run_id: &str,
    generation: u64,
    request_id: &ygg_ai::ToolCallId,
    interactive: bool,
) -> Result<(EffectIntent, EffectReservation), ToolError> {
    let effect = tool.effect(arguments, context)?;
    let intent = EffectIntent::new(
        principal,
        run_id,
        generation,
        request_id.0.clone(),
        name,
        effect,
        arguments,
    )
    .map_err(|error| ToolError::new(error.to_string()))?;
    let reservation = broker
        .reserve(&intent, interactive.then_some(&context.progress))
        .await
        .map_err(|error| ToolError::new(error.to_string()))?;
    Ok((intent, reservation))
}

#[allow(clippy::too_many_arguments)]
async fn execute_parallel_tool_call(
    tool: Arc<dyn Tool>,
    hooks: &[Arc<dyn ToolCallHook>],
    broker: &EffectBroker,
    run_id: &str,
    generation: u64,
    request_id: &ygg_ai::ToolCallId,
    name: &str,
    arguments: serde_json::Value,
    sandbox: &SandboxConfig,
    tool_scope: &str,
    resource_owner: &str,
    active_skills: &[crate::session::SkillActivatedSnapshot],
    registered_tools: &[String],
    cancellation: CancellationToken,
) -> CompletedToolExecution {
    let start = std::time::Instant::now();
    let mut started_unix_ms: Option<u64> = None;
    let (progress_tx, progress_rx) = mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
    let progress_sink = ToolProgressSink::live(progress_tx);
    let tool_ctx = ToolContext {
        workspace: &sandbox.workspace,
        sandbox,
        execution_scope: tool_scope,
        resource_owner,
        active_skills,
        registered_tools,
        progress: progress_sink.clone(),
        cancellation: cancellation.clone(),
    };

    let admission = reserve_tool_effect(
        broker,
        tool.as_ref(),
        name,
        &arguments,
        &tool_ctx,
        resource_owner,
        run_id,
        generation,
        request_id,
        false,
    )
    .await;
    let mut reservation = None;
    let mut hook_denial = match admission {
        Ok(admission) => {
            reservation = Some(admission);
            None
        }
        Err(error) => Some(error),
    };
    if reservation.is_some() {
        for hook in hooks {
            if let Err(error) = hook.before_tool_call(name, &arguments, &tool_ctx).await {
                hook_denial = Some(error);
                break;
            }
        }
    }

    let mut committed = false;
    let mut cancellation_won = false;
    let result = if let Some(error) = hook_denial {
        if cancellation.is_cancelled() {
            cancellation_won = true;
            Err(cancelled_tool_error())
        } else {
            Err(error)
        }
    } else if cancellation.is_cancelled() {
        cancellation_won = true;
        Err(cancelled_tool_error())
    } else {
        // Preserve the original arguments for after-call hooks, but complete
        // this potentially large allocation before consuming admission.
        let execute_arguments = arguments.clone();
        if cancellation.is_cancelled() {
            cancellation_won = true;
            return CompletedToolExecution {
                result: Err(cancelled_tool_error()),
                progress_rx,
                progress_sink,
                cancellation_won,
                duration: start.elapsed(),
                started_unix_ms: None,
                finished_unix_ms: Some(crate::session::now_unix_millis()),
            };
        }
        let (intent, effect_reservation) = reservation
            .take()
            .expect("successful admission retains its exact reservation");
        match effect_reservation.commit(&intent) {
            Err(error) => Err(ToolError::new(error.to_string())),
            Ok(_receipt) => {
                committed = true;
                started_unix_ms = Some(crate::session::now_unix_millis());
                let execute = tool.execute(execute_arguments, &tool_ctx);
                tokio::pin!(execute);
                let execution_result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(cancelled_tool_error()),
                    result = &mut execute => result,
                };
                if cancellation.is_cancelled() {
                    cancellation_won = true;
                    Err(cancelled_tool_error())
                } else {
                    execution_result
                }
            }
        }
    };

    if committed {
        let (output, is_error) = match &result {
            Ok(output) => (output.text.as_str(), output.is_error()),
            Err(error) => (error.message.as_str(), true),
        };
        for hook in hooks {
            hook.after_tool_call(name, &arguments, output, is_error, &tool_ctx)
                .await;
        }
    }

    CompletedToolExecution {
        result,
        progress_rx,
        progress_sink,
        cancellation_won,
        duration: start.elapsed(),
        started_unix_ms,
        finished_unix_ms: Some(crate::session::now_unix_millis()),
    }
}

/// A bash call whose execution started while the provider response was still
/// streaming. The result is only consumed if the authoritative tool call
/// matches the speculated arguments exactly; otherwise it is cancelled and
/// the call re-executes serially.
struct SpeculativeExecution {
    arguments: serde_json::Value,
    handle: tokio::task::JoinHandle<CompletedToolExecution>,
    cancellation: CancellationToken,
}

/// A bash tool call observed mid-stream, with its argument deltas
/// accumulated from `ToolCallArgsDelta` events. This shadow copy exists only
/// to decide speculation; the authoritative parsed call always comes from the
/// finished response.
struct PartialSpeculativeCall {
    id: ygg_ai::ToolCallId,
    args_json: String,
}

/// Per-run speculative bash state. Tracks partially streamed calls during a
/// provider turn and in-flight speculative executions, reconciling them at
/// the normal commit point. Dropping the state cancels every in-flight
/// execution; leaked tasks then observe cancellation and finish promptly.
#[derive(Default)]
struct SpeculativeBash {
    partial: HashMap<usize, PartialSpeculativeCall>,
    active: HashMap<ygg_ai::ToolCallId, SpeculativeExecution>,
}

impl SpeculativeBash {
    /// Cancels leftovers from a previous provider turn and forgets partial
    /// accumulation. Called at every turn boundary so a retried stream never
    /// collides with stale indexes or identifiers.
    fn begin_turn(&mut self) {
        self.partial.clear();
        for (_, entry) in self.active.drain() {
            entry.cancellation.cancel();
        }
    }

    fn note_start(&mut self, index: usize, id: ygg_ai::ToolCallId, name: String) {
        if name == "bash" {
            self.partial.insert(
                index,
                PartialSpeculativeCall {
                    id,
                    args_json: String::new(),
                },
            );
        } else {
            // A different tool invalidates nothing else; only bash calls are
            // tracked, keyed by their own part index.
            self.partial.remove(&index);
        }
    }

    fn note_args_delta(&mut self, index: usize, delta: &str) {
        if let Some(partial) = self.partial.get_mut(&index) {
            partial.args_json.push_str(delta);
        }
    }

    /// Drops an unchecked or schema-rejected partial call before it can enter
    /// the speculative execution path.
    fn discard(&mut self, index: usize) {
        self.partial.remove(&index);
    }

    /// Completes a tracked call: parses the accumulated arguments and removes
    /// the partial entry. Returns `None` when the JSON does not parse (the
    /// authoritative path will surface the same failure later).
    fn complete(&mut self, index: usize) -> Option<(ygg_ai::ToolCallId, serde_json::Value)> {
        let partial = self.partial.remove(&index)?;
        let arguments = serde_json::from_str(&partial.args_json).ok()?;
        Some((partial.id, arguments))
    }

    fn insert_active(
        &mut self,
        id: ygg_ai::ToolCallId,
        arguments: serde_json::Value,
        handle: tokio::task::JoinHandle<CompletedToolExecution>,
        cancellation: CancellationToken,
    ) {
        self.active.insert(
            id,
            SpeculativeExecution {
                arguments,
                handle,
                cancellation,
            },
        );
    }

    /// Consumes the speculative execution for `id` when its arguments match
    /// the authoritative parsed call. On mismatch — or when no speculation
    /// exists — returns `None` so the caller executes serially; mismatched
    /// executions are cancelled, never surfaced.
    async fn take_matched(
        &mut self,
        id: &ygg_ai::ToolCallId,
        authoritative: Option<&serde_json::Value>,
    ) -> Option<CompletedToolExecution> {
        let entry = self.active.remove(id)?;
        match authoritative {
            Some(value) if *value == entry.arguments => {
                // A panicked task falls back to full serial execution.
                entry.handle.await.ok()
            }
            _ => {
                entry.cancellation.cancel();
                None
            }
        }
    }
}

impl Drop for SpeculativeBash {
    fn drop(&mut self) {
        for (_, entry) in self.active.drain() {
            entry.cancellation.cancel();
        }
    }
}

/// Spawns one speculative bash execution with its own cancellation token so
/// it can be discarded independently of the run's abort flag while still
/// observing run aborts.
#[allow(clippy::too_many_arguments)]
fn spawn_speculative_execution(
    tool: Arc<dyn Tool>,
    broker: EffectBroker,
    run_id: String,
    generation: u64,
    request_id: ygg_ai::ToolCallId,
    name: String,
    arguments: serde_json::Value,
    sandbox: SandboxConfig,
    tool_scope: String,
    resource_owner: String,
    abort: Arc<AbortFlag>,
) -> (
    tokio::task::JoinHandle<CompletedToolExecution>,
    CancellationToken,
) {
    let cancellation = CancellationToken::default();
    let execution_token = cancellation.clone();
    let hooks: Vec<Arc<dyn ToolCallHook>> = Vec::new();
    let handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = abort.wait() => {
                let (progress_tx, progress_rx) =
                    mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
                CompletedToolExecution {
                    result: Err(cancelled_tool_error()),
                    progress_rx,
                    progress_sink: ToolProgressSink::live(progress_tx),
                    cancellation_won: true,
                    duration: std::time::Duration::ZERO,
                    started_unix_ms: None,
                    finished_unix_ms: None,
                }
            }
            execution = execute_parallel_tool_call(
                tool,
                &hooks,
                &broker,
                &run_id,
                generation,
                &request_id,
                &name,
                arguments,
                &sandbox,
                &tool_scope,
                &resource_owner,
                &[],
                &[],
                execution_token.clone(),
            ) => execution,
        }
    });
    (handle, cancellation)
}

fn bounded_gate_text(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let half = max_chars.saturating_sub(32) / 2;
    let head = text.chars().take(half).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(half)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head}\n[… {count} chars total …]\n{tail}")
}

fn message_visible_text(message: &Message) -> Option<String> {
    let text = match message {
        Message::User(user) => user
            .content
            .iter()
            .filter_map(|part| match part {
                UserPart::Text(text) => Some(text.as_str()),
                UserPart::Media(Media::Audio(audio)) => audio
                    .transcript
                    .as_deref()
                    .filter(|transcript| !transcript.trim().is_empty())
                    .or(Some("[audio]")),
                UserPart::Media(Media::Image(_)) => Some("[image]"),
                UserPart::ToolResult(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                AssistantPart::Media(Media::Audio(audio)) => audio
                    .transcript
                    .as_deref()
                    .filter(|transcript| !transcript.trim().is_empty())
                    .or(Some("[generated audio]")),
                AssistantPart::Media(Media::Image(_)) => Some("[generated image]"),
                AssistantPart::Reasoning(_) | AssistantPart::ToolCall(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    (!text.trim().is_empty()).then_some(text)
}

fn recent_conversational_context(messages: &[Message]) -> String {
    let mut selected = messages
        .iter()
        .rev()
        .filter_map(message_visible_text)
        .take(2)
        .collect::<Vec<_>>();
    selected.reverse();
    bounded_gate_text(&selected.join("\n---\n"), TERMINAL_GATE_TEXT_LIMIT)
}

fn terminal_gate_capsule(
    prior_context: &str,
    requests: &[String],
    candidate: &AssistantMessage,
    receipts: &[TerminalActionReceipt],
) -> String {
    let candidate =
        message_visible_text(&Message::Assistant(candidate.clone())).unwrap_or_default();
    let omitted = receipts.len().saturating_sub(TERMINAL_GATE_RECEIPT_LIMIT);
    let receipts = if receipts.len() <= TERMINAL_GATE_RECEIPT_LIMIT {
        receipts.iter().collect::<Vec<_>>()
    } else {
        let half = TERMINAL_GATE_RECEIPT_LIMIT / 2;
        receipts[..half]
            .iter()
            .chain(receipts[receipts.len() - half..].iter())
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "prior_context": bounded_gate_text(prior_context, TERMINAL_GATE_TEXT_LIMIT),
        "requests": requests.iter().map(|text| bounded_gate_text(text, TERMINAL_GATE_TEXT_LIMIT)).collect::<Vec<_>>(),
        "candidate": bounded_gate_text(&candidate, TERMINAL_GATE_TEXT_LIMIT),
        "actions_omitted": omitted,
        "actions": receipts.iter().map(|receipt| serde_json::json!({
            "tool": receipt.tool,
            "arguments": bounded_gate_text(&receipt.arguments, 400),
            "status": receipt.status,
            "result": bounded_gate_text(&receipt.result, 600),
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

fn parse_terminal_gate(response: &ygg_ai::Response) -> Option<TerminalGateDecision> {
    if !matches!(
        response.stop_reason,
        StopReason::EndTurn | StopReason::StopSequence
    ) {
        return None;
    }
    match assistant_text(response)?.trim() {
        "R" => Some(TerminalGateDecision::Return),
        "C" => Some(TerminalGateDecision::Continue),
        _ => None,
    }
}

fn continuation_instruction(stop_reason: &StopReason) -> &'static str {
    match stop_reason {
        StopReason::MaxTokens => {
            "The previous response was truncated at the token limit. Continue the task from the persisted state; do not claim completion until the work is finished and verified."
        }
        StopReason::Other(reason) if reason == "tool_output_locked" => {
            "The previous response emitted an internal locked-output placeholder instead of the intended structured call. Re-issue that tool call now using the provider's required tool-call format; do not print any control placeholder."
        }
        _ => {
            "The provider paused the turn. Continue the task from the persisted state and do not claim completion until the work is finished and verified."
        }
    }
}

fn next_tool_scope() -> String {
    static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);
    format!(
        "agent-{}-{}",
        std::process::id(),
        NEXT_SCOPE.fetch_add(1, Ordering::Relaxed)
    )
}

fn reasoning_token_budget(model: &Model, reasoning: &ReasoningConfig) -> u64 {
    match reasoning {
        ReasoningConfig::Budget(budget) => *budget,
        ReasoningConfig::Effort(effort) => model
            .spec
            .capabilities
            .reasoning
            .as_ref()
            .filter(|capability| capability.control == ygg_ai::ReasoningControl::TokenBudget)
            .and_then(|capability| {
                let budgets = capability.effort_budgets?;
                let effort = (*effort).min(capability.max_effort);
                Some(match effort {
                    ygg_ai::ReasoningEffort::Minimal => budgets.minimal,
                    ygg_ai::ReasoningEffort::Low => budgets.low,
                    ygg_ai::ReasoningEffort::Medium => budgets.medium,
                    ygg_ai::ReasoningEffort::High => budgets.high,
                    ygg_ai::ReasoningEffort::Xhigh => budgets.xhigh,
                    ygg_ai::ReasoningEffort::Max | ygg_ai::ReasoningEffort::Ultra => budgets.max,
                })
            })
            .unwrap_or_default(),
        ReasoningConfig::Off | ReasoningConfig::On => 0,
    }
}

fn agent_compaction_reserve_tokens(model: &Model, reasoning: &ReasoningConfig) -> u64 {
    let model_max = model.spec.limits.max_output_tokens.max(1);
    let reasoning_floor = reasoning_token_budget(model, reasoning)
        .saturating_add(REASONING_ANSWER_RESERVE)
        .min(model_max);
    DEFAULT_COMPACTION_RESERVE_TOKENS
        .max(reasoning_floor)
        .min(model_max)
}

fn resolve_request_max_output_tokens(
    context_window: u64,
    input_tokens: u64,
    provider_output_ceiling: u64,
) -> u64 {
    provider_output_ceiling.min(context_window.saturating_sub(input_tokens))
}

fn add_usage(total: &mut Usage, turn: &Usage) {
    total.input_tokens = total.input_tokens.saturating_add(turn.input_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(turn.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(turn.cache_write_tokens);
    total.cache_write_1h_tokens = total
        .cache_write_1h_tokens
        .saturating_add(turn.cache_write_1h_tokens);
    total.output_tokens = total.output_tokens.saturating_add(turn.output_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(turn.reasoning_tokens);
    total.total_tokens = total.total_tokens.saturating_add(turn.total_tokens);
}

fn usage_since(after: Usage, before: Usage) -> Usage {
    Usage {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        cache_read_tokens: after
            .cache_read_tokens
            .saturating_sub(before.cache_read_tokens),
        cache_write_tokens: after
            .cache_write_tokens
            .saturating_sub(before.cache_write_tokens),
        cache_write_1h_tokens: after
            .cache_write_1h_tokens
            .saturating_sub(before.cache_write_1h_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        reasoning_tokens: after
            .reasoning_tokens
            .saturating_sub(before.reasoning_tokens),
        total_tokens: after.total_tokens.saturating_sub(before.total_tokens),
    }
}

#[derive(Default)]
struct CostAccumulator {
    microdollars: u64,
    picodollars_remainder: u32,
}

impl CostAccumulator {
    /// Aggregate a request after its usage record durably updates the session.
    /// Models without pricing contribute zero.
    fn add(&mut self, cost: Option<Cost>) {
        let Some(cost) = cost else {
            return;
        };
        let remainder = u64::from(self.picodollars_remainder)
            .saturating_add(u64::from(cost.total_picodollars_remainder));
        let carry = remainder / u64::from(PICODOLLARS_PER_MICRODOLLAR);
        self.microdollars = self
            .microdollars
            .saturating_add(cost.total)
            .saturating_add(carry);
        self.picodollars_remainder = (remainder % u64::from(PICODOLLARS_PER_MICRODOLLAR)) as u32;
    }
}

fn active_branch_entries(session: &Session) -> Vec<&crate::session::Entry> {
    let mut reverse = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(&id) else {
            break;
        };
        cursor = entry.parent.clone();
        reverse.push(entry);
    }
    reverse.reverse();
    reverse
}

fn resolve_tool_delivery_after_persistence(
    result: &Result<ToolOutput, ToolError>,
    text_limit: usize,
) {
    if let Ok(output) = result {
        output.resolve_delivery(output.text.len() <= text_limit);
    }
}

fn cancelled_tool_error() -> ToolError {
    ToolError::new(
        "tool execution cancelled by user; state may be partially changed and must not be replayed automatically",
    )
}

fn pending_tool_state(session: &Session) -> Option<(Vec<ToolCall>, HashSet<ygg_ai::ToolCallId>)> {
    let mut persisted = HashSet::new();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let entry = session.entry(id)?;
        match &entry.value {
            EntryValue::Message(Message::Assistant(assistant)) => {
                let calls = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::ToolCall(call) => Some(call.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                return (!calls.is_empty()).then_some((calls, persisted));
            }
            EntryValue::Message(Message::User(user)) => {
                for part in &user.content {
                    let UserPart::ToolResult(result) = part else {
                        continue;
                    };
                    persisted.insert(result.tool_call_id.clone());
                }
            }
            EntryValue::Compaction { .. }
            | EntryValue::ResponsesTurn { .. }
            | EntryValue::ResponsesCompaction { .. }
            | EntryValue::Config { .. }
            | EntryValue::PromptTemplateSelected { .. }
            | EntryValue::SkillActivated { .. }
            | EntryValue::SkillResourceRead { .. }
            | EntryValue::SkillDeactivated { .. } => {}
        }
        cursor = entry.parent.as_ref();
    }
    None
}

fn tool_call_arguments_fingerprint(name: &str, args: &serde_json::Value) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    if serde_json::to_writer(&mut bytes, args).is_err() {
        bytes.extend_from_slice(b"<invalid-json>");
    }
    content_hash(&bytes)
}

fn repeated_tool_annotation(repeated_recently: usize) -> String {
    format!(
        "\n[agent diagnostic: exact call repeated {}x recently; if no progress, change approach or verify state.]",
        repeated_recently.saturating_add(1)
    )
}

fn annotate_repeated_tool_result(
    result: Result<ToolOutput, ToolError>,
    repeated_recently: usize,
) -> Result<ToolOutput, ToolError> {
    if repeated_recently < REPEATED_TOOL_CALL_THRESHOLD {
        return result;
    }
    let annotation = repeated_tool_annotation(repeated_recently);
    match result {
        Ok(output) if serde_json::from_str::<serde_json::Value>(&output.text).is_ok() => {
            // Keep machine-readable tool contracts valid. A trailing hint
            // would turn an otherwise valid JSON result into invalid JSON;
            // the next model turn can still be diagnosed from telemetry.
            Ok(output)
        }
        Ok(output) => Ok(output.with_model_annotation(&annotation)),
        Err(error) if serde_json::from_str::<serde_json::Value>(&error.message).is_ok() => {
            Err(error)
        }
        Err(error) => Err(ToolError::new(format!("{}{}", error.message, annotation))),
    }
}

fn assistant_has_terminal_content(assistant: &AssistantMessage) -> bool {
    assistant.content.iter().any(|part| match part {
        AssistantPart::Text(text) => !text.trim().is_empty(),
        AssistantPart::ToolCall(_) | AssistantPart::Media(_) => true,
        AssistantPart::Reasoning(_) => false,
    })
}

fn truncate_tool_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    if limit == 0 {
        return String::new();
    }
    if limit <= TOOL_TRUNCATION_MARKER.len() {
        return TOOL_TRUNCATION_MARKER[..limit].to_owned();
    }
    let available = limit - TOOL_TRUNCATION_MARKER.len();
    let head = available / 2;
    let tail = available - head;
    let mut result = String::with_capacity(limit);
    let mut head_end = head.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    result.push_str(&text[..head_end]);
    result.push_str(TOOL_TRUNCATION_MARKER);
    let mut tail_start = text.len().saturating_sub(tail);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    result.push_str(&text[tail_start..]);
    result
}

fn truncate_ordered_tool_text(
    content_parts: &[ToolOutputContentPart],
    limit: usize,
) -> Vec<Option<String>> {
    let mut lowered = vec![None; content_parts.len()];
    let text_indices = content_parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| matches!(part, ToolOutputContentPart::Text(_)).then_some(index))
        .collect::<Vec<_>>();
    let Some(&first_text_index) = text_indices.first() else {
        return lowered;
    };
    let total_text_bytes = text_indices.iter().fold(0usize, |total, &index| {
        let ToolOutputContentPart::Text(text) = &content_parts[index] else {
            unreachable!("text_indices contains only text parts");
        };
        total.saturating_add(text.len())
    });
    if total_text_bytes <= limit {
        for index in text_indices {
            let ToolOutputContentPart::Text(text) = &content_parts[index] else {
                unreachable!("text_indices contains only text parts");
            };
            lowered[index] = Some(text.clone());
        }
        return lowered;
    }
    if limit == 0 {
        lowered[first_text_index] = Some(String::new());
        return lowered;
    }
    if limit <= TOOL_TRUNCATION_MARKER.len() {
        lowered[first_text_index] = Some(TOOL_TRUNCATION_MARKER[..limit].to_owned());
        return lowered;
    }

    let available = limit - TOOL_TRUNCATION_MARKER.len();
    let mut head_remaining = available / 2;
    let mut tail_remaining = available - head_remaining;
    let mut prefixes = vec![String::new(); content_parts.len()];
    let mut suffixes = vec![String::new(); content_parts.len()];

    for &index in &text_indices {
        if head_remaining == 0 {
            break;
        }
        let ToolOutputContentPart::Text(text) = &content_parts[index] else {
            unreachable!("text_indices contains only text parts");
        };
        let mut end = head_remaining.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        prefixes[index].push_str(&text[..end]);
        head_remaining -= end;
    }
    for &index in text_indices.iter().rev() {
        if tail_remaining == 0 {
            break;
        }
        let ToolOutputContentPart::Text(text) = &content_parts[index] else {
            unreachable!("text_indices contains only text parts");
        };
        let mut start = text.len().saturating_sub(tail_remaining);
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        suffixes[index].push_str(&text[start..]);
        tail_remaining -= text.len() - start;
    }

    let marker_index = text_indices
        .iter()
        .rev()
        .copied()
        .find(|&index| !prefixes[index].is_empty())
        .unwrap_or(first_text_index);
    for index in text_indices {
        let mut text = std::mem::take(&mut prefixes[index]);
        if index == marker_index {
            text.push_str(TOOL_TRUNCATION_MARKER);
        }
        text.push_str(&suffixes[index]);
        if !text.is_empty() {
            lowered[index] = Some(text);
        }
    }
    lowered
}

fn lower_tool_media_part(
    media: &Media,
    model: &Model,
    inline_media: bool,
    result_parts: &mut Vec<ToolResultPart>,
    adjacent_media: &mut Vec<Media>,
    accepted_kinds: &mut Vec<ToolOutputMediaKind>,
    omissions: &mut Vec<String>,
) {
    match media {
        Media::Image(_) => {
            if !model
                .spec
                .capabilities
                .input_modalities
                .contains(ygg_ai::Modality::Image)
            {
                omissions
                    .push("[image omitted: the active model does not accept image input]".into());
            } else {
                accepted_kinds.push(ToolOutputMediaKind::Image);
                if inline_media {
                    result_parts.push(ToolResultPart::Media(media.clone()));
                } else {
                    adjacent_media.push(media.clone());
                }
            }
        }
        Media::Audio(audio) => {
            if !model
                .spec
                .capabilities
                .input_modalities
                .contains(ygg_ai::Modality::Audio)
            {
                omissions
                    .push("[audio omitted: the active model does not accept audio input]".into());
            } else if model.spec.protocol != Protocol::OpenAiChat {
                omissions
                    .push("[audio omitted: this protocol cannot replay audio tool output]".into());
            } else if !matches!(
                audio.format,
                ygg_ai::AudioFormat::Wav | ygg_ai::AudioFormat::Mp3
            ) {
                omissions.push(format!(
                    "[audio omitted: OpenAI Chat accepts WAV or MP3 input, got {:?}]",
                    audio.format
                ));
            } else {
                accepted_kinds.push(ToolOutputMediaKind::Audio);
                adjacent_media.push(media.clone());
            }
        }
    }
}

fn lower_tool_result(
    call_id: ygg_ai::ToolCallId,
    result: &Result<ToolOutput, ToolError>,
    model: &Model,
    text_limit: usize,
    added_tool_names: Vec<String>,
) -> (
    UserMessage,
    Vec<ToolOutputMediaKind>,
    String,
    bool,
    Option<ToolOutputDetails>,
) {
    let (raw_text, is_error) = match result {
        Ok(output) => (output.text.as_str(), output.is_error()),
        Err(error) => (error.message.as_str(), true),
    };
    let persisted_text = truncate_tool_text(raw_text, text_limit);
    let mut result_parts = Vec::new();
    let mut adjacent_media = Vec::new();
    let mut accepted_kinds = Vec::new();
    let mut omissions = Vec::new();

    match result {
        Err(_) => result_parts.push(ToolResultPart::Text(persisted_text.clone())),
        Ok(output)
            if matches!(
                model.spec.protocol,
                Protocol::OpenAiResponses | Protocol::AnthropicMessages
            ) =>
        {
            let bounded_text = truncate_ordered_tool_text(output.content_parts(), text_limit);
            for (index, part) in output.content_parts().iter().enumerate() {
                match part {
                    ToolOutputContentPart::Text(_) => {
                        if let Some(text) = bounded_text[index].as_ref() {
                            result_parts.push(ToolResultPart::Text(text.clone()));
                        }
                    }
                    ToolOutputContentPart::Media(media) => lower_tool_media_part(
                        media,
                        model,
                        true,
                        &mut result_parts,
                        &mut adjacent_media,
                        &mut accepted_kinds,
                        &mut omissions,
                    ),
                }
            }
        }
        Ok(output) => {
            result_parts.push(ToolResultPart::Text(persisted_text.clone()));
            for media in output.media() {
                lower_tool_media_part(
                    media,
                    model,
                    false,
                    &mut result_parts,
                    &mut adjacent_media,
                    &mut accepted_kinds,
                    &mut omissions,
                );
            }
        }
    }
    result_parts.extend(omissions.iter().cloned().map(ToolResultPart::Text));
    let effective_is_error = is_error
        || result
            .as_ref()
            .is_ok_and(|output| !output.media().is_empty() && accepted_kinds.is_empty());
    let presented_text = if omissions.is_empty() {
        persisted_text.clone()
    } else if persisted_text.is_empty() {
        omissions.join("\n")
    } else {
        format!("{persisted_text}\n{}", omissions.join("\n"))
    };

    let mut content = Vec::with_capacity(1 + adjacent_media.len());
    content.push(UserPart::ToolResult(ToolResult {
        tool_call_id: call_id,
        content: result_parts,
        is_error: effective_is_error,
        added_tool_names: (!added_tool_names.is_empty()).then_some(added_tool_names),
    }));
    content.extend(adjacent_media.into_iter().map(UserPart::Media));
    (
        UserMessage { content },
        accepted_kinds,
        presented_text,
        effective_is_error,
        result.as_ref().ok().and_then(ToolOutput::details).cloned(),
    )
}

fn persist_pending_cancellations(session: &mut Session) -> Result<(), AgentError> {
    let Some((calls, persisted)) = pending_tool_state(session) else {
        return Ok(());
    };
    let unresolved = calls
        .into_iter()
        .filter(|call| !persisted.contains(&call.id));
    for call in unresolved {
        let text = match call.argument_error {
            Some(argument_error) => rejected_argument_tool_error(argument_error).message,
            None => cancelled_tool_error().message,
        };
        session.append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::ToolResult(ToolResult {
                tool_call_id: call.id,
                content: vec![ToolResultPart::Text(text)],
                is_error: true,
                added_tool_names: None,
            })],
        })))?;
    }
    Ok(())
}

fn close_failed_turn(session: &mut Session, model: &Model) -> Result<(), AgentError> {
    let ends_with_user = {
        let context = session.context_ref()?;
        matches!(context.last(), Some(Message::User(_)))
    };
    if ends_with_user {
        session.append_with_metadata(
            EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text(FAILED_TURN_CONTEXT_MARKER.to_owned())],
                model: model.spec.id.clone(),
                protocol: model.spec.protocol,
            })),
            Some(EntryMetadata {
                local_synthetic_assistant: true,
                ..EntryMetadata::default()
            }),
        )?;
    }
    Ok(())
}

fn retryable_before_generation(error: &AiError) -> bool {
    // A wrapper is replayable before generation only if its inner failure is.
    if let AiError::StreamFailure { inner, .. } = error {
        return retryable_before_generation(inner);
    }
    match error {
        AiError::Http(error) => error.is_safe_to_retry(),
        AiError::Transport(error) => {
            !error.timeout && error.phase == ygg_ai::TransportPhase::Connect
        }
        _ => false,
    }
}

fn is_replayable_network_failure(error: &AiError) -> bool {
    // A mid-stream wrapper keeps the replayability of the failure that ended
    // the stream (e.g. a body disconnect that already streamed bytes is
    // replayable exactly as the bare transport error was).
    if let AiError::StreamFailure { inner, .. } = error {
        return is_replayable_network_failure(inner);
    }
    matches!(
        error,
        AiError::Transport(transport)
            if !transport.timeout
                && matches!(
                    transport.phase,
                    ygg_ai::TransportPhase::Connect | ygg_ai::TransportPhase::Body
                )
    )
}

fn looks_like_context_error(error: &AiError) -> bool {
    // A mid-stream wrapper delegates classification to the failure that
    // actually ended the stream: a provider context-error frame inside a 2xx
    // stream must still be detected, while a wrapped transport timeout must
    // still never be mistaken for context overflow.
    if let AiError::StreamFailure { inner, .. } = error {
        return looks_like_context_error(inner);
    }
    // Transport timeouts often contain phrases such as "context deadline
    // exceeded". They are connectivity failures, not evidence that model
    // history is too large, and must never destroy full-fidelity context.
    if matches!(error, AiError::Transport(_)) {
        return false;
    }
    if matches!(error, AiError::Http(http) if http.status.as_u16() == 429)
        || matches!(
            error,
            AiError::Provider(provider)
                if provider.code.as_deref().is_some_and(|code| {
                    let code = code.to_ascii_lowercase();
                    code.contains("rate_limit") || code.contains("throttl")
                }) || provider.kind.as_deref().is_some_and(|kind| {
                    let kind = kind.to_ascii_lowercase();
                    kind.contains("rate_limit") || kind.contains("throttl")
                })
        )
    {
        return false;
    }
    let text = error.to_string().to_ascii_lowercase();
    [
        "context window exceeded",
        "context window exceeds",
        "context length exceeded",
        "context_length_exceeded",
        "model_context_window_exceeded",
        "maximum context length",
        "exceeds the context window",
        "exceeds model's maximum context length",
        "request_too_large",
        "too many tokens",
        "token limit",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn provider_requests_connection_refresh(error: &ygg_ai::ProviderError) -> bool {
    let Some(code) = error.code.as_deref() else {
        return false;
    };
    let code = code.to_ascii_lowercase();
    code == "websocket_connection_limit_reached"
        || (code.contains("websocket") && code.contains("connection") && code.contains("limit"))
}

fn retryable_provider_error(error: &ygg_ai::ProviderError) -> bool {
    if provider_requests_connection_refresh(error) {
        // The provider rejected the generation because its long-lived socket
        // expired. The WebSocket pool retires that socket, so a retry opens a
        // fresh transport (or the safe HTTP fallback) before any generation.
        return true;
    }
    if error
        .code
        .as_deref()
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (500..600).contains(&code))
    {
        return true;
    }
    let text = format!(
        "{} {} {}",
        error.code.as_deref().unwrap_or_default(),
        error.kind.as_deref().unwrap_or_default(),
        error.message
    )
    .to_ascii_lowercase();
    [
        "rate_limit",
        "rate limit",
        "throttl",
        "overload",
        "temporarily_unavailable",
        "temporarily unavailable",
        "service_unavailable",
        "service unavailable",
        "server_error",
        "server error",
        "internal_error",
        "internal error",
        "timed out",
        "timeout",
        "try again",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn retryable_stream_start(error: &AiError) -> bool {
    retryable_before_generation(error)
        || matches!(
            error,
            AiError::Transport(transport)
                if !transport.timeout && transport.phase == ygg_ai::TransportPhase::Body
        )
        || matches!(error, AiError::Provider(provider) if retryable_provider_error(provider))
        || matches!(
            error,
            AiError::StreamProtocol(
                ygg_ai::StreamProtocolError::MissingFinish
                    | ygg_ai::StreamProtocolError::PrematureEof
            )
        )
        // The clauses above are variant-specific; a mid-stream wrapper
        // delegates so that e.g. a stream ending without a terminal event is
        // retried exactly as the bare protocol error was.
        || matches!(
            error,
            AiError::StreamFailure { inner, .. } if retryable_stream_start(inner)
        )
}

fn provider_retry_limit(error: &AiError) -> usize {
    // A mid-stream wrapper inherits the retry budget of the failure that
    // ended the stream, so introducing the wrapper changes no retry behavior.
    if let AiError::StreamFailure { inner, .. } = error {
        return provider_retry_limit(inner);
    }
    if matches!(
        error,
        AiError::Transport(transport)
            if transport.timeout || transport.phase == ygg_ai::TransportPhase::ResponseHeaders
    ) {
        // A timeout has already consumed its configured deadline. A request
        // that failed while sending or awaiting headers may also have been
        // accepted by the provider. Neither class is replayed automatically.
        0
    } else if is_replayable_network_failure(error) {
        MAX_NETWORK_RETRIES
    } else {
        MAX_PROVIDER_RETRIES
    }
}

fn retry_after(error: &AiError, attempt: usize) -> Duration {
    if let AiError::Http(error) = error {
        if let Some(delay) = error.retry_after {
            return delay.min(Duration::from_secs(30));
        }
    }
    // Keep retries bounded and add a small deterministic stagger in lieu of a
    // rand dependency. The provider's Retry-After always takes precedence.
    let base = 200u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(base + (attempt as u64 * 37) % 100)
}

fn provider_retry_diagnostic(model: &Model, error: &AiError) -> String {
    let diagnostic = public_ai_error_diagnostic(error, &model.endpoint.id.0, &model.spec.id.0);
    if is_replayable_network_failure(error) {
        format!("Network connection lost. Are you connected to the internet? {diagnostic}")
    } else {
        diagnostic
    }
}

fn provider_failure(error: AiError, retries: usize) -> AgentError {
    if is_replayable_network_failure(&error) {
        AgentError::NetworkUnavailable {
            retries,
            detail: ai_error_phase(&error).to_owned(),
        }
    } else {
        error.into()
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_recovery_call(
    tool: Arc<dyn Tool>,
    hooks: &[Arc<dyn ToolCallHook>],
    broker: &EffectBroker,
    generation: u64,
    run_id: &str,
    call: &ToolCall,
    sandbox: &SandboxConfig,
    tool_scope: &str,
    resource_owner: &str,
    registered_tools: &[String],
    session: &mut Session,
) -> Result<Result<ToolOutput, ToolError>, AgentError> {
    let parsed = call
        .arguments_value()
        .map_err(|error| ToolError::new(format!("invalid tool arguments: {error}")));
    let result = match parsed {
        Err(error) => Err(error),
        Ok(args) => {
            let active_skills = session
                .head()
                .and_then(|head| session.resolve_active_skills(&head).ok())
                .map(|state| state.active_skills)
                .unwrap_or_default();
            let (progress_tx, mut progress_rx) =
                mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
            let progress_sink = ToolProgressSink::live(progress_tx);
            let context = ToolContext {
                workspace: &sandbox.workspace,
                sandbox,
                execution_scope: tool_scope,
                resource_owner,
                active_skills: &active_skills,
                registered_tools,
                progress: progress_sink,
                cancellation: CancellationToken::default(),
            };
            let effect = match tool.effect(&args, &context) {
                Ok(effect) => effect,
                Err(error) => return Ok(Err(error)),
            };
            if !effect_is_repeatable_observation(effect) {
                return Ok(Err(ToolError::new(format!(
                    "indeterminate after restart: `{}` may have completed before its result was persisted; Ygg did not replay this host-classified effect. Inspect external state and retry explicitly if needed",
                    call.name
                ))));
            }
            // Bind admission to the same exact classification used by the
            // replay gate. A second classification could otherwise authorize
            // an effect different from the one that passed replay admission.
            let intent = match EffectIntent::new(
                resource_owner,
                run_id,
                generation,
                call.id.0.clone(),
                &call.name,
                effect,
                &args,
            ) {
                Ok(intent) => intent,
                Err(error) => return Ok(Err(ToolError::new(error.to_string()))),
            };
            let effect_reservation = match broker.reserve(&intent, None).await {
                Ok(reservation) => reservation,
                Err(error) => return Ok(Err(ToolError::new(error.to_string()))),
            };
            // Retain hook arguments before the execution admission point so no
            // payload-sized allocation separates commit from dispatch.
            let hook_arguments = args.clone();
            for hook in hooks {
                if let Err(error) = hook.before_tool_call(&call.name, &args, &context).await {
                    return Ok(Err(error));
                }
            }
            if let Err(error) = effect_reservation.commit(&intent) {
                return Ok(Err(ToolError::new(error.to_string())));
            }
            let execute = tool.execute(args, &context);
            tokio::pin!(execute);
            let result = loop {
                tokio::select! {
                    result = &mut execute => break result,
                    progress = progress_rx.recv() => {
                        if let Some(ToolProgress::SessionEvent(event, reply)) = progress {
                            match session.append(*event) {
                                Ok(entry_id) => {
                                    if let Ok(mut slot) = reply.lock() {
                                        if let Some(sender) = slot.take() {
                                            let _ = sender.send(Ok(entry_id));
                                        }
                                    }
                                }
                                Err(error) => {
                                    let message = error.to_string();
                                    if let Ok(mut slot) = reply.lock() {
                                        if let Some(sender) = slot.take() {
                                            let _ = sender.send(Err(message));
                                        }
                                    }
                                    return Err(AgentError::Session(error));
                                }
                            }
                        }
                    }
                }
            };
            // A tool can enqueue a final semantic event just before returning.
            // Apply every already-accepted event before writing its result.
            while let Ok(progress) = progress_rx.try_recv() {
                if let ToolProgress::SessionEvent(event, reply) = progress {
                    match session.append(*event) {
                        Ok(entry_id) => {
                            if let Ok(mut slot) = reply.lock() {
                                if let Some(sender) = slot.take() {
                                    let _ = sender.send(Ok(entry_id));
                                }
                            }
                        }
                        Err(error) => {
                            let message = error.to_string();
                            if let Ok(mut slot) = reply.lock() {
                                if let Some(sender) = slot.take() {
                                    let _ = sender.send(Err(message));
                                }
                            }
                            return Err(AgentError::Session(error));
                        }
                    }
                }
            }
            let (output, is_error) = match &result {
                Ok(output) => (output.text.as_str(), output.is_error()),
                Err(error) => (error.message.as_str(), true),
            };
            for hook in hooks {
                hook.after_tool_call(&call.name, &hook_arguments, output, is_error, &context)
                    .await;
            }
            result
        }
    };
    Ok(result)
}

fn model_visible_branch_entries(session: &Session) -> Vec<&crate::session::Entry> {
    let branch = active_branch_entries(session);
    let first_kept = branch.iter().rev().find_map(|entry| match &entry.value {
        EntryValue::Compaction { first_kept, .. } => Some(first_kept),
        _ => None,
    });
    let start = first_kept
        .and_then(|first_kept| branch.iter().position(|entry| &entry.id == first_kept))
        .unwrap_or_default();
    branch.into_iter().skip(start).collect()
}

fn previous_message_is_user(session: &Session, entry: &crate::session::Entry) -> bool {
    let mut cursor = entry.parent.clone();
    while let Some(id) = cursor {
        let Some(previous) = session.entry(&id) else {
            return false;
        };
        match &previous.value {
            EntryValue::Message(Message::User(user)) => return !user.content.is_empty(),
            EntryValue::Message(Message::Assistant(_)) => return false,
            EntryValue::Compaction { .. }
            | EntryValue::ResponsesTurn { .. }
            | EntryValue::ResponsesCompaction { .. }
            | EntryValue::Config { .. }
            | EntryValue::PromptTemplateSelected { .. }
            | EntryValue::SkillActivated { .. }
            | EntryValue::SkillResourceRead { .. }
            | EntryValue::SkillDeactivated { .. } => cursor = previous.parent.clone(),
        }
    }
    false
}

fn turn_starts(session: &Session) -> Vec<EntryId> {
    model_visible_branch_entries(session)
        .into_iter()
        .filter_map(|entry| {
            if !matches!(&entry.value, EntryValue::Message(Message::Assistant(_)))
                || !previous_message_is_user(session, entry)
            {
                return None;
            }
            // Every assistant whose previous durable message is a user message
            // is a potential episode boundary. Non-message compaction/config/
            // skill markers may sit between them and must not hide the turn.
            Some(entry.id.clone())
        })
        .collect()
}

#[derive(Default)]
struct CountingWriter(u64);

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const ESTIMATED_IMAGE_TOKENS: u64 = 1_600;
const ESTIMATED_AUDIO_TOKENS: u64 = 8_000;

fn inline_media_payload_bytes(media: &Media) -> u64 {
    let raw_bytes = match media {
        Media::Image(image) => match &image.source {
            ImageSource::Inline(data) => data.len() as u64,
            ImageSource::Url(_) | ImageSource::ProviderRef(_) => 0,
        },
        Media::Audio(audio) => match &audio.payload {
            AudioPayload::Inline(data) | AudioPayload::InlineWithProviderRef { data, .. } => {
                data.len() as u64
            }
            AudioPayload::ProviderRef(_) => 0,
        },
    };
    // Inline media's serde representation is one padded base64 string. The
    // surrounding quotes and variant metadata remain in the structural byte
    // estimate; remove only payload characters before adding semantic tokens.
    raw_bytes.div_ceil(3).saturating_mul(4)
}

fn media_tokens(media: &Media) -> u64 {
    match media {
        Media::Image(_) => ESTIMATED_IMAGE_TOKENS,
        Media::Audio(_) => ESTIMATED_AUDIO_TOKENS,
    }
}

fn request_media_adjustment(messages: &[Message]) -> (u64, u64) {
    let mut inline_payload_bytes = 0u64;
    let mut semantic_tokens = 0u64;
    let mut observe = |media: &Media| {
        inline_payload_bytes =
            inline_payload_bytes.saturating_add(inline_media_payload_bytes(media));
        semantic_tokens = semantic_tokens.saturating_add(media_tokens(media));
    };
    for message in messages {
        match message {
            Message::User(user) => {
                for part in &user.content {
                    match part {
                        UserPart::Media(media) => observe(media),
                        UserPart::ToolResult(result) => {
                            for part in &result.content {
                                if let ToolResultPart::Media(media) = part {
                                    observe(media);
                                }
                            }
                        }
                        UserPart::Text(_) => {}
                    }
                }
            }
            Message::Assistant(assistant) => {
                for part in &assistant.content {
                    if let AssistantPart::Media(media) = part {
                        observe(media);
                    }
                }
            }
        }
    }
    (inline_payload_bytes, semantic_tokens)
}

fn responses_replay_media_adjustment(replay: &[ResponsesReplayItem]) -> (u64, u64) {
    let mut inline_payload_bytes = 0u64;
    let mut semantic_tokens = 0u64;
    let mut observe = |media: &Media| {
        inline_payload_bytes =
            inline_payload_bytes.saturating_add(inline_media_payload_bytes(media));
        semantic_tokens = semantic_tokens.saturating_add(media_tokens(media));
    };
    for item in replay {
        let ResponsesReplayItem::User(user) = item else {
            continue;
        };
        for part in &user.content {
            match part {
                UserPart::Media(media) => observe(media),
                UserPart::ToolResult(result) => {
                    for part in &result.content {
                        if let ToolResultPart::Media(media) = part {
                            observe(media);
                        }
                    }
                }
                UserPart::Text(_) => {}
            }
        }
    }
    (inline_payload_bytes, semantic_tokens)
}

fn estimate_request_tokens(system: &str, messages: &[Message], tools: &[ToolDef]) -> u64 {
    let mut bytes = CountingWriter::default();
    if serde_json::to_writer(&mut bytes, &(system, messages, tools)).is_err() {
        return 64;
    }
    let (inline_payload_bytes, semantic_tokens) = request_media_adjustment(messages);
    bytes
        .0
        .saturating_sub(inline_payload_bytes)
        .div_ceil(4)
        .saturating_add(semantic_tokens)
        .saturating_add(64)
}

struct ExactResponsesReplay {
    input: ResponsesInput,
    replay: Vec<ResponsesReplayItem>,
    instructions: Option<String>,
}

fn exact_responses_replay(
    session: &Session,
    model: &Model,
    system: &str,
) -> Option<ExactResponsesReplay> {
    if model.spec.protocol != Protocol::OpenAiResponses {
        return None;
    }
    let replay = session
        .responses_replay_items(&model.endpoint.id, &model.spec.id)
        .ok()
        .flatten()?;
    let instructions = matches!(replay.first(), Some(ResponsesReplayItem::Compacted(_)))
        .then(|| system.to_owned())
        .filter(|system| !system.is_empty());
    let input = ygg_ai::responses::encode_responses_replay(
        model,
        (!system.is_empty()).then_some(system),
        &replay,
    );
    Some(ExactResponsesReplay {
        input,
        replay,
        instructions,
    })
}

fn current_head_is_native_checkpoint(session: &Session, model: &Model) -> bool {
    session
        .head_ref()
        .and_then(|head| session.entry(head))
        .is_some_and(|entry| {
            matches!(
                &entry.value,
                EntryValue::ResponsesCompaction {
                    endpoint,
                    model: recorded_model,
                    ..
                } if endpoint == &model.endpoint.id && recorded_model == &model.spec.id
            )
        })
}

fn validate_native_compact_output(output: &ygg_ai::ResponsesOutput) -> Result<(), AgentError> {
    if output.has_valid_compaction() {
        Ok(())
    } else {
        Err(AiError::Decode(DecodeError::Json(
            "Responses compact output did not contain exactly one complete compaction item"
                .to_owned(),
        ))
        .into())
    }
}

fn durable_responses_options(
    session: &Session,
    model: &Model,
    system: &str,
) -> Option<ResponsesOptions> {
    exact_responses_replay(session, model, system)
        .map(|exact| ResponsesOptions::full_replay(exact.input))
}

fn native_responses_options(
    session: &Session,
    model: &Model,
    system: &str,
) -> Result<ResponsesOptions, AgentError> {
    let replay = session
        .responses_replay_items(&model.endpoint.id, &model.spec.id)?
        .ok_or_else(|| {
            AgentError::InvalidCompactionPolicy(
                "native Responses mode requires complete route-affine opaque replay before every provider request"
                    .to_owned(),
            )
        })?;
    Ok(ResponsesOptions::full_replay(
        ygg_ai::responses::encode_responses_replay(
            model,
            (!system.is_empty()).then_some(system),
            &replay,
        ),
    ))
}

fn estimate_responses_request_tokens(
    input: &ResponsesInput,
    replay: &[ResponsesReplayItem],
    tools: &[ToolDef],
    instructions: Option<&str>,
) -> u64 {
    let mut bytes = CountingWriter::default();
    if serde_json::to_writer(&mut bytes, &(input, tools, instructions)).is_err() {
        return 64;
    }
    let (inline_payload_bytes, semantic_tokens) = responses_replay_media_adjustment(replay);
    // Opaque replay and native compact checkpoints are estimated from exactly
    // what will be serialized, never from canonical history they replaced.
    // Only canonical replay media is converted from base64 bytes to a semantic
    // modality estimate; opaque provider output remains fully byte-counted.
    bytes
        .0
        .saturating_sub(inline_payload_bytes)
        .div_ceil(4)
        .saturating_add(semantic_tokens)
        .saturating_add(64)
}

fn estimate_compact_request_tokens(
    request: &ResponsesCompactRequest,
    replay: &[ResponsesReplayItem],
) -> u64 {
    let mut bytes = CountingWriter::default();
    if serde_json::to_writer(&mut bytes, request).is_err() {
        return 64;
    }
    let (inline_payload_bytes, semantic_tokens) = responses_replay_media_adjustment(replay);
    bytes
        .0
        .saturating_sub(inline_payload_bytes)
        .div_ceil(4)
        .saturating_add(semantic_tokens)
        .saturating_add(64)
}

fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    let mut bytes = CountingWriter::default();
    if serde_json::to_writer(&mut bytes, messages).is_err() {
        return 64;
    }
    let (inline_payload_bytes, semantic_tokens) = request_media_adjustment(messages);
    bytes
        .0
        .saturating_sub(inline_payload_bytes)
        .div_ceil(4)
        .saturating_add(semantic_tokens)
        .saturating_add(16)
}

fn usage_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens)
            .saturating_add(usage.output_tokens)
    }
}

/// Provider usage is the best available tokenizer measurement of the prefix
/// through its assistant response. Add structural estimates only for messages
/// persisted after that response. Usage from before the latest compaction or
/// from a different route/model is stale and must not retrigger compaction.
fn provider_context_estimate(session: &Session, model: &Model) -> Option<u64> {
    let branch = active_branch_entries(session);
    let boundary = branch
        .iter()
        .rposition(|entry| {
            matches!(
                entry.value,
                EntryValue::Compaction { .. } | EntryValue::ResponsesCompaction { .. }
            )
        })
        .map_or(0, |index| index.saturating_add(1));

    for (index, entry) in branch.iter().enumerate().skip(boundary).rev() {
        if !matches!(entry.value, EntryValue::Message(Message::Assistant(_))) {
            continue;
        }
        let Some(record) = session.usage_records().iter().rev().find(|record| {
            matches!(
                &record.kind,
                crate::session::UsageRecordKind::AssistantTurn { assistant }
                    if assistant == &entry.id
            ) && record.endpoint.as_ref() == Some(&model.endpoint.id)
                && record.model.as_ref() == Some(&model.spec.id)
                && usage_context_tokens(&record.usage) > 0
        }) else {
            continue;
        };
        let trailing = branch[index.saturating_add(1)..]
            .iter()
            .filter_map(|entry| match &entry.value {
                EntryValue::Message(message) => Some(message),
                _ => None,
            })
            .fold(0u64, |total, message| {
                total.saturating_add(estimate_messages_tokens(std::slice::from_ref(message)))
            });
        return Some(usage_context_tokens(&record.usage).saturating_add(trailing));
    }
    None
}

fn reconcile_context_estimate(
    session: &Session,
    model: &Model,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> RequestContextEstimate {
    let structural_tokens = exact_responses_replay(session, model, system).map_or_else(
        || estimate_request_tokens(system, messages, tools),
        |exact| {
            estimate_responses_request_tokens(
                &exact.input,
                &exact.replay,
                tools,
                exact.instructions.as_deref(),
            )
        },
    );
    let provider_tokens = provider_context_estimate(session, model);
    let input_tokens = provider_tokens.map_or(structural_tokens, |provider| {
        structural_tokens.max(provider)
    });
    RequestContextEstimate {
        structural_tokens,
        provider_tokens,
        input_tokens,
    }
}

fn serialized_tokens<T: Serialize>(value: &T) -> u64 {
    let mut bytes = CountingWriter::default();
    if serde_json::to_writer(&mut bytes, value).is_err() {
        return 0;
    }
    bytes.0.div_ceil(4)
}

fn visible_compaction_summary(session: &Session) -> Option<String> {
    active_branch_entries(session)
        .into_iter()
        .rev()
        .find_map(|entry| {
            let EntryValue::Compaction { summary, .. } = &entry.value else {
                return None;
            };
            Some(format!("[summary of earlier conversation]\n{summary}"))
        })
}

fn context_breakdown(
    session: &Session,
    model: &Model,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> ContextBreakdown {
    let estimate = reconcile_context_estimate(session, model, system, messages, tools);
    let mut remaining_structural = estimate.structural_tokens;
    let mut take = |requested: u64| {
        let accepted = requested.min(remaining_structural);
        remaining_structural = remaining_structural.saturating_sub(accepted);
        accepted
    };

    let instruction_tokens = take(serialized_tokens(&system));
    let summary = visible_compaction_summary(session);
    let mut conversation_tokens = 0u64;
    let mut tool_result_tokens = 0u64;
    let mut attachment_tokens = 0u64;
    let mut compaction_summary_tokens = 0u64;

    for message in messages {
        let message_slice = std::slice::from_ref(message);
        let mut bytes = CountingWriter::default();
        if serde_json::to_writer(&mut bytes, message).is_err() {
            continue;
        }
        let (inline_payload_bytes, semantic_media_tokens) = request_media_adjustment(message_slice);
        let media_tokens = take(semantic_media_tokens);
        attachment_tokens = attachment_tokens.saturating_add(media_tokens);
        let non_media_tokens = bytes.0.saturating_sub(inline_payload_bytes).div_ceil(4);
        let accepted = take(non_media_tokens);
        let is_tool = match message {
            Message::User(user) => user
                .content
                .iter()
                .any(|part| matches!(part, UserPart::ToolResult(_))),
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .any(|part| matches!(part, AssistantPart::ToolCall(_))),
        };
        let is_summary = summary.as_ref().is_some_and(|summary| {
            matches!(
                message,
                Message::User(user)
                    if user.content.len() == 1
                        && matches!(&user.content[0], UserPart::Text(text) if text == summary)
            )
        });
        if is_summary {
            compaction_summary_tokens = compaction_summary_tokens.saturating_add(accepted);
        } else if is_tool {
            tool_result_tokens = tool_result_tokens.saturating_add(accepted);
        } else {
            conversation_tokens = conversation_tokens.saturating_add(accepted);
        }
    }

    // Whatever remains in the serializer-derived total is request framing,
    // tool definitions, and provider/runtime system structure. Provider usage
    // above that structural estimate is authoritative but intentionally left in
    // `other` rather than assigned with fabricated precision.
    let system_tokens = remaining_structural;
    let other_tokens = estimate
        .input_tokens
        .saturating_sub(estimate.structural_tokens);
    let total_tokens = system_tokens
        .saturating_add(instruction_tokens)
        .saturating_add(conversation_tokens)
        .saturating_add(tool_result_tokens)
        .saturating_add(attachment_tokens)
        .saturating_add(compaction_summary_tokens)
        .saturating_add(other_tokens);
    debug_assert_eq!(total_tokens, estimate.input_tokens);

    ContextBreakdown {
        system_tokens,
        instruction_tokens,
        conversation_tokens,
        tool_result_tokens,
        attachment_tokens,
        compaction_summary_tokens,
        other_tokens,
        total_tokens,
        structural_tokens: estimate.structural_tokens,
        provider_tokens: estimate.provider_tokens,
        context_limit: model.spec.limits.context_window,
    }
}

fn observe_context_tracker(
    tracker: &ContextTracker,
    session: &Session,
    model: &Model,
    system: &str,
    tools: &[ToolDef],
) -> Result<ContextBreakdown, SessionError> {
    let messages = session.context_ref()?;
    let breakdown = context_breakdown(session, model, system, &messages, tools);
    tracker.observe_context(breakdown.clone());
    Ok(breakdown)
}

/// Which queue a batch of control inputs came from; only the announced event
/// differs between steering and follow-up delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlDeliveryKind {
    Steering,
    FollowUp,
}

impl ControlDeliveryKind {
    fn delivered_event(self, messages: Vec<String>) -> AgentEvent {
        match self {
            Self::Steering => AgentEvent::SteeringDelivered { messages },
            Self::FollowUp => AgentEvent::FollowUpDelivered { messages },
        }
    }
}

/// Outcome of appending one queued control-input batch (steering or
/// follow-up) to the session.
enum ControlDelivery {
    /// Every input was appended; announce them with this event when present.
    Completed { event: Option<AgentEvent> },
    /// Persistence failed. Any prefix that did reach the session is announced
    /// by `event`; the run must then end with `finish`.
    Interrupted {
        event: Option<AgentEvent>,
        finish: FinishReason,
    },
}

/// Snapshot of everything `observe_context_tracker` needs to re-observe the
/// tracker after control inputs change the session.
struct ContextObservation<'a> {
    tracker: &'a ContextTracker,
    model: &'a Model,
    system: &'a str,
    tools: &'a [ToolDef],
}

impl ContextObservation<'_> {
    fn observe(&self, session: &Session) -> Result<ContextBreakdown, SessionError> {
        observe_context_tracker(self.tracker, session, self.model, self.system, self.tools)
    }
}

async fn next_delegation_snapshot(
    receiver: &mut watch::Receiver<Option<DelegationTelemetrySnapshot>>,
) -> Option<DelegationTelemetrySnapshot> {
    receiver.changed().await.ok()?;
    receiver.borrow_and_update().clone()
}

/// Result of applying one drained tool-progress item to the run.
enum ProgressSettlement {
    /// Cancellation took precedence before the item was accepted; semantic
    /// state was discarded and the caller must stop accepting progress.
    Cancelled,
    /// Consumed internally as a durable session event (persisted, or its
    /// reply resolved with the persistence error).
    Settled,
    /// Pure progress; surface it to observers as a `ToolProgress` event.
    Emit(ToolProgress),
}

/// Apply one drained tool-progress item.
///
/// When `cancelled` won, any queued session event is rejected through its
/// reply channel without touching the session. Otherwise a session event is
/// appended durably and acknowledged; every other progress flavor is returned
/// for the caller to emit.
fn settle_tool_progress(
    p: ToolProgress,
    cancelled: bool,
    session: &mut Session,
) -> ProgressSettlement {
    if cancelled {
        // The biased select deliberately gives cancellation
        // precedence. Events already accepted in the loop
        // remain durable, but a queued semantic event must
        // not take effect after the tool was reported as
        // cancelled (notably, it must not activate a skill).
        if let ToolProgress::SessionEvent(_, reply_tx_mutex) = p {
            if let Ok(mut opt) = reply_tx_mutex.lock() {
                if let Some(reply_tx) = opt.take() {
                    let _ = reply_tx.send(Err(
                        "session event discarded because cancellation won".to_string()
                    ));
                }
            }
        }
        return ProgressSettlement::Cancelled;
    }
    if let ToolProgress::SessionEvent(event, reply_tx_mutex) = p {
        let res = session.append(*event);
        if let Ok(mut opt) = reply_tx_mutex.lock() {
            if let Some(reply_tx) = opt.take() {
                let _ = reply_tx.send(res.map_err(|e| e.to_string()));
            }
        }
        ProgressSettlement::Settled
    } else {
        ProgressSettlement::Emit(p)
    }
}

/// Append a batch of already-queued control inputs as durable user messages
/// and report what was delivered.
///
/// Each summary is recorded for the terminal gate before its append is
/// attempted, mirroring the original inline blocks. On append failure the
/// context tracker is still observed (its error ignored) so observers see the
/// partial delivery before the run ends.
fn deliver_control_inputs(
    queued: Vec<UserInput>,
    kind: ControlDeliveryKind,
    session: &mut Session,
    metadata: &EntryMetadata,
    terminal_gate_requests: &mut Vec<String>,
    observation: &ContextObservation<'_>,
) -> ControlDelivery {
    let mut delivered = Vec::with_capacity(queued.len());
    for input in queued {
        let summary = input.text_summary();
        terminal_gate_requests.push(summary.clone());
        if let Err(e) = session.append_with_metadata(user_message(input), Some(metadata.clone())) {
            let event = (!delivered.is_empty())
                .then(|| kind.delivered_event(std::mem::take(&mut delivered)));
            let _ = observation.observe(session);
            return ControlDelivery::Interrupted {
                event,
                finish: FinishReason::Failed(e.into()),
            };
        }
        delivered.push(summary);
    }
    if !delivered.is_empty() {
        if let Err(error) = observation.observe(session) {
            return ControlDelivery::Interrupted {
                event: None,
                finish: FinishReason::Failed(error.into()),
            };
        }
        return ControlDelivery::Completed {
            event: Some(kind.delivered_event(delivered)),
        };
    }
    ControlDelivery::Completed { event: None }
}

fn worst_case_request_cost(model: &Model, input_tokens: u64, output_tokens: u64) -> Option<u64> {
    let pricing = model.spec.pricing.as_ref()?;
    let mut input_rate = pricing
        .input
        .0
        .max(pricing.cache_read.0)
        .max(pricing.cache_write_5m.0)
        .max(
            pricing
                .cache_write_1h
                .map(|rate| rate.0)
                .unwrap_or_else(|| pricing.input.0.saturating_mul(2)),
        );
    let mut output_rate = pricing
        .output
        .0
        .max(pricing.reasoning.map(|rate| rate.0).unwrap_or_default());
    for tier in &pricing.tiers {
        for rate in [
            tier.input,
            tier.cache_read,
            tier.cache_write_5m,
            tier.cache_write_1h,
        ]
        .into_iter()
        .flatten()
        {
            input_rate = input_rate.max(rate.0);
        }
        for rate in [tier.output, tier.reasoning].into_iter().flatten() {
            output_rate = output_rate.max(rate.0);
        }
    }
    let numerator = u128::from(input_tokens)
        .saturating_mul(u128::from(input_rate))
        .saturating_add(u128::from(output_tokens).saturating_mul(u128::from(output_rate)));
    let denominator = u128::from(PICODOLLARS_PER_MICRODOLLAR);
    u64::try_from(numerator.div_ceil(denominator)).ok()
}

fn usage_total_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens)
            .saturating_add(usage.output_tokens)
    }
}

fn session_total_tokens_for_own_context(session: &Session) -> u64 {
    session
        .usage_records()
        .iter()
        .filter(|record| !matches!(&record.kind, UsageRecordKind::DelegatedAgent { .. }))
        .fold(0u64, |total, record| {
            total.saturating_add(usage_total_tokens(&record.usage))
        })
}

fn reserve_request_tokens(
    session: &Session,
    input_tokens: u64,
    output_tokens: u64,
    limit: Option<u64>,
) -> Result<(), AgentError> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let current = session_total_tokens_for_own_context(session);
    let reserved = input_tokens.saturating_add(output_tokens);
    if current >= limit || current.saturating_add(reserved) > limit {
        return Err(AgentError::TokenLimit {
            current,
            reserved,
            limit,
        });
    }
    Ok(())
}

fn reserve_request_cost(
    session: &Session,
    model: &Model,
    input_tokens: u64,
    output_tokens: u64,
    limit: Option<u64>,
) -> Result<(), AgentError> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let current = session.total_cost_microdollars();
    let reserved = worst_case_request_cost(model, input_tokens, output_tokens)
        .ok_or(AgentError::CostUnavailable { limit })?;
    if current >= limit || current.saturating_add(reserved) > limit {
        return Err(AgentError::CostLimit {
            current,
            reserved,
            limit,
        });
    }
    Ok(())
}

fn assistant_text(response: &ygg_ai::Response) -> Option<String> {
    let text = response
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ygg_ai::AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (!text.trim().is_empty()).then_some(text)
}

struct CompactionContext<'a> {
    client: &'a AiClient,
    /// Active model, used for context-window sizing and the normal request.
    model: &'a Model,
    /// Optional configured route used for the summary request itself.
    compaction_model: &'a Model,
    session: &'a mut Session,
    usage: &'a mut Usage,
    run_cost: &'a mut CostAccumulator,
    cache_retention: CacheRetention,
    reasoning: &'a ReasoningConfig,
    reasoning_mode: ReasoningMode,
    session_id: &'a str,
    max_session_tokens: Option<u64>,
    max_session_cost_microdollars: Option<u64>,
    abort: &'a AbortFlag,
    mode: AgentCompactionMode,
    threshold_fraction: f64,
    keep_recent_tokens: u64,
    events: &'a mpsc::UnboundedSender<AgentEvent>,
    context: &'a ContextTracker,
    tool_generation: u64,
    capacity: &'a mut ContextCapacityCache,
}

struct CapacityEstimate {
    input_tokens: u64,
    max_output_tokens: u64,
    active_system: String,
}

/// Total-only context accounting used by the pre-request capacity gate.
///
/// The first value is seeded from the exact context observation. For ordinary
/// canonical-message appends, later values advance from the cached head using
/// a per-message upper bound instead of serializing the complete history. A
/// branch checkout, local compaction, tool-surface change, or Responses replay
/// falls back to the exact estimator. The detailed category breakdown remains
/// the on-demand telemetry path in [`context_breakdown`].
struct ContextCapacityCache {
    head: Option<EntryId>,
    tool_generation: u64,
    structural_tokens: u64,
    provider_tokens: Option<u64>,
    valid: bool,
    #[cfg(test)]
    full_rebuilds: usize,
}

impl ContextCapacityCache {
    fn seeded(session: &Session, tool_generation: u64, context: &ContextBreakdown) -> Self {
        Self {
            head: session.head(),
            tool_generation,
            structural_tokens: context.structural_tokens,
            provider_tokens: context.provider_tokens,
            valid: true,
            #[cfg(test)]
            full_rebuilds: 0,
        }
    }

    fn invalidate(&mut self) {
        self.valid = false;
    }

    /// Advances over entries appended below the cached head.
    ///
    /// Only a local compaction changes the canonical message sequence among
    /// the entries handled here. All other non-message entries are invisible
    /// to canonical requests, while each message contributes a conservative
    /// standalone estimate. The entries are collected and validated before
    /// mutating the cache so a failed ancestry walk cannot leave a partial
    /// estimate behind.
    fn advance_messages(&mut self, session: &Session) -> bool {
        if !self.valid {
            return false;
        }
        let cached_head = self.head.clone();
        let mut cursor = session.head_ref();
        if cursor == cached_head.as_ref() {
            return true;
        }

        let mut appended = Vec::new();
        while cursor != cached_head.as_ref() {
            let Some(id) = cursor else {
                return false;
            };
            let Some(entry) = session.entry(id) else {
                return false;
            };
            if matches!(entry.value, EntryValue::Compaction { .. }) {
                return false;
            }
            appended.push(entry);
            cursor = entry.parent.as_ref();
        }

        for entry in appended.into_iter().rev() {
            if let EntryValue::Message(message) = &entry.value {
                let delta = estimate_messages_tokens(std::slice::from_ref(message));
                self.structural_tokens = self.structural_tokens.saturating_add(delta);
                if let Some(provider) = self.provider_tokens.as_mut() {
                    *provider = provider.saturating_add(delta);
                }
            }
        }
        self.head = session.head();
        true
    }

    /// Replaces the cache with a route-accurate full estimate.
    fn rebuild(
        &mut self,
        session: &Session,
        model: &Model,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        tool_generation: u64,
    ) -> RequestContextEstimate {
        let estimate = reconcile_context_estimate(session, model, system, messages, tools);
        self.head = session.head();
        self.tool_generation = tool_generation;
        self.structural_tokens = estimate.structural_tokens;
        self.provider_tokens = estimate.provider_tokens;
        self.valid = true;
        #[cfg(test)]
        {
            self.full_rebuilds = self.full_rebuilds.saturating_add(1);
        }
        estimate
    }

    fn estimate(
        &mut self,
        session: &Session,
        model: &Model,
        system: &str,
        tools: &[ToolDef],
        tool_generation: u64,
    ) -> Result<RequestContextEstimate, SessionError> {
        let messages = session.context_ref()?;
        let can_advance = model.spec.protocol != Protocol::OpenAiResponses
            && self.valid
            && self.tool_generation == tool_generation
            && self.advance_messages(session);
        if !can_advance {
            self.rebuild(session, model, system, &messages, tools, tool_generation);
        }
        let input_tokens = self
            .provider_tokens
            .map_or(self.structural_tokens, |provider| {
                self.structural_tokens.max(provider)
            });
        Ok(RequestContextEstimate {
            structural_tokens: self.structural_tokens,
            provider_tokens: self.provider_tokens,
            input_tokens,
        })
    }

    /// Re-anchors provider reconciliation after a completed assistant turn.
    ///
    /// Provider usage is authoritative for the prefix through that assistant.
    /// A zero usage report is left on the incrementally advanced estimate,
    /// matching `provider_context_estimate`'s behavior of ignoring unusable
    /// records rather than replacing a usable older measurement with zero.
    fn observe_assistant_response(&mut self, session: &Session, usage: &Usage) {
        if !self.advance_messages(session) {
            self.invalidate();
            return;
        }
        let measured = usage_context_tokens(usage);
        if measured > 0 {
            self.provider_tokens = Some(measured);
        }
    }

    #[cfg(test)]
    fn full_rebuilds(&self) -> usize {
        self.full_rebuilds
    }
}

/// An immutable request snapshot assembled only after context capacity has
/// been established. Compaction and dynamic tool publication can both cross
/// an await before the provider request is opened, so the snapshot carries the
/// durable cursor and the exact prompt/tool generations it was built from.
struct PreparedTurn {
    durable_head: Option<EntryId>,
    active_system: String,
    tool_generation: u64,
    request: Request,
    input_tokens: u64,
}

impl PreparedTurn {
    fn new(
        durable_head: Option<EntryId>,
        active_system: String,
        tool_generation: u64,
        request: Request,
        input_tokens: u64,
    ) -> Self {
        Self {
            durable_head,
            active_system,
            tool_generation,
            request,
            input_tokens,
        }
    }

    /// The request may be sent only while all inputs used to prepare it still
    /// describe the authoritative session. A mismatch is retried through the
    /// normal turn-boundary path, which rebuilds context and tool maps instead
    /// of mixing generations in one provider call.
    fn is_current(&self, session: &Session, active_system: &str, tool_generation: u64) -> bool {
        self.durable_head == session.head()
            && self.active_system == active_system
            && self.tool_generation == tool_generation
            && self.request.system.as_deref()
                == (!active_system.is_empty()).then_some(active_system)
    }
}

impl CompactionContext<'_> {
    async fn call(
        &mut self,
        system: &str,
        messages: Vec<Message>,
        output_tokens: u64,
    ) -> Result<Option<String>, AgentError> {
        // Compaction is a normal provider request: retaining the stable session
        // affinity lets compatible providers reuse any common prefix and keeps
        // its accounting visible alongside autonomous turns.
        let request = Request {
            system: Some(system.to_owned()),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: Some(
                self.compaction_model
                    .spec
                    .limits
                    .max_output_tokens
                    .clamp(1, output_tokens),
            ),
            temperature: None,
            stop: Vec::new(),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ReasoningMode::Standard,
            responses: None,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: self.cache_retention,
            session_id: Some(self.session_id.to_owned()),
        };
        let input_tokens = estimate_request_tokens(
            request.system.as_deref().unwrap_or_default(),
            &request.messages,
            &request.tools,
        );
        let input_budget = self
            .compaction_model
            .spec
            .limits
            .context_window
            .saturating_sub(request.max_output_tokens.unwrap_or(output_tokens));
        if input_tokens > input_budget {
            return Err(AgentError::ContextExceeded {
                estimate: input_tokens,
                budget: input_budget,
            });
        }
        let reserved_output_tokens = request.max_output_tokens.unwrap_or(output_tokens);
        reserve_request_tokens(
            self.session,
            input_tokens,
            reserved_output_tokens,
            self.max_session_tokens,
        )?;
        reserve_request_cost(
            self.session,
            self.compaction_model,
            input_tokens,
            reserved_output_tokens,
            self.max_session_cost_microdollars,
        )?;
        let response = tokio::select! {
            biased;
            _ = self.abort.wait() => return Err(AgentError::Cancelled),
            response = self.client.complete(self.compaction_model, request) => response?,
        };
        // Cancellation wins a same-poll race and is checked again before the
        // first accounting or session commit.
        if self.abort.is_set() {
            return Err(AgentError::Cancelled);
        }
        add_usage(self.usage, &response.usage);
        let request_cost = response.cost;
        // Record even a response whose stop reason makes compaction fail: it
        // was still billable provider work and must survive resume accurately.
        self.session.record_compaction_usage(
            self.compaction_model.endpoint.id.clone(),
            self.compaction_model.spec.id.clone(),
            response.usage,
            request_cost,
        )?;
        self.run_cost.add(request_cost);
        if !matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence
        ) {
            return Ok(None);
        }
        Ok(assistant_text(&response))
    }

    /// Generate a Pi-compatible structured handoff, including a dedicated
    /// summary when the retained boundary splits the current turn.
    async fn summarize(
        &mut self,
        preparation: &HandoffPreparation,
    ) -> Result<Option<String>, AgentError> {
        let history = if preparation.messages.is_empty() {
            preparation
                .previous_summary
                .clone()
                .or_else(|| Some("No prior history.".to_owned()))
        } else {
            self.call(
                SUMMARIZATION_SYSTEM_PROMPT,
                vec![build_handoff_message(preparation)],
                SUMMARY_OUTPUT_TOKENS,
            )
            .await?
        };
        let Some(mut summary) = history else {
            return Ok(None);
        };

        if !preparation.turn_prefix_messages.is_empty() {
            let Some(prefix_summary) = self
                .call(
                    SUMMARIZATION_SYSTEM_PROMPT,
                    vec![build_turn_prefix_handoff_message(
                        &preparation.turn_prefix_messages,
                    )],
                    TURN_PREFIX_OUTPUT_TOKENS,
                )
                .await?
            else {
                return Ok(None);
            };
            summary.push_str("\n\n---\n\n**Turn Context (split turn):**\n\n");
            summary.push_str(&prefix_summary);
        }

        Ok(Some(summary))
    }

    fn preferred_boundary(&self) -> Result<Option<EntryId>, AgentError> {
        let Some(candidate) =
            choose_first_kept_by_tokens(self.session, self.keep_recent_tokens, |message| {
                estimate_messages_tokens(std::slice::from_ref(message))
            })?
        else {
            return Ok(None);
        };
        // Pi's cut-point fallback may select the oldest visible message when
        // the token budget exceeds the available history. That is a no-op for
        // an agent compaction unless a split-turn prefix is available; allow
        // the episode fallback below to make progress in that case.
        let preparation = prepare_handoff(self.session, &candidate)?;
        if preparation.messages.is_empty() && preparation.turn_prefix_messages.is_empty() {
            Ok(None)
        } else {
            Ok(Some(candidate))
        }
    }

    fn oldest_reducible_boundary(&self) -> Option<EntryId> {
        turn_starts(self.session).get(1).cloned()
    }

    fn begin_compaction(
        &self,
        system: &str,
        tools: &[ToolDef],
        reason: CompactionReason,
    ) -> Result<u64, AgentError> {
        observe_context_tracker(self.context, self.session, self.model, system, tools)?;
        let id = self.context.compaction_started(reason);
        let _ = self.events.send(AgentEvent::CompactionStarted { reason });
        Ok(id)
    }

    fn finish_compaction(
        &self,
        id: u64,
        system: &str,
        tools: &[ToolDef],
        reason: CompactionReason,
        operation: &Result<CompactionInfo, AgentError>,
        provider_model: &Model,
    ) {
        let after = operation.as_ref().ok().and_then(|_| {
            observe_context_tracker(self.context, self.session, self.model, system, tools).ok()
        });
        self.context
            .compaction_finished(id, after, operation.is_ok());
        let event_result = match operation {
            Ok(info) => Ok(info.clone()),
            Err(error) => Err(public_error_diagnostic(
                error,
                &provider_model.endpoint.id.0,
                &provider_model.spec.id.0,
            )),
        };
        let _ = self.events.send(AgentEvent::CompactionFinished {
            reason,
            result: event_result,
        });
    }

    async fn compact_native_responses(
        &mut self,
        system: &str,
        tools: &[ToolDef],
        reason: CompactionReason,
    ) -> Result<CompactionInfo, AgentError> {
        let id = self.begin_compaction(system, tools, reason)?;
        let operation_started = std::time::Instant::now();
        let usage_before = *self.usage;
        let cost_before = self.run_cost.microdollars;
        let mut operation = async {
            if self.model.spec.protocol != Protocol::OpenAiResponses {
                return Err(AgentError::InvalidCompactionPolicy(
                    "native Responses compaction requires an OpenAI Responses model route"
                        .to_owned(),
                ));
            }
            if current_head_is_native_checkpoint(self.session, self.model) {
                return Err(AgentError::InvalidCompactionPolicy(
                    "native Responses compaction made no progress since the previous checkpoint"
                        .to_owned(),
                ));
            }
            let replay = self
                .session
                .responses_replay_items(&self.model.endpoint.id, &self.model.spec.id)?
                .ok_or_else(|| {
                    AgentError::InvalidCompactionPolicy(
                        "native Responses compaction requires complete route-affine opaque replay"
                            .to_owned(),
                    )
                })?;
            let input = ygg_ai::responses::encode_responses_replay(self.model, None, &replay);
            let instructions = (!system.is_empty()).then_some(system);
            let request = ResponsesCompactRequest::for_model(
                self.model,
                input,
                instructions.map(str::to_owned),
                tools,
                self.reasoning,
                self.reasoning_mode,
                &OutputFormat::Text,
                self.cache_retention,
                Some(self.session_id),
            );
            let input_tokens = estimate_compact_request_tokens(&request, &replay);
            reserve_request_tokens(
                self.session,
                input_tokens,
                self.model.spec.limits.max_output_tokens,
                self.max_session_tokens,
            )?;
            reserve_request_cost(
                self.session,
                self.model,
                input_tokens,
                self.model.spec.limits.max_output_tokens,
                self.max_session_cost_microdollars,
            )?;
            let covered_through = self.session.head().ok_or(SessionError::EmptySession)?;
            let response = tokio::select! {
                biased;
                _ = self.abort.wait() => return Err(AgentError::Cancelled),
                response = self.client.compact_responses(self.model, request) => response?,
            };
            if self.abort.is_set() {
                return Err(AgentError::Cancelled);
            }
            let usage = response.usage;
            let cost = self
                .model
                .spec
                .pricing
                .as_ref()
                .and_then(|pricing| ygg_ai::pricing::cost_of(pricing, &usage).ok());
            add_usage(self.usage, &usage);
            self.session.record_compaction_usage(
                self.model.endpoint.id.clone(),
                self.model.spec.id.clone(),
                usage,
                cost,
            )?;
            self.run_cost.add(cost);
            validate_native_compact_output(&response.output)?;
            let checkpoint = self.session.append_responses_compaction(
                self.model.endpoint.id.clone(),
                self.model.spec.id.clone(),
                response.output,
            )?;
            Ok(CompactionInfo {
                kind: CompactionKind::NativeResponses {
                    checkpoint,
                    covered_through: covered_through.clone(),
                },
                summary: String::new(),
                first_kept: covered_through,
                usage: Usage::default(),
                elapsed: Duration::ZERO,
                cost_microdollars: None,
            })
        }
        .await;
        if let Ok(info) = operation.as_mut() {
            info.usage = usage_since(*self.usage, usage_before);
            info.elapsed = operation_started.elapsed();
            info.cost_microdollars = self
                .model
                .spec
                .pricing
                .as_ref()
                .map(|_| self.run_cost.microdollars.saturating_sub(cost_before));
        }

        self.finish_compaction(id, system, tools, reason, &operation, self.model);
        operation
    }

    async fn compact_boundary(
        &mut self,
        first_kept: EntryId,
        system: &str,
        tools: &[ToolDef],
        reason: CompactionReason,
    ) -> Result<CompactionInfo, AgentError> {
        let id = self.begin_compaction(system, tools, reason)?;
        let operation_started = std::time::Instant::now();
        let usage_before = *self.usage;
        let cost_before = self.run_cost.microdollars;
        let mut operation = async {
            let preparation = prepare_handoff(self.session, &first_kept)?;
            if preparation.messages.is_empty() && preparation.turn_prefix_messages.is_empty() {
                return Err(AgentError::ContextExceeded {
                    estimate: 0,
                    budget: self
                        .model
                        .spec
                        .limits
                        .context_window
                        .saturating_sub(self.model.spec.limits.max_output_tokens),
                });
            }
            let summary = match self.summarize(&preparation).await? {
                Some(summary) => finish_handoff(summary, &preparation.details),
                None => {
                    return Err(AgentError::IncompleteResponse {
                        stop_reason: "compaction summary did not finish normally".to_owned(),
                    });
                }
            };
            if self.abort.is_set() {
                return Err(AgentError::Cancelled);
            }
            self.session.compact_with_details(
                summary.clone(),
                first_kept.clone(),
                preparation.details,
            )?;
            Ok(CompactionInfo {
                kind: CompactionKind::Local,
                summary,
                first_kept,
                usage: Usage::default(),
                elapsed: Duration::ZERO,
                cost_microdollars: None,
            })
        }
        .await;
        if let Ok(info) = operation.as_mut() {
            info.usage = usage_since(*self.usage, usage_before);
            info.elapsed = operation_started.elapsed();
            info.cost_microdollars = self
                .compaction_model
                .spec
                .pricing
                .as_ref()
                .map(|_| self.run_cost.microdollars.saturating_sub(cost_before));
        }

        self.finish_compaction(id, system, tools, reason, &operation, self.compaction_model);
        operation
    }

    async fn ensure_capacity(
        &mut self,
        system: &str,
        tools: &[ToolDef],
        compaction_reserve_tokens: u64,
        provider_output_ceiling: u64,
    ) -> Result<CapacityEstimate, AgentError> {
        let context_window = self.model.spec.limits.context_window;
        let budget = context_window.saturating_sub(compaction_reserve_tokens);
        let threshold = ((context_window as f64) * self.threshold_fraction).floor() as u64;
        let resolve = |input_tokens, active_system| CapacityEstimate {
            input_tokens,
            max_output_tokens: resolve_request_max_output_tokens(
                context_window,
                input_tokens,
                provider_output_ceiling,
            ),
            active_system,
        };
        let mut native_attempted = false;
        loop {
            let active_system = system.to_owned();
            let estimate = self
                .capacity
                .estimate(
                    self.session,
                    self.model,
                    &active_system,
                    tools,
                    self.tool_generation,
                )?
                .input_tokens;
            let over_capacity = estimate > budget;
            let over_threshold = estimate.saturating_add(compaction_reserve_tokens) > threshold;
            if !over_capacity && (self.mode == AgentCompactionMode::Disabled || !over_threshold) {
                return Ok(resolve(estimate, active_system));
            }
            if self.mode == AgentCompactionMode::Disabled {
                return Err(AgentError::ContextExceeded { estimate, budget });
            }
            let reason = if over_capacity {
                CompactionReason::Overflow
            } else {
                CompactionReason::Threshold
            };
            if self.mode == AgentCompactionMode::NativeResponses {
                // One native compaction attempt per capacity check. If the
                // provider returns an output that does not make progress, do
                // not loop forever or silently switch to local summarization.
                if native_attempted {
                    if over_capacity {
                        return Err(AgentError::ContextExceeded { estimate, budget });
                    }
                    return Ok(resolve(estimate, active_system));
                }
                self.compact_native_responses(&active_system, tools, reason)
                    .await?;
                native_attempted = true;
                continue;
            }
            // `keep_recent_tokens` is a preference, not permission to sail past
            // the configured threshold. If the retained episodes themselves
            // are unusually large, compact the oldest reducible episode.
            let boundary = self
                .preferred_boundary()?
                .or_else(|| self.oldest_reducible_boundary());
            if let Some(first_kept) = boundary {
                self.compact_boundary(first_kept, &active_system, tools, reason)
                    .await?;
                continue;
            }
            if estimate <= budget {
                return Ok(resolve(estimate, active_system));
            }
            return Err(AgentError::ContextExceeded { estimate, budget });
        }
    }

    async fn force_one_boundary(
        &mut self,
        system: &str,
        tools: &[ToolDef],
        compaction_reserve_tokens: u64,
    ) -> Result<(), AgentError> {
        if self.mode == AgentCompactionMode::NativeResponses {
            let active_system = system.to_owned();
            self.compact_native_responses(&active_system, tools, CompactionReason::Overflow)
                .await?;
            return Ok(());
        }
        let boundary = if self.mode == AgentCompactionMode::Local {
            self.preferred_boundary()?
                .or_else(|| self.oldest_reducible_boundary())
        } else {
            None
        };
        if let Some(first_kept) = boundary {
            self.compact_boundary(first_kept, system, tools, CompactionReason::Overflow)
                .await?;
            return Ok(());
        }
        let estimate = self
            .capacity
            .estimate(
                self.session,
                self.model,
                system,
                tools,
                self.tool_generation,
            )?
            .input_tokens;
        let budget = self
            .model
            .spec
            .limits
            .context_window
            .saturating_sub(compaction_reserve_tokens);
        Err(AgentError::ContextExceeded { estimate, budget })
    }
}

struct TerminalGateContext<'a> {
    client: &'a AiClient,
    model: &'a Model,
    session: &'a mut Session,
    usage: &'a mut Usage,
    run_cost: &'a mut CostAccumulator,
    cache_retention: CacheRetention,
    session_id: &'a str,
    max_session_tokens: Option<u64>,
    max_session_cost_microdollars: Option<u64>,
    abort: &'a AbortFlag,
}

impl TerminalGateContext<'_> {
    async fn decide(&mut self, capsule: String) -> Result<TerminalGateDecision, AgentError> {
        for _ in 0..TERMINAL_GATE_ATTEMPTS {
            let request = Request {
                system: Some(TERMINAL_GATE_SYSTEM.to_owned()),
                messages: vec![Message::User(UserMessage {
                    content: vec![UserPart::Text(capsule.clone())],
                })],
                tools: Vec::new(),
                tool_choice: ToolChoice::None,
                max_output_tokens: Some(1),
                temperature: Some(0.0),
                stop: Vec::new(),
                reasoning: ReasoningConfig::Off,
                reasoning_mode: ReasoningMode::Standard,
                responses: None,
                output_format: OutputFormat::Text,
                output_modalities: OutputModalities::Text,
                compatibility: CompatibilityMode::Strict,
                cache_retention: self.cache_retention,
                session_id: Some(format!("{}:terminal-gate", self.session_id)),
            };
            let input_tokens = estimate_request_tokens(
                request.system.as_deref().unwrap_or_default(),
                &request.messages,
                &request.tools,
            );
            let budget = self.model.spec.limits.context_window.saturating_sub(1);
            if input_tokens > budget {
                return Err(AgentError::ContextExceeded {
                    estimate: input_tokens,
                    budget,
                });
            }
            reserve_request_tokens(self.session, input_tokens, 1, self.max_session_tokens)?;
            reserve_request_cost(
                self.session,
                self.model,
                input_tokens,
                1,
                self.max_session_cost_microdollars,
            )?;
            let response = tokio::select! {
                biased;
                _ = self.abort.wait() => return Err(AgentError::Cancelled),
                response = self.client.complete(self.model, request) => response?,
            };
            if self.abort.is_set() {
                return Err(AgentError::Cancelled);
            }
            let decision = parse_terminal_gate(&response);
            add_usage(self.usage, &response.usage);
            let request_cost = response.cost;
            self.session.record_terminal_gate_usage(
                self.model.endpoint.id.clone(),
                self.model.spec.id.clone(),
                response.usage,
                request_cost,
                decision.map(|decision| decision == TerminalGateDecision::Return),
            )?;
            self.run_cost.add(request_cost);
            if let Some(decision) = decision {
                return Ok(decision);
            }
        }
        Err(AgentError::IncompleteResponse {
            stop_reason: "terminal gate returned neither R nor C after two attempts".to_owned(),
        })
    }
}

async fn open_provider_stream(
    client: &AiClient,
    model: &Model,
    request: Request,
    abort: &AbortFlag,
) -> Result<Option<ygg_ai::ResponseStream>, AiError> {
    tokio::select! {
        biased;
        _ = abort.wait() => Ok(None),
        result = client.stream(model, request) => result.map(Some),
    }
}

impl Agent {
    /// Creates a new agent: canonicalizes the sandbox workspace and validates
    /// the registered extensions (duplicate tool names are rejected).
    pub fn new(mut config: AgentConfig) -> Result<Self, AgentError> {
        if let Some(duplicate) = config.extensions.duplicate_tools.first() {
            return Err(AgentError::DuplicateTool(duplicate.clone()));
        }
        let workspace = config.sandbox.workspace.canonicalize().map_err(|e| {
            AgentError::Workspace(format!("{}: {e}", config.sandbox.workspace.display()))
        })?;
        if !workspace.is_dir() {
            return Err(AgentError::Workspace(format!(
                "{}: not a directory",
                workspace.display()
            )));
        }
        config.sandbox.workspace = workspace;
        let resource_owner = config.session.resource_owner_key();
        let session_id = config.session_id.unwrap_or_else(|| resource_owner.clone());
        let max_output_tokens = config.model.spec.limits.max_output_tokens;
        let tool_scope = next_tool_scope();
        Ok(Self {
            client: config.client,
            model: config.model,
            session: config.session,
            extensions: config.extensions,
            sandbox: config.sandbox,
            effect_broker: config.effect_broker,
            system: config.system,
            max_turns: config.max_turns,
            reasoning: config.reasoning,
            reasoning_mode: config.reasoning_mode,
            cache_retention: config.cache_retention,
            compaction_model: None,
            auto_compaction_mode: AgentCompactionMode::Local,
            compaction_threshold_fraction: 1.0,
            compaction_keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            session_id,
            resource_owner,
            tool_scope,
            completion_policy: CompletionPolicy::Natural,
            output_modalities: OutputModalities::Text,
            max_output_tokens,
            prompt_model_source: None,
            prompt_color: None,
            prompt_display_text: None,
            max_session_tokens: None,
            max_session_cost_microdollars: None,
            provider_retries_enabled: true,
            ultra_observation_managed: false,
            delegation: None,
            last_run_lifecycle: None,
        })
    }

    /// Builds an owned startup request that can establish the Responses
    /// WebSocket while the frontend is idle. The request contains only the
    /// current durable context and uses `generate=false` in `AiClient`; it is
    /// never part of agent accounting or persistence.
    pub fn responses_prewarm_request(
        &self,
    ) -> Result<Option<(AiClient, Model, Request)>, AgentError> {
        if !matches!(
            self.model.endpoint.transport,
            ygg_ai::EndpointTransport::WebSocketPreferred
        ) || self.model.spec.protocol != Protocol::OpenAiResponses
        {
            return Ok(None);
        }
        let responses = match self.auto_compaction_mode {
            AgentCompactionMode::NativeResponses => Some(native_responses_options(
                &self.session,
                &self.model,
                &self.system,
            )?),
            AgentCompactionMode::Local | AgentCompactionMode::Disabled => {
                durable_responses_options(&self.session, &self.model, &self.system)
            }
        };
        let request = Request {
            system: (!self.system.is_empty()).then(|| self.system.clone()),
            messages: self.session.context()?,
            tools: self.extensions.tool_definitions(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(self.max_output_tokens),
            temperature: None,
            stop: Vec::new(),
            reasoning: self.reasoning.clone(),
            reasoning_mode: self.reasoning_mode,
            responses,
            output_format: OutputFormat::Text,
            output_modalities: self.output_modalities.clone(),
            compatibility: CompatibilityMode::Strict,
            cache_retention: self.cache_retention,
            session_id: Some(self.session_id.clone()),
        };
        Ok(Some((self.client.clone(), self.model.clone(), request)))
    }

    /// Read-only access to the agent's session (its entries and head).
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) fn resource_owner_id(&self) -> &str {
        &self.resource_owner
    }

    /// Persist a non-model-visible terminal marker for a frontend-owned run.
    ///
    /// Callers should record this only after the [`Run`] has been dropped, so
    /// the run no longer holds the authoritative mutable session borrow.
    pub fn record_run_outcome(
        &mut self,
        outcome: SessionRunOutcome,
    ) -> Result<EntryId, AgentError> {
        self.session.append_run_outcome(outcome).map_err(Into::into)
    }

    /// Read-only access to the selected model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Requested output modalities for subsequent model turns.
    ///
    /// Text is the default. Generated audio is currently delivered as a
    /// complete [`AgentEvent::OutputMedia`] event and retained in
    /// [`RunOutput::media`]; unsupported requests fail through `ygg-ai`'s
    /// normal capability validation.
    pub fn output_modalities(&self) -> &OutputModalities {
        &self.output_modalities
    }

    /// Configure output modalities for subsequent runs.
    pub fn set_output_modalities(&mut self, output_modalities: OutputModalities) {
        self.output_modalities = output_modalities;
        self.sync_delegation_runtime_settings();
    }

    /// Replace the system prompt at an idle boundary. Product frontends use
    /// this to apply typed extension context without exposing private agent
    /// state; the value is cloned into the next run when [`prompt`](Self::prompt)
    /// starts.
    pub fn set_system_prompt(&mut self, system: impl Into<String>) {
        let system = system.into();
        if let Some(binding) = &self.delegation {
            binding.update_base_system(system.clone());
        }
        self.system = system;
        let delegation_instructions = self
            .delegation
            .as_ref()
            .map(|binding| binding.system_instructions().to_owned())
            .filter(|instructions| !instructions.is_empty());
        if let Some(instructions) = delegation_instructions {
            self.append_system_instructions(instructions);
        }
    }

    /// Returns the complete system prompt used by subsequent runs.
    pub fn system_prompt(&self) -> &str {
        &self.system
    }

    /// Enables the bounded host-side V2 collaboration runtime.
    ///
    /// Child agents inherit the resolved model, sandbox, approved extension
    /// tools, reasoning, compaction, completion, and cost settings present at
    /// this idle boundary. Each child receives an isolated durable session.
    pub fn enable_v2_delegation(
        &mut self,
        config: DelegationConfig,
    ) -> Result<std::path::PathBuf, DelegationError> {
        self.enable_v2_delegation_with_surface(config, true)
    }

    /// Enables the bounded V2 runtime without exposing native root
    /// collaboration tools. Product layers use this when an extension owns the
    /// user-facing orchestration and observation surface.
    pub fn enable_v2_delegation_extension_only(
        &mut self,
        config: DelegationConfig,
    ) -> Result<std::path::PathBuf, DelegationError> {
        self.enable_v2_delegation_with_surface(config, false)
    }

    fn enable_v2_delegation_with_surface(
        &mut self,
        config: DelegationConfig,
        root_tools: bool,
    ) -> Result<std::path::PathBuf, DelegationError> {
        if self.delegation.is_some() {
            return Err(DelegationError::AlreadyEnabled);
        }
        let template = DelegationTemplate {
            client: self.client.clone(),
            model: self.model.clone(),
            base_system: std::sync::RwLock::new(self.system.clone()),
            sandbox: self.sandbox.clone(),
            effect_broker: self.effect_broker.clone(),
            extensions: self.extensions.clone(),
            max_turns: self.max_turns,
            reasoning: self.reasoning.clone(),
            reasoning_mode: self.reasoning_mode,
            cache_retention: self.cache_retention,
            runtime: std::sync::RwLock::new(self.delegation_runtime_settings()),
        };
        let binding = enable_root_delegation(self, config, template, root_tools)?;
        let team_directory = binding.team_directory().to_path_buf();
        self.delegation = Some(binding);
        Ok(team_directory)
    }

    /// Returns the private team directory when V2 delegation is enabled.
    pub fn delegation_team_directory(&self) -> Option<&std::path::Path> {
        self.delegation
            .as_ref()
            .map(DelegationBinding::team_directory)
    }

    /// Opens one exact child transcript from this agent's current delegation
    /// team as a read-only session. Opaque references from another parent are
    /// never resolved.
    pub fn open_delegated_session_reference(
        &self,
        extension_principal: &str,
        reference: &str,
    ) -> Result<Option<Session>, AgentError> {
        let Some(binding) = self.delegation.as_ref() else {
            return Ok(None);
        };
        binding.open_session_reference(extension_principal, reference)
    }

    /// Binds an executable extension's negotiated child-session service to
    /// this root agent's V2 delegation manager.
    pub fn bind_extension_agent_sessions(
        &self,
        process: &ExtensionProcess,
    ) -> Result<bool, AgentError> {
        if !process
            .negotiated_features()
            .contains(EXTENSION_FEATURE_AGENT_SESSIONS)
        {
            return Ok(false);
        }
        let binding = self.delegation.as_ref().ok_or_else(|| {
            AgentError::Delegation(
                "an extension negotiated agent_sessions before V2 delegation was enabled".into(),
            )
        })?;
        let service = binding
            .extension_service(
                process.agent_session_principal(),
                self.session_id.clone(),
                self.resource_owner.clone(),
            )
            .map_err(AgentError::Delegation)?;
        process
            .bind_agent_session_service(service)
            .map_err(|error| AgentError::Delegation(error.to_string()))?;
        Ok(true)
    }

    fn delegation_runtime_settings(&self) -> DelegationRuntimeSettings {
        DelegationRuntimeSettings {
            compaction_model: self.compaction_model.clone(),
            auto_compaction_mode: self.auto_compaction_mode,
            auto_compaction_threshold: self.compaction_threshold_fraction,
            compaction_keep_recent_tokens: self.compaction_keep_recent_tokens,
            completion_policy: self.completion_policy,
            output_modalities: self.output_modalities.clone(),
            max_output_tokens: self.max_output_tokens,
            max_session_tokens: self.max_session_tokens,
            max_session_cost_microdollars: self.max_session_cost_microdollars,
            provider_retries_enabled: self.provider_retries_enabled,
        }
    }

    fn sync_delegation_runtime_settings(&self) {
        if let Some(binding) = &self.delegation {
            binding.update_runtime_settings(self.delegation_runtime_settings());
        }
    }

    pub(crate) fn append_system_instructions(&mut self, instructions: String) {
        if !self.system.is_empty() {
            self.system.push_str("\n\n");
        }
        self.system.push_str(&instructions);
    }

    pub(crate) fn install_delegation_tools(&mut self, tools: Vec<Arc<dyn Tool>>) {
        for tool in tools {
            self.extensions.tool_arc(tool);
        }
    }

    pub(crate) fn set_delegation_binding(
        &mut self,
        binding: DelegationBinding,
    ) -> Result<(), DelegationError> {
        if self.delegation.is_some() {
            return Err(DelegationError::AlreadyEnabled);
        }
        self.delegation = Some(binding);
        Ok(())
    }

    pub(crate) fn mark_ultra_observation_managed(&mut self) {
        self.ultra_observation_managed = true;
    }

    /// Set the stable semantic creator/source key persisted with future user
    /// prompts (for example `openai` or `deepseek`). This is presentation
    /// metadata only and never enters provider-visible message content.
    pub fn set_prompt_model_source(&mut self, source: Option<String>) {
        self.prompt_model_source = source.filter(|source| !source.trim().is_empty());
    }

    /// Set the exact inert sRGB highlight persisted with future user prompts.
    /// Validation and normalization happen at the durable session boundary.
    pub fn set_prompt_color(&mut self, color: Option<String>) {
        self.prompt_color = color.filter(|color| !color.trim().is_empty());
    }

    /// Set the transcript text for the next submitted prompt. It is consumed
    /// exactly once by `prompt`; model-visible text remains in the durable
    /// message payload for replay. An explicitly empty string is retained so
    /// media-only turns do not expose synthetic model instructions as caller text.
    pub fn set_prompt_display_text(&mut self, text: Option<String>) {
        self.prompt_display_text = text;
    }

    fn prompt_entry_metadata(&mut self) -> EntryMetadata {
        EntryMetadata {
            prompt_model: Some(self.model.spec.id.clone()),
            prompt_model_source: self.prompt_model_source.clone(),
            prompt_color: self.prompt_color.clone(),
            display_text: self.prompt_display_text.take(),
            run_outcome: None,
            tool_output: None,
            tool_started_unix_ms: None,
            tool_finished_unix_ms: None,
            local_synthetic_assistant: false,
        }
    }

    /// Check a prospective provider request against this agent's configured
    /// conservative cost reservation. Product-level manual subrequests use
    /// this same gate as autonomous turns.
    pub fn ensure_request_cost_capacity(
        &self,
        model: &Model,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), AgentError> {
        reserve_request_cost(
            &self.session,
            model,
            input_tokens,
            output_tokens,
            self.max_session_cost_microdollars,
        )
    }

    /// Configure a conservative hard ceiling for total provider-reported
    /// tokens across this session. Every provider operation reserves its
    /// estimated input plus maximum output before network I/O.
    pub(crate) fn set_max_session_tokens(&mut self, limit: Option<u64>) {
        self.max_session_tokens = limit;
        self.sync_delegation_runtime_settings();
    }

    /// Configure a conservative hard ceiling for billable session requests.
    /// Before every normal or compaction request, priced models reserve their
    /// worst-case input/output cost; a request that could cross the ceiling is
    /// rejected before network I/O.
    pub fn set_max_session_cost_microdollars(&mut self, limit: Option<u64>) {
        self.max_session_cost_microdollars = limit;
        self.sync_delegation_runtime_settings();
    }

    /// Enable or disable transient provider retries for subsequent runs.
    pub fn set_provider_retries_enabled(&mut self, enabled: bool) {
        self.provider_retries_enabled = enabled;
        self.sync_delegation_runtime_settings();
    }

    /// Configure the model used for autonomous context summaries. Passing
    /// `None` keeps summaries on the active conversation model.
    pub fn set_compaction_model(&mut self, model: Option<Model>) {
        self.compaction_model = model;
        self.sync_delegation_runtime_settings();
    }

    /// Read-only access to the autonomous compaction model, if overridden.
    pub fn compaction_model(&self) -> Option<&Model> {
        self.compaction_model.as_ref()
    }

    /// Approximate token budget used to migrate one deprecated retained turn.
    const LEGACY_COMPACTION_TOKENS_PER_TURN: u64 = 1_000;

    /// Configure autonomous compaction with the deprecated turn-count API.
    ///
    /// Each retained turn maps to a 1,000-token tail budget. Use
    /// [`Self::set_compaction_token_policy`] for exact token control.
    #[deprecated(note = "use set_compaction_token_policy")]
    pub fn set_compaction_policy(
        &mut self,
        enabled: bool,
        threshold_fraction: f64,
        keep_recent_turns: usize,
    ) -> Result<(), AgentError> {
        self.set_compaction_token_policy(
            enabled,
            threshold_fraction,
            u64::try_from(keep_recent_turns)
                .unwrap_or(u64::MAX)
                .saturating_mul(Self::LEGACY_COMPACTION_TOKENS_PER_TURN),
        )
    }

    /// Configure autonomous compaction with the deprecated turn-count API.
    #[deprecated(note = "use set_compaction_token_mode")]
    pub fn set_compaction_mode(
        &mut self,
        mode: AgentCompactionMode,
        threshold_fraction: f64,
        keep_recent_turns: usize,
    ) -> Result<(), AgentError> {
        self.set_compaction_token_mode(
            mode,
            threshold_fraction,
            u64::try_from(keep_recent_turns)
                .unwrap_or(u64::MAX)
                .saturating_mul(Self::LEGACY_COMPACTION_TOKENS_PER_TURN),
        )
    }

    /// Current deprecated turn-count compaction policy.
    #[deprecated(note = "use compaction_token_policy")]
    pub fn compaction_policy(&self) -> (bool, f64, usize) {
        let (enabled, threshold, tokens) = self.compaction_token_policy();
        (
            enabled,
            threshold,
            usize::try_from(tokens / Self::LEGACY_COMPACTION_TOKENS_PER_TURN)
                .unwrap_or(usize::MAX)
                .max(1),
        )
    }

    /// Configure autonomous context compaction for subsequent runs.
    ///
    /// `threshold_fraction` is the fraction of the complete model context
    /// available to current input plus the independently resolved compaction
    /// reserve. The default `1.0` therefore adds no percentage buffer.
    /// `keep_recent_tokens` is the approximate verbatim tail budget; when the
    /// retained tail alone exceeds the configured threshold or capacity,
    /// recovery advances the boundary until the request fits.
    pub fn set_compaction_token_policy(
        &mut self,
        enabled: bool,
        threshold_fraction: f64,
        keep_recent_tokens: u64,
    ) -> Result<(), AgentError> {
        self.set_compaction_token_mode(
            if enabled {
                AgentCompactionMode::Local
            } else {
                AgentCompactionMode::Disabled
            },
            threshold_fraction,
            keep_recent_tokens,
        )
    }

    /// Configure the autonomous compaction strategy for subsequent runs.
    pub fn set_compaction_token_mode(
        &mut self,
        mode: AgentCompactionMode,
        threshold_fraction: f64,
        keep_recent_tokens: u64,
    ) -> Result<(), AgentError> {
        if !threshold_fraction.is_finite() || threshold_fraction <= 0.0 || threshold_fraction > 1.0
        {
            return Err(AgentError::InvalidCompactionPolicy(
                "threshold fraction must be finite and between 0 and 1".to_owned(),
            ));
        }
        if keep_recent_tokens == 0 {
            return Err(AgentError::InvalidCompactionPolicy(
                "keep_recent_tokens must be at least 1".to_owned(),
            ));
        }
        if mode == AgentCompactionMode::NativeResponses
            && self.model.spec.protocol != Protocol::OpenAiResponses
        {
            return Err(AgentError::InvalidCompactionPolicy(
                "native Responses compaction requires an OpenAI Responses model route".to_owned(),
            ));
        }
        if mode == AgentCompactionMode::NativeResponses
            && self
                .session
                .responses_replay_items(&self.model.endpoint.id, &self.model.spec.id)?
                .is_none()
        {
            return Err(AgentError::InvalidCompactionPolicy(
                "native Responses compaction requires complete route-affine opaque replay on the active branch"
                    .to_owned(),
            ));
        }
        self.auto_compaction_mode = mode;
        self.compaction_threshold_fraction = threshold_fraction;
        self.compaction_keep_recent_tokens = keep_recent_tokens;
        self.sync_delegation_runtime_settings();
        Ok(())
    }

    /// Current autonomous compaction policy `(enabled, threshold, keep)`.
    pub fn compaction_token_policy(&self) -> (bool, f64, u64) {
        (
            self.auto_compaction_mode != AgentCompactionMode::Disabled,
            self.compaction_threshold_fraction,
            self.compaction_keep_recent_tokens,
        )
    }

    /// Current autonomous compaction strategy.
    pub fn compaction_mode(&self) -> AgentCompactionMode {
        self.auto_compaction_mode
    }

    /// Provider-advertised output ceiling for the active model.
    pub fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    /// Minimum output headroom used by autonomous capacity checks.
    pub fn compaction_reserve_tokens(&self) -> u64 {
        agent_compaction_reserve_tokens(&self.model, &self.reasoning)
    }

    #[cfg(test)]
    pub(crate) fn max_session_tokens(&self) -> Option<u64> {
        self.max_session_tokens
    }

    /// Apply the root agent's provider output ceiling to a delegated child.
    pub(crate) fn inherit_max_output_tokens(&mut self, max_output_tokens: u64) {
        self.max_output_tokens = max_output_tokens;
        self.sync_delegation_runtime_settings();
    }

    /// Estimate the next request using the same provider-reconciled baseline
    /// as autonomous capacity checks, without mutating the session.
    pub fn request_context_estimate(&self) -> Result<RequestContextEstimate, SessionError> {
        let messages = self.session.context_ref()?;
        let system = self.system.clone();
        let tools = self.extensions.tool_definitions();
        Ok(reconcile_context_estimate(
            &self.session,
            &self.model,
            &system,
            &messages,
            &tools,
        ))
    }

    /// Build the detailed, provider-reconciled context categories on demand.
    pub fn request_context_breakdown(&self) -> Result<ContextBreakdown, SessionError> {
        let messages = self.session.context_ref()?;
        let system = self.system.clone();
        let tools = self.extensions.tool_definitions();
        Ok(context_breakdown(
            &self.session,
            &self.model,
            &system,
            &messages,
            &tools,
        ))
    }

    /// Complete route-affine Responses replay input for the active branch.
    ///
    /// `None` means the active route is not Responses or a legacy/crash gap
    /// makes exact opaque replay unavailable. Route-mismatched sidecars are
    /// returned as an explicit session error.
    pub fn responses_replay_input(&self) -> Result<Option<ResponsesInput>, SessionError> {
        if self.model.spec.protocol != Protocol::OpenAiResponses {
            return Ok(None);
        }
        let Some(replay) = self
            .session
            .responses_replay_items(&self.model.endpoint.id, &self.model.spec.id)?
        else {
            return Ok(None);
        };
        let system = self.system.clone();
        Ok(Some(ygg_ai::responses::encode_responses_replay(
            &self.model,
            (!system.is_empty()).then_some(system.as_str()),
            &replay,
        )))
    }

    /// Performs one native Responses compaction while the agent is idle.
    ///
    /// The complete unpruned provider output is durably appended as a
    /// route-affine branch checkpoint and becomes the next replay base.
    pub async fn compact_responses_native(&mut self) -> Result<CompactionInfo, AgentError> {
        if self.model.spec.protocol != Protocol::OpenAiResponses {
            return Err(AgentError::InvalidCompactionPolicy(
                "native Responses compaction requires an OpenAI Responses model route".to_owned(),
            ));
        }
        if current_head_is_native_checkpoint(&self.session, &self.model) {
            return Err(AgentError::InvalidCompactionPolicy(
                "native Responses compaction made no progress since the previous checkpoint"
                    .to_owned(),
            ));
        }
        let replay = self
            .session
            .responses_replay_items(&self.model.endpoint.id, &self.model.spec.id)?
            .ok_or_else(|| {
                AgentError::InvalidCompactionPolicy(
                    "native Responses compaction requires complete route-affine opaque replay"
                        .to_owned(),
                )
            })?;
        let input = ygg_ai::responses::encode_responses_replay(&self.model, None, &replay);
        let active_system = self.system.clone();
        let instructions = (!active_system.is_empty()).then_some(active_system.as_str());
        let tools = self.extensions.tool_definitions();
        if replay.is_empty() {
            return Err(AgentError::InvalidCompactionPolicy(
                "native Responses compaction requires non-empty replay".to_owned(),
            ));
        }
        let request = ResponsesCompactRequest::for_model(
            &self.model,
            input,
            instructions.map(str::to_owned),
            &tools,
            &self.reasoning,
            self.reasoning_mode,
            &OutputFormat::Text,
            self.cache_retention,
            Some(&self.session_id),
        );
        let input_tokens = estimate_compact_request_tokens(&request, &replay);
        reserve_request_tokens(
            &self.session,
            input_tokens,
            self.model.spec.limits.max_output_tokens,
            self.max_session_tokens,
        )?;
        reserve_request_cost(
            &self.session,
            &self.model,
            input_tokens,
            self.model.spec.limits.max_output_tokens,
            self.max_session_cost_microdollars,
        )?;
        let covered_through = self.session.head().ok_or(SessionError::EmptySession)?;
        let operation_started = std::time::Instant::now();
        let response = self.client.compact_responses(&self.model, request).await?;
        let cost = self
            .model
            .spec
            .pricing
            .as_ref()
            .and_then(|pricing| ygg_ai::pricing::cost_of(pricing, &response.usage).ok());
        self.session.record_compaction_usage(
            self.model.endpoint.id.clone(),
            self.model.spec.id.clone(),
            response.usage,
            cost,
        )?;
        validate_native_compact_output(&response.output)?;
        let checkpoint = self.session.append_responses_compaction(
            self.model.endpoint.id.clone(),
            self.model.spec.id.clone(),
            response.output,
        )?;
        Ok(CompactionInfo {
            kind: CompactionKind::NativeResponses {
                checkpoint,
                covered_through: covered_through.clone(),
            },
            summary: String::new(),
            first_kept: covered_through,
            usage: response.usage,
            elapsed: operation_started.elapsed(),
            cost_microdollars: cost.map(|cost| cost.total),
        })
    }

    /// Mutable access to the session for history operations between runs
    /// (checkout, manual compaction, config entries).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Selects completion behavior for subsequent runs.
    pub fn set_completion_policy(&mut self, policy: CompletionPolicy) {
        self.completion_policy = policy;
        self.sync_delegation_runtime_settings();
    }

    /// Returns the selected completion policy.
    pub fn completion_policy(&self) -> CompletionPolicy {
        self.completion_policy
    }

    /// Provider schemas for all currently executable tools, in wire order.
    pub fn registered_tool_definitions(&self) -> Vec<ToolDef> {
        self.extensions.tool_definitions()
    }

    /// Exact registered tool names after the frontend has applied all policy
    /// filters and extension registration. The sorted result is suitable for
    /// deterministic diagnostics and capability validation at idle boundaries.
    pub fn registered_tool_names(&self) -> Vec<String> {
        let mut names = self
            .extensions
            .tool_snapshot()
            .1
            .iter()
            .map(|tool| tool.definition().name)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Reconciles unresolved calls from the latest persisted assistant turn.
    ///
    /// Only tools explicitly marked [`ReplaySafety::Safe`] execute again.
    /// Every other call receives a durable indeterminate error, preserving
    /// provider call/result pairing without silently duplicating an external
    /// mutation after a process crash.
    async fn recover_pending_tools(
        &mut self,
        previous_run_was_dropped: bool,
    ) -> Result<(), AgentError> {
        let Some((calls, persisted)) = pending_tool_state(&self.session) else {
            return Ok(());
        };
        // Keep each call's original assistant-turn index. Filtering first
        // would renumber unresolved calls and let crash recovery execute calls
        // that the live path would have skipped after the per-turn limit.
        let unresolved: Vec<(usize, ToolCall)> = calls
            .into_iter()
            .enumerate()
            .filter(|(_, call)| !persisted.contains(&call.id))
            .collect();
        if unresolved.is_empty() {
            return Ok(());
        }

        if previous_run_was_dropped {
            persist_pending_cancellations(&mut self.session)?;
            return Ok(());
        }

        let (tool_generation, tools) = self.extensions.tool_snapshot();
        let mut tool_map: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        for tool in &tools {
            let definition = tool.definition();
            tool_map.insert(definition.name, Arc::clone(tool));
        }
        let mut registered_tools = tool_map.keys().cloned().collect::<Vec<_>>();
        registered_tools.sort();
        let sandbox = self.sandbox.clone();
        let tool_scope = self.tool_scope.clone();
        let resource_owner = self.resource_owner.clone();
        let recovery_run_id = format!("{tool_scope}:recovery");
        let effect_broker = self.effect_broker.clone();
        let tool_call_hooks = self.extensions.tool_call_hooks.clone();
        for (call_index, call) in unresolved {
            let result = if let Some(argument_error) = call.argument_error {
                // A schema-rejected call was never admitted for execution in
                // the live path; retain that fact across a restart as well.
                Err(rejected_argument_tool_error(argument_error))
            } else if call_index >= MAX_TOOL_CALLS_PER_TURN {
                Err(ToolError::new(
                    "tool call skipped: per-turn tool-call limit reached",
                ))
            } else {
                match tool_map.get(&call.name) {
                    None => Err(ToolError::new(format!("unknown tool: {}", call.name))),
                    Some(tool) if tool.replay_safety() == ReplaySafety::Safe => {
                        execute_recovery_call(
                            Arc::clone(tool),
                            &tool_call_hooks,
                            &effect_broker,
                            tool_generation,
                            &recovery_run_id,
                            &call,
                            &sandbox,
                            &tool_scope,
                            &resource_owner,
                            &registered_tools,
                            &mut self.session,
                        )
                        .await?
                    }
                    Some(_) => Err(ToolError::new(format!(
                        "indeterminate after restart: `{}` may have completed before its result was persisted; Ygg did not replay it. Inspect external state and retry explicitly if needed",
                        call.name
                    ))),
                }
            };
            let (message, _, _, _, details) = lower_tool_result(
                call.id,
                &result,
                &self.model,
                sandbox.max_output_bytes,
                Vec::new(),
            );
            self.session.append_with_metadata(
                EntryValue::Message(Message::User(message)),
                details.map(|tool_output| EntryMetadata {
                    tool_output: Some(tool_output),
                    ..EntryMetadata::default()
                }),
            )?;
            resolve_tool_delivery_after_persistence(&result, sandbox.max_output_bytes);
        }
        Ok(())
    }

    /// Begins a run: appends the user message to the session and returns the
    /// caller-driven event stream plus its control handle.
    ///
    /// Pre-flight failures (e.g. the session append) are returned here; once
    /// the run has started every terminal outcome — completed, aborted,
    /// failed, or max-turns — is reported by exactly one
    /// [`AgentEvent::RunFinished`].
    pub async fn prompt(&mut self, input: impl Into<UserInput>) -> Result<Run<'_>, AgentError> {
        self.prompt_with_tools(input.into(), true).await
    }

    /// Begins a run whose provider requests expose no tools. This is used for
    /// explicit answer-now flows that must synthesize from existing evidence.
    pub async fn prompt_without_tools(
        &mut self,
        input: impl Into<UserInput>,
    ) -> Result<Run<'_>, AgentError> {
        self.prompt_with_tools(input.into(), false).await
    }

    async fn prompt_with_tools(
        &mut self,
        input: UserInput,
        tools_enabled: bool,
    ) -> Result<Run<'_>, AgentError> {
        if self.reasoning == ygg_ai::ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Ultra)
            && self.delegation.is_none()
            && !self.ultra_observation_managed
        {
            return Err(AgentError::Delegation(
                "Ultra requires an enabled child-session observation runtime".into(),
            ));
        }
        // Direct library callers may not have an explicit construction
        // boundary. Keep this idempotent fallback so their first owning run
        // cannot leave dynamic publishers waiting forever.
        self.extensions.finalize_tool_surface();
        // A previous process may have died after persisting an assistant tool
        // call but before persisting its result. Repair that semantic boundary
        // before appending a new user message; otherwise strict provider
        // validation would reject the resumed conversation as malformed.
        let previous_run_was_dropped = self
            .last_run_lifecycle
            .take()
            .is_some_and(|lifecycle| lifecycle.dropped.load(Ordering::Acquire));
        self.recover_pending_tools(previous_run_was_dropped).await?;
        let terminal_gate_prior_context =
            if self.completion_policy == CompletionPolicy::TerminalGate {
                recent_conversational_context(&self.session.context()?)
            } else {
                String::new()
            };
        let initial_request = input.text_summary();
        let prompt_metadata = self.prompt_entry_metadata();
        // `display_text` belongs only to the draft that started this run.
        // Steering and follow-up inputs are independent user submissions and
        // must render their own durable message bodies after replay.
        let control_prompt_metadata = EntryMetadata {
            display_text: None,
            ..prompt_metadata.clone()
        };
        let observer_input = (!self.extensions.observers.is_empty()).then(|| input.clone());
        let first_entry = self
            .session
            .append_with_metadata(user_message(input), Some(prompt_metadata.clone()))?;
        if let Some(input) = observer_input.as_ref() {
            for observer in &self.extensions.observers {
                observer.on_run_started_for_owner(
                    &first_entry.0,
                    input,
                    &self.model,
                    &self.resource_owner,
                );
            }
        }
        let lifecycle = Arc::new(RunLifecycle {
            finished: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
        });
        self.last_run_lifecycle = Some(lifecycle.clone());
        let context = Arc::new(ContextTracker::default());
        let stream_context = context.clone();

        let (control_tx, mut control_rx) = mpsc::channel::<Control>(8);
        let abort = Arc::new(AbortFlag::default());
        let control = RunControl {
            tx: control_tx,
            abort: abort.clone(),
        };

        // Disjoint borrows: the run stream owns clones of everything except
        // the session, which it borrows mutably for the run's lifetime —
        // preserving one authoritative head.
        let client = self.client.clone();
        let model = self.model.clone();
        let compaction_model = self
            .compaction_model
            .clone()
            .unwrap_or_else(|| model.clone());
        let system = self.system.clone();
        let sandbox = self.sandbox.clone();
        let extension_host = self.extensions.clone();
        let (initial_tool_revision, initial_tools) = extension_host.tool_snapshot();
        let initial_tool_defs: Vec<ToolDef> = if tools_enabled {
            initial_tools.iter().map(|tool| tool.definition()).collect()
        } else {
            Vec::new()
        };
        let initial_context =
            observe_context_tracker(&context, &self.session, &model, &system, &initial_tool_defs)?;
        let initial_capacity =
            ContextCapacityCache::seeded(&self.session, initial_tool_revision, &initial_context);
        if let Some(delegation) = &self.delegation {
            delegation.prepare_owning_run()?;
        }
        let observers = ObserverDispatch {
            observers: self.extensions.observers.clone(),
            resource_owner: self.resource_owner.clone(),
        };
        let tool_call_hooks = self.extensions.tool_call_hooks.clone();
        let max_turns = self.max_turns;
        let reasoning = self.reasoning.clone();
        let reasoning_mode = self.reasoning_mode;
        let cache_retention = self.cache_retention;
        let session_id = self.session_id.clone();
        let resource_owner = self.resource_owner.clone();
        let tool_scope = self.tool_scope.clone();
        let effect_broker = self.effect_broker.clone();
        let effect_run_id = format!("run:{}", first_entry.0);
        let completion_policy = self.completion_policy;
        let output_modalities = self.output_modalities.clone();
        let provider_output_ceiling = self.max_output_tokens;
        let compaction_reserve_tokens = self.compaction_reserve_tokens();
        let max_session_tokens = self.max_session_tokens;
        let max_session_cost_microdollars = self.max_session_cost_microdollars;
        let auto_compaction_mode = self.auto_compaction_mode;
        let compaction_threshold_fraction = self.compaction_threshold_fraction;
        let compaction_keep_recent_tokens = self.compaction_keep_recent_tokens;
        let provider_retries_enabled = self.provider_retries_enabled;
        let stream_delegation = self.delegation.clone();
        let run_delegation = self.delegation.clone();
        let mut delegation_telemetry = self
            .delegation
            .as_ref()
            .and_then(DelegationBinding::telemetry_receiver);
        let stream_lifecycle = lifecycle.clone();
        let session = &mut self.session;

        let stream = async_stream::stream! {
            // This guard owns the mutable session borrow for exactly as long as
            // the generated stream. If the caller drops the stream at any
            // suspension point, its Drop implementation durably pairs pending
            // tool calls before `Run::drop` returns.
            let mut session_guard = RunSessionGuard {
                session,
                lifecycle: stream_lifecycle.clone(),
            };
            let session = &mut *session_guard;
            let mut context_capacity = initial_capacity;

            let (mut tool_revision, tools) = extension_host.tool_snapshot();
            let mut tool_defs: Vec<ToolDef> = if tools_enabled {
                tools.iter().map(|tool| tool.definition()).collect()
            } else {
                Vec::new()
            };
            let mut tool_map: HashMap<String, Arc<dyn Tool>> =
                HashMap::with_capacity(if tools_enabled { tools.len() } else { 0 });
            if tools_enabled {
                for tool in &tools {
                    let definition = tool.definition();
                    tool_map.insert(definition.name, Arc::clone(tool));
                }
            }
            let mut registered_tools = tool_map.keys().cloned().collect::<Vec<_>>();
            registered_tools.sort();
            // Tool names already visible to the provider either as static
            // schemas or via an earlier `added_tool_names` announcement.
            let mut announced_tools: std::collections::HashSet<String> =
                registered_tools.iter().cloned().collect();

            let mut pending_steer: Vec<UserInput> = Vec::new();
            let mut followups: VecDeque<UserInput> = VecDeque::new();
            // Preserve Ygg's historical defaults; frontends that expose queue
            // modes can update either mode through RunControl.
            let mut steering_mode = QueueDeliveryMode::All;
            let mut follow_up_mode = QueueDeliveryMode::OneAtATime;
            let mut control_open = true;
            let mut answer_only = !tools_enabled;
            let mut finish_pending = false;
            let mut completed_turns: u64 = 0;
            let mut terminal_gate_requests = vec![initial_request];
            let mut terminal_action_receipts = Vec::<TerminalActionReceipt>::new();
            let mut context_retries = 0usize;
            let mut run_usage = Usage::default();
            let mut run_cost = CostAccumulator::default();
            let mut speculative_bash = SpeculativeBash::default();
            let mut recent_tool_calls: VecDeque<(String, String)> =
                VecDeque::with_capacity(MAX_RECENT_TOOL_CALLS);

            let mut reason: FinishReason = 'run: loop {
                // Cancel any speculative bash executions left over from a
                // previous provider turn; every surviving call of a completed
                // turn was consumed or discarded at its commit point.
                speculative_bash.begin_turn();
                // ── Drain control at the turn boundary ─────────────────────
                while control_open {
                    match control_rx.try_recv() {
                        Ok(Control::Steer(input)) => pending_steer.push(input),
                        Ok(Control::FollowUp(input)) => followups.push_back(input),
                        Ok(Control::FinishNow(input)) => {
                            pending_steer.push(input);
                            answer_only = true;
                            finish_pending = true;
                            context_capacity.invalidate();
                        }
                        Ok(Control::SetSteeringMode(mode)) => steering_mode = mode,
                        Ok(Control::SetFollowUpMode(mode)) => follow_up_mode = mode,
                        Ok(Control::Abort) => break 'run FinishReason::Aborted,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => control_open = false,
                    }
                }
                if abort.is_set() {
                    break 'run FinishReason::Aborted;
                }

                // ── Steering enters here, at the model-turn boundary ───────
                if !pending_steer.is_empty() {
                    let queued = if std::mem::take(&mut finish_pending) {
                        std::mem::take(&mut pending_steer)
                    } else {
                        match steering_mode {
                            QueueDeliveryMode::All => std::mem::take(&mut pending_steer),
                            QueueDeliveryMode::OneAtATime => vec![pending_steer.remove(0)],
                        }
                    };
                    let visible_tools = if answer_only {
                        &[][..]
                    } else {
                        tool_defs.as_slice()
                    };
                    let observation = ContextObservation {
                        tracker: &stream_context,
                        model: &model,
                        system: &system,
                        tools: visible_tools,
                    };
                    match deliver_control_inputs(
                        queued,
                        ControlDeliveryKind::Steering,
                        session,
                        &control_prompt_metadata,
                        &mut terminal_gate_requests,
                        &observation,
                    ) {
                        ControlDelivery::Completed { event } => {
                            if let Some(ev) = event {
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                        }
                        ControlDelivery::Interrupted { event, finish } => {
                            if let Some(ev) = event {
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            break 'run finish;
                        }
                    }
                }

                // ── Turn guard ─────────────────────────────────────────────
                if let Some(limit) = max_turns {
                    if completed_turns >= limit {
                        break 'run FinishReason::MaxTurns;
                    }
                }

                // Freeze one coherent schema/implementation snapshot after
                // control and steering have settled but before context sizing.
                // Every call emitted by this request resolves against exactly
                // the tool set the provider saw.
                let (current_revision, current_tools) = extension_host.tool_snapshot();
                if current_revision != tool_revision {
                    tool_revision = current_revision;
                    if tools_enabled && !answer_only {
                        tool_defs = current_tools
                            .iter()
                            .map(|tool| tool.definition())
                            .collect();
                        tool_map.clear();
                        tool_map.reserve(current_tools.len());
                        for tool in &current_tools {
                            let definition = tool.definition();
                            tool_map.insert(definition.name, Arc::clone(tool));
                        }
                        registered_tools = tool_map.keys().cloned().collect();
                        registered_tools.sort();
                        announced_tools.extend(registered_tools.iter().cloned());
                    }
                }
                let request_tool_defs = if answer_only {
                    Vec::new()
                } else {
                    tool_defs.clone()
                };

                // ── Reconstruct and size context for this exact turn ───────
                // This gate is inside the autonomous loop, after every tool
                // result, and uses the exact active tool schema set.
                let (compaction_event_tx, mut compaction_event_rx) =
                    mpsc::unbounded_channel::<AgentEvent>();
                let capacity = {
                    let mut compaction = CompactionContext {
                        client: &client,
                        model: &model,
                        compaction_model: &compaction_model,
                        session,
                        usage: &mut run_usage,
                        run_cost: &mut run_cost,
                        cache_retention,
                        reasoning: &reasoning,
                        reasoning_mode,
                        session_id: &session_id,
                        max_session_tokens,
                        max_session_cost_microdollars,
                        abort: &abort,
                        mode: auto_compaction_mode,
                        threshold_fraction: compaction_threshold_fraction,
                        keep_recent_tokens: compaction_keep_recent_tokens,
                        events: &compaction_event_tx,
                        context: &stream_context,
                        tool_generation: tool_revision,
                        capacity: &mut context_capacity,
                    };
                    let operation = compaction.ensure_capacity(
                        &system,
                        &request_tool_defs,
                        compaction_reserve_tokens,
                        provider_output_ceiling,
                    );
                    tokio::pin!(operation);
                    let result = loop {
                        tokio::select! {
                            biased;
                            Some(event) = compaction_event_rx.recv() => {
                                notify_observers(&observers, &event);
                                yield event;
                            }
                            result = &mut operation => break result,
                        }
                    };
                    while let Ok(event) = compaction_event_rx.try_recv() {
                        notify_observers(&observers, &event);
                        yield event;
                    }
                    result
                };
                let capacity = match capacity {
                    Ok(capacity) => capacity,
                    Err(error) => {
                        break 'run if matches!(&error, AgentError::Cancelled) {
                            FinishReason::Aborted
                        } else {
                            FinishReason::Failed(error)
                        };
                    }
                };
                let input_tokens = capacity.input_tokens;
                let request_max_output_tokens = capacity.max_output_tokens;
                let messages = match session.context() {
                    Ok(m) => m,
                    Err(e) => break 'run FinishReason::Failed(e.into()),
                };
                let active_system = capacity.active_system;
                let responses =
                    if auto_compaction_mode == AgentCompactionMode::NativeResponses {
                        match native_responses_options(session, &model, &active_system) {
                            Ok(options) => Some(options),
                            Err(error) => break 'run FinishReason::Failed(error),
                        }
                    } else {
                        durable_responses_options(session, &model, &active_system)
                    };

                let request = Request {
                    system: if active_system.is_empty() { None } else { Some(active_system.clone()) },
                    messages,
                    tools: request_tool_defs.clone(),
                    tool_choice: if answer_only {
                        ToolChoice::None
                    } else {
                        ToolChoice::Auto
                    },
                    max_output_tokens: Some(request_max_output_tokens),
                    temperature: None,
                    stop: vec![],
                    reasoning: reasoning.clone(),
                    reasoning_mode,
                    responses,
                    output_format: OutputFormat::Text,
                    output_modalities: output_modalities.clone(),
                    compatibility: CompatibilityMode::Strict,
                    cache_retention,
                    session_id: Some(session_id.clone()),
                };
                let prepared = PreparedTurn::new(
                    session.head(),
                    active_system.clone(),
                    tool_revision,
                    request,
                    input_tokens,
                );
                let current_tool_generation = extension_host.tool_snapshot().0;
                if !prepared.is_current(session, &active_system, current_tool_generation) {
                    // Re-enter the boundary so a publication or append that
                    // crossed compaction cannot pair an old request with a new
                    // tool map or durable cursor.
                    continue 'run;
                }
                let input_tokens = prepared.input_tokens;
                let request = prepared.request;

                if let Err(error) = reserve_request_tokens(
                    session,
                    input_tokens,
                    request_max_output_tokens,
                    max_session_tokens,
                ) {
                    break 'run FinishReason::Failed(error);
                }
                if let Err(error) = reserve_request_cost(
                    session,
                    &model,
                    input_tokens,
                    request_max_output_tokens,
                    max_session_cost_microdollars,
                ) {
                    break 'run FinishReason::Failed(error);
                }

                // ── Open the provider stream (abortable) ───────────────────
                // A new provider request for this model turn starts here.
                // Anchor first-token-latency measurement for consumers that
                // track it per attempt: the first OutputDelta of this stream
                // measured from this event is the attempt's TTFT.
                let ev = AgentEvent::TurnStarted;
                notify_observers(&observers, &ev);
                yield ev;
                let request_for_retry = request;
                let mut stream_retries = 0usize;
                let opened = loop {
                    match open_provider_stream(
                        &client,
                        &model,
                        request_for_retry.clone(),
                        &abort,
                    )
                    .await
                    {
                        Err(error)
                            if provider_retries_enabled
                                && stream_retries < provider_retry_limit(&error)
                                && retryable_before_generation(&error) =>
                        {
                            let delay = retry_after(&error, stream_retries);
                            stream_retries += 1;
                            stream_context.provider_retry();
                            let ev = AgentEvent::ProviderRetry {
                                attempt: stream_retries,
                                max_attempts: provider_retry_limit(&error),
                                delay,
                                error: provider_retry_diagnostic(&model, &error),
                            };
                            notify_observers(&observers, &ev);
                            yield ev;
                            let cancelled = tokio::select! {
                                biased;
                                _ = abort.wait() => true,
                                _ = tokio::time::sleep(delay) => false,
                            };
                            if cancelled {
                                break Ok(None);
                            }
                            // The retry is a distinct physical provider request.
                            // Re-open its lifecycle after backoff so observers can
                            // measure this attempt without charging sleep time to
                            // request latency or losing its TTFT.
                            let ev = AgentEvent::TurnStarted;
                            notify_observers(&observers, &ev);
                            yield ev;
                        }
                        result => break result,
                    }
                };
                let mut response_stream = match opened {
                    Err(error) if context_retries < MAX_PROVIDER_RETRIES && looks_like_context_error(&error) => {
                        context_retries += 1;
                        let compacted = {
                            let mut compaction = CompactionContext {
                                client: &client,
                                model: &model,
                                compaction_model: &compaction_model,
                                session,
                                usage: &mut run_usage,
                                run_cost: &mut run_cost,
                                cache_retention,
                                reasoning: &reasoning,
                                reasoning_mode,
                                session_id: &session_id,
                                max_session_tokens,
                                max_session_cost_microdollars,
                                abort: &abort,
                                mode: auto_compaction_mode,
                                threshold_fraction: compaction_threshold_fraction,
                                keep_recent_tokens: compaction_keep_recent_tokens,
                                events: &compaction_event_tx,
                                context: &stream_context,
                                tool_generation: tool_revision,
                                capacity: &mut context_capacity,
                            };
                            let operation = compaction.force_one_boundary(
                                &system,
                                &request_tool_defs,
                                compaction_reserve_tokens,
                            );
                            tokio::pin!(operation);
                            let result = loop {
                                tokio::select! {
                                    biased;
                                    Some(event) = compaction_event_rx.recv() => {
                                        notify_observers(&observers, &event);
                                        yield event;
                                    }
                                    result = &mut operation => break result,
                                }
                            };
                            while let Ok(event) = compaction_event_rx.try_recv() {
                                notify_observers(&observers, &event);
                                yield event;
                            }
                            result
                        };
                        if let Err(compaction_error) = compacted {
                            break 'run if matches!(&compaction_error, AgentError::Cancelled) {
                                FinishReason::Aborted
                            } else {
                                FinishReason::Failed(compaction_error)
                            };
                        }
                        continue 'run;
                    }
                    Err(error) => {
                        break 'run FinishReason::Failed(provider_failure(error, stream_retries));
                    }
                    Ok(None) => break 'run FinishReason::Aborted,
                    Ok(Some(s)) => s,
                };

                // ── Consume the stream, staying responsive to control ──────
                // Text/tool deltas dominate this hot path; keep StreamEvent
                // inline rather than allocating a box for every event.
                #[allow(clippy::large_enum_variant)]
                enum Next {
                    Event(Option<Result<StreamEvent, AiError>>),
                    Ctl(Option<Control>),
                    Delegation(Option<DelegationTelemetrySnapshot>),
                    Abort,
                }
                let mut attempt_saw_generation = false;
                let turn = 'consume: loop {
                    let next = tokio::select! {
                        ev = response_stream.next() => Next::Event(ev),
                        c = control_rx.recv(), if control_open => Next::Ctl(c),
                        snapshot = async {
                            match &mut delegation_telemetry {
                                Some(receiver) => next_delegation_snapshot(receiver).await,
                                None => std::future::pending().await,
                            }
                        }, if delegation_telemetry.is_some() => Next::Delegation(snapshot),
                        _ = abort.wait() => Next::Abort,
                    };
                    match next {
                        Next::Abort | Next::Ctl(Some(Control::Abort)) => {
                            break Err(FinishReason::Aborted);
                        }
                        Next::Ctl(Some(Control::Steer(input))) => pending_steer.push(input),
                        Next::Ctl(Some(Control::FollowUp(input))) => followups.push_back(input),
                        Next::Ctl(Some(Control::FinishNow(input))) => {
                            pending_steer.push(input);
                            answer_only = true;
                            finish_pending = true;
                            context_capacity.invalidate();
                        }
                        Next::Ctl(Some(Control::SetSteeringMode(mode))) => steering_mode = mode,
                        Next::Ctl(Some(Control::SetFollowUpMode(mode))) => follow_up_mode = mode,
                        Next::Ctl(None) => control_open = false,
                        Next::Delegation(Some(snapshot)) => {
                            let event = AgentEvent::DelegationUpdated { snapshot };
                            notify_observers(&observers, &event);
                            yield event;
                        }
                        Next::Delegation(None) => delegation_telemetry = None,
                        Next::Event(None) => {
                            let error = AiError::StreamProtocol(
                                ygg_ai::StreamProtocolError::MissingFinish,
                            );
                            if provider_retries_enabled
                                && !attempt_saw_generation
                                && stream_retries < MAX_PROVIDER_RETRIES
                                && retryable_stream_start(&error)
                            {
                                let delay = retry_after(&error, stream_retries);
                                stream_retries += 1;
                                stream_context.provider_retry();
                                let ev = AgentEvent::ProviderRetry {
                                    attempt: stream_retries,
                                    max_attempts: MAX_PROVIDER_RETRIES,
                                    delay,
                                    error: provider_retry_diagnostic(&model, &error),
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                let cancelled = tokio::select! {
                                    _ = tokio::time::sleep(delay) => false,
                                    _ = abort.wait() => true,
                                };
                                if cancelled {
                                    break 'consume Err(FinishReason::Aborted);
                                }
                                // Count and time the physical replacement request,
                                // including stream establishment and TTFT but not backoff.
                                let ev = AgentEvent::TurnStarted;
                                notify_observers(&observers, &ev);
                                yield ev;
                                let reopened = open_provider_stream(
                                    &client,
                                    &model,
                                    request_for_retry.clone(),
                                    &abort,
                                )
                                .await;
                                match reopened {
                                    Ok(Some(stream)) => {
                                        response_stream = stream;
                                        attempt_saw_generation = false;
                                        continue 'consume;
                                    }
                                    Ok(None) => break 'consume Err(FinishReason::Aborted),
                                    Err(error) => break Err(FinishReason::Failed(error.into())),
                                }
                            }
                            break Err(FinishReason::Failed(error.into()));
                        }
                        Next::Event(Some(Err(mut error))) => {
                            if !attempt_saw_generation
                                && context_retries < MAX_PROVIDER_RETRIES
                                && looks_like_context_error(&error)
                            {
                                stream_context.provider_retry();
                                context_retries += 1;
                                let compacted = {
                                    let mut compaction = CompactionContext {
                                        client: &client,
                                        model: &model,
                                        compaction_model: &compaction_model,
                                        session,
                                        usage: &mut run_usage,
                                        run_cost: &mut run_cost,
                                        cache_retention,
                                        reasoning: &reasoning,
                                        reasoning_mode,
                                        session_id: &session_id,
                                        max_session_tokens,
                                        max_session_cost_microdollars,
                                        abort: &abort,
                                        mode: auto_compaction_mode,
                                        threshold_fraction: compaction_threshold_fraction,
                                        keep_recent_tokens: compaction_keep_recent_tokens,
                                        events: &compaction_event_tx,
                                        context: &stream_context,
                                        tool_generation: tool_revision,
                                        capacity: &mut context_capacity,
                                    };
                                    let operation = compaction.force_one_boundary(
                                        &system,
                                        &request_tool_defs,
                                        compaction_reserve_tokens,
                                    );
                                    tokio::pin!(operation);
                                    let result = loop {
                                        tokio::select! {
                                            biased;
                                            Some(event) = compaction_event_rx.recv() => {
                                                notify_observers(&observers, &event);
                                                yield event;
                                            }
                                            result = &mut operation => break result,
                                        }
                                    };
                                    while let Ok(event) = compaction_event_rx.try_recv() {
                                        notify_observers(&observers, &event);
                                        yield event;
                                    }
                                    result
                                };
                                match compacted {
                                    Ok(()) => continue 'run,
                                    Err(error) if matches!(&error, AgentError::Cancelled) => {
                                        break Err(FinishReason::Aborted);
                                    }
                                    Err(error) => {
                                        break Err(FinishReason::Failed(error));
                                    }
                                }
                            }
                            while provider_retries_enabled
                                && !attempt_saw_generation
                                && stream_retries < provider_retry_limit(&error)
                                && retryable_stream_start(&error)
                            {
                                let retry_limit = provider_retry_limit(&error);
                                let delay = retry_after(&error, stream_retries);
                                stream_retries += 1;
                                stream_context.provider_retry();
                                let ev = AgentEvent::ProviderRetry {
                                    attempt: stream_retries,
                                    max_attempts: retry_limit,
                                    delay,
                                    error: provider_retry_diagnostic(&model, &error),
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                let cancelled = tokio::select! {
                                    _ = tokio::time::sleep(delay) => false,
                                    _ = abort.wait() => true,
                                };
                                if cancelled {
                                    break 'consume Err(FinishReason::Aborted);
                                }
                                // Count and time the physical replacement request,
                                // including stream establishment and TTFT but not backoff.
                                let ev = AgentEvent::TurnStarted;
                                notify_observers(&observers, &ev);
                                yield ev;
                                let reopened = open_provider_stream(
                                    &client,
                                    &model,
                                    request_for_retry.clone(),
                                    &abort,
                                )
                                .await;
                                match reopened {
                                    Ok(Some(stream)) => {
                                        response_stream = stream;
                                        attempt_saw_generation = false;
                                        // The retried stream re-emits every
                                        // event; drop shadow state from the
                                        // failed attempt so part indexes
                                        // cannot collide.
                                        speculative_bash.begin_turn();
                                        continue 'consume;
                                    }
                                    Ok(None) => break 'consume Err(FinishReason::Aborted),
                                    Err(next_error) => error = next_error,
                                }
                            }
                            break Err(FinishReason::Failed(provider_failure(
                                error,
                                stream_retries,
                            )));
                        }
                        Next::Event(Some(Ok(event))) => {
                            stream_context.observe_stream(&event);
                            match event {
                            StreamEvent::TextDelta { delta, .. } => {
                                attempt_saw_generation = true;
                                let ev = AgentEvent::OutputDelta {
                                    channel: OutputChannel::Text,
                                    text: delta,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            StreamEvent::ReasoningDelta { delta, .. } => {
                                attempt_saw_generation = true;
                                let ev = AgentEvent::OutputDelta {
                                    channel: OutputChannel::Reasoning,
                                    text: delta,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            // `ygg-ai`'s ResponseBuilder assembles the message
                            // and validates the stream; the agent does not
                            // duplicate that parser. Raw tool-argument deltas
                            // are deliberately not exposed. The speculative
                            // shadow copy below is never authoritative: it
                            // only pre-runs shallow recon bash calls whose
                            // exact arguments are later re-verified against
                            // the finished response before any result is used.
                            StreamEvent::ToolCallStart { index, id, name } => {
                                attempt_saw_generation = true;
                                if tool_call_hooks.is_empty() {
                                    speculative_bash.note_start(index, id, name);
                                }
                            }
                            StreamEvent::ToolCallArgsDelta { index, delta } => {
                                attempt_saw_generation = true;
                                speculative_bash.note_args_delta(index, &delta);
                            }
                            StreamEvent::ToolCallEnd {
                                index,
                                argument_error,
                            } => {
                                attempt_saw_generation = true;
                                // `ygg-ai` validates completed parseable calls
                                // against the immutable request snapshot before
                                // emitting this event. A rejected call must
                                // never reach even read-only speculation.
                                if argument_error.is_some() {
                                    speculative_bash.discard(index);
                                // Speculative bash: start shallow read-only
                                // commands while generation continues, so
                                // their latency hides inside streaming time.
                                } else if !answer_only && tool_call_hooks.is_empty() {
                                    if let Some((call_id, arguments)) =
                                        speculative_bash.complete(index)
                                    {
                                        if is_speculatable_recon_bash(&arguments) {
                                            if let Some(tool) = tool_map.get("bash").cloned() {
                                                let (handle, cancellation) =
                                                    spawn_speculative_execution(
                                                        tool,
                                                        effect_broker.clone(),
                                                        effect_run_id.clone(),
                                                        tool_revision,
                                                        call_id.clone(),
                                                        "bash".to_string(),
                                                        arguments.clone(),
                                                        sandbox.clone(),
                                                        tool_scope.clone(),
                                                        resource_owner.clone(),
                                                        Arc::clone(&abort),
                                                    );
                                                speculative_bash.insert_active(
                                                    call_id,
                                                    arguments,
                                                    handle,
                                                    cancellation,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            StreamEvent::MediaCompleted { index, media } => {
                                attempt_saw_generation = true;
                                let ev = AgentEvent::OutputMedia { index, media };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            StreamEvent::Finished(response) => break Ok(response),
                            _ => {}
                            }
                        },
                    }
                };
                let response = match turn {
                    Ok(r) => r,
                    Err(reason) => break 'run reason,
                };
                // Context-recovery attempts are scoped to one logical provider
                // turn. A successful response proves the current compacted
                // prefix is accepted and restores the recovery budget for a
                // later autonomous turn in the same run.
                context_retries = 0;
                // Max-turns counts completed provider turns. Context rejection
                // and transport recovery happen within the same logical turn
                // and must not consume the autonomous work budget.
                completed_turns = completed_turns.saturating_add(1);
                drop(response_stream);

                // ── Persist the completed assistant message ────────────────
                // StopReason is semantic control data, not parser metadata. It
                // must be inspected before deciding whether a no-tool turn is
                // a successful completion.
                let stop_reason = response.stop_reason.clone();
                let turn_usage = response.usage;
                let raw_responses_output = response.responses_output.clone();
                let assistant = response.message;
                let calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ygg_ai::AssistantPart::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();

                if auto_compaction_mode == AgentCompactionMode::NativeResponses
                    && model.spec.protocol == Protocol::OpenAiResponses
                    && raw_responses_output.is_none()
                {
                    add_usage(&mut run_usage, &turn_usage);
                    let turn_cost = response.cost;
                    if let Err(error) = session.record_rejected_responses_turn_usage(
                        model.endpoint.id.clone(),
                        model.spec.id.clone(),
                        turn_usage,
                        turn_cost,
                    ) {
                        break 'run FinishReason::Failed(error.into());
                    }
                    run_cost.add(turn_cost);
                    break 'run FinishReason::Failed(AgentError::IncompleteResponse {
                        stop_reason:
                            "native Responses mode requires non-empty authoritative terminal output"
                                .to_owned(),
                    });
                }

                if let Err(error) = session.append_assistant_turn(
                    assistant.clone(),
                    model.endpoint.id.clone(),
                    model.spec.id.clone(),
                    turn_usage,
                    response.cost,
                    stop_reason.clone(),
                    raw_responses_output,
                ) {
                    break 'run FinishReason::Failed(error.into());
                }
                context_capacity.observe_assistant_response(session, &turn_usage);
                add_usage(&mut run_usage, &turn_usage);
                let turn_cost = response.cost;
                run_cost.add(turn_cost);
                let normal_end = matches!(stop_reason, StopReason::EndTurn | StopReason::StopSequence);
                let output_truncated = matches!(stop_reason, StopReason::MaxTokens);
                let needs_continuation = output_truncated
                    || matches!(stop_reason, StopReason::PauseTurn)
                    || matches!(&stop_reason, StopReason::Other(reason) if reason == "tool_output_locked");
                if normal_end && calls.is_empty() && !assistant_has_terminal_content(&assistant) {
                    break 'run FinishReason::Failed(AgentError::IncompleteResponse {
                        stop_reason: "provider returned no user-visible content".to_owned(),
                    });
                }
                let gated_candidate = completion_policy == CompletionPolicy::TerminalGate
                    && calls.is_empty()
                    && normal_end;

                // Candidate turns stay provisional until their isolated gate
                // returns R. Tool turns and natural-policy answers commit now.
                if !gated_candidate {
                    let session_cost = (session.total_cost_microdollars() > 0
                        || model.spec.pricing.is_some())
                    .then(|| session.total_cost_microdollars());
                    let ev = AgentEvent::TurnFinished {
                        message: assistant.clone(),
                        stop_reason: stop_reason.clone(),
                        turn_usage,
                        usage: run_usage,
                        session_cost_microdollars: session_cost,
                        run_cost_microdollars: run_cost.microdollars,
                    };
                    notify_observers(&observers, &ev);
                    yield ev;
                }

                // Drain control before deciding whether a provisional candidate
                // is terminal. New user input takes precedence over the gate.
                while control_open {
                    match control_rx.try_recv() {
                        Ok(Control::Steer(input)) => pending_steer.push(input),
                        Ok(Control::FollowUp(input)) => followups.push_back(input),
                        Ok(Control::FinishNow(input)) => {
                            pending_steer.push(input);
                            answer_only = true;
                            finish_pending = true;
                            context_capacity.invalidate();
                        }
                        Ok(Control::SetSteeringMode(mode)) => steering_mode = mode,
                        Ok(Control::SetFollowUpMode(mode)) => follow_up_mode = mode,
                        Ok(Control::Abort) => {
                            abort.set();
                            break;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => control_open = false,
                    }
                }

                // A response is not successful merely because it contains no
                // tool calls. Refusals, pauses, provider-specific reasons, and
                // malformed tool-use endings are terminal failures; a length
                // stop gets one corrective continuation instead.
                if !normal_end
                    && !needs_continuation
                    && !matches!(stop_reason, StopReason::ToolUse)
                {
                    break 'run FinishReason::Failed(AgentError::IncompleteResponse {
                        stop_reason: stop_reason.as_canonical().to_owned(),
                    });
                }

                if calls.is_empty() {
                    if abort.is_set() {
                        if gated_candidate {
                            let ev = AgentEvent::CandidateRejected {
                                usage: run_usage,
                                run_cost_microdollars: run_cost.microdollars,
                                session_cost_microdollars: (session.total_cost_microdollars() > 0
                                    || model.spec.pricing.is_some())
                                .then(|| session.total_cost_microdollars()),
                            };
                            notify_observers(&observers, &ev);
                            yield ev;
                        }
                        break 'run FinishReason::Aborted;
                    }
                    if needs_continuation {
                        let instruction = continuation_instruction(&stop_reason);
                        if let Err(e) = session.append(user_message(UserInput::from(instruction))) {
                            break 'run FinishReason::Failed(e.into());
                        }
                        continue;
                    }
                    if !normal_end {
                        break 'run FinishReason::Failed(AgentError::IncompleteResponse {
                            stop_reason: stop_reason.as_canonical().to_owned(),
                        });
                    }
                    // Steering and follow-ups make this a normal intermediate
                    // turn, so commit it without spending a gate request.
                    if !pending_steer.is_empty() {
                        if gated_candidate {
                            let session_cost = (session.total_cost_microdollars() > 0
                                || model.spec.pricing.is_some())
                            .then(|| session.total_cost_microdollars());
                            let ev = AgentEvent::TurnFinished {
                                message: assistant.clone(),
                                stop_reason: stop_reason.clone(),
                                turn_usage,
                                usage: run_usage,
                                session_cost_microdollars: session_cost,
                                run_cost_microdollars: run_cost.microdollars,
                            };
                            notify_observers(&observers, &ev);
                            yield ev;
                        }
                        continue;
                    }
                    if !followups.is_empty() {
                        if gated_candidate {
                            let session_cost = (session.total_cost_microdollars() > 0
                                || model.spec.pricing.is_some())
                            .then(|| session.total_cost_microdollars());
                            let ev = AgentEvent::TurnFinished {
                                message: assistant.clone(),
                                stop_reason: stop_reason.clone(),
                                turn_usage,
                                usage: run_usage,
                                session_cost_microdollars: session_cost,
                                run_cost_microdollars: run_cost.microdollars,
                            };
                            notify_observers(&observers, &ev);
                            yield ev;
                        }
                        let queued = match follow_up_mode {
                            QueueDeliveryMode::All => followups.drain(..).collect::<Vec<_>>(),
                            QueueDeliveryMode::OneAtATime => {
                                vec![followups.pop_front().expect("follow-up queue is non-empty")]
                            }
                        };
                        let visible_tools = if answer_only {
                            &[][..]
                        } else {
                            tool_defs.as_slice()
                        };
                        let observation = ContextObservation {
                            tracker: &stream_context,
                            model: &model,
                            system: &system,
                            tools: visible_tools,
                        };
                        match deliver_control_inputs(
                            queued,
                            ControlDeliveryKind::FollowUp,
                            session,
                            &control_prompt_metadata,
                            &mut terminal_gate_requests,
                            &observation,
                        ) {
                            ControlDelivery::Completed { event } => {
                                if let Some(ev) = event {
                                    notify_observers(&observers, &ev);
                                    yield ev;
                                }
                            }
                            ControlDelivery::Interrupted { event, finish } => {
                                if let Some(ev) = event {
                                    notify_observers(&observers, &ev);
                                    yield ev;
                                }
                                break 'run finish;
                            }
                        }
                        continue;
                    }
                    if completion_policy == CompletionPolicy::TerminalGate {
                        let capsule = terminal_gate_capsule(
                            &terminal_gate_prior_context,
                            &terminal_gate_requests,
                            &assistant,
                            &terminal_action_receipts,
                        );
                        let decision = TerminalGateContext {
                            client: &client,
                            model: &model,
                            session,
                            usage: &mut run_usage,
                            run_cost: &mut run_cost,
                            cache_retention,
                            session_id: &session_id,
                            max_session_tokens,
                            max_session_cost_microdollars,
                            abort: &abort,
                        }
                        .decide(capsule)
                        .await;
                        match decision {
                            Ok(TerminalGateDecision::Return) => {
                                let session_cost = (session.total_cost_microdollars() > 0
                                    || model.spec.pricing.is_some())
                                .then(|| session.total_cost_microdollars());
                                let ev = AgentEvent::TurnFinished {
                                    message: assistant.clone(),
                                    stop_reason: stop_reason.clone(),
                                    turn_usage,
                                    usage: run_usage,
                                    session_cost_microdollars: session_cost,
                                    run_cost_microdollars: run_cost.microdollars,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                break 'run FinishReason::Completed;
                            }
                            Ok(TerminalGateDecision::Continue) => {
                                let ev = AgentEvent::CandidateRejected {
                                    usage: run_usage,
                                    run_cost_microdollars: run_cost.microdollars,
                                    session_cost_microdollars: (session.total_cost_microdollars() > 0
                                        || model.spec.pricing.is_some())
                                    .then(|| session.total_cost_microdollars()),
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                if let Err(error) = session.append(user_message(UserInput::from(
                                    TERMINAL_GATE_CORRECTION,
                                ))) {
                                    break 'run FinishReason::Failed(error.into());
                                }
                                continue;
                            }
                            Err(AgentError::Cancelled) => {
                                let ev = AgentEvent::CandidateRejected {
                                    usage: run_usage,
                                    run_cost_microdollars: run_cost.microdollars,
                                    session_cost_microdollars: (session.total_cost_microdollars() > 0
                                        || model.spec.pricing.is_some())
                                    .then(|| session.total_cost_microdollars()),
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                break 'run FinishReason::Aborted;
                            }
                            Err(error) => {
                                let ev = AgentEvent::CandidateRejected {
                                    usage: run_usage,
                                    run_cost_microdollars: run_cost.microdollars,
                                    session_cost_microdollars: (session.total_cost_microdollars() > 0
                                        || model.spec.pricing.is_some())
                                    .then(|| session.total_cost_microdollars()),
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                break 'run FinishReason::Failed(error);
                            }
                        }
                    }
                    break 'run FinishReason::Completed;
                }

                // A model can emit several independent reads in one turn.
                // Start every explicitly parallel-safe call before awaiting
                // any of them, but retain model order for persistence and
                // ToolFinished events. The static tool promise is intersected
                // with the host-owned classification for the exact arguments:
                // network, host, process, mutation, and unknown effects always
                // remain on the sequential path.
                let parallel_active_skills = session
                    .head()
                    .and_then(|head| session.resolve_active_skills(&head).ok())
                    .map(|state| state.active_skills)
                    .unwrap_or_default();
                let classification_context = ToolContext {
                    workspace: &sandbox.workspace,
                    sandbox: &sandbox,
                    execution_scope: &tool_scope,
                    resource_owner: &resource_owner,
                    active_skills: &parallel_active_skills,
                    registered_tools: &registered_tools,
                    progress: ToolProgressSink::null(),
                    cancellation: CancellationToken::default(),
                };
                let parallel_batch = !answer_only
                    && !output_truncated
                    && calls.len() > 1
                    && calls.len() <= MAX_TOOL_CALLS_PER_TURN
                    && calls.iter().all(|call| {
                        call.argument_error.is_none()
                            && call.arguments_value().is_ok_and(|arguments| {
                            tool_map.get(&call.name).is_some_and(|tool| {
                                tool.concurrency() == ToolConcurrency::Parallel
                                    && tool
                                        .effect(&arguments, &classification_context)
                                        .is_ok_and(effect_is_repeatable_observation)
                            })
                        })
                    });
                let mut parallel_results = if parallel_batch {
                    let active_skills = parallel_active_skills;
                    for call in &calls {
                        let parsed = call
                            .arguments_value()
                            .expect("parallel batch validates arguments before execution");
                        stream_context.tool_started();
                        let ev = AgentEvent::ToolStarted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: parsed,
                        };
                        notify_observers(&observers, &ev);
                        yield ev;
                    }

                    let operations = calls.iter().map(|call| {
                        execute_parallel_tool_call(
                            Arc::clone(
                                tool_map
                                    .get(&call.name)
                                    .expect("parallel batch validates registered tools"),
                            ),
                            &tool_call_hooks,
                            &effect_broker,
                            &effect_run_id,
                            tool_revision,
                            &call.id,
                            &call.name,
                            call.arguments_value()
                                .expect("parallel batch validates arguments before execution"),
                            &sandbox,
                            &tool_scope,
                            &resource_owner,
                            &active_skills,
                            &registered_tools,
                            abort.cancellation.clone(),
                        )
                    });
                    let executions = futures_util::future::join_all(operations);
                    tokio::pin!(executions);
                    let mut abort_observed = abort.is_set();
                    let completed = loop {
                        tokio::select! {
                            biased;
                            results = &mut executions => break results,
                            _ = abort.wait(), if !abort_observed => {
                                abort_observed = true;
                            }
                            control = control_rx.recv(), if control_open => match control {
                                Some(Control::Steer(input)) => pending_steer.push(input),
                                Some(Control::FollowUp(input)) => followups.push_back(input),
                                Some(Control::FinishNow(input)) => {
                                    pending_steer.push(input);
                                    answer_only = true;
                                    finish_pending = true;
                                    context_capacity.invalidate();
                                }
                                Some(Control::SetSteeringMode(mode)) => steering_mode = mode,
                                Some(Control::SetFollowUpMode(mode)) => follow_up_mode = mode,
                                Some(Control::Abort) => {
                                    abort.set();
                                    abort_observed = true;
                                }
                                None => control_open = false,
                            },
                        }
                    };
                    Some(completed.into_iter())
                } else {
                    None
                };

                // Calls in one assistant response form a single batch. Do not
                // treat parallel or otherwise batched identical calls as a
                // no-progress loop; only compare against earlier responses.
                let batch_fingerprints: Vec<(String, String)> = calls
                    .iter()
                    .filter(|call| call.argument_error.is_none())
                    .filter_map(|call| {
                        call.arguments_value().ok().map(|args| {
                            (
                                call.name.clone(),
                                tool_call_arguments_fingerprint(&call.name, &args),
                            )
                        })
                    })
                    .collect();

                // ── Commit tool results in emitted order ───────────────────
                for (call_index, call) in calls.into_iter().enumerate() {
                    let argument_error = call.argument_error;
                    let parsed = call.arguments_value();
                    let call_fingerprint = if argument_error.is_none() {
                        parsed.as_ref().ok().map(|args| {
                            (
                                call.name.clone(),
                                tool_call_arguments_fingerprint(&call.name, args),
                            )
                        })
                    } else {
                        None
                    };
                    let repeated_recently = call_fingerprint.as_ref().map_or(0, |fingerprint| {
                        recent_tool_calls
                            .iter()
                            .filter(|previous| *previous == fingerprint)
                            .count()
                    });
                    let should_annotate_repetition =
                        repeated_recently >= REPEATED_TOOL_CALL_THRESHOLD;
                    let mut preexecuted = parallel_results.as_mut().and_then(Iterator::next);
                    if preexecuted.is_none() {
                        stream_context.tool_started();
                        let ev = AgentEvent::ToolStarted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: parsed
                                .as_ref()
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        };
                        notify_observers(&observers, &ev);
                        yield ev;

                        // Reconcile speculative bash: consume the pre-run
                        // result only on an exact argument match with the
                        // authoritative call; otherwise cancel it and fall
                        // through to normal serial execution.
                        preexecuted = if output_truncated || argument_error.is_some() {
                            let _ = speculative_bash.take_matched(&call.id, None).await;
                            None
                        } else {
                            speculative_bash
                                .take_matched(&call.id, parsed.as_ref().ok())
                                .await
                        };
                    }
                    let CompletedToolExecution {
                        result,
                        duration,
                        mut progress_rx,
                        progress_sink,
                        cancellation_won,
                        started_unix_ms,
                        finished_unix_ms,
                    } = if let Some(argument_error) = argument_error {
                        // Do not classify effects, run hooks, or invoke the
                        // tool for a call already rejected by the request's
                        // schema snapshot. The paired static error is durable
                        // and safe to show back to the model.
                        rejected_argument_tool_execution(argument_error)
                    } else if let Some(execution) = preexecuted {
                        execution
                    } else {
                        // Create a fresh progress channel for every sequential
                        // call. Non-streaming tools simply never push into it.
                        let (progress_tx, mut progress_rx) =
                            mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
                        let progress_sink = ToolProgressSink::live(progress_tx);
                        let mut cancellation_won = false;
                        let start = std::time::Instant::now();
                        let started_at = Arc::new(AtomicU64::new(u64::MAX));
                        let started_at_marker = Arc::clone(&started_at);
                        let result: Result<ToolOutput, ToolError> = if answer_only {
                            Err(ToolError::new(format!(
                                "tool call `{}` was not executed: the user requested an immediate final answer without tools",
                                call.name
                            )))
                        } else if output_truncated {
                            Err(ToolError::new(format!(
                                "tool call `{}` was not executed: the provider reached its output token limit, so the arguments may be truncated; re-issue the call with complete arguments",
                                call.name
                            )))
                        } else if call_index >= MAX_TOOL_CALLS_PER_TURN {
                            Err(ToolError::new(
                                "tool call skipped: per-turn tool-call limit reached",
                            ))
                    } else if abort.is_set() {
                        cancellation_won = true;
                        Err(cancelled_tool_error())
                    } else {
                        match (tool_map.get(&call.name), parsed) {
                            (None, _) => {
                                Err(ToolError::new(format!("unknown tool: {}", call.name)))
                            }
                            (Some(_), Err(e)) => {
                                Err(ToolError::new(format!("invalid tool arguments: {e}")))
                            }
                            (Some(tool), Ok(args)) => {
                                let active_skills = session
                                    .head()
                                    .and_then(|head| session.resolve_active_skills(&head).ok())
                                    .map(|state| state.active_skills)
                                    .unwrap_or_default();
                                let tool_ctx = ToolContext {
                                    workspace: &sandbox.workspace,
                                    sandbox: &sandbox,
                                    execution_scope: &tool_scope,
                                    resource_owner: &resource_owner,
                                    active_skills: &active_skills,
                                    registered_tools: &registered_tools,
                                    progress: progress_sink.clone(),
                                    cancellation: abort.cancellation.clone(),
                                };
                                let hook_arguments = args.clone();
                                let effect_committed = Arc::new(AtomicBool::new(false));
                                let committed_marker = Arc::clone(&effect_committed);
                                let operation = async {
                                    let (intent, effect_reservation) = reserve_tool_effect(
                                        &effect_broker,
                                        tool.as_ref(),
                                        &call.name,
                                        &args,
                                        &tool_ctx,
                                        &resource_owner,
                                        &effect_run_id,
                                        tool_revision,
                                        &call.id,
                                        true,
                                    )
                                    .await?;
                                    for hook in &tool_call_hooks {
                                        hook.before_tool_call(
                                            &call.name,
                                            &hook_arguments,
                                            &tool_ctx,
                                        )
                                        .await?;
                                    }
                                    if tool_ctx.cancellation.is_cancelled() {
                                        return Err(cancelled_tool_error());
                                    }
                                    effect_reservation
                                        .commit(&intent)
                                        .map_err(|error| ToolError::new(error.to_string()))?;
                                    committed_marker.store(true, Ordering::Release);
                                    started_at_marker
                                        .store(crate::session::now_unix_millis(), Ordering::Release);
                                    tool.execute(args, &tool_ctx).await
                                };
                                tokio::pin!(operation);
                                // Cancellation drops the pinned future, which
                                // kills any child process tree it spawned.
                                let outcome = loop {
                                    tokio::select! {
                                        biased;
                                        _ = abort.wait() => break None,
                                        r = &mut operation => break Some(r),
                                        c = control_rx.recv(), if control_open => match c {
                                            Some(Control::Steer(input)) => pending_steer.push(input),
                                            Some(Control::FollowUp(input)) => followups.push_back(input),
                                            Some(Control::FinishNow(input)) => {
                                                pending_steer.push(input);
                                                answer_only = true;
                                                finish_pending = true;
                                                context_capacity.invalidate();
                                            }
                                            Some(Control::SetSteeringMode(mode)) => steering_mode = mode,
                                            Some(Control::SetFollowUpMode(mode)) => follow_up_mode = mode,
                                            Some(Control::Abort) => {
                                                abort.set();
                                                break None;
                                            }
                                            None => control_open = false,
                                        },
                                        progress = progress_rx.recv() => {
                                            if let Some(p) = progress {
                                                // `operation` can enqueue progress and synchronously
                                                // trigger cancellation during the same select poll,
                                                // after the biased abort branch was already checked.
                                                // Recheck before accepting semantic state.
                                                match settle_tool_progress(p, abort.is_set(), session) {
                                                    ProgressSettlement::Cancelled => break None,
                                                    ProgressSettlement::Settled => {}
                                                    ProgressSettlement::Emit(p) => {
                                                        let ev = AgentEvent::ToolProgress {
                                                            id: call.id.clone(),
                                                            progress: p,
                                                        };
                                                        notify_observers(&observers, &ev);
                                                        yield ev;
                                                    }
                                                }
                                            }
                                        },
                                        snapshot = async {
                                            match &mut delegation_telemetry {
                                                Some(receiver) => next_delegation_snapshot(receiver).await,
                                                None => std::future::pending().await,
                                            }
                                        }, if delegation_telemetry.is_some() => {
                                            // Keep delegated-worker telemetry
                                            // flowing while a long root tool is
                                            // executing, not only while the root
                                            // streams from the provider.
                                            match snapshot {
                                                Some(snapshot) => {
                                                    let event =
                                                        AgentEvent::DelegationUpdated { snapshot };
                                                    notify_observers(&observers, &event);
                                                    yield event;
                                                }
                                                None => delegation_telemetry = None,
                                            }
                                        },
                                    }
                                };
                                let result = match outcome {
                                    Some(_) if abort.is_set() => {
                                        cancellation_won = true;
                                        Err(cancelled_tool_error())
                                    }
                                    Some(result) => result,
                                    None => {
                                        cancellation_won = true;
                                        Err(cancelled_tool_error())
                                    }
                                };
                                if effect_committed.load(Ordering::Acquire) {
                                    let (output, is_error) = match &result {
                                        Ok(output) => (output.text.as_str(), output.is_error()),
                                        Err(error) => (error.message.as_str(), true),
                                    };
                                    for hook in &tool_call_hooks {
                                        hook.after_tool_call(
                                            &call.name,
                                            &hook_arguments,
                                            output,
                                            is_error,
                                            &tool_ctx,
                                        )
                                        .await;
                                    }
                                }
                                result
                            }
                        }
                        };
                        let started_at_value = started_at.load(Ordering::Acquire);
                        let started_unix_ms =
                            (started_at_value != u64::MAX).then_some(started_at_value);
                        CompletedToolExecution {
                            result,
                            duration: start.elapsed(),
                            started_unix_ms,
                            finished_unix_ms: Some(crate::session::now_unix_millis()),
                            progress_rx,
                            progress_sink,
                            cancellation_won,
                        }
                    };
                    let result = if should_annotate_repetition {
                        annotate_repeated_tool_result(result, repeated_recently)
                    } else {
                        result
                    };

                    // ── COMMIT BOUNDARY ──────────────────────────────────
                    // Tool::execute resolved (or an immediate error was
                    // produced). Persist the result immediately before
                    // draining progress or checking abort. An abort
                    // received after this point cannot erase an already-
                    // committed result.
                    // Every tool owns the same configured output allowance.
                    // A large early result must never starve later successful
                    // calls in the same model turn. Structured media is lowered
                    // only when the active model/protocol can replay it safely.
                    // Announce tools that appeared as a consequence of this
                    // execution (extension/MCP registrations). Later requests
                    // exclude announced schemas under deferred tool loading.
                    let (_, snapshot_tools) = extension_host.tool_snapshot();
                    let newly_added: Vec<String> = snapshot_tools
                        .iter()
                        .map(|tool| tool.definition().name)
                        .filter(|name| !announced_tools.contains(name))
                        .collect();
                    if !newly_added.is_empty() {
                        announced_tools.extend(newly_added.iter().cloned());
                    }
                    let (message, accepted_media, text, is_error, details) = lower_tool_result(
                        call.id.clone(),
                        &result,
                        &model,
                        sandbox.max_output_bytes,
                        newly_added,
                    );
                    terminal_action_receipts.push(TerminalActionReceipt {
                        tool: call.name.clone(),
                        arguments: call.arguments_json.clone(),
                        status: if is_error { "error" } else { "ok" },
                        result: text.clone(),
                    });
                    if let Err(e) = session.append_with_metadata(
                        EntryValue::Message(Message::User(message)),
                        details.map(|tool_output| EntryMetadata {
                            tool_output: Some(tool_output),
                            tool_started_unix_ms: started_unix_ms,
                            tool_finished_unix_ms: finished_unix_ms,
                            ..EntryMetadata::default()
                        }),
                    ) {
                        break 'run FinishReason::Failed(e.into());
                    }
                    // Internal durable-delivery tools may provisionally lease
                    // work while executing. Acknowledge it only once the
                    // complete, untruncated result is in the session.
                    resolve_tool_delivery_after_persistence(&result, sandbox.max_output_bytes);

                    // ── Drain accepted progress before ToolFinished ───────
                    while let Ok(p) = progress_rx.try_recv() {
                        match settle_tool_progress(p, cancellation_won, session) {
                            ProgressSettlement::Cancelled => continue,
                            ProgressSettlement::Settled => {}
                            ProgressSettlement::Emit(p) => {
                                let ev = AgentEvent::ToolProgress {
                                    id: call.id.clone(),
                                    progress: p,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                        }
                    }
                    // Report dropped progress if any.
                    let (dropped_bytes, dropped_events) = progress_sink.take_dropped();
                    if dropped_bytes > 0 || dropped_events > 0 {
                        let ev = AgentEvent::ToolProgress {
                            id: call.id.clone(),
                            progress: ToolProgress::Dropped {
                                bytes: dropped_bytes,
                                events: dropped_events,
                            },
                        };
                        notify_observers(&observers, &ev);
                        yield ev;
                    }

                    stream_context.tool_finished();
                    let result = match result {
                        Ok(output) => Ok(output
                            .without_media_payloads_for(accepted_media)
                            .with_is_error(is_error)),
                        Err(error) => Err(error),
                    };
                    let ev = AgentEvent::ToolFinished {
                        id: call.id.clone(),
                        result,
                        duration,
                    };
                    notify_observers(&observers, &ev);
                    yield ev;

                }
                for fingerprint in batch_fingerprints {
                    recent_tool_calls.push_back(fingerprint);
                    while recent_tool_calls.len() > MAX_RECENT_TOOL_CALLS {
                        recent_tool_calls.pop_front();
                    }
                }

                // Every emitted call now has a durable result, including calls
                // that were never started because the user aborted. Do not
                // enter another model turn after controlled cancellation.
                if abort.is_set() {
                    break 'run FinishReason::Aborted;
                }

                if needs_continuation {
                    let instruction = continuation_instruction(&stop_reason);
                    if let Err(e) = session.append(user_message(UserInput::from(instruction))) {
                        break 'run FinishReason::Failed(e.into());
                    }
                }
                // Context reconstruction coalesces the consecutive tool-result
                // entries into the provider-required single user message.
            };

            // A fully driven prompt always leaves an explicit durable restore
            // point, including controlled abort/max-turn/failure outcomes. A
            // dropped stream is not complete and never reaches this boundary.
            // Failed provider turns also need an assistant boundary. Without
            // one, the next prompt is appended after the unresolved user task
            // and models commonly continue the stale request instead.
            if matches!(reason, FinishReason::Failed(_)) {
                if let Err(error) = close_failed_turn(session, &model) {
                    reason = FinishReason::Failed(error);
                }
            }
            if let Some(delegation) = &stream_delegation {
                // Stop and briefly settle extension-owned children before the
                // root checkpoint so their durable provider records can be
                // mirrored into the root accounting ledger exactly once.
                delegation.request_shutdown();
                delegation
                    .settle_descendants(Duration::from_secs(2))
                    .await;
                for delegated in delegation.delegated_usage_records() {
                    if let Err(error) = session.record_delegated_agent_usage(DelegatedUsage {
                        agent_id: delegated.agent_id,
                        turn_count: delegated.turn_count,
                        tool_call_count: delegated.tool_call_count,
                        endpoint: model.endpoint.id.clone(),
                        model: model.spec.id.clone(),
                        usage: delegated.usage,
                        cost: delegated.cost,
                    }) {
                        reason = FinishReason::Failed(error.into());
                        break;
                    }
                }
                if let Some(receiver) = delegation_telemetry.as_mut() {
                    if receiver.has_changed().unwrap_or(false) {
                        let snapshot = { receiver.borrow_and_update().clone() };
                        if let Some(snapshot) = snapshot {
                            let event = AgentEvent::DelegationUpdated { snapshot };
                            notify_observers(&observers, &event);
                            yield event;
                        }
                    }
                }
                delegation.detach_telemetry();
            }
            // Capacity checks use the incremental total-only cache. Refresh the
            // detailed snapshot once at the settled boundary so observers retain
            // an accurate final breakdown without paying for it on every turn.
            let _ = observe_context_tracker(&stream_context, session, &model, &system, &tool_defs);
            let checkpoint_usage = (completed_turns > 0).then_some(run_usage);
            let checkpoint_cost = model
                .spec
                .pricing
                .as_ref()
                .map(|_| run_cost.microdollars);
            if let Err(error) = session.checkpoint_with_telemetry(
                first_entry.clone(),
                checkpoint_usage,
                checkpoint_cost,
            ) {
                reason = FinishReason::Failed(error.into());
            }
            let head = session.head().unwrap_or(first_entry);
            stream_context.run_finished(&reason);
            stream_lifecycle.finished.store(true, Ordering::Release);
            let ev = AgentEvent::RunFinished { head, reason };
            notify_observers(&observers, &ev);
            yield ev;
        };

        Ok(Run {
            stream: Box::pin(stream),
            control,
            lifecycle,
            context,
            delegation: run_delegation,
        })
    }

    /// Declares the Agent's static tool overlay complete and releases queued
    /// dynamic extension catalog updates. Products should call this after
    /// installing collaboration or other post-construction host tools, before
    /// exposing the Agent for its first prompt.
    pub fn finalize_tool_surface(&self) {
        self.extensions.finalize_tool_surface();
    }

    /// Runs to completion, returning the aggregate output.
    ///
    /// A run that ends with [`FinishReason::Failed`] is returned as `Err`;
    /// aborted and max-turns runs return `Ok` with their reason.
    pub async fn complete(&mut self, input: impl Into<UserInput>) -> Result<RunOutput, AgentError> {
        let mut run = self.prompt(input).await?;
        let mut text = String::new();
        let mut media = Vec::new();
        // Output is provisional until its provider turn reaches `Finished`.
        // A retry invalidates only the current attempt, not output committed by
        // earlier tool turns in the same autonomous run.
        let mut committed_text_len = 0usize;
        let mut committed_media_len = 0usize;
        let mut usage = Usage::default();
        let mut run_cost: u64 = 0;
        while let Some(event) = run.next().await {
            match event {
                AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: delta,
                } => text.push_str(&delta),
                AgentEvent::OutputMedia {
                    media: output_media,
                    ..
                } => media.push(output_media),
                AgentEvent::ProviderRetry { .. } => {
                    text.truncate(committed_text_len);
                    media.truncate(committed_media_len);
                }
                AgentEvent::CandidateRejected {
                    usage: total,
                    run_cost_microdollars: cost,
                    ..
                } => {
                    text.truncate(committed_text_len);
                    media.truncate(committed_media_len);
                    usage = total;
                    run_cost = cost;
                }
                AgentEvent::SteeringDelivered { .. }
                | AgentEvent::FollowUpDelivered { .. }
                | AgentEvent::CompactionStarted { .. }
                | AgentEvent::CompactionFinished { .. } => {}
                AgentEvent::TurnFinished {
                    usage: total,
                    run_cost_microdollars: cost,
                    ..
                } => {
                    committed_text_len = text.len();
                    committed_media_len = media.len();
                    usage = total;
                    run_cost = cost;
                }
                AgentEvent::RunFinished { head, reason } => {
                    return match reason {
                        FinishReason::Failed(e) => Err(e),
                        reason => Ok(RunOutput {
                            text,
                            media,
                            usage,
                            cost_microdollars: run_cost,
                            head,
                            reason,
                        }),
                    };
                }
                AgentEvent::ToolProgress { .. } => {}
                _ => {}
            }
        }
        // Unreachable for a started run: the stream always ends with RunFinished.
        Err(AgentError::RunEnded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_turn_rejects_stale_durable_head_system_and_tool_generation() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("prepared-turn.jsonl")).unwrap();
        let request = Request {
            system: Some("system".into()),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(16),
            temperature: None,
            stop: Vec::new(),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ReasoningMode::Standard,
            responses: None,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: CacheRetention::default(),
            session_id: Some("session".into()),
        };
        let prepared = PreparedTurn::new(session.head(), "system".into(), 7, request, 3);

        assert!(prepared.is_current(&session, "system", 7));
        assert!(!prepared.is_current(&session, "changed", 7));
        assert!(!prepared.is_current(&session, "system", 8));

        session
            .append(user_message(UserInput::from("new durable input")))
            .unwrap();
        assert!(!prepared.is_current(&session, "system", 7));
    }

    #[test]
    fn request_output_uses_provider_ceiling_then_clamps_to_remaining_context() {
        assert_eq!(
            resolve_request_max_output_tokens(200_000, 20_000, 65_536),
            65_536
        );
        assert_eq!(
            resolve_request_max_output_tokens(200_000, 170_000, 65_536),
            30_000
        );
    }

    #[tokio::test]
    async fn speculative_bash_reconciles_exact_match_and_discards_drift() {
        let mut speculative = SpeculativeBash::default();
        let id = ygg_ai::ToolCallId("call_spec".into());

        speculative.note_start(0, id.clone(), "bash".into());
        speculative.note_args_delta(0, r#"{"command":"#);
        speculative.note_args_delta(0, r#""ls"}"#);
        let (completed_id, arguments) = speculative.complete(0).unwrap();
        assert_eq!(completed_id, id);

        fn completed_execution(text: &str) -> CompletedToolExecution {
            let (progress_tx, progress_rx) =
                mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
            CompletedToolExecution {
                result: Ok(ToolOutput::new(text)),
                duration: std::time::Duration::ZERO,
                started_unix_ms: None,
                finished_unix_ms: None,
                progress_rx,
                progress_sink: ToolProgressSink::live(progress_tx),
                cancellation_won: false,
            }
        }
        let handle = tokio::spawn(async { completed_execution("listed") });
        speculative.insert_active(
            id.clone(),
            arguments.clone(),
            handle,
            CancellationToken::default(),
        );

        // Exact argument match consumes the speculative execution.
        let matched = speculative.take_matched(&id, Some(&arguments)).await;
        assert_eq!(matched.unwrap().result.unwrap().text, "listed");

        // Drifted authoritative arguments cancel the speculation instead of
        // surfacing its result, so the caller executes serially.
        speculative.note_start(1, id.clone(), "bash".into());
        speculative.note_args_delta(1, r#"{"command":"ls -la"}"#);
        let (_, drifted_arguments) = speculative.complete(1).unwrap();
        let handle = tokio::spawn(async { completed_execution("wrong branch") });
        speculative.insert_active(
            id.clone(),
            drifted_arguments.clone(),
            handle,
            CancellationToken::default(),
        );
        let authoritative = serde_json::json!({ "command": "ls" });
        assert!(speculative
            .take_matched(&id, Some(&authoritative))
            .await
            .is_none());
    }
    #[test]
    fn provisional_delivery_rolls_back_when_generic_tool_output_limiting_truncates_it() {
        use std::sync::atomic::{AtomicI8, Ordering};

        let resolution = Arc::new(AtomicI8::new(0));
        let committed = Arc::clone(&resolution);
        let rolled_back = Arc::clone(&resolution);
        let result: Result<ToolOutput, ToolError> = Ok(ToolOutput::new("x".repeat(128))
            .with_delivery_commit(
                move || committed.store(1, Ordering::SeqCst),
                move || rolled_back.store(-1, Ordering::SeqCst),
            ));
        let model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-4o-mini".into()))
            .unwrap();
        let (_, _, persisted_text, _, _) = lower_tool_result(
            ygg_ai::ToolCallId("delivery".into()),
            &result,
            &model,
            32,
            Vec::new(),
        );
        assert_ne!(persisted_text, result.as_ref().unwrap().text);
        assert!(persisted_text.len() <= 32);

        resolve_tool_delivery_after_persistence(&result, 32);
        assert_eq!(resolution.load(Ordering::SeqCst), -1);
    }

    #[test]
    fn repeated_tool_annotation_is_bounded_and_model_visible() {
        let result = annotate_repeated_tool_result(Ok(ToolOutput::new("result")), 2).unwrap();
        assert!(result.text.contains("exact call repeated 3x"));
        assert_eq!(
            result
                .content_parts()
                .iter()
                .filter_map(|part| match part {
                    ToolOutputContentPart::Text(text) => Some(text.as_str()),
                    ToolOutputContentPart::Media(_) => None,
                })
                .collect::<String>(),
            result.text
        );
        assert!(
            !annotate_repeated_tool_result(Ok(ToolOutput::new("result")), 1)
                .unwrap()
                .text
                .contains("diagnostic")
        );
    }

    #[test]
    fn repeated_tool_annotation_preserves_machine_readable_output() {
        let original = r#"{"timed_out":false,"messages":[]}"#;
        let result = annotate_repeated_tool_result(Ok(ToolOutput::new(original)), 2).unwrap();
        assert_eq!(result.text, original);
    }

    #[test]
    fn response_header_failures_are_not_automatically_replayed() {
        for timeout in [false, true] {
            let error = AiError::Transport(ygg_ai::TransportError {
                phase: ygg_ai::TransportPhase::ResponseHeaders,
                timeout,
                message: "response headers unavailable".into(),
            });
            assert!(!retryable_before_generation(&error));
            assert!(!retryable_stream_start(&error));
            assert_eq!(provider_retry_limit(&error), 0);
        }
    }

    #[test]
    fn body_timeout_is_not_automatically_retried() {
        let error = AiError::Transport(ygg_ai::TransportError {
            phase: ygg_ai::TransportPhase::Body,
            timeout: true,
            message: "stream idle deadline reached".into(),
        });
        assert!(!retryable_stream_start(&error));
        assert_eq!(provider_retry_limit(&error), 0);
    }

    #[test]
    fn context_deadline_and_throttling_are_not_misclassified_as_overflow() {
        let deadline = AiError::Transport(ygg_ai::TransportError {
            phase: ygg_ai::TransportPhase::Body,
            timeout: true,
            message: "context deadline exceeded".into(),
        });
        assert!(!looks_like_context_error(&deadline));

        let throttled = AiError::Provider(ygg_ai::ProviderError {
            code: Some("rate_limit_exceeded".into()),
            kind: Some("throttled".into()),
            message: "context window exceeded in shared capacity".into(),
            request_id: None,
        });
        assert!(!looks_like_context_error(&throttled));
    }

    #[test]
    fn provider_validation_errors_do_not_retry_but_transient_failures_do() {
        let validation = AiError::Provider(ygg_ai::ProviderError {
            code: Some("400".into()),
            kind: Some("Bad Request".into()),
            message: "reasoning_effort is invalid".into(),
            request_id: None,
        });
        assert!(!retryable_stream_start(&validation));

        for transient in [
            ygg_ai::ProviderError {
                code: Some("503".into()),
                kind: Some("server_error".into()),
                message: "temporarily unavailable".into(),
                request_id: None,
            },
            ygg_ai::ProviderError {
                code: Some("rate_limit_exceeded".into()),
                kind: Some("overloaded".into()),
                message: "try again".into(),
                request_id: None,
            },
        ] {
            assert!(retryable_stream_start(&AiError::Provider(transient)));
        }
    }

    #[test]
    fn provider_retry_diagnostics_include_bounded_operational_details() {
        let model = tool_media_model(Protocol::OpenAiChat, ygg_ai::ModalitySet::none());
        let errors = [
            AiError::Http(ygg_ai::HttpError {
                status: http::StatusCode::TOO_MANY_REQUESTS,
                request_id: Some("req-429".into()),
                retry_after: Some(Duration::from_secs(3)),
                provider_code: Some("rate_limit_exceeded".into()),
                body_snippet: Some(r#"{"error":{"message":"temporarily rate limited"}}"#.into()),
                retryable: true,
            }),
            AiError::Provider(ygg_ai::ProviderError {
                code: Some("upstream_error".into()),
                kind: Some("server_error".into()),
                message: "upstream temporarily unavailable".into(),
                request_id: Some("req-stream".into()),
            }),
            AiError::Transport(ygg_ai::TransportError {
                phase: ygg_ai::TransportPhase::Connect,
                timeout: false,
                message: "connection reset by peer".into(),
            }),
        ];

        let retry = provider_retry_diagnostic(&model, &errors[0]);
        assert!(retry.contains("status=429 (rate limited)"), "{retry}");
        assert!(retry.contains("code=rate_limit_exceeded"), "{retry}");
        assert!(retry.contains("retry_after=3s"), "{retry}");
        assert!(retry.contains("request_id=req-429"), "{retry}");

        for error in &errors[1..] {
            let diagnostic = provider_retry_diagnostic(&model, error);
            assert!(diagnostic.contains("provider="), "{diagnostic}");
            assert!(diagnostic.contains("model="), "{diagnostic}");
            assert!(diagnostic.contains("phase="), "{diagnostic}");
        }

        let credential_error = AiError::Http(ygg_ai::HttpError {
            status: http::StatusCode::UNAUTHORIZED,
            request_id: Some("req-auth".into()),
            retry_after: None,
            provider_code: Some("invalid_api_key".into()),
            body_snippet: Some(r#"{"error":{"message":"invalid api key: sk-secret"}}"#.into()),
            retryable: false,
        });
        let diagnostic = provider_retry_diagnostic(&model, &credential_error);
        assert!(diagnostic.contains("status=401 (authentication failed)"));
        assert!(!diagnostic.contains("sk-secret"));
    }

    #[test]
    fn provider_context_limit_variants_are_classified_as_overflow() {
        for message in [
            "model_context_window_exceeded",
            "prompt is too long",
            "request_too_large",
            "context window exceeds limit",
        ] {
            let error = AiError::Provider(ygg_ai::ProviderError {
                code: None,
                kind: None,
                message: message.into(),
                request_id: None,
            });
            assert!(looks_like_context_error(&error), "{message}");
        }

        let request_too_large = AiError::Http(ygg_ai::HttpError {
            status: "413".parse().unwrap(),
            request_id: None,
            retry_after: None,
            provider_code: Some("request_too_large".into()),
            body_snippet: Some("request exceeds the context window".into()),
            retryable: false,
        });
        assert!(looks_like_context_error(&request_too_large));

        let media_too_large = AiError::Http(ygg_ai::HttpError {
            status: "413".parse().unwrap(),
            request_id: None,
            retry_after: None,
            provider_code: Some("image_too_large".into()),
            body_snippet: Some("uploaded image payload exceeds 20 MB".into()),
            retryable: false,
        });
        assert!(!looks_like_context_error(&media_too_large));
    }

    #[test]
    fn non_timeout_network_failure_gets_five_retries_and_friendly_failure() {
        let error = AiError::Transport(ygg_ai::TransportError {
            phase: ygg_ai::TransportPhase::Connect,
            timeout: false,
            message: "connection refused".into(),
        });
        assert!(retryable_before_generation(&error));
        assert!(retryable_stream_start(&error));
        assert_eq!(provider_retry_limit(&error), 5);

        let failure = provider_failure(error, 5).to_string();
        assert!(failure.contains("Are you connected to the internet?"));
        assert!(failure.contains("connection"));
        assert!(!failure.contains("connection refused"));
    }

    #[test]
    fn public_provider_failures_include_safe_operational_details() {
        let errors = [
            (
                AgentError::Ai(AiError::Http(ygg_ai::HttpError {
                    status: http::StatusCode::BAD_REQUEST,
                    request_id: Some("req-400".into()),
                    retry_after: None,
                    provider_code: Some("invalid_request".into()),
                    body_snippet: Some(
                        r#"{"error":{"message":"model does not support this request"}}"#.into(),
                    ),
                    retryable: false,
                })),
                "status=400 (bad request) code=invalid_request detail=model does not support this request request_id=req-400",
            ),
            (
                AgentError::Ai(AiError::Provider(ygg_ai::ProviderError {
                    code: Some("upstream_error".into()),
                    kind: Some("server_error".into()),
                    message: "upstream temporarily unavailable".into(),
                    request_id: Some("req-stream".into()),
                })),
                "phase=response body (provider error) code=upstream_error kind=server_error detail=upstream temporarily unavailable request_id=req-stream",
            ),
            (
                AgentError::Ai(AiError::Transport(ygg_ai::TransportError {
                    phase: ygg_ai::TransportPhase::Body,
                    timeout: true,
                    message: "stream idle beyond its timeout".into(),
                })),
                "phase=response body timeout detail=stream idle beyond its timeout",
            ),
            (
                AgentError::IncompleteResponse {
                    stop_reason: "refusal".to_owned(),
                },
                "phase=response completion reason=refusal",
            ),
        ];

        for (error, suffix) in errors {
            let diagnostic = public_error_diagnostic(&error, "openai", "gpt-test");
            assert!(diagnostic.ends_with(suffix), "{diagnostic}");
            assert!(diagnostic.starts_with("provider=openai model=gpt-test "));
        }

        let error = AgentError::Ai(AiError::Http(ygg_ai::HttpError {
            status: http::StatusCode::UNAUTHORIZED,
            request_id: Some("req-auth".into()),
            retry_after: None,
            provider_code: Some("invalid_api_key".into()),
            body_snippet: Some(r#"{"error":{"message":"invalid api key: sk-secret"}}"#.into()),
            retryable: false,
        }));
        let diagnostic = public_error_diagnostic(&error, "openrouter", "openrouter/test");
        assert!(diagnostic.contains("status=401 (authentication failed)"));
        assert!(diagnostic.contains("code=invalid_api_key"));
        assert!(diagnostic.contains("request_id=req-auth"));
        assert!(!diagnostic.contains("sk-secret"));

        assert_eq!(
            public_error_diagnostic(&AgentError::RunEnded, "openai", "gpt-test"),
            "the run has already finished"
        );
    }

    #[test]
    fn connect_timeout_is_not_automatically_retried() {
        let error = AiError::Transport(ygg_ai::TransportError {
            phase: ygg_ai::TransportPhase::Connect,
            timeout: true,
            message: "connection timed out".into(),
        });
        assert!(!retryable_before_generation(&error));
        assert!(!retryable_stream_start(&error));
        assert_eq!(provider_retry_limit(&error), 0);
    }

    #[test]
    fn stream_failure_delegates_classification_to_inner_failure() {
        let progress = ygg_ai::StreamProgress {
            provider_events: 412,
            decoded_events: 38,
            content_bytes: 18_204,
            buffered_bytes: 96,
            first_body_seen: true,
            elapsed_ms: 97_321,
            last_event_ms: Some(97_000),
        };

        // A body-phase disconnect that already streamed bytes: replayable
        // network failure, exactly as the bare transport error was.
        let disconnect = AiError::StreamFailure {
            inner: Box::new(AiError::Transport(ygg_ai::TransportError {
                phase: ygg_ai::TransportPhase::Body,
                timeout: false,
                message: "connection reset by peer".into(),
            })),
            progress,
        };
        assert_eq!(ai_error_phase(&disconnect), "response body");
        assert!(!retryable_before_generation(&disconnect));
        assert!(retryable_stream_start(&disconnect));
        assert!(is_replayable_network_failure(&disconnect));
        assert!(!looks_like_context_error(&disconnect));
        assert_eq!(provider_retry_limit(&disconnect), MAX_NETWORK_RETRIES);

        // A stream that ended on a provider 503 frame keeps that frame's
        // retry budget instead of being demoted to the wrapper's behavior.
        let server_error = AiError::StreamFailure {
            inner: Box::new(AiError::Provider(ygg_ai::ProviderError {
                code: Some("503".into()),
                kind: Some("server_error".into()),
                message: "temporarily unavailable".into(),
                request_id: None,
            })),
            progress,
        };
        assert_eq!(
            ai_error_phase(&server_error),
            "response body (provider error)"
        );
        assert!(retryable_stream_start(&server_error));
        assert!(!is_replayable_network_failure(&server_error));
        assert_eq!(provider_retry_limit(&server_error), MAX_PROVIDER_RETRIES);

        // A transport timeout with a context-flavoured message must still
        // never be classified as context overflow, wrapped or bare.
        let deadline = AiError::StreamFailure {
            inner: Box::new(AiError::Transport(ygg_ai::TransportError {
                phase: ygg_ai::TransportPhase::Body,
                timeout: true,
                message: "context deadline exceeded".into(),
            })),
            progress,
        };
        assert!(!looks_like_context_error(&deadline));
        assert!(!retryable_stream_start(&deadline));
        assert_eq!(provider_retry_limit(&deadline), 0);

        // A post-send heartbeat deadline has ambiguous provider acceptance, so
        // it is terminal even if no generation was decoded.
        let heartbeat = AiError::StreamFailure {
            inner: Box::new(AiError::Transport(ygg_ai::TransportError {
                phase: ygg_ai::TransportPhase::Body,
                timeout: true,
                message: "Responses WebSocket heartbeat acknowledgement timed out".into(),
            })),
            progress,
        };
        assert!(!retryable_before_generation(&heartbeat));
        assert!(!retryable_stream_start(&heartbeat));
        assert!(!is_replayable_network_failure(&heartbeat));
        assert_eq!(provider_retry_limit(&heartbeat), 0);

        // And a provider context-error frame inside a 2xx stream must still
        // be detected through the wrapper, so compaction still triggers.
        let overflow = AiError::StreamFailure {
            inner: Box::new(AiError::Provider(ygg_ai::ProviderError {
                code: None,
                kind: None,
                message: "prompt is too long".into(),
                request_id: None,
            })),
            progress,
        };
        assert!(looks_like_context_error(&overflow));

        // Even the unit variant keeps its exact phase label through the
        // wrapper.
        let canceled = AiError::StreamFailure {
            inner: Box::new(AiError::Canceled),
            progress,
        };
        assert_eq!(ai_error_phase(&canceled), "request cancellation");
    }

    #[test]
    fn websocket_connection_limit_is_retried_before_generation() {
        let error = ygg_ai::ProviderError {
            code: Some("websocket_connection_limit_reached".into()),
            kind: None,
            message: "create a new websocket connection".into(),
            request_id: None,
        };
        assert!(provider_requests_connection_refresh(&error));
        assert!(retryable_stream_start(&AiError::Provider(error)));
        assert_eq!(
            provider_retry_limit(&AiError::Provider(ygg_ai::ProviderError {
                code: Some("websocket_connection_limit_reached".into()),
                kind: None,
                message: "create a new websocket connection".into(),
                request_id: None,
            })),
            MAX_PROVIDER_RETRIES
        );
    }

    #[test]
    fn stream_failure_diagnostic_appends_wire_progress_inside_the_public_bound() {
        let progress = ygg_ai::StreamProgress {
            provider_events: 412,
            decoded_events: 38,
            content_bytes: 18_204,
            buffered_bytes: 96,
            first_body_seen: true,
            elapsed_ms: 97_321,
            last_event_ms: Some(97_000),
        };
        let suffix = "stream_progress=frames=412 events=38 content=18204B buffered=96B first_byte=seen elapsed=97321ms last_event=97000ms";

        let inner = AiError::Provider(ygg_ai::ProviderError {
            // Four oversized fields push the bare diagnostic past the public
            // bound, so the wrapper must reserve room for its progress field
            // instead of letting truncation clip the progress off the end.
            code: Some("x".repeat(600)),
            kind: Some("y".repeat(600)),
            message: "z".repeat(600),
            request_id: Some("w".repeat(600)),
        });
        let bare = public_ai_error_diagnostic(&inner, "openai", "gpt-test");
        assert_eq!(
            bare.len(),
            MAX_PUBLIC_PROVIDER_DIAGNOSTIC_BYTES,
            "fixture must overflow the public bound"
        );
        assert!(bare.ends_with('…'));

        let wrapped = public_ai_error_diagnostic(
            &AiError::StreamFailure {
                inner: Box::new(inner),
                progress,
            },
            "openai",
            "gpt-test",
        );
        assert!(
            wrapped.len() <= MAX_PUBLIC_PROVIDER_DIAGNOSTIC_BYTES,
            "wrapped diagnostic must stay inside the public bound: {}",
            wrapped.len()
        );
        assert!(
            wrapped.ends_with(suffix),
            "truncation must not clip the progress field: {wrapped}"
        );
        assert!(wrapped.contains("phase=response body (provider error)"));
    }

    #[test]
    fn bare_ai_error_variants_surface_bounded_detail() {
        let errors = [
            AiError::Config(ygg_ai::ConfigError::Parse("malformed endpoint file".into())),
            AiError::Auth(ygg_ai::AuthError::Resolve),
            AiError::Validation(ygg_ai::ValidationError::OrphanToolResult(
                ygg_ai::ToolCallId("call_orphan".into()),
            )),
            AiError::Unsupported(ygg_ai::UnsupportedError::Image),
            AiError::Decode(ygg_ai::DecodeError::Json("unterminated string".into())),
            AiError::Pricing(ygg_ai::PricingError::ArithmeticOverflow),
            AiError::StreamProtocol(ygg_ai::StreamProtocolError::MissingFinish),
        ];
        for error in &errors {
            let diagnostic = public_ai_error_diagnostic(error, "openai", "gpt-test");
            assert!(diagnostic.contains("detail="), "{diagnostic}");
            assert!(
                diagnostic.starts_with("provider=openai model=gpt-test phase="),
                "{diagnostic}"
            );
        }
        let config = public_ai_error_diagnostic(
            &AiError::Config(ygg_ai::ConfigError::Parse("malformed endpoint file".into())),
            "openai",
            "gpt-test",
        );
        assert_eq!(
            config,
            "provider=openai model=gpt-test phase=request preparation detail=Parse error: malformed endpoint file"
        );
        let canceled = public_ai_error_diagnostic(&AiError::Canceled, "openai", "gpt-test");
        assert_eq!(
            canceled,
            "provider=openai model=gpt-test phase=request cancellation"
        );
    }

    #[test]
    fn request_estimator_counts_inline_media_semantically_not_as_base64_text() {
        let image = Media::image_bytes(
            bytes::Bytes::from(vec![7u8; 1024 * 1024]),
            "image/png".parse().unwrap(),
        );
        let messages = vec![Message::User(UserMessage {
            content: vec![UserPart::Media(image)],
        })];

        let estimate = estimate_request_tokens("system", &messages, &[]);
        assert!(estimate >= ESTIMATED_IMAGE_TOKENS, "{estimate}");
        assert!(
            estimate < 10_000,
            "inline image bytes were miscounted as text tokens: {estimate}"
        );
    }

    #[test]
    fn canonical_capacity_advances_new_messages_without_rebuilding_history() {
        use ygg_ai::{ModelCatalog, ModelId};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("capacity.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        let system = "system";
        session
            .append(user_message(UserInput::from("first message")))
            .unwrap();
        let messages = session.context().unwrap();
        let baseline = context_breakdown(&session, &model, system, &messages, &[]);
        let mut cache = ContextCapacityCache::seeded(&session, 1, &baseline);

        session
            .append(user_message(UserInput::from("second message")))
            .unwrap();
        let incremental = cache.estimate(&session, &model, system, &[], 1).unwrap();
        let full_messages = session.context().unwrap();
        let full = reconcile_context_estimate(&session, &model, system, &full_messages, &[]);

        assert!(
            incremental.input_tokens >= full.input_tokens,
            "incremental capacity undercounted the request: incremental={incremental:?} full={full:?}"
        );
        assert_eq!(cache.full_rebuilds(), 0);
    }

    #[test]
    fn canonical_capacity_overbounds_coalesced_tool_results_without_a_full_scan() {
        use ygg_ai::{ModelCatalog, ModelId, ToolResult, ToolResultPart};

        fn tool_result(id: &str, text: &str) -> EntryValue {
            EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ygg_ai::ToolCallId(id.into()),
                    content: vec![ToolResultPart::Text(text.into())],
                    is_error: false,
                    added_tool_names: None,
                })],
            }))
        }

        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::create(directory.path().join("coalesced-capacity.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        let system = "system";
        session.append(tool_result("one", "first result")).unwrap();
        let messages = session.context().unwrap();
        let baseline = context_breakdown(&session, &model, system, &messages, &[]);
        let mut cache = ContextCapacityCache::seeded(&session, 1, &baseline);

        session.append(tool_result("two", "second result")).unwrap();
        let incremental = cache.estimate(&session, &model, system, &[], 1).unwrap();
        let full_messages = session.context().unwrap();
        let full = reconcile_context_estimate(&session, &model, system, &full_messages, &[]);

        assert_eq!(
            full_messages.len(),
            1,
            "tool results should coalesce in context"
        );
        assert!(
            incremental.input_tokens >= full.input_tokens,
            "coalesced tool result undercounted the request: incremental={incremental:?} full={full:?}"
        );
        assert_eq!(cache.full_rebuilds(), 0);
    }

    #[test]
    fn canonical_capacity_reanchors_to_authoritative_provider_usage() {
        use ygg_ai::{AssistantMessage, AssistantPart, ModelCatalog, ModelId};

        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::create(directory.path().join("provider-capacity.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        let system = "system";
        session
            .append(user_message(UserInput::from("prompt")))
            .unwrap();
        let messages = session.context().unwrap();
        let baseline = context_breakdown(&session, &model, system, &messages, &[]);
        let mut cache = ContextCapacityCache::seeded(&session, 1, &baseline);
        let usage = Usage {
            input_tokens: 90_000,
            output_tokens: 10_000,
            total_tokens: 100_000,
            ..Usage::default()
        };

        session
            .append_assistant_turn(
                AssistantMessage {
                    content: vec![AssistantPart::Text("answer".into())],
                    model: model.spec.id.clone(),
                    protocol: model.spec.protocol,
                },
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                usage,
                None,
                StopReason::EndTurn,
                None,
            )
            .unwrap();
        cache.observe_assistant_response(&session, &usage);
        let incremental = cache.estimate(&session, &model, system, &[], 1).unwrap();
        let full_messages = session.context().unwrap();
        let full = reconcile_context_estimate(&session, &model, system, &full_messages, &[]);

        assert_eq!(incremental.provider_tokens, Some(100_000));
        assert_eq!(incremental.provider_tokens, full.provider_tokens);
        assert!(incremental.input_tokens >= full.input_tokens);
        assert_eq!(cache.full_rebuilds(), 0);
    }

    #[test]
    fn canonical_capacity_rebuilds_after_a_local_compaction_boundary() {
        use ygg_ai::{ModelCatalog, ModelId};

        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::create(directory.path().join("compaction-capacity.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        let system = "system";
        session
            .append(user_message(UserInput::from("old message")))
            .unwrap();
        let first_kept = session
            .append(user_message(UserInput::from("kept message")))
            .unwrap();
        let messages = session.context().unwrap();
        let baseline = context_breakdown(&session, &model, system, &messages, &[]);
        let mut cache = ContextCapacityCache::seeded(&session, 1, &baseline);

        session.compact("summary", first_kept).unwrap();
        let incremental = cache.estimate(&session, &model, system, &[], 1).unwrap();
        let full_messages = session.context().unwrap();
        let full = reconcile_context_estimate(&session, &model, system, &full_messages, &[]);

        assert_eq!(incremental, full);
        assert_eq!(cache.full_rebuilds(), 1);
    }

    fn tool_media_model(protocol: Protocol, modalities: ygg_ai::ModalitySet) -> Model {
        let base = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-4o-mini".into()))
            .unwrap();
        let mut spec = (*base.spec).clone();
        spec.protocol = protocol;
        spec.capabilities.input_modalities = modalities;
        Model {
            spec: Arc::new(spec),
            endpoint: base.endpoint,
        }
    }

    #[test]
    fn anthropic_tool_image_stays_inside_the_paired_result() {
        let model = tool_media_model(
            Protocol::AnthropicMessages,
            ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image),
        );
        let result = Ok(ToolOutput::new("read=image").with_media(Media::image_bytes(
            bytes::Bytes::from_static(b"png"),
            "image/png".parse().unwrap(),
        )));
        let (message, accepted, _, is_error, _) = lower_tool_result(
            ygg_ai::ToolCallId("call".into()),
            &result,
            &model,
            4096,
            Vec::new(),
        );
        assert_eq!(accepted, vec![ToolOutputMediaKind::Image]);
        assert!(!is_error);
        assert_eq!(message.content.len(), 1);
        let UserPart::ToolResult(result) = &message.content[0] else {
            panic!("tool result must remain first");
        };
        assert!(matches!(
            result.content.get(1),
            Some(ToolResultPart::Media(Media::Image(_)))
        ));
    }

    #[test]
    fn ordered_tool_parts_keep_text_image_text_order_under_one_text_budget() {
        let text_limit = TOOL_TRUNCATION_MARKER.len() + 6;
        for protocol in [Protocol::OpenAiResponses, Protocol::AnthropicMessages] {
            let model = tool_media_model(
                protocol,
                ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image),
            );
            let result = Ok(ToolOutput::from_content_parts([
                ToolOutputContentPart::Text("ABCDEFGHIJKLMNOPQRSTUVWXYZ".into()),
                ToolOutputContentPart::Media(Media::image_bytes(
                    bytes::Bytes::from_static(b"png"),
                    "image/png".parse().unwrap(),
                )),
                ToolOutputContentPart::Text("abcdefghijklmnopqrstuvwxyz".into()),
            ]));

            let (message, accepted, persisted_text, is_error, _) = lower_tool_result(
                ygg_ai::ToolCallId("call".into()),
                &result,
                &model,
                text_limit,
                Vec::new(),
            );

            assert_eq!(accepted, vec![ToolOutputMediaKind::Image]);
            assert!(!is_error);
            assert!(persisted_text.len() <= text_limit);
            let UserPart::ToolResult(result) = &message.content[0] else {
                panic!("expected canonical tool result");
            };
            assert_eq!(result.content.len(), 3);
            assert!(matches!(
                &result.content[0],
                ToolResultPart::Text(text)
                    if text == &format!("ABC{TOOL_TRUNCATION_MARKER}")
            ));
            assert!(matches!(
                result.content[1],
                ToolResultPart::Media(Media::Image(_))
            ));
            assert!(matches!(
                &result.content[2],
                ToolResultPart::Text(text) if text == "xyz"
            ));
            let provider_text_bytes = result
                .content
                .iter()
                .filter_map(|part| match part {
                    ToolResultPart::Text(text) => Some(text.len()),
                    ToolResultPart::Media(_) => None,
                })
                .sum::<usize>();
            assert_eq!(provider_text_bytes, text_limit);
        }
    }

    #[test]
    fn lowering_keeps_structured_details_outside_provider_visible_content() {
        let model = tool_media_model(Protocol::OpenAiResponses, ygg_ai::ModalitySet::none());
        let result = Ok(ToolOutput::new("Found one source.")
            .try_with_details(
                Some(serde_json::json!({"sources": [{"title": "Primary"}]})),
                Some(serde_json::json!({"cache": "miss"})),
            )
            .unwrap());
        let (message, _, _, is_error, details) = lower_tool_result(
            ygg_ai::ToolCallId("call".into()),
            &result,
            &model,
            4096,
            Vec::new(),
        );

        assert!(!is_error);
        let details = details.expect("durable details");
        assert_eq!(
            details.structured_content(),
            Some(&serde_json::json!({"sources": [{"title": "Primary"}]}))
        );
        assert_eq!(
            details.metadata(),
            Some(&serde_json::json!({"cache": "miss"}))
        );
        let UserPart::ToolResult(provider_result) = &message.content[0] else {
            panic!("expected canonical tool result");
        };
        assert_eq!(provider_result.content.len(), 1);
        assert!(matches!(
            provider_result.content[0],
            ToolResultPart::Text(ref text) if text == "Found one source."
        ));
    }

    #[test]
    fn openai_chat_wav_and_mp3_follow_the_paired_tool_result() {
        let model = tool_media_model(
            Protocol::OpenAiChat,
            ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Audio),
        );
        for format in [ygg_ai::AudioFormat::Wav, ygg_ai::AudioFormat::Mp3] {
            let result = Ok(ToolOutput::new("read=audio").with_media(Media::audio_bytes(
                bytes::Bytes::from_static(b"audio"),
                format,
            )));
            let (message, accepted, _, is_error, _) = lower_tool_result(
                ygg_ai::ToolCallId("call".into()),
                &result,
                &model,
                4096,
                Vec::new(),
            );
            assert_eq!(accepted, vec![ToolOutputMediaKind::Audio]);
            assert!(!is_error);
            assert!(matches!(message.content[0], UserPart::ToolResult(_)));
            assert!(matches!(
                message.content[1],
                UserPart::Media(Media::Audio(_))
            ));
        }
    }

    #[test]
    fn unsupported_tool_audio_is_an_error_without_media_or_indicator() {
        let responses = tool_media_model(
            Protocol::OpenAiResponses,
            ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Audio),
        );
        let audio = Ok(ToolOutput::new("read=audio").with_media(Media::audio_bytes(
            bytes::Bytes::from_static(b"audio"),
            ygg_ai::AudioFormat::Wav,
        )));
        let (message, accepted, text, is_error, _) = lower_tool_result(
            ygg_ai::ToolCallId("call".into()),
            &audio,
            &responses,
            4096,
            Vec::new(),
        );
        assert!(accepted.is_empty());
        assert!(is_error);
        assert!(text.contains("protocol cannot replay audio"));
        assert_eq!(message.content.len(), 1);

        let chat = tool_media_model(
            Protocol::OpenAiChat,
            ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Audio),
        );
        let aac = Ok(ToolOutput::new("read=audio").with_media(Media::audio_bytes(
            bytes::Bytes::from_static(b"audio"),
            ygg_ai::AudioFormat::Aac,
        )));
        let (message, accepted, text, is_error, _) = lower_tool_result(
            ygg_ai::ToolCallId("call".into()),
            &aac,
            &chat,
            4096,
            Vec::new(),
        );
        assert!(accepted.is_empty());
        assert!(is_error);
        assert!(text.contains("accepts WAV or MP3"));
        assert_eq!(message.content.len(), 1);
    }

    #[test]
    fn exact_responses_replay_estimate_counts_opaque_provider_payloads() {
        use ygg_ai::{ModelCatalog, ModelId, ResponsesItem, ResponsesOutput};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        session
            .append(user_message(UserInput::from("small prompt")))
            .unwrap();
        let assistant = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("small answer".into())],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiResponses,
            })))
            .unwrap();
        session
            .append_responses_turn(
                assistant,
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                ResponsesOutput::new(vec![ResponsesItem::new(serde_json::json!({
                    "type": "reasoning",
                    "id": "rs_large",
                    "encrypted_content": "x".repeat(40_000),
                    "unknown": {"phase": "analysis"}
                }))
                .unwrap()]),
            )
            .unwrap();

        let messages = session.context().unwrap();
        let canonical = estimate_request_tokens("system", &messages, &[]);
        let estimate = reconcile_context_estimate(&session, &model, "system", &messages, &[]);
        assert!(
            estimate.structural_tokens > canonical.saturating_add(8_000),
            "opaque replay must drive the structural estimate: canonical={canonical}, replay={}",
            estimate.structural_tokens
        );
        assert_eq!(estimate.provider_tokens, None);

        let options = durable_responses_options(&session, &model, "system").unwrap();
        assert!(options.input.is_some());
        assert_eq!(options.previous_response_id, None);
        assert!(!options.store);
    }

    #[test]
    fn native_checkpoint_estimate_excludes_compacted_canonical_media() {
        use ygg_ai::{ModelCatalog, ModelId, ResponsesItem, ResponsesOutput};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Media(Media::image_bytes(
                    bytes::Bytes::from(vec![7u8; 1024 * 1024]),
                    "image/png".parse().unwrap(),
                ))],
            })))
            .unwrap();
        let assistant = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("seen".into())],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiResponses,
            })))
            .unwrap();
        session
            .append_responses_turn(
                assistant,
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                ResponsesOutput::new(vec![ResponsesItem::new(serde_json::json!({
                    "type": "message",
                    "id": "old-output"
                }))
                .unwrap()]),
            )
            .unwrap();
        session
            .append_responses_compaction(
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                ResponsesOutput::new(vec![ResponsesItem::new(serde_json::json!({
                    "type": "compaction",
                    "id": "small-checkpoint",
                    "encrypted_content": "opaque"
                }))
                .unwrap()]),
            )
            .unwrap();

        let messages = session.context().unwrap();
        let estimate = reconcile_context_estimate(
            &session,
            &model,
            "system must already be compacted",
            &messages,
            &[],
        );
        assert!(
            estimate.structural_tokens < 1_000,
            "compacted-away media leaked into the replay estimate: {estimate:?}"
        );
        let exact =
            exact_responses_replay(&session, &model, "system must already be compacted").unwrap();
        let wire = serde_json::to_string(&exact.input).unwrap();
        assert!(wire.contains("small-checkpoint"));
        assert!(!wire.contains("system must already be compacted"));
    }

    #[test]
    fn exact_responses_estimate_counts_current_media_semantically() {
        use ygg_ai::{ModelCatalog, ModelId};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Media(Media::image_bytes(
                    bytes::Bytes::from(vec![7u8; 1024 * 1024]),
                    "image/png".parse().unwrap(),
                ))],
            })))
            .unwrap();

        let messages = session.context().unwrap();
        let estimate = reconcile_context_estimate(&session, &model, "system", &messages, &[]);
        assert!(
            (ESTIMATED_IMAGE_TOKENS..10_000).contains(&estimate.structural_tokens),
            "inline base64 must be replaced by a semantic image estimate: {estimate:?}"
        );
    }

    #[test]
    fn post_checkpoint_instructions_are_included_in_the_exact_estimate() {
        use ygg_ai::{ModelCatalog, ModelId, ResponsesItem, ResponsesOutput};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("old".into())],
            })))
            .unwrap();
        session
            .append_responses_compaction(
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                ResponsesOutput::new(vec![ResponsesItem::new(serde_json::json!({
                    "type": "compaction",
                    "encrypted_content": "small"
                }))
                .unwrap()]),
            )
            .unwrap();

        let messages = session.context().unwrap();
        let short = reconcile_context_estimate(&session, &model, "short", &messages, &[]);
        let long_system = "x".repeat(128 * 1024);
        let long = reconcile_context_estimate(&session, &model, &long_system, &messages, &[]);
        assert!(
            long.structural_tokens > short.structural_tokens.saturating_add(30_000),
            "top-level instructions must participate in capacity checks: short={short:?}, long={long:?}"
        );
    }

    #[test]
    fn marked_failed_turn_boundary_keeps_exact_replay_available_after_restart() {
        use ygg_ai::{ModelCatalog, ModelId};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("fails".into())],
            })))
            .unwrap();
        close_failed_turn(&mut session, &model).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("try again".into())],
            })))
            .unwrap();
        drop(session);

        let session = Session::open(path).unwrap();
        let replay = session
            .responses_replay_items(&model.endpoint.id, &model.spec.id)
            .unwrap()
            .expect("explicit local provenance must not look like a missing sidecar");
        assert!(matches!(
            replay.get(1),
            Some(ResponsesReplayItem::LocalAssistant(message))
                if matches!(
                    message.content.as_slice(),
                    [AssistantPart::Text(text)] if text == FAILED_TURN_CONTEXT_MARKER
                )
        ));
    }

    #[test]
    fn configured_cost_limit_fails_closed_without_trusted_model_pricing() {
        let directory = tempfile::tempdir().unwrap();
        let session = Session::create(directory.path().join("unpriced.jsonl")).unwrap();
        let mut model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-4o-mini".into()))
            .unwrap();
        std::sync::Arc::make_mut(&mut model.spec).pricing = None;

        assert!(matches!(
            reserve_request_cost(&session, &model, 1, 1, Some(10)),
            Err(AgentError::CostUnavailable { limit: 10 })
        ));
    }

    #[tokio::test]
    async fn native_compaction_honors_the_session_cost_limit_before_network() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(user_message(UserInput::from("compact this")))
            .unwrap();
        let model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        let mut agent = Agent::new(AgentConfig {
            client: AiClient::new(),
            model,
            session,
            system: "system".into(),
            sandbox: SandboxConfig::new(directory.path()),
            effect_broker: EffectBroker::default(),
            extensions: ExtensionHost::new(),
            max_turns: Some(1),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ReasoningMode::Standard,
            cache_retention: CacheRetention::Short,
            session_id: None,
        })
        .unwrap();
        agent.set_max_session_cost_microdollars(Some(0));

        let error = agent.compact_responses_native().await.unwrap_err();
        assert!(matches!(error, AgentError::CostLimit { limit: 0, .. }));
        assert!(
            !matches!(
                agent
                    .session()
                    .head_ref()
                    .and_then(|head| agent.session().entry(head)),
                Some(crate::session::Entry {
                    value: EntryValue::ResponsesCompaction { .. },
                    ..
                })
            ),
            "a rejected native request must not persist a checkpoint"
        );
    }

    #[test]
    fn provider_usage_baseline_skips_newer_unusable_records_and_counts_trailing_messages() {
        use ygg_ai::{AssistantMessage, AssistantPart, ModelCatalog, ModelId, Protocol};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        session
            .append(user_message(UserInput::from("old prompt")))
            .unwrap();
        let measured = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("old response".into())],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .record_assistant_usage(
                measured,
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                Usage {
                    input_tokens: 79_000,
                    output_tokens: 1_000,
                    total_tokens: 80_000,
                    ..Usage::default()
                },
                None,
            )
            .unwrap();
        session
            .append(user_message(UserInput::from("x".repeat(4_000))))
            .unwrap();
        let unmeasured = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("new response".into())],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .record_assistant_usage(
                unmeasured,
                model.endpoint.id.clone(),
                ModelId("different-model".into()),
                Usage::default(),
                None,
            )
            .unwrap();

        let estimate = provider_context_estimate(&session, &model).unwrap();
        assert!(estimate > 81_000, "{estimate}");
    }

    #[test]
    fn provider_usage_before_latest_compaction_is_not_reused() {
        use ygg_ai::{AssistantMessage, AssistantPart, ModelCatalog, ModelId, Protocol};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let model = ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        session
            .append(user_message(UserInput::from("old prompt")))
            .unwrap();
        let assistant = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("old response".into())],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .record_assistant_usage(
                assistant.clone(),
                model.endpoint.id.clone(),
                model.spec.id.clone(),
                Usage {
                    total_tokens: 100_000,
                    ..Usage::default()
                },
                None,
            )
            .unwrap();
        session.compact("short summary", assistant).unwrap();

        assert_eq!(provider_context_estimate(&session, &model), None);
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut total = Usage::default();
        let turn = Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 2,
            total_tokens: 15,
            ..Usage::default()
        };
        add_usage(&mut total, &turn);
        add_usage(&mut total, &turn);
        assert_eq!(total.input_tokens, 20);
        assert_eq!(total.output_tokens, 10);
        assert_eq!(total.reasoning_tokens, 4);
        assert_eq!(total.total_tokens, 30);
    }

    #[test]
    fn run_cost_carries_submicrodollar_remainders_across_turns() {
        let mut total = CostAccumulator::default();
        let fractional = Cost {
            total_picodollars_remainder: 600_000,
            ..Cost::default()
        };
        total.add(Some(fractional));
        total.add(Some(fractional));
        assert_eq!(total.microdollars, 1);
        assert_eq!(total.picodollars_remainder, 200_000);
    }

    #[test]
    fn compaction_boundaries_include_each_completed_tool_episode() {
        use ygg_ai::{
            AssistantMessage, AssistantPart, ModelId, Protocol, ToolResult, ToolResultPart,
        };

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(user_message(UserInput::from("one task")))
            .unwrap();
        for (index, text) in [("a", "first"), ("b", "second")] {
            session
                .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::ToolCall(ToolCall {
                        id: ygg_ai::ToolCallId(index.into()),
                        name: "read".into(),
                        arguments_json: "{}".into(),
                        argument_error: None,
                    })],
                    model: ModelId("test".into()),
                    protocol: Protocol::AnthropicMessages,
                })))
                .unwrap();
            session
                .append(EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::ToolResult(ToolResult {
                        tool_call_id: ygg_ai::ToolCallId(index.into()),
                        content: vec![ToolResultPart::Text(text.into())],
                        is_error: false,
                        added_tool_names: None,
                    })],
                })))
                .unwrap();
        }
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("done".into())],
                model: ModelId("test".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();

        assert_eq!(turn_starts(&session).len(), 3);
    }

    #[test]
    fn assistant_after_compaction_marker_remains_a_turn_boundary() {
        use ygg_ai::{AssistantMessage, AssistantPart, ModelId, Protocol};

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(user_message(UserInput::from("one task")))
            .unwrap();
        let first_assistant = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("first".into())],
                model: ModelId("test".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        session
            .append(user_message(UserInput::from("continue")))
            .unwrap();
        session.compact("summary", first_assistant).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("after marker".into())],
                model: ModelId("test".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();

        assert_eq!(turn_starts(&session).len(), 2);
    }

    #[test]
    fn hard_token_reservation_rejects_before_a_request_can_cross_the_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let session = Session::create(directory.path().join("token-limit.jsonl")).unwrap();
        let error = reserve_request_tokens(&session, 700, 400, Some(1_000)).unwrap_err();
        assert!(matches!(
            error,
            AgentError::TokenLimit {
                current: 0,
                reserved: 1_100,
                limit: 1_000
            }
        ));
    }

    #[test]
    fn delegated_usage_is_accounting_not_parent_context_token_consumption() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("delegated-ledger.jsonl")).unwrap();
        session
            .record_delegated_agent_usage(DelegatedUsage {
                agent_id: "agent-1".into(),
                turn_count: 2,
                tool_call_count: 1,
                endpoint: ygg_ai::EndpointId("test-endpoint".into()),
                model: ygg_ai::ModelId("test-model".into()),
                usage: Usage {
                    input_tokens: 40_000,
                    output_tokens: 10_000,
                    total_tokens: 50_000,
                    ..Usage::default()
                },
                cost: None,
            })
            .unwrap();

        assert_eq!(session_total_tokens_for_own_context(&session), 0);
        assert!(reserve_request_tokens(&session, 700, 200, Some(1_000)).is_ok());
        assert_eq!(session.usage_records()[0].usage.total_tokens, 50_000);
    }

    #[tokio::test]
    async fn abort_flag_wakes_waiters_and_stays_set() {
        let flag = Arc::new(AbortFlag::default());
        let waiter = {
            let flag = flag.clone();
            tokio::spawn(async move { flag.wait().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flag.set();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter must wake")
            .unwrap();
        // Late waiters return immediately.
        tokio::time::timeout(std::time::Duration::from_secs(1), flag.wait())
            .await
            .expect("level-triggered wait");
        assert!(flag.is_set());
    }
}
