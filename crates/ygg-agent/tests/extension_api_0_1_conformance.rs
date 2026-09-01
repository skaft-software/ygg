#![cfg(unix)]
#![allow(missing_docs)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::{json, Value};
use tempfile::TempDir;
use ygg_agent::extension_process::{
    ExtensionActiveSkill, ExtensionRequestId, EXTENSION_API_VERSION_0_2,
    EXTENSION_FEATURE_REQUEST_PROGRESS,
};
use ygg_agent::{
    CancellationToken, DiscoveredExtension, ExtensionActivation, ExtensionConfirmationResponse,
    ExtensionEvent, ExtensionHook, ExtensionHostState, ExtensionManifest, ExtensionPrincipal,
    ExtensionProcess, ExtensionRuntimeConfig, ExtensionRuntimeError, ExtensionSource,
    ExtensionTrust, SandboxConfig, ToolCallHook, ToolContext, ToolProgressSink,
    EXTENSION_API_VERSION_0_1, EXTENSION_MANIFEST_FILENAME,
};

const RAW_WIRE_CHILD: &str = r#"#!/bin/sh
set -eu
log=$1

read_message() {
  IFS= read -r message
  printf '%s\n' "$message" >> "$log"
}

request_id() {
  printf '%s\n' "$message" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'
}

read_message
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[{"name":"wire_tool","description":"Wire tool","parameters":{"type":"object"}}],"commands":[{"name":"wire_command","description":"Wire command","usage":"/wire_command"}]}}\n' "$id"

read_message
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"disposition":{"action":"continue"},"context":[],"notifications":[]}}\n' "$id"

read_message
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"disposition":{"action":"continue"},"context":[],"notifications":[]}}\n' "$id"

printf '%s\n' '{"jsonrpc":"2.0","id":"__CONFIRMATION_ID__","method":"confirmation/request","params":{"prompt":"Apply the wire fixture?","detail":"Exercises exact ID correlation.","destructive":false,"default":false}}'
read_message

read_message
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#;

