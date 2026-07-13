//! Anthropic Messages private wire protocol codec.

use base64::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{AiError, ConfigError, DecodeError, ProviderError};
use crate::protocol::sse::SseEvent;
use crate::protocol::HttpRequestParts;
use crate::stream::{ResponseBuilder, StreamEvent};
use crate::types::{
    AssistantPart, ImageSource, Media, Message, Protocol, ReasoningConfig, ReasoningState,
    ReasoningStateKind, Request, StopReason, ToolCallId, ToolChoice, ToolResultPart, Usage,
    UserPart,
};
use crate::validate::validate_request;

/// Documented Anthropic base64 image media type, or `None` if absent/unsupported.
///
/// Anthropic's `source.type == "base64"` requires an explicit media type from a
/// documented set (apidocs anthropic). A missing or out-of-set type has no wire
/// mapping, so — rather than guess (design §75) — the codec drops the part
/// (validation already emitted the diagnostic).
fn anthropic_image_media_type(image: &crate::types::ImageMedia) -> Option<String> {
    let mime = image.media_type.as_ref()?.to_string();
    match mime.as_str() {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" => Some(mime),
        _ => None,
    }
}

// --- Private Anthropic request DTOs ---

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
    stream: bool,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum AnthropicMessage {
    User { content: Vec<AnthropicContentBlock> },
    Assistant { content: Vec<AnthropicContentBlock> },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<AnthropicToolResultBlock>,
        is_error: bool,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolResultBlock {
    Text { text: String },
    Image { source: AnthropicImageSource },
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

#[derive(Serialize)]
struct AnthropicThinkingConfig {
    r#type: String,
    budget_tokens: u64,
}

#[derive(Serialize)]
struct AnthropicOutputConfig {
    format: AnthropicOutputFormat,
}

#[derive(Serialize)]
struct AnthropicOutputFormat {
    r#type: String,
    schema: serde_json::Value,
}

// --- Private Anthropic Response / SSE Chunk DTOs ---

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicSseData {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicResponseMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: AnthropicResponseContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: usize,
        delta: AnthropicResponseDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicResponseMsgDelta,
        #[serde(default)]
        usage: Option<AnthropicResponseUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: AnthropicResponseError },
}

#[derive(Deserialize)]
struct AnthropicResponseMessage {
    id: String,
    usage: AnthropicResponseUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseContentBlock {
    // Text/Thinking block openers carry a (usually empty) initial payload; the
    // real content arrives via `content_block_delta`, so the payload is unused
    // and the extra wire field is simply ignored on deserialize.
    Text {},
    Thinking {},
    RedactedThinking { data: String },
    ToolUse { id: String, name: String },
}

// Anthropic content_block_delta uses `*_delta` type tags on the wire (see
// docs/research/apidocs/anthropic-messages/messages.md), NOT bare snake_case.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicResponseDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Deserialize)]
struct AnthropicResponseMsgDelta {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicResponseUsage {
    // `input_tokens` appears on `message_start`; `message_delta` usage carries
    // only `output_tokens`. Both default so either shape deserializes.
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation: Option<AnthropicCacheCreation>,
    // `message_delta` usage carries a cumulative `output_tokens_details` with the
    // documented `thinking_tokens` subset (apidocs anthropic-messages
    // messages.md §"Message Delta Usage"). Optional so `message_start` (which
    // omits it) still deserializes.
    #[serde(default)]
    output_tokens_details: Option<AnthropicOutputTokensDetails>,
}

#[derive(Deserialize)]
struct AnthropicCacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
}

#[derive(Deserialize)]
struct AnthropicOutputTokensDetails {
    #[serde(default)]
    thinking_tokens: u64,
}

#[derive(Deserialize)]
struct AnthropicResponseError {
    r#type: String,
    message: String,
}

// --- Request Builder ---

/// Builds the Anthropic Messages HTTP request parts.
pub(crate) fn build_request(
    model: &crate::catalog::Model,
    req: &Request,
) -> Result<HttpRequestParts, AiError> {
    // 1. Run validation
    let diagnostics = validate_request(
        req,
        &model.spec.capabilities,
        &model.spec.limits,
        Protocol::AnthropicMessages,
        &model.spec.id,
        req.compatibility,
    )?;

    // 2. Map system prompt
    let system = req.system.clone();

    // 3. Map messages with alternation merging
    let mut messages = Vec::new();
    let mut pending_tool_calls = std::collections::BTreeSet::new();
    let mut synthetic_tool_results = std::collections::HashSet::new();
    for msg in &req.messages {
        match msg {
            Message::User(ref user) => {
                let mut blocks = Vec::new();
                for part in &user.content {
                    match part {
                        UserPart::Text(ref text) => {
                            blocks.push(AnthropicContentBlock::Text { text: text.clone() });
                        }
                        UserPart::Media(Media::Image(ref image)) => {
                            if !model
                                .spec
                                .capabilities
                                .input_modalities
                                .contains(crate::types::Modality::Image)
                            {
                                continue;
                            }

                            let source = match &image.source {
                                ImageSource::Inline(bytes) => {
                                    let Some(media_type) = anthropic_image_media_type(image) else {
                                        continue;
                                    };
                                    AnthropicImageSource::Base64 {
                                        media_type,
                                        data: BASE64_STANDARD.encode(bytes),
                                    }
                                }
                                ImageSource::Url(url) => AnthropicImageSource::Url {
                                    url: url.to_string(),
                                },
                                ImageSource::ProviderRef(_) => continue,
                            };
                            blocks.push(AnthropicContentBlock::Image { source });
                        }
                        UserPart::Media(Media::Audio(_)) => {}
                        UserPart::ToolResult(ref tr) => {
                            if synthetic_tool_results.contains(&tr.tool_call_id.0) {
                                continue;
                            }
                            pending_tool_calls.remove(&tr.tool_call_id.0);
                            let mut tool_blocks = Vec::new();
                            for tr_part in &tr.content {
                                match tr_part {
                                    ToolResultPart::Text(ref text) => {
                                        tool_blocks.push(AnthropicToolResultBlock::Text {
                                            text: text.clone(),
                                        });
                                    }
                                    ToolResultPart::Media(Media::Image(ref image)) => {
                                        let source = match &image.source {
                                            ImageSource::Inline(bytes) => {
                                                anthropic_image_media_type(image).map(
                                                    |media_type| AnthropicImageSource::Base64 {
                                                        media_type,
                                                        data: BASE64_STANDARD.encode(bytes),
                                                    },
                                                )
                                            }
                                            ImageSource::Url(url) => {
                                                Some(AnthropicImageSource::Url {
                                                    url: url.to_string(),
                                                })
                                            }
                                            ImageSource::ProviderRef(_) => None,
                                        };
                                        if let Some(source) = source {
                                            tool_blocks
                                                .push(AnthropicToolResultBlock::Image { source });
                                        }
                                    }
                                    ToolResultPart::Media(Media::Audio(_)) => {}
                                }
                            }
                            blocks.push(AnthropicContentBlock::ToolResult {
                                tool_use_id: crate::protocol::normalize_tool_call_id(
                                    &tr.tool_call_id.0,
                                ),
                                content: tool_blocks,
                                is_error: tr.is_error,
                            });
                        }
                    }
                }

                if !blocks.is_empty() {
                    if let Some(AnthropicMessage::User { ref mut content }) = messages.last_mut() {
                        content.extend(blocks);
                    } else {
                        messages.push(AnthropicMessage::User { content: blocks });
                    }
                }
            }
            Message::Assistant(ref assistant) => {
                if req.compatibility == crate::CompatibilityMode::Lossy {
                    push_synthetic_tool_results(
                        &mut messages,
                        &mut pending_tool_calls,
                        &mut synthetic_tool_results,
                    );
                }
                let mut blocks = Vec::new();
                for part in &assistant.content {
                    match part {
                        AssistantPart::Text(ref text) => {
                            blocks.push(AnthropicContentBlock::Text { text: text.clone() });
                        }
                        AssistantPart::ToolCall(ref tc) => {
                            pending_tool_calls.insert(tc.id.0.clone());
                            let input_val: serde_json::Value =
                                serde_json::from_str(&tc.arguments_json).map_err(|e| {
                                    AiError::Decode(DecodeError::Json(e.to_string()))
                                })?;
                            blocks.push(AnthropicContentBlock::ToolUse {
                                id: crate::protocol::normalize_tool_call_id(&tc.id.0),
                                name: tc.name.clone(),
                                input: input_val,
                            });
                        }
                        AssistantPart::Reasoning(ref reasoning) => {
                            if let Some(ref state) = reasoning.state {
                                if state.protocol == Protocol::AnthropicMessages
                                    && state.model == model.spec.id
                                {
                                    match &state.kind {
                                        ReasoningStateKind::AnthropicSignature { signature } => {
                                            if let Some(text) = &reasoning.text {
                                                blocks.push(AnthropicContentBlock::Thinking {
                                                    thinking: text.clone(),
                                                    signature: signature.clone(),
                                                });
                                            }
                                        }
                                        ReasoningStateKind::AnthropicRedacted { data } => {
                                            blocks.push(AnthropicContentBlock::RedactedThinking {
                                                data: data.clone(),
                                            });
                                        }
                                        ReasoningStateKind::OpenAiReasoning { .. } => {}
                                    }
                                }
                            }
                        }
                        AssistantPart::Media(_) => {}
                    }
                }

                if !blocks.is_empty() {
                    if let Some(AnthropicMessage::Assistant { ref mut content }) =
                        messages.last_mut()
                    {
                        content.extend(blocks);
                    } else {
                        messages.push(AnthropicMessage::Assistant { content: blocks });
                    }
                }
            }
        }
    }

    if req.compatibility == crate::CompatibilityMode::Lossy {
        push_synthetic_tool_results(
            &mut messages,
            &mut pending_tool_calls,
            &mut synthetic_tool_results,
        );
    }

    // 4. Map tools & tool_choice
    let tools_opt = if req.tools.is_empty()
        || !model.spec.capabilities.tools
        || matches!(req.tool_choice, ToolChoice::None)
    {
        None
    } else {
        Some(
            req.tools
                .iter()
                .map(|t| AnthropicTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.parameters.clone(),
                })
                .collect(),
        )
    };

    let tool_choice_opt = if !model.spec.capabilities.tools {
        None
    } else {
        match &req.tool_choice {
            ToolChoice::Auto => Some(AnthropicToolChoice::Auto),
            ToolChoice::Required => Some(AnthropicToolChoice::Any),
            ToolChoice::None => None,
            ToolChoice::Named(name) => Some(AnthropicToolChoice::Tool { name: name.clone() }),
        }
    };

    // 5. Thinking / reasoning config
    let thinking_opt = if model.spec.capabilities.reasoning.is_some() {
        match req.reasoning {
            ReasoningConfig::Off => None,
            ReasoningConfig::Effort(effort) => {
                let budgets = model
                    .spec
                    .capabilities
                    .reasoning
                    .as_ref()
                    .and_then(|r| r.effort_budgets.as_ref());
                if let Some(b) = budgets {
                    let limit = match effort {
                        crate::types::ReasoningEffort::Minimal => b.minimal,
                        crate::types::ReasoningEffort::Low => b.low,
                        crate::types::ReasoningEffort::Medium => b.medium,
                        crate::types::ReasoningEffort::High => b.high,
                    };
                    Some(AnthropicThinkingConfig {
                        r#type: "enabled".to_string(),
                        budget_tokens: limit,
                    })
                } else {
                    None
                }
            }
            ReasoningConfig::Budget(b) => Some(AnthropicThinkingConfig {
                r#type: "enabled".to_string(),
                budget_tokens: b,
            }),
        }
    } else {
        None
    };

    // Design §7: a Lossy structured-output downgrade must drop the capability
    // from the wire, not merely emit a diagnostic. Strict mode already errored in
    // `validate_request`, so an unsupported format reaching here is Lossy and is
    // serialized as plain text (`output_config` omitted).
    let output_config = match &req.output_format {
        crate::types::OutputFormat::JsonSchema(schema)
            if model.spec.capabilities.structured_output =>
        {
            Some(AnthropicOutputConfig {
                format: AnthropicOutputFormat {
                    r#type: "json_schema".to_string(),
                    schema: schema.schema.clone(),
                },
            })
        }
        _ => None,
    };

    // 6. Max tokens
    let max_tokens = req
        .max_output_tokens
        .unwrap_or(model.spec.limits.max_output_tokens);

    let anth_req = AnthropicRequest {
        model: model.spec.api_name.clone(),
        messages,
        system,
        max_tokens,
        temperature: req.temperature,
        stop_sequences: req.stop.clone(),
        tools: tools_opt,
        tool_choice: tool_choice_opt,
        thinking: thinking_opt,
        output_config,
        stream: true,
    };

    let body_bytes = serde_json::to_vec(&anth_req)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let url = model
        .endpoint
        .base_url
        .join("messages")
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::HeaderName::from_static("anthropic-version"),
        http::HeaderValue::from_static("2023-06-01"),
    );

    Ok(HttpRequestParts {
        url,
        headers,
        body: bytes::Bytes::from(body_bytes),
        streaming: true,
        diagnostics,
    })
}

