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
    AiClient, AiError, AssistantMessage, AssistantPart, AudioPayload, CacheRetention,
    CompatibilityMode, Cost, ImageSource, Media, Message, Model, OutputFormat, OutputModalities,
    ReasoningConfig, ReasoningMode, Request, StopReason, StreamEvent, ToolCall, ToolChoice,
    ToolDef, ToolResult, ToolResultPart, Usage, UserMessage, UserPart, PICODOLLARS_PER_MICRODOLLAR,
};

use crate::compaction::{
    build_handoff_message, finish_handoff, prepare_handoff, HandoffPreparation,
    SUMMARIZATION_SYSTEM_PROMPT,
};
use crate::context::{ContextSnapshot, ContextTracker};
use crate::events::{
    AgentEvent, CompactionInfo, CompactionReason, Control, FinishReason, OutputChannel,
};
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
    /// Repeated non-timeout network failures exhausted automatic recovery.
    #[error(
        "network connection failed after {retries} retries. Are you connected to the internet? ({detail})"
    )]
    NetworkUnavailable {
        /// Number of replacement attempts made after the initial request.
        retries: usize,
        /// Sanitized transport detail for diagnostics.
        detail: String,
    },
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
    system: String,
    max_turns: Option<u64>,
    reasoning: ReasoningConfig,
    reasoning_mode: ReasoningMode,
    cache_retention: CacheRetention,
    /// Optional provider route used for autonomous context summaries.
    /// Defaults to the active model when unset.
    compaction_model: Option<Model>,
    auto_compaction_enabled: bool,
    compaction_threshold_fraction: f64,
    compaction_keep_recent_turns: usize,
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

        // Tool process groups are owned by per-call RAII guards. There are no
        // persistent shell sessions to clean up when the agent is dropped.
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
/// Bound actual tool executions emitted in one assistant turn. Every excess
/// call still receives a compact error result so provider pairing remains valid.
const MAX_TOOL_CALLS_PER_TURN: usize = 32;
const FAILED_TURN_CONTEXT_MARKER: &str =
    "The previous assistant turn failed before completion. Do not continue that request unless the user asks again.";
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
                UserPart::Media(_) => Some("[media]"),
                UserPart::ToolResult(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                AssistantPart::Media(_) => Some("[generated media]"),
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
        ReasoningConfig::Off | ReasoningConfig::On => 0,
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

fn persist_pending_cancellations(session: &mut Session) -> Result<(), AgentError> {
    let Some((calls, persisted)) = pending_tool_state(session) else {
        return Ok(());
    };
    let unresolved = calls
        .into_iter()
        .filter(|call| !persisted.contains(&call.id));
    for call in unresolved {
        let text = cancelled_tool_error().message;
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

fn close_failed_turn(session: &mut Session, model: &Model) -> Result<(), AgentError> {
    let ends_with_user = {
        let context = session.context_ref()?;
        matches!(context.last(), Some(Message::User(_)))
    };
    if ends_with_user {
        session.append(EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Text(FAILED_TURN_CONTEXT_MARKER.to_owned())],
            model: model.spec.id.clone(),
            protocol: model.spec.protocol,
        })))?;
    }
    Ok(())
}

fn retryable_before_generation(error: &AiError) -> bool {
    match error {
        AiError::Http(error) => error.is_safe_to_retry(),
        AiError::Transport(error) => {
            !error.timeout && error.phase == ygg_ai::TransportPhase::ConnectOrHeaders
        }
        _ => false,
    }
}

fn is_transient_network_failure(error: &AiError) -> bool {
    matches!(error, AiError::Transport(transport) if !transport.timeout)
}

