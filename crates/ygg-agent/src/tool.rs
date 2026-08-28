//! The semantic tool boundary: [`Tool`], [`ToolContext`], [`ToolOutput`],
//! [`ToolError`], live [`ToolProgress`] streaming, and the content hash
//! used for optimistic edit checks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::mpsc;
use ygg_ai::{Media, ToolDef};

use crate::effect::ToolEffect;
use crate::sandbox::{self, SandboxConfig};
/// Whether an unresolved call may be executed automatically after reopening a
/// session whose previous process stopped before persisting its result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplaySafety {
    /// Never replay automatically. This is the safe default for mutations,
    /// process execution, extensions, and tools with unknown effects.
    #[default]
    Unsafe,
    /// The tool is read-only or otherwise idempotent and safe to repeat.
    Safe,
}

/// Whether calls to a tool may overlap other calls from the same model turn.
///
/// Parallel execution is deliberately opt-in and stricter than crash replay:
/// an implementation must be read-only, independent of call order, and must
/// not require interactive progress handling while it runs. Mutations,
/// process execution, and extension tools remain sequential by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolConcurrency {
    /// Preserve the model's emitted order and execute one call at a time.
    #[default]
    Sequential,
    /// Calls may execute concurrently with other parallel-safe calls.
    Parallel,
}

/// A tool the model can call.
///
/// Core tools (`read`, `search`, `edit`, `write`, `bash`) and third-party tools
/// implement the same trait and register through the same
/// [`ExtensionHost::tool`](crate::ExtensionHost::tool) method — nothing is
/// hardcoded into the agent loop.
///
/// Success versus failure is carried by the `Result`, never by inspecting
/// output text: an `Err` becomes an error tool result for the model, an `Ok`
/// a normal one. Either way the run continues; tools cannot terminate a run.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Returns the tool definition used for provider function-calling.
    /// The definition's `name` must be unique across all registered tools.
    fn definition(&self) -> ToolDef;

    /// Deterministically classifies the authority required by one parsed call.
    /// The default is deliberately unknown and is denied by every broker
    /// policy. Implementations are trusted host code; model-provided metadata
    /// must never select or lower this classification.
    fn effect(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Unknown)
    }

    /// Declares whether crash recovery may repeat an unresolved call.
    /// Mutating and extension tools remain unsafe unless they explicitly prove
    /// idempotent behavior.
    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Unsafe
    }

    /// Declares whether independent calls emitted in one model turn may
    /// overlap. This is separate from [`ReplaySafety`]: an idempotent mutation
    /// may be safe to retry after a crash while still depending on call order.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Sequential
    }

    /// Executes the tool with the model-provided arguments (a JSON object
    /// matching the definition's schema).
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError>;
}

/// A descriptor containing basic tool metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDescriptor {
    /// The unique name of the tool.
    pub name: String,
    /// The description of what the tool does.
    pub description: String,
}

/// A complete definition of a tool, mapping its descriptor to its schema.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    /// The tool metadata descriptor.
    pub descriptor: ToolDescriptor,
    /// The JSON schema for the tool inputs.
    pub input_schema: serde_json::Value,
}

/// An object-safe tool definition for dynamic dispatch in the registry.
#[async_trait::async_trait]
pub trait ErasedTool: Send + Sync {
    /// Returns a reference to the cached ToolDefinition.
    fn definition(&self) -> &ToolDefinition;

    /// Declares the host-owned authority class for this erased tool.
    fn effect(&self) -> ToolEffect {
        ToolEffect::Unknown
    }

    /// Executes the tool with erased JSON values.
    async fn execute_erased(
        &self,
        args: serde_json::Value,
        context: &ToolContext<'_>,
    ) -> Result<serde_json::Value, ToolError>;
}

/// Generic adapter that wraps an `ErasedTool` and exposes it via the standard `Tool` trait.
pub struct ErasedToolAdapter<E> {
    inner: E,
}

impl<E: ErasedTool> ErasedToolAdapter<E> {
    /// Creates a new adapter wrapping the erased tool.
    pub fn new(inner: E) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl<E: ErasedTool> Tool for ErasedToolAdapter<E> {
    fn definition(&self) -> ToolDef {
        let def = self.inner.definition();
        ToolDef {
            name: def.descriptor.name.clone(),
            description: def.descriptor.description.clone(),
            parameters: def.input_schema.clone(),
        }
    }

    fn effect(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolEffect, ToolError> {
        Ok(self.inner.effect())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let val = self.inner.execute_erased(args, ctx).await?;
        let text = if let serde_json::Value::String(s) = val {
            s
        } else {
            serde_json::to_string_pretty(&val).unwrap_or_default()
        };
        Ok(ToolOutput::new(text))
    }
}

/// Maximum bytes carried in a single [`ToolProgress::Output`] message.
/// Oversized payloads sent through [`ToolProgressSink::output`] are
/// automatically split into chunks at or below this bound, so the bounded
/// channel memory guarantee holds for built-in and extension tools alike.
pub const MAX_PROGRESS_CHUNK_BYTES: usize = 8 * 1024;

/// Capacity of the bounded progress channel, in messages.
/// At `MAX_PROGRESS_CHUNK_BYTES` per message the maximum buffered live
/// progress is ~512 KB.
pub(crate) const PROGRESS_CHANNEL_CAPACITY: usize = 64;

/// Reply channel for session-entry append operations.
type SessionReplyTx = Arc<
    std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<crate::session::EntryId, String>>>>,
>;

/// One ephemeral confirmation requested by a running tool or executable
/// extension. The frontend answers exactly once; dropping the request is an
/// explicit denial.
#[derive(Clone)]
pub struct ToolConfirmation {
    /// Short action-oriented question.
    pub prompt: String,
    /// Optional consequence or scope detail.
    pub detail: Option<String>,
    /// Stronger UI treatment for potentially destructive actions.
    pub destructive: bool,
    /// Suggested choice when a frontend can represent a default.
    pub default: bool,
    reply: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
}

/// One ephemeral text request owned by a running tool. Secret answers are
/// never included in progress events, debug output, or session state.
#[derive(Clone)]
pub struct ToolInputRequest {
    /// Short prompt shown by an interactive frontend.
    pub prompt: String,
    /// Whether the frontend must suppress echo and ordinary editor handling.
    pub secret: bool,
    reply: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Option<ToolInputResponse>>>>>,
}

impl ToolInputRequest {
    /// Deliver one answer. Repeated answers are ignored.
    pub fn respond(&self, bytes: Vec<u8>) {
        if let Ok(mut reply) = self.reply.lock() {
            if let Some(reply) = reply.take() {
                let _ = reply.send(Some(ToolInputResponse(bytes)));
            }
        }
    }

    /// Cancel the request without sending input to the child.
    pub fn cancel(&self) {
        if let Ok(mut reply) = self.reply.lock() {
            if let Some(reply) = reply.take() {
                let _ = reply.send(None);
            }
        }
    }
}

impl std::fmt::Debug for ToolInputRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolInputRequest")
            .field("prompt", &self.prompt)
            .field("secret", &self.secret)
            .finish_non_exhaustive()
    }
}