fn push_synthetic_tool_results(
    messages: &mut Vec<AnthropicMessage>,
    pending: &mut std::collections::BTreeSet<String>,
    synthetic_ids: &mut std::collections::HashSet<String>,
) {
    let synthetic: Vec<_> = std::mem::take(pending)
        .into_iter()
        .map(|call_id| {
            synthetic_ids.insert(call_id.clone());
            AnthropicContentBlock::ToolResult {
                // Wire write: normalize to match the paired `tool_use` above.
                tool_use_id: crate::protocol::normalize_tool_call_id(&call_id),
                content: vec![AnthropicToolResultBlock::Text {
                    text: "Tool execution result was not supplied by the caller.".to_string(),
                }],
                is_error: true,
            }
        })
        .collect();
    if !synthetic.is_empty() {
        if let Some(AnthropicMessage::User { content }) = messages.last_mut() {
            content.extend(synthetic);
        } else {
            messages.push(AnthropicMessage::User { content: synthetic });
        }
    }
}

// --- SSE Stream Decoder ---
//
// Anthropic Messages is always streamed (design §12.3); there is no
// non-streaming decode path, so this codec deliberately exposes none.

fn emit_event(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    ev: StreamEvent,
) -> Result<(), AiError> {
    builder.on_event(&ev)?;
    events.push(ev);
    Ok(())
}

