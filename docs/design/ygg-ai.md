# `ygg-ai` — Technical Design

> Provider-independent inference package for Ygg's agent loop.
> Status: **implemented**. Target crate: `crates/ygg-ai`.
>
> **Gap notice (2026-07):** The `Auth::Dynamic` + `CredentialResolver` infrastructure
> (§9) is built and tested, but no concrete OAuth resolver (OpenAI Codex, Anthropic
> Pro/Max, GitHub Copilot) has been implemented yet. See
> `docs/plans/ygg-ai-oauth-codex.md` for the remaining work. Endpoint catalog
> entries for subscription-based models are likewise deferred.

---

## 0. Reading notes, conflicts, and source-of-truth

This design was written against a docs-only repository (no existing `crates/`,
no `Cargo.toml`, no `.git`). There are therefore **no pre-existing Rust
conventions to match**; conventions are established here.

**Source of truth for wire formats** is the repository's own API documentation
under `docs/research/apidocs/`. Pi (`docs/research/refImpls/ai/pi-ai.md`) is
consulted only to determine *which capabilities must be possible*; its classes,
factories, layering, and TypeScript organization are **not** ported.

### Resolved conflicts

1. **Audio scope (RESOLVED by explicit user override).** The base task prompt's
   "Scope constraints" list audio as excluded, and Pi never implemented audio
   (`pi-ai.md` §"Capability Gap Analysis"). The user explicitly overrode this:
   *"audio should be first class and a simple enough unified implementation with
   every other modality."* User instructions take precedence over the base spec,
   so **request-based conversational audio is first-class in v0.1**, carried
   through a **modality-discriminated** `Media` type shared with images (§6.4).
   The unification idea (from LiteLLM, the backend Terminus-2 itself uses —
   `terminus-2.md` §"LLM Backend (`LiteLLM`)") is the *content model*, not the
   field layout: illegal field combinations are made unrepresentable by an
   `enum Media { Image, Audio }` rather than a flat optional-bag. See §12 for
   exact per-protocol audio mappings.

   **Audio support is per protocol and is not inferred beyond the documented
   wire schema:**
   - **OpenAI Chat Completions** — audio **input** via inline WAV/MP3
     (`input_audio`); audio **output** via a **non-streaming** request
     (`modalities:["text","audio"]` → completed `message.audio`). The documented
     Chat *streaming* delta schema does **not** expose audio; we do not invent
     `choices[].delta.audio`.
   - **OpenAI Responses** — **no audio in v0.1.** Current OpenAI docs state the
     Responses API supports text/image input with text output for this use case
     and direct conversational audio to Chat Completions. There are **no**
     Responses audio input mappings and **no** `response.audio.*` handling; audio
     is rejected with a structured `Unsupported` error in Strict mode.
   - **Anthropic Messages** — **no audio in v0.1.** Audio is rejected with a
     structured `Unsupported` error in Strict mode.

   Audio is **never silently removed** from a request: Strict rejects with a
   structured error; Lossy drops with a reported `Diagnostic`.

2. **What audio scope does *not* include (FINAL, not open questions).** v0.1 is
   *request-based conversational audio only*. Deferred, by decision (§24):
   realtime speech-to-speech (WebRTC/WebSocket/SIP sessions), standalone
   transcription (STT, `POST /v1/audio/transcriptions`), standalone text-to-speech
   (TTS, `POST /v1/audio/speech`), and voice-activity detection (VAD). Other
   base-spec exclusions (image generation, computer use, code interpreter, MCP
   execution, live catalog refresh, plugin ABI, browser OAuth UI,
   agent loop/session/TUI) are retained (§21).

3. **Provider-hosted state vs. client-owned history.** OpenAI Responses supports
   server-side `conversation`/`store`. Ygg keeps history client-side (§8, §11),
   matching Pi's decision. Server-side conversation persistence is out of scope.

Everything else in the base architecture spec is adopted as written.

### Normative precedence and implementer rules

This file is normative. When prose, a Rust signature, a mapping table, and the
implementation plan disagree, precedence is: **Final decisions (§24) → Rust
signatures/type definitions → exact mapping tables (§12) → validation tables
(§7) → remaining prose → implementation plan**. The implementation plan may
sequence work but may not change behavior.

An implementer MUST NOT guess a wire field, silently omit canonical data, add a
provider capability, or weaken a test to make it pass. If repository API docs do
not establish a mapping, return `Unsupported` in Strict mode and omit it with one
specific `Diagnostic` in Lossy mode. Record any genuine blocker in the final
report; do not substitute a speculative implementation. No `todo!()`,
`unimplemented!()`, placeholder DTO, ignored failing test, or empty catalog entry
is acceptable in a completed package.

---

## 1. Purpose and non-goals

### Purpose

`ygg-ai` is the single inference dependency of Ygg's agent loop. It turns a
**provider-independent request** plus a **selected model** into either a
streamed sequence of events or a single assembled response, across three wire
protocols:

- **OpenAI Responses** (`POST /responses`, SSE)
- **OpenAI Chat Completions** (`POST /v1/chat/completions`, SSE)
- **Anthropic Messages** (`POST /v1/messages`, SSE)

It owns: one canonical conversation model, one canonical response model,
provider-independent streaming, tools + streamed tool arguments, provider-exposed
reasoning (and opaque continuation state), **unified multimodal input/output
(text, image, audio)**, integer usage + cost accounting, custom endpoints and
headers, extensible auth (including dynamic credentials), safe cross-provider
model switching, and a serializable model catalog.

### Non-goals (v0.1)

- No agent loop, tool execution, session storage, compaction, or TUI (those live
  above/beside `ygg-ai`).
- No realtime/WebSocket session inference; no standalone STT/TTS endpoints.
- No image generation, computer use, code interpreter, hosted web/file search,
  or MCP execution.
- No live model-registry fetching inside the crate.
- No runtime plugin ABI; no browser OAuth UI.
- No concrete `CredentialResolver` implementations for subscription-based providers
  (OpenAI Codex, Anthropic Pro/Max, GitHub Copilot). The `Auth::Dynamic` trait is
  the integration point; implementations are deferred to a downstream crate or a
  future version (see `docs/plans/ygg-ai-oauth-codex.md`).
- No automatic retry after stream output has begun.

---

## 2. Design principles

1. **Data over object hierarchies.** Providers are *configuration*
   (`Endpoint` + `ModelSpec`), not polymorphic implementations. There is no
   `Provider` trait.
2. **One request path.** `complete()` is `stream()` fully consumed. There is no
   second "simple" option system.
3. **Compile-time protocol dispatch.** The client `match`es on `Protocol` and
   calls a codec module. No `dyn Codec`, no trait-object provider registry.
4. **Private wire DTOs.** Canonical types never mirror provider JSON. Every
   protocol module owns private request/response/event DTOs and converts.
5. **Make illegal states unrepresentable.** Role-specific content; failures are
   `AiError`, never assistant messages; opaque reasoning state is never
   flattened into visible text.
6. **Unified modalities.** Text, image, and audio flow through one `Media` value
   tagged by `Modality`; adding a modality is adding an enum arm + per-codec
   mapping, not a new content system.
7. **Strict by default.** Unsupported capabilities are rejected; `Lossy` mode
   drops with *reported* diagnostics and never silently mutates history or
   inserts placeholders.
8. **Switching is serialization.** The canonical conversation is immutable input;
   each request derives temporary wire messages. No destructive
   `transform_messages()`.
9. **Precise money.** `u64` tokens; integer microdollars for cost. No `f64`.
10. **Cancellation is drop.** Dropping the stream cancels body processing. No
    background channel keeps it alive.
11. **Add an abstraction only when ≥2 components need it.**

---

## 3. Architecture diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│ Caller (Ygg agent loop)                                               │
│   let model = catalog.resolve(&model_id)?;                            │
│   let stream = client.stream(&model, request).await?;                 │
│   // or: let response = client.complete(&model, request).await?;      │
└───────────────────────────────┬──────────────────────────────────────┘
                                │  canonical Request + Model
┌───────────────────────────────▼──────────────────────────────────────┐
│ AiClient  (client.rs)          shared reqwest::Client                 │
│   • resolve Auth  -> HeaderMap (auth applied last, uncloneable secret)│
│   • match model.spec.protocol  -> codec (compile-time dispatch)       │
│   • one code path: complete() = stream() consumed to Finished         │
└───────────────────────────────┬──────────────────────────────────────┘
             ┌──────────────────┼───────────────────┐
             ▼                  ▼                   ▼
   protocol/openai_chat  protocol/openai_responses  protocol/anthropic
   ─ build_request(DTO)  ─ build_request(DTO)        ─ build_request(DTO)
   ─ decode_events       ─ decode_events             ─ decode_events
             │                  │                   │
             └──────────────────┼───────────────────┘
                                ▼
        protocol/sse.rs   one push SSE decoder (bytes -> SseEvent)
                                │
                                ▼
        stream.rs   ResponseBuilder + StreamGuard (invariants) ->
                    ResponseStream = Stream<Item=Result<StreamEvent,AiError>>
                                │  Finished(Response)  ── pricing.rs -> Cost
                                ▼
        types.rs  canonical Message / Request / Response / Media / Usage
        auth.rs   Auth + Secret + CredentialResolver
        error.rs  AiError taxonomy
```

Nothing under `protocol/` is public. `AiClient`, `types`, `stream` events,
`auth`, `error`, `pricing`, and the catalog are the public surface.

---

## 4. Package / module boundaries

```
crates/ygg-ai/
├── Cargo.toml
└── src/
    ├── lib.rs          # re-exports; crate docs; CompatibilityMode
    ├── client.rs       # AiClient, Model handle, stream()/complete(), dispatch
    ├── types.rs        # IDs, Endpoint, ModelSpec, Capabilities, Message,
    │                   #   Media, Request, Response, Usage, StopReason, catalog
    ├── stream.rs       # StreamEvent, ResponseStream, ResponseBuilder, StreamGuard
    ├── auth.rs         # Auth, Secret, CredentialResolver, header application
    ├── error.rs        # AiError + variants, Diagnostic
    ├── pricing.rs      # TokenRate, Pricing, Cost, tier selection, cost calc
    ├── catalog.rs      # ModelCatalog, CatalogConfig (serde), resolve()
    └── protocol/
        ├── mod.rs          # Protocol enum, shared codec helpers, HttpRequestParts
        ├── sse.rs          # push SSE decoder (shared by all codecs)
        ├── openai_chat.rs      # private DTOs + build_request + decode
        ├── openai_responses.rs # private DTOs + build_request + decode
        └── anthropic.rs        # private DTOs + build_request + decode
