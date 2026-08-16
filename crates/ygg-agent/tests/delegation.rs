#![allow(missing_docs)]

//! End-to-end tests for the bounded V2 delegation runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};
use ygg_agent::{
    Agent, AgentConfig, AgentEvent, CoreTools, DelegationConfig, DelegationLimits, EntryValue,
    ExtensionHost, FinishReason, SandboxConfig, Session,
};
use ygg_ai::{
    AiClient, Auth, Capabilities, Endpoint, EndpointId, Message, ModalitySet, Model, ModelId,
    ModelLimits, ModelSpec, Protocol, ReasoningConfig, ToolResultPart, UserPart,
};

fn frame(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn message_start() -> String {
    frame(
        "message_start",
        serde_json::json!({
            "type": "message_start",
            "message": {"id": "delegation-test", "usage": {"input_tokens": 5, "output_tokens": 0}}
        }),
    )
}

fn message_end(stop_reason: &str) -> String {
    frame(
        "message_delta",
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": {"output_tokens": 3}
        }),
    ) + &frame("message_stop", serde_json::json!({"type": "message_stop"}))
}

fn text_turn(text: &str) -> String {
    message_start()
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
        + &frame(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        )
        + &message_end("end_turn")
}

fn failed_text_turn(text: &str) -> String {
    text_turn(text).replace(&message_end("end_turn"), &message_end("refusal"))
}

fn tool_turn(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut body = message_start();
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        body += &frame(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name}
            }),
        );
        body += &frame(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": arguments.to_string()}
            }),
        );
        body += &frame(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": index}),
        );
    }
    body + &message_end("tool_use")
}

fn response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string(body)
        .insert_header("content-type", "text/event-stream")
}

fn delayed_response(body: String) -> ResponseTemplate {
    response(body).set_delay(Duration::from_secs(30))
}

fn request_system(body: &serde_json::Value) -> String {
    body.get("system")
        .map(serde_json::Value::to_string)
        .unwrap_or_default()
}

fn next_index(counters: &Mutex<BTreeMap<String, usize>>, route: &str) -> usize {
    let mut counters = counters.lock().unwrap();
    let next = counters.entry(route.to_owned()).or_default();
    let index = *next;
    *next += 1;
    index
}

#[derive(Default)]
struct ScriptState {
    counters: Mutex<BTreeMap<String, usize>>,
    requests: Mutex<Vec<serde_json::Value>>,
    unexpected: Mutex<Vec<String>>,
}

impl ScriptState {
    fn record(&self, request: &wiremock::Request) -> serde_json::Value {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("request body must be JSON");
        self.requests.lock().unwrap().push(body.clone());
        body
    }

    fn unexpected(&self, route: &str, index: usize) -> ResponseTemplate {
        self.unexpected
            .lock()
            .unwrap()
            .push(format!("{route}:{index}"));
        response(text_turn(&format!(
            "unexpected script step {route}:{index}"
        )))
    }
}

struct LifecycleScript {
    state: Arc<ScriptState>,
}

