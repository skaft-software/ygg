//! Amazon Bedrock Converse/ConverseStream private wire codec.
//!
//! Bedrock streams AWS Event Stream binary frames rather than SSE. The framing
//! decoder below is deliberately incremental, CRC-checked, and bounded before a
//! payload is handed to JSON decoding.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::error::{AiError, DecodeError, ProviderError};
use crate::protocol::{
    emit_event, get_canonical_index, normalize_tool_call_id, Base64Bytes, HttpRequestParts,
};
use crate::stream::{ResponseBuilder, StreamEvent};
use crate::types::{
    AssistantPart, ImageSource, Media, Message, Protocol, Request, StopReason, ToolCallId,
    ToolChoice, ToolResultPart, Usage, UserPart,
};
use crate::validate::{normalize_request_reasoning, validate_request};

const MAX_EVENT_STREAM_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENT_STREAM_BUFFER_BYTES: usize = MAX_EVENT_STREAM_FRAME_BYTES + 12;

/// Incremental AWS Event Stream frame decoder used by the HTTP client.
pub(crate) struct BedrockEventStreamDecoder {
    buffer: Vec<u8>,
}

impl BedrockEventStreamDecoder {
    /// Creates an empty frame decoder.
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(4096),
        }
    }

    /// Feeds one network chunk and returns all complete Event Stream messages.
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<BedrockEventStreamMessage>, DecodeError> {
        if self
            .buffer
            .len()
            .checked_add(chunk.len())
            .is_none_or(|size| size > MAX_EVENT_STREAM_BUFFER_BYTES)
        {
            return Err(DecodeError::BodyTooLarge);
        }
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        loop {
            if self.buffer.len() < 12 {
                break;
            }
            let total_length = read_u32(&self.buffer[..4])? as usize;
            let headers_length = read_u32(&self.buffer[4..8])? as usize;
            if !(16..=MAX_EVENT_STREAM_FRAME_BYTES).contains(&total_length)
                || headers_length > total_length.saturating_sub(16)
            {
                return Err(invalid_frame());
            }
            if crc32(&self.buffer[..8]) != read_u32(&self.buffer[8..12])? {
                return Err(invalid_frame());
            }
            if self.buffer.len() < total_length {
                break;
            }
            if crc32(&self.buffer[..total_length - 4])
                != read_u32(&self.buffer[total_length - 4..total_length])?
            {
                return Err(invalid_frame());
            }
            let header_end = 12 + headers_length;
            let headers = parse_event_headers(&self.buffer[12..header_end])?;
            let payload = bytes::Bytes::copy_from_slice(&self.buffer[header_end..total_length - 4]);
            self.buffer.drain(..total_length);
            messages.push(BedrockEventStreamMessage { headers, payload });
        }
        Ok(messages)
    }

    /// Verifies that the body did not end halfway through an Event Stream frame.
    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(invalid_frame())
        }
    }
}

/// One decoded AWS Event Stream message.
pub(crate) struct BedrockEventStreamMessage {
    headers: BTreeMap<String, String>,
    payload: bytes::Bytes,
}

/// Stateful terminal handling for one ConverseStream response.
#[derive(Default)]
pub(crate) struct BedrockStreamState {
    message_stopped: bool,
    finished: bool,
}

