//! Stateful event-stream assembly, state machine invariants, and guards.

use crate::error::{AiError, DecodeError, Diagnostic, StreamProtocolError};
use crate::pricing::Pricing;
use crate::types::{
    AssistantMessage, AssistantPart, Media, ModelId, Protocol, ReasoningPart, ReasoningState,
    Response, StopReason, ToolCall, ToolCallId, Usage,
};
use std::collections::{HashMap, HashSet};

/// Hard cap on accumulated tool-call argument bytes before assembly (design §20).
/// Crossing it is a [`DecodeError::ToolArgumentsTooLarge`], never a panic.
pub(crate) const MAX_TOOL_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;
/// Absolute cap across streamed text, reasoning, tool arguments, and media.
pub(crate) const MAX_RESPONSE_CONTENT_BYTES: usize = 64 * 1024 * 1024;
/// Event-count cap prevents endless tiny deltas from holding a request open.
pub(crate) const MAX_RESPONSE_EVENTS: usize = 100_000;
/// Indexed-part cap bounds maps and provider-controlled sparse indices.
pub(crate) const MAX_RESPONSE_PARTS: usize = 1_024;

/// Unified events emitted by the client generation stream.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    /// Stream started. Always first.
    Started {
        /// Provider-assigned response identifier.
        response_id: Option<String>,
    },

    /// Text generation segment started.
    TextStart {
        /// Canonical part index.
        index: usize,
    },
    /// Text chunk generated.
    TextDelta {
        /// Canonical part index.
        index: usize,
        /// Newly generated text chunk.
        delta: String,
    },
    /// Text generation segment finished.
    TextEnd {
        /// Canonical part index.
        index: usize,
    },

    /// Reasoning text generation segment started.
    ReasoningStart {
        /// Canonical part index.
        index: usize,
    },
    /// Reasoning text chunk generated.
    ReasoningDelta {
        /// Canonical part index.
        index: usize,
        /// Newly generated reasoning text chunk.
        delta: String,
    },
    /// Reasoning text generation segment finished.
    ReasoningEnd {
        /// Canonical part index.
        index: usize,
    },

    /// Tool call generation started.
    ToolCallStart {
        /// Canonical part index.
        index: usize,
        /// Tool call identifier.
        id: ToolCallId,
        /// Name of the tool to invoke.
        name: String,
    },
    /// Tool call arguments chunk generated.
    ToolCallArgsDelta {
        /// Canonical part index.
        index: usize,
        /// Newly generated JSON arguments string chunk.
        delta: String,
    },
    /// Tool call generation finished.
    ToolCallEnd {
        /// Canonical part index.
        index: usize,
    },

    /// Self-contained multimodal media generated.
    MediaCompleted {
        /// Canonical part index.
        index: usize,
        /// Assembled media object.
        media: Media,
    },

    /// Intermediate or final token billing counters.
    Usage(Usage),
    /// Generation successfully finished. Always last on success.
    Finished(Response),
}

/// A pinned, boxed stream of generation events.
pub type ResponseStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<StreamEvent, AiError>> + Send>>;

pub(crate) struct ToolCallBuilder {
    pub(crate) id: ToolCallId,
    pub(crate) name: String,
    pub(crate) arguments_json: String,
}

/// Incremental state for the OpenAI Chat content-tool compatibility parser.
///
/// Search offsets always point at the first byte not yet examined for the
/// state's delimiter. Keeping them here makes a marker split across SSE events
/// cheap to resume instead of rescanning the entire pending response prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenAiChatCompatibilityState {
    /// Search ordinary content for an XML/control marker.
    Scanning { scan_from: usize },
    /// `<tool_call...` was found; search incrementally for its opening `>`.
    ToolCallOpen { scan_from: usize },
    /// The opening tag is complete; search incrementally for `</tool_call>`.
    ToolCallBody { open_end: usize, scan_from: usize },
    /// `<function...` was found; search incrementally for `</function>`.
    FunctionBody { scan_from: usize },
    /// A standalone `</function>` completed; briefly wait for an optional
    /// outer `</tool_call>` that may be split across the next provider delta.
    FunctionClosed { close_end: usize },
    /// An explicitly enabled ambiguous bare-JSON candidate is held to EOF.
    BareJson,
}

