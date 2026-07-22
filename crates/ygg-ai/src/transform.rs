//! Non-destructive normalization of canonical history for a target model.

use std::collections::HashSet;

use crate::catalog::Model;
use crate::types::{
    AssistantMessage, AssistantPart, AudioFormat, AudioPayload, ImageSource, Media, Message,
    Modality, ToolCallId, ToolResult, ToolResultPart, UserMessage, UserPart,
};

const IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";
const UNSUPPORTED_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: target protocol does not support tool images)";
const UNAVAILABLE_IMAGE_PLACEHOLDER: &str =
    "(image omitted: provider media reference is unavailable)";
const UNAVAILABLE_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: provider media reference is unavailable)";
const AUDIO_PLACEHOLDER: &str = "(audio omitted: model does not support audio)";
const TOOL_AUDIO_PLACEHOLDER: &str = "(tool audio omitted: model does not support audio)";
const ASSISTANT_IMAGE_PLACEHOLDER: &str =
    "(assistant image omitted: target model cannot replay images)";
const ASSISTANT_AUDIO_PLACEHOLDER: &str =
    "(assistant audio omitted: provider media reference is unavailable)";
const MISSING_TOOL_RESULT: &str = "No result provided";

/// Returns a target-compatible copy of canonical conversation history.
///
/// This is Ygg's non-destructive equivalent of pi-ai's `transformMessages()`:
/// the input slice is never changed. The transform:
///
/// - replaces unsupported images and audio with visible placeholders;
/// - converts non-empty cross-model reasoning text into ordinary assistant
///   text while dropping opaque/empty cross-model reasoning;
/// - normalizes tool-call IDs to the common provider-safe wire shape; and
/// - inserts synthetic error results for tool calls that have no result before
///   the next assistant message (or the end of history).
///
/// [`crate::AiClient`] applies this automatically before validation and wire
/// serialization. It is public for callers that need to inspect or estimate the
/// exact replay history in advance.
pub fn transform_messages(messages: &[Message], target: &Model) -> Vec<Message> {
    let transformed = messages
        .iter()
        .map(|message| transform_message(message, target))
        .collect();
    insert_missing_tool_results(transformed)
}

/// Normalizes replay history while preserving the pending user-input suffix for
/// strict capability validation. A user message is historical once an
/// assistant message follows it; user messages after the final assistant are
/// inputs for the request being opened and must not be replaced by replay
/// placeholders before validation.
#[cfg(test)]
pub(crate) fn transform_request_messages(messages: &[Message], target: &Model) -> Vec<Message> {
    let final_assistant = messages
        .iter()
        .rposition(|message| matches!(message, Message::Assistant(_)));
    let transformed = messages
        .iter()
        .enumerate()
        .map(|(index, message)| match message {
            Message::User(user) if final_assistant.is_none_or(|assistant| index > assistant) => {
                Message::User(UserMessage {
                    content: user
                        .content
                        .iter()
                        .map(normalize_pending_user_part)
                        .collect(),
                })
            }
            _ => transform_message(message, target),
        })
        .collect();
    insert_missing_tool_results(transformed)
}

/// Owned request-history normalization used on the send path.
///
/// A request is consumed by [`crate::AiClient`], so copying every text block,
/// tool payload, and media handle before wire serialization only increases
/// latency and peak memory. This transform moves unchanged canonical values and
/// allocates only when compatibility normalization actually replaces content.
pub(crate) fn transform_request_messages_owned(
    messages: Vec<Message>,
    target: &Model,
) -> Vec<Message> {
    let final_assistant = messages
        .iter()
        .rposition(|message| matches!(message, Message::Assistant(_)));
    let transformed = messages
        .into_iter()
        .enumerate()
        .map(|(index, message)| match message {
            Message::User(user) if final_assistant.is_none_or(|assistant| index > assistant) => {
                Message::User(UserMessage {
                    content: user
                        .content
                        .into_iter()
                        .map(normalize_pending_user_part_owned)
                        .collect(),
                })
            }
            message => transform_message_owned(message, target),
        })
        .collect();
    insert_missing_tool_results(transformed)
}

