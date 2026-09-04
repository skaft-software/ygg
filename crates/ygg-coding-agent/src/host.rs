#![allow(missing_docs)]

//! Versioned, bounded NDJSON host for non-Rust consumers.
//!
//! Standard output is protocol-only. Provider, discovery, and diagnostic logs
//! remain on standard error so one malformed dependency cannot corrupt IPC.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use ygg_agent::{
    AgentEvent, EntryValue, InputPart, OutputChannel, PolicyValueSource, Session,
    ToolPolicyProvenance, ToolProgress, UserInput,
};
use ygg_ai::{
    AssistantMessage, AssistantPart, AudioFormat, Auth, CacheRetention, Capabilities, Endpoint,
    EndpointId, Media, Message, Modality, ModalitySet, ModelLimits, ModelSpec,
    OpenAiChatReasoningMode, Protocol, ReasoningCapability, ReasoningControl, ReasoningEffort,
    UserMessage, UserPart,
};

use crate::app::bootstrap::{self, LaunchSelection, SessionSelection};
use crate::config::{
    ColorMode, CompactionPolicy, Config, Mode, MouseMode, ResumeSelector, SandboxPolicy, ToolPolicy,
};
use crate::modes::HostRunOutcome;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_PROMPT_BYTES: usize = 512 * 1024;
const MAX_PROMPT_DISPLAY_BYTES: usize = 256 * 1024;
const MAX_EVENT_TEXT_BYTES: usize = 256 * 1024;
const MAX_HISTORY_MESSAGES: usize = 256;
const MAX_HISTORY_BYTES: usize = 2 * 1024 * 1024;
const MAX_MEDIA_COUNT: usize = 12;
const MAX_IMAGE_COUNT: usize = 8;
const MAX_AUDIO_COUNT: usize = 4;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_AUDIO_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TOTAL_AUDIO_BYTES: u64 = 40 * 1024 * 1024;
const MAX_CUSTOM_HEADERS: usize = 64;
const MAX_CUSTOM_HEADER_BYTES: usize = 64 * 1024;
const MAX_API_KEY_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct HostRequest {
    protocol_version: u16,
    request_id: String,
    #[serde(flatten)]
    command: HostCommand,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum HostCommand {
    Hello,
    Models {
        #[serde(default)]
        offline: bool,
    },
    Run(Box<RunRequest>),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize)]
struct RunRequest {
    run_id: String,
    #[serde(default)]
    session_id: Option<String>,
    workspace: PathBuf,
    #[serde(default)]
    working_dir: Option<PathBuf>,
    #[serde(default)]
    session_dir: Option<PathBuf>,
    #[serde(default)]
    resume_session: Option<PathBuf>,
    model: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    custom_headers: HashMap<String, String>,
    #[serde(default)]
    provider_mode: Option<String>,
    #[serde(default)]
    context_window_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    input_modalities: Vec<HostInputModality>,
    #[serde(default)]
    supports_reasoning: bool,
    prompt: String,
    #[serde(default)]
    prompt_display_text: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default = "default_true")]
    allow_file_mutation: bool,
    #[serde(default)]
    allow_external_paths: bool,
    #[serde(default = "default_true")]
    context_files: bool,
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    max_turns: Option<u64>,
    #[serde(default)]
    max_cost_microdollars: Option<u64>,
    #[serde(default)]
    history: Vec<SeedMessage>,
    #[serde(default)]
    media: Vec<MediaInput>,
    #[serde(default)]
    image_paths: Vec<PathBuf>,
    #[serde(default)]
    prompt_paths: Vec<PathBuf>,
    #[serde(default)]
    skill_paths: Vec<PathBuf>,
    #[serde(default)]
    extension_paths: Vec<PathBuf>,
    #[serde(default)]
    enabled_extensions: Vec<String>,
    #[serde(default)]
    trusted_extensions: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HostInputModality {
    Image,
    Audio,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum MediaInput {
    Image { path: PathBuf },
    Audio { path: PathBuf },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedMessage {
    role: SeedRole,
    text: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SeedRole {
    User,
    Assistant,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct HostEvent<'a> {
    protocol_version: u16,
    request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    seq: u64,
    #[serde(rename = "type")]
    event_type: &'a str,
    data: serde_json::Value,
}

struct Emitter<'a> {
    output: &'a mut tokio::io::Stdout,
    request_id: String,
    run_id: Option<String>,
    session_id: Option<String>,
    seq: u64,
    terminal: bool,
}

impl<'a> Emitter<'a> {
    fn new(output: &'a mut tokio::io::Stdout, request_id: String) -> Self {
        Self {
            output,
            request_id,
            run_id: None,
            session_id: None,
            seq: 0,
            terminal: false,
        }
    }

    fn scoped(mut self, run_id: String, session_id: Option<String>) -> Self {
        self.run_id = Some(run_id);
        self.session_id = session_id;
        self
    }

    async fn emit(&mut self, event_type: &str, data: impl Serialize) -> anyhow::Result<()> {
        if self.terminal {
            anyhow::bail!("request already emitted a terminal protocol event");
        }
        self.seq = self.seq.saturating_add(1);
        let data = serde_json::to_value(data)?;
        let event = HostEvent {
            protocol_version: PROTOCOL_VERSION,
            request_id: &self.request_id,
            run_id: self.run_id.as_deref(),
            session_id: self.session_id.as_deref(),
            seq: self.seq,
            event_type,
            data,
        };
        let terminal = matches!(
            event_type,
            "final_result" | "protocol_error" | "hello" | "models" | "shutdown"
        );
        let serialized = serialize_bounded(&event, MAX_FRAME_BYTES.saturating_sub(1))?;
        let oversized = serialized.is_none();
        let mut line = match serialized {
            Some(line) => line,
            None => {
                self.terminal = true;
                eprintln!("ygg-host: dropping oversized outbound {event_type} event");
                serde_json::to_vec(&HostEvent {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: &self.request_id,
                    run_id: self.run_id.as_deref(),
                    session_id: self.session_id.as_deref(),
                    seq: self.seq,
                    event_type: "protocol_error",
                    data: serde_json::json!({
                        "error": "outbound event exceeded the protocol frame limit",
                        "discarded_type": event_type,
                    }),
                })?
            }
        };
        if line.len().saturating_add(1) > MAX_FRAME_BYTES {
            anyhow::bail!("protocol error event exceeded the outbound frame limit");
        }
        line.push(b'\n');
        self.output.write_all(&line).await?;
        self.output.flush().await?;
        if terminal {
            self.terminal = true;
        }
        if oversized {
            anyhow::bail!("outbound event exceeded the protocol frame limit");
        }
        Ok(())
    }
}

fn serialize_bounded<T: Serialize>(
    value: &T,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, serde_json::Error> {
    let mut writer = BoundedFrame::new(max_bytes);
    let result = serde_json::to_writer(&mut writer, value);
    if writer.overflowed {
        return Ok(None);
    }
    result?;
    Ok(Some(writer.bytes))
}

struct BoundedFrame {
    bytes: Vec<u8>,
    max_bytes: usize,
    overflowed: bool,
}

impl BoundedFrame {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8 * 1024)),
            max_bytes,
            overflowed: false,
        }
    }
}

impl std::io::Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(std::io::Error::other("protocol frame exceeds limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum Frame {
    Data(Vec<u8>),
    Incomplete,
    Oversized,
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Frame>> {
    let mut frame = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() && !oversized {
                Ok(None)
            } else {
                Ok(Some(Frame::Incomplete))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if !oversized {
            if frame.len().saturating_add(consumed) > MAX_FRAME_BYTES {
                oversized = true;
                frame.clear();
            } else {
                frame.extend_from_slice(&available[..consumed]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            if oversized {
                return Ok(Some(Frame::Oversized));
            }
            while matches!(frame.last(), Some(b'\n' | b'\r')) {
                frame.pop();
            }
            return Ok(Some(Frame::Data(frame)));
        }
    }
}

struct StrictJsonValue;

impl<'de> DeserializeSeed<'de> for StrictJsonValue {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StrictJsonValue {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a strict JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if !value.is_finite() {
            return Err(E::custom("non-finite JSON number"));
        }
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_string(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictJsonValue)? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {key:?}"
                )));
            }
            values.insert(key, map.next_value_seed(StrictJsonValue)?);
        }
        Ok(serde_json::Value::Object(values))
    }
}