impl Default for OpenAiChatCompatibilityState {
    fn default() -> Self {
        Self::Scanning { scan_from: 0 }
    }
}

/// Helper builder that statefully assembles stream events into a finished Response.
pub(crate) struct ResponseBuilder {
    pub(crate) model: ModelId,
    pub(crate) protocol: Protocol,
    pub(crate) pricing: Option<Pricing>,
    pub(crate) response_id: Option<String>,
    pub(crate) text_buffers: HashMap<usize, String>,
    pub(crate) reasoning_text_buffers: HashMap<usize, String>,
    pub(crate) reasoning_states: HashMap<usize, ReasoningState>,
    pub(crate) tool_call_builders: HashMap<usize, ToolCallBuilder>,
    pub(crate) media_parts: HashMap<usize, Media>,
    pub(crate) usage: Option<Usage>,
    pub(crate) stop_reason: Option<StopReason>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) observed_indices: HashSet<usize>,
    aggregate_content_bytes: usize,
    /// Bytes retained outside canonical response parts while a codec waits for
    /// enough provider data to classify them. Together with
    /// `aggregate_content_bytes`, this may never exceed the response cap.
    buffered_content_bytes: usize,
    event_count: usize,
    /// Raw provider events are counted before decoding because compatibility
    /// buffering can otherwise consume arbitrarily many events without
    /// producing a canonical [`StreamEvent`].
    provider_event_count: usize,
    pub(crate) provider_to_canonical_indices: HashMap<String, usize>,
    pub(crate) temp_buffers: HashMap<String, String>,
    /// Content buffered by a compatibility parser until it is known whether it
    /// is ordinary assistant text or a Qwen XML tool call. This is only used by
    /// the OpenAI Chat codec; keeping it in the shared builder avoids losing a
    /// marker split across SSE chunks.
    pub(crate) qwen_xml_pending: String,
    /// Incremental parser state for `qwen_xml_pending`.
    pub(crate) qwen_xml_state: OpenAiChatCompatibilityState,
    /// Whether ambiguous bare JSON may be held until turn completion and
    /// interpreted as a compatibility tool call. The default is deliberately
    /// false so ordinary streamed JSON remains visible.
    pub(crate) buffer_ambiguous_compatibility_content: bool,
    /// Complete compatibility calls held until turn completion. A later native
    /// structured call supersedes these without leaking duplicate calls.
    pub(crate) qwen_xml_buffered_calls: Vec<(String, String)>,
    /// Number of synthetic tool-call IDs allocated for content-based XML/JSON
    /// calls in this response.
    pub(crate) qwen_xml_call_count: usize,
    /// A local-model control placeholder was emitted instead of the intended
    /// tool call. The Chat codec suppresses it and requests a corrective turn.
    pub(crate) tool_output_locked_seen: bool,
    /// Whether the provider has emitted a structured tool call in this
    /// response. Structured calls are authoritative over compatibility text.
    pub(crate) native_tool_call_seen: bool,
    /// Canonical indices whose `*End` event was already emitted. Codecs consult
    /// this to keep provider quirks (duplicate finish chunks, deltas after a
    /// close) from violating the one-End-per-part invariant.
    pub(crate) ended_indices: HashSet<usize>,
    /// Next canonical index to allocate. Monotonic: it never decreases, so a
    /// re-keyed provider segment can never collide with an existing index.
    pub(crate) next_canonical_index: usize,
    /// Whether the `Started` event was emitted. Tracked separately from
    /// `response_id` so a first chunk with an empty/absent provider id does
    /// not re-arm the start gate.
    pub(crate) started: bool,
}

