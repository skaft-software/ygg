//! Pollable, backend-owned context telemetry for an active agent run.

use std::sync::Mutex;

use ygg_ai::{StreamEvent, Usage};

use crate::{CompactionReason, FinishReason};

/// Reconciled model-visible context accounting for one request boundary.
///
/// Category values are deterministic structural estimates. `other_tokens`
/// contains any provider-authoritative amount that cannot be attributed to a
/// structural category without inventing precision. The category fields always
/// sum exactly to `total_tokens`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// Runtime/provider request framing and tool definitions.
    pub system_tokens: u64,
    /// Host-supplied instruction text.
    pub instruction_tokens: u64,
    /// User and assistant conversation content.
    pub conversation_tokens: u64,
    /// Tool calls and tool results.
    pub tool_result_tokens: u64,
    /// Multimodal attachment content.
    pub attachment_tokens: u64,
    /// Local compaction handoff summaries.
    pub compaction_summary_tokens: u64,
    /// Reconciled tokens that cannot be attributed more precisely.
    pub other_tokens: u64,
    /// Exact sum of all category fields and conservative next-request total.
    pub total_tokens: u64,
    /// Serializer-derived request estimate before provider reconciliation.
    pub structural_tokens: u64,
    /// Latest applicable provider measurement, when available.
    pub provider_tokens: Option<u64>,
    /// Model context-window limit.
    pub context_limit: u64,
}

impl ContextBreakdown {
    /// Checked sum of all public category fields.
    pub fn categorized_tokens(&self) -> u64 {
        self.system_tokens
            .saturating_add(self.instruction_tokens)
            .saturating_add(self.conversation_tokens)
            .saturating_add(self.tool_result_tokens)
            .saturating_add(self.attachment_tokens)
            .saturating_add(self.compaction_summary_tokens)
            .saturating_add(self.other_tokens)
    }
}

/// Coarse active-run lifecycle derived from authoritative execution boundaries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunPhase {
    /// Preparing the next provider request.
    #[default]
    Preparing,
    /// Receiving one provider response.
    Responding,
    /// Retrying a discarded provider attempt.
    Retrying,
    /// Compacting model-visible context.
    Compacting,
    /// Executing a model-requested tool.
    ExecutingTool,
    /// The run reached a terminal boundary.
    Finished,
}

/// Sanitized terminal state for lifecycle polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTerminalState {
    /// The run completed normally.
    Completed,
    /// The caller aborted the run.
    Aborted,
    /// The run failed.
    Failed,
    /// The configured model-turn limit was reached.
    MaxTurns,
    /// The run stream was dropped before its terminal event.
    Dropped,
}

/// Pollable state for a compaction in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveContextCompaction {
    /// Monotonic identity scoped to this run.
    pub id: u64,
    /// Why compaction was requested.
    pub reason: CompactionReason,
    /// Exact reconciled context observed before compaction.
    pub before: ContextBreakdown,
}

/// Pollable state for the most recently finished compaction attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishedContextCompaction {
    /// Monotonic identity scoped to this run.
    pub id: u64,
    /// Why compaction was requested.
    pub reason: CompactionReason,
    /// Exact reconciled context observed before compaction.
    pub before: ContextBreakdown,
    /// Reconciled context after the attempt. Failed attempts retain `before`.
    pub after: ContextBreakdown,
    /// Whether the compaction operation committed successfully.
    pub succeeded: bool,
}

