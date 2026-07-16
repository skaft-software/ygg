//! OpenAI Responses private wire protocol codec.

use base64::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{AiError, ConfigError, DecodeError, ProviderError};
use crate::protocol::sse::SseEvent;
use crate::protocol::{cache_session_id, prompt_cache_key, HttpRequestParts};
use crate::stream::{ResponseBuilder, StreamEvent};
use crate::types::{
    AssistantPart, ImageSource, Media, Message, Protocol, ReasoningConfig, ReasoningState,
    ReasoningStateKind, Request, StopReason, ToolCallId, ToolChoice, ToolResultPart, Usage,
    UserPart,
};
use crate::validate::validate_request;

// --- Private OpenAI Responses Request DTOs ---

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ResponsesTextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    store: bool,
    // The streaming intent must be in the body, not only the transport. Standard
    // OpenAI Responses needs it to stream, and the ChatGPT Codex backend
    // outright rejects its absence (`{"detail":"Stream must be set to true"}`).
    // This codec is always-streamed (there is no non-streaming Responses decode
    // path — see `decode_stream_event`), so it is unconditionally true.
    stream: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesInputItem {
    Message {
        role: String,
        content: Vec<ResponsesContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: Vec<ResponsesToolResultBlock>,
    },
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        // The Responses API requires `summary` on replayed reasoning items even
        // when the model returned no visible summary (`[]`). Omitting it makes
        // newer Codex models reject the post-tool continuation request.
        summary: Vec<ResponsesReasoningSummary>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesContentPart {
    InputText {
        text: String,
    },
    // Replayed assistant messages are output items, not new user input. Newer
    // Responses/Codex models reject `input_text` under role `assistant`.
    OutputText {
        text: String,
        annotations: Vec<serde_json::Value>,
    },
    InputImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesToolResultBlock {
    InputText {
        text: String,
    },
    InputImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
    },
}

#[derive(Serialize)]
struct ResponsesReasoningSummary {
    r#type: String,
    text: String,
}

#[derive(Serialize)]
struct ResponsesTool {
    r#type: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct ResponsesReasoningConfig {
    effort: String,
    // Request visible summary deltas in addition to encrypted continuation
    // state. Without this, reasoning-capable Codex models think silently.
    summary: &'static str,
}

#[derive(Serialize)]
struct ResponsesTextConfig {
    format: ResponsesFormat,
}

// Only the non-default output formats produce a wire `text.format`; plain text
// output emits no format object, so there is no `Text` variant here.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesFormat {
    JsonObject,
    JsonSchema {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        schema: serde_json::Value,
        strict: bool,
    },
}

// --- Request Builder ---

