//! Google Generative AI and Vertex `generateContent` wire protocol codec.
//!
//! Both public Gemini and Vertex AI expose the same content, function-calling,
//! and SSE response shapes. Endpoint construction and authentication remain in
//! provider registration; this module only maps the bounded canonical request.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{AiError, ConfigError, DecodeError, ProviderError};
use crate::protocol::sse::SseEvent;
use crate::protocol::{emit_event, HttpRequestParts};
use crate::stream::{ResponseBuilder, StreamEvent};
use crate::types::{
    AssistantPart, ImageSource, Media, Message, OutputFormat, Protocol, ProviderPartMetadata,
    ReasoningConfig, ReasoningEffort, Request, StopReason, ToolCallId, ToolChoice, ToolResultPart,
    Usage, UserPart,
};
use crate::validate::{normalize_request_reasoning, validate_request};

/// A thought signature is opaque continuation data, not visible content. Keep
/// it well below the aggregate response cap so a malicious provider cannot use
/// a single metadata field to retain an excessive allocation.
const MAX_THOUGHT_SIGNATURE_BYTES: usize = 64 * 1024;
const FUNCTION_ARGS_KEY_PREFIX: &str = "google_function_args_";

/// Builds a Google Generative AI / Vertex streaming request.
pub(crate) fn build_request(
    model: &crate::catalog::Model,
    req: &Request,
) -> Result<HttpRequestParts, AiError> {
    let req = normalize_request_reasoning(req, &model.spec.capabilities);
    let diagnostics = validate_request(
        &req,
        &model.spec.capabilities,
        &model.spec.limits,
        Protocol::GoogleGenerativeAi,
        &model.spec.id,
        req.compatibility,
    )?;

    let mut root = Map::new();
    if let Some(system) = &req.system {
        root.insert(
            "systemInstruction".to_owned(),
            system_instruction_value(vec![text_part(system.clone(), None, false)]),
        );
    }

    let contents = google_contents(model, &req)?;
    root.insert("contents".to_owned(), Value::Array(contents));

    if let Some(tools) = google_tools(model, &req) {
        root.insert("tools".to_owned(), tools);
        if let Some(tool_config) = google_tool_config(&req.tool_choice) {
            root.insert("toolConfig".to_owned(), tool_config);
        }
    }

    let generation_config = google_generation_config(model, &req);
    if !generation_config.is_empty() {
        root.insert(
            "generationConfig".to_owned(),
            Value::Object(generation_config),
        );
    }

    let body = serde_json::to_vec(&Value::Object(root))
        .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
    // `api_name` is validated as a single safe path segment when the model is
    // registered. It cannot alter the configured endpoint authority or path.
    let url = model
        .endpoint
        .base_url
        .join(&format!(
            "models/{}:streamGenerateContent?alt=sse",
            model.spec.api_name
        ))
        .map_err(|error| AiError::Config(ConfigError::Parse(error.to_string())))?;
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("text/event-stream"),
    );

    Ok(HttpRequestParts {
        url,
        headers,
        body: bytes::Bytes::from(body),
        streaming: true,
        diagnostics,
    })
}

