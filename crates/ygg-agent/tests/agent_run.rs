#![allow(missing_docs)]

//! Agent integration tests against a deterministic scripted model.
//!
//! The scripted boundary is `ygg-ai`'s real HTTP + SSE path: a wiremock server
//! replays hand-written Anthropic Messages SSE bodies in sequence, so the
//! agent exercises the exact stream-assembly and request-building code it
//! uses in production, with no live provider.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};
use ygg_agent::{
    Agent, AgentConfig, AgentEvent, CompletionPolicy, CoreTools, EntryId, EntryValue,
    ExtensionHost, FinishReason, InputPart, OutputChannel, OutputStream, ReplaySafety, RunControl,
    SandboxConfig, Session, Tool, ToolContext, ToolError, ToolOutput, UsageRecordKind, UserInput,
};
use ygg_ai::{
    AiClient, AssistantMessage, AssistantPart, Auth, Capabilities, Endpoint, EndpointId, Media,
    Message, Modality, ModalitySet, Model, ModelId, ModelLimits, ModelSpec, Pricing, Protocol,
    ReasoningCapability, ReasoningConfig, ReasoningControl, ReasoningEffortBudgets, TokenRate,
    ToolCall, Usage, UserMessage, UserPart,
};

const MAX_CONNECT_ATTEMPTS_FOR_TEST: usize = 6;

// ── Scripted SSE bodies (Anthropic Messages wire shapes) ───────────────────

fn frame(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn msg_start() -> String {
    frame(
        "message_start",
        serde_json::json!({
            "type": "message_start",
            "message": {"id": "msg_1", "usage": {"input_tokens": 5, "output_tokens": 0}}
        }),
    )
}

fn msg_end(stop_reason: &str) -> String {
    frame(
        "message_delta",
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": {"output_tokens": 3}
        }),
    ) + &frame("message_stop", serde_json::json!({"type": "message_stop"}))
}

fn text_block(index: usize, deltas: &[&str]) -> String {
    let mut s = frame(
        "content_block_start",
        serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""}
        }),
    );
    for delta in deltas {
        s += &frame(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        );
    }
    s + &frame(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": index}),
    )
}

fn thinking_block(index: usize, text: &str) -> String {
    frame(
        "content_block_start",
        serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "thinking", "thinking": ""}
        }),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": text}
        }),
    ) + &frame(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": index}),
    )
}

fn tool_block(index: usize, id: &str, name: &str, args: &serde_json::Value) -> String {
    frame(
        "content_block_start",
        serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name}
        }),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": args.to_string()}
        }),
    ) + &frame(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": index}),
    )
}

/// A complete turn that answers with plain text.
fn text_turn(text: &str) -> String {
    msg_start() + &text_block(0, &[text]) + &msg_end("end_turn")
}

fn text_turn_with_stop(text: &str, stop_reason: &str) -> String {
    msg_start() + &text_block(0, &[text]) + &msg_end(stop_reason)
}

fn openai_text_turn(text: &str) -> String {
    let text = serde_json::to_string(text).unwrap();
    format!(
        "data: {{\"id\":\"chat\",\"choices\":[{{\"delta\":{{\"role\":\"assistant\",\"content\":{text}}}}}]}}\n\ndata: {{\"id\":\"chat\",\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}}}\n\ndata: [DONE]\n\n"
    )
}

/// A syntactically valid stream prefix with visible output but no terminal
/// message event. Closing the HTTP body after this prefix reproduces the
/// provider/proxy disconnect that used to fail long Ygg runs.
fn partial_text_turn(text: &str) -> String {
    msg_start()
        + &frame(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        )
        + &frame(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }),
        )
}

/// A complete turn that requests the given tool calls.
fn tool_turn(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut s = msg_start();
    for (i, (id, name, args)) in calls.iter().enumerate() {
        s += &tool_block(i, id, name, args);
    }
    s + &msg_end("tool_use")
}

// ── Scripted server + agent harness ────────────────────────────────────────

/// Replays SSE bodies in sequence; the last body repeats once exhausted.
struct Script {
    bodies: Vec<String>,
    next: AtomicUsize,
}

impl Respond for Script {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .get(i)
            .or_else(|| self.bodies.last())
            .expect("script must have at least one body")
            .clone();
        ResponseTemplate::new(200)
            .set_body_string(body)
            .insert_header("content-type", "text/event-stream")
    }
}

struct RetryInitialOpen {
    calls: Arc<AtomicUsize>,
}

impl Respond for RetryInitialOpen {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(503)
                .set_body_string(r#"{"error":{"message":"temporarily unavailable"}}"#)
                .insert_header("retry-after", "0")
        } else {
            ResponseTemplate::new(200)
                .set_body_string(text_turn("recovered"))
                .insert_header("content-type", "text/event-stream")
        }
    }
}

struct FailThenSucceed {
    calls: AtomicUsize,
}

impl Respond for FailThenSucceed {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(400).set_body_string(r#"{"error":{"message":"request failed"}}"#)
        } else {
            ResponseTemplate::new(200)
                .set_body_string(text_turn("hello from the new turn"))
                .insert_header("content-type", "text/event-stream")
        }
    }
}

/// Serve one truncated HTTP body followed by a complete response. The first
/// response advertises more bytes than it sends, making reqwest surface the
/// same non-timeout body transport error seen when a provider closes an SSE
/// connection mid-generation.
async fn interrupted_body_server(partial: String, recovered: String) -> (String, Arc<AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    tokio::spawn(async move {
        for (index, body) in [partial, recovered].into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let (header_end, content_length) = loop {
                let read = socket.read(&mut buf).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..read]);
                let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
                    continue;
                };
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                });
                break (header_end, content_length.unwrap_or_default());
            };
            while request.len().saturating_sub(header_end) < content_length {
                let read = socket.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            server_calls.fetch_add(1, Ordering::SeqCst);

            let declared_length = if index == 0 {
                body.len() + 128
            } else {
                body.len()
            };
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    (uri, calls)
}

/// Accept requests and close each socket before writing response headers. This
/// deterministically exercises ConnectOrHeaders recovery without waiting on a
/// real DNS route or provider timeout.
async fn dropped_header_server(attempts: usize) -> (String, Arc<AtomicUsize>) {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    tokio::spawn(async move {
        for _ in 0..attempts {
            let (socket, _) = listener.accept().await.unwrap();
            server_calls.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (uri, calls)
}

struct ContextAwareScript {
    main_calls: AtomicUsize,
    reject_at: Vec<usize>,
}

struct AbortableCompactionScript {
    summary_started: Arc<std::sync::atomic::AtomicBool>,
}

struct SummaryThenSlowMain;

impl Respond for SummaryThenSlowMain {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let tools_empty = body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_none_or(Vec::is_empty);
        let response = if tools_empty {
            text_turn("authoritative usage forced this summary")
        } else {
            text_turn("normal response should not open before compaction is visible")
        };
        let template = ResponseTemplate::new(200)
            .set_body_string(response)
            .insert_header("content-type", "text/event-stream");
        if tools_empty {
            template
        } else {
            template.set_delay(Duration::from_secs(5))
        }
    }
}

impl Respond for AbortableCompactionScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let tools_empty = body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .is_none_or(Vec::is_empty);
        if tools_empty {
            self.summary_started.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_string(text_turn("summary that must never commit"))
                .insert_header("content-type", "text/event-stream")
        } else {
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":{"message":"context window exceeded"}}"#)
        }
    }
}

impl Respond for ContextAwareScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let tools_empty = match body.get("tools") {
            None | Some(serde_json::Value::Null) => true,
            Some(tools) => tools.as_array().is_some_and(Vec::is_empty),
        };
        if tools_empty {
            ResponseTemplate::new(200)
                .set_body_string(text_turn("compacted summary"))
                .insert_header("content-type", "text/event-stream")
        } else {
            let index = self.main_calls.fetch_add(1, Ordering::SeqCst);
            if self.reject_at.contains(&index) {
                return ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"message":"context window exceeded"}}"#);
            }
            let body = if index < 3 {
                tool_turn(&[(
                    &format!("read_{index}"),
                    "read",
                    serde_json::json!({"path": "large.txt"}),
                )])
            } else {
                text_turn("done after compaction")
            };
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream")
        }
    }
}

fn scripted_model(uri: &str) -> Model {
    Model {
        spec: Arc::new(ModelSpec {
            id: ModelId("scripted".to_string()),
            endpoint: EndpointId("test".to_string()),
            api_name: "scripted-model".to_string(),
            display_name: None,
            protocol: Protocol::AnthropicMessages,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none().with(Modality::Image),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: None,
                structured_output: false,
            },
            limits: ModelLimits {
                context_window: 200_000,
                max_output_tokens: 8192,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        }),
        endpoint: Arc::new(Endpoint {
            id: EndpointId("test".to_string()),
            base_url: url::Url::parse(uri).unwrap(),
            auth: Auth::bearer("test-key"),
            default_headers: http::HeaderMap::new(),
            transport: ygg_ai::EndpointTransport::Http,
            timeout: Duration::from_secs(10),
        }),
    }
}

fn openai_multimodal_model(uri: &str) -> Model {
    let base = scripted_model(uri);
    let mut spec = (*base.spec).clone();
    spec.protocol = Protocol::OpenAiChat;
    Model {
        spec: Arc::new(spec),
        endpoint: Arc::new(Endpoint {
            id: EndpointId("test".to_string()),
            base_url: url::Url::parse(&format!("{uri}/v1/")).unwrap(),
            auth: Auth::bearer("test-key"),
            default_headers: http::HeaderMap::new(),
            transport: ygg_ai::EndpointTransport::Http,
            timeout: Duration::from_secs(10),
        }),
    }
}