/// Builds the OpenAI Responses HTTP request parts.
pub(crate) fn build_request(
    model: &crate::catalog::Model,
    req: &Request,
) -> Result<HttpRequestParts, AiError> {
    // 1. Run validation
    let diagnostics = validate_request(
        req,
        &model.spec.capabilities,
        &model.spec.limits,
        Protocol::OpenAiResponses,
        &model.spec.id,
        req.compatibility,
    )?;

    // 2. Map system prompt
    let mut input = Vec::new();
    let has_reasoning = model.spec.capabilities.reasoning.is_some();
    if let Some(ref sys) = req.system {
        let role = if has_reasoning {
            "developer".to_string()
        } else {
            "system".to_string()
        };
        input.push(ResponsesInputItem::Message {
            role,
            content: vec![ResponsesContentPart::InputText { text: sys.clone() }],
        });
    }

    // 3. Map history
    let mut pending_tool_calls = std::collections::BTreeSet::new();
    let mut synthetic_tool_results = std::collections::HashSet::new();
    for msg in &req.messages {
        match msg {
            Message::User(ref user) => {
                let mut content = Vec::new();
                for part in &user.content {
                    match part {
                        UserPart::Text(ref text) => {
                            content.push(ResponsesContentPart::InputText { text: text.clone() });
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

                            let (image_url, file_id) = match &image.source {
                                ImageSource::Url(u) => (Some(u.to_string()), None),
                                ImageSource::Inline(bytes) => {
                                    // No documented default MIME; do not guess a
                                    // wire field (design §75). Validation already
                                    // diagnosed the drop.
                                    let Some(mime_str) = image.media_type.as_ref() else {
                                        continue;
                                    };
                                    (
                                        Some(format!(
                                            "data:{};base64,{}",
                                            mime_str,
                                            BASE64_STANDARD.encode(bytes)
                                        )),
                                        None,
                                    )
                                }
                                ImageSource::ProviderRef(reference) => {
                                    // Design §7: an expired or wrong-protocol
                                    // provider ref is dropped (validation already
                                    // emitted the diagnostic); never serialize an
                                    // invalid file ID.
                                    if !crate::validate::provider_ref_is_usable(
                                        reference,
                                        Protocol::OpenAiResponses,
                                    ) {
                                        continue;
                                    }
                                    (None, Some(reference.id.clone()))
                                }
                            };

                            let detail = image.detail.map(|d| match d {
                                crate::types::ImageDetail::Auto => "auto".to_string(),
                                crate::types::ImageDetail::Low => "low".to_string(),
                                crate::types::ImageDetail::High => "high".to_string(),
                            });

                            content.push(ResponsesContentPart::InputImage {
                                image_url,
                                file_id,
                                detail,
                            });
                        }
                        UserPart::Media(Media::Audio(_)) => {}
                        UserPart::ToolResult(ref tr) => {
                            if synthetic_tool_results.contains(&tr.tool_call_id.0) {
                                continue;
                            }
                            pending_tool_calls.remove(&tr.tool_call_id.0);
                            let mut outputs = Vec::new();
                            for tr_part in &tr.content {
                                match tr_part {
                                    ToolResultPart::Text(ref text) => {
                                        outputs.push(ResponsesToolResultBlock::InputText {
                                            text: text.clone(),
                                        });
                                    }
                                    ToolResultPart::Media(Media::Image(ref image)) => {
                                        match &image.source {
                                            ImageSource::Url(u) => {
                                                outputs.push(
                                                    ResponsesToolResultBlock::InputImage {
                                                        image_url: Some(u.to_string()),
                                                        file_id: None,
                                                    },
                                                );
                                            }
                                            ImageSource::Inline(bytes) => {
                                                // Do not guess a wire MIME (§75);
                                                // drop the part if absent.
                                                if let Some(mime_str) = image.media_type.as_ref() {
                                                    let url_str = format!(
                                                        "data:{};base64,{}",
                                                        mime_str,
                                                        BASE64_STANDARD.encode(bytes)
                                                    );
                                                    outputs.push(
                                                        ResponsesToolResultBlock::InputImage {
                                                            image_url: Some(url_str),
                                                            file_id: None,
                                                        },
                                                    );
                                                }
                                            }
                                            ImageSource::ProviderRef(ref p_ref) => {
                                                // Drop expired/wrong-protocol refs
                                                // rather than serialize an invalid
                                                // file ID (design §7).
                                                if crate::validate::provider_ref_is_usable(
                                                    p_ref,
                                                    Protocol::OpenAiResponses,
                                                ) {
                                                    outputs.push(
                                                        ResponsesToolResultBlock::InputImage {
                                                            image_url: None,
                                                            file_id: Some(p_ref.id.clone()),
                                                        },
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    ToolResultPart::Media(Media::Audio(_)) => {}
                                }
                            }
                            // Preserve canonical order: emit any buffered user
                            // content before this tool-result item (design §11).
                            flush_user_content(&mut input, &mut content);
                            input.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: crate::protocol::normalize_tool_call_id(
                                    &tr.tool_call_id.0,
                                ),
                                output: outputs,
                            });
                        }
                    }
                }
                flush_user_content(&mut input, &mut content);
            }
            Message::Assistant(ref assistant) => {
                if req.compatibility == crate::CompatibilityMode::Lossy {
                    push_synthetic_tool_results(
                        &mut input,
                        &mut pending_tool_calls,
                        &mut synthetic_tool_results,
                    );
                }
                // Preserve canonical part order: buffered assistant text is
                // flushed as a `message` item immediately before each
                // `function_call`/`reasoning` item it precedes, rather than all
                // text being deferred to the end (design §11 immutable replay).
                let mut text_parts = Vec::new();
                for part in &assistant.content {
                    match part {
                        AssistantPart::Text(ref text) => {
                            text_parts.push(text.clone());
                        }
                        AssistantPart::ToolCall(ref tc) => {
                            flush_assistant_text(&mut input, &mut text_parts);
                            pending_tool_calls.insert(tc.id.0.clone());
                            input.push(ResponsesInputItem::FunctionCall {
                                call_id: crate::protocol::normalize_tool_call_id(&tc.id.0),
                                name: tc.name.clone(),
                                arguments: tc.arguments_json.clone(),
                            });
                        }
                        AssistantPart::Reasoning(ref reasoning) => {
                            if let Some(ref state) = reasoning.state {
                                if state.protocol == Protocol::OpenAiResponses
                                    && state.model == model.spec.id
                                {
                                    if let ReasoningStateKind::OpenAiReasoning {
                                        ref item_id,
                                        ref encrypted_content,
                                    } = state.kind
                                    {
                                        flush_assistant_text(&mut input, &mut text_parts);
                                        input.push(ResponsesInputItem::Reasoning {
                                            id: item_id.clone(),
                                            summary: reasoning
                                                .text
                                                .as_ref()
                                                .map(|text| {
                                                    vec![ResponsesReasoningSummary {
                                                        r#type: "summary_text".to_string(),
                                                        text: text.clone(),
                                                    }]
                                                })
                                                .unwrap_or_default(),
                                            encrypted_content: encrypted_content.clone(),
                                        });
                                    }
                                }
                            }
                        }
                        AssistantPart::Media(_) => {}
                    }
                }
                flush_assistant_text(&mut input, &mut text_parts);
            }
        }
    }

    if req.compatibility == crate::CompatibilityMode::Lossy {
        push_synthetic_tool_results(
            &mut input,
            &mut pending_tool_calls,
            &mut synthetic_tool_results,
        );
    }

    // 4. Map tools & tool_choice
    let tools_opt = if req.tools.is_empty() || !model.spec.capabilities.tools {
        None
    } else {
        Some(
            req.tools
                .iter()
                .map(|t| ResponsesTool {
                    r#type: "function".to_string(),
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                })
                .collect(),
        )
    };

    let tool_choice_opt = if !model.spec.capabilities.tools {
        None
    } else {
        match &req.tool_choice {
            ToolChoice::Auto => Some(serde_json::Value::String("auto".to_string())),
            ToolChoice::Required => Some(serde_json::Value::String("required".to_string())),
            ToolChoice::None => Some(serde_json::Value::String("none".to_string())),
            ToolChoice::Named(name) => Some(serde_json::json!({
                "type": "function",
                "name": name
            })),
        }
    };

    // 5. Reasoning Configuration
    let reasoning_opt = if model.spec.capabilities.reasoning.is_some() {
        match req.reasoning {
            ReasoningConfig::Off => None,
            ReasoningConfig::Effort(effort) => {
                let effort_str = match effort {
                    crate::types::ReasoningEffort::Minimal => "minimal".to_string(),
                    crate::types::ReasoningEffort::Low => "low".to_string(),
                    crate::types::ReasoningEffort::Medium => "medium".to_string(),
                    crate::types::ReasoningEffort::High => "high".to_string(),
                };
                Some(ResponsesReasoningConfig {
                    effort: effort_str,
                    summary: "auto",
                })
            }
            ReasoningConfig::Budget(_) => None,
        }
    } else {
        None
    };

    // 6. Text / Output Format Config
    //
    // Design §7: a Lossy structured-output downgrade must actually drop the
    // capability from the wire request, not just emit a diagnostic. Strict mode
    // has already returned `Err` in `validate_request` above, so an unsupported
    // format only reaches here under Lossy — in which case we serialize plain
    // text (`text` omitted) rather than send a `text.format` the model lacks.
    let structured_supported = model.spec.capabilities.structured_output;
    let text_opt = match &req.output_format {
        crate::types::OutputFormat::Text => None,
        _ if !structured_supported => None,
        crate::types::OutputFormat::JsonObject => Some(ResponsesTextConfig {
            format: ResponsesFormat::JsonObject,
        }),
        crate::types::OutputFormat::JsonSchema(ref s) => Some(ResponsesTextConfig {
            format: ResponsesFormat::JsonSchema {
                name: s.name.clone(),
                description: s.description.clone(),
                schema: s.schema.clone(),
                strict: s.strict,
            },
        }),
    };

    // 7. Request Encrypted Reasoning
    let include = if model.spec.capabilities.reasoning.is_some() {
        vec!["reasoning.encrypted_content".to_string()]
    } else {
        vec![]
    };

    // Only forward an explicit caller cap. The Responses API treats this as
    // optional, and the ChatGPT Codex backend rejects it outright
    // (`{"detail":"Unsupported parameter: max_output_tokens"}`), so we never
    // synthesize a default from the local capacity limit.
    let max_output_tokens = req.max_output_tokens;

    let responses_req = ResponsesRequest {
        model: model.spec.api_name.clone(),
        input,
        tools: tools_opt,
        tool_choice: tool_choice_opt,
        max_output_tokens,
        temperature: req.temperature,
        reasoning: reasoning_opt,
        text: text_opt,
        prompt_cache_key: prompt_cache_key(req),
        prompt_cache_retention: (req.cache_retention == crate::types::CacheRetention::Long
            && model.spec.cache.supports_long_retention)
            .then_some("24h"),
        include,
        store: false,
        stream: true,
    };

    let body_bytes = serde_json::to_vec(&responses_req)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let url = model
        .endpoint
        .base_url
        .join("responses")
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let mut headers = http::HeaderMap::new();
    if let Some(session_id) = cache_session_id(req) {
        let value = http::HeaderValue::from_str(session_id)
            .map_err(|_| ConfigError::InvalidHeader("x-client-request-id".into()))?;
        headers.insert(
            http::HeaderName::from_static("x-client-request-id"),
            value.clone(),
        );
        if model.spec.cache.send_session_id_header {
            headers.insert(http::HeaderName::from_static("session_id"), value);
        }
    }

    Ok(HttpRequestParts {
        url,
        headers,
        body: bytes::Bytes::from(body_bytes),
        streaming: true,
        diagnostics,
    })
}

/// Flush buffered user content parts as a `message` item, preserving canonical
/// order relative to interleaved `function_call_output` items (design §11).
fn flush_user_content(
    input: &mut Vec<ResponsesInputItem>,
    content: &mut Vec<ResponsesContentPart>,
) {
    if !content.is_empty() {
        input.push(ResponsesInputItem::Message {
            role: "user".to_string(),
            content: std::mem::take(content),
        });
    }
}

/// Flush buffered assistant text as a `message` item, preserving canonical
/// order relative to interleaved `function_call`/`reasoning` items (design §11
/// immutable replay). Consecutive text parts are joined; a `\n` boundary only
/// appears where the canonical parts were themselves adjacent text.
fn flush_assistant_text(input: &mut Vec<ResponsesInputItem>, text_parts: &mut Vec<String>) {
    if !text_parts.is_empty() {
        input.push(ResponsesInputItem::Message {
            role: "assistant".to_string(),
            content: vec![ResponsesContentPart::OutputText {
                text: std::mem::take(text_parts).join("\n"),
                annotations: vec![],
            }],
        });
    }
}

fn push_synthetic_tool_results(
    input: &mut Vec<ResponsesInputItem>,
    pending: &mut std::collections::BTreeSet<String>,
    synthetic: &mut std::collections::HashSet<String>,
) {
    for call_id in std::mem::take(pending) {
        synthetic.insert(call_id.clone());
        input.push(ResponsesInputItem::FunctionCallOutput {
            // Wire write: normalize to match the paired `function_call` above.
            call_id: crate::protocol::normalize_tool_call_id(&call_id),
            output: vec![ResponsesToolResultBlock::InputText {
                text: "Tool execution result was not supplied by the caller.".to_string(),
            }],
        });
    }
}

// --- SSE Chunk / Responses Response DTOs ---

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesSseEvent {
    #[serde(rename = "response.created")]
    ResponseCreated { response: ResponsesResponseIdBlock },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        output_index: usize,
        item: ResponsesResponseItem,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        output_index: usize,
        content_index: usize,
        part: ResponsesContentPartAdded,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        output_index: usize,
        #[serde(default)]
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        output_index: usize,
        #[serde(default)]
        content_index: usize,
    },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta { output_index: usize, delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryDelta { output_index: usize, delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { output_index: usize, delta: String },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone { output_index: usize },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        output_index: usize,
        item: ResponsesResponseItemDone,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        response: ResponsesResponseCompletedBlock,
    },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete {
        response: ResponsesResponseIncompleteBlock,
    },
    #[serde(rename = "response.failed")]
    ResponseFailed {
        response: ResponsesResponseFailedBlock,
    },
    // Top-level stream error event (apidocs openai-responses
    // 07-streaming-events.md §error: `{type:"error", code, message, param,
    // sequence_number}`). Distinct from `response.failed`, which nests the error
    // under `response.error`. Without this branch `#[serde(other)]` would swallow
    // it and the stream would surface `PrematureEof` instead of the real cause.
    #[serde(rename = "error")]
    StreamError {
        #[serde(default)]
        code: Option<String>,
        message: String,
    },
    // Out-of-scope event families
    #[serde(other)]
    IgnoredEvent,
}

#[derive(Deserialize)]
struct ResponsesResponseIdBlock {
    id: String,
}

#[derive(Deserialize)]
struct ResponsesContentPartAdded {
    r#type: String,
}

#[derive(Deserialize)]
struct ResponsesResponseItem {
    id: String,
    r#type: String,
    #[serde(default)]
    name: Option<String>,
    // A function_call item carries a `call_id` that pairs with its
    // `function_call_output` (design §12.2); prefer it over the item `id`.
    #[serde(default)]
    call_id: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesResponseItemDone {
    id: String,
    r#type: String,
    #[serde(default)]
    encrypted_content: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesResponseCompletedBlock {
    // `usage` is nullable in the Responses object (apidocs
    // openai-responses/01-responses.md: `usage: null` on non-terminal snapshots,
    // populated on completion). Model it as optional so a documented terminal
    // event without usage still decodes to a default-usage `Finished`.
    #[serde(default)]
    usage: Option<ResponsesUsageDto>,
}

#[derive(Deserialize)]
struct ResponsesResponseIncompleteBlock {
    // The documented field is `incomplete_details` (object with `reason`), not
    // `status_details` (apidocs openai-responses/01-responses.md:6013,15394).
    incomplete_details: ResponsesIncompleteDetailsDto,
    #[serde(default)]
    usage: Option<ResponsesUsageDto>,
}

#[derive(Deserialize)]
struct ResponsesIncompleteDetailsDto {
    reason: String,
}

#[derive(Deserialize)]
struct ResponsesResponseFailedBlock {
    error: ResponsesErrorDto,
}

#[derive(Deserialize)]
struct ResponsesErrorDto {
    code: String,
    message: String,
}

// OpenAI Responses usage uses `input_tokens`/`output_tokens` (NOT the Chat
// `prompt_tokens`/`completion_tokens`), with cache + reasoning detail objects
// (design §15; docs/research/apidocs/openai-responses/02-create.md).
#[derive(Deserialize)]
struct ResponsesUsageDto {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<ResponsesOutputTokensDetails>,
}

#[derive(Deserialize)]
struct ResponsesInputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Deserialize)]
struct ResponsesOutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

// --- Decode Implementations ---
//
// OpenAI Responses is always streamed (design §12.2); there is no non-streaming
// decode path, so this codec deliberately exposes none.

fn emit_event(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    ev: StreamEvent,
) -> Result<(), AiError> {
    builder.on_event(&ev)?;
    events.push(ev);
    Ok(())
}

/// Decodes a streaming SSE event from OpenAI Responses, emitting StreamEvents.
pub(crate) fn decode_stream_event(
    _model: &crate::catalog::Model,
    sse_event: &SseEvent,
    builder: &mut ResponseBuilder,
) -> Result<Vec<StreamEvent>, AiError> {
    let raw_data = sse_event.data.trim();
    if raw_data.is_empty() {
        return Ok(vec![]);
    }

    let event: ResponsesSseEvent = serde_json::from_str(raw_data)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let mut events = Vec::new();

    match event {
        ResponsesSseEvent::ResponseCreated { response } => {
            builder.response_id = Some(response.id.clone());
            emit_event(
                &mut events,
                builder,
                StreamEvent::Started {
                    response_id: Some(response.id),
                },
            )?;
        }
        ResponsesSseEvent::OutputItemAdded { output_index, item } => {
            if item.r#type == "function_call" {
                let key = format!("item_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                if let Some(name) = item.name {
                    let call_id = item.call_id.unwrap_or(item.id);
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ToolCallStart {
                            index: canonical_idx,
                            id: ToolCallId(call_id),
                            name,
                        },
                    )?;
                }
            }
        }
        ResponsesSseEvent::ContentPartAdded {
            output_index,
            content_index,
            part,
        } => {
            if part.r#type == "output_text" {
                let key = format!("item_{}_content_{}", output_index, content_index);
                let canonical_idx = get_canonical_index(builder, &key);
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextStart {
                        index: canonical_idx,
                    },
                )?;
            }
        }
        ResponsesSseEvent::OutputTextDelta {
            output_index,
            content_index,
            delta,
        } => {
            if !delta.is_empty() {
                let key = format!("item_{}_content_{}", output_index, content_index);
                let canonical_idx = get_canonical_index(builder, &key);
                if !builder.text_buffers.contains_key(&canonical_idx) {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::TextStart {
                            index: canonical_idx,
                        },
                    )?;
                }
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextDelta {
                        index: canonical_idx,
                        delta,
                    },
                )?;
            }
        }
        ResponsesSseEvent::OutputTextDone {
            output_index,
            content_index,
        } => {
            let key = format!("item_{}_content_{}", output_index, content_index);
            let canonical_idx = get_canonical_index(builder, &key);
            if builder.text_buffers.contains_key(&canonical_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextEnd {
                        index: canonical_idx,
                    },
                )?;
            }
        }
        ResponsesSseEvent::ReasoningTextDelta {
            output_index,
            delta,
        } => {
            if !delta.is_empty() {
                let key = format!("reasoning_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                if !builder.reasoning_text_buffers.contains_key(&canonical_idx) {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningStart {
                            index: canonical_idx,
                        },
                    )?;
                }
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ReasoningDelta {
                        index: canonical_idx,
                        delta,
                    },
                )?;
            }
        }
        ResponsesSseEvent::ReasoningSummaryDelta {
            output_index,
            delta,
        } => {
            if !delta.is_empty() {
                let key = format!("reasoning_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                if !builder.reasoning_text_buffers.contains_key(&canonical_idx) {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningStart {
                            index: canonical_idx,
                        },
                    )?;
                }
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ReasoningDelta {
                        index: canonical_idx,
                        delta,
                    },
                )?;
            }
        }
        ResponsesSseEvent::FunctionCallArgumentsDelta {
            output_index,
            delta,
        } => {
            if !delta.is_empty() {
                let key = format!("item_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallArgsDelta {
                        index: canonical_idx,
                        delta,
                    },
                )?;
            }
        }
        ResponsesSseEvent::FunctionCallArgumentsDone { output_index } => {
            let key = format!("item_{}", output_index);
            let canonical_idx = get_canonical_index(builder, &key);
            if builder.tool_call_builders.contains_key(&canonical_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallEnd {
                        index: canonical_idx,
                    },
                )?;
            }
        }
        ResponsesSseEvent::OutputItemDone { output_index, item } => {
            if item.r#type == "reasoning" {
                let key = format!("reasoning_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                let had_visible_text = builder.reasoning_text_buffers.contains_key(&canonical_idx);
                if had_visible_text {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningEnd {
                            index: canonical_idx,
                        },
                    )?;
                } else if item.encrypted_content.is_some() {
                    // Opaque reasoning with no visible delta (design §6.3/§14):
                    // still surface a reasoning part so the opaque `item_id`/
                    // `encrypted_content` is preserved. Without an observed part
                    // (`ReasoningStart`), `ResponseBuilder::finish` — which only
                    // assembles observed indices — would silently drop the state.
                    // The empty text buffer becomes `ReasoningPart.text = None`.
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningStart {
                            index: canonical_idx,
                        },
                    )?;
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningEnd {
                            index: canonical_idx,
                        },
                    )?;
                }

                if item.encrypted_content.is_some() {
                    builder.set_reasoning_state(
                        canonical_idx,
                        ReasoningState {
                            model: builder.model.clone(),
                            protocol: Protocol::OpenAiResponses,
                            kind: ReasoningStateKind::OpenAiReasoning {
                                item_id: Some(item.id),
                                encrypted_content: item.encrypted_content,
                            },
                        },
                    );
                }
            }
        }
        ResponsesSseEvent::ResponseCompleted { response } => {
            // Design §15: a completed response that produced a function call is a
            // tool-use stop; otherwise it is a normal end-of-turn.
            let stop = if builder.tool_call_builders.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            };
            builder.set_stop_reason(stop);
            // Usage is optional on the wire; only emit a `Usage` event when the
            // provider reported one so `Finished.usage` is a default rather than a
            // misleading all-zero count.
            if let Some(usage) = &response.usage {
                let u = map_usage(usage)?;
                emit_event(&mut events, builder, StreamEvent::Usage(u))?;
            }

            let resp = builder.finish_mut()?;
            emit_event(&mut events, builder, StreamEvent::Finished(resp))?;
        }
        ResponsesSseEvent::ResponseIncomplete { response } => {
            let stop = match response.incomplete_details.reason.as_str() {
                "max_output_tokens" => StopReason::MaxTokens,
                "content_filter" => StopReason::Refusal,
                other => StopReason::Other(other.to_string()),
            };
            builder.set_stop_reason(stop);

            if let Some(usage) = &response.usage {
                let u = map_usage(usage)?;
                emit_event(&mut events, builder, StreamEvent::Usage(u))?;
            }

            let resp = builder.finish_mut()?;
            emit_event(&mut events, builder, StreamEvent::Finished(resp))?;
        }
        ResponsesSseEvent::ResponseFailed { response } => {
            return Err(AiError::Provider(ProviderError {
                code: Some(response.error.code),
                kind: None,
                message: response.error.message,
                request_id: None,
            }));
        }
        ResponsesSseEvent::StreamError { code, message } => {
            return Err(AiError::Provider(ProviderError {
                code,
                kind: None,
                message,
                request_id: None,
            }));
        }
        ResponsesSseEvent::IgnoredEvent => {}
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

