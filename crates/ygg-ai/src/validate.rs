//! Pure request validation and capability checks shared by every codec.

use crate::error::{AiError, Diagnostic, UnsupportedError, ValidationError};
use crate::types::{
    AssistantPart, AudioFormat, AudioPayload, Capabilities, ImageSource, Media, Message, ModelId,
    ModelLimits, OutputFormat, OutputModalities, Protocol, ReasoningConfig, Request, ToolCallId,
    ToolChoice, ToolResultPart, UserPart,
};
use crate::CompatibilityMode;
use std::borrow::Cow;
use std::collections::HashSet;
use std::time::SystemTime;

/// Whether a provider media reference can be sent on `protocol` right now.
///
/// A reference is usable only when it was minted for the same protocol and has
/// not expired. Validation flags an unusable reference with a diagnostic
/// (Strict errors); codecs call this to actually *drop* the part on the wire so
/// an expired/wrong-protocol file ID is never serialized (design §7).
pub(crate) fn provider_ref_is_usable(
    reference: &crate::types::ProviderMediaRef,
    protocol: Protocol,
) -> bool {
    reference.protocol == protocol
        && reference
            .expires_at
            .is_none_or(|expires_at| expires_at > SystemTime::now())
}

/// Returns a request-local copy with portable effort clamped to the model's
/// advertised range. Keeping this normalization beside validation ensures all
/// codecs, including direct codec tests, apply identical model gating.
pub(crate) fn normalize_request_reasoning<'a>(
    req: &'a Request,
    caps: &Capabilities,
) -> Cow<'a, Request> {
    let (ReasoningConfig::Effort(effort), Some(capability)) = (&req.reasoning, &caps.reasoning)
    else {
        return Cow::Borrowed(req);
    };
    let effective = (*effort)
        .max(capability.min_effort)
        .min(capability.max_effort);
    if effective == *effort {
        return Cow::Borrowed(req);
    }
    let mut normalized = req.clone();
    normalized.reasoning = ReasoningConfig::Effort(effective);
    Cow::Owned(normalized)
}