fn scripted_model_with_limits(uri: &str, context_window: u64, max_output_tokens: u64) -> Model {
    let base = scripted_model(uri);
    let mut spec = (*base.spec).clone();
    spec.limits.context_window = context_window;
    spec.limits.max_output_tokens = max_output_tokens;
    Model {
        spec: Arc::new(spec),
        endpoint: base.endpoint,
    }
}

struct Harness {
    agent: Agent,
    server: Option<MockServer>,
    session_path: PathBuf,
    workspace: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

fn build_agent(uri: &str, workspace: &Path, session_path: &Path, max_turns: Option<u64>) -> Agent {
    build_agent_from_session(
        uri,
        workspace,
        Session::create(session_path).unwrap(),
        max_turns,
    )
}

fn build_agent_from_session(
    uri: &str,
    workspace: &Path,
    session: Session,
    max_turns: Option<u64>,
) -> Agent {
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(workspace);
    sandbox.allow_edit = true;
    sandbox.allow_write = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model(uri),
        session,
        system: "You are a scripted test agent.".to_string(),
        sandbox,
        extensions,
        max_turns,
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap()
}

/// A scripted Anthropic model that advertises token-budget reasoning, so a
/// non-off [`ReasoningConfig`] passes `ygg-ai` validation and serializes a
/// `thinking` block onto the wire.
fn scripted_model_with_reasoning(uri: &str) -> Model {
    let base = scripted_model(uri);
    let mut spec = (*base.spec).clone();
    spec.capabilities.reasoning = Some(ReasoningCapability {
        control: ReasoningControl::TokenBudget,
        exposes_text: true,
        preserves_state: true,
        min_effort: ygg_ai::ReasoningEffort::Minimal,
        effort_budgets: Some(ReasoningEffortBudgets {
            minimal: 1024,
            low: 2048,
            medium: 4096,
            high: 8192,
            xhigh: 8192,
            max: 8192,
        }),
        openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
        max_effort: ygg_ai::ReasoningEffort::High,
    });
    Model {
        spec: Arc::new(spec),
        endpoint: base.endpoint,
    }
}

/// Builds an agent bound to `model` with an explicit [`ReasoningConfig`], the
/// core tools, and a fully-enabled sandbox.
fn build_agent_with_reasoning(
    model: Model,
    session_path: &Path,
    workspace: &Path,
    reasoning: ReasoningConfig,
    max_turns: Option<u64>,
) -> Agent {
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(workspace);
    sandbox.allow_edit = true;
    sandbox.allow_write = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    Agent::new(AgentConfig {
        client: AiClient::new(),
        model,
        session: Session::create(session_path).unwrap(),
        system: "You are a scripted test agent.".to_string(),
        sandbox,
        extensions,
        max_turns,
        reasoning,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap()
}

/// Spins up a scripted server replaying `bodies`, then builds a reasoning-capable
/// agent with the given `reasoning` config against it.
async fn reasoning_harness(
    bodies: Vec<String>,
    reasoning: ReasoningConfig,
    reasoning_capable: bool,
) -> (
    Agent,
    MockServer,
    PathBuf,
    (tempfile::TempDir, tempfile::TempDir),
) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies,
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let model = if reasoning_capable {
        scripted_model_with_reasoning(&server.uri())
    } else {
        scripted_model(&server.uri())
    };
    let agent = build_agent_with_reasoning(model, &session_path, &workspace, reasoning, Some(8));
    (agent, server, session_path, (workspace_dir, session_dir))
}

async fn harness(bodies: Vec<String>, max_turns: Option<u64>) -> Harness {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies,
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let agent = build_agent(&server.uri(), &workspace, &session_path, max_turns);
    Harness {
        agent,
        server: Some(server),
        session_path,
        workspace,
        _dirs: (workspace_dir, session_dir),
    }
}

async fn collect(run: &mut ygg_agent::Run<'_>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        events.push(event);
    }
    events
}

fn session_with_authoritative_pressure(path: &Path, total_tokens: u64) -> Session {
    let mut session = Session::create(path).unwrap();
    let mut latest_assistant = None;
    for index in 0..5 {
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text(format!("prior user {index}"))],
            })))
            .unwrap();
        latest_assistant = Some(
            session
                .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::Text(format!("prior answer {index}"))],
                    model: ModelId("scripted".into()),
                    protocol: Protocol::AnthropicMessages,
                })))
                .unwrap(),
        );
    }
    session
        .record_assistant_usage(
            latest_assistant.unwrap(),
            EndpointId("test".into()),
            ModelId("scripted".into()),
            Usage {
                input_tokens: total_tokens.saturating_sub(1_000),
                output_tokens: 1_000,
                total_tokens,
                ..Usage::default()
            },
            None,
        )
        .unwrap();
    session
}

/// Every started run must emit exactly one `RunFinished`, as its final event.
fn assert_single_run_finished(events: &[AgentEvent]) -> &FinishReason {
    let finishes: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e, AgentEvent::RunFinished { .. }).then_some(i))
        .collect();
    assert_eq!(finishes.len(), 1, "expected exactly one RunFinished");
    assert_eq!(finishes[0], events.len() - 1, "RunFinished must be last");
    match &events[events.len() - 1] {
        AgentEvent::RunFinished { reason, .. } => reason,
        _ => unreachable!(),
    }
}

async fn wire_requests(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn count_tool_results(message: &serde_json::Value) -> usize {
    message["content"]
        .as_array()
        .map(|parts| parts.iter().filter(|p| p["type"] == "tool_result").count())
        .unwrap_or(0)
}

fn request_has_no_tools(request: &serde_json::Value) -> bool {
    match request.get("tools").and_then(serde_json::Value::as_array) {
        None => true,
        Some(tools) => tools.is_empty(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn retryable_initial_stream_open_is_retried_by_the_agent() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(RetryInitialOpen {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut agent = build_agent(&server.uri(), &workspace, &session_path, Some(4));

    let mut run = agent.prompt("retry safely").await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
    let retries = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ProviderRetry { .. }))
        .count();
    assert_eq!(retries, 1, "the retry must be visible while it happens");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn repeated_header_transport_failure_is_visible_and_bounded() {
    let (uri, calls) = dropped_header_server(MAX_CONNECT_ATTEMPTS_FOR_TEST).await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut agent = build_agent(&uri, &workspace, &session_path, Some(4));

    let mut run = agent.prompt("fail visibly").await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Failed(_)
    ));
    let retries = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                error,
                ..
            } => Some((*attempt, *max_attempts, error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retries.len(), 5);
    assert_eq!((retries[0].0, retries[0].1), (1, 5));
    assert_eq!((retries[4].0, retries[4].1), (5, 5));
    assert!(retries
        .iter()
        .all(|(_, _, error)| error.contains("Are you connected to the internet?")));
    assert!(retries[0].2.contains("provider=test model=scripted"));
    assert!(retries[0].2.contains("ConnectOrHeaders"));
    assert!(
        retries[0].2.contains("connection") || retries[0].2.contains("closed"),
        "the sanitized transport cause chain was lost: {}",
        retries[0].2
    );
    assert!(!retries[0].2.contains(&uri));
    assert!(!retries[0].2.contains("test-key"));
    assert_eq!(calls.load(Ordering::SeqCst), MAX_CONNECT_ATTEMPTS_FOR_TEST);

    let FinishReason::Failed(error) = assert_single_run_finished(&events) else {
        unreachable!("failure was asserted above")
    };
    let failure = error.to_string();
    assert!(failure.contains("after 5 retries"), "{failure}");
    assert!(
        failure.contains("Are you connected to the internet?"),
        "{failure}"
    );
}

#[tokio::test]
async fn connect_retry_delay_is_cancellable() {
    let (uri, calls) = dropped_header_server(1).await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut agent = build_agent(&uri, &workspace, &session_path, Some(4));

    let mut run = agent.prompt("cancel retry").await.unwrap();
    let control = run.control();
    let started = std::time::Instant::now();
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        if matches!(&event, AgentEvent::ProviderRetry { .. }) {
            control.abort();
        }
        events.push(event);
    }
    drop(run);

    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ProviderRetry { .. }))
            .count(),
        1
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "aborting the visible backoff must prevent the retry request"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "abort must not wait for the retry backoff"
    );
}

