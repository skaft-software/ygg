//! Pollable, backend-owned context telemetry for an active agent run.

use std::sync::Mutex;

use ygg_ai::{StreamEvent, Usage};

/// Incremental context telemetry for one active [`Run`](crate::Run).
///
/// A snapshot changes at provider response boundaries, every text/reasoning or
/// tool-argument delta, every structured tool boundary, every provider usage
/// report, and every tool-execution boundary. It is independent of any UI and
/// can be polled through [`Run::context_snapshot`](crate::Run::context_snapshot).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextSnapshot {
    /// Monotonic change counter for inexpensive polling.
    pub revision: u64,
    /// Number of provider response attempts opened, including retried attempts.
    pub responses_started: u64,
    /// Number of provider responses assembled successfully.
    pub responses_finished: u64,
    /// Number of provider attempts discarded before completion.
    pub responses_discarded: u64,
    /// Provider response identifier for the current or most recent attempt.
    pub response_id: Option<String>,
    /// Whether a provider response is currently being assembled.
    pub response_active: bool,
    /// Visible text bytes assembled for the current or most recent response.
    pub response_text_bytes: u64,
    /// Reasoning bytes assembled for the current or most recent response.
    pub response_reasoning_bytes: u64,
    /// Tool-argument bytes assembled for the current or most recent response.
    pub response_tool_argument_bytes: u64,
    /// Structured tool calls whose generation began in this run.
    pub tool_calls_started: u64,
    /// Structured tool calls whose generation reached a complete boundary.
    pub tool_calls_finished: u64,
    /// Tool executions started in this run.
    pub tool_executions_started: u64,
    /// Tool executions completed (successfully or with an error) in this run.
    pub tool_executions_finished: u64,
    /// Most recent intermediate or final usage report for the active response.
    pub response_usage: Usage,
    /// Usage accumulated across successfully completed responses in this run.
    pub run_usage: Usage,
}

#[derive(Default)]
pub(crate) struct ContextTracker {
    snapshot: Mutex<ContextSnapshot>,
}

impl ContextTracker {
    pub(crate) fn snapshot(&self) -> ContextSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn observe_stream(&self, event: &StreamEvent) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = true;
        match event {
            StreamEvent::Started { response_id } => {
                state.responses_started = state.responses_started.saturating_add(1);
                state.response_id.clone_from(response_id);
                state.response_active = true;
                state.response_text_bytes = 0;
                state.response_reasoning_bytes = 0;
                state.response_tool_argument_bytes = 0;
                state.response_usage = Usage::default();
            }
            StreamEvent::TextDelta { delta, .. } => {
                state.response_text_bytes =
                    state.response_text_bytes.saturating_add(delta.len() as u64);
            }
            StreamEvent::ReasoningDelta { delta, .. } => {
                state.response_reasoning_bytes = state
                    .response_reasoning_bytes
                    .saturating_add(delta.len() as u64);
            }
            StreamEvent::ToolCallStart { .. } => {
                state.tool_calls_started = state.tool_calls_started.saturating_add(1);
            }
            StreamEvent::ToolCallArgsDelta { delta, .. } => {
                state.response_tool_argument_bytes = state
                    .response_tool_argument_bytes
                    .saturating_add(delta.len() as u64);
            }
            StreamEvent::ToolCallEnd { .. } => {
                state.tool_calls_finished = state.tool_calls_finished.saturating_add(1);
            }
            StreamEvent::Usage(usage) => state.response_usage = *usage,
            StreamEvent::Finished(response) => {
                state.responses_finished = state.responses_finished.saturating_add(1);
                state.response_active = false;
                state.response_usage = response.usage;
                add_usage(&mut state.run_usage, &response.usage);
            }
            StreamEvent::TextStart { .. }
            | StreamEvent::TextEnd { .. }
            | StreamEvent::ReasoningStart { .. }
            | StreamEvent::ReasoningEnd { .. }
            | StreamEvent::MediaCompleted { .. } => changed = false,
        }
        if changed {
            state.revision = state.revision.saturating_add(1);
        }
    }

    pub(crate) fn provider_retry(&self) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.responses_discarded = state.responses_discarded.saturating_add(1);
        state.response_active = false;
        state.response_id = None;
        state.response_text_bytes = 0;
        state.response_reasoning_bytes = 0;
        state.response_tool_argument_bytes = 0;
        state.response_usage = Usage::default();
        state.revision = state.revision.saturating_add(1);
    }

    pub(crate) fn tool_started(&self) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tool_executions_started = state.tool_executions_started.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
    }

    pub(crate) fn tool_finished(&self) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tool_executions_finished = state.tool_executions_finished.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
    }
}

fn add_usage(total: &mut Usage, next: &Usage) {
    total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(next.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(next.cache_write_tokens);
    total.cache_write_1h_tokens = total
        .cache_write_1h_tokens
        .saturating_add(next.cache_write_1h_tokens);
    total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(next.reasoning_tokens);
    total.total_tokens = total.total_tokens.saturating_add(next.total_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_ai::{AssistantMessage, ModelId, Protocol, Response, StopReason, ToolCallId};

    #[test]
    fn tracks_deltas_tool_boundaries_usage_and_retry_resets() {
        let tracker = ContextTracker::default();
        tracker.observe_stream(&StreamEvent::Started {
            response_id: Some("r1".into()),
        });
        tracker.observe_stream(&StreamEvent::TextDelta {
            index: 0,
            delta: "hello".into(),
        });
        tracker.observe_stream(&StreamEvent::ToolCallStart {
            index: 1,
            id: ToolCallId("c1".into()),
            name: "read".into(),
        });
        tracker.observe_stream(&StreamEvent::ToolCallArgsDelta {
            index: 1,
            delta: "{}".into(),
        });
        tracker.observe_stream(&StreamEvent::Usage(Usage {
            input_tokens: 7,
            output_tokens: 2,
            total_tokens: 9,
            ..Usage::default()
        }));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.response_text_bytes, 5);
        assert_eq!(snapshot.response_tool_argument_bytes, 2);
        assert_eq!(snapshot.tool_calls_started, 1);
        assert_eq!(snapshot.response_usage.total_tokens, 9);

        tracker.provider_retry();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.responses_discarded, 1);
        assert_eq!(snapshot.response_text_bytes, 0);

        tracker.observe_stream(&StreamEvent::Started {
            response_id: Some("r2".into()),
        });
        tracker.observe_stream(&StreamEvent::Finished(Response {
            message: AssistantMessage {
                content: Vec::new(),
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            },
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 1,
                total_tokens: 4,
                ..Usage::default()
            },
            cost: None,
            response_id: Some("r2".into()),
            diagnostics: Vec::new(),
        }));
        tracker.tool_started();
        tracker.tool_finished();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.responses_started, 2);
        assert_eq!(snapshot.responses_finished, 1);
        assert_eq!(snapshot.run_usage.total_tokens, 4);
        assert_eq!(snapshot.tool_executions_started, 1);
        assert_eq!(snapshot.tool_executions_finished, 1);
    }
}