fn parse_strict_json(bytes: &[u8]) -> Result<serde_json::Value, String> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictJsonValue
        .deserialize(&mut deserializer)
        .map_err(|error| error.to_string())?;
    deserializer.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn parse_request(bytes: &[u8]) -> Result<HostRequest, String> {
    let value = parse_strict_json(bytes)?;
    let object = value
        .as_object()
        .ok_or_else(|| "request must be a JSON object".to_owned())?;
    let command = object
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if let Some(field) = object
        .keys()
        .find(|field| !known_request_field(command, field))
    {
        return Err(format!("unknown request field {field:?}"));
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn known_request_field(command: &str, field: &str) -> bool {
    if matches!(field, "protocol_version" | "request_id" | "command") {
        return true;
    }
    match command {
        "hello" | "shutdown" => false,
        "models" => field == "offline",
        "run" => matches!(
            field,
            "run_id"
                | "session_id"
                | "workspace"
                | "working_dir"
                | "session_dir"
                | "resume_session"
                | "model"
                | "provider"
                | "base_url"
                | "api_key"
                | "custom_headers"
                | "provider_mode"
                | "context_window_tokens"
                | "max_output_tokens"
                | "vision"
                | "input_modalities"
                | "supports_reasoning"
                | "prompt"
                | "prompt_display_text"
                | "system_prompt"
                | "reasoning"
                | "tools"
                | "allow_file_mutation"
                | "allow_external_paths"
                | "context_files"
                | "offline"
                | "max_turns"
                | "max_cost_microdollars"
                | "history"
                | "media"
                | "image_paths"
                | "prompt_paths"
                | "skill_paths"
                | "extension_paths"
                | "enabled_extensions"
                | "trusted_extensions"
        ),
        // Let the tagged-enum deserializer produce the canonical unknown-command
        // diagnostic rather than misclassifying its accompanying fields.
        _ => true,
    }
}

async fn cleanup_host_processes() {
    ygg_agent::extension_process::begin_host_shutdown();
    ygg_agent::extension_process::terminate_bash_process_groups(std::time::Duration::from_millis(
        400,
    ))
    .await;
    ygg_agent::extension_process::force_kill_registered_process_groups();
}

enum RunRequestOutcome {
    Completed,
    Signaled,
}

/// Serve bounded NDJSON requests until `shutdown`, EOF, or a termination signal.
pub async fn run_stdio() -> anyhow::Result<()> {
    crate::tui::terminal::install_signal_restore()?;
    let result = run_stdio_loop().await;
    cleanup_host_processes().await;
    crate::tui::terminal::exit_if_signaled();
    result
}

async fn run_stdio_loop() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut input = BufReader::new(stdin);
    let mut output = tokio::io::stdout();

    loop {
        let frame = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => break,
            frame = read_frame(&mut input) => frame?,
        };
        let Some(frame) = frame else {
            break;
        };
        let bytes = match frame {
            Frame::Data(bytes) => bytes,
            Frame::Incomplete => {
                let mut emitter = Emitter::new(&mut output, "invalid".to_owned());
                emitter
                    .emit(
                        "protocol_error",
                        serde_json::json!({"error": "incomplete protocol frame at EOF"}),
                    )
                    .await?;
                break;
            }
            Frame::Oversized => {
                let mut emitter = Emitter::new(&mut output, "invalid".to_owned());
                emitter
                    .emit(
                        "protocol_error",
                        serde_json::json!({
                            "error": "request exceeded the protocol frame limit",
                            "max_bytes": MAX_FRAME_BYTES,
                        }),
                    )
                    .await?;
                continue;
            }
        };
        if bytes.is_empty() {
            continue;
        }
        let request = match parse_request(&bytes) {
            Ok(request) => request,
            Err(error) => {
                let mut emitter = Emitter::new(&mut output, "invalid".to_owned());
                emitter
                    .emit(
                        "protocol_error",
                        serde_json::json!({"error": format!("invalid request: {error}")}),
                    )
                    .await?;
                continue;
            }
        };
        if !valid_protocol_id(&request.request_id) {
            let mut emitter = Emitter::new(&mut output, "invalid".to_owned());
            emitter
                .emit(
                    "protocol_error",
                    serde_json::json!({"error": "request_id is invalid"}),
                )
                .await?;
            continue;
        }
        let mut emitter = Emitter::new(&mut output, request.request_id);
        if request.protocol_version != PROTOCOL_VERSION {
            emitter
                .emit(
                    "protocol_error",
                    serde_json::json!({
                        "error": "unsupported protocol version",
                        "received": request.protocol_version,
                        "supported": [PROTOCOL_VERSION],
                    }),
                )
                .await?;
            continue;
        }
        match request.command {
            HostCommand::Hello => emit_hello(&mut emitter).await?,
            HostCommand::Models { offline } => emit_models(&mut emitter, offline).await?,
            HostCommand::Run(run) => {
                if !valid_protocol_id(&run.run_id)
                    || run
                        .session_id
                        .as_deref()
                        .is_some_and(|id| !valid_session_id(id))
                {
                    emitter
                        .emit(
                            "protocol_error",
                            serde_json::json!({"error": "run_id or session_id is invalid"}),
                        )
                        .await?;
                    continue;
                }
                let mut emitter = emitter.scoped(run.run_id.clone(), run.session_id.clone());
                match run_request(&mut emitter, *run).await {
                    Ok(RunRequestOutcome::Completed) => {}
                    Ok(RunRequestOutcome::Signaled) => break,
                    Err(_) if crate::tui::terminal::received_shutdown_signal().is_some() => break,
                    Err(error) => {
                        if emitter.terminal {
                            continue;
                        }
                        emitter
                            .emit(
                                "final_result",
                                serde_json::json!({
                                    "status": "error",
                                    "output": "",
                                    "error": clip_text(&error.to_string(), MAX_EVENT_TEXT_BYTES),
                                    "filesChanged": [],
                                    "toolCalls": 0,
                                    "steps": 0,
                                    "sessionFile": "",
                                }),
                            )
                            .await?;
                    }
                }
            }
            HostCommand::Shutdown => {
                emitter
                    .emit("shutdown", serde_json::json!({"accepted": true}))
                    .await?;
                break;
            }
        }
    }
    Ok(())
}

async fn emit_hello(emitter: &mut Emitter<'_>) -> anyhow::Result<()> {
    emitter
        .emit(
            "hello",
            serde_json::json!({
                "sdk_version": env!("CARGO_PKG_VERSION"),
                "protocol_version": PROTOCOL_VERSION,
                "max_frame_bytes": MAX_FRAME_BYTES,
                "max_concurrent_runs": 1,
                "commands": ["hello", "models", "run", "shutdown"],
                "features": {
                    "streaming": true,
                    "persistent_sessions": true,
                    "seed_history": true,
                    "typed_media_input": true,
                    "typed_image_input": true,
                    "typed_audio_input": true,
                    "prompt_display_text": true,
                    "inline_models": true,
                    "tools": true,
                    "skills": true,
                    "extensions": true,
                    "process_group_abort": true,
                    "in_band_abort": false,
                },
            }),
        )
        .await
}

