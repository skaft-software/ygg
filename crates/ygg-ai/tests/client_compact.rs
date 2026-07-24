#![allow(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ygg_ai::{
    AiClient, AiError, Auth, Capabilities, Endpoint, EndpointId, ModalitySet, Model, ModelId,
    ModelLimits, ModelSpec, Protocol, ResponsesCompactRequest, ResponsesInput, ResponsesItem,
};

fn model(base_url: &str, protocol: Protocol) -> Model {
    Model {
        spec: Arc::new(ModelSpec {
            id: ModelId("compact-test".into()),
            endpoint: EndpointId("compact-endpoint".into()),
            api_name: "gpt-compact".into(),
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
                context_window: 100_000,
                max_output_tokens: 8_000,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        }),
        endpoint: Arc::new(Endpoint {
            id: EndpointId("compact-endpoint".into()),
            base_url: url::Url::parse(base_url).unwrap(),
            auth: Auth::bearer("compact-secret"),
            default_headers: http::HeaderMap::new(),
            transport: ygg_ai::EndpointTransport::Http,
            timeout: Duration::from_secs(2),
        }),
    }
}

fn input_item(value: serde_json::Value) -> ResponsesItem {
    ResponsesItem::new(value).unwrap()
}

#[tokio::test]
async fn compact_posts_exact_body_and_preserves_complete_output() {
    let server = MockServer::start().await;
    let request_input = ResponsesInput::new(vec![input_item(serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "compact me"}],
        "unknown_input": true
    }))]);
    let expected_body = serde_json::json!({
        "model": "gpt-compact",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "compact me"}],
            "unknown_input": true
        }],
        "instructions": "retain exact state"
    });
    Mock::given(method("POST"))
        .and(path("/responses/compact"))
        .and(header("authorization", "Bearer compact-secret"))
        .and(header("content-type", "application/json"))
        .and(body_json(expected_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [
                {"type": "message", "id": "before", "unknown": {"a": 1}},
                {"type": "compaction", "id": "cmp", "encrypted_content": "opaque", "phase": "analysis"},
                {"type": "message", "id": "after", "future_field": [1, 2, 3]}
            ],
            "usage": {
                "input_tokens": 120,
                "output_tokens": 20,
                "input_tokens_details": {"cached_tokens": 10, "cache_write_tokens": 5},
                "output_tokens_details": {"reasoning_tokens": 7}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let response = AiClient::new()
        .compact_responses(
            &model(&format!("{}/", server.uri()), Protocol::OpenAiResponses),
            ResponsesCompactRequest {
                model: "gpt-compact".into(),
                input: request_input,
                instructions: Some("retain exact state".into()),
            },
        )
        .await
        .unwrap();

    assert_eq!(response.output.items().len(), 3);
    assert_eq!(response.output.items()[0].as_json()["unknown"]["a"], 1);
    assert_eq!(
        response.output.items()[1].as_json()["encrypted_content"],
        "opaque"
    );
    assert_eq!(response.output.items()[2].as_json()["future_field"][2], 3);
    assert_eq!(response.usage.input_tokens, 105);
    assert_eq!(response.usage.cache_read_tokens, 10);
    assert_eq!(response.usage.cache_write_tokens, 5);
    assert_eq!(response.usage.output_tokens, 20);
    assert_eq!(response.usage.reasoning_tokens, 7);
}

#[tokio::test]
async fn compact_rejects_non_responses_routes_before_http() {
    let server = MockServer::start().await;
    let error = AiClient::new()
        .compact_responses(
            &model(&format!("{}/", server.uri()), Protocol::OpenAiChat),
            ResponsesCompactRequest {
                model: "gpt-compact".into(),
                input: ResponsesInput::default(),
                instructions: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AiError::Unsupported(_)));
    assert!(server.received_requests().await.unwrap().is_empty());
}