impl Respond for LifecycleScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = self.state.record(request);
        let system = request_system(&body);
        let route = if system.contains("You are /root/beta/gamma") {
            "gamma"
        } else if system.contains("You are /root/beta") {
            "beta"
        } else if system.contains("You are /root/alpha") {
            "alpha"
        } else {
            "root"
        };
        let index = next_index(&self.state.counters, route);
        match (route, index) {
            ("root", 0) => response(tool_turn(&[(
                "root-spawn-alpha",
                "spawn_agent",
                serde_json::json!({"task_name": "alpha", "message": "perform the alpha task"}),
            )])),
            ("root", 1) => response(tool_turn(&[(
                "root-wait-alpha-message",
                "wait_agent",
                serde_json::json!({"timeout_ms": 2_000}),
            )])),
            ("root", 2) => response(tool_turn(&[(
                "root-wait-alpha-complete",
                "wait_agent",
                serde_json::json!({"timeout_ms": 2_000}),
            )])),
            ("root", 3) => response(tool_turn(&[(
                "root-list-alpha",
                "list_agents",
                serde_json::json!({}),
            )])),
            ("root", 4) => response(tool_turn(&[(
                "root-send-alpha",
                "send_message",
                serde_json::json!({"target": "/root/alpha", "message": "queued context"}),
            )])),
            ("root", 5) => response(tool_turn(&[(
                "root-follow-alpha",
                "followup_task",
                serde_json::json!({"target": "/root/alpha", "message": "perform the follow-up"}),
            )])),
            ("root", 6) => response(tool_turn(&[(
                "root-wait-alpha-follow",
                "wait_agent",
                serde_json::json!({"timeout_ms": 2_000}),
            )])),
            ("root", 7) => response(tool_turn(&[(
                "root-list-follow",
                "list_agents",
                serde_json::json!({}),
            )])),
            ("root", 8) => response(tool_turn(&[(
                "root-spawn-beta",
                "spawn_agent",
                serde_json::json!({"task_name": "beta", "message": "spawn a grandchild and keep working"}),
            )])),
            ("root", 9) => response(tool_turn(&[(
                "root-wait-beta-message",
                "wait_agent",
                serde_json::json!({"timeout_ms": 2_000}),
            )])),
            ("root", 10) => response(tool_turn(&[(
                "root-interrupt-beta",
                "interrupt_agent",
                serde_json::json!({"target": "/root/beta"}),
            )])),
            ("root", 11) => response(tool_turn(&[(
                "root-wait-beta-interrupt",
                "wait_agent",
                serde_json::json!({"timeout_ms": 2_000}),
            )])),
            ("root", 12) => response(tool_turn(&[(
                "root-list-final",
                "list_agents",
                serde_json::json!({}),
            )])),
            ("root", 13) => response(text_turn("root integrated result")),
            ("alpha", 0) => response(tool_turn(&[(
                "alpha-message-root",
                "send_message",
                serde_json::json!({"target": "root", "message": "alpha evidence"}),
            )])),
            ("alpha", 1) => response(text_turn("alpha initial done")),
            ("alpha", 2) => response(text_turn("alpha follow-up done")),
            ("beta", 0) => response(tool_turn(&[(
                "beta-spawn-gamma",
                "spawn_agent",
                serde_json::json!({"task_name": "gamma", "message": "keep working until cancelled"}),
            )])),
            ("beta", 1) => response(tool_turn(&[(
                "beta-message-root",
                "send_message",
                serde_json::json!({"target": "root", "message": "gamma is running"}),
            )])),
            ("beta", 2) | ("gamma", 0) => delayed_response(text_turn("must be cancelled")),
            _ => self.state.unexpected(route, index),
        }
    }
}

struct LimitsScript {
    state: Arc<ScriptState>,
}

impl Respond for LimitsScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = self.state.record(request);
        let route = if request_system(&body).contains("You are /root/") {
            "child"
        } else {
            "root"
        };
        let index = next_index(&self.state.counters, route);
        match (route, index) {
            ("root", 0) => response(tool_turn(&[
                (
                    "spawn-first",
                    "spawn_agent",
                    serde_json::json!({"task_name": "first", "message": "first bounded task"}),
                ),
                (
                    "spawn-second",
                    "spawn_agent",
                    serde_json::json!({"task_name": "second", "message": "second bounded task"}),
                ),
            ])),
            ("root", 1) => response(tool_turn(&[(
                "wait-winner",
                "wait_agent",
                serde_json::json!({"timeout_ms": 2_000}),
            )])),
            ("root", 2) => response(tool_turn(&[(
                "spawn-third",
                "spawn_agent",
                serde_json::json!({"task_name": "third", "message": "must exceed total"}),
            )])),
            ("root", 3) => response(tool_turn(&[(
                "list-bounded",
                "list_agents",
                serde_json::json!({}),
            )])),
            ("root", 4) => response(text_turn("bounded result")),
            ("child", 0) => response(tool_turn(&[(
                "spawn-too-deep",
                "spawn_agent",
                serde_json::json!({"task_name": "nested", "message": "must exceed depth"}),
            )]))
            .set_delay(Duration::from_millis(150)),
            ("child", 1) => response(text_turn("bounded child done")),
            _ => self.state.unexpected(route, index),
        }
    }
}

struct CancellationScript {
    state: Arc<ScriptState>,
}

impl Respond for CancellationScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = self.state.record(request);
        let route = if request_system(&body).contains("You are /root/slow") {
            "child"
        } else {
            "root"
        };
        let index = next_index(&self.state.counters, route);
        match (route, index) {
            ("root", 0) => response(tool_turn(&[(
                "spawn-slow",
                "spawn_agent",
                serde_json::json!({"task_name": "slow", "message": "wait until cancelled"}),
            )])),
            ("root", 1) | ("child", 0) => delayed_response(text_turn("too late")),
            _ => self.state.unexpected(route, index),
        }
    }
}