/// Decodes a streaming SSE event from Anthropic, emitting StreamEvents.
pub(crate) fn decode_stream_event(
    _model: &crate::catalog::Model,
    sse_event: &SseEvent,
    builder: &mut ResponseBuilder,
) -> Result<Vec<StreamEvent>, AiError> {
    let raw_data = sse_event.data.trim();
    if raw_data.is_empty() {
        return Ok(vec![]);
    }

    let data: AnthropicSseData = serde_json::from_str(raw_data)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let mut events = Vec::new();

    match data {
        AnthropicSseData::MessageStart { message } => {
            builder.response_id = Some(message.id.clone());
            emit_event(
                &mut events,
                builder,
                StreamEvent::Started {
                    response_id: Some(message.id),
                },
            )?;

            // Store initial usage input tokens
            let u = map_usage(&message.usage)?;
            builder.usage = Some(u);
        }
        AnthropicSseData::ContentBlockStart {
            index,
            content_block,
        } => {
            let key = format!("block_{}", index);
            let canonical_idx = get_canonical_index(builder, &key);

            match content_block {
                AnthropicResponseContentBlock::Text { .. } => {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::TextStart {
                            index: canonical_idx,
                        },
                    )?;
                }
                AnthropicResponseContentBlock::Thinking { .. } => {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningStart {
                            index: canonical_idx,
                        },
                    )?;
                }
                AnthropicResponseContentBlock::RedactedThinking { data } => {
                    // Opaque, no-visible-text reasoning (design §6.3, §12.3): open a
                    // reasoning part and attach the redacted continuation state; it
                    // is never flattened into visible text.
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningStart {
                            index: canonical_idx,
                        },
                    )?;
                    builder.set_reasoning_state(
                        canonical_idx,
                        ReasoningState {
                            model: builder.model.clone(),
                            protocol: Protocol::AnthropicMessages,
                            kind: ReasoningStateKind::AnthropicRedacted { data },
                        },
                    );
                }
                AnthropicResponseContentBlock::ToolUse { id, name } => {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ToolCallStart {
                            index: canonical_idx,
                            id: ToolCallId(id),
                            name,
                        },
                    )?;
                }
            }
        }
        AnthropicSseData::ContentBlockDelta { index, delta } => {
            let key = format!("block_{}", index);
            let canonical_idx = get_canonical_index(builder, &key);

            match delta {
                AnthropicResponseDelta::Text { text } => {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::TextDelta {
                            index: canonical_idx,
                            delta: text,
                        },
                    )?;
                }
                AnthropicResponseDelta::Thinking { thinking } => {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningDelta {
                            index: canonical_idx,
                            delta: thinking,
                        },
                    )?;
                }
                AnthropicResponseDelta::Signature { signature } => {
                    let sig_key = format!("sig_{}", index);
                    builder
                        .temp_buffers
                        .entry(sig_key)
                        .or_default()
                        .push_str(&signature);
                }
                AnthropicResponseDelta::InputJson { partial_json } => {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ToolCallArgsDelta {
                            index: canonical_idx,
                            delta: partial_json,
                        },
                    )?;
                }
            }
        }
        AnthropicSseData::ContentBlockStop { index } => {
            let key = format!("block_{}", index);
            let canonical_idx = get_canonical_index(builder, &key);

            // Determine if it was text, reasoning, or tool
            if builder.text_buffers.contains_key(&canonical_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextEnd {
                        index: canonical_idx,
                    },
                )?;
            } else if builder.reasoning_text_buffers.contains_key(&canonical_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ReasoningEnd {
                        index: canonical_idx,
                    },
                )?;

                // Check signature
                let sig_key = format!("sig_{}", index);
                if let Some(sig) = builder.temp_buffers.remove(&sig_key) {
                    builder.set_reasoning_state(
                        canonical_idx,
                        ReasoningState {
                            model: builder.model.clone(),
                            protocol: Protocol::AnthropicMessages,
                            kind: ReasoningStateKind::AnthropicSignature { signature: sig },
                        },
                    );
                }
            } else if builder.tool_call_builders.contains_key(&canonical_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallEnd {
                        index: canonical_idx,
                    },
                )?;
            }
        }
        AnthropicSseData::MessageDelta { delta, usage } => {
            if let Some(ref reason) = delta.stop_reason {
                let stop = map_stop_reason(reason);
                builder.set_stop_reason(stop);
            }

            if let Some(u_dto) = usage {
                // `message_delta` usage is cumulative and authoritative (apidocs
                // MessageDeltaUsage): it carries the final output count and may
                // also restate input/cache buckets and the thinking-token subset.
                // Merge every field the delta actually reports; fall back to the
                // `message_start` baseline only where the delta omits a value.
                let delta_usage = map_usage(&u_dto)?;
                match builder.usage.as_mut() {
                    Some(existing) => {
                        existing.output_tokens = delta_usage.output_tokens;
                        if delta_usage.input_tokens > 0 {
                            existing.input_tokens = delta_usage.input_tokens;
                        }
                        if delta_usage.cache_read_tokens > 0 {
                            existing.cache_read_tokens = delta_usage.cache_read_tokens;
                        }
                        if delta_usage.cache_write_tokens > 0 {
                            existing.cache_write_tokens = delta_usage.cache_write_tokens;
                            existing.cache_write_1h_tokens = delta_usage.cache_write_1h_tokens;
                        }
                        if delta_usage.reasoning_tokens > 0 {
                            existing.reasoning_tokens = delta_usage.reasoning_tokens;
                        }
                        if existing.reasoning_tokens > existing.output_tokens {
                            return Err(AiError::Decode(DecodeError::UsageUnderflow));
                        }
                        existing.total_tokens = existing
                            .input_tokens
                            .checked_add(existing.cache_read_tokens)
                            .and_then(|value| value.checked_add(existing.cache_write_tokens))
                            .and_then(|value| value.checked_add(existing.output_tokens))
                            .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?;
                    }
                    None => builder.usage = Some(delta_usage),
                }
            }
        }
        AnthropicSseData::MessageStop => {
            // Send final usage if we have one
            if let Some(u) = builder.usage {
                emit_event(&mut events, builder, StreamEvent::Usage(u))?;
            }

            let resp = builder.finish_mut()?;
            emit_event(&mut events, builder, StreamEvent::Finished(resp))?;
        }
        AnthropicSseData::Ping => {}
        AnthropicSseData::Error { error } => {
            return Err(AiError::Provider(ProviderError {
                code: None,
                kind: Some(error.r#type),
                message: error.message,
                request_id: None,
            }));
        }
    }

    Ok(events)
}

