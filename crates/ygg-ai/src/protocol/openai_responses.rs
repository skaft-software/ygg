//! OpenAI Responses private wire protocol codec.

use serde::{Deserialize, Serialize};

use crate::error::{AiError, ConfigError, DecodeError, ProviderError};
use crate::protocol::sse::SseEvent;
use crate::protocol::{
    cache_session_id, cache_session_id_for, emit_event, get_canonical_index, prompt_cache_key,
    prompt_cache_key_for, HttpRequestParts, WireImageUrl,
};
use crate::stream::{ResponseBuilder, StreamEvent};
use crate::types::{
    AssistantPart, CacheRetention, ImageSource, Media, Message, OutputFormat, Protocol,
    ReasoningConfig, ReasoningMode, ReasoningState, ReasoningStateKind, Request, StopReason,
    ToolCallId, ToolChoice, ToolDef, ToolResultPart, Usage, UserPart,
};
use crate::validate::{normalize_reasoning_config, normalize_request_reasoning, validate_request};

// --- Private OpenAI Responses Request DTOs ---

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    input: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
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
    AdditionalTools {
        role: String,
        tools: Vec<serde_json::Value>,
    },
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
        image_url: Option<WireImageUrl>,
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
        image_url: Option<WireImageUrl>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'static str>,
    // Request visible summary deltas in addition to encrypted continuation
    // state. Without this, reasoning-capable Codex models think silently.
    summary: &'static str,
}

#[derive(Serialize)]
struct ResponsesTextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<ResponsesFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verbosity: Option<&'static str>,
}

// Only non-default output formats produce a wire `text.format`. The private
// Codex route still emits `text` for its low-verbosity latency default.
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

fn opaque_input_item(item: ResponsesInputItem) -> crate::responses::ResponsesItem {
    crate::responses::ResponsesItem::new(
        serde_json::to_value(item).expect("private Responses input item serializes to an object"),
    )
    .expect("private Responses input item is always an object")
}

fn map_responses_tools(
    model: &crate::catalog::Model,
    tools: &[ToolDef],
) -> Option<Vec<ResponsesTool>> {
    if tools.is_empty() || !model.spec.capabilities.tools {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|tool| ResponsesTool {
                r#type: "function".to_owned(),
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
    )
}

fn map_responses_lite_tools(
    model: &crate::catalog::Model,
    tools: &[ToolDef],
) -> Vec<serde_json::Value> {
    if tools.is_empty() || !model.spec.capabilities.tools {
        return Vec::new();
    }
    let tools = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "strict": false,
                "parameters": tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    vec![serde_json::json!({
        "type": "namespace",
        "name": "functions",
        "description": "",
        "tools": tools,
    })]
}

fn responses_lite_prefix(
    model: &crate::catalog::Model,
    instructions: Option<&str>,
    tools: &[ToolDef],
) -> Vec<crate::responses::ResponsesItem> {
    let mut prefix = vec![opaque_input_item(ResponsesInputItem::AdditionalTools {
        role: "developer".to_owned(),
        tools: map_responses_lite_tools(model, tools),
    })];
    if let Some(instructions) = instructions.filter(|instructions| !instructions.is_empty()) {
        prefix.push(opaque_input_item(ResponsesInputItem::Message {
            role: "developer".to_owned(),
            content: vec![ResponsesContentPart::InputText {
                text: instructions.to_owned(),
            }],
        }));
    }
    prefix
}

fn responses_reasoning_effort(effort: crate::types::ReasoningEffort) -> &'static str {
    use crate::types::ReasoningEffort;

    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
        // Ultra is a host-orchestration tier; current Codex wire requests use
        // maximum model reasoning while the V2 runtime supplies delegation.
        ReasoningEffort::Ultra => "max",
    }
}

fn map_responses_reasoning(
    model: &crate::catalog::Model,
    reasoning: &ReasoningConfig,
    _reasoning_mode: ReasoningMode,
) -> Option<ResponsesReasoningConfig> {
    model.spec.capabilities.reasoning.as_ref()?;
    let effort = match reasoning {
        ReasoningConfig::Effort(effort) => Some(responses_reasoning_effort(*effort).to_owned()),
        ReasoningConfig::Off | ReasoningConfig::On | ReasoningConfig::Budget(_) => None,
    };
    let context = model
        .spec
        .capabilities
        .responses_lite
        .then_some("all_turns");
    (effort.is_some() || context.is_some()).then_some(ResponsesReasoningConfig {
        effort,
        context,
        summary: "auto",
    })
}