struct CompletionCancellationScript {
    state: Arc<ScriptState>,
}

impl Respond for CompletionCancellationScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = self.state.record(request);
        let route = if request_system(&body).contains("You are /root/slow") {
            "child"
        } else {
            "root"
        };
        let index = next_index(&self.state.counters, route);
        match (route, index) {
            ("root", 0) => response(tool_turn(&[(
                "spawn-slow",
                "spawn_agent",
                serde_json::json!({"task_name": "slow", "message": "wait until the root completes"}),
            )])),
            ("root", 1) => response(text_turn("root finished without waiting")),
            ("child", 0) => delayed_response(text_turn("too late")),
            _ => self.state.unexpected(route, index),
        }
    }
}

struct InFlightDeliveryScript {
    state: Arc<ScriptState>,
}

impl Respond for InFlightDeliveryScript {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = self.state.record(request);
        let route = if request_system(&body).contains("You are /root/recover") {
            "child"
        } else {
            "root"
        };
        let index = next_index(&self.state.counters, route);
        match (route, index) {
            ("root", 0) => response(tool_turn(&[(
                "spawn-recover",
                "spawn_agent",
                serde_json::json!({"task_name": "recover", "message": "start the failing task"}),
            )])),
            ("root", 1) => response(tool_turn(&[(
                "send-in-flight",
                "send_message",
                serde_json::json!({
                    "target": "/root/recover",
                    "message": "context that must survive"
                }),
            )]))
            .set_delay(Duration::from_millis(300)),
            ("root", 2) => response(tool_turn(&[(
                "follow-in-flight",
                "followup_task",
                serde_json::json!({
                    "target": "/root/recover",
                    "message": "retry task after failure"
                }),
            )])),
            ("root", 3) | ("root", 4) => response(tool_turn(&[(
                if index == 3 {
                    "wait-in-flight-failure"
                } else {
                    "wait-in-flight-recovery"
                },
                "wait_agent",
                serde_json::json!({"timeout_ms": 3_000}),
            )])),
            ("root", 5) => response(tool_turn(&[(
                "list-in-flight",
                "list_agents",
                serde_json::json!({}),
            )])),
            ("root", 6) => response(text_turn("root observed recovery")),
            ("child", 0) => response(failed_text_turn("initial task failed before delivery"))
                .set_delay(Duration::from_millis(1_500)),
            ("child", 1) => response(text_turn("recovered accepted work")),
            _ => self.state.unexpected(route, index),
        }
    }
}

fn scripted_model(uri: &str) -> Model {
    Model {
        spec: Arc::new(ModelSpec {
            id: ModelId("delegation-scripted".into()),
            endpoint: EndpointId("delegation-test".into()),
            api_name: "delegation-scripted".into(),
            display_name: None,
            protocol: Protocol::AnthropicMessages,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none(),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: false,
            },
            limits: ModelLimits {
                context_window: 200_000,
                max_output_tokens: 8_192,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        }),
        endpoint: Arc::new(Endpoint {
            id: EndpointId("delegation-test".into()),
            base_url: url::Url::parse(uri).unwrap(),
            auth: Auth::bearer("test-key"),
            default_headers: http::HeaderMap::new(),
            transport: ygg_ai::EndpointTransport::Http,
            timeout: Duration::from_secs(60),
        }),
    }
}

struct EnabledAgent {
    agent: Agent,
    team_directory: PathBuf,
    _workspace: tempfile::TempDir,
    _sessions: tempfile::TempDir,
}

fn build_enabled_agent(server: &MockServer, limits: DelegationLimits) -> EnabledAgent {
    let workspace_dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let mut sandbox = SandboxConfig::new(&workspace);
    sandbox.allow_edit = true;
    sandbox.allow_write = true;
    sandbox.allow_process = true;
    sandbox.allow_shell = true;
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model: scripted_model(&server.uri()),
        session: Session::create(session_dir.path().join("root.jsonl")).unwrap(),
        system: "You are a delegation integration test agent.".into(),
        sandbox,
        extensions,
        max_turns: Some(40),
        reasoning: ReasoningConfig::Off,
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        cache_retention: ygg_ai::CacheRetention::Short,
        session_id: Some("delegation-integration".into()),
    })
    .unwrap();
    let mut config = DelegationConfig::new(session_dir.path().join("delegation"));
    config.limits = limits;
    let team_directory = agent.enable_v2_delegation(config).unwrap();
    EnabledAgent {
        agent,
        team_directory,
        _workspace: workspace_dir,
        _sessions: session_dir,
    }
}

