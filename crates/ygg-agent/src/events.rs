//! The agent event surface and run-control messages.

use std::time::Duration;

use ygg_ai::{AssistantMessage, ToolCallId, Usage};

use crate::agent::AgentError;
use crate::session::EntryId;
use crate::tool::{ToolError, ToolOutput, ToolProgress};

/// Events emitted by a [`Run`](crate::Run).
///
/// All events are non-error: a successfully started run always emits exactly
/// one [`AgentEvent::RunFinished`] as its final event, even when it fails or
/// is aborted. Errors that occur *before* a run starts are returned by
/// [`Agent::prompt`](crate::Agent::prompt) instead. Tool failures are not run
/// failures — they arrive as `Err` inside [`AgentEvent::ToolFinished`] and are
/// returned to the model as error tool results.
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

    /// A tool call was emitted by the model and its execution begins now.
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
        /// The execution outcome; `Err` becomes an error tool result.
        result: Result<ToolOutput, ToolError>,
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
    /// Summary injected at the front of reconstructed provider context.
    pub summary: String,
    /// Oldest entry retained at full fidelity.
    pub first_kept: EntryId,
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
    /// Abort the run at the next safe boundary.
    Abort,
}
