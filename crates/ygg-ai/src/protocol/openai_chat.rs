//! OpenAI Chat Completions private wire protocol codec.

use base64::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{AiError, ConfigError, DecodeError};
use crate::protocol::sse::SseEvent;
use crate::protocol::{
    cache_control, cache_session_id, prompt_cache_key, CacheControl, HttpRequestParts,
};
use crate::stream::{ResponseBuilder, StreamEvent};
use crate::types::{
    AssistantMessage, AssistantPart, AudioFormat, AudioMedia, AudioPayload, AudioVoice,
    ImageSource, Media, Message, OpenAiChatReasoningMode, OutputFormat, OutputModalities, Protocol,
    ProviderMediaRef, ReasoningConfig, ReasoningPart, Request, Response, StopReason, ToolCall,
    ToolCallId, ToolChoice, ToolResultPart, Usage, UserPart,
};
use crate::validate::validate_request;

// --- Private OpenAI Chat Request DTOs ---

#[derive(Serialize)]
struct ChatCompletionsRequest {
    model: String,
    messages: Vec<ChatCompletionsMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ChatThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ChatResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modalities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<ChatAudioOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<&'static str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<ChatStreamOptions>,
}

#[derive(Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
}

/// DeepSeek's documented OpenAI-compatible thinking toggle.
///
/// See <https://api-docs.deepseek.com/guides/thinking_mode>.
#[derive(Serialize)]
struct ChatThinkingConfig {
    r#type: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "role")]
enum ChatCompletionsMessage {
    #[serde(rename = "developer")]
    Developer { content: ChatInstructionContent },
    #[serde(rename = "system")]
    System { content: ChatInstructionContent },
    #[serde(rename = "user")]
    User { content: Vec<ChatContentPart> },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ChatInstructionContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ChatToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<ChatAssistantAudioRef>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum ChatInstructionContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatContentPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ImageUrl {
        image_url: ChatImageUrl,
    },
    InputAudio {
        input_audio: ChatInputAudio,
    },
}

#[derive(Serialize)]
struct ChatImageUrl {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Serialize)]
struct ChatInputAudio {
    data: String,
    format: String,
}

#[derive(Serialize)]
struct ChatAssistantAudioRef {
    id: String,
}

#[derive(Serialize)]
struct ChatToolCall {
    id: String,
    r#type: String,
    function: ChatFunctionCall,
}

#[derive(Serialize)]
struct ChatFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ChatTool {
    r#type: String,
    function: ChatFunctionDef,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct ChatFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatResponseFormat {
    JsonObject,
    JsonSchema { json_schema: ChatJsonSchema },
}

#[derive(Serialize)]
struct ChatJsonSchema {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    schema: serde_json::Value,
    strict: bool,
}

#[derive(Serialize)]
struct ChatAudioOptions {
    voice: serde_json::Value, // string or object
    format: String,
}

// --- Private OpenAI Chat Response DTOs ---

#[derive(Deserialize)]
struct ChatCompletionsResponse {
    id: String,
    choices: Vec<ChatChoice>,
    usage: ChatUsage,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    // `role` is always "assistant" here and is not needed after decode; the wire
    // field is ignored rather than stored.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatResponseMessageToolCall>>,
    #[serde(default)]
    audio: Option<ChatAudioResponse>,
}

#[derive(Deserialize)]
struct ChatResponseMessageToolCall {
    id: String,
    function: ChatResponseMessageFunction,
}

#[derive(Deserialize)]
struct ChatResponseMessageFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatAudioResponse {
    id: String,
    data: String,
    transcript: String,
    expires_at: u64,
}

#[derive(Deserialize)]
struct ChatUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<ChatPromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<ChatCompletionTokensDetails>,
}

#[derive(Deserialize, Default)]
struct ChatPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct ChatCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Deserialize)]
struct ChatChunk {
    id: String,
    choices: Vec<ChatChunkChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChunkChoice {
    delta: ChatChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatChunkToolCall>>,
}

#[derive(Deserialize)]
struct ChatChunkToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatChunkFunction>,
}

#[derive(Deserialize)]
struct ChatChunkFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// --- Core Codec Implementations ---

