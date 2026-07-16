//! Stateful event-stream assembly, state machine invariants, and guards.

use crate::error::{AiError, DecodeError, Diagnostic, StreamProtocolError};
use crate::pricing::Pricing;
use crate::types::{
    AssistantMessage, AssistantPart, Media, ModelId, Protocol, ReasoningPart, ReasoningState,
    Response, StopReason, ToolCall, ToolCallId, Usage,
};
use std::collections::HashMap;

/// Hard cap on accumulated tool-call argument bytes before assembly (design §20).
/// Crossing it is a [`DecodeError::ToolArgumentsTooLarge`], never a panic.
pub(crate) const MAX_TOOL_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

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
    pub(crate) observed_indices: Vec<usize>,
    pub(crate) provider_to_canonical_indices: HashMap<String, usize>,
    pub(crate) temp_buffers: HashMap<String, String>,
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
            observed_indices: Vec::with_capacity(4),
            provider_to_canonical_indices: HashMap::with_capacity(4),
            temp_buffers: HashMap::with_capacity(2),
        }
    }

    /// Records a diagnostic from lossy translation.
    pub(crate) fn add_diagnostic(&mut self, diag: Diagnostic) {
        self.diagnostics.push(diag);
    }

    /// Feeds a stream event into the builder.
    pub(crate) fn on_event(&mut self, event: &StreamEvent) -> Result<(), AiError> {
        match event {
            StreamEvent::Started { response_id } => {
                self.response_id = response_id.clone();
            }
            StreamEvent::TextStart { index } => {
                if !self.observed_indices.contains(index) {
                    self.observed_indices.push(*index);
                }
                self.text_buffers.insert(*index, String::new());
            }
            StreamEvent::TextDelta { index, delta } => {
                if let Some(buf) = self.text_buffers.get_mut(index) {
                    buf.push_str(delta);
                }
            }
            StreamEvent::ReasoningStart { index } => {
                if !self.observed_indices.contains(index) {
                    self.observed_indices.push(*index);
                }
                self.reasoning_text_buffers.insert(*index, String::new());
            }
            StreamEvent::ReasoningDelta { index, delta } => {
                if let Some(buf) = self.reasoning_text_buffers.get_mut(index) {
                    buf.push_str(delta);
                }
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                if !self.observed_indices.contains(index) {
                    self.observed_indices.push(*index);
                }
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
                if let Some(builder) = self.tool_call_builders.get_mut(index) {
                    if builder
                        .arguments_json
                        .len()
                        .checked_add(delta.len())
                        .map_or(true, |size| size > MAX_TOOL_ARGUMENT_BYTES)
                    {
                        return Err(AiError::Decode(DecodeError::ToolArgumentsTooLarge));
                    }
                    builder.arguments_json.push_str(delta);
                }
            }
            StreamEvent::MediaCompleted { index, media } => {
                if !self.observed_indices.contains(index) {
                    self.observed_indices.push(*index);
                }
                self.media_parts.insert(*index, media.clone());
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
        let mut indices = self.observed_indices.clone();
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
                // Verify args are a valid JSON object
                let arguments_json = if builder.arguments_json.is_empty() {
                    "{}".to_string()
                } else {
                    builder.arguments_json
                };
                let val: serde_json::Value = serde_json::from_str(&arguments_json)
                    .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;
                if !val.is_object() {
                    return Err(AiError::Decode(DecodeError::Json(
                        "Arguments must be a JSON object".to_string(),
                    )));
                }

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