#[tokio::test]
async fn api_0_1_golden_wire_covers_initialization_hooks_confirmation_and_shutdown() {
    let temp = TempDir::new().expect("tempdir");
    let log_path = temp.path().join("wire.jsonl");
    let child_path = temp.path().join("wire-child.sh");
    let confirmation_id = "c".repeat(256);
    write_executable(
        &child_path,
        &RAW_WIRE_CHILD.replace("__CONFIRMATION_ID__", &confirmation_id),
    );

    let mut manifest = ExtensionManifest::parse(
        r#"
name = "wire-fixture"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "wire-child.sh"

[capabilities]
filesystem = "workspace"
process = true
network = false

[contributes]
tools = ["wire_tool"]
commands = ["wire_command"]
hooks = ["before_tool_call", "after_tool_call"]
confirmations = true
"#,
    )
    .expect("manifest");
    manifest.entrypoint.args = vec![log_path.to_string_lossy().into_owned()];
    let manifest_path = temp.path().join(EXTENSION_MANIFEST_FILENAME);
    let descriptor = trusted_descriptor(manifest_path.clone(), manifest.clone());
    let host_state = ExtensionHostState {
        session_id: Some("session-wire-1".into()),
        session_name: Some("Wire contract".into()),
        model: Some("local/model".into()),
        reasoning: Some(json!({"effort": "high"})),
        active_skills: vec![ExtensionActiveSkill {
            id: "skill-1".into(),
            name: "Conformance".into(),
            version: Some("1.2.3".into()),
        }],
    };
    let mut config = ExtensionRuntimeConfig::new(temp.path());
    config.host_state = host_state.clone();
    config.request_timeout = Duration::from_secs(2);
    config.shutdown_timeout = Duration::from_secs(2);

    let process = ExtensionProcess::start(descriptor, config)
        .await
        .expect("start raw wire fixture");
    let mut events = process.subscribe();

    let sandbox = SandboxConfig::new(temp.path());
    let active_skills = [];
    let registered_tools = ["read".to_owned()];
    let tool_context = ToolContext {
        workspace: temp.path(),
        sandbox: &sandbox,
        execution_scope: "wire-scope-1",
        resource_owner: "wire-scope-1",
        active_skills: &active_skills,
        registered_tools: &registered_tools,
        progress: ToolProgressSink::null(),
        cancellation: CancellationToken::default(),
    };
    let tool_arguments = json!({"path": "README.md"});
    ToolCallHook::before_tool_call(&process, "read", &tool_arguments, &tool_context)
        .await
        .expect("before_tool_call");
    ToolCallHook::after_tool_call(
        &process,
        "read",
        &tool_arguments,
        "file contents",
        false,
        &tool_context,
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("confirmation event timeout")
        .expect("confirmation event");
    let (request_id, generation) = match event {
        ExtensionEvent::ConfirmationRequested {
            request_id,
            generation,
            request,
            ..
        } => {
            assert_eq!(request.prompt, "Apply the wire fixture?");
            assert_eq!(
                request.detail.as_deref(),
                Some("Exercises exact ID correlation.")
            );
            (request_id, generation)
        }
        other => panic!("unexpected extension event: {other:?}"),
    };
    assert_eq!(
        request_id,
        ExtensionRequestId::String(confirmation_id.clone())
    );
    process
        .respond_to_confirmation(
            request_id,
            generation,
            ExtensionConfirmationResponse { confirmed: true },
        )
        .await
        .expect("confirmation response");
    assert!(process.shutdown().await, "shutdown should be acknowledged");
    assert!(!process.is_running());

    let frames = read_frames(&log_path);
    assert_eq!(frames.len(), 5, "unexpected wire transcript: {frames:#?}");
    let base_context = json!({
        "workspace": temp.path(),
        "execution_scope": null,
        "host": host_state,
    });
    assert_eq!(
        frames[0],
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "api_version": EXTENSION_API_VERSION_0_1,
                "ygg_version": env!("CARGO_PKG_VERSION"),
                "extension": {
                    "name": "wire-fixture",
                    "version": "0.1.0",
                    "manifest_path": manifest_path,
                    "source": "explicit",
                },
                "workspace": temp.path(),
                "capabilities": {
                    "filesystem": "workspace",
                    "process": true,
                    "network": false,
                },
                "contributes": {
                    "tools": ["wire_tool"],
                    "commands": ["wire_command"],
                    "hooks": [
                        "before_tool_call",
                        "after_tool_call",
                    ],
                    "ui": [],
                    "context": false,
                    "tool_renderers": [],
                    "notifications": false,
                    "confirmations": true,
                },
                "host": base_context["host"].clone(),
            },
        })
    );
    let tool_host = json!({
        "session_id": "session-wire-1",
        "session_name": "Wire contract",
        "model": "local/model",
        "reasoning": {"effort": "high"},
        "active_skills": [],
    });
    let tool_wire_context = json!({
        "workspace": temp.path(),
        "execution_scope": "wire-scope-1",
        "host": tool_host,
    });
    assert_eq!(
        frames[1],
        hook_frame(
            2,
            "before_tool_call",
            json!({"name": "read", "arguments": {"path": "README.md"}}),
            tool_wire_context.clone(),
        )
    );
    assert_eq!(
        frames[2],
        hook_frame(
            3,
            "after_tool_call",
            json!({
                "name": "read",
                "arguments": {"path": "README.md"},
                "output": "file contents",
                "is_error": false,
            }),
            tool_wire_context,
        )
    );
    assert_eq!(
        frames[3],
        json!({
            "jsonrpc": "2.0",
            "id": confirmation_id,
            "result": {"confirmed": true},
        })
    );
    assert_eq!(
        frames[4],
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "shutdown",
            "params": {},
        })
    );
}