// --- Helpers ---

fn get_canonical_index(builder: &mut ResponseBuilder, key: &str) -> usize {
    if let Some(&idx) = builder.provider_to_canonical_indices.get(key) {
        idx
    } else {
        let idx = builder.provider_to_canonical_indices.len();
        builder
            .provider_to_canonical_indices
            .insert(key.to_string(), idx);
        idx
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

fn map_usage(usage: &AnthropicResponseUsage) -> Result<Usage, AiError> {
    // Design §15: Anthropic `input_tokens` ALREADY excludes cache, so it maps
    // directly (no subtraction — that is the OpenAI rule). `total` includes cache.
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write_1h = usage
        .cache_creation
        .as_ref()
        .map(|c| c.ephemeral_1h_input_tokens)
        .unwrap_or(0);
    let cache_write = usage
        .cache_creation_input_tokens
        .or_else(|| {
            usage.cache_creation.as_ref().and_then(|c| {
                c.ephemeral_1h_input_tokens
                    .checked_add(c.ephemeral_5m_input_tokens)
            })
        })
        .unwrap_or(0);
    let input = usage.input_tokens;
    let total_tokens = input
        .checked_add(cache_read)
        .and_then(|value| value.checked_add(cache_write))
        .and_then(|value| value.checked_add(usage.output_tokens))
        .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?;
    if cache_write_1h > cache_write {
        return Err(AiError::Decode(DecodeError::UsageUnderflow));
    }
    // `thinking_tokens` is the documented reasoning subset of `output_tokens`
    // (apidocs; always ≤ output_tokens). Absent on `message_start`, reported on
    // `message_delta`. Resolves the design §15 "reasoning=0" note in favor of the
    // checked-in API docs (which establish the wire mapping, design §0/§7).
    let reasoning = usage
        .output_tokens_details
        .as_ref()
        .map(|d| d.thinking_tokens)
        .unwrap_or(0);
    if reasoning > usage.output_tokens {
        return Err(AiError::Decode(DecodeError::UsageUnderflow));
    }
    Ok(Usage {
        input_tokens: input,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cache_write_1h_tokens: cache_write_1h,
        output_tokens: usage.output_tokens,
        reasoning_tokens: reasoning,
        total_tokens,
    })
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Model;
    use crate::types::{
        Capabilities, Endpoint, EndpointId, ImageMedia, ImageSource, Media, Message, ModalitySet,
        ModelId, ModelLimits, ModelSpec, OutputFormat, OutputModalities, ReasoningConfig,
        ReasoningPart, Request, ToolChoice, UserMessage, UserPart,
    };
    use crate::CompatibilityMode;
    use std::sync::Arc;

    fn make_test_model(reasoning: bool) -> Model {
        let spec = ModelSpec {
            id: ModelId("test-claude".to_string()),
            endpoint: EndpointId("anthropic-ep".to_string()),
            api_name: "claude-3-5-sonnet".to_string(),
            protocol: Protocol::AnthropicMessages,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none().with(crate::types::Modality::Image),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: if reasoning {
                    Some(crate::types::ReasoningCapability {
                        control: crate::types::ReasoningControl::TokenBudget,
                        exposes_text: true,
                        preserves_state: true,
                        effort_budgets: Some(crate::types::ReasoningEffortBudgets {
                            minimal: 1024,
                            low: 2048,
                            medium: 4096,
                            high: 8192,
                        }),
                        openai_chat_mode: crate::types::OpenAiChatReasoningMode::Standard,
                    })
                } else {
                    None
                },
                structured_output: true,
            },
            limits: ModelLimits {
                context_window: 200000,
                max_output_tokens: 8192,
            },
            pricing: None,
        };

        let ep = Endpoint {
            id: EndpointId("anthropic-ep".to_string()),
            base_url: url::Url::parse("https://api.anthropic.com/v1/").unwrap(),
            auth: crate::auth::Auth::none(),
            default_headers: http::HeaderMap::new(),
            timeout: std::time::Duration::from_secs(30),
        };

        Model {
            spec: Arc::new(spec),
            endpoint: Arc::new(ep),
        }
    }

    #[test]
    fn test_build_request_anthropic_basic() {
        let model = make_test_model(false);
        let req = Request {
            system: Some("System instructions".to_string()),
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("Hello".to_string())],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(1000),
            temperature: Some(0.5),
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
        };

        let parts = build_request(&model, &req).unwrap();
        assert_eq!(
            parts.url.to_string(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            parts
                .headers
                .get("anthropic-version")
                .unwrap()
                .to_str()
                .unwrap(),
            "2023-06-01"
        );

        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["model"], "claude-3-5-sonnet");
        assert_eq!(body["max_tokens"], 1000);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["system"], "System instructions");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn test_build_request_anthropic_url_image_and_structured_output() {
        let model = make_test_model(false);
        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Media(Media::image_url(
                    url::Url::parse("https://example.test/image.png").unwrap(),
                    None,
                ))],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::JsonSchema(crate::types::JsonSchemaFormat {
                name: "answer".to_string(),
                description: None,
                schema: serde_json::json!({"type":"object"}),
                strict: true,
            }),
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
        };
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["source"]["type"], "url");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
    }

    #[test]
    fn test_build_request_anthropic_thinking_replay() {
        let model = make_test_model(true);
        let state = ReasoningState {
            model: ModelId("test-claude".to_string()),
            protocol: Protocol::AnthropicMessages,
            kind: ReasoningStateKind::AnthropicSignature {
                signature: "test-sig".to_string(),
            },
        };

        let req = Request {
            system: None,
            messages: vec![
                Message::User(UserMessage {
                    content: vec![UserPart::Text("Hello".to_string())],
                }),
                Message::Assistant(crate::types::AssistantMessage {
                    content: vec![
                        AssistantPart::Reasoning(ReasoningPart {
                            text: Some("Thinking content".to_string()),
                            state: Some(state),
                        }),
                        AssistantPart::Text("Final answer".to_string()),
                    ],
                    model: ModelId("test-claude".to_string()),
                    protocol: Protocol::AnthropicMessages,
                }),
            ],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Effort(crate::types::ReasoningEffort::Medium),
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
        };

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);

        let assistant_msg = &body["messages"][1];
        assert_eq!(assistant_msg["role"], "assistant");
        assert_eq!(assistant_msg["content"][0]["type"], "thinking");
        assert_eq!(assistant_msg["content"][0]["thinking"], "Thinking content");
        assert_eq!(assistant_msg["content"][0]["signature"], "test-sig");
        assert_eq!(assistant_msg["content"][1]["type"], "text");
        assert_eq!(assistant_msg["content"][1]["text"], "Final answer");
    }

    #[test]
    fn test_decode_stream_event_anthropic() {
        let model = make_test_model(false);
        let mut builder =
            ResponseBuilder::new(ModelId("m".to_string()), Protocol::AnthropicMessages, None);

        let sse_start = SseEvent {
            event: Some("message_start".to_string()),
            data: r#"{"type": "message_start", "message": {"id": "msg-123", "usage": {"input_tokens": 10, "output_tokens": 0}}}"#.to_string(),
        };

        let evs = decode_stream_event(&model, &sse_start, &mut builder).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::Started { .. }));
    }

    #[test]
    fn test_build_request_image_input() {
        let model = make_test_model(false);

        let inline_image = Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from(vec![0x47, 0x49, 0x46])),
            media_type: Some(mime::IMAGE_GIF),
            detail: None,
        });

        let url_image = Media::Image(ImageMedia {
            source: ImageSource::Url(url::Url::parse("https://example.com/test.png").unwrap()),
            media_type: None,
            detail: None,
        });

        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Media(inline_image), UserPart::Media(url_image)],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
        };

        let parts = build_request(&model, &req).unwrap();
        let body_val: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();

        let messages = body_val["messages"].as_array().unwrap();
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);

        assert_eq!(content[0]["type"], "image");
        let source0 = &content[0]["source"];
        assert_eq!(source0["type"], "base64");
        assert_eq!(source0["media_type"], "image/gif");
        assert_eq!(source0["data"], "R0lG");

        assert_eq!(content[1]["type"], "image");
        let source1 = &content[1]["source"];
        assert_eq!(source1["type"], "url");
        assert_eq!(source1["url"], "https://example.com/test.png");
    }
}