impl ResponseBuilder {
    /// Creates a new ResponseBuilder.
    pub(crate) fn new(model: ModelId, protocol: Protocol, pricing: Option<Pricing>) -> Self {
        Self {
            model,
            protocol,
            pricing,
            response_id: None,
            text_buffers: HashMap::with_capacity(4),
            reasoning_text_buffers: HashMap::with_capacity(2),
            reasoning_states: HashMap::with_capacity(2),
            tool_call_builders: HashMap::with_capacity(4),
            media_parts: HashMap::with_capacity(2),
            usage: None,
            stop_reason: None,
            diagnostics: Vec::new(),
            observed_indices: HashSet::with_capacity(4),
            aggregate_content_bytes: 0,
            buffered_content_bytes: 0,
            event_count: 0,
            provider_event_count: 0,
            provider_to_canonical_indices: HashMap::with_capacity(4),
            temp_buffers: HashMap::with_capacity(2),
            qwen_xml_pending: String::new(),
            qwen_xml_state: OpenAiChatCompatibilityState::default(),
            buffer_ambiguous_compatibility_content: false,
            qwen_xml_buffered_calls: Vec::new(),
            qwen_xml_call_count: 0,
            tool_output_locked_seen: false,
            native_tool_call_seen: false,
            ended_indices: HashSet::with_capacity(4),
            next_canonical_index: 0,
            started: false,
        }
    }

    /// Records a diagnostic from lossy translation.
    pub(crate) fn add_diagnostic(&mut self, diag: Diagnostic) {
        // Diagnostics are non-semantic hints. Bound them rather than allowing a
        // malformed provider response to retain an unlimited vector.
        if self.diagnostics.len() < MAX_RESPONSE_PARTS {
            self.diagnostics.push(diag);
        }
    }

    fn add_content_bytes(&mut self, bytes: usize) -> Result<(), AiError> {
        let aggregate = self
            .aggregate_content_bytes
            .checked_add(bytes)
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        aggregate
            .checked_add(self.buffered_content_bytes)
            .filter(|total| *total <= MAX_RESPONSE_CONTENT_BYTES)
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        self.aggregate_content_bytes = aggregate;
        Ok(())
    }

    /// Reserves bytes retained by a codec before they become canonical stream
    /// events. This closes the gap where pre-ID tool arguments and content
    /// compatibility candidates previously bypassed the aggregate limit.
    pub(crate) fn reserve_buffered_content(&mut self, bytes: usize) -> Result<(), AiError> {
        let buffered = self
            .buffered_content_bytes
            .checked_add(bytes)
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        self.aggregate_content_bytes
            .checked_add(buffered)
            .filter(|total| *total <= MAX_RESPONSE_CONTENT_BYTES)
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        self.buffered_content_bytes = buffered;
        Ok(())
    }

    /// Releases a codec reservation immediately before the corresponding data
    /// is emitted, discarded as control syntax, or replaced.
    pub(crate) fn release_buffered_content(&mut self, bytes: usize) {
        debug_assert!(bytes <= self.buffered_content_bytes);
        self.buffered_content_bytes = self.buffered_content_bytes.saturating_sub(bytes);
    }

    fn resize_buffered_content(&mut self, old: usize, new: usize) -> Result<(), AiError> {
        let without_old = self
            .buffered_content_bytes
            .checked_sub(old)
            .ok_or_else(|| {
                AiError::Decode(DecodeError::Json(
                    "internal buffered-content accounting underflow".to_string(),
                ))
            })?;
        let buffered = without_old
            .checked_add(new)
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        self.aggregate_content_bytes
            .checked_add(buffered)
            .filter(|total| *total <= MAX_RESPONSE_CONTENT_BYTES)
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        self.buffered_content_bytes = buffered;
        Ok(())
    }

    /// Replaces a temporary provider field while preserving aggregate buffer
    /// accounting. OpenAI Chat uses this for IDs/names that may arrive before
    /// the first argument delta.
    pub(crate) fn replace_temp_buffer(
        &mut self,
        key: String,
        value: String,
    ) -> Result<(), AiError> {
        let old = self.temp_buffers.get(&key).map_or(0, String::len);
        self.resize_buffered_content(old, value.len())?;
        self.temp_buffers.insert(key, value);
        Ok(())
    }

