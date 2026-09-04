//! Stateful event-stream assembly, state machine invariants, and guards.

use crate::error::{AiError, DecodeError, Diagnostic, StreamProtocolError};
use crate::pricing::Pricing;
use crate::types::{
    AssistantMessage, AssistantPart, Media, ModelId, Protocol, ProviderPartMetadata, ReasoningPart,
    ReasoningState, Response, StopReason, ToolArgumentValidation, ToolCall, ToolCallArgumentError,
    ToolCallId, ToolDef, Usage,
};
use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// A bounded advisory state reported by an opt-in OpenAI-compatible endpoint
/// while it prepares a cold model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLifecycleState {
    /// The endpoint accepted the request but has not started loading it.
    Queued,
    /// The endpoint is loading or initializing the requested model.
    Loading,
    /// The endpoint is ready to generate the requested response.
    Ready,
}

impl ProviderLifecycleState {
    /// Stable lowercase wire and serialization value for this state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Loading => "loading",
            Self::Ready => "ready",
        }
    }

    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value.trim() {
            "queued" => Some(Self::Queued),
            "loading" => Some(Self::Loading),
            "ready" => Some(Self::Ready),
            _ => None,
        }
    }
}

/// Sanitized, non-semantic lifecycle feedback from an opt-in provider.
///
/// This advisory value is never included in an assembled [`Response`] and is
/// not suitable for replay or persistence. `detail`, when present, has already
/// crossed the client transport sanitization and byte bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderLifecycle {
    /// Endpoint-reported preparation state.
    pub state: ProviderLifecycleState,
    /// Optional bounded status detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

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
///
/// The final response stays inline to avoid a heap allocation and a public API
/// change at the one terminal event emitted per generation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum StreamEvent {
    /// Stream started. Always first.
    Started {
        /// Provider-assigned response identifier.
        response_id: Option<String>,
    },

    /// Advisory lifecycle feedback from an opt-in provider.
    ///
    /// This is transport telemetry, not assistant content, and therefore is
    /// never assembled into a [`Response`].
    ProviderLifecycle(ProviderLifecycle),

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
        /// Recoverable schema validation status for the completed call.
        ///
        /// Codecs emit `None`; stream assembly fills this after normalizing and
        /// validating the completed arguments against the request snapshot.
        argument_error: Option<ToolCallArgumentError>,
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
    pub(crate) argument_error: Option<ToolCallArgumentError>,
    /// True once arguments were normalized and schema-checked at an explicit
    /// `ToolCallEnd`; malformed max-token output remains false for final
    /// truncation handling.
    pub(crate) arguments_normalized: bool,
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
    /// The request's exact tool-definition snapshot. `None` is reserved for
    /// direct schema-less codec fixtures; production assembly sets `Some`, even
    /// when the request has no tools, so known response tools are validated
    /// against the exact snapshot while unknown names remain available for
    /// the agent's bounded unknown-tool recovery path.
    pub(crate) tool_definitions: Option<Vec<ToolDef>>,
    pub(crate) response_id: Option<String>,
    /// Authoritative terminal OpenAI Responses output, if supplied.
    pub(crate) responses_output: Option<crate::responses::ResponsesOutput>,
    pub(crate) text_buffers: HashMap<usize, String>,
    pub(crate) reasoning_text_buffers: HashMap<usize, String>,
    pub(crate) reasoning_states: HashMap<usize, ReasoningState>,
    /// Opaque provider metadata retained immediately before its target part.
    pub(crate) part_metadata: HashMap<usize, ProviderPartMetadata>,
    pub(crate) tool_call_builders: HashMap<usize, ToolCallBuilder>,
    pub(crate) media_parts: HashMap<usize, Media>,
    pub(crate) usage: Option<Usage>,
    pub(crate) stop_reason: Option<StopReason>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) observed_indices: HashSet<usize>,
    pub(crate) aggregate_content_bytes: usize,
    /// Bytes retained outside canonical response parts while a codec waits for
    /// enough provider data to classify them. Together with
    /// `aggregate_content_bytes`, this may never exceed the response cap.
    pub(crate) buffered_content_bytes: usize,
    pub(crate) event_count: usize,
    /// Raw provider events are counted before decoding because compatibility
    /// buffering can otherwise consume arbitrarily many events without
    /// producing a canonical [`StreamEvent`].
    pub(crate) provider_event_count: usize,
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
            tool_definitions: None,
            response_id: None,
            responses_output: None,
            text_buffers: HashMap::with_capacity(4),
            reasoning_text_buffers: HashMap::with_capacity(2),
            reasoning_states: HashMap::with_capacity(2),
            part_metadata: HashMap::with_capacity(2),
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

    /// Installs the exact request tool-definition snapshot used to validate
    /// assembled calls. A failed schema check leaves the builder unchanged.
    pub(crate) fn set_tool_definitions(&mut self, definitions: &[ToolDef]) -> Result<(), AiError> {
        crate::json_repair::validate_tool_definitions(definitions).map_err(AiError::Decode)?;
        self.tool_definitions = Some(definitions.to_vec());
        Ok(())
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
                        argument_error: None,
                        arguments_normalized: false,
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
            StreamEvent::ToolCallEnd { index, .. } => {
                // A completed, parseable call is schema-checked before this
                // terminal event reaches consumers. This prevents downstream
                // speculative execution from observing unchecked arguments.
                // Unparseable output is deferred to `finish`: a MaxTokens
                // terminal can safely retain its envelope with discarded args.
                self.normalize_completed_tool_arguments(*index)?;
                self.ended_indices.insert(*index);
            }
            StreamEvent::TextEnd { index } | StreamEvent::ReasoningEnd { index } => {
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

    fn apply_normalized_tool_arguments(
        builder: &mut ToolCallBuilder,
        arguments_json: String,
        tool_definitions: Option<&[ToolDef]>,
    ) -> Result<(), AiError> {
        let argument_error = if let Some(definitions) = tool_definitions {
            let arguments = serde_json::from_str(&arguments_json)
                .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
            match crate::json_repair::validate_tool_arguments(
                &builder.name,
                &arguments,
                definitions,
            )
            .map_err(AiError::Decode)?
            {
                ToolArgumentValidation::SchemaMismatch => {
                    Some(ToolCallArgumentError::SchemaMismatch)
                }
                ToolArgumentValidation::Valid | ToolArgumentValidation::UnknownTool => None,
            }
        } else {
            None
        };
        builder.arguments_json = arguments_json;
        builder.argument_error = argument_error;
        builder.arguments_normalized = true;
        Ok(())
    }

    /// Normalizes a parseable call at its explicit terminal event.
    ///
    /// A malformed value is deliberately deferred to [`Self::normalize_tool_arguments`]:
    /// the eventual stop reason determines whether a max-token response may retain
    /// the call envelope with discarded arguments. A parseable call, including a
    /// schema mismatch, is marked before consumers can speculate on it.
    fn normalize_completed_tool_arguments(&mut self, index: usize) -> Result<(), AiError> {
        let tool_definitions = self.tool_definitions.as_deref();
        let Some(builder) = self.tool_call_builders.get_mut(&index) else {
            return Ok(());
        };
        if builder.arguments_normalized {
            return Ok(());
        }
        let arguments_json = {
            let raw_arguments = if builder.arguments_json.trim().is_empty() {
                "{}"
            } else {
                builder.arguments_json.as_str()
            };
            match crate::json_repair::normalize_json_object(raw_arguments) {
                Ok(arguments_json) => arguments_json,
                Err(_) => return Ok(()),
            }
        };
        Self::apply_normalized_tool_arguments(builder, arguments_json, tool_definitions)
    }

    /// Returns the schema-mismatch marker computed for an explicitly completed
    /// streamed call. `None` also covers a call whose malformed arguments must
    /// wait for final truncation handling.
    pub(crate) fn tool_call_argument_error(&self, index: usize) -> Option<ToolCallArgumentError> {
        self.tool_call_builders
            .get(&index)
            .and_then(|builder| builder.argument_error)
    }

    /// Attaches opaque provider metadata to an already-started canonical part.
    ///
    /// The metadata is retained immediately before that part during assembly so
    /// a later request can replay provider continuation context without
    /// reclassifying the content as reasoning.
    pub(crate) fn set_provider_metadata(
        &mut self,
        index: usize,
        metadata: ProviderPartMetadata,
    ) -> Result<(), AiError> {
        if self.part_metadata.contains_key(&index) {
            return Ok(());
        }
        if !self.observed_indices.contains(&index)
            || self
                .observed_indices
                .len()
                .checked_add(self.part_metadata.len())
                .is_none_or(|parts| parts >= MAX_RESPONSE_PARTS)
        {
            return Err(AiError::Decode(DecodeError::TooManyResponseParts));
        }
        let bytes = match &metadata {
            ProviderPartMetadata::GoogleThoughtSignature { signature } => signature.len(),
        };
        self.add_content_bytes(bytes)?;
        self.part_metadata.insert(index, metadata);
        Ok(())
    }

    /// Normalizes unprocessed provider-generated tool arguments before consuming
    /// the builder.
    ///
    /// A max-token terminal may cut a tool argument string in the middle. Keep
    /// the call envelope so the agent can pair it with a synthetic error result,
    /// but never expose guessed partial arguments for execution. Other malformed
    /// completions remain decode failures. Performing this pass before
    /// [`Self::finish_mut`] replaces the builder also preserves stream-progress
    /// counters when strict normalization fails.
    fn normalize_tool_arguments(&mut self) -> Result<(), AiError> {
        let output_truncated = matches!(self.stop_reason, Some(StopReason::MaxTokens));
        let mut discarded_truncated_arguments = false;
        {
            let tool_definitions = self.tool_definitions.as_deref();
            for builder in self.tool_call_builders.values_mut() {
                if builder.arguments_normalized {
                    continue;
                }
                let raw_arguments = if builder.arguments_json.trim().is_empty() {
                    "{}"
                } else {
                    builder.arguments_json.as_str()
                };
                let arguments_json = match crate::json_repair::normalize_json_object(raw_arguments)
                {
                    Ok(arguments_json) => arguments_json,
                    Err(_) if output_truncated => {
                        builder.arguments_json = "{}".to_owned();
                        builder.argument_error = None;
                        builder.arguments_normalized = true;
                        discarded_truncated_arguments = true;
                        continue;
                    }
                    Err(error) => return Err(AiError::Decode(error)),
                };
                Self::apply_normalized_tool_arguments(builder, arguments_json, tool_definitions)?;
            }
        }
        if discarded_truncated_arguments {
            self.add_diagnostic(Diagnostic {
                code: "discarded_truncated_tool_arguments".to_owned(),
                message: "Tool arguments truncated at the provider output limit were replaced with an empty object and must not be executed".to_owned(),
            });
        }
        Ok(())
    }

    /// Assembles the final Response by replacing the builder with an empty one.
    pub(crate) fn finish_mut(&mut self) -> Result<Response, AiError> {
        self.normalize_tool_arguments()?;
        let dummy = Self::new(self.model.clone(), self.protocol, self.pricing.clone());
        let owned = std::mem::replace(self, dummy);
        owned.finish_normalized()
    }

    /// Assembles the final Response.
    pub(crate) fn finish(mut self) -> Result<Response, AiError> {
        self.normalize_tool_arguments()?;
        self.finish_normalized()
    }

    fn finish_normalized(mut self) -> Result<Response, AiError> {
        let mut content = Vec::new();

        // Sort indices based on first-observation order
        let mut indices = self.observed_indices.into_iter().collect::<Vec<_>>();
        indices.sort_unstable();

        for index in indices {
            if let Some(metadata) = self.part_metadata.remove(&index) {
                content.push(AssistantPart::ProviderMetadata(metadata));
            }
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
                content.push(AssistantPart::ToolCall(ToolCall {
                    id: builder.id,
                    name: builder.name,
                    arguments_json: builder.arguments_json,
                    argument_error: builder.argument_error,
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
            responses_output: self.responses_output,
            diagnostics: self.diagnostics,
        })
    }
}

/// Wraps a raw stream of events, statefully enforcing the stream protocol invariants.
/// Public, strict assembler for canonical events emitted by host-mediated
/// provider transports.
///
/// Native protocol codecs keep using the crate-private [`ResponseBuilder`].
/// This adapter intentionally exposes only canonical event ingestion: an
/// integration cannot alter pricing, diagnostics, response snapshots, or the
/// request tool-definition snapshot while a response is being assembled.
/// Callers must deliver one `Started` event, balanced parts, and then call
/// [`Self::finish`] exactly once with the provider's terminal stop reason.
pub struct CanonicalStreamAssembler {
    builder: ResponseBuilder,
    active_parts: HashMap<usize, CanonicalPartKind>,
    started: bool,
    finished: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CanonicalPartKind {
    Text,
    Reasoning,
    ToolCall,
}

impl CanonicalStreamAssembler {
    /// Creates an assembler for one selected model and an exact request tool
    /// snapshot. The snapshot is validated before any provider event is
    /// accepted.
    pub fn new(
        model: ModelId,
        protocol: Protocol,
        pricing: Option<Pricing>,
        tool_definitions: &[ToolDef],
    ) -> Result<Self, AiError> {
        let mut builder = ResponseBuilder::new(model, protocol, pricing);
        builder.set_tool_definitions(tool_definitions)?;
        Ok(Self {
            builder,
            active_parts: HashMap::new(),
            started: false,
            finished: false,
        })
    }

    /// Adds diagnostics generated by the host while validating a request.
    ///
    /// Diagnostics are non-semantic and remain bounded by the canonical
    /// response builder. Provider transports must not use this to inject raw
    /// provider errors or credential-bearing data.
    pub fn add_host_diagnostics(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        for diagnostic in diagnostics {
            self.builder.add_diagnostic(diagnostic);
        }
    }

    /// Records one raw, transport-level frame before it is decoded into a
    /// canonical event. This preserves the normal response-event limit for
    /// adapters which receive many small wire frames.
    pub fn observe_transport_event(&mut self) -> Result<(), AiError> {
        self.ensure_open()?;
        self.builder.observe_provider_stream_event()
    }

    /// Validates and records a canonical event.
    ///
    /// A terminal [`StreamEvent::Finished`] is rejected because final response
    /// construction remains host-owned; use [`Self::finish`] instead.
    pub fn push(&mut self, event: StreamEvent) -> Result<(), AiError> {
        self.ensure_open()?;
        match &event {
            StreamEvent::Started { .. } => {
                if self.started {
                    return Err(StreamProtocolError::DuplicateStart.into());
                }
                self.started = true;
            }
            StreamEvent::Finished(_) => {
                return Err(StreamProtocolError::UnexpectedEvent(
                    "host-mediated transports must finish through CanonicalStreamAssembler::finish"
                        .to_owned(),
                )
                .into());
            }
            _ if !self.started => return Err(StreamProtocolError::MissingStart.into()),
            StreamEvent::TextStart { index } => self.start_part(*index, CanonicalPartKind::Text)?,
            StreamEvent::ReasoningStart { index } => {
                self.start_part(*index, CanonicalPartKind::Reasoning)?
            }
            StreamEvent::ToolCallStart { index, .. } => {
                self.start_part(*index, CanonicalPartKind::ToolCall)?
            }
            StreamEvent::TextDelta { index, .. } => {
                self.require_part(*index, CanonicalPartKind::Text)?
            }
            StreamEvent::ReasoningDelta { index, .. } => {
                self.require_part(*index, CanonicalPartKind::Reasoning)?
            }
            StreamEvent::ToolCallArgsDelta { index, .. } => {
                self.require_part(*index, CanonicalPartKind::ToolCall)?
            }
            StreamEvent::TextEnd { index } => self.end_part(*index, CanonicalPartKind::Text)?,
            StreamEvent::ReasoningEnd { index } => {
                self.end_part(*index, CanonicalPartKind::Reasoning)?
            }
            StreamEvent::ToolCallEnd { index, .. } => {
                self.end_part(*index, CanonicalPartKind::ToolCall)?
            }
            StreamEvent::MediaCompleted { .. }
            | StreamEvent::ProviderLifecycle(_)
            | StreamEvent::Usage(_) => {}
        }
        self.builder.on_event(&event)
    }

    /// Completes the response with an explicit terminal reason.
    pub fn finish(&mut self, stop_reason: StopReason) -> Result<Response, AiError> {
        self.ensure_open()?;
        if !self.started {
            return Err(StreamProtocolError::MissingStart.into());
        }
        if let Some(index) = self.active_parts.keys().min().copied() {
            return Err(StreamProtocolError::UnbalancedPart { index }.into());
        }
        self.finished = true;
        self.builder.set_stop_reason(stop_reason);
        self.builder.finish_mut()
    }

    fn ensure_open(&self) -> Result<(), AiError> {
        if self.finished {
            Err(StreamProtocolError::EventAfterFinish.into())
        } else {
            Ok(())
        }
    }

    fn start_part(&mut self, index: usize, kind: CanonicalPartKind) -> Result<(), AiError> {
        if self.active_parts.insert(index, kind).is_some() {
            return Err(StreamProtocolError::UnexpectedEvent(format!(
                "part {index} was started more than once"
            ))
            .into());
        }
        Ok(())
    }

    fn require_part(&self, index: usize, kind: CanonicalPartKind) -> Result<(), AiError> {
        if self.active_parts.get(&index) == Some(&kind) {
            Ok(())
        } else {
            Err(StreamProtocolError::UnexpectedEvent(format!(
                "event does not match an active part at index {index}"
            ))
            .into())
        }
    }

    fn end_part(&mut self, index: usize, kind: CanonicalPartKind) -> Result<(), AiError> {
        self.require_part(index, kind)?;
        self.active_parts.remove(&index);
        Ok(())
    }
}

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
                StreamEvent::Started { .. } | StreamEvent::ProviderLifecycle(_) => {}
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
                StreamEvent::ToolCallEnd { index, .. } => {
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
            .on_event(&StreamEvent::ToolCallEnd {
                index: 0,
                argument_error: None,
            })
            .unwrap();

        assert!(builder.finish().is_err());
    }

    #[test]
    fn schema_mismatch_marks_the_completed_event_and_retains_normalized_call() {
        let definitions = [ToolDef {
            name: "strict".to_owned(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"count": {"type": "integer"}},
                "required": ["count"],
                "additionalProperties": false,
            }),
        }];
        let mut builder = ResponseBuilder::new(
            ModelId("test-model".to_string()),
            Protocol::OpenAiChat,
            None,
        );
        builder.set_tool_definitions(&definitions).unwrap();
        let mut events = Vec::new();
        crate::protocol::emit_event(
            &mut events,
            &mut builder,
            StreamEvent::ToolCallStart {
                index: 0,
                id: ToolCallId("call-canonical".to_owned()),
                name: "strict".to_owned(),
            },
        )
        .unwrap();
        crate::protocol::emit_event(
            &mut events,
            &mut builder,
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                delta: r#"{"unexpected":"provider-secret","count":"bad"}"#.to_owned(),
            },
        )
        .unwrap();
        crate::protocol::emit_event(
            &mut events,
            &mut builder,
            StreamEvent::ToolCallEnd {
                index: 0,
                argument_error: None,
            },
        )
        .unwrap();

        assert!(matches!(
            events.last(),
            Some(StreamEvent::ToolCallEnd {
                argument_error: Some(ToolCallArgumentError::SchemaMismatch),
                ..
            })
        ));
        builder.set_stop_reason(StopReason::ToolUse);
        let response = builder.finish().unwrap();
        let AssistantPart::ToolCall(call) = &response.message.content[0] else {
            panic!("expected retained tool call");
        };
        assert_eq!(call.id.0, "call-canonical");
        assert_eq!(
            call.arguments_json,
            r#"{"count":"bad","unexpected":"provider-secret"}"#
        );
        assert_eq!(
            call.argument_error,
            Some(ToolCallArgumentError::SchemaMismatch)
        );
    }

    #[test]
    fn max_token_response_retains_call_envelope_without_guessing_truncated_arguments() {
        let mut builder = ResponseBuilder::new(
            ModelId("test-model".to_string()),
            Protocol::OpenAiChat,
            None,
        );
        builder
            .on_event(&StreamEvent::ToolCallStart {
                index: 0,
                id: ToolCallId("call_truncated".to_string()),
                name: "write".to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::ToolCallArgsDelta {
                index: 0,
                delta: r#"{"path":"src/main.rs","content":"unterminated"#.to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::ToolCallEnd {
                index: 0,
                argument_error: None,
            })
            .unwrap();
        builder.set_stop_reason(StopReason::MaxTokens);

        let response = builder.finish().unwrap();
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
        let AssistantPart::ToolCall(call) = &response.message.content[0] else {
            panic!("expected retained tool call");
        };
        assert_eq!(call.id.0, "call_truncated");
        assert_eq!(call.name, "write");
        assert_eq!(call.arguments_json, "{}");
        assert!(response
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.code == "discarded_truncated_tool_arguments" }));
    }

    #[test]
    fn finish_mut_keeps_progress_when_strict_tool_argument_decode_fails() {
        let mut builder = ResponseBuilder::new(
            ModelId("test-model".to_string()),
            Protocol::OpenAiChat,
            None,
        );
        builder.observe_provider_stream_event().unwrap();
        builder
            .on_event(&StreamEvent::ToolCallStart {
                index: 0,
                id: ToolCallId("call_bad".to_string()),
                name: "write".to_string(),
            })
            .unwrap();
        builder
            .on_event(&StreamEvent::ToolCallArgsDelta {
                index: 0,
                delta: r#"{"content":"unterminated"#.to_string(),
            })
            .unwrap();

        assert!(builder.finish_mut().is_err());
        assert_eq!(builder.provider_event_count, 1);
        assert!(builder.event_count >= 2);
        assert!(builder.aggregate_content_bytes > 0);
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
    async fn test_guard_rejects_lifecycle_before_start() {
        let raw_stream = futures_util::stream::iter(vec![Ok(StreamEvent::ProviderLifecycle(
            ProviderLifecycle {
                state: ProviderLifecycleState::Loading,
                detail: Some("warming".into()),
            },
        ))]);
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

    #[test]
    fn canonical_assembler_keeps_final_response_host_owned() {
        let mut assembler = CanonicalStreamAssembler::new(
            ModelId("host-model".to_owned()),
            Protocol::OpenAiChat,
            None,
            &[],
        )
        .expect("valid assembler");
        assert!(matches!(
            assembler.push(StreamEvent::TextStart { index: 0 }),
            Err(AiError::StreamProtocol(StreamProtocolError::MissingStart))
        ));
        assembler
            .push(StreamEvent::Started {
                response_id: Some("response-1".to_owned()),
            })
            .expect("started");
        assembler
            .push(StreamEvent::ProviderLifecycle(ProviderLifecycle {
                state: ProviderLifecycleState::Loading,
                detail: Some("warming".to_owned()),
            }))
            .expect("lifecycle feedback");
        assembler
            .push(StreamEvent::TextStart { index: 0 })
            .expect("text start");
        assembler
            .push(StreamEvent::TextDelta {
                index: 0,
                delta: "hello".to_owned(),
            })
            .expect("text delta");
        assembler
            .push(StreamEvent::TextEnd { index: 0 })
            .expect("text end");
        let response = assembler.finish(StopReason::EndTurn).expect("finished");
        assert_eq!(response.response_id.as_deref(), Some("response-1"));
        assert!(matches!(
            response.message.content.as_slice(),
            [AssistantPart::Text(text)] if text == "hello"
        ));
        assert!(matches!(
            assembler.push(StreamEvent::Started { response_id: None }),
            Err(AiError::StreamProtocol(
                StreamProtocolError::EventAfterFinish
            ))
        ));
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
                responses_output: None,
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
