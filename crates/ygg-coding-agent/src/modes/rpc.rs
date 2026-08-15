#![allow(missing_docs)]

//! Pi-compatible JSONL RPC frontend.
//!
//! A single task owns `App` and its borrowed `Run`. A blocking stdin reader
//! performs strict LF framing and forwards complete JSON values to that task;
//! only the owner writes stdout, so responses and streaming events can never
//! interleave at the byte level.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use ygg_agent::{
    AgentCompactionMode, AgentError, AgentEvent, BashTool, CancellationToken, EntryValue,
    FinishReason, InputPart, OutputChannel, QueueDeliveryMode, Run, RunControl, SandboxConfig,
    SkillRegistry, Tool, ToolContext, ToolProgress, ToolProgressSink, UserInput,
};
use ygg_ai::{
    AssistantMessage, AssistantPart, Cost, ImageSource, Media, Message, Modality, Model, ModelId,
    Protocol, StopReason, ToolResult, ToolResultPart, Usage, UserMessage, UserPart,
};

use crate::app::bootstrap::{build_app, rebuild_app, resolve_launch_print, Bootstrap};
use crate::app::{
    apply_reconfig, reasoning_label, supported_levels, thinking_to_reasoning, App, Reconfig,
};
use crate::compaction::{attempt_compaction, estimate_text_tokens, CompactionOutcome};
use crate::config::{CompactionMode, ThinkingLevel};
use crate::prompts::{PromptRegistry, PromptRenderContext};
use crate::resources::{compose_instructions, expand_skill_command};

const MAX_RPC_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
enum RpcInput {
    Value(Value),
    ParseError(String),
    Eof,
}

struct RpcOutput {
    stdout: std::io::BufWriter<std::io::Stdout>,
}

impl RpcOutput {
    fn new() -> Self {
        Self {
            stdout: std::io::BufWriter::new(std::io::stdout()),
        }
    }

    fn send(&mut self, value: Value) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.stdout, &value)?;
        self.stdout.write_all(b"\n")?;
        self.stdout.flush()?;
        Ok(())
    }

    fn success(
        &mut self,
        id: Option<&str>,
        command: &str,
        data: Option<Value>,
    ) -> anyhow::Result<()> {
        let mut response = Map::new();
        if let Some(id) = id {
            response.insert("id".into(), Value::String(id.to_owned()));
        }
        response.insert("type".into(), Value::String("response".into()));
        response.insert("command".into(), Value::String(command.to_owned()));
        response.insert("success".into(), Value::Bool(true));
        if let Some(data) = data {
            response.insert("data".into(), data);
        }
        self.send(Value::Object(response))
    }

    fn error(
        &mut self,
        id: Option<&str>,
        command: &str,
        error: impl Into<String>,
    ) -> anyhow::Result<()> {
        let mut response = Map::new();
        if let Some(id) = id {
            response.insert("id".into(), Value::String(id.to_owned()));
        }
        response.insert("type".into(), Value::String("response".into()));
        response.insert("command".into(), Value::String(command.to_owned()));
        response.insert("success".into(), Value::Bool(false));
        response.insert("error".into(), Value::String(error.into()));
        self.send(Value::Object(response))
    }
}

fn spawn_input_reader() -> mpsc::Receiver<RpcInput> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::Builder::new()
        .name("ygg-rpc-stdin".into())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut chunk = [0u8; 8192];
            let mut pending = Vec::new();
            let mut discarding_oversized = false;
            loop {
                let read = match input.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) => {
                        let _ = tx.blocking_send(RpcInput::ParseError(format!(
                            "Failed to read command: {error}"
                        )));
                        break;
                    }
                };
                let mut start = 0usize;
                for (index, byte) in chunk[..read].iter().enumerate() {
                    if *byte != b'\n' {
                        continue;
                    }
                    if discarding_oversized {
                        discarding_oversized = false;
                    } else {
                        pending.extend_from_slice(&chunk[start..index]);
                        dispatch_line(&tx, &mut pending);
                    }
                    start = index + 1;
                }
                if start < read && !discarding_oversized {
                    pending.extend_from_slice(&chunk[start..read]);
                    if pending.len() > MAX_RPC_LINE_BYTES {
                        pending.clear();
                        discarding_oversized = true;
                        let _ = tx.blocking_send(RpcInput::ParseError(format!(
                            "Failed to parse command: JSONL record exceeds {MAX_RPC_LINE_BYTES} bytes"
                        )));
                    }
                }
            }
            if !discarding_oversized && !pending.is_empty() {
                dispatch_line(&tx, &mut pending);
            }
            let _ = tx.blocking_send(RpcInput::Eof);
        })
        .expect("RPC stdin reader thread must start");
    rx
}

fn dispatch_line(tx: &mpsc::Sender<RpcInput>, pending: &mut Vec<u8>) {
    if pending.last() == Some(&b'\r') {
        pending.pop();
    }
    let parsed = std::str::from_utf8(pending)
        .map_err(|_| "Failed to parse command: record is not valid UTF-8".to_owned())
        .and_then(|line| {
            serde_json::from_str::<Value>(line)
                .map_err(|error| format!("Failed to parse command: {error}"))
        });
    pending.clear();
    let input = match parsed {
        Ok(value) => RpcInput::Value(value),
        Err(error) => RpcInput::ParseError(error),
    };
    let _ = tx.blocking_send(input);
}

fn command_type(command: &Value) -> Option<&str> {
    command.as_object()?.get("type")?.as_str()
}

fn command_id(command: &Value) -> Option<&str> {
    command.as_object()?.get("id")?.as_str()
}

fn required_string<'a>(command: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    command
        .as_object()
        .and_then(|object| object.get(field))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a string"))
}

fn required_bool(command: &Value, field: &str) -> anyhow::Result<bool> {
    command
        .as_object()
        .and_then(|object| object.get(field))
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a boolean"))
}

fn optional_bool(command: &Value, field: &str) -> anyhow::Result<Option<bool>> {
    let Some(value) = command.as_object().and_then(|object| object.get(field)) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a boolean"))
}

#[derive(Debug)]
struct RpcBashResult {
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
}

impl RpcBashResult {
    fn value(&self) -> Value {
        let mut value = Map::new();
        value.insert("output".into(), Value::String(self.output.clone()));
        if let Some(exit_code) = self.exit_code {
            value.insert("exitCode".into(), json!(exit_code));
        }
        value.insert("cancelled".into(), Value::Bool(self.cancelled));
        value.insert("truncated".into(), Value::Bool(self.truncated));
        Value::Object(value)
    }
}

struct ActiveRpcBash {
    id: Option<String>,
    command: String,
    exclude_from_context: bool,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<RpcBashResult>>,
}

fn parse_bash_exit_status(line: &str) -> Option<i32> {
    line.strip_prefix("exit=")?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

fn rpc_bash_result(text: String) -> RpcBashResult {
    let truncated = text.contains("truncated_stdout=") || text.contains("truncated_stderr=");
    let payload = text.strip_prefix("error nonzero_exit\n").unwrap_or(&text);
    let (status, body) = payload.split_once('\n').unwrap_or((payload, ""));
    let exit_code = parse_bash_exit_status(status);
    let output = if exit_code.is_some() {
        if body == "(no output)" {
            String::new()
        } else {
            body.to_owned()
        }
    } else {
        text
    };
    RpcBashResult {
        output,
        exit_code,
        cancelled: false,
        truncated,
    }
}

async fn run_sandboxed_bash(
    command: String,
    workspace: PathBuf,
    sandbox: SandboxConfig,
    cancellation: CancellationToken,
) -> anyhow::Result<RpcBashResult> {
    let active_skills = Vec::new();
    let registered_tools = vec!["bash".to_owned()];
    let context = ToolContext {
        workspace: &workspace,
        sandbox: &sandbox,
        execution_scope: "rpc-bash",
        active_skills: &active_skills,
        registered_tools: &registered_tools,
        progress: ToolProgressSink::null(),
        cancellation: cancellation.clone(),
    };
    let execution = BashTool.execute(json!({"command": command}), &context);
    tokio::pin!(execution);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(RpcBashResult {
            output: String::new(),
            exit_code: None,
            cancelled: true,
            truncated: false,
        }),
        result = &mut execution => match result {
            Ok(output) => Ok(rpc_bash_result(output.text)),
            Err(error) if error.message.starts_with("error nonzero_exit\n")
                || error.message.starts_with("error timeout\n") => {
                    Ok(rpc_bash_result(error.message))
                }
            Err(error) => Err(anyhow::anyhow!(error.message)),
        }
    }
}

async fn drive_rpc_bash(
    active: &mut ActiveRpcBash,
    input: &mut mpsc::Receiver<RpcInput>,
    output: &mut RpcOutput,
) -> anyhow::Result<(anyhow::Result<RpcBashResult>, VecDeque<Value>, bool)> {
    let mut deferred = VecDeque::new();
    let mut eof = false;
    let result = loop {
        tokio::select! {
            biased;
            result = &mut active.task => {
                break match result {
                    Ok(result) => result,
                    Err(error) => Err(anyhow::anyhow!("bash task failed: {error}")),
                };
            }
            inbound = input.recv(), if !eof => match inbound {
                Some(RpcInput::Value(command))
                    if command_type(&command) == Some("abort_bash") => {
                        active.cancellation.cancel();
                        output.success(command_id(&command), "abort_bash", None)?;
                    }
                Some(RpcInput::Value(command)) => deferred.push_back(command),
                Some(RpcInput::ParseError(error)) => output.error(None, "parse", error)?,
                Some(RpcInput::Eof) | None => {
                    eof = true;
                    active.cancellation.cancel();
                }
            }
        }
    };
    Ok((result, deferred, eof))
}