/// Offline fixture matrix for the Anthropic Messages stream decoder
/// (design §19; plan Task 10.2).
#[cfg(test)]
mod fixture_tests {
    use super::decode_stream_event;
    use crate::error::{AiError, StreamProtocolError};
    use crate::protocol::harness;
    use crate::stream::StreamEvent;
    use crate::types::{AssistantPart, Protocol, ReasoningStateKind, StopReason};

    macro_rules! fx {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/anthropic/",
                $name
            ))
        };
    }

    async fn run(name: &'static [u8], chunk: usize) -> Result<Vec<StreamEvent>, AiError> {
        let model = harness::model(Protocol::AnthropicMessages, None);
        harness::drive(&model, decode_stream_event, name, chunk).await
    }

    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn text_with_ping_keepalive() {
        let events = run(fx!("text.sse"), 0).await.unwrap();
        assert_eq!(text_of(&events), "Hello there");
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
        assert_eq!(resp.usage.total_tokens, 15);
    }

    // f8: final usage merges the cumulative cache buckets and the documented
    // thinking-token subset from `message_delta`, not just `output_tokens`.
    #[tokio::test]
    async fn final_usage_merges_cache_and_thinking_tokens() {
        let events = run(fx!("thinking_usage.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        let u = &resp.usage;
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.cache_read_tokens, 40);
        assert_eq!(u.cache_write_tokens, 10);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(
            u.reasoning_tokens, 30,
            "thinking_tokens must not be discarded"
        );
        assert_eq!(u.total_tokens, 100 + 40 + 10 + 50);
    }

    #[tokio::test]
    async fn text_identical_across_byte_boundaries() {
        let data = fx!("text.sse");
        let base = format!("{:?}", run(data, 0).await.unwrap());
        for chunk in 1..=data.len() {
            assert_eq!(
                format!("{:?}", run(data, chunk).await.unwrap()),
                base,
                "chunk {chunk}"
            );
        }
    }

    #[tokio::test]
    async fn thinking_preserves_signature_state() {
        let events = run(fx!("thinking.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        let reasoning = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .unwrap();
        assert_eq!(reasoning.text.as_deref(), Some("Consider the options."));
        let state = reasoning.state.as_ref().expect("signature state");
        assert_eq!(state.protocol, Protocol::AnthropicMessages);
        match &state.kind {
            ReasoningStateKind::AnthropicSignature { signature } => {
                assert_eq!(signature, "c2lnbmF0dXJl");
            }
            other => panic!("expected AnthropicSignature, got {other:?}"),
        }
        assert_eq!(text_of(&events), "Final answer.");
    }

    #[tokio::test]
    async fn redacted_thinking_has_no_text() {
        let events = run(fx!("redacted_thinking.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        let reasoning = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .unwrap();
        assert!(
            reasoning.text.is_none(),
            "redacted reasoning has no visible text"
        );
        match &reasoning.state.as_ref().unwrap().kind {
            ReasoningStateKind::AnthropicRedacted { data } => {
                assert_eq!(data, "RW5jcnlwdGVkQmxvYg==");
            }
            other => panic!("expected AnthropicRedacted, got {other:?}"),
        }
        assert_eq!(text_of(&events), "Done.");
    }

    #[tokio::test]
    async fn single_tool_call() {
        let events = run(fx!("tool_call.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        let tc = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantPart::ToolCall(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.name, "grep");
        assert_eq!(tc.id.0, "toolu_1");
        assert_eq!(
            tc.arguments_value().unwrap(),
            serde_json::json!({"pattern":"foo"})
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls() {
        let events = run(fx!("parallel_tool_calls.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        let calls: Vec<_> = resp
            .message
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantPart::ToolCall(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "alpha");
        assert_eq!(calls[1].name, "beta");
    }

    #[tokio::test]
    async fn malformed_tool_json_is_decode_error() {
        let err = run(fx!("malformed_tool_json.sse"), 0).await.unwrap_err();
        assert!(matches!(err, AiError::Decode(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn stop_reason_variants() {
        assert_eq!(
            harness::finished(&run(fx!("max_tokens.sse"), 0).await.unwrap()).stop_reason,
            StopReason::MaxTokens
        );
        assert_eq!(
            harness::finished(&run(fx!("stop_sequence.sse"), 0).await.unwrap()).stop_reason,
            StopReason::StopSequence
        );
        assert_eq!(
            harness::finished(&run(fx!("pause_turn.sse"), 0).await.unwrap()).stop_reason,
            StopReason::PauseTurn
        );
    }

    #[tokio::test]
    async fn error_event_becomes_provider_error() {
        let err = run(fx!("error_event.sse"), 0).await.unwrap_err();
        match err {
            AiError::Provider(p) => {
                assert_eq!(p.kind.as_deref(), Some("overloaded_error"));
                assert_eq!(p.message, "Overloaded");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn premature_eof() {
        let err = run(fx!("premature_eof.sse"), 0).await.unwrap_err();
        assert!(
            matches!(
                err,
                AiError::StreamProtocol(StreamProtocolError::PrematureEof)
            ),
            "got {err:?}"
        );
    }
}