fn google_contents(model: &crate::catalog::Model, req: &Request) -> Result<Vec<Value>, AiError> {
    let mut contents = Vec::new();
    let mut tool_names = HashMap::<String, String>::new();

    for message in &req.messages {
        match message {
            Message::User(user) => {
                let mut parts = Vec::new();
                for part in &user.content {
                    match part {
                        UserPart::Text(text) => parts.push(text_part(text.clone(), None, false)),
                        UserPart::Media(Media::Image(image)) => {
                            if model
                                .spec
                                .capabilities
                                .input_modalities
                                .contains(crate::types::Modality::Image)
                            {
                                if let Some(part) = google_inline_image_part(image) {
                                    parts.push(part);
                                }
                            }
                        }
                        UserPart::Media(Media::Audio(_)) => {}
                        UserPart::ToolResult(result) => {
                            let id =
                                crate::protocol::normalize_tool_call_id(&result.tool_call_id.0);
                            let Some(name) = tool_names.get(&id) else {
                                // Pairing has already been validated. This is a
                                // defensive fallback for a direct codec caller in
                                // Lossy mode, where a malformed historical turn is
                                // deliberately omitted rather than guessed.
                                continue;
                            };
                            parts.push(function_response_part(name, &id, result));
                        }
                    }
                }
                push_content(&mut contents, "user", parts);
            }
            Message::Assistant(assistant) => {
                let same_google_model = assistant.protocol == Protocol::GoogleGenerativeAi
                    && assistant.model == model.spec.id;
                let mut pending_signature = None;
                let mut parts = Vec::new();
                for part in &assistant.content {
                    match part {
                        AssistantPart::ProviderMetadata(
                            ProviderPartMetadata::GoogleThoughtSignature { signature },
                        ) if same_google_model && signature_is_bounded(signature) => {
                            pending_signature = Some(signature.clone());
                        }
                        AssistantPart::ProviderMetadata(_) => {
                            pending_signature = None;
                        }
                        AssistantPart::Text(text) => {
                            let signature = pending_signature.take();
                            parts.push(text_part(text.clone(), signature, false));
                        }
                        AssistantPart::Reasoning(reasoning) => {
                            let signature = pending_signature.take();
                            // Google identifies reasoning by `thought: true`; the
                            // signature, when any, stays an independent marker.
                            if let Some(text) = &reasoning.text {
                                parts.push(text_part(text.clone(), signature, true));
                            }
                        }
                        AssistantPart::ToolCall(call) => {
                            let signature = pending_signature.take();
                            let args = serde_json::from_str::<Value>(&call.arguments_json)
                                .map_err(|error| {
                                    AiError::Decode(DecodeError::Json(error.to_string()))
                                })?;
                            if !args.is_object() {
                                return Err(AiError::Decode(DecodeError::Json(
                                    "Google function-call arguments must be an object".to_owned(),
                                )));
                            }
                            let id = crate::protocol::normalize_tool_call_id(&call.id.0);
                            tool_names.insert(id.clone(), call.name.clone());
                            parts.push(function_call_part(call.name.clone(), id, args, signature));
                        }
                        AssistantPart::Media(_) => {
                            // A signature is position-bound. Never move one from
                            // an unsupported media part onto later text/calls.
                            pending_signature = None;
                        }
                    }
                }
                push_content(&mut contents, "model", parts);
            }
        }
    }

    Ok(contents)
}

fn google_inline_image_part(image: &crate::types::ImageMedia) -> Option<Value> {
    let ImageSource::Inline(data) = &image.source else {
        return None;
    };
    let mime_type = image.media_type.as_ref()?.to_string();
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
    let mut inline_data = Map::new();
    inline_data.insert("mimeType".to_owned(), Value::String(mime_type));
    inline_data.insert("data".to_owned(), Value::String(encoded));
    let mut part = Map::new();
    part.insert("inlineData".to_owned(), Value::Object(inline_data));
    Some(Value::Object(part))
}

fn text_part(text: String, signature: Option<String>, thought: bool) -> Value {
    let mut part = Map::new();
    part.insert("text".to_owned(), Value::String(text));
    if thought {
        part.insert("thought".to_owned(), Value::Bool(true));
    }
    if let Some(signature) = signature {
        part.insert("thoughtSignature".to_owned(), Value::String(signature));
    }
    Value::Object(part)
}

fn function_call_part(name: String, id: String, args: Value, signature: Option<String>) -> Value {
    let mut function_call = Map::new();
    function_call.insert("name".to_owned(), Value::String(name));
    function_call.insert("id".to_owned(), Value::String(id));
    function_call.insert("args".to_owned(), args);
    let mut part = Map::new();
    part.insert("functionCall".to_owned(), Value::Object(function_call));
    if let Some(signature) = signature {
        part.insert("thoughtSignature".to_owned(), Value::String(signature));
    }
    Value::Object(part)
}