#[tokio::test]
async fn body_disconnect_after_output_discards_partial_and_retries_network_loss() {
    let (uri, calls) = interrupted_body_server(
        partial_text_turn("discarded partial"),
        text_turn("recovered exactly once"),
    )
    .await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut agent = build_agent(&uri, &workspace, &session_path, Some(4));

    let mut run = agent.prompt("do not replay generated work").await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text,
        } if text.contains("discarded partial")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ProviderRetry {
            attempt: 1,
            max_attempts: 5,
            error,
            ..
        } if error.contains("Are you connected to the internet?")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text,
        } if text.contains("recovered exactly once")
    )));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn body_disconnect_before_output_retries_as_network_loss() {
    let (uri, calls) =
        interrupted_body_server(msg_start(), text_turn("recovered after reconnect")).await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut agent = build_agent(&uri, &workspace, &session_path, Some(4));

    let mut run = agent.prompt("recover safely").await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ProviderRetry {
            attempt: 1,
            max_attempts: 5,
            error,
            ..
        } if error.contains("Are you connected to the internet?")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text,
        } if text.contains("recovered after reconnect")
    )));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_provider_turn_does_not_replay_old_intent_on_next_prompt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(FailThenSucceed {
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let mut agent = build_agent(
        &server.uri(),
        &workspace,
        &session_dir.path().join("failed-turn.jsonl"),
        Some(4),
    );

    assert!(agent.complete("old request").await.is_err());
    let output = agent.complete("hi").await.unwrap();
    assert_eq!(output.text, "hello from the new turn");

    let requests = wire_requests(&server).await;
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert!(messages[0].to_string().contains("old request"));
    assert_eq!(messages[1]["role"], "assistant");
    assert!(messages[1].to_string().contains("failed before completion"));
    assert_eq!(messages[2]["role"], "user");
    assert!(messages[2].to_string().contains("hi"));
}

#[tokio::test]
async fn one_user_task_compacts_completed_tool_episodes_in_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(ContextAwareScript {
            main_calls: AtomicUsize::new(0),
            reject_at: vec![],
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    std::fs::write(
        workspace.join("large.txt"),
        (0..600)
            .map(|index| format!("line {index}: a long repeated payload for sizing\n"))
            .collect::<String>(),
    )
    .unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(&workspace);
    sandbox.allow_edit = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    let model = scripted_model_with_limits(&server.uri(), 12_000, 1_024);
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model,
        session: Session::create(&session_path).unwrap(),
        system: "test".into(),
        sandbox,
        extensions,
        max_turns: Some(10),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap();

    let output = agent
        .complete("inspect the large file repeatedly")
        .await
        .unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert!(agent
        .session()
        .entries()
        .iter()
        .any(|entry| matches!(entry.value, EntryValue::Compaction { .. })));
}

#[tokio::test]
async fn authoritative_usage_compacts_and_reports_phase_before_opening_slow_main_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(SummaryThenSlowMain)
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let session_path = sessions.path().join("authoritative-pressure.jsonl");
    let session = session_with_authoritative_pressure(&session_path, 180_000);
    let mut agent = build_agent_from_session(&server.uri(), workspace.path(), session, Some(4));
    // Even when the keep preference exceeds the number of available turns,
    // the configured threshold still has to trigger compaction.
    agent.set_compaction_policy(true, 0.85, 10).unwrap();

    let mut run = agent.prompt("new work").await.unwrap();
    let control = run.control();
    let mut saw_start = false;
    let mut saw_finish = false;
    while !saw_finish {
        let event = tokio::time::timeout(Duration::from_secs(1), run.next())
            .await
            .expect("compaction phase must be visible before the slow main route")
            .expect("run event");
        match event {
            AgentEvent::CompactionStarted {
                reason: ygg_agent::CompactionReason::Threshold,
            } => saw_start = true,
            AgentEvent::CompactionFinished {
                result: Ok(ref info),
                ..
            } => {
                assert!(saw_start, "finish preceded start");
                assert!(info.summary.contains("authoritative usage"));
                saw_finish = true;
                control.abort();
            }
            _ => {}
        }
    }
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    drop(run);

    let requests = wire_requests(&server).await;
    assert_eq!(
        requests.len(),
        1,
        "normal provider request opened too early"
    );
    assert!(requests[0]
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_none_or(Vec::is_empty));
    assert!(agent
        .session()
        .entries()
        .iter()
        .any(|entry| matches!(entry.value, EntryValue::Compaction { .. })));
}

#[tokio::test]
async fn disabled_auto_compaction_allows_below_capacity_request_past_threshold() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(text_turn("auto compaction is off"))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let session = session_with_authoritative_pressure(
        &sessions.path().join("disabled-auto-compaction.jsonl"),
        180_000,
    );
    let mut agent = build_agent_from_session(&server.uri(), workspace.path(), session, Some(1));
    agent.set_compaction_policy(false, 0.85, 4).unwrap();

    let mut run = agent.prompt("new work").await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::CompactionStarted { .. } | AgentEvent::CompactionFinished { .. }
    )));
    drop(run);
    assert!(agent
        .session()
        .entries()
        .iter()
        .all(|entry| !matches!(entry.value, EntryValue::Compaction { .. })));
    let requests = wire_requests(&server).await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| !tools.is_empty()));
}

#[tokio::test]
async fn hard_cost_reservation_blocks_network_before_a_request_can_overshoot() {
    let server = MockServer::start().await;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let mut model = scripted_model(&server.uri());
    let mut spec = (*model.spec).clone();
    spec.pricing = Some(Pricing {
        input: TokenRate(1_000_000),
        output: TokenRate(1_000_000),
        cache_read: TokenRate(1_000_000),
        cache_write_5m: TokenRate(1_000_000),
        cache_write_1h: Some(TokenRate(1_000_000)),
        reasoning: Some(TokenRate(1_000_000)),
        tiers: vec![],
    });
    model.spec = Arc::new(spec);
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model,
        session: Session::create(sessions.path().join("cost-limit.jsonl")).unwrap(),
        system: "cost test".into(),
        sandbox: SandboxConfig::new(workspace.path()),
        extensions,
        max_turns: Some(2),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap();
    agent.set_max_session_cost_microdollars(Some(1));

    let error = agent
        .complete("do not spend beyond the ceiling")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cost limit"), "{error}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn zero_input_budget_fails_locally_before_opening_a_provider_request() {
    let server = MockServer::start().await;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let model = scripted_model_with_limits(&server.uri(), 64, 64);
    let mut agent = build_agent_with_reasoning(
        model,
        &sessions.path().join("zero-budget.jsonl"),
        workspace.path(),
        ReasoningConfig::Off,
        Some(1),
    );

    let error = agent.complete("this request cannot fit").await.unwrap_err();
    assert!(error.to_string().contains("context"), "{error}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn provider_context_error_forces_one_compaction_before_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(ContextAwareScript {
            main_calls: AtomicUsize::new(0),
            reject_at: vec![2],
        })
        .mount(&server)
        .await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    std::fs::write(workspace.join("large.txt"), "small\n").unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(&workspace);
    sandbox.allow_edit = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model(&server.uri()),
        session: Session::create(&session_path).unwrap(),
        system: "test".into(),
        sandbox,
        extensions,
        max_turns: Some(10),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap();
    let output = agent.complete("force context recovery").await.unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert!(agent
        .session()
        .entries()
        .iter()
        .any(|entry| matches!(entry.value, EntryValue::Compaction { .. })));
}

#[tokio::test]
async fn abort_cancels_compaction_without_late_usage_or_summary_commits() {
    let server = MockServer::start().await;
    let summary_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(AbortableCompactionScript {
            summary_started: Arc::clone(&summary_started),
        })
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let mut agent = build_agent(
        &server.uri(),
        workspace.path(),
        &sessions.path().join("abort-compaction.jsonl"),
        Some(4),
    );
    for index in 0..3 {
        agent
            .session_mut()
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text(format!("prior user {index}"))],
            })))
            .unwrap();
        agent
            .session_mut()
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text(format!("prior answer {index}"))],
                model: ModelId("scripted".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
    }

    let mut run = agent.prompt("trigger context recovery").await.unwrap();
    let control = run.control();
    let abort_task = tokio::spawn(async move {
        while !summary_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        control.abort();
    });
    let started = std::time::Instant::now();
    let mut reason = None;
    while let Some(event) = run.next().await {
        if let AgentEvent::RunFinished { reason: ended, .. } = event {
            reason = Some(ended);
        }
    }
    abort_task.await.unwrap();
    drop(run);

    assert!(matches!(reason, Some(FinishReason::Aborted)));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(agent
        .session()
        .entries()
        .iter()
        .all(|entry| !matches!(entry.value, EntryValue::Compaction { .. })));
    assert!(agent
        .session()
        .usage_records()
        .iter()
        .all(|record| !matches!(record.kind, UsageRecordKind::Compaction)));
}

#[tokio::test]
async fn repeated_context_rejection_advances_compaction_without_spending_turns() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(ContextAwareScript {
            main_calls: AtomicUsize::new(0),
            // Three completed tool episodes provide two successively newer
            // compaction boundaries. Reject both requests before generation.
            reject_at: vec![3, 4],
        })
        .mount(&server)
        .await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    std::fs::write(workspace.join("large.txt"), "small\n").unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(&workspace);
    sandbox.allow_edit = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    let mut model = scripted_model(&server.uri());
    Arc::make_mut(&mut model.spec).pricing = Some(Pricing {
        input: TokenRate(1_000_000),
        output: TokenRate(1_000_000),
        cache_read: TokenRate(1_000_000),
        cache_write_5m: TokenRate(1_000_000),
        cache_write_1h: Some(TokenRate(1_000_000)),
        reasoning: None,
        tiers: vec![],
    });
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model,
        session: Session::create(&session_path).unwrap(),
        system: "test".into(),
        sandbox,
        extensions,
        // Exactly three tool responses plus the final response. Failed context
        // opens and summary calls must not consume this logical-turn budget.
        max_turns: Some(4),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap();

    let output = agent
        .complete("force repeated context recovery")
        .await
        .unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert_eq!(output.text, "done after compaction");
    // Each compaction performs one grounded summary request, so two
    // recoveries add two tool-free subagent calls to the four main turns.
    assert_eq!(output.usage.input_tokens, 30);
    assert_eq!(output.usage.output_tokens, 18);
    // Four main turns plus two compaction summaries each cost 5 + 3
    // microdollars. Compaction must reach both run and durable session totals.
    assert_eq!(output.cost_microdollars, 48);
    assert_eq!(agent.session().total_cost_microdollars(), 48);
    assert_eq!(
        agent
            .session()
            .usage_records()
            .iter()
            .filter(|record| matches!(record.kind, UsageRecordKind::Compaction))
            .count(),
        2
    );

    let compactions = agent
        .session()
        .entries()
        .iter()
        .filter(|entry| matches!(entry.value, EntryValue::Compaction { .. }))
        .count();
    assert_eq!(compactions, 2);
    let visible_summaries = agent
        .session()
        .context()
        .unwrap()
        .iter()
        .filter(|message| {
            matches!(message, Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::Text(text) if text.starts_with("[summary of earlier conversation]"))))
        })
        .count();
    assert_eq!(visible_summaries, 1, "older overlapping summary leaked");
}