fn bash_context_message(command: &str, result: &RpcBashResult) -> Message {
    Message::User(UserMessage {
        content: vec![UserPart::Text(format!(
            "Ran `{command}`\n```\n{}\n```",
            result.output
        ))],
    })
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn iso_timestamp(unix_ms: u64) -> String {
    let seconds = unix_ms / 1_000;
    let millis = unix_ms % 1_000;
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;

    // Gregorian civil date conversion from a Unix-day count. Session
    // timestamps are non-negative, but the formula also remains valid before
    // 1970 should a migrated record ever carry such a value.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn protocol_name(protocol: &Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiResponses => "openai-responses",
        Protocol::OpenAiChat => "openai-completions",
        Protocol::AnthropicMessages => "anthropic-messages",
    }
}

fn dollars(microdollars: u64) -> f64 {
    microdollars as f64 / 1_000_000.0
}

fn model_value(model: &Model) -> Value {
    let mut input = vec!["text"];
    if model
        .spec
        .capabilities
        .input_modalities
        .contains(Modality::Image)
    {
        input.push("image");
    }
    let cost = model.spec.pricing.as_ref().map_or_else(
        || json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}),
        |pricing| {
            json!({
                "input": dollars(pricing.input.0),
                "output": dollars(pricing.output.0),
                "cacheRead": dollars(pricing.cache_read.0),
                "cacheWrite": dollars(pricing.cache_write_5m.0)
            })
        },
    );
    json!({
        "id": model.spec.id.0,
        "name": model.spec.display_name.as_deref().unwrap_or(&model.spec.id.0),
        "api": protocol_name(&model.spec.protocol),
        "provider": model.endpoint.id.0,
        "baseUrl": model.endpoint.base_url.as_str().trim_end_matches('/'),
        "reasoning": model.spec.capabilities.reasoning.is_some(),
        "input": input,
        "cost": cost,
        "contextWindow": model.spec.limits.context_window,
        "maxTokens": model.spec.limits.max_output_tokens
    })
}

fn media_content(media: &Media) -> Value {
    match media {
        Media::Image(image) => match &image.source {
            ImageSource::Inline(data) => json!({
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(data),
                "mimeType": image.media_type.as_ref().map_or("image/png", mime::Mime::as_ref)
            }),
            ImageSource::Url(url) => {
                json!({"type": "text", "text": format!("[image: {url}]")})
            }
            ImageSource::ProviderRef(_) => json!({"type": "text", "text": "[image]"}),
        },
        Media::Audio(_) => json!({"type": "text", "text": "[audio]"}),
    }
}

fn user_content_at(parts: impl IntoIterator<Item = Value>, timestamp: u64) -> Value {
    json!({
        "role": "user",
        "content": parts.into_iter().collect::<Vec<_>>(),
        "timestamp": timestamp
    })
}

fn user_content(parts: impl IntoIterator<Item = Value>) -> Value {
    user_content_at(parts, now_millis().min(u128::from(u64::MAX)) as u64)
}

fn user_input_value(input: &UserInput) -> Value {
    user_content(input.parts.iter().map(|part| match part {
        InputPart::Text(text) => json!({"type": "text", "text": text}),
        InputPart::Media(media) => media_content(media),
    }))
}

fn user_value(text: &str) -> Value {
    user_content([json!({"type": "text", "text": text})])
}

fn usage_value(usage: &Usage, cost: Option<Cost>) -> Value {
    let cost = cost.unwrap_or_default();
    json!({
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "cacheRead": usage.cache_read_tokens,
        "cacheWrite": usage.cache_write_tokens,
        "totalTokens": usage.total_tokens,
        "cost": {
            "input": dollars(cost.input),
            "output": dollars(cost.output.saturating_add(cost.reasoning)),
            "cacheRead": dollars(cost.cache_read),
            "cacheWrite": dollars(cost.cache_write),
            "total": dollars(cost.total)
        }
    })
}

fn pi_stop_reason(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Refusal | StopReason::PauseTurn | StopReason::Other(_) => "error",
        _ => "error",
    }
}

fn inferred_stop_reason(message: &AssistantMessage) -> StopReason {
    if message
        .content
        .iter()
        .any(|part| matches!(part, AssistantPart::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    }
}

fn assistant_content(message: &AssistantMessage) -> Vec<Value> {
    message
        .content
        .iter()
        .map(|part| match part {
            AssistantPart::Text(text) => json!({"type": "text", "text": text}),
            AssistantPart::Reasoning(reasoning) => {
                json!({"type": "thinking", "thinking": reasoning.text.as_deref().unwrap_or_default()})
            }
            AssistantPart::ToolCall(call) => json!({
                "type": "toolCall",
                "id": call.id.0,
                "name": call.name,
                "arguments": serde_json::from_str::<Value>(&call.arguments_json).unwrap_or(Value::Null)
            }),
            AssistantPart::Media(media) => media_content(media),
        })
        .collect()
}

fn assistant_value(
    message: &AssistantMessage,
    endpoint: &str,
    usage: &Usage,
    cost: Option<Cost>,
    stop_reason: &StopReason,
    timestamp: Option<u64>,
) -> Value {
    json!({
        "role": "assistant",
        "content": assistant_content(message),
        "api": protocol_name(&message.protocol),
        "provider": endpoint,
        "model": message.model.0,
        "usage": usage_value(usage, cost),
        "stopReason": pi_stop_reason(stop_reason),
        "timestamp": timestamp.map_or_else(now_millis, u128::from)
    })
}

fn tool_result_content(result: &ToolResult) -> Vec<Value> {
    result
        .content
        .iter()
        .map(|part| match part {
            ToolResultPart::Text(text) => json!({"type": "text", "text": text}),
            ToolResultPart::Media(media) => media_content(media),
        })
        .collect()
}

fn tool_result_value_at(result: &ToolResult, tool_name: &str, timestamp: u64) -> Value {
    json!({
        "role": "toolResult",
        "toolCallId": result.tool_call_id.0,
        "toolName": tool_name,
        "content": tool_result_content(result),
        "isError": result.is_error,
        "timestamp": timestamp
    })
}

fn tool_result_value(result: &ToolResult, tool_name: &str) -> Value {
    tool_result_value_at(
        result,
        tool_name,
        now_millis().min(u128::from(u64::MAX)) as u64,
    )
}

fn rpc_messages(app: &App) -> Vec<Value> {
    let usage_records = app
        .agent
        .session()
        .usage_records()
        .iter()
        .filter(|record| {
            matches!(
                &record.kind,
                ygg_agent::UsageRecordKind::AssistantTurn { .. }
            )
        })
        .collect::<Vec<_>>();
    let mut usage_index = 0usize;
    let mut tool_names = HashMap::<String, String>::new();
    let mut messages = Vec::new();
    for message in app.agent.session().context().unwrap_or_default() {
        match message {
            Message::Assistant(message) => {
                for part in &message.content {
                    if let AssistantPart::ToolCall(call) = part {
                        tool_names.insert(call.id.0.clone(), call.name.clone());
                    }
                }
                let record = usage_records.get(usage_index).copied();
                usage_index = usage_index.saturating_add(1);
                let reason = record
                    .and_then(|record| record.stop_reason.clone())
                    .unwrap_or_else(|| inferred_stop_reason(&message));
                messages.push(assistant_value(
                    &message,
                    record
                        .and_then(|record| record.endpoint.as_ref())
                        .map_or(app.model.endpoint.id.0.as_str(), |endpoint| {
                            endpoint.0.as_str()
                        }),
                    record.map_or(&Usage::default(), |record| &record.usage),
                    record.and_then(|record| record.cost),
                    &reason,
                    record.and_then(|record| record.completed_at_unix_ms),
                ));
            }
            Message::User(message) => {
                let mut ordinary = Vec::new();
                for part in &message.content {
                    match part {
                        UserPart::Text(text) => {
                            ordinary.push(json!({"type": "text", "text": text}));
                        }
                        UserPart::Media(media) => ordinary.push(media_content(media)),
                        UserPart::ToolResult(result) => {
                            if !ordinary.is_empty() {
                                messages.push(user_content(std::mem::take(&mut ordinary)));
                            }
                            let name = tool_names
                                .get(&result.tool_call_id.0)
                                .map_or("", String::as_str);
                            messages.push(tool_result_value(result, name));
                        }
                    }
                }
                if !ordinary.is_empty() {
                    messages.push(user_content(ordinary));
                }
            }
        }
    }
    messages
}

fn skill_source_info(path: &std::path::Path, trust: ygg_agent::SkillTrust) -> Value {
    let (source, scope) = match trust {
        ygg_agent::SkillTrust::UserInstalled | ygg_agent::SkillTrust::BuiltIn => ("local", "user"),
        ygg_agent::SkillTrust::Workspace => ("local", "project"),
        ygg_agent::SkillTrust::ExplicitExternal => ("cli", "temporary"),
    };
    json!({
        "path": path,
        "source": source,
        "scope": scope,
        "origin": "top-level",
        "baseDir": path.parent()
    })
}

fn prompt_source_info(path: &std::path::Path, trust: crate::prompts::PromptTrust) -> Value {
    let (source, scope) = match trust {
        crate::prompts::PromptTrust::UserInstalled => ("local", "user"),
        crate::prompts::PromptTrust::Workspace => ("local", "project"),
        crate::prompts::PromptTrust::ExplicitExternal => ("cli", "temporary"),
    };
    json!({
        "path": path,
        "source": source,
        "scope": scope,
        "origin": "top-level",
        "baseDir": path.parent()
    })
}

fn rpc_commands(app: &App) -> Value {
    let mut commands = Vec::new();
    for (name, description) in app.executable_extensions.command_suggestions() {
        commands.push(json!({
            "name": name,
            "description": description,
            "source": "extension",
            "sourceInfo": {
                "path": name,
                "source": "extension",
                "scope": "temporary",
                "origin": "top-level"
            }
        }));
    }
    for prompt in app.prompts.descriptors().iter() {
        commands.push(json!({
            "name": prompt.name,
            "description": prompt.description,
            "source": "prompt",
            "sourceInfo": prompt_source_info(&prompt.path, prompt.trust)
        }));
    }
    for skill in app.skills.descriptors().iter() {
        if let ygg_agent::SkillSource::FileSystem { entrypoint, .. } = &skill.source {
            commands.push(json!({
                "name": format!("skill:{}", skill.id),
                "description": skill.description,
                "source": "skill",
                "sourceInfo": skill_source_info(entrypoint, skill.trust)
            }));
        }
    }
    json!({"commands": commands})
}

#[derive(Clone)]
struct RpcSettings {
    steering_mode: String,
    follow_up_mode: String,
    auto_retry_enabled: bool,
}

impl Default for RpcSettings {
    fn default() -> Self {
        Self {
            steering_mode: "one-at-a-time".into(),
            follow_up_mode: "one-at-a-time".into(),
            auto_retry_enabled: true,
        }
    }
}

impl RpcSettings {
    fn steering_mode(&self) -> QueueDeliveryMode {
        queue_mode(&self.steering_mode)
    }

    fn follow_up_mode(&self) -> QueueDeliveryMode {
        queue_mode(&self.follow_up_mode)
    }
}

fn queue_mode(mode: &str) -> QueueDeliveryMode {
    if mode == "all" {
        QueueDeliveryMode::All
    } else {
        QueueDeliveryMode::OneAtATime
    }
}

#[derive(Clone)]
struct QueuedInput {
    text: String,
    input: UserInput,
    message: Value,
}

#[derive(Default)]
struct QueueState {
    steering: VecDeque<QueuedInput>,
    follow_up: VecDeque<QueuedInput>,
}

impl QueueState {
    fn len(&self) -> usize {
        self.steering.len().saturating_add(self.follow_up.len())
    }

    fn event(&self) -> Value {
        json!({
            "type": "queue_update",
            "steering": self.steering.iter().map(|queued| queued.text.as_str()).collect::<Vec<_>>(),
            "followUp": self.follow_up.iter().map(|queued| queued.text.as_str()).collect::<Vec<_>>()
        })
    }

    fn take_steering(&mut self, count: usize) -> Vec<QueuedInput> {
        (0..count)
            .filter_map(|_| self.steering.pop_front())
            .collect()
    }

    fn take_follow_up(&mut self, count: usize) -> Vec<QueuedInput> {
        (0..count)
            .filter_map(|_| self.follow_up.pop_front())
            .collect()
    }
}

fn session_id(app: &App) -> String {
    app.agent
        .session()
        .path()
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_owned()
}

struct RpcSessionProjection<'a> {
    app: &'a App,
    usage_by_assistant: HashMap<String, &'a ygg_agent::UsageRecord>,
    tool_names: HashMap<String, String>,
}