fn normalize_pending_user_part_owned(part: UserPart) -> UserPart {
    match part {
        UserPart::Text(_) | UserPart::Media(_) => part,
        UserPart::ToolResult(mut result) => {
            result.tool_call_id = normalize_id_owned(result.tool_call_id);
            UserPart::ToolResult(result)
        }
    }
}

#[cfg(test)]
fn normalize_pending_user_part(part: &UserPart) -> UserPart {
    match part {
        UserPart::Text(_) | UserPart::Media(_) => part.clone(),
        UserPart::ToolResult(result) => UserPart::ToolResult(ToolResult {
            tool_call_id: normalize_id(&result.tool_call_id),
            content: result.content.clone(),
            is_error: result.is_error,
        }),
    }
}

fn transform_message(message: &Message, target: &Model) -> Message {
    match message {
        Message::User(user) => Message::User(UserMessage {
            content: user
                .content
                .iter()
                .map(|part| transform_user_part(part, target))
                .collect(),
        }),
        Message::Assistant(assistant) => Message::Assistant(AssistantMessage {
            content: assistant
                .content
                .iter()
                .filter_map(|part| transform_assistant_part(part, assistant, target))
                .collect(),
            model: assistant.model.clone(),
            protocol: assistant.protocol,
        }),
    }
}

fn transform_message_owned(message: Message, target: &Model) -> Message {
    match message {
        Message::User(user) => Message::User(UserMessage {
            content: user
                .content
                .into_iter()
                .map(|part| transform_user_part_owned(part, target))
                .collect(),
        }),
        Message::Assistant(assistant) => {
            let same_model =
                assistant.model == target.spec.id && assistant.protocol == target.spec.protocol;
            Message::Assistant(AssistantMessage {
                content: assistant
                    .content
                    .into_iter()
                    .filter_map(|part| transform_assistant_part_owned(part, same_model, target))
                    .collect(),
                model: assistant.model,
                protocol: assistant.protocol,
            })
        }
    }
}

fn transform_user_part(part: &UserPart, target: &Model) -> UserPart {
    match part {
        UserPart::Text(text) => UserPart::Text(text.clone()),
        UserPart::Media(media) => match media {
            Media::Image(image) => {
                if !target
                    .spec
                    .capabilities
                    .input_modalities
                    .contains(Modality::Image)
                {
                    UserPart::Text(IMAGE_PLACEHOLDER.to_string())
                } else if let ImageSource::ProviderRef(reference) = &image.source {
                    if crate::validate::provider_ref_is_usable(reference, target.spec.protocol)
                        && target.spec.protocol == crate::types::Protocol::OpenAiResponses
                    {
                        UserPart::Media(media.clone())
                    } else {
                        UserPart::Text(UNAVAILABLE_IMAGE_PLACEHOLDER.to_string())
                    }
                } else {
                    UserPart::Media(media.clone())
                }
            }
            Media::Audio(audio) => {
                if user_audio_is_replayable(audio, target) {
                    UserPart::Media(media.clone())
                } else {
                    UserPart::Text(AUDIO_PLACEHOLDER.to_string())
                }
            }
        },
        UserPart::ToolResult(result) => UserPart::ToolResult(ToolResult {
            tool_call_id: normalize_id(&result.tool_call_id),
            content: result
                .content
                .iter()
                .map(|part| transform_tool_result_part(part, target))
                .collect(),
            is_error: result.is_error,
        }),
    }
}

