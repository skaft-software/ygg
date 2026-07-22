//! The semantic tool boundary: [`Tool`], [`ToolContext`], [`ToolOutput`],
//! [`ToolError`], live [`ToolProgress`] streaming, and the content hash
//! used for optimistic edit checks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use ygg_ai::ToolDef;

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

/// A tool the model can call.
///
/// Core tools (`read`, `search`, `edit`, `write`, `exec`) and third-party tools
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

    /// Declares whether crash recovery may repeat an unresolved call.
    /// Mutating and extension tools remain unsafe unless they explicitly prove
    /// idempotent behavior.
    fn replay_safety(&self) -> ReplaySafety {
        ReplaySafety::Unsafe
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

/// One deterministic semantic validation failure for a typed tool input.
///
/// Ygg owns this small boundary instead of requiring a proc-macro validation
/// framework. That keeps downstream SDK users in control of validation logic
/// and avoids coupling the workspace MSRV to a transitive derive dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInputValidationIssue {
    /// JSONPath-like location of the invalid value.
    pub path: String,
    /// Human-readable constraint the model should satisfy when retrying.
    pub expected: String,
    /// Bounded description of the received value.
    pub received: String,
}

/// Semantic validation implemented by every strongly typed tool input.
pub trait ValidateToolInput {
    /// Returns every actionable validation issue, or success when the input is
    /// semantically valid. Structural validation remains Serde's responsibility.
    fn validate_tool_input(&self) -> Result<(), Vec<ToolInputValidationIssue>>;
}

/// A strongly-typed tool definition.
#[async_trait::async_trait]
pub trait TypedTool: Send + Sync + 'static {
    /// The deserializable, schema-generating, and validated Input type.
    type Input: serde::de::DeserializeOwned
        + schemars::JsonSchema
        + ValidateToolInput
        + Send
        + 'static;
    /// The serializable Output type.
    type Output: serde::Serialize + Send + 'static;

    /// Returns the basic tool descriptor.
    fn descriptor(&self) -> ToolDescriptor;

    /// Executes the tool with the strongly-typed input.
    async fn execute(
        &self,
        input: Self::Input,
        context: &ToolContext<'_>,
    ) -> Result<Self::Output, ToolError>;
}

/// An object-safe tool definition for dynamic dispatch in the registry.
#[async_trait::async_trait]
pub trait ErasedTool: Send + Sync {
    /// Returns a reference to the cached ToolDefinition.
    fn definition(&self) -> &ToolDefinition;

    /// Executes the tool with erased JSON values.
    async fn execute_erased(
        &self,
        args: serde_json::Value,
        context: &ToolContext<'_>,
    ) -> Result<serde_json::Value, ToolError>;
}

/// Generic adapter that converts a `TypedTool` into an `ErasedTool`.
pub struct TypedToolAdapter<T> {
    inner: T,
    definition: ToolDefinition,
}

impl<T: TypedTool + 'static> TypedToolAdapter<T> {
    /// Creates a new adapter wrapping the typed tool, caching its definition and schema.
    pub fn new(inner: T) -> Self {
        let definition = ToolDefinition {
            descriptor: inner.descriptor(),
            input_schema: serde_json::to_value(schemars::schema_for!(T::Input))
                .expect("generated tool schemas must serialize"),
        };
        Self { inner, definition }
    }
}

#[derive(serde::Serialize)]
struct ValidationErrorPayload {
    error: &'static str,
    tool: String,
    issues: Vec<ValidationIssue>,
    retryable: bool,
}

#[derive(serde::Serialize)]
struct ValidationIssue {
    path: String,
    expected: String,
    received: String,
}

#[async_trait::async_trait]
impl<T: TypedTool + 'static> ErasedTool for TypedToolAdapter<T> {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute_erased(
        &self,
        args: serde_json::Value,
        context: &ToolContext<'_>,
    ) -> Result<serde_json::Value, ToolError> {
        let tool_name = &self.definition.descriptor.name;

        // 1. Structural Validation via Serde deserialization
        let input: T::Input = serde_json::from_value(args.clone()).map_err(|err| {
            let payload = ValidationErrorPayload {
                error: "invalid_tool_arguments",
                tool: tool_name.clone(),
                issues: vec![ValidationIssue {
                    path: "$".to_string(),
                    expected: "valid JSON structure".to_string(),
                    received: format!("{err}: {args}"),
                }],
                retryable: true,
            };
            ToolError::new(
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| err.to_string()),
            )
        })?;

        // 2. Semantic validation through Ygg's dependency-free SDK boundary.
        input.validate_tool_input().map_err(|input_issues| {
            let issues = input_issues
                .into_iter()
                .map(|issue| ValidationIssue {
                    path: issue.path,
                    expected: issue.expected,
                    received: issue.received,
                })
                .collect();
            let payload = ValidationErrorPayload {
                error: "invalid_tool_arguments",
                tool: tool_name.clone(),
                issues,
                retryable: true,
            };
            ToolError::new(
                serde_json::to_string_pretty(&payload)
                    .unwrap_or_else(|_| "Validation failed".to_string()),
            )
        })?;

        // 3. Execution
        let output = self.inner.execute(input, context).await?;

        // 4. Serialization
        serde_json::to_value(output)
            .map_err(|err| ToolError::new(format!("Serialization error: {err}")))
    }
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
            .field("detail", &self.detail)
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
/// All methods are infallible and non‑blocking — sends use [`try_send`]
/// against a bounded channel and are silently discarded when the channel
/// is full or the consumer is disconnected. A tool never waits for a
/// progress consumer.
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
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the owning run has been aborted.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
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

/// Successful tool output: compact, line-oriented text for the model.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    /// Compact, line-oriented text optimized for LLM consumption.
    pub text: String,
}

impl ToolOutput {
    /// Creates a tool output from text.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
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