/// Builds an Amazon Bedrock ConverseStream HTTP request.
pub(crate) fn build_request(
    model: &crate::catalog::Model,
    request: &Request,
) -> Result<HttpRequestParts, AiError> {
    let request = normalize_request_reasoning(request, &model.spec.capabilities);
    let diagnostics = validate_request(
        &request,
        &model.spec.capabilities,
        &model.spec.limits,
        Protocol::BedrockConverse,
        &model.spec.id,
        request.compatibility,
    )?;

    let mut messages = Vec::new();
    let mut pending_tool_uses = BTreeSet::new();
    let mut synthetic_tool_results = BTreeSet::new();
    for message in &request.messages {
        match message {
            Message::User(user) => {
                let mut content = Vec::new();
                for part in &user.content {
                    match part {
                        UserPart::Text(text) => content.push(json!({"text": text})),
                        UserPart::Media(Media::Image(image)) => {
                            if !model
                                .spec
                                .capabilities
                                .input_modalities
                                .contains(crate::types::Modality::Image)
                            {
                                continue;
                            }
                            let (ImageSource::Inline(bytes), Some(media_type)) =
                                (&image.source, &image.media_type)
                            else {
                                continue;
                            };
                            let Some(format) = bedrock_image_format(media_type.as_ref()) else {
                                continue;
                            };
                            content.push(json!({
                                "image": {
                                    "format": format,
                                    "source": {"bytes": Base64Bytes::from(bytes)},
                                }
                            }));
                        }
                        UserPart::Media(Media::Audio(_)) => {}
                        UserPart::ToolResult(result) => {
                            let tool_use_id = normalize_tool_call_id(&result.tool_call_id.0);
                            if synthetic_tool_results.contains(&tool_use_id) {
                                continue;
                            }
                            pending_tool_uses.remove(&tool_use_id);
                            let mut content = result
                                .content
                                .iter()
                                .filter_map(|part| match part {
                                    ToolResultPart::Text(text) => Some(json!({"text": text})),
                                    ToolResultPart::Media(_) => None,
                                })
                                .collect::<Vec<_>>();
                            let result_content = if content.is_empty() {
                                vec![json!({"text": ""})]
                            } else {
                                std::mem::take(&mut content)
                            };
                            content.push(json!({
                                "toolResult": {
                                    "toolUseId": tool_use_id,
                                    "content": result_content,
                                    "status": if result.is_error { "error" } else { "success" },
                                }
                            }));
                        }
                    }
                }
                push_message(&mut messages, "user", content);
            }
            Message::Assistant(assistant) => {
                if request.compatibility == crate::CompatibilityMode::Lossy {
                    push_synthetic_tool_results(
                        &mut messages,
                        &mut pending_tool_uses,
                        &mut synthetic_tool_results,
                    );
                }
                let mut content = Vec::new();
                for part in &assistant.content {
                    match part {
                        AssistantPart::Text(text) => content.push(json!({"text": text})),
                        AssistantPart::ToolCall(call) => {
                            let tool_use_id = normalize_tool_call_id(&call.id.0);
                            let input = serde_json::from_str::<Value>(&call.arguments_json)
                                .map_err(|error| {
                                    AiError::Decode(DecodeError::Json(error.to_string()))
                                })?;
                            pending_tool_uses.insert(tool_use_id.clone());
                            content.push(json!({
                                "toolUse": {
                                    "toolUseId": tool_use_id,
                                    "name": call.name,
                                    "input": input,
                                }
                            }));
                        }
                        AssistantPart::Reasoning(_)
                        | AssistantPart::Media(_)
                        | AssistantPart::ProviderMetadata(_) => {}
                    }
                }
                push_message(&mut messages, "assistant", content);
            }
        }
    }
    if request.compatibility == crate::CompatibilityMode::Lossy {
        push_synthetic_tool_results(
            &mut messages,
            &mut pending_tool_uses,
            &mut synthetic_tool_results,
        );
    }

    let mut body = Map::new();
    body.insert(
        "messages".to_owned(),
        serde_json::to_value(messages)
            .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?,
    );
    if let Some(system) = &request.system {
        body.insert("system".to_owned(), json!([{"text": system}]));
    }
    let mut inference = Map::new();
    inference.insert(
        "maxTokens".to_owned(),
        json!(request
            .max_output_tokens
            .unwrap_or(model.spec.limits.max_output_tokens)),
    );
    if let Some(temperature) = request.temperature {
        inference.insert("temperature".to_owned(), json!(temperature));
    }
    if !request.stop.is_empty() {
        inference.insert("stopSequences".to_owned(), json!(request.stop));
    }
    body.insert("inferenceConfig".to_owned(), Value::Object(inference));

    if !request.tools.is_empty() && request.tool_choice != ToolChoice::None {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "toolSpec": {
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": {"json": tool.parameters},
                    }
                })
            })
            .collect::<Vec<_>>();
        let tool_choice = match &request.tool_choice {
            ToolChoice::Auto => json!({"auto": {}}),
            ToolChoice::Required => json!({"any": {}}),
            ToolChoice::Named(name) => json!({"tool": {"name": name}}),
            ToolChoice::None => unreachable!("ToolChoice::None omits Bedrock toolConfig"),
        };
        body.insert(
            "toolConfig".to_owned(),
            json!({"tools": tools, "toolChoice": tool_choice}),
        );
    }

    let mut url = model.endpoint.base_url.clone();
    {
        let mut segments = url.path_segments_mut().map_err(|_| {
            AiError::Decode(DecodeError::InvalidProviderField(
                "invalid Bedrock endpoint URL".to_owned(),
            ))
        })?;
        segments.pop_if_empty();
        segments.push("model");
        segments.push(&model.spec.api_name);
        segments.push("converse-stream");
    }
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/vnd.amazon.eventstream"),
    );
    headers.insert(
        http::HeaderName::from_static("x-amzn-bedrock-accept"),
        http::HeaderValue::from_static("application/json"),
    );
    Ok(HttpRequestParts {
        url,
        headers,
        body: bytes::Bytes::from(
            serde_json::to_vec(&Value::Object(body))
                .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?,
        ),
        streaming: true,
        diagnostics,
    })
}