/// A tool input answer whose backing allocation is erased on drop.
pub struct ToolInputResponse(Vec<u8>);

impl ToolInputResponse {
    /// Borrow the answer bytes without creating an additional secret copy.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for ToolInputResponse {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl ToolConfirmation {
    /// Answer the request. Repeated answers are ignored.
    pub fn respond(&self, confirmed: bool) {
        if let Ok(mut reply) = self.reply.lock() {
            if let Some(reply) = reply.take() {
                let _ = reply.send(confirmed);
            }
        }
    }
}

impl std::fmt::Debug for ToolConfirmation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolConfirmation")
            .field("prompt", &self.prompt)
            .field("detail", &self.detail.as_ref().map(|_| "[REDACTED]"))
            .field("destructive", &self.destructive)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

/// Ephemeral progress update emitted by a running tool.
///
/// Never persisted in the session. The final [`ToolOutput`] remains the
/// only model-visible and durable result.
#[derive(Clone)]
pub enum ToolProgress {
    /// A chunk of live stdout or stderr bytes. Not guaranteed to be valid
    /// UTF‑8 — consumers decode with [`String::from_utf8_lossy`].
    Output {
        /// Which output stream produced these bytes.
        stream: OutputStream,
        /// The bytes. Cloning is cheap (reference-counted).
        bytes: Bytes,
    },
    /// A human-readable status message (e.g. `"Running tests… 3/15"`).
    Status(String),
    /// A typed yes/no request. Frontends that do not handle it deny by
    /// dropping the event; tools never receive implicit approval.
    Confirmation(ToolConfirmation),
    /// A typed input request. Interactive frontends temporarily own input;
    /// headless consumers cancel by dropping the request.
    Input(ToolInputRequest),
    /// Consolidated report of progress discarded because the bounded channel
    /// was full. Emitted at most once per tool execution, immediately before
    /// `ToolFinished`.
    Dropped {
        /// Total bytes of live output dropped during this tool execution.
        bytes: u64,
        /// Number of semantic session-entry events that were dropped.
        events: u64,
    },
    /// Internal channel event to append a session entry from a tool.
    #[doc(hidden)]
    SessionEvent(Box<crate::session::EntryValue>, SessionReplyTx),
}

impl std::fmt::Debug for ToolProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output { stream, bytes } => f
                .debug_struct("Output")
                .field("stream", stream)
                .field("bytes", bytes)
                .finish(),
            Self::Status(s) => f.debug_tuple("Status").field(s).finish(),
            Self::Confirmation(request) => f.debug_tuple("Confirmation").field(request).finish(),
            Self::Input(request) => f.debug_tuple("Input").field(request).finish(),
            Self::Dropped { bytes, events } => f
                .debug_struct("Dropped")
                .field("bytes", bytes)
                .field("events", events)
                .finish(),
            Self::SessionEvent(ev, _) => f.debug_tuple("SessionEvent").field(ev).finish(),
        }
    }
}

/// Identifies an output stream in [`ToolProgress::Output`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Sink through which a tool emits live progress during execution.
///
/// Cheaply cloneable (wraps an `mpsc::Sender` and an `Arc<AtomicU64>`).
/// Output and status sends are infallible and non-blocking: they use
/// [`try_send`] against a bounded channel and are discarded when full or
/// disconnected. Typed confirmation/input methods wait only for the explicit
/// reply; a missing or disconnected consumer resolves them as denied/cancelled.
///
/// Dropped output bytes and semantic session-entry events are counted
/// internally and can be retrieved after tool completion via `take_dropped`.
///
/// [`try_send`]: mpsc::Sender::try_send
#[derive(Clone)]
pub struct ToolProgressSink {
    tx: mpsc::Sender<ToolProgress>,
    dropped_bytes: Arc<AtomicU64>,
    dropped_events: Arc<AtomicU64>,
}

