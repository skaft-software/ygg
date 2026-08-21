//! The agent event surface and run-control messages.

use std::time::Duration;

use serde::Serialize;
use ygg_ai::{AssistantMessage, Cost, Media, ToolCallId, Usage};

use crate::agent::AgentError;
use crate::session::EntryId;
use crate::tool::{ToolError, ToolOutput, ToolProgress};

/// A bounded, host-owned live view of one delegated child.
///
/// The fields are observations from the child session, not extension-supplied
/// prose. Token buckets are disjoint; `reasoning_tokens` is a subset of
/// `output_tokens`. The opaque session reference is safe for a frontend to pass
/// back to the host for read-only inspection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DelegationTelemetryChild {
    /// Stable host-created child ID.
    pub child_id: String,
    /// Public task name selected by the orchestrator.
    pub task_name: String,
    /// Orchestrator profile, when the child was created by an extension.
    pub profile: Option<String>,
    /// Effective inherited model identifier.
    pub model: String,
    /// Current lifecycle state (`pending`, `running`, or a terminal state).
    pub state: String,
    /// Structured phase such as `queued`, `thinking`, or `using_tool`.
    pub phase: String,
    /// Current child tool, if one is executing.
    pub current_tool: Option<String>,
    /// Host-observed child tool starts. The spawn call itself is not included.
    pub tool_use_count: u64,
    /// Prompt tokens billed at the uncached input rate.
    pub input_tokens: u64,
    /// Prompt tokens read from cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to cache.
    pub cache_write_tokens: u64,
    /// Generated output tokens.
    pub output_tokens: u64,
    /// Reasoning tokens, a subset of output tokens.
    pub reasoning_tokens: u64,
    /// Provider-reported total tokens for the child session.
    pub total_tokens: u64,
    /// Exact priced cost when the inherited model has pricing.
    pub cost: Option<Cost>,
    /// Whole microdollar cost for compact frontend rendering.
    pub cost_microdollars: Option<u64>,
    /// Elapsed child wall time in milliseconds.
    pub elapsed_ms: u64,
    /// Bounded machine-readable terminal failure class.
    pub failure_class: Option<String>,
    /// Bounded authoritative terminal failure reason, when applicable.
    pub failure_reason: Option<String>,
    /// Opaque host-owned child transcript reference.
    pub session: Option<String>,
}

/// Complete monotonic telemetry for the current delegation team.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DelegationTelemetrySnapshot {
    /// Monotonic revision within the owning delegation manager.
    pub revision: u64,
    /// Host capture time in Unix milliseconds.
    pub captured_at_ms: u64,
    /// Current children in stable manager order.
    pub children: Vec<DelegationTelemetryChild>,
    /// Sum of the currently observed child whole-microdollar costs.
    pub total_cost_microdollars: Option<u64>,
    /// Bounded failure outside a child row, such as a rejected spawn.
    pub failure_reason: Option<String>,
    /// Bounded machine-readable class for `failure_reason`.
    pub failure_class: Option<String>,
}

/// Events emitted by a [`Run`](crate::Run).
///
/// All events are non-error: a successfully started run always emits exactly
/// one [`AgentEvent::RunFinished`] as its final event, even when it fails or
/// is aborted. Errors that occur *before* a run starts are returned by
/// [`Agent::prompt`](crate::Agent::prompt) instead. Tool failures are not run
/// failures. Transport/execution failures arrive as `Err` inside
/// [`AgentEvent::ToolFinished`]; a completed rich result may instead be
/// `Ok(output)` with [`ToolOutput::is_error`](crate::ToolOutput::is_error)
/// set, preserving structured media/details while still becoming an error
/// tool result for the model.
///
/// Streaming events are never persisted in the session; only completed
/// messages and tool results are.
#[derive(Debug)]
pub enum AgentEvent {
    /// A text or reasoning delta from the model. Raw tool-argument deltas are
    /// never exposed; assembled arguments arrive in [`AgentEvent::ToolStarted`].
    OutputDelta {
        /// Whether this is visible text or reasoning output.
        channel: OutputChannel,
        /// The delta text.
        text: String,
    },