#[tokio::test]
async fn text_only_completion() {
    let mut h = harness(vec![text_turn("Hello world")], Some(8)).await;
    let output = h.agent.complete("hi").await.unwrap();

    assert_eq!(output.text, "Hello world");
    assert!(matches!(output.reason, FinishReason::Completed));
    assert!(output.usage.output_tokens > 0, "usage must propagate");

    // User + assistant persisted, head on the assistant message.
    let session = h.agent.session();
    assert_eq!(session.entries().len(), 2);
    assert_eq!(session.head(), Some(output.head.clone()));
    assert_eq!(session.checkpoints().len(), 1);
    assert_eq!(session.checkpoints()[0].prompt, session.entries()[0].id);
    assert_eq!(session.checkpoints()[0].head, output.head);
}

#[tokio::test]
async fn run_context_snapshot_updates_without_a_presentation_layer() {
    let mut h = harness(vec![text_turn("streamed context")], Some(4)).await;
    let mut run = h.agent.prompt("track it").await.unwrap();
    let mut revision = 0;
    while run.next().await.is_some() {
        let snapshot = run.context_snapshot();
        assert!(snapshot.revision >= revision);
        revision = snapshot.revision;
    }
    let snapshot = run.context_snapshot();
    assert_eq!(snapshot.responses_started, 1);
    assert_eq!(snapshot.responses_finished, 1);
    assert!(snapshot.response_text_bytes >= "streamed context".len() as u64);
    assert!(snapshot.response_usage.total_tokens > 0);
    assert_eq!(snapshot.run_usage, snapshot.response_usage);
}

#[tokio::test]
async fn openai_compatible_agent_sends_inline_image_end_to_end() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"id\":\"vision\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"I see it\"}}]}\n\n",
        "data: {\"id\":\"vision\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":3,\"total_tokens\":12}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let mut agent = build_agent_with_reasoning(
        openai_multimodal_model(&server.uri()),
        &sessions.path().join("vision.jsonl"),
        workspace.path(),
        ReasoningConfig::Off,
        Some(4),
    );
    let input = UserInput::from(vec![
        InputPart::Text("describe this image".into()),
        InputPart::Media(Media::image_bytes(
            bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            "image/png".parse().unwrap(),
        )),
    ]);
    let output = agent.complete(input).await.unwrap();
    assert_eq!(output.text, "I see it");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap();
    assert_eq!(
        content[0],
        serde_json::json!({"type":"text","text":"describe this image"})
    );
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
    assert!(matches!(
        &agent.session().context().unwrap()[0],
        Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::Media(_)))
    ));
}

#[tokio::test]
async fn tool_output_locked_causes_a_corrective_openai_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Script {
            bodies: vec![
                openai_text_turn("preparing [tool_output_locked]"),
                openai_text_turn("LOCK_RECOVERED"),
            ],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let mut agent = build_agent_with_reasoning(
        openai_multimodal_model(&server.uri()),
        &sessions.path().join("locked.jsonl"),
        workspace.path(),
        ReasoningConfig::Off,
        Some(4),
    );

    let output = agent.complete("recover the call").await.unwrap();
    assert!(output.text.contains("LOCK_RECOVERED"));
    let context = agent.session().context().unwrap();
    let serialized = serde_json::to_string(&context).unwrap();
    assert!(!serialized.contains("tool_output_locked"));
    assert!(serialized.contains("Re-issue that tool call now"));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn non_normal_stop_reasons_never_become_completed() {
    let mut h = harness(
        vec![
            text_turn_with_stop("truncated", "max_tokens"),
            text_turn("continued"),
        ],
        Some(8),
    )
    .await;
    let output = h.agent.complete("finish the task").await.unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert_eq!(output.text, "truncatedcontinued");
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .to_string()
        .contains("truncated at the token limit"));

    let mut paused = harness(
        vec![
            text_turn_with_stop("paused", "pause_turn"),
            text_turn("resumed"),
        ],
        Some(8),
    )
    .await;
    assert!(paused.agent.complete("resume").await.is_ok());

    let mut refusal = harness(vec![text_turn_with_stop("no", "refusal")], Some(8)).await;
    assert!(refusal.agent.complete("try").await.is_err());
    let mut unknown = harness(
        vec![text_turn_with_stop("?", "provider_new_reason")],
        Some(8),
    )
    .await;
    assert!(unknown.agent.complete("try").await.is_err());
}

#[tokio::test]
async fn resumed_agent_reexecutes_only_missing_tool_results() {
    let mut h = harness(vec![text_turn("resumed")], Some(8)).await;
    std::fs::write(h.workspace.join("recover.txt"), "recovered content\n").unwrap();
    h.agent
        .session_mut()
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text("prior request".into())],
        })))
        .unwrap();
    h.agent
        .session_mut()
        .append(EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::ToolCall(ToolCall {
                id: ygg_ai::ToolCallId("crashed_call".into()),
                name: "read".into(),
                arguments_json: serde_json::json!({"path": "recover.txt"}).to_string(),
            })],
            model: ModelId("scripted".into()),
            protocol: Protocol::AnthropicMessages,
        })))
        .unwrap();

    let output = h.agent.complete("continue after restart").await.unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert!(h.agent.session().entries().iter().any(|entry| {
        matches!(
            &entry.value,
            EntryValue::Message(Message::User(user))
                if user.content.iter().any(|part| matches!(part, UserPart::ToolResult(result) if result.tool_call_id.0 == "crashed_call"))
        )
    }));
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].to_string().contains("recovered content"));
}

#[tokio::test]
async fn restart_never_replays_a_mutating_tool_without_an_idempotency_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(text_turn("reconciled")))
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(1));
    let mut agent = build_agent_with_extra_tool(
        &server.uri(),
        workspace.path(),
        &sessions.path().join("unsafe-recovery.jsonl"),
        Some(8),
        UnsafeRecoveryTool {
            calls: Arc::clone(&calls),
        },
    );
    agent
        .session_mut()
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text("perform one irreversible action".into())],
        })))
        .unwrap();
    agent
        .session_mut()
        .append(EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::ToolCall(ToolCall {
                id: ygg_ai::ToolCallId("possibly_committed".into()),
                name: "unsafe_recovery".into(),
                arguments_json: "{}".into(),
            })],
            model: ModelId("scripted".into()),
            protocol: Protocol::AnthropicMessages,
        })))
        .unwrap();

    let output = agent.complete("continue after restart").await.unwrap();
    assert_eq!(output.text, "reconciled");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.to_string().contains("indeterminate after restart"));
}

#[tokio::test]
async fn confirmed_policy_uses_an_isolated_internal_confirmation_turn() {
    let mut h = harness(
        vec![
            text_turn("Completed and verified for the user."),
            text_turn(
                r#"{"complete":true,"reason":"The requested work and verification are complete."}"#,
            ),
        ],
        Some(8),
    )
    .await;
    h.agent.set_completion_policy(CompletionPolicy::Confirmed);

    let output = h.agent.complete("complete autonomously").await.unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert_eq!(output.text, "Completed and verified for the user.");
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["name"] != "finish"));
    assert!(request_has_no_tools(&requests[1]));
    assert!(!requests[1].to_string().contains("\"name\":\"exec\""));
    // Internal JSON is persisted for audit but never added to visible output.
    assert!(h.agent.session().entries().iter().any(|entry| {
        matches!(
            &entry.value,
            EntryValue::Message(Message::Assistant(assistant))
                if assistant.content.iter().any(|part| matches!(part, AssistantPart::Text(text) if text.contains("\"complete\":true")))
        )
    }));
}

#[tokio::test]
async fn rejected_internal_confirmation_returns_to_the_normal_tool_surface() {
    let mut h = harness(
        vec![
            // This is the critical local-model failure mode: a natural stop
            // while narrating the next action must not end the user prompt.
            text_turn("Let me inspect that now."),
            text_turn(r#"{"complete":false,"reason":"The promised inspection has not happened."}"#),
            tool_turn(&[(
                "read_after_reject",
                "read",
                serde_json::json!({"path": "verification.txt"}),
            )]),
            text_turn("Final answer after verification."),
            text_turn(r#"{"complete":true,"reason":"Verification now passes."}"#),
        ],
        Some(8),
    )
    .await;
    std::fs::write(h.workspace.join("verification.txt"), "verified\n").unwrap();
    h.agent.set_completion_policy(CompletionPolicy::Confirmed);

    let output = h.agent.complete("complete and verify").await.unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    assert_eq!(
        output.text,
        "Let me inspect that now.Final answer after verification."
    );
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 5);
    assert!(request_has_no_tools(&requests[1]));
    assert!(requests[2]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "read"));
    assert!(!requests[2].to_string().contains("\"name\":\"finish\""));
}

#[tokio::test]
async fn prompt_with_media_persists_media_user_part() {
    use ygg_agent::{InputPart, UserInput};
    use ygg_ai::{Media, Message, UserPart};

    let mut h = harness(vec![text_turn("seen")], Some(8)).await;
    let input = UserInput::from(vec![
        InputPart::Text("what is in this image?".into()),
        InputPart::Media(Media::image_bytes(
            bytes::Bytes::from_static(&[0x89, 0x50, 0x4e, 0x47]),
            "image/png".parse().unwrap(),
        )),
    ]);
    let mut run = h.agent.prompt(input).await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
    drop(run);

    // The first session entry is the user message with both parts.
    let session = h.agent.session();
    let mut entries = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let entry = session.entry(&id).unwrap();
        entries.push(entry);
        cursor = entry.parent.clone();
    }
    entries.reverse();
    let EntryValue::Message(Message::User(user)) = &entries[0].value else {
        panic!("first entry is not a user message");
    };
    assert_eq!(user.content.len(), 2);
    assert!(matches!(&user.content[0], UserPart::Text(t) if t == "what is in this image?"));
    assert!(matches!(&user.content[1], UserPart::Media(Media::Image(_))));
}

