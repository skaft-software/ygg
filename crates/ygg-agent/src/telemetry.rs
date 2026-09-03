//! Optional, bounded JSONL telemetry for measuring agent work.
//!
//! Telemetry is deliberately outside the normal session format and is disabled
//! unless a caller explicitly installs [`TelemetryObserver`].  It records
//! operational facts, not prompts, tool arguments, or tool output.  Hashes are
//! included where correlation is useful without making a debug run a secret
//! exfiltration channel.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use ygg_ai::{Model, Protocol, Usage};

use crate::events::{AgentEvent, CompactionKind, CompactionReason, FinishReason};
use crate::extension::EventObserver;
use crate::input::{InputPart, UserInput};
use crate::tool::{ToolError, ToolOutput};

/// The stable schema identifier written to every telemetry file.
pub const TELEMETRY_SCHEMA: &str = "ygg.telemetry.v1";
const MAX_ERROR_BYTES: usize = 512;
const MAX_RECENT_CALLS: usize = 16;

/// A file-backed observer for optional agent performance and reliability data.
///
/// The observer writes one bounded JSON object per line.  It never writes raw
/// user input, tool arguments, tool results, credentials, or provider payloads.
/// File I/O is synchronous by design but happens only for coarse lifecycle
/// boundaries; streaming deltas are aggregated in memory and are not written.
#[derive(Clone)]
pub struct TelemetryObserver {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    file: File,
    path: PathBuf,
    sequence: u64,
    owners: HashMap<String, RunState>,
    write_failed: bool,
}

struct RunState {
    run_id: String,
    model: ModelIdentity,
    started: Instant,
    turn_index: u64,
    step_index: u64,
    request_attempts: u64,
    requests_finished: u64,
    requests_discarded: u64,
    tool_calls: u64,
    tool_executions: u64,
    repeated_tool_calls: u64,
    useful_state_changes: u64,
    no_progress_streak: u64,
    awaiting_retry: bool,
    current_attempt: Option<AttemptState>,
    active_tools: HashMap<String, ActiveTool>,
    recent_calls: VecDeque<CallFingerprint>,
    pending_calls: Vec<CallFingerprint>,
    last_usage: Usage,
}

struct AttemptState {
    logical_turn: u64,
    attempt: u64,
    step_index: u64,
    started: Instant,
    ttft: Option<Duration>,
    text_bytes: u64,
    reasoning_bytes: u64,
}

struct ActiveTool {
    name: String,
    step_index: u64,
    started: Instant,
    args_bytes: u64,
    args_sha256: String,
    repeated_recently: u64,
}

#[derive(Clone)]
struct ModelIdentity {
    endpoint: String,
    model: String,
    protocol: &'static str,
    context_limit: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct CallFingerprint {
    name: String,
    args_sha256: String,
}

impl TelemetryObserver {
    /// Opens or creates an append-only telemetry file and writes its header.
    ///
    /// The parent directory is created when necessary.  The file is owner-only
    /// on Unix because even hashed task identities and operational timings may
    /// be sensitive in aggregate.
    pub fn new(path: impl Into<PathBuf>, version: impl Into<String>) -> io::Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() || path.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "telemetry path must identify a file",
            ));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let observer = Self {
            inner: Arc::new(Mutex::new(Inner {
                file,
                path,
                sequence: 0,
                owners: HashMap::new(),
                write_failed: false,
            })),
        };
        let mut fields = Map::new();
        fields.insert("version".into(), Value::String(version.into()));
        fields.insert("pid".into(), Value::Number(std::process::id().into()));
        observer.emit(None, None, "header", fields);
        Ok(observer)
    }

    /// Returns the file selected for this observer.
    pub fn path(&self) -> PathBuf {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .path
            .clone()
    }

    fn emit(
        &self,
        owner: Option<&str>,
        run_id: Option<&str>,
        record: &str,
        fields: Map<String, Value>,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.emit(owner, run_id, record, fields);
    }
}

impl EventObserver for TelemetryObserver {
    fn on_event(&self, _event: &AgentEvent) {
        // The owner-scoped callback is authoritative for telemetry. This no-op
        // keeps the observer compatible with unscoped library dispatch.
    }