async fn emit_models(emitter: &mut Emitter<'_>, offline: bool) -> anyhow::Result<()> {
    let catalog = if offline {
        // The public no-network path is built through a minimal host config.
        let workspace = std::env::current_dir()?.canonicalize()?;
        let config = host_config(&RunRequest {
            run_id: "models".into(),
            session_id: None,
            workspace: workspace.clone(),
            working_dir: Some(workspace.clone()),
            session_dir: Some(workspace.join(".ygg/sessions")),
            resume_session: None,
            model: "unused".into(),
            provider: None,
            base_url: None,
            api_key: None,
            custom_headers: HashMap::new(),
            provider_mode: None,
            context_window_tokens: None,
            max_output_tokens: None,
            vision: false,
            input_modalities: Vec::new(),
            supports_reasoning: false,
            prompt: String::new(),
            prompt_display_text: None,
            system_prompt: None,
            reasoning: None,
            tools: Some(Vec::new()),
            allow_file_mutation: false,
            allow_external_paths: false,
            context_files: false,
            offline: true,
            max_turns: Some(1),
            max_cost_microdollars: None,
            history: Vec::new(),
            media: Vec::new(),
            image_paths: Vec::new(),
            prompt_paths: Vec::new(),
            skill_paths: Vec::new(),
            extension_paths: Vec::new(),
            enabled_extensions: Vec::new(),
            trusted_extensions: Vec::new(),
        })?;
        bootstrap::bootstrap(config)?.catalog
    } else {
        bootstrap::model_catalog()?
    };
    let mut models = catalog
        .models()
        .map(|model| {
            let effective = model.effective_input_modalities();
            let mut input_modalities = vec!["text"];
            if effective.contains(Modality::Image) {
                input_modalities.push("image");
            }
            if effective.contains(Modality::Audio) {
                input_modalities.push("audio");
            }
            serde_json::json!({
                "id": model.id.0,
                "provider": model.endpoint.0,
                "api_name": model.api_name,
                "display_name": model.display_name,
                "protocol": format!("{:?}", model.protocol),
                "context_window": model.limits.context_window,
                "max_output_tokens": model.limits.max_output_tokens,
                "tools": model.capabilities.tools,
                "vision": effective.contains(Modality::Image),
                "audio": effective.contains(Modality::Audio),
                "input_modalities": input_modalities,
                "reasoning": model.capabilities.reasoning.is_some(),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    emitter
        .emit("models", serde_json::json!({"models": models}))
        .await
}

async fn run_request(
    emitter: &mut Emitter<'_>,
    request: RunRequest,
) -> anyhow::Result<RunRequestOutcome> {
    validate_run_request(&request)?;
    let config = host_config(&request)?;
    let system = match request.system_prompt.as_deref() {
        Some(system) => system.to_owned(),
        None => crate::resources::compose_instructions(&config)?,
    };
    let mut boot = bootstrap::bootstrap(config)?;
    let model_id = register_inline_model(&mut boot.catalog, &request)?;
    boot.config.model = Some(model_id.clone());
    let (selection, prepared_session) = session_selection(&boot.config.session_dir, &request)?;
    if let Some(session) = prepared_session {
        boot.set_prepared_session(session);
    }
    let session_path = match &selection {
        SessionSelection::CreateNew(path) | SessionSelection::OpenExisting(path) => path.clone(),
    };
    let new_session = matches!(selection, SessionSelection::CreateNew(_));
    let launch = LaunchSelection {
        model: model_id,
        session: selection,
        reasoning: request
            .reasoning
            .as_deref()
            .map(crate::config::parse_reasoning)
            .transpose()?
            .unwrap_or(ygg_ai::ReasoningConfig::Off),
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
    };
    let mut app = bootstrap::build_app(boot, launch, system)?;
    if new_session {
        seed_history(&mut app, &request.history)?;
    }

    emitter
        .emit(
            "accepted",
            serde_json::json!({
                "model": request.model,
                "resolved_model": app.model.spec.id.0,
                "session_file": session_path,
                "registered_tools": app.agent.registered_tool_names(),
                "effective_tool_policy": app.config.sandbox.effective_tool_policy(
                    &app.config.workspace,
                    app.config.effect_policy,
                ),
                "extensions": app.executable_extensions.summaries(),
            }),
        )
        .await?;

    app.executable_extensions.refresh_host_state(
        app.agent.session(),
        &app.model,
        &app.reasoning,
        &app.sessions,
    );
    let composition = app
        .executable_extensions
        .compose_prompt(&app.system, request.prompt.clone())
        .await?;
    for notification in &composition.notifications {
        emitter
            .emit(
                "extension_notification",
                serde_json::json!({"message": clip_text(notification, 16 * 1024)}),
            )
            .await?;
    }
    let pending_context_count = composition.pending_context_count;
    app.agent.set_system_prompt(composition.system);
    app.agent.set_prompt_display_text(Some(
        request
            .prompt_display_text
            .clone()
            .unwrap_or_else(|| request.prompt.clone()),
    ));
    let input = load_user_input(&request, composition.prompt, &app.model.spec)?;
    let mut run = match app.agent.prompt(input).await {
        Ok(run) => run,
        Err(error) => anyhow::bail!(
            "{}",
            ygg_agent::public_error_diagnostic(
                &error,
                &app.model.endpoint.id.0,
                &app.model.spec.id.0,
            )
        ),
    };
    let extension_turn = app.executable_extensions.begin_turn().await;
    let control = run.control();
    app.executable_extensions
        .commit_prompt_context(pending_context_count);

    emitter
        .emit("started", serde_json::json!({"model": request.model}))
        .await?;
    let mut pending_text = String::new();
    let mut final_output = String::new();
    let mut tool_calls = 0u64;
    let mut steps = 0u64;
    let mut files_changed = BTreeSet::new();
    let mut active_tools: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    let mut terminal_head = None;

    let outcome = loop {
        let event = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                control.abort();
                ygg_agent::extension_process::terminate_bash_process_groups(
                    std::time::Duration::from_millis(400),
                )
                .await;
                break HostRunOutcome::shutdown();
            }
            event = run.next() => event,
        };
        let Some(event) = event else {
            break HostRunOutcome::stream_lost();
        };
        match event {
            AgentEvent::OutputDelta { channel, text } => {
                if channel == OutputChannel::Text {
                    append_bounded(&mut pending_text, &text, MAX_EVENT_TEXT_BYTES);
                }
                emitter
                    .emit(
                        "model_delta",
                        serde_json::json!({
                            "channel": match channel {
                                OutputChannel::Text => "text",
                                OutputChannel::Reasoning => "reasoning",
                            },
                            "text": clip_text(&text, 64 * 1024),
                        }),
                    )
                    .await?;
            }
            AgentEvent::OutputMedia { index, media } => {
                emitter
                    .emit("output_media", media_payload(index, &media))
                    .await?;
            }
            AgentEvent::ProviderLifecycle { lifecycle } => {
                // This is a structured protocol event, not human-facing stdout
                // diagnostics or assistant content.
                emitter
                    .emit(
                        "provider_lifecycle",
                        serde_json::json!({
                            "state": lifecycle.state.as_str(),
                            "detail": lifecycle.detail.as_deref().map(|detail| clip_text(detail, 512)),
                        }),
                    )
                    .await?;
            }
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                delay,
                error,
            } => {
                pending_text.clear();
                emitter
                    .emit(
                        "provider_retry",
                        serde_json::json!({
                            "attempt": attempt,
                            "max_attempts": max_attempts,
                            "delay_ms": delay.as_millis(),
                            "error": clip_text(&error, 16 * 1024),
                        }),
                    )
                    .await?;
            }
            AgentEvent::SteeringDelivered { messages } => {
                emitter
                    .emit(
                        "steering_delivered",
                        serde_json::json!({"messages": messages}),
                    )
                    .await?;
            }
            AgentEvent::FollowUpDelivered { messages } => {
                emitter
                    .emit(
                        "follow_up_delivered",
                        serde_json::json!({"messages": messages}),
                    )
                    .await?;
            }
            AgentEvent::CompactionStarted { reason } => {
                emitter
                    .emit(
                        "compaction_start",
                        serde_json::json!({"reason": compaction_reason_label(reason)}),
                    )
                    .await?;
            }
            AgentEvent::CompactionFinished { reason, result } => {
                let data = match result {
                    Ok(info) => serde_json::json!({
                        "reason": compaction_reason_label(reason),
                        "ok": true,
                        "kind": compaction_kind_payload(&info.kind),
                        "summary": clip_text(&info.summary, MAX_EVENT_TEXT_BYTES),
                        "first_kept_entry_id": info.first_kept.0,
                    }),
                    Err(error) => serde_json::json!({
                        "reason": compaction_reason_label(reason),
                        "ok": false,
                        "error": clip_text(&error, 64 * 1024),
                    }),
                };
                emitter.emit("compaction_finish", data).await?;
            }
            AgentEvent::ToolStarted { id, name, args } => {
                tool_calls = tool_calls.saturating_add(1);
                active_tools.insert(id.0.clone(), (name.clone(), args.clone()));
                emitter
                    .emit(
                        "tool_start",
                        serde_json::json!({
                            "toolCallId": id.0,
                            "toolName": name,
                            "input": args,
                        }),
                    )
                    .await?;
            }
            AgentEvent::ToolPolicyDecision { id, name, decision } => {
                emitter
                    .emit(
                        "tool_policy",
                        serde_json::json!({
                            "toolCallId": id.0,
                            "toolName": name,
                            "decision": decision,
                        }),
                    )
                    .await?;
            }
            AgentEvent::ToolProgress { id, progress } => {
                let data = progress_payload(progress);
                emitter
                    .emit(
                        "tool_progress",
                        serde_json::json!({"toolCallId": id.0, "progress": data}),
                    )
                    .await?;
            }
            AgentEvent::ToolFinished { id, result, .. } => {
                if result.as_ref().is_ok_and(|output| !output.is_error()) {
                    if let Some((name, args)) = active_tools.get(&id.0) {
                        if matches!(name.as_str(), "edit" | "write") {
                            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str)
                            {
                                files_changed.insert(path.to_owned());
                            }
                        }
                    }
                }
                let (ok, output, error) = match result {
                    Ok(output) if output.is_error() => {
                        (false, String::new(), clip_text(&output.text, 64 * 1024))
                    }
                    Ok(output) => (true, clip_text(&output.text, 64 * 1024), String::new()),
                    Err(error) => (
                        false,
                        String::new(),
                        clip_text(&error.to_string(), 64 * 1024),
                    ),
                };
                emitter
                    .emit(
                        "tool_finish",
                        serde_json::json!({
                            "toolCallId": id.0,
                            "ok": ok,
                            "output": output,
                            "error": error,
                        }),
                    )
                    .await?;
                active_tools.remove(&id.0);
            }
            AgentEvent::CandidateRejected {
                usage,
                run_cost_microdollars,
                session_cost_microdollars,
            } => {
                pending_text.clear();
                emitter
                    .emit(
                        "candidate_rejected",
                        serde_json::json!({
                            "run_usage": usage,
                            "run_cost_microdollars": run_cost_microdollars,
                            "session_cost_microdollars": session_cost_microdollars,
                            "discard_provisional_output": true,
                        }),
                    )
                    .await?;
            }
            AgentEvent::TurnFinished {
                message,
                turn_usage,
                usage,
                session_cost_microdollars,
                run_cost_microdollars,
                ..
            } => {
                steps = steps.saturating_add(1);
                final_output = assistant_text(&message);
                pending_text.clear();
                emitter
                    .emit(
                        "model_step",
                        serde_json::json!({
                            "step": steps,
                            "turn_usage": turn_usage,
                            "run_usage": usage,
                            "session_cost_microdollars": session_cost_microdollars,
                            "run_cost_microdollars": run_cost_microdollars,
                        }),
                    )
                    .await?;
            }
            AgentEvent::DelegationUpdated { snapshot } => {
                emitter
                    .emit(
                        "delegation_updated",
                        serde_json::json!({ "snapshot": snapshot }),
                    )
                    .await?;
            }
            // Attempt boundaries are not part of the host protocol surface.
            AgentEvent::TurnStarted => {}
            AgentEvent::RunFinished { head, reason } => {
                terminal_head = Some(head.0);
                break HostRunOutcome::from_finish_reason(
                    &reason,
                    &app.model.endpoint.id.0,
                    &app.model.spec.id.0,
                );
            }
        }
    };
    drop(run);
    app.executable_extensions
        .settle_turn(extension_turn, &outcome)
        .await;
    app.agent.set_system_prompt(app.system.clone());
    let (status, terminal_error) = match &outcome {
        HostRunOutcome::Completed => ("completed", String::new()),
        HostRunOutcome::Aborted => ("blocked", "run aborted".to_owned()),
        HostRunOutcome::MaxTurns => (
            "blocked",
            "run reached the configured turn limit".to_owned(),
        ),
        HostRunOutcome::Failed(error) => ("error", error.clone()),
        HostRunOutcome::StreamLost | HostRunOutcome::Shutdown => (
            "error",
            outcome.failure_message().unwrap_or_default().to_owned(),
        ),
    };
    let terminal_head = terminal_head
        .or_else(|| app.agent.session().head().map(|head| head.0.clone()))
        .unwrap_or_default();
    if outcome.shutdown_requested() {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1400),
            app.executable_extensions.shutdown(),
        )
        .await;
        ygg_agent::extension_process::force_kill_registered_process_groups();
        // Process shutdown truncates the in-flight protocol request. Do not
        // emit a non-terminal `settled` event that cannot be followed by the
        // contractually required `final_result` or `protocol_error`.
        return Ok(RunRequestOutcome::Signaled);
    }
    emitter
        .emit(
            "settled",
            serde_json::json!({
                "status": status,
                "head": terminal_head,
                "error": clip_text(&terminal_error, 64 * 1024),
            }),
        )
        .await?;
    if outcome.allows_after_response() {
        for notification in app
            .executable_extensions
            .after_response(&final_output)
            .await
        {
            emitter
                .emit(
                    "extension_notification",
                    serde_json::json!({"message": clip_text(&notification, 16 * 1024)}),
                )
                .await?;
        }
    }
    app.executable_extensions.shutdown().await;
    emitter
        .emit(
            "final_result",
            serde_json::json!({
                "status": status,
                "output": clip_text(&final_output, MAX_EVENT_TEXT_BYTES),
                "error": clip_text(&terminal_error, MAX_EVENT_TEXT_BYTES),
                "filesChanged": files_changed,
                "toolCalls": tool_calls,
                "steps": steps,
                "sessionFile": session_path,
            }),
        )
        .await?;
    Ok(RunRequestOutcome::Completed)
}