fn transform_user_part_owned(part: UserPart, target: &Model) -> UserPart {
    match part {
        UserPart::Text(_) => part,
        UserPart::Media(Media::Image(image)) => {
            if !target
                .spec
                .capabilities
                .input_modalities
                .contains(Modality::Image)
            {
                UserPart::Text(IMAGE_PLACEHOLDER.to_string())
            } else if let ImageSource::ProviderRef(reference) = &image.source {
                if crate::validate::provider_ref_is_usable(reference, target.spec.protocol)
                    && target.spec.protocol == crate::types::Protocol::OpenAiResponses
                {
                    UserPart::Media(Media::Image(image))
                } else {
                    UserPart::Text(UNAVAILABLE_IMAGE_PLACEHOLDER.to_string())
                }
            } else {
                UserPart::Media(Media::Image(image))
            }
        }
        UserPart::Media(Media::Audio(audio)) => {
            if user_audio_is_replayable(&audio, target) {
                UserPart::Media(Media::Audio(audio))
            } else {
                UserPart::Text(AUDIO_PLACEHOLDER.to_string())
            }
        }
        UserPart::ToolResult(result) => UserPart::ToolResult(ToolResult {
            tool_call_id: normalize_id_owned(result.tool_call_id),
            content: result
                .content
                .into_iter()
                .map(|part| transform_tool_result_part_owned(part, target))
                .collect(),
            is_error: result.is_error,
        }),
    }
}

fn transform_tool_result_part(part: &ToolResultPart, target: &Model) -> ToolResultPart {
    match part {
        ToolResultPart::Text(text) => ToolResultPart::Text(text.clone()),
        ToolResultPart::Media(media) => match media {
            Media::Image(image) => {
                if !target
                    .spec
                    .capabilities
                    .input_modalities
                    .contains(Modality::Image)
                {
                    ToolResultPart::Text(TOOL_IMAGE_PLACEHOLDER.to_string())
                } else if target.spec.protocol == crate::types::Protocol::OpenAiChat {
                    ToolResultPart::Text(UNSUPPORTED_TOOL_IMAGE_PLACEHOLDER.to_string())
                } else if let ImageSource::ProviderRef(reference) = &image.source {
                    if target.spec.protocol == crate::types::Protocol::OpenAiResponses
                        && crate::validate::provider_ref_is_usable(reference, target.spec.protocol)
                    {
                        ToolResultPart::Media(media.clone())
                    } else {
                        ToolResultPart::Text(UNAVAILABLE_TOOL_IMAGE_PLACEHOLDER.to_string())
                    }
                } else {
                    ToolResultPart::Media(media.clone())
                }
            }
            Media::Audio(_) => ToolResultPart::Text(TOOL_AUDIO_PLACEHOLDER.to_string()),
        },
    }
}

fn transform_tool_result_part_owned(part: ToolResultPart, target: &Model) -> ToolResultPart {
    match part {
        ToolResultPart::Text(_) => part,
        ToolResultPart::Media(Media::Image(image)) => {
            if !target
                .spec
                .capabilities
                .input_modalities
                .contains(Modality::Image)
            {
                ToolResultPart::Text(TOOL_IMAGE_PLACEHOLDER.to_string())
            } else if target.spec.protocol == crate::types::Protocol::OpenAiChat {
                ToolResultPart::Text(UNSUPPORTED_TOOL_IMAGE_PLACEHOLDER.to_string())
            } else if let ImageSource::ProviderRef(reference) = &image.source {
                if target.spec.protocol == crate::types::Protocol::OpenAiResponses
                    && crate::validate::provider_ref_is_usable(reference, target.spec.protocol)
                {
                    ToolResultPart::Media(Media::Image(image))
                } else {
                    ToolResultPart::Text(UNAVAILABLE_TOOL_IMAGE_PLACEHOLDER.to_string())
                }
            } else {
                ToolResultPart::Media(Media::Image(image))
            }
        }
        ToolResultPart::Media(Media::Audio(_)) => {
            ToolResultPart::Text(TOOL_AUDIO_PLACEHOLDER.to_string())
        }
    }
}