    fn on_run_started_for_owner(
        &self,
        run_id: &str,
        input: &UserInput,
        model: &Model,
        resource_owner: &str,
    ) {
        let identity = ModelIdentity {
            endpoint: model.endpoint.id.0.clone(),
            model: model.spec.id.0.clone(),
            protocol: protocol_label(model.spec.protocol),
            context_limit: model.spec.limits.context_window,
        };
        let input_summary = input.text_summary();
        let mut state = RunState {
            run_id: run_id.to_owned(),
            model: identity.clone(),
            started: Instant::now(),
            turn_index: 0,
            step_index: 0,
            request_attempts: 0,
            requests_finished: 0,
            requests_discarded: 0,
            tool_calls: 0,
            tool_executions: 0,
            repeated_tool_calls: 0,
            useful_state_changes: 0,
            no_progress_streak: 0,
            awaiting_retry: false,
            current_attempt: None,
            active_tools: HashMap::new(),
            recent_calls: VecDeque::with_capacity(MAX_RECENT_CALLS),
            pending_calls: Vec::with_capacity(4),
            last_usage: Usage::default(),
        };

        let mut fields = Map::new();
        fields.insert(
            "input_sha256".into(),
            Value::String(sha256_hex(input_summary.as_bytes())),
        );
        fields.insert(
            "input_text_bytes".into(),
            Value::Number(input_text_bytes(input).into()),
        );
        fields.insert(
            "input_parts".into(),
            Value::Number((input.parts.len() as u64).into()),
        );
        fields.insert(
            "media_parts".into(),
            Value::Number(
                (input
                    .parts
                    .iter()
                    .filter(|part| matches!(part, InputPart::Media(_)))
                    .count() as u64)
                    .into(),
            ),
        );
        fields.insert("endpoint".into(), Value::String(identity.endpoint.clone()));
        fields.insert("model".into(), Value::String(identity.model.clone()));
        fields.insert("protocol".into(), Value::String(identity.protocol.into()));
        fields.insert(
            "context_limit_tokens".into(),
            Value::Number(identity.context_limit.into()),
        );
        fields.insert(
            "usage_semantics".into(),
            Value::String(
                "input_tokens is uncached; cache_read_tokens and cache_write_tokens are disjoint additions"
                    .into(),
            ),
        );

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A second start for the same owner means the caller rebuilt or
        // restarted an active run. Retain the new run as authoritative and
        // make the replacement visible rather than merging counters.
        state.started = Instant::now();
        inner.owners.insert(resource_owner.to_owned(), state);
        inner.emit(Some(resource_owner), Some(run_id), "run_started", fields);
    }

