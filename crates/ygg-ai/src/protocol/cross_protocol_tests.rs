use std::sync::Arc;
use url::Url;

use crate::{
    AssistantMessage, AssistantPart, Auth, Capabilities, CompatibilityMode::Lossy,
    CompatibilityMode::Strict, Endpoint, EndpointId, ImageMedia, ImageSource, Media, Message,
    Modality, ModalitySet, Model, ModelId, ModelLimits, ModelSpec, OutputFormat, OutputModalities,
    Protocol, ReasoningCapability, ReasoningConfig, ReasoningControl, ReasoningEffortBudgets,
    ReasoningPart, ReasoningState, ReasoningStateKind, Request, ToolCall, ToolCallId, ToolChoice,
    ToolResult, ToolResultPart, UserMessage, UserPart,
};

fn make_model(
    protocol: Protocol,
    image: bool,
    audio_in: bool,
    audio_out: bool,
    reasoning: bool,
) -> Model {
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

    let spec = ModelSpec {
        id: ModelId(format!("model-{:?}", protocol)),
        endpoint: EndpointId("ep-1".to_string()),
        api_name: "test-model".to_string(),
        protocol,
        capabilities: Capabilities {
            input_modalities: input,
            output_modalities: output,
            tools: true,
            parallel_tool_calls: true,
            reasoning: if reasoning {
                Some(ReasoningCapability {
                    control: if protocol == Protocol::AnthropicMessages {
                        ReasoningControl::TokenBudget
                    } else {
                        ReasoningControl::Effort
                    },
                    exposes_text: true,
                    preserves_state: true,
                    effort_budgets: if protocol == Protocol::AnthropicMessages {
                        Some(ReasoningEffortBudgets {
                            minimal: 1024,
                            low: 2048,
                            medium: 4096,
                            high: 8192,
                        })
                    } else {
                        None
                    },
                    openai_chat_mode: crate::OpenAiChatReasoningMode::Standard,
                })
            } else {
                None
            },
            structured_output: true,
        },
        limits: ModelLimits {
            context_window: 10000,
            max_output_tokens: 8192,
        },
        pricing: None,
        cache: crate::types::CacheCompatibility::default(),
    };

    let ep = Endpoint {
        id: EndpointId("ep-1".to_string()),
        base_url: Url::parse("https://api.provider.com/v1/").unwrap(),
        auth: Auth::none(),
        default_headers: http::HeaderMap::new(),
        timeout: std::time::Duration::from_secs(10),
    };

    Model {
        spec: Arc::new(spec),
        endpoint: Arc::new(ep),
    }
}