```

Boundary rules:

- `protocol::*` items are `pub(crate)` at most. DTOs are private to each module.
- `types.rs` has **no** dependency on `protocol::*` (canonical is upstream of wire).
- `client.rs` is the only module that touches `reqwest` request execution.
- `pricing.rs` depends only on `types` (Usage, Pricing).

---

## 5. Public API (exact)

```rust
// lib.rs
pub use client::{AiClient, Model};
pub use catalog::{ModelCatalog, CatalogConfig, EndpointConfig, ModelConfig, AuthConfig};
pub use types::{
    EndpointId, ModelId, ToolCallId, Endpoint, ModelSpec, Protocol,
    Capabilities, ModalitySet, Modality, ReasoningCapability, ReasoningControl,
    ModelLimits, Message, UserMessage, AssistantMessage, UserPart, AssistantPart,
    ToolResult, ToolResultPart, ToolCall, ToolDef, ToolChoice,
    Media, ImageMedia, ImageSource, ImageDetail, AudioMedia, AudioPayload,
    ProviderMediaRef, AudioFormat,
    ReasoningPart, ReasoningState, Request, Response, Usage, StopReason,
    ReasoningConfig, ReasoningEffort, ReasoningEffortBudgets,
    CacheRetention, CacheCompatibility, CacheControlFormat,
    OutputFormat, JsonSchemaFormat, OutputModalities, AudioOutputOptions, AudioVoice,
};
// `mime::Mime` (from the `mime` crate) is re-exported for open media types.
pub use mime::Mime;
pub use stream::{StreamEvent, ResponseStream};
pub use auth::{Auth, Secret, CredentialResolver, CredentialResolverRegistry,
    ResolvedCredential, CredentialScheme};
pub use pricing::{Pricing, PricingTier, TokenRate, Cost};
pub use error::{AiError, Diagnostic, HttpError, TransportError, TransportPhase,
    ProviderError, PricingError, ValidationError, UnsupportedError, ConfigError,
    AuthError, DecodeError, StreamProtocolError};

/// Strictness for cross-protocol / capability degradation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilityMode { #[default] Strict, Lossy }

// client.rs
impl AiClient {
    pub fn new() -> Self;                              // default reqwest client
    pub fn with_http_client(http: reqwest::Client) -> Self;

    /// Begin a streamed generation. Errors that occur before the first byte
    /// (config/auth/validation/HTTP handshake) are returned here, not in-stream.
    pub async fn stream(&self, model: &Model, request: Request)
        -> Result<ResponseStream, AiError>;

    /// Run to completion by consuming exactly the same stream and returning the
    /// `Finished` response. No second option system, no second code path.
    pub async fn complete(&self, model: &Model, request: Request)
        -> Result<Response, AiError>;
}

// catalog.rs
impl ModelCatalog {
    /// Parse + validate the embedded `crates/ygg-ai/models/catalog.json` snapshot.
    pub fn builtin() -> Result<Self, ConfigError>;
    /// Loads configs containing None/BearerEnv/HeaderEnv auth. If a Dynamic
    /// entry is present, returns ConfigError::MissingCredentialResolver.
    pub fn from_config(cfg: CatalogConfig) -> Result<Self, ConfigError>;
    /// Loads all auth kinds, resolving Dynamic.resolver_id from `resolvers`.
    pub fn from_config_with_resolvers(
        cfg: CatalogConfig,
        resolvers: &CredentialResolverRegistry,
    ) -> Result<Self, ConfigError>;
    pub fn register_endpoint(&mut self, endpoint: Endpoint) -> Result<(), ConfigError>;
    pub fn register_model(&mut self, spec: ModelSpec) -> Result<(), ConfigError>;
    pub fn resolve(&self, id: &ModelId) -> Result<Model, ConfigError>;
    pub fn models(&self) -> impl Iterator<Item = &ModelSpec>;
}
```

`Model` is the value passed to the client — a resolved `(spec, endpoint)` binding:

```rust
#[derive(Clone)]
pub struct Model {
    pub spec: std::sync::Arc<ModelSpec>,
    pub endpoint: std::sync::Arc<Endpoint>,
}
```

---

## 6. Canonical types

All canonical types are `serde`-serializable **except** anything wrapping a
`Secret` (secrets are neither `Debug` nor `Serialize`; see §9). Serialization
enables client-side history persistence and cross-protocol replay.

### 6.1 IDs, endpoint, model, capabilities

```rust
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct EndpointId(pub String);
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModelId(pub String);
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

pub struct Endpoint {
    pub id: EndpointId,
    pub base_url: url::Url,
    pub auth: Auth,                     // NOT Serialize/Debug-transparent (secrets)
    pub default_headers: http::HeaderMap,
    pub timeout: std::time::Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: ModelId,
    pub endpoint: EndpointId,
    pub api_name: String,              // provider's on-wire model name
    pub protocol: Protocol,
    pub capabilities: Capabilities,
    pub limits: ModelLimits,
    pub pricing: Option<Pricing>,
    pub cache: CacheCompatibility, // provider/model cache compatibility
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol { OpenAiResponses, OpenAiChat, AnthropicMessages }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub input_modalities: ModalitySet,   // Text always implied present
    pub output_modalities: ModalitySet,  // Text always implied present
    pub tools: bool,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<ReasoningCapability>,
    pub structured_output: bool,
}

/// Compact set over a small closed modality universe.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ModalitySet { bits: u8 }
impl ModalitySet {
    pub const fn none() -> Self;
    pub fn with(self, m: Modality) -> Self;
    pub fn contains(self, m: Modality) -> bool;
}

