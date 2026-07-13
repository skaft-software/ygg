#![deny(missing_docs)]

//! `ygg-ai` — provider-independent inference for Ygg's agent loop.
//!
//! This crate turns a provider-independent [`Request`] plus a selected [`Model`]
//! into either a streamed sequence of [`StreamEvent`]s or a single assembled
//! [`Response`], across three wire protocols (OpenAI Responses, OpenAI Chat
//! Completions, Anthropic Messages).
//!
//! The public surface is the canonical [`types`], the [`stream`] events,
//! [`auth`], [`error`], [`pricing`], and the model [`catalog`]. Everything under
//! `protocol` is private: canonical types never mirror provider JSON.
//!
//! See `docs/design/ygg-ai.md` for the normative design.
//!
//! # Example
//!
//! ```no_run
//! use ygg_ai::{AiClient, ModelCatalog, ModelId};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let catalog = ModelCatalog::builtin()?;
//! let model = catalog.resolve(&ModelId("gpt-4o-mini".into()))?;
//! let client = AiClient::new();
//! // Build a provider-independent `Request`, then call `client.stream(...)` or
//! // `client.complete(...)` from your async runtime.
//! let _ = (client, model);
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};

pub mod auth;
pub mod catalog;
pub mod client;
pub mod error;
pub mod pricing;
pub mod stream;
pub mod types;
mod validate;

pub(crate) mod protocol;

pub use auth::{
    Auth, CredentialResolver, CredentialResolverRegistry, CredentialScheme, ResolvedCredential,
    Secret,
};
pub use catalog::{AuthConfig, CatalogConfig, EndpointConfig, Model, ModelCatalog, ModelConfig};
pub use client::AiClient;
pub use error::{
    AiError, AuthError, ConfigError, DecodeError, Diagnostic, HttpError, PricingError,
    ProviderError, StreamProtocolError, TransportError, TransportPhase, UnsupportedError,
    ValidationError,
};
pub use mime::Mime;
pub use pricing::{Cost, Pricing, PricingTier, TokenRate};
pub use stream::{ResponseStream, StreamEvent};
pub use types::{
    AssistantMessage, AssistantPart, AudioFormat, AudioMedia, AudioOutputOptions, AudioPayload,
    AudioVoice, Capabilities, Endpoint, EndpointId, ImageDetail, ImageMedia, ImageSource,
    JsonSchemaFormat, Media, Message, Modality, ModalitySet, ModelId, ModelLimits, ModelSpec,
    OpenAiChatReasoningMode, OutputFormat, OutputModalities, Protocol, ProviderMediaRef,
    ReasoningCapability, ReasoningConfig, ReasoningControl, ReasoningEffort,
    ReasoningEffortBudgets, ReasoningPart, ReasoningState, ReasoningStateKind, Request, Response,
    StopReason, ToolCall, ToolCallId, ToolChoice, ToolDef, ToolResult, ToolResultPart, Usage,
    UserMessage, UserPart,
};

/// Strictness for cross-protocol / capability degradation.
///
/// `Strict` (the default) rejects any unsupported capability with a structured
/// error before any bytes leave. `Lossy` performs only the specified
/// derived-wire conversions and reports every loss via [`Diagnostic`]s; it never
/// silently mutates history or inserts placeholder content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilityMode {
    /// Reject unsupported capabilities with a structured error.
    #[default]
    Strict,
    /// Drop unsupported data with a reported diagnostic; never silently.
    Lossy,
}