impl ToolProgressSink {
    /// Creates a sink that discards all progress. Use in tests or when
    /// no consumer is attached (print mode, headless operation).
    pub fn null() -> Self {
        let (tx, _) = mpsc::channel(1);
        Self {
            tx,
            dropped_bytes: Arc::new(AtomicU64::new(0)),
            dropped_events: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Creates a live sink backed by the given bounded sender.
    /// `pub(crate)` — only the agent loop constructs a live sink.
    pub(crate) fn live(tx: mpsc::Sender<ToolProgress>) -> Self {
        Self {
            tx,
            dropped_bytes: Arc::new(AtomicU64::new(0)),
            dropped_events: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Emit a stdout or stderr chunk. Non‑blocking; drops silently when
    /// the channel is full. Oversized payloads are split into bounded
    /// chunks at [`MAX_PROGRESS_CHUNK_BYTES`].
    pub fn output(&self, stream: OutputStream, bytes: impl Into<Bytes>) {
        let bytes: Bytes = bytes.into();
        if bytes.len() <= MAX_PROGRESS_CHUNK_BYTES {
            self.send_one(ToolProgress::Output { stream, bytes });
        } else {
            for chunk in bytes.chunks(MAX_PROGRESS_CHUNK_BYTES) {
                self.send_one(ToolProgress::Output {
                    stream,
                    bytes: Bytes::copy_from_slice(chunk),
                });
            }
        }
    }

    /// Emit a human-readable status message. Non‑blocking.
    /// Oversized messages are split into bounded chunks at
    /// [`MAX_PROGRESS_CHUNK_BYTES`], respecting UTF‑8 character
    /// boundaries so every chunk is valid Unicode.
    pub fn status(&self, message: impl Into<String>) {
        let msg: String = message.into();
        if msg.len() <= MAX_PROGRESS_CHUNK_BYTES {
            self.send_one(ToolProgress::Status(msg));
        } else {
            let mut remaining: &str = &msg;
            while !remaining.is_empty() {
                // Walk forwards from the byte-boundary candidate to the
                // nearest char boundary so splits never break a multibyte
                // sequence.
                let mut end = remaining.len().min(MAX_PROGRESS_CHUNK_BYTES);
                while end < remaining.len() && !remaining.is_char_boundary(end) {
                    end += 1;
                }
                let (chunk, rest) = remaining.split_at(end);
                let s = chunk.to_string();
                let len = s.len() as u64;
                match self.tx.try_send(ToolProgress::Status(s)) {
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.dropped_bytes.fetch_add(len, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Ok(()) => {}
                }
                remaining = rest;
            }
        }
    }

    /// Request explicit user confirmation and wait for the frontend answer.
    /// A missing, lagged, or non-interactive consumer deterministically denies.
    pub async fn confirmation(
        &self,
        prompt: String,
        detail: Option<String>,
        destructive: bool,
        default: bool,
    ) -> bool {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send_one(ToolProgress::Confirmation(ToolConfirmation {
            prompt,
            detail,
            destructive,
            default,
            reply: Arc::new(std::sync::Mutex::new(Some(reply))),
        }));
        answer.await.unwrap_or(false)
    }

    /// Request ephemeral input from an interactive frontend. Secret responses
    /// are carried only through the reply channel and wiped after use.
    pub async fn input(&self, prompt: String, secret: bool) -> Option<ToolInputResponse> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send_one(ToolProgress::Input(ToolInputRequest {
            prompt,
            secret,
            reply: Arc::new(std::sync::Mutex::new(Some(reply))),
        }));
        answer.await.ok().flatten()
    }

    /// Returns dropped output bytes and session-entry events since the last
    /// call, resetting both counters. `pub(crate)` — only the agent loop calls
    /// this.
    pub(crate) fn take_dropped(&self) -> (u64, u64) {
        (
            self.dropped_bytes.swap(0, Ordering::Relaxed),
            self.dropped_events.swap(0, Ordering::Relaxed),
        )
    }

    pub(crate) fn send_one(&self, msg: ToolProgress) {
        let (bytes, events) = match &msg {
            ToolProgress::Output { bytes, .. } => (bytes.len() as u64, 0),
            ToolProgress::Status(s) => (s.len() as u64, 0),
            ToolProgress::Confirmation(_) => (0, 1),
            ToolProgress::Input(_) => (0, 1),
            ToolProgress::Dropped { .. } => (0, 0),
            ToolProgress::SessionEvent { .. } => (0, 1),
        };
        match self.tx.try_send(msg) {
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_bytes.fetch_add(bytes, Ordering::Relaxed);
                self.dropped_events.fetch_add(events, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // No consumer (null sink, aborted run). Silently discard.
            }
            Ok(()) => {}
        }
    }
}

/// Cooperative cancellation state shared with bounded blocking tool work.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<CancellationState>);

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancellationToken {
    /// Signal cooperative cancellation to all holders of this token.
    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            self.0.notify.notify_waiters();
        }
    }

    /// Whether the owning run has been aborted.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Wait until the owning run is aborted.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.0.notify.notified();
        tokio::pin!(notified);
        // Register before the second atomic check so cancellation cannot fall
        // between the check and waiter registration.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Ambient state passed to every tool execution.
pub struct ToolContext<'a> {
    /// Canonicalized workspace root.
    pub workspace: &'a Path,
    /// The sandbox configuration (capability gates and limits).
    pub sandbox: &'a SandboxConfig,
    /// Unique owner for process-local resources created by this Agent. Core
    /// tools use this to isolate persistent PTYs even when multiple agents
    /// share the same workspace.
    pub execution_scope: &'a str,
    /// Durable session-derived owner for extension resources. Unlike
    /// `execution_scope`, this survives Agent rebuilds and process reloads.
    pub resource_owner: &'a str,
    /// Active skills resolved from the session immediately before this tool
    /// call. Tools may use it to authorize skill-scoped operations.
    pub active_skills: &'a [crate::session::SkillActivatedSnapshot],
    /// Exact tool names registered for this Agent after product allowlists,
    /// capability gates, and extension discovery have all been applied.
    /// Tools that activate higher-level capabilities use this rather than a
    /// static product list so their requirements match executable reality.
    pub registered_tools: &'a [String],
    /// Live progress sink. Owned (cheaply cloneable). Tools that produce
    /// streaming output call [`ToolProgressSink::output`] or
    /// [`ToolProgressSink::status`] during execution. Ignored by tools
    /// that execute quickly or have no streaming output.
    pub progress: ToolProgressSink,
    /// Cooperative cancellation observed by bounded blocking filesystem work.
    pub cancellation: CancellationToken,
}

impl ToolContext<'_> {
    /// Resolves an existing local path. Relative paths use the workspace as
    /// their base; hosts that enable trusted-local access may also use absolute
    /// paths, `~/…`, parent components, and external symlinks.
    pub fn resolve_existing(&self, path: &str) -> Result<PathBuf, ToolError> {
        sandbox::resolve_existing(self.workspace, path, self.sandbox.allow_external_paths)
            .map_err(ToolError::new)
    }

    /// Resolves a local path for creation. Relative paths use the workspace as
    /// their base; external path access follows the host's sandbox policy.
    pub fn resolve_create(&self, path: &str) -> Result<PathBuf, ToolError> {
        sandbox::resolve_create(self.workspace, path, self.sandbox.allow_external_paths)
            .map_err(ToolError::new)
    }

    /// Returns a stable display spelling without changing the path used for
    /// execution. Workspace paths become relative; external paths retain their
    /// original spelling.
    pub fn display_path(&self, path: &str) -> String {
        sandbox::display_path(self.workspace, path, self.sandbox.allow_external_paths)
    }

    /// Appends a custom entry value to the active session.
    /// Returns the resulting EntryId.
    pub async fn append_session_entry(
        &self,
        value: crate::session::EntryValue,
    ) -> Result<crate::session::EntryId, ToolError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let event = ToolProgress::SessionEvent(
            Box::new(value),
            Arc::new(std::sync::Mutex::new(Some(reply_tx))),
        );
        self.progress.send_one(event);
        reply_rx
            .await
            .map_err(|_| ToolError::new("Session channel closed without response"))?
            .map_err(ToolError::new)
    }
}