    /// Appends a temporary provider field, enforcing both its category cap and
    /// the aggregate response cap before allocating/growing the buffer.
    pub(crate) fn append_temp_buffer_bounded(
        &mut self,
        key: String,
        delta: &str,
        max_bytes: usize,
    ) -> Result<(), AiError> {
        let old = self.temp_buffers.get(&key).map_or(0, String::len);
        let new = old
            .checked_add(delta.len())
            .filter(|size| *size <= max_bytes)
            .ok_or(AiError::Decode(DecodeError::ToolArgumentsTooLarge))?;
        self.resize_buffered_content(old, new)?;
        self.temp_buffers.entry(key).or_default().push_str(delta);
        Ok(())
    }

    /// Appends a temporary provider field whose only category limit is the
    /// aggregate response cap (for example, an opaque reasoning signature).
    pub(crate) fn append_temp_buffer(&mut self, key: String, delta: &str) -> Result<(), AiError> {
        let old = self.temp_buffers.get(&key).map_or(0, String::len);
        let new = old
            .checked_add(delta.len())
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
        self.resize_buffered_content(old, new)?;
        self.temp_buffers.entry(key).or_default().push_str(delta);
        Ok(())
    }

    /// Removes a temporary provider field and releases its reservation.
    pub(crate) fn take_temp_buffer(&mut self, key: &str) -> Option<String> {
        let value = self.temp_buffers.remove(key)?;
        self.release_buffered_content(value.len());
        Some(value)
    }

    /// Removes a temporary field while transferring its reservation to the
    /// retained canonical response content budget.
    pub(crate) fn take_temp_buffer_as_content(
        &mut self,
        key: &str,
    ) -> Result<Option<String>, AiError> {
        let Some(value) = self.temp_buffers.remove(key) else {
            return Ok(None);
        };
        self.release_buffered_content(value.len());
        self.add_content_bytes(value.len())?;
        Ok(Some(value))
    }

    /// Selects whether the OpenAI Chat codec may buffer ambiguous bare JSON.
    pub(crate) fn set_buffer_ambiguous_compatibility_content(&mut self, enabled: bool) {
        self.buffer_ambiguous_compatibility_content = enabled;
    }

    /// Counts a raw provider stream event before it reaches a codec. Canonical
    /// output events remain independently guarded by [`Self::on_event`].
    pub(crate) fn observe_provider_stream_event(&mut self) -> Result<(), AiError> {
        self.provider_event_count = self
            .provider_event_count
            .checked_add(1)
            .filter(|count| *count <= MAX_RESPONSE_EVENTS)
            .ok_or(AiError::Decode(DecodeError::TooManyStreamEvents))?;
        Ok(())
    }

    fn observe_index(&mut self, index: usize) -> Result<(), AiError> {
        self.observed_indices.insert(index);
        if self.observed_indices.len() > MAX_RESPONSE_PARTS {
            return Err(AiError::Decode(DecodeError::TooManyResponseParts));
        }
        Ok(())
    }