fn map_usage(usage: &ResponsesUsageDto) -> Result<Usage, AiError> {
    // Design §15: OpenAI `input_tokens` INCLUDES cache, so cache read + write are
    // subtracted out to keep the canonical buckets disjoint (full-rate input only).
    let cache_read = usage
        .input_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens)
        .unwrap_or(0);
    let cache_write = usage
        .input_tokens_details
        .as_ref()
        .map(|d| d.cache_write_tokens)
        .unwrap_or(0);
    let reasoning = usage
        .output_tokens_details
        .as_ref()
        .map(|d| d.reasoning_tokens)
        .unwrap_or(0);
    let input = usage
        .input_tokens
        .checked_sub(cache_read)
        .and_then(|value| value.checked_sub(cache_write))
        .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?;
    if reasoning > usage.output_tokens {
        return Err(AiError::Decode(DecodeError::UsageUnderflow));
    }
    let total = if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        input
            .checked_add(cache_read)
            .and_then(|value| value.checked_add(cache_write))
            .and_then(|value| value.checked_add(usage.output_tokens))
            .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?
    };
    Ok(Usage {
        input_tokens: input,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cache_write_1h_tokens: 0,
        output_tokens: usage.output_tokens,
        reasoning_tokens: reasoning,
        total_tokens: total,
    })
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Model;
    use crate::types::{
        Capabilities, Endpoint, EndpointId, ImageMedia, ImageSource, JsonSchemaFormat, Media,
        Message, ModalitySet, ModelId, ModelLimits, ModelSpec, OutputFormat, OutputModalities,
        ProviderMediaRef, ReasoningConfig, Request, ToolChoice, UserMessage, UserPart,
    };
    use crate::CompatibilityMode;
    use std::sync::Arc;

    fn without_structured_output(model: &Model) -> Model {
        let mut spec = (*model.spec).clone();
        spec.capabilities.structured_output = false;
        Model {
            spec: Arc::new(spec),
            endpoint: model.endpoint.clone(),
        }
    }

    fn user_req(content: Vec<UserPart>, compatibility: CompatibilityMode) -> Request {
        Request {
            system: None,
            messages: vec![Message::User(UserMessage { content })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        }
    }

    fn make_test_model(reasoning: bool) -> Model {
        let spec = ModelSpec {
            id: ModelId("test-o1".to_string()),
            endpoint: EndpointId("responses-ep".to_string()),
            api_name: "o1-2024-12-17".to_string(),
            protocol: Protocol::OpenAiResponses,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none().with(crate::types::Modality::Image),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: if reasoning {
                    Some(crate::types::ReasoningCapability {
                        control: crate::types::ReasoningControl::Effort,
                        exposes_text: true,
                        preserves_state: true,
                        effort_budgets: None,
                        openai_chat_mode: crate::types::OpenAiChatReasoningMode::Standard,
                    })
                } else {
                    None
                },
                structured_output: true,
            },
            limits: ModelLimits {
                context_window: 200000,
                max_output_tokens: 16384,
            },
            pricing: None,
            cache: crate::types::CacheCompatibility::default(),
        };

        let ep = Endpoint {
            id: EndpointId("responses-ep".to_string()),
            base_url: url::Url::parse("https://api.openai.com/v1/").unwrap(),
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
    fn test_build_request_responses_basic() {
        let model = make_test_model(true);
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
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };

        let parts = build_request(&model, &req).unwrap();
        assert_eq!(parts.url.to_string(), "https://api.openai.com/v1/responses");

        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["model"], "o1-2024-12-17");
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "System instructions"
        );
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["content"][0]["text"], "Hello");
    }

    #[test]
    fn reasoning_request_streams_summaries_and_replay_keeps_empty_summary() {
        let model = make_test_model(true);
        let mut req = user_req(
            vec![UserPart::Text("follow up".to_string())],
            CompatibilityMode::Strict,
        );
        req.reasoning = ReasoningConfig::Effort(crate::types::ReasoningEffort::High);
        req.messages = vec![
            Message::User(UserMessage {
                content: vec![UserPart::Text("initial".to_string())],
            }),
            Message::Assistant(crate::types::AssistantMessage {
                content: vec![AssistantPart::Reasoning(crate::types::ReasoningPart {
                    text: None,
                    state: Some(ReasoningState {
                        protocol: Protocol::OpenAiResponses,
                        model: model.spec.id.clone(),
                        kind: ReasoningStateKind::OpenAiReasoning {
                            item_id: Some("rs_terra".to_string()),
                            encrypted_content: Some("encrypted".to_string()),
                        },
                    }),
                })],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiResponses,
            }),
            Message::User(UserMessage {
                content: vec![UserPart::Text("follow up".to_string())],
            }),
        ];

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][1]["id"], "rs_terra");
        assert_eq!(body["input"][1]["summary"], serde_json::json!([]));
        assert_eq!(body["input"][1]["encrypted_content"], "encrypted");
    }

    #[test]
    fn completed_assistant_history_uses_output_text_for_the_next_turn() {
        let model = make_test_model(false);
        let mut req = user_req(
            vec![UserPart::Text("second prompt".to_string())],
            CompatibilityMode::Strict,
        );
        req.messages = vec![
            Message::User(UserMessage {
                content: vec![UserPart::Text("first prompt".to_string())],
            }),
            Message::Assistant(crate::types::AssistantMessage {
                content: vec![AssistantPart::Text("first response".to_string())],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiResponses,
            }),
            Message::User(UserMessage {
                content: vec![UserPart::Text("second prompt".to_string())],
            }),
        ];

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(body["input"][1]["content"][0]["text"], "first response");
        assert_eq!(
            body["input"][1]["content"][0]["annotations"],
            serde_json::json!([])
        );
        assert_eq!(body["input"][2]["role"], "user");
        assert_eq!(body["input"][2]["content"][0]["type"], "input_text");
    }

    #[test]
    fn cache_retention_controls_responses_key_and_headers() {
        let model = make_test_model(false);
        let mut req = user_req(
            vec![UserPart::Text("hello".to_string())],
            CompatibilityMode::Strict,
        );
        req.session_id = Some("a".repeat(70));

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(
            body["prompt_cache_key"].as_str().unwrap().chars().count(),
            64
        );
        assert!(body.get("prompt_cache_retention").is_none());
        assert_eq!(
            parts.headers["session_id"],
            req.session_id.as_deref().unwrap()
        );
        assert_eq!(
            parts.headers["x-client-request-id"],
            req.session_id.as_deref().unwrap()
        );

        req.cache_retention = crate::types::CacheRetention::Long;
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["prompt_cache_retention"], "24h");

        req.cache_retention = crate::types::CacheRetention::None;
        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
        assert!(parts.headers.get("session_id").is_none());
        assert!(parts.headers.get("x-client-request-id").is_none());
    }

    #[test]
    fn responses_compat_can_disable_standard_session_and_long_retention() {
        let mut model = make_test_model(false);
        let cache = &mut Arc::make_mut(&mut model.spec).cache;
        cache.send_session_id_header = false;
        cache.supports_long_retention = false;

        let mut req = user_req(
            vec![UserPart::Text("hello".to_string())],
            CompatibilityMode::Strict,
        );
        req.cache_retention = crate::types::CacheRetention::Long;
        req.session_id = Some("codex-session".to_string());

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["prompt_cache_key"], "codex-session");
        assert!(body.get("prompt_cache_retention").is_none());
        assert!(parts.headers.get("session_id").is_none());
        assert_eq!(parts.headers["x-client-request-id"], "codex-session");
    }

    #[test]
    fn test_build_request_responses_tool_shape() {
        let model = make_test_model(false);
        let mut req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("hello".to_string())],
            })],
            tools: vec![crate::types::ToolDef {
                name: "lookup".to_string(),
                description: "lookup data".to_string(),
                parameters: serde_json::json!({"type":"object"}),
            }],
            tool_choice: ToolChoice::Named("lookup".to_string()),
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };
        let mut capable = (*model.spec).clone();
        capable.capabilities.tools = true;
        let model = crate::catalog::Model {
            spec: std::sync::Arc::new(capable),
            endpoint: model.endpoint.clone(),
        };
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"]["name"], "lookup");
        assert!(body.get("stop").is_none());
        assert!(body.get("max_completion_tokens").is_none());

        req.stop.push("END".to_string());
        assert!(matches!(
            build_request(&model, &req),
            Err(AiError::Unsupported(crate::UnsupportedError::StopSequences))
        ));
    }

    #[test]
    fn test_decode_stream_event_responses() {
        let model = make_test_model(false);
        let mut builder =
            ResponseBuilder::new(ModelId("m".to_string()), Protocol::OpenAiResponses, None);

        let sse_created = SseEvent {
            event: None,
            data: r#"{"type": "response.created", "response": {"id": "resp-123"}}"#.to_string(),
        };

        let evs = decode_stream_event(&model, &sse_created, &mut builder).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::Started { .. }));
    }

    #[test]
    fn test_build_request_image_input() {
        let model = make_test_model(false);

        let inline_image = Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from(vec![0x47, 0x49, 0x46])),
            media_type: Some(mime::IMAGE_GIF),
            detail: Some(crate::types::ImageDetail::Low),
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
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };

        let parts = build_request(&model, &req).unwrap();
        let body_val: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();

        let input_items = body_val["input"].as_array().unwrap();
        assert_eq!(input_items.len(), 1);
        let parts_array = input_items[0]["content"].as_array().unwrap();
        assert_eq!(parts_array.len(), 2);

        assert_eq!(parts_array[0]["type"], "input_image");
        assert_eq!(
            parts_array[0]["image_url"].as_str(),
            Some("data:image/gif;base64,R0lG")
        );
        assert_eq!(parts_array[0]["detail"].as_str(), Some("low"));

        assert_eq!(parts_array[1]["type"], "input_image");
        assert_eq!(
            parts_array[1]["image_url"].as_str(),
            Some("https://example.com/test.png")
        );
        assert!(parts_array[1]["detail"].is_null());
    }

    // f3: a Lossy structured-output downgrade must drop `text.format`, not just
    // emit a diagnostic.
    #[test]
    fn lossy_structured_output_downgrade_omits_text_format() {
        let model = without_structured_output(&make_test_model(false));
        let mut req = user_req(
            vec![UserPart::Text("hi".to_string())],
            CompatibilityMode::Lossy,
        );
        req.output_format = OutputFormat::JsonSchema(JsonSchemaFormat {
            name: "Out".to_string(),
            description: None,
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        });

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert!(
            body.get("text").is_none(),
            "downgraded request must not serialize `text.format`: {body}"
        );
        assert!(parts
            .diagnostics
            .iter()
            .any(|d| d.code == "downgraded_output_format"));

        // Strict still rejects outright.
        req.compatibility = CompatibilityMode::Strict;
        assert!(matches!(
            build_request(&model, &req),
            Err(AiError::Unsupported(
                crate::UnsupportedError::StructuredOutput
            ))
        ));
    }

    // f4: an expired provider ref is dropped from the wire (Lossy) with a
    // diagnostic; Strict rejects it.
    #[test]
    fn lossy_expired_provider_ref_is_dropped() {
        let model = make_test_model(false);
        let expired = UserPart::Media(Media::Image(ImageMedia {
            source: ImageSource::ProviderRef(ProviderMediaRef {
                protocol: Protocol::OpenAiResponses,
                id: "file_expired".to_string(),
                expires_at: Some(std::time::UNIX_EPOCH),
            }),
            media_type: None,
            detail: None,
        }));
        let req = user_req(vec![expired], CompatibilityMode::Lossy);

        let parts = build_request(&model, &req).unwrap();
        let body = String::from_utf8(parts.body.to_vec()).unwrap();
        assert!(
            !body.contains("file_expired"),
            "expired provider ref must not be serialized: {body}"
        );
        assert!(parts
            .diagnostics
            .iter()
            .any(|d| d.code == "dropped_expired_media_ref"));
    }

    // f10: an inline image with no media type is dropped rather than defaulted to
    // a guessed `image/jpeg` (design §75).
    #[test]
    fn lossy_inline_image_without_media_type_is_dropped() {
        let model = make_test_model(false);
        let img = UserPart::Media(Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from(vec![1, 2, 3])),
            media_type: None,
            detail: None,
        }));
        let req = user_req(vec![img], CompatibilityMode::Lossy);

        let parts = build_request(&model, &req).unwrap();
        let body = String::from_utf8(parts.body.to_vec()).unwrap();
        assert!(
            !body.contains("image/jpeg") && !body.contains("input_image"),
            "inline image without media type must be dropped, not guessed: {body}"
        );
        assert!(parts
            .diagnostics
            .iter()
            .any(|d| d.code == "dropped_image_media_type"));
    }
}

