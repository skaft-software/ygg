//! The agent: configuration, the procedural run loop, and run control.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_core::Stream;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use ygg_ai::{
    AiClient, AiError, AssistantPart, AudioPayload, CacheRetention, CompatibilityMode, Cost,
    ImageSource, Media, Message, Model, OutputFormat, OutputModalities, ReasoningConfig, Request,
    StopReason, StreamEvent, ToolCall, ToolChoice, ToolDef, ToolResult, ToolResultPart, Usage,
    UserMessage, UserPart, PICODOLLARS_PER_MICRODOLLAR,
};

use crate::context::{ContextSnapshot, ContextTracker};
use crate::events::{AgentEvent, Control, FinishReason, OutputChannel};
use crate::extension::{EventObserver, ExtensionHost, ToolCallHook};
use crate::input::UserInput;
use crate::sandbox::SandboxConfig;
use crate::session::{EntryId, EntryMetadata, EntryValue, Session, SessionError};
use crate::tool::{
    CancellationToken, ReplaySafety, Tool, ToolContext, ToolError, ToolOutput, ToolProgress,
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
    /// Two tools were registered under the same name.
    #[error("duplicate tool name registered: {0}")]
    DuplicateTool(String),
    /// The configured workspace root is unusable.
    #[error("invalid workspace: {0}")]
    Workspace(String),
    /// The provider ended a response without a normal completion signal.
    #[error("model response did not complete normally: {stop_reason}")]
    IncompleteResponse {
        /// Provider termination reason.
        stop_reason: String,
    },
    /// The next billable request's conservative reservation would cross the
    /// configured session spend ceiling.
    #[error("session cost limit would be exceeded: current {current} µUSD + reserved {reserved} µUSD > limit {limit} µUSD")]
    CostLimit {
        /// Durable session cost before the request.
        current: u64,
        /// Conservative worst-case request reservation.
        reserved: u64,
        /// Configured ceiling.
        limit: u64,
    },
    /// The request would exceed the model's context budget after compaction.
    #[error("request context is too large: approximately {estimate} tokens exceeds the {budget}-token input budget")]
    ContextExceeded {
        /// Estimated request size.
        estimate: u64,
        /// Maximum input size after reserving output capacity.
        budget: u64,
    },
    /// Internal autonomous work was cancelled before its commit point.
    #[error("operation cancelled")]
    Cancelled,
    /// A control message was sent after the run finished.
    #[error("the run has already finished")]
    RunEnded,
}

/// How an agent decides that a natural no-tool response is complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// Accept the first normal no-tool response.
    #[default]
    Natural,
    /// Require a second, internal JSON confirmation turn. Work tools are
    /// unavailable and its text is not rendered to the user, matching Terminus
    /// 2's parsed double `task_complete` check without exposing a looping
    /// completion tool.
    Confirmed,
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
    /// Registered tools and event observers. Register [`CoreTools`](crate::tools::CoreTools)
    /// here for the built-in `read`/`edit`/`write`/`exec`/`search` tools.
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
    system: String,
    max_turns: Option<u64>,
    reasoning: ReasoningConfig,
    cache_retention: CacheRetention,
    /// Optional provider route used for autonomous context summaries.
    /// Defaults to the active model when unset.
    compaction_model: Option<Model>,
    session_id: String,
    tool_scope: String,
    completion_policy: CompletionPolicy,
    max_output_tokens: u64,
    /// Stable semantic source key persisted with user-submitted prompts.
    prompt_model_source: Option<String>,
    prompt_color: Option<String>,
    /// One-shot user-visible text for the next prompt. Model-only context is
    /// persisted in the message body for exact replay instead.
    prompt_display_text: Option<String>,
    max_session_cost_microdollars: Option<u64>,
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

        // PTY sessions are process resources, not durable conversation state.
        // Scope cleanup to this exact Agent so concurrent sessions in the same
        // workspace cannot kill each other's interactive children.
        if self
            .extensions
            .tools
            .iter()
            .any(|tool| tool.definition().name == "exec")
        {
            crate::tools::cleanup_pty_scope(&self.tool_scope);
        }
    }
}

/// Aggregate result of [`Agent::complete`].
#[derive(Debug)]
pub struct RunOutput {
    /// Concatenated visible text from all turns.
    pub text: String,
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
}

impl<'a> Run<'a> {
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