#[tokio::test]
async fn rust_host_runs_a_real_python_sdk_extension_end_to_end() {
    let temp = TempDir::new().expect("tempdir");
    let child_path = temp.path().join("python-sdk-extension.py");
    let shutdown_marker = temp.path().join("python-sdk-shutdown.txt");
    let python_sdk = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/python")
        .canonicalize()
        .expect("canonical Python SDK path");
    write_executable(
        &child_path,
        r#"#!/usr/bin/env python3
import os
from pathlib import Path

from ygg_extension import Extension

extension = Extension()

@extension.tool(
    name="sdk_echo",
    description="Echo text through the real Python SDK",
    parameters={
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    },
)
def sdk_echo(arguments, context):
    return {
        "content": f"{arguments['text']}@{context.get('execution_scope')}",
        "metadata": {"sdk": "python"},
    }

@extension.command(
    name="sdk_checkpoint",
    description="Checkpoint through the real Python SDK",
    usage="/sdk_checkpoint <label>",
)
def sdk_checkpoint(arguments, context):
    return {
        "text": "checkpoint:" + "|".join(arguments),
        "context": [
            {
                "label": "python-command",
                "content": context.get("execution_scope") or "none",
                "placement": "prompt_suffix",
            }
        ],
    }

@extension.hook("before_prompt")
def before_prompt(payload, context):
    return {
        "context": [
            {
                "label": "python-hook",
                "content": payload["prompt"],
                "placement": "system_suffix",
            }
        ]
    }

@extension.on_shutdown
def on_shutdown(params, context):
    Path(os.environ["YGG_TEST_SHUTDOWN_MARKER"]).write_text("shutdown", encoding="utf-8")

extension.run()
"#,
    );

    let mut manifest = ExtensionManifest::parse(
        r#"
name = "python-sdk"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "python3"

[contributes]
tools = ["sdk_echo"]
commands = ["sdk_checkpoint"]
hooks = ["before_prompt"]
"#,
    )
    .expect("manifest");
    manifest.entrypoint.args = vec![child_path.to_string_lossy().into_owned()];
    manifest.entrypoint.env.insert(
        "PYTHONPATH".into(),
        python_sdk.to_string_lossy().into_owned(),
    );
    manifest.entrypoint.env.insert(
        "YGG_TEST_SHUTDOWN_MARKER".into(),
        shutdown_marker.to_string_lossy().into_owned(),
    );
    let host_state = ExtensionHostState {
        session_id: Some("python-sdk-session".into()),
        session_name: Some("Python SDK conformance".into()),
        model: Some("local/python-proof".into()),
        reasoning: None,
        active_skills: Vec::new(),
    };
    let mut config = ExtensionRuntimeConfig::new(temp.path());
    config.host_state = host_state.clone();
    config.request_timeout = Duration::from_secs(3);
    config.shutdown_timeout = Duration::from_secs(3);
    let process = ExtensionProcess::start(
        trusted_descriptor(temp.path().join(EXTENSION_MANIFEST_FILENAME), manifest),
        config,
    )
    .await
    .expect("start real Python SDK extension");

    assert_eq!(process.contributions().tools.len(), 1);
    assert_eq!(process.contributions().tools[0].name, "sdk_echo");
    assert_eq!(
        process.contributions().tools[0].parameters,
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        })
    );
    assert_eq!(process.contributions().commands.len(), 1);
    assert_eq!(process.contributions().commands[0].name, "sdk_checkpoint");
    assert_eq!(
        process.contributions().commands[0].usage.as_deref(),
        Some("/sdk_checkpoint <label>")
    );

    let mut context = process.current_context();
    context.execution_scope = Some("python-scope-1".into());
    assert_eq!(context.host, host_state);
    let tool_output = process
        .call_tool("sdk_echo", json!({"text": "hello"}), context.clone())
        .await
        .expect("Python SDK tool call");
    assert_eq!(tool_output.content, "hello@python-scope-1");
    assert!(!tool_output.is_error);
    assert_eq!(tool_output.metadata, json!({"sdk": "python"}));

    let command_output = process
        .execute_command(
            "sdk_checkpoint",
            vec!["alpha".into(), "beta".into()],
            context.clone(),
        )
        .await
        .expect("Python SDK command call");
    assert_eq!(command_output.text, "checkpoint:alpha|beta");
    assert_eq!(command_output.context.len(), 1);
    assert_eq!(command_output.context[0].label, "python-command");
    assert_eq!(command_output.context[0].content, "python-scope-1");

    let hook_output = process
        .run_hook(
            ExtensionHook::BeforePrompt,
            json!({"prompt": "source-backed"}),
            context,
        )
        .await
        .expect("Python SDK hook call");
    assert_eq!(hook_output.context.len(), 1);
    assert_eq!(hook_output.context[0].label, "python-hook");
    assert_eq!(hook_output.context[0].content, "source-backed");

    assert!(
        process.shutdown().await,
        "Python SDK should acknowledge shutdown"
    );
    assert!(!process.is_running());
    assert_eq!(
        std::fs::read_to_string(shutdown_marker).expect("Python SDK shutdown marker"),
        "shutdown"
    );
}