    /// A complete generated media part from the current provider attempt.
    ///
    /// Like [`AgentEvent::OutputDelta`], this is provisional until the matching
    /// [`AgentEvent::TurnFinished`]. A later `ProviderRetry` or
    /// `CandidateRejected` discards it.
    OutputMedia {
        /// Canonical content-part index within the assistant message.
        index: usize,
        /// Complete generated image or audio payload.
        media: Media,
    },

    /// The current provider attempt ended transiently and the same logical
    /// model turn will be started again after a bounded backoff.
    ///
    /// Any [`AgentEvent::OutputDelta`] events emitted since the previous
    /// `TurnFinished` or `ProviderRetry` belong to the failed attempt and must
    /// be discarded. They were never committed to the session, and no tool
    /// represented by that partial stream has been executed.
    ProviderRetry {
        /// One-based retry attempt number.
        attempt: usize,
        /// Maximum retries allowed for this logical turn.
        max_attempts: usize,
        /// Backoff before opening the replacement provider stream.
        delay: Duration,
        /// Sanitized cause of the interrupted attempt.
        error: String,
    },

    /// Steering messages were appended together before the next model turn.
    ///
    /// The messages are emitted as one batch after the preceding assistant
    /// turn's tool calls have completed, so a caller can remove all of them
    /// from its pending-steering display at the same boundary the model sees
    /// them.
    SteeringDelivered {
        /// Single-line summaries of the delivered inputs, in FIFO order.
        messages: Vec<String>,
    },

    /// Follow-up messages were appended after the preceding assistant turn
    /// completed and before the next model turn starts.
    FollowUpDelivered {
        /// Single-line summaries of the delivered inputs, in FIFO order.
        messages: Vec<String>,
    },

    /// Autonomous context compaction has started and the next provider call
    /// is the tool-free summary request, not a normal model turn.
    CompactionStarted {
        /// Why the run loop requested compaction.
        reason: CompactionReason,
    },

    /// Autonomous context compaction ended. A successful summary and boundary
    /// have already been persisted when this event is observed. Failures are
    /// reported here before the run's terminal failure/abort event.
    CompactionFinished {
        /// Why the run loop requested compaction.
        reason: CompactionReason,
        /// Durable result, or a concise diagnostic when summarization failed.
        result: Result<CompactionInfo, String>,
    },

    /// A tool call was emitted by the model and host-side admission begins now.
    /// A matching [`ToolFinished`](Self::ToolFinished) is emitted even when
    /// argument validation or the effect broker denies execution.
    ToolStarted {
        /// The provider-assigned tool call ID.
        id: ToolCallId,
        /// The tool name.
        name: String,
        /// The parsed tool arguments (`null` when they failed to parse; the
        /// parse failure is then reported in the matching `ToolFinished`).
        args: serde_json::Value,
    },

    /// Live progress from a running tool.
    ///
    /// Emitted zero or more times between [`AgentEvent::ToolStarted`] and
    /// the matching [`AgentEvent::ToolFinished`]. Never persisted in the
    /// session. Delivered to registered [`EventObserver`](crate::EventObserver)s
    /// alongside the stream consumer.
    ToolProgress {
        /// The tool call this progress belongs to.
        id: ToolCallId,
        /// The progress update.
        progress: ToolProgress,
    },

    /// A tool call completed execution. Its result has already been appended
    /// to the session when this event is observed.
    ToolFinished {
        /// The tool call ID this result answers.
        id: ToolCallId,
        /// The execution outcome. `Err` and a marked rich `Ok` output both
        /// become error tool results.
        result: Result<ToolOutput, ToolError>,
    },

    /// A complete host-owned snapshot of delegated child activity.
    ///
    /// This is emitted on the owning root run stream. It is never persisted in
    /// the conversation and does not represent a model-visible tool result.
    DelegationUpdated {
        /// Monotonic child/session telemetry for the current delegation team.
        snapshot: DelegationTelemetrySnapshot,
    },