fn function_response_part(name: &str, id: &str, result: &crate::types::ToolResult) -> Value {
    let text = result
        .content
        .iter()
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => Some(text.as_str()),
            ToolResultPart::Media(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut response = Map::new();
    response.insert(
        if result.is_error { "error" } else { "output" }.to_owned(),
        Value::String(text),
    );
    let mut function_response = Map::new();
    function_response.insert("name".to_owned(), Value::String(name.to_owned()));
    function_response.insert("id".to_owned(), Value::String(id.to_owned()));
    function_response.insert("response".to_owned(), Value::Object(response));
    let mut part = Map::new();
    part.insert(
        "functionResponse".to_owned(),
        Value::Object(function_response),
    );
    Value::Object(part)
}

fn content_value(role: &str, parts: Vec<Value>) -> Value {
    let mut content = Map::new();
    content.insert("role".to_owned(), Value::String(role.to_owned()));
    content.insert("parts".to_owned(), Value::Array(parts));
    Value::Object(content)
}

fn system_instruction_value(parts: Vec<Value>) -> Value {
    let mut instruction = Map::new();
    instruction.insert("parts".to_owned(), Value::Array(parts));
    Value::Object(instruction)
}

/// Gemini accepts adjacent same-role content, but combining it avoids invalid
/// alternation around a function-response turn without altering part order.
fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(Value::Object(last)) = contents.last_mut() {
        if last.get("role") == Some(&Value::String(role.to_owned())) {
            if let Some(Value::Array(existing)) = last.get_mut("parts") {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(content_value(role, parts));
}

fn google_tools(model: &crate::catalog::Model, req: &Request) -> Option<Value> {
    if req.tools.is_empty()
        || !model.spec.capabilities.tools
        || matches!(req.tool_choice, ToolChoice::None)
    {
        return None;
    }
    let declarations = req
        .tools
        .iter()
        .map(|tool| {
            let mut declaration = Map::new();
            declaration.insert("name".to_owned(), Value::String(tool.name.clone()));
            if !tool.description.is_empty() {
                declaration.insert(
                    "description".to_owned(),
                    Value::String(tool.description.clone()),
                );
            }
            // `parametersJsonSchema` accepts the canonical JSON Schema directly;
            // do not silently rewrite it into Google's narrower Schema dialect.
            declaration.insert("parametersJsonSchema".to_owned(), tool.parameters.clone());
            Value::Object(declaration)
        })
        .collect();
    let mut tool = Map::new();
    tool.insert(
        "functionDeclarations".to_owned(),
        Value::Array(declarations),
    );
    Some(Value::Array(vec![Value::Object(tool)]))
}

fn google_tool_config(choice: &ToolChoice) -> Option<Value> {
    let mut function_calling = Map::new();
    match choice {
        ToolChoice::Auto => {
            function_calling.insert("mode".to_owned(), Value::String("AUTO".to_owned()));
        }
        ToolChoice::Required => {
            function_calling.insert("mode".to_owned(), Value::String("ANY".to_owned()));
        }
        ToolChoice::Named(name) => {
            function_calling.insert("mode".to_owned(), Value::String("ANY".to_owned()));
            function_calling.insert(
                "allowedFunctionNames".to_owned(),
                Value::Array(vec![Value::String(name.clone())]),
            );
        }
        ToolChoice::None => return None,
    }
    let mut config = Map::new();
    config.insert(
        "functionCallingConfig".to_owned(),
        Value::Object(function_calling),
    );
    Some(Value::Object(config))
}

fn google_generation_config(model: &crate::catalog::Model, req: &Request) -> Map<String, Value> {
    let mut config = Map::new();
    if let Some(max_tokens) = req.max_output_tokens {
        config.insert("maxOutputTokens".to_owned(), Value::from(max_tokens));
    }
    if let Some(temperature) = req.temperature {
        config.insert("temperature".to_owned(), Value::from(temperature));
    }
    if !req.stop.is_empty() {
        config.insert(
            "stopSequences".to_owned(),
            Value::Array(req.stop.iter().cloned().map(Value::String).collect()),
        );
    }
    match &req.output_format {
        OutputFormat::JsonObject if model.spec.capabilities.structured_output => {
            config.insert(
                "responseMimeType".to_owned(),
                Value::String("application/json".to_owned()),
            );
        }
        OutputFormat::JsonSchema(schema) if model.spec.capabilities.structured_output => {
            config.insert(
                "responseMimeType".to_owned(),
                Value::String("application/json".to_owned()),
            );
            config.insert("responseJsonSchema".to_owned(), schema.schema.clone());
        }
        _ => {}
    }

    if model.spec.capabilities.reasoning.is_some() {
        if let Some(thinking) = google_thinking_config(model, &req.reasoning, req.max_output_tokens)
        {
            config.insert("thinkingConfig".to_owned(), thinking);
        }
    }
    config
}

fn google_thinking_config(
    model: &crate::catalog::Model,
    reasoning: &ReasoningConfig,
    requested_output_limit: Option<u64>,
) -> Option<Value> {
    let mut config = Map::new();
    let is_budget_model = model.spec.api_name.starts_with("gemini-2.");
    match reasoning {
        ReasoningConfig::Off => {
            if is_budget_model {
                config.insert("thinkingBudget".to_owned(), Value::from(0_u64));
            } else {
                config.insert(
                    "thinkingLevel".to_owned(),
                    Value::String("MINIMAL".to_owned()),
                );
            }
        }
        ReasoningConfig::Effort(effort) => {
            config.insert("includeThoughts".to_owned(), Value::Bool(true));
            if is_budget_model {
                let maximum = requested_output_limit.unwrap_or(model.spec.limits.max_output_tokens);
                config.insert(
                    "thinkingBudget".to_owned(),
                    Value::from(google_thinking_budget(*effort).min(maximum)),
                );
            } else {
                config.insert(
                    "thinkingLevel".to_owned(),
                    Value::String(google_thinking_level(*effort).to_owned()),
                );
            }
        }
        // Catalog validation exposes Google reasoning as portable effort only.
        // Direct codec callers cannot obtain a documented representation for
        // these variants, so omit rather than guess a provider field.
        ReasoningConfig::On | ReasoningConfig::Budget(_) => return None,
    }
    Some(Value::Object(config))
}

fn google_thinking_budget(effort: ReasoningEffort) -> u64 {
    match effort {
        ReasoningEffort::Minimal => 1_024,
        ReasoningEffort::Low => 2_048,
        ReasoningEffort::Medium => 8_192,
        ReasoningEffort::High => 16_384,
        ReasoningEffort::Xhigh => 24_576,
        ReasoningEffort::Max | ReasoningEffort::Ultra => 32_768,
    }
}

fn google_thinking_level(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "MINIMAL",
        ReasoningEffort::Low => "LOW",
        ReasoningEffort::Medium => "MEDIUM",
        ReasoningEffort::High
        | ReasoningEffort::Xhigh
        | ReasoningEffort::Max
        | ReasoningEffort::Ultra => "HIGH",
    }
}

#[derive(Deserialize)]
struct GoogleStreamResponse {
    #[serde(default, rename = "responseId")]
    response_id: Option<String>,
    #[serde(default)]
    candidates: Vec<GoogleCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GoogleUsageMetadata>,
    #[serde(default, rename = "promptFeedback")]
    prompt_feedback: Option<GooglePromptFeedback>,
    #[serde(default)]
    error: Option<GoogleProviderError>,
}

#[derive(Deserialize)]
struct GoogleCandidate {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    content: Option<GoogleContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GoogleContent {
    #[serde(default)]
    parts: Vec<GooglePart>,
}

#[derive(Deserialize)]
struct GooglePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GoogleFunctionCall>,
}

#[derive(Deserialize)]
struct GoogleFunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default = "empty_object")]
    args: Value,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Deserialize)]
