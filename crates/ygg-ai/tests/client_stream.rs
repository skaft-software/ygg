#![allow(missing_docs)]

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message as WebSocketMessage};
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
            responses_lite: false,
            agent_delegation: None,
            structured_output: false,
            deferred_tool_loading: false,
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
        responses: None,
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
        responses: None,
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
        responses: None,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: Strict,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    }
}
#[derive(Clone, Copy)]
enum WebSocketBehavior {
    Complete,
    CloseBeforeEvents,
    ConnectionLimit,
    RejectHandshake,
    Stall,
}

struct TestResponsesServer {
    base_url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl TestResponsesServer {
    async fn start(behavior: WebSocketBehavior, fallback_body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let recorded_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let requests = Arc::clone(&recorded_requests);
                        let fallback_body = fallback_body.clone();
                        tokio::spawn(async move {
                            let _ = handle_test_responses_connection(
                                stream,
                                behavior,
                                fallback_body,
                                requests,
                            )
                            .await;
                        });
                    }
                }
            }
        });
        Self {
            base_url: format!("http://{address}/"),
            requests,
            shutdown: Some(shutdown),
            task,
        }
    }

    async fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().await.clone()
    }
}

impl Drop for TestResponsesServer {
    fn drop(&mut self) {
        self.shutdown.take();
        self.task.abort();
    }
}

async fn handle_test_responses_connection(
    mut stream: TcpStream,
    behavior: WebSocketBehavior,
    fallback_body: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut peek = [0_u8; 1024];
    let count = stream.peek(&mut peek).await?;
    let request_head = String::from_utf8_lossy(&peek[..count]).to_ascii_lowercase();
    if !request_head.contains("upgrade: websocket") {
        let mut request = [0_u8; 16 * 1024];
        let _ = stream.read(&mut request).await?;
        requests
            .lock()
            .await
            .push(serde_json::json!({"transport": "http"}));
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            fallback_body.len(), fallback_body
        );
        stream.write_all(response.as_bytes()).await?;
        return Ok(());
    }

    if matches!(behavior, WebSocketBehavior::RejectHandshake) {
        stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await?;
        return Ok(());
    }

    let mut socket = accept_async(stream).await?;
    loop {
        let Some(Ok(WebSocketMessage::Text(text))) = socket.next().await else {
            return Ok(());
        };
        let body: serde_json::Value = serde_json::from_str(text.as_ref())?;
        requests.lock().await.push(body.clone());

        match behavior {
            WebSocketBehavior::CloseBeforeEvents => return Ok(()),
            WebSocketBehavior::Stall => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Ok(());
            }
            WebSocketBehavior::Complete
            | WebSocketBehavior::ConnectionLimit
            | WebSocketBehavior::RejectHandshake => {}
        }

        let prewarm = body.get("generate") == Some(&serde_json::Value::Bool(false));
        let id = if prewarm { "resp-prewarm" } else { "resp-turn" };
        if matches!(behavior, WebSocketBehavior::ConnectionLimit) && !prewarm {
            for event in [
                serde_json::json!({
                    "type": "response.created",
                    "response": {"id": id}
                }),
                serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "error": {
                            "code": "websocket_connection_limit_reached",
                            "message": "Create a new websocket connection to continue."
                        }
                    }
                }),
            ] {
                socket
                    .send(WebSocketMessage::Text(event.to_string().into()))
                    .await?;
            }
            return Ok(());
        }
        let mut events = vec![serde_json::json!({
            "type": "response.created",
            "response": {"id": id}
        })];
        if !prewarm {
            events.extend([
                serde_json::json!({
                    "type": "response.content_part.added",
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text"}
                }),
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": "websocket"
                }),
                serde_json::json!({
                    "type": "response.output_text.done",
                    "output_index": 0,
                    "content_index": 0
                }),
            ]);
        }
        events.push(serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "usage": {"input_tokens": 2, "output_tokens": if prewarm { 0 } else { 1 }, "total_tokens": if prewarm { 2 } else { 3 }}
            }
        }));
        for event in events {
            socket
                .send(WebSocketMessage::Text(event.to_string().into()))
                .await?;
        }
        if !prewarm {
            return Ok(());
        }
    }
}

