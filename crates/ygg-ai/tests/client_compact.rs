#![allow(missing_docs)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ygg_ai::{
    AgentDelegation, AiClient, AiError, Auth, CacheRetention, Capabilities, DecodeError, Endpoint,
    EndpointId, ModalitySet, Model, ModelId, ModelLimits, ModelSpec, OpenAiChatReasoningMode,
    OutputFormat, Protocol, ReasoningCapability, ReasoningConfig, ReasoningControl,
    ReasoningEffort, ReasoningMode, ResponsesCompactRequest, ResponsesInput, ResponsesItem,
    ToolDef, TransportPhase,
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
                responses_lite: false,
                agent_delegation: None,
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

fn codex_model(base_url: &str) -> Model {
    let mut model = model(base_url, Protocol::OpenAiResponses);
    let mut spec = (*model.spec).clone();
    spec.endpoint = EndpointId("openai-codex".into());
    spec.cache.session_affinity_format = Some(ygg_ai::SessionAffinityFormat::Codex);
    model.spec = Arc::new(spec);
    let mut endpoint = (*model.endpoint).clone();
    endpoint.id = EndpointId("openai-codex".into());
    model.endpoint = Arc::new(endpoint);
    model
}

fn responses_lite_model(base_url: &str) -> Model {
    let mut model = model(base_url, Protocol::OpenAiResponses);
    let spec = Arc::make_mut(&mut model.spec);
    spec.capabilities.tools = true;
    spec.capabilities.parallel_tool_calls = true;
    spec.capabilities.responses_lite = true;
    spec.capabilities.agent_delegation = Some(AgentDelegation::V2);
    spec.capabilities.reasoning = Some(ReasoningCapability {
        control: ReasoningControl::Effort,
        exposes_text: true,
        preserves_state: true,
        effort_budgets: None,
        openai_chat_mode: OpenAiChatReasoningMode::Standard,
        min_effort: ReasoningEffort::Minimal,
        max_effort: ReasoningEffort::Ultra,
    });
    model
}

fn input_item(value: serde_json::Value) -> ResponsesItem {
    ResponsesItem::new(value).unwrap()
}

fn compact_request(input: ResponsesInput, instructions: Option<&str>) -> ResponsesCompactRequest {
    ResponsesCompactRequest {
        model: "gpt-compact".into(),
        input,
        instructions: instructions.map(str::to_owned),
        tools: None,
        parallel_tool_calls: None,
        reasoning: None,
        text: None,
        prompt_cache_key: None,
        session_id: None,
    }
}

fn drain_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let (body_start, content_length) = loop {
        let read = stream.read(&mut buffer).unwrap();
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..body_start]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        break (body_start, content_length);
    };
    while request.len().saturating_sub(body_start) < content_length {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
}

