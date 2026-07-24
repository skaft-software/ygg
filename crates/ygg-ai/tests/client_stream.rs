#![allow(missing_docs)]

use base64::Engine;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ygg_ai::{
    AiClient, AiError, Auth, Capabilities, CompatibilityMode::Strict, Endpoint, EndpointId, Media,
    Message, Modality, ModalitySet, Model, ModelId, ModelLimits, ModelSpec, OutputFormat,
    OutputModalities, Protocol, Request, StreamEvent, UserMessage, UserPart,
};

fn make_test_model(base_url_str: &str, protocol: Protocol, is_audio: bool) -> Model {
    let spec = ModelSpec {
        id: ModelId("test-model".to_string()),
        endpoint: EndpointId("test-ep".to_string()),
        api_name: "gpt-4-test".to_string(),
        display_name: None,
        protocol,
        capabilities: Capabilities {
            input_modalities: ModalitySet::none().with(Modality::Image),
            output_modalities: if is_audio {
                ModalitySet::none().with(Modality::Audio)
            } else {
                ModalitySet::none()
            },
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
async fn test_client_stream_sse_openai_chat() {
    let mock_server = MockServer::start().await;

    // Stub OpenAI Chat streaming SSE response
    let sse_body = "data: {\"id\": \"chatcmpl-1\", \"choices\": [{\"delta\": {\"content\": \"Hello\"}}]}\n\n\
                    data: {\"id\": \"chatcmpl-1\", \"choices\": [{\"delta\": {\"content\": \" world\"}}]}\n\n\
                    data: {\"id\": \"chatcmpl-1\", \"choices\": [{\"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n\
                    data: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("chat/completions"))
        .and(header_exists("authorization"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let client = AiClient::new();
    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let req = Request {
        system: None,
        messages: vec![Message::User(UserMessage {
            content: vec![UserPart::Text("Hi".to_string())],
        })],
        tools: vec![],
        tool_choice: ygg_ai::ToolChoice::Auto,
        max_output_tokens: None,
        temperature: None,
        stop: vec![],
        reasoning: ygg_ai::ReasoningConfig::Off,
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: Strict,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    };

    let mut stream = client.stream(&model, req).await.unwrap();

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }

    assert!(events.len() >= 4);
    assert!(matches!(events[0], StreamEvent::Started { .. }));
    assert!(matches!(events[1], StreamEvent::TextStart { .. }));
    if let StreamEvent::TextDelta { ref delta, .. } = events[2] {
        assert_eq!(delta, "Hello");
    } else {
        panic!("Expected TextDelta Hello");
    }
}

// f1: the client stops reading the HTTP body the instant the codec emits
// `Finished`. Any frames after the terminal `[DONE]` must be ignored, not
// decoded into post-terminal events (design §8 "No events after Finished").
#[tokio::test]
async fn test_client_stops_reading_after_terminal_event() {
    let mock_server = MockServer::start().await;

    // A well-formed extra text delta *after* `[DONE]`. If the read loop kept
    // going it would decode into a post-terminal `TextDelta`, tripping the
    // guard's `EventAfterFinish`. With the short-circuit it is never read.
    let sse_body = "data: {\"id\": \"chatcmpl-1\", \"choices\": [{\"delta\": {\"content\": \"Hello\"}}]}\n\n\
                    data: {\"id\": \"chatcmpl-1\", \"choices\": [{\"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n\
                    data: [DONE]\n\n\
                    data: {\"id\": \"chatcmpl-1\", \"choices\": [{\"delta\": {\"content\": \"LEAKED\"}}]}\n\n";

    Mock::given(method("POST"))
        .and(path("chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let client = AiClient::new();
    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let mut stream = client.stream(&model, text_request()).await.unwrap();

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("stream must end cleanly, not error after terminal"));
    }

    assert!(
        matches!(events.last(), Some(StreamEvent::Finished(_))),
        "Finished must be the last event"
    );
    assert!(
        !events.iter().any(|ev| matches!(
            ev,
            StreamEvent::TextDelta { delta, .. } if delta == "LEAKED"
        )),
        "post-terminal frame must not be decoded"
    );
}

#[tokio::test]
async fn test_client_stream_non_streaming_chat_audio() {
    let mock_server = MockServer::start().await;

    // base64 for "wav_payload"
    let mock_audio_base64 = base64::prelude::BASE64_STANDARD.encode(b"RIFFmockwavcontent");

    let completed_json = format!(
        r#"{{
            "id": "chatcmpl-audio-1",
            "choices": [{{
                "message": {{
                    "role": "assistant",
                    "content": "Transcription response",
                    "audio": {{
                        "id": "audio_123",
                        "data": "{}",
                        "transcript": "Transcription response",
                        "expires_at": 1800000000
                    }}
                }},
                "finish_reason": "stop"
            }}],
            "usage": {{
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20
            }}
        }}"#,
        mock_audio_base64
    );

    Mock::given(method("POST"))
        .and(path("chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(completed_json)
                .insert_header("content-type", "application/json"),
        )
        .mount(&mock_server)
        .await;

    let client = AiClient::new();
    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, true);
    let req = Request {
        system: None,
        messages: vec![Message::User(UserMessage {
            content: vec![UserPart::Text("Speak".to_string())],
        })],
        tools: vec![],
        tool_choice: ygg_ai::ToolChoice::Auto,
        max_output_tokens: None,
        temperature: None,
        stop: vec![],
        reasoning: ygg_ai::ReasoningConfig::Off,
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::TextAndAudio(ygg_ai::AudioOutputOptions {
            format: ygg_ai::AudioFormat::Wav,
            voice: ygg_ai::AudioVoice::Named("alloy".to_string()),
        }),
        compatibility: Strict,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    };

    let mut stream = client.stream(&model, req).await.unwrap();

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }

    // Started, TextStart, TextDelta, TextEnd, MediaCompleted, Usage, Finished
    assert!(events.len() >= 6);
    assert!(matches!(events[0], StreamEvent::Started { .. }));

    let has_media = events
        .iter()
        .any(|ev| matches!(ev, StreamEvent::MediaCompleted { .. }));
    assert!(has_media, "Expected a MediaCompleted event");
}

#[tokio::test]
async fn test_client_stream_openai_responses() {
    let mock_server = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.content_part.added\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n",
        "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"content_index\":0}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("responses"))
        .and(wiremock::matchers::header(
            "content-type",
            "application/json",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&mock_server)
        .await;

    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiResponses, false);
    let req = text_request();
    let response = AiClient::new().complete(&model, req).await.unwrap();
    assert_eq!(response.response_id.as_deref(), Some("resp_1"));
}

#[tokio::test]
async fn test_client_stream_anthropic() {
    let mock_server = MockServer::start().await;
    let body = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("messages"))
        .and(wiremock::matchers::header(
            "content-type",
            "application/json",
        ))
        .and(wiremock::matchers::header(
            "anthropic-version",
            "2023-06-01",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&mock_server)
        .await;

    let model = make_test_model(&mock_server.uri(), Protocol::AnthropicMessages, false);
    let response = AiClient::new()
        .complete(&model, text_request())
        .await
        .unwrap();
    assert_eq!(response.response_id.as_deref(), Some("msg_1"));
}

fn text_request() -> Request {
    Request {
        system: None,
        messages: vec![Message::User(UserMessage {
            content: vec![UserPart::Text("Hi".to_string())],
        })],
        tools: vec![],
        tool_choice: ygg_ai::ToolChoice::Auto,
        max_output_tokens: None,
        temperature: None,
        stop: vec![],
        reasoning: ygg_ai::ReasoningConfig::Off,
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: Strict,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    }
}

#[tokio::test]
async fn test_client_stream_http_error_handling() {
    let mock_server = MockServer::start().await;

    let error_json = r#"{"error": {"type": "invalid_request_error", "message": "Failed parameters", "code": "invalid_val"}}"#;

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(error_json)
                .insert_header("x-request-id", "req-abc-123"),
        )
        .mount(&mock_server)
        .await;

    let client = AiClient::new();
    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let req = Request {
        system: None,
        messages: vec![Message::User(UserMessage {
            content: vec![UserPart::Text("Hi".to_string())],
        })],
        tools: vec![],
        tool_choice: ygg_ai::ToolChoice::Auto,
        max_output_tokens: None,
        temperature: None,
        stop: vec![],
        reasoning: ygg_ai::ReasoningConfig::Off,
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: Strict,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    };

    let res = client.stream(&model, req).await;
    assert!(res.is_err());
    if let Err(ygg_ai::error::AiError::Http(http_err)) = res {
        assert_eq!(http_err.status.as_u16(), 400);
        let snippet = http_err.body_snippet.as_ref().unwrap();
        assert!(snippet.contains("invalid_request_error"));
        assert!(snippet.contains("Failed parameters"));
        assert_eq!(http_err.provider_code, Some("invalid_val".to_string()));
        assert_eq!(http_err.request_id, Some("req-abc-123".to_string()));
    } else {
        panic!("Expected HttpError");
    }
}