/// Media kind attached to a successful tool output.
///
/// This small metadata enum is safe to pass to presentation layers without
/// copying or exposing the underlying binary payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutputMediaKind {
    /// An image that the model can inspect with vision.
    Image,
    /// Audio that the model can hear or transcribe.
    Audio,
}

impl ToolOutputMediaKind {
    fn from_media(media: &Media) -> Self {
        match media {
            Media::Image(_) => Self::Image,
            Media::Audio(_) => Self::Audio,
        }
    }
}

/// One ordered model-visible part of a successful tool output.
///
/// Text remains the compact fallback for every provider. Image and audio
/// parts reuse Ygg's canonical media types so built-in and executable tools
/// cross the same persistence and provider-lowering boundary.
#[derive(Clone, Debug)]
pub enum ToolOutputContentPart {
    /// Plain model-visible text.
    Text(String),
    /// An image or audio payload already vetted by the host.
    Media(Media),
}

/// Maximum serialized bytes retained as structured tool output.
pub const MAX_TOOL_STRUCTURED_CONTENT_BYTES: usize = 256 * 1024;
/// Maximum serialized bytes retained as non-model-visible tool metadata.
pub const MAX_TOOL_METADATA_BYTES: usize = 64 * 1024;
const MAX_TOOL_DETAIL_DEPTH: usize = 32;
const MAX_TOOL_DETAIL_NODES: usize = 16 * 1024;
const MAX_TOOL_METADATA_KEY_BYTES: usize = 256;

/// Validated, durable data retained beside a canonical tool result.
///
/// Neither field is implicitly sent to a model. Product surfaces may inspect
/// these values after reopening a session without reparsing compact text.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolOutputDetails {
    /// Optional machine-readable result produced by the tool.
    #[serde(
        default,
        deserialize_with = "deserialize_present_json",
        skip_serializing_if = "Option::is_none"
    )]
    structured_content: Option<serde_json::Value>,
    /// Optional inert host-vetted presentation/provenance metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

impl ToolOutputDetails {
    /// Validates and constructs durable tool-output details.
    pub fn try_new(
        structured_content: Option<serde_json::Value>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Self, ToolOutputValidationError> {
        let metadata = normalize_optional_json(metadata);
        if let Some(value) = structured_content.as_ref() {
            validate_tool_detail(
                "structured_content",
                value,
                MAX_TOOL_STRUCTURED_CONTENT_BYTES,
                false,
            )?;
        }
        if let Some(value) = metadata.as_ref() {
            validate_tool_detail("metadata", value, MAX_TOOL_METADATA_BYTES, true)?;
        }
        Ok(Self {
            structured_content,
            metadata,
        })
    }

    /// Returns the retained machine-readable result.
    pub fn structured_content(&self) -> Option<&serde_json::Value> {
        self.structured_content.as_ref()
    }

    /// Returns the retained non-model-visible metadata object.
    pub fn metadata(&self) -> Option<&serde_json::Value> {
        self.metadata.as_ref()
    }

    /// Returns whether neither optional detail is present.
    pub fn is_empty(&self) -> bool {
        self.structured_content.is_none() && self.metadata.is_none()
    }

    pub(crate) fn into_validated(self) -> Result<Self, ToolOutputValidationError> {
        Self::try_new(self.structured_content, self.metadata)
    }
}

fn deserialize_present_json<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <serde_json::Value as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// Rejection raised while admitting structured content or metadata.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolOutputValidationError {
    /// The serialized JSON value crossed its field-specific hard limit.
    #[error("{field} exceeds {limit} serialized bytes (got {actual})")]
    TooLarge {
        /// Field being validated.
        field: &'static str,
        /// Actual serialized byte count.
        actual: usize,
        /// Maximum serialized byte count.
        limit: usize,
    },
    /// The JSON tree is too deeply nested for safe downstream inspection.
    #[error("{field} exceeds the maximum JSON depth of {limit}")]
    TooDeep {
        /// Field being validated.
        field: &'static str,
        /// Maximum accepted nesting depth.
        limit: usize,
    },
    /// The JSON tree has too many aggregate values.
    #[error("{field} exceeds the maximum JSON node count of {limit}")]
    TooManyNodes {
        /// Field being validated.
        field: &'static str,
        /// Maximum accepted aggregate value count.
        limit: usize,
    },
    /// Metadata must be an object so consumers never have to guess its shape.
    #[error("metadata must be a JSON object")]
    MetadataNotObject,
    /// One metadata key is unsuitable for durable host-side inspection.
    #[error("metadata contains an invalid key")]
    InvalidMetadataKey,
}

fn normalize_optional_json(value: Option<serde_json::Value>) -> Option<serde_json::Value> {
    value.filter(|value| !value.is_null())
}

