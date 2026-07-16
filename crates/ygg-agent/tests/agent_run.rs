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
    Agent, AgentConfig, AgentEvent, CoreTools, EntryId, EntryValue, ExtensionHost, FinishReason,
    OutputChannel, OutputStream, SandboxConfig, Session, Tool, ToolContext, ToolError, ToolOutput,
};
use ygg_ai::{
    AiClient, Auth, Capabilities, Endpoint, EndpointId, Modality, ModalitySet, Model, ModelId,
    ModelLimits, ModelSpec, Protocol, ReasoningCapability, ReasoningConfig, ReasoningControl,
    ReasoningEffortBudgets,
};

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

fn scripted_model(uri: &str) -> Model {
    Model {
        spec: Arc::new(ModelSpec {
            id: ModelId("scripted".to_string()),
            endpoint: EndpointId("test".to_string()),
            api_name: "scripted-model".to_string(),
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
            timeout: Duration::from_secs(10),
        }),
    }
}

struct Harness {
    agent: Agent,
    server: Option<MockServer>,
    session_path: PathBuf,
    workspace: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

fn build_agent(uri: &str, workspace: &Path, session_path: &Path, max_turns: u64) -> Agent {
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
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
        effort_budgets: Some(ReasoningEffortBudgets {
            minimal: 1024,
            low: 2048,
            medium: 4096,
            high: 8192,
        }),
        openai_chat_mode: ygg_ai::OpenAiChatReasoningMode::Standard,
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
    max_turns: u64,
) -> Agent {
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(workspace);
    sandbox.allow_edit = true;
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
    let agent = build_agent_with_reasoning(model, &session_path, &workspace, reasoning, 8);
    (agent, server, session_path, (workspace_dir, session_dir))
}

async fn harness(bodies: Vec<String>, max_turns: u64) -> Harness {
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn text_only_completion() {
    let mut h = harness(vec![text_turn("Hello world")], 8).await;
    let output = h.agent.complete("hi").await.unwrap();

    assert_eq!(output.text, "Hello world");
    assert!(matches!(output.reason, FinishReason::Completed));
    assert!(output.usage.output_tokens > 0, "usage must propagate");

    // User + assistant persisted, head on the assistant message.
    let session = h.agent.session();
    assert_eq!(session.entries().len(), 2);
    assert_eq!(session.head(), Some(output.head));
}

#[tokio::test]
async fn reasoning_and_text_deltas_use_distinct_channels() {
    let body = msg_start()
        + &thinking_block(0, "pondering deeply")
        + &text_block(1, &["Hello", " world"])
        + &msg_end("end_turn");
    let mut h = harness(vec![body], 8).await;

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
        8,
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
        8,
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
        8,
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
        8,
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
    let mut h = harness(vec![text_turn("first"), text_turn("second")], 8).await;

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
    let mut agent = build_agent(&uri, &workspace, &session_path, 8);

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
            tool_turn(&[(
                "call_1",
                "exec",
                serde_json::json!({"mode": "process", "program": "sleep", "args": ["60"]}),
            )]),
            text_turn("never reached"),
        ],
        8,
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
    // Completed entries preserved: user + assistant (the aborted tool's
    // result was never produced, so no result entry exists).
    assert_eq!(h.agent.session().entries().len(), 2);
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolFinished { .. })));
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
        2,
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
        max_turns: 8,
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
                "edit",
                serde_json::json!({
                    "operation": "create",
                    "path": "hello.txt",
                    "content": "hello from the agent\n"
                }),
            )]),
            tool_turn(&[("call_2", "read", serde_json::json!({"path": "hello.txt"}))]),
            text_turn("all done"),
            text_turn("branched"),
        ],
        8,
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

fn scripted_tool_turn(tool_name: &str) -> String {
    msg_start() + &tool_block(0, "call_p", tool_name, &serde_json::json!({})) + &msg_end("tool_use")
}

fn build_agent_with_extra_tool(
    uri: &str,
    workspace: &Path,
    session_path: &Path,
    max_turns: u64,
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
    let mut agent = build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, 8, tool);

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
async fn abort_before_completion_persists_no_result() {
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
    let mut agent = build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, 8, tool);

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
    // No ToolFinished for the aborted tool.
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolFinished { id, .. } if id.0 == "call_p"
    )));
    // No tool result persisted.
    let session_entries = agent.session().entries().to_vec();
    let result_count = session_entries
        .iter()
        .filter(|e| {
            matches!(&e.value, EntryValue::Message(
            ygg_ai::Message::User(ygg_ai::UserMessage { content, .. })
        ) if content.iter().any(|p| matches!(p, ygg_ai::UserPart::ToolResult(_))))
        })
        .count();
    assert_eq!(result_count, 0, "aborted tool must not persist");
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
    let mut agent = build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, 8, tool);

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
    let mut agent = build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, 8, tool);

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
    let mut agent = build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, 8, tool);

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
    let mut agent = build_agent_with_extra_tool(&server.uri(), &workspace, &session_path, 8, tool);

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
    // And no assistant turn was persisted.
    assert_eq!(agent.session().entries().len(), 1);
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