impl<'a> RpcSessionProjection<'a> {
    fn new(app: &'a App) -> Self {
        let usage_by_assistant = app
            .agent
            .session()
            .usage_records()
            .iter()
            .filter_map(|record| match &record.kind {
                ygg_agent::UsageRecordKind::AssistantTurn { assistant } => {
                    Some((assistant.0.clone(), record))
                }
                _ => None,
            })
            .collect();
        let mut tool_names = HashMap::new();
        for entry in app.agent.session().entries() {
            let EntryValue::Message(Message::Assistant(message)) = &entry.value else {
                continue;
            };
            for part in &message.content {
                if let AssistantPart::ToolCall(call) = part {
                    tool_names.insert(call.id.0.clone(), call.name.clone());
                }
            }
        }
        Self {
            app,
            usage_by_assistant,
            tool_names,
        }
    }

    fn user_message(&self, message: &UserMessage, timestamp: u64) -> Value {
        let mut tool_results = message.content.iter().filter_map(|part| match part {
            UserPart::ToolResult(result) => Some(result),
            _ => None,
        });
        if let Some(result) = tool_results.next() {
            if tool_results.next().is_none() {
                let tool_name = self
                    .tool_names
                    .get(&result.tool_call_id.0)
                    .map_or("", String::as_str);
                let mut value = tool_result_value_at(result, tool_name, timestamp);
                if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
                    content.extend(message.content.iter().filter_map(|part| match part {
                        UserPart::Text(text) => Some(json!({"type": "text", "text": text})),
                        UserPart::Media(media) => Some(media_content(media)),
                        UserPart::ToolResult(_) => None,
                    }));
                }
                return value;
            }
        }
        user_content_at(
            message.content.iter().map(|part| match part {
                UserPart::Text(text) => json!({"type": "text", "text": text}),
                UserPart::Media(media) => media_content(media),
                UserPart::ToolResult(result) => json!({
                    "type": "text",
                    "text": tool_result_content(result)
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
            }),
            timestamp,
        )
    }

    fn entry(&self, entry: &ygg_agent::Entry) -> Value {
        let timestamp_ms = entry.timestamp_unix_ms.unwrap_or_default();
        let mut value = Map::new();
        value.insert("id".into(), Value::String(entry.id.0.clone()));
        value.insert(
            "parentId".into(),
            entry
                .parent
                .as_ref()
                .map_or(Value::Null, |parent| Value::String(parent.0.clone())),
        );
        value.insert(
            "timestamp".into(),
            Value::String(iso_timestamp(timestamp_ms)),
        );

        match &entry.value {
            EntryValue::Message(Message::User(message)) => {
                value.insert("type".into(), Value::String("message".into()));
                value.insert("message".into(), self.user_message(message, timestamp_ms));
            }
            EntryValue::Message(Message::Assistant(message)) => {
                let record = self.usage_by_assistant.get(&entry.id.0).copied();
                let reason = record
                    .and_then(|record| record.stop_reason.clone())
                    .unwrap_or_else(|| inferred_stop_reason(message));
                let usage = record.map_or_else(Usage::default, |record| record.usage);
                let endpoint = record
                    .and_then(|record| record.endpoint.as_ref())
                    .map_or(self.app.model.endpoint.id.0.as_str(), |endpoint| {
                        endpoint.0.as_str()
                    });
                value.insert("type".into(), Value::String("message".into()));
                value.insert(
                    "message".into(),
                    assistant_value(
                        message,
                        endpoint,
                        &usage,
                        record.and_then(|record| record.cost),
                        &reason,
                        Some(timestamp_ms),
                    ),
                );
            }
            EntryValue::Config {
                model: Some(model), ..
            } => {
                let provider = self
                    .app
                    .catalog
                    .resolve(&ModelId(model.clone()))
                    .map_or_else(
                        |_| self.app.model.endpoint.id.0.clone(),
                        |resolved| resolved.endpoint.id.0.clone(),
                    );
                value.insert("type".into(), Value::String("model_change".into()));
                value.insert("provider".into(), Value::String(provider));
                value.insert("modelId".into(), Value::String(model.clone()));
            }
            EntryValue::Config {
                reasoning: Some(reasoning),
                ..
            } => {
                value.insert("type".into(), Value::String("thinking_level_change".into()));
                value.insert("thinkingLevel".into(), Value::String(reasoning.clone()));
            }
            EntryValue::Compaction {
                summary,
                first_kept,
                details,
                ..
            } => {
                value.insert("type".into(), Value::String("compaction".into()));
                value.insert("summary".into(), Value::String(summary.clone()));
                value.insert(
                    "firstKeptEntryId".into(),
                    Value::String(first_kept.0.clone()),
                );
                value.insert("tokensBefore".into(), json!(0));
                if let Ok(details) = serde_json::to_value(details) {
                    value.insert("details".into(), details);
                }
            }
            other => {
                let custom_type = match other {
                    EntryValue::Config { .. } => "ygg:config",
                    EntryValue::PromptTemplateSelected { .. } => "ygg:prompt-template-selected",
                    EntryValue::SkillActivated { .. } => "ygg:legacy-skill-activated",
                    EntryValue::SkillResourceRead { .. } => "ygg:legacy-skill-resource-read",
                    EntryValue::SkillDeactivated { .. } => "ygg:legacy-skill-deactivated",
                    EntryValue::ResponsesTurn { .. } => "ygg:responses-turn",
                    EntryValue::ResponsesCompaction { .. } => "ygg:responses-compaction",
                    EntryValue::Message(_) | EntryValue::Compaction { .. } => {
                        unreachable!("message and compaction entries are handled above")
                    }
                };
                value.insert("type".into(), Value::String("custom".into()));
                value.insert("customType".into(), Value::String(custom_type.into()));
                if let Ok(data) = serde_json::to_value(other) {
                    value.insert("data".into(), data);
                }
            }
        }
        Value::Object(value)
    }
}

fn rpc_tree(app: &App, projection: &RpcSessionProjection<'_>) -> Vec<Value> {
    let mut children = HashMap::<String, Vec<Value>>::new();
    let mut roots = Vec::new();
    for entry in app.agent.session().entries().iter().rev() {
        let mut entry_children = children.remove(&entry.id.0).unwrap_or_default();
        entry_children.reverse();
        let node = json!({
            "entry": projection.entry(entry),
            "children": entry_children
        });
        if let Some(parent) = &entry.parent {
            children.entry(parent.0.clone()).or_default().push(node);
        } else {
            roots.push(node);
        }
    }
    // Defensive replay rejects orphans, so only true roots remain here.
    roots.reverse();
    roots
}

fn state_value(
    app: &App,
    settings: &RpcSettings,
    streaming: bool,
    pending_messages: usize,
    message_count: Option<usize>,
) -> Value {
    let id = session_id(app);
    let session_name = app
        .sessions
        .load_metadata(&id)
        .ok()
        .and_then(|metadata| metadata.name);
    let mut state = json!({
        "model": model_value(&app.model),
        "thinkingLevel": reasoning_label(&app.reasoning),
        "isStreaming": streaming,
        "isCompacting": false,
        "steeringMode": settings.steering_mode,
        "followUpMode": settings.follow_up_mode,
        "sessionFile": app.agent.session().path(),
        "sessionId": id,
        "autoCompactionEnabled": app.config.compaction.mode != CompactionMode::Disabled,
        "messageCount": message_count.unwrap_or_else(|| rpc_messages(app).len()),
        "pendingMessageCount": pending_messages
    });
    if let Some(name) = session_name {
        state
            .as_object_mut()
            .expect("state is an object")
            .insert("sessionName".into(), Value::String(name));
    }
    state
}

fn input_from_command(command: &Value, text: String) -> anyhow::Result<UserInput> {
    let mut parts = vec![InputPart::Text(text)];
    if let Some(images) = command.as_object().and_then(|object| object.get("images")) {
        let images = images
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("images must be an array"))?;
        for image in images {
            let data = image
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("image data must be a base64 string"))?;
            let mime_type = image
                .get("mimeType")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("image mimeType must be a string"))?
                .parse::<mime::Mime>()?;
            if mime_type.type_() != mime::IMAGE {
                anyhow::bail!("RPC prompt images must use an image MIME type");
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|error| anyhow::anyhow!("invalid base64 image: {error}"))?;
            parts.push(InputPart::Media(Media::image_bytes(
                bytes.into(),
                mime_type,
            )));
        }
    }
    Ok(UserInput::from(parts))
}