    /// Queues input for after the current run settles: when the model completes
    /// a turn without tool calls, the run continues with this input instead of
    /// finishing.
    pub async fn follow_up(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .send(Control::FollowUp(input.into()))
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

fn notify_observers(observers: &[Arc<dyn EventObserver>], event: &AgentEvent) {
    for observer in observers {
        observer.on_event(event);
    }
}

/// A realistic default output reservation for coding turns. Reserving the
/// provider's maximum (which may be hundreds of thousands of tokens) makes
/// proactive context recovery happen far too early.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16 * 1024;
/// Leave room for a visible answer after token-budget reasoning when the model
/// advertises enough output capacity.
const REASONING_ANSWER_RESERVE: u64 = 1024;
/// Aggregate model-visible tool-result budget for one model turn.
const TOOL_RESULT_TURN_BUDGET: usize = 16 * 1024;
/// Bound actual tool executions emitted in one assistant turn. Every excess
/// call still receives a compact error result so provider pairing remains valid.
const MAX_TOOL_CALLS_PER_TURN: usize = 32;
const TOOL_TRUNCATION_MARKER: &str = "\n[tool output truncated]\n";
/// Maximum retries for a transient provider failure. A replacement attempt is
/// safe even after deltas were received: streamed output is provisional, the
/// assistant message is persisted only after `Finished`, and tools are not
/// executed until that point.
const MAX_PROVIDER_RETRIES: usize = 3;
/// A failed connection/header exchange is expensive (DNS/TCP/TLS/proxy waits)
/// and has not produced model output. One visible replacement attempt avoids
/// multiplying a long network outage into minutes of apparent UI silence.
const MAX_CONNECT_RETRIES: usize = 1;
const COMPLETION_CONFIRMATION_PROMPT: &str = r#"A candidate final response was produced. Based only on the current bounded state and latest verification evidence, report whether the requested task is actually complete. Do not rerun commands merely to confirm. Respond with exactly one JSON object and no Markdown or prose:
{"complete":true|false,"reason":"concise evidence or remaining work"}"#;

#[derive(Debug)]
struct CompletionConfirmation {
    complete: bool,
    reason: String,
}

fn parse_completion_confirmation(
    assistant: &ygg_ai::AssistantMessage,
) -> Result<CompletionConfirmation, String> {
    let text = assistant
        .content
        .iter()
        .filter_map(|part| match part {
            ygg_ai::AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err("confirmation response contained no JSON text".to_owned());
    }
    let value = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .or_else(|| {
            let start = text.find('{')?;
            let end = text.rfind('}')?;
            serde_json::from_str(&text[start..=end]).ok()
        })
        .ok_or_else(|| "confirmation response was not a valid JSON object".to_owned())?;
    let complete = value
        .get("complete")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "confirmation requires boolean `complete`".to_owned())?;
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty() && reason.chars().count() <= 4000)
        .ok_or_else(|| "confirmation requires a concise non-empty `reason`".to_owned())?
        .to_owned();
    Ok(CompletionConfirmation { complete, reason })
}

fn continuation_instruction(stop_reason: &StopReason) -> &'static str {
    match stop_reason {
        StopReason::MaxTokens => "The previous response was truncated at the token limit. Continue the task from the persisted state; do not claim completion until the work is finished and verified.",
        StopReason::Other(reason) if reason == "tool_output_locked" => "The previous response emitted an internal locked-output placeholder instead of the intended structured call. Re-issue that tool call now using the provider's required tool-call format; do not print any control placeholder.",
        _ => "The provider paused the turn. Continue the task from the persisted state and do not claim completion until the work is finished and verified.",
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
                    ygg_ai::ReasoningEffort::Max => budgets.max,
                })
            })
            .unwrap_or_default(),
        ReasoningConfig::Off => 0,
    }
}

fn agent_max_output_tokens(model: &Model, reasoning: &ReasoningConfig) -> u64 {
    let model_max = model.spec.limits.max_output_tokens.max(1);
    let reasoning_floor = reasoning_token_budget(model, reasoning)
        .saturating_add(REASONING_ANSWER_RESERVE)
        .min(model_max);
    DEFAULT_MAX_OUTPUT_TOKENS
        .max(reasoning_floor)
        .min(model_max)
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

fn cancelled_tool_error() -> ToolError {
    ToolError::new(
        "tool execution cancelled by user; state may be partially changed and must not be replayed automatically",
    )
}

fn pending_tool_state(
    session: &Session,
) -> Option<(Vec<ToolCall>, HashSet<ygg_ai::ToolCallId>, usize)> {
    let mut persisted = HashSet::new();
    let mut persisted_text_bytes = 0usize;
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
                return (!calls.is_empty()).then_some((calls, persisted, persisted_text_bytes));
            }
            EntryValue::Message(Message::User(user)) => {
                for part in &user.content {
                    let UserPart::ToolResult(result) = part else {
                        continue;
                    };
                    persisted.insert(result.tool_call_id.clone());
                    for content in &result.content {
                        if let ToolResultPart::Text(text) = content {
                            persisted_text_bytes = persisted_text_bytes.saturating_add(text.len());
                        }
                    }
                }
            }
            EntryValue::Compaction { .. }
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

fn truncate_tool_text(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    // Once the aggregate budget is exhausted, later calls carry an empty
    // result rather than one new omission marker each.
    if budget == 0 {
        return String::new();
    }
    if budget <= TOOL_TRUNCATION_MARKER.len() {
        return TOOL_TRUNCATION_MARKER[..budget].to_owned();
    }
    let available = budget - TOOL_TRUNCATION_MARKER.len();
    let head = available / 2;
    let tail = available - head;
    let mut result = String::with_capacity(budget);
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

fn persist_pending_cancellations(session: &mut Session) -> Result<(), AgentError> {
    let Some((calls, persisted, persisted_text_bytes)) = pending_tool_state(session) else {
        return Ok(());
    };
    let unresolved = calls
        .into_iter()
        .filter(|call| !persisted.contains(&call.id));
    let mut tool_result_budget = TOOL_RESULT_TURN_BUDGET.saturating_sub(persisted_text_bytes);
    for call in unresolved {
        let text = cancelled_tool_error().message;
        let text = truncate_tool_text(&text, tool_result_budget);
        tool_result_budget = tool_result_budget.saturating_sub(text.len());
        session.append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::ToolResult(ToolResult {
                tool_call_id: call.id,
                content: vec![ToolResultPart::Text(text)],
                is_error: true,
            })],
        })))?;
    }
    Ok(())
}

fn retryable_before_generation(error: &AiError) -> bool {
    match error {
        AiError::Http(error) => error.is_safe_to_retry(),
        AiError::Transport(error) => error.phase == ygg_ai::TransportPhase::ConnectOrHeaders,
        _ => false,
    }
}