    /// Feeds a stream event into the builder.
    pub(crate) fn on_event(&mut self, event: &StreamEvent) -> Result<(), AiError> {
        self.event_count = self
            .event_count
            .checked_add(1)
            .filter(|count| *count <= MAX_RESPONSE_EVENTS)
            .ok_or(AiError::Decode(DecodeError::TooManyStreamEvents))?;
        match event {
            StreamEvent::Started { response_id } => {
                self.response_id = response_id.clone();
                self.started = true;
            }
            StreamEvent::TextStart { index } => {
                self.observe_index(*index)?;
                self.text_buffers.insert(*index, String::new());
            }
            StreamEvent::TextDelta { index, delta } => {
                self.add_content_bytes(delta.len())?;
                if let Some(buf) = self.text_buffers.get_mut(index) {
                    buf.push_str(delta);
                }
            }
            StreamEvent::ReasoningStart { index } => {
                self.observe_index(*index)?;
                self.reasoning_text_buffers.insert(*index, String::new());
            }
            StreamEvent::ReasoningDelta { index, delta } => {
                self.add_content_bytes(delta.len())?;
                if let Some(buf) = self.reasoning_text_buffers.get_mut(index) {
                    buf.push_str(delta);
                }
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                self.observe_index(*index)?;
                self.add_content_bytes(id.0.len().saturating_add(name.len()))?;
                self.tool_call_builders.insert(
                    *index,
                    ToolCallBuilder {
                        id: id.clone(),
                        name: name.clone(),
                        arguments_json: String::new(),
                    },
                );
            }
            StreamEvent::ToolCallArgsDelta { index, delta } => {
                self.add_content_bytes(delta.len())?;
                if let Some(builder) = self.tool_call_builders.get_mut(index) {
                    if builder
                        .arguments_json
                        .len()
                        .checked_add(delta.len())
                        .is_none_or(|size| size > MAX_TOOL_ARGUMENT_BYTES)
                    {
                        return Err(AiError::Decode(DecodeError::ToolArgumentsTooLarge));
                    }
                    builder.arguments_json.push_str(delta);
                }
            }
            StreamEvent::MediaCompleted { index, media } => {
                self.observe_index(*index)?;
                let bytes = serde_json::to_vec(media)
                    .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
                self.add_content_bytes(bytes.len())?;
                self.media_parts.insert(*index, media.clone());
            }
            StreamEvent::TextEnd { index }
            | StreamEvent::ReasoningEnd { index }
            | StreamEvent::ToolCallEnd { index } => {
                self.ended_indices.insert(*index);
            }
            StreamEvent::Usage(u) => {
                self.usage = Some(*u);
            }
            _ => {}
        }
        Ok(())
    }

    /// Sets the stop reason at stream finish.
    pub(crate) fn set_stop_reason(&mut self, reason: StopReason) {
        self.stop_reason = Some(reason);
    }

    /// Feeds reasoning continuation state.
    pub(crate) fn set_reasoning_state(&mut self, index: usize, state: ReasoningState) {
        self.reasoning_states.insert(index, state);
    }

    /// Assembles the final Response by replacing the builder with an empty one.
    pub(crate) fn finish_mut(&mut self) -> Result<Response, AiError> {
        let dummy = Self::new(self.model.clone(), self.protocol, self.pricing.clone());
        let owned = std::mem::replace(self, dummy);
        owned.finish()
    }

    /// Assembles the final Response.
    pub(crate) fn finish(mut self) -> Result<Response, AiError> {
        let mut content = Vec::new();

        // Sort indices based on first-observation order
        let mut indices = self.observed_indices.into_iter().collect::<Vec<_>>();
        indices.sort_unstable();

        for index in indices {
            if let Some(text) = self.text_buffers.remove(&index) {
                content.push(AssistantPart::Text(text));
            } else if let Some(reasoning_text) = self.reasoning_text_buffers.remove(&index) {
                // Redacted/opaque reasoning carries no visible text (design §6.3):
                // an empty buffer becomes `None`, not `Some("")`.
                content.push(AssistantPart::Reasoning(ReasoningPart {
                    text: if reasoning_text.is_empty() {
                        None
                    } else {
                        Some(reasoning_text)
                    },
                    state: self.reasoning_states.remove(&index),
                }));
            } else if let Some(builder) = self.tool_call_builders.remove(&index) {
                // Local/open-compatible providers frequently emit otherwise
                // complete arguments with raw controls, invalid path escapes,
                // Python literals, or trailing commas. Repair those lexical
                // mistakes, but never synthesize missing values or delimiters.
                let raw_arguments = if builder.arguments_json.trim().is_empty() {
                    "{}"
                } else {
                    &builder.arguments_json
                };
                let arguments_json = crate::json_repair::normalize_json_object(raw_arguments)
                    .map_err(AiError::Decode)?;

                content.push(AssistantPart::ToolCall(ToolCall {
                    id: builder.id,
                    name: builder.name,
                    arguments_json,
                }));
            } else if let Some(media) = self.media_parts.remove(&index) {
                content.push(AssistantPart::Media(media));
            }
        }

        let message = AssistantMessage {
            content,
            model: self.model,
            protocol: self.protocol,
        };

        let usage = self.usage.unwrap_or_default();
        let cost = self
            .pricing
            .as_ref()
            .map(|p| crate::pricing::cost_of(p, &usage).map_err(AiError::Pricing))
            .transpose()?;

        Ok(Response {
            message,
            stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
            usage,
            cost,
            response_id: self.response_id,
            diagnostics: self.diagnostics,
        })
    }
}