#[tokio::test]
async fn strict_pending_media_is_rejected_before_network_io() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let mut model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let mut spec = (*model.spec).clone();
    spec.capabilities.input_modalities = ModalitySet::none();
    model.spec = Arc::new(spec);
    let mut req = text_request();
    req.messages = vec![Message::User(UserMessage {
        content: vec![UserPart::Media(Media::image_bytes(
            bytes::Bytes::from_static(b"image"),
            mime::IMAGE_PNG,
        ))],
    })];

    assert!(matches!(
        AiClient::new().stream(&model, req).await,
        Err(AiError::Unsupported(ygg_ai::UnsupportedError::Image))
    ));
    assert!(mock_server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn client_leaves_retry_policy_to_the_caller() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "1"))
        .mount(&mock_server)
        .await;

    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let error = match AiClient::new().stream(&model, text_request()).await {
        Err(error) => error,
        Ok(_) => panic!("503 response unexpectedly opened a stream"),
    };
    assert!(matches!(error, AiError::Http(ref http) if http.retryable));
    assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn completed_response_body_obeys_idle_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let model = make_test_model(&uri, Protocol::OpenAiChat, true);
    let mut req = text_request();
    req.output_modalities = OutputModalities::TextAndAudio(ygg_ai::AudioOutputOptions {
        format: ygg_ai::AudioFormat::Wav,
        voice: ygg_ai::AudioVoice::Named("alloy".to_string()),
    });
    let client =
        AiClient::new().with_stream_timeouts(Duration::from_millis(30), Duration::from_millis(200));
    let started = std::time::Instant::now();
    let error = match client.stream(&model, req).await {
        Err(error) => error,
        Ok(_) => panic!("incomplete completed body unexpectedly opened a stream"),
    };
    assert!(matches!(
        error,
        AiError::Transport(ref transport)
            if transport.phase == ygg_ai::TransportPhase::Body && transport.timeout
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn error_response_body_obeys_idle_timeout_but_preserves_status() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 100\r\nRetry-After: 2\r\n\r\n{",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let model = make_test_model(&uri, Protocol::OpenAiChat, false);
    let client =
        AiClient::new().with_stream_timeouts(Duration::from_millis(30), Duration::from_millis(200));
    let started = std::time::Instant::now();
    let error = match client.stream(&model, text_request()).await {
        Err(error) => error,
        Ok(_) => panic!("503 response unexpectedly opened a stream"),
    };
    assert!(matches!(
        error,
        AiError::Http(ref http)
            if http.status.as_u16() == 503
                && http.retryable
                && http.retry_after == Some(Duration::from_secs(2))
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn successful_status_json_error_is_not_misreported_as_missing_sse_finish() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": {
                "message": "reasoning_effort must be one of none, low, or high",
                "type": "Bad Request",
                "code": 400
            }
        })))
        .mount(&mock_server)
        .await;

    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let mut stream = AiClient::new()
        .stream(&model, text_request())
        .await
        .expect("HTTP 200 opens the response body stream");
    let error = stream
        .next()
        .await
        .expect("provider error event")
        .expect_err("JSON error envelope must fail");
    assert!(matches!(
        error,
        AiError::Provider(ref provider)
            if provider.code.as_deref() == Some("400")
                && provider.kind.as_deref() == Some("Bad Request")
                && provider.message.contains("reasoning_effort")
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn transient_gateway_errors_are_marked_retryable() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("upstream unavailable"))
        .mount(&mock_server)
        .await;

    let model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let error = match AiClient::new().stream(&model, text_request()).await {
        Err(error) => error,
        Ok(_) => panic!("502 unexpectedly opened a stream"),
    };
    assert!(matches!(
        error,
        AiError::Http(http) if http.status == http::StatusCode::BAD_GATEWAY && http.retryable
    ));
}