fn validate_tool_detail(
    field: &'static str,
    value: &serde_json::Value,
    byte_limit: usize,
    require_object: bool,
) -> Result<(), ToolOutputValidationError> {
    if require_object && !value.is_object() {
        return Err(ToolOutputValidationError::MetadataNotObject);
    }
    let mut nodes = 0usize;
    let mut pending = vec![(value, 1usize)];
    while let Some((current, depth)) = pending.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > MAX_TOOL_DETAIL_NODES {
            return Err(ToolOutputValidationError::TooManyNodes {
                field,
                limit: MAX_TOOL_DETAIL_NODES,
            });
        }
        if depth > MAX_TOOL_DETAIL_DEPTH {
            return Err(ToolOutputValidationError::TooDeep {
                field,
                limit: MAX_TOOL_DETAIL_DEPTH,
            });
        }
        match current {
            serde_json::Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    if require_object
                        && (key.is_empty()
                            || key.len() > MAX_TOOL_METADATA_KEY_BYTES
                            || key.chars().any(char::is_control))
                    {
                        return Err(ToolOutputValidationError::InvalidMetadataKey);
                    }
                    pending.push((value, depth + 1));
                }
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value).expect("serde_json::Value must serialize");
    let actual = counter.bytes;
    if actual > byte_limit {
        return Err(ToolOutputValidationError::TooLarge {
            field,
            actual,
            limit: byte_limit,
        });
    }
    Ok(())
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: usize,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type ToolOutputResolution = Box<dyn FnOnce() + Send + 'static>;

struct ToolOutputCommitState {
    hooks: Option<(ToolOutputResolution, ToolOutputResolution)>,
}

struct ToolOutputCommitInner {
    state: Mutex<ToolOutputCommitState>,
}

impl Drop for ToolOutputCommitInner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, rollback)) = state.hooks.take() {
            rollback();
        }
    }
}

#[derive(Clone)]
struct ToolOutputCommit {
    inner: Arc<ToolOutputCommitInner>,
}

impl std::fmt::Debug for ToolOutputCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .hooks
            .is_some();
        formatter
            .debug_struct("ToolOutputCommit")
            .field("pending", &pending)
            .finish()
    }
}

impl ToolOutputCommit {
    fn new(
        commit: impl FnOnce() + Send + 'static,
        rollback: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(ToolOutputCommitInner {
                state: Mutex::new(ToolOutputCommitState {
                    hooks: Some((Box::new(commit), Box::new(rollback))),
                }),
            }),
        }
    }

    fn resolve(&self, delivered: bool) {
        let action = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .hooks
                .take()
                .map(|(commit, rollback)| if delivered { commit } else { rollback })
        };
        if let Some(action) = action {
            action();
        }
    }
}

/// Canonical tool output: compact text plus optional structured media and a
/// semantic error marker. Transport-level failures still use [`ToolError`]; a
/// completed tool may return a rich error envelope without losing its media or
/// durable details.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    /// Compact, line-oriented text optimized for LLM consumption.
    pub text: String,
    media: Vec<Media>,
    media_kinds: Vec<ToolOutputMediaKind>,
    content_parts: Vec<ToolOutputContentPart>,
    details: ToolOutputDetails,
    is_error: bool,
    delivery_commit: Option<ToolOutputCommit>,
}

