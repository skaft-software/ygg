//! The agent event surface and run-control messages.

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

    /// Steering messages were appended together before the next model turn.
    ///
    /// The messages are emitted as one batch after the preceding assistant
    /// turn's tool calls have completed, so a caller can remove all of them
    /// from its pending-steering display at the same boundary the model sees
    /// them.
    SteeringDelivered {
        /// The ordered steering messages injected into the conversation.
        messages: Vec<String>,
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

    /// The model finished a turn. The assembled assistant message has already
    /// been appended to the session.
    TurnFinished {
        /// The complete assistant message for the turn.
        message: AssistantMessage,
        /// Cumulative token usage across the run so far.
        usage: Usage,
    },

    /// The run finished. Always the last event of a started run.
    RunFinished {
        /// The session head entry after the run.
        head: EntryId,
        /// How the run ended.
        reason: FinishReason,
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

/// Control messages accepted by an active run via [`RunControl`](crate::RunControl).
#[derive(Debug)]
pub enum Control {
    /// Inject text into the conversation at the next model-turn boundary of
    /// the active run.
    Steer(String),
    /// Queue text for after the current run settles (the model completes a
    /// turn without tool calls). The run then continues with this input
    /// instead of finishing.
    FollowUp(String),
    /// Abort the run at the next safe boundary.
    Abort,
}