fn websocket_test_model(base_url: &str) -> Model {
    let mut model = make_test_model(base_url, Protocol::OpenAiResponses, false);
    let mut endpoint = (*model.endpoint).clone();
    endpoint.transport = ygg_ai::EndpointTransport::WebSocketPreferred;
    model.endpoint = Arc::new(endpoint);
    model
}

fn responses_request(messages: Vec<Message>, session_id: Option<&str>) -> Request {
    let mut request = text_request();
    request.messages = messages;
    request.session_id = session_id.map(str::to_owned);
    request
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![UserPart::Text(text.to_owned())],
    })
}

fn fallback_responses_body() -> String {
    concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-http\"}}\n\n",
        "data: {\"type\":\"response.content_part.added\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"http\"}\n\n",
        "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"content_index\":0}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-http\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n"
    )
    .to_owned()
}

#[tokio::test]
async fn responses_websocket_decodes_events_and_uses_continuation_suffix() {
    let server =
        TestResponsesServer::start(WebSocketBehavior::Complete, fallback_responses_body()).await;
    let model = websocket_test_model(&server.base_url);
    let client = AiClient::new();
    let session_id = "session-ws";
    client
        .prewarm_responses(
            &model,
            responses_request(vec![user_message("first")], Some(session_id)),
        )
        .await
        .unwrap();

    let mut stream = client
        .stream(
            &model,
            responses_request(
                vec![user_message("first"), user_message("second")],
                Some(session_id),
            ),
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::TextDelta { delta, .. } if delta == "websocket"
        )),
        "events: {events:?}"
    );
    assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["generate"], false);
    assert_eq!(requests[1]["previous_response_id"], "resp-prewarm");
    assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn responses_websocket_connection_limit_retires_socket_and_falls_back() {
    let server = TestResponsesServer::start(
        WebSocketBehavior::ConnectionLimit,
        fallback_responses_body(),
    )
    .await;
    let model = websocket_test_model(&server.base_url);
    let client = AiClient::new();
    let mut stream = client
        .stream(
            &model,
            responses_request(
                vec![user_message("connection limit")],
                Some("session-limit"),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::Started { .. }))
    ));
    let error = stream
        .next()
        .await
        .expect("provider connection limit must be surfaced")
        .expect_err("connection limit is a provider error, not a successful response");
    assert!(matches!(
        error,
        AiError::Provider(provider)
            if provider.code.as_deref() == Some("websocket_connection_limit_reached")
    ));
    assert!(stream.next().await.is_none());

    // Retirement is authoritative before the provider error is published, so
    // an immediate next request deterministically takes HTTP/SSE.
    let mut fallback = client
        .stream(
            &model,
            responses_request(vec![user_message("after refresh")], Some("session-limit")),
        )
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(event) = fallback.next().await {
        match event.unwrap() {
            StreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
            StreamEvent::Finished(_) => break,
            _ => {}
        }
    }
    assert_eq!(text, "http");
    let requests = server.requests().await;
    assert!(requests
        .iter()
        .any(|request| request["transport"] == "http"));
}

#[tokio::test]
async fn responses_websocket_handshake_failure_falls_back_to_http_sse() {
    let server = TestResponsesServer::start(
        WebSocketBehavior::RejectHandshake,
        fallback_responses_body(),
    )
    .await;
    let model = websocket_test_model(&server.base_url);
    let mut stream = AiClient::new()
        .stream(
            &model,
            responses_request(vec![user_message("fallback")], Some("session-handshake")),
        )
        .await
        .unwrap();
    let mut deltas = Vec::new();
    while let Some(event) = stream.next().await {
        if let StreamEvent::TextDelta { delta, .. } = event.unwrap() {
            deltas.push(delta);
        }
    }
    assert_eq!(deltas, vec!["http"]);
    assert_eq!(
        server.requests().await,
        vec![serde_json::json!({"transport": "http"})]
    );
}

#[tokio::test]
async fn responses_websocket_failure_after_send_is_terminal() {
    let server = TestResponsesServer::start(
        WebSocketBehavior::CloseBeforeEvents,
        fallback_responses_body(),
    )
    .await;
    let model = websocket_test_model(&server.base_url);
    let mut stream = AiClient::new()
        .stream(
            &model,
            responses_request(vec![user_message("fallback")], Some("session-close")),
        )
        .await
        .unwrap();
    let error = stream
        .next()
        .await
        .expect("post-send socket failure must be surfaced")
        .expect_err("post-send socket failure must not replay over HTTP");
    assert!(matches!(
        error,
        AiError::Transport(ref transport)
            if transport.phase == ygg_ai::TransportPhase::Body && !transport.timeout
    ));
    assert!(stream.next().await.is_none());
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].get("transport").is_none());
}