fn canonicalize_with_missing_tail(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or(error)?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path has no existing ancestor",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn host_config(request: &RunRequest) -> anyhow::Result<Config> {
    let workspace = request
        .workspace
        .canonicalize()
        .with_context(|| format!("workspace {} is unavailable", request.workspace.display()))?;
    let invocation_cwd = request
        .working_dir
        .as_deref()
        .unwrap_or(&workspace)
        .canonicalize()
        .with_context(|| "working directory is unavailable")?;
    if !invocation_cwd.starts_with(&workspace) {
        anyhow::bail!("working directory must stay inside the workspace");
    }
    let requested_session_dir = request
        .session_dir
        .clone()
        .unwrap_or_else(|| workspace.join(".ygg/sessions"));
    let session_dir = if requested_session_dir.is_absolute() {
        requested_session_dir
    } else {
        workspace.join(requested_session_dir)
    };
    if session_dir.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        anyhow::bail!("session directory must not contain '.' or '..' components");
    }
    let session_dir =
        canonicalize_with_missing_tail(&session_dir).context("session directory is unavailable")?;
    if !request.allow_external_paths && !session_dir.starts_with(&workspace) {
        anyhow::bail!(
            "session directory must stay inside the workspace unless allow_external_paths is enabled"
        );
    }
    // Protocol v1 is fixed to Controlled. Host-supplied paths may use the
    // request boundary above, but model-controlled file tools must remain
    // workspace-relative under that effect policy.
    let mut sandbox = SandboxPolicy {
        allow_external_paths: false,
        policy_provenance: ToolPolicyProvenance::all(PolicyValueSource::HostRequest),
        ..SandboxPolicy::default()
    };
    if !request.allow_file_mutation {
        sandbox.allow_edit = false;
        sandbox.allow_write = false;
        sandbox.allow_process = false;
        sandbox.allow_shell = false;
    }
    let tools = match &request.tools {
        Some(tools) => ToolPolicy::only(tools.clone())?,
        None => ToolPolicy::default(),
    };
    Ok(Config {
        workspace,
        invocation_cwd,
        model: Some(ygg_ai::ModelId(request.model.clone())),
        model_explicit: true,
        reasoning: request
            .reasoning
            .as_deref()
            .map(crate::config::parse_reasoning)
            .transpose()?
            .unwrap_or(ygg_ai::ReasoningConfig::Off),
        reasoning_explicit: request.reasoning.is_some(),
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        reasoning_mode_explicit: true,
        cache_retention: CacheRetention::Short,
        effect_policy: ygg_agent::EffectPolicy::Controlled,
        sandbox,
        theme: None,
        system_prompt: request.system_prompt.clone(),
        theme_paths: Vec::new(),
        color: ColorMode::Never,
        mouse: MouseMode::Off,
        plain: true,
        show_images: false,
        session_dir,
        compaction: CompactionPolicy::default(),
        max_cost_microdollars: request.max_cost_microdollars,
        cost_warning_microdollars: None,
        max_turns: request.max_turns,
        show_reasoning_in_print: false,
        initial_prompt: None,
        prompt_template: None,
        debug_prompt: false,
        prompt_paths: request.prompt_paths.clone(),
        mode: Mode::Print {
            prompt: request.prompt.clone(),
        },
        resume: ResumeSelector::New,
        skill_paths: request.skill_paths.clone(),
        extension_paths: request.extension_paths.clone(),
        enabled_extensions: request.enabled_extensions.clone(),
        extension_activation_overridden: true,
        trusted_extensions: request.trusted_extensions.clone(),
        invocation_trusted_extensions: Vec::new(),
        tools,
        telemetry: None,
        context_files: request.context_files,
        offline: request.offline,
        workspace_trusted: true,
    })
}