/// Incremental context telemetry for one active [`Run`](crate::Run).
///
/// A snapshot changes at provider response boundaries, every text/reasoning or
/// tool-argument delta, every structured tool boundary, every provider usage
/// report, every tool-execution boundary, and every compaction boundary. It is
/// independent of any UI and can be polled through
/// [`Run::context_snapshot`](crate::Run::context_snapshot).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextSnapshot {
    /// Monotonic change counter for inexpensive polling.
    pub revision: u64,
    /// Current execution phase.
    pub phase: RunPhase,
    /// Sanitized terminal state after the run finishes or is dropped.
    pub terminal_state: Option<RunTerminalState>,
    /// Current reconciled model-visible context.
    pub context: ContextBreakdown,
    /// Active compaction, if any.
    pub active_compaction: Option<ActiveContextCompaction>,
    /// Most recently finished compaction attempt.
    pub last_compaction: Option<FinishedContextCompaction>,
    /// Number of compaction attempts opened.
    pub compactions_started: u64,
    /// Number of successful compactions.
    pub compactions_completed: u64,
    /// Number of failed or cancelled compactions.
    pub compactions_failed: u64,
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

    pub(crate) fn observe_context(&self, context: ContextBreakdown) {
        debug_assert_eq!(context.categorized_tokens(), context.total_tokens);
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.context != context {
            state.context = context;
            state.revision = state.revision.saturating_add(1);
        }
    }

    pub(crate) fn compaction_started(&self, reason: CompactionReason) -> u64 {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = state.compactions_started.saturating_add(1);
        state.compactions_started = id;
        state.phase = RunPhase::Compacting;
        state.active_compaction = Some(ActiveContextCompaction {
            id,
            reason,
            before: state.context.clone(),
        });
        state.revision = state.revision.saturating_add(1);
        id
    }

    pub(crate) fn compaction_finished(
        &self,
        id: u64,
        after: Option<ContextBreakdown>,
        succeeded: bool,
    ) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = state.active_compaction.take() else {
            return;
        };
        if active.id != id {
            state.active_compaction = Some(active);
            return;
        }
        let after = if succeeded {
            after.unwrap_or_else(|| active.before.clone())
        } else {
            active.before.clone()
        };
        if succeeded {
            state.compactions_completed = state.compactions_completed.saturating_add(1);
            state.context = after.clone();
        } else {
            state.compactions_failed = state.compactions_failed.saturating_add(1);
            state.context = active.before.clone();
        }
        state.last_compaction = Some(FinishedContextCompaction {
            id,
            reason: active.reason,
            before: active.before,
            after,
            succeeded,
        });
        state.phase = RunPhase::Preparing;
        state.revision = state.revision.saturating_add(1);
    }

    pub(crate) fn observe_stream(&self, event: &StreamEvent) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = true;
        match event {
            StreamEvent::Started { response_id } => {
                state.phase = RunPhase::Responding;
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
                state.phase = RunPhase::Preparing;
                state.responses_finished = state.responses_finished.saturating_add(1);
                state.response_active = false;
                state.response_usage = response.usage;
                let usage = state.response_usage;
                add_usage(&mut state.run_usage, &usage);
            }
            StreamEvent::TextStart { .. }
            | StreamEvent::TextEnd { .. }
            | StreamEvent::ReasoningStart { .. }
            | StreamEvent::ReasoningEnd { .. }
            | StreamEvent::MediaCompleted { .. }
            | StreamEvent::ProviderLifecycle(_) => changed = false,
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
        state.phase = RunPhase::Retrying;
        if state.response_active {
            state.responses_discarded = state.responses_discarded.saturating_add(1);
        }
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
        state.phase = RunPhase::ExecutingTool;
        state.tool_executions_started = state.tool_executions_started.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
    }

    pub(crate) fn tool_finished(&self) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.phase = RunPhase::Preparing;
        state.tool_executions_finished = state.tool_executions_finished.saturating_add(1);
        state.revision = state.revision.saturating_add(1);
    }

    pub(crate) fn run_finished(&self, reason: &FinishReason) {
        let terminal_state = match reason {
            FinishReason::Completed => RunTerminalState::Completed,
            FinishReason::Aborted => RunTerminalState::Aborted,
            FinishReason::Failed(_) => RunTerminalState::Failed,
            FinishReason::MaxTurns => RunTerminalState::MaxTurns,
        };
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.phase = RunPhase::Finished;
        state.terminal_state = Some(terminal_state);
        if state.response_active {
            state.responses_discarded = state.responses_discarded.saturating_add(1);
        }
        state.response_active = false;
        state.revision = state.revision.saturating_add(1);
    }

    pub(crate) fn run_dropped(&self) {
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal_state.is_none() {
            state.phase = RunPhase::Finished;
            state.terminal_state = Some(RunTerminalState::Dropped);
            if state.response_active {
                state.responses_discarded = state.responses_discarded.saturating_add(1);
            }
            state.response_active = false;
            if let Some(active) = state.active_compaction.take() {
                state.compactions_failed = state.compactions_failed.saturating_add(1);
                state.last_compaction = Some(FinishedContextCompaction {
                    id: active.id,
                    reason: active.reason,
                    before: active.before.clone(),
                    after: active.before,
                    succeeded: false,
                });
            }
            state.revision = state.revision.saturating_add(1);
        }
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

    fn breakdown(total_tokens: u64) -> ContextBreakdown {
        ContextBreakdown {
            conversation_tokens: total_tokens,
            total_tokens,
            structural_tokens: total_tokens,
            context_limit: 1_000,
            ..ContextBreakdown::default()
        }
    }

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
        assert_eq!(snapshot.phase, RunPhase::Responding);
        assert_eq!(snapshot.response_text_bytes, 5);
        assert_eq!(snapshot.response_tool_argument_bytes, 2);
        assert_eq!(snapshot.tool_calls_started, 1);
        assert_eq!(snapshot.response_usage.total_tokens, 9);

        tracker.provider_retry();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.phase, RunPhase::Retrying);
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
            responses_output: None,
            diagnostics: Vec::new(),
        }));
        tracker.tool_started();
        tracker.tool_finished();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.phase, RunPhase::Preparing);
        assert_eq!(snapshot.responses_started, 2);
        assert_eq!(snapshot.responses_finished, 1);
        assert_eq!(snapshot.run_usage.total_tokens, 4);
        assert_eq!(snapshot.tool_executions_started, 1);
        assert_eq!(snapshot.tool_executions_finished, 1);
    }

    #[test]
    fn lifecycle_feedback_does_not_change_context_state() {
        let tracker = ContextTracker::default();
        tracker.observe_stream(&StreamEvent::Started {
            response_id: Some("r1".into()),
        });
        let before = tracker.snapshot();

        tracker.observe_stream(&StreamEvent::ProviderLifecycle(ygg_ai::ProviderLifecycle {
            state: ygg_ai::ProviderLifecycleState::Loading,
            detail: Some("warming".into()),
        }));

        let after = tracker.snapshot();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.responses_started, before.responses_started);
        assert_eq!(after.response_usage, before.response_usage);
    }

    #[test]
    fn retries_and_terminal_states_reconcile_every_started_response() {
        let tracker = ContextTracker::default();
        tracker.provider_retry();
        let before_start = tracker.snapshot();
        assert_eq!(before_start.responses_started, 0);
        assert_eq!(before_start.responses_discarded, 0);
        assert!(!before_start.response_active);

        tracker.observe_stream(&StreamEvent::Started {
            response_id: Some("r1".into()),
        });
        tracker.run_finished(&FinishReason::Failed(crate::agent::AgentError::Cancelled));
        let failed = tracker.snapshot();
        assert_eq!(failed.responses_started, 1);
        assert_eq!(failed.responses_finished, 0);
        assert_eq!(failed.responses_discarded, 1);
        assert!(!failed.response_active);
        assert_eq!(failed.phase, RunPhase::Finished);
    }

    #[test]
    fn dropping_during_compaction_records_an_explicit_failed_attempt() {
        let tracker = ContextTracker::default();
        tracker.observe_context(breakdown(80));
        let id = tracker.compaction_started(CompactionReason::Threshold);

        tracker.run_dropped();

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.phase, RunPhase::Finished);
        assert_eq!(snapshot.terminal_state, Some(RunTerminalState::Dropped));
        assert!(snapshot.active_compaction.is_none());
        assert_eq!(snapshot.compactions_started, 1);
        assert_eq!(snapshot.compactions_completed, 0);
        assert_eq!(snapshot.compactions_failed, 1);
        let finished = snapshot.last_compaction.unwrap();
        assert_eq!(finished.id, id);
        assert_eq!(finished.before, finished.after);
        assert!(!finished.succeeded);
    }

    #[test]
    fn compaction_snapshots_reconcile_success_and_failure() {
        let tracker = ContextTracker::default();
        tracker.observe_context(breakdown(80));
        let first = tracker.compaction_started(CompactionReason::Threshold);
        let active = tracker.snapshot();
        assert_eq!(active.phase, RunPhase::Compacting);
        assert_eq!(
            active
                .active_compaction
                .as_ref()
                .unwrap()
                .before
                .total_tokens,
            80
        );

        tracker.compaction_finished(first, Some(breakdown(30)), true);
        let completed = tracker.snapshot();
        assert_eq!(completed.context.total_tokens, 30);
        assert_eq!(completed.compactions_completed, 1);
        assert!(completed.last_compaction.as_ref().unwrap().succeeded);

        let second = tracker.compaction_started(CompactionReason::Overflow);
        tracker.compaction_finished(second, Some(breakdown(10)), false);
        let failed = tracker.snapshot();
        assert_eq!(failed.context.total_tokens, 30);
        assert_eq!(failed.compactions_failed, 1);
        assert!(!failed.last_compaction.as_ref().unwrap().succeeded);
        assert_eq!(
            failed.last_compaction.as_ref().unwrap().after.total_tokens,
            30
        );
    }
}