#[tokio::test]
async fn responses_websocket_idle_timeout_after_send_is_terminal() {
    let server =
        TestResponsesServer::start(WebSocketBehavior::Stall, fallback_responses_body()).await;
    let model = websocket_test_model(&server.base_url);
    let mut stream = AiClient::new()
        .with_stream_timeouts(Duration::from_millis(10), Duration::from_millis(200))
        .stream(
            &model,
            responses_request(vec![user_message("timeout")], Some("session-timeout")),
        )
        .await
        .unwrap();
    let error = stream
        .next()
        .await
        .expect("post-send idle timeout must be surfaced")
        .expect_err("post-send idle timeout must not replay over HTTP");
    assert!(matches!(
        error,
        AiError::Transport(ref transport)
            if transport.phase == ygg_ai::TransportPhase::Body && transport.timeout
    ));
    assert!(stream.next().await.is_none());
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].get("transport").is_none());
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
        responses: None,
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

/// The mid-stream annotation is an implementation detail: tests assert on
/// the failure that actually ended the stream, so the wrapper is peeled off
/// before matching.
fn stream_inner(error: &AiError) -> &AiError {
    match error {
        AiError::StreamFailure { inner, .. } => inner,
        other => other,
    }
}

fn assert_secret_and_controls_are_absent(error: &AiError, secrets: &[&str]) {
    let display = error.to_string();
    let debug = format!("{error:?}");
    for rendered in [&display, &debug] {
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "credential leaked: {rendered:?}"
            );
        }
        assert!(!rendered.contains('\x1b'), "ESC leaked: {rendered:?}");
        assert!(!rendered.contains('\x07'), "BEL leaked: {rendered:?}");
    }
}

#[tokio::test]
async fn http_error_diagnostics_redact_request_credentials_and_controls() {
    const AUTH_SECRET: &str = "AUTH_SECRET_7f3c";
    const HEADER_SECRET: &str = "HEADER_SECRET_91ab";

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_bytes(format!(
            "provider echoed {AUTH_SECRET} and {HEADER_SECRET} \x1b]52;c;YXR0YWNr\x07"
        )))
        .mount(&mock_server)
        .await;

    let mut model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let mut endpoint = (*model.endpoint).clone();
    endpoint.auth = Auth::bearer(AUTH_SECRET);
    endpoint.default_headers.insert(
        "x-gateway-key",
        http::HeaderValue::from_static(HEADER_SECRET),
    );
    model.endpoint = Arc::new(endpoint);

    let error = match AiClient::new().stream(&model, text_request()).await {
        Err(error) => error,
        Ok(_) => panic!("400 response unexpectedly opened a stream"),
    };
    let AiError::Http(http) = &error else {
        panic!("expected HTTP error, got {error:?}");
    };
    let snippet = http.body_snippet.as_deref().expect("body snippet");
    assert_eq!(snippet.matches("[REDACTED]").count(), 2, "{snippet:?}");
    assert!(
        snippet.contains(r"\u{1b}]52;c;YXR0YWNr\u{7}"),
        "{snippet:?}"
    );
    assert_secret_and_controls_are_absent(&error, &[AUTH_SECRET, HEADER_SECRET]);
}