async fn prepare_prompt(
    app: &mut App,
    command: &Value,
) -> anyhow::Result<(UserInput, usize, Value)> {
    let original = required_string(command, "message")?.to_owned();
    let mut expanded =
        expand_skill_command(app.skills.as_ref(), &original)?.unwrap_or(original.clone());
    if let Some(invocation) = expanded.trim().strip_prefix('/') {
        let split = invocation
            .find(char::is_whitespace)
            .unwrap_or(invocation.len());
        let (name, arguments) = invocation.split_at(split);
        if !name.is_empty() && app.prompts.contains(name) {
            let rendered = app.prompts.render(
                name,
                arguments.trim_start(),
                &PromptRenderContext {
                    workspace: &app.config.workspace,
                    selection: None,
                    active_skills: &[],
                },
            )?;
            app.agent
                .session_mut()
                .append(ygg_agent::EntryValue::PromptTemplateSelected {
                    name: rendered.name,
                    content_hash: rendered.content_hash,
                })?;
            expanded = rendered.text;
        }
    }
    let rendered = match crate::prompts::render_configured(app, &expanded)? {
        Some(rendered) => rendered.text,
        None => expanded,
    };
    app.executable_extensions.refresh_host_state(
        app.agent.session(),
        &app.model,
        &app.reasoning,
        &app.sessions,
    );
    let composition = app
        .executable_extensions
        .compose_prompt(&app.system, rendered)
        .await?;
    for notification in composition.notifications {
        crate::output::stderr_line(format!("extension: {notification}"));
    }
    app.agent.set_system_prompt(composition.system);
    app.agent.set_prompt_display_text(Some(original.clone()));
    let input = input_from_command(command, composition.prompt)?;
    let mut display_input = input.clone();
    if let Some(InputPart::Text(text)) = display_input.parts.first_mut() {
        *text = original;
    }
    let display_message = user_input_value(&display_input);
    Ok((input, composition.pending_context_count, display_message))
}

fn rpc_error_diagnostic(model: &Model, error: &AgentError) -> String {
    ygg_agent::public_error_diagnostic(error, &model.endpoint.id.0, &model.spec.id.0)
}

struct EventTranslator {
    endpoint: String,
    model: Model,
    api: String,
    partial_text: String,
    partial_reasoning: String,
    channels: Vec<OutputChannel>,
    message_started: bool,
    message_timestamp: u128,
    turn_open: bool,
    pending_turn: Option<Value>,
    pending_tool_results: Vec<Value>,
    expected_tools: usize,
    tools: HashMap<String, (String, Value, String)>,
    messages: Vec<Value>,
    run_messages: Vec<Value>,
    last_assistant_text: String,
    retry_attempt: Option<usize>,
    pending_retry_end: Option<Value>,
}