struct GoogleUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_tokens: u64,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_content_tokens: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    candidate_tokens: u64,
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts_tokens: u64,
    #[serde(default, rename = "totalTokenCount")]
    total_tokens: u64,
}

#[derive(Deserialize)]
struct GooglePromptFeedback {
    #[serde(default, rename = "blockReason")]
    block_reason: Option<Value>,
}

#[derive(Deserialize)]
struct GoogleProviderError {
    #[serde(default)]
    code: Option<Value>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    message: String,
}

/// Decodes one Google SSE frame into canonical stream events.
pub(crate) fn decode_stream_event(
    _model: &crate::catalog::Model,
    sse_event: &SseEvent,
    builder: &mut ResponseBuilder,
) -> Result<Vec<StreamEvent>, AiError> {
    builder.observe_provider_stream_event()?;
    let raw_data = sse_event.data.trim();
    if raw_data.is_empty() {
        return Ok(Vec::new());
    }
    if raw_data == "[DONE]" {
        let mut events = Vec::new();
        ensure_started(&mut events, builder)?;
        finish_google(&mut events, builder, StopReason::EndTurn)?;
        return Ok(events);
    }

    let data: GoogleStreamResponse = serde_json::from_str(raw_data)
        .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
    if let Some(error) = data.error {
        return Err(AiError::Provider(ProviderError {
            code: error.code.map(|value| match value {
                Value::String(value) => value,
                other => other.to_string(),
            }),
            kind: error.status,
            message: error.message,
            request_id: data.response_id,
        }));
    }

    if let Some(response_id) = data.response_id.filter(|id| !id.is_empty()) {
        if builder.response_id.is_none() {
            builder.response_id = Some(response_id);
        }
    }

    let mut events = Vec::new();
    ensure_started(&mut events, builder)?;

    if let Some(usage) = data.usage_metadata {
        builder.usage = Some(map_usage(usage)?);
    }

    let candidate = data
        .candidates
        .into_iter()
        .find(|candidate| candidate.index.unwrap_or(0) == 0);
    if let Some(candidate) = candidate {
        if let Some(content) = candidate.content {
            for (part_index, part) in content.parts.into_iter().enumerate() {
                if let Some(text) = part.text {
                    decode_google_text_part(
                        &mut events,
                        builder,
                        part_index,
                        text,
                        part.thought,
                        part.thought_signature.as_deref(),
                    )?;
                }
                if let Some(function_call) = part.function_call {
                    decode_google_function_call(
                        &mut events,
                        builder,
                        part_index,
                        function_call,
                        part.thought_signature.as_deref(),
                    )?;
                }
            }
        }
        if let Some(reason) = candidate.finish_reason {
            let has_tool_calls = !builder.tool_call_builders.is_empty();
            finish_google(
                &mut events,
                builder,
                map_stop_reason(&reason, has_tool_calls),
            )?;
        }
    } else if data
        .prompt_feedback
        .and_then(|feedback| feedback.block_reason)
        .is_some()
    {
        finish_google(&mut events, builder, StopReason::Refusal)?;
    }

    Ok(events)
}