fn transform_assistant_part(
    part: &AssistantPart,
    source: &AssistantMessage,
    target: &Model,
) -> Option<AssistantPart> {
    match part {
        AssistantPart::Text(text) => Some(AssistantPart::Text(text.clone())),
        AssistantPart::ToolCall(call) => {
            let mut call = call.clone();
            call.id = normalize_id(&call.id);
            Some(AssistantPart::ToolCall(call))
        }
        AssistantPart::Reasoning(reasoning) => {
            let same_model =
                source.model == target.spec.id && source.protocol == target.spec.protocol;
            if same_model {
                return Some(AssistantPart::Reasoning(reasoning.clone()));
            }

            let redacted = reasoning.state.as_ref().is_some_and(|state| {
                matches!(
                    state.kind,
                    crate::types::ReasoningStateKind::AnthropicRedacted { .. }
                )
            });
            if redacted {
                return None;
            }
            reasoning
                .text
                .as_ref()
                .filter(|text| !text.trim().is_empty())
                .cloned()
                .map(AssistantPart::Text)
        }
        AssistantPart::Media(Media::Image(_)) => {
            Some(AssistantPart::Text(ASSISTANT_IMAGE_PLACEHOLDER.to_string()))
        }
        AssistantPart::Media(Media::Audio(audio)) => {
            if assistant_audio_is_replayable(audio, target) {
                Some(part.clone())
            } else {
                Some(AssistantPart::Text(ASSISTANT_AUDIO_PLACEHOLDER.to_string()))
            }
        }
    }
}

fn transform_assistant_part_owned(
    part: AssistantPart,
    same_model: bool,
    target: &Model,
) -> Option<AssistantPart> {
    match part {
        AssistantPart::Text(_) => Some(part),
        AssistantPart::ToolCall(mut call) => {
            call.id = normalize_id_owned(call.id);
            Some(AssistantPart::ToolCall(call))
        }
        AssistantPart::Reasoning(reasoning) => {
            if same_model {
                return Some(AssistantPart::Reasoning(reasoning));
            }
            if reasoning.state.as_ref().is_some_and(|state| {
                matches!(
                    state.kind,
                    crate::types::ReasoningStateKind::AnthropicRedacted { .. }
                )
            }) {
                return None;
            }
            reasoning
                .text
                .filter(|text| !text.trim().is_empty())
                .map(AssistantPart::Text)
        }
        AssistantPart::Media(Media::Image(_)) => {
            Some(AssistantPart::Text(ASSISTANT_IMAGE_PLACEHOLDER.to_string()))
        }
        AssistantPart::Media(Media::Audio(audio)) => {
            if assistant_audio_is_replayable(&audio, target) {
                Some(AssistantPart::Media(Media::Audio(audio)))
            } else {
                Some(AssistantPart::Text(ASSISTANT_AUDIO_PLACEHOLDER.to_string()))
            }
        }
    }
}

fn normalize_id(id: &ToolCallId) -> ToolCallId {
    ToolCallId(crate::protocol::normalize_tool_call_id(&id.0))
}

fn normalize_id_owned(id: ToolCallId) -> ToolCallId {
    ToolCallId(crate::protocol::normalize_tool_call_id_owned(id.0))
}

fn user_audio_is_replayable(audio: &crate::types::AudioMedia, target: &Model) -> bool {
    target.spec.protocol == crate::types::Protocol::OpenAiChat
        && target
            .spec
            .capabilities
            .input_modalities
            .contains(Modality::Audio)
        && matches!(audio.format, AudioFormat::Wav | AudioFormat::Mp3)
        && matches!(
            audio.payload,
            AudioPayload::Inline(_) | AudioPayload::InlineWithProviderRef { .. }
        )
}