fn looks_like_context_error(error: &AiError) -> bool {
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

fn retryable_provider_error(error: &ygg_ai::ProviderError) -> bool {
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
}

fn provider_retry_limit(error: &AiError) -> usize {
    if matches!(error, AiError::Transport(transport) if transport.timeout) {
        // A timeout already consumed the configured wait budget. Repeating it
        // automatically would turn one bounded deadline into prolonged UI
        // silence; let the user explicitly retry instead.
        0
    } else if is_transient_network_failure(error) {
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
    let context = format!(
        "provider={} model={}: {error}",
        model.endpoint.id.0, model.spec.id.0
    );
    if is_transient_network_failure(error) {
        format!("Network connection lost. Are you connected to the internet? {context}")
    } else {
        context
    }
}

fn provider_failure(error: AiError, retries: usize) -> AgentError {
    if is_transient_network_failure(&error) {
        AgentError::NetworkUnavailable {
            retries,
            detail: error.to_string(),
        }
    } else {
        error.into()
    }
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
        .rposition(|entry| matches!(entry.value, EntryValue::Compaction { .. }))
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

fn estimate_context_tokens(
    session: &Session,
    model: &Model,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> u64 {
    reconcile_context_estimate(session, model, system, messages, tools).input_tokens
}

fn reconcile_context_estimate(
    session: &Session,
    model: &Model,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> RequestContextEstimate {
    let structural_tokens = estimate_request_tokens(system, messages, tools);
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
    enabled: bool,
    threshold_fraction: f64,
    keep_recent_turns: usize,
    events: &'a mpsc::UnboundedSender<AgentEvent>,
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
        enabled: bool,
        threshold_fraction: f64,
        keep_recent_turns: usize,
        events: &'a mpsc::UnboundedSender<AgentEvent>,
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
            enabled,
            threshold_fraction,
            keep_recent_turns,
            events,
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
            reasoning_mode: ReasoningMode::Standard,
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

    /// Generate one Pi-compatible structured handoff in a tool-free request.
    async fn summarize(
        &mut self,
        preparation: &HandoffPreparation,
    ) -> Result<Option<String>, AgentError> {
        self.call(
            SUMMARIZATION_SYSTEM_PROMPT,
            vec![build_handoff_message(preparation)],
            4096,
        )
        .await
    }

    fn preferred_boundary(&self) -> Option<EntryId> {
        let starts = turn_starts(self.session);
        (starts.len() > self.keep_recent_turns)
            .then(|| starts[starts.len() - self.keep_recent_turns].clone())
    }

    fn oldest_reducible_boundary(&self) -> Option<EntryId> {
        turn_starts(self.session).get(1).cloned()
    }

    async fn compact_boundary(
        &mut self,
        first_kept: EntryId,
        reason: CompactionReason,
    ) -> Result<CompactionInfo, AgentError> {
        let _ = self.events.send(AgentEvent::CompactionStarted { reason });
        let preparation = prepare_handoff(self.session, &first_kept)?;
        if preparation.messages.is_empty() {
            let error = AgentError::ContextExceeded {
                estimate: 0,
                budget: self
                    .model
                    .spec
                    .limits
                    .context_window
                    .saturating_sub(self.model.spec.limits.max_output_tokens),
            };
            let _ = self.events.send(AgentEvent::CompactionFinished {
                reason,
                result: Err(error.to_string()),
            });
            return Err(error);
        }
        let summary = match self.summarize(&preparation).await {
            Ok(Some(summary)) => finish_handoff(summary, &preparation.details),
            Ok(None) => {
                let error = AgentError::IncompleteResponse {
                    stop_reason: "compaction summary did not finish normally".to_owned(),
                };
                let _ = self.events.send(AgentEvent::CompactionFinished {
                    reason,
                    result: Err(error.to_string()),
                });
                return Err(error);
            }
            Err(error) => {
                let _ = self.events.send(AgentEvent::CompactionFinished {
                    reason,
                    result: Err(error.to_string()),
                });
                return Err(error);
            }
        };
        if self.abort.is_set() {
            let error = AgentError::Cancelled;
            let _ = self.events.send(AgentEvent::CompactionFinished {
                reason,
                result: Err(error.to_string()),
            });
            return Err(error);
        }
        if let Err(error) = self.session.compact_with_details(
            summary.clone(),
            first_kept.clone(),
            preparation.details,
        ) {
            let error = AgentError::Session(error);
            let _ = self.events.send(AgentEvent::CompactionFinished {
                reason,
                result: Err(error.to_string()),
            });
            return Err(error);
        }
        let info = CompactionInfo {
            summary,
            first_kept,
        };
        let _ = self.events.send(AgentEvent::CompactionFinished {
            reason,
            result: Ok(info.clone()),
        });
        Ok(info)
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
        let threshold = ((self.model.spec.limits.context_window as f64) * self.threshold_fraction)
            .floor() as u64;
        loop {
            let active_system = active_system_prompt(system, self.session);
            let estimate = {
                let messages = self.session.context_ref()?;
                estimate_context_tokens(self.session, self.model, &active_system, &messages, tools)
            };
            let over_capacity = estimate > budget;
            let over_threshold = estimate.saturating_add(max_output_tokens) > threshold;
            if !over_capacity && (!self.enabled || !over_threshold) {
                return Ok(CapacityEstimate {
                    input_tokens: estimate,
                    active_system,
                });
            }
            if !self.enabled {
                return Err(AgentError::ContextExceeded { estimate, budget });
            }
            // `keep_recent_turns` is a preference, not permission to sail past
            // the configured threshold. If the retained episodes themselves
            // are unusually large, compact the oldest reducible episode.
            let boundary = self
                .preferred_boundary()
                .or_else(|| self.oldest_reducible_boundary());
            if let Some(first_kept) = boundary {
                let reason = if over_capacity {
                    CompactionReason::Overflow
                } else {
                    CompactionReason::Threshold
                };
                self.compact_boundary(first_kept, reason).await?;
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
        let boundary = self
            .enabled
            .then(|| {
                self.preferred_boundary()
                    .or_else(|| self.oldest_reducible_boundary())
            })
            .flatten();
        if let Some(first_kept) = boundary {
            self.compact_boundary(first_kept, CompactionReason::Overflow)
                .await?;
            return Ok(());
        }
        let estimate = {
            let messages = self.session.context_ref()?;
            let active_system = active_system_prompt(system, self.session);
            estimate_context_tokens(self.session, self.model, &active_system, &messages, tools)
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

struct TerminalGateContext<'a> {
    client: &'a AiClient,
    model: &'a Model,
    session: &'a mut Session,
    usage: &'a mut Usage,
    run_cost: &'a mut CostAccumulator,
    cache_retention: CacheRetention,
    session_id: &'a str,
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
            reasoning_mode: config.reasoning_mode,
            cache_retention: config.cache_retention,
            compaction_model: None,
            auto_compaction_enabled: true,
            compaction_threshold_fraction: 0.85,
            compaction_keep_recent_turns: 4,
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

    /// Configure autonomous context compaction for subsequent runs.
    ///
    /// `threshold_fraction` is the fraction of the complete model context
    /// window reserved by current input plus the requested output allowance.
    /// `keep_recent_turns` is a preference: when those turns alone exceed the
    /// configured threshold or capacity, recovery compacts the oldest episode.
    pub fn set_compaction_policy(
        &mut self,
        enabled: bool,
        threshold_fraction: f64,
        keep_recent_turns: usize,
    ) -> Result<(), AgentError> {
        if !threshold_fraction.is_finite() || threshold_fraction <= 0.0 || threshold_fraction > 1.0
        {
            return Err(AgentError::InvalidCompactionPolicy(
                "threshold fraction must be finite and between 0 and 1".to_owned(),
            ));
        }
        if keep_recent_turns == 0 {
            return Err(AgentError::InvalidCompactionPolicy(
                "keep_recent_turns must be at least 1".to_owned(),
            ));
        }
        self.auto_compaction_enabled = enabled;
        self.compaction_threshold_fraction = threshold_fraction;
        self.compaction_keep_recent_turns = keep_recent_turns;
        Ok(())
    }

    /// Current autonomous compaction policy `(enabled, threshold, keep)`.
    pub fn compaction_policy(&self) -> (bool, f64, usize) {
        (
            self.auto_compaction_enabled,
            self.compaction_threshold_fraction,
            self.compaction_keep_recent_turns,
        )
    }

    /// Output-token reservation applied to each normal provider request.
    pub fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    /// Estimate the next request using the same provider-reconciled baseline
    /// as autonomous capacity checks, without mutating the session.
    pub fn request_context_estimate(&self) -> Result<RequestContextEstimate, SessionError> {
        let messages = self.session.context_ref()?;
        let system = active_system_prompt(&self.system, &self.session);
        let tools = self.extensions.tool_definitions();
        Ok(reconcile_context_estimate(
            &self.session,
            &self.model,
            &system,
            &messages,
            &tools,
        ))
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
                        content: vec![ToolResultPart::Text(truncate_tool_text(
                            &text,
                            sandbox.max_output_bytes,
                        ))],
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
        let input = input.into();
        let terminal_gate_prior_context =
            if self.completion_policy == CompletionPolicy::TerminalGate {
                recent_conversational_context(&self.session.context()?)
            } else {
                String::new()
            };
        let initial_request = input.text_summary();
        let prompt_metadata = self.prompt_entry_metadata();
        let first_entry = self
            .session
            .append_with_metadata(user_message(input), Some(prompt_metadata.clone()))?;
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
        let reasoning_mode = self.reasoning_mode;
        let cache_retention = self.cache_retention;
        let session_id = self.session_id.clone();
        let tool_scope = self.tool_scope.clone();
        let completion_policy = self.completion_policy;
        let max_output_tokens = self.max_output_tokens;
        let max_session_cost_microdollars = self.max_session_cost_microdollars;
        let auto_compaction_enabled = self.auto_compaction_enabled;
        let compaction_threshold_fraction = self.compaction_threshold_fraction;
        let compaction_keep_recent_turns = self.compaction_keep_recent_turns;
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
            let mut terminal_gate_requests = vec![initial_request];
            let mut terminal_action_receipts = Vec::<TerminalActionReceipt>::new();
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
                    let queued = std::mem::take(&mut pending_steer);
                    let mut delivered = Vec::with_capacity(queued.len());
                    for input in queued {
                        let summary = input.text_summary();
                        terminal_gate_requests.push(summary.clone());
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

                let request_tool_defs = tool_defs.clone();

                // ── Reconstruct and size context for this exact turn ───────
                // This gate is inside the autonomous loop, after every tool
                // result, and uses the exact active tool schema set.
                let (compaction_event_tx, mut compaction_event_rx) =
                    mpsc::unbounded_channel::<AgentEvent>();
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
                        auto_compaction_enabled,
                        compaction_threshold_fraction,
                        compaction_keep_recent_turns,
                        &compaction_event_tx,
                    );
                    let operation = compaction
                        .ensure_capacity(&system, &request_tool_defs, max_output_tokens);
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
                let messages = match session.context() {
                    Ok(m) => m,
                    Err(e) => break 'run FinishReason::Failed(e.into()),
                };
                let active_system = capacity.active_system;

                let request = Request {
                    system: if active_system.is_empty() { None } else { Some(active_system) },
                    messages,
                    tools: request_tool_defs.clone(),
                    tool_choice: ToolChoice::Auto,
                    max_output_tokens: Some(max_output_tokens),
                    temperature: None,
                    stop: vec![],
                    reasoning: reasoning.clone(),
                    reasoning_mode,
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
                                auto_compaction_enabled,
                                compaction_threshold_fraction,
                                compaction_keep_recent_turns,
                                &compaction_event_tx,
                            );
                            let operation = compaction.force_one_boundary(
                                &system,
                                &request_tool_defs,
                                max_output_tokens,
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
                                        auto_compaction_enabled,
                                        compaction_threshold_fraction,
                                        compaction_keep_recent_turns,
                                        &compaction_event_tx,
                                    );
                                    let operation = compaction.force_one_boundary(
                                        &system,
                                        &request_tool_defs,
                                        max_output_tokens,
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
                            while (!attempt_saw_generation
                                || is_transient_network_failure(&error))
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
                let normal_end = matches!(stop_reason, StopReason::EndTurn | StopReason::StopSequence);
                let needs_continuation = matches!(stop_reason, StopReason::MaxTokens | StopReason::PauseTurn)
                    || matches!(&stop_reason, StopReason::Other(reason) if reason == "tool_output_locked");
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
                        stop_reason: format!("{stop_reason:?}"),
                    });
                }

                if calls.is_empty() {
                    if abort.is_set() {
                        if gated_candidate {
                            let ev = AgentEvent::CandidateRejected {
                                usage: run_usage,
                                run_cost_microdollars: run_cost.microdollars,
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
                            stop_reason: format!("{stop_reason:?}"),
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
                    if let Some(input) = followups.pop_front() {
                        if gated_candidate {
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
                        }
                        let summary = input.text_summary();
                        terminal_gate_requests.push(summary);
                        if let Err(e) = session.append_with_metadata(
                            user_message(input),
                            Some(prompt_metadata.clone()),
                        ) {
                            break 'run FinishReason::Failed(e.into());
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
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                break 'run FinishReason::Aborted;
                            }
                            Err(error) => {
                                let ev = AgentEvent::CandidateRejected {
                                    usage: run_usage,
                                    run_cost_microdollars: run_cost.microdollars,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                                break 'run FinishReason::Failed(error);
                            }
                        }
                    }
                    break 'run FinishReason::Completed;
                }

                // ── Execute tools sequentially, in emitted order ───────────
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
                    // Every tool owns the same configured output allowance.
                    // A large early result must never starve later successful
                    // calls in the same model turn. Core tools already return
                    // bounded, continuation-aware output; this defensive cap
                    // also covers third-party tools.
                    let text = truncate_tool_text(raw_text, sandbox.max_output_bytes);
                    terminal_action_receipts.push(TerminalActionReceipt {
                        tool: call.name.clone(),
                        arguments: call.arguments_json.clone(),
                        status: if is_error { "error" } else { "ok" },
                        result: text.clone(),
                    });
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
            // Failed provider turns also need an assistant boundary. Without
            // one, the next prompt is appended after the unresolved user task
            // and models commonly continue the stale request instead.
            if matches!(reason, FinishReason::Failed(_)) {
                if let Err(error) = close_failed_turn(session, &model) {
                    reason = FinishReason::Failed(error);
                }
            }
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
                AgentEvent::CandidateRejected {
                    usage: total,
                    run_cost_microdollars: cost,
                } => {
                    text.truncate(committed_text_len);
                    usage = total;
                    run_cost = cost;
                }
                AgentEvent::SteeringDelivered { .. }
                | AgentEvent::CompactionStarted { .. }
                | AgentEvent::CompactionFinished { .. } => {}
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
            phase: ygg_ai::TransportPhase::ConnectOrHeaders,
            timeout: false,
            message: "connection refused".into(),
        });
        assert!(retryable_before_generation(&error));
        assert_eq!(provider_retry_limit(&error), 5);

        let failure = provider_failure(error, 5).to_string();
        assert!(failure.contains("Are you connected to the internet?"));
        assert!(failure.contains("connection refused"));
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