fn register_inline_model(
    catalog: &mut ygg_ai::ModelCatalog,
    request: &RunRequest,
) -> anyhow::Result<ygg_ai::ModelId> {
    let Some(raw_base_url) = request
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        return Ok(ygg_ai::ModelId(request.model.clone()));
    };
    let base_url = parse_inline_base_url(raw_base_url)?;
    let protocol = inline_protocol(request.provider_mode.as_deref())?;
    let route_digest = inline_route_digest(
        &base_url,
        protocol,
        request.provider.as_deref(),
        &request.model,
    );
    let endpoint_id = EndpointId(format!("host-inline-{route_digest}"));
    let model_id = ygg_ai::ModelId(format!("host-inline/{route_digest}"));
    let auth = request
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .map(|key| {
            if protocol == Protocol::AnthropicMessages {
                Auth::header(http::HeaderName::from_static("x-api-key"), key)
            } else {
                Auth::bearer(key)
            }
        })
        .unwrap_or(Auth::None);
    let default_headers = build_inline_headers(&request.custom_headers)?;
    catalog.register_endpoint(Endpoint {
        id: endpoint_id.clone(),
        base_url,
        auth,
        default_headers,
        transport: ygg_ai::EndpointTransport::Http,
        runtime: ygg_ai::RequestRuntime::default(),
        timeout: std::time::Duration::from_secs(30),
    })?;
    let context_window = request.context_window_tokens.unwrap_or(262_144).max(1);
    let max_output_tokens = request
        .max_output_tokens
        .unwrap_or(16_384)
        .max(1)
        .min(context_window);
    let mut input_modalities = ModalitySet::none();
    if request.vision {
        input_modalities = input_modalities.with(Modality::Image);
    }
    for modality in &request.input_modalities {
        input_modalities = input_modalities.with(match modality {
            HostInputModality::Image => Modality::Image,
            HostInputModality::Audio => Modality::Audio,
        });
    }
    catalog.register_model(ModelSpec {
        id: model_id.clone(),
        endpoint: endpoint_id,
        api_name: request.model.clone(),
        display_name: None,
        protocol,
        capabilities: Capabilities {
            input_modalities,
            output_modalities: ModalitySet::none(),
            tools: true,
            parallel_tool_calls: true,
            reasoning: request.supports_reasoning.then_some(ReasoningCapability {
                control: ReasoningControl::Effort,
                exposes_text: true,
                preserves_state: protocol == Protocol::OpenAiResponses,
                effort_budgets: None,
                openai_chat_mode: OpenAiChatReasoningMode::Standard,
                min_effort: ReasoningEffort::Minimal,
                max_effort: ReasoningEffort::Max,
            }),
            responses_lite: false,
            agent_delegation: None,
            structured_output: true,
            deferred_tool_loading: false,
        },
        limits: ModelLimits {
            context_window,
            max_output_tokens,
        },
        pricing: None,
        cache: ygg_ai::CacheCompatibility::default(),
    })?;
    Ok(model_id)
}

fn parse_inline_base_url(raw: &str) -> anyhow::Result<url::Url> {
    let normalized = if raw.ends_with('/') {
        raw.to_owned()
    } else {
        format!("{raw}/")
    };
    let url = url::Url::parse(&normalized).with_context(|| "inline model base_url is invalid")?;
    if url.cannot_be_a_base()
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "inline model base_url must be an absolute HTTP(S) URL without userinfo, query, or fragment"
        );
    }
    Ok(url)
}

fn build_inline_headers(headers: &HashMap<String, String>) -> anyhow::Result<http::HeaderMap> {
    if headers.len() > MAX_CUSTOM_HEADERS {
        anyhow::bail!("custom_headers exceeds the {MAX_CUSTOM_HEADERS}-header limit");
    }
    let total_bytes = headers.iter().try_fold(0usize, |total, (name, value)| {
        total
            .checked_add(name.len())
            .and_then(|total| total.checked_add(value.len()))
    });
    if total_bytes.is_none_or(|total| total > MAX_CUSTOM_HEADER_BYTES) {
        anyhow::bail!("custom_headers exceeds the {MAX_CUSTOM_HEADER_BYTES}-byte limit");
    }

    let mut result = http::HeaderMap::new();
    for (raw_name, raw_value) in headers {
        let name = http::HeaderName::from_bytes(raw_name.as_bytes())
            .with_context(|| format!("invalid custom header name {raw_name:?}"))?;
        if matches!(
            name.as_str(),
            "connection"
                | "content-length"
                | "host"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "proxy-connection"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) {
            anyhow::bail!("custom header {name} is not allowed");
        }
        let value = http::HeaderValue::from_str(raw_value)
            .with_context(|| format!("invalid value for custom header {name}"))?;
        result.insert(name, value);
    }
    Ok(result)
}

fn inline_protocol(provider_mode: Option<&str>) -> anyhow::Result<Protocol> {
    let mode = provider_mode
        .unwrap_or("openai-compatible")
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    match mode.as_str() {
        "" | "openai" | "openai-chat" | "openai-compatible" | "chat" => Ok(Protocol::OpenAiChat),
        "openai-responses" | "responses" => Ok(Protocol::OpenAiResponses),
        "anthropic" | "anthropic-compatible" | "anthropic-messages" => {
            Ok(Protocol::AnthropicMessages)
        }
        _ => anyhow::bail!(
            "unsupported provider_mode {mode:?}; use openai-compatible, openai-responses, or anthropic-messages"
        ),
    }
}