#[non_exhaustive]                        // future: Video, Document
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modality { Image, Audio }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningCapability {
    pub control: ReasoningControl,       // how the request selects effort
    pub exposes_text: bool,              // streams reasoning/summary text
    pub preserves_state: bool,           // signature / encrypted continuation
    /// Required iff `control == TokenBudget`; maps every portable effort to an
    /// explicit provider token budget. Catalog validation enforces this.
    pub effort_budgets: Option<ReasoningEffortBudgets>,
    /// OpenAI Chat-Completions-specific reasoning behavior.
    #[serde(default)]
    pub openai_chat_mode: OpenAiChatReasoningMode,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub enum OpenAiChatReasoningMode {
    #[default] Standard,
    /// DeepSeek's `thinking` toggle and `reasoning_content` replay extension.
    DeepSeekThinking,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ReasoningControl { Effort, TokenBudget }

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ReasoningEffortBudgets {
    pub minimal: u64,
    pub low: u64,
    pub medium: u64,
    pub high: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ModelLimits { pub context_window: u64, pub max_output_tokens: u64 }
```

### 6.2 Messages and content

Role-specific content makes illegal states hard to build. A "tool turn" is a
`User` message carrying `ToolResult` parts (the caller executed the tool).

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message { User(UserMessage), Assistant(AssistantMessage) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserMessage { pub content: Vec<UserPart> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<AssistantPart>,
    pub model: ModelId,       // which model produced it (replay gating)
    pub protocol: Protocol,   // which protocol produced it (state scoping)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UserPart {
    Text(String),
    Media(Media),                 // image or audio input
    ToolResult(ToolResult),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AssistantPart {
    Text(String),
    Reasoning(ReasoningPart),
    ToolCall(ToolCall),
    Media(Media),                 // e.g. generated audio (with transcript)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub content: Vec<ToolResultPart>,
    pub is_error: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolResultPart { Text(String), Media(Media) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    /// Raw JSON text of arguments, validated to parse as a JSON object.
    /// Kept verbatim for faithful replay; parse via `arguments_value()`.
    pub arguments_json: String,
}
impl ToolCall {
    pub fn arguments_value(&self) -> Result<serde_json::Value, DecodeError>;
}
```

### 6.3 Reasoning (visible text + opaque protocol-scoped state)

Only reasoning an API explicitly exposes is represented. Required signatures,
encrypted content, and item identifiers are preserved as opaque,
protocol-scoped metadata — never flattened into text.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningPart {
    /// Human-visible reasoning/summary text, when the API exposes it.
    pub text: Option<String>,
    /// Opaque continuation state, replayable only through a compatible protocol.
    pub state: Option<ReasoningState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningState {
    pub protocol: Protocol,       // producing protocol (replay gate)
    pub model: ModelId,           // producing model (signatures are model-scoped)
    pub kind: ReasoningStateKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReasoningStateKind {
    /// Anthropic `thinking` block signature (text lives in ReasoningPart.text).
    AnthropicSignature { signature: String },
    /// Anthropic `redacted_thinking` opaque blob (no visible text).
    AnthropicRedacted { data: String },
    /// OpenAI Responses reasoning item continuation.
    OpenAiReasoning { item_id: Option<String>, encrypted_content: Option<String> },
}
```

### 6.4 Media (modality-discriminated; illegal field combinations unrepresentable)

Text is its own content variant. Image and audio are one **enum**, not a flat
optional bag. This makes invalid combinations impossible to construct: a
transcript can only exist on audio; a provider reference is a *source variant*,
not a field parallel to inline/URL data; image options (`detail`) cannot be set
on audio and vice-versa; and each modality carries only the source kinds it
actually supports (notably **audio has no `Url` source** — the Chat codec, the
only audio codec, does not accept URL audio).

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Media {
    Image(ImageMedia),
    Audio(AudioMedia),
}

// ---- Image ----
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageMedia {
    pub source: ImageSource,
    /// Open media type. `mime::Mime` (the `mime` crate), e.g. `image/png`.
    /// Serialized as a MIME string through the crate's private serde helper;
    /// `mime::Mime` itself does not implement serde.
    #[serde(default, with = "crate::types::optional_mime")]
    pub media_type: Option<mime::Mime>,
    pub detail: Option<ImageDetail>,     // OpenAI-only hint; ignored elsewhere
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ImageSource {
    Url(url::Url),                        // codecs pass the URL through; never fetched
    Inline(bytes::Bytes),                // raw bytes; codec base64-encodes for the wire
    ProviderRef(ProviderMediaRef),       // replay a provider-hosted image by id
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ImageDetail { Auto, Low, High }

// ---- Audio ----
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioMedia {
    pub payload: AudioPayload,
    pub format: AudioFormat,             // closed, provider-mappable format enum
    pub transcript: Option<String>,      // exists ONLY on audio
}

/// Bytes and a provider reference are NOT mutually exclusive: a completed OpenAI
/// Chat audio response returns `data` + `id` + `expires_at` + `transcript`
/// together — the bytes ARE the media, and the id/expiry let the SAME audio be
/// referenced in later turns. `InlineWithProviderRef` models exactly that.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioPayload {
    /// New inline audio the caller supplies (input).
    Inline(bytes::Bytes),
    /// Reuse a previously provider-hosted audio object by reference (no bytes held).
    ProviderRef(ProviderMediaRef),
    /// Generated audio that returned both bytes AND a reusable reference.
    InlineWithProviderRef { data: bytes::Bytes, reference: ProviderMediaRef },
    // NOTE: no `Url` — no supported protocol accepts URL audio in v0.1.
}

/// Closed set of wire audio formats we can map onto a provider. Image types stay
/// open (`mime::Mime`); audio types are closed because each maps to a specific
/// provider `format`/codec string and unknown formats cannot be serialized safely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat { Wav, Aac, Mp3, Flac, Opus, Pcm16 }

/// A reference to media the provider already holds, replayable only through the
/// producing protocol (id formats are protocol-scoped). `expires_at` lives here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderMediaRef {
    pub protocol: Protocol,
    pub id: String,
    pub expires_at: Option<std::time::SystemTime>,
}
```

Invariants enforced by this shape (no runtime check needed):

- `transcript` exists only on `AudioMedia`.
- Bytes and a provider reference co-exist via `AudioPayload::InlineWithProviderRef`
  — they are not modeled as mutually exclusive alternatives, so a completed Chat
  audio object (`data`+`id`+`expires_at`+`transcript`) is representable faithfully.
- Image and audio cannot accept each other's options (`detail` vs `format`).
- Audio has no URL payload; the Chat codec rejects nothing at runtime because the
  type cannot express URL audio.
- **Voice is not stored here.** Voice is a *generation request option*
  (`AudioOutputOptions`, §6.5), not a property needed to replay the resulting
  audio, so it never appears on `AudioMedia`.
- Codecs **never** auto-download an `ImageSource::Url`; the URL is passed through
  to the provider (which fetches it) or, if a provider needs inline bytes and only
  a URL is available, that is a Strict `Unsupported`/Lossy `Diagnostic` — never a
  silent network fetch.

Constructors keep call sites clean:

```rust
impl Media {
    pub fn image_url(url: url::Url, media_type: Option<mime::Mime>) -> Self;
    pub fn image_bytes(data: bytes::Bytes, media_type: mime::Mime) -> Self;
    pub fn audio_bytes(data: bytes::Bytes, format: AudioFormat) -> Self;         // input
    pub fn audio_ref(reference: ProviderMediaRef, format: AudioFormat) -> Self;  // reuse
}
```

### 6.5 Request

The request is protocol-independent. Reasoning is expressed once and mapped to
either effort or token budget per model capability.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f32>,          // sampling knob; not money
    pub stop: Vec<String>,
    pub reasoning: ReasoningConfig,
    #[serde(default)]
    pub output_format: OutputFormat,
    pub output_modalities: OutputModalities,
    #[serde(default)]
    pub compatibility: CompatibilityMode,  // default Strict
    #[serde(default)]
    pub cache_retention: CacheRetention,   // default Short; None disables controls
    #[serde(default)]
    pub session_id: Option<String>,        // stable provider cache-affinity key
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,     // JSON Schema object
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum ToolChoice { #[default] Auto, Required, None, Named(String) }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum ReasoningConfig {
    #[default] Off,
    Effort(ReasoningEffort),               // mapped to effort or budget
    Budget(u64),                           // explicit token budget
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ReasoningEffort { Minimal, Low, Medium, High }

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum OutputFormat {
    #[default]
    Text,
    JsonObject,
    JsonSchema(JsonSchemaFormat),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonSchemaFormat {
    /// 1–64 ASCII letters, digits, `_`, or `-`.
    pub name: String,
    pub description: Option<String>,
    /// Must be a JSON object.
    pub schema: serde_json::Value,
    pub strict: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum OutputModalities {
    #[default] Text,
    TextAndAudio(AudioOutputOptions),
}

/// Generation-only options for audio output. Neither field is stored on the
/// resulting `AudioMedia` (§6.4): `voice` steers generation, `format` is the
/// closed wire format also used to tag the returned audio.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioOutputOptions {
    pub format: AudioFormat,   // closed enum, defined once in §6.4
    pub voice: AudioVoice,
}

/// OpenAI accepts named voices as strings and custom voices as an object with an
/// id. The named form is kept OPEN so a new provider voice never forces a rebuild.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioVoice {
    Named(String),          // e.g. "alloy" — passed through as a string
    ProviderRef(String),    // custom voice id → provider voice-reference object
}
```

`OutputModalities::TextAndAudio` is only valid for the Chat protocol on an
audio-output-capable model. For Responses and Anthropic it is a Strict
`Unsupported` / Lossy downgrade-to-`Text` + `Diagnostic` (§7). When audio output
is requested the Chat codec issues a **non-streaming** request (§12.1).

### 6.6 Response, usage, stop reason

`Response` is what a *successful* generation yields. History stores only
`response.message` (§8). Failures are `AiError`, never a `Response`.

```rust
#[derive(Clone, Debug)]
pub struct Response {
    pub message: AssistantMessage,   // append THIS to history on success
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub cost: Option<Cost>,          // Some iff model has Pricing
    pub response_id: Option<String>, // provider response/message id
    pub diagnostics: Vec<Diagnostic>,// Lossy-mode losses (empty in Strict)
}

/// Canonical, mutually-exclusive prompt-token buckets. `input_tokens` counts
/// tokens billed at full input rate ONLY (never includes cache); each codec
/// normalizes provider accounting into these buckets (see §15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,           // full-rate input (excludes cache)
    pub cache_read_tokens: u64,      // discounted cache hits
    pub cache_write_tokens: u64,     // ALL cache creation, including 1h below
    pub cache_write_1h_tokens: u64,  // subset of cache_write_tokens at 1h TTL
    pub output_tokens: u64,          // includes reasoning_tokens
    pub reasoning_tokens: u64,       // subset of output that was reasoning
    pub total_tokens: u64,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn, MaxTokens, ToolUse, StopSequence, Refusal, PauseTurn,
    Other(String),
}
```

---

## 7. Type invariants and validation rules

Validation runs in `build_request` before any bytes leave, and the stream layer
enforces output invariants. Violations are `ValidationError` / `UnsupportedError`
(input) or `StreamProtocolError` (output).

**Conversation invariants (validated pre-send):**

1. Every `ToolResult.tool_call_id` in the message list must match a
   `ToolCall.id` in a preceding `AssistantMessage`. Orphan results → error.
2. Every `ToolCall` should have a following `ToolResult` before the next
   `Assistant` turn *if* the target protocol requires paired results
   (Anthropic, Responses `function_call`). Missing pair → error in Strict;
   Lossy inserts a synthetic error result **into the derived wire messages only**
   (never into stored history) and records a `Diagnostic`.
3. `ToolCall.arguments_json` must parse as a JSON **object** (not array/scalar).
4. Media modality/field agreement is guaranteed by the type (§6.4): no runtime
   mime-vs-modality check exists or is needed.
5. `AudioOutputOptions` on `OutputModalities::TextAndAudio` is only valid on the Chat
   protocol when the model advertises `output_modalities.contains(Audio)`.
6. `AudioMedia.format` on Chat input must be a Chat-supported inline format
   (`Wav` or `Mp3`); other formats → Strict `Unsupported` / Lossy drop.
7. Every `ProviderMediaRef` must match the target protocol and must not be
   expired at request-build time. A mismatched or expired ref is Strict
   `Unsupported` / Lossy drop+`Diagnostic`. A codec may only accept a ref in a
   wire position explicitly listed in §12.

**Capability checks (per §7 policy table):**

| Condition | Strict | Lossy |
|-----------|--------|-------|
| `Media::Image` but `!input_modalities.contains(Image)` | `Unsupported` | drop part + `Diagnostic` |
| `Media::Audio` but `!input_modalities.contains(Audio)` (always true for Responses & Anthropic in v0.1) | `Unsupported` | drop part + `Diagnostic` |
| `Media::Audio` on Chat with a non-inline-capable format (not `Wav`/`Mp3`) | `Unsupported` | drop part + `Diagnostic` |
| `OutputModalities::TextAndAudio` but no audio output cap (Responses, Anthropic, non-audio Chat models) | `Unsupported` | downgrade to Text + `Diagnostic` |
| `ImageSource::Url` but target protocol needs inline bytes | `Unsupported` | drop part + `Diagnostic` (never auto-fetch the URL) |
| non-empty `tools` but `!capabilities.tools` | `Unsupported` | drop tools + `Diagnostic` |
| `tool_choice = Named/Required` but not supported | `Unsupported` | `Auto` + `Diagnostic` |
| `ReasoningConfig != Off` but `capabilities.reasoning = None` | `Unsupported` | ignore + `Diagnostic` |
| `ReasoningState` whose `(protocol,model)` ≠ target | `Unsupported` | drop the reasoning part + `Diagnostic` (never convert to text) |
| `output_format != Text` but `!structured_output` | `Unsupported(StructuredOutput)` | downgrade to `Text` + `Diagnostic` |

`JsonSchemaFormat` validation also requires an object-valued schema and a name
matching `[A-Za-z0-9_-]{1,64}`. `max_output_tokens` must be in
`1..=model.limits.max_output_tokens`; when absent, codecs use the model limit
(Anthropic therefore always receives its required `max_tokens`). Temperature,
when present, must be finite and in `0.0..=2.0`.

Lossy **never** invents placeholder text and **never** mutates the caller's
`Request.messages`; drops apply only to the derived wire DTOs, and every drop is
reported via `Response.diagnostics` (and, for pre-send drops, also available on
the returned stream's first `Finished`).

---

## 8. Stream event state machine

```rust
// stream.rs
pub type ResponseStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<StreamEvent, AiError>> + Send>>;

#[derive(Clone, Debug)]
pub enum StreamEvent {
    Started { response_id: Option<String> },

    TextStart { index: usize },
    TextDelta { index: usize, delta: String },
    TextEnd   { index: usize },

    ReasoningStart { index: usize },
    ReasoningDelta { index: usize, delta: String },
    ReasoningEnd   { index: usize },

    ToolCallStart     { index: usize, id: ToolCallId, name: String },
    ToolCallArgsDelta { index: usize, delta: String },   // raw JSON text fragment
    ToolCallEnd       { index: usize },

    // Completed media output. There is NO `MediaDelta` in v0.1: no documented
    // provider streams audio bytes in the Chat delta schema, and Responses does
    // not stream audio here at all. Audio output is produced by a non-streaming
    // provider request (§12.1); the unified stream surfaces the whole result as
    // a single completed event, then `Finished`.
    MediaCompleted { index: usize, media: Media }, // fully assembled (audio + transcript + ref)

    Usage(Usage),
    Finished(Response),      // fully assembled canonical Response
}
```

For audio output the event sequence is:

```text
non-streaming Chat request
        ↓
StreamEvent::Started
        ↓
optional TextStart/TextDelta/TextEnd (if completed message.content is non-empty)
        ↓
StreamEvent::MediaCompleted   // exactly once for message.audio
        ↓
StreamEvent::Usage            // exactly once when provider usage is present
        ↓
StreamEvent::Finished
```

`Started`, `MediaCompleted`, and `Finished` are mandatory on success; text and
`Usage` are conditional as stated. No other media event is emitted.

Text/reasoning/tool events carry **only their delta** (never a cloned partial
response). `index` identifies the content part; the consumer may keep its own
partial if desired.

> **The public event-stream abstraction does not guarantee that every underlying
> provider request is incrementally streamed.** `stream()` is a uniform event
> surface, not a promise of token-by-token transport for every part. Text, tool
> arguments, and reasoning are streamed where the provider streams them; audio
> output is delivered as one completed `MediaCompleted` event because the provider
> request that produces it is non-streaming. A future base64 media-delta protocol
> (deferred, §24) may add incremental media, decoding once at completion — v0.1
> does not.

### Invariants (enforced by `StreamGuard`)

- **Exactly one `Started`**, first, before any content event.
- **Canonical indices are unique across all part kinds.** A codec allocates the
  next index when a part is first observed and keeps a private map from provider
  keys (for example Chat tool index) to canonical index. Provider-native indices
  are never exposed directly. Final `AssistantMessage.content` is ordered by
  canonical index (first-observation order).
- **Content-part lifecycle:** for a *streamed* part (text, reasoning, tool call)
  at each `index`, exactly one `*Start`, zero or more matching `*Delta`, exactly
  one `*End`; deltas only between its start/end; parts may interleave across
  indices but a part's own events are ordered. A **`MediaCompleted`** part is a
  single self-contained event at its own `index` (no start/delta/end).
- **Tool arguments** are accumulated as raw text across `ToolCallArgsDelta` and
  parsed to a JSON object **only** at assembly; a parse failure yields
  `AiError::Decode` and terminates the stream (before `Finished`).
- **`Usage`** may appear at most once (often just before `Finished`); if usage
  arrives only at stream end, it still populates `Finished.usage`.
- **Exactly one `Finished`**, last. `Finished.message` contains only completed
  parts; `Finished.usage`/`cost`/`stop_reason` are final.
- **No events after `Finished`.**
- **Malformed or prematurely terminated** transport → an `Err(AiError)` item and
  the stream ends; no `Finished` is emitted.
- **Drop cancels:** dropping the `ResponseStream` drops the underlying reqwest
  body future, cancelling further network reads (no background task survives).

### Terminal conditions

| Terminal | Emits | Then |
|----------|-------|------|
| Normal end (`response.completed` / `message_stop` / final chunk) | `Usage`, `Finished(Response)` | stream ends |
| Transport closes before terminal event | `Err(StreamProtocol(PrematureEof))` | stream ends |
| Malformed event JSON | `Err(Decode(..))` | stream ends |
| Provider error frame mid-stream (`response.failed`, Responses top-level `error`, Anthropic `error`) | `Err(Provider(..))` | stream ends |
| Bad tool-arg JSON at assembly | `Err(Decode(..))` | stream ends |
| Consumer drops stream | (nothing) | body future dropped |

`complete()` implementation (the single shared path):

```rust
pub async fn complete(&self, model: &Model, request: Request) -> Result<Response, AiError> {
    use futures_util::StreamExt;
    let mut stream = self.stream(model, request).await?;
    let mut finished = None;
    while let Some(ev) = stream.next().await {
        if let StreamEvent::Finished(resp) = ev? { finished = Some(resp); }
    }
    finished.ok_or_else(|| AiError::StreamProtocol(StreamProtocolError::MissingFinish))
}
```

### Internal assembly

`ResponseBuilder` (private) tracks per-index open parts, appends deltas, parses
tool args at part end, and produces the final `AssistantMessage` + `Usage`.
`StreamGuard` wraps each codec's event iterator and validates the state machine,
converting violations to `StreamProtocolError`. Both live in `stream.rs`; codecs
push semantic events and never re-implement invariant checking.

---

## 9. Authentication and secret handling

```rust
// auth.rs
pub enum Auth {
    None,
    Bearer(Secret),                         // fixed credential
    Header { name: http::HeaderName, value: Secret },
    BearerEnv { var: String },              // read per request; enables rotation
    HeaderEnv { name: http::HeaderName, var: String },
    Dynamic(std::sync::Arc<dyn CredentialResolver>),
}

#[async_trait::async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Integration seam for OAuth / refreshable credentials. Called per request;
    /// implementors do their own caching/locking/refresh.
    async fn resolve(&self) -> Result<ResolvedCredential, AuthError>;
}

pub struct ResolvedCredential {
    pub scheme: CredentialScheme,
    pub value: Secret,
    pub extra_headers: http::HeaderMap,     // non-secret headers (e.g. version)
}
pub enum CredentialScheme { Bearer, Header(http::HeaderName) }

pub type CredentialResolverRegistry = std::collections::HashMap<
    String,
    std::sync::Arc<dyn CredentialResolver>,
>;

// Constructors
impl Auth {
    pub fn none() -> Self;
    pub fn bearer(secret: impl Into<Secret>) -> Self;
    pub fn bearer_env(var: impl Into<String>) -> Self; // value read per request
    pub fn header(name: http::HeaderName, secret: impl Into<Secret>) -> Self;
    pub fn header_env(name: http::HeaderName, var: impl Into<String>) -> Self;
    pub fn dynamic(r: std::sync::Arc<dyn CredentialResolver>) -> Self;
}
```

**Secret** wraps the value with hardened traits:

```rust
pub struct Secret(Box<str>);
impl std::fmt::Debug   for Secret { /* writes "Secret(<redacted>)" */ }
impl std::fmt::Display for Secret { /* writes "<redacted>" */ }
// NOT Serialize, NOT Deserialize.
impl Secret {
    pub fn from_env(var: &str) -> Result<Self, ConfigError>;
    /// The ONLY reveal path; used solely when composing an outgoing header.
    pub(crate) fn expose(&self) -> &str;
}
```

**Header application order (enforced in `client.rs`):**

1. `endpoint.default_headers` (ordinary headers) applied first.
2. Protocol-required headers (e.g. Anthropic `anthropic-version`,
   `content-type: application/json`) applied next.
3. Resolved credential `extra_headers` applied next.
4. The primary auth header applied **last** — so no ordinary or extra header can
   override auth. Secret-bearing `HeaderValue`s are marked sensitive before
   insertion, so `HeaderMap` debug output redacts them.

Additionally, `Endpoint` construction/registration **rejects** a
`default_headers` map that contains the auth header name (`Authorization` for
`Bearer`, or the configured header name) → `ConfigError::AuthHeaderCollision`.
This closes the accidental-override path at configuration time, not just at send.

**Secret guarantees:** no secret in `Debug`; no secret serialized; no secret in
any `AiError` (`HttpError` stores status/request-id/retry-after/body-snippet with
Authorization-style headers never captured). Browser login, token persistence,
and OAuth UI live entirely in a `CredentialResolver` implementation *outside*
this crate and outside every protocol codec.

**Implementation status:** All `Auth` variants (including `Dynamic`) and the
`CredentialResolver` trait are implemented and tested. What remains is a
concrete `CredentialResolver` for subscription-based providers (OpenAI Codex
OAuth, Anthropic Pro/Max OAuth, GitHub Copilot token exchange). The PI reference
(`docs/research/refImpls/ai/pi-ai.md` §"OAuth Auth") documents the pattern:
`login()` → store credential → `refresh()` under double-checked locking →
`toAuth()` → `{ apiKey, headers, baseUrl }`. See `docs/plans/ygg-ai-oauth-codex.md`
for the concrete implementation plan.

---

## 10. Model catalog and configuration loading

```rust
// catalog.rs
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogConfig {
    pub endpoints: Vec<EndpointConfig>,
    pub models: Vec<ModelConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub id: EndpointId,
    pub base_url: url::Url,
    pub auth: AuthConfig,
    #[serde(default)] pub default_headers: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_timeout_secs")] pub timeout_secs: u64,
}

/// Secrets are referenced by env var, never inlined. This keeps the checked-in
/// catalog free of credentials and keeps `CatalogConfig` `Serialize`-safe.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthConfig {
    None,
    BearerEnv { var: String },
    HeaderEnv { name: String, var: String },
    Dynamic  { resolver_id: String },   // bound to a registered resolver at load
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: ModelId,
    pub endpoint: EndpointId,
    pub api_name: String,
    pub protocol: Protocol,
    pub capabilities: Capabilities,
    pub limits: ModelLimits,
    #[serde(default)] pub pricing: Option<Pricing>,
}
```

- `ModelCatalog::from_config` resolves `None` and the `*Env` variants without
  reading credential values. Environment values are read per request, so the
  embedded catalog loads and validates with no credentials present and supports
  key rotation. A missing/invalid value is `AuthError` from `stream()`. The method
  rejects `Dynamic` with `MissingCredentialResolver`; the explicit
  `from_config_with_resolvers` resolves `Dynamic.resolver_id` from the supplied
  registry and rejects a missing id.
- **No network at all.** The catalog never fetches models.dev or any registry.
- **The built-in catalog snapshot lives at `crates/ygg-ai/models/catalog.json`**
  and is embedded via `include_str!`; `ModelCatalog::builtin()` parses it.
  `ygg-ai` **owns** the catalog schema, parser, validation, and embedded snapshot.
  Refreshing that snapshot is **workspace tooling's** job, out of crate; **no
  refresh command is required in v0.1** and none exists in the crate.
- Custom OpenAI-compatible endpoints require **configuration only** — an
  `EndpointConfig` + `ModelConfig { protocol: OpenAiChat }`. No new Rust code.

### Base URL contract (normative)

`Endpoint.base_url` is the **versioned API directory**, must be an absolute
`http` or `https` URL, must have no query/fragment, and must end in `/`. Examples:
`https://api.openai.com/v1/` and `https://api.anthropic.com/v1/`. Catalog
validation rejects violations rather than repairing them. Codecs call
`Url::join` with relative paths only: Chat `chat/completions`, Responses
`responses`, Anthropic `messages`. Tests cover a custom path prefix (for example
`https://gateway.example/tenant/acme/v1/`) to prove it is preserved.

---

## 11. Cross-protocol serialization rules

One canonical conversation is kept forever. For each request the selected codec
derives *temporary* wire messages. There is no destructive transform.

The codec may, as required by the target protocol:

- **Map system instructions:** Chat → leading `system`/`developer` role message;
  Responses → `developer` (reasoning models) or `system` input item; Anthropic →
  top-level `system` parameter.
- **Merge adjacent same-role messages** where the protocol demands strict
  alternation. Anthropic requires alternating user/assistant: consecutive user
  messages (e.g. a text turn followed by tool-result turns) merge into one user
  message with multiple content blocks. Chat and Responses accept separate items.
- **Map tool calls / tool results.** Correspondence is preserved automatically
  because a `ToolCall.id` and its `ToolResult.tool_call_id` share the same
  canonical `ToolCallId`.
- **Normalize temporary tool IDs** when a protocol constrains the format. The
  normalization is *deterministic and per-request* (stable transform, e.g.
  truncate+hash to the target's charset/length), applied identically to the call
  and its result within one build so the pairing is never broken. Canonical IDs
  are left untouched.
- **Replay protocol-private continuation state only through a compatible
  protocol.** A `ReasoningState` replays iff `state.protocol == target.protocol`
  **and** `state.model == target model id` (signatures/encrypted content are
  model-scoped). Otherwise: Strict → `Unsupported`; Lossy → drop the reasoning
  part with a `Diagnostic`. Reasoning text is **never** auto-converted into
  ordinary assistant text.
- **Skip incomplete turns.** Assistant turns that never completed are not part of
  canonical history (failures are `AiError`, never stored), so no error/aborted
  messages ever enter serialization.

---

## 12. Exact mapping tables

Legend: `→` = canonical-to-wire (request build); `←` = wire-to-canonical (decode).
DTO field names are the private structs each codec owns.

### 12.1 OpenAI Chat Completions (`POST /v1/chat/completions`)

**Request (`→`):**

| Canonical | Wire |
|-----------|------|
| `system` | first message `{ role: "system"\|"developer", content }` (developer if reasoning model) |
| `UserMessage` text | `{ role:"user", content:[{type:"text",text}] }` |
| `UserPart::Media(Image)` | content part `{type:"image_url", image_url:{url}}` where `url` = the `ImageSource::Url` verbatim, or `data:<media_type>;base64,<b64(inline bytes)>`; `detail` → `image_url.detail` |
| `UserPart::Media(Audio)` | content part `{type:"input_audio", input_audio:{data:<b64(inline bytes)>, format:"wav"\|"mp3"}}`. Inline bytes come from `AudioPayload::Inline` or the `data` of `InlineWithProviderRef`; a bare `AudioPayload::ProviderRef` (no bytes) → Strict `Unsupported` / Lossy drop. The type cannot express URL audio. `format` other than `Wav`/`Mp3` → Strict `Unsupported` / Lossy drop |
| `UserPart::ToolResult` | `{ role:"tool", tool_call_id, content:[{type:"text",text},...] }`; Chat supports text tool-result parts only, so media is Strict `Unsupported` / Lossy drop+`Diagnostic` |
| `AssistantMessage` text | `{ role:"assistant", content }` |
| prior `AssistantPart::Media(Audio)` | assistant top-level `audio:{id}` iff payload has a same-protocol, unexpired provider ref; inline-only, wrong-protocol, or expired refs are Strict `Unsupported` / Lossy drop+`Diagnostic` |
| `AssistantPart::ToolCall` | assistant `tool_calls:[{id, type:"function", function:{name, arguments}}]` |
| `AssistantPart::Reasoning` | standard Chat: dropped on send (no reasoning input slot); `OpenAiChatReasoningMode::DeepSeekThinking`: same-model text → assistant `reasoning_content` (required after DeepSeek tool calls) |
| `ToolDef` | `tools:[{type:"function", function:{name, description, parameters}}]` |
| `ToolChoice` | `tool_choice`: `"auto"`/`"required"`/`"none"`/`{type:"function",function:{name}}` |
| effective output limit | `max_completion_tokens` (never speculative `max_tokens` fallback) |
| `OutputFormat::JsonObject` | `response_format:{type:"json_object"}` |
| `OutputFormat::JsonSchema(s)` | `response_format:{type:"json_schema",json_schema:{name,description?,schema,strict}}` |
| `ReasoningConfig::Effort` | standard Chat: `reasoning_effort: "minimal"\|"low"\|"medium"\|"high"`; DeepSeek-thinking Chat: `thinking:{type:"enabled"}` plus `reasoning_effort` (portable `minimal` → `low`, because DeepSeek rejects `minimal`) |
| `ReasoningConfig::Off` (DeepSeek-thinking Chat) | `thinking:{type:"disabled"}`; DeepSeek otherwise defaults thinking to enabled |
| `OutputModalities::TextAndAudio(AudioOutputOptions)` | `modalities:["text","audio"]`, `audio:{voice, format}` where `voice` = `AudioVoice::Named(s)` → the string `s`, or `AudioVoice::ProviderRef(id)` → `{id}`. Forces a **non-streaming** request (see below) |
| `cache_retention: Short/Long` on direct OpenAI or compatible endpoints | `prompt_cache_key` from the session ID; `Long` adds `prompt_cache_retention:"24h"` when supported |
| Anthropic-compatible cache format | `cache_control` markers on the system prompt, final conversation text, and final tool where supported |
| `cache_retention: None` | Omits all explicit prompt-cache fields and affinity headers |
| streaming usage | `stream_options:{include_usage:true}` (text/tool path only) |

**Two request shapes, one event surface.** When `output_modalities` is `Text`,
the Chat codec issues a **streaming** request and decodes `chat.completion.chunk`
deltas. When `TextAndAudio` is requested, the codec issues a **non-streaming**
request (the documented Chat streaming delta schema has no audio field; we do not
invent `choices[].delta.audio`). Either way `stream()` presents the same event
sequence.

**Streaming decode (`←`), `chat.completion.chunk` (text/tool path):**

| Wire delta | Canonical event |
|------------|-----------------|
| first chunk | `Started{response_id: chunk.id}` |
| `choices[0].delta.content` | `TextStart`/`TextDelta`/`TextEnd` |
| `choices[0].delta.reasoning_content` | `ReasoningStart`/`ReasoningDelta`/`ReasoningEnd` (text only, no state) |
| `choices[0].delta.tool_calls[i]` (id,name first) | `ToolCallStart` |
| `choices[0].delta.tool_calls[i].function.arguments` | `ToolCallArgsDelta` (accumulate by `tool_calls[i].index`) |
| tool block change / finish | `ToolCallEnd` |
| `choices[0].finish_reason` | maps to `StopReason` (§15) |
| `usage` (final) | `Usage` (see §15) |

**Completed audio decode (`←`), non-streaming `message`:** the whole
`message.audio:{id,data,transcript,expires_at}` decodes — with **all four fields
preserved together** — into one `AssistantPart::Media(Audio)`:

```rust
Media::Audio(AudioMedia {
    payload: AudioPayload::InlineWithProviderRef {
        data: base64_decode(message.audio.data),
        reference: ProviderMediaRef {
            protocol: Protocol::OpenAiChat,
            id: message.audio.id,
            expires_at: Some(message.audio.expires_at),
        },
    },
    format: requested_format,                 // the AudioOutputOptions.format we asked for
    transcript: Some(message.audio.transcript),
})
```

The bytes are the returned media; the id + expiry let the same audio be referenced
in later turns. The unified stream emits `Started`, any `Text*` events for
`message.content`, then a single `MediaCompleted` event, then `Usage` and
`Finished`. **There is no `MediaDelta`.**

### 12.2 OpenAI Responses (`POST /responses`)

**Request (`→`):**

| Canonical | Wire input item |
|-----------|-----------------|
| `system` | `{ role:"developer"\|"system", content:[{type:"input_text",text}] }` |
| user text | `{ role:"user", content:[{type:"input_text",text}] }` |
| `Media(Image)` | `{type:"input_image", image_url:<url or data URL>, detail?}` |
| `Media(Audio)` **input** | **UNSUPPORTED in v0.1.** Strict → structured `Unsupported(Audio)`; Lossy → drop + `Diagnostic`. Current OpenAI docs scope Responses to text/image input with text output and direct conversational audio to Chat Completions. We do not infer undocumented audio support. |
| `ToolCall` | `{type:"function_call", call_id, name, arguments}` |
| `ToolResult` | `{type:"function_call_output", call_id, output:[...]}`; text→`input_text`, image URL/inline→`input_image.image_url`, same-protocol provider ref→`input_image.file_id`; audio is unsupported |
| `ReasoningPart` w/ `OpenAiReasoning` state (same model) | `{type:"reasoning", id:item_id, summary:[...], encrypted_content}` replayed verbatim |
| `ToolDef` | `tools:[{type:"function", name, description, parameters}]` |
| `ReasoningConfig::Effort` | `reasoning:{effort:"minimal".."high"}` |
| `OutputFormat::JsonObject` | `text:{format:{type:"json_object"}}` |
| `OutputFormat::JsonSchema(s)` | `text:{format:{type:"json_schema",name,description?,schema,strict}}` |
| request encrypted reasoning | `include:["reasoning.encrypted_content"]`, `store:false` |
| `cache_retention: Short/Long` | `prompt_cache_key` from the session ID (clamped to 64 characters); `Long` also sends `prompt_cache_retention:"24h"` when compatible |
| `cache_retention: None` | Omits prompt-cache body fields and cache-affinity headers |
| `OutputModalities::TextAndAudio` | **UNSUPPORTED in v0.1.** Strict → `Unsupported(AudioOutput)`; Lossy → downgrade to `Text` + `Diagnostic`. No `response.audio.*` handling exists. |

**Stream decode (`←`):**

| Wire event | Canonical |
|------------|-----------|
| `response.created` | `Started{response_id: response.id}` |
| `response.output_item.added` (message) → `response.content_part.added` | `TextStart` |
| `response.output_text.delta` | `TextDelta` |
| `response.output_text.done` / `content_part.done` | `TextEnd` |
| `response.reasoning_text.delta` / `reasoning_summary_text.delta` | `ReasoningStart`/`ReasoningDelta` |
| reasoning item `done` (carries `encrypted_content`,`id`) | `ReasoningEnd` + fold `OpenAiReasoning` state |
| `response.output_item.added` (function_call) | `ToolCallStart{id:call_id, name}` |
| `response.function_call_arguments.delta` | `ToolCallArgsDelta` (accumulate by `output_index`/`item_id`) |
| `response.function_call_arguments.done` | `ToolCallEnd` |
| `response.completed` | `Usage` + `Finished` (`StopReason` §15) |
| `response.incomplete` | `Usage` + `Finished` with `MaxTokens`/`Refusal` |
| `response.failed` | `Err(Provider(..))` |
| top-level `error` event (`{type:"error", code, message}`) | `Err(Provider(..))` |

**No `response.audio.*` handling.** The Responses codec does not map audio events;
Responses is text/image-in, text-out in v0.1. Ignored (out of scope) event
families are skipped without error unless they are terminal: `response.audio.*`,
`web_search_call.*`, `file_search_call.*`, `code_interpreter_call.*`, `mcp_*`,
`image_generation_call.*`, `custom_tool_call_input.*`.

### 12.3 Anthropic Messages (`POST /v1/messages`)

**Request (`→`):**

| Canonical | Wire |
|-----------|------|
| `system` | top-level `system:[{type:"text",text,cache_control?}]` text-block array |
| user text | `{role:"user", content:[{type:"text",text}]}` |
| `Media(Image)` inline | `{type:"image", source:{type:"base64", media_type:<mime>, data:<b64(bytes)>}}` |
| `Media(Image)` url | `{type:"image", source:{type:"url", url}}` |
| `Media(Audio)` | **UNSUPPORTED in v0.1.** Strict → structured `Unsupported(Audio)`; Lossy → drop + `Diagnostic`. Audio is **never** silently removed. |
| `ToolResult` | user `{type:"tool_result", tool_use_id, content, is_error}` (merged into a user message); text and supported Anthropic image URL/inline parts map directly; audio and provider refs are Strict `Unsupported` / Lossy drop+`Diagnostic` |
| `AssistantPart::ToolCall` | `{type:"tool_use", id, name, input:<parsed args>}` |
| `ReasoningPart` w/ `AnthropicSignature` (same model) | `{type:"thinking", thinking:<text>, signature}` |
| `ReasoningPart` w/ `AnthropicRedacted` (same model) | `{type:"redacted_thinking", data}` |
| `ToolDef` | `tools:[{name, description, input_schema}]`; the final tool receives `cache_control` when enabled and supported |
| `ToolChoice` | `tool_choice`: `{type:"auto"\|"any"\|"tool", name?}` |
| `ReasoningConfig::Budget`/`Effort` | `thinking:{type:"enabled", budget_tokens}`; effort uses `ReasoningCapability.effort_budgets`, while explicit budget passes through after limit validation |
| effective output limit | `max_tokens` (request value or model maximum) |
| `OutputFormat::JsonSchema(s)` | `output_config:{format:{type:"json_schema",schema}}`; Anthropic has no name/description/strict wire fields, so non-default metadata is accepted without loss because it only labels/strengthens the same schema locally |
| `OutputFormat::JsonObject` | unsupported (Strict error / Lossy downgrade+diagnostic); Anthropic documents schema output, not unconstrained JSON mode |
| `cache_retention: Short` | `cache_control:{type:"ephemeral"}` on the system prompt, final user block, and final tool (where supported) |
| `cache_retention: Long` | Same markers with `ttl:"1h"` when the model compatibility allows long retention |
| `cache_retention: None` | Emits no `cache_control` fields or session-affinity header |

Cache usage returned by providers is normalized and priced. OpenAI prompt-cache
keys are derived from the stable session ID and clamped to the provider's
64-character limit; `None` is the explicit opt-out for requests that must not
send Ygg's cache controls.

**Stream decode (`←`), SSE with `event:` names:**

| Wire event | Canonical |
|------------|-----------|
| `message_start` | `Started{response_id: message.id}` |
| `content_block_start` type `text` | `TextStart` |
| `content_block_delta` `text_delta` | `TextDelta` |
| `content_block_start` type `thinking` | `ReasoningStart` |
| `content_block_delta` `thinking_delta` | `ReasoningDelta` |
| `content_block_delta` `signature_delta` | accumulate signature into `AnthropicSignature` |
| `content_block_start` type `redacted_thinking` | `ReasoningStart` (+ `AnthropicRedacted` data) |
| `content_block_start` type `tool_use` (id,name) | `ToolCallStart` |
| `content_block_delta` `input_json_delta` | `ToolCallArgsDelta` (accumulate by block index) |
| `content_block_stop` | `TextEnd`/`ReasoningEnd`/`ToolCallEnd` for that index |
| `message_delta` (`stop_reason`, `usage`) | set `StopReason` (§15), accumulate `Usage` |
| `message_stop` | `Usage` + `Finished` |
| `ping` | ignored (keep-alive) |
| `error` event | `Err(Provider(..))` |

Anthropic has **no audio** in v0.1: audio input returns a structured
`Unsupported(Audio)` error in Strict mode (dropped with a `Diagnostic` in Lossy);
audio is never silently removed, and no audio output events exist. Anthropic's own
OpenAI-compatibility layer *silently* strips audio input, ignores audio request
fields, and returns empty audio; Ygg deliberately **rejects before transmission**
rather than inheriting that silent behavior.

---

## 13. Tool-call argument accumulation (per protocol)

All three accumulate **raw text**, keyed by a protocol-native index, parsed to a
JSON object exactly once at part end. Parse failure → `AiError::Decode`
terminating the stream before `Finished`.

| Protocol | Key | Start (id, name) | Fragments | End |
|----------|-----|------------------|-----------|-----|
| Chat | `delta.tool_calls[].index` | first fragment carries `id`+`function.name` | `function.arguments` string pieces | next tool index / `finish_reason` |
| Responses | `output_index` / `item_id` | `output_item.added` function_call (`call_id`,`name`) | `function_call_arguments.delta` | `function_call_arguments.done` |
| Anthropic | content block `index` | `content_block_start` tool_use (`id`,`name`) | `input_json_delta.partial_json` | `content_block_stop` |

Empty arguments normalize to `{}` at end (all three may emit zero fragments for a
no-arg tool). Parallel tool calls interleave by key; the accumulator maintains
one buffer per open key and `ToolCallEnd` fires per key independently.

---

## 14. Reasoning & opaque continuation state (per protocol)

| Protocol | Visible text source | State captured | Replay rule |
|----------|--------------------|----------------|-------------|
| Chat | `delta.reasoning_content` | none (`state = None`) | standard Chat: not replayable; `DeepSeekThinking`: same-model text is sent back as assistant `reasoning_content` (required after tool calls) |
| Responses | `reasoning_text` / `reasoning_summary_text` deltas | `OpenAiReasoning{item_id, encrypted_content}` (encrypted only if `include` requested) | replay iff same protocol+model; sent as a `reasoning` input item verbatim |
| Anthropic | `thinking_delta` text | `AnthropicSignature{signature}`; `redacted_thinking` → `AnthropicRedacted{data}` (no text) | replay iff same protocol+model; sent as `thinking`/`redacted_thinking` block |

Rules that hold across all: opaque state is **never** flattened to text; reasoning
text is **never** auto-promoted to assistant text on a protocol/model switch;
mismatched state is dropped (Lossy + `Diagnostic`) or rejected (Strict). Redacted
reasoning carries no visible text and is only ever replayed to the same model.

---

## 15. Usage and stop-reason mapping

**Token normalization** into the canonical mutually-exclusive buckets (§6.6).
Canonical `input_tokens` is *full-rate input only*.

| Canonical | Chat | Responses | Anthropic |
|-----------|------|-----------|-----------|
| `input_tokens` | `prompt_tokens − cached_tokens` | `input_tokens − cached_tokens − cache_write_tokens` | `input_tokens` (already excludes cache) |
| `cache_read_tokens` | `prompt_tokens_details.cached_tokens` | `input_tokens_details.cached_tokens` | `cache_read_input_tokens` |
| `cache_write_tokens` | `0` (no write surcharge) | `input_tokens_details.cache_write_tokens` (0 if absent) | `cache_creation_input_tokens` |
| `cache_write_1h_tokens` | `0` | `0` | `cache_creation.ephemeral_1h_input_tokens` |
| `output_tokens` | `completion_tokens` | `output_tokens` | `output_tokens` |
| `reasoning_tokens` | `completion_tokens_details.reasoning_tokens` (0 if absent) | `output_tokens_details.reasoning_tokens` | `message_delta` `usage.output_tokens_details.thinking_tokens` (0 if absent) |
| `total_tokens` | provider `total_tokens` or sum | provider `total_tokens` or sum | `input+cache_read+cache_write+output` |

Rationale for the OpenAI subtraction: OpenAI's `input_tokens`/`prompt_tokens`
*include* cached tokens, so we subtract them out to keep buckets disjoint;
Anthropic's `input_tokens` already *excludes* cache, so it maps directly. This
makes `pricing.rs` provider-agnostic: it applies one rate per bucket.
`cache_write_1h_tokens` is a subset of `cache_write_tokens`, so 5m-priced writes
are `cache_write_tokens - cache_write_1h_tokens`; it is never double-charged.
Likewise, `reasoning_tokens` is a subset of `output_tokens`: when a dedicated
reasoning rate exists it applies to that subset and the ordinary output rate
applies to the remainder. `cost_of` returns `Result<Cost, PricingError>` and uses
checked `u128` arithmetic; malformed subset relationships or a final `u64`
overflow are errors, never saturation or wrapping. Usage normalization uses
checked subtraction; contradictory provider counters are
`DecodeError::UsageUnderflow`.

**Stop reasons:**

| Canonical | Chat `finish_reason` | Responses | Anthropic `stop_reason` |
|-----------|----------------------|-----------|-------------------------|
| `EndTurn` | `stop` | `completed` (no tool call) | `end_turn` |
| `MaxTokens` | `length` | `incomplete{reason:max_output_tokens}` | `max_tokens` |
| `ToolUse` | `tool_calls` (or legacy `function_call`) | `completed` with `function_call` output | `tool_use` |
| `StopSequence` | — | — | `stop_sequence` |
| `Refusal` | `content_filter` | `incomplete{reason:content_filter}` | `refusal` |
| `PauseTurn` | — | — | `pause_turn` |
| `Other(s)` | any unknown | any unknown | any unknown |

---

## 16. Error taxonomy

```rust
// error.rs
#[derive(Debug)]
pub enum AiError {
    Config(ConfigError),
    Auth(AuthError),
    Validation(ValidationError),
    Unsupported(UnsupportedError),
    Http(HttpError),                 // non-2xx response; status exists
    Transport(TransportError),       // request/body I/O, timeout, no HTTP status
    Provider(ProviderError),         // provider error frame inside a 2xx stream
    Decode(DecodeError),
    Pricing(PricingError),
    StreamProtocol(StreamProtocolError),
    Canceled,
}

pub struct HttpError {
    pub status: http::StatusCode,
    pub request_id: Option<String>,
    pub retry_after: Option<std::time::Duration>,
    pub provider_code: Option<String>,
    pub body_snippet: Option<String>,      // truncated; never contains auth headers
    pub retryable: bool,                   // safe pre-first-byte retry hint
}
pub struct TransportError {
    pub phase: TransportPhase,       // ConnectOrHeaders | Body
    pub timeout: bool,
    pub message: String,             // sanitized; never a request URL with credentials
}
pub enum TransportPhase { ConnectOrHeaders, Body }
pub struct ProviderError {
    pub code: Option<String>,
    pub kind: Option<String>,              // provider "type"
    pub message: String,
    pub request_id: Option<String>,
}
#[non_exhaustive] pub enum StreamProtocolError {
    MissingStart, DuplicateStart, MissingFinish, EventAfterFinish,
    UnbalancedPart { index: usize }, PrematureEof, UnexpectedEvent(String),
}
pub enum UnsupportedError {
    Image, Audio, AudioOutput, Tools, ToolChoice, Reasoning,
    ReasoningStateMismatch { have: Protocol, want: Protocol },
    ProviderMediaRef, ToolResultMedia, StructuredOutput,
}
pub enum ConfigError {
    Parse(String), DuplicateEndpoint(EndpointId), DuplicateModel(ModelId),
    UnknownEndpoint(EndpointId), UnknownModel(ModelId), MissingEnv(String),
    MissingCredentialResolver(String), AuthHeaderCollision(http::HeaderName),
    InvalidHeader(String), InvalidBaseUrl(String), InvalidReasoningConfig(ModelId),
    InvalidPricing(ModelId),
}
pub enum AuthError {
    Resolve, MissingEnvironment(String), InvalidHeaderValue, InvalidCredential,
}
pub enum ValidationError {
    OrphanToolResult(ToolCallId), MissingToolResult(ToolCallId),
    ToolArgumentsNotObject(ToolCallId), InvalidToolSchema(String),
    InvalidOutputSchema(String), InvalidOutputFormatName(String),
    InvalidMaxOutputTokens { requested: u64, model_max: u64 },
    InvalidTemperature, ReasoningBudgetOutOfRange,
}
pub enum DecodeError {
    Json(String), InvalidUtf8, InvalidBase64, ToolArgumentsTooLarge,
    BodyTooLarge, UsageUnderflow, InvalidProviderField(String),
}
pub enum PricingError { ArithmeticOverflow, InvalidUsageBuckets }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic { pub code: String, pub message: String }
```

All sub-errors implement `Error` + `Display`; variants carrying source-library
errors store only sanitized strings, never request headers, credentials, or URLs
with userinfo. `AiError` implements `Error` + `Display` (via `thiserror`). **No
variant ever carries a `Secret` or raw credential.** Diagnostic codes are stable
snake_case identifiers defined as constants beside validation (for example
`dropped_audio`, `dropped_provider_state`, `downgraded_output_format`); tests
assert exact codes, not only non-empty diagnostics. `HttpError.retryable` and
`retry_after` are informational; the crate performs no automatic retries (§17).

---

## 17. Cancellation, timeout, and retry

- **Cancellation = drop.** The `ResponseStream` owns the reqwest response body.
  Dropping it drops the body future; no spawned task and no channel keep reading.
  A test asserts a drop-sentinel body stops being polled after drop.
- **Timeout.** `Endpoint.timeout` is applied to the request (connect + headers +
  overall) via reqwest. A timeout before headers →
  `AiError::Transport { phase: ConnectOrHeaders, timeout: true }` returned from
  `stream()`. A body timeout or body I/O failure is emitted in-stream as
  `AiError::Transport { phase: Body, .. }`. `PrematureEof` is reserved for a
  clean body close before the provider's terminal event.
- **Retry policy (v0.1 decision).** The crate does **no** automatic retries. A
  *pre-first-byte* retry (idempotent handshake failure, 429/503 with
  `retry_after`) is **the caller's responsibility**: `stream()` returns the
  `HttpError` with `retryable`/`retry_after` so the caller can re-issue the same
  `Request`. Rationale: `Request` is cheap to clone and the caller owns backoff
  policy and budget; baking retry in would duplicate that concern and risk
  double-charging. **Never** retry after any `TextDelta`/`ReasoningDelta`/
  `ToolCall*`/`Media` output event has been emitted — partial output makes a
  silent re-issue unsafe. A helper `HttpError::is_safe_to_retry()` encodes the
  pre-first-byte condition for callers.

---

## 18. Dependencies (direct, justified)

| Crate | Features | Why |
|-------|----------|-----|
| `reqwest` | `rustls-tls`, `stream`, `http2`, no default TLS | The one HTTP client; `bytes_stream()` feeds the SSE decoder; rustls avoids OpenSSL. |
| `http` | — | `HeaderMap`/`HeaderName`/`StatusCode` in canonical + error types without pulling reqwest into `types`. |
| `bytes` | `serde` | `Bytes` chunks from reqwest; inline media bytes (`ImageSource::Inline`/`AudioPayload::Inline`/`InlineWithProviderRef`); zero-copy slicing in the SSE decoder. |
| `futures-core` | — | `Stream` trait for `ResponseStream` (no runtime coupling). |
| `futures-util` | — | `StreamExt` in `complete()` and stream adapters. |
| `async-stream` | — | Author codec streams with `try_stream!` without hand-writing pinned state machines. Uses a pinned generator, **not** a channel; dropping the stream drops the body (satisfies §17). This is the one ergonomics dependency; justified because all three codecs need it. |
| `serde` | `derive` | Canonical types + catalog + DTOs. |
| `serde_json` | — | DTO (de)serialization; tool-arg parsing. |
| `async-trait` | — | `CredentialResolver` async method in a trait object (`Auth::Dynamic`). |
| `thiserror` | — | Ergonomic `Error`/`Display` for the taxonomy without hand-rolling. |
| `url` | `serde` | `Url` type for `base_url` and media URLs; validated parsing. |
| `base64` | — | Encode/decode inline media (image data URLs, Chat `input_audio`, completed `message.audio`). |
| `mime` | — | Open image media types (serialized by a private string serde adapter because `Mime` has no serde impl) (`mime::Mime` on `ImageMedia.media_type`); parse/format image MIME for wire data URLs and Anthropic `media_type`. |

**Dev-dependencies:** `tokio` (`rt`, `macros`) to drive async tests;
`wiremock` for a small number of *offline* HTTP-path integration tests (no API
keys, no live network); `pretty_assertions` for readable fixture diffs.

**Deliberately avoided:** provider SDKs (openai/anthropic), any model-registry
client, a separate SSE crate (our `sse.rs` gives full control over the §19 test
matrix and is ~150 lines), and any `f64`/decimal-money crate (integers only).

---

## 19. Test architecture and fixture strategy

Deterministic, offline, no API keys. Three layers:

1. **SSE decoder unit tests** (`sse.rs`): the §13-of-the-base-spec matrix —
   arbitrary byte chunking, LF vs CRLF, multiline `data:`, comments/keep-alives,
   empty events, UTF-8 spanning chunk boundaries, `[DONE]`, Anthropic `event:`
   names, malformed frames, transport closure before terminal. A property-style
   helper re-feeds each fixture at **every** byte boundary and asserts identical
   `SseEvent` output.

2. **Codec fixture tests** (`tests/`): captured/hand-authored `.sse` payloads per
   protocol under `tests/fixtures/{openai_chat,openai_responses,anthropic}/`,
   each paired with an expected canonical outcome. A shared harness feeds a
   fixture (optionally re-chunked at arbitrary boundaries) into the codec's
   `decode`, collecting the `StreamEvent` sequence and final `Response`. Request
   builders are tested by asserting the emitted DTO JSON against golden files.

3. **Client path integration tests** (`tests/`, offline): `wiremock` serves a
   fixture body with `content-type: text/event-stream` (and error statuses) so
   `AiClient::stream`/`complete` are exercised end-to-end without network or
   keys.

**Required fixture cases (all offline):**

plain streamed text; multiple text blocks; streamed reasoning; redacted/opaque
reasoning state; one tool call; parallel tool calls with interleaved deltas; tool
args split at arbitrary byte boundaries; malformed tool JSON; usage only at
stream end; length stop; tool-use stop; cancellation (drop mid-stream);
HTTP non-2xx provider error; premature EOF; **image serialization** (inline bytes
+ URL, all three protocols); **Chat audio-input serialization** (`input_audio`,
WAV/MP3 inline); **Chat completed audio-output decode** (non-streaming
`message.audio` → a single `Media` event + `Finished`); tool-result
serialization; same-protocol replay of provider state (Responses
`encrypted_content`, Anthropic signature); cross-protocol handling of provider
state (drop/reject per mode); strict unsupported-capability rejection
(audio→Anthropic, audio→Responses input **and** output, image→text-only);
explicit lossy conversion diagnostics; cost calculation and pricing-tier
boundaries; secret redaction (`Debug`/`Display`/serialize attempts).

**Fixture prohibitions:** no fixture may invent `choices[].delta.audio` or any
`response.audio.*` event — neither has authoritative captured payloads or
documented streaming support in this repo. Chat audio output is tested via the
non-streaming `message.audio` object; Responses/Anthropic audio is tested only as
*rejection*. Fixtures are hand-authored where capture would require a key; each
carries a short header comment citing the `apidocs/` section it mirrors.

---

## 20. Security and reliability review

- **Secrets:** never in `Debug`/`Display`/`Serialize`/errors; single `expose()`
  path used only to build a header; auth header applied last and collision
  rejected at config time (§9). Optional future: `zeroize` on drop.
- **Header injection:** header names/values validated by the `http` crate;
  invalid names → `ConfigError`.
- **SSE robustness:** the shared decoder tolerates chunk splitting, CRLF,
  keep-alives, and UTF-8 boundary spans, and reports (never panics on) malformed
  frames. Hard constants prevent hostile buffering:
  `MAX_SSE_EVENT_BYTES = 2 * 1024 * 1024`,
  `MAX_TOOL_ARGUMENT_BYTES = 16 * 1024 * 1024`, and
  `MAX_COMPLETED_BODY_BYTES = 64 * 1024 * 1024`. Crossing a limit is a
  `DecodeError`; the non-streaming Chat response body is still read chunk-wise
  and bounded before JSON decoding.
- **No unbounded channels / no partial clones:** memory per stream is O(open
  parts), not O(events).
- **Cancellation safety:** drop stops network I/O deterministically; no retry
  after output begins prevents duplicate side effects and double-charge.
- **Integer money:** `u128` intermediate, `u64` microdollar output; no float
  rounding drift; tiers selected by *this request's* input tokens only.
- **Deterministic ID normalization:** cross-protocol tool-ID mapping is a pure
  function; call/result pairing cannot silently break.

---

## 21. Deferred features (explicit)

Realtime/WebSocket/SIP session inference; voice-activity detection (VAD);
standalone transcription (`/audio/transcriptions`) and TTS (`/audio/speech`);
incremental media-delta streaming (a future base64 media-delta protocol,
decoded once at completion — §24.5); Responses/Anthropic audio of any kind;
image generation; computer use; code interpreter; hosted web/file search; MCP
execution; provider-hosted conversation persistence (`store`/`conversation`);
background responses; live online catalog refresh; runtime plugin ABI; browser
OAuth UI; concrete `CredentialResolver` implementations for subscription-based
providers (OpenAI Codex, Anthropic Pro/Max, GitHub Copilot); and everything in
the agent layer (loop, tool execution, session
storage, compaction, TUI). `Modality` is `#[non_exhaustive]` so Video/Document
slot in later without a breaking change.

---

## 22. End-to-end usage example

```rust
use ygg_ai::*;
use bytes::Bytes;

# async fn demo(png: Bytes, wav: Bytes) -> Result<(), AiError> {
// 1. Load the built-in, embedded catalog (env names only; credential values are
// resolved when a request is sent; no network I/O occurs while loading).
let catalog = ModelCatalog::builtin()?;

// 2. Resolve an audio-capable Chat model (audio in/out lives on the Chat protocol).
let model = catalog.resolve(&ModelId("gpt-audio-1.5".into()))?;

// 3. Provider-independent request — image + audio INPUT, and audio OUTPUT requested.
let request = Request {
    system: Some("You are Ygg, a terse coding agent.".into()),
    messages: vec![Message::User(UserMessage { content: vec![
        UserPart::Text("What's in this screenshot, and what did I say?".into()),
        UserPart::Media(Media::image_bytes(png, "image/png".parse().unwrap())),
        UserPart::Media(Media::audio_bytes(wav, AudioFormat::Wav)),
    ]})],
    tools: vec![ToolDef {
        name: "grep".into(),
        description: "Search files".into(),
        parameters: serde_json::json!({
            "type":"object","properties":{"pattern":{"type":"string"}},
            "required":["pattern"]
        }),
    }],
    tool_choice: ToolChoice::Auto,
    max_output_tokens: Some(2048),
    temperature: None,
    stop: vec![],
    reasoning: ReasoningConfig::Off,
    output_format: OutputFormat::Text,
    // Audio output forces a non-streaming Chat request; the event surface is unchanged.
    output_modalities: OutputModalities::TextAndAudio(AudioOutputOptions {
        format: AudioFormat::Wav,
        voice: AudioVoice::Named("alloy".into()),
    }),
    compatibility: CompatibilityMode::Strict,
};

let client = AiClient::new();

// 4a. Stream (agent loop path). Same events whether the underlying request streams or not:
use futures_util::StreamExt;
let mut stream = client.stream(&model, request.clone()).await?;
while let Some(ev) = stream.next().await {
    match ev? {
        StreamEvent::TextDelta { delta, .. } => print!("{delta}"),
        StreamEvent::ToolCallEnd { index } => eprintln!("(tool {index} ready)"),
        // Audio output arrives as ONE completed media event (no MediaDelta in v0.1):
        StreamEvent::MediaCompleted { media: Media::Audio(a), .. } => {
            let bytes = match &a.payload {
                AudioPayload::Inline(b) => b.len(),
                AudioPayload::InlineWithProviderRef { data, .. } => data.len(),
                AudioPayload::ProviderRef(_) => 0,
            };
            eprintln!("(audio: {bytes} bytes, transcript={:?})", a.transcript);
        }
        StreamEvent::Finished(resp) => {
            if let Some(cost) = resp.cost { eprintln!("cost: {} µ$", cost.total); }
            // Only successful content goes into history:
            // history.push(Message::Assistant(resp.message));
        }
        _ => {}
    }
}

// 4b. Or one-shot — same path, same events, just consumed for you.
let response = client.complete(&model, request).await?;
assert!(matches!(response.stop_reason, StopReason::ToolUse | StopReason::EndTurn));
# Ok(()) }
```

---

## 23. Rejected alternatives

- **A large `Provider` trait** (models + auth + refresh + inference). Rejected:
  it forces every service into one polymorphic shape and makes an
  OpenAI-compatible endpoint a *code* change. Ygg uses `Endpoint` + `ModelSpec`
  data and a `Protocol` `match`; a custom endpoint is pure configuration.
- **Separate provider + API trait layers** (Pi's `Provider` → `ProviderStreams`
  lazy indirection). Rejected: two indirection layers to select one of three
  codecs. A compile-time `match` on `Protocol` is smaller and monomorphized.
- **Unbounded MPSC stream channels** (Pi's `EventStream` queue). Rejected: an
  unbounded queue to adapt an HTTP byte stream both risks memory blowup and
  detaches producer lifetime from consumer drop. `async-stream` over the reqwest
  body ties cancellation to drop and needs no channel.
- **Cloning the full partial response into every delta** (Pi puts `partial` on
  every event). Rejected: O(n²) copying over a long generation. Deltas carry only
  their own payload; the consumer keeps a partial if it wants one.
- **Destructive `transform_messages()`** that rewrites history for the selected
  provider. Rejected: it corrupts the canonical conversation and defeats
  cross-provider switching. Switching is per-request serialization from an
  immutable canonical history (§11).
- **Errors as assistant messages** (Pi's `errorMessage`/aborted assistant turns).
  Rejected: it pollutes history with non-content and forces every consumer to
  distinguish success from failure inside the content model. Failures are
  `AiError`; only successful `Response.message` is appendable.
- **Live model-registry refresh inside the crate** (models.dev fetch). Rejected:
  it puts network I/O and a moving dependency inside the inference hot path. The
  catalog is static config; snapshots are refreshed by an out-of-crate task.
- **Automatic retry after stream output begins.** Rejected: re-issuing after any
  emitted delta/tool-call risks duplicated output and side effects. Only a
  pre-first-byte retry is safe, and that is the caller's decision (§17).

---

## 24. Final decisions (previously open; now settled)

These are decisions, not questions:

1. **Scope of audio.** `ygg-ai` v0.1 supports **request-based conversational
   audio only**. Realtime speech-to-speech (WebRTC/WebSocket/SIP), standalone STT
   (`/audio/transcriptions`), standalone TTS (`/audio/speech`), and VAD are
   **deferred**.
2. **Catalog location.** The built-in catalog lives at
   **`crates/ygg-ai/models/catalog.json`** (embedded via `include_str!`).
3. **Catalog ownership.** `ygg-ai` owns the catalog schema, parser, validation,
   and embedded snapshot. Workspace tooling owns eventual refresh. **No refresh
   command is required or shipped in v0.1.**
4. **No `MediaDelta` in v0.1 (deferred, not open).** Audio output is a single
   completed `MediaCompleted` event followed by `Finished`. The documented Chat
   streaming delta schema exposes `content`, `refusal`, `role`, and tool/function
   fragments but **no audio delta**; the audio guide routes conversational audio
   through Chat Completions and demonstrates *completed* audio output. So
   streaming audio is simply deferred, not an open design question.
5. **Future media-delta protocol.** A later base64 media-delta protocol will
   expose encoded string fragments and decode **once at media completion**. v0.1
   does not implement it.
6. **MIME vs format.** Open image media types use **`mime::Mime`**; audio wire
   formats use the closed **`AudioFormat`** enum (`Wav, Aac, Mp3, Flac, Opus,
   Pcm16`). Chat audio **input** additionally validates to WAV/MP3 only.
7. **Voice.** Voice is a generation-only option on `AudioOutputOptions`
   (`AudioVoice::Named(String)` open for new named voices; `ProviderRef(String)`
   for custom-voice ids). It is **not** stored on `AudioMedia` and **not**
   represented by `AudioFormat`.
8. **`expires_at`.** Kept in `ProviderMediaRef`; a completed Chat audio object is
   `AudioPayload::InlineWithProviderRef { data, reference: ProviderMediaRef{ id,
   expires_at, .. } }`, preserving all four documented response fields (`data`,
   `id`, `expires_at`, `transcript`) without leaking the wire DTO.
9. **Workspace shape.** A **root Cargo workspace** with `crates/ygg-ai`
   (`[workspace]` root + member crate).
10. **Structured output is first-class.** `Request.output_format` maps exactly as
    specified in §12; it is not a dangling capability bit.
11. **Dynamic and environment auth loading.** `from_config` rejects dynamic
    entries; `from_config_with_resolvers` is the only config-loading path that
    binds them. Environment-backed auth is intentionally lazy and read per
    request, so `builtin()` never requires credentials and key rotation works.
12. **Base URLs are versioned directories.** They end in `/`; codec paths are
    relative and preserve custom gateway prefixes (§10).
13. **Request cache controls are explicit and provider-specific.** Requests default
    to short retention, support `none` as an opt-out and `long` where compatible,
    and codecs map the policy to Anthropic cache markers or OpenAI prompt-cache
    fields/affinity headers. Usage decoding/pricing remains normalized.
14. **Transport failures are distinct from HTTP failures.** A failure without an
    HTTP status is `AiError::Transport`; non-2xx is always `AiError::Http`, and
    provider error frames within a 2xx stream are `AiError::Provider`.

### Remaining uncertainty (implementation-time, not blocking)

- Exact provider-accepted `format` value sets are model-versioned; `AudioFormat`
  covers the common codecs and unknown named voices pass through as strings.
  Confirm against the target model at integration time.