#[tokio::test]
async fn reasoning_and_text_deltas_use_distinct_channels() {
    let body = msg_start()
        + &thinking_block(0, "pondering deeply")
        + &text_block(1, &["Hello", " world"])
        + &msg_end("end_turn");
    let mut h = harness(vec![body], Some(8)).await;

    let mut run = h.agent.prompt("hi").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);

    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "pondering deeply");
    assert_eq!(text, "Hello world");
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
}

#[tokio::test]
async fn one_tool_call_executes_and_persists() {
    let mut h = harness(
        vec![
            tool_turn(&[("call_1", "read", serde_json::json!({"path": "foo.txt"}))]),
            text_turn("done"),
        ],
        Some(8),
    )
    .await;
    std::fs::write(h.workspace.join("foo.txt"), "alpha\nbeta\n").unwrap();

    let mut run = h.agent.prompt("read foo").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);

    // ToolStarted carries the parsed args; ToolFinished the bounded output.
    let started = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolStarted { name, args, .. } if name == "read" => Some(args.clone()),
            _ => None,
        })
        .expect("ToolStarted for read");
    assert_eq!(started["path"], "foo.txt");
    let finished = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolFinished { result, .. } => Some(result),
            _ => None,
        })
        .expect("ToolFinished");
    let output = finished.as_ref().expect("read must succeed");
    assert!(output.text.contains("1: alpha"), "{}", output.text);
    assert!(output.text.contains("hash="), "{}", output.text);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));

    // Persistence: user, assistant(tool call), tool result, final assistant.
    let session = h.agent.session();
    assert_eq!(session.entries().len(), 4);
    match &events[events.len() - 1] {
        AgentEvent::RunFinished { head, .. } => assert_eq!(session.head(), Some(head.clone())),
        _ => unreachable!(),
    }

    // The second wire request must carry the tool result back to the model.
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 2);
    let last_message = requests[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last_message["role"], "user");
    assert_eq!(count_tool_results(last_message), 1);
    assert!(
        requests[1].to_string().contains("1: alpha"),
        "tool output must reach the model"
    );
}

#[tokio::test]
async fn multiple_sequential_tool_calls_execute_in_order_and_coalesce() {
    let mut h = harness(
        vec![
            tool_turn(&[
                ("call_a", "read", serde_json::json!({"path": "a.txt"})),
                ("call_b", "read", serde_json::json!({"path": "b.txt"})),
            ]),
            text_turn("ok"),
        ],
        Some(8),
    )
    .await;
    std::fs::write(h.workspace.join("a.txt"), "AAA\n").unwrap();
    std::fs::write(h.workspace.join("b.txt"), "BBB\n").unwrap();

    let mut run = h.agent.prompt("read both").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));

    // Sequential, emitted order: started(a), finished(a), started(b), finished(b).
    let tool_order: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolStarted { id, .. } => Some(format!("start:{}", id.0)),
            AgentEvent::ToolFinished { id, .. } => Some(format!("finish:{}", id.0)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_order,
        vec![
            "start:call_a",
            "finish:call_a",
            "start:call_b",
            "finish:call_b"
        ]
    );

    // Each result is its own session entry (user, assistant, 2 results, assistant).
    assert_eq!(h.agent.session().entries().len(), 5);

    // On the wire the two persisted results coalesce into ONE user message.
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    let last_message = requests[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last_message["role"], "user");
    assert_eq!(count_tool_results(last_message), 2);
}

#[tokio::test]
async fn tool_errors_and_unknown_tools_return_to_the_model() {
    let mut h = harness(
        vec![
            tool_turn(&[("call_1", "no_such_tool", serde_json::json!({}))]),
            tool_turn(&[("call_2", "read", serde_json::json!({"path": "missing.txt"}))]),
            text_turn("recovered"),
        ],
        Some(8),
    )
    .await;

    let output = h.agent.complete("try tools").await.unwrap();
    assert_eq!(output.text, "recovered");
    assert!(matches!(output.reason, FinishReason::Completed));

    // Both failures went back as is_error tool results, not run failures.
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 3);
    let unknown = requests[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(unknown["content"][0]["type"], "tool_result");
    assert_eq!(unknown["content"][0]["is_error"], true);
    assert!(
        unknown.to_string().contains("unknown tool: no_such_tool"),
        "{unknown}"
    );
    let failed_read = requests[2]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(failed_read["content"][0]["is_error"], true);
}

#[tokio::test]
async fn steering_enters_at_the_next_turn_boundary() {
    let mut h = harness(
        vec![
            tool_turn(&[("call_1", "read", serde_json::json!({"path": "f.txt"}))]),
            text_turn("steered answer"),
        ],
        Some(8),
    )
    .await;
    std::fs::write(h.workspace.join("f.txt"), "data\n").unwrap();

    let mut run = h.agent.prompt("start").await.unwrap();
    let control = run.control();
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        // Queue multiple steers mid-run, right after the tool finishes; the
        // texts must enter together before the *next* model turn.
        if matches!(&event, AgentEvent::ToolFinished { .. }) {
            control.steer("also check the docs").await.unwrap();
            control.steer("and run the tests").await.unwrap();
        }
        events.push(event);
    }
    drop(run);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));

    // The steers are persisted together and included in the second request.
    let delivered = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SteeringDelivered { messages } => Some(messages),
            _ => None,
        })
        .expect("steering delivery event");
    assert_eq!(
        delivered,
        &vec![
            "also check the docs".to_owned(),
            "and run the tests".to_owned()
        ]
    );
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1].to_string().contains("also check the docs"));
    assert!(requests[1].to_string().contains("and run the tests"));
    let session_texts = format!("{:?}", h.agent.session().entries());
    assert!(session_texts.contains("also check the docs"));
    assert!(session_texts.contains("and run the tests"));
}

#[tokio::test]
async fn follow_up_begins_after_the_run_settles() {
    let mut h = harness(vec![text_turn("first"), text_turn("second")], Some(8)).await;

    let mut run = h.agent.prompt("question one").await.unwrap();
    let control = run.control();
    control.follow_up("question two").await.unwrap();

    let events = collect(&mut run).await;
    drop(run);

    // Two model turns on one run; exactly one RunFinished.
    let turns = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnFinished { .. }))
        .count();
    assert_eq!(turns, 2);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));

    // The follow-up became a persisted user message and the second request
    // contains it (after the first answer).
    assert_eq!(h.agent.session().entries().len(), 4);
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1].to_string().contains("question two"));
    assert!(requests[1].to_string().contains("first"));
}

#[tokio::test]
async fn abort_during_model_streaming_finishes_once_and_preserves_entries() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A raw server that streams one delta and then stalls forever.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let server_task = tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 16384];
            let mut read = 0;
            loop {
                let n = socket.read(&mut buf[read..]).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                read += n;
                if String::from_utf8_lossy(&buf[..read]).contains("\r\n\r\n") {
                    break;
                }
            }
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            socket.write_all(headers.as_bytes()).await.unwrap();
            let partial = msg_start()
                + &frame(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "text", "text": ""}
                    }),
                )
                + &frame(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": "partial"}
                    }),
                );
            let chunk = format!("{:x}\r\n{partial}\r\n", partial.len());
            socket.write_all(chunk.as_bytes()).await.unwrap();
            // Stall: never finish the stream.
            tokio::time::sleep(Duration::from_secs(120)).await;
        }
    });

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let mut agent = build_agent(&uri, &workspace, &session_path, Some(8));

    let mut run = agent.prompt("stream forever").await.unwrap();
    let control = run.control();
    let mut events = Vec::new();
    let started = std::time::Instant::now();
    while let Some(event) = run.next().await {
        if matches!(&event, AgentEvent::OutputDelta { .. }) {
            control.abort();
        }
        events.push(event);
    }
    drop(run);

    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "abort must not wait for the stalled stream"
    );
    // The incomplete assistant turn was never persisted; the user entry was.
    assert_eq!(agent.session().entries().len(), 1);
    server_task.abort();
}

#[tokio::test]
async fn abort_during_process_execution_kills_the_tool() {
    let mut h = harness(
        vec![
            tool_turn(&[("call_1", "exec", serde_json::json!({"command": "sleep 60"}))]),
            text_turn("never reached"),
        ],
        Some(8),
    )
    .await;

    let mut run = h.agent.prompt("run something slow").await.unwrap();
    let control = run.control();
    let mut events = Vec::new();
    let started = std::time::Instant::now();
    while let Some(event) = run.next().await {
        if matches!(&event, AgentEvent::ToolStarted { .. }) {
            control.abort();
        }
        events.push(event);
    }
    drop(run);

    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "abort must cancel the child, not wait for it"
    );
    // Controlled aborts persist an explicit cancellation result so reopening
    // the session does not mistake the deliberate stop for a crash.
    assert_eq!(h.agent.session().entries().len(), 3);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolFinished {
            result: Err(error), ..
        } if error.message.contains("cancelled by user")
    )));
}

#[tokio::test]
async fn abort_then_new_prompt_does_not_replay_cancelled_tool() {
    let mut h = harness(
        vec![
            tool_turn(&[(
                "edit_1",
                "write",
                serde_json::json!({
                    "path": "must-not-exist.txt",
                    "content": "aborted side effect"
                }),
            )]),
            text_turn("new prompt handled"),
        ],
        Some(8),
    )
    .await;

    let mut run = h.agent.prompt("make the file").await.unwrap();
    let control = run.control();
    while let Some(event) = run.next().await {
        if matches!(event, AgentEvent::ToolStarted { .. }) {
            control.abort();
        }
    }
    drop(run);
    assert!(!h.workspace.join("must-not-exist.txt").exists());

    let output = h.agent.complete("continue after the abort").await.unwrap();
    assert_eq!(output.text, "new prompt handled");
    assert!(!h.workspace.join("must-not-exist.txt").exists());
}