    fn on_event_for_owner(&self, event: &AgentEvent, resource_owner: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = inner.owners.get_mut(resource_owner) else {
            // Events can be observed from a library caller that did not use the
            // optional run-start hook in an older integration. Preserve the
            // event rather than losing the evidence, with an explicit marker.
            let mut fields = Map::new();
            fields.insert("missing_run_start".into(), Value::Bool(true));
            inner.emit(Some(resource_owner), None, event_label(event), fields);
            return;
        };
        let run_id = state.run_id.clone();

        match event {
            AgentEvent::OutputDelta { channel, text } => {
                let Some(attempt) = state.current_attempt.as_mut() else {
                    return;
                };
                if attempt.ttft.is_none() {
                    attempt.ttft = Some(attempt.started.elapsed());
                }
                match channel {
                    crate::events::OutputChannel::Text => {
                        attempt.text_bytes = attempt.text_bytes.saturating_add(text.len() as u64)
                    }
                    crate::events::OutputChannel::Reasoning => {
                        attempt.reasoning_bytes =
                            attempt.reasoning_bytes.saturating_add(text.len() as u64)
                    }
                }
            }
            AgentEvent::TurnStarted => {
                // ToolStarted events for one assistant response arrive after
                // that response's TurnFinished event. Move the previous batch
                // into the comparison window only when the next model turn
                // begins, so parallel identical calls are not false loops.
                let previous_calls = std::mem::take(&mut state.pending_calls);
                for call in previous_calls {
                    state.recent_calls.push_back(call);
                    while state.recent_calls.len() > MAX_RECENT_CALLS {
                        state.recent_calls.pop_front();
                    }
                }
                if state.current_attempt.is_some() {
                    // This is only expected after a retry event. Close the
                    // stale attempt as discarded instead of corrupting the
                    // request timing chain.
                    state.requests_discarded = state.requests_discarded.saturating_add(1);
                }
                if !state.awaiting_retry {
                    state.turn_index = state.turn_index.saturating_add(1);
                }
                state.awaiting_retry = false;
                state.request_attempts = state.request_attempts.saturating_add(1);
                state.step_index = state.step_index.saturating_add(1);
                let attempt = state.request_attempts;
                let logical_turn = state.turn_index;
                state.current_attempt = Some(AttemptState {
                    logical_turn,
                    attempt,
                    step_index: state.step_index,
                    started: Instant::now(),
                    ttft: None,
                    text_bytes: 0,
                    reasoning_bytes: 0,
                });
                let mut fields = Map::new();
                fields.insert("logical_turn".into(), Value::Number(logical_turn.into()));
                fields.insert("attempt".into(), Value::Number(attempt.into()));
                fields.insert("step".into(), Value::Number(state.step_index.into()));
                fields.insert("phase".into(), Value::String("provider_request".into()));
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "model_request_started",
                    fields,
                );
            }
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                delay,
                error,
            } => {
                state.awaiting_retry = true;
                state.requests_discarded = state.requests_discarded.saturating_add(1);
                let timing = state.current_attempt.take().map(|attempt_state| {
                    (
                        attempt_state.started.elapsed().as_millis() as u64,
                        attempt_state.ttft.map(duration_ms),
                    )
                });
                let mut fields = Map::new();
                fields.insert(
                    "retry_attempt".into(),
                    Value::Number((*attempt as u64).into()),
                );
                fields.insert(
                    "max_attempts".into(),
                    Value::Number((*max_attempts as u64).into()),
                );
                fields.insert(
                    "delay_ms".into(),
                    Value::Number((delay.as_millis().min(u64::MAX as u128) as u64).into()),
                );
                fields.insert("error".into(), Value::String(bounded_text(error)));
                if let Some((elapsed, ttft)) = timing {
                    fields.insert("elapsed_ms".into(), Value::Number(elapsed.into()));
                    if let Some(ttft) = ttft {
                        fields.insert("ttft_ms".into(), Value::Number(ttft.into()));
                    }
                }
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "provider_retry",
                    fields,
                );
            }
            AgentEvent::ToolStarted { id, name, args } => {
                state.tool_calls = state.tool_calls.saturating_add(1);
                state.step_index = state.step_index.saturating_add(1);
                let args_bytes = serde_json::to_vec(args).map_or(0, |bytes| bytes.len() as u64);
                let args_sha256 = serde_json::to_vec(args)
                    .map(|bytes| sha256_hex(&bytes))
                    .unwrap_or_else(|_| sha256_hex(b"<invalid-json>"));
                let repeated_recently = state
                    .recent_calls
                    .iter()
                    .filter(|previous| {
                        previous.name == *name && previous.args_sha256 == args_sha256
                    })
                    .count() as u64;
                if repeated_recently > 0 {
                    state.repeated_tool_calls = state.repeated_tool_calls.saturating_add(1);
                }
                state.pending_calls.push(CallFingerprint {
                    name: name.clone(),
                    args_sha256: args_sha256.clone(),
                });
                state.active_tools.insert(
                    id.0.clone(),
                    ActiveTool {
                        name: name.clone(),
                        step_index: state.step_index,
                        started: Instant::now(),
                        args_bytes,
                        args_sha256: args_sha256.clone(),
                        repeated_recently,
                    },
                );
                let mut fields = Map::new();
                fields.insert(
                    "tool_call_id_sha256".into(),
                    Value::String(sha256_hex(id.0.as_bytes())),
                );
                fields.insert("tool".into(), Value::String(name.clone()));
                fields.insert("step".into(), Value::Number(state.step_index.into()));
                fields.insert("args_bytes".into(), Value::Number(args_bytes.into()));
                fields.insert("args_sha256".into(), Value::String(args_sha256));
                fields.insert(
                    "repeated_recently".into(),
                    Value::Number(repeated_recently.into()),
                );
                fields.insert(
                    "same_call_recently".into(),
                    Value::Bool(repeated_recently > 0),
                );
                inner.emit(Some(resource_owner), Some(&run_id), "tool_started", fields);
            }
            AgentEvent::ToolPolicyDecision { id, name, decision } => {
                let mut fields = Map::new();
                fields.insert(
                    "tool_call_id_sha256".into(),
                    Value::String(sha256_hex(id.0.as_bytes())),
                );
                fields.insert("tool".into(), Value::String(name.clone()));
                fields.insert(
                    "decision".into(),
                    serde_json::to_value(decision).expect(
                        "tool policy decision contains only serializable diagnostic fields",
                    ),
                );
                if let Some(active) = state.active_tools.get(&id.0) {
                    fields.insert("step".into(), Value::Number(active.step_index.into()));
                }
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "tool_policy_decision",
                    fields,
                );
            }
            AgentEvent::ToolFinished {
                id,
                result,
                duration,
            } => {
                state.tool_executions = state.tool_executions.saturating_add(1);
                let active = state.active_tools.remove(&id.0);
                let (status, result_bytes, state_change) =
                    tool_result_facts(active.as_ref().map(|active| active.name.as_str()), result);
                let repeated_recently =
                    active.as_ref().map_or(0, |active| active.repeated_recently);
                if state_change == Some(true) {
                    state.useful_state_changes = state.useful_state_changes.saturating_add(1);
                    state.no_progress_streak = 0;
                } else if state_change == Some(false) && repeated_recently > 0 {
                    state.no_progress_streak = state.no_progress_streak.saturating_add(1);
                }
                let mut fields = Map::new();
                fields.insert(
                    "tool_call_id_sha256".into(),
                    Value::String(sha256_hex(id.0.as_bytes())),
                );
                fields.insert("status".into(), Value::String(status.into()));
                fields.insert(
                    "elapsed_ms".into(),
                    Value::Number(duration_ms(*duration).into()),
                );
                fields.insert("result_bytes".into(), Value::Number(result_bytes.into()));
                match state_change {
                    Some(changed) => {
                        fields.insert("filesystem_state_changed".into(), Value::Bool(changed));
                        fields.insert(
                            "state_change_basis".into(),
                            Value::String("built_in_tool_contract".into()),
                        );
                    }
                    None => {
                        fields.insert("filesystem_state_changed".into(), Value::Null);
                        fields.insert(
                            "state_change_basis".into(),
                            Value::String("unknown_or_external_tool".into()),
                        );
                    }
                }
                fields.insert(
                    "repeated_recently".into(),
                    Value::Number(repeated_recently.into()),
                );
                fields.insert(
                    "no_progress_streak".into(),
                    Value::Number(state.no_progress_streak.into()),
                );
                if let Some(active) = active {
                    fields.insert("step".into(), Value::Number(active.step_index.into()));
                    fields.insert("args_bytes".into(), Value::Number(active.args_bytes.into()));
                    fields.insert("args_sha256".into(), Value::String(active.args_sha256));
                    fields.insert(
                        "observed_elapsed_ms".into(),
                        Value::Number(duration_ms(active.started.elapsed()).into()),
                    );
                }
                inner.emit(Some(resource_owner), Some(&run_id), "tool_finished", fields);
            }
            AgentEvent::CompactionStarted { reason } => {
                let mut fields = Map::new();
                fields.insert(
                    "reason".into(),
                    Value::String(compaction_reason_label(*reason).into()),
                );
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "compaction_started",
                    fields,
                );
            }
            AgentEvent::CompactionFinished { reason, result } => {
                let mut fields = Map::new();
                fields.insert(
                    "reason".into(),
                    Value::String(compaction_reason_label(*reason).into()),
                );
                match result {
                    Ok(info) => {
                        accumulate_usage(&mut state.last_usage, &info.usage);
                        fields.insert("status".into(), Value::String("committed".into()));
                        fields.insert(
                            "summary_bytes".into(),
                            Value::Number((info.summary.len() as u64).into()),
                        );
                        fields.insert(
                            "first_kept".into(),
                            Value::String(info.first_kept.0.clone()),
                        );
                        fields.insert(
                            "kind".into(),
                            Value::String(compaction_kind_label(&info.kind).into()),
                        );
                        fields.insert(
                            "elapsed_ms".into(),
                            Value::Number(duration_ms(info.elapsed).into()),
                        );
                        fields.extend(usage_fields(
                            &info.usage,
                            info.usage
                                .input_tokens
                                .saturating_add(info.usage.cache_read_tokens)
                                .saturating_add(info.usage.cache_write_tokens),
                            "operation",
                        ));
                        if let Some(cost) = info.cost_microdollars {
                            fields.insert("cost_microdollars".into(), Value::Number(cost.into()));
                        }
                    }
                    Err(error) => {
                        fields.insert("status".into(), Value::String("failed".into()));
                        fields.insert("error".into(), Value::String(bounded_text(error)));
                    }
                }
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "compaction_finished",
                    fields,
                );
            }
            AgentEvent::TurnFinished {
                turn_usage,
                usage,
                stop_reason,
                message,
                ..
            } => {
                state.requests_finished = state.requests_finished.saturating_add(1);
                state.last_usage = *usage;
                let attempt = state.current_attempt.take();
                let (
                    logical_turn,
                    attempt_number,
                    step,
                    elapsed,
                    ttft,
                    text_bytes,
                    reasoning_bytes,
                ) = attempt.map_or(
                    (
                        state.turn_index,
                        state.request_attempts,
                        state.step_index,
                        0,
                        None,
                        0,
                        0,
                    ),
                    |attempt| {
                        (
                            attempt.logical_turn,
                            attempt.attempt,
                            attempt.step_index,
                            duration_ms(attempt.started.elapsed()),
                            attempt.ttft.map(duration_ms),
                            attempt.text_bytes,
                            attempt.reasoning_bytes,
                        )
                    },
                );
                let provider_input_tokens = turn_usage
                    .input_tokens
                    .saturating_add(turn_usage.cache_read_tokens)
                    .saturating_add(turn_usage.cache_write_tokens);
                let mut fields = usage_fields(turn_usage, provider_input_tokens, "request");
                fields.insert("logical_turn".into(), Value::Number(logical_turn.into()));
                fields.insert("attempt".into(), Value::Number(attempt_number.into()));
                fields.insert("step".into(), Value::Number(step.into()));
                fields.insert("elapsed_ms".into(), Value::Number(elapsed.into()));
                if let Some(ttft) = ttft {
                    fields.insert("ttft_ms".into(), Value::Number(ttft.into()));
                    fields.insert(
                        "generation_ms".into(),
                        Value::Number(elapsed.saturating_sub(ttft).into()),
                    );
                }
                fields.insert("output_text_bytes".into(), Value::Number(text_bytes.into()));
                fields.insert(
                    "output_reasoning_bytes".into(),
                    Value::Number(reasoning_bytes.into()),
                );
                fields.insert(
                    "assistant_parts".into(),
                    Value::Number((message.content.len() as u64).into()),
                );
                fields.insert(
                    "stop_reason".into(),
                    Value::String(stop_reason.as_canonical().into()),
                );
                add_context_occupancy(
                    &mut fields,
                    provider_input_tokens,
                    state.model.context_limit,
                );
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "model_request_finished",
                    fields,
                );
            }
            AgentEvent::CandidateRejected { usage, .. } => {
                state.last_usage = *usage;
                let mut fields = usage_fields(
                    usage,
                    usage
                        .input_tokens
                        .saturating_add(usage.cache_read_tokens)
                        .saturating_add(usage.cache_write_tokens),
                    "run_cumulative",
                );
                fields.insert("status".into(), Value::String("candidate_rejected".into()));
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "candidate_rejected",
                    fields,
                );
            }
            AgentEvent::RunFinished { head, reason } => {
                let mut fields = Map::new();
                fields.insert(
                    "status".into(),
                    Value::String(finish_reason_label(reason).into()),
                );
                fields.insert("head".into(), Value::String(head.0.clone()));
                fields.insert(
                    "elapsed_ms".into(),
                    Value::Number(duration_ms(state.started.elapsed()).into()),
                );
                fields.insert(
                    "model_requests".into(),
                    Value::Number(state.request_attempts.into()),
                );
                fields.insert(
                    "successful_model_requests".into(),
                    Value::Number(state.requests_finished.into()),
                );
                fields.insert(
                    "discarded_model_requests".into(),
                    Value::Number(state.requests_discarded.into()),
                );
                fields.insert("tool_calls".into(), Value::Number(state.tool_calls.into()));
                fields.insert(
                    "tool_executions".into(),
                    Value::Number(state.tool_executions.into()),
                );
                fields.insert(
                    "repeated_tool_calls".into(),
                    Value::Number(state.repeated_tool_calls.into()),
                );
                fields.insert(
                    "useful_state_changes".into(),
                    Value::Number(state.useful_state_changes.into()),
                );
                fields.insert(
                    "no_progress_streak".into(),
                    Value::Number(state.no_progress_streak.into()),
                );
                fields.extend(usage_fields(
                    &state.last_usage,
                    state
                        .last_usage
                        .input_tokens
                        .saturating_add(state.last_usage.cache_read_tokens)
                        .saturating_add(state.last_usage.cache_write_tokens),
                    "run_cumulative",
                ));
                inner.emit(Some(resource_owner), Some(&run_id), "run_finished", fields);
                inner.owners.remove(resource_owner);
            }
            AgentEvent::SteeringDelivered { messages }
            | AgentEvent::FollowUpDelivered { messages } => {
                let mut fields = Map::new();
                fields.insert(
                    "message_count".into(),
                    Value::Number((messages.len() as u64).into()),
                );
                fields.insert(
                    "message_bytes".into(),
                    Value::Number(
                        messages
                            .iter()
                            .map(|message| message.len() as u64)
                            .sum::<u64>()
                            .into(),
                    ),
                );
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    event_label(event),
                    fields,
                );
            }
            AgentEvent::DelegationUpdated { snapshot } => {
                let mut fields = Map::new();
                fields.insert(
                    "children".into(),
                    Value::Number((snapshot.children.len() as u64).into()),
                );
                fields.insert("revision".into(), Value::Number(snapshot.revision.into()));
                inner.emit(
                    Some(resource_owner),
                    Some(&run_id),
                    "delegation_updated",
                    fields,
                );
            }
            AgentEvent::ToolProgress { .. } | AgentEvent::OutputMedia { .. } => {}
        }
    }
}