/// Validates a request against the model's capabilities and protocol constraints.
///
/// In `Strict` mode, returns the first error encountered.
/// In `Lossy` mode, returns a list of diagnostics for any dropped or downgraded features.
pub(crate) fn validate_request(
    req: &Request,
    caps: &Capabilities,
    limits: &ModelLimits,
    protocol: Protocol,
    target_model: &ModelId,
    mode: CompatibilityMode,
) -> Result<Vec<Diagnostic>, AiError> {
    let mut diagnostics = Vec::new();

    // 1. Temperature check
    if let Some(temp) = req.temperature {
        if !temp.is_finite() || !(0.0..=2.0).contains(&temp) {
            return Err(AiError::Validation(ValidationError::InvalidTemperature));
        }
    }

    // 2. Max output tokens check
    if let Some(requested) = req.max_output_tokens {
        if requested == 0 || requested > limits.max_output_tokens {
            return Err(AiError::Validation(
                ValidationError::InvalidMaxOutputTokens {
                    requested,
                    model_max: limits.max_output_tokens,
                },
            ));
        }
    }

    for tool in &req.tools {
        if tool.name.is_empty() || !tool.parameters.is_object() {
            return Err(AiError::Validation(ValidationError::InvalidToolSchema(
                tool.name.clone(),
            )));
        }
    }
    if let ToolChoice::Named(name) = &req.tool_choice {
        if !req.tools.iter().any(|tool| &tool.name == name) {
            return Err(AiError::Validation(ValidationError::InvalidToolSchema(
                format!("named tool `{name}` is not defined"),
            )));
        }
    }
    if protocol == Protocol::OpenAiResponses && !req.stop.is_empty() {
        if mode == CompatibilityMode::Strict {
            return Err(AiError::Unsupported(UnsupportedError::StopSequences));
        }
        diagnostics.push(Diagnostic {
            code: "dropped_stop_sequences".to_string(),
            message: "OpenAI Responses does not accept stop sequences".to_string(),
        });
    }

    // 3. Tool results and calls pairing
    let mut all_tool_calls: HashSet<ToolCallId> = HashSet::new();
    let mut pending_calls: HashSet<ToolCallId> = HashSet::new();

    for msg in &req.messages {
        match msg {
            Message::Assistant(ref assistant) => {
                // If the protocol requires pairing (Anthropic, OpenAI Responses),
                // check if there were any unresolved tool calls from the previous assistant turn.
                if (protocol == Protocol::AnthropicMessages
                    || protocol == Protocol::OpenAiResponses)
                    && !pending_calls.is_empty()
                {
                    // In Strict mode, this is a ValidationError.
                    // In Lossy mode, we emit a diagnostic and expect the codec to insert a synthetic error.
                    for call_id in &pending_calls {
                        if mode == CompatibilityMode::Strict {
                            return Err(AiError::Validation(ValidationError::MissingToolResult(
                                call_id.clone(),
                            )));
                        } else {
                            diagnostics.push(Diagnostic {
                                code: "missing_tool_result".to_string(),
                                message: format!("Missing tool result for call ID {:?}", call_id),
                            });
                        }
                    }
                    pending_calls.clear();
                }

                // Collect tool calls from this assistant message
                for part in &assistant.content {
                    if let AssistantPart::ToolCall(ref tc) = part {
                        // Invalid caller input is a validation error, not a wire decode error.
                        tc.arguments_value().map_err(|_| {
                            AiError::Validation(ValidationError::ToolArgumentsNotObject(
                                tc.id.clone(),
                            ))
                        })?;
                        all_tool_calls.insert(tc.id.clone());
                        pending_calls.insert(tc.id.clone());
                    }
                }
            }
            Message::User(ref user) => {
                for part in &user.content {
                    if let UserPart::ToolResult(ref tr) = part {
                        if !all_tool_calls.contains(&tr.tool_call_id) {
                            return Err(AiError::Validation(ValidationError::OrphanToolResult(
                                tr.tool_call_id.clone(),
                            )));
                        }
                        pending_calls.remove(&tr.tool_call_id);
                    }
                }
            }
        }
    }

    if (protocol == Protocol::AnthropicMessages || protocol == Protocol::OpenAiResponses)
        && !pending_calls.is_empty()
    {
        for call_id in &pending_calls {
            if mode == CompatibilityMode::Strict {
                return Err(AiError::Validation(ValidationError::MissingToolResult(
                    call_id.clone(),
                )));
            }
            diagnostics.push(Diagnostic {
                code: "missing_tool_result".to_string(),
                message: format!("Missing tool result for call ID {:?}", call_id),
            });
        }
    }

    // 4. Modality/capability checks on messages
    for msg in &req.messages {
        match msg {
            Message::User(ref user) => {
                for part in &user.content {
                    match part {
                        UserPart::Media(Media::Image(ref image)) => {
                            if !caps
                                .input_modalities
                                .contains(crate::types::Modality::Image)
                            {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(UnsupportedError::Image));
                                } else {
                                    diagnostics.push(Diagnostic {
                                        code: "dropped_image".to_string(),
                                        message: "Model does not support image input".to_string(),
                                    });
                                }
                            }

                            // Inline images must carry an explicit media type: the
                            // wire mapping (`data:<mime>;base64,…` / Anthropic
                            // `source.media_type`) has no documented default, and
                            // guessing a wire field is forbidden (design §75).
                            // Anthropic further restricts it to a documented set.
                            if let ImageSource::Inline(_) = &image.source {
                                let media_issue = match &image.media_type {
                                    None => Some(
                                        "Inline images require an explicit media type".to_string(),
                                    ),
                                    Some(mime)
                                        if protocol == Protocol::AnthropicMessages
                                            && !matches!(
                                                mime.as_ref(),
                                                "image/jpeg"
                                                    | "image/png"
                                                    | "image/gif"
                                                    | "image/webp"
                                            ) =>
                                    {
                                        Some("Anthropic inline images require JPEG, PNG, GIF, or WebP media type".to_string())
                                    }
                                    Some(_) => None,
                                };
                                if let Some(message) = media_issue {
                                    if mode == CompatibilityMode::Strict {
                                        return Err(AiError::Unsupported(UnsupportedError::Image));
                                    }
                                    diagnostics.push(Diagnostic {
                                        code: "dropped_image_media_type".to_string(),
                                        message,
                                    });
                                }
                            }

                            // Provider-hosted image IDs are only documented for Responses.
                            if matches!(&image.source, ImageSource::ProviderRef(_))
                                && protocol != Protocol::OpenAiResponses
                            {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(
                                        UnsupportedError::ProviderMediaRef,
                                    ));
                                }
                                diagnostics.push(Diagnostic {
                                    code: "dropped_provider_media_ref".to_string(),
                                    message: "Image provider references are only supported by OpenAI Responses".to_string(),
                                });
                            }

                            // Check provider ref protocol and expiration
                            if let ImageSource::ProviderRef(ref r) = image.source {
                                if r.protocol != protocol {
                                    if mode == CompatibilityMode::Strict {
                                        return Err(AiError::Unsupported(
                                            UnsupportedError::ProviderMediaRef,
                                        ));
                                    } else {
                                        diagnostics.push(Diagnostic {
                                            code: "dropped_mismatched_media_ref".to_string(),
                                            message: "Provider media reference protocol mismatch"
                                                .to_string(),
                                        });
                                    }
                                }
                                if let Some(expires_at) = r.expires_at {
                                    if expires_at <= SystemTime::now() {
                                        if mode == CompatibilityMode::Strict {
                                            return Err(AiError::Unsupported(
                                                UnsupportedError::ProviderMediaRef,
                                            ));
                                        } else {
                                            diagnostics.push(Diagnostic {
                                                code: "dropped_expired_media_ref".to_string(),
                                                message: "Provider media reference is expired"
                                                    .to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        UserPart::Media(Media::Audio(ref audio)) => {
                            if protocol != Protocol::OpenAiChat {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(UnsupportedError::Audio));
                                }
                                diagnostics.push(Diagnostic {
                                    code: "dropped_audio".to_string(),
                                    message: "Audio input is Chat-only in v0.1".to_string(),
                                });
                            }
                            if protocol == Protocol::OpenAiChat
                                && !caps
                                    .input_modalities
                                    .contains(crate::types::Modality::Audio)
                            {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(UnsupportedError::Audio));
                                } else {
                                    diagnostics.push(Diagnostic {
                                        code: "dropped_audio".to_string(),
                                        message: "Model does not support audio input".to_string(),
                                    });
                                }
                            }

                            if protocol == Protocol::OpenAiChat
                                && matches!(audio.payload, AudioPayload::ProviderRef(_))
                            {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(
                                        UnsupportedError::ProviderMediaRef,
                                    ));
                                }
                                diagnostics.push(Diagnostic {
                                    code: "dropped_provider_media_ref".to_string(),
                                    message:
                                        "Bare audio references cannot be used as Chat user input"
                                            .to_string(),
                                });
                            }

                            // Chat format gate (only Wav or Mp3 for inline input)
                            if protocol == Protocol::OpenAiChat {
                                match audio.format {
                                    AudioFormat::Wav | AudioFormat::Mp3 => {}
                                    _ => {
                                        if mode == CompatibilityMode::Strict {
                                            return Err(AiError::Unsupported(
                                                UnsupportedError::Audio,
                                            ));
                                        } else {
                                            diagnostics.push(Diagnostic {
                                                code: "dropped_audio_format".to_string(),
                                                message: format!("OpenAI Chat only supports Wav/Mp3 audio input. Got {:?}", audio.format),
                                            });
                                        }
                                    }
                                }
                            }

                            // Check provider ref protocol and expiration
                            let ref_opt = match &audio.payload {
                                AudioPayload::ProviderRef(r) => Some(r),
                                AudioPayload::InlineWithProviderRef { reference, .. } => {
                                    Some(reference)
                                }
                                AudioPayload::Inline(_) => None,
                            };
                            if let Some(r) = ref_opt.filter(|_| protocol == Protocol::OpenAiChat) {
                                if r.protocol != protocol {
                                    if mode == CompatibilityMode::Strict {
                                        return Err(AiError::Unsupported(
                                            UnsupportedError::ProviderMediaRef,
                                        ));
                                    } else {
                                        diagnostics.push(Diagnostic {
                                            code: "dropped_mismatched_media_ref".to_string(),
                                            message: "Provider media reference protocol mismatch"
                                                .to_string(),
                                        });
                                    }
                                }
                                if let Some(expires_at) = r.expires_at {
                                    if expires_at <= SystemTime::now() {
                                        if mode == CompatibilityMode::Strict {
                                            return Err(AiError::Unsupported(
                                                UnsupportedError::ProviderMediaRef,
                                            ));
                                        } else {
                                            diagnostics.push(Diagnostic {
                                                code: "dropped_expired_media_ref".to_string(),
                                                message: "Provider media reference is expired"
                                                    .to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        UserPart::ToolResult(ref tr) => {
                            for part in &tr.content {
                                let supported = match part {
                                    ToolResultPart::Text(_) => true,
                                    ToolResultPart::Media(Media::Image(image)) => match protocol {
                                        Protocol::OpenAiChat => false,
                                        Protocol::OpenAiResponses => match &image.source {
                                            ImageSource::ProviderRef(reference) => {
                                                reference.protocol == protocol
                                                    && reference.expires_at.is_none_or(|expiry| {
                                                        expiry > SystemTime::now()
                                                    })
                                            }
                                            _ => true,
                                        },
                                        Protocol::AnthropicMessages => {
                                            matches!(
                                                &image.source,
                                                ImageSource::Inline(_) | ImageSource::Url(_)
                                            )
                                        }
                                    },
                                    ToolResultPart::Media(Media::Audio(_)) => false,
                                };
                                if !supported {
                                    if mode == CompatibilityMode::Strict {
                                        return Err(AiError::Unsupported(
                                            UnsupportedError::ToolResultMedia,
                                        ));
                                    }
                                    diagnostics.push(Diagnostic {
                                        code: "dropped_tool_result_media".to_string(),
                                        message: "Tool-result media has no mapping on the target protocol".to_string(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant(ref assistant) => {
                let assistant_media_count = assistant
                    .content
                    .iter()
                    .filter(|part| matches!(part, AssistantPart::Media(_)))
                    .count();
                if assistant_media_count > 1 {
                    if mode == CompatibilityMode::Strict {
                        return Err(AiError::Unsupported(UnsupportedError::ProviderMediaRef));
                    }
                    diagnostics.push(Diagnostic {
                        code: "dropped_assistant_media".to_string(),
                        message: "Only one prior assistant audio reference can be replayed"
                            .to_string(),
                    });
                }
                for part in &assistant.content {
                    match part {
                        AssistantPart::Reasoning(rp) => {
                            if protocol != Protocol::OpenAiChat && rp.state.is_none() {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(
                                        UnsupportedError::ReasoningStateMismatch {
                                            have: assistant.protocol,
                                            want: protocol,
                                        },
                                    ));
                                }
                                diagnostics.push(Diagnostic {
                                    code: "dropped_reasoning_state".to_string(),
                                    message: "Target protocol requires replayable reasoning state"
                                        .to_string(),
                                });
                            }
                            if let Some(state) = &rp.state {
                                let kind_matches = matches!(
                                    (protocol, &state.kind),
                                    (
                                        Protocol::OpenAiResponses,
                                        crate::types::ReasoningStateKind::OpenAiReasoning { .. }
                                    ) | (
                                        Protocol::AnthropicMessages,
                                        crate::types::ReasoningStateKind::AnthropicSignature { .. }
                                    ) | (
                                        Protocol::AnthropicMessages,
                                        crate::types::ReasoningStateKind::AnthropicRedacted { .. }
                                    )
                                );
                                if state.protocol != protocol
                                    || &state.model != target_model
                                    || !kind_matches
                                {
                                    if mode == CompatibilityMode::Strict {
                                        return Err(AiError::Unsupported(
                                            UnsupportedError::ReasoningStateMismatch {
                                                have: state.protocol,
                                                want: protocol,
                                            },
                                        ));
                                    }
                                    diagnostics.push(Diagnostic {
                                        code: "dropped_reasoning_state".to_string(),
                                        message:
                                            "Reasoning state protocol, kind, or model mismatch"
                                                .to_string(),
                                    });
                                }
                            }
                        }
                        AssistantPart::Media(Media::Audio(audio)) => {
                            let reference = match &audio.payload {
                                AudioPayload::ProviderRef(reference)
                                | AudioPayload::InlineWithProviderRef { reference, .. } => {
                                    Some(reference)
                                }
                                AudioPayload::Inline(_) => None,
                            };
                            let valid = protocol == Protocol::OpenAiChat
                                && reference.is_some_and(|reference| {
                                    reference.protocol == protocol
                                        && reference
                                            .expires_at
                                            .is_none_or(|expiry| expiry > SystemTime::now())
                                });
                            if !valid {
                                if mode == CompatibilityMode::Strict {
                                    return Err(AiError::Unsupported(
                                        UnsupportedError::ProviderMediaRef,
                                    ));
                                }
                                diagnostics.push(Diagnostic {
                                    code: "dropped_assistant_media".to_string(),
                                    message: "Assistant audio is replayable only as a valid Chat provider reference".to_string(),
                                });
                            }
                        }
                        AssistantPart::Media(Media::Image(_)) => {
                            if mode == CompatibilityMode::Strict {
                                return Err(AiError::Unsupported(UnsupportedError::Image));
                            }
                            diagnostics.push(Diagnostic {
                                code: "dropped_assistant_media".to_string(),
                                message: "Assistant image replay has no v0.1 wire mapping"
                                    .to_string(),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // 5. Tools capability check
    if !req.tools.is_empty() && !caps.tools {
        if mode == CompatibilityMode::Strict {
            return Err(AiError::Unsupported(UnsupportedError::Tools));
        } else {
            diagnostics.push(Diagnostic {
                code: "dropped_tools".to_string(),
                message: "Model does not support tool calling".to_string(),
            });
        }
    }

    // 6. Tool choice capability check
    if req.tool_choice != ToolChoice::Auto && req.tool_choice != ToolChoice::None && !caps.tools {
        if mode == CompatibilityMode::Strict {
            return Err(AiError::Unsupported(UnsupportedError::ToolChoice));
        } else {
            diagnostics.push(Diagnostic {
                code: "dropped_tool_choice".to_string(),
                message: "Model does not support tool calling choice".to_string(),
            });
        }
    }

    // 7. Reasoning capability and control checks
    if req.reasoning != ReasoningConfig::Off {
        if let ReasoningConfig::Budget(budget) = &req.reasoning {
            let effective_output_limit = req.max_output_tokens.unwrap_or(limits.max_output_tokens);
            if *budget < 1024 || *budget > effective_output_limit {
                return Err(AiError::Validation(
                    ValidationError::ReasoningBudgetOutOfRange,
                ));
            }
        }
        let supported = match (&req.reasoning, &caps.reasoning) {
            (_, None) => false,
            (ReasoningConfig::Effort(_), Some(_)) => true,
            (ReasoningConfig::Budget(budget), Some(capability)) => {
                capability.control == crate::types::ReasoningControl::TokenBudget
                    && *budget <= limits.max_output_tokens
            }
            (ReasoningConfig::Off, _) => true,
        };
        if !supported {
            if mode == CompatibilityMode::Strict {
                return Err(AiError::Unsupported(UnsupportedError::Reasoning));
            }
            diagnostics.push(Diagnostic {
                code: "ignored_reasoning".to_string(),
                message: "Reasoning request cannot be represented by this model".to_string(),
            });
        }
    }

    // 8. Structured output format check
    match &req.output_format {
        OutputFormat::JsonObject => {
            if !caps.structured_output || protocol == Protocol::AnthropicMessages {
                if mode == CompatibilityMode::Strict {
                    return Err(AiError::Unsupported(UnsupportedError::StructuredOutput));
                }
                diagnostics.push(Diagnostic {
                    code: "downgraded_output_format".to_string(),
                    message: "JSON object output is unsupported by the target model or protocol"
                        .to_string(),
                });
            }
        }
        OutputFormat::JsonSchema(ref s) => {
            if !caps.structured_output {
                if mode == CompatibilityMode::Strict {
                    return Err(AiError::Unsupported(UnsupportedError::StructuredOutput));
                } else {
                    diagnostics.push(Diagnostic {
                        code: "downgraded_output_format".to_string(),
                        message: "Model does not support structured output formats".to_string(),
                    });
                }
            }
            // Schema must be a JSON object
            if !s.schema.is_object() {
                return Err(AiError::Validation(ValidationError::InvalidOutputSchema(
                    "JSON Schema must be a JSON object".to_string(),
                )));
            }
            // Name validation: 1-64 ASCII letters, digits, _, or -
            let is_valid_name = !s.name.is_empty()
                && s.name.len() <= 64
                && s.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !is_valid_name {
                return Err(AiError::Validation(
                    ValidationError::InvalidOutputFormatName(s.name.clone()),
                ));
            }
        }
        OutputFormat::Text => {}
    }

    // 9. Audio output capability check
    if let OutputModalities::TextAndAudio(_) = req.output_modalities {
        let supported = protocol == Protocol::OpenAiChat
            && caps
                .output_modalities
                .contains(crate::types::Modality::Audio);
        if !supported {
            if mode == CompatibilityMode::Strict {
                return Err(AiError::Unsupported(UnsupportedError::AudioOutput));
            }
            diagnostics.push(Diagnostic {
                code: "downgraded_audio_output".to_string(),
                message: "Audio output requires an audio-capable OpenAI Chat model".to_string(),
            });
        }
    }

    Ok(diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, Message, ModalitySet, ToolCall, ToolCallId, UserMessage};

    fn dummy_caps(
        image: bool,
        audio_in: bool,
        audio_out: bool,
        tools: bool,
        reasoning: bool,
        structured: bool,
    ) -> Capabilities {
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

        Capabilities {
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
        }
    }

    fn dummy_limits() -> ModelLimits {
        ModelLimits {
            context_window: 10000,
            max_output_tokens: 1000,
        }
    }

    #[test]
    fn test_orphan_tool_result() {
        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::ToolResult(crate::types::ToolResult {
                    tool_call_id: ToolCallId("orphan".to_string()),
                    content: vec![],
                    is_error: false,
                })],
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
        let caps = dummy_caps(false, false, false, true, false, false);
        let limits = dummy_limits();

        let res = validate_request(
            &req,
            &caps,
            &limits,
            Protocol::OpenAiChat,
            &ModelId("model".to_string()),
            CompatibilityMode::Strict,
        );
        assert!(matches!(
            res,
            Err(AiError::Validation(ValidationError::OrphanToolResult(_)))
        ));
    }

    #[test]
    fn test_missing_tool_result() {
        let req = Request {
            system: None,
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("call_1".to_string()),
                        name: "tool".to_string(),
                        arguments_json: r#"{}"#.to_string(),
                    })],
                    model: ModelId("model".to_string()),
                    protocol: Protocol::OpenAiChat,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::Text("consecutive assistant".to_string())],
                    model: ModelId("model".to_string()),
                    protocol: Protocol::OpenAiChat,
                }),
            ],
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
        let caps = dummy_caps(false, false, false, true, false, false);
        let limits = dummy_limits();

        // Anthropic requires tool pairing. In Strict mode, consecutive Assistant message before resolving call_1 is error.
        let res_strict = validate_request(
            &req,
            &caps,
            &limits,
            Protocol::AnthropicMessages,
            &ModelId("model".to_string()),
            CompatibilityMode::Strict,
        );
        assert!(matches!(
            res_strict,
            Err(AiError::Validation(ValidationError::MissingToolResult(_)))
        ));

        // In Lossy mode, it emits a Diagnostic
        let res_lossy = validate_request(
            &req,
            &caps,
            &limits,
            Protocol::AnthropicMessages,
            &ModelId("model".to_string()),
            CompatibilityMode::Lossy,
        )
        .unwrap();
        assert_eq!(res_lossy[0].code, "missing_tool_result");
    }

    #[test]
    fn test_image_input_gate() {
        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Media(Media::image_url(
                    url::Url::parse("https://example.com/a.jpg").unwrap(),
                    None,
                ))],
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
        let caps_no_image = dummy_caps(false, false, false, false, false, false);
        let limits = dummy_limits();

        let res_strict = validate_request(
            &req,
            &caps_no_image,
            &limits,
            Protocol::OpenAiChat,
            &ModelId("model".to_string()),
            CompatibilityMode::Strict,
        );
        assert!(matches!(
            res_strict,
            Err(AiError::Unsupported(UnsupportedError::Image))
        ));

        let res_lossy = validate_request(
            &req,
            &caps_no_image,
            &limits,
            Protocol::OpenAiChat,
            &ModelId("model".to_string()),
            CompatibilityMode::Lossy,
        )
        .unwrap();
        assert_eq!(res_lossy[0].code, "dropped_image");
    }

    #[test]
    fn test_audio_input_gate() {
        let req = Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Media(Media::audio_bytes(
                    bytes::Bytes::from("wav"),
                    AudioFormat::Flac,
                ))],
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
        // Anthropic has no audio capability
        let caps_anthropic = dummy_caps(true, false, false, false, false, false);
        let limits = dummy_limits();

        let res_strict = validate_request(
            &req,
            &caps_anthropic,
            &limits,
            Protocol::AnthropicMessages,
            &ModelId("model".to_string()),
            CompatibilityMode::Strict,
        );
        assert!(matches!(
            res_strict,
            Err(AiError::Unsupported(UnsupportedError::Audio))
        ));

        let res_lossy = validate_request(
            &req,
            &caps_anthropic,
            &limits,
            Protocol::AnthropicMessages,
            &ModelId("model".to_string()),
            CompatibilityMode::Lossy,
        )
        .unwrap();
        assert_eq!(res_lossy[0].code, "dropped_audio");
    }
}

/// The design §7 capability/validation table, exercised row-by-row in both
/// Strict and Lossy modes (plan Task 3.1 acceptance). Strict rows assert the
/// exact structured error; Lossy rows assert the exact diagnostic code.
#[cfg(test)]
mod matrix_tests {
    use super::{normalize_request_reasoning, validate_request};
    use crate::error::{AiError, UnsupportedError, ValidationError};
    use crate::types::{
        AssistantMessage, AssistantPart, AudioFormat, AudioOutputOptions, AudioVoice, Capabilities,
        ImageDetail, ImageMedia, ImageSource, JsonSchemaFormat, Media, Message, Modality,
        ModalitySet, ModelId, ModelLimits, OutputFormat, OutputModalities, Protocol,
        ProviderMediaRef, ReasoningConfig, ReasoningEffort, ReasoningPart, ReasoningState,
        ReasoningStateKind, Request, ToolCall, ToolCallId, ToolChoice, ToolDef, ToolResult,
        ToolResultPart, UserMessage, UserPart,
    };
    use crate::CompatibilityMode::{Lossy, Strict};

    fn caps(
        image: bool,
        audio_in: bool,
        audio_out: bool,
        tools: bool,
        reasoning: bool,
        structured: bool,
    ) -> Capabilities {
        let mut input = ModalitySet::none();
        if image {
            input = input.with(Modality::Image);
        }
        if audio_in {
            input = input.with(Modality::Audio);
        }
        let mut output = ModalitySet::none();
        if audio_out {
            output = output.with(Modality::Audio);
        }
        Capabilities {
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
        }
    }

    fn limits() -> ModelLimits {
        ModelLimits {
            context_window: 100_000,
            max_output_tokens: 1000,
        }
    }

    fn base() -> Request {
        Request {
            system: None,
            messages: vec![],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        }
    }

    fn user(parts: Vec<UserPart>) -> Message {
        Message::User(UserMessage { content: parts })
    }

    fn run(
        req: &Request,
        c: &Capabilities,
        p: Protocol,
    ) -> Result<Vec<crate::Diagnostic>, AiError> {
        validate_request(
            req,
            c,
            &limits(),
            p,
            &ModelId("target".into()),
            req.compatibility,
        )
    }

    fn has_code(diags: &[crate::Diagnostic], code: &str) -> bool {
        diags.iter().any(|d| d.code == code)
    }

    // --- image input gate ---
    #[test]
    fn image_without_capability() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::Media(Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from_static(b"x")),
            media_type: Some("image/png".parse().unwrap()),
            detail: Some(ImageDetail::Auto),
        }))])];
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Unsupported(UnsupportedError::Image))
        ));
        req.compatibility = Lossy;
        let diags = run(
            &req,
            &caps(false, false, false, false, false, false),
            Protocol::OpenAiChat,
        )
        .unwrap();
        assert!(has_code(&diags, "dropped_image"));
    }

    // --- Anthropic documents URL image sources ---
    #[test]
    fn image_url_on_anthropic_is_supported() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::Media(Media::image_url(
            url::Url::parse("https://example.test/a.png").unwrap(),
            None,
        ))])];
        let c = caps(true, false, false, false, false, false);
        assert!(run(&req, &c, Protocol::AnthropicMessages)
            .unwrap()
            .is_empty());
        req.compatibility = Lossy;
        assert!(run(&req, &c, Protocol::AnthropicMessages)
            .unwrap()
            .is_empty());
    }

    // --- audio input gate (always fails on Responses & Anthropic) ---
    #[test]
    fn audio_input_rejected_on_responses_and_anthropic() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::Media(Media::audio_bytes(
            bytes::Bytes::from_static(b"RIFF"),
            AudioFormat::Wav,
        ))])];
        for p in [Protocol::OpenAiResponses, Protocol::AnthropicMessages] {
            assert!(
                matches!(
                    run(&req, &caps(true, false, false, false, false, false), p),
                    Err(AiError::Unsupported(UnsupportedError::Audio))
                ),
                "strict audio reject on {p:?}"
            );
        }
        req.compatibility = Lossy;
        let diags = run(
            &req,
            &caps(true, false, false, false, false, false),
            Protocol::AnthropicMessages,
        )
        .unwrap();
        assert!(has_code(&diags, "dropped_audio"));
    }

    // --- Chat audio non-inline format gate ---
    #[test]
    fn chat_audio_non_wav_mp3_rejected() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::Media(Media::audio_bytes(
            bytes::Bytes::from_static(b"OggS"),
            AudioFormat::Opus,
        ))])];
        let c = caps(false, true, false, false, false, false);
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Unsupported(UnsupportedError::Audio))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(&req, &c, Protocol::OpenAiChat).unwrap(),
            "dropped_audio_format"
        ));
    }

    // --- audio output on non-audio models / non-Chat protocols ---
    #[test]
    fn audio_output_requires_chat_and_capability() {
        let mut req = base();
        req.output_modalities = OutputModalities::TextAndAudio(AudioOutputOptions {
            format: AudioFormat::Wav,
            voice: AudioVoice::Named("alloy".into()),
        });
        // Responses cannot do audio output even with the cap bit.
        assert!(matches!(
            run(
                &req,
                &caps(false, false, true, false, false, false),
                Protocol::OpenAiResponses
            ),
            Err(AiError::Unsupported(UnsupportedError::AudioOutput))
        ));
        // Chat model lacking the audio-out cap.
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Unsupported(UnsupportedError::AudioOutput))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::AnthropicMessages
            )
            .unwrap(),
            "downgraded_audio_output"
        ));
    }

    // --- tools without capability ---
    #[test]
    fn tools_without_capability() {
        let mut req = base();
        req.tools = vec![ToolDef {
            name: "grep".into(),
            description: "search".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Unsupported(UnsupportedError::Tools))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            )
            .unwrap(),
            "dropped_tools"
        ));
    }

    // --- tool_choice without capability ---
    #[test]
    fn tool_choice_without_capability() {
        let mut req = base();
        req.tool_choice = ToolChoice::Required;
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Unsupported(UnsupportedError::ToolChoice))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            )
            .unwrap(),
            "dropped_tool_choice"
        ));
    }

    #[test]
    fn reasoning_effort_is_clamped_to_the_advertised_range() {
        let mut req = base();
        req.reasoning = ReasoningConfig::Effort(ReasoningEffort::Max);
        let mut capabilities = caps(false, false, false, false, true, false);
        let reasoning = capabilities.reasoning.as_mut().unwrap();
        reasoning.min_effort = ReasoningEffort::Low;
        reasoning.max_effort = ReasoningEffort::High;

        let high = normalize_request_reasoning(&req, &capabilities);
        assert_eq!(
            high.reasoning,
            ReasoningConfig::Effort(ReasoningEffort::High)
        );

        req.reasoning = ReasoningConfig::Effort(ReasoningEffort::Minimal);
        let low = normalize_request_reasoning(&req, &capabilities);
        assert_eq!(low.reasoning, ReasoningConfig::Effort(ReasoningEffort::Low));
    }

    #[test]
    fn reasoning_budget_must_fit_the_effective_request_output_limit() {
        let mut req = base();
        req.max_output_tokens = Some(2_000);
        req.reasoning = ReasoningConfig::Budget(3_000);
        let mut capabilities = caps(false, false, false, false, true, false);
        capabilities.reasoning.as_mut().unwrap().control =
            crate::types::ReasoningControl::TokenBudget;
        let model_limits = ModelLimits {
            context_window: 100_000,
            max_output_tokens: 8_000,
        };

        assert!(matches!(
            validate_request(
                &req,
                &capabilities,
                &model_limits,
                Protocol::AnthropicMessages,
                &ModelId("target".into()),
                Strict,
            ),
            Err(AiError::Validation(
                ValidationError::ReasoningBudgetOutOfRange
            ))
        ));
    }

    // --- reasoning without capability ---
    #[test]
    fn reasoning_without_capability() {
        let mut req = base();
        req.reasoning = ReasoningConfig::Effort(ReasoningEffort::High);
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Unsupported(UnsupportedError::Reasoning))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            )
            .unwrap(),
            "ignored_reasoning"
        ));
    }

    // --- reasoning-state protocol/model mismatch ---
    #[test]
    fn reasoning_state_mismatch() {
        let mut req = base();
        req.messages = vec![Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Reasoning(ReasoningPart {
                text: Some("prior".into()),
                state: Some(ReasoningState {
                    protocol: Protocol::AnthropicMessages,
                    model: ModelId("other-model".into()),
                    kind: ReasoningStateKind::AnthropicSignature {
                        signature: "s".into(),
                    },
                }),
            })],
            model: ModelId("other-model".into()),
            protocol: Protocol::AnthropicMessages,
        })];
        let c = caps(false, false, false, false, true, false);
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiResponses),
            Err(AiError::Unsupported(
                UnsupportedError::ReasoningStateMismatch { .. }
            ))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(&req, &c, Protocol::OpenAiResponses).unwrap(),
            "dropped_reasoning_state"
        ));
    }

    // --- structured output without capability ---
    #[test]
    fn structured_output_without_capability() {
        let mut req = base();
        req.output_format = OutputFormat::JsonObject;
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Unsupported(UnsupportedError::StructuredOutput))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(
                &req,
                &caps(false, false, false, false, false, false),
                Protocol::OpenAiChat
            )
            .unwrap(),
            "downgraded_output_format"
        ));
    }

    // --- JsonObject on Anthropic (unsupported) ---
    #[test]
    fn json_object_unsupported_on_anthropic() {
        let mut req = base();
        req.output_format = OutputFormat::JsonObject;
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, true),
                Protocol::AnthropicMessages
            ),
            Err(AiError::Unsupported(UnsupportedError::StructuredOutput))
        ));
    }

    // --- invalid JSON schema name / non-object schema ---
    #[test]
    fn invalid_schema_name_and_shape() {
        let mut req = base();
        req.output_format = OutputFormat::JsonSchema(JsonSchemaFormat {
            name: "bad name!".into(),
            description: None,
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        });
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, true),
                Protocol::OpenAiChat
            ),
            Err(AiError::Validation(
                ValidationError::InvalidOutputFormatName(_)
            ))
        ));

        req.output_format = OutputFormat::JsonSchema(JsonSchemaFormat {
            name: "ok".into(),
            description: None,
            schema: serde_json::json!([1, 2, 3]),
            strict: true,
        });
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, false, false, true),
                Protocol::OpenAiChat
            ),
            Err(AiError::Validation(ValidationError::InvalidOutputSchema(_)))
        ));
    }

    // --- max_output_tokens bounds ---
    #[test]
    fn max_output_tokens_bounds() {
        let c = caps(false, false, false, false, false, false);
        let mut req = base();
        req.max_output_tokens = Some(0);
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Validation(
                ValidationError::InvalidMaxOutputTokens { .. }
            ))
        ));
        req.max_output_tokens = Some(5000); // over model max 1000
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Validation(
                ValidationError::InvalidMaxOutputTokens { .. }
            ))
        ));
    }

    // --- temperature bounds ---
    #[test]
    fn temperature_bounds() {
        let c = caps(false, false, false, false, false, false);
        let mut req = base();
        req.temperature = Some(f32::NAN);
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Validation(ValidationError::InvalidTemperature))
        ));
        req.temperature = Some(2.5);
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Validation(ValidationError::InvalidTemperature))
        ));
    }

    // --- tool-result media on Chat (text-only tool results) ---
    #[test]
    fn tool_result_media_rejected() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::ToolResult(ToolResult {
            tool_call_id: ToolCallId("call_1".into()),
            content: vec![ToolResultPart::Media(Media::image_url(
                url::Url::parse("https://example.test/a.png").unwrap(),
                None,
            ))],
            is_error: false,
        })])];
        // Provide a preceding assistant tool call so pairing passes and we reach the media check.
        req.messages.insert(
            0,
            Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("call_1".into()),
                    name: "t".into(),
                    arguments_json: "{}".into(),
                })],
                model: ModelId("target".into()),
                protocol: Protocol::OpenAiChat,
            }),
        );
        let c = caps(true, false, false, true, false, false);
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Unsupported(UnsupportedError::ToolResultMedia))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(&req, &c, Protocol::OpenAiChat).unwrap(),
            "dropped_tool_result_media"
        ));
    }

    // --- provider media ref: wrong protocol + expired ---
    #[test]
    fn provider_media_ref_mismatch_and_expiry() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::Media(Media::Image(ImageMedia {
            source: ImageSource::ProviderRef(ProviderMediaRef {
                protocol: Protocol::OpenAiResponses,
                id: "img_1".into(),
                expires_at: None,
            }),
            media_type: None,
            detail: None,
        }))])];
        let c = caps(true, false, false, false, false, false);
        // Wrong protocol (ref is Responses, target Chat).
        assert!(matches!(
            run(&req, &c, Protocol::OpenAiChat),
            Err(AiError::Unsupported(UnsupportedError::ProviderMediaRef))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(&req, &c, Protocol::OpenAiChat).unwrap(),
            "dropped_mismatched_media_ref"
        ));

        // Expired ref on the matching protocol.
        let mut req2 = base();
        req2.compatibility = Lossy;
        req2.messages = vec![user(vec![UserPart::Media(Media::Image(ImageMedia {
            source: ImageSource::ProviderRef(ProviderMediaRef {
                protocol: Protocol::OpenAiChat,
                id: "img_2".into(),
                expires_at: Some(std::time::UNIX_EPOCH),
            }),
            media_type: None,
            detail: None,
        }))])];
        assert!(has_code(
            &run(&req2, &c, Protocol::OpenAiChat).unwrap(),
            "dropped_expired_media_ref"
        ));
    }

    // --- orphan tool result ---
    #[test]
    fn orphan_tool_result() {
        let mut req = base();
        req.messages = vec![user(vec![UserPart::ToolResult(ToolResult {
            tool_call_id: ToolCallId("nope".into()),
            content: vec![ToolResultPart::Text("r".into())],
            is_error: false,
        })])];
        assert!(matches!(
            run(
                &req,
                &caps(false, false, false, true, false, false),
                Protocol::OpenAiChat
            ),
            Err(AiError::Validation(ValidationError::OrphanToolResult(_)))
        ));
    }

    // --- missing tool result (paired protocols) ---
    #[test]
    fn missing_tool_result_paired_protocols() {
        let mut req = base();
        req.messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("call_1".into()),
                    name: "t".into(),
                    arguments_json: "{}".into(),
                })],
                model: ModelId("target".into()),
                protocol: Protocol::AnthropicMessages,
            }),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("next".into())],
                model: ModelId("target".into()),
                protocol: Protocol::AnthropicMessages,
            }),
        ];
        let c = caps(false, false, false, true, false, false);
        assert!(matches!(
            run(&req, &c, Protocol::AnthropicMessages),
            Err(AiError::Validation(ValidationError::MissingToolResult(_)))
        ));
        req.compatibility = Lossy;
        assert!(has_code(
            &run(&req, &c, Protocol::AnthropicMessages).unwrap(),
            "missing_tool_result"
        ));
    }
}