#[tokio::test]
async fn dropping_run_does_not_replay_an_interrupted_tool() {
    let mut h = harness(
        vec![
            tool_turn(&[(
                "edit_drop",
                "write",
                serde_json::json!({
                    "path": "drop-must-not-run.txt",
                    "content": "dropped side effect"
                }),
            )]),
            text_turn("drop continuation handled"),
        ],
        Some(8),
    )
    .await;
    let mut run = h.agent.prompt("start and then drop").await.unwrap();
    let _ = run.next().await;
    drop(run);
    assert!(!h.workspace.join("drop-must-not-run.txt").exists());
    // Run::drop itself must close the durable tool-call boundary. Do not rely
    // on a later prompt or Agent::drop, because the process can die between
    // those operations.
    assert!(h.agent.session().context().unwrap().iter().any(|message| {
        matches!(message, Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::ToolResult(result) if result.tool_call_id.0 == "edit_drop" && result.is_error)))
    }));
    let concurrently_reopened = Session::open(&h.session_path).unwrap();
    assert!(concurrently_reopened.context().unwrap().iter().any(|message| {
        matches!(message, Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::ToolResult(result) if result.tool_call_id.0 == "edit_drop" && result.is_error)))
    }));

    let session_path = h.session_path.clone();
    let server_uri = h.server.as_ref().unwrap().uri();
    let workspace = h.workspace.clone();
    drop(h.agent);
    let reopened = Session::open(&session_path).unwrap();
    assert!(reopened.context().unwrap().iter().any(|message| {
        matches!(message, Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::ToolResult(result) if result.is_error)))
    }));
    h.agent = build_agent_from_session(
        &server_uri,
        &workspace,
        Session::open(&session_path).unwrap(),
        Some(8),
    );

    let output = h
        .agent
        .complete("continue after dropping the run")
        .await
        .unwrap();
    assert_eq!(output.text, "drop continuation handled");
    assert!(!h.workspace.join("drop-must-not-run.txt").exists());
}

#[tokio::test]
async fn max_turns_terminates_the_run() {
    // The script always answers with another tool call; the guard must stop it.
    let mut h = harness(
        vec![tool_turn(&[(
            "call_loop",
            "read",
            serde_json::json!({"path": "loop.txt"}),
        )])],
        Some(2),
    )
    .await;
    std::fs::write(h.workspace.join("loop.txt"), "again\n").unwrap();

    let mut run = h.agent.prompt("loop forever").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);

    let turns = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnFinished { .. }))
        .count();
    assert_eq!(turns, 2);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::MaxTurns
    ));
}

#[tokio::test]
async fn duplicate_tool_registration_is_rejected() {
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    extensions.load(&CoreTools); // registers every core tool twice

    let result = Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model("http://127.0.0.1:1/"),
        session: Session::create(session_dir.path().join("s.jsonl")).unwrap(),
        system: String::new(),
        sandbox: SandboxConfig::new(workspace_dir.path()),
        extensions,
        max_turns: Some(8),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    });
    match result {
        Err(ygg_agent::AgentError::DuplicateTool(name)) => assert_eq!(name, "read"),
        Err(other) => panic!("expected DuplicateTool, got {other}"),
        Ok(_) => panic!("duplicate registration must be rejected"),
    }
}

/// The key end-to-end invariant: create session → prompt scripted model →
/// execute multiple tools → persist every semantic boundary → complete →
/// reopen session → reconstruct equivalent provider context → checkout an
/// ancestor → continue on a new branch.
#[tokio::test]
async fn end_to_end_session_invariant() {
    let mut h = harness(
        vec![
            tool_turn(&[(
                "call_1",
                "write",
                serde_json::json!({
                    "path": "hello.txt",
                    "content": "hello from the agent\n"
                }),
            )]),
            tool_turn(&[("call_2", "read", serde_json::json!({"path": "hello.txt"}))]),
            text_turn("all done"),
            text_turn("branched"),
        ],
        Some(8),
    )
    .await;

    // Run to completion through two tools.
    let output = h
        .agent
        .complete("create then verify hello.txt")
        .await
        .unwrap();
    assert_eq!(output.text, "all done");
    assert!(matches!(output.reason, FinishReason::Completed));
    assert_eq!(
        std::fs::read_to_string(h.workspace.join("hello.txt")).unwrap(),
        "hello from the agent\n"
    );
    // Every semantic boundary persisted: user, assistant, result, assistant,
    // result, assistant.
    assert_eq!(h.agent.session().entries().len(), 6);

    // Reopen the file independently: identical head and equivalent context.
    let reopened = Session::open(&h.session_path).unwrap();
    assert_eq!(reopened.head(), h.agent.session().head());
    let original_ctx = serde_json::to_value(h.agent.session().context().unwrap()).unwrap();
    let reopened_ctx = serde_json::to_value(reopened.context().unwrap()).unwrap();
    assert_eq!(original_ctx, reopened_ctx);

    // Checkout the first user entry and continue: a new branch forms while
    // the old one is preserved.
    let root = EntryId("001".to_string());
    let old_head = h.agent.session().head().unwrap();
    let entries_before = h.agent.session().entries().len();
    h.agent.session_mut().checkout(root.clone()).unwrap();

    let output = h
        .agent
        .complete("take a different direction")
        .await
        .unwrap();
    assert_eq!(output.text, "branched");

    let session = h.agent.session();
    assert_eq!(session.entries().len(), entries_before + 2);
    // The new user entry forks from the checked-out ancestor…
    let new_user = &session.entries()[entries_before];
    assert_eq!(new_user.parent, Some(root));
    // …the old branch is intact and the head moved to the new branch.
    assert!(session.entry(&old_head).is_some());
    assert_ne!(session.head(), Some(old_head));

    // The branched request must not contain the abandoned branch's messages.
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    let branched_request = requests.last().unwrap().to_string();
    assert!(branched_request.contains("take a different direction"));
    // Nothing from the abandoned branch: no assistant turns, no tool traffic.
    assert!(!branched_request.contains("all done"));
    assert!(!branched_request.contains("tool_result"));
    assert!(!branched_request.contains("tool_use"));

    // And the branch survives reopening: original user, branch user, branch
    // assistant — with the abandoned branch absent from the context.
    drop(h.agent);
    let reopened = Session::open(&h.session_path).unwrap();
    let ctx = serde_json::to_value(reopened.context().unwrap()).unwrap();
    assert_eq!(ctx.as_array().unwrap().len(), 3);
    assert_eq!(ctx[2]["Assistant"]["content"][0]["Text"], "branched");
}

// ── Tool-progress integration tests ──────────────────────────────────────

use bytes::Bytes;

/// A tool that sleeps briefly while emitting progress chunks.
struct ProgressTool {
    duration_ms: u64,
    abortable: bool,
}