impl Inner {
    fn emit(
        &mut self,
        owner: Option<&str>,
        run_id: Option<&str>,
        record: &str,
        mut fields: Map<String, Value>,
    ) {
        if self.write_failed {
            return;
        }
        self.sequence = self.sequence.saturating_add(1);
        let mut object = Map::new();
        object.insert("schema".into(), Value::String(TELEMETRY_SCHEMA.into()));
        object.insert("sequence".into(), Value::Number(self.sequence.into()));
        object.insert("timestamp_unix_ms".into(), Value::Number(unix_ms().into()));
        object.insert("record".into(), Value::String(record.into()));
        if let Some(owner) = owner {
            object.insert("resource_owner".into(), Value::String(owner.into()));
        }
        if let Some(run_id) = run_id {
            object.insert("run_id".into(), Value::String(run_id.into()));
        }
        object.append(&mut fields);
        let result = (|| -> io::Result<()> {
            let mut bytes = Vec::new();
            serde_json::to_writer(&mut bytes, &Value::Object(object))?;
            bytes.push(b'\n');
            self.file.lock_exclusive()?;
            let write_result = self.file.write_all(&bytes).and_then(|_| self.file.flush());
            let unlock_result = fs2::FileExt::unlock(&self.file);
            write_result.and(unlock_result)
        })();
        if result.is_err() {
            self.write_failed = true;
        }
    }
}