    /// A complete no-tool assistant turn was rejected by the terminal gate.
    /// Deltas emitted since the previous `TurnFinished` are provisional and
    /// must be discarded before the autonomous loop continues.
    CandidateRejected {
        /// Cumulative billable token usage, including terminal-gate calls.
        usage: Usage,
        /// Cost accrued during this run, including terminal-gate calls.
        run_cost_microdollars: u64,
    },

    /// The model finished a turn. The assembled assistant message has already
    /// been appended to the session.
    TurnFinished {
        /// The complete assistant message for the turn.
        message: AssistantMessage,
        /// Provider-reported reason this turn stopped.
        stop_reason: ygg_ai::StopReason,
        /// Provider-reported usage for this single request/response turn.
        ///
        /// Prompt buckets are disjoint; `reasoning_tokens` is a subset of
        /// `output_tokens`. `total_tokens` is therefore the actual context
        /// consumed by this turn, not a session or run total.
        turn_usage: Usage,
        /// Cumulative billable token usage across the run so far. This is for
        /// run accounting only and must not be used as context-window usage.
        usage: Usage,
        /// Cumulative session cost in microdollars (1/1,000,000 USD).
        /// `None` when pricing is not configured for the active model.
        session_cost_microdollars: Option<u64>,
        /// Cost accrued during this run only, in microdollars.
        run_cost_microdollars: u64,
    },

    /// The run finished. Always the last event of a started run.
    RunFinished {
        /// The session head entry after the run.
        head: EntryId,
        /// How the run ended.
        reason: FinishReason,
    },
}

/// Reason an autonomous run compacted its active context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    /// The configured proactive context threshold was reached.
    Threshold,
    /// The estimated request exceeded local capacity or the provider rejected
    /// it as exceeding the model context window.
    Overflow,
}

/// Durable result of one autonomous compaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionInfo {
    /// Provider-independent local summary or native Responses checkpoint.
    pub kind: CompactionKind,
    /// Summary injected at the front of reconstructed provider context.
    /// Empty for native Responses compaction, whose opaque output is the base.
    pub summary: String,
    /// Oldest entry retained at full fidelity.
    /// For native Responses compaction this is the covered-through head.
    pub first_kept: EntryId,
}

/// Durable compaction representation selected by the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionKind {
    /// A local canonical summary and retained full-fidelity tail.
    Local,
    /// A route-affine opaque Responses checkpoint.
    NativeResponses {
        /// Session entry containing the opaque compact output.
        checkpoint: EntryId,
        /// Active-branch head covered by the compact request.
        covered_through: EntryId,
    },
}

/// Distinguishes normal text from reasoning output in [`AgentEvent::OutputDelta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputChannel {
    /// Visible assistant text.
    Text,
    /// Model reasoning / thinking text.
    Reasoning,
}

/// Terminal outcome of a run, carried by [`AgentEvent::RunFinished`].
#[derive(Debug)]
pub enum FinishReason {
    /// The model completed without further tool calls (and no follow-up was queued).
    Completed,
    /// The run was aborted via [`RunControl::abort`](crate::RunControl::abort).
    Aborted,
    /// The run failed. This is the only asynchronous error channel.
    Failed(AgentError),
    /// The maximum turn count was reached.
    MaxTurns,
}

/// How queued user messages are delivered at an agent turn boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueDeliveryMode {
    /// Deliver one queued message per eligible boundary.
    #[default]
    OneAtATime,
    /// Deliver every message currently queued at the same boundary.
    All,
}

/// Control messages accepted by an active run via [`RunControl`](crate::RunControl).
#[derive(Debug)]
pub enum Control {
    /// Inject input into the conversation at the next model-turn boundary of
    /// the active run.
    Steer(crate::input::UserInput),
    /// Queue input for after the current run settles (the model completes a
    /// turn without tool calls). The run then continues with this input
    /// instead of finishing.
    FollowUp(crate::input::UserInput),
    /// Change how pending steering messages are delivered.
    SetSteeringMode(QueueDeliveryMode),
    /// Change how pending follow-up messages are delivered.
    SetFollowUpMode(QueueDeliveryMode),
    /// Abort the run at the next safe boundary.
    Abort,
}