#[tokio::test]
async fn successful_provider_error_diagnostics_redact_credentials_and_controls() {
    const SECRET: &str = "SUCCESS_SECRET_31de";

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": {
                "message": format!("provider echoed {SECRET} \x1b]0;owned\x07"),
                "type": "bad_request",
                "code": "invalid"
            }
        })))
        .mount(&mock_server)
        .await;

    let mut model = make_test_model(&mock_server.uri(), Protocol::OpenAiChat, false);
    let mut endpoint = (*model.endpoint).clone();
    endpoint.auth = Auth::bearer(SECRET);
    model.endpoint = Arc::new(endpoint);

    let mut stream = AiClient::new()
        .stream(&model, text_request())
        .await
        .expect("HTTP 200 opens the body stream");
    let error = stream
        .next()
        .await
        .expect("provider error event")
        .expect_err("JSON error envelope must fail");
    let AiError::Provider(provider) = stream_inner(&error) else {
        panic!("expected provider error, got {error:?}");
    };
    assert_eq!(
        provider.message,
        r"provider echoed [REDACTED] \u{1b}]0;owned\u{7}"
    );
    assert_secret_and_controls_are_absent(&error, &[SECRET]);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn authenticated_requests_do_not_follow_cross_origin_redirects() {
    let origin = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/sink", destination.uri())),
        )
        .mount(&origin)
        .await;

    let mut model = make_test_model(&origin.uri(), Protocol::OpenAiChat, false);
    let mut endpoint = (*model.endpoint).clone();
    endpoint.auth = Auth::bearer("redirect-bearer-secret");
    endpoint.default_headers.insert(
        "x-gateway-key",
        http::HeaderValue::from_static("redirect-custom-secret"),
    );
    model.endpoint = Arc::new(endpoint);

    let error = match AiClient::new().stream(&model, text_request()).await {
        Err(error) => error,
        Ok(_) => panic!("redirect response was followed"),
    };
    assert!(matches!(
        error,
        AiError::Http(ref http) if http.status == http::StatusCode::TEMPORARY_REDIRECT
    ));
    assert!(destination.received_requests().await.unwrap().is_empty());
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
async fn initial_response_body_timeout_is_separate_from_inter_chunk_idle_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let body = concat!(
        "data: {\"id\":\"chatcmpl-initial\",\"choices\":[{\"delta\":{\"content\":\"ready\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-initial\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let _ = socket.read(&mut request).await.unwrap();
        socket.write_all(headers.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        socket.write_all(body.as_bytes()).await.unwrap();
    });

    let model = make_test_model(&uri, Protocol::OpenAiChat, false);
    let client = AiClient::new()
        .with_stream_timeouts(Duration::from_millis(30), Duration::from_secs(1))
        .with_initial_stream_timeout(Duration::from_millis(200));
    let mut stream = client
        .stream(&model, text_request())
        .await
        .expect("initial response-body allowance should cover prompt processing");
    let mut saw_finished = false;
    while let Some(event) = stream.next().await {
        saw_finished |= matches!(event.unwrap(), StreamEvent::Finished(_));
    }
    assert!(saw_finished);
}

#[tokio::test]
async fn inter_chunk_idle_timeout_applies_after_a_delayed_first_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let first =
        "data: {\"id\":\"chatcmpl-initial\",\"choices\":[{\"delta\":{\"content\":\"ready\"}}]}\n\n";
    let rest = concat!(
        "data: {\"id\":\"chatcmpl-initial\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
        first.len() + rest.len()
    );
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let _ = socket.read(&mut request).await.unwrap();
        socket.write_all(headers.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        socket.write_all(first.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = socket.write_all(rest.as_bytes()).await;
    });

    let model = make_test_model(&uri, Protocol::OpenAiChat, false);
    let client = AiClient::new()
        .with_stream_timeouts(Duration::from_millis(30), Duration::from_secs(1))
        .with_initial_stream_timeout(Duration::from_millis(200));
    let mut stream = client.stream(&model, text_request()).await.unwrap();
    let mut saw_ready = false;
    let mut saw_idle_timeout = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta { delta, .. }) if delta == "ready" => saw_ready = true,
            Err(error)
                if matches!(
                    stream_inner(&error),
                    AiError::Transport(transport)
                        if transport.phase == ygg_ai::TransportPhase::Body
                            && transport.timeout
                ) =>
            {
                saw_idle_timeout = true;
                break;
            }
            Ok(_) => {}
            Err(error) => panic!("unexpected stream error: {error}"),
        }
    }
    assert!(saw_ready, "the longer initial allowance was not applied");
    assert!(
        saw_idle_timeout,
        "the shorter inter-chunk idle timeout was not applied"
    );
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
    let client = AiClient::new()
        .with_stream_timeouts(Duration::from_millis(30), Duration::from_millis(200))
        .with_initial_stream_timeout(Duration::from_secs(1));
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
        stream_inner(&error),
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