#[test]
fn test_cross_protocol_canonical_immutability() {
    let model = make_model(Protocol::OpenAiChat, true, true, false, true);

    let image = Media::Image(ImageMedia {
        source: ImageSource::Inline(bytes::Bytes::from(vec![1, 2, 3])),
        media_type: Some(mime::IMAGE_PNG),
        detail: None,
    });

    let tool_call = ToolCall {
        id: ToolCallId("call_1".to_string()),
        name: "test_tool".to_string(),
        arguments_json: "{}".to_string(),
    };

    let tool_result = ToolResult {
        tool_call_id: ToolCallId("call_1".to_string()),
        content: vec![ToolResultPart::Text("done".to_string())],
        is_error: false,
    };

    let assistant_reasoning = AssistantPart::Reasoning(ReasoningPart {
        text: Some("Thinking".to_string()),
        state: None,
    });

    let req = Request {
        system: Some("System prompt".to_string()),
        messages: vec![
            Message::User(UserMessage {
                content: vec![UserPart::Text("Hello".to_string()), UserPart::Media(image)],
            }),
            Message::Assistant(AssistantMessage {
                content: vec![assistant_reasoning, AssistantPart::ToolCall(tool_call)],
                model: ModelId("model-OpenAiChat".to_string()),
                protocol: Protocol::OpenAiChat,
            }),
            Message::User(UserMessage {
                content: vec![UserPart::ToolResult(tool_result)],
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
        compatibility: Strict,
        cache_retention: crate::types::CacheRetention::Short,
        session_id: None,
    };

    // Serialize to Chat Completions
    let parts_chat = crate::protocol::openai_chat::build_request(&model, &req).unwrap();
    assert!(!parts_chat.body.is_empty());

    // Verify req is unmodified
    assert_eq!(req.system, Some("System prompt".to_string()));
    assert_eq!(req.messages.len(), 3);
}

#[test]
fn test_cross_protocol_anthropic_message_merging() {
    let model = make_model(Protocol::AnthropicMessages, true, false, false, false);

    let req = Request {
        system: None,
        messages: vec![
            Message::User(UserMessage {
                content: vec![UserPart::Text("First user message".to_string())],
            }),
            Message::User(UserMessage {
                content: vec![UserPart::Text(
                    "Second consecutive user message".to_string(),
                )],
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
        compatibility: Strict,
        cache_retention: crate::types::CacheRetention::Short,
        session_id: None,
    };

    let parts = crate::protocol::anthropic::build_request(&model, &req).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&parts.body).unwrap();

    // Consecutive user messages must be merged into one User message
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn test_lossy_inserts_missing_tool_result_before_next_assistant() {
    for protocol in [Protocol::OpenAiResponses, Protocol::AnthropicMessages] {
        let model = make_model(protocol, false, false, false, false);
        let req = Request {
            system: None,
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("call_missing".to_string()),
                        name: "lookup".to_string(),
                        arguments_json: "{}".to_string(),
                    })],
                    model: model.spec.id.clone(),
                    protocol,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::Text("continued".to_string())],
                    model: model.spec.id.clone(),
                    protocol,
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
            compatibility: Lossy,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };
        let body: serde_json::Value = match protocol {
            Protocol::OpenAiResponses => serde_json::from_slice(
                &crate::protocol::openai_responses::build_request(&model, &req)
                    .unwrap()
                    .body,
            )
            .unwrap(),
            Protocol::AnthropicMessages => serde_json::from_slice(
                &crate::protocol::anthropic::build_request(&model, &req)
                    .unwrap()
                    .body,
            )
            .unwrap(),
            Protocol::OpenAiChat => unreachable!(),
        };
        let serialized = body.to_string();
        assert!(serialized.contains("call_missing"));
        assert!(serialized.contains("Tool execution result was not supplied"));
    }
}

#[test]
fn test_cross_protocol_reasoning_state_rejection() {
    // OpenAI model
    let model_openai = make_model(Protocol::OpenAiChat, false, false, false, true);

    // Reasoning state from Anthropic (cross-protocol/model mismatch)
    let state = ReasoningState {
        model: ModelId("claude-model".to_string()),
        protocol: Protocol::AnthropicMessages,
        kind: ReasoningStateKind::AnthropicSignature {
            signature: "sig_abc".to_string(),
        },
    };

    let req = Request {
        system: None,
        messages: vec![
            Message::User(UserMessage {
                content: vec![UserPart::Text("Hello".to_string())],
            }),
            Message::Assistant(AssistantMessage {
                content: vec![
                    AssistantPart::Reasoning(ReasoningPart {
                        text: Some("Thinking".to_string()),
                        state: Some(state),
                    }),
                    AssistantPart::Text("Hi".to_string()),
                ],
                model: ModelId("claude-model".to_string()),
                protocol: Protocol::AnthropicMessages,
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
        compatibility: Strict,
        cache_retention: crate::types::CacheRetention::Short,
        session_id: None,
    };

    // In Strict mode, it should be rejected
    let res_strict = crate::protocol::openai_chat::build_request(&model_openai, &req);
    assert!(
        res_strict.is_err(),
        "Expected Strict mode rejection of cross-model reasoning state"
    );

    // In Lossy mode, it should compile successfully and drop the mismatched state
    let mut req_lossy = req.clone();
    req_lossy.compatibility = Lossy;
    let res_lossy = crate::protocol::openai_chat::build_request(&model_openai, &req_lossy);
    assert!(
        res_lossy.is_ok(),
        "Expected Lossy mode to drop state and pass"
    );
}