fn map_responses_text(
    model: &crate::catalog::Model,
    output_format: &OutputFormat,
) -> Option<ResponsesTextConfig> {
    let text_format = match output_format {
        OutputFormat::Text => None,
        _ if !model.spec.capabilities.structured_output => None,
        OutputFormat::JsonObject => Some(ResponsesFormat::JsonObject),
        OutputFormat::JsonSchema(schema) => Some(ResponsesFormat::JsonSchema {
            name: schema.name.clone(),
            description: schema.description.clone(),
            schema: schema.schema.clone(),
            strict: schema.strict,
        }),
    };
    let verbosity = model
        .endpoint
        .runtime
        .responses_profile
        .uses_low_verbosity()
        .then_some("low");
    (text_format.is_some() || verbosity.is_some()).then_some(ResponsesTextConfig {
        format: text_format,
        verbosity,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_compact_request(
    model: &crate::catalog::Model,
    mut input: crate::responses::ResponsesInput,
    instructions: Option<String>,
    tools: &[ToolDef],
    reasoning: &ReasoningConfig,
    reasoning_mode: ReasoningMode,
    output_format: &OutputFormat,
    cache_retention: CacheRetention,
    session_id: Option<&str>,
) -> crate::responses::ResponsesCompactRequest {
    // The private ChatGPT Codex compact route accepts the same active tool and
    // generation controls as normal Responses calls. Public OpenAI compact
    // currently exposes a narrower schema and may reject these extra fields.
    let responses_lite = model.spec.capabilities.responses_lite;
    let reasoning = normalize_reasoning_config(reasoning, &model.spec.capabilities);
    let rich_codex_schema = model
        .endpoint
        .runtime
        .responses_profile
        .supports_rich_compact_schema()
        || model.spec.cache.session_affinity_format
            == Some(crate::types::SessionAffinityFormat::Codex)
        || responses_lite;
    let mapped_tools = if responses_lite {
        None
    } else {
        rich_codex_schema
            .then(|| map_responses_tools(model, tools))
            .flatten()
            .map(|tools| {
                tools
                    .into_iter()
                    .map(|tool| serde_json::to_value(tool).expect("Responses tool serializes"))
                    .collect()
            })
    };
    let parallel_tool_calls = if responses_lite {
        // The internal Responses Lite route requires an explicit false even
        // when the model otherwise advertises parallel tool-call support.
        Some(false)
    } else {
        mapped_tools
            .as_ref()
            .map(|_| model.spec.capabilities.parallel_tool_calls)
    };
    let (input, instructions) = if responses_lite {
        input.strip_image_details_for_responses_lite();
        let mut items = responses_lite_prefix(model, instructions.as_deref(), tools);
        items.extend(input.into_items());
        (crate::responses::ResponsesInput::new(items), None)
    } else {
        (input, instructions)
    };
    let reasoning = rich_codex_schema
        .then(|| map_responses_reasoning(model, reasoning.as_ref(), reasoning_mode))
        .flatten()
        .map(|config| serde_json::to_value(config).expect("Responses reasoning serializes"));
    let text = rich_codex_schema
        .then(|| map_responses_text(model, output_format))
        .flatten()
        .map(|config| serde_json::to_value(config).expect("Responses text serializes"));
    crate::responses::ResponsesCompactRequest {
        model: model.spec.api_name.clone(),
        input,
        instructions,
        parallel_tool_calls,
        tools: mapped_tools,
        reasoning,
        text,
        prompt_cache_key: prompt_cache_key_for(cache_retention, session_id),
        session_id: cache_session_id_for(cache_retention, session_id).map(str::to_owned),
    }
}

pub(crate) fn responses_affinity_headers(
    model: &crate::catalog::Model,
    session_id: Option<&str>,
) -> Result<http::HeaderMap, AiError> {
    let mut headers = http::HeaderMap::new();
    if model.spec.capabilities.responses_lite {
        headers.insert(
            http::HeaderName::from_static("x-openai-internal-codex-responses-lite"),
            http::HeaderValue::from_static("true"),
        );
    }
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Ok(headers);
    };
    let value = http::HeaderValue::from_str(session_id)
        .map_err(|_| ConfigError::InvalidHeader("session affinity".into()))?;
    match model.spec.cache.session_affinity_format {
        Some(crate::types::SessionAffinityFormat::OpenRouter) => {
            headers.insert(http::HeaderName::from_static("x-session-id"), value);
        }
        // Mistral is a Chat route; accept the configuration here without
        // applying a Chat-only header to a Responses request.
        Some(crate::types::SessionAffinityFormat::Mistral) => {}
        Some(crate::types::SessionAffinityFormat::Codex) => {
            headers.insert(http::HeaderName::from_static("session-id"), value.clone());
            headers.insert(http::HeaderName::from_static("x-client-request-id"), value);
        }
        Some(crate::types::SessionAffinityFormat::OpenAiNoSession) => {
            headers.insert(
                http::HeaderName::from_static("x-client-request-id"),
                value.clone(),
            );
            headers.insert(http::HeaderName::from_static("x-session-affinity"), value);
        }
        Some(crate::types::SessionAffinityFormat::OpenAi) => {
            headers.insert(
                http::HeaderName::from_static("x-client-request-id"),
                value.clone(),
            );
            headers.insert(
                http::HeaderName::from_static("x-session-affinity"),
                value.clone(),
            );
            if model.spec.cache.send_session_id_header {
                headers.insert(http::HeaderName::from_static("session_id"), value);
            }
        }
        None => {
            headers.insert(
                http::HeaderName::from_static("x-client-request-id"),
                value.clone(),
            );
            if model.spec.cache.send_session_id_header {
                headers.insert(http::HeaderName::from_static("session_id"), value);
            }
        }
    }
    Ok(headers)
}

fn map_system_input(
    model: &crate::catalog::Model,
    system: Option<&str>,
) -> Vec<ResponsesInputItem> {
    let Some(system) = system else {
        return Vec::new();
    };
    let role = if model.spec.capabilities.reasoning.is_some() {
        "developer"
    } else {
        "system"
    };
    vec![ResponsesInputItem::Message {
        role: role.to_owned(),
        content: vec![ResponsesContentPart::InputText {
            text: system.to_owned(),
        }],
    }]
}

fn map_user_input(
    model: &crate::catalog::Model,
    user: &crate::types::UserMessage,
    preserve_tool_call_ids: bool,
    pending_tool_calls: &mut std::collections::BTreeSet<String>,
    synthetic_tool_results: &std::collections::HashSet<String>,
) -> Vec<ResponsesInputItem> {
    let mut input = Vec::new();
    let mut content = Vec::new();
    for part in &user.content {
        match part {
            UserPart::Text(text) => {
                content.push(ResponsesContentPart::InputText { text: text.clone() });
            }
            UserPart::Media(Media::Image(image)) => {
                if !model
                    .spec
                    .capabilities
                    .input_modalities
                    .contains(crate::types::Modality::Image)
                {
                    continue;
                }

                let (image_url, file_id) = match &image.source {
                    ImageSource::Url(url) => (Some(WireImageUrl::Url(url.to_string())), None),
                    ImageSource::Inline(bytes) => {
                        // No documented default MIME; do not guess a wire field
                        // (design §75). Validation already diagnosed the drop.
                        let Some(media_type) = image.media_type.as_ref() else {
                            continue;
                        };
                        (
                            Some(WireImageUrl::Inline {
                                media_type: media_type.to_string(),
                                data: bytes.clone(),
                            }),
                            None,
                        )
                    }
                    ImageSource::ProviderRef(reference) => {
                        // An expired or wrong-protocol provider ref is dropped
                        // (validation already emitted the diagnostic).
                        if !crate::validate::provider_ref_is_usable(
                            reference,
                            Protocol::OpenAiResponses,
                        ) {
                            continue;
                        }
                        (None, Some(reference.id.clone()))
                    }
                };

                let detail = image.detail.map(|detail| match detail {
                    crate::types::ImageDetail::Auto => "auto".to_owned(),
                    crate::types::ImageDetail::Low => "low".to_owned(),
                    crate::types::ImageDetail::High => "high".to_owned(),
                });
                content.push(ResponsesContentPart::InputImage {
                    image_url,
                    file_id,
                    detail,
                });
            }
            UserPart::Media(Media::Audio(_)) => {}
            UserPart::ToolResult(result) => {
                if synthetic_tool_results.contains(&result.tool_call_id.0) {
                    continue;
                }
                pending_tool_calls.remove(&result.tool_call_id.0);
                let mut outputs = Vec::new();
                for result_part in &result.content {
                    match result_part {
                        ToolResultPart::Text(text) => {
                            outputs
                                .push(ResponsesToolResultBlock::InputText { text: text.clone() });
                        }
                        ToolResultPart::Media(Media::Image(image)) => match &image.source {
                            ImageSource::Url(url) => {
                                outputs.push(ResponsesToolResultBlock::InputImage {
                                    image_url: Some(WireImageUrl::Url(url.to_string())),
                                    file_id: None,
                                });
                            }
                            ImageSource::Inline(bytes) => {
                                // Do not guess a wire MIME (§75); drop the part
                                // if absent.
                                if let Some(media_type) = image.media_type.as_ref() {
                                    outputs.push(ResponsesToolResultBlock::InputImage {
                                        image_url: Some(WireImageUrl::Inline {
                                            media_type: media_type.to_string(),
                                            data: bytes.clone(),
                                        }),
                                        file_id: None,
                                    });
                                }
                            }
                            ImageSource::ProviderRef(reference) => {
                                if crate::validate::provider_ref_is_usable(
                                    reference,
                                    Protocol::OpenAiResponses,
                                ) {
                                    outputs.push(ResponsesToolResultBlock::InputImage {
                                        image_url: None,
                                        file_id: Some(reference.id.clone()),
                                    });
                                }
                            }
                        },
                        ToolResultPart::Media(Media::Audio(_)) => {}
                    }
                }
                // Preserve canonical order: emit buffered user content before
                // this standalone tool-result item.
                flush_user_content(&mut input, &mut content);
                let call_id = if preserve_tool_call_ids {
                    result.tool_call_id.0.clone()
                } else {
                    crate::protocol::normalize_tool_call_id(&result.tool_call_id.0)
                };
                input.push(ResponsesInputItem::FunctionCallOutput {
                    call_id,
                    output: outputs,
                });
            }
        }
    }
    flush_user_content(&mut input, &mut content);
    input
}

fn map_assistant_input(
    assistant: &crate::types::AssistantMessage,
    model: &crate::catalog::Model,
    pending_tool_calls: &mut std::collections::BTreeSet<String>,
) -> Vec<ResponsesInputItem> {
    let mut input = Vec::new();
    // Preserve canonical part order: buffered assistant text is flushed as a
    // `message` item immediately before each function/reasoning item.
    let mut text_parts = Vec::new();
    for part in &assistant.content {
        match part {
            AssistantPart::Text(text) => text_parts.push(text.clone()),
            AssistantPart::ToolCall(tool_call) => {
                flush_assistant_text(&mut input, &mut text_parts);
                pending_tool_calls.insert(tool_call.id.0.clone());
                input.push(ResponsesInputItem::FunctionCall {
                    call_id: crate::protocol::normalize_tool_call_id(&tool_call.id.0),
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments_json.clone(),
                });
            }
            AssistantPart::Reasoning(reasoning) => {
                if let Some(state) = &reasoning.state {
                    if state.protocol == Protocol::OpenAiResponses && state.model == model.spec.id {
                        if let ReasoningStateKind::OpenAiReasoning {
                            item_id,
                            encrypted_content,
                        } = &state.kind
                        {
                            flush_assistant_text(&mut input, &mut text_parts);
                            input.push(ResponsesInputItem::Reasoning {
                                id: item_id.clone(),
                                summary: reasoning
                                    .text
                                    .as_ref()
                                    .map(|text| {
                                        vec![ResponsesReasoningSummary {
                                            r#type: "summary_text".to_owned(),
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
            AssistantPart::ProviderMetadata(_) => {}
        }
    }
    flush_assistant_text(&mut input, &mut text_parts);
    input
}

pub(crate) fn encode_canonical_input(
    model: &crate::catalog::Model,
    system: Option<&str>,
    messages: &[Message],
    compatibility: crate::CompatibilityMode,
) -> crate::responses::ResponsesInput {
    let mut input = map_system_input(model, system);
    let mut pending_tool_calls = std::collections::BTreeSet::new();
    let mut synthetic_tool_results = std::collections::HashSet::new();
    for message in messages {
        match message {
            Message::User(user) => input.extend(map_user_input(
                model,
                user,
                false,
                &mut pending_tool_calls,
                &synthetic_tool_results,
            )),
            Message::Assistant(assistant) => {
                if compatibility == crate::CompatibilityMode::Lossy {
                    push_synthetic_tool_results(
                        &mut input,
                        &mut pending_tool_calls,
                        &mut synthetic_tool_results,
                    );
                }
                input.extend(map_assistant_input(
                    assistant,
                    model,
                    &mut pending_tool_calls,
                ));
            }
        }
    }
    if compatibility == crate::CompatibilityMode::Lossy {
        push_synthetic_tool_results(
            &mut input,
            &mut pending_tool_calls,
            &mut synthetic_tool_results,
        );
    }
    crate::responses::ResponsesInput::new(input.into_iter().map(opaque_input_item).collect())
}

pub(crate) fn encode_replay_input(
    model: &crate::catalog::Model,
    system: Option<&str>,
    replay: &[crate::responses::ResponsesReplayItem],
) -> crate::responses::ResponsesInput {
    let compacted_base = matches!(
        replay.first(),
        Some(crate::responses::ResponsesReplayItem::Compacted(_))
    );
    let mut input: Vec<crate::responses::ResponsesItem> = if compacted_base {
        Vec::new()
    } else {
        map_system_input(model, system)
            .into_iter()
            .map(opaque_input_item)
            .collect()
    };
    let mut pending_tool_calls = std::collections::BTreeSet::new();
    let synthetic_tool_results = std::collections::HashSet::new();
    for item in replay {
        match item {
            crate::responses::ResponsesReplayItem::User(user) => {
                input.extend(
                    map_user_input(
                        model,
                        user,
                        true,
                        &mut pending_tool_calls,
                        &synthetic_tool_results,
                    )
                    .into_iter()
                    .map(opaque_input_item),
                );
            }
            crate::responses::ResponsesReplayItem::LocalAssistant(assistant) => {
                input.extend(
                    map_assistant_input(assistant, model, &mut pending_tool_calls)
                        .into_iter()
                        .map(opaque_input_item),
                );
            }
            crate::responses::ResponsesReplayItem::Output(output)
            | crate::responses::ResponsesReplayItem::Compacted(output) => {
                input.extend(output.items().iter().cloned());
            }
        }
    }
    crate::responses::ResponsesInput::new(input)
}

/// Builds the OpenAI Responses HTTP request parts.
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
        Protocol::OpenAiResponses,
        &model.spec.id,
        req.compatibility,
    )?;
    if req
        .responses
        .as_ref()
        .is_some_and(|options| options.input.is_some() && options.previous_response_id.is_some())
    {
        return Err(ConfigError::Parse(
            "Responses raw input and previous_response_id cannot be used together".to_owned(),
        )
        .into());
    }

    // 2–3. Encode the request prompt and canonical history through the same
    // mapper used by durable opaque replay.
    let canonical_input = crate::responses::encode_canonical_responses_input(
        model,
        req.system.as_deref(),
        &req.messages,
        req.compatibility,
    );

    // 4. Map tools & tool_choice
    let responses_lite = model.spec.capabilities.responses_lite;
    let tools_opt = (!responses_lite)
        .then(|| map_responses_tools(model, &req.tools))
        .flatten();

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
    let reasoning_opt = map_responses_reasoning(model, &req.reasoning, req.reasoning_mode);

    // 6. Text / Output Format Config
    //
    // Design §7: a Lossy structured-output downgrade must actually drop the
    // capability from the wire request, not just emit a diagnostic. Strict mode
    // has already returned `Err` in `validate_request` above, so an unsupported
    // format only reaches here under Lossy — in which case we serialize plain
    // text (`text` omitted) rather than send a `text.format` the model lacks.
    let text_opt = map_responses_text(model, &req.output_format);

    // 7. Request Encrypted Reasoning
    let include = if model.spec.capabilities.reasoning.is_some() {
        vec!["reasoning.encrypted_content".to_string()]
    } else {
        vec![]
    };

    // Only forward an explicit caller cap. The Responses API treats this as
    // optional, and the ChatGPT Codex backend rejects it outright
    // (`{"detail":"Unsupported parameter: max_output_tokens"}`), so we never
    // synthesize a default from the local capacity limit. Subscription
    // endpoints that reject this parameter select omission through runtime
    // metadata rather than a codec-side provider identity check.
    let max_output_tokens = (!model
        .endpoint
        .runtime
        .responses_profile
        .omits_max_output_tokens())
    .then_some(req.max_output_tokens)
    .flatten();

    let responses_options = req.responses.as_ref();
    let raw_input = responses_options.and_then(|options| options.input.as_ref());
    let refresh_instructions = raw_input
        .is_some_and(crate::responses::ResponsesInput::contains_compaction)
        .then(|| req.system.clone())
        .flatten();
    let mut input = raw_input.cloned().unwrap_or(canonical_input);
    let instructions = if responses_lite {
        input.strip_image_details_for_responses_lite();
        let mut items = responses_lite_prefix(model, refresh_instructions.as_deref(), &req.tools);
        items.extend(input.into_items());
        input = crate::responses::ResponsesInput::new(items);
        None
    } else {
        refresh_instructions
    };
    let wire_input = serde_json::to_value(input).expect("Responses input serializes");
    let responses_req = ResponsesRequest {
        model: model.spec.api_name.clone(),
        input: wire_input,
        instructions,
        previous_response_id: responses_options
            .and_then(|options| options.previous_response_id.clone()),
        context_management: responses_options
            .and_then(|options| options.context_management.clone()),
        tools: tools_opt,
        tool_choice: tool_choice_opt,
        // The internal Responses Lite route requires an explicit false even
        // when the model otherwise advertises parallel tool-call support.
        parallel_tool_calls: if responses_lite {
            Some(false)
        } else {
            (!req.tools.is_empty() && model.spec.capabilities.tools)
                .then_some(model.spec.capabilities.parallel_tool_calls)
        },
        max_output_tokens,
        // Verified Astra routes reject sampling controls; all other Responses
        // models remain unchanged. `top_p` and `logprobs` have no Responses
        // DTO fields and remain absent.
        temperature: if matches!(
            model.spec.id.0.as_str(),
            "gpt-6-astra" | "codex/gpt-6-astra"
        ) {
            None
        } else {
            req.temperature
        },
        reasoning: reasoning_opt,
        text: text_opt,
        prompt_cache_key: prompt_cache_key(&req),
        prompt_cache_retention: (req.cache_retention == crate::types::CacheRetention::Long
            && model.spec.cache.supports_long_retention)
            .then_some("24h"),
        include,
        store: responses_options.is_some_and(|options| options.store),
        stream: true,
    };

    let body_bytes = serde_json::to_vec(&responses_req)
        .map_err(|e| AiError::Decode(DecodeError::Json(e.to_string())))?;

    let url = crate::protocol::endpoint_url(&model.endpoint.base_url, "responses")?;

    let headers = responses_affinity_headers(model, cache_session_id(&req))?;

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
    FunctionCallArgumentsDone {
        output_index: usize,
        /// The Responses API includes the complete JSON argument string on
        /// the terminal `*.done` event. Some Codex gateways omit the
        /// intermediate `*.delta` events (or deliver only this event), so it
        /// must be retained instead of ending an empty tool call.
        #[serde(default)]
        arguments: Option<String>,
    },
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
        #[serde(default)]
        message: Option<String>,
        /// Codex's Responses gateway nests the documented error fields under
        /// `error`, while the public OpenAI API emits them at the top level.
        #[serde(default)]
        error: Option<ResponsesErrorDto>,
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
    /// Some codex/Responses endpoints send the full arguments inline in the
    /// `output_item.added` event rather than (or in addition to) separate
    /// `function_call_arguments.delta` events. Capture them here so they are
    /// not silently dropped by serde (unknown-field ignore).
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesResponseItemDone {
    id: String,
    r#type: String,
    #[serde(default)]
    encrypted_content: Option<String>,
    /// A few Responses-compatible gateways put the final function-call
    /// arguments only on `response.output_item.done`. Preserve this fallback
    /// shape as well as the documented `function_call_arguments.done` form.
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesResponseCompletedBlock {
    /// Full terminal output is the only authoritative raw replay source. Added
    /// events are intentionally not used because some servers send skeletons.
    #[serde(default)]
    output: Option<Vec<crate::responses::ResponsesItem>>,
    // `usage` is nullable in the Responses object (apidocs
    // openai-responses/01-responses.md: `usage: null` on non-terminal snapshots,
    // populated on completion). Model it as optional so a documented terminal
    // event without usage still decodes to a default-usage `Finished`.
    #[serde(default)]
    usage: Option<ResponsesUsageDto>,
}

#[derive(Deserialize)]
struct ResponsesResponseIncompleteBlock {
    /// Incomplete terminal responses carry the authoritative output produced
    /// before the limit/refusal stopped generation. Preserve it for exact
    /// Responses replay just as we do for completed responses.
    #[serde(default)]
    output: Option<Vec<crate::responses::ResponsesItem>>,
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
    /// OpenAI-compatible gateways emit JSON `null` when no stable error code is
    /// available. Preserve the provider message instead of turning that valid
    /// error envelope into a decoder failure.
    #[serde(default)]
    code: Option<String>,
    /// The human-readable provider error remains required. Missing or nullable
    /// messages are malformed and must not be silently replaced.
    message: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
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

/// Close any tool-call parts that a provider left open before its terminal
/// response event. Some Responses-compatible gateways send complete arguments
/// in `output_item.added` and omit `function_call_arguments.done`; closing here
/// keeps the canonical stream balanced without prematurely rejecting a later
/// argument delta.
fn close_open_tool_calls(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    let open: Vec<usize> = builder
        .tool_call_builders
        .keys()
        .copied()
        .filter(|index| !builder.ended_indices.contains(index))
        .collect();
    for index in open {
        emit_event(
            events,
            builder,
            StreamEvent::ToolCallEnd {
                index,
                argument_error: None,
            },
        )?;
    }
    Ok(())
}

/// Decodes a streaming SSE event from OpenAI Responses, emitting StreamEvents.
pub(crate) fn decode_stream_event(
    _model: &crate::catalog::Model,
    sse_event: &SseEvent,
    builder: &mut ResponseBuilder,
) -> Result<Vec<StreamEvent>, AiError> {
    builder.observe_provider_stream_event()?;
    let raw_data = sse_event.data.trim();
    if raw_data.is_empty() {
        return Ok(vec![]);
    }

    let event: ResponsesSseEvent = serde_json::from_str(raw_data).map_err(|error| {
        let value = serde_json::from_str::<serde_json::Value>(raw_data).ok();
        let event_type = value
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let keys = value
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        let error_keys = value
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(serde_json::Value::as_object)
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        AiError::Decode(DecodeError::Json(format!(
            "invalid OpenAI Responses `{event_type}` event ({keys}; error={error_keys}): {error}"
        )))
    })?;

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
                    // Some codex/Responses endpoints send the full arguments inline
                    // in the `output_item.added` event rather than via separate
                    // `function_call_arguments.delta` events. Feed them as an
                    // initial delta so the tool call builder is populated even
                    // when no delta events follow.
                    if let Some(ref inline_args) = item.arguments {
                        if !inline_args.trim().is_empty() {
                            emit_event(
                                &mut events,
                                builder,
                                StreamEvent::ToolCallArgsDelta {
                                    index: canonical_idx,
                                    delta: inline_args.clone(),
                                },
                            )?;
                        }
                    }
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
            // Tolerate a duplicated `output_text.done` (§8: one *End per part).
            if builder.text_buffers.contains_key(&canonical_idx)
                && !builder.ended_indices.contains(&canonical_idx)
            {
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
        ResponsesSseEvent::FunctionCallArgumentsDone {
            output_index,
            arguments,
        } => {
            let key = format!("item_{}", output_index);
            let canonical_idx = get_canonical_index(builder, &key);
            // Providers are allowed to send the complete argument payload
            // only on the terminal event. If no deltas populated the builder,
            // feed that payload before closing the call. If deltas already
            // arrived, ignore the duplicate complete value to avoid appending
            // arguments twice.
            if let Some(arguments) = arguments {
                if !arguments.trim().is_empty()
                    && builder
                        .tool_call_builders
                        .get(&canonical_idx)
                        .is_some_and(|call| call.arguments_json.trim().is_empty())
                {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ToolCallArgsDelta {
                            index: canonical_idx,
                            delta: arguments,
                        },
                    )?;
                }
            }
            // Tolerate a duplicated `function_call_arguments.done` (§8).
            if builder.tool_call_builders.contains_key(&canonical_idx)
                && !builder.ended_indices.contains(&canonical_idx)
            {
                emit_event(
                    &mut events,
                    builder,
                    StreamEvent::ToolCallEnd {
                        index: canonical_idx,
                        argument_error: None,
                    },
                )?;
            }
        }
        ResponsesSseEvent::OutputItemDone { output_index, item } => {
            if item.r#type == "reasoning" {
                let key = format!("reasoning_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                // A duplicated `output_item.done` must not re-emit End (§8).
                let already_ended = builder.ended_indices.contains(&canonical_idx);
                let had_visible_text = builder.reasoning_text_buffers.contains_key(&canonical_idx);
                if had_visible_text && !already_ended {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ReasoningEnd {
                            index: canonical_idx,
                        },
                    )?;
                } else if item.encrypted_content.is_some() && !already_ended {
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
            } else if item.r#type == "function_call" {
                // Some Codex-compatible streams omit both argument deltas and
                // `function_call_arguments.done`, putting the complete payload
                // on output_item.done. Feed it before terminal closure.
                let key = format!("item_{}", output_index);
                let canonical_idx = get_canonical_index(builder, &key);
                if let Some(arguments) = item.arguments {
                    if !arguments.trim().is_empty()
                        && builder
                            .tool_call_builders
                            .get(&canonical_idx)
                            .is_some_and(|call| call.arguments_json.trim().is_empty())
                    {
                        emit_event(
                            &mut events,
                            builder,
                            StreamEvent::ToolCallArgsDelta {
                                index: canonical_idx,
                                delta: arguments,
                            },
                        )?;
                    }
                }
                if builder.tool_call_builders.contains_key(&canonical_idx)
                    && !builder.ended_indices.contains(&canonical_idx)
                {
                    emit_event(
                        &mut events,
                        builder,
                        StreamEvent::ToolCallEnd {
                            index: canonical_idx,
                            argument_error: None,
                        },
                    )?;
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
            if let Some(output) = response.output.filter(|output| !output.is_empty()) {
                builder.responses_output = Some(crate::responses::ResponsesOutput::new(output));
            }
            close_open_tool_calls(&mut events, builder)?;
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
            if let Some(output) = response.output.filter(|output| !output.is_empty()) {
                builder.responses_output = Some(crate::responses::ResponsesOutput::new(output));
            }
            close_open_tool_calls(&mut events, builder)?;

            if let Some(usage) = &response.usage {
                let u = map_usage(usage)?;
                emit_event(&mut events, builder, StreamEvent::Usage(u))?;
            }

            let resp = builder.finish_mut()?;
            emit_event(&mut events, builder, StreamEvent::Finished(resp))?;
        }
        ResponsesSseEvent::ResponseFailed { response } => {
            return Err(AiError::Provider(ProviderError {
                code: response.error.code,
                kind: response.error.kind,
                message: response.error.message,
                request_id: None,
            }));
        }
        ResponsesSseEvent::StreamError {
            code,
            message,
            error,
        } => {
            let nested_code = error.as_ref().and_then(|error| error.code.clone());
            let nested_kind = error.as_ref().and_then(|error| error.kind.clone());
            let nested_message = error.map(|error| error.message);
            return Err(AiError::Provider(ProviderError {
                code: code.or(nested_code),
                kind: nested_kind,
                message: message
                    .or(nested_message)
                    .unwrap_or_else(|| "provider stream error".to_owned()),
                request_id: None,
            }));
        }
        ResponsesSseEvent::IgnoredEvent => {}
    }

    Ok(events)
}

// --- Helpers ---

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
    // Some OpenAI-compatible gateways emit detail counters that exceed the
    // nominal aggregate. Preserve disjoint buckets and the completed response
    // by flooring only the residual full-rate input bucket.
    let input = usage
        .input_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    crate::responses::normalize_responses_usage(
        input,
        cache_read,
        cache_write,
        usage.output_tokens,
        reasoning,
    )
    .ok_or(AiError::Decode(DecodeError::UsageUnderflow))
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Model;
    use crate::types::{
        Capabilities, Endpoint, EndpointId, ImageMedia, ImageSource, JsonSchemaFormat, Media,
        Message, ModalitySet, ModelId, ModelLimits, ModelSpec, OutputFormat, OutputModalities,
        ProviderMediaRef, ReasoningConfig, Request, ResponsesRuntimeProfile, ToolChoice,
        UserMessage, UserPart,
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
            reasoning_mode: crate::types::ReasoningMode::Standard,
            responses: None,
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
            display_name: None,
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
                        min_effort: crate::types::ReasoningEffort::Minimal,
                        max_effort: crate::types::ReasoningEffort::High,
                    })
                } else {
                    None
                },
                responses_lite: false,
                agent_delegation: None,
                structured_output: true,
                deferred_tool_loading: false,
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
            transport: crate::types::EndpointTransport::Http,
            runtime: crate::types::RequestRuntime::default(),
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
            reasoning_mode: crate::types::ReasoningMode::Standard,
            responses: None,
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
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("parallel_tool_calls").is_none());
        assert!(body.get("text").is_none());
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
    fn gpt_6_astra_preserves_default_reasoning_and_emits_top_wire_efforts() {
        let model = crate::catalog::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-6-astra".to_owned()))
            .unwrap();
        let mut req = user_req(
            vec![UserPart::Text("Hello".to_owned())],
            CompatibilityMode::Strict,
        );

        req.temperature = Some(0.7);
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["model"], "gpt-6-astra");
        assert!(body.get("reasoning").is_none());
        for unsupported in ["temperature", "top_p", "logprobs"] {
            assert!(
                body.get(unsupported).is_none(),
                "Astra request unexpectedly included {unsupported}"
            );
        }

        for (effort, expected) in [
            (crate::types::ReasoningEffort::Minimal, "low"),
            (crate::types::ReasoningEffort::Low, "low"),
            (crate::types::ReasoningEffort::Xhigh, "xhigh"),
            (crate::types::ReasoningEffort::Max, "max"),
        ] {
            req.reasoning = ReasoningConfig::Effort(effort);
            let body: serde_json::Value =
                serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
            assert_eq!(body["reasoning"]["effort"], expected);
        }
    }

    #[test]
    fn responses_lite_uses_header_and_input_items_for_tools_and_instructions() {
        let mut model = make_test_model(true);
        let mut spec = (*model.spec).clone();
        spec.capabilities.responses_lite = true;
        spec.capabilities.agent_delegation = Some(crate::types::AgentDelegation::V2);
        spec.capabilities.reasoning.as_mut().unwrap().max_effort =
            crate::types::ReasoningEffort::Ultra;
        model.spec = Arc::new(spec);
        assert!(model.spec.capabilities.parallel_tool_calls);

        let mut req = user_req(
            vec![UserPart::Text("Hello".to_owned())],
            CompatibilityMode::Strict,
        );
        req.system = Some("System instructions".to_owned());
        req.reasoning = ReasoningConfig::Effort(crate::types::ReasoningEffort::Ultra);
        req.tools.push(ToolDef {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        });

        let parts = build_request(&model, &req).unwrap();
        assert_eq!(
            parts
                .headers
                .get("x-openai-internal-codex-responses-lite")
                .unwrap(),
            "true"
        );
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert!(body.get("instructions").is_none());
        assert!(body.get("tools").is_none());
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["tools"][0]["type"], "namespace");
        assert_eq!(body["input"][0]["tools"][0]["name"], "functions");
        assert_eq!(body["input"][0]["tools"][0]["tools"][0]["name"], "read");
        assert_eq!(body["input"][1]["role"], "developer");
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            "System instructions"
        );
        assert_eq!(body["input"][2]["role"], "user");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert!(body["reasoning"].get("mode").is_none());
    }

    #[test]
    fn responses_lite_honors_disabled_parallel_tool_capability() {
        let mut model = make_test_model(true);
        let mut spec = (*model.spec).clone();
        spec.capabilities.responses_lite = true;
        spec.capabilities.parallel_tool_calls = false;
        model.spec = Arc::new(spec);

        let mut req = user_req(
            vec![UserPart::Text("Hello".to_owned())],
            CompatibilityMode::Strict,
        );
        req.tools.push(ToolDef {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        });

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn responses_lite_disables_parallel_calls_without_tools_and_strips_image_detail() {
        let mut model = make_test_model(true);
        let mut spec = (*model.spec).clone();
        spec.capabilities.responses_lite = true;
        model.spec = Arc::new(spec);

        let req = user_req(
            vec![UserPart::Media(Media::Image(ImageMedia {
                source: ImageSource::Url(url::Url::parse("https://example.com/image.png").unwrap()),
                media_type: None,
                detail: Some(crate::types::ImageDetail::High),
            }))],
            CompatibilityMode::Strict,
        );

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][0]["tools"], serde_json::json!([]));
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(
            body["input"][1]["content"][0]["image_url"],
            "https://example.com/image.png"
        );
        assert!(body["input"][1]["content"][0].get("detail").is_none());
    }

    #[test]
    fn public_replay_encoder_keeps_opaque_items_and_verbatim_call_ids() {
        let model = make_test_model(true);
        let raw = serde_json::json!({
            "type": "function_call",
            "id": "fc_provider_item",
            "call_id": "call_raw|item_exact",
            "name": "exec",
            "arguments": "{\"command\":\"pwd\"}",
            "phase": "commentary",
            "encrypted_content": "opaque",
            "programmatic_tool": {"runtime": "python"},
            "future_field": {"nested": true}
        });
        let output =
            crate::responses::ResponsesOutput::new(vec![crate::responses::ResponsesItem::new(
                raw.clone(),
            )
            .unwrap()]);
        let replay = vec![
            crate::responses::ResponsesReplayItem::User(UserMessage {
                content: vec![UserPart::Text("run it".to_owned())],
            }),
            crate::responses::ResponsesReplayItem::Output(output),
            crate::responses::ResponsesReplayItem::User(UserMessage {
                content: vec![UserPart::ToolResult(crate::types::ToolResult {
                    tool_call_id: crate::types::ToolCallId("call_raw|item_exact".to_owned()),
                    content: vec![crate::types::ToolResultPart::Text("ok".to_owned())],
                    is_error: false,
                    added_tool_names: None,
                })],
            }),
        ];

        let input = crate::responses::encode_responses_replay(&model, Some("be precise"), &replay);
        let value = serde_json::to_value(input).unwrap();
        assert_eq!(value[0]["role"], "developer");
        assert_eq!(value[2], raw);
        assert_eq!(value[3]["type"], "function_call_output");
        assert_eq!(value[3]["call_id"], "call_raw|item_exact");
    }

    #[test]
    fn compacted_replay_base_replaces_prior_system_input_verbatim() {
        let model = make_test_model(true);
        let compacted = serde_json::json!({
            "type": "compaction",
            "id": "cmp_exact",
            "encrypted_content": "opaque-instructions-and-history"
        });
        let replay = vec![
            crate::responses::ResponsesReplayItem::Compacted(
                crate::responses::ResponsesOutput::new(vec![
                    crate::responses::ResponsesItem::new(serde_json::json!({
                        "type": "message",
                        "id": "leading-preserved-output"
                    }))
                    .unwrap(),
                    crate::responses::ResponsesItem::new(compacted.clone()).unwrap(),
                ]),
            ),
            crate::responses::ResponsesReplayItem::User(UserMessage {
                content: vec![UserPart::Text("after checkpoint".to_owned())],
            }),
        ];

        let input = crate::responses::encode_responses_replay(
            &model,
            Some("must not be reinserted"),
            &replay,
        );
        let value = serde_json::to_value(input).unwrap();
        assert_eq!(value[0]["id"], "leading-preserved-output");
        assert_eq!(value[1], compacted);
        assert_eq!(value[2]["role"], "user");
        assert!(!value.to_string().contains("must not be reinserted"));

        let mut req = user_req(
            vec![UserPart::Text("canonical fallback is unused".to_owned())],
            CompatibilityMode::Strict,
        );
        req.system = Some("current instructions".to_owned());
        req.responses = Some(crate::responses::ResponsesOptions::full_replay(
            crate::responses::ResponsesInput::new(
                value
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .map(crate::responses::ResponsesItem::new)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            ),
        ));
        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(body["input"][0]["id"], "leading-preserved-output");
        assert_eq!(body["input"][1], compacted);
        assert_eq!(body["instructions"], "current instructions");
        assert!(!body["input"].to_string().contains("current instructions"));
    }

    #[test]
    fn full_replay_request_does_not_mix_previous_response_id_or_storage() {
        let model = make_test_model(true);
        let mut req = user_req(
            vec![UserPart::Text("next".to_owned())],
            CompatibilityMode::Strict,
        );
        req.responses = Some(crate::responses::ResponsesOptions::full_replay(
            crate::responses::ResponsesInput::new(vec![crate::responses::ResponsesItem::new(
                serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "next"}]
                }),
            )
            .unwrap()]),
        ));

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["store"], false);

        req.responses.as_mut().unwrap().previous_response_id = Some("resp_server".to_owned());
        let error = match build_request(&model, &req) {
            Err(error) => error,
            Ok(_) => panic!("full input plus previous_response_id must be rejected"),
        };
        assert!(error.to_string().contains("cannot be used together"));
    }

    #[test]
    fn legacy_pro_reasoning_mode_is_never_serialized() {
        let model = make_test_model(true);
        let mut req = user_req(
            vec![UserPart::Text("review this migration".to_string())],
            CompatibilityMode::Lossy,
        );
        req.reasoning = ReasoningConfig::Effort(crate::types::ReasoningEffort::Medium);
        req.reasoning_mode = crate::types::ReasoningMode::Pro;

        let parts = build_request(&model, &req).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();
        assert!(body["reasoning"].get("mode").is_none());
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert!(parts
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "ignored_reasoning_mode"));

        req.compatibility = CompatibilityMode::Strict;
        assert!(matches!(
            build_request(&model, &req),
            Err(AiError::Unsupported(
                crate::error::UnsupportedError::ReasoningMode
            ))
        ));
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
    fn reasoning_effort_emits_xhigh_and_max_and_maps_ultra_to_max() {
        for (effort, expected) in [
            (crate::types::ReasoningEffort::Xhigh, "xhigh"),
            (crate::types::ReasoningEffort::Max, "max"),
            (crate::types::ReasoningEffort::Ultra, "max"),
        ] {
            let mut model = make_test_model(true);
            let mut spec = (*model.spec).clone();
            spec.capabilities.reasoning.as_mut().unwrap().max_effort =
                crate::types::ReasoningEffort::Ultra;
            model.spec = std::sync::Arc::new(spec);
            let mut req = user_req(
                vec![UserPart::Text("hi".to_string())],
                CompatibilityMode::Strict,
            );
            req.reasoning = ReasoningConfig::Effort(effort);
            let body: serde_json::Value =
                serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
            assert_eq!(body["reasoning"]["effort"], expected);
        }
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
    fn responses_usage_tolerates_gateway_cache_totals_with_a_broader_denominator() {
        let usage: ResponsesUsageDto = serde_json::from_value(serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 3,
            "total_tokens": 103,
            "input_tokens_details": {
                "cached_tokens": 120,
                "cache_write_tokens": 20
            },
            "output_tokens_details": { "reasoning_tokens": 5 }
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
    fn responses_explicit_affinity_formats_emit_provider_specific_headers() {
        let mut model = make_test_model(false);
        let mut request = user_req(
            vec![UserPart::Text("hello".to_string())],
            CompatibilityMode::Strict,
        );
        request.session_id = Some("stable-session".to_string());

        Arc::make_mut(&mut model.spec).cache.session_affinity_format =
            Some(crate::types::SessionAffinityFormat::OpenAi);
        let parts = build_request(&model, &request).unwrap();
        assert_eq!(parts.headers["session_id"], "stable-session");
        assert_eq!(parts.headers["x-client-request-id"], "stable-session");
        assert_eq!(parts.headers["x-session-affinity"], "stable-session");

        let cache = &mut Arc::make_mut(&mut model.spec).cache;
        cache.send_session_id_header = false;
        cache.session_affinity_format = Some(crate::types::SessionAffinityFormat::OpenAiNoSession);
        let parts = build_request(&model, &request).unwrap();
        assert!(parts.headers.get("session_id").is_none());
        assert_eq!(parts.headers["x-client-request-id"], "stable-session");
        assert_eq!(parts.headers["x-session-affinity"], "stable-session");

        Arc::make_mut(&mut model.spec).cache.session_affinity_format =
            Some(crate::types::SessionAffinityFormat::OpenRouter);
        let parts = build_request(&model, &request).unwrap();
        assert_eq!(parts.headers["x-session-id"], "stable-session");
        assert!(parts.headers.get("x-client-request-id").is_none());
    }

    #[test]
    fn responses_codex_affinity_uses_the_request_session_id() {
        let mut model = make_test_model(false);
        let cache = &mut Arc::make_mut(&mut model.spec).cache;
        cache.send_session_id_header = false;
        cache.session_affinity_format = Some(crate::types::SessionAffinityFormat::Codex);
        let mut request = user_req(
            vec![UserPart::Text("hello".to_string())],
            CompatibilityMode::Strict,
        );
        request.session_id = Some("durable-session".to_string());

        let parts = build_request(&model, &request).unwrap();
        assert_eq!(parts.headers["session-id"], "durable-session");
        assert_eq!(parts.headers["x-client-request-id"], "durable-session");
        assert!(parts.headers.get("session_id").is_none());
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
            reasoning_mode: crate::types::ReasoningMode::Standard,
            responses: None,
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
        assert_eq!(body["parallel_tool_calls"], true);
        assert!(body.get("stop").is_none());
        assert!(body.get("max_completion_tokens").is_none());

        req.stop.push("END".to_string());
        assert!(matches!(
            build_request(&model, &req),
            Err(AiError::Unsupported(crate::UnsupportedError::StopSequences))
        ));
    }

    #[test]
    fn declared_responses_runtime_defaults_to_low_verbosity_and_gates_parallel_tools() {
        let mut model = make_test_model(false);
        Arc::make_mut(&mut model.endpoint).runtime.responses_profile =
            ResponsesRuntimeProfile::Codex;
        Arc::make_mut(&mut model.spec)
            .capabilities
            .parallel_tool_calls = false;

        let mut req = user_req(
            vec![UserPart::Text("hello".to_string())],
            CompatibilityMode::Strict,
        );
        req.tools = vec![crate::types::ToolDef {
            name: "lookup".to_string(),
            description: "lookup data".to_string(),
            parameters: serde_json::json!({"type":"object"}),
        }];

        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["text"]["verbosity"], "low");
        assert!(body["text"].get("format").is_none());
        assert_eq!(body["parallel_tool_calls"], false);

        Arc::make_mut(&mut model.spec)
            .capabilities
            .parallel_tool_calls = true;
        let body: serde_json::Value =
            serde_json::from_slice(&build_request(&model, &req).unwrap().body).unwrap();
        assert_eq!(body["parallel_tool_calls"], true);
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
            reasoning_mode: crate::types::ReasoningMode::Standard,
            responses: None,
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
    use crate::types::{
        AssistantPart, Protocol, ReasoningStateKind, StopReason, ToolCallArgumentError, ToolDef,
    };

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
    async fn schema_mismatch_is_marked_before_tool_call_end() {
        let model = harness::model(Protocol::OpenAiResponses, None);
        let tools = [ToolDef {
            name: "grep".to_owned(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"pattern": {"type": "integer"}},
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        }];
        let events =
            harness::drive_with_tools(&model, decode_stream_event, fx!("tool_call.sse"), 0, &tools)
                .await
                .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallEnd {
                argument_error: Some(ToolCallArgumentError::SchemaMismatch),
                ..
            }
        )));
        let call = harness::finished(&events)
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("schema-rejected call is retained");
        assert_eq!(call.id.0, "call_1");
        assert_eq!(call.arguments_json, r#"{"pattern":"foo"}"#);
        assert_eq!(
            call.argument_error,
            Some(ToolCallArgumentError::SchemaMismatch)
        );
    }

    #[tokio::test]
    async fn terminal_output_is_authoritative_over_added_item_skeleton() {
        let stream = br#"data: {"type":"response.created","response":{"id":"resp_raw"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_skeleton","type":"function_call","call_id":"call_skeleton","name":"exec","arguments":"{}"}}

data: {"type":"response.completed","response":{"output":[{"type":"function_call","id":"fc_terminal","call_id":"call_terminal","name":"exec","arguments":"{\"command\":\"pwd\"}","phase":"commentary","unknown":{"kept":true}}]}}

"#;
        let events = run(stream, 0).await.unwrap();
        let response = harness::finished(&events);
        let output = response
            .responses_output
            .as_ref()
            .expect("terminal response output must be retained");
        assert_eq!(output.items().len(), 1);
        assert_eq!(output.items()[0].as_json()["id"], "fc_terminal");
        assert_eq!(output.items()[0].as_json()["call_id"], "call_terminal");
        assert_eq!(output.items()[0].as_json()["phase"], "commentary");
        assert_eq!(output.items()[0].as_json()["unknown"]["kept"], true);
        assert_ne!(output.items()[0].as_json()["id"], "fc_skeleton");
    }

    #[tokio::test]
    async fn inline_tool_arguments_close_at_response_completion() {
        let events = run(fx!("inline_tool_arguments.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        let tc = resp
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(tool) => Some(tool),
                _ => None,
            })
            .expect("inline function call must be preserved");
        assert_eq!(
            tc.arguments_value().unwrap(),
            serde_json::json!({"pattern": "foo"})
        );
    }

    #[tokio::test]
    async fn done_event_arguments_are_recovered_when_no_deltas_arrive() {
        let events = run(fx!("done_tool_arguments.sse"), 0).await.unwrap();
        let resp = harness::finished(&events);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        let tc = resp
            .message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::ToolCall(tool) => Some(tool),
                _ => None,
            })
            .expect("terminal function-call event must preserve the tool call");
        assert_eq!(tc.name, "exec");
        assert_eq!(
            tc.arguments_value().unwrap(),
            serde_json::json!({"command":"pwd"})
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
        let response = harness::finished(&events);
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
        let output = response
            .responses_output
            .as_ref()
            .expect("incomplete terminal output must be retained for exact replay");
        assert_eq!(output.items()[0].as_json()["id"], "msg_terminal_partial");
        assert_eq!(output.items()[0].as_json()["unknown"]["kept"], "verbatim");
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

    #[tokio::test]
    async fn codex_nested_error_event_becomes_provider_error() {
        let err = run(fx!("codex_nested_stream_error.sse"), 1)
            .await
            .unwrap_err();
        match err {
            AiError::Provider(provider) => {
                assert_eq!(provider.code.as_deref(), Some("upstream_failure"));
                assert_eq!(provider.message, "Nested Codex stream failure");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_nested_error_accepts_observed_nullable_code() {
        let err = run(fx!("codex_nested_nullable_error.sse"), 1)
            .await
            .unwrap_err();
        match err {
            AiError::Provider(provider) => {
                assert_eq!(provider.code, None);
                assert_eq!(provider.kind.as_deref(), Some("server_error"));
                assert_eq!(provider.message, "The upstream provider ended the request");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_nested_error_still_requires_a_string_message() {
        let err = run(fx!("codex_nested_error_missing_message.sse"), 1)
            .await
            .unwrap_err();
        assert!(matches!(err, AiError::Decode(_)), "got {err:?}");
        assert!(
            err.to_string()
                .contains("invalid OpenAI Responses `error` event"),
            "got {err}"
        );
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