fn chunked_compact_server(stall_after_first_chunk: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        drain_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        if stall_after_first_chunk {
            stream.write_all(b"1\r\n{\r\n").unwrap();
            std::thread::sleep(Duration::from_secs(1));
            return;
        }

        let chunk = vec![b'x'; 1024 * 1024];
        // Sixty-five 1 MiB chunks cross the production 64 MiB response cap
        // without a Content-Length preflight.
        for _ in 0..65 {
            if stream.write_all(b"100000\r\n").is_err()
                || stream.write_all(&chunk).is_err()
                || stream.write_all(b"\r\n").is_err()
            {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    format!("http://{address}/")
}

#[tokio::test]
async fn compact_codex_posts_rich_exact_body_and_preserves_complete_output() {
    let server = MockServer::start().await;
    let request_input = ResponsesInput::new(vec![
        input_item(serde_json::json!({
            "type": "function_call",
            "call_id": "call_exact",
            "name": "read",
            "arguments": "{\"path\":\"AGENTS.md\"}",
            "unknown_input": true
        })),
        input_item(serde_json::json!({
            "type": "function_call_output",
            "call_id": "call_exact",
            "output": "instructions"
        })),
    ]);
    let expected_body = serde_json::json!({
        "model": "gpt-compact",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_exact",
                "name": "read",
                "arguments": "{\"path\":\"AGENTS.md\"}",
                "unknown_input": true
            },
            {
                "type": "function_call_output",
                "call_id": "call_exact",
                "output": "instructions"
            }
        ],
        "instructions": "retain exact state",
        "tools": [{
            "type": "function",
            "name": "read",
            "description": "Read a file",
            "parameters": {"type": "object"}
        }],
        "parallel_tool_calls": false,
        "reasoning": {"effort": "high", "summary": "auto"},
        "text": {"verbosity": "low"},
        "prompt_cache_key": "session-exact"
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
            &codex_model(&format!("{}/", server.uri())),
            ResponsesCompactRequest {
                tools: Some(vec![serde_json::json!({
                    "type": "function",
                    "name": "read",
                    "description": "Read a file",
                    "parameters": {"type": "object"}
                })]),
                parallel_tool_calls: Some(false),
                reasoning: Some(serde_json::json!({"effort": "high", "summary": "auto"})),
                text: Some(serde_json::json!({"verbosity": "low"})),
                prompt_cache_key: Some("session-exact".into()),
                session_id: None,
                ..compact_request(request_input, Some("retain exact state"))
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

#[test]
fn compact_for_model_clamps_reasoning_to_the_advertised_range() {
    let mut model = responses_lite_model("https://example.com/");
    Arc::make_mut(&mut model.spec)
        .capabilities
        .reasoning
        .as_mut()
        .unwrap()
        .max_effort = ReasoningEffort::High;

    let request = ResponsesCompactRequest::for_model(
        &model,
        ResponsesInput::default(),
        None,
        &[],
        &ReasoningConfig::Effort(ReasoningEffort::Ultra),
        ReasoningMode::Standard,
        &OutputFormat::Text,
        CacheRetention::None,
        None,
    );

    assert_eq!(request.reasoning.unwrap()["effort"], "high");
}

#[tokio::test]
async fn compact_responses_lite_uses_advertised_transport_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/compact"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{"type": "compaction", "encrypted_content": "opaque"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let model = responses_lite_model(&format!("{}/", server.uri()));
    let input = ResponsesInput::new(vec![input_item(serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_image",
            "image_url": "data:image/png;base64,eA==",
            "detail": "high"
        }]
    }))]);
    let request = ResponsesCompactRequest::for_model(
        &model,
        input,
        Some("current instructions".into()),
        &[ToolDef {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }],
        &ReasoningConfig::Effort(ReasoningEffort::Ultra),
        ReasoningMode::Standard,
        &OutputFormat::Text,
        CacheRetention::Short,
        None,
    );

    AiClient::new()
        .compact_responses(&model, request)
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let request = &requests[0];
    assert_eq!(
        request.headers["x-openai-internal-codex-responses-lite"],
        "true"
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body.get("instructions").is_none());
    assert!(body.get("tools").is_none());
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning"]["effort"], "max");
    assert_eq!(body["reasoning"]["context"], "all_turns");
    assert_eq!(body["input"][0]["type"], "additional_tools");
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["input"][0]["tools"][0]["type"], "namespace");
    assert_eq!(body["input"][0]["tools"][0]["name"], "functions");
    assert_eq!(body["input"][0]["tools"][0]["tools"][0]["name"], "read");
    assert_eq!(body["input"][1]["role"], "developer");
    assert_eq!(
        body["input"][1]["content"][0]["text"],
        "current instructions"
    );
    assert_eq!(body["input"][2]["role"], "user");
    assert!(body["input"][2]["content"][0].get("detail").is_none());
}