/// Wraps a raw stream of events, statefully enforcing the stream protocol invariants.
pub(crate) fn guard<S>(inner: S) -> ResponseStream
where
    S: futures_core::Stream<Item = Result<StreamEvent, AiError>> + Send + 'static,
{
    use async_stream::try_stream;
    use futures_util::StreamExt;

    let mut inner = Box::pin(inner);
    let mut started = false;
    let mut finished = false;
    let mut part_states = HashMap::with_capacity(4);
    let mut usage_seen = false;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum PartState {
        Streaming(PartKind),
        Completed,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum PartKind {
        Text,
        Reasoning,
        ToolCall,
    }

    let stream = try_stream! {
        while let Some(res) = inner.next().await {
            let ev = res?;

            if finished {
                Err(AiError::StreamProtocol(StreamProtocolError::EventAfterFinish))?;
            }

            match &ev {
                StreamEvent::Started { .. } => {
                    if started {
                        Err(AiError::StreamProtocol(StreamProtocolError::DuplicateStart))?;
                    }
                    started = true;
                }
                _ => {
                    if !started {
                        Err(AiError::StreamProtocol(StreamProtocolError::MissingStart))?;
                    }
                }
            }

            match &ev {
                StreamEvent::Started { .. } => {}
                StreamEvent::TextStart { index } => {
                    if part_states.contains_key(index) {
                        Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("TextStart on index {}", index))))?;
                    }
                    part_states.insert(*index, PartState::Streaming(PartKind::Text));
                }
                StreamEvent::TextDelta { index, .. } => {
                    match part_states.get(index) {
                        Some(PartState::Streaming(PartKind::Text)) => {}
                        _ => Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("TextDelta on index {}", index))))?,
                    }
                }
                StreamEvent::TextEnd { index } => {
                    match part_states.get(index) {
                        Some(PartState::Streaming(PartKind::Text)) => {
                            part_states.insert(*index, PartState::Completed);
                        }
                        _ => Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("TextEnd on index {}", index))))?,
                    }
                }
                StreamEvent::ReasoningStart { index } => {
                    if part_states.contains_key(index) {
                        Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("ReasoningStart on index {}", index))))?;
                    }
                    part_states.insert(*index, PartState::Streaming(PartKind::Reasoning));
                }
                StreamEvent::ReasoningDelta { index, .. } => {
                    match part_states.get(index) {
                        Some(PartState::Streaming(PartKind::Reasoning)) => {}
                        _ => Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("ReasoningDelta on index {}", index))))?,
                    }
                }
                StreamEvent::ReasoningEnd { index } => {
                    match part_states.get(index) {
                        Some(PartState::Streaming(PartKind::Reasoning)) => {
                            part_states.insert(*index, PartState::Completed);
                        }
                        _ => Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("ReasoningEnd on index {}", index))))?,
                    }
                }
                StreamEvent::ToolCallStart { index, .. } => {
                    if part_states.contains_key(index) {
                        Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("ToolCallStart on index {}", index))))?;
                    }
                    part_states.insert(*index, PartState::Streaming(PartKind::ToolCall));
                }
                StreamEvent::ToolCallArgsDelta { index, .. } => {
                    match part_states.get(index) {
                        Some(PartState::Streaming(PartKind::ToolCall)) => {}
                        _ => Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("ToolCallArgsDelta on index {}", index))))?,
                    }
                }
                StreamEvent::ToolCallEnd { index } => {
                    match part_states.get(index) {
                        Some(PartState::Streaming(PartKind::ToolCall)) => {
                            part_states.insert(*index, PartState::Completed);
                        }
                        _ => Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("ToolCallEnd on index {}", index))))?,
                    }
                }
                StreamEvent::MediaCompleted { index, .. } => {
                    if part_states.contains_key(index) {
                        Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(format!("MediaCompleted on index {}", index))))?;
                    }
                    part_states.insert(*index, PartState::Completed);
                }
                StreamEvent::Usage(_) => {
                    if usage_seen {
                        Err(AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(
                            "duplicate Usage event".to_string(),
                        )))?;
                    }
                    usage_seen = true;
                }
                StreamEvent::Finished(_) => {
                    // Check if all streaming parts are completed
                    for (idx, state) in &part_states {
                        if let PartState::Streaming(_) = state {
                            Err(AiError::StreamProtocol(StreamProtocolError::UnbalancedPart { index: *idx }))?;
                        }
                    }
                    finished = true;
                }
            }

            yield ev;
        }

        // A started stream whose transport closed before the provider's terminal
        // event (`[DONE]` / `message_stop` / `response.completed`) is a premature
        // EOF (design §8 terminal table, §17). `MissingFinish` is reserved for a
        // stream that yields no `Finished` at all (handled in `complete()`).
        if started && !finished {
            Err(AiError::StreamProtocol(StreamProtocolError::PrematureEof))?;
        }
    };

    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AudioFormat, AudioMedia, AudioPayload, StopReason};
    use futures_util::StreamExt;

    #[tokio::test]
    async fn test_response_builder_full() {
        let mut builder = ResponseBuilder::new(
            ModelId("test-model".to_string()),
            Protocol::OpenAiChat,
            None,
        );

        builder
            .on_event(&StreamEvent::Started {
                response_id: Some("resp_1".to_string()),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::TextStart { index: 0 })
            .unwrap();
        builder
            .on_event(&StreamEvent::TextDelta {
                index: 0,
                delta: "Hello ".to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::TextDelta {
                index: 0,
                delta: "world!".to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::TextEnd { index: 0 })
            .unwrap();

        builder
            .on_event(&StreamEvent::MediaCompleted {
                index: 1,
                media: Media::Audio(AudioMedia {
                    payload: AudioPayload::Inline(bytes::Bytes::from("voice")),
                    format: AudioFormat::Wav,
                    transcript: Some("hello".to_string()),
                }),
            })
            .unwrap();

        builder.set_stop_reason(StopReason::EndTurn);

        let resp = builder.finish().unwrap();
        assert_eq!(resp.response_id, Some("resp_1".to_string()));
        assert_eq!(resp.message.content.len(), 2);
        if let AssistantPart::Text(ref t) = resp.message.content[0] {
            assert_eq!(t, "Hello world!");
        } else {
            panic!("Expected Text part first");
        }
    }

    #[test]
    fn response_builder_bounds_parts_events_and_aggregate_bytes() {
        let mut parts = ResponseBuilder::new(ModelId("m".into()), Protocol::OpenAiChat, None);
        for index in 0..MAX_RESPONSE_PARTS {
            parts.on_event(&StreamEvent::TextStart { index }).unwrap();
        }
        assert!(matches!(
            parts.on_event(&StreamEvent::TextStart {
                index: MAX_RESPONSE_PARTS
            }),
            Err(AiError::Decode(DecodeError::TooManyResponseParts))
        ));

        let mut events = ResponseBuilder::new(ModelId("m".into()), Protocol::OpenAiChat, None);
        for _ in 0..MAX_RESPONSE_EVENTS {
            events
                .on_event(&StreamEvent::Usage(Usage::default()))
                .unwrap();
        }
        assert!(matches!(
            events.on_event(&StreamEvent::Usage(Usage::default())),
            Err(AiError::Decode(DecodeError::TooManyStreamEvents))
        ));

        let mut bytes = ResponseBuilder::new(ModelId("m".into()), Protocol::OpenAiChat, None);
        bytes
            .on_event(&StreamEvent::TextStart { index: 0 })
            .unwrap();
        let chunk = "x".repeat(1024 * 1024);
        for _ in 0..64 {
            bytes
                .on_event(&StreamEvent::TextDelta {
                    index: 0,
                    delta: chunk.clone(),
                })
                .unwrap();
        }
        assert!(matches!(
            bytes.on_event(&StreamEvent::TextDelta {
                index: 0,
                delta: "x".into()
            }),
            Err(AiError::Decode(DecodeError::ResponseTooLarge))
        ));
    }

    #[tokio::test]
    async fn test_response_builder_tool_call_invalid_json() {
        let mut builder = ResponseBuilder::new(
            ModelId("test-model".to_string()),
            Protocol::OpenAiChat,
            None,
        );

        builder
            .on_event(&StreamEvent::ToolCallStart {
                index: 0,
                id: ToolCallId("call_1".to_string()),
                name: "grep".to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::ToolCallArgsDelta {
                index: 0,
                delta: "invalid-json".to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::ToolCallEnd { index: 0 })
            .unwrap();

        assert!(builder.finish().is_err());
    }

    #[tokio::test]
    async fn test_response_builder_oversized_args() {
        let mut builder = ResponseBuilder::new(
            ModelId("test-model".to_string()),
            Protocol::OpenAiChat,
            None,
        );

        builder
            .on_event(&StreamEvent::ToolCallStart {
                index: 0,
                id: ToolCallId("call_1".to_string()),
                name: "grep".to_string(),
            })
            .unwrap();

        let delta = "x".repeat(16 * 1024 * 1024 + 1);
        let res = builder.on_event(&StreamEvent::ToolCallArgsDelta { index: 0, delta });
        assert!(matches!(
            res,
            Err(AiError::Decode(DecodeError::ToolArgumentsTooLarge))
        ));
    }

    #[tokio::test]
    async fn test_guard_missing_start() {
        let raw_stream = futures_util::stream::iter(vec![Ok(StreamEvent::TextStart { index: 0 })]);
        let mut guarded = guard(raw_stream);
        let res = guarded.next().await.unwrap();
        assert!(matches!(
            res,
            Err(AiError::StreamProtocol(StreamProtocolError::MissingStart))
        ));
    }

    #[tokio::test]
    async fn test_guard_duplicate_start() {
        let raw_stream = futures_util::stream::iter(vec![
            Ok(StreamEvent::Started { response_id: None }),
            Ok(StreamEvent::Started { response_id: None }),
        ]);
        let mut guarded = guard(raw_stream);
        let _started = guarded.next().await.unwrap();
        let res = guarded.next().await.unwrap();
        assert!(matches!(
            res,
            Err(AiError::StreamProtocol(StreamProtocolError::DuplicateStart))
        ));
    }

    #[tokio::test]
    async fn test_drop_cancels_inner_stream() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct DropStream {
            yielded: bool,
            dropped: Arc<AtomicBool>,
        }
        impl futures_core::Stream for DropStream {
            type Item = Result<StreamEvent, AiError>;

            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                if self.yielded {
                    std::task::Poll::Pending
                } else {
                    self.yielded = true;
                    std::task::Poll::Ready(Some(Ok(StreamEvent::Started { response_id: None })))
                }
            }
        }
        impl Drop for DropStream {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let mut guarded = guard(DropStream {
            yielded: false,
            dropped: dropped.clone(),
        });
        assert!(matches!(
            guarded.next().await,
            Some(Ok(StreamEvent::Started { .. }))
        ));
        drop(guarded);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_guard_event_after_finish() {
        let raw_stream = futures_util::stream::iter(vec![
            Ok(StreamEvent::Started { response_id: None }),
            Ok(StreamEvent::Finished(Response {
                message: AssistantMessage {
                    content: vec![],
                    model: ModelId("m".to_string()),
                    protocol: Protocol::OpenAiChat,
                },
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                cost: None,
                response_id: None,
                diagnostics: vec![],
            })),
            Ok(StreamEvent::TextStart { index: 0 }),
        ]);
        let mut guarded = guard(raw_stream);
        let _started = guarded.next().await.unwrap();
        let _finished = guarded.next().await.unwrap();
        let res = guarded.next().await.unwrap();
        assert!(matches!(
            res,
            Err(AiError::StreamProtocol(
                StreamProtocolError::EventAfterFinish
            ))
        ));
    }
}
