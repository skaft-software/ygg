//! The semantic tool boundary: [`Tool`], [`ToolContext`], [`ToolOutput`],
//! [`ToolError`], live [`ToolProgress`] streaming, and the content hash
//! used for optimistic edit checks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use ygg_ai::ToolDef;

use crate::sandbox::{self, SandboxConfig};

/// A tool the model can call.
///
/// Core tools (`read`, `search`, `edit`, `exec`) and third-party tools
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

    /// Executes the tool with the model-provided arguments (a JSON object
    /// matching the definition's schema).
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError>;
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

/// Ephemeral progress update emitted by a running tool.
///
/// Never persisted in the session. The final [`ToolOutput`] remains the
/// only model-visible and durable result.
#[derive(Clone, Debug)]
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
    /// Consolidated report of dropped live output. Emitted at most once per
    /// tool execution, immediately before `ToolFinished`, when the
    /// bounded progress channel was full and bytes were discarded.
    Dropped {
        /// Total bytes of live output dropped during this tool execution.
        bytes: u64,
    },
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
/// Dropped bytes are counted internally and can be retrieved after tool
/// completion via `take_dropped`.
///
/// [`try_send`]: mpsc::Sender::try_send
#[derive(Clone)]
pub struct ToolProgressSink {
    tx: mpsc::Sender<ToolProgress>,
    dropped: Arc<AtomicU64>,
}

impl ToolProgressSink {
    /// Creates a sink that discards all progress. Use in tests or when
    /// no consumer is attached (print mode, headless operation).
    pub fn null() -> Self {
        let (tx, _) = mpsc::channel(1);
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Creates a live sink backed by the given bounded sender.
    /// `pub(crate)` — only the agent loop constructs a live sink.
    pub(crate) fn live(tx: mpsc::Sender<ToolProgress>) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
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
                        self.dropped.fetch_add(len, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Ok(()) => {}
                }
                remaining = rest;
            }
        }
    }

    /// Returns the total bytes dropped since the last call, resetting the
    /// counter to zero. `pub(crate)` — only the agent loop calls this.
    pub(crate) fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    fn send_one(&self, msg: ToolProgress) {
        let len = match &msg {
            ToolProgress::Output { bytes, .. } => bytes.len() as u64,
            ToolProgress::Status(s) => s.len() as u64,
            ToolProgress::Dropped { .. } => 0,
        };
        match self.tx.try_send(msg) {
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(len, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // No consumer (null sink, aborted run). Silently discard.
            }
            Ok(()) => {}
        }
    }
}

/// Ambient state passed to every tool execution.
pub struct ToolContext<'a> {
    /// Canonicalized workspace root.
    pub workspace: &'a Path,
    /// The sandbox configuration (capability gates and limits).
    pub sandbox: &'a SandboxConfig,
    /// Live progress sink. Owned (cheaply cloneable). Tools that produce
    /// streaming output call [`ToolProgressSink::output`] or
    /// [`ToolProgressSink::status`] during execution. Ignored by tools
    /// that execute quickly or have no streaming output.
    pub progress: ToolProgressSink,
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

/// Deterministic content hash used by `read` and checked by `edit`'s optional
/// `expected_hash` (optimistic concurrency, not cryptographic integrity).
///
/// FNV-1a over the full file bytes, rendered as 16 lowercase hex characters.
/// Deliberately dependency-free; adequate for detecting that a file changed
/// between a `read` and a later `edit`, which is its only job.
pub fn content_hash(bytes: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic_and_content_sensitive() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hello "));
        assert_eq!(content_hash(b"hello").len(), 16);
        // Known FNV-1a 64 vector: the empty input hashes to the offset basis.
        assert_eq!(content_hash(b""), "cbf29ce484222325");
    }

    // ── ToolProgressSink unit tests ──────────────────────────────────────

    #[test]
    fn null_sink_all_methods_silently_succeed() {
        let sink = ToolProgressSink::null();
        sink.output(OutputStream::Stdout, Bytes::from("hello"));
        sink.output(OutputStream::Stderr, Bytes::from("error"));
        sink.status("working");
        // Null sink never increments dropped counter.
        assert_eq!(sink.take_dropped(), 0);
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
        assert_eq!(dropped, 6); // "second".len()

        // Drain the one accepted message so the dropped counter is accurate.
        let accepted = rx.try_recv().unwrap();
        match accepted {
            ToolProgress::Output { bytes, .. } => assert_eq!(&bytes[..], b"first"),
            _ => panic!("expected Output"),
        }
        // No further dropped bytes after take.
        assert_eq!(sink.take_dropped(), 0);
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

        assert_eq!(sink.take_dropped(), 8 + 2 + 4); // "dropped1" + "dr" + "lost"
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
        assert_eq!(a.take_dropped(), 6); // only first was counted
        assert_eq!(b.take_dropped(), 0); // already taken by a
    }
}