fn assistant_audio_is_replayable(audio: &crate::types::AudioMedia, target: &Model) -> bool {
    if target.spec.protocol != crate::types::Protocol::OpenAiChat {
        return false;
    }
    let reference = match &audio.payload {
        AudioPayload::ProviderRef(reference)
        | AudioPayload::InlineWithProviderRef { reference, .. } => reference,
        AudioPayload::Inline(_) => return false,
    };
    crate::validate::provider_ref_is_usable(reference, target.spec.protocol)
}

fn insert_missing_tool_results(messages: Vec<Message>) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    let mut pending = Vec::<ToolCallId>::new();
    let mut synthetic_ids = HashSet::<ToolCallId>::new();

    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                push_synthetic_results(&mut out, &mut pending, &mut synthetic_ids);
                for part in &assistant.content {
                    if let AssistantPart::ToolCall(call) = part {
                        pending.push(call.id.clone());
                    }
                }
                out.push(Message::Assistant(assistant));
            }
            Message::User(mut user) => {
                user.content.retain(|part| {
                    let UserPart::ToolResult(result) = part else {
                        return true;
                    };
                    if synthetic_ids.contains(&result.tool_call_id) {
                        // A result arriving after a later assistant turn can no
                        // longer satisfy the original protocol position. Keep
                        // the synthetic result and avoid emitting a duplicate.
                        return false;
                    }
                    pending.retain(|id| id != &result.tool_call_id);
                    true
                });
                if !user.content.is_empty() {
                    out.push(Message::User(user));
                }
            }
        }
    }
    push_synthetic_results(&mut out, &mut pending, &mut synthetic_ids);
    out
}