/// Decodes one Event Stream message into canonical events.
pub(crate) fn decode_stream_event(
    _model: &crate::catalog::Model,
    message: &BedrockEventStreamMessage,
    builder: &mut ResponseBuilder,
    state: &mut BedrockStreamState,
) -> Result<Vec<StreamEvent>, AiError> {
    builder.observe_provider_stream_event()?;
    let message_type = message.headers.get(":message-type").map(String::as_str);
    if message_type == Some("exception") {
        return Err(bedrock_exception(message));
    }
    let Some(event_type) = message.headers.get(":event-type").map(String::as_str) else {
        return Ok(Vec::new());
    };
    let payload = parse_payload(&message.payload)?;
    let mut events = Vec::new();
    match event_type {
        "messageStart" => emit_event(
            &mut events,
            builder,
            StreamEvent::Started { response_id: None },
        )?,
        "contentBlockStart" => {
            let index = content_block_index(&payload)?;
            let canonical = get_canonical_index(builder, &format!("block_{index}"));
            if let Some(tool_use) = payload.get("start").and_then(|start| start.get("toolUse")) {
                let id = required_string(tool_use, "toolUseId")?;
                let name = required_string(tool_use, "name")?;
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallStart {
                        index: canonical,
                        id: ToolCallId(id),
                        name,
                    },
                )?;
            }
        }
        "contentBlockDelta" => {
            let index = content_block_index(&payload)?;
            let canonical = get_canonical_index(builder, &format!("block_{index}"));
            let delta = payload
                .get("delta")
                .ok_or_else(|| invalid_provider_field("contentBlockDelta.delta"))?;
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                if !builder.text_buffers.contains_key(&canonical) {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::TextStart { index: canonical },
                    )?;
                }
                if !text.is_empty() {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::TextDelta {
                            index: canonical,
                            delta: text.to_owned(),
                        },
                    )?;
                }
            } else if let Some(input) = delta
                .get("toolUse")
                .and_then(|tool_use| tool_use.get("input"))
                .and_then(Value::as_str)
            {
                if !builder.tool_call_builders.contains_key(&canonical) {
                    return Err(invalid_provider_field(
                        "toolUse delta without toolUse start",
                    ));
                }
                if !input.is_empty() {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ToolCallArgsDelta {
                            index: canonical,
                            delta: input.to_owned(),
                        },
                    )?;
                }
            }
        }
        "contentBlockStop" => {
            let index = content_block_index(&payload)?;
            let canonical = get_canonical_index(builder, &format!("block_{index}"));
            if builder.text_buffers.contains_key(&canonical)
                && !builder.ended_indices.contains(&canonical)
            {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextEnd { index: canonical },
                )?;
            } else if builder.tool_call_builders.contains_key(&canonical)
                && !builder.ended_indices.contains(&canonical)
            {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallEnd {
                        index: canonical,
                        argument_error: None,
                    },
                )?;
            }
        }
        "messageStop" => {
            let stop = payload
                .get("stopReason")
                .and_then(Value::as_str)
                .map(map_stop_reason)
                .unwrap_or(StopReason::EndTurn);
            builder.set_stop_reason(stop);
            state.message_stopped = true;
        }
        "metadata" => {
            if let Some(usage) = payload.get("usage") {
                let usage = map_usage(usage)?;
                emit_event(&mut events, builder, StreamEvent::Usage(usage))?;
            }
            if state.message_stopped {
                finish_stream(builder, state, &mut events)?;
            }
        }
        _ => {}
    }
    Ok(events)
}