#[tokio::test]
async fn rust_host_runs_a_real_python_sdk_api_0_2_extension_end_to_end() {
    let temp = TempDir::new().expect("tempdir");
    let child_path = temp.path().join("python-sdk-v02-extension.py");
    let shutdown_marker = temp.path().join("python-sdk-v02-shutdown.txt");
    let python_sdk = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/python")
        .canonicalize()
        .expect("canonical Python SDK path");
    write_executable(
        &child_path,
        r#"#!/usr/bin/env python3
import os
from pathlib import Path

from ygg_extension import Extension, text_content, tool_result

extension = Extension(api_version="0.2")

@extension.tool(
    name="sdk_v02_echo",
    description="Echo a structured value through API 0.2",
    parameters={
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    },
    output_schema={
        "type": "object",
        "properties": {"echo": {"type": "string"}},
        "required": ["echo"],
        "additionalProperties": False,
    },
)
def sdk_v02_echo(arguments, context):
    extension.progress(message="halfway", current=1, total=2, unit="steps")
    value = arguments["text"]
    return tool_result(
        text_content("echo:" + value),
        structured_content={"echo": value},
        metadata={"sdk": "python", "api": "0.2"},
    )

@extension.on_shutdown
def on_shutdown(params, context):
    Path(os.environ["YGG_TEST_SHUTDOWN_MARKER"]).write_text("shutdown", encoding="utf-8")

extension.run()
"#,
    );

    let mut manifest = ExtensionManifest::parse(
        r#"
name = "python-sdk-v02"
version = "0.2.0"
api_version = "0.2"

[entrypoint]
command = "python3"

[contributes]
tools = ["sdk_v02_echo"]
"#,
    )
    .expect("manifest");
    manifest.entrypoint.args = vec![child_path.to_string_lossy().into_owned()];
    manifest.entrypoint.env.insert(
        "PYTHONPATH".into(),
        python_sdk.to_string_lossy().into_owned(),
    );
    manifest.entrypoint.env.insert(
        "YGG_TEST_SHUTDOWN_MARKER".into(),
        shutdown_marker.to_string_lossy().into_owned(),
    );
    let mut config = ExtensionRuntimeConfig::new(temp.path());
    config.request_timeout = Duration::from_secs(3);
    config.shutdown_timeout = Duration::from_secs(3);
    let process = ExtensionProcess::start(
        trusted_descriptor(temp.path().join(EXTENSION_MANIFEST_FILENAME), manifest),
        config,
    )
    .await
    .expect("start real Python SDK API 0.2 extension");

    assert_eq!(process.api_version(), EXTENSION_API_VERSION_0_2);
    assert!(process
        .negotiated_features()
        .contains(EXTENSION_FEATURE_REQUEST_PROGRESS));
    let output = process
        .call_tool(
            "sdk_v02_echo",
            json!({"text": "hello"}),
            process.current_context(),
        )
        .await
        .expect("API 0.2 Python tool call");
    assert_eq!(output.content, "echo:hello");
    assert_eq!(output.structured_content, Some(json!({"echo": "hello"})));
    assert_eq!(output.metadata, json!({"sdk": "python", "api": "0.2"}));
    assert!(
        process.shutdown().await,
        "Python SDK should drain and shut down"
    );
    assert_eq!(
        std::fs::read_to_string(shutdown_marker).expect("Python SDK shutdown marker"),
        "shutdown"
    );
}