fn inline_route_digest(
    base_url: &url::Url,
    protocol: Protocol,
    provider: Option<&str>,
    model: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_url.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(format!("{protocol:?}").as_bytes());
    hasher.update([0]);
    hasher.update(provider.unwrap_or("inline").as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_run_request(request: &RunRequest) -> anyhow::Result<()> {
    if request.prompt.len() > MAX_PROMPT_BYTES {
        anyhow::bail!("prompt exceeds the {MAX_PROMPT_BYTES}-byte limit");
    }
    if request.prompt_display_text.as_ref().is_some_and(|text| {
        text.len() > MAX_PROMPT_DISPLAY_BYTES
            || text
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    }) {
        anyhow::bail!(
            "prompt_display_text is malformed or exceeds the {MAX_PROMPT_DISPLAY_BYTES}-byte limit"
        );
    }
    if request.model.trim().is_empty()
        || request.model.trim() != request.model
        || request.model.len() > 512
        || request.model.chars().any(char::is_control)
    {
        anyhow::bail!("model id is empty, malformed, or too long");
    }
    if request
        .provider
        .as_ref()
        .is_some_and(|provider| !valid_protocol_id(provider))
    {
        anyhow::bail!("provider id is malformed or too long");
    }
    if request.api_key.as_ref().is_some_and(|api_key| {
        api_key.len() > MAX_API_KEY_BYTES
            || api_key.trim() != api_key
            || api_key.chars().any(char::is_control)
    }) {
        anyhow::bail!("api_key is malformed or exceeds the {MAX_API_KEY_BYTES}-byte limit");
    }
    if let Some(base_url) = request
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        parse_inline_base_url(base_url)?;
        inline_protocol(request.provider_mode.as_deref())?;
        build_inline_headers(&request.custom_headers)?;
    }
    if request.history.len() > MAX_HISTORY_MESSAGES {
        anyhow::bail!("history exceeds the {MAX_HISTORY_MESSAGES}-message limit");
    }
    let history_bytes = request
        .history
        .iter()
        .try_fold(0usize, |total, message| {
            total.checked_add(message.text.len())
        })
        .ok_or_else(|| anyhow::anyhow!("history size overflow"))?;
    if history_bytes > MAX_HISTORY_BYTES {
        anyhow::bail!("history exceeds the {MAX_HISTORY_BYTES}-byte limit");
    }
    if !request.media.is_empty() && !request.image_paths.is_empty() {
        anyhow::bail!("media and legacy image_paths cannot be combined");
    }
    let (image_count, audio_count) = if request.media.is_empty() {
        (request.image_paths.len(), 0)
    } else {
        request
            .media
            .iter()
            .fold((0usize, 0usize), |(images, audio), input| match input {
                MediaInput::Image { .. } => (images + 1, audio),
                MediaInput::Audio { .. } => (images, audio + 1),
            })
    };
    let media_count = image_count
        .checked_add(audio_count)
        .ok_or_else(|| anyhow::anyhow!("media count overflow"))?;
    if media_count > MAX_MEDIA_COUNT {
        anyhow::bail!("media exceeds the {MAX_MEDIA_COUNT}-item limit");
    }
    if image_count > MAX_IMAGE_COUNT {
        anyhow::bail!("media exceeds the {MAX_IMAGE_COUNT}-image limit");
    }
    if audio_count > MAX_AUDIO_COUNT {
        anyhow::bail!("media exceeds the {MAX_AUDIO_COUNT}-audio limit");
    }
    if request.input_modalities.len() > 2
        || request
            .input_modalities
            .iter()
            .enumerate()
            .any(|(index, modality)| request.input_modalities[..index].contains(modality))
    {
        anyhow::bail!("input_modalities contains duplicate or excess values");
    }
    if request
        .system_prompt
        .as_ref()
        .is_some_and(|system| system.len() > MAX_PROMPT_BYTES)
    {
        anyhow::bail!("system prompt exceeds the {MAX_PROMPT_BYTES}-byte limit");
    }
    Ok(())
}

fn load_user_input(
    request: &RunRequest,
    prompt: String,
    model: &ModelSpec,
) -> anyhow::Result<UserInput> {
    let media = if request.media.is_empty() {
        request
            .image_paths
            .iter()
            .cloned()
            .map(|path| MediaInput::Image { path })
            .collect::<Vec<_>>()
    } else {
        request.media.clone()
    };
    let mut parts = Vec::with_capacity(media.len() + 1);
    parts.push(InputPart::Text(prompt));
    if media.is_empty() {
        return Ok(UserInput::from(parts));
    }

    let workspace = request
        .workspace
        .canonicalize()
        .with_context(|| format!("resolving workspace {}", request.workspace.display()))?;
    let effective_modalities = model.effective_input_modalities();
    let mut total_image_bytes = 0u64;
    let mut total_audio_bytes = 0u64;
    for input in media {
        let (kind, path) = match &input {
            MediaInput::Image { path } => ("image", path),
            MediaInput::Audio { path } => ("audio", path),
        };
        let resolved = path
            .canonicalize()
            .with_context(|| format!("resolving {kind} {}", path.display()))?;
        if !request.allow_external_paths && !resolved.starts_with(&workspace) {
            anyhow::bail!(
                "{kind} {} is outside the configured workspace",
                resolved.display()
            );
        }

        let media = match input {
            MediaInput::Image { .. } => {
                if !effective_modalities.contains(Modality::Image) {
                    anyhow::bail!("model {} does not support native image input", model.id.0);
                }
                let mime = image_mime(&resolved)?;
                let bytes = ygg_agent::secure_fs::read_regular_file_bounded(
                    &resolved,
                    MAX_IMAGE_BYTES as usize,
                )
                .with_context(|| format!("reading image {}", resolved.display()))?;
                total_image_bytes = total_image_bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("image byte size overflow"))?;
                if total_image_bytes > MAX_TOTAL_IMAGE_BYTES {
                    anyhow::bail!("images exceed the {MAX_TOTAL_IMAGE_BYTES}-byte total limit");
                }
                Media::image_bytes(bytes::Bytes::from(bytes), mime)
            }
            MediaInput::Audio { .. } => {
                let format = audio_format(&resolved)?;
                if !model.supports_audio_input(format) {
                    anyhow::bail!(
                        "model {} does not support native {} audio input through {:?}",
                        model.id.0,
                        audio_format_name(format),
                        model.protocol
                    );
                }
                let bytes = ygg_agent::secure_fs::read_regular_file_bounded(
                    &resolved,
                    MAX_AUDIO_BYTES as usize,
                )
                .with_context(|| format!("reading audio {}", resolved.display()))?;
                total_audio_bytes = total_audio_bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("audio byte size overflow"))?;
                if total_audio_bytes > MAX_TOTAL_AUDIO_BYTES {
                    anyhow::bail!("audio exceeds the {MAX_TOTAL_AUDIO_BYTES}-byte total limit");
                }
                Media::audio_bytes(bytes::Bytes::from(bytes), format)
            }
        };
        parts.push(InputPart::Media(media));
    }
    Ok(UserInput::from(parts))
}

fn image_mime(path: &Path) -> anyhow::Result<mime::Mime> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Ok(mime::IMAGE_PNG),
        "jpg" | "jpeg" => Ok(mime::IMAGE_JPEG),
        "gif" => Ok(mime::IMAGE_GIF),
        "webp" => Ok("image/webp".parse().expect("static MIME is valid")),
        _ => anyhow::bail!("unsupported image extension for {}", path.display()),
    }
}

fn audio_format(path: &Path) -> anyhow::Result<AudioFormat> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "wav" => Ok(AudioFormat::Wav),
        "mp3" => Ok(AudioFormat::Mp3),
        "flac" => Ok(AudioFormat::Flac),
        "opus" | "ogg" => Ok(AudioFormat::Opus),
        "aac" | "m4a" => Ok(AudioFormat::Aac),
        "pcm" | "pcm16" => Ok(AudioFormat::Pcm16),
        _ => anyhow::bail!("unsupported audio extension for {}", path.display()),
    }
}

fn audio_format_name(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Wav => "WAV",
        AudioFormat::Aac => "AAC",
        AudioFormat::Mp3 => "MP3",
        AudioFormat::Flac => "FLAC",
        AudioFormat::Opus => "Opus",
        AudioFormat::Pcm16 => "PCM16",
    }
}

fn session_selection(
    session_dir: &Path,
    request: &RunRequest,
) -> anyhow::Result<(SessionSelection, Option<Session>)> {
    ygg_agent::secure_fs::create_private_directory_all(session_dir)
        .with_context(|| format!("creating session directory {}", session_dir.display()))?;
    let canonical_dir = session_dir.canonicalize()?;
    if let Some(resume) = request.resume_session.as_deref() {
        let (path, session) = open_confined_session(resume, &canonical_dir, "resume session")?;
        return Ok((SessionSelection::OpenExisting(path), Some(session)));
    }
    let id = request
        .session_id
        .clone()
        .unwrap_or_else(generated_session_id);
    if !valid_session_id(&id) {
        anyhow::bail!("session id is invalid");
    }
    let path = canonical_dir.join(format!("{id}.jsonl"));
    match path.symlink_metadata() {
        Ok(_) => {
            let (path, session) = open_confined_session(&path, &canonical_dir, "session")?;
            Ok((SessionSelection::OpenExisting(path), Some(session)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((SessionSelection::CreateNew(path), None))
        }
        Err(error) => Err(error).with_context(|| format!("checking session {}", path.display())),
    }
}

fn open_confined_session(
    path: &Path,
    directory: &Path,
    label: &str,
) -> anyhow::Result<(PathBuf, Session)> {
    let canonical = confined_session_file(path, directory, label)?;
    let file = ygg_agent::secure_fs::open_regular_file_for_append(&canonical)
        .with_context(|| format!("opening {label} {}", canonical.display()))?;
    let session = Session::open_with_file(canonical.clone(), file)
        .with_context(|| format!("replaying {label} {}", canonical.display()))?;
    Ok((canonical, session))
}

fn confined_session_file(path: &Path, directory: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let link_metadata = path
        .symlink_metadata()
        .with_context(|| format!("{label} {} is unavailable", path.display()))?;
    if link_metadata.file_type().is_symlink() {
        anyhow::bail!("{label} must not be a symbolic link");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("{label} {} is unavailable", path.display()))?;
    if !canonical.starts_with(directory) {
        anyhow::bail!("{label} must stay inside the configured session directory");
    }
    if !canonical
        .metadata()
        .with_context(|| format!("reading {label} metadata"))?
        .is_file()
    {
        anyhow::bail!("{label} must be a regular file");
    }
    Ok(canonical)
}

fn generated_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("host-{}-{nanos}", std::process::id())
}

fn valid_protocol_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn seed_history(app: &mut crate::app::App, history: &[SeedMessage]) -> anyhow::Result<()> {
    for message in history {
        let message = match message.role {
            SeedRole::User => Message::User(UserMessage {
                content: vec![UserPart::Text(message.text.clone())],
            }),
            SeedRole::Assistant => Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text(message.text.clone())],
                model: app.model.spec.id.clone(),
                protocol: app.model.spec.protocol,
            }),
        };
        app.agent
            .session_mut()
            .append(EntryValue::Message(message))?;
    }
    Ok(())
}

