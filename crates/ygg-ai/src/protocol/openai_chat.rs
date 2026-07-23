//! OpenAI Chat Completions private wire protocol codec.

use base64::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{AiError, ConfigError, DecodeError};
use crate::protocol::sse::SseEvent;
use crate::protocol::{
    cache_control, cache_session_id, prompt_cache_key, CacheControl, HttpRequestParts,
};
use crate::stream::{
    OpenAiChatCompatibilityState, ResponseBuilder, StreamEvent, MAX_RESPONSE_PARTS,
    MAX_TOOL_ARGUMENT_BYTES,
};
use crate::types::{
    AssistantMessage, AssistantPart, AudioFormat, AudioMedia, AudioPayload, AudioVoice,
    ImageSource, Media, Message, OpenAiChatReasoningMode, OutputFormat, OutputModalities, Protocol,
    ProviderMediaRef, ReasoningConfig, ReasoningEffort, ReasoningPart, Request, Response,
    StopReason, ToolCall, ToolCallId, ToolChoice, ToolResultPart, Usage, UserPart,
};
use crate::validate::{normalize_request_reasoning, validate_request};

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
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ChatReasoningConfig>,
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
struct ChatReasoningConfig {
    effort: String,
}

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
    User { content: ChatInstructionContent },
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
    reasoning: Option<String>,
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
    #[serde(default)]
    prompt_tokens_details: Option<ChatPromptTokensDetails>,
    // OpenRouter and some OpenAI-compatible gateways expose this legacy
    // top-level spelling instead of `prompt_tokens_details.cached_tokens`.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens_details: Option<ChatCompletionTokensDetails>,
}

#[derive(Deserialize, Default)]
struct ChatPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    // Unlike direct OpenAI Chat, compatible gateways can report writes in the
    // same bucket. Keep it disjoint from full-rate prompt input.
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ChatCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Deserialize)]
struct ChatChunk {
    // Some OpenAI-compatible providers omit `id` and/or `choices` on trailing
    // usage-only chunks; neither absence is fatal to the stream.
    #[serde(default)]
    id: String,
    #[serde(default)]
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
    // OpenRouter and several OpenAI-compatible gateways normalize reasoning to
    // a flat `reasoning` string instead of `reasoning_content`; without this
    // alias their reasoning is silently dropped.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatChunkToolCall>>,
}

#[derive(Deserialize)]
struct ChatChunkToolCall {
    // A single tool call streamed without an explicit index is index 0.
    #[serde(default)]
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

fn provider_effort(value: &str) -> Option<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" | "min" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" | "med" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" | "x-high" | "extra_high" => Some(ReasoningEffort::Xhigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

fn provider_reasoning_value(
    values: &[String],
    default: Option<&str>,
    reasoning: &ReasoningConfig,
) -> Option<String> {
    let find = |predicate: fn(&str) -> bool| {
        values
            .iter()
            .find(|value| predicate(value))
            .map(ToOwned::to_owned)
    };
    match reasoning {
        ReasoningConfig::Off => find(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "none" | "off" | "disabled" | "false"
            )
        }),
        ReasoningConfig::On => find(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "default" | "on" | "enabled" | "true"
            )
        })
        .or_else(|| {
            default
                .filter(|candidate| values.iter().any(|value| value == *candidate))
                .map(ToOwned::to_owned)
        }),
        ReasoningConfig::Effort(effort) => values
            .iter()
            .find(|value| provider_effort(value) == Some(*effort))
            .map(ToOwned::to_owned),
        ReasoningConfig::Budget(_) => None,
    }
}