fn protocol_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiResponses => "openai_responses",
        Protocol::OpenAiChat => "openai_chat",
        Protocol::AnthropicMessages => "anthropic_messages",
    }
}

fn input_text_bytes(input: &UserInput) -> u64 {
    input
        .parts
        .iter()
        .filter_map(|part| match part {
            InputPart::Text(text) => Some(text.len() as u64),
            InputPart::Media(_) => None,
        })
        .sum()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u64::MAX as u128) as u64
        })
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_ERROR_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_ERROR_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn event_label(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::SteeringDelivered { .. } => "steering_delivered",
        AgentEvent::FollowUpDelivered { .. } => "follow_up_delivered",
        AgentEvent::CompactionStarted { .. } => "compaction_started",
        AgentEvent::CompactionFinished { .. } => "compaction_finished",
        AgentEvent::TurnStarted => "model_request_started",
        AgentEvent::ToolStarted { .. } => "tool_started",
        AgentEvent::ToolPolicyDecision { .. } => "tool_policy_decision",
        AgentEvent::ToolFinished { .. } => "tool_finished",
        AgentEvent::CandidateRejected { .. } => "candidate_rejected",
        AgentEvent::TurnFinished { .. } => "model_request_finished",
        AgentEvent::RunFinished { .. } => "run_finished",
        AgentEvent::DelegationUpdated { .. } => "delegation_updated",
        AgentEvent::OutputDelta { .. }
        | AgentEvent::OutputMedia { .. }
        | AgentEvent::ProviderRetry { .. }
        | AgentEvent::ToolProgress { .. } => "event",
    }
}