/// Emits the terminal event when an otherwise valid Bedrock stream reaches EOF.
pub(crate) fn finish_stream(
    builder: &mut ResponseBuilder,
    state: &mut BedrockStreamState,
    events: &mut Vec<StreamEvent>,
) -> Result<(), AiError> {
    if state.message_stopped && !state.finished {
        let response = builder.finish_mut()?;
        emit_event(events, builder, StreamEvent::Finished(response))?;
        state.finished = true;
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct BedrockMessage {
    role: &'static str,
    content: Vec<Value>,
}

fn push_message(messages: &mut Vec<BedrockMessage>, role: &'static str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(previous) = messages.last_mut().filter(|message| message.role == role) {
        previous.content.extend(content);
    } else {
        messages.push(BedrockMessage { role, content });
    }
}

fn push_synthetic_tool_results(
    messages: &mut Vec<BedrockMessage>,
    pending: &mut BTreeSet<String>,
    synthetic: &mut BTreeSet<String>,
) {
    let content = std::mem::take(pending)
        .into_iter()
        .map(|tool_use_id| {
            synthetic.insert(tool_use_id.clone());
            json!({
                "toolResult": {
                    "toolUseId": tool_use_id,
                    "content": [{"text": "Tool execution result was not supplied by the caller."}],
                    "status": "error",
                }
            })
        })
        .collect::<Vec<_>>();
    push_message(messages, "user", content);
}

fn bedrock_image_format(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/jpeg" => Some("jpeg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn parse_payload(payload: &[u8]) -> Result<Value, AiError> {
    serde_json::from_slice(payload)
        .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))
}

fn bedrock_exception(message: &BedrockEventStreamMessage) -> AiError {
    let code = message
        .headers
        .get(":exception-type")
        .cloned()
        .or_else(|| message.headers.get(":event-type").cloned());
    let text = serde_json::from_slice::<Value>(&message.payload)
        .ok()
        .and_then(|payload| {
            payload
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Bedrock stream returned an exception".to_owned());
    AiError::Provider(ProviderError {
        code,
        kind: Some("bedrock_event_stream".to_owned()),
        message: text,
        request_id: None,
    })
}

fn content_block_index(payload: &Value) -> Result<usize, AiError> {
    let index = payload
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_provider_field("contentBlockIndex"))?;
    usize::try_from(index).map_err(|_| invalid_provider_field("contentBlockIndex"))
}

fn required_string(value: &Value, field: &str) -> Result<String, AiError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid_provider_field(field))
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "content_filtered" | "guardrail_intervened" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

fn map_usage(value: &Value) -> Result<Usage, AiError> {
    let input_tokens = value
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let minimum_total = input_tokens
        .checked_add(output_tokens)
        .ok_or_else(|| AiError::Decode(DecodeError::UsageUnderflow))?;
    let total_tokens = value
        .get("totalTokens")
        .and_then(Value::as_u64)
        .unwrap_or(minimum_total);
    if total_tokens < minimum_total {
        return Err(AiError::Decode(DecodeError::UsageUnderflow));
    }
    Ok(Usage {
        input_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        output_tokens,
        reasoning_tokens: 0,
        total_tokens,
    })
}

fn invalid_provider_field(field: impl Into<String>) -> AiError {
    AiError::Decode(DecodeError::InvalidProviderField(field.into()))
}

fn invalid_frame() -> DecodeError {
    DecodeError::InvalidProviderField("invalid AWS Event Stream frame".to_owned())
}

fn read_u32(bytes: &[u8]) -> Result<u32, DecodeError> {
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| invalid_frame())?;
    Ok(u32::from_be_bytes(bytes))
}

fn parse_event_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, DecodeError> {
    let mut offset = 0usize;
    let mut headers = BTreeMap::new();
    while offset < bytes.len() {
        let name_length = usize::from(*bytes.get(offset).ok_or_else(invalid_frame)?);
        offset = offset.checked_add(1).ok_or_else(invalid_frame)?;
        let name_end = offset.checked_add(name_length).ok_or_else(invalid_frame)?;
        let name = std::str::from_utf8(bytes.get(offset..name_end).ok_or_else(invalid_frame)?)
            .map_err(|_| invalid_frame())?;
        offset = name_end;
        let value_type = *bytes.get(offset).ok_or_else(invalid_frame)?;
        offset = offset.checked_add(1).ok_or_else(invalid_frame)?;
        let value = match value_type {
            // AWS Event Stream string header.
            7 => {
                let length = read_u16(bytes.get(offset..offset + 2).ok_or_else(invalid_frame)?)?;
                offset = offset.checked_add(2).ok_or_else(invalid_frame)?;
                let end = offset
                    .checked_add(usize::from(length))
                    .ok_or_else(invalid_frame)?;
                let value = std::str::from_utf8(bytes.get(offset..end).ok_or_else(invalid_frame)?)
                    .map_err(|_| invalid_frame())?
                    .to_owned();
                offset = end;
                Some(value)
            }
            // true/false, byte, int16, int32, int64, timestamp, UUID.
            0 | 1 => None,
            2 => {
                offset = offset.checked_add(1).ok_or_else(invalid_frame)?;
                None
            }
            3 => {
                offset = offset.checked_add(2).ok_or_else(invalid_frame)?;
                None
            }
            4 => {
                offset = offset.checked_add(4).ok_or_else(invalid_frame)?;
                None
            }
            5 | 8 => {
                offset = offset.checked_add(8).ok_or_else(invalid_frame)?;
                None
            }
            6 => {
                let length = read_u16(bytes.get(offset..offset + 2).ok_or_else(invalid_frame)?)?;
                offset = offset
                    .checked_add(2 + usize::from(length))
                    .ok_or_else(invalid_frame)?;
                None
            }
            9 => {
                offset = offset.checked_add(16).ok_or_else(invalid_frame)?;
                None
            }
            _ => return Err(invalid_frame()),
        };
        if offset > bytes.len() {
            return Err(invalid_frame());
        }
        if let Some(value) = value {
            headers.insert(name.to_owned(), value);
        }
    }
    Ok(headers)
}