impl EventTranslator {
    fn new(app: &App, user: Value) -> Self {
        let mut messages = rpc_messages(app);
        messages.push(user.clone());
        Self {
            endpoint: app.model.endpoint.id.0.clone(),
            model: app.model.clone(),
            api: protocol_name(&app.model.spec.protocol).to_owned(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            channels: Vec::new(),
            message_started: false,
            message_timestamp: now_millis(),
            turn_open: true,
            pending_turn: None,
            pending_tool_results: Vec::new(),
            expected_tools: 0,
            tools: HashMap::new(),
            messages,
            run_messages: vec![user],
            last_assistant_text: String::new(),
            retry_attempt: None,
            pending_retry_end: None,
        }
    }

    fn partial_message(&self) -> Value {
        let content = self
            .channels
            .iter()
            .map(|channel| match channel {
                OutputChannel::Text => json!({"type": "text", "text": self.partial_text}),
                OutputChannel::Reasoning => {
                    json!({"type": "thinking", "thinking": self.partial_reasoning})
                }
            })
            .collect::<Vec<_>>();
        json!({
            "role": "assistant",
            "content": content,
            "api": self.api,
            "provider": self.endpoint,
            "model": self.model.spec.id.0,
            "usage": usage_value(&Usage::default(), None),
            "stopReason": null,
            "timestamp": self.message_timestamp
        })
    }

    fn ensure_turn_started(&mut self, output: &mut RpcOutput) -> anyhow::Result<()> {
        if !self.turn_open {
            output.send(json!({"type": "turn_start"}))?;
            self.turn_open = true;
        }
        Ok(())
    }

    fn begin_assistant(&mut self, output: &mut RpcOutput) -> anyhow::Result<()> {
        self.ensure_turn_started(output)?;
        if !self.message_started {
            self.message_started = true;
            output.send(json!({"type": "message_start", "message": self.partial_message()}))?;
        }
        Ok(())
    }

    fn emit_delta(
        &mut self,
        output: &mut RpcOutput,
        channel: OutputChannel,
        text: String,
    ) -> anyhow::Result<()> {
        self.begin_assistant(output)?;
        let first = !self.channels.contains(&channel);
        if first {
            self.channels.push(channel);
        }
        match channel {
            OutputChannel::Text => self.partial_text.push_str(&text),
            OutputChannel::Reasoning => self.partial_reasoning.push_str(&text),
        }
        let kind = match channel {
            OutputChannel::Text => "text",
            OutputChannel::Reasoning => "thinking",
        };
        let content_index = self
            .channels
            .iter()
            .position(|candidate| *candidate == channel)
            .unwrap_or_default();
        if first {
            output.send(json!({
                "type": "message_update",
                "message": self.partial_message(),
                "assistantMessageEvent": {
                    "type": format!("{kind}_start"),
                    "contentIndex": content_index,
                    "partial": self.partial_message()
                }
            }))?;
        }
        output.send(json!({
            "type": "message_update",
            "message": self.partial_message(),
            "assistantMessageEvent": {
                "type": format!("{kind}_delta"),
                "contentIndex": content_index,
                "delta": text,
                "partial": self.partial_message()
            }
        }))
    }

    fn emit_content_ends(&self, output: &mut RpcOutput, message: &Value) -> anyhow::Result<()> {
        for channel in &self.channels {
            let (kind, content) = match channel {
                OutputChannel::Text => ("text", self.partial_text.as_str()),
                OutputChannel::Reasoning => ("thinking", self.partial_reasoning.as_str()),
            };
            let content_index = self
                .channels
                .iter()
                .position(|candidate| candidate == channel)
                .unwrap_or_default();
            output.send(json!({
                "type": "message_update",
                "message": message,
                "assistantMessageEvent": {
                    "type": format!("{kind}_end"),
                    "contentIndex": content_index,
                    "content": content,
                    "partial": message
                }
            }))?;
        }
        Ok(())
    }

    fn emit_tool_call_updates(
        &self,
        output: &mut RpcOutput,
        assistant: &AssistantMessage,
        value: &Value,
    ) -> anyhow::Result<()> {
        for (content_index, part) in assistant.content.iter().enumerate() {
            let AssistantPart::ToolCall(call) = part else {
                continue;
            };
            let tool_call = json!({
                "type": "toolCall",
                "id": call.id.0,
                "name": call.name,
                "arguments": serde_json::from_str::<Value>(&call.arguments_json).unwrap_or(Value::Null)
            });
            output.send(json!({
                "type": "message_update",
                "message": value,
                "assistantMessageEvent": {
                    "type": "toolcall_start",
                    "contentIndex": content_index,
                    "id": call.id.0,
                    "name": call.name,
                    "partial": value
                }
            }))?;
            output.send(json!({
                "type": "message_update",
                "message": value,
                "assistantMessageEvent": {
                    "type": "toolcall_end",
                    "contentIndex": content_index,
                    "toolCall": tool_call,
                    "partial": value
                }
            }))?;
        }
        Ok(())
    }

    fn finish_pending_turn(&mut self, output: &mut RpcOutput) -> anyhow::Result<()> {
        let Some(message) = self.pending_turn.take() else {
            return Ok(());
        };
        output.send(json!({
            "type": "turn_end",
            "message": message,
            "toolResults": std::mem::take(&mut self.pending_tool_results)
        }))?;
        self.turn_open = false;
        Ok(())
    }

    fn reset_partial(&mut self) {
        self.partial_text.clear();
        self.partial_reasoning.clear();
        self.channels.clear();
        self.message_started = false;
        self.message_timestamp = now_millis();
    }

    fn deliver_queued(
        &mut self,
        output: &mut RpcOutput,
        queue: &mut QueueState,
        steering: bool,
        summaries: Vec<String>,
    ) -> anyhow::Result<()> {
        let mut delivered = if steering {
            queue.take_steering(summaries.len())
        } else {
            queue.take_follow_up(summaries.len())
        };
        output.send(queue.event())?;
        self.ensure_turn_started(output)?;
        if delivered.is_empty() {
            delivered.extend(summaries.into_iter().map(|text| QueuedInput {
                message: user_value(&text),
                input: UserInput::from(text.clone()),
                text,
            }));
        }
        for queued in delivered {
            output.send(json!({"type": "message_start", "message": queued.message}))?;
            output.send(json!({"type": "message_end", "message": queued.message}))?;
            self.messages.push(queued.message.clone());
            self.run_messages.push(queued.message);
        }
        Ok(())
    }

    fn finish_interrupted(
        &mut self,
        output: &mut RpcOutput,
        stop_reason: &str,
        error_message: Option<&str>,
    ) -> anyhow::Result<()> {
        if !self.turn_open {
            return Ok(());
        }
        self.begin_assistant(output)?;
        let mut message = self.partial_message();
        if let Some(object) = message.as_object_mut() {
            object.insert("stopReason".into(), Value::String(stop_reason.to_owned()));
            if let Some(error) = error_message {
                object.insert("errorMessage".into(), Value::String(error.to_owned()));
            }
        }
        self.emit_content_ends(output, &message)?;
        output.send(json!({"type": "message_end", "message": message}))?;
        self.messages.push(message.clone());
        self.run_messages.push(message.clone());
        self.pending_turn = Some(message);
        self.reset_partial();
        self.finish_pending_turn(output)
    }

    fn observe(
        &mut self,
        event: AgentEvent,
        output: &mut RpcOutput,
        queue: &mut QueueState,
    ) -> anyhow::Result<Option<FinishReason>> {
        match event {
            AgentEvent::OutputDelta { channel, text } => self.emit_delta(output, channel, text)?,
            // The final TurnFinished message carries generated media in the
            // Pi-compatible content array; no provisional RPC event exists.
            AgentEvent::OutputMedia { .. } => {}
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                delay,
                error,
            } => {
                if self.message_started {
                    output
                        .send(json!({"type": "message_end", "message": self.partial_message()}))?;
                }
                self.reset_partial();
                self.retry_attempt = Some(attempt);
                output.send(json!({
                    "type": "auto_retry_start",
                    "attempt": attempt,
                    "maxAttempts": max_attempts,
                    "delayMs": delay.as_millis(),
                    "errorMessage": error
                }))?;
            }
            AgentEvent::SteeringDelivered { messages } => {
                self.deliver_queued(output, queue, true, messages)?;
            }
            AgentEvent::FollowUpDelivered { messages } => {
                self.deliver_queued(output, queue, false, messages)?;
            }
            AgentEvent::CompactionStarted { reason } => {
                output.send(json!({
                    "type": "compaction_start",
                    "reason": format!("{reason:?}").to_ascii_lowercase()
                }))?;
            }
            AgentEvent::CompactionFinished { reason, result } => match result {
                Ok(info) => output.send(json!({
                    "type": "compaction_end",
                    "reason": format!("{reason:?}").to_ascii_lowercase(),
                    "result": {"summary": info.summary, "firstKeptEntryId": info.first_kept.0},
                    "aborted": false,
                    "willRetry": false
                }))?,
                Err(error) => output.send(json!({
                    "type": "compaction_end",
                    "reason": format!("{reason:?}").to_ascii_lowercase(),
                    "aborted": false,
                    "willRetry": false,
                    "errorMessage": error
                }))?,
            },
            AgentEvent::ToolStarted { id, name, args } => {
                self.tools
                    .insert(id.0.clone(), (name.clone(), args.clone(), String::new()));
                output.send(json!({
                    "type": "tool_execution_start",
                    "toolCallId": id.0,
                    "toolName": name,
                    "args": args
                }))?;
            }
            AgentEvent::ToolProgress { id, progress } => {
                if let Some((name, args, accumulated)) = self.tools.get_mut(&id.0) {
                    match progress {
                        ToolProgress::Output { bytes, .. } => {
                            accumulated.push_str(&String::from_utf8_lossy(&bytes));
                        }
                        ToolProgress::Status(status) => {
                            if !accumulated.is_empty() {
                                accumulated.push('\n');
                            }
                            accumulated.push_str(&status);
                        }
                        ToolProgress::Dropped { bytes, events } => {
                            accumulated.push_str(&format!(
                                "\n[dropped {bytes} bytes and {events} events]"
                            ));
                        }
                        ToolProgress::Confirmation(_)
                        | ToolProgress::Input(_)
                        | ToolProgress::SessionEvent(_, _) => {}
                    }
                    output.send(json!({
                        "type": "tool_execution_update",
                        "toolCallId": id.0,
                        "toolName": name,
                        "args": args,
                        "partialResult": {"content": [{"type": "text", "text": accumulated}]}
                    }))?;
                }
            }
            AgentEvent::ToolFinished { id, result } => {
                let (name, _args, _) = self
                    .tools
                    .remove(&id.0)
                    .unwrap_or_else(|| (String::new(), Value::Null, String::new()));
                let (text, is_error) = match result {
                    Ok(output_value) => (output_value.text, false),
                    Err(error) => (error.to_string(), true),
                };
                let result = json!({"content": [{"type": "text", "text": text}]});
                output.send(json!({
                    "type": "tool_execution_end",
                    "toolCallId": id.0,
                    "toolName": name,
                    "result": result,
                    "isError": is_error
                }))?;
                let tool_message = json!({
                    "role": "toolResult",
                    "toolCallId": id.0,
                    "toolName": name,
                    "content": [{"type": "text", "text": text}],
                    "isError": is_error,
                    "timestamp": now_millis()
                });
                output.send(json!({"type": "message_start", "message": tool_message}))?;
                output.send(json!({"type": "message_end", "message": tool_message}))?;
                self.messages.push(tool_message.clone());
                self.run_messages.push(tool_message.clone());
                self.pending_tool_results.push(tool_message);
                self.expected_tools = self.expected_tools.saturating_sub(1);
                if self.expected_tools == 0 {
                    self.finish_pending_turn(output)?;
                }
            }
            AgentEvent::CandidateRejected { .. } => {
                if self.message_started {
                    let mut message = self.partial_message();
                    message["stopReason"] = Value::String("stop".into());
                    self.emit_content_ends(output, &message)?;
                    output.send(json!({"type": "message_end", "message": message}))?;
                    output.send(json!({
                        "type": "turn_end",
                        "message": message,
                        "toolResults": []
                    }))?;
                }
                self.turn_open = false;
                self.reset_partial();
            }
            AgentEvent::TurnFinished {
                message,
                stop_reason,
                turn_usage,
                ..
            } => {
                self.ensure_turn_started(output)?;
                let cost = self
                    .model
                    .spec
                    .pricing
                    .as_ref()
                    .and_then(|pricing| ygg_ai::pricing::cost_of(pricing, &turn_usage).ok());
                let value = assistant_value(
                    &message,
                    &self.endpoint,
                    &turn_usage,
                    cost,
                    &stop_reason,
                    Some(now_millis() as u64),
                );
                if !self.message_started {
                    output.send(json!({"type": "message_start", "message": value}))?;
                } else {
                    self.emit_content_ends(output, &value)?;
                }
                self.emit_tool_call_updates(output, &message, &value)?;
                output.send(json!({"type": "message_end", "message": value}))?;
                if let Some(attempt) = self.retry_attempt.take() {
                    output.send(json!({
                        "type": "auto_retry_end",
                        "success": true,
                        "attempt": attempt
                    }))?;
                }
                self.last_assistant_text = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                self.expected_tools = message
                    .content
                    .iter()
                    .filter(|part| matches!(part, AssistantPart::ToolCall(_)))
                    .count();
                self.messages.push(value.clone());
                self.run_messages.push(value.clone());
                self.pending_turn = Some(value);
                self.reset_partial();
                if self.expected_tools == 0 {
                    self.finish_pending_turn(output)?;
                }
            }
            AgentEvent::RunFinished { reason, .. } => {
                self.finish_pending_turn(output)?;
                let (stop_reason, owned_error) = match &reason {
                    FinishReason::Completed => (None, None),
                    FinishReason::Aborted => {
                        (Some("aborted"), Some("Operation aborted".to_owned()))
                    }
                    FinishReason::MaxTurns => (
                        Some("error"),
                        Some("Maximum agent turns reached".to_owned()),
                    ),
                    FinishReason::Failed(error) => (
                        Some("error"),
                        Some(rpc_error_diagnostic(&self.model, error)),
                    ),
                };
                if let Some(stop_reason) = stop_reason {
                    self.finish_interrupted(output, stop_reason, owned_error.as_deref())?;
                }
                if let Some(attempt) = self.retry_attempt.take() {
                    self.pending_retry_end = Some(json!({
                        "type": "auto_retry_end",
                        "success": false,
                        "attempt": attempt,
                        "finalError": owned_error
                    }));
                }
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }
}

fn active_state_value(base: &Value, translator: &EventTranslator, queue: &QueueState) -> Value {
    let mut state = base.clone();
    if let Some(object) = state.as_object_mut() {
        object.insert("isStreaming".into(), Value::Bool(true));
        object.insert("messageCount".into(), json!(translator.messages.len()));
        object.insert("pendingMessageCount".into(), json!(queue.len()));
    }
    state
}

fn parse_queue_mode(mode: &str) -> anyhow::Result<QueueDeliveryMode> {
    match mode {
        "all" => Ok(QueueDeliveryMode::All),
        "one-at-a-time" => Ok(QueueDeliveryMode::OneAtATime),
        _ => anyhow::bail!("mode must be all or one-at-a-time"),
    }
}

fn expand_queued_prompt(
    skills: &dyn SkillRegistry,
    prompts: &PromptRegistry,
    workspace: &Path,
    raw: &str,
) -> anyhow::Result<String> {
    let expanded = expand_skill_command(skills, raw)?.unwrap_or_else(|| raw.to_owned());
    let invocation = expanded.trim().strip_prefix('/');
    let Some(invocation) = invocation else {
        return Ok(expanded);
    };
    let split = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());
    let (name, arguments) = invocation.split_at(split);
    if name.is_empty() || !prompts.contains(name) {
        return Ok(expanded);
    }
    let rendered = prompts.render(
        name,
        arguments.trim_start(),
        &PromptRenderContext {
            workspace,
            selection: None,
            // Legacy active skills are intentionally context-inert. Agent
            // Skills have already been expanded above when applicable.
            active_skills: &[],
        },
    )?;
    Ok(rendered.text)
}

fn queued_input(
    command: &Value,
    raw: &str,
    skills: &dyn SkillRegistry,
    prompts: &PromptRegistry,
    workspace: &Path,
) -> anyhow::Result<QueuedInput> {
    let expanded = expand_queued_prompt(skills, prompts, workspace, raw)?;
    let input = input_from_command(command, expanded)?;
    let mut display_input = input.clone();
    if let Some(InputPart::Text(text)) = display_input.parts.first_mut() {
        *text = raw.to_owned();
    }
    Ok(QueuedInput {
        text: raw.to_owned(),
        message: user_input_value(&display_input),
        input,
    })
}

// RPC active-run routing intentionally exposes the independently borrowed
// protocol registries, queues, settings, and output sink at this dispatch boundary.
#[allow(clippy::too_many_arguments)]
async fn active_input(
    command: Value,
    control: &RunControl,
    skills: &Arc<dyn SkillRegistry>,
    prompts: &PromptRegistry,
    workspace: &Path,
    state: &Value,
    commands: &Value,
    translator: &EventTranslator,
    queue: &mut QueueState,
    settings: &mut RpcSettings,
    output: &mut RpcOutput,
    deferred: &mut VecDeque<Value>,
) -> anyhow::Result<()> {
    let id = command_id(&command).map(str::to_owned);
    let kind = command_type(&command).unwrap_or("parse").to_owned();
    let result: anyhow::Result<()> = async {
        match kind.as_str() {
            "abort" => {
                control.abort();
                output.success(id.as_deref(), &kind, None)?;
            }
            "abort_retry" => {
                if translator.retry_attempt.is_some() {
                    control.abort();
                }
                output.success(id.as_deref(), &kind, None)?;
            }
            "steer" | "follow_up" => {
                let raw = required_string(&command, "message")?;
                let queued = queued_input(&command, raw, skills.as_ref(), prompts, workspace)?;
                if kind == "steer" {
                    control.steer(queued.input.clone()).await?;
                    queue.steering.push_back(queued);
                } else {
                    control.follow_up(queued.input.clone()).await?;
                    queue.follow_up.push_back(queued);
                }
                output.success(id.as_deref(), &kind, None)?;
                output.send(queue.event())?;
            }
            "prompt" => {
                let behavior = command
                    .get("streamingBehavior")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Agent is already streaming; specify streamingBehavior as steer or followUp"
                        )
                    })?;
                let raw = required_string(&command, "message")?;
                let queued = queued_input(&command, raw, skills.as_ref(), prompts, workspace)?;
                match behavior {
                    "steer" => {
                        control.steer(queued.input.clone()).await?;
                        queue.steering.push_back(queued);
                    }
                    "followUp" => {
                        control.follow_up(queued.input.clone()).await?;
                        queue.follow_up.push_back(queued);
                    }
                    _ => anyhow::bail!("streamingBehavior must be steer or followUp"),
                }
                output.success(id.as_deref(), "prompt", None)?;
                output.send(queue.event())?;
            }
            "set_steering_mode" => {
                let mode = required_string(&command, "mode")?;
                let delivery = parse_queue_mode(mode)?;
                control.set_steering_mode(delivery).await?;
                settings.steering_mode = mode.to_owned();
                output.success(id.as_deref(), &kind, None)?;
            }
            "set_follow_up_mode" => {
                let mode = required_string(&command, "mode")?;
                let delivery = parse_queue_mode(mode)?;
                control.set_follow_up_mode(delivery).await?;
                settings.follow_up_mode = mode.to_owned();
                output.success(id.as_deref(), &kind, None)?;
            }
            "get_state" => output.success(
                id.as_deref(),
                "get_state",
                Some(active_state_value(state, translator, queue)),
            )?,
            "get_messages" => output.success(
                id.as_deref(),
                "get_messages",
                Some(json!({"messages": translator.messages})),
            )?,
            "get_commands" => {
                output.success(id.as_deref(), "get_commands", Some(commands.clone()))?
            }
            // Mutating lifecycle commands are serialized at the first idle
            // boundary. Their response is deliberately delayed until applied.
            _ => deferred.push_back(command),
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        output.error(id.as_deref(), &kind, error.to_string())?;
    }
    Ok(())
}