impl ToolOutput {
    /// Creates a tool output from text.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            content_parts: vec![ToolOutputContentPart::Text(text.clone())],
            text,
            media: Vec::new(),
            media_kinds: Vec::new(),
            details: ToolOutputDetails::default(),
            is_error: false,
            delivery_commit: None,
        }
    }

    /// Creates an output from ordered text and media parts.
    ///
    /// Multiple text parts are joined with newlines for the backward-
    /// compatible compact [`ToolOutput::text`] representation while their
    /// original boundaries remain available through [`ToolOutput::content_parts`].
    pub fn from_content_parts(
        content_parts: impl IntoIterator<Item = ToolOutputContentPart>,
    ) -> Self {
        let content_parts = content_parts.into_iter().collect::<Vec<_>>();
        let text = content_parts
            .iter()
            .filter_map(|part| match part {
                ToolOutputContentPart::Text(text) => Some(text.as_str()),
                ToolOutputContentPart::Media(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let media = content_parts
            .iter()
            .filter_map(|part| match part {
                ToolOutputContentPart::Text(_) => None,
                ToolOutputContentPart::Media(media) => Some(media.clone()),
            })
            .collect::<Vec<_>>();
        let media_kinds = media.iter().map(ToolOutputMediaKind::from_media).collect();
        Self {
            text,
            media,
            media_kinds,
            content_parts,
            details: ToolOutputDetails::default(),
            is_error: false,
            delivery_commit: None,
        }
    }

    /// Marks whether this completed output represents a semantic tool error.
    /// Rich content and durable details remain available when this is true.
    pub fn with_is_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Returns whether the completed output represents a semantic tool error.
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// Attaches one structured image or audio payload to this output.
    ///
    /// Agent hosts persist supported media as canonical tool-result parts (or
    /// as an adjacent user-media part when the target protocol cannot carry
    /// media inside a tool result).
    pub fn with_media(mut self, media: Media) -> Self {
        self.media_kinds
            .push(ToolOutputMediaKind::from_media(&media));
        self.media.push(media.clone());
        self.content_parts.push(ToolOutputContentPart::Media(media));
        self
    }

    /// Validates and attaches optional structured content and metadata.
    pub fn try_with_details(
        mut self,
        structured_content: Option<serde_json::Value>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Self, ToolOutputValidationError> {
        self.details = ToolOutputDetails::try_new(structured_content, metadata)?;
        Ok(self)
    }

    /// Validates and attaches machine-readable structured content.
    pub fn try_with_structured_content(
        self,
        structured_content: serde_json::Value,
    ) -> Result<Self, ToolOutputValidationError> {
        let metadata = self.details.metadata.clone();
        self.try_with_details(Some(structured_content), metadata)
    }

    /// Validates and attaches an inert non-model-visible metadata object.
    pub fn try_with_metadata(
        self,
        metadata: serde_json::Value,
    ) -> Result<Self, ToolOutputValidationError> {
        let structured_content = self.details.structured_content.clone();
        self.try_with_details(structured_content, Some(metadata))
    }

    /// Returns the ordered text and media parts.
    pub fn content_parts(&self) -> &[ToolOutputContentPart] {
        &self.content_parts
    }

    /// Returns the retained machine-readable structured result.
    pub fn structured_content(&self) -> Option<&serde_json::Value> {
        self.details.structured_content()
    }

    /// Returns the retained non-model-visible metadata object.
    pub fn metadata(&self) -> Option<&serde_json::Value> {
        self.details.metadata()
    }

    /// Returns validated details suitable for durable session metadata.
    pub fn details(&self) -> Option<&ToolOutputDetails> {
        (!self.details.is_empty()).then_some(&self.details)
    }

    /// Returns the structured media payloads for canonical persistence.
    pub fn media(&self) -> &[Media] {
        &self.media
    }

    /// Returns presentation-safe media metadata without exposing payloads.
    pub fn media_kinds(&self) -> &[ToolOutputMediaKind] {
        &self.media_kinds
    }

    /// Appends a bounded host diagnostic to the model-visible text while
    /// keeping structured parts and their compact text representation aligned.
    /// This is used for generic execution diagnostics, never for user content.
    pub(crate) fn with_model_annotation(mut self, annotation: &str) -> Self {
        self.text.push_str(annotation);
        if let Some(text) = self
            .content_parts
            .iter_mut()
            .rev()
            .find_map(|part| match part {
                ToolOutputContentPart::Text(text) => Some(text),
                ToolOutputContentPart::Media(_) => None,
            })
        {
            text.push_str(annotation);
        } else {
            self.content_parts
                .push(ToolOutputContentPart::Text(annotation.to_owned()));
        }
        self
    }

    /// Installs internal resolution hooks for work that is acknowledged only
    /// after this output is durably appended to the session. Dropping every
    /// copy without resolution rolls the provisional delivery back.
    pub(crate) fn with_delivery_commit(
        mut self,
        commit: impl FnOnce() + Send + 'static,
        rollback: impl FnOnce() + Send + 'static,
    ) -> Self {
        self.delivery_commit = Some(ToolOutputCommit::new(commit, rollback));
        self
    }

    /// Resolve provisional work after the agent's durable tool-result boundary.
    /// `delivered` must be false when generic output limiting changed the text.
    pub(crate) fn resolve_delivery(&self, delivered: bool) {
        if let Some(commit) = &self.delivery_commit {
            commit.resolve(delivered);
        }
    }

    /// Returns a copy suitable for observers and presentation layers.
    ///
    /// Text and media-kind metadata remain available, while binary payloads
    /// stay inside the agent's persistence boundary.
    pub fn without_media_payloads(&self) -> Self {
        self.without_media_payloads_for(self.media_kinds.iter().copied())
    }

    /// Returns a presentation copy containing only successfully ingested
    /// media kinds.
    ///
    /// Hosts use this after protocol/capability lowering so an unsupported
    /// payload cannot produce a misleading vision or audio indicator.
    pub fn without_media_payloads_for(
        &self,
        media_kinds: impl IntoIterator<Item = ToolOutputMediaKind>,
    ) -> Self {
        Self {
            text: self.text.clone(),
            media: Vec::new(),
            media_kinds: media_kinds.into_iter().collect(),
            content_parts: self
                .content_parts
                .iter()
                .filter_map(|part| match part {
                    ToolOutputContentPart::Text(text) => {
                        Some(ToolOutputContentPart::Text(text.clone()))
                    }
                    ToolOutputContentPart::Media(_) => None,
                })
                .collect(),
            details: self.details.clone(),
            is_error: self.is_error,
            delivery_commit: None,
        }
    }
}

/// A failed tool execution. Returned to the model as an error tool result;
/// it does not terminate the run.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ToolError {
    /// Compact description of the failure, written for the model.
    pub message: String,
}

impl ToolError {
    /// Creates a tool error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// SHA-256 content digest used by `read` and checked by mutation tools'
/// optional `expected_hash` optimistic-concurrency guard.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic_and_content_sensitive() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hello "));
        assert_eq!(content_hash(b"hello").len(), 64);
        assert_eq!(
            content_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn tool_output_presentation_copy_keeps_kind_but_drops_payload() {
        let media = Media::image_bytes(
            Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            "image/png".parse().unwrap(),
        );
        let output = ToolOutput::new("read=vision").with_media(media);
        assert_eq!(output.media().len(), 1);
        assert_eq!(output.media_kinds(), &[ToolOutputMediaKind::Image]);

        let presentation = output.without_media_payloads();
        assert_eq!(presentation.text, "read=vision");
        assert!(presentation.media().is_empty());
        assert_eq!(presentation.media_kinds(), &[ToolOutputMediaKind::Image]);
    }

    #[test]
    fn rich_error_marker_survives_presentation_copy() {
        let ordinary = ToolOutput::new("ok");
        assert!(!ordinary.is_error());

        let rich_error = ToolOutput::new("extension rejected the action")
            .try_with_structured_content(serde_json::json!({"code": "rejected"}))
            .unwrap()
            .with_is_error(true);
        assert!(rich_error.is_error());
        let presentation = rich_error.without_media_payloads();
        assert!(presentation.is_error());
        assert_eq!(
            presentation.structured_content(),
            Some(&serde_json::json!({"code": "rejected"}))
        );
    }

    #[test]
    fn provisional_tool_delivery_commits_once_or_rolls_back_on_drop() {
        use std::sync::atomic::{AtomicI8, Ordering};

        let committed = Arc::new(AtomicI8::new(0));
        let on_commit = Arc::clone(&committed);
        let on_rollback = Arc::clone(&committed);
        let output = ToolOutput::new("leased").with_delivery_commit(
            move || on_commit.store(1, Ordering::SeqCst),
            move || on_rollback.store(-1, Ordering::SeqCst),
        );
        let clone = output.clone();
        let presentation = output.without_media_payloads();
        drop((output, presentation));
        assert_eq!(committed.load(Ordering::SeqCst), 0);
        clone.resolve_delivery(true);
        clone.resolve_delivery(false);
        drop(clone);
        assert_eq!(committed.load(Ordering::SeqCst), 1);

        let rolled_back = Arc::new(AtomicI8::new(0));
        let on_commit = Arc::clone(&rolled_back);
        let on_rollback = Arc::clone(&rolled_back);
        drop(ToolOutput::new("leased").with_delivery_commit(
            move || on_commit.store(1, Ordering::SeqCst),
            move || on_rollback.store(-1, Ordering::SeqCst),
        ));
        assert_eq!(rolled_back.load(Ordering::SeqCst), -1);
    }

    #[test]
    fn tool_output_retains_ordered_parts_and_vetted_details() {
        let media = Media::image_bytes(
            Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            "image/png".parse().unwrap(),
        );
        let output = ToolOutput::from_content_parts([
            ToolOutputContentPart::Text("Found one result.".into()),
            ToolOutputContentPart::Media(media),
            ToolOutputContentPart::Text("Source is attached.".into()),
        ])
        .try_with_details(
            Some(serde_json::json!({"sources": [{"title": "Primary"}]})),
            Some(serde_json::json!({"cache": "miss"})),
        )
        .unwrap();

        assert_eq!(output.text, "Found one result.\nSource is attached.");
        assert_eq!(output.content_parts().len(), 3);
        assert!(matches!(
            output.content_parts()[0],
            ToolOutputContentPart::Text(ref text) if text == "Found one result."
        ));
        assert!(matches!(
            output.content_parts()[1],
            ToolOutputContentPart::Media(Media::Image(_))
        ));
        assert_eq!(
            output.structured_content(),
            Some(&serde_json::json!({"sources": [{"title": "Primary"}]}))
        );
        assert_eq!(
            output.metadata(),
            Some(&serde_json::json!({"cache": "miss"}))
        );

        let presentation = output.without_media_payloads();
        assert_eq!(presentation.content_parts().len(), 2);
        assert_eq!(
            presentation.structured_content(),
            output.structured_content()
        );
        assert_eq!(presentation.metadata(), output.metadata());
    }

    #[test]
    fn tool_output_details_distinguish_structured_null_from_missing() {
        let details = ToolOutputDetails::try_new(Some(serde_json::Value::Null), None).unwrap();
        assert_eq!(details.structured_content(), Some(&serde_json::Value::Null));
        assert!(!details.is_empty());

        let serialized = serde_json::to_value(&details).unwrap();
        assert_eq!(serialized, serde_json::json!({"structured_content": null}));
        let reopened: ToolOutputDetails = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            reopened.structured_content(),
            Some(&serde_json::Value::Null)
        );

        let null_metadata =
            ToolOutputDetails::try_new(None, Some(serde_json::Value::Null)).unwrap();
        assert!(null_metadata.is_empty());
    }

    #[test]
    fn tool_output_details_reject_unbounded_or_ambiguous_metadata() {
        assert_eq!(
            ToolOutputDetails::try_new(None, Some(serde_json::json!(["not", "an", "object"])))
                .unwrap_err(),
            ToolOutputValidationError::MetadataNotObject
        );
        assert!(matches!(
            ToolOutputDetails::try_new(
                Some(serde_json::Value::String(
                    "x".repeat(MAX_TOOL_STRUCTURED_CONTENT_BYTES)
                )),
                None
            ),
            Err(ToolOutputValidationError::TooLarge {
                field: "structured_content",
                ..
            })
        ));

        let mut nested = serde_json::json!(true);
        for _ in 0..=MAX_TOOL_DETAIL_DEPTH {
            nested = serde_json::json!([nested]);
        }
        assert!(matches!(
            ToolOutputDetails::try_new(Some(nested), None),
            Err(ToolOutputValidationError::TooDeep {
                field: "structured_content",
                ..
            })
        ));
    }

    // ── ToolProgressSink unit tests ──────────────────────────────────────

    #[test]
    fn null_sink_all_methods_silently_succeed() {
        let sink = ToolProgressSink::null();
        sink.output(OutputStream::Stdout, Bytes::from("hello"));
        sink.output(OutputStream::Stderr, Bytes::from("error"));
        sink.status("working");
        // Null sink never increments dropped counter.
        assert_eq!(sink.take_dropped(), (0, 0));
    }

    #[tokio::test]
    async fn live_sink_delivers_messages_to_receiver() {
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
        let sink = ToolProgressSink::live(tx);

        sink.output(OutputStream::Stdout, Bytes::from("hello"));
        sink.status("started");
        sink.output(OutputStream::Stderr, Bytes::from("oops"));
        drop(sink); // close sender so recv() eventually returns None

        let mut messages = Vec::new();
        while let Some(msg) = rx.recv().await {
            messages.push(msg);
        }
        assert_eq!(messages.len(), 3);
        match &messages[0] {
            ToolProgress::Output { stream, bytes } => {
                assert_eq!(*stream, OutputStream::Stdout);
                assert_eq!(&bytes[..], b"hello");
            }
            _ => panic!("expected Output"),
        }
        match &messages[1] {
            ToolProgress::Status(s) => assert_eq!(s, "started"),
            _ => panic!("expected Status"),
        }
        match &messages[2] {
            ToolProgress::Output { stream, bytes } => {
                assert_eq!(*stream, OutputStream::Stderr);
                assert_eq!(&bytes[..], b"oops");
            }
            _ => panic!("expected Output"),
        }
    }

    #[tokio::test]
    async fn confirmation_detail_is_redacted_from_debug_output() {
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
        let sink = ToolProgressSink::live(tx);
        let waiter = tokio::spawn(async move {
            sink.confirmation(
                "Approve?".into(),
                Some("exact-secret-effect-arguments".into()),
                true,
                false,
            )
            .await
        });
        let request = match rx.recv().await.expect("confirmation request") {
            ToolProgress::Confirmation(request) => request,
            _ => panic!("expected confirmation request"),
        };

        let debug = format!("{request:?}");
        assert!(debug.contains("Approve?"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("exact-secret-effect-arguments"));
        request.respond(false);
        assert!(!waiter.await.unwrap());
    }

    #[tokio::test]
    async fn secret_input_answer_exists_only_on_the_private_reply_channel() {
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
        let sink = ToolProgressSink::live(tx);
        let waiter = tokio::spawn(async move {
            sink.input("Password:".into(), true)
                .await
                .expect("interactive answer")
        });
        let request = match rx.recv().await.expect("input request") {
            ToolProgress::Input(request) => request,
            _ => panic!("expected input request"),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("Password:"));
        assert!(!debug.contains("swordfish"));
        request.respond(b"swordfish".to_vec());
        let response = waiter.await.unwrap();
        assert_eq!(response.as_bytes(), b"swordfish");
        assert!(!format!("{request:?}").contains("swordfish"));
    }

    #[test]
    fn oversized_output_is_split_into_bounded_chunks() {
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
        let sink = ToolProgressSink::live(tx);

        let payload = vec![0x41u8; MAX_PROGRESS_CHUNK_BYTES * 2 + 500];
        sink.output(OutputStream::Stdout, Bytes::from(payload));
        drop(sink);

        // All chunks must be ≤ MAX_PROGRESS_CHUNK_BYTES and independently
        // allocated (not slices into a shared backing buffer).
        let mut total: usize = 0;
        while let Ok(msg) = rx.try_recv() {
            if let ToolProgress::Output { bytes, .. } = msg {
                assert!(
                    bytes.len() <= MAX_PROGRESS_CHUNK_BYTES,
                    "chunk {} > max",
                    bytes.len()
                );
                total += bytes.len();
            }
        }
        assert_eq!(total, MAX_PROGRESS_CHUNK_BYTES * 2 + 500);
    }

    #[test]
    fn oversized_status_is_split_into_bounded_chunks() {
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
        let sink = ToolProgressSink::live(tx);

        let payload = "X".repeat(MAX_PROGRESS_CHUNK_BYTES * 2 + 500);
        sink.status(payload.clone());
        drop(sink);

        let mut total: usize = 0;
        while let Ok(msg) = rx.try_recv() {
            if let ToolProgress::Status(s) = msg {
                assert!(
                    s.len() <= MAX_PROGRESS_CHUNK_BYTES,
                    "status chunk {} > max",
                    s.len()
                );
                total += s.len();
            }
        }
        // Character-boundary splitting preserves every codepoint.
        assert_eq!(total, payload.len());
    }

    #[test]
    fn full_channel_drops_rather_than_blocks() {
        // Channel capacity 1 — second send must be dropped.
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(1);
        let sink = ToolProgressSink::live(tx);

        // Fill the single slot.
        sink.output(OutputStream::Stdout, Bytes::from("first"));
        // This send must be rejected; sink must not block.
        let before = std::time::Instant::now();
        sink.output(OutputStream::Stdout, Bytes::from("second"));
        assert!(before.elapsed() < std::time::Duration::from_millis(50));

        // Dropped bytes counter must reflect the lost payload.
        let dropped = sink.take_dropped();
        assert_eq!(dropped, (6, 0)); // "second".len()

        // Drain the one accepted message so the dropped counter is accurate.
        let accepted = rx.try_recv().unwrap();
        match accepted {
            ToolProgress::Output { bytes, .. } => assert_eq!(&bytes[..], b"first"),
            _ => panic!("expected Output"),
        }
        // No further dropped bytes after take.
        assert_eq!(sink.take_dropped(), (0, 0));
    }

    #[tokio::test]
    async fn full_channel_counts_dropped_session_events() {
        let (tx, _rx) = mpsc::channel::<ToolProgress>(1);
        let sink = ToolProgressSink::live(tx);
        sink.status("fills the channel");

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sink.send_one(ToolProgress::SessionEvent(
            Box::new(crate::session::EntryValue::Config {
                model: None,
                reasoning: None,
                reasoning_mode: None,
            }),
            Arc::new(std::sync::Mutex::new(Some(reply_tx))),
        ));

        assert_eq!(sink.take_dropped(), (0, 1));
        assert!(
            reply_rx.await.is_err(),
            "dropped event must close its reply"
        );
    }

    #[test]
    fn dropped_counter_accumulates_across_multiple_failures() {
        let (tx, _rx) = mpsc::channel::<ToolProgress>(2);
        let sink = ToolProgressSink::live(tx);

        sink.output(OutputStream::Stdout, Bytes::from("a"));
        sink.output(OutputStream::Stdout, Bytes::from("b"));
        // Channel full; next three sends are dropped.
        sink.output(OutputStream::Stdout, Bytes::from("dropped1"));
        sink.output(OutputStream::Stderr, Bytes::from("dr"));
        sink.status("lost");

        assert_eq!(sink.take_dropped(), (8 + 2 + 4, 0)); // "dropped1" + "dr" + "lost"
    }

    #[test]
    fn exporter_sink_delivers_dropped_event() {
        let (tx, mut rx) = mpsc::channel::<ToolProgress>(1);
        let sink = ToolProgressSink::live(tx);

        sink.output(OutputStream::Stdout, Bytes::from("only"));
        sink.output(OutputStream::Stdout, Bytes::from("gone"));
        drop(sink);

        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }
        assert_eq!(messages.len(), 1);
    }

    // ── Verify worst-case memory bound ───────────────────────────────────

    #[test]
    fn worst_case_channel_memory_is_bounded() {
        // 64 slots × 8 KB = 512 KB. Backing allocations for Bytes are
        // reference-counted and released when the channel is drained.
        // The AtomicU64 and Arc overhead is negligible (≤ 128 bytes).
        let max_slot_bytes = MAX_PROGRESS_CHUNK_BYTES as u64;
        let max_total = PROGRESS_CHANNEL_CAPACITY as u64 * max_slot_bytes;
        assert_eq!(max_total, 512 * 1024);
    }

    // ── Clone behaviour ──────────────────────────────────────────────────

    #[test]
    fn cloned_sinks_share_the_dropped_counter() {
        let (tx, _rx) = mpsc::channel::<ToolProgress>(1);
        let a = ToolProgressSink::live(tx);
        let b = a.clone();

        a.output(OutputStream::Stdout, Bytes::from("first"));
        b.output(OutputStream::Stdout, Bytes::from("second"));

        // Both sinks share the same counter.
        assert_eq!(a.take_dropped(), (6, 0)); // only first was counted
        assert_eq!(b.take_dropped(), (0, 0)); // already taken by a
    }
}