fn looks_like_context_error(error: &AiError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    [
        "context",
        "too many tokens",
        "token limit",
        "prompt is too long",
        "input is too long",
        "maximum tokens",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn retryable_stream_start(error: &AiError) -> bool {
    retryable_before_generation(error)
        || matches!(
            error,
            AiError::Transport(transport)
                if transport.phase == ygg_ai::TransportPhase::Body
        )
        || matches!(error, AiError::Provider(_))
        || matches!(
            error,
            AiError::StreamProtocol(
                ygg_ai::StreamProtocolError::MissingFinish
                    | ygg_ai::StreamProtocolError::PrematureEof
            )
        )
}

fn provider_retry_limit(error: &AiError) -> usize {
    if matches!(
        error,
        AiError::Transport(transport)
            if transport.phase == ygg_ai::TransportPhase::ConnectOrHeaders
    ) {
        if matches!(error, AiError::Transport(transport) if transport.timeout) {
            // Retrying a complete response-header deadline can double a long
            // period with no provider progress. Let the user explicitly retry
            // after the first bounded timeout instead.
            0
        } else {
            MAX_CONNECT_RETRIES
        }
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
    format!(
        "provider={} model={}: {error}",
        model.endpoint.id.0, model.spec.id.0
    )
}

async fn execute_recovery_call(
    tool: Arc<dyn Tool>,
    hooks: &[Arc<dyn ToolCallHook>],
    call: &ToolCall,
    sandbox: &SandboxConfig,
    tool_scope: &str,
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
                active_skills: &active_skills,
                registered_tools,
                progress: progress_sink,
                cancellation: CancellationToken::default(),
            };
            for hook in hooks {
                if let Err(error) = hook.before_tool_call(&call.name, &args, &context).await {
                    return Ok(Err(error));
                }
            }
            let hook_arguments = args.clone();
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
                Ok(output) => (output.text.as_str(), false),
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

fn xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn active_system_prompt(base: &str, session: &Session) -> String {
    let mut system = base.to_owned();
    if let Some(head_id) = session.head() {
        if let Ok(state) = session.resolve_active_skills(&head_id) {
            for skill in state.active_skills {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&format!(
                    "<skill_instructions id=\"{}\" hash=\"{}\">\n",
                    xml_attribute(&skill.descriptor.id),
                    xml_attribute(&skill.instructions_hash)
                ));
                system.push_str(&skill.instructions);
                system.push_str("\n</skill_instructions>");
            }
        }
    }
    system
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

const PROACTIVE_COMPACTION_FREE_TOKENS: u64 = 8_000;

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
    let reserved = worst_case_request_cost(model, input_tokens, output_tokens).unwrap_or_default();
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
    session_id: &'a str,
    max_session_cost_microdollars: Option<u64>,
    abort: &'a AbortFlag,
}

struct CapacityEstimate {
    input_tokens: u64,
    active_system: String,
}

impl<'a> CompactionContext<'a> {
    #[allow(clippy::too_many_arguments)] // Mirrors the borrowed request-run state held by the context.
    fn new(
        client: &'a AiClient,
        model: &'a Model,
        compaction_model: &'a Model,
        session: &'a mut Session,
        usage: &'a mut Usage,
        run_cost: &'a mut CostAccumulator,
        cache_retention: CacheRetention,
        session_id: &'a str,
        max_session_cost_microdollars: Option<u64>,
        abort: &'a AbortFlag,
    ) -> Self {
        Self {
            client,
            model,
            compaction_model,
            session,
            usage,
            run_cost,
            cache_retention,
            session_id,
            max_session_cost_microdollars,
            abort,
        }
    }

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
        reserve_request_cost(
            self.session,
            self.compaction_model,
            input_tokens,
            request.max_output_tokens.unwrap_or(output_tokens),
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

    /// Summarize a history in one tool-free request.
    ///
    /// The former Terminus-style question/answer follow-ups replayed the full
    /// expensive history, defeating prompt reuse and charging it multiple
    /// times. A single grounded summary has the same durable handoff role.
    async fn summarize(&mut self, messages: &[Message]) -> Result<Option<String>, AgentError> {
        self.call(
            "Summarize this coding-agent history for continuation. Preserve completed work, file paths, commands, edits, failures, decisions, constraints, and unresolved work. Be concise and factual; do not claim unverified completion.",
            messages.to_vec(),
            4096,
        )
        .await
    }

    async fn compact_one_boundary(&mut self, boundary_index: usize) -> Result<bool, AgentError> {
        let starts = turn_starts(self.session);
        let Some(first_kept) = starts.get(boundary_index).cloned() else {
            return Ok(false);
        };
        let before = self.session.context_before(&first_kept)?;
        if before.is_empty() {
            return Ok(false);
        }
        let Some(summary) = self.summarize(&before).await? else {
            return Ok(false);
        };
        if self.abort.is_set() {
            return Err(AgentError::Cancelled);
        }
        self.session.compact(summary, first_kept)?;
        Ok(true)
    }

    async fn ensure_capacity(
        &mut self,
        system: &str,
        tools: &[ToolDef],
        max_output_tokens: u64,
    ) -> Result<CapacityEstimate, AgentError> {
        let budget = self
            .model
            .spec
            .limits
            .context_window
            .saturating_sub(max_output_tokens);
        // Do not wait for a provider-side context error when fewer than 8K
        // input tokens remain. A small model gets the smaller of the configured
        // reserve and its whole input budget.
        let proactive_free_tokens = PROACTIVE_COMPACTION_FREE_TOKENS.min(budget);
        loop {
            let active_system = active_system_prompt(system, self.session);
            let estimate = {
                let messages = self.session.context_ref()?;
                estimate_request_tokens(&active_system, &messages, tools)
            };
            let free_tokens = budget.saturating_sub(estimate);
            if estimate <= budget && free_tokens >= proactive_free_tokens {
                return Ok(CapacityEstimate {
                    input_tokens: estimate,
                    active_system,
                });
            }
            // Boundary 0 is already the oldest full-fidelity episode. Recompute
            // starts after every compaction and drop one more episode.
            if self.compact_one_boundary(1).await? {
                continue;
            }
            if estimate <= budget {
                return Ok(CapacityEstimate {
                    input_tokens: estimate,
                    active_system,
                });
            }
            return Err(AgentError::ContextExceeded { estimate, budget });
        }
    }

    async fn force_one_boundary(
        &mut self,
        system: &str,
        tools: &[ToolDef],
        max_output_tokens: u64,
    ) -> Result<(), AgentError> {
        if self.compact_one_boundary(1).await? {
            return Ok(());
        }
        let estimate = {
            let messages = self.session.context_ref()?;
            let active_system = active_system_prompt(system, self.session);
            estimate_request_tokens(&active_system, &messages, tools)
        };
        let budget = self
            .model
            .spec
            .limits
            .context_window
            .saturating_sub(max_output_tokens);
        Err(AgentError::ContextExceeded { estimate, budget })
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
        let session_id = config
            .session_id
            .unwrap_or_else(|| config.session.cache_key());
        let max_output_tokens = agent_max_output_tokens(&config.model, &config.reasoning);
        let tool_scope = next_tool_scope();
        Ok(Self {
            client: config.client,
            model: config.model,
            session: config.session,
            extensions: config.extensions,
            sandbox: config.sandbox,
            system: config.system,
            max_turns: config.max_turns,
            reasoning: config.reasoning,
            cache_retention: config.cache_retention,
            compaction_model: None,
            session_id,
            tool_scope,
            completion_policy: CompletionPolicy::Natural,
            max_output_tokens,
            prompt_model_source: None,
            prompt_color: None,
            prompt_display_text: None,
            max_session_cost_microdollars: None,
            last_run_lifecycle: None,
        })
    }

    /// Read-only access to the agent's session (its entries and head).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Read-only access to the selected model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Replace the system prompt at an idle boundary. Product frontends use
    /// this to apply typed extension context without exposing private agent
    /// state; the value is cloned into the next run when [`prompt`](Self::prompt)
    /// starts.
    pub fn set_system_prompt(&mut self, system: impl Into<String>) {
        self.system = system.into();
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
    /// message payload for replay.
    pub fn set_prompt_display_text(&mut self, text: Option<String>) {
        self.prompt_display_text = text.filter(|text| !text.is_empty());
    }

    fn prompt_entry_metadata(&mut self) -> EntryMetadata {
        EntryMetadata {
            prompt_model: Some(self.model.spec.id.clone()),
            prompt_model_source: self.prompt_model_source.clone(),
            prompt_color: self.prompt_color.clone(),
            display_text: self.prompt_display_text.take(),
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

    /// Configure a conservative hard ceiling for billable session requests.
    /// Before every normal or compaction request, priced models reserve their
    /// worst-case input/output cost; a request that could cross the ceiling is
    /// rejected before network I/O.
    pub fn set_max_session_cost_microdollars(&mut self, limit: Option<u64>) {
        self.max_session_cost_microdollars = limit;
    }

    /// Configure the model used for autonomous context summaries. Passing
    /// `None` keeps summaries on the active conversation model.
    pub fn set_compaction_model(&mut self, model: Option<Model>) {
        self.compaction_model = model;
    }

    /// Read-only access to the autonomous compaction model, if overridden.
    pub fn compaction_model(&self) -> Option<&Model> {
        self.compaction_model.as_ref()
    }

    /// Mutable access to the session for history operations between runs
    /// (checkout, manual compaction, config entries).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Selects completion behavior for subsequent runs.
    pub fn set_completion_policy(&mut self, policy: CompletionPolicy) {
        self.completion_policy = policy;
    }

    /// Returns the selected completion policy.
    pub fn completion_policy(&self) -> CompletionPolicy {
        self.completion_policy
    }

    /// Exact registered tool names after the frontend has applied all policy
    /// filters and extension registration. The sorted result is suitable for
    /// deterministic diagnostics and capability validation at idle boundaries.
    pub fn registered_tool_names(&self) -> Vec<String> {
        let mut names = self
            .extensions
            .tools
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
        let Some((calls, persisted, persisted_text_bytes)) = pending_tool_state(&self.session)
        else {
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

        let mut tool_map: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        for tool in &self.extensions.tools {
            let definition = tool.definition();
            tool_map.insert(definition.name, Arc::clone(tool));
        }
        let mut registered_tools = tool_map.keys().cloned().collect::<Vec<_>>();
        registered_tools.sort();
        let sandbox = self.sandbox.clone();
        let tool_scope = self.tool_scope.clone();
        let tool_call_hooks = self.extensions.tool_call_hooks.clone();
        let mut tool_result_budget = TOOL_RESULT_TURN_BUDGET.saturating_sub(persisted_text_bytes);
        for (call_index, call) in unresolved {
            let result = if call_index >= MAX_TOOL_CALLS_PER_TURN {
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
                            &call,
                            &sandbox,
                            &tool_scope,
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
            let (text, is_error) = match result {
                Ok(output) => (output.text, false),
                Err(error) => (error.message, true),
            };
            self.session
                .append(EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::ToolResult(ToolResult {
                        tool_call_id: call.id,
                        content: vec![ToolResultPart::Text({
                            let truncated = truncate_tool_text(&text, tool_result_budget);
                            tool_result_budget = if text.len() > tool_result_budget {
                                0
                            } else {
                                tool_result_budget.saturating_sub(text.len())
                            };
                            truncated
                        })],
                        is_error,
                    })],
                })))?;
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
        // A previous process may have died after persisting an assistant tool
        // call but before persisting its result. Repair that semantic boundary
        // before appending a new user message; otherwise strict provider
        // validation would reject the resumed conversation as malformed.
        let previous_run_was_dropped = self
            .last_run_lifecycle
            .take()
            .is_some_and(|lifecycle| lifecycle.dropped.load(Ordering::Acquire));
        self.recover_pending_tools(previous_run_was_dropped).await?;
        let prompt_metadata = self.prompt_entry_metadata();
        let first_entry = self
            .session
            .append_with_metadata(user_message(input.into()), Some(prompt_metadata.clone()))?;
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
        let tools = self.extensions.tools.clone();
        let observers = self.extensions.observers.clone();
        let tool_call_hooks = self.extensions.tool_call_hooks.clone();
        let max_turns = self.max_turns;
        let reasoning = self.reasoning.clone();
        let cache_retention = self.cache_retention;
        let session_id = self.session_id.clone();
        let tool_scope = self.tool_scope.clone();
        let completion_policy = self.completion_policy;
        let max_output_tokens = self.max_output_tokens;
        let max_session_cost_microdollars = self.max_session_cost_microdollars;
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

            let mut tool_map: HashMap<String, Arc<dyn Tool>> =
                HashMap::with_capacity(tools.len());
            for tool in &tools {
                let definition = tool.definition();
                tool_map.insert(definition.name, Arc::clone(tool));
            }
            let mut registered_tools = tool_map.keys().cloned().collect::<Vec<_>>();
            registered_tools.sort();
            let tool_defs: Vec<ToolDef> = tools.iter().map(|t| t.definition()).collect();

            let mut pending_steer: Vec<UserInput> = Vec::new();
            let mut followups: VecDeque<UserInput> = VecDeque::new();
            let mut control_open = true;
            let mut completed_turns: u64 = 0;
            let mut completion_confirmation_pending = false;
            let mut context_retries = 0usize;
            let mut run_usage = Usage::default();
            let mut run_cost = CostAccumulator::default();

            let mut reason: FinishReason = 'run: loop {
                // ── Drain control at the turn boundary ─────────────────────
                while control_open {
                    match control_rx.try_recv() {
                        Ok(Control::Steer(input)) => pending_steer.push(input),
                        Ok(Control::FollowUp(input)) => followups.push_back(input),
                        Ok(Control::Abort) => break 'run FinishReason::Aborted,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => control_open = false,
                    }
                }
                if abort.is_set() {
                    break 'run FinishReason::Aborted;
                }

                // ── Steering enters here, at the model-turn boundary ───────
                // Drain the whole steering queue before opening the next
                // request. This is the "all at once" steering mode: every
                // message queued since the previous boundary is visible to
                // the same model turn, after all of that turn's tools have
                // finished.
                if !pending_steer.is_empty() {
                    // New user intent supersedes an internal completion check.
                    completion_confirmation_pending = false;
                    let queued = std::mem::take(&mut pending_steer);
                    let mut delivered = Vec::with_capacity(queued.len());
                    for input in queued {
                        let summary = input.text_summary();
                        if let Err(e) = session.append_with_metadata(
                            user_message(input),
                            Some(prompt_metadata.clone()),
                        ) {
                            if !delivered.is_empty() {
                                let ev = AgentEvent::SteeringDelivered {
                                    messages: delivered,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            break 'run FinishReason::Failed(e.into());
                        }
                        delivered.push(summary);
                    }
                    if !delivered.is_empty() {
                        let ev = AgentEvent::SteeringDelivered {
                            messages: delivered,
                        };
                        notify_observers(&observers, &ev);
                        yield ev;
                    }
                }

                // ── Turn guard ─────────────────────────────────────────────
                if let Some(limit) = max_turns {
                    if completed_turns >= limit {
                        break 'run FinishReason::MaxTurns;
                    }
                }

                let confirmation_turn = completion_policy == CompletionPolicy::Confirmed
                    && completion_confirmation_pending;
                let request_tool_defs = if confirmation_turn {
                    Vec::new()
                } else {
                    tool_defs.clone()
                };

                // ── Reconstruct and size context for this exact turn ───────
                // This gate is inside the autonomous loop, after every tool
                // result, and uses the exact active tool schema set. Internal
                // confirmation turns deliberately expose no tools.
                let capacity = {
                    let mut compaction = CompactionContext::new(
                        &client,
                        &model,
                        &compaction_model,
                        session,
                        &mut run_usage,
                        &mut run_cost,
                        cache_retention,
                        &session_id,
                        max_session_cost_microdollars,
                        &abort,
                    );
                    compaction
                        .ensure_capacity(&system, &request_tool_defs, max_output_tokens)
                        .await
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
                let messages = match session.context() {
                    Ok(m) => m,
                    Err(e) => break 'run FinishReason::Failed(e.into()),
                };
                let active_system = capacity.active_system;

                let request = Request {
                    system: if active_system.is_empty() { None } else { Some(active_system) },
                    messages,
                    tools: request_tool_defs.clone(),
                    tool_choice: if confirmation_turn {
                        ToolChoice::None
                    } else {
                        ToolChoice::Auto
                    },
                    max_output_tokens: Some(max_output_tokens),
                    temperature: None,
                    stop: vec![],
                    reasoning: reasoning.clone(),
                    output_format: OutputFormat::Text,
                    output_modalities: OutputModalities::Text,
                    compatibility: CompatibilityMode::Strict,
                    cache_retention,
                    session_id: Some(session_id.clone()),
                };

                if let Err(error) = reserve_request_cost(
                    session,
                    &model,
                    input_tokens,
                    max_output_tokens,
                    max_session_cost_microdollars,
                ) {
                    break 'run FinishReason::Failed(error);
                }

                // ── Open the provider stream (abortable) ───────────────────
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
                            if stream_retries < provider_retry_limit(&error)
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
                        }
                        result => break result,
                    }
                };
                let mut response_stream = match opened {
                    Err(error) if context_retries < MAX_PROVIDER_RETRIES && looks_like_context_error(&error) => {
                        context_retries += 1;
                        let compacted = {
                            let mut compaction = CompactionContext::new(
                                &client,
                                &model,
                                &compaction_model,
                                session,
                                &mut run_usage,
                                &mut run_cost,
                                cache_retention,
                                &session_id,
                                max_session_cost_microdollars,
                                &abort,
                            );
                            compaction
                                .force_one_boundary(&system, &request_tool_defs, max_output_tokens)
                                .await
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
                    Err(error) => break 'run FinishReason::Failed(error.into()),
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
                    Abort,
                }
                let mut attempt_saw_generation = false;
                let turn = 'consume: loop {
                    let next = tokio::select! {
                        ev = response_stream.next() => Next::Event(ev),
                        c = control_rx.recv(), if control_open => Next::Ctl(c),
                        _ = abort.wait() => Next::Abort,
                    };
                    match next {
                        Next::Abort | Next::Ctl(Some(Control::Abort)) => {
                            break Err(FinishReason::Aborted);
                        }
                        Next::Ctl(Some(Control::Steer(input))) => pending_steer.push(input),
                        Next::Ctl(Some(Control::FollowUp(input))) => followups.push_back(input),
                        Next::Ctl(None) => control_open = false,
                        Next::Event(None) => {
                            let error = AiError::StreamProtocol(
                                ygg_ai::StreamProtocolError::MissingFinish,
                            );
                            if !attempt_saw_generation
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
                                let reopened = tokio::select! {
                                    result = async {
                                        tokio::time::sleep(delay).await;
                                        open_provider_stream(
                                            &client,
                                            &model,
                                            request_for_retry.clone(),
                                            &abort,
                                        ).await
                                    } => result,
                                    _ = abort.wait() => Ok(None),
                                };
                                match reopened {
                                    Ok(Some(stream)) => {
                                        response_stream = stream;
                                        attempt_saw_generation = false;
                                        continue 'consume;
                                    }
                                    Ok(None) => break Err(FinishReason::Aborted),
                                    Err(error) => break Err(FinishReason::Failed(error.into())),
                                }
                            }
                            break Err(FinishReason::Failed(error.into()));
                        }
                        Next::Event(Some(Err(error))) => {
                            if !attempt_saw_generation
                                && context_retries < MAX_PROVIDER_RETRIES
                                && looks_like_context_error(&error)
                            {
                                context_retries += 1;
                                let compacted = {
                                    let mut compaction = CompactionContext::new(
                                        &client,
                                        &model,
                                        &compaction_model,
                                        session,
                                        &mut run_usage,
                                        &mut run_cost,
                                        cache_retention,
                                        &session_id,
                                        max_session_cost_microdollars,
                                        &abort,
                                    );
                                    compaction
                                        .force_one_boundary(
                                            &system,
                                            &request_tool_defs,
                                            max_output_tokens,
                                        )
                                        .await
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
                            if !attempt_saw_generation
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
                                let reopened = tokio::select! {
                                    result = async {
                                        tokio::time::sleep(delay).await;
                                        open_provider_stream(
                                            &client,
                                            &model,
                                            request_for_retry.clone(),
                                            &abort,
                                        ).await
                                    } => result,
                                    _ = abort.wait() => Ok(None),
                                };
                                match reopened {
                                    Ok(Some(stream)) => {
                                        response_stream = stream;
                                        attempt_saw_generation = false;
                                        continue 'consume;
                                    }
                                    Ok(None) => break Err(FinishReason::Aborted),
                                    Err(error) => break Err(FinishReason::Failed(error.into())),
                                }
                            }
                            break Err(FinishReason::Failed(error.into()));
                        }
                        Next::Event(Some(Ok(event))) => {
                            stream_context.observe_stream(&event);
                            match event {
                            StreamEvent::TextDelta { delta, .. } => {
                                attempt_saw_generation = true;
                                if !confirmation_turn {
                                    let ev = AgentEvent::OutputDelta {
                                        channel: OutputChannel::Text,
                                        text: delta,
                                    };
                                    notify_observers(&observers, &ev);
                                    yield ev;
                                }
                            }
                            StreamEvent::ReasoningDelta { delta, .. } => {
                                attempt_saw_generation = true;
                                if !confirmation_turn {
                                    let ev = AgentEvent::OutputDelta {
                                        channel: OutputChannel::Reasoning,
                                        text: delta,
                                    };
                                    notify_observers(&observers, &ev);
                                    yield ev;
                                }
                            }
                            // `ygg-ai`'s ResponseBuilder assembles the message
                            // and validates the stream; the agent does not
                            // duplicate that parser. Raw tool-argument deltas
                            // are deliberately not exposed.
                            StreamEvent::ToolCallStart { .. }
                            | StreamEvent::ToolCallArgsDelta { .. }
                            | StreamEvent::ToolCallEnd { .. }
                            | StreamEvent::MediaCompleted { .. } => {
                                attempt_saw_generation = true;
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
                let assistant = response.message;
                let calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ygg_ai::AssistantPart::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();

                let assistant_entry = match session
                    .append(EntryValue::Message(Message::Assistant(assistant.clone())))
                {
                    Ok(entry) => entry,
                    Err(error) => break 'run FinishReason::Failed(error.into()),
                };
                add_usage(&mut run_usage, &turn_usage);
                let turn_cost = response.cost;
                if let Err(error) = session.record_assistant_usage(
                    assistant_entry,
                    model.endpoint.id.clone(),
                    model.spec.id.clone(),
                    turn_usage,
                    turn_cost,
                ) {
                    break 'run FinishReason::Failed(error.into());
                }
                run_cost.add(turn_cost);
                let session_cost = (session.total_cost_microdollars() > 0
                    || model.spec.pricing.is_some())
                .then(|| session.total_cost_microdollars());
                let ev = AgentEvent::TurnFinished {
                    message: assistant.clone(),
                    turn_usage,
                    usage: run_usage,
                    session_cost_microdollars: session_cost,
                    run_cost_microdollars: run_cost.microdollars,
                };
                notify_observers(&observers, &ev);
                yield ev;

                // Drain any steer/follow-up that arrived while TurnFinished
                // was being processed by the caller. Without this, a steer
                // that lands between TurnFinished and the calls.is_empty()
                // check below is left stranded in the channel buffer and
                // lost when the run completes without tool calls.
                while control_open {
                    match control_rx.try_recv() {
                        Ok(Control::Steer(input)) => pending_steer.push(input),
                        Ok(Control::FollowUp(input)) => followups.push_back(input),
                        Ok(Control::Abort) => {
                            abort.set();
                            break;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => control_open = false,
                    }
                }

                let normal_end = matches!(stop_reason, StopReason::EndTurn | StopReason::StopSequence);
                let needs_continuation = matches!(stop_reason, StopReason::MaxTokens | StopReason::PauseTurn)
                    || matches!(&stop_reason, StopReason::Other(reason) if reason == "tool_output_locked");

                // A response is not successful merely because it contains no
                // tool calls. Refusals, pauses, provider-specific reasons, and
                // malformed tool-use endings are terminal failures; a length
                // stop gets one corrective continuation instead.
                if !normal_end
                    && !needs_continuation
                    && !matches!(stop_reason, StopReason::ToolUse)
                {
                    break 'run FinishReason::Failed(AgentError::IncompleteResponse {
                        stop_reason: format!("{stop_reason:?}"),
                    });
                }

                if confirmation_turn {
                    let confirmation = if needs_continuation {
                        Err("confirmation response was truncated or paused".to_owned())
                    } else if calls.is_empty() {
                        parse_completion_confirmation(&assistant)
                    } else {
                        Err("confirmation turn emitted an unexpected tool call".to_owned())
                    };
                    // The request exposed no tools, but pair any provider bug or
                    // malformed historical call so strict replay remains valid.
                    for call in &calls {
                        if let Err(error) = session.append(EntryValue::Message(Message::User(
                            UserMessage {
                                content: vec![UserPart::ToolResult(ToolResult {
                                    tool_call_id: call.id.clone(),
                                    content: vec![ToolResultPart::Text(
                                        "confirmation accepts JSON text only".to_owned(),
                                    )],
                                    is_error: true,
                                })],
                            },
                        ))) {
                            break 'run FinishReason::Failed(error.into());
                        }
                    }

                    if abort.is_set() {
                        break 'run FinishReason::Aborted;
                    }
                    match confirmation {
                        Ok(confirmation) if confirmation.complete => {
                            if !pending_steer.is_empty() {
                                completion_confirmation_pending = false;
                                continue;
                            }
                            if let Some(input) = followups.pop_front() {
                                completion_confirmation_pending = false;
                                if let Err(error) = session.append_with_metadata(
                                    user_message(input),
                                    Some(prompt_metadata.clone()),
                                ) {
                                    break 'run FinishReason::Failed(error.into());
                                }
                                continue;
                            }
                            break 'run FinishReason::Completed;
                        }
                        Ok(confirmation) => {
                            completion_confirmation_pending = false;
                            let prompt = format!(
                                "Completion was not confirmed: {} Continue the work and produce a new final response after verification.",
                                confirmation.reason
                            );
                            if let Err(error) =
                                session.append(user_message(UserInput::from(prompt)))
                            {
                                break 'run FinishReason::Failed(error.into());
                            }
                            continue;
                        }
                        Err(error) => {
                            let prompt = format!(
                                "The completion confirmation was invalid ({error}). Return exactly the requested JSON object; no tools, Markdown, or additional prose."
                            );
                            if let Err(session_error) =
                                session.append(user_message(UserInput::from(prompt)))
                            {
                                break 'run FinishReason::Failed(session_error.into());
                            }
                            continue;
                        }
                    }
                }

                if calls.is_empty() {
                    if abort.is_set() {
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
                            stop_reason: format!("{stop_reason:?}"),
                        });
                    }
                    // A pending steer re-enters the loop so the model sees it;
                    // otherwise a queued follow-up begins now that the run has
                    // settled. A normal no-tool end turn is completion.
                    if !pending_steer.is_empty() {
                        continue;
                    }
                    if let Some(input) = followups.pop_front() {
                        if let Err(e) = session.append_with_metadata(
                            user_message(input),
                            Some(prompt_metadata.clone()),
                        ) {
                            break 'run FinishReason::Failed(e.into());
                        }
                        continue;
                    }
                    if completion_policy == CompletionPolicy::Confirmed {
                        completion_confirmation_pending = true;
                        if let Err(error) = session.append(user_message(UserInput::from(
                            COMPLETION_CONFIRMATION_PROMPT,
                        ))) {
                            break 'run FinishReason::Failed(error.into());
                        }
                        continue;
                    }
                    break 'run FinishReason::Completed;
                }

                // ── Execute tools sequentially, in emitted order ───────────
                let mut tool_result_budget = TOOL_RESULT_TURN_BUDGET;
                for (call_index, call) in calls.into_iter().enumerate() {
                    let parsed = call.arguments_value();
                    stream_context.tool_started();
                    let ev = AgentEvent::ToolStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: parsed.as_ref().cloned().unwrap_or(serde_json::Value::Null),
                    };
                    notify_observers(&observers, &ev);
                    yield ev;

                    // Create a fresh progress channel for every tool
                    // call. Non‑streaming tools (unknown, bad args)
                    // simply never push into it; the drain below is a
                    // no‑op.
                    let (progress_tx, mut progress_rx) =
                        mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
                    let progress_sink = ToolProgressSink::live(progress_tx);

                    let mut cancellation_won = false;
                    let result: Result<ToolOutput, ToolError> = if call_index >= MAX_TOOL_CALLS_PER_TURN {
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
                                    active_skills: &active_skills,
                                    registered_tools: &registered_tools,
                                    progress: progress_sink.clone(),
                                    cancellation: abort.cancellation.clone(),
                                };
                                let hook_arguments = args.clone();
                                let mut hook_denial = None;
                                for hook in &tool_call_hooks {
                                    if let Err(error) = hook
                                        .before_tool_call(&call.name, &hook_arguments, &tool_ctx)
                                        .await
                                    {
                                        hook_denial = Some(error);
                                        break;
                                    }
                                }
                                if let Some(error) = hook_denial {
                                    Err(error)
                                } else if abort.is_set() {
                                    cancellation_won = true;
                                    Err(cancelled_tool_error())
                                } else {
                                let execute = tool.execute(args, &tool_ctx);
                                tokio::pin!(execute);
                                // Cancellation drops the pinned future, which
                                // kills any child process tree it spawned.
                                let outcome = loop {
                                    tokio::select! {
                                        biased;
                                        _ = abort.wait() => break None,
                                        r = &mut execute => break Some(r),
                                        c = control_rx.recv(), if control_open => match c {
                                            Some(Control::Steer(input)) => pending_steer.push(input),
                                            Some(Control::FollowUp(input)) => followups.push_back(input),
                                            Some(Control::Abort) => {
                                                abort.set();
                                                break None;
                                            }
                                            None => control_open = false,
                                        },
                                        progress = progress_rx.recv() => {
                                            if let Some(p) = progress {
                                                // `execute` can enqueue progress and synchronously
                                                // trigger cancellation during the same select poll,
                                                // after the biased abort branch was already checked.
                                                // Recheck before accepting semantic state.
                                                if abort.is_set() {
                                                    if let ToolProgress::SessionEvent(_, reply_tx_mutex) = p {
                                                        if let Ok(mut opt) = reply_tx_mutex.lock() {
                                                            if let Some(reply_tx) = opt.take() {
                                                                let _ = reply_tx.send(Err(
                                                                    "session event discarded because cancellation won".to_string()
                                                                ));
                                                            }
                                                        }
                                                    }
                                                    break None;
                                                }
                                                if let ToolProgress::SessionEvent(event, reply_tx_mutex) = p {
                                                    let res = session.append(*event);
                                                    if let Ok(mut opt) = reply_tx_mutex.lock() {
                                                        if let Some(reply_tx) = opt.take() {
                                                            let _ = reply_tx.send(res.map_err(|e| e.to_string()));
                                                        }
                                                    }
                                                } else {
                                                    let ev = AgentEvent::ToolProgress {
                                                        id: call.id.clone(),
                                                        progress: p,
                                                    };
                                                    notify_observers(&observers, &ev);
                                                    yield ev;
                                                }
                                            }
                                        },
                                    }
                                };
                                let result = match outcome {
                                    Some(r) => r,
                                    None => {
                                        cancellation_won = true;
                                        Err(cancelled_tool_error())
                                    }
                                };
                                let (output, is_error) = match &result {
                                    Ok(output) => (output.text.as_str(), false),
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
                                result
                                }
                            }
                        }
                    };

                    // ── COMMIT BOUNDARY ──────────────────────────────────
                    // Tool::execute resolved (or an immediate error was
                    // produced). Persist the result immediately before
                    // draining progress or checking abort. An abort
                    // received after this point cannot erase an already-
                    // committed result.
                    let (raw_text, is_error) = match &result {
                        Ok(output) => (output.text.as_str(), false),
                        Err(error) => (error.message.as_str(), true),
                    };
                    let text = truncate_tool_text(raw_text, tool_result_budget);
                    tool_result_budget = if raw_text.len() > tool_result_budget {
                        0
                    } else {
                        tool_result_budget.saturating_sub(raw_text.len())
                    };
                    let tool_result = ToolResult {
                        tool_call_id: call.id.clone(),
                        content: vec![ToolResultPart::Text(text)],
                        is_error,
                    };
                    if let Err(e) = session.append(EntryValue::Message(Message::User(
                        UserMessage {
                            content: vec![UserPart::ToolResult(tool_result)],
                        },
                    ))) {
                        break 'run FinishReason::Failed(e.into());
                    }

                    // ── Drain accepted progress before ToolFinished ───────
                    while let Ok(p) = progress_rx.try_recv() {
                        if cancellation_won {
                            // The biased select deliberately gives cancellation
                            // precedence. Events already accepted in the loop
                            // remain durable, but a queued semantic event must
                            // not take effect after the tool was reported as
                            // cancelled (notably, it must not activate a skill).
                            if let ToolProgress::SessionEvent(_, reply_tx_mutex) = p {
                                if let Ok(mut opt) = reply_tx_mutex.lock() {
                                    if let Some(reply_tx) = opt.take() {
                                        let _ = reply_tx.send(Err(
                                            "session event discarded because cancellation won"
                                                .to_string(),
                                        ));
                                    }
                                }
                            }
                            continue;
                        }
                        if let ToolProgress::SessionEvent(event, reply_tx_mutex) = p {
                            let res = session.append(*event);
                            if let Ok(mut opt) = reply_tx_mutex.lock() {
                                if let Some(reply_tx) = opt.take() {
                                    let _ = reply_tx.send(res.map_err(|e| e.to_string()));
                                }
                            }
                        } else {
                            let ev = AgentEvent::ToolProgress {
                                id: call.id.clone(),
                                progress: p,
                            };
                            notify_observers(&observers, &ev);
                            yield ev;
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
                    let ev = AgentEvent::ToolFinished {
                        id: call.id.clone(),
                        result,
                    };
                    notify_observers(&observers, &ev);
                    yield ev;

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
        })
    }

    /// Runs to completion, returning the aggregate output.
    ///
    /// A run that ends with [`FinishReason::Failed`] is returned as `Err`;
    /// aborted and max-turns runs return `Ok` with their reason.
    pub async fn complete(&mut self, input: impl Into<UserInput>) -> Result<RunOutput, AgentError> {
        let mut run = self.prompt(input).await?;
        let mut text = String::new();
        // Deltas are provisional until their provider turn reaches `Finished`.
        // A retry invalidates only the current attempt, not text committed by
        // earlier tool turns in the same autonomous run.
        let mut committed_text_len = 0usize;
        let mut usage = Usage::default();
        let mut run_cost: u64 = 0;
        while let Some(event) = run.next().await {
            match event {
                AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: delta,
                } => text.push_str(&delta),
                AgentEvent::ProviderRetry { .. } => text.truncate(committed_text_len),
                AgentEvent::SteeringDelivered { .. } => {}
                AgentEvent::TurnFinished {
                    usage: total,
                    run_cost_microdollars: cost,
                    ..
                } => {
                    committed_text_len = text.len();
                    usage = total;
                    run_cost = cost;
                }
                AgentEvent::RunFinished { head, reason } => {
                    return match reason {
                        FinishReason::Failed(e) => Err(e),
                        reason => Ok(RunOutput {
                            text,
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
    fn active_skill_instructions_are_labelled_and_injected_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::SkillActivated {
                descriptor: crate::skills::SkillDescriptor {
                    id: "focused-review".into(),
                    name: "Focused review".into(),
                    description: "Review relevant code".into(),
                    version: None,
                    source: crate::skills::SkillSource::BuiltIn,
                    trust: crate::skills::SkillTrust::BuiltIn,
                    required_tools: vec![],
                    tags: vec![],
                },
                instructions_hash: "abc123".into(),
                instructions: "Inspect only the relevant files.".into(),
            })
            .unwrap();

        let prompt = active_system_prompt("base", &session);
        assert!(prompt.contains("<skill_instructions id=\"focused-review\" hash=\"abc123\">"));
        assert!(prompt.contains("</skill_instructions>"));
        assert_eq!(
            prompt.matches("Inspect only the relevant files.").count(),
            1
        );
    }

    #[test]
    fn response_header_timeout_is_not_automatically_retried() {
        let error = AiError::Transport(ygg_ai::TransportError {
            phase: ygg_ai::TransportPhase::ConnectOrHeaders,
            timeout: true,
            message: "response headers stalled".into(),
        });
        assert_eq!(provider_retry_limit(&error), 0);
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