/// Builds the OpenAI Chat Completions HTTP request parts.
pub(crate) fn build_request(
    model: &crate::catalog::Model,
    req: &Request,
) -> Result<HttpRequestParts, AiError> {
    // 1. Run validation
    let diagnostics = validate_request(
        req,
        &model.spec.capabilities,
        &model.spec.limits,
        Protocol::OpenAiChat,
        &model.spec.id,
        req.compatibility,
    )?;

    // 2. Map system prompt
    let reasoning_capability = model.spec.capabilities.reasoning.as_ref();
    let has_reasoning = reasoning_capability.is_some();
    let deepseek_thinking = matches!(
        reasoning_capability.map(|capability| capability.openai_chat_mode),
        Some(OpenAiChatReasoningMode::DeepSeekThinking)
    );
    let mut messages = Vec::new();
    let cache_marker = if matches!(
        model.spec.cache.cache_control_format,
        Some(crate::types::CacheControlFormat::Anthropic)
    ) {
        cache_control(req, &model.spec.cache)
    } else {
        None
    };
    if let Some(ref sys) = req.system {
        // DeepSeek's documented Chat Completions examples use `system`; its
        // reasoning extension is not OpenAI's developer-message convention.
        let content = cache_marker.map_or_else(
            || ChatInstructionContent::Text(sys.clone()),
            |marker| {
                ChatInstructionContent::Parts(vec![ChatContentPart::Text {
                    text: sys.clone(),
                    cache_control: Some(marker),
                }])
            },
        );
        if has_reasoning && !deepseek_thinking {
            messages.push(ChatCompletionsMessage::Developer { content });
        } else {
            messages.push(ChatCompletionsMessage::System { content });
        }
    }

    // 3. Map messages
    for msg in &req.messages {
        match msg {
            Message::User(ref user) => {
                let mut parts = Vec::new();
                for part in &user.content {
                    match part {
                        UserPart::Text(ref text) => {
                            parts.push(ChatContentPart::Text {
                                text: text.clone(),
                                cache_control: None,
                            });
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

                            let url_str = match &image.source {
                                ImageSource::Url(u) => u.to_string(),
                                ImageSource::Inline(bytes) => {
                                    // No documented default MIME; guessing a wire
                                    // field is forbidden (design §75). Validation
                                    // has already diagnosed the drop, so skip the
                                    // part when the media type is absent.
                                    let Some(mime_str) = image.media_type.as_ref() else {
                                        continue;
                                    };
                                    format!(
                                        "data:{};base64,{}",
                                        mime_str,
                                        BASE64_STANDARD.encode(bytes)
                                    )
                                }
                                ImageSource::ProviderRef(_) => continue,
                            };

                            let detail = image.detail.map(|d| match d {
                                crate::types::ImageDetail::Auto => "auto".to_string(),
                                crate::types::ImageDetail::Low => "low".to_string(),
                                crate::types::ImageDetail::High => "high".to_string(),
                            });

                            parts.push(ChatContentPart::ImageUrl {
                                image_url: ChatImageUrl {
                                    url: url_str,
                                    detail,
                                },
                            });
                        }
                        UserPart::Media(Media::Audio(ref audio)) => {
                            if !model
                                .spec
                                .capabilities
                                .input_modalities
                                .contains(crate::types::Modality::Audio)
                            {
                                continue;
                            }

                            let format_str = match audio.format {
                                AudioFormat::Wav => "wav".to_string(),
                                AudioFormat::Mp3 => "mp3".to_string(),
                                _ => continue,
                            };

                            let data_str = match &audio.payload {
                                AudioPayload::Inline(bytes) => BASE64_STANDARD.encode(bytes),
                                AudioPayload::InlineWithProviderRef { data, .. } => {
                                    BASE64_STANDARD.encode(data)
                                }
                                AudioPayload::ProviderRef(_) => continue,
                            };

                            parts.push(ChatContentPart::InputAudio {
                                input_audio: ChatInputAudio {
                                    data: data_str,
                                    format: format_str,
                                },
                            });
                        }
                        UserPart::ToolResult(ref tr) => {
                            // Preserve canonical order: flush any buffered user
                            // content before this `role:"tool"` message rather
                            // than emitting all tool results first (design §11).
                            if !parts.is_empty() {
                                messages.push(ChatCompletionsMessage::User {
                                    content: std::mem::take(&mut parts),
                                });
                            }
                            let mut text_parts = Vec::new();
                            for tr_part in &tr.content {
                                if let ToolResultPart::Text(ref text) = tr_part {
                                    text_parts.push(text.clone());
                                }
                            }
                            messages.push(ChatCompletionsMessage::Tool {
                                tool_call_id: crate::protocol::normalize_tool_call_id(
                                    &tr.tool_call_id.0,
                                ),
                                content: text_parts.join("\n"),
                            });
                        }
                    }
                }
                if !parts.is_empty() {
                    messages.push(ChatCompletionsMessage::User { content: parts });
                }
            }
            Message::Assistant(ref assistant) => {
                let mut text_parts = Vec::new();
                let mut reasoning_parts = Vec::new();
                let mut tool_calls = Vec::new();
                let mut audio_ref = None;

                for part in &assistant.content {
                    match part {
                        AssistantPart::Text(ref text) => {
                            text_parts.push(text.clone());
                        }
                        // DeepSeek requires the previous turn's full
                        // `reasoning_content` when that turn called a tool.
                        // Sending it for ordinary same-model turns is allowed
                        // (the API ignores it), and retaining the model check
                        // prevents cross-model Chat reasoning from leaking in.
                        AssistantPart::Reasoning(reasoning)
                            if deepseek_thinking
                                && assistant.protocol == Protocol::OpenAiChat
                                && assistant.model == model.spec.id =>
                        {
                            if let Some(text) = &reasoning.text {
                                reasoning_parts.push(text.clone());
                            }
                        }
                        AssistantPart::ToolCall(ref tc) => {
                            tool_calls.push(ChatToolCall {
                                id: crate::protocol::normalize_tool_call_id(&tc.id.0),
                                r#type: "function".to_string(),
                                function: ChatFunctionCall {
                                    name: tc.name.clone(),
                                    arguments: tc.arguments_json.clone(),
                                },
                            });
                        }
                        AssistantPart::Media(Media::Audio(ref audio)) => {
                            // Design §7: only replay an assistant audio id whose
                            // reference is still usable (same protocol, not
                            // expired). An expired/wrong-protocol ref is dropped
                            // rather than serialized.
                            let reference = match &audio.payload {
                                AudioPayload::InlineWithProviderRef { reference, .. } => {
                                    Some(reference)
                                }
                                AudioPayload::ProviderRef(reference) => Some(reference),
                                AudioPayload::Inline(_) => None,
                            };
                            if let Some(reference) = reference {
                                if crate::validate::provider_ref_is_usable(
                                    reference,
                                    Protocol::OpenAiChat,
                                ) {
                                    audio_ref = Some(ChatAssistantAudioRef {
                                        id: reference.id.clone(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }

                let content_str = if text_parts.is_empty() {
                    None
                } else {
                    Some(ChatInstructionContent::Text(text_parts.join("\n")))
                };
                let reasoning_content =
                    (!reasoning_parts.is_empty()).then(|| reasoning_parts.join("\n"));
                let tool_calls_opt = if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                };

                messages.push(ChatCompletionsMessage::Assistant {
                    content: content_str,
                    reasoning_content,
                    tool_calls: tool_calls_opt,
                    audio: audio_ref,
                });
            }
        }
    }

    if let Some(marker) = cache_marker {
        add_cache_control_to_last_conversation_message(&mut messages, marker);
    }

    // 4. Map tools and tool_choice
    let tools_opt = if req.tools.is_empty() || !model.spec.capabilities.tools {
        None
    } else {
        Some(
            req.tools
                .iter()
                .enumerate()
                .map(|(index, t)| ChatTool {
                    r#type: "function".to_string(),
                    function: ChatFunctionDef {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                    cache_control: (index + 1 == req.tools.len()
                        && model.spec.cache.supports_cache_control_on_tools)
                        .then_some(cache_marker)
                        .flatten(),
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
                "function": { "name": name }
            })),
        }
    };

    // 5. Reasoning configuration
    let reasoning_effort = if has_reasoning {
        match req.reasoning {
            ReasoningConfig::Off => None,
            ReasoningConfig::Effort(e) => Some(match e {
                // DeepSeek accepts high/low/medium/max/xhigh but not
                // `minimal`; map Ygg's portable minimum to its lowest valid
                // accepted value. The provider currently maps low and medium
                // to high internally (DeepSeek Thinking Mode docs).
                crate::types::ReasoningEffort::Minimal if deepseek_thinking => "low".to_string(),
                crate::types::ReasoningEffort::Minimal => "minimal".to_string(),
                crate::types::ReasoningEffort::Low => "low".to_string(),
                crate::types::ReasoningEffort::Medium => "medium".to_string(),
                crate::types::ReasoningEffort::High => "high".to_string(),
            }),
            ReasoningConfig::Budget(_) => None,
        }
    } else {
        None
    };
    let thinking = deepseek_thinking.then_some(ChatThinkingConfig {
        r#type: if matches!(&req.reasoning, ReasoningConfig::Effort(_)) {
            "enabled"
        } else {
            // DeepSeek defaults thinking to enabled, so omitting this field
            // would make canonical `ReasoningConfig::Off` incorrect.
            "disabled"
        },
    });

    // 6. Response format
    let response_format_opt = if model.spec.capabilities.structured_output {
        match &req.output_format {
            OutputFormat::Text => None,
            OutputFormat::JsonObject => Some(ChatResponseFormat::JsonObject),
            OutputFormat::JsonSchema(ref s) => Some(ChatResponseFormat::JsonSchema {
                json_schema: ChatJsonSchema {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    schema: s.schema.clone(),
                    strict: s.strict,
                },
            }),
        }
    } else {
        None
    };

    // 7. Modalities & Audio Output
    let mut modalities_opt = None;
    let mut audio_opt = None;
    let mut streaming = true;

    if let OutputModalities::TextAndAudio(ref opts) = req.output_modalities {
        if model
            .spec
            .capabilities
            .output_modalities
            .contains(crate::types::Modality::Audio)
        {
            modalities_opt = Some(vec!["text".to_string(), "audio".to_string()]);
            streaming = false;

            let voice_val = match &opts.voice {
                AudioVoice::Named(ref s) => serde_json::Value::String(s.clone()),
                AudioVoice::ProviderRef(ref id) => serde_json::json!({ "id": id }),
            };

            let format_str = match opts.format {
                AudioFormat::Wav => "wav".to_string(),
                AudioFormat::Mp3 => "mp3".to_string(),
                AudioFormat::Aac => "aac".to_string(),
                AudioFormat::Flac => "flac".to_string(),
                AudioFormat::Opus => "opus".to_string(),
                AudioFormat::Pcm16 => "pcm16".to_string(),
            };

            audio_opt = Some(ChatAudioOptions {
                voice: voice_val,
                format: format_str,
            });
        }
    }

    let stream_options = if streaming {
        Some(ChatStreamOptions {
            include_usage: true,
        })
    } else {
        None
    };

    let max_completion_tokens = req
        .max_output_tokens
        .or(Some(model.spec.limits.max_output_tokens));

    let chat_req = ChatCompletionsRequest {
        model: model.spec.api_name.clone(),
        messages,
        tools: tools_opt,
        tool_choice: tool_choice_opt,
        max_completion_tokens,
        temperature: req.temperature,
        stop: req.stop.clone(),
        reasoning_effort,
        thinking,
        response_format: response_format_opt,
        modalities: modalities_opt,
        audio: audio_opt,
        prompt_cache_key: {
            let direct_openai = model
                .endpoint
                .base_url
                .to_string()
                .contains("api.openai.com");
            ((direct_openai && req.cache_retention != crate::types::CacheRetention::None)
                || (req.cache_retention == crate::types::CacheRetention::Long
                    && model.spec.cache.supports_long_retention))
                .then(|| prompt_cache_key(req))
                .flatten()
        },
        prompt_cache_retention: (req.cache_retention == crate::types::CacheRetention::Long
            && model.spec.cache.supports_long_retention)
            .then_some("24h"),
        stream: streaming,
        stream_options,
    };

    let body_bytes = serde_json::to_vec(&chat_req)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let url = model
        .endpoint
        .base_url
        .join("chat/completions")
        .map_err(|e| ConfigError::Parse(e.to_string()))?;

    let mut headers = http::HeaderMap::new();
    if model.spec.cache.send_session_affinity_headers {
        if let Some(session_id) = cache_session_id(req) {
            let value = http::HeaderValue::from_str(session_id)
                .map_err(|_| ConfigError::InvalidHeader("x-session-affinity".into()))?;
            headers.insert(http::HeaderName::from_static("session_id"), value.clone());
            headers.insert(
                http::HeaderName::from_static("x-client-request-id"),
                value.clone(),
            );
            headers.insert(http::HeaderName::from_static("x-session-affinity"), value);
        }
    }

    Ok(HttpRequestParts {
        url,
        headers,
        body: bytes::Bytes::from(body_bytes),
        streaming,
        diagnostics,
    })
}

fn add_cache_control_to_last_conversation_message(
    messages: &mut [ChatCompletionsMessage],
    marker: CacheControl,
) {
    for message in messages.iter_mut().rev() {
        let applied = match message {
            ChatCompletionsMessage::User { content } => {
                content.iter_mut().rev().find_map(|part| match part {
                    ChatContentPart::Text { cache_control, .. } => {
                        *cache_control = Some(marker);
                        Some(())
                    }
                    ChatContentPart::ImageUrl { .. } | ChatContentPart::InputAudio { .. } => None,
                })
            }
            ChatCompletionsMessage::Assistant { content, .. } => content
                .as_mut()
                .and_then(|content| add_cache_control_to_instruction(content, marker)),
            ChatCompletionsMessage::Developer { .. }
            | ChatCompletionsMessage::System { .. }
            | ChatCompletionsMessage::Tool { .. } => None,
        };
        if applied.is_some() {
            return;
        }
    }
}

fn add_cache_control_to_instruction(
    content: &mut ChatInstructionContent,
    marker: CacheControl,
) -> Option<()> {
    match content {
        ChatInstructionContent::Text(text) if !text.is_empty() => {
            let text = std::mem::take(text);
            *content = ChatInstructionContent::Parts(vec![ChatContentPart::Text {
                text,
                cache_control: Some(marker),
            }]);
            Some(())
        }
        ChatInstructionContent::Parts(parts) => {
            parts.iter_mut().rev().find_map(|part| match part {
                ChatContentPart::Text { cache_control, .. } => {
                    *cache_control = Some(marker);
                    Some(())
                }
                ChatContentPart::ImageUrl { .. } | ChatContentPart::InputAudio { .. } => None,
            })
        }
        ChatInstructionContent::Text(_) => None,
    }
}

/// Decodes the non-streaming JSON response body from OpenAI Chat Completions.
pub(crate) fn decode_response(
    model: &crate::catalog::Model,
    body: &[u8],
    requested_audio_format: Option<AudioFormat>,
) -> Result<Response, AiError> {
    let resp: ChatCompletionsResponse = serde_json::from_slice(body)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let choice = resp
        .choices
        .first()
        .ok_or_else(|| AiError::Decode(DecodeError::Json("Empty choices array".to_string())))?;

    let mut content = Vec::new();

    // 1. Map content text
    if let Some(ref text) = choice.message.content {
        if !text.is_empty() {
            content.push(AssistantPart::Text(text.clone()));
        }
    }

    // 2. Map reasoning
    if let Some(ref reasoning) = choice.message.reasoning_content {
        if !reasoning.is_empty() {
            content.push(AssistantPart::Reasoning(ReasoningPart {
                text: Some(reasoning.clone()),
                state: None,
            }));
        }
    }

    // 3. Map tool calls
    if let Some(ref tcs) = choice.message.tool_calls {
        for tc in tcs {
            // Verify JSON arguments
            let val: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;
            if !val.is_object() {
                return Err(AiError::Decode(DecodeError::Json(
                    "Tool arguments must be a JSON object".to_string(),
                )));
            }

            content.push(AssistantPart::ToolCall(ToolCall {
                id: ToolCallId(tc.id.clone()),
                name: tc.function.name.clone(),
                arguments_json: tc.function.arguments.clone(),
            }));
        }
    }

    // 4. Map audio response
    if let Some(ref audio) = choice.message.audio {
        let decoded_data = BASE64_STANDARD
            .decode(&audio.data)
            .map_err(|_| AiError::Decode(DecodeError::InvalidBase64))?;

        let format = requested_audio_format.ok_or_else(|| {
            AiError::Decode(DecodeError::InvalidProviderField(
                "audio returned without requested output format".to_string(),
            ))
        })?;
        let expires_at_sys = std::time::SystemTime::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(audio.expires_at))
            .ok_or_else(|| {
                AiError::Decode(DecodeError::InvalidProviderField(
                    "audio expires_at is out of range".to_string(),
                ))
            })?;

        content.push(AssistantPart::Media(Media::Audio(AudioMedia {
            payload: AudioPayload::InlineWithProviderRef {
                data: bytes::Bytes::from(decoded_data),
                reference: ProviderMediaRef {
                    protocol: Protocol::OpenAiChat,
                    id: audio.id.clone(),
                    expires_at: Some(expires_at_sys),
                },
            },
            format,
            transcript: Some(audio.transcript.clone()),
        })));
    }

    let message = AssistantMessage {
        content,
        model: model.spec.id.clone(),
        protocol: Protocol::OpenAiChat,
    };

    let stop_reason = choice
        .finish_reason
        .as_deref()
        .map(map_stop_reason)
        .unwrap_or(StopReason::EndTurn);
    let usage = map_usage(&resp.usage)?;

    let cost = model
        .spec
        .pricing
        .as_ref()
        .map(|p| crate::pricing::cost_of(p, &usage).map_err(AiError::Pricing))
        .transpose()?;

    Ok(Response {
        message,
        stop_reason,
        usage,
        cost,
        response_id: Some(resp.id),
        diagnostics: Vec::new(),
    })
}

fn emit_event(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    ev: StreamEvent,
) -> Result<(), AiError> {
    builder.on_event(&ev)?;
    events.push(ev);
    Ok(())
}

/// Decodes a streaming SSE event from OpenAI Chat Completions, emitting StreamEvents.
pub(crate) fn decode_stream_event(
    _model: &crate::catalog::Model,
    sse_event: &SseEvent,
    builder: &mut ResponseBuilder,
) -> Result<Vec<StreamEvent>, AiError> {
    if sse_event.data == "[DONE]" {
        let resp = builder.finish_mut()?;
        return Ok(vec![StreamEvent::Finished(resp)]);
    }

    let chunk: ChatChunk = serde_json::from_str(&sse_event.data)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let mut events = Vec::new();

    if builder.response_id.is_none() {
        builder.response_id = Some(chunk.id.clone());
        emit_event(
            &mut events,
            builder,
            StreamEvent::Started {
                response_id: Some(chunk.id.clone()),
            },
        )?;
    }

    for choice in chunk.choices {
        // Delta text content
        if let Some(ref text) = choice.delta.content {
            if !text.is_empty() {
                let idx = get_canonical_index(builder, "text");
                if !builder.text_buffers.contains_key(&idx) {
                    emit_event(&mut events, builder, StreamEvent::TextStart { index: idx })?;
                }
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextDelta {
                        index: idx,
                        delta: text.clone(),
                    },
                )?;
            }
        }

        // Delta reasoning content
        if let Some(ref reasoning) = choice.delta.reasoning_content {
            if !reasoning.is_empty() {
                let idx = get_canonical_index(builder, "reasoning");
                if !builder.reasoning_text_buffers.contains_key(&idx) {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningStart { index: idx },
                    )?;
                }
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ReasoningDelta {
                        index: idx,
                        delta: reasoning.clone(),
                    },
                )?;
            }
        }

        // Delta tool calls
        if let Some(ref tcs) = choice.delta.tool_calls {
            for tc in tcs {
                let key = format!("tool_{}", tc.index);
                let idx = get_canonical_index(builder, &key);

                let id_key = format!("tool_id_{}", tc.index);
                let name_key = format!("tool_name_{}", tc.index);
                let args_key = format!("tool_args_{}", tc.index);
                if let Some(id) = &tc.id {
                    builder.temp_buffers.insert(id_key.clone(), id.clone());
                }
                if let Some(function) = &tc.function {
                    if let Some(name) = &function.name {
                        builder.temp_buffers.insert(name_key.clone(), name.clone());
                    }
                    if let Some(args) = &function.arguments {
                        builder
                            .temp_buffers
                            .entry(args_key.clone())
                            .or_default()
                            .push_str(args);
                    }
                }

                if !builder.tool_call_builders.contains_key(&idx) {
                    if let (Some(id), Some(name)) = (
                        builder.temp_buffers.get(&id_key).cloned(),
                        builder.temp_buffers.get(&name_key).cloned(),
                    ) {
                        emit_event(
                            &mut events,
                            builder,
                            StreamEvent::ToolCallStart {
                                index: idx,
                                id: ToolCallId(id),
                                name,
                            },
                        )?;
                    }
                }
                if builder.tool_call_builders.contains_key(&idx) {
                    if let Some(args) = builder.temp_buffers.remove(&args_key) {
                        if !args.is_empty() {
                            emit_event(
                                &mut events,
                                builder,
                                StreamEvent::ToolCallArgsDelta {
                                    index: idx,
                                    delta: args,
                                },
                            )?;
                        }
                    }
                }
            }
        }

        // Finish reason
        if let Some(ref finish_reason) = choice.finish_reason {
            let stop_reason = map_stop_reason(finish_reason);
            builder.set_stop_reason(stop_reason);

            // Close active segments
            let text_idx = get_canonical_index(builder, "text");
            if builder.text_buffers.contains_key(&text_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::TextEnd { index: text_idx },
                )?;
            }

            let reasoning_idx = get_canonical_index(builder, "reasoning");
            if builder.reasoning_text_buffers.contains_key(&reasoning_idx) {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ReasoningEnd {
                        index: reasoning_idx,
                    },
                )?;
            }

            let incomplete_tool = builder
                .provider_to_canonical_indices
                .iter()
                .filter(|(key, _)| key.starts_with("tool_"))
                .any(|(_, index)| !builder.tool_call_builders.contains_key(index));
            if incomplete_tool {
                return Err(AiError::Decode(DecodeError::InvalidProviderField(
                    "tool call ended before id and name were received".to_string(),
                )));
            }
            let active_tools: Vec<usize> = builder.tool_call_builders.keys().cloned().collect();
            for t_idx in active_tools {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallEnd { index: t_idx },
                )?;
            }
        }
    }

    if let Some(ref usage) = chunk.usage {
        let u = map_usage(usage)?;
        emit_event(&mut events, builder, StreamEvent::Usage(u))?;
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
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_string()),
    }
}