/// Builds the OpenAI Chat Completions HTTP request parts.
pub(crate) fn build_request(
    model: &crate::catalog::Model,
    req: &Request,
) -> Result<HttpRequestParts, AiError> {
    // 1. Normalize model-gated reasoning, then run validation.
    let req = normalize_request_reasoning(req, &model.spec.capabilities);
    let diagnostics = validate_request(
        &req,
        &model.spec.capabilities,
        &model.spec.limits,
        Protocol::OpenAiChat,
        &model.spec.id,
        req.compatibility,
    )?;

    // 2. Map system prompt
    let reasoning_capability = model.spec.capabilities.reasoning.as_ref();
    let has_reasoning = reasoning_capability.is_some();
    let reasoning_mode = reasoning_capability.map(|capability| &capability.openai_chat_mode);
    let deepseek_thinking = matches!(
        reasoning_mode,
        Some(OpenAiChatReasoningMode::DeepSeekThinking)
    );
    let openrouter_reasoning = matches!(reasoning_mode, Some(OpenAiChatReasoningMode::OpenRouter));
    let provider_uses_system_message = matches!(
        reasoning_mode,
        Some(OpenAiChatReasoningMode::ProviderValues {
            system_message: true,
            ..
        })
    );
    let mut messages = Vec::new();
    let cache_marker = if matches!(
        model.spec.cache.cache_control_format,
        Some(crate::types::CacheControlFormat::Anthropic)
    ) {
        cache_control(&req, &model.spec.cache)
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
        if has_reasoning
            && !deepseek_thinking
            && !matches!(reasoning_mode, Some(OpenAiChatReasoningMode::SystemMessage))
            && !provider_uses_system_message
        {
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
                                    content: compact_text_content(std::mem::take(&mut parts)),
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
                    messages.push(ChatCompletionsMessage::User {
                        content: compact_text_content(parts),
                    });
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
                        // Most OpenAI-compatible local servers expose reasoning
                        // as a response-only field. Replaying an assistant turn
                        // containing only that field serializes as `content:
                        // null`, which llama.cpp rejects before it can see the
                        // following tool result. Preserve it as ordinary
                        // assistant text instead; DeepSeek is handled above
                        // because it requires the dedicated field.
                        AssistantPart::Reasoning(reasoning) => {
                            if let Some(text) = &reasoning.text {
                                text_parts.push(text.clone());
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
    let provider_reasoning = match reasoning_mode {
        Some(OpenAiChatReasoningMode::ProviderValues {
            values, default, ..
        }) => Some((values, default.as_deref())),
        _ => None,
    };
    let reasoning_effort = if let Some((values, default)) = provider_reasoning {
        provider_reasoning_value(values, default, &req.reasoning)
    } else if has_reasoning && !openrouter_reasoning {
        match req.reasoning {
            ReasoningConfig::Off => None,
            ReasoningConfig::On => None,
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
                crate::types::ReasoningEffort::Xhigh => "xhigh".to_string(),
                crate::types::ReasoningEffort::Max => "max".to_string(),
            }),
            ReasoningConfig::Budget(_) => None,
        }
    } else {
        None
    };
    let reasoning = if openrouter_reasoning {
        match req.reasoning {
            ReasoningConfig::Effort(e) => Some(ChatReasoningConfig {
                effort: match e {
                    crate::types::ReasoningEffort::Minimal => "minimal",
                    crate::types::ReasoningEffort::Low => "low",
                    crate::types::ReasoningEffort::Medium => "medium",
                    crate::types::ReasoningEffort::High => "high",
                    crate::types::ReasoningEffort::Xhigh => "xhigh",
                    crate::types::ReasoningEffort::Max => "max",
                }
                .to_string(),
            }),
            ReasoningConfig::Off | ReasoningConfig::On | ReasoningConfig::Budget(_) => None,
        }
    } else {
        None
    };
    let thinking = deepseek_thinking.then_some(ChatThinkingConfig {
        r#type: if matches!(
            &req.reasoning,
            ReasoningConfig::On | ReasoningConfig::Effort(_)
        ) {
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

    // Model limits are local capacity metadata, not request defaults. Only
    // forward a cap explicitly chosen by the caller. DeepSeek implements the
    // compatible `max_tokens` field, while current OpenAI Chat uses
    // `max_completion_tokens`.
    let (max_tokens, max_completion_tokens) = if deepseek_thinking {
        (req.max_output_tokens, None)
    } else {
        (None, req.max_output_tokens)
    };

    let chat_req = ChatCompletionsRequest {
        model: model.spec.api_name.clone(),
        messages,
        tools: tools_opt,
        tool_choice: tool_choice_opt,
        max_tokens,
        max_completion_tokens,
        temperature: req.temperature,
        stop: req.stop.clone(),
        reasoning_effort,
        reasoning,
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
                .then(|| prompt_cache_key(&req))
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
    let affinity_format = model.spec.cache.send_session_affinity_headers.then_some(
        model
            .spec
            .cache
            .session_affinity_format
            .unwrap_or(crate::types::SessionAffinityFormat::OpenAi),
    );
    if let (Some(format), Some(session_id)) = (affinity_format, cache_session_id(&req)) {
        let value = http::HeaderValue::from_str(session_id)
            .map_err(|_| ConfigError::InvalidHeader("session affinity".into()))?;
        match format {
            crate::types::SessionAffinityFormat::OpenAi => {
                headers.insert(http::HeaderName::from_static("session_id"), value.clone());
                headers.insert(
                    http::HeaderName::from_static("x-client-request-id"),
                    value.clone(),
                );
                headers.insert(http::HeaderName::from_static("x-session-affinity"), value);
            }
            crate::types::SessionAffinityFormat::OpenAiNoSession => {
                headers.insert(
                    http::HeaderName::from_static("x-client-request-id"),
                    value.clone(),
                );
                headers.insert(http::HeaderName::from_static("x-session-affinity"), value);
            }
            crate::types::SessionAffinityFormat::OpenRouter => {
                headers.insert(http::HeaderName::from_static("x-session-id"), value);
            }
            // Codex is a Responses route; accepting this value here keeps
            // configuration forward-compatible without emitting invalid Chat
            // headers.
            crate::types::SessionAffinityFormat::Codex => {}
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

/// Prefer the universally supported string form for plain-text user content.
/// Multipart content remains an array for image/audio requests and for
/// provider-specific cache-control annotations.
fn compact_text_content(parts: Vec<ChatContentPart>) -> ChatInstructionContent {
    if !parts.is_empty()
        && parts.iter().all(|part| {
            matches!(
                part,
                ChatContentPart::Text {
                    cache_control: None,
                    ..
                }
            )
        })
    {
        let text = parts
            .into_iter()
            .map(|part| match part {
                ChatContentPart::Text { text, .. } => text,
                _ => unreachable!("all parts were checked as text"),
            })
            .collect();
        ChatInstructionContent::Text(text)
    } else {
        ChatInstructionContent::Parts(parts)
    }
}

fn add_cache_control_to_last_conversation_message(
    messages: &mut [ChatCompletionsMessage],
    marker: CacheControl,
) {
    for message in messages.iter_mut().rev() {
        let applied = match message {
            ChatCompletionsMessage::User { content } => {
                add_cache_control_to_instruction(content, marker)
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

    // 1. Map content text. A non-streaming OpenAI-compatible response can
    // have the same Qwen XML fallback as the streaming path when vLLM's
    // parser was not configured.
    let has_native_tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    let mut recovered_content_tool = false;
    let mut recovered_locked_placeholder = false;
    if let Some(ref text) = choice.message.content {
        if !text.is_empty() {
            if model.spec.capabilities.tools
                && !has_native_tool_calls
                && content_may_contain_tool_call(text)
            {
                let mut fallback =
                    ResponseBuilder::new(model.spec.id.clone(), Protocol::OpenAiChat, None);
                // The completed body is already fully available, so enabling
                // ambiguous compatibility recovery cannot hide streamed JSON.
                fallback.set_buffer_ambiguous_compatibility_content(true);
                let mut ignored_events = Vec::new();
                consume_qwen_xml_content(&mut ignored_events, &mut fallback, text)?;
                flush_qwen_xml_pending(&mut ignored_events, &mut fallback)?;
                recovered_locked_placeholder = fallback.tool_output_locked_seen;
                let fallback_response = fallback.finish()?;
                recovered_content_tool = fallback_response
                    .message
                    .content
                    .iter()
                    .any(|part| matches!(part, AssistantPart::ToolCall(_)));
                content.extend(fallback_response.message.content);
            } else {
                let sanitized = text.replace(TOOL_OUTPUT_LOCKED, "");
                recovered_locked_placeholder = sanitized.len() != text.len();
                if !sanitized.is_empty() {
                    content.push(AssistantPart::Text(sanitized));
                }
            }
        }
    }

    // 2. Map reasoning. Some OpenAI-compatible gateways use the flat
    // `reasoning` spelling in completed responses too.
    let reasoning = choice
        .message
        .reasoning_content
        .as_ref()
        .filter(|text| !text.is_empty())
        .or_else(|| {
            choice
                .message
                .reasoning
                .as_ref()
                .filter(|text| !text.is_empty())
        });
    if let Some(reasoning) = reasoning {
        content.push(AssistantPart::Reasoning(ReasoningPart {
            text: Some(reasoning.clone()),
            state: None,
        }));
    }

    // 3. Map tool calls
    if let Some(ref tcs) = choice.message.tool_calls {
        for tc in tcs {
            let arguments_json = crate::json_repair::normalize_json_object(&tc.function.arguments)
                .map_err(AiError::Decode)?;

            content.push(AssistantPart::ToolCall(ToolCall {
                id: ToolCallId(tc.id.clone()),
                name: tc.function.name.clone(),
                arguments_json,
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
        .map(|reason| {
            if recovered_content_tool && reason == "stop" {
                StopReason::ToolUse
            } else if recovered_locked_placeholder && reason == "stop" {
                StopReason::Other("tool_output_locked".to_string())
            } else {
                map_stop_reason(reason)
            }
        })
        .unwrap_or_else(|| {
            if recovered_content_tool {
                StopReason::ToolUse
            } else if recovered_locked_placeholder {
                StopReason::Other("tool_output_locked".to_string())
            } else {
                StopReason::EndTurn
            }
        });
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
    model: &crate::catalog::Model,
    sse_event: &SseEvent,
    builder: &mut ResponseBuilder,
) -> Result<Vec<StreamEvent>, AiError> {
    // Count provider frames before decoding. Compatibility candidates can be
    // intentionally withheld and would otherwise evade the canonical event
    // cap until EOF.
    builder.observe_provider_stream_event()?;

    if sse_event.data == "[DONE]" {
        let mut events = Vec::new();
        // Resolve a content-based Qwen tool call before closing the response.
        // The marker and its body are commonly split over many SSE chunks.
        flush_qwen_xml_pending(&mut events, builder)?;
        if builder.tool_output_locked_seen
            && builder.qwen_xml_call_count == 0
            && !builder.native_tool_call_seen
        {
            builder.set_stop_reason(StopReason::Other("tool_output_locked".to_string()));
        }
        // Providers that omit a finish_reason chunk entirely leave parts open
        // here; close them so the terminal response stays balanced (§8).
        close_open_parts(&mut events, builder)?;
        let resp = builder.finish_mut()?;
        events.push(StreamEvent::Finished(resp));
        return Ok(events);
    }

    let chunk: ChatChunk = serde_json::from_str(&sse_event.data)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let mut events = Vec::new();

    if !builder.started {
        // A chunk with an empty/absent `id` still starts the stream; the
        // provider-assigned id is then simply unknown (None).
        let response_id = (!chunk.id.is_empty()).then_some(chunk.id.clone());
        emit_event(&mut events, builder, StreamEvent::Started { response_id })?;
    }

    for choice in chunk.choices {
        // Delta text content
        if let Some(ref text) = choice.delta.content {
            if !text.is_empty() {
                if model.spec.capabilities.tools {
                    // vLLM can return native XML/JSON tool syntax as ordinary
                    // content. Defer recognized calls until turn completion so
                    // a later structured `tool_calls` delta can supersede them.
                    consume_qwen_xml_content(&mut events, builder, text)?;
                } else {
                    emit_text_without_locked_marker(&mut events, builder, text)?;
                }
            }
        }

        // Delta reasoning content. OpenRouter and similar gateways send
        // `reasoning` where DeepSeek/Moonshot send `reasoning_content`;
        // prefer the documented field when a chunk carries both.
        let reasoning_text = choice
            .delta
            .reasoning_content
            .as_ref()
            .filter(|text| !text.is_empty())
            .or_else(|| {
                choice
                    .delta
                    .reasoning
                    .as_ref()
                    .filter(|text| !text.is_empty())
            });
        if let Some(reasoning) = reasoning_text {
            let idx = segment_index(builder, "reasoning");
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

        // Delta tool calls
        if let Some(ref tcs) = choice.delta.tool_calls {
            if !tcs.is_empty() {
                builder.native_tool_call_seen = true;
            }
            for tc in tcs {
                let key = format!("tool_{}", tc.index);
                let idx = segment_index(builder, &key);

                let id_key = format!("tool_id_{}", tc.index);
                let name_key = format!("tool_name_{}", tc.index);
                let args_key = format!("tool_args_{}", tc.index);
                if !builder.tool_call_builders.contains_key(&idx) {
                    if let Some(id) = &tc.id {
                        builder.replace_temp_buffer(id_key.clone(), id.clone())?;
                    }
                    if let Some(function) = &tc.function {
                        if let Some(name) = &function.name {
                            builder.replace_temp_buffer(name_key.clone(), name.clone())?;
                        }
                    }
                }
                if let Some(function) = &tc.function {
                    if let Some(args) = &function.arguments {
                        builder.append_temp_buffer_bounded(
                            args_key.clone(),
                            args,
                            MAX_TOOL_ARGUMENT_BYTES,
                        )?;
                    }
                }

                if !builder.tool_call_builders.contains_key(&idx)
                    && builder.temp_buffers.contains_key(&id_key)
                    && builder.temp_buffers.contains_key(&name_key)
                {
                    let id = builder
                        .take_temp_buffer(&id_key)
                        .expect("presence checked above");
                    let name = builder
                        .take_temp_buffer(&name_key)
                        .expect("presence checked above");
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
                if builder.tool_call_builders.contains_key(&idx) {
                    if let Some(args) = builder.take_temp_buffer(&args_key) {
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

        // Finish reason. Several providers (Moonshot, OpenRouter passthrough)
        // repeat `finish_reason` in the trailing usage-bearing chunk; part
        // closing is idempotent so the duplicate close emits nothing (§8).
        if let Some(ref finish_reason) = choice.finish_reason {
            let stop_reason = if builder.qwen_xml_call_count > 0 && finish_reason == "stop" {
                // A Qwen XML fallback call is semantically tool use even though
                // the unconfigured vLLM endpoint labels the content turn stop.
                StopReason::ToolUse
            } else {
                map_stop_reason(finish_reason)
            };
            builder.set_stop_reason(stop_reason);
            flush_qwen_xml_pending(&mut events, builder)?;
            if builder.tool_output_locked_seen
                && builder.qwen_xml_call_count == 0
                && !builder.native_tool_call_seen
            {
                builder.set_stop_reason(StopReason::Other("tool_output_locked".to_string()));
            }
            close_open_parts(&mut events, builder)?;
        }
    }

    if let Some(ref usage) = chunk.usage {
        let u = map_usage(usage)?;
        emit_event(&mut events, builder, StreamEvent::Usage(u))?;
    }

    Ok(events)
}

// --- Helpers ---

const QWEN_XML_CLOSE: &str = "</tool_call>";
const TOOL_CALL_OPEN_PREFIX: &str = "<tool_call";
const FUNCTION_OPEN_PREFIX: &str = "<function";
const TOOL_OUTPUT_LOCKED: &str = "[tool_output_locked]";

fn emit_text_delta(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    text: &str,
) -> Result<(), AiError> {
    if text.is_empty() {
        return Ok(());
    }
    let idx = segment_index(builder, "text");
    if !builder.text_buffers.contains_key(&idx) {
        emit_event(events, builder, StreamEvent::TextStart { index: idx })?;
    }
    emit_event(
        events,
        builder,
        StreamEvent::TextDelta {
            index: idx,
            delta: text.to_owned(),
        },
    )
}

/// Consume assistant content while keeping compatibility tool syntax and local
/// control placeholders out of the visible transcript. Native structured calls
/// remain authoritative; this path is only used when an OpenAI-compatible
/// server returned model text instead of `delta.tool_calls`.
fn consume_qwen_xml_content(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    delta: &str,
) -> Result<(), AiError> {
    builder.reserve_buffered_content(delta.len())?;
    builder.qwen_xml_pending.push_str(delta);
    // Prefixes are consumed logically and compacted once before returning.
    // This keeps a single event containing many compatibility blocks linear:
    // repeatedly draining a String prefix would shift the remaining event for
    // every block.
    let mut pending_head = 0usize;

    loop {
        match builder.qwen_xml_state {
            OpenAiChatCompatibilityState::BareJson => {
                compact_pending_prefix(builder, pending_head);
                return Ok(());
            }
            OpenAiChatCompatibilityState::Scanning { scan_from } => {
                let pending = &builder.qwen_xml_pending[pending_head..];
                if builder.buffer_ambiguous_compatibility_content && json_tool_candidate(pending) {
                    // Bare JSON is ambiguous until EOF. Holding it is a lossy
                    // compatibility behavior and therefore must be explicitly
                    // enabled by the request; strict/default streams expose it
                    // immediately as ordinary assistant text.
                    builder.qwen_xml_state = OpenAiChatCompatibilityState::BareJson;
                    compact_pending_prefix(builder, pending_head);
                    return Ok(());
                }

                let Some((index, marker)) = earliest_compat_marker(pending, scan_from) else {
                    if !builder.qwen_xml_buffered_calls.is_empty() {
                        // Preserve ordering after a deferred call, but resume
                        // searching at the unexamined suffix next time.
                        builder.qwen_xml_state = OpenAiChatCompatibilityState::Scanning {
                            scan_from: next_scan_offset(pending, 0, MAX_COMPAT_MARKER_LEN),
                        };
                        compact_pending_prefix(builder, pending_head);
                        return Ok(());
                    }
                    let keep = compatibility_marker_suffix_len(
                        pending,
                        builder.buffer_ambiguous_compatibility_content,
                    );
                    let flush_len = pending.len().saturating_sub(keep);
                    if flush_len == 0 {
                        builder.qwen_xml_state =
                            OpenAiChatCompatibilityState::Scanning { scan_from: 0 };
                        compact_pending_prefix(builder, pending_head);
                        return Ok(());
                    }
                    let text = take_pending_prefix_at(builder, &mut pending_head, flush_len);
                    builder.qwen_xml_state =
                        OpenAiChatCompatibilityState::Scanning { scan_from: 0 };
                    compact_pending_prefix(builder, pending_head);
                    if builder.qwen_xml_call_count == 0 || !text.trim().is_empty() {
                        emit_text_delta(events, builder, &text)?;
                    }
                    return Ok(());
                };

                if index > 0 {
                    let text = take_pending_prefix_at(builder, &mut pending_head, index);
                    if !text.trim().is_empty() {
                        emit_text_delta(events, builder, &text)?;
                    }
                }

                match marker {
                    CompatMarker::ToolOutputLocked => {
                        skip_pending_prefix_at(
                            builder,
                            &mut pending_head,
                            TOOL_OUTPUT_LOCKED.len(),
                        );
                        builder.tool_output_locked_seen = true;
                        builder.qwen_xml_state =
                            OpenAiChatCompatibilityState::Scanning { scan_from: 0 };
                    }
                    CompatMarker::ToolCall => {
                        builder.qwen_xml_state = OpenAiChatCompatibilityState::ToolCallOpen {
                            scan_from: TOOL_CALL_OPEN_PREFIX.len(),
                        };
                    }
                    CompatMarker::Function => {
                        builder.qwen_xml_state = OpenAiChatCompatibilityState::FunctionBody {
                            scan_from: FUNCTION_OPEN_PREFIX.len(),
                        };
                    }
                }
            }
            OpenAiChatCompatibilityState::ToolCallOpen { scan_from } => {
                let pending = &builder.qwen_xml_pending[pending_head..];
                let Some(open_end) = pending[scan_from..]
                    .find('>')
                    .map(|offset| scan_from + offset + 1)
                else {
                    builder.qwen_xml_state = OpenAiChatCompatibilityState::ToolCallOpen {
                        scan_from: pending.len(),
                    };
                    compact_pending_prefix(builder, pending_head);
                    return Ok(());
                };
                builder.qwen_xml_state = OpenAiChatCompatibilityState::ToolCallBody {
                    open_end,
                    scan_from: open_end,
                };
            }
            OpenAiChatCompatibilityState::ToolCallBody {
                open_end,
                scan_from,
            } => {
                let pending = &builder.qwen_xml_pending[pending_head..];
                let Some(close) = pending[scan_from..]
                    .find(QWEN_XML_CLOSE)
                    .map(|offset| scan_from + offset)
                else {
                    builder.qwen_xml_state = OpenAiChatCompatibilityState::ToolCallBody {
                        open_end,
                        scan_from: next_scan_offset(pending, open_end, QWEN_XML_CLOSE.len()),
                    };
                    compact_pending_prefix(builder, pending_head);
                    return Ok(());
                };
                let block = pending[open_end..close].to_owned();
                let consumed = close + QWEN_XML_CLOSE.len();
                skip_pending_prefix_at(builder, &mut pending_head, consumed);
                builder.qwen_xml_state = OpenAiChatCompatibilityState::Scanning { scan_from: 0 };
                let calls = parse_compat_tool_calls(&block)?;
                buffer_compat_tool_calls(builder, calls)?;
            }
            OpenAiChatCompatibilityState::FunctionBody { scan_from } => {
                const FUNCTION_CLOSE: &str = "</function>";
                let pending = &builder.qwen_xml_pending[pending_head..];
                let Some(close) = pending[scan_from..]
                    .find(FUNCTION_CLOSE)
                    .map(|offset| scan_from + offset + FUNCTION_CLOSE.len())
                else {
                    builder.qwen_xml_state = OpenAiChatCompatibilityState::FunctionBody {
                        scan_from: next_scan_offset(
                            pending,
                            FUNCTION_OPEN_PREFIX.len(),
                            FUNCTION_CLOSE.len(),
                        ),
                    };
                    compact_pending_prefix(builder, pending_head);
                    return Ok(());
                };
                builder.qwen_xml_state =
                    OpenAiChatCompatibilityState::FunctionClosed { close_end: close };
            }
            OpenAiChatCompatibilityState::FunctionClosed { close_end } => {
                let pending = &builder.qwen_xml_pending[pending_head..];
                let trailing = &pending[close_end..];
                if trailing.len() < QWEN_XML_CLOSE.len() && QWEN_XML_CLOSE.starts_with(trailing) {
                    compact_pending_prefix(builder, pending_head);
                    return Ok(());
                }
                let block = pending[..close_end].to_owned();
                let consumed = if trailing.starts_with(QWEN_XML_CLOSE) {
                    close_end + QWEN_XML_CLOSE.len()
                } else {
                    close_end
                };
                skip_pending_prefix_at(builder, &mut pending_head, consumed);
                builder.qwen_xml_state = OpenAiChatCompatibilityState::Scanning { scan_from: 0 };
                let calls = parse_compat_tool_calls(&block)?;
                buffer_compat_tool_calls(builder, calls)?;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompatMarker {
    ToolOutputLocked,
    ToolCall,
    Function,
}

const MAX_COMPAT_MARKER_LEN: usize = TOOL_OUTPUT_LOCKED.len();

fn earliest_compat_marker(input: &str, scan_from: usize) -> Option<(usize, CompatMarker)> {
    let suffix = &input[scan_from.min(input.len())..];
    [
        suffix
            .find(TOOL_OUTPUT_LOCKED)
            .map(|index| (scan_from + index, CompatMarker::ToolOutputLocked)),
        suffix
            .find(TOOL_CALL_OPEN_PREFIX)
            .map(|index| (scan_from + index, CompatMarker::ToolCall)),
        suffix
            .find(FUNCTION_OPEN_PREFIX)
            .map(|index| (scan_from + index, CompatMarker::Function)),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|(index, _)| *index)
}

fn next_scan_offset(input: &str, floor: usize, marker_len: usize) -> usize {
    let mut offset = input
        .len()
        .saturating_sub(marker_len.saturating_sub(1))
        .max(floor);
    while offset > floor && !input.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn take_pending_prefix_at(
    builder: &mut ResponseBuilder,
    pending_head: &mut usize,
    bytes: usize,
) -> String {
    let start = *pending_head;
    let end = start + bytes;
    let value = builder.qwen_xml_pending[start..end].to_owned();
    *pending_head = end;
    builder.release_buffered_content(bytes);
    value
}

fn skip_pending_prefix_at(builder: &mut ResponseBuilder, pending_head: &mut usize, bytes: usize) {
    *pending_head += bytes;
    builder.release_buffered_content(bytes);
}

fn compact_pending_prefix(builder: &mut ResponseBuilder, pending_head: usize) {
    if pending_head > 0 {
        builder.qwen_xml_pending.drain(..pending_head);
    }
}

fn drain_pending_prefix(builder: &mut ResponseBuilder, bytes: usize) -> String {
    let value = builder.qwen_xml_pending.drain(..bytes).collect();
    builder.release_buffered_content(bytes);
    value
}

fn discard_pending_prefix(builder: &mut ResponseBuilder, bytes: usize) {
    builder.qwen_xml_pending.drain(..bytes);
    builder.release_buffered_content(bytes);
}

fn buffer_compat_tool_calls(
    builder: &mut ResponseBuilder,
    calls: Vec<(String, String)>,
) -> Result<(), AiError> {
    if builder
        .qwen_xml_buffered_calls
        .len()
        .checked_add(calls.len())
        .is_none_or(|count| count > MAX_RESPONSE_PARTS)
    {
        return Err(AiError::Decode(DecodeError::TooManyResponseParts));
    }
    let mut bytes = 0usize;
    for (name, arguments) in &calls {
        if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
            return Err(AiError::Decode(DecodeError::ToolArgumentsTooLarge));
        }
        bytes = bytes
            .checked_add(name.len())
            .and_then(|total| total.checked_add(arguments.len()))
            .ok_or(AiError::Decode(DecodeError::ResponseTooLarge))?;
    }
    builder.reserve_buffered_content(bytes)?;
    builder.qwen_xml_buffered_calls.extend(calls);
    Ok(())
}

fn clear_buffered_compat_tool_calls(builder: &mut ResponseBuilder) {
    let bytes = builder
        .qwen_xml_buffered_calls
        .iter()
        .map(|(name, arguments)| name.len().saturating_add(arguments.len()))
        .sum();
    builder.qwen_xml_buffered_calls.clear();
    builder.release_buffered_content(bytes);
}

fn emit_compat_tool_call(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    name: String,
    arguments_json: String,
) -> Result<(), AiError> {
    close_open_text_parts(events, builder)?;
    builder.set_stop_reason(StopReason::ToolUse);
    let call_number = builder.qwen_xml_call_count;
    builder.qwen_xml_call_count = builder.qwen_xml_call_count.saturating_add(1);
    let index = get_canonical_index(builder, &format!("qwen_xml_tool_{call_number}"));
    emit_event(
        events,
        builder,
        StreamEvent::ToolCallStart {
            index,
            id: ToolCallId(format!("qwen_xml_call_{}", call_number + 1)),
            name,
        },
    )?;
    emit_event(
        events,
        builder,
        StreamEvent::ToolCallArgsDelta {
            index,
            delta: arguments_json,
        },
    )?;
    emit_event(events, builder, StreamEvent::ToolCallEnd { index })
}

fn emit_text_without_locked_marker(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    delta: &str,
) -> Result<(), AiError> {
    builder.reserve_buffered_content(delta.len())?;
    builder.qwen_xml_pending.push_str(delta);
    while let Some(index) = builder.qwen_xml_pending.find(TOOL_OUTPUT_LOCKED) {
        if index > 0 {
            let text = drain_pending_prefix(builder, index);
            emit_text_delta(events, builder, &text)?;
        }
        discard_pending_prefix(builder, TOOL_OUTPUT_LOCKED.len());
        builder.tool_output_locked_seen = true;
    }
    let keep = marker_suffix_len(&builder.qwen_xml_pending, TOOL_OUTPUT_LOCKED);
    let flush = builder.qwen_xml_pending.len().saturating_sub(keep);
    if flush > 0 {
        let text = drain_pending_prefix(builder, flush);
        emit_text_delta(events, builder, &text)?;
    }
    Ok(())
}

fn close_open_text_parts(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    let open: Vec<usize> = builder
        .text_buffers
        .keys()
        .filter(|index| !builder.ended_indices.contains(index))
        .copied()
        .collect();
    for index in open {
        emit_event(events, builder, StreamEvent::TextEnd { index })?;
    }
    Ok(())
}

fn flush_qwen_xml_pending(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    let mut pending = std::mem::take(&mut builder.qwen_xml_pending);
    builder.release_buffered_content(pending.len());
    builder.qwen_xml_state = OpenAiChatCompatibilityState::default();
    if pending.contains(TOOL_OUTPUT_LOCKED) {
        pending = pending.replace(TOOL_OUTPUT_LOCKED, "");
        builder.tool_output_locked_seen = true;
    }
    let trimmed = pending.trim();

    // A provider may emit compatibility content first and structured calls in
    // a later chunk. Structured calls are authoritative: discard only content
    // that successfully parses (or structurally identifies) as the duplicate
    // call, while preserving unrelated prose.
    if builder.native_tool_call_seen {
        clear_buffered_compat_tool_calls(builder);
        let duplicate_xml =
            trimmed.contains(TOOL_CALL_OPEN_PREFIX) || trimmed.contains(FUNCTION_OPEN_PREFIX);
        let duplicate_json = builder.buffer_ambiguous_compatibility_content
            && json_tool_candidate(trimmed)
            && (parse_compat_tool_calls(trimmed).is_ok() || looks_like_json_tool_envelope(trimmed));
        if !trimmed.is_empty() && !duplicate_xml && !duplicate_json {
            emit_text_delta(events, builder, &pending)?;
        }
        return Ok(());
    }

    let mut trailing_text = None;
    if !trimmed.is_empty()
        && builder.buffer_ambiguous_compatibility_content
        && json_tool_candidate(trimmed)
    {
        match parse_compat_tool_calls(trimmed) {
            Ok(calls) => buffer_compat_tool_calls(builder, calls)?,
            Err(error) if looks_like_json_tool_envelope(trimmed) => return Err(error),
            Err(_) => trailing_text = Some(pending),
        }
    } else if !trimmed.is_empty()
        && (trimmed.contains(TOOL_CALL_OPEN_PREFIX) || trimmed.contains(FUNCTION_OPEN_PREFIX))
    {
        if (trimmed.contains(TOOL_CALL_OPEN_PREFIX) && !trimmed.contains(QWEN_XML_CLOSE))
            || (trimmed.contains(FUNCTION_OPEN_PREFIX) && !trimmed.contains("</function>"))
        {
            return Err(invalid_tool_field("incomplete XML tool call"));
        }
        let calls = parse_compat_tool_calls(strip_compat_outer(trimmed))?;
        buffer_compat_tool_calls(builder, calls)?;
    } else if !trimmed.is_empty() {
        trailing_text = Some(pending);
    }

    for (name, arguments) in std::mem::take(&mut builder.qwen_xml_buffered_calls) {
        builder.release_buffered_content(name.len().saturating_add(arguments.len()));
        emit_compat_tool_call(events, builder, name, arguments)?;
    }
    if let Some(text) = trailing_text {
        emit_text_delta(events, builder, &text)?;
    }
    Ok(())
}

fn marker_suffix_len(input: &str, marker: &str) -> usize {
    (1..=input.len().min(marker.len().saturating_sub(1)))
        .rev()
        .find(|&len| input.ends_with(&marker[..len]))
        .unwrap_or(0)
}

fn compatibility_marker_suffix_len(input: &str, include_json_fence: bool) -> usize {
    [
        TOOL_CALL_OPEN_PREFIX,
        FUNCTION_OPEN_PREFIX,
        TOOL_OUTPUT_LOCKED,
    ]
    .into_iter()
    .chain(include_json_fence.then_some("```json"))
    .chain(include_json_fence.then_some("```JSON"))
    .map(|marker| marker_suffix_len(input, marker))
    .max()
    .unwrap_or_default()
}

fn content_may_contain_tool_call(text: &str) -> bool {
    text.contains(TOOL_CALL_OPEN_PREFIX)
        || text.contains(FUNCTION_OPEN_PREFIX)
        || text.contains(TOOL_OUTPUT_LOCKED)
        || json_tool_candidate(text)
}

fn json_tool_candidate(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || trimmed.starts_with("```json")
        || trimmed.starts_with("```JSON")
}

fn looks_like_json_tool_envelope(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    (lower.contains("\"arguments\"")
        || lower.contains("'arguments'")
        || lower.contains("arguments:"))
        && (lower.contains("\"name\"")
            || lower.contains("'name'")
            || lower.contains("name:")
            || lower.contains("\"tool\"")
            || lower.contains("'tool'")
            || lower.contains("tool:")
            || lower.contains("\"function\"")
            || lower.contains("'function'")
            || lower.contains("function:"))
}

fn strip_compat_outer(input: &str) -> &str {
    let trimmed = input.trim();
    if trimmed.starts_with(TOOL_CALL_OPEN_PREFIX) {
        if let Some(open_end) = trimmed.find('>') {
            let body = &trimmed[open_end + 1..];
            return body
                .strip_suffix(QWEN_XML_CLOSE)
                .map(str::trim)
                .unwrap_or(body);
        }
    }
    trimmed
}

fn parse_compat_tool_calls(block: &str) -> Result<Vec<(String, String)>, AiError> {
    let trimmed = strip_compat_outer(block);
    if json_tool_candidate(trimmed) {
        let value = crate::json_repair::parse_json_value(trimmed).map_err(AiError::Decode)?;
        return calls_from_json_value(&value);
    }

    if let Some(function) = trimmed.find(FUNCTION_OPEN_PREFIX) {
        if !trimmed[..function].trim().is_empty() {
            return Err(invalid_tool_field(
                "unexpected content before XML function call",
            ));
        }
        return parse_xml_function_call(&trimmed[function..]).map(|call| vec![call]);
    }

    if let (Some(name), Some(arguments)) = (
        xml_element(trimmed, "name"),
        xml_element(trimmed, "arguments"),
    ) {
        let arguments = crate::json_repair::normalize_json_object(&xml_unescape(arguments.trim()))
            .map_err(AiError::Decode)?;
        return Ok(vec![(xml_unescape(name.trim()), arguments)]);
    }

    Err(invalid_tool_field(
        "content looked like a tool call but no supported XML/JSON envelope was found",
    ))
}

fn calls_from_json_value(value: &serde_json::Value) -> Result<Vec<(String, String)>, AiError> {
    if let Some(calls) = value.as_array() {
        let mut parsed = Vec::with_capacity(calls.len());
        for call in calls {
            parsed.extend(calls_from_json_value(call)?);
        }
        if parsed.is_empty() {
            return Err(invalid_tool_field("JSON tool-call array was empty"));
        }
        return Ok(parsed);
    }

    let object = value
        .as_object()
        .ok_or_else(|| invalid_tool_field("JSON tool call must be an object or array"))?;
    if let Some(calls) = object.get("tool_calls").or_else(|| object.get("calls")) {
        return calls_from_json_value(calls);
    }

    let nested = object
        .get("function")
        .and_then(serde_json::Value::as_object);
    let name = nested
        .and_then(|function| function.get("name"))
        .or_else(|| object.get("name"))
        .or_else(|| object.get("tool"))
        .or_else(|| object.get("function_name"))
        .or_else(|| object.get("function").filter(|value| value.is_string()))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_tool_field("JSON tool call is missing a function name"))?;
    if name.chars().any(char::is_whitespace) {
        return Err(invalid_tool_field(
            "JSON tool-call name contains whitespace",
        ));
    }

    let arguments = nested
        .and_then(|function| {
            function
                .get("arguments")
                .or_else(|| function.get("args"))
                .or_else(|| function.get("parameters"))
                .or_else(|| function.get("input"))
        })
        .or_else(|| object.get("arguments"))
        .or_else(|| object.get("args"))
        .or_else(|| object.get("parameters"))
        .or_else(|| object.get("input"));
    let arguments = match arguments {
        None | Some(serde_json::Value::Null) => "{}".to_string(),
        Some(serde_json::Value::String(arguments)) => {
            crate::json_repair::normalize_json_object(arguments).map_err(AiError::Decode)?
        }
        Some(arguments) if arguments.is_object() => serde_json::to_string(arguments)
            .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?,
        Some(_) => return Err(invalid_tool_field("tool arguments must be a JSON object")),
    };
    Ok(vec![(name.to_string(), arguments)])
}

fn parse_xml_function_call(input: &str) -> Result<(String, String), AiError> {
    let open_end = input
        .find('>')
        .ok_or_else(|| invalid_tool_field("XML function tag is incomplete"))?;
    let name = tag_name(&input[..=open_end], "function")?;
    let body_start = open_end + 1;
    let body_end = input[body_start..]
        .find("</function>")
        .map(|relative| body_start + relative)
        .ok_or_else(|| invalid_tool_field("XML function tag is incomplete"))?;
    let trailing = input
        .get(body_end + "</function>".len()..)
        .unwrap_or_default()
        .trim();
    if !trailing.is_empty() && trailing != QWEN_XML_CLOSE {
        return Err(invalid_tool_field(
            "unexpected content after XML function call",
        ));
    }
    let body = &input[body_start..body_end];

    if !body.contains("<parameter") {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Ok((name, "{}".to_string()));
        }
        if json_tool_candidate(trimmed) {
            let arguments =
                crate::json_repair::normalize_json_object(trimmed).map_err(AiError::Decode)?;
            return Ok((name, arguments));
        }
        return Err(invalid_tool_field(
            "XML function contains text outside a parameter",
        ));
    }

    let mut arguments = serde_json::Map::new();
    let mut cursor = 0usize;
    while cursor < body.len() {
        let rest = &body[cursor..];
        let Some(relative_open) = rest.find("<parameter") else {
            if !rest.trim().is_empty() {
                return Err(invalid_tool_field(
                    "XML function contains text outside a parameter",
                ));
            }
            break;
        };
        if !rest[..relative_open].trim().is_empty() {
            return Err(invalid_tool_field(
                "XML function contains text outside a parameter",
            ));
        }
        let open = cursor + relative_open;
        let open_end = body[open..]
            .find('>')
            .map(|relative| open + relative)
            .ok_or_else(|| invalid_tool_field("XML parameter tag is incomplete"))?;
        let key = tag_name(&body[open..=open_end], "parameter")?;
        let value_start = open_end + 1;
        let explicit_close = body[value_start..]
            .find("</parameter>")
            .map(|relative| value_start + relative);
        let next_parameter = body[value_start..]
            .find("<parameter")
            .map(|relative| value_start + relative);
        let value_end = explicit_close.or(next_parameter).unwrap_or(body.len());
        let raw = xml_unescape(body[value_start..value_end].trim());
        arguments.insert(key, qwen_xml_value(&raw));
        cursor = explicit_close
            .map(|close| close + "</parameter>".len())
            .unwrap_or(value_end);
    }

    serde_json::to_string(&serde_json::Value::Object(arguments))
        .map(|arguments| (name, arguments))
        .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))
}

fn tag_name(tag: &str, kind: &str) -> Result<String, AiError> {
    let inner = tag
        .strip_prefix('<')
        .and_then(|tag| tag.strip_suffix('>'))
        .ok_or_else(|| invalid_tool_field("malformed XML tool tag"))?;
    let remainder = inner
        .strip_prefix(kind)
        .ok_or_else(|| invalid_tool_field("unexpected XML tool tag"))?
        .trim();
    let raw = if let Some(value) = remainder.strip_prefix('=') {
        value.trim()
    } else if let Some(value) = remainder.strip_prefix("name=") {
        value.trim()
    } else if let Some(value) = remainder.strip_prefix("name =") {
        value.trim()
    } else {
        remainder
    };
    let name = raw.trim_matches(['\'', '"']).trim();
    if name.is_empty()
        || name.chars().any(char::is_whitespace)
        || !name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '-' | '.'))
    {
        return Err(invalid_tool_field("XML tool tag has an invalid name"));
    }
    Ok(name.to_string())
}

fn xml_element<'a>(input: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = input.find(&open)? + open.len();
    let end = input[start..].find(&close)? + start;
    Some(&input[start..end])
}

fn invalid_tool_field(message: impl Into<String>) -> AiError {
    AiError::Decode(DecodeError::InvalidProviderField(message.into()))
}

fn qwen_xml_value(raw: &str) -> serde_json::Value {
    if raw.is_empty() {
        return serde_json::Value::String(String::new());
    }
    if let Ok(value) = crate::json_repair::parse_json_value(raw) {
        return value;
    }
    if raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2 {
        return serde_json::Value::String(raw[1..raw.len() - 1].to_string());
    }
    serde_json::Value::String(raw.to_string())
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn get_canonical_index(builder: &mut ResponseBuilder, key: &str) -> usize {
    if let Some(&idx) = builder.provider_to_canonical_indices.get(key) {
        idx
    } else {
        let idx = builder.next_canonical_index;
        builder.next_canonical_index += 1;
        builder
            .provider_to_canonical_indices
            .insert(key.to_string(), idx);
        idx
    }
}

/// Canonical index for `key`'s current segment, reopening a fresh segment when
/// the provider kept streaming a part kind after closing it (deltas arriving
/// after a finish chunk). Reopening allocates from the monotonic counter, so
/// the fresh index can never collide with an existing part.
fn segment_index(builder: &mut ResponseBuilder, key: &str) -> usize {
    let idx = get_canonical_index(builder, key);
    if builder.ended_indices.contains(&idx) {
        builder.provider_to_canonical_indices.remove(key);
        get_canonical_index(builder, key)
    } else {
        idx
    }
}

/// Emits the `*End` event for every part that is still open, exactly once, in
/// canonical index order. Shared by the `finish_reason` path and the terminal
/// `[DONE]` path so both tolerate providers that duplicate or omit the finish
/// chunk.
fn close_open_parts(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    // A tool-call key whose id/name never arrived cannot form a ToolCall.
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

    let mut open: Vec<usize> = builder
        .text_buffers
        .keys()
        .chain(builder.reasoning_text_buffers.keys())
        .chain(builder.tool_call_builders.keys())
        .filter(|idx| !builder.ended_indices.contains(idx))
        .cloned()
        .collect();
    open.sort_unstable();
    open.dedup();
    for idx in open {
        if builder.text_buffers.contains_key(&idx) {
            emit_event(events, builder, StreamEvent::TextEnd { index: idx })?;
        } else if builder.reasoning_text_buffers.contains_key(&idx) {
            emit_event(events, builder, StreamEvent::ReasoningEnd { index: idx })?;
        } else if builder.tool_call_builders.contains_key(&idx) {
            emit_event(events, builder, StreamEvent::ToolCallEnd { index: idx })?;
        }
    }
    Ok(())
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
    // Prefer the nested OpenAI field when both aliases are present. They are
    // alternate spellings of the same bucket, not additive counters.
    let cache_read = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .or(usage.prompt_cache_hit_tokens)
        .unwrap_or(0);
    let cache_write = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_write_tokens)
        .unwrap_or(0);
    // Chat's documented `prompt_tokens` includes cache reads and writes.
    // Gateways occasionally report detail counters larger than that total;
    // mirror Pi's compatibility behavior by saturating the full-rate bucket
    // instead of failing an otherwise completed response.
    let input = usage
        .prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let reported_output = usage.completion_tokens;
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .map(|d| d.reasoning_tokens)
        .unwrap_or(0);
    // OpenAI defines reasoning as a subset of completion tokens. A few
    // gateways instead report visible completion and reasoning separately; in
    // that shape, combine them so the canonical subset invariant still holds.
    let output = if reasoning > reported_output {
        reported_output
            .checked_add(reasoning)
            .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?
    } else {
        reported_output
    };
    let total = input
        .checked_add(cache_read)
        .and_then(|value| value.checked_add(cache_write))
        .and_then(|value| value.checked_add(output))
        .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?;
    Ok(Usage {
        input_tokens: input,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cache_write_1h_tokens: 0,
        output_tokens: output,
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
            display_name: None,
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
                        min_effort: crate::types::ReasoningEffort::Minimal,
                        max_effort: crate::types::ReasoningEffort::High,
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
            transport: crate::types::EndpointTransport::Http,
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
        let mut req = Request {
            system: Some("System instructions".to_string()),
            messages: vec![Message::User(UserMessage {
                content: vec![
                    UserPart::Text("Hel".to_string()),
                    UserPart::Text("lo".to_string()),
                ],
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
        assert!(
            body.get("max_completion_tokens").is_none() && body.get("max_tokens").is_none(),
            "local model limits must not become provider request parameters"
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "System instructions");
        assert_eq!(body["messages"][1]["role"], "user");
        // A plain text user message uses the string form accepted by both
        // OpenAI and text-only OpenAI-compatible providers such as DeepSeek.
        assert_eq!(body["messages"][1]["content"], "Hello");

        req.max_output_tokens = Some(1000);
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["max_completion_tokens"], 1000);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn system_message_reasoning_mode_keeps_qwen_compatible_role() {
        let mut model = make_test_model(false, false, false, true, true, false);
        Arc::make_mut(&mut model.spec)
            .capabilities
            .reasoning
            .as_mut()
            .unwrap()
            .openai_chat_mode = OpenAiChatReasoningMode::SystemMessage;
        let req = Request {
            system: Some("system prompt".to_string()),
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("hello".to_string())],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Effort(ReasoningEffort::High),
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn provider_reasoning_values_are_preserved_on_the_wire() {
        let mut model = make_test_model(false, false, false, true, true, false);
        let capability = Arc::make_mut(&mut model.spec)
            .capabilities
            .reasoning
            .as_mut()
            .unwrap();
        capability.control = crate::types::ReasoningControl::Toggle;
        capability.openai_chat_mode = OpenAiChatReasoningMode::ProviderValues {
            values: vec!["none".into(), "default".into()],
            default: Some("default".into()),
            system_message: true,
        };
        let mut req = Request {
            system: Some("system prompt".to_string()),
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
            session_id: None,
        };

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["reasoning_effort"], "none");

        req.reasoning = ReasoningConfig::On;
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["reasoning_effort"], "default");

        let capability = Arc::make_mut(&mut model.spec)
            .capabilities
            .reasoning
            .as_mut()
            .unwrap();
        capability.control = crate::types::ReasoningControl::Effort;
        capability.openai_chat_mode = OpenAiChatReasoningMode::ProviderValues {
            values: vec!["none".into(), "low".into(), "high".into()],
            default: Some("low".into()),
            system_message: true,
        };
        capability.min_effort = ReasoningEffort::Low;
        capability.max_effort = ReasoningEffort::High;
        req.reasoning = ReasoningConfig::Effort(ReasoningEffort::High);
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
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
            messages: vec![
                Message::User(UserMessage {
                    content: vec![UserPart::Text("look this up".to_string())],
                }),
                Message::Assistant(AssistantMessage {
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
                }),
            ],
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
        assert!(
            body.get("max_completion_tokens").is_none() && body.get("max_tokens").is_none(),
            "DeepSeek must not receive the local capacity reserve as a generated cap"
        );
        // DeepSeek rejects `minimal`; its lowest accepted effort is `low`.
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["messages"][1]["content"], "look this up");
        assert_eq!(
            body["messages"][2]["reasoning_content"],
            "I need the tool result first."
        );
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");

        req.max_output_tokens = Some(1000);
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["max_tokens"], 1000);
        assert!(body.get("max_completion_tokens").is_none());

        req.max_output_tokens = None;
        req.reasoning = ReasoningConfig::Off;
        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_only_local_turn_replays_as_assistant_content() {
        let model = make_test_model(false, false, false, false, false, false);
        let request = Request {
            system: None,
            messages: vec![Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Reasoning(ReasoningPart {
                    text: Some("I need to inspect the picker first.".into()),
                    state: None,
                })],
                model: model.spec.id.clone(),
                protocol: Protocol::OpenAiChat,
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

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &request).unwrap().body).unwrap();
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(
            body["messages"][0]["content"],
            "I need to inspect the picker first."
        );
        assert!(body["messages"][0].get("reasoning_content").is_none());
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
    fn test_decode_response_recovers_qwen_xml_tool_call() {
        let model = make_test_model(false, false, false, true, false, false);
        let raw_json = r#"{
            "id": "chatcmpl-qwen-xml",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "<tool_call><function=read><parameter=path>README.md</parameter></function></tool_call>"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        }"#;

        let response = decode_response(&model, raw_json.as_bytes(), None).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        let call = response
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("Qwen XML call present");
        assert_eq!(call.name, "read");
        assert_eq!(
            call.arguments_value().unwrap(),
            serde_json::json!({"path": "README.md"})
        );
    }

    #[test]
    fn compatibility_parser_accepts_common_json_and_xml_tool_dialects() {
        for (raw, expected_name, expected_args) in [
            (
                r#"{"name":"read","arguments":{"path":"README.md",}}"#,
                "read",
                serde_json::json!({"path": "README.md"}),
            ),
            (
                r#"{"type":"function","function":{"name":"exec","arguments":"{'command':'pwd',}"}}"#,
                "exec",
                serde_json::json!({"command": "pwd"}),
            ),
            (
                r#"{name:'read', arguments:{path:'src/lib.rs',},}"#,
                "read",
                serde_json::json!({"path": "src/lib.rs"}),
            ),
            (
                r#"<function name="read"><parameter name="path">src/lib.rs</parameter></function>"#,
                "read",
                serde_json::json!({"path": "src/lib.rs"}),
            ),
            (
                r#"<function read><parameter=path>src/main.rs</parameter></function>"#,
                "read",
                serde_json::json!({"path": "src/main.rs"}),
            ),
        ] {
            let calls = parse_compat_tool_calls(raw).unwrap();
            assert_eq!(calls.len(), 1, "{raw}");
            assert_eq!(calls[0].0, expected_name, "{raw}");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&calls[0].1).unwrap(),
                expected_args,
                "{raw}"
            );
        }

        let calls = parse_compat_tool_calls(
            r#"[{"tool":"read","args":{"path":"a"}},{"name":"read","input":{"path":"b"}}]"#,
        )
        .unwrap();
        assert_eq!(calls.len(), 2);

        for truncated in [
            r#"{"name":"exec","arguments":{"command":"rm -rf /"#,
            r#"{name:'exec',arguments:{command:'rm -rf /"#,
            "<function=exec><parameter=command>rm -rf /",
        ] {
            assert!(
                parse_compat_tool_calls(truncated).is_err(),
                "accepted truncated call {truncated:?}"
            );
        }
    }

    #[test]
    fn completed_native_tool_arguments_are_repaired_conservatively() {
        let model = make_test_model(false, false, false, true, false, false);
        let raw_json = r#"{
            "id":"repair",
            "choices":[{
                "message":{"tool_calls":[{
                    "id":"call_1","type":"function",
                    "function":{"name":"read","arguments":"{'path':'C:\\Users\\example',}"}
                }]},
                "finish_reason":"tool_calls"
            }],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }"#;
        let response = decode_response(&model, raw_json.as_bytes(), None).unwrap();
        let call = response
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.arguments_value().unwrap()["path"], r"C:\Users\example");
    }

    #[test]
    fn completed_bare_json_tool_call_is_recovered_from_content() {
        let model = make_test_model(false, false, false, true, false, false);
        let raw_json = r#"{
            "id":"bare-json",
            "choices":[{
                "message":{"content":"```json\n{\"tool\":\"read\",\"arguments\":{\"path\":\"README.md\",}}\n```"},
                "finish_reason":"stop"
            }],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }"#;
        let response = decode_response(&model, raw_json.as_bytes(), None).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert!(matches!(
            &response.message.content[..],
            [AssistantPart::ToolCall(call)] if call.name == "read"
        ));
    }

    #[test]
    fn tool_output_locked_is_suppressed_and_requests_recovery() {
        let model = make_test_model(false, false, false, true, false, false);
        let raw_json = r#"{
            "id":"locked",
            "choices":[{
                "message":{"content":"I will inspect it now.\n[tool_output_locked]"},
                "finish_reason":"stop"
            }],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }"#;
        let response = decode_response(&model, raw_json.as_bytes(), None).unwrap();
        assert_eq!(
            response.stop_reason,
            StopReason::Other("tool_output_locked".to_string())
        );
        let text = response
            .message
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(!text.contains("tool_output_locked"));
        assert!(text.contains("inspect it"));
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
    fn anthropic_style_chat_cache_markers_cover_system_conversation_and_tools() {
        let mut model = make_test_model(false, false, false, true, false, false);
        Arc::make_mut(&mut model.spec).cache.cache_control_format =
            Some(crate::types::CacheControlFormat::Anthropic);
        let request = Request {
            system: Some("system".to_string()),
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("latest user turn".to_string())],
            })],
            tools: vec![crate::types::ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: Some("stable-session".to_string()),
        };

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &request).unwrap().body).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn chat_usage_accepts_gateway_cache_aliases_and_keeps_buckets_disjoint() {
        let usage: ChatUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 1_000,
            "completion_tokens": 20,
            "total_tokens": 1_020,
            "prompt_cache_hit_tokens": 700,
            "prompt_tokens_details": {
                "cache_write_tokens": 100
            },
            "completion_tokens_details": { "reasoning_tokens": 5 }
        }))
        .unwrap();
        let mapped = map_usage(&usage).unwrap();
        assert_eq!(mapped.input_tokens, 200);
        assert_eq!(mapped.cache_read_tokens, 700);
        assert_eq!(mapped.cache_write_tokens, 100);
        assert_eq!(mapped.reasoning_tokens, 5);

        // When both read spellings occur, they describe one counter. The
        // documented nested field wins rather than double-counting a hit.
        let usage: ChatUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 1,
            "total_tokens": 101,
            "prompt_cache_hit_tokens": 80,
            "prompt_tokens_details": { "cached_tokens": 60 }
        }))
        .unwrap();
        assert_eq!(map_usage(&usage).unwrap().cache_read_tokens, 60);

        // A completed gateway response must not be discarded merely because
        // its cache detail counters use a broader denominator than
        // `prompt_tokens`.
        let usage: ChatUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 3,
            "total_tokens": 103,
            "prompt_tokens_details": {
                "cached_tokens": 120,
                "cache_write_tokens": 20
            },
            "completion_tokens_details": { "reasoning_tokens": 5 }
        }))
        .unwrap();
        let mapped = map_usage(&usage).unwrap();
        assert_eq!(mapped.input_tokens, 0);
        assert_eq!(mapped.cache_read_tokens, 120);
        assert_eq!(mapped.cache_write_tokens, 20);
        assert_eq!(mapped.output_tokens, 8);
        assert_eq!(mapped.reasoning_tokens, 5);
        assert_eq!(mapped.total_tokens, 148);
    }

    #[test]
    fn cache_retention_controls_openai_chat_key() {
        let mut model = make_test_model(false, false, false, false, false, false);
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

        let cache = &mut Arc::make_mut(&mut model.spec).cache;
        cache.send_session_affinity_headers = true;
        cache.session_affinity_format = Some(crate::types::SessionAffinityFormat::OpenRouter);
        let parts = build_request(&model, &req).unwrap();
        assert_eq!(
            parts.headers["x-session-id"],
            req.session_id.as_deref().unwrap()
        );
        assert!(parts.headers.get("x-client-request-id").is_none());

        Arc::make_mut(&mut model.spec).cache.session_affinity_format =
            Some(crate::types::SessionAffinityFormat::OpenAi);
        let parts = build_request(&model, &req).unwrap();
        assert_eq!(
            parts.headers["session_id"],
            req.session_id.as_deref().unwrap()
        );
        assert_eq!(
            parts.headers["x-client-request-id"],
            req.session_id.as_deref().unwrap()
        );
        assert_eq!(
            parts.headers["x-session-affinity"],
            req.session_id.as_deref().unwrap()
        );

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
    use super::{consume_qwen_xml_content, decode_stream_event};
    use crate::error::{AiError, DecodeError};
    use crate::protocol::harness;
    use crate::protocol::sse::SseEvent;
    use crate::stream::{
        ResponseBuilder, StreamEvent, MAX_RESPONSE_CONTENT_BYTES, MAX_RESPONSE_EVENTS,
        MAX_TOOL_ARGUMENT_BYTES,
    };
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

    fn content_event(content: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: serde_json::json!({
                "id": "compat-adversarial",
                "choices": [{"delta": {"content": content}}]
            })
            .to_string(),
        }
    }

    fn content_as_one_character_events(content: &str) -> Vec<u8> {
        let mut data = String::new();
        for character in content.chars() {
            data.push_str("data: ");
            data.push_str(&content_event(&character.to_string()).data);
            data.push_str("\n\n");
        }
        data.push_str(
            "data: {\"id\":\"compat-adversarial\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        data.into_bytes()
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
    async fn duplicate_finish_reason_closes_parts_once() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(
            &model,
            decode_stream_event,
            fx!("duplicate_finish_reason.sse"),
            0,
        )
        .await
        .unwrap();
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
        assert_eq!(
            joined_text(&events),
            "I'll help you fix the model picker filter. Let me first explore the repository."
        );
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.output_tokens, 20);
    }

    #[tokio::test]
    async fn missing_finish_reason_closes_parts_at_done() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(&model, decode_stream_event, fx!("no_finish_reason.sse"), 0)
            .await
            .unwrap();
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
        assert_eq!(joined_text(&events), "Done without a finish chunk");
        let resp = harness::finished(&events);
        assert_eq!(resp.usage.output_tokens, 4);
    }

    #[tokio::test]
    async fn openrouter_reasoning_alias_decodes() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(&model, decode_stream_event, fx!("reasoning_alias.sse"), 0)
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
        assert_eq!(joined_text(&events), "Answer: 42");
    }

    #[tokio::test]
    async fn duplicate_finish_reason_closes_tool_calls_once() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(
            &model,
            decode_stream_event,
            fx!("duplicate_finish_tool_calls.sse"),
            0,
        )
        .await
        .unwrap();
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(joined_text(&events), "Let me explore the repository.");
        let calls: Vec<_> = resp
            .message
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantPart::ToolCall(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "grep");
        assert_eq!(
            calls[0].arguments_value().unwrap(),
            serde_json::json!({"pattern": "foo"})
        );
        // Each part ended exactly once (§8).
        let ends = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    StreamEvent::TextEnd { .. } | StreamEvent::ToolCallEnd { .. }
                )
            })
            .count();
        assert_eq!(ends, 2);
    }

    #[tokio::test]
    async fn deltas_after_finish_reopen_a_fresh_segment() {
        // A provider that keeps streaming after a finish chunk must not trip
        // the §8 guard; the late text becomes a fresh canonical segment.
        let model = harness::model(Protocol::OpenAiChat, None);
        let data: &[u8] = b"data: {\"id\":\"gen-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"first\"}}]}\n\ndata: {\"id\":\"gen-x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"id\":\"gen-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"second\"}}]}\n\ndata: {\"id\":\"gen-x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let events = harness::drive(&model, decode_stream_event, data, 0)
            .await
            .unwrap();
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
        let texts: Vec<&str> = harness::finished(&events)
            .message
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[tokio::test]
    async fn qwen_xml_tool_call_is_recovered_from_content() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let events = harness::drive(
            &model,
            decode_stream_event,
            fx!("qwen_xml_tool_call.sse"),
            1,
        )
        .await
        .unwrap();
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert!(!joined_text(&events).contains("<tool_call>"));
        let call = resp
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("Qwen XML call present");
        assert_eq!(call.name, "read");
        assert_eq!(call.id.0, "qwen_xml_call_1");
        assert_eq!(
            call.arguments_value().unwrap(),
            serde_json::json!({"path": "src/main.rs"})
        );
    }

    #[test]
    fn ordinary_json_is_visible_before_stream_completion_by_default() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let event = content_event(r#"{"status":"working"}"#);
        let data = format!("data: {}\n\n", event.data);
        let (events, error) = harness::drive_raw(&model, decode_stream_event, data.as_bytes(), 1);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(joined_text(&events), r#"{"status":"working"}"#);
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { .. })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finished(_))));
    }

    #[tokio::test]
    async fn xml_compatibility_parser_recovers_one_character_events() {
        let model = harness::model(Protocol::OpenAiChat, None);
        for content in [
            r#"<tool_call><function name="read"><parameter name="path">README.md</parameter></function></tool_call>"#,
            r#"<function name="read"><parameter name="path">README.md</parameter></function></tool_call>"#,
        ] {
            let data = content_as_one_character_events(content);
            let events = harness::drive(&model, decode_stream_event, &data, 1)
                .await
                .unwrap();
            let call = harness::finished(&events)
                .message
                .content
                .iter()
                .find_map(|part| match part {
                    AssistantPart::ToolCall(call) => Some(call),
                    _ => None,
                })
                .expect("one-character XML stream should recover a tool call");
            assert_eq!(call.name, "read");
            assert_eq!(call.arguments_value().unwrap()["path"], "README.md");
            assert!(!joined_text(&events).contains("</tool_call>"));
        }
    }

    #[tokio::test]
    async fn explicitly_buffered_json_recovers_one_character_events() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let data =
            content_as_one_character_events(r#"{"name":"read","arguments":{"path":"README.md"}}"#);
        let events =
            harness::drive_with_compatibility_buffering(&model, decode_stream_event, &data, 1)
                .await
                .unwrap();
        let call = harness::finished(&events)
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("one-character JSON stream should recover a tool call");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments_value().unwrap()["path"], "README.md");
    }

    #[test]
    fn endless_ambiguous_candidate_hits_provider_event_cap_incrementally() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let mut builder = ResponseBuilder::new(
            model.spec.id.clone(),
            model.spec.protocol,
            model.spec.pricing.clone(),
        );
        builder.set_buffer_ambiguous_compatibility_content(true);

        decode_stream_event(&model, &content_event("{"), &mut builder).unwrap();
        let continuation = content_event(" ");
        for _ in 1..MAX_RESPONSE_EVENTS {
            decode_stream_event(&model, &continuation, &mut builder).unwrap();
        }
        let error = decode_stream_event(&model, &continuation, &mut builder).unwrap_err();
        assert!(matches!(
            error,
            AiError::Decode(DecodeError::TooManyStreamEvents)
        ));
        assert_eq!(builder.qwen_xml_pending.len(), MAX_RESPONSE_EVENTS);
    }

    #[test]
    fn qwen_pending_reserves_aggregate_bytes_before_append() {
        let mut builder = ResponseBuilder::new(
            crate::types::ModelId("m".to_string()),
            Protocol::OpenAiChat,
            None,
        );
        builder
            .reserve_buffered_content(MAX_RESPONSE_CONTENT_BYTES)
            .unwrap();
        let mut events = Vec::new();
        let error = consume_qwen_xml_content(&mut events, &mut builder, "{").unwrap_err();
        assert!(matches!(
            error,
            AiError::Decode(DecodeError::ResponseTooLarge)
        ));
        assert!(builder.qwen_xml_pending.is_empty());
    }

    #[test]
    fn arguments_before_tool_id_are_capped_before_append() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let mut builder = ResponseBuilder::new(
            model.spec.id.clone(),
            model.spec.protocol,
            model.spec.pricing.clone(),
        );
        let key = "tool_args_0".to_string();
        builder
            .reserve_buffered_content(MAX_TOOL_ARGUMENT_BYTES)
            .unwrap();
        builder
            .temp_buffers
            .insert(key.clone(), "x".repeat(MAX_TOOL_ARGUMENT_BYTES));
        let event = SseEvent {
            event: None,
            data: serde_json::json!({
                "id": "pre-id",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "x"}
                        }]
                    }
                }]
            })
            .to_string(),
        };
        let error = decode_stream_event(&model, &event, &mut builder).unwrap_err();
        assert!(matches!(
            error,
            AiError::Decode(DecodeError::ToolArgumentsTooLarge)
        ));
        assert_eq!(builder.temp_buffers[&key].len(), MAX_TOOL_ARGUMENT_BYTES);
    }

    #[tokio::test]
    async fn explicitly_buffered_json_and_xml_variants_are_recovered_across_boundaries() {
        let model = harness::model(Protocol::OpenAiChat, None);
        for content in [
            r#"{"name":"read","arguments":{"path":"README.md",}}"#,
            r#"<tool_call><function name="read"><parameter name="path">README.md</parameter></function></tool_call>"#,
        ] {
            let escaped = serde_json::to_string(content).unwrap();
            let data = format!(
                "data: {{\"id\":\"compat\",\"choices\":[{{\"delta\":{{\"content\":{escaped}}}}}]}}\n\ndata: {{\"id\":\"compat\",\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            for chunk in [1, 2, 7, data.len()] {
                let events = harness::drive_with_compatibility_buffering(
                    &model,
                    decode_stream_event,
                    data.as_bytes(),
                    chunk,
                )
                .await
                .unwrap();
                let response = harness::finished(&events);
                assert_eq!(response.stop_reason, StopReason::ToolUse, "{content}");
                let call = response
                    .message
                    .content
                    .iter()
                    .find_map(|part| match part {
                        AssistantPart::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .expect("compatibility call");
                assert_eq!(call.name, "read");
                assert_eq!(call.arguments_value().unwrap()["path"], "README.md");
            }
        }
    }

    #[tokio::test]
    async fn native_tool_call_supersedes_earlier_compatibility_content() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let xml = "<tool_call><function=read><parameter=path>duplicate.txt</parameter></function></tool_call>";
        let xml = serde_json::to_string(xml).unwrap();
        let data = format!(
            "data: {{\"id\":\"native-wins\",\"choices\":[{{\"delta\":{{\"content\":{xml}}}}}]}}\n\ndata: {{\"id\":\"native-wins\",\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"native_1\",\"type\":\"function\",\"function\":{{\"name\":\"read\",\"arguments\":\"{{\\\"path\\\":\\\"native.txt\\\"}}\"}}}}]}}}}]}}\n\ndata: {{\"id\":\"native-wins\",\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let events = harness::drive(&model, decode_stream_event, data.as_bytes(), 1)
            .await
            .unwrap();
        let calls = harness::finished(&events)
            .message
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.0, "native_1");
        assert_eq!(calls[0].arguments_value().unwrap()["path"], "native.txt");
        assert!(!joined_text(&events).contains("duplicate.txt"));
    }

    #[tokio::test]
    async fn streamed_tool_output_locked_marker_never_reaches_text() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let content = "about to inspect\n[tool_output_locked]";
        let escaped = serde_json::to_string(content).unwrap();
        let data = format!(
            "data: {{\"id\":\"locked\",\"choices\":[{{\"delta\":{{\"content\":{escaped}}}}}]}}\n\ndata: {{\"id\":\"locked\",\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let events = harness::drive(&model, decode_stream_event, data.as_bytes(), 1)
            .await
            .unwrap();
        assert!(!joined_text(&events).contains("tool_output_locked"));
        assert_eq!(
            harness::finished(&events).stop_reason,
            StopReason::Other("tool_output_locked".to_string())
        );
    }

    #[tokio::test]
    async fn incomplete_qwen_xml_tool_call_is_not_rendered_as_text() {
        let model = harness::model(Protocol::OpenAiChat, None);
        let data: &[u8] = b"data: {\"id\":\"qwen-xml-2\",\"choices\":[{\"delta\":{\"content\":\"<tool_call><function=read>\"}}]}\n\ndata: [DONE]\n\n";
        let error = harness::drive(&model, decode_stream_event, data, 0)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::error::AiError::Decode(crate::error::DecodeError::InvalidProviderField(_))
        ));
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