#[async_trait::async_trait]
impl Tool for ProgressTool {
    fn definition(&self) -> ygg_ai::ToolDef {
        ygg_ai::ToolDef {
            name: "progress_test".to_string(),
            description: "Emits progress and sleeps".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let chunk_count = 20u64;
        for i in 0..chunk_count {
            if self.abortable {
                tokio::time::sleep(Duration::from_millis(self.duration_ms / chunk_count)).await;
            }
            ctx.progress
                .output(OutputStream::Stdout, Bytes::from(format!("chunk{i}\n")));
        }
        Ok(ToolOutput::new("progress_test done"))
    }
}

struct QueuedActivationTool {
    abort_control: Arc<std::sync::Mutex<Option<RunControl>>>,
}

#[async_trait::async_trait]
impl Tool for QueuedActivationTool {
    fn definition(&self) -> ygg_ai::ToolDef {
        ygg_ai::ToolDef {
            name: "queued_activation".into(),
            description: "Queues a semantic activation event".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let append = ctx.append_session_entry(EntryValue::SkillActivated {
            descriptor: ygg_agent::SkillDescriptor {
                id: "queued-skill".into(),
                name: "Queued Skill".into(),
                description: "Cancellation regression fixture".into(),
                version: None,
                source: ygg_agent::SkillSource::BuiltIn,
                trust: ygg_agent::SkillTrust::BuiltIn,
                required_tools: vec![],
                tags: vec![],
            },
            instructions_hash: "queued-hash".into(),
            instructions: "must not survive cancellation".into(),
        });
        tokio::pin!(append);
        assert!(futures_util::poll!(&mut append).is_pending());
        self.abort_control
            .lock()
            .unwrap()
            .as_ref()
            .expect("test installs abort control before polling the tool")
            .abort();
        append.await?;
        Ok(ToolOutput::new("activated"))
    }
}

struct LargeOutputTool;

#[async_trait::async_trait]
impl Tool for LargeOutputTool {
    fn definition(&self) -> ygg_ai::ToolDef {
        ygg_ai::ToolDef {
            name: "large_output".into(),
            description: "Returns a large result".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::new("x".repeat(10_000)))
    }
}

struct RegisteredToolsProbe {
    observed: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Tool for RegisteredToolsProbe {
    fn definition(&self) -> ygg_ai::ToolDef {
        ygg_ai::ToolDef {
            name: "registered_tools_probe".into(),
            description: "Records the final registered tool set".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        *self.observed.lock().unwrap() = ctx.registered_tools.to_vec();
        Ok(ToolOutput::new("recorded"))
    }
}

#[tokio::test]
async fn tool_context_sees_the_exact_post_filter_core_and_extension_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![
                tool_turn(&[(
                    "call_registered",
                    "registered_tools_probe",
                    serde_json::json!({}),
                )]),
                text_turn("done"),
            ],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    extensions.tool(RegisteredToolsProbe {
        observed: Arc::clone(&observed),
    });
    extensions.retain_tools(|name| matches!(name, "read" | "registered_tools_probe"));
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model(&server.uri()),
        session: Session::create(session_dir.path().join("session.jsonl")).unwrap(),
        system: "test".into(),
        sandbox: SandboxConfig::new(&workspace),
        extensions,
        max_turns: Some(4),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap();

    let output = agent.complete("inspect tools").await.unwrap();

    assert!(matches!(output.reason, FinishReason::Completed));
    assert_eq!(
        *observed.lock().unwrap(),
        vec!["read".to_string(), "registered_tools_probe".to_string()]
    );
    assert_eq!(
        agent.registered_tool_names(),
        vec!["read".to_string(), "registered_tools_probe".to_string()]
    );
}

struct CountingRecoveryTool {
    calls: Arc<AtomicUsize>,
}

struct UnsafeRecoveryTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for UnsafeRecoveryTool {
    fn definition(&self) -> ygg_ai::ToolDef {
        ygg_ai::ToolDef {
            name: "unsafe_recovery".into(),
            description: "Represents an irreversible external mutation".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::new("mutated"))
    }
}

#[async_trait::async_trait]
impl Tool for CountingRecoveryTool {
    fn definition(&self) -> ygg_ai::ToolDef {
        ygg_ai::ToolDef {
            name: "count_recovery".into(),
            description: "Counts crash-recovery executions".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Safe
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::new("executed"))
    }
}

fn scripted_tool_turn(tool_name: &str) -> String {
    msg_start() + &tool_block(0, "call_p", tool_name, &serde_json::json!({})) + &msg_end("tool_use")
}

fn build_agent_with_extra_tool(
    uri: &str,
    workspace: &Path,
    session_path: &Path,
    max_turns: Option<u64>,
    tool: impl Tool + 'static,
) -> Agent {
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    extensions.tool(tool);
    let mut sandbox = SandboxConfig::new(workspace);
    sandbox.allow_edit = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model(uri),
        session: Session::create(session_path).unwrap(),
        system: "You are a scripted test agent.".to_string(),
        sandbox,
        extensions,
        max_turns,
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap()
}

#[tokio::test]
async fn crash_recovery_preserves_the_live_tool_call_execution_cap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![text_turn("recovered")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("recovery-cap.jsonl");
    let mut session = Session::create(&session_path).unwrap();
    session
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text("persisted request".into())],
        })))
        .unwrap();
    session
        .append(EntryValue::Message(Message::Assistant(AssistantMessage {
            content: (0..35)
                .map(|index| {
                    AssistantPart::ToolCall(ToolCall {
                        id: ygg_ai::ToolCallId(format!("recover-{index}")),
                        name: "count_recovery".into(),
                        arguments_json: "{}".into(),
                    })
                })
                .collect(),
            model: ModelId("scripted".into()),
            protocol: Protocol::AnthropicMessages,
        })))
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    extensions.tool(CountingRecoveryTool {
        calls: Arc::clone(&calls),
    });
    let mut sandbox = SandboxConfig::new(&workspace);
    sandbox.allow_edit = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model(&server.uri()),
        session,
        system: "test".into(),
        sandbox,
        extensions,
        max_turns: Some(2),
        reasoning: ReasoningConfig::Off,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: None,
    })
    .unwrap();

    let output = agent.complete("continue").await.unwrap();
    assert_eq!(output.text, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 32);
    let results = agent
        .session()
        .entries()
        .iter()
        .filter_map(|entry| match &entry.value {
            EntryValue::Message(Message::User(user)) => user.content.iter().find_map(|part| {
                let UserPart::ToolResult(result) = part else {
                    return None;
                };
                Some(result)
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 35);
    for index in 32..35 {
        let id = format!("recover-{index}");
        let result = results
            .iter()
            .find(|result| result.tool_call_id.0 == id)
            .unwrap();
        assert!(result.is_error);
        assert!(matches!(
            result.content.first(),
            Some(ygg_ai::ToolResultPart::Text(text)) if text.contains("per-turn tool-call limit")
        ));
    }
}

#[tokio::test]
async fn batched_tool_results_keep_independent_bounded_outputs() {
    let h = harness(
        vec![
            tool_turn(&[
                ("large_a", "large_output", serde_json::json!({})),
                ("large_b", "large_output", serde_json::json!({})),
                ("large_c", "large_output", serde_json::json!({})),
            ]),
            text_turn("done"),
        ],
        Some(8),
    )
    .await;
    // Use a separate session file for the extension-enabled agent while
    // reusing the scripted server.
    let session_path = h.session_path.with_file_name("large-output.jsonl");
    let server_uri = h.server.as_ref().unwrap().uri();
    let mut agent = build_agent_with_extra_tool(
        &server_uri,
        &h.workspace,
        &session_path,
        Some(8),
        LargeOutputTool,
    );
    let output = agent.complete("produce bounded output").await.unwrap();
    assert!(matches!(output.reason, FinishReason::Completed));
    let requests = wire_requests(h.server.as_ref().unwrap()).await;
    let results = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap();
    let text_bytes: usize = results
        .iter()
        .map(|result| result["content"][0]["text"].as_str().unwrap().len())
        .sum();
    assert_eq!(text_bytes, 30_000);
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|result| {
        result["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.len() == 10_000)
    }));
}

#[tokio::test]
async fn tool_progress_events_arrive_between_start_and_finish() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![scripted_tool_turn("progress_test"), text_turn("done")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");

    let tool = ProgressTool {
        duration_ms: 200,
        abortable: true,
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    let mut run = agent.prompt("test progress").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));

    // Verify ordering: ToolStarted first, then ToolProgress*, then ToolFinished.
    let mut saw_start = false;
    let mut saw_progress = false;
    let mut saw_finish = false;
    let mut last_was_finish = false;
    for event in &events {
        match event {
            AgentEvent::ToolStarted { name, .. } if name == "progress_test" => {
                assert!(!saw_start, "duplicate ToolStarted");
                assert!(!saw_finish);
                saw_start = true;
            }
            AgentEvent::ToolProgress { .. } => {
                assert!(saw_start, "ToolProgress before ToolStarted");
                assert!(!saw_finish, "ToolProgress after ToolFinished");
                assert!(!last_was_finish);
                saw_progress = true;
            }
            AgentEvent::ToolFinished { id, .. } if id.0 == "call_p" => {
                assert!(saw_start, "ToolFinished before ToolStarted");
                assert!(!saw_finish, "duplicate ToolFinished");
                saw_finish = true;
                last_was_finish = true;
            }
            _ => {}
        }
    }
    assert!(saw_start);
    assert!(saw_progress);
    assert!(saw_finish);
}

#[tokio::test]
async fn abort_before_completion_persists_cancellation_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![scripted_tool_turn("progress_test"), text_turn("never")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");

    let tool = ProgressTool {
        duration_ms: 5000, // long enough to abort
        abortable: true,
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    let mut run = agent.prompt("test abort").await.unwrap();
    let control = run.control();
    let mut events = Vec::new();
    let started = std::time::Instant::now();
    while let Some(event) = run.next().await {
        if matches!(&event, AgentEvent::ToolStarted { name, .. } if name == "progress_test") {
            control.abort();
        }
        events.push(event);
    }
    drop(run);

    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "abort must cancel, not wait"
    );
    // The controlled abort is itself reported and persisted as a result.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolFinished {
            id,
            result: Err(error),
        } if id.0 == "call_p" && error.message.contains("cancelled by user")
    )));
    // One cancellation result persisted.
    let session_entries = agent.session().entries().to_vec();
    let result_count = session_entries
        .iter()
        .filter(|e| {
            matches!(&e.value, EntryValue::Message(
            ygg_ai::Message::User(ygg_ai::UserMessage { content, .. })
        ) if content.iter().any(|p| matches!(p, ygg_ai::UserPart::ToolResult(_))))
        })
        .count();
    assert_eq!(
        result_count, 1,
        "controlled abort must persist cancellation"
    );
}

#[tokio::test]
async fn cancellation_discards_a_queued_semantic_session_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![scripted_tool_turn("queued_activation"), text_turn("never")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    let abort_control = Arc::new(std::sync::Mutex::new(None));
    let tool = QueuedActivationTool {
        abort_control: abort_control.clone(),
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    let mut run = agent.prompt("test semantic cancellation").await.unwrap();
    *abort_control.lock().unwrap() = Some(run.control());
    let events = collect(&mut run).await;
    drop(run);

    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    assert!(!agent
        .session()
        .entries()
        .iter()
        .any(|entry| matches!(entry.value, EntryValue::SkillActivated { .. })));
}

#[tokio::test]
async fn abort_after_completion_preserves_result_and_emits_finished() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![
                tool_turn(&[("call_r", "read", serde_json::json!({"path": "f.txt"}))]),
                scripted_tool_turn("progress_test"),
                text_turn("never_reached"),
            ],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");
    std::fs::write(workspace.join("f.txt"), "data\n").unwrap();

    let tool = ProgressTool {
        duration_ms: 100, // fast: completes before abort
        abortable: true,
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    let mut run = agent.prompt("test post-completion abort").await.unwrap();
    let control = run.control();
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        // Abort AFTER the progress tool's ToolFinished.
        if matches!(&event, AgentEvent::ToolFinished { id, .. } if id.0 == "call_p") {
            control.abort();
        }
        events.push(event);
    }
    drop(run);

    // Run ends as Aborted (no subsequent model turn).
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Aborted
    ));
    // But the progress_test tool DID finish before abort.
    let progress_finished = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolFinished { id, .. } if id.0 == "call_p"
        )
    });
    assert!(
        progress_finished,
        "ToolFinished must be emitted before abort stops the run"
    );
    // And its result was persisted.
    let session_entries = agent.session().entries().to_vec();
    let has_progress_result = session_entries.iter().any(|e| {
        matches!(&e.value, EntryValue::Message(_))
            && format!("{:?}", e).contains("progress_test done")
    });
    assert!(has_progress_result, "committed result must survive abort");
    // No subsequent model request was made.
    let requests = wire_requests(&server).await;
    assert_eq!(
        requests.len(),
        2,
        "third request (never_reached) must not fire"
    );
}