async fn mount_script(server: &MockServer, responder: impl Respond + 'static) {
    Mock::given(method("POST"))
        .and(path("messages"))
        .respond_with(responder)
        .mount(server)
        .await;
}

fn tool_results(session: &Session) -> BTreeMap<String, (bool, String)> {
    let mut results = BTreeMap::new();
    for entry in session.entries() {
        let EntryValue::Message(Message::User(message)) = &entry.value else {
            continue;
        };
        for part in &message.content {
            let UserPart::ToolResult(result) = part else {
                continue;
            };
            let text = result
                .content
                .iter()
                .filter_map(|part| match part {
                    ToolResultPart::Text(text) => Some(text.as_str()),
                    ToolResultPart::Media(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            results.insert(result.tool_call_id.0.clone(), (result.is_error, text));
        }
    }
    results
}

fn parse_result(results: &BTreeMap<String, (bool, String)>, id: &str) -> serde_json::Value {
    let (is_error, text) = results
        .get(id)
        .unwrap_or_else(|| panic!("missing result {id}"));
    assert!(!is_error, "{id} failed: {text}");
    serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("invalid result {id}: {error}: {text}"))
}

fn read_provenance(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn wait_for_provenance(
    path: &Path,
    predicate: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let events = read_provenance(path);
            if predicate(&events) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for delegation provenance")
}

fn listed_agent<'a>(value: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    value["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["agent_path"] == path)
        .unwrap_or_else(|| panic!("missing listed agent {path}: {value}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_lifecycle_messaging_followups_interrupts_and_durability() {
    let server = MockServer::start().await;
    let state = Arc::new(ScriptState::default());
    mount_script(
        &server,
        LifecycleScript {
            state: state.clone(),
        },
    )
    .await;

    let mut harness = build_enabled_agent(&server, DelegationLimits::default());
    let provenance_path = harness.team_directory.join("provenance.jsonl");
    let output = harness
        .agent
        .complete("coordinate the scripted work")
        .await
        .unwrap();
    assert_eq!(output.text, "root integrated result");
    assert!(state.unexpected.lock().unwrap().is_empty());

    let results = tool_results(harness.agent.session());
    assert_eq!(
        parse_result(&results, "root-send-alpha")["delivery"],
        "queued"
    );
    assert_eq!(
        parse_result(&results, "root-follow-alpha")["delivery"],
        "new_run"
    );
    for id in [
        "root-wait-alpha-message",
        "root-wait-alpha-complete",
        "root-wait-alpha-follow",
        "root-wait-beta-message",
        "root-wait-beta-interrupt",
    ] {
        assert_ne!(parse_result(&results, id)["timed_out"], true, "{id}");
    }

    let after_follow = parse_result(&results, "root-list-follow");
    let alpha = listed_agent(&after_follow, "/root/alpha");
    assert_eq!(alpha["status"]["state"], "completed");
    assert_eq!(alpha["status"]["output"], "alpha follow-up done");

    let final_list = parse_result(&results, "root-list-final");
    assert_eq!(
        listed_agent(&final_list, "/root/beta")["status"]["state"],
        "interrupted"
    );
    assert_eq!(
        listed_agent(&final_list, "/root/beta/gamma")["status"]["state"],
        "shutdown"
    );

    let requests = state.requests.lock().unwrap();
    let alpha_request = requests
        .iter()
        .find(|request| request_system(request).contains("You are /root/alpha"))
        .expect("alpha request");
    let encoded_alpha_request = alpha_request.to_string();
    assert!(encoded_alpha_request.contains("delegation integration test agent"));
    for tool in [
        "spawn_agent",
        "followup_task",
        "send_message",
        "wait_agent",
        "list_agents",
        "interrupt_agent",
        "read",
    ] {
        assert!(
            encoded_alpha_request.contains(&format!("\"{tool}\"")),
            "{tool}"
        );
    }
    drop(requests);

    let events_before_drop = read_provenance(&provenance_path);
    let spawned = events_before_drop
        .iter()
        .filter(|event| event["event"] == "agent_spawned")
        .collect::<Vec<_>>();
    assert_eq!(spawned.len(), 3);
    let paths = spawned
        .iter()
        .map(|event| event["agent_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        paths,
        BTreeSet::from(["/root/alpha", "/root/beta", "/root/beta/gamma"])
    );
    assert!(events_before_drop.iter().any(|event| {
        event["event"] == "message"
            && event["from"] == "agent-1"
            && event["to"] == "root"
            && event["message"] == "alpha evidence"
    }));
    assert!(events_before_drop.iter().any(|event| {
        event["event"] == "message"
            && event["kind"] == "follow_up"
            && event["message"] == "perform the follow-up"
    }));
    assert!(events_before_drop.iter().any(|event| {
        event["event"] == "interrupt_requested"
            && event["from"] == "root"
            && event["to"] == "agent-2"
    }));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&harness.team_directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&provenance_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let team_directory = harness.team_directory.clone();
    let EnabledAgent {
        agent,
        _workspace: workspace_guard,
        _sessions: session_guard,
        ..
    } = harness;
    drop(agent);
    let events = wait_for_provenance(&provenance_path, |events| {
        events.iter().any(|event| event["event"] == "team_shutdown")
    })
    .await;
    assert!(events.iter().any(|event| event["event"] == "team_shutdown"));

    let child_sessions = spawned
        .iter()
        .map(|event| PathBuf::from(event["session"].as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(child_sessions.len(), 3);
    assert_eq!(child_sessions.iter().collect::<BTreeSet<_>>().len(), 3);
    for path in child_sessions {
        assert!(path.starts_with(&team_directory));
        let session = Session::open(&path).unwrap();
        assert!(
            !session.entries().is_empty(),
            "empty child session: {path:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    drop((workspace_guard, session_guard));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_messages_and_followups_survive_a_run_failure_until_durable_delivery() {
    let server = MockServer::start().await;
    let state = Arc::new(ScriptState::default());
    mount_script(
        &server,
        InFlightDeliveryScript {
            state: state.clone(),
        },
    )
    .await;

    let mut harness = build_enabled_agent(&server, DelegationLimits::default());
    let output = tokio::time::timeout(
        Duration::from_secs(8),
        harness.agent.complete("recover accepted in-flight work"),
    )
    .await
    .expect("delegation recovery timed out")
    .unwrap();
    assert_eq!(output.text, "root observed recovery");
    assert!(state.unexpected.lock().unwrap().is_empty());

    let results = tool_results(harness.agent.session());
    assert_eq!(
        parse_result(&results, "send-in-flight")["delivery"],
        "steering"
    );
    assert_eq!(
        parse_result(&results, "follow-in-flight")["delivery"],
        "follow_up"
    );
    let listed = parse_result(&results, "list-in-flight");
    let child = listed_agent(&listed, "/root/recover");
    assert_eq!(child["status"]["state"], "completed");
    assert_eq!(child["status"]["output"], "recovered accepted work");

    let requests = state.requests.lock().unwrap();
    let child_requests = requests
        .iter()
        .filter(|request| request_system(request).contains("You are /root/recover"))
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 2, "{child_requests:?}");
    let recovered_request = child_requests[1].to_string();
    let message_index = recovered_request
        .find("context that must survive")
        .expect("recovered request must contain accepted steering");
    let follow_up_index = recovered_request
        .find("retry task after failure")
        .expect("recovered request must contain accepted follow-up");
    assert!(
        message_index < follow_up_index,
        "accepted work must retain FIFO order: {recovered_request}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_enforces_concurrency_depth_and_total_limits() {
    let server = MockServer::start().await;
    let state = Arc::new(ScriptState::default());
    mount_script(
        &server,
        LimitsScript {
            state: state.clone(),
        },
    )
    .await;
    let limits = DelegationLimits {
        max_concurrent_agents: 2,
        max_depth: 1,
        max_total_agents: 2,
    };
    let mut harness = build_enabled_agent(&server, limits);
    let provenance_path = harness.team_directory.join("provenance.jsonl");
    let output = harness
        .agent
        .complete("exercise every bound")
        .await
        .unwrap();
    assert_eq!(output.text, "bounded result");
    assert!(state.unexpected.lock().unwrap().is_empty());

    let root_results = tool_results(harness.agent.session());
    let initial = [
        root_results.get("spawn-first").unwrap(),
        root_results.get("spawn-second").unwrap(),
    ];
    assert_eq!(initial.iter().filter(|(error, _)| *error).count(), 1);
    assert!(initial
        .iter()
        .any(|(error, text)| *error && text.contains("concurrency limit")));
    let (third_error, third_text) = root_results.get("spawn-third").unwrap();
    assert!(*third_error);
    assert!(third_text.contains("agent limit"), "{third_text}");

    let listed = parse_result(&root_results, "list-bounded");
    assert_eq!(listed["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listed["agents"][0]["depth"], 1);
    assert_eq!(listed["agents"][0]["status"]["state"], "completed");

    let spawned = read_provenance(&provenance_path)
        .into_iter()
        .filter(|event| event["event"] == "agent_spawned")
        .collect::<Vec<_>>();
    assert_eq!(spawned.len(), 1);
    let child_path = PathBuf::from(spawned[0]["session"].as_str().unwrap());
    let EnabledAgent {
        agent,
        _workspace: workspace_guard,
        _sessions: session_guard,
        ..
    } = harness;
    drop(agent);
    wait_for_provenance(&provenance_path, |events| {
        events.iter().any(|event| event["event"] == "team_shutdown")
    })
    .await;
    let child = Session::open(child_path).unwrap();
    let child_results = tool_results(&child);
    let (depth_error, depth_text) = child_results.get("spawn-too-deep").unwrap();
    assert!(*depth_error);
    assert!(depth_text.contains("depth limit"), "{depth_text}");
    drop((workspace_guard, session_guard));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completing_a_root_run_cancels_unfinished_delegated_workers() {
    let server = MockServer::start().await;
    let state = Arc::new(ScriptState::default());
    mount_script(
        &server,
        CompletionCancellationScript {
            state: state.clone(),
        },
    )
    .await;
    let mut harness = build_enabled_agent(&server, DelegationLimits::default());
    let provenance_path = harness.team_directory.join("provenance.jsonl");

    let output = harness.agent.complete("spawn and finish").await.unwrap();
    assert_eq!(output.text, "root finished without waiting");
    wait_for_provenance(&provenance_path, |events| {
        events.iter().any(|event| {
            event["event"] == "agent_status"
                && event["agent_id"] == "agent-1"
                && event["status"]["state"] == "shutdown"
        })
    })
    .await;
    assert!(state.unexpected.lock().unwrap().is_empty());
}

async fn cancellation_harness() -> (EnabledAgent, MockServer, Arc<ScriptState>) {
    let server = MockServer::start().await;
    let state = Arc::new(ScriptState::default());
    mount_script(
        &server,
        CancellationScript {
            state: state.clone(),
        },
    )
    .await;
    let harness = build_enabled_agent(&server, DelegationLimits::default());
    (harness, server, state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_or_dropping_a_root_run_cancels_delegated_workers() {
    let (mut explicit, _server, state) = cancellation_harness().await;
    let explicit_provenance = explicit.team_directory.join("provenance.jsonl");
    let mut run = explicit.agent.prompt("spawn then abort").await.unwrap();
    let control = run.control();
    let reason = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = run.next().await {
            match event {
                AgentEvent::ToolFinished { id, .. } if id.0 == "spawn-slow" => control.abort(),
                AgentEvent::RunFinished { reason, .. } => return reason,
                _ => {}
            }
        }
        panic!("run ended without RunFinished")
    })
    .await
    .expect("explicit abort did not settle");
    assert!(matches!(reason, FinishReason::Aborted));
    drop(run);
    wait_for_provenance(&explicit_provenance, |events| {
        events
            .iter()
            .any(|event| event["event"] == "agent_status" && event["status"]["state"] == "shutdown")
    })
    .await;
    assert!(state.unexpected.lock().unwrap().is_empty());
    drop(explicit);

    let (mut dropped, _server, state) = cancellation_harness().await;
    let dropped_provenance = dropped.team_directory.join("provenance.jsonl");
    let mut run = dropped.agent.prompt("spawn then drop").await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = run.next().await {
            if matches!(event, AgentEvent::ToolFinished { ref id, .. } if id.0 == "spawn-slow") {
                return;
            }
        }
        panic!("run ended before spawn completed")
    })
    .await
    .expect("spawn tool did not finish");
    drop(run);
    wait_for_provenance(&dropped_provenance, |events| {
        events
            .iter()
            .any(|event| event["event"] == "agent_status" && event["status"]["state"] == "shutdown")
    })
    .await;
    assert!(state.unexpected.lock().unwrap().is_empty());
    drop(dropped);
}