#[tokio::test]
async fn compact_public_route_uses_the_narrow_public_body() {
    let server = MockServer::start().await;
    let input = ResponsesInput::new(vec![input_item(serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "compact me"}]
    }))]);
    Mock::given(method("POST"))
        .and(path("/responses/compact"))
        .and(body_json(serde_json::json!({
            "model": "gpt-compact",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "compact me"}]
            }],
            "instructions": "public instructions",
            "prompt_cache_key": "public-session"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{"type": "compaction", "encrypted_content": "opaque"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = compact_request(input, Some("public instructions"));
    request.tools = Some(vec![
        serde_json::json!({"type": "function", "name": "read"}),
    ]);
    request.parallel_tool_calls = Some(true);
    request.reasoning = Some(serde_json::json!({"effort": "high"}));
    request.text = Some(serde_json::json!({"verbosity": "low"}));
    request.prompt_cache_key = Some("public-session".into());

    AiClient::new()
        .compact_responses(
            &model(&format!("{}/", server.uri()), Protocol::OpenAiResponses),
            request,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn compact_rejects_oversized_chunked_body_while_streaming() {
    let error = AiClient::new()
        .with_stream_timeouts(Duration::from_secs(2), Duration::from_secs(10))
        .compact_responses(
            &model(&chunked_compact_server(false), Protocol::OpenAiResponses),
            compact_request(ResponsesInput::default(), None),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AiError::Decode(DecodeError::BodyTooLarge)));
}

#[tokio::test]
async fn compact_response_header_timeout_is_classified_separately() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/compact"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(100)))
        .expect(1)
        .mount(&server)
        .await;

    let mut model = model(&format!("{}/", server.uri()), Protocol::OpenAiResponses);
    Arc::make_mut(&mut model.endpoint).timeout = Duration::from_millis(10);
    let error = AiClient::new()
        .compact_responses(&model, compact_request(ResponsesInput::default(), None))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AiError::Transport(ref transport)
            if transport.phase == TransportPhase::ResponseHeaders && transport.timeout
    ));
}

#[tokio::test]
async fn compact_stalled_body_obeys_the_idle_timeout() {
    let error = AiClient::new()
        .with_stream_timeouts(Duration::from_millis(25), Duration::from_secs(2))
        .compact_responses(
            &model(&chunked_compact_server(true), Protocol::OpenAiResponses),
            compact_request(ResponsesInput::default(), None),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AiError::Transport(ref transport)
            if transport.phase == TransportPhase::Body && transport.timeout
    ));
}

#[tokio::test]
async fn compact_rejects_non_responses_routes_before_http() {
    let server = MockServer::start().await;
    let error = AiClient::new()
        .compact_responses(
            &model(&format!("{}/", server.uri()), Protocol::OpenAiChat),
            compact_request(ResponsesInput::default(), None),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, AiError::Unsupported(_)));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn compact_codex_route_sends_plain_json_and_affinity_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/compact"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{"type": "compaction", "encrypted_content": "opaque"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let model = codex_model(&format!("{}/", server.uri()));

    let mut request = compact_request(
        ResponsesInput::new(vec![input_item(serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "x".repeat(64 * 1024)}]
        }))]),
        Some("current instructions"),
    );
    request.session_id = Some("stable-session".into());
    request.prompt_cache_key = Some("stable-session".into());
    AiClient::new()
        .compact_responses(&model, request)
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let request = &requests[0];
    assert!(!request.headers.contains_key("content-encoding"));
    assert_eq!(request.headers["session-id"], "stable-session");
    assert_eq!(request.headers["x-client-request-id"], "stable-session");
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["prompt_cache_key"], "stable-session");
    assert!(body.get("session_id").is_none());
}

#[tokio::test]
async fn compact_http_errors_preserve_retry_metadata_and_taxonomy() {
    for (status, retryable) in [(400, false), (429, true), (503, true)] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses/compact"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("x-request-id", "req-compact")
                    .insert_header("retry-after", "7")
                    .set_body_json(serde_json::json!({
                        "error": {"code": "compact_error", "message": "nope"}
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let error = AiClient::new()
            .compact_responses(
                &model(&format!("{}/", server.uri()), Protocol::OpenAiResponses),
                compact_request(ResponsesInput::default(), None),
            )
            .await
            .unwrap_err();
        let AiError::Http(error) = error else {
            panic!("expected structured HTTP error");
        };
        assert_eq!(error.status.as_u16(), status);
        assert_eq!(error.request_id.as_deref(), Some("req-compact"));
        assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(error.provider_code.as_deref(), Some("compact_error"));
        assert_eq!(error.retryable, retryable);
    }
}