fn ensure_started(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    if !builder.started {
        emit_event(
            events,
            builder,
            StreamEvent::Started {
                response_id: builder.response_id.clone(),
            },
        )?;
    }
    Ok(())
}

fn decode_google_text_part(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    provider_part_index: usize,
    text: String,
    thought: bool,
    signature: Option<&str>,
) -> Result<(), AiError> {
    let kind = if thought { "reasoning" } else { "text" };
    let key = format!("google_candidate_0_part_{provider_part_index}_{kind}");
    let index = google_canonical_index(builder, &key);
    if thought {
        if !builder.reasoning_text_buffers.contains_key(&index) {
            emit_event(events, builder, StreamEvent::ReasoningStart { index })?;
        }
    } else if !builder.text_buffers.contains_key(&index) {
        emit_event(events, builder, StreamEvent::TextStart { index })?;
    }
    attach_thought_signature(builder, index, signature)?;

    let previous = if thought {
        builder.reasoning_text_buffers.get(&index)
    } else {
        builder.text_buffers.get(&index)
    }
    .map(String::as_str)
    .unwrap_or_default();
    let Some(delta) = cumulative_or_delta(previous, &text) else {
        return Ok(());
    };
    if thought {
        emit_event(
            events,
            builder,
            StreamEvent::ReasoningDelta { index, delta },
        )?;
    } else {
        emit_event(events, builder, StreamEvent::TextDelta { index, delta })?;
    }
    Ok(())
}