/// Offline fixture matrix for the OpenAI Responses stream decoder
/// (design §19; plan Task 11.2).
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
                "/tests/fixtures/openai_responses/",
                $name
            ))
        };
    }

    async fn run(name: &'static [u8], chunk: usize) -> Result<Vec<StreamEvent>, AiError> {
        let model = harness::model(Protocol::OpenAiResponses, None);
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
    async fn plain_text() {
        let events = run(fx!("plain_text.sse"), 0).await.unwrap();
        assert_eq!(text_of(&events), "Hello world");
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn plain_text_identical_across_byte_boundaries() {
        let data = fx!("plain_text.sse");
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
    async fn encrypted_reasoning_state_preserved() {
        let events = run(fx!("reasoning_encrypted.sse"), 0).await.unwrap();
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
        assert_eq!(reasoning.text.as_deref(), Some("Let me reason carefully."));
        match &reasoning.state.as_ref().unwrap().kind {
            ReasoningStateKind::OpenAiReasoning {
                item_id,
                encrypted_content,
            } => {
                assert_eq!(item_id.as_deref(), Some("rs_1"));
                assert_eq!(encrypted_content.as_deref(), Some("RU5DUllQVEVE"));
            }
            other => panic!("expected OpenAiReasoning, got {other:?}"),
        }
        assert_eq!(text_of(&events), "Answer: 42");
        assert_eq!(resp.usage.reasoning_tokens, 18);
    }

    #[tokio::test]
    async fn reasoning_summary_deltas_stream_and_preserve_state() {
        let events = run(fx!("reasoning_summary.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        let reasoning = resp
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::Reasoning(reasoning) => Some(reasoning),
                _ => None,
            })
            .expect("reasoning summary must be surfaced");
        assert_eq!(reasoning.text.as_deref(), Some("Planning briefly."));
        match &reasoning.state.as_ref().unwrap().kind {
            ReasoningStateKind::OpenAiReasoning {
                item_id,
                encrypted_content,
            } => {
                assert_eq!(item_id.as_deref(), Some("rs_summary"));
                assert_eq!(
                    encrypted_content.as_deref(),
                    Some("RU5DUllQVEVEX1NVTU1BUlk=")
                );
            }
            other => panic!("expected OpenAiReasoning, got {other:?}"),
        }
        assert_eq!(text_of(&events), "DONE");
    }

    #[tokio::test]
    async fn tool_call_uses_call_id_and_tool_use_stop() {
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
        assert_eq!(tc.id.0, "call_1");
        assert_eq!(tc.name, "grep");
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
        assert_eq!(calls[0].id.0, "call_a");
        assert_eq!(calls[1].id.0, "call_b");
    }

    #[tokio::test]
    async fn malformed_tool_json_is_decode_error() {
        let err = run(fx!("malformed_tool_json.sse"), 0).await.unwrap_err();
        assert!(matches!(err, AiError::Decode(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn incomplete_maps_to_max_tokens() {
        let events = run(fx!("incomplete_max_tokens.sse"), 0).await.unwrap();
        assert_eq!(
            harness::finished(&events).stop_reason,
            StopReason::MaxTokens
        );
    }

    #[tokio::test]
    async fn out_of_scope_event_is_ignored() {
        let events = run(fx!("ignored_event.sse"), 0).await.unwrap();
        assert_eq!(text_of(&events), "ok");
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
    }

    #[tokio::test]
    async fn response_failed_becomes_provider_error() {
        let err = run(fx!("response_failed.sse"), 0).await.unwrap_err();
        match err {
            AiError::Provider(p) => {
                assert_eq!(p.code.as_deref(), Some("server_error"));
                assert_eq!(p.message, "boom");
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

    // f9: a documented top-level `error` event is surfaced as `Provider`, not
    // swallowed by `#[serde(other)]` into a `PrematureEof`.
    #[tokio::test]
    async fn top_level_error_event_becomes_provider_error() {
        let err = run(fx!("stream_error.sse"), 0).await.unwrap_err();
        match err {
            AiError::Provider(p) => {
                assert_eq!(p.code.as_deref(), Some("ERR_SOMETHING"));
                assert_eq!(p.message, "Something went wrong");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    // f5: opaque reasoning with no visible delta must still surface a reasoning
    // part carrying the item_id/encrypted_content (else it is silently dropped).
    #[tokio::test]
    async fn opaque_reasoning_without_text_is_preserved() {
        let events = run(fx!("reasoning_opaque_no_text.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        let reasoning = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .expect("opaque reasoning part must be present");
        assert_eq!(reasoning.text, None, "opaque reasoning carries no text");
        match &reasoning.state.as_ref().unwrap().kind {
            ReasoningStateKind::OpenAiReasoning {
                item_id,
                encrypted_content,
            } => {
                assert_eq!(item_id.as_deref(), Some("rs_9"));
                assert_eq!(encrypted_content.as_deref(), Some("T1BBUVVF"));
            }
            other => panic!("expected OpenAiReasoning, got {other:?}"),
        }
        assert_eq!(text_of(&events), "Answer");
    }

    // f2: a completed response without a `usage` object still decodes; usage
    // falls back to the default and no `Usage` event is emitted.
    #[tokio::test]
    async fn completed_without_usage_defaults() {
        let events = run(fx!("completed_no_usage.sse"), 0).await.unwrap();
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Usage(_))),
            "no Usage event should be emitted when usage is absent"
        );
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage, crate::types::Usage::default());
        assert_eq!(text_of(&events), "hi");
    }
}