fn assistant_text(message: &AssistantMessage) -> String {
    let mut text = String::new();
    for part in &message.content {
        if let AssistantPart::Text(value) = part {
            append_bounded(&mut text, value, MAX_EVENT_TEXT_BYTES);
        }
    }
    text
}

fn media_payload(index: usize, media: &Media) -> serde_json::Value {
    match media {
        Media::Image(image) => {
            let (source, bytes) = match &image.source {
                ygg_ai::ImageSource::Url(_) => ("url", None),
                ygg_ai::ImageSource::Inline(data) => ("inline", Some(data.len())),
                ygg_ai::ImageSource::ProviderRef(_) => ("provider_ref", None),
            };
            serde_json::json!({
                "index": index,
                "kind": "image",
                "media_type": image.media_type.as_ref().map(ToString::to_string),
                "source": source,
                "bytes": bytes,
                "payload_omitted": true,
            })
        }
        Media::Audio(audio) => {
            let (source, bytes) = match &audio.payload {
                ygg_ai::AudioPayload::Inline(data) => ("inline", Some(data.len())),
                ygg_ai::AudioPayload::ProviderRef(_) => ("provider_ref", None),
                ygg_ai::AudioPayload::InlineWithProviderRef { data, .. } => {
                    ("inline_with_provider_ref", Some(data.len()))
                }
            };
            serde_json::json!({
                "index": index,
                "kind": "audio",
                "format": format!("{:?}", audio.format).to_ascii_lowercase(),
                "source": source,
                "bytes": bytes,
                "transcript": audio.transcript.as_deref().map(|text| clip_text(text, 64 * 1024)),
                "payload_omitted": true,
            })
        }
    }
}

fn compaction_reason_label(reason: ygg_agent::CompactionReason) -> &'static str {
    match reason {
        ygg_agent::CompactionReason::Threshold => "threshold",
        ygg_agent::CompactionReason::Overflow => "overflow",
    }
}

fn compaction_kind_payload(kind: &ygg_agent::CompactionKind) -> serde_json::Value {
    match kind {
        ygg_agent::CompactionKind::Local => serde_json::json!({"type": "local"}),
        ygg_agent::CompactionKind::NativeResponses {
            checkpoint,
            covered_through,
        } => serde_json::json!({
            "type": "native_responses",
            "checkpoint_entry_id": checkpoint.0,
            "covered_through_entry_id": covered_through.0,
        }),
    }
}

fn progress_payload(progress: ToolProgress) -> serde_json::Value {
    match progress {
        ToolProgress::Output { stream, bytes } => serde_json::json!({
            "type": "output",
            "stream": match stream {
                ygg_agent::OutputStream::Stdout => "stdout",
                ygg_agent::OutputStream::Stderr => "stderr",
            },
            "text": clip_text(&String::from_utf8_lossy(&bytes), 64 * 1024),
        }),
        ToolProgress::Status(message) => serde_json::json!({
            "type": "status",
            "message": clip_text(&message, 16 * 1024),
        }),
        ToolProgress::Decoration(decoration) => serde_json::json!({
            "type": "decoration",
            "label": clip_text(decoration.label(), 256),
            "detail": decoration.detail().map(|detail| clip_text(detail, 4 * 1024)),
        }),
        ToolProgress::Confirmation(request) => {
            let payload = serde_json::json!({
                "type": "confirmation_required",
                "prompt": clip_text(&request.prompt, 16 * 1024),
                "detail": request.detail.as_deref().map(|detail| clip_text(detail, 16 * 1024)),
                "destructive": request.destructive,
                "default": false,
                "denied": true,
            });
            request.respond(false);
            payload
        }
        ToolProgress::Input(request) => {
            let payload = serde_json::json!({
                "type": "input_required",
                "prompt": clip_text(&request.prompt, 16 * 1024),
                "secret": request.secret,
                "cancelled": true,
            });
            request.cancel();
            payload
        }
        ToolProgress::Dropped { bytes, events } => serde_json::json!({
            "type": "dropped",
            "bytes": bytes,
            "events": events,
        }),
        ToolProgress::SessionEvent(_, _) => serde_json::json!({
            "type": "session_event",
        }),
    }
}

fn clip_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = text.len().saturating_sub(end);
    format!("{}\n[… {omitted} bytes omitted]", &text[..end])
}