fn compaction_reason_label(reason: CompactionReason) -> &'static str {
    match reason {
        CompactionReason::Threshold => "threshold",
        CompactionReason::Overflow => "overflow",
    }
}

fn compaction_kind_label(kind: &CompactionKind) -> &'static str {
    match kind {
        CompactionKind::Local => "local",
        CompactionKind::NativeResponses { .. } => "native_responses",
    }
}

fn finish_reason_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::Aborted => "aborted",
        FinishReason::Failed(_) => "failed",
        FinishReason::MaxTurns => "max_turns",
    }
}

fn accumulate_usage(total: &mut Usage, next: &Usage) {
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

fn usage_fields(
    usage: &Usage,
    provider_input_tokens: u64,
    usage_scope: &'static str,
) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("usage_scope".into(), Value::String(usage_scope.into()));
    fields.insert(
        "uncached_input_tokens".into(),
        Value::Number(usage.input_tokens.into()),
    );
    fields.insert(
        "cache_read_tokens".into(),
        Value::Number(usage.cache_read_tokens.into()),
    );
    fields.insert(
        "cache_write_tokens".into(),
        Value::Number(usage.cache_write_tokens.into()),
    );
    fields.insert(
        "cache_write_1h_tokens".into(),
        Value::Number(usage.cache_write_1h_tokens.into()),
    );
    fields.insert(
        "provider_input_tokens".into(),
        Value::Number(provider_input_tokens.into()),
    );
    fields.insert(
        "output_tokens".into(),
        Value::Number(usage.output_tokens.into()),
    );
    fields.insert(
        "reasoning_tokens".into(),
        Value::Number(usage.reasoning_tokens.into()),
    );
    fields.insert(
        "total_tokens".into(),
        Value::Number(usage.total_tokens.into()),
    );
    fields
}

fn add_context_occupancy(fields: &mut Map<String, Value>, input_tokens: u64, limit: u64) {
    fields.insert(
        "context_occupancy_tokens".into(),
        Value::Number(input_tokens.into()),
    );
    fields.insert("context_limit_tokens".into(), Value::Number(limit.into()));
    if limit > 0 {
        fields.insert(
            "context_occupancy_basis_points".into(),
            Value::Number(
                input_tokens
                    .saturating_mul(10_000)
                    .saturating_div(limit)
                    .into(),
            ),
        );
    }
}