fn decode_google_function_call(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    provider_part_index: usize,
    call: GoogleFunctionCall,
    signature: Option<&str>,
) -> Result<(), AiError> {
    if !call.args.is_object() {
        return Err(AiError::Decode(DecodeError::Json(
            "Google function-call args must be an object".to_owned(),
        )));
    }
    if call.name.is_empty() {
        return Err(AiError::Decode(DecodeError::Json(
            "Google function-call name is empty".to_owned(),
        )));
    }
    let key = format!("google_candidate_0_function_{provider_part_index}");
    let index = google_canonical_index(builder, &key);
    if !builder.tool_call_builders.contains_key(&index) {
        let id = call
            .id
            .filter(|id| !id.is_empty())
            .map(|id| crate::protocol::normalize_tool_call_id(&id))
            .unwrap_or_else(|| format!("google_call_{index}"));
        emit_event(
            events,
            builder,
            StreamEvent::ToolCallStart {
                index,
                id: ToolCallId(id),
                name: call.name,
            },
        )?;
    }
    attach_thought_signature(builder, index, signature)?;

    // Gemini may send cumulative function-call snapshots. Keep a merged object
    // privately until terminal rather than appending multiple complete JSON
    // objects into canonical tool arguments.
    let args_key = function_args_key(index);
    let merged = match builder.temp_buffers.get(&args_key) {
        Some(previous) => {
            let mut prior = serde_json::from_str::<Value>(previous)
                .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
            merge_json_object(&mut prior, call.args);
            prior
        }
        None => call.args,
    };
    let args = serde_json::to_string(&merged)
        .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
    builder.replace_temp_buffer(args_key, args)?;
    Ok(())
}

fn attach_thought_signature(
    builder: &mut ResponseBuilder,
    index: usize,
    signature: Option<&str>,
) -> Result<(), AiError> {
    let Some(signature) = signature.filter(|signature| !signature.is_empty()) else {
        return Ok(());
    };
    if !signature_is_bounded(signature) {
        return Err(AiError::Decode(DecodeError::ResponseTooLarge));
    }
    builder.set_provider_metadata(
        index,
        ProviderPartMetadata::GoogleThoughtSignature {
            signature: signature.to_owned(),
        },
    )
}

fn signature_is_bounded(signature: &str) -> bool {
    !signature.is_empty() && signature.len() <= MAX_THOUGHT_SIGNATURE_BYTES
}

fn cumulative_or_delta(previous: &str, next: &str) -> Option<String> {
    if let Some(delta) = next.strip_prefix(previous) {
        (!delta.is_empty()).then(|| delta.to_owned())
    } else if previous.starts_with(next) {
        // Repeated/stale cumulative snapshot.
        None
    } else {
        // Native streaming deltas are not prefix-related.
        Some(next.to_owned())
    }
}

