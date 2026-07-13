#![deny(missing_docs)]

//! `ygg-agent` — stateful agent loop with tool execution and event streaming.
//!
//! Sits above [`ygg_ai`]: the AI crate turns a `Request` into a stream of
//! `StreamEvent`s; this crate orchestrates that stream. It reconstructs
//! provider requests from a persistent JSONL [`Session`], executes tool calls
//! through a small extension boundary, persists every semantic boundary
//! (complete messages and individual tool results — never streaming deltas),
//! and emits [`AgentEvent`]s to the caller. Only `ygg-ai`'s public canonical
//! types are used; provider wire formats never leak into this crate.
//!
//! See `docs/design/ygg-agent.md` for the normative design.
//!
//! # Example
//!
//! ```no_run
//! use ygg_agent::{Agent, AgentConfig, CoreTools, ExtensionHost, SandboxConfig, Session};
//! use ygg_ai::{AiClient, ModelCatalog, ModelId, ReasoningConfig};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let catalog = ModelCatalog::builtin()?;
//! let mut extensions = ExtensionHost::new();
//! extensions.load(&CoreTools);
//!
//! let mut agent = Agent::new(AgentConfig {
//!     client: AiClient::new(),
//!     model: catalog.resolve(&ModelId("gpt-4o-mini".into()))?,
//!     session: Session::create("session.jsonl")?,
//!     system: "You are a coding agent.".into(),
//!     sandbox: SandboxConfig::new("."),
//!     extensions,
//!     max_turns: 40,
//!     reasoning: ReasoningConfig::Off,
//! })?;
//!
//! // Streaming: drive events and control concurrently.
//! let mut run = agent.prompt("Find where auth logic lives").await?;
//! let control = run.control();
//! while let Some(event) = run.next().await {
//!     // Render AgentEvent; use `control` (clonable) to steer/follow_up/abort.
//!     let _ = (&event, &control);
//! }
//! drop(run);
//!
//! // Or run to completion:
//! let output = agent.complete("Fix the failing tests").await?;
//! println!("{}", output.text);
//! # Ok(())
//! # }
//! ```
//!
//! # Crash semantics
//!
//! Tool execution is **at least once** after an unclean crash: each tool
//! result is written to the session immediately after execution, but a crash
//! in the window between a tool's external side effect and the write of its
//! result entry means the tool may run again when the conversation is
//! retried. This crate never claims exactly-once execution. Session appends
//! are process-crash safe but not fsynced (not power-loss durable) — see
//! [`session`] for the precise persistence and recovery rules.

pub mod agent;
pub mod events;
pub mod extension;
pub mod sandbox;
pub mod session;
pub mod tool;
pub mod tools;

pub use agent::{Agent, AgentConfig, AgentError, Run, RunControl, RunOutput};
pub use events::{AgentEvent, Control, FinishReason, OutputChannel};
pub use extension::{EventObserver, Extension, ExtensionHost};
pub use sandbox::SandboxConfig;
pub use session::{Entry, EntryId, EntryValue, Session, SessionError, SessionRecord};
pub use tool::{
    content_hash, OutputStream, Tool, ToolContext, ToolError, ToolOutput, ToolProgress,
    ToolProgressSink, MAX_PROGRESS_CHUNK_BYTES,
};
pub use tools::{CoreTools, EditTool, ExecTool, ReadTool, SearchTool};