fn tool_result_facts(
    name: Option<&str>,
    result: &Result<ToolOutput, ToolError>,
) -> (&'static str, u64, Option<bool>) {
    match result {
        Ok(output) => {
            let status = if output.is_error() { "error" } else { "ok" };
            let state_change = match name {
                Some("edit" | "write") => Some(!output.is_error()),
                Some("read" | "search") => Some(false),
                _ => None,
            };
            (status, output.text.len() as u64, state_change)
        }
        Err(error) => ("error", error.message.len() as u64, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{EffectPolicy, ToolEffect, ToolPolicyDenialCode};
    use crate::events::{OutputChannel, ToolPolicyDecision};
    use crate::input::UserInput;
    use crate::sandbox::SandboxConfig;
    use ygg_ai::{
        AssistantMessage, AssistantPart, Capabilities, Endpoint, EndpointId, EndpointTransport,
        ModalitySet, ModelId, ModelLimits, ModelSpec, Protocol, StopReason, ToolCallId, Usage,
    };

    fn model() -> Model {
        Model {
            spec: Arc::new(ModelSpec {
                id: ModelId("telemetry-model".into()),
                endpoint: EndpointId("local".into()),
                api_name: "telemetry-model".into(),
                display_name: None,
                protocol: Protocol::OpenAiChat,
                capabilities: Capabilities {
                    input_modalities: ModalitySet::none(),
                    output_modalities: ModalitySet::none(),
                    tools: true,
                    parallel_tool_calls: false,
                    reasoning: None,
                    responses_lite: false,
                    agent_delegation: None,
                    structured_output: false,
                    deferred_tool_loading: false,
                },
                limits: ModelLimits {
                    context_window: 100,
                    max_output_tokens: 20,
                },
                pricing: None,
                cache: Default::default(),
            }),
            endpoint: Arc::new(Endpoint {
                id: EndpointId("local".into()),
                base_url: "http://127.0.0.1/".parse().unwrap(),
                auth: ygg_ai::Auth::None,
                default_headers: http::HeaderMap::new(),
                transport: EndpointTransport::Http,
                runtime: ygg_ai::RequestRuntime::default(),
                timeout: Duration::from_secs(1),
            }),
        }
    }

    #[test]
    fn writes_bounded_machine_readable_records_without_raw_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let observer = TelemetryObserver::new(&path, "test").unwrap();
        let input = UserInput::from("secret task");
        observer.on_run_started_for_owner("entry-1", &input, &model(), "owner-1");
        observer.on_event_for_owner(&AgentEvent::TurnStarted, "owner-1");
        observer.on_event_for_owner(
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: "answer".into(),
            },
            "owner-1",
        );
        observer.on_event_for_owner(
            &AgentEvent::ToolStarted {
                id: ToolCallId("call-1".into()),
                name: "read".into(),
                args: serde_json::json!({"path": "secret.txt"}),
            },
            "owner-1",
        );
        observer.on_event_for_owner(
            &AgentEvent::ToolFinished {
                id: ToolCallId("call-1".into()),
                result: Ok(ToolOutput::new("contents")),
                duration: Duration::from_millis(2),
            },
            "owner-1",
        );
        observer.on_event_for_owner(
            &AgentEvent::TurnFinished {
                message: AssistantMessage {
                    content: vec![AssistantPart::Text("answer".into())],
                    model: ModelId("telemetry-model".into()),
                    protocol: Protocol::OpenAiChat,
                },
                stop_reason: StopReason::EndTurn,
                turn_usage: Usage {
                    input_tokens: 3,
                    cache_read_tokens: 4,
                    output_tokens: 2,
                    total_tokens: 9,
                    ..Usage::default()
                },
                usage: Usage {
                    input_tokens: 3,
                    cache_read_tokens: 4,
                    output_tokens: 2,
                    total_tokens: 9,
                    ..Usage::default()
                },
                session_cost_microdollars: None,
                run_cost_microdollars: 0,
            },
            "owner-1",
        );
        observer.on_event_for_owner(
            &AgentEvent::RunFinished {
                head: crate::EntryId("entry-2".into()),
                reason: FinishReason::Completed,
            },
            "owner-1",
        );
        drop(observer);

        let lines = std::fs::read_to_string(path).unwrap();
        let records = lines
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records[0]["record"], "header");
        assert!(records
            .iter()
            .any(|record| record["record"] == "model_request_finished"));
        assert!(records
            .iter()
            .any(|record| record["record"] == "tool_started"));
        assert!(!lines.contains("secret task"));
        assert!(!lines.contains("secret.txt"));
        let request = records
            .iter()
            .find(|record| record["record"] == "model_request_finished")
            .unwrap();
        assert_eq!(request["uncached_input_tokens"], 3);
        assert_eq!(request["cache_read_tokens"], 4);
        assert_eq!(request["provider_input_tokens"], 7);
        assert_eq!(request["usage_scope"], "request");
        let run = records
            .iter()
            .find(|record| record["record"] == "run_finished")
            .unwrap();
        assert_eq!(run["usage_scope"], "run_cumulative");
    }

    #[test]
    fn policy_decision_records_are_secret_safe_and_machine_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let observer = TelemetryObserver::new(&path, "test").unwrap();
        observer.on_run_started_for_owner(
            "entry-1",
            &UserInput::from("telemetry prompt secret"),
            &model(),
            "owner-1",
        );
        observer.on_event_for_owner(
            &AgentEvent::ToolStarted {
                id: ToolCallId("secret-tool-call-id".into()),
                name: "bash".into(),
                args: serde_json::json!({"command": "echo command secret"}),
            },
            "owner-1",
        );
        let mut sandbox = SandboxConfig::new("/private/users/alice/secret-workspace");
        sandbox.shell_path = Some("/private/users/alice/.secrets/ygg-shell".into());
        observer.on_event_for_owner(
            &AgentEvent::ToolPolicyDecision {
                id: ToolCallId("secret-tool-call-id".into()),
                name: "bash".into(),
                decision: ToolPolicyDecision {
                    effect: Some(ToolEffect::HostProcess),
                    allowed: false,
                    authorization: None,
                    denial_code: Some(ToolPolicyDenialCode::ProcessDisabled),
                    policy: sandbox.effective_tool_policy(EffectPolicy::Controlled),
                },
            },
            "owner-1",
        );
        drop(observer);

        let raw = std::fs::read_to_string(path).unwrap();
        let record = raw
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|record| record["record"] == "tool_policy_decision")
            .expect("tool policy decision telemetry record");
        assert_eq!(record["schema"], TELEMETRY_SCHEMA);
        assert_eq!(record["tool"], "bash");
        assert_eq!(record["decision"]["effect"], "host_process");
        assert_eq!(record["decision"]["allowed"], false);
        assert_eq!(record["decision"]["denial_code"], "process_disabled");
        assert_eq!(
            record["decision"]["policy"]["effect_policy"]["value"],
            "controlled"
        );
        assert_eq!(
            record["decision"]["policy"]["effect_policy"]["source"],
            "default"
        );
        assert_eq!(
            record["decision"]["policy"]["shell_path"]["value"]["selection"],
            "configured"
        );
        assert!(record["decision"]["policy"]["shell_path"]["value"]
            .get("sha256")
            .is_none());
        assert_ne!(record["tool_call_id_sha256"], "secret-tool-call-id");
        assert!(!raw.contains("telemetry prompt secret"));
        assert!(!raw.contains("echo command secret"));
        assert!(!raw.contains("/private/users/alice/secret-workspace"));
        assert!(!raw.contains("/private/users/alice/.secrets/ygg-shell"));
    }

    #[test]
    fn run_usage_includes_compaction_when_the_following_request_never_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let observer = TelemetryObserver::new(&path, "test").unwrap();
        observer.on_run_started_for_owner("entry-1", &UserInput::from("task"), &model(), "owner-1");
        observer.on_event_for_owner(
            &AgentEvent::CompactionFinished {
                reason: CompactionReason::Threshold,
                result: Ok(crate::events::CompactionInfo {
                    kind: CompactionKind::Local,
                    summary: "summary".into(),
                    first_kept: crate::EntryId("entry-1".into()),
                    usage: Usage {
                        input_tokens: 5,
                        cache_read_tokens: 2,
                        output_tokens: 3,
                        total_tokens: 10,
                        ..Usage::default()
                    },
                    elapsed: Duration::from_millis(1),
                    cost_microdollars: None,
                }),
            },
            "owner-1",
        );
        observer.on_event_for_owner(
            &AgentEvent::RunFinished {
                head: crate::EntryId("entry-1".into()),
                reason: FinishReason::Failed(crate::AgentError::Workspace("failed".into())),
            },
            "owner-1",
        );
        drop(observer);

        let records = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let run = records
            .iter()
            .find(|record| record["record"] == "run_finished")
            .unwrap();
        assert_eq!(run["uncached_input_tokens"], 5);
        assert_eq!(run["cache_read_tokens"], 2);
        assert_eq!(run["output_tokens"], 3);
        assert_eq!(run["total_tokens"], 10);
    }

    #[cfg(unix)]
    #[test]
    fn creates_owner_only_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let _observer = TelemetryObserver::new(&path, "test").unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn appends_after_malformed_existing_content_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        std::fs::write(&path, "not-json\n").unwrap();
        let observer = TelemetryObserver::new(&path, "test").unwrap();
        observer.on_run_started_for_owner("entry-1", &UserInput::from("task"), &model(), "owner");
        let lines = std::fs::read_to_string(path).unwrap();
        assert!(lines.starts_with("not-json\n"));
        assert!(lines
            .lines()
            .skip(1)
            .all(|line| serde_json::from_str::<Value>(line).is_ok()));
    }

    #[test]
    fn repeated_calls_are_counted_without_changing_agent_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let observer = TelemetryObserver::new(&path, "test").unwrap();
        observer.on_run_started_for_owner("entry-1", &UserInput::from("task"), &model(), "owner");
        for id in ["a", "b"] {
            observer.on_event_for_owner(&AgentEvent::TurnStarted, "owner");
            observer.on_event_for_owner(
                &AgentEvent::ToolStarted {
                    id: ToolCallId(id.into()),
                    name: "read".into(),
                    args: serde_json::json!({"path": "same"}),
                },
                "owner",
            );
            observer.on_event_for_owner(
                &AgentEvent::ToolFinished {
                    id: ToolCallId(id.into()),
                    result: Ok(ToolOutput::new("same")),
                    duration: Duration::ZERO,
                },
                "owner",
            );
        }
        let lines = std::fs::read_to_string(path).unwrap();
        let repeated = lines
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|record| record["record"] == "tool_started" && record["repeated_recently"] == 1)
            .unwrap();
        assert_eq!(repeated["same_call_recently"], true);
    }
}