#[tokio::test]
async fn steer_arrives_during_continuous_progress() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![scripted_tool_turn("progress_test"), text_turn("steered")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");

    let tool = ProgressTool {
        duration_ms: 500, // enough time for steer to arrive
        abortable: true,
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    let mut run = agent.prompt("test steer").await.unwrap();
    let control = run.control();
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        if matches!(&event, AgentEvent::ToolStarted { name, .. } if name == "progress_test") {
            // Send steer during tool execution.
            control.steer("redirect").await.unwrap();
        }
        events.push(event);
    }
    drop(run);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
    // The steered text must have been persisted and sent in the second request.
    let requests = wire_requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].to_string().contains("redirect"),
        "steer must reach the model"
    );
}

#[tokio::test]
async fn progress_never_persisted_in_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![scripted_tool_turn("progress_test"), text_turn("ok")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");

    let tool = ProgressTool {
        duration_ms: 50,
        abortable: false,
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    agent.complete("test").await.unwrap();

    // Read the raw session file — no progress keywords.
    let raw = std::fs::read_to_string(&session_path).unwrap();
    assert!(!raw.contains("ToolProgress"));
    assert!(!raw.contains("chunk"));
    assert!(raw.contains("progress_test done"));
}

#[tokio::test]
async fn multiple_tools_have_isolated_progress() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![
                tool_turn(&[
                    ("call_a", "progress_test", serde_json::json!({})),
                    ("call_b", "progress_test", serde_json::json!({})),
                ]),
                text_turn("all_done"),
            ],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("session.jsonl");

    let tool = ProgressTool {
        duration_ms: 100,
        abortable: false,
    };
    let mut agent =
        build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, Some(8), tool);

    let mut run = agent.prompt("test isolation").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));

    // All ToolProgress events for call_a must appear before ToolStarted for call_b.
    let mut call_a_done = false;
    for event in &events {
        match event {
            AgentEvent::ToolStarted { id, .. } if id.0 == "call_b" => {
                call_a_done = true;
            }
            AgentEvent::ToolProgress { id, .. } if id.0 == "call_a" => {
                assert!(!call_a_done, "call_a progress after call_b started");
            }
            AgentEvent::ToolProgress { id, .. } if id.0 == "call_b" => {
                assert!(call_a_done, "call_b progress before call_b ToolStarted");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn torn_session_and_crash_recovery_still_green() {
    // Sanity: the session invariants still hold with progress infrastructure.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");

    let mut s = Session::create(&path).unwrap();
    let e1 = s
        .append(EntryValue::Message(ygg_ai::Message::User(
            ygg_ai::UserMessage {
                content: vec![ygg_ai::UserPart::Text("hello".to_string())],
            },
        )))
        .unwrap();
    drop(s);

    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.head(), Some(e1));
}

// ── Reasoning configuration ───────────────────────────────────────────────
//
// `Agent::prompt` must thread the configured `ReasoningConfig` into every
// `ygg_ai::Request` instead of hardcoding `ReasoningConfig::Off`. These tests
// pin that behavior end-to-end against the real request-build + SSE path.

/// `thinking` in the outgoing Anthropic body proves the request carried a
/// non-off reasoning budget; its absence proves the opposite.
fn request_has_thinking(request: &serde_json::Value) -> bool {
    request.get("thinking").is_some()
}

#[tokio::test]
async fn reasoning_off_omits_thinking_from_the_request() {
    // A reasoning-capable model with reasoning explicitly OFF must not send a
    // `thinking` block: the config, not the capability, gates it.
    let (mut agent, server, _path, _dirs) =
        reasoning_harness(vec![text_turn("hi")], ReasoningConfig::Off, true).await;

    let output = agent.complete("hello").await.unwrap();
    assert_eq!(output.text, "hi");

    let requests = wire_requests(&server).await;
    assert_eq!(requests.len(), 1);
    assert!(
        !request_has_thinking(&requests[0]),
        "reasoning off must omit `thinking`: {}",
        requests[0]
    );
}

#[tokio::test]
async fn non_off_reasoning_reaches_the_provider_request() {
    let (mut agent, server, _path, _dirs) =
        reasoning_harness(vec![text_turn("hi")], ReasoningConfig::Budget(2048), true).await;

    agent.complete("hello").await.unwrap();

    let requests = wire_requests(&server).await;
    assert_eq!(requests.len(), 1);
    let thinking = requests[0]
        .get("thinking")
        .expect("reasoning budget must reach the wire");
    assert_eq!(thinking["type"], "enabled");
    assert_eq!(thinking["budget_tokens"], 2048);
}

#[tokio::test]
async fn large_reasoning_budget_raises_the_agent_output_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(Script {
            bodies: vec![text_turn("hi")],
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let session_path = session_dir.path().join("large-reasoning.jsonl");
    let base = scripted_model_with_reasoning(&server.uri());
    let mut spec = (*base.spec).clone();
    spec.limits.max_output_tokens = 65_536;
    let capability = spec.capabilities.reasoning.as_mut().unwrap();
    capability.effort_budgets = Some(ReasoningEffortBudgets {
        minimal: 1024,
        low: 2048,
        medium: 4096,
        high: 32_768,
        xhigh: 32_768,
        max: 32_768,
    });
    let model = Model {
        spec: Arc::new(spec),
        endpoint: base.endpoint,
    };
    let mut agent = build_agent_with_reasoning(
        model,
        &session_path,
        &workspace,
        ReasoningConfig::Budget(32_768),
        Some(2),
    );

    agent.complete("hello").await.unwrap();
    let requests = wire_requests(&server).await;
    assert_eq!(requests[0]["thinking"]["budget_tokens"], 32_768);
    assert_eq!(requests[0]["max_tokens"], 33_792);
}

#[tokio::test]
async fn reasoning_deltas_stream_on_the_reasoning_channel_when_enabled() {
    let body = msg_start()
        + &thinking_block(0, "weighing options")
        + &text_block(1, &["Answer", " here"])
        + &msg_end("end_turn");
    let (mut agent, _server, _path, _dirs) =
        reasoning_harness(vec![body], ReasoningConfig::Budget(2048), true).await;

    let mut run = agent.prompt("go").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);

    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "weighing options");
    assert_eq!(text, "Answer here");

    // Ordering: reasoning deltas precede the text deltas.
    let first_reasoning = events.iter().position(|e| {
        matches!(
            e,
            AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                ..
            }
        )
    });
    let first_text = events.iter().position(|e| {
        matches!(
            e,
            AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                ..
            }
        )
    });
    assert!(first_reasoning < first_text, "reasoning must precede text");
    assert!(matches!(
        assert_single_run_finished(&events),
        FinishReason::Completed
    ));
}

#[tokio::test]
async fn complete_with_reasoning_enabled_returns_only_visible_text() {
    let body = msg_start()
        + &thinking_block(0, "internal deliberation")
        + &text_block(1, &["final answer"])
        + &msg_end("end_turn");
    let (mut agent, _server, _path, _dirs) =
        reasoning_harness(vec![body], ReasoningConfig::Budget(4096), true).await;

    let output = agent.complete("question").await.unwrap();
    // Reasoning text must not leak into the aggregate visible text.
    assert_eq!(output.text, "final answer");
    assert!(matches!(output.reason, FinishReason::Completed));
}

#[tokio::test]
async fn unsupported_reasoning_fails_through_ygg_ai_validation() {
    // A model WITHOUT a reasoning capability plus a non-off config must fail via
    // `ygg-ai`'s strict validation, surfacing as a failed run — never silently
    // disabled.
    let (mut agent, server, _path, _dirs) = reasoning_harness(
        vec![text_turn("unused")],
        ReasoningConfig::Budget(2048),
        false,
    )
    .await;

    let mut run = agent.prompt("go").await.unwrap();
    let events = collect(&mut run).await;
    drop(run);

    assert!(
        matches!(assert_single_run_finished(&events), FinishReason::Failed(_)),
        "unsupported reasoning must fail the run, not be silently dropped"
    );
    // No provider request should have been accepted (validation fails pre-send).
    let requests = wire_requests(&server).await;
    assert!(
        requests.is_empty(),
        "request must be rejected before send: {requests:?}"
    );
    // No provider-produced assistant turn was persisted, but the failed user
    // turn is closed by the durable synthetic boundary used on the next run.
    assert_eq!(agent.session().entries().len(), 2);
    assert!(matches!(
        &agent.session().entries()[1].value,
        EntryValue::Message(Message::Assistant(message))
            if message.content.iter().any(|part| matches!(
                part,
                AssistantPart::Text(text) if text.contains("failed before completion")
            ))
    ));
}

#[tokio::test]
async fn complete_surfaces_unsupported_reasoning_as_err() {
    let (mut agent, _server, _path, _dirs) = reasoning_harness(
        vec![text_turn("unused")],
        ReasoningConfig::Budget(2048),
        false,
    )
    .await;

    let result = agent.complete("go").await;
    assert!(
        matches!(result, Err(ygg_agent::AgentError::Ai(_))),
        "complete must return the ygg-ai error, got {result:?}"
    );
}