fn map_usage(usage: &ChatUsage) -> Result<Usage, AiError> {
    let cache_read = usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens)
        .unwrap_or(0);
    let input = usage
        .prompt_tokens
        .checked_sub(cache_read)
        .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?;
    let output = usage.completion_tokens;
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .map(|d| d.reasoning_tokens)
        .unwrap_or(0);
    if reasoning > output {
        return Err(AiError::Decode(DecodeError::UsageUnderflow));
    }
    Ok(Usage {
        input_tokens: input,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        output_tokens: output,
        reasoning_tokens: reasoning,
        total_tokens: usage.total_tokens,
    })
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Model;
    use crate::types::{
        AssistantMessage, AssistantPart, AudioFormat, AudioOutputOptions, Capabilities, Endpoint,
        EndpointId, ImageDetail, ImageMedia, ImageSource, Media, Message, ModalitySet, ModelId,
        ModelLimits, ModelSpec, OpenAiChatReasoningMode, OutputFormat, OutputModalities,
        ReasoningConfig, ReasoningEffort, ReasoningPart, Request, ToolCall, ToolCallId, ToolChoice,
        UserMessage, UserPart,
    };
    use crate::CompatibilityMode;
    use std::sync::Arc;

    fn make_test_model(
        image: bool,
        audio_in: bool,
        audio_out: bool,
        tools: bool,
        reasoning: bool,
        structured: bool,
    ) -> Model {
        let mut input = ModalitySet::none();
        if image {
            input = input.with(crate::types::Modality::Image);
        }
        if audio_in {
            input = input.with(crate::types::Modality::Audio);
        }

        let mut output = ModalitySet::none();
        if audio_out {
            output = output.with(crate::types::Modality::Audio);
        }

        let spec = ModelSpec {
            id: ModelId("test-model".to_string()),
            endpoint: EndpointId("test-ep".to_string()),
            api_name: "gpt-4-test".to_string(),
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
                input_modalities: input,
                output_modalities: output,
                tools,
                parallel_tool_calls: tools,
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
                structured_output: structured,
            },
            limits: ModelLimits {
                context_window: 10000,
                max_output_tokens: 2000,
            },
            pricing: None,
            cache: crate::types::CacheCompatibility::default(),
        };

        let ep = Endpoint {
            id: EndpointId("test-ep".to_string()),
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
    fn test_build_request_text_only() {
        let model = make_test_model(false, false, false, false, false, false);
        let req = Request {
            system: Some("System instructions".to_string()),
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("Hello".to_string())],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: Some(0.8),
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };

        let parts = build_request(&model, &req).unwrap();
        assert_eq!(
            parts.url.to_string(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert!(parts.streaming);

        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["model"], "gpt-4-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "System instructions");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["type"], "text");
        assert_eq!(body["messages"][1]["content"][0]["text"], "Hello");
    }

    #[test]
    fn deepseek_thinking_toggle_effort_and_tool_reasoning_replay() {
        let mut model = make_test_model(false, false, false, true, true, false);
        let spec = Arc::make_mut(&mut model.spec);
        spec.api_name = "deepseek-v4-pro".to_string();
        spec.capabilities
            .reasoning
            .as_mut()
            .unwrap()
            .openai_chat_mode = OpenAiChatReasoningMode::DeepSeekThinking;

        let mut req = Request {
            system: Some("system prompt".to_string()),
            messages: vec![Message::Assistant(AssistantMessage {
                content: vec![
                    AssistantPart::Reasoning(ReasoningPart {
                        text: Some("I need the tool result first.".to_string()),
                        state: None,
                    }),
                    AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("call_1".to_string()),
                        name: "lookup".to_string(),
                        arguments_json: "{}".to_string(),
                    }),
                ],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiChat,
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Effort(ReasoningEffort::Minimal),
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["thinking"]["type"], "enabled");
        // DeepSeek rejects `minimal`; its lowest accepted effort is `low`.
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(
            body["messages"][1]["reasoning_content"],
            "I need the tool result first."
        );
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_1");

        req.reasoning = ReasoningConfig::Off;
        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_build_request_audio_out() {
        let model = make_test_model(false, false, true, false, false, false);
        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("Say hello".to_string())],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::TextAndAudio(AudioOutputOptions {
                format: AudioFormat::Wav,
                voice: AudioVoice::Named("alloy".to_string()),
            }),
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };

        let parts = build_request(&model, &req).unwrap();
        assert!(!parts.streaming);

        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["stream"], false);
        assert_eq!(body["modalities"][0], "text");
        assert_eq!(body["modalities"][1], "audio");
        assert_eq!(body["audio"]["voice"], "alloy");
        assert_eq!(body["audio"]["format"], "wav");
    }

    #[test]
    fn test_decode_response_basic() {
        let model = make_test_model(false, false, false, false, false, false);
        let raw_json = r#"{
            "id": "chatcmpl-123",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello back!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let resp = decode_response(&model, raw_json.as_bytes(), None).unwrap();
        assert_eq!(resp.response_id, Some("chatcmpl-123".to_string()));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
        if let AssistantPart::Text(ref t) = resp.message.content[0] {
            assert_eq!(t, "Hello back!");
        } else {
            panic!("Expected Text part");
        }
    }

    #[test]
    fn test_decode_stream_event_text() {
        let model = make_test_model(false, false, false, false, false, false);
        let mut builder =
            ResponseBuilder::new(ModelId("m".to_string()), Protocol::OpenAiChat, None);

        let sse = SseEvent {
            event: None,
            data: r#"{"id": "chunk-1", "choices": [{"delta": {"content": "Hello"}}]}"#.to_string(),
        };

        let evs = decode_stream_event(&model, &sse, &mut builder).unwrap();
        assert_eq!(evs.len(), 3); // Started, TextStart, TextDelta
        assert!(matches!(evs[0], StreamEvent::Started { .. }));
        assert!(matches!(evs[1], StreamEvent::TextStart { .. }));
        if let StreamEvent::TextDelta { ref delta, .. } = evs[2] {
            assert_eq!(delta, "Hello");
        } else {
            panic!("Expected TextDelta");
        }
    }

    #[test]
    fn cache_retention_controls_openai_chat_key() {
        let model = make_test_model(false, false, false, false, false, false);
        let mut req = Request {
            system: Some("system".to_string()),
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("hello".to_string())],
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
            session_id: Some("b".repeat(70)),
        };

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(
            body["prompt_cache_key"].as_str().unwrap().chars().count(),
            64
        );
        assert!(body.get("prompt_cache_retention").is_none());

        req.cache_retention = crate::types::CacheRetention::Long;
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["prompt_cache_retention"], "24h");

        req.cache_retention = crate::types::CacheRetention::None;
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn test_build_request_audio_input() {
        let model = make_test_model(false, true, false, false, false, false);
        let audio_payload = bytes::Bytes::from(vec![0x00, 0x01, 0x02, 0x03]);
        let audio_media = crate::types::AudioMedia {
            payload: crate::types::AudioPayload::Inline(audio_payload),
            format: AudioFormat::Wav,
            transcript: None,
        };
        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Media(crate::types::Media::Audio(audio_media))],
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

        let messages = body_val["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_audio");
        let input_audio = &content[0]["input_audio"];
        assert_eq!(input_audio["format"], "wav");
        assert_eq!(input_audio["data"], "AAECAw==");
    }

    #[test]
    fn test_build_request_image_input() {
        let model = make_test_model(true, false, false, false, false, false);

        let inline_image = Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from(vec![0x47, 0x49, 0x46])),
            media_type: Some(mime::IMAGE_GIF),
            detail: Some(ImageDetail::High),
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

        let messages = body_val["messages"].as_array().unwrap();
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);

        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], "data:image/gif;base64,R0lG");
        assert_eq!(content[0]["image_url"]["detail"], "high");

        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "https://example.com/test.png"
        );
        assert!(content[1]["image_url"]["detail"].is_null());
    }
}