// The run loop coordinates independently owned protocol state and channels;
// retaining explicit borrows makes mutation and queue ownership auditable.
#[allow(clippy::too_many_arguments)]
async fn drive_run(
    run: &mut Run<'_>,
    input: &mut mpsc::Receiver<RpcInput>,
    output: &mut RpcOutput,
    app_snapshot: &Value,
    commands: &Value,
    skills: Arc<dyn SkillRegistry>,
    prompts: Arc<PromptRegistry>,
    workspace: PathBuf,
    translator: &mut EventTranslator,
    queue: &mut QueueState,
    settings: &mut RpcSettings,
) -> anyhow::Result<(VecDeque<Value>, bool, FinishReason)> {
    let control = run.control();
    control.set_steering_mode(settings.steering_mode()).await?;
    control
        .set_follow_up_mode(settings.follow_up_mode())
        .await?;
    // A prior abort leaves RPC queues intact. Re-submit those messages to the
    // new agent run; delivery events remove them from QueueState exactly once.
    for queued in &queue.steering {
        control.steer(queued.input.clone()).await?;
    }
    for queued in &queue.follow_up {
        control.follow_up(queued.input.clone()).await?;
    }

    let mut deferred = VecDeque::new();
    let mut eof = false;
    let finish = loop {
        tokio::select! {
            biased;
            inbound = input.recv(), if !eof => match inbound {
                Some(RpcInput::Value(command)) => {
                    active_input(
                        command, &control, &skills, prompts.as_ref(), &workspace,
                        app_snapshot, commands, translator, queue, settings,
                        output, &mut deferred,
                    ).await?;
                }
                Some(RpcInput::ParseError(error)) => output.error(None, "parse", error)?,
                Some(RpcInput::Eof) | None => {
                    eof = true;
                    control.abort();
                }
            },
            event = run.next() => {
                let Some(event) = event else {
                    anyhow::bail!("agent run ended without RunFinished");
                };
                if let Some(reason) = translator.observe(event, output, queue)? {
                    break reason;
                }
            }
        }
    };
    Ok((deferred, eof, finish))
}

fn command_error(
    output: &mut RpcOutput,
    command: &Value,
    error: anyhow::Error,
) -> anyhow::Result<()> {
    output.error(
        command_id(command),
        command_type(command).unwrap_or("parse"),
        error.to_string(),
    )
}

async fn reload_resources(mut app: App) -> anyhow::Result<App> {
    app.system = compose_instructions(&app.config)?;
    app.system_tokens = estimate_text_tokens(&app.system);
    rebuild_app(app, None, None, None, None)
}

fn available_models(app: &App) -> Vec<Value> {
    let mut ids = app
        .catalog
        .models()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| left.0.cmp(&right.0));
    ids.into_iter()
        .filter_map(|id| app.catalog.resolve(&id).ok())
        .map(|model| model_value(&model))
        .collect()
}

fn context_usage_value(app: &App) -> Value {
    let context_window = app.model.spec.limits.context_window;
    let mut cursor = app.agent.session().head_ref();
    let mut has_fresh_assistant_usage = false;
    let mut unknown_after_compaction = false;
    while let Some(id) = cursor {
        let Some(entry) = app.agent.session().entry(id) else {
            break;
        };
        match &entry.value {
            EntryValue::Message(Message::Assistant(_))
                if app.agent.session().usage_records().iter().any(|record| {
                    matches!(
                        &record.kind,
                        ygg_agent::UsageRecordKind::AssistantTurn { assistant }
                            if assistant == id
                    )
                }) =>
            {
                has_fresh_assistant_usage = true;
            }
            EntryValue::Compaction { .. } | EntryValue::ResponsesCompaction { .. } => {
                unknown_after_compaction = !has_fresh_assistant_usage;
                break;
            }
            _ => {}
        }
        cursor = entry.parent.as_ref();
    }
    if unknown_after_compaction {
        return json!({
            "tokens": Value::Null,
            "contextWindow": context_window,
            "percent": Value::Null
        });
    }

    let messages = rpc_messages(app);
    let serialized = serde_json::to_string(&messages).unwrap_or_default();
    let tokens = app
        .system_tokens
        .saturating_add(estimate_text_tokens(&serialized));
    let percent = if context_window == 0 {
        0.0
    } else {
        tokens as f64 / context_window as f64 * 100.0
    };
    json!({
        "tokens": tokens,
        "contextWindow": context_window,
        "percent": percent
    })
}