fn read_u16(bytes: &[u8]) -> Result<u16, DecodeError> {
    let bytes: [u8; 2] = bytes.try_into().map_err(|_| invalid_frame())?;
    Ok(u16::from_be_bytes(bytes))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::harness;
    use crate::types::Request;
    use crate::CompatibilityMode;

    fn frame(headers: &[(&str, &str)], payload: &Value) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7);
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let payload = serde_json::to_vec(payload).unwrap();
        let total = 16 + header_bytes.len() + payload.len();
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(&(total as u32).to_be_bytes());
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());
        bytes.extend_from_slice(&header_bytes);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());
        bytes
    }

    #[test]
    fn frame_decoder_handles_fragmented_converse_stream() {
        let bytes = [
            frame(
                &[(":message-type", "event"), (":event-type", "messageStart")],
                &json!({"role": "assistant"}),
            ),
            frame(
                &[
                    (":message-type", "event"),
                    (":event-type", "contentBlockDelta"),
                ],
                &json!({"contentBlockIndex": 0, "delta": {"text": "hello"}}),
            ),
            frame(
                &[
                    (":message-type", "event"),
                    (":event-type", "contentBlockStop"),
                ],
                &json!({"contentBlockIndex": 0}),
            ),
            frame(
                &[(":message-type", "event"), (":event-type", "messageStop")],
                &json!({"stopReason": "end_turn"}),
            ),
            frame(
                &[(":message-type", "event"), (":event-type", "metadata")],
                &json!({"usage": {"inputTokens": 3, "outputTokens": 2, "totalTokens": 5}}),
            ),
        ]
        .concat();
        let mut decoder = BedrockEventStreamDecoder::new();
        let mut messages = Vec::new();
        for chunk in bytes.chunks(7) {
            messages.extend(decoder.push(chunk).unwrap());
        }
        decoder.finish().unwrap();
        let model = harness::model(Protocol::BedrockConverse, None);
        let mut builder =
            ResponseBuilder::new(model.spec.id.clone(), Protocol::BedrockConverse, None);
        let mut state = BedrockStreamState::default();
        let mut events = Vec::new();
        for message in messages {
            events.extend(decode_stream_event(&model, &message, &mut builder, &mut state).unwrap());
        }
        finish_stream(&mut builder, &mut state, &mut events).unwrap();
        assert!(matches!(events.first(), Some(StreamEvent::Started { .. })));
        assert!(events.iter().any(
            |event| matches!(event, StreamEvent::TextDelta { delta, .. } if delta == "hello")
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 2,
                total_tokens: 5,
                ..
            })
        )));
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
    }

    #[test]
    fn frame_decoder_rejects_bad_crc() {
        let mut bytes = frame(
            &[(":message-type", "event"), (":event-type", "messageStart")],
            &json!({"role": "assistant"}),
        );
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(BedrockEventStreamDecoder::new().push(&bytes).is_err());
    }

    #[test]
    fn request_builder_uses_converse_shape() {
        let model = harness::model(Protocol::BedrockConverse, None);
        let request = Request {
            system: Some("be concise".to_owned()),
            messages: vec![Message::User(crate::types::UserMessage {
                content: vec![UserPart::Text("hello".to_owned())],
            })],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(64),
            temperature: None,
            stop: Vec::new(),
            reasoning: crate::types::ReasoningConfig::Off,
            reasoning_mode: crate::types::ReasoningMode::Standard,
            responses: None,
            output_format: crate::types::OutputFormat::Text,
            output_modalities: crate::types::OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::None,
            session_id: None,
        };
        let parts = build_request(&model, &request).unwrap();
        let body: Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 64);
        assert!(parts
            .url
            .path()
            .ends_with("/model/fixture-api-name/converse-stream"));
    }
}