#[tokio::test]
async fn adversarial_raw_child_must_match_nonempty_tool_and_command_declarations_exactly() {
    fn tool(name: &str) -> Value {
        json!({
            "name": name,
            "description": format!("{name} description"),
            "parameters": {"type": "object"},
        })
    }

    fn command(name: &str) -> Value {
        json!({
            "name": name,
            "description": format!("{name} description"),
            "usage": format!("/{name}"),
        })
    }

    let cases = vec![
        (
            "tool-omission",
            vec!["required_tool"],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            "tools",
        ),
        (
            "tool-addition",
            Vec::new(),
            vec![tool("unexpected_tool")],
            Vec::new(),
            Vec::new(),
            "tools",
        ),
        (
            "tool-duplicate",
            vec!["required_tool"],
            vec![tool("required_tool"), tool("required_tool")],
            Vec::new(),
            Vec::new(),
            "tools",
        ),
        (
            "command-omission",
            Vec::new(),
            Vec::new(),
            vec!["required_command"],
            Vec::new(),
            "commands",
        ),
        (
            "command-addition",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![command("unexpected_command")],
            "commands",
        ),
        (
            "command-duplicate",
            Vec::new(),
            Vec::new(),
            vec!["required_command"],
            vec![command("required_command"), command("required_command")],
            "commands",
        ),
    ];

    for (
        case,
        declared_tools,
        initialized_tools,
        declared_commands,
        initialized_commands,
        mismatched_kind,
    ) in cases
    {
        let temp = TempDir::new().expect("tempdir");
        let child_name = format!("{case}.sh");
        let child_path = temp.path().join(&child_name);
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "api_version": EXTENSION_API_VERSION_0_1,
                "tools": initialized_tools,
                "commands": initialized_commands,
            },
        });
        write_executable(
            &child_path,
            &format!("#!/bin/sh\nIFS= read -r initialize\nprintf '%s\\n' '{response}'\nsleep 30\n"),
        );
        let mut manifest = ExtensionManifest::parse(&format!(
            r#"
name = "{case}"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "{child_name}"
"#,
        ))
        .expect("manifest");
        manifest.contributes.tools = declared_tools.into_iter().map(str::to_owned).collect();
        manifest.contributes.commands = declared_commands.into_iter().map(str::to_owned).collect();
        let descriptor =
            trusted_descriptor(temp.path().join(EXTENSION_MANIFEST_FILENAME), manifest);
        let error =
            match ExtensionProcess::start(descriptor, ExtensionRuntimeConfig::new(temp.path()))
                .await
            {
                Ok(process) => {
                    process.shutdown().await;
                    panic!("{case}: mismatched raw child unexpectedly initialized");
                }
                Err(error) => error,
            };
        let expected = format!("initialized {mismatched_kind} do not match manifest declarations");
        assert!(
            matches!(
                &error,
                ExtensionRuntimeError::Protocol(message) if message == &expected
            ),
            "{case}: unexpected mismatch error: {error}"
        );
    }
}

fn hook_frame(id: u64, hook: &str, payload: Value, context: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "hook/run",
        "params": {
            "hook": hook,
            "payload": payload,
            "context": context,
        },
    })
}

fn trusted_descriptor(
    manifest_path: std::path::PathBuf,
    manifest: ExtensionManifest,
) -> DiscoveredExtension {
    if !manifest_path.exists() {
        std::fs::write(
            &manifest_path,
            toml::to_string_pretty(&manifest).expect("serialize identity manifest"),
        )
        .expect("write identity manifest");
    }
    let principal = ExtensionPrincipal::derive(&manifest.name, &manifest_path)
        .expect("derive extension principal");
    DiscoveredExtension {
        manifest,
        manifest_path,
        principal,
        source: ExtensionSource::Explicit,
        activation: ExtensionActivation {
            enabled: true,
            trust: ExtensionTrust::Trusted,
        },
    }
}

fn write_executable(path: &Path, source: &str) {
    std::fs::write(path, source).expect("write fixture");
    let mut permissions = std::fs::metadata(path)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).expect("chmod fixture");
}

fn read_frames(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read wire log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid logged JSON-RPC frame"))
        .collect()
}