#[tokio::test]
async fn test_client_custom_gateway_prefix_preserved() {
    let mock_server = MockServer::start().await;

    let sse_body = "data: {\"id\": \"chatcmpl-gateway\", \"choices\": [{\"delta\": {\"content\": \"hello\"}}]}\n\n\
                    data: {\"id\": \"chatcmpl-gateway\", \"choices\": [{\"delta\": {}, \"finish_reason\": \"stop\"}]}\n\n\
                    data: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("/tenant/acme/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let base_url = format!("{}/tenant/acme/v1/", mock_server.uri());
    let mut model = make_test_model(&base_url, Protocol::OpenAiChat, false);

    let mut ep = (*model.endpoint).clone();
    ep.base_url = url::Url::parse(&base_url).unwrap();
    model.endpoint = std::sync::Arc::new(ep);

    let client = AiClient::new();
    let req = text_request();
    let mut stream = client.stream(&model, req).await.unwrap();

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }

    assert!(events
        .iter()
        .any(|ev| matches!(ev, StreamEvent::Started { .. })));
}

#[tokio::test]
async fn test_client_drop_sentinel() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let server_uri = format!("http://{}", local_addr);

    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_clone = dropped.clone();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let mut bytes_read = 0;
            loop {
                let n = socket.read(&mut buf[bytes_read..]).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes_read += n;
                let s = String::from_utf8_lossy(&buf[..bytes_read]);
                if s.contains("\r\n\r\n") {
                    break;
                }
            }

            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            socket.write_all(headers.as_bytes()).await.unwrap();

            let event = "5b\r\ndata: {\"id\": \"drop-1\", \"choices\": [{\"delta\": {\"content\": \"hello\"}}]}\n\n\r\n";
            socket.write_all(event.as_bytes()).await.unwrap();

            let mut dummy = [0u8; 128];
            let read_res = socket.read(&mut dummy).await;
            if let Ok(0) = read_res {
                dropped_clone.store(true, Ordering::SeqCst);
            } else if read_res.is_err() {
                dropped_clone.store(true, Ordering::SeqCst);
            }
        }
    });

    let client = AiClient::new();
    let model = make_test_model(&server_uri, Protocol::OpenAiChat, false);
    let req = text_request();

    let mut stream = client.stream(&model, req).await.unwrap();

    let first_ev = stream.next().await.unwrap().unwrap();
    assert!(matches!(first_ev, StreamEvent::Started { .. }));

    drop(stream);

    for _ in 0..10 {
        if dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    assert!(
        dropped.load(Ordering::SeqCst),
        "Expected server to detect client socket close after stream drop"
    );
}