fn push_synthetic_results(
    out: &mut Vec<Message>,
    pending: &mut Vec<ToolCallId>,
    synthetic_ids: &mut HashSet<ToolCallId>,
) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending)
        .into_iter()
        .map(|tool_call_id| {
            synthetic_ids.insert(tool_call_id.clone());
            UserPart::ToolResult(ToolResult {
                tool_call_id,
                content: vec![ToolResultPart::Text(MISSING_TOOL_RESULT.to_string())],
                is_error: true,
            })
        })
        .collect();
    out.push(Message::User(UserMessage { content }));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::types::{
        Capabilities, Endpoint, EndpointId, ImageMedia, ModalitySet, ModelId, ModelLimits,
        ModelSpec, Protocol, ReasoningPart, ReasoningState, ReasoningStateKind, ToolCall,
    };

    fn model(id: &str, protocol: Protocol, images: bool) -> Model {
        let input_modalities = if images {
            ModalitySet::none().with(Modality::Image)
        } else {
            ModalitySet::none()
        };
        Model {
            spec: Arc::new(ModelSpec {
                id: ModelId(id.to_string()),
                endpoint: EndpointId("test".to_string()),
                api_name: id.to_string(),
                display_name: None,
                protocol,
                capabilities: Capabilities {
                    input_modalities,
                    output_modalities: ModalitySet::none(),
                    tools: true,
                    parallel_tool_calls: true,
                    reasoning: None,
                    structured_output: false,
                },
                limits: ModelLimits {
                    context_window: 16_384,
                    max_output_tokens: 4096,
                },
                pricing: None,
                cache: Default::default(),
            }),
            endpoint: Arc::new(Endpoint {
                id: EndpointId("test".to_string()),
                base_url: url::Url::parse("https://example.invalid/v1/").unwrap(),
                auth: crate::Auth::none(),
                default_headers: http::HeaderMap::new(),
                transport: crate::types::EndpointTransport::Http,
                timeout: std::time::Duration::from_secs(1),
            }),
        }
    }

    fn call(id: &str) -> AssistantPart {
        AssistantPart::ToolCall(ToolCall {
            id: ToolCallId(id.to_string()),
            name: "read".to_string(),
            arguments_json: "{}".to_string(),
        })
    }

    #[test]
    fn cross_model_reasoning_becomes_text_but_empty_and_redacted_are_dropped() {
        let messages = vec![Message::Assistant(AssistantMessage {
            model: ModelId("source".to_string()),
            protocol: Protocol::AnthropicMessages,
            content: vec![
                AssistantPart::Reasoning(ReasoningPart {
                    text: Some("use the cache".to_string()),
                    state: None,
                }),
                AssistantPart::Reasoning(ReasoningPart {
                    text: Some("  ".to_string()),
                    state: None,
                }),
                AssistantPart::Reasoning(ReasoningPart {
                    text: None,
                    state: Some(ReasoningState {
                        protocol: Protocol::AnthropicMessages,
                        model: ModelId("source".to_string()),
                        kind: ReasoningStateKind::AnthropicRedacted {
                            data: "opaque".to_string(),
                        },
                    }),
                }),
            ],
        })];

        let transformed = transform_messages(
            &messages,
            &model("target", Protocol::OpenAiResponses, false),
        );
        let Message::Assistant(assistant) = &transformed[0] else {
            panic!("expected assistant")
        };
        assert_eq!(assistant.content.len(), 1);
        assert!(matches!(
            &assistant.content[0],
            AssistantPart::Text(text) if text == "use the cache"
        ));
        // The canonical source is immutable.
        let Message::Assistant(source) = &messages[0] else {
            unreachable!()
        };
        assert_eq!(source.content.len(), 3);
    }

    #[test]
    fn same_model_reasoning_state_is_preserved() {
        let state = ReasoningState {
            protocol: Protocol::AnthropicMessages,
            model: ModelId("same".to_string()),
            kind: ReasoningStateKind::AnthropicSignature {
                signature: "sig".to_string(),
            },
        };
        let messages = vec![Message::Assistant(AssistantMessage {
            model: ModelId("same".to_string()),
            protocol: Protocol::AnthropicMessages,
            content: vec![AssistantPart::Reasoning(ReasoningPart {
                text: Some("thought".to_string()),
                state: Some(state),
            })],
        })];
        let transformed = transform_messages(
            &messages,
            &model("same", Protocol::AnthropicMessages, false),
        );
        assert!(matches!(
            &transformed[0],
            Message::Assistant(AssistantMessage { content, .. })
                if matches!(&content[0], AssistantPart::Reasoning(_))
        ));
    }

    #[test]
    fn unsupported_user_and_tool_images_become_visible_placeholders() {
        let image = Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from_static(b"image")),
            media_type: Some(mime::IMAGE_PNG),
            detail: None,
        });
        let messages = vec![Message::User(UserMessage {
            content: vec![
                UserPart::Media(image.clone()),
                UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("call_1".to_string()),
                    content: vec![ToolResultPart::Media(image)],
                    is_error: false,
                }),
            ],
        })];
        let transformed =
            transform_messages(&messages, &model("text-only", Protocol::OpenAiChat, false));
        let Message::User(user) = &transformed[0] else {
            panic!("expected user")
        };
        assert!(matches!(&user.content[0], UserPart::Text(text) if text == IMAGE_PLACEHOLDER));
        let UserPart::ToolResult(result) = &user.content[1] else {
            panic!("expected result")
        };
        assert!(
            matches!(&result.content[0], ToolResultPart::Text(text) if text == TOOL_IMAGE_PLACEHOLDER)
        );
    }

    #[test]
    fn request_transform_preserves_pending_media_but_normalizes_history() {
        let image = Media::Image(ImageMedia {
            source: ImageSource::Inline(bytes::Bytes::from_static(b"image")),
            media_type: Some(mime::IMAGE_PNG),
            detail: None,
        });
        let messages = vec![
            Message::User(UserMessage {
                content: vec![UserPart::Media(image.clone())],
            }),
            Message::Assistant(AssistantMessage {
                model: ModelId("source".to_string()),
                protocol: Protocol::OpenAiResponses,
                content: vec![AssistantPart::Text("seen".to_string())],
            }),
            Message::User(UserMessage {
                content: vec![UserPart::Media(image)],
            }),
        ];

        let transformed =
            transform_request_messages(&messages, &model("text-only", Protocol::OpenAiChat, false));
        assert!(matches!(
            &transformed[0],
            Message::User(UserMessage { content })
                if matches!(&content[0], UserPart::Text(text) if text == IMAGE_PLACEHOLDER)
        ));
        assert!(matches!(
            &transformed[2],
            Message::User(UserMessage { content })
                if matches!(&content[0], UserPart::Media(_))
        ));
    }

    #[test]
    fn owned_request_transform_matches_borrowed_and_moves_pending_text() {
        let invalid_id = "call_x|item_y/invalid";
        let pending_text = String::from("large pending prompt stays owned");
        let pending_allocation = pending_text.as_ptr();
        let messages = vec![
            Message::Assistant(AssistantMessage {
                model: ModelId("source".to_string()),
                protocol: Protocol::OpenAiResponses,
                content: vec![call(invalid_id)],
            }),
            Message::User(UserMessage {
                content: vec![
                    UserPart::Text(pending_text),
                    UserPart::ToolResult(ToolResult {
                        tool_call_id: ToolCallId(invalid_id.to_string()),
                        content: vec![ToolResultPart::Text("done".to_string())],
                        is_error: false,
                    }),
                ],
            }),
        ];
        let target = model("target", Protocol::AnthropicMessages, false);
        let expected = transform_request_messages(&messages, &target);
        let actual = transform_request_messages_owned(messages, &target);
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        let Message::User(user) = &actual[1] else {
            panic!("expected pending user message")
        };
        let UserPart::Text(text) = &user.content[0] else {
            panic!("expected pending text")
        };
        assert_eq!(text.as_ptr(), pending_allocation);
    }

    #[test]
    fn tool_ids_are_normalized_and_missing_results_are_inserted_in_position() {
        let invalid_id = format!("call_{}|item_{}", "a".repeat(100), "b".repeat(100));
        let messages = vec![
            Message::Assistant(AssistantMessage {
                model: ModelId("source".to_string()),
                protocol: Protocol::OpenAiResponses,
                content: vec![call(&invalid_id)],
            }),
            Message::Assistant(AssistantMessage {
                model: ModelId("source".to_string()),
                protocol: Protocol::OpenAiResponses,
                content: vec![AssistantPart::Text("continued".to_string())],
            }),
        ];
        let transformed = transform_messages(
            &messages,
            &model("target", Protocol::AnthropicMessages, false),
        );
        assert_eq!(transformed.len(), 3);
        let Message::Assistant(first) = &transformed[0] else {
            panic!("expected assistant")
        };
        let AssistantPart::ToolCall(call) = &first.content[0] else {
            panic!("expected call")
        };
        assert!(call.id.0.len() <= 64);
        assert!(call
            .id
            .0
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'));

        let Message::User(result_message) = &transformed[1] else {
            panic!("expected synthetic result")
        };
        let UserPart::ToolResult(result) = &result_message.content[0] else {
            panic!("expected result")
        };
        assert_eq!(result.tool_call_id, call.id);
        assert!(result.is_error);
        assert!(
            matches!(&result.content[0], ToolResultPart::Text(text) if text == MISSING_TOOL_RESULT)
        );
    }

    #[test]
    fn existing_tool_result_prevents_synthetic_result() {
        let messages = vec![
            Message::Assistant(AssistantMessage {
                model: ModelId("source".to_string()),
                protocol: Protocol::OpenAiResponses,
                content: vec![call("call_1")],
            }),
            Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("call_1".to_string()),
                    content: vec![ToolResultPart::Text("ok".to_string())],
                    is_error: false,
                })],
            }),
        ];
        let transformed = transform_messages(
            &messages,
            &model("target", Protocol::AnthropicMessages, false),
        );
        assert_eq!(transformed.len(), 2);
    }
}