fn append_bounded(target: &mut String, text: &str, max_bytes: usize) {
    if target.len() >= max_bytes {
        return;
    }
    let remaining = max_bytes - target.len();
    if text.len() <= remaining {
        target.push_str(text);
        return;
    }
    let mut end = remaining;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request(workspace: PathBuf) -> RunRequest {
        RunRequest {
            run_id: "run".into(),
            session_id: None,
            workspace,
            working_dir: None,
            session_dir: None,
            resume_session: None,
            model: "model".into(),
            provider: None,
            base_url: None,
            api_key: None,
            custom_headers: HashMap::new(),
            provider_mode: None,
            context_window_tokens: None,
            max_output_tokens: None,
            vision: false,
            input_modalities: Vec::new(),
            supports_reasoning: false,
            prompt: "prompt".into(),
            prompt_display_text: None,
            system_prompt: None,
            reasoning: None,
            tools: None,
            allow_file_mutation: true,
            allow_external_paths: false,
            context_files: true,
            offline: true,
            max_turns: None,
            max_cost_microdollars: None,
            history: Vec::new(),
            media: Vec::new(),
            image_paths: Vec::new(),
            prompt_paths: Vec::new(),
            skill_paths: Vec::new(),
            extension_paths: Vec::new(),
            enabled_extensions: Vec::new(),
            trusted_extensions: Vec::new(),
        }
    }

    fn model_with_inputs(protocol: Protocol, input_modalities: ModalitySet) -> ModelSpec {
        ModelSpec {
            id: ygg_ai::ModelId("test-model".into()),
            endpoint: EndpointId("test-provider".into()),
            api_name: "test-model".into(),
            display_name: None,
            protocol,
            capabilities: Capabilities {
                input_modalities,
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: false,
                deferred_tool_loading: false,
            },
            limits: ModelLimits {
                context_window: 32_768,
                max_output_tokens: 4_096,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        }
    }

    #[test]
    fn controlled_host_config_keeps_model_tools_workspace_relative() {
        let workspace = tempfile::tempdir().unwrap();
        let mut request = base_request(workspace.path().to_path_buf());
        request.allow_external_paths = true;

        let config = host_config(&request).unwrap();

        assert_eq!(config.effect_policy, ygg_agent::EffectPolicy::Controlled);
        assert!(!config.sandbox.allow_external_paths);
        assert_eq!(
            config.sandbox.policy_provenance,
            ToolPolicyProvenance::all(PolicyValueSource::HostRequest)
        );
    }

    #[test]
    fn ids_are_bounded_and_cannot_be_paths() {
        assert!(valid_protocol_id("request-1:turn_2"));
        assert!(valid_session_id("session-1.turn_2"));
        assert!(!valid_session_id("session:stream"));
        assert!(!valid_protocol_id("../session"));
        assert!(!valid_protocol_id("has/slash"));
        assert!(!valid_protocol_id(""));
        assert!(!valid_protocol_id(&"x".repeat(MAX_ID_BYTES + 1)));
    }

    #[test]
    fn clipping_preserves_utf8_boundaries() {
        let clipped = clip_text("abc🙂def", 5);
        assert!(clipped.starts_with("abc"));
        assert!(!clipped.contains('�'));
        assert!(clipped.contains("bytes omitted"));
    }

    #[tokio::test]
    async fn frame_reader_discards_oversized_input_without_desynchronizing() {
        let data = format!("{}\n{{\"ok\":true}}\n", "x".repeat(MAX_FRAME_BYTES + 1));
        let mut reader = BufReader::new(data.as_bytes());
        assert!(matches!(
            read_frame(&mut reader).await.unwrap(),
            Some(Frame::Oversized)
        ));
        let Some(Frame::Data(next)) = read_frame(&mut reader).await.unwrap() else {
            panic!("expected the next bounded frame");
        };
        assert_eq!(next, br#"{"ok":true}"#);
    }

    #[test]
    fn run_request_rejects_unbounded_history() {
        let mut request = base_request(PathBuf::from("."));
        request.history = (0..=MAX_HISTORY_MESSAGES)
            .map(|_| SeedMessage {
                role: SeedRole::User,
                text: "x".into(),
            })
            .collect();
        assert!(validate_run_request(&request).is_err());
    }

    #[test]
    fn run_request_rejects_malformed_or_unbounded_display_text() {
        let mut request = base_request(PathBuf::from("."));
        request.prompt_display_text = Some("x".repeat(MAX_PROMPT_DISPLAY_BYTES + 1));
        assert!(validate_run_request(&request).is_err());

        request.prompt_display_text = Some("caller\u{1b}[31m".into());
        assert!(validate_run_request(&request).is_err());

        request.prompt_display_text = Some(String::new());
        assert!(validate_run_request(&request).is_ok());
    }

    #[test]
    fn request_objects_reject_unknown_fields() {
        let mut request = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": "request",
            "command": "run",
            "run_id": "run",
            "workspace": ".",
            "model": "test-model",
            "prompt": "hello",
            "allow_file_mutations": true,
        });
        let error = parse_request(request.to_string().as_bytes()).unwrap_err();
        assert!(error.contains("allow_file_mutations"));

        request
            .as_object_mut()
            .unwrap()
            .remove("allow_file_mutations");
        request["history"] = serde_json::json!([{
            "role": "user",
            "text": "hello",
            "unexpected": true,
        }]);
        let error = parse_request(request.to_string().as_bytes()).unwrap_err();
        assert!(error.contains("unexpected"));
    }

    #[test]
    fn outbound_serialization_never_crosses_its_bound() {
        let oversized = serde_json::json!({"text": "x".repeat(MAX_FRAME_BYTES)});
        assert!(serialize_bounded(&oversized, MAX_FRAME_BYTES - 1)
            .unwrap()
            .is_none());

        let bounded = serialize_bounded(&serde_json::json!({"ok": true}), 128)
            .unwrap()
            .unwrap();
        assert!(bounded.len() <= 128);
    }

    #[test]
    fn inline_routes_reject_unsafe_urls_modes_and_headers() {
        for url in [
            "file:///tmp/provider",
            "https://user@example.com/v1",
            "https://example.com/v1?token=secret",
            "https://example.com/v1#fragment",
        ] {
            assert!(parse_inline_base_url(url).is_err(), "accepted {url}");
        }
        assert_eq!(
            inline_protocol(Some("openai_responses")).unwrap(),
            Protocol::OpenAiResponses
        );
        assert!(inline_protocol(Some("unknown")).is_err());

        let mut headers = HashMap::from([("Connection".to_owned(), "close".to_owned())]);
        assert!(build_inline_headers(&headers).is_err());
        headers = HashMap::from([("x-test".to_owned(), "x".repeat(1024))]);
        assert!(build_inline_headers(&headers).is_ok());
        headers.insert("x-extra".to_owned(), "x".repeat(MAX_CUSTOM_HEADER_BYTES));
        assert!(build_inline_headers(&headers).is_err());
    }

    #[test]
    fn session_paths_are_confined_and_regular() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let outside = root.path().join("outside.jsonl");
        std::fs::write(&outside, b"").unwrap();
        let mut request = base_request(root.path().to_path_buf());
        request.resume_session = Some(outside);
        let error = session_selection(&sessions, &request).unwrap_err();
        assert!(error
            .to_string()
            .contains("must stay inside the configured session directory"));

        let directory = sessions.join("not-a-file.jsonl");
        std::fs::create_dir(&directory).unwrap();
        request.resume_session = Some(directory);
        let error = session_selection(&sessions, &request).unwrap_err();
        assert!(error.to_string().contains("must be a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn session_paths_reject_final_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let target = sessions.join("target.jsonl");
        let link = sessions.join("session.jsonl");
        std::fs::write(&target, b"").unwrap();
        symlink(&target, &link).unwrap();

        let mut request = base_request(root.path().to_path_buf());
        request.resume_session = Some(link);
        let error = session_selection(&sessions, &request).unwrap_err();
        assert!(error.to_string().contains("must not be a symbolic link"));
    }

    #[test]
    fn image_input_is_typed_and_confined_to_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_image = workspace.path().join("resume.jpg");
        let outside_image = outside.path().join("resume.jpg");
        std::fs::write(&inside_image, b"image").unwrap();
        std::fs::write(&outside_image, b"image").unwrap();

        let mut request = base_request(workspace.path().to_path_buf());
        request.image_paths.push(inside_image);
        let model = model_with_inputs(
            Protocol::OpenAiChat,
            ModalitySet::none().with(Modality::Image),
        );
        let input = load_user_input(&request, "inspect".into(), &model).unwrap();
        assert!(matches!(
            input.parts.as_slice(),
            [InputPart::Text(_), InputPart::Media(Media::Image(_))]
        ));

        request.image_paths = vec![outside_image];
        let error = load_user_input(&request, "inspect".into(), &model).unwrap_err();
        assert!(error
            .to_string()
            .contains("outside the configured workspace"));
    }

    #[test]
    fn ordered_image_and_audio_inputs_remain_typed() {
        let workspace = tempfile::tempdir().unwrap();
        let first_image = workspace.path().join("first.png");
        let audio = workspace.path().join("music.wav");
        let second_image = workspace.path().join("second.jpg");
        std::fs::write(&first_image, b"image-one").unwrap();
        std::fs::write(&audio, b"audio").unwrap();
        std::fs::write(&second_image, b"image-two").unwrap();

        let mut request = base_request(workspace.path().to_path_buf());
        request.media = vec![
            MediaInput::Image { path: first_image },
            MediaInput::Audio { path: audio },
            MediaInput::Image { path: second_image },
        ];
        let model = model_with_inputs(
            Protocol::OpenAiChat,
            ModalitySet::none()
                .with(Modality::Image)
                .with(Modality::Audio),
        );
        let input = load_user_input(&request, "compare".into(), &model).unwrap();

        assert!(matches!(
            input.parts.as_slice(),
            [
                InputPart::Text(_),
                InputPart::Media(Media::Image(_)),
                InputPart::Media(Media::Audio(audio)),
                InputPart::Media(Media::Image(_)),
            ] if audio.format == AudioFormat::Wav
        ));
    }

    #[test]
    fn audio_requires_a_route_effectively_supporting_its_format() {
        let workspace = tempfile::tempdir().unwrap();
        let audio = workspace.path().join("music.wav");
        std::fs::write(&audio, b"audio").unwrap();
        let mut request = base_request(workspace.path().to_path_buf());
        request.media = vec![MediaInput::Audio { path: audio }];

        let image_only = model_with_inputs(
            Protocol::OpenAiChat,
            ModalitySet::none().with(Modality::Image),
        );
        let error = load_user_input(&request, "listen".into(), &image_only).unwrap_err();
        assert!(error
            .to_string()
            .contains("does not support native WAV audio input"));

        let advertised_on_unsupported_protocol = model_with_inputs(
            Protocol::OpenAiResponses,
            ModalitySet::none().with(Modality::Audio),
        );
        let error = load_user_input(
            &request,
            "listen".into(),
            &advertised_on_unsupported_protocol,
        )
        .unwrap_err();
        assert!(error.to_string().contains("through OpenAiResponses"));
    }
}