fn google_canonical_index(builder: &mut ResponseBuilder, key: &str) -> usize {
    if let Some(index) = builder.provider_to_canonical_indices.get(key) {
        return *index;
    }
    let index = builder.next_canonical_index;
    builder.next_canonical_index += 1;
    builder
        .provider_to_canonical_indices
        .insert(key.to_owned(), index);
    index
}

fn function_args_key(index: usize) -> String {
    format!("{FUNCTION_ARGS_KEY_PREFIX}{index}")
}

fn merge_json_object(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => merge_json_object(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

fn finish_google(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
    stop_reason: StopReason,
) -> Result<(), AiError> {
    close_google_text_parts(events, builder)?;
    close_google_tool_calls(events, builder)?;
    builder.set_stop_reason(stop_reason);
    if let Some(usage) = builder.usage {
        emit_event(events, builder, StreamEvent::Usage(usage))?;
    }
    let response = builder.finish_mut()?;
    emit_event(events, builder, StreamEvent::Finished(response))
}

fn close_google_text_parts(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    let mut text_indices = builder
        .text_buffers
        .keys()
        .copied()
        .filter(|index| !builder.ended_indices.contains(index))
        .collect::<Vec<_>>();
    text_indices.sort_unstable();
    for index in text_indices {
        emit_event(events, builder, StreamEvent::TextEnd { index })?;
    }
    let mut reasoning_indices = builder
        .reasoning_text_buffers
        .keys()
        .copied()
        .filter(|index| !builder.ended_indices.contains(index))
        .collect::<Vec<_>>();
    reasoning_indices.sort_unstable();
    for index in reasoning_indices {
        emit_event(events, builder, StreamEvent::ReasoningEnd { index })?;
    }
    Ok(())
}

fn close_google_tool_calls(
    events: &mut Vec<StreamEvent>,
    builder: &mut ResponseBuilder,
) -> Result<(), AiError> {
    let mut indices = builder
        .tool_call_builders
        .keys()
        .copied()
        .filter(|index| !builder.ended_indices.contains(index))
        .collect::<Vec<_>>();
    indices.sort_unstable();
    for index in indices {
        if let Some(args) = builder.take_temp_buffer(&function_args_key(index)) {
            emit_event(
                events,
                builder,
                StreamEvent::ToolCallArgsDelta { index, delta: args },
            )?;
        }
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

fn map_stop_reason(reason: &str, has_tool_calls: bool) -> StopReason {
    match reason {
        "STOP" if has_tool_calls => StopReason::ToolUse,
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            StopReason::Refusal
        }
        other => StopReason::Other(other.to_owned()),
    }
}

fn map_usage(usage: GoogleUsageMetadata) -> Result<Usage, AiError> {
    if usage.cached_content_tokens > usage.prompt_tokens
        || usage.thoughts_tokens > usage.candidate_tokens
    {
        return Err(AiError::Decode(DecodeError::UsageUnderflow));
    }
    let input_tokens = usage.prompt_tokens - usage.cached_content_tokens;
    let calculated_total = usage
        .prompt_tokens
        .checked_add(usage.candidate_tokens)
        .ok_or(AiError::Decode(DecodeError::UsageUnderflow))?;
    let total_tokens = if usage.total_tokens == 0 {
        calculated_total
    } else {
        usage.total_tokens
    };
    Ok(Usage {
        input_tokens,
        cache_read_tokens: usage.cached_content_tokens,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        output_tokens: usage.candidate_tokens,
        reasoning_tokens: usage.thoughts_tokens,
        total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Model;
    use crate::types::{
        Capabilities, Endpoint, EndpointId, ModalitySet, ModelId, ModelLimits, ModelSpec,
        OutputModalities,
    };

    fn model() -> Model {
        Model {
            spec: std::sync::Arc::new(ModelSpec {
                id: ModelId("gemini-test".to_owned()),
                api_name: "gemini-2.5-flash".to_owned(),
                display_name: None,
                endpoint: EndpointId("google".to_owned()),
                protocol: Protocol::GoogleGenerativeAi,
                capabilities: Capabilities {
                    input_modalities: ModalitySet::none(),
                    output_modalities: ModalitySet::none(),
                    tools: true,
                    parallel_tool_calls: true,
                    reasoning: None,
                    responses_lite: false,
                    agent_delegation: None,
                    structured_output: true,
                    deferred_tool_loading: false,
                },
                limits: ModelLimits {
                    context_window: 1_000_000,
                    max_output_tokens: 65_536,
                },
                pricing: None,
                cache: Default::default(),
            }),
            endpoint: std::sync::Arc::new(Endpoint {
                id: EndpointId("google".to_owned()),
                base_url: url::Url::parse("https://example.invalid/v1beta/").unwrap(),
                auth: crate::auth::Auth::None,
                default_headers: http::HeaderMap::new(),
                transport: Default::default(),
                runtime: Default::default(),
                timeout: std::time::Duration::from_secs(10),
            }),
        }
    }

    #[test]
    fn maps_structured_output_to_native_json_schema() {
        let request = Request {
            messages: Vec::new(),
            system: None,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: Vec::new(),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: Default::default(),
            output_format: OutputFormat::JsonSchema(crate::types::JsonSchemaFormat {
                name: "answer".to_owned(),
                description: None,
                schema: serde_json::json!({"type": "object"}),
                strict: true,
            }),
            output_modalities: OutputModalities::Text,
            session_id: None,
            cache_retention: Default::default(),
            compatibility: Default::default(),
            responses: None,
        };
        let parts = build_request(&model(), &request).unwrap();
        let body: Value = serde_json::from_slice(&parts.body).unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            body["generationConfig"]["responseJsonSchema"]["type"],
            "object"
        );
    }

    #[test]
    fn thought_signature_is_metadata_not_reasoning_classification() {
        let stream_model = model();
        let mut builder = ResponseBuilder::new(
            stream_model.spec.id.clone(),
            Protocol::GoogleGenerativeAi,
            None,
        );
        let frame = SseEvent {
            event: None,
            data: r#"{"candidates":[{"content":{"parts":[{"text":"visible","thoughtSignature":"opaque"}]},"finishReason":"STOP"}]}"#.to_owned(),
        };
        let events = decode_stream_event(&stream_model, &frame, &mut builder).unwrap();
        let response = events
            .into_iter()
            .find_map(|event| match event {
                StreamEvent::Finished(response) => Some(response),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            response.message.content.as_slice(),
            [
                AssistantPart::ProviderMetadata(ProviderPartMetadata::GoogleThoughtSignature { .. }),
                AssistantPart::Text(text)
            ] if text == "visible"
        ));
    }

    #[test]
    fn merges_cumulative_function_arguments_before_finish() {
        let stream_model = model();
        let mut builder = ResponseBuilder::new(
            stream_model.spec.id.clone(),
            Protocol::GoogleGenerativeAi,
            None,
        );
        let first = SseEvent {
            event: None,
            data: r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"read","args":{"path":"a"}}}]}}]}"#.to_owned(),
        };
        let last = SseEvent {
            event: None,
            data: r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"read","args":{"path":"a","line":2}}}]},"finishReason":"STOP"}]}"#.to_owned(),
        };
        decode_stream_event(&stream_model, &first, &mut builder).unwrap();
        let response = decode_stream_event(&stream_model, &last, &mut builder)
            .unwrap()
            .into_iter()
            .find_map(|event| match event {
                StreamEvent::Finished(response) => Some(response),
                _ => None,
            })
            .unwrap();
        assert!(matches!(response.stop_reason, StopReason::ToolUse));
        assert!(matches!(
            response.message.content.as_slice(),
            [AssistantPart::ToolCall(call)] if call.arguments_json == r#"{"line":2,"path":"a"}"#
        ));
    }
}
