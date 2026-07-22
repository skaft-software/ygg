#![allow(missing_docs)]

use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use ygg_ai::{
    AiClient, Auth, Capabilities, CompatibilityMode::Strict, Endpoint, EndpointId, Message,
    ModalitySet, Model, ModelId, ModelLimits, ModelSpec, OutputFormat, OutputModalities, Protocol,
    Request, StopReason, UserMessage, UserPart,
};

fn make_test_model(base_url_str: &str, protocol: Protocol) -> Model {
    let spec = ModelSpec {
        id: ModelId("test-model".to_string()),
        endpoint: EndpointId("test-ep".to_string()),
        api_name: "gpt-4-test".to_string(),
        display_name: None,
        protocol,
        capabilities: Capabilities {
            input_modalities: ModalitySet::none(),
            output_modalities: ModalitySet::none(),
            tools: false,
            parallel_tool_calls: false,
            reasoning: None,
            structured_output: false,
        },
        limits: ModelLimits {
            context_window: 10000,
            max_output_tokens: 2000,
        },
        pricing: None,
        cache: ygg_ai::CacheCompatibility::default(),
    };

    let ep = Endpoint {
        id: EndpointId("test-ep".to_string()),
        base_url: url::Url::parse(base_url_str).unwrap(),
        auth: Auth::bearer("test-api-key"),
        default_headers: http::HeaderMap::new(),
        transport: ygg_ai::EndpointTransport::Http,
        timeout: Duration::from_secs(2),
    };

    Model {
        spec: Arc::new(spec),
        endpoint: Arc::new(ep),
    }
}

#[tokio::test]
async fn test_client_complete_happy_path() {
    let mock_server = MockServer::start().await;

    let sse_body = "data: {\"id\": \"chatcmpl-complete-1\", \"choices\": [{\"delta\": {\"content\": \"Full completion text\"}}]}\n\n\
                    data: {\"id\": \"chatcmpl-complete-1\", \"choices\": [{\"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n\
                    data: [DONE]\n\n";

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let client = AiClient::new();
    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat);
    let req = Request {
        system: None,
        messages: vec![Message::User(UserMessage {
            content: vec![UserPart::Text("Go".to_string())],
        })],
        tools: vec![],
        tool_choice: ygg_ai::ToolChoice::Auto,
        max_output_tokens: None,
        temperature: None,
        stop: vec![],
        reasoning: ygg_ai::ReasoningConfig::Off,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: Strict,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    };

    let response = client.complete(&model, req).await.unwrap();

    assert_eq!(
        response.response_id,
        Some("chatcmpl-complete-1".to_string())
    );
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    if let ygg_ai::types::AssistantPart::Text(ref text) = response.message.content[0] {
        assert_eq!(text, "Full completion text");
    } else {
        panic!("Expected Assistant text part");
    }
}