/// Offline fixture matrix for the Chat streaming + completed-audio decoders
/// (design §19; plan Task 9.2). Every case drives a hand-authored `.sse`/`.json`
/// fixture (cited in each file) through the shared harness + `guard`.
#[cfg(test)]
mod fixture_tests {
    use super::decode_stream_event;
    use crate::protocol::harness;
    use crate::stream::StreamEvent;
    use crate::types::{AssistantPart, AudioPayload, Media, Protocol, StopReason};

    macro_rules! fx {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/openai_chat/",
                $name
            ))
        };
    }

    fn joined_text(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn plain_streamed_text() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(&model, decode_stream_event, fx!("plain_text.sse"), 0)
            .await
            .unwrap();
        assert!(matches!(events.first(), Some(StreamEvent::Started { .. })));
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
        assert_eq!(joined_text(&events), "Hello, world");
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 9);
        assert_eq!(resp.usage.output_tokens, 3);
        assert_eq!(resp.usage.total_tokens, 12);
    }

    #[tokio::test]
    async fn identical_across_every_byte_boundary() {
        // Re-feed the same fixture at every chunk size; the event sequence must
        // be byte-boundary independent (design §19 layer 1/2).
        let model = harness::model(Protocol::OpenAiChat, None);
        let data = fx!("plain_text.sse");
        let oneshot = harness::drive(&model, decode_stream_event, data, 0)
            .await
            .unwrap();
        let baseline = format!("{oneshot:?}");
        for chunk in 1..=data.len() {
            let got = harness::drive(&model, decode_stream_event, data, chunk)
                .await
                .unwrap();
            assert_eq!(
                format!("{got:?}"),
                baseline,
                "mismatch at chunk size {chunk}"
            );
        }
    }

    #[tokio::test]
    async fn streamed_reasoning_has_no_state() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(&model, decode_stream_event, fx!("reasoning.sse"), 3)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ReasoningStart { .. })));
        let resp = harness::finished(&events);
        let reasoning = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .expect("reasoning part present");
        assert_eq!(reasoning.text.as_deref(), Some("Let me think about it."));
        assert!(reasoning.state.is_none(), "Chat reasoning carries no state");
        assert_eq!(resp.usage.reasoning_tokens, 5);
    }

    #[tokio::test]
    async fn single_tool_call_args_reassemble() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(&model, decode_stream_event, fx!("one_tool_call.sse"), 4)
            .await
            .unwrap();
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
            .expect("tool call present");
        assert_eq!(tc.name, "grep");
        assert_eq!(tc.id.0, "call_a");
        assert_eq!(
            tc.arguments_value().unwrap(),
            serde_json::json!({"pattern": "foo"})
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_interleaved() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(
            &model,
            decode_stream_event,
            fx!("parallel_tool_calls.sse"),
            0,
        )
        .await
        .unwrap();
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
        assert_eq!(
            calls[0].arguments_value().unwrap(),
            serde_json::json!({"a":1})
        );
        assert_eq!(
            calls[1].arguments_value().unwrap(),
            serde_json::json!({"b":2})
        );
    }

    #[tokio::test]
    async fn malformed_tool_json_is_decode_error() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let err = harness::drive(
            &model,
            decode_stream_event,
            fx!("malformed_tool_json.sse"),
            0,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, crate::error::AiError::Decode(_)),
            "expected Decode, got {err:?}"
        );
    }

    #[tokio::test]
    async fn length_finish_maps_to_max_tokens() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(&model, decode_stream_event, fx!("length_stop.sse"), 0)
            .await
            .unwrap();
        assert_eq!(
            harness::finished(&events).stop_reason,
            StopReason::MaxTokens
        );
    }

    #[tokio::test]
    async fn premature_eof_before_done() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let err = harness::drive(&model, decode_stream_event, fx!("premature_eof.sse"), 0)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                crate::error::AiError::StreamProtocol(
                    crate::error::StreamProtocolError::PrematureEof
                )
            ),
            "expected PrematureEof, got {err:?}"
        );
    }

    #[test]
    fn completed_audio_output_decodes_to_single_media() {
        // Non-streaming `message.audio` path (design §12.1): one completed
        // AssistantPart::Media carrying data + provider ref + transcript.
        let model = harness::model(Protocol::OpenAiChat, None);
        let resp = super::decode_response(
            &model,
            fx!("audio_output.json"),
            Some(crate::types::AudioFormat::Wav),
        )
        .unwrap();
        let audio = resp
            .message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantPart::Media(Media::Audio(a)) => Some(a),
                _ => None,
            })
            .expect("audio media present");
        match &audio.payload {
            AudioPayload::InlineWithProviderRef { data, reference } => {
                assert!(data.starts_with(b"RIFF"), "decoded WAV bytes");
                assert_eq!(reference.id, "audio_abc123");
                assert_eq!(reference.protocol, Protocol::OpenAiChat);
                assert!(reference.expires_at.is_some());
            }
            other => panic!("expected InlineWithProviderRef, got {other:?}"),
        }
        assert_eq!(audio.transcript.as_deref(), Some("Hello from audio."));
    }
}