fn session_stats_value(app: &App) -> Value {
    let mut user_messages = 0usize;
    let mut assistant_messages = 0usize;
    let mut tool_calls = 0usize;
    let mut tool_results = 0usize;
    let mut total_messages = 0usize;
    for entry in app.agent.session().entries() {
        match &entry.value {
            EntryValue::Message(Message::Assistant(message)) => {
                assistant_messages = assistant_messages.saturating_add(1);
                total_messages = total_messages.saturating_add(1);
                tool_calls = tool_calls.saturating_add(
                    message
                        .content
                        .iter()
                        .filter(|part| matches!(part, AssistantPart::ToolCall(_)))
                        .count(),
                );
            }
            EntryValue::Message(Message::User(message)) => {
                let results = message
                    .content
                    .iter()
                    .filter(|part| matches!(part, UserPart::ToolResult(_)))
                    .count();
                if results == 0 {
                    user_messages = user_messages.saturating_add(1);
                    total_messages = total_messages.saturating_add(1);
                } else {
                    tool_results = tool_results.saturating_add(results);
                    total_messages = total_messages.saturating_add(results);
                }
            }
            _ => {}
        }
    }

    let mut usage = Usage::default();
    for record in app.agent.session().usage_records() {
        usage.input_tokens = usage.input_tokens.saturating_add(record.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(record.usage.output_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(record.usage.cache_read_tokens);
        usage.cache_write_tokens = usage
            .cache_write_tokens
            .saturating_add(record.usage.cache_write_tokens);
    }
    let total_tokens = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    json!({
        "sessionFile": app.agent.session().path(),
        "sessionId": session_id(app),
        "userMessages": user_messages,
        "assistantMessages": assistant_messages,
        "toolCalls": tool_calls,
        "toolResults": tool_results,
        "totalMessages": total_messages,
        "tokens": {
            "input": usage.input_tokens,
            "output": usage.output_tokens,
            "cacheRead": usage.cache_read_tokens,
            "cacheWrite": usage.cache_write_tokens,
            "total": total_tokens
        },
        "cost": dollars(app.agent.session().total_cost_microdollars()),
        "contextUsage": context_usage_value(app)
    })
}

async fn handle_idle_command(
    mut app: App,
    command: Value,
    output: &mut RpcOutput,
    settings: &mut RpcSettings,
) -> anyhow::Result<App> {
    let id = command_id(&command).map(str::to_owned);
    let kind = command_type(&command).unwrap_or("parse").to_owned();
    macro_rules! respond_error {
        ($error:expr) => {{
            output.error(id.as_deref(), &kind, $error.to_string())?;
            return Ok(app);
        }};
    }

    match kind.as_str() {
        "get_state" => output.success(
            id.as_deref(),
            &kind,
            Some(state_value(&app, settings, false, 0, None)),
        )?,
        "get_messages" => output.success(
            id.as_deref(),
            &kind,
            Some(json!({"messages": rpc_messages(&app)})),
        )?,
        "get_commands" => output.success(id.as_deref(), &kind, Some(rpc_commands(&app)))?,
        "get_available_models" => output.success(
            id.as_deref(),
            &kind,
            Some(json!({"models": available_models(&app)})),
        )?,
        "get_available_thinking_levels" => output.success(
            id.as_deref(),
            &kind,
            Some(json!({
                "levels": supported_levels(&app.model)
                    .into_iter()
                    .map(|level| level.label())
                    .collect::<Vec<_>>()
            })),
        )?,
        "set_model" => {
            let model_id = match required_string(&command, "modelId") {
                Ok(model_id) => ModelId(model_id.to_owned()),
                Err(error) => respond_error!(error),
            };
            let model = match app.catalog.resolve(&model_id) {
                Ok(model) => model,
                Err(error) => respond_error!(error),
            };
            let value = model_value(&model);
            app = apply_reconfig(app, Reconfig::Model(model.spec.id.clone()))?;
            output.success(id.as_deref(), &kind, Some(value))?;
        }
        "cycle_model" => {
            let mut ids = app
                .catalog
                .models()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>();
            ids.sort_by(|left, right| left.0.cmp(&right.0));
            if ids.len() <= 1 {
                output.success(id.as_deref(), &kind, Some(Value::Null))?;
            } else {
                let index = ids
                    .iter()
                    .position(|candidate| candidate == &app.model.spec.id)
                    .unwrap_or_default();
                let model = app.catalog.resolve(&ids[(index + 1) % ids.len()])?;
                app = apply_reconfig(app, Reconfig::Model(model.spec.id.clone()))?;
                output.success(
                    id.as_deref(),
                    &kind,
                    Some(json!({
                        "model": model_value(&app.model),
                        "thinkingLevel": reasoning_label(&app.reasoning),
                        "isScoped": false
                    })),
                )?;
            }
        }
        "set_thinking_level" => {
            let level = match required_string(&command, "level").and_then(ThinkingLevel::parse) {
                Ok(level) => level,
                Err(error) => respond_error!(error),
            };
            let reasoning = match thinking_to_reasoning(level, &app.model) {
                Ok(reasoning) => reasoning,
                Err(error) => respond_error!(error),
            };
            app = apply_reconfig(app, Reconfig::Thinking(reasoning))?;
            output.success(id.as_deref(), &kind, None)?;
        }
        "cycle_thinking_level" => {
            let levels = supported_levels(&app.model);
            if levels.len() <= 1 {
                output.success(id.as_deref(), &kind, Some(Value::Null))?;
            } else {
                let current = reasoning_label(&app.reasoning);
                let index = levels
                    .iter()
                    .position(|level| level.label() == current)
                    .unwrap_or_default();
                let level = levels[(index + 1) % levels.len()];
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app = apply_reconfig(app, Reconfig::Thinking(reasoning))?;
                output.success(id.as_deref(), &kind, Some(json!({"level": level.label()})))?;
            }
        }
        "new_session" => {
            app = apply_reconfig(app, Reconfig::NewSession)?;
            output.success(id.as_deref(), &kind, Some(json!({"cancelled": false})))?;
        }
        "switch_session" => {
            let path = match required_string(&command, "sessionPath") {
                Ok(path) => PathBuf::from(path),
                Err(error) => respond_error!(error),
            };
            app = apply_reconfig(app, Reconfig::Resume(path))?;
            output.success(id.as_deref(), &kind, Some(json!({"cancelled": false})))?;
        }
        "reload" => {
            app = reload_resources(app).await?;
            output.success(id.as_deref(), &kind, None)?;
        }
        "compact" => {
            output.send(json!({"type": "compaction_start", "reason": "manual"}))?;
            let outcome = attempt_compaction(&mut app).await?;
            let data = match outcome {
                CompactionOutcome::Compacted { elided } => json!({"elided": elided}),
                CompactionOutcome::NativeCompacted => json!({"native": true}),
                CompactionOutcome::Skipped { reason } => json!({"skipped": true, "reason": reason}),
            };
            output.send(json!({"type": "compaction_end", "result": data}))?;
            output.success(id.as_deref(), &kind, Some(data))?;
        }
        "set_auto_compaction" => {
            let enabled = match required_bool(&command, "enabled") {
                Ok(enabled) => enabled,
                Err(error) => respond_error!(error),
            };
            app.config.compaction.mode = if enabled {
                CompactionMode::Local
            } else {
                CompactionMode::Disabled
            };
            app.agent.set_compaction_mode(
                if enabled {
                    AgentCompactionMode::Local
                } else {
                    AgentCompactionMode::Disabled
                },
                app.config.compaction.threshold_fraction,
                app.config.compaction.keep_recent_tokens,
            )?;
            output.success(id.as_deref(), &kind, None)?;
        }
        "set_steering_mode" | "set_follow_up_mode" => {
            let mode = match required_string(&command, "mode") {
                Ok("all" | "one-at-a-time") => required_string(&command, "mode")?.to_owned(),
                Ok(_) => respond_error!(anyhow::anyhow!("mode must be all or one-at-a-time")),
                Err(error) => respond_error!(error),
            };
            if kind == "set_steering_mode" {
                settings.steering_mode = mode;
            } else {
                settings.follow_up_mode = mode;
            }
            output.success(id.as_deref(), &kind, None)?;
        }
        "set_auto_retry" => {
            let enabled = match required_bool(&command, "enabled") {
                Ok(enabled) => enabled,
                Err(error) => respond_error!(error),
            };
            settings.auto_retry_enabled = enabled;
            app.agent.set_provider_retries_enabled(enabled);
            output.success(id.as_deref(), &kind, None)?;
        }
        "abort_retry" | "abort" | "abort_bash" => {
            output.success(id.as_deref(), &kind, None)?;
        }
        "get_session_stats" => {
            output.success(id.as_deref(), &kind, Some(session_stats_value(&app)))?
        }
        "get_entries" => {
            let entries = app.agent.session().entries();
            let start = if let Some(since) = command.get("since") {
                let Some(since) = since.as_str() else {
                    respond_error!(anyhow::anyhow!("since must be a string"));
                };
                let Some(index) = entries.iter().position(|entry| entry.id.0 == since) else {
                    respond_error!(anyhow::anyhow!("Entry not found: {since}"));
                };
                index.saturating_add(1)
            } else {
                0
            };
            let projection = RpcSessionProjection::new(&app);
            let projected = entries[start..]
                .iter()
                .map(|entry| projection.entry(entry))
                .collect::<Vec<_>>();
            output.success(
                id.as_deref(),
                &kind,
                Some(json!({
                    "entries": projected,
                    "leafId": app.agent.session().head_ref().map(|head| &head.0)
                })),
            )?;
        }
        "get_tree" => {
            let projection = RpcSessionProjection::new(&app);
            output.success(
                id.as_deref(),
                &kind,
                Some(json!({
                    "tree": rpc_tree(&app, &projection),
                    "leafId": app.agent.session().head_ref().map(|head| &head.0)
                })),
            )?;
        }
        "get_last_assistant_text" => {
            let text = app.agent.session().context().ok().and_then(|messages| {
                messages
                    .into_iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::Assistant(message) => Some(
                            message
                                .content
                                .iter()
                                .filter_map(|part| match part {
                                    AssistantPart::Text(text) => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<String>(),
                        ),
                        Message::User(_) => None,
                    })
            });
            output.success(id.as_deref(), &kind, Some(json!({"text": text})))?;
        }
        "set_session_name" => {
            let name = match required_string(&command, "name") {
                Ok(name) => name,
                Err(error) => respond_error!(error),
            };
            if let Err(error) = app.sessions.rename(&session_id(&app), name) {
                respond_error!(error);
            }
            output.success(id.as_deref(), &kind, None)?;
        }
        "export_html" | "fork" | "clone" | "get_fork_messages" => {
            output.error(
                id.as_deref(),
                &kind,
                format!("Command {kind:?} is not supported by Ygg RPC mode"),
            )?;
        }
        _ => output.error(id.as_deref(), &kind, format!("Unknown command: {kind}"))?,
    }
    Ok(app)
}

/// Run the strict LF-delimited JSONL RPC frontend until stdin closes.
pub async fn run_rpc(boot: Bootstrap) -> anyhow::Result<()> {
    let launch = resolve_launch_print(&boot, &crate::modes::timestamp())?;
    let system = compose_instructions(&boot.config)?;
    let mut app = build_app(boot, launch, system)?;
    let mut input = spawn_input_reader();
    let mut output = RpcOutput::new();
    let mut deferred = VecDeque::new();
    let mut settings = RpcSettings::default();
    let mut queue = QueueState::default();
    let mut eof = false;

    while !eof {
        let inbound = if let Some(command) = deferred.pop_front() {
            RpcInput::Value(command)
        } else {
            match input.recv().await {
                Some(input) => input,
                None => RpcInput::Eof,
            }
        };
        let command = match inbound {
            RpcInput::ParseError(error) => {
                output.error(None, "parse", error)?;
                continue;
            }
            RpcInput::Eof => break,
            RpcInput::Value(command) => command,
        };
        let Some(kind) = command_type(&command).map(str::to_owned) else {
            output.error(
                command_id(&command),
                "parse",
                "Command must be an object with a string type",
            )?;
            continue;
        };

        if kind == "bash" {
            let id = command_id(&command).map(str::to_owned);
            let shell_command = match required_string(&command, "command") {
                Ok(command) => command.to_owned(),
                Err(error) => {
                    output.error(id.as_deref(), "bash", error.to_string())?;
                    continue;
                }
            };
            let exclude_from_context = match optional_bool(&command, "excludeFromContext") {
                Ok(value) => value.unwrap_or(false),
                Err(error) => {
                    output.error(id.as_deref(), "bash", error.to_string())?;
                    continue;
                }
            };
            let workspace = app.config.workspace.clone();
            let sandbox = app.config.sandbox.to_sandbox_config(&workspace);
            let cancellation = CancellationToken::default();
            let task = tokio::spawn(run_sandboxed_bash(
                shell_command.clone(),
                workspace,
                sandbox,
                cancellation.clone(),
            ));
            let mut active = ActiveRpcBash {
                id,
                command: shell_command,
                exclude_from_context,
                cancellation,
                task,
            };

            // Commands deferred by an agent run retain their order, but an
            // already-buffered abort must be able to stop this command now.
            let mut held = VecDeque::new();
            while let Some(pending) = deferred.pop_front() {
                if command_type(&pending) == Some("abort_bash") {
                    active.cancellation.cancel();
                    output.success(command_id(&pending), "abort_bash", None)?;
                } else {
                    held.push_back(pending);
                }
            }
            let (result, newly_deferred, input_eof) =
                drive_rpc_bash(&mut active, &mut input, &mut output).await?;
            held.extend(newly_deferred);
            deferred = held;
            eof |= input_eof;

            match result {
                Ok(result) => {
                    if !active.exclude_from_context {
                        if let Err(error) = app.agent.session_mut().append(EntryValue::Message(
                            bash_context_message(&active.command, &result),
                        )) {
                            output.error(active.id.as_deref(), "bash", error.to_string())?;
                            continue;
                        }
                    }
                    output.success(active.id.as_deref(), "bash", Some(result.value()))?;
                }
                Err(error) => {
                    output.error(active.id.as_deref(), "bash", error.to_string())?;
                }
            }
            continue;
        }

        // steer/follow_up queue at idle; they're applied when the next
        // prompt starts (drive_run re-submits QueueState).
        if kind == "steer" || kind == "follow_up" {
            let id = command_id(&command).map(str::to_owned);
            let raw = match required_string(&command, "message") {
                Ok(message) => message.to_owned(),
                Err(error) => {
                    output.error(id.as_deref(), &kind, error.to_string())?;
                    continue;
                }
            };
            let queued = match queued_input(
                &command,
                &raw,
                app.skills.as_ref(),
                app.prompts.as_ref(),
                &app.config.workspace,
            ) {
                Ok(queued) => queued,
                Err(error) => {
                    output.error(id.as_deref(), &kind, error.to_string())?;
                    continue;
                }
            };
            if kind == "steer" {
                queue.steering.push_back(queued);
            } else {
                queue.follow_up.push_back(queued);
            }
            output.success(id.as_deref(), &kind, None)?;
            output.send(queue.event())?;
            continue;
        }

        if kind != "prompt" {
            app = handle_idle_command(app, command, &mut output, &mut settings).await?;
            continue;
        }

        // `/reload` is a resource command in every Ygg frontend. RPC applies it
        // synchronously at this idle boundary without creating a user turn.
        if command.get("message").and_then(Value::as_str) == Some("/reload") {
            let id = command_id(&command).map(str::to_owned);
            app = reload_resources(app).await?;
            output.success(id.as_deref(), "prompt", None)?;
            continue;
        }

        let id = command_id(&command).map(str::to_owned);
        let (prompt, pending_context_count, user_message) =
            match prepare_prompt(&mut app, &command).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    command_error(&mut output, &command, error)?;
                    continue;
                }
            };
        let skills = app.skills.clone();
        let prompts = app.prompts.clone();
        let workspace = app.config.workspace.clone();
        let commands = rpc_commands(&app);
        let state = state_value(&app, &settings, true, queue.len(), None);
        let mut translator = EventTranslator::new(&app, user_message.clone());
        app.agent
            .set_provider_retries_enabled(settings.auto_retry_enabled);
        let mut run = match app.agent.prompt(prompt).await {
            Ok(run) => run,
            Err(error) => {
                output.error(
                    id.as_deref(),
                    "prompt",
                    rpc_error_diagnostic(&translator.model, &error),
                )?;
                continue;
            }
        };
        app.executable_extensions
            .commit_prompt_context(pending_context_count);
        output.success(id.as_deref(), "prompt", None)?;
        output.send(json!({"type": "agent_start"}))?;
        output.send(json!({"type": "turn_start"}))?;
        output.send(json!({"type": "message_start", "message": user_message}))?;
        output.send(json!({"type": "message_end", "message": user_message}))?;

        let (queued, input_eof, finish) = drive_run(
            &mut run,
            &mut input,
            &mut output,
            &state,
            &commands,
            skills,
            prompts,
            workspace,
            &mut translator,
            &mut queue,
            &mut settings,
        )
        .await?;
        drop(run);
        app.agent.set_system_prompt(app.system.clone());
        deferred.extend(queued);
        eof = input_eof;
        output.send(json!({
            "type": "agent_end",
            "messages": translator.run_messages,
            "willRetry": false
        }))?;
        if let Some(event) = translator.pending_retry_end.take() {
            output.send(event)?;
        }
        output.send(json!({"type": "agent_settled"}))?;
        if matches!(finish, FinishReason::Completed) {
            for notification in app
                .executable_extensions
                .after_response(&translator.last_assistant_text)
                .await
            {
                crate::output::stderr_line(format!("extension: {notification}"));
            }
        }
    }

    app.executable_extensions.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_failure_diagnostic_omits_provider_payloads_and_codes() {
        let model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ModelId("gpt-4o-mini".into()))
            .unwrap();
        let error = AgentError::Ai(ygg_ai::AiError::Http(ygg_ai::HttpError {
            status: http::StatusCode::BAD_REQUEST,
            request_id: Some("secret-request-id".into()),
            retry_after: None,
            provider_code: Some("secret-provider-code".into()),
            body_snippet: Some("secret provider body with user prompt".into()),
            retryable: false,
        }));

        let diagnostic = rpc_error_diagnostic(&model, &error);
        assert_eq!(
            diagnostic,
            "provider=openai model=gpt-4o-mini phase=HTTP response"
        );
        assert!(!diagnostic.contains("secret"));
        assert!(!diagnostic.contains("400"));
        assert!(!diagnostic.contains("prompt"));
    }

    #[test]
    fn lf_framing_keeps_unicode_line_separators_inside_json() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut line = br#"{"type":"prompt","message":"a\u2028b"}"#.to_vec();
        dispatch_line(&tx, &mut line);
        let RpcInput::Value(value) = rx.try_recv().unwrap() else {
            panic!("expected parsed value");
        };
        assert_eq!(value["message"], "a\u{2028}b");
    }

    #[test]
    fn response_shape_omits_id_when_request_did() {
        let mut response = Map::new();
        response.insert("type".into(), Value::String("response".into()));
        response.insert("command".into(), Value::String("get_state".into()));
        response.insert("success".into(), Value::Bool(true));
        assert!(response.get("id").is_none());
    }
}
