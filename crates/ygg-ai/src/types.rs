//! Canonical, provider-independent conversation and request/response types.

use crate::error::DecodeError;
use crate::pricing::Pricing;
use crate::CompatibilityMode;
use serde::{Deserialize, Serialize};

/// Newtype representing an endpoint identifier.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct EndpointId(pub String);

/// Newtype representing a model identifier.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ModelId(pub String);

/// Newtype representing a tool call identifier.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

/// Supported prompt-cache retention policies.
///
/// `Short` is the default and matches pi's provider defaults. `None` disables
/// all explicit cache controls and cache-affinity identifiers for a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    /// Disable prompt caching controls for this request.
    None,
    /// Provider default short-lived cache retention.
    #[default]
    Short,
    /// Request the provider's long-lived retention where supported.
    Long,
}

/// Cache compatibility knobs for provider/model variants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCompatibility {
    /// Whether long retention is supported by this model/endpoint.
    #[serde(default = "default_true")]
    pub supports_long_retention: bool,
    /// Whether Responses-style `session_id` cache affinity is supported.
    #[serde(default = "default_true")]
    pub send_session_id_header: bool,
    /// Whether Chat/Anthropic-compatible session-affinity headers are supported.
    #[serde(default)]
    pub send_session_affinity_headers: bool,
    /// Provider-specific session-affinity header convention. When omitted,
    /// codecs retain their protocol's historical default behavior.
    #[serde(default)]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    /// Optional Anthropic-style cache-control convention on Chat payloads.
    #[serde(default)]
    pub cache_control_format: Option<CacheControlFormat>,
    /// Whether Anthropic-style cache markers are accepted on tool definitions.
    #[serde(default = "default_true")]
    pub supports_cache_control_on_tools: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for CacheCompatibility {
    fn default() -> Self {
        Self {
            supports_long_retention: true,
            send_session_id_header: true,
            send_session_affinity_headers: false,
            session_affinity_format: None,
            cache_control_format: None,
            supports_cache_control_on_tools: true,
        }
    }
}

/// Cache-control wire convention used by an OpenAI-compatible endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlFormat {
    /// Anthropic `cache_control: { type: "ephemeral", ttl?: "1h" }`.
    Anthropic,
}

/// Provider-specific headers used to keep a prompt-cache session routed
/// consistently across requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAffinityFormat {
    /// `session_id`, `x-client-request-id`, and `x-session-affinity`.
    OpenAi,
    /// `x-client-request-id` and `x-session-affinity`, without `session_id`.
    OpenAiNoSession,
    /// OpenRouter's `x-session-id` header.
    OpenRouter,
    /// Codex's `session-id` and `x-client-request-id` headers.
    Codex,
}

/// Supported wire protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI Responses protocol.
    OpenAiResponses,
    /// OpenAI Chat Completions protocol.
    OpenAiChat,
    /// Anthropic Messages protocol.
    AnthropicMessages,
}

/// Preferred transport for streaming provider responses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointTransport {
    /// Use the provider's ordinary HTTP/SSE transport.
    #[default]
    Http,
    /// Prefer WebSocket when the protocol implements it, with HTTP/SSE as a
    /// compatibility fallback.
    WebSocketPreferred,
}

/// Endpoint configuration for connecting to a provider.
#[derive(Clone)]
pub struct Endpoint {
    /// Unique endpoint identifier.
    pub id: EndpointId,
    /// Versioned base URL of the endpoint (must end with trailing slash).
    pub base_url: url::Url,
    /// Auth method for the endpoint.
    pub auth: crate::auth::Auth,
    /// Default headers to apply to requests.
    pub default_headers: http::HeaderMap,
    /// Preferred response transport.
    pub transport: EndpointTransport,
    /// Request timeout.
    pub timeout: std::time::Duration,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("default_headers", &"<redacted>")
            .field("transport", &self.transport)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Model capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Supported modalities for model input.
    pub input_modalities: ModalitySet,
    /// Supported modalities for model output.
    pub output_modalities: ModalitySet,
    /// Whether the model supports tools.
    pub tools: bool,
    /// Whether the model supports parallel tool calling.
    pub parallel_tool_calls: bool,
    /// Model reasoning capability options, if supported.
    pub reasoning: Option<ReasoningCapability>,
    /// Whether the model supports structured outputs (JSON schema / mode).
    pub structured_output: bool,
}

/// Compact set over a small closed modality universe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalitySet {
    bits: u8,
}

impl ModalitySet {
    /// Creates an empty modality set.
    pub const fn none() -> Self {
        Self { bits: 0 }
    }

    /// Returns a new set containing the given modality.
    pub fn with(self, m: Modality) -> Self {
        let bit = match m {
            Modality::Image => 1 << 0,
            Modality::Audio => 1 << 1,
        };
        Self {
            bits: self.bits | bit,
        }
    }

    /// Returns whether this set contains the given modality.
    pub fn contains(self, m: Modality) -> bool {
        let bit = match m {
            Modality::Image => 1 << 0,
            Modality::Audio => 1 << 1,
        };
        (self.bits & bit) != 0
    }

    pub(crate) fn is_valid(self) -> bool {
        self.bits & !0b11 == 0
    }
}

/// Supported modalities (besides Text which is always implied).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modality {
    /// Image input/output.
    Image,
    /// Audio input/output.
    Audio,
}

/// Model reasoning capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningCapability {
    /// How the request selects reasoning effort.
    pub control: ReasoningControl,
    /// Whether the model streams reasoning/summary text.
    pub exposes_text: bool,
    /// Whether the model preserves reasoning/thinking signatures or state for continuation.
    pub preserves_state: bool,
    /// Budget maps from portable effort to token budgets, required iff control is TokenBudget.
    pub effort_budgets: Option<ReasoningEffortBudgets>,
    /// OpenAI Chat-Completions-specific reasoning behavior.
    #[serde(default)]
    pub openai_chat_mode: OpenAiChatReasoningMode,
    /// Lowest reasoning effort this model meaningfully distinguishes.
    /// Pickers omit lower tiers and request normalization raises lower values
    /// to this floor.
    /// Defaults to `Minimal` so models that support the full portable range
    /// work without catalog changes.
    #[serde(default = "default_min_effort")]
    pub min_effort: ReasoningEffort,
    /// Highest reasoning effort this model accepts. A request above this tier is
    /// clamped down rather than emitting a value the backend would reject.
    /// Defaults to `High` so models predating `xhigh`/`max` never advertise them.
    #[serde(default = "default_max_effort")]
    pub max_effort: ReasoningEffort,
}

/// Default floor for [`ReasoningCapability::min_effort`] when unspecified.
fn default_min_effort() -> ReasoningEffort {
    ReasoningEffort::Minimal
}

/// Default ceiling for [`ReasoningCapability::max_effort`] when unspecified.
fn default_max_effort() -> ReasoningEffort {
    ReasoningEffort::High
}

/// Provider extension used by an OpenAI Chat Completions reasoning model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiChatReasoningMode {
    /// Standard OpenAI-compatible `reasoning_effort` behavior.
    #[default]
    Standard,
    /// DeepSeek's explicit `thinking` toggle and `reasoning_content` replay.
    DeepSeekThinking,
    /// OpenRouter's provider-neutral reasoning object (`reasoning.effort`).
    OpenRouter,
    /// Reasoning effort is supported, but the system instruction must remain a
    /// `system` message rather than OpenAI's `developer` role. This is used by
    /// OpenAI-compatible servers whose chat template rejects `developer`, such
    /// as Qwen3.5 served by vLLM.
    SystemMessage,
}

/// Selection control mechanism for reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningControl {
    /// Control via effort tags (Minimal, Low, Medium, High).
    Effort,
    /// Control via explicit token budget.
    TokenBudget,
}

/// Maps portable effort levels to token budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningEffortBudgets {
    /// Minimal effort token budget.
    pub minimal: u64,
    /// Low effort token budget.
    pub low: u64,
    /// Medium effort token budget.
    pub medium: u64,
    /// High effort token budget.
    pub high: u64,
    /// Extra-high effort token budget.
    pub xhigh: u64,
    /// Maximum effort token budget.
    pub max: u64,
}

/// Model limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    /// Context window size in tokens.
    pub context_window: u64,
    /// Maximum allowed output tokens.
    pub max_output_tokens: u64,
}

/// Complete description of a model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Model identifier.
    pub id: ModelId,
    /// Endpoint identifier.
    pub endpoint: EndpointId,
    /// Wire-level API model name.
    pub api_name: String,
    /// Optional stable human-facing name supplied by configuration or registry.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Protocol used to communicate with this model.
    pub protocol: Protocol,
    /// Capabilities of this model.
    pub capabilities: Capabilities,
    /// Model context/output token limits.
    pub limits: ModelLimits,
    /// Pricing rates for this model.
    pub pricing: Option<Pricing>,
    /// Prompt-cache compatibility settings for this model/endpoint.
    #[serde(default)]
    pub cache: CacheCompatibility,
}

/// Multimodal data structure.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Media {
    /// Image content.
    Image(ImageMedia),
    /// Audio content.
    Audio(AudioMedia),
}

impl Media {
    /// Creates an image media from a URL.
    pub fn image_url(url: url::Url, media_type: Option<mime::Mime>) -> Self {
        Self::Image(ImageMedia {
            source: ImageSource::Url(url),
            media_type,
            detail: None,
        })
    }

    /// Creates an image media from inline bytes.
    pub fn image_bytes(data: bytes::Bytes, media_type: mime::Mime) -> Self {
        Self::Image(ImageMedia {
            source: ImageSource::Inline(data),
            media_type: Some(media_type),
            detail: None,
        })
    }

    /// Creates an audio media from inline bytes (input).
    pub fn audio_bytes(data: bytes::Bytes, format: AudioFormat) -> Self {
        Self::Audio(AudioMedia {
            payload: AudioPayload::Inline(data),
            format,
            transcript: None,
        })
    }

    /// Creates an audio media from a provider reference (reuse).
    pub fn audio_ref(reference: ProviderMediaRef, format: AudioFormat) -> Self {
        Self::Audio(AudioMedia {
            payload: AudioPayload::ProviderRef(reference),
            format,
            transcript: None,
        })
    }
}

/// Image media container.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageMedia {
    /// Source of the image.
    pub source: ImageSource,
    /// MIME type of the image.
    #[serde(default, with = "optional_mime")]
    pub media_type: Option<mime::Mime>,
    /// Quality/detail hint.
    pub detail: Option<ImageDetail>,
}

/// Source of an image.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ImageSource {
    /// Publicly accessible URL.
    Url(url::Url),
    /// Inline binary data.
    Inline(#[serde(with = "base64_bytes")] bytes::Bytes),
    /// Replayed reference to provider-hosted media.
    ProviderRef(ProviderMediaRef),
}

/// Detail hints for image processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageDetail {
    /// Automatic selection based on image size.
    Auto,
    /// Process at low resolution.
    Low,
    /// Process at high resolution.
    High,
}

/// Audio media container.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioMedia {
    /// Payload containing the audio data or reference.
    pub payload: AudioPayload,
    /// Audio coding format.
    pub format: AudioFormat,
    /// Optional transcription text. Exists only on audio.
    pub transcript: Option<String>,
}

/// Audio payload variants.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioPayload {
    /// Inline raw audio bytes.
    Inline(#[serde(with = "base64_bytes")] bytes::Bytes),
    /// Opaque reference to provider-hosted audio.
    ProviderRef(ProviderMediaRef),
    /// Completed audio output containing both bytes and a reusable reference.
    InlineWithProviderRef {
        /// Audio binary data.
        #[serde(with = "base64_bytes")]
        data: bytes::Bytes,
        /// Reusable provider-hosted reference.
        reference: ProviderMediaRef,
    },
}

/// Supported audio formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    /// Waveform Audio File Format.
    Wav,
    /// Advanced Audio Coding.
    Aac,
    /// MPEG-1 Audio Layer III.
    Mp3,
    /// Free Lossless Audio Codec.
    Flac,
    /// Opus codec.
    Opus,
    /// Raw 16-bit linear PCM.
    Pcm16,
}

/// Reference to a file/media resource stored on the provider's servers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderMediaRef {
    /// Protocol used to create this reference.
    pub protocol: Protocol,
    /// Provider-specific file/media identifier.
    pub id: String,
    /// Expiration time of the reference, if applicable.
    pub expires_at: Option<std::time::SystemTime>,
}

/// A conversation turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message {
    /// Turn by the user (input).
    User(UserMessage),
    /// Turn by the assistant (output).
    Assistant(AssistantMessage),
}

/// Turn authored by the user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserMessage {
    /// Content blocks comprising this turn.
    pub content: Vec<UserPart>,
}

/// Turn authored by the assistant.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Content blocks comprising this turn.
    pub content: Vec<AssistantPart>,
    /// The model that produced this message.
    pub model: ModelId,
    /// The protocol used to communicate with the model.
    pub protocol: Protocol,
}

/// Part of a user message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UserPart {
    /// Plain text string.
    Text(String),
    /// Multimodal media object (image/audio).
    Media(Media),
    /// Outcome of a tool execution.
    ToolResult(ToolResult),
}

/// Part of an assistant message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AssistantPart {
    /// Plain text string.
    Text(String),
    /// Intermediate reasoning text and state.
    Reasoning(ReasoningPart),
    /// Request to execute a tool.
    ToolCall(ToolCall),
    /// Generated output media (e.g. spoken audio).
    Media(Media),
}

/// Execution outcome of a tool call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    /// Matching tool call identifier.
    pub tool_call_id: ToolCallId,
    /// Output data blocks.
    pub content: Vec<ToolResultPart>,
    /// Whether the tool execution resulted in a terminal error.
    pub is_error: bool,
}

/// Part of a tool result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolResultPart {
    /// Plain text outcome.
    Text(String),
    /// Multimodal media generated by the tool.
    Media(Media),
}

/// Call to a tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique call identifier.
    pub id: ToolCallId,
    /// Name of the tool to invoke.
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments_json: String,
}

impl ToolCall {
    /// Parses the raw JSON arguments as a JSON object, returning a decode error if not an object.
    pub fn arguments_value(&self) -> Result<serde_json::Value, DecodeError> {
        crate::json_repair::parse_json_value(&self.arguments_json).and_then(|value| {
            if value.is_object() {
                Ok(value)
            } else {
                Err(DecodeError::Json(
                    "Arguments must be a JSON object".to_string(),
                ))
            }
        })
    }
}

/// Reasoning component of an assistant's output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningPart {
    /// Human-visible summary or step-by-step reasoning.
    pub text: Option<String>,
    /// Opaque continuation metadata for replaying context.
    pub state: Option<ReasoningState>,
}

/// Opaque reasoning continuation state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningState {
    /// Producing protocol.
    pub protocol: Protocol,
    /// Model identifier.
    pub model: ModelId,
    /// Protocol-specific continuation variant.
    pub kind: ReasoningStateKind,
}

/// Protocol-specific reasoning metadata shapes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReasoningStateKind {
    /// Anthropic `thinking` signature.
    AnthropicSignature {
        /// Opaque signature value.
        signature: String,
    },
    /// Anthropic `redacted_thinking` block.
    AnthropicRedacted {
        /// Opaque redacted data.
        data: String,
    },
    /// OpenAI Responses reasoning continuation.
    OpenAiReasoning {
        /// Opaque item ID.
        item_id: Option<String>,
        /// Opaque encrypted reasoning block.
        encrypted_content: Option<String>,
    },
}

/// Inline media bytes serialize as base64 strings. serde_json renders raw
/// bytes as a number array (~3-4x the payload size), which bloats session
/// files and anything else that serializes messages. Deserialization also
/// accepts the legacy number-array form so older session files stay readable.
mod base64_bytes {
    use base64::prelude::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(data: &bytes::Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64_STANDARD.encode(data))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<bytes::Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = bytes::Bytes;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a base64 string or a byte array")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                BASE64_STANDARD
                    .decode(value)
                    .map(bytes::Bytes::from)
                    .map_err(serde::de::Error::custom)
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(bytes::Bytes::copy_from_slice(value))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut data = Vec::with_capacity(seq.size_hint().unwrap_or_default());
                while let Some(byte) = seq.next_element::<u8>()? {
                    data.push(byte);
                }
                Ok(bytes::Bytes::from(data))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

mod optional_mime {
    use super::*;
    use serde::{Deserializer, Serializer};
    use std::str::FromStr;

    pub fn serialize<S>(mime: &Option<mime::Mime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match mime {
            Some(ref m) => serializer.serialize_some(m.as_ref()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<mime::Mime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt_str: Option<String> = Option::deserialize(deserializer)?;
        match opt_str {
            Some(s) => mime::Mime::from_str(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

/// Provider-independent request envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    /// Optional developer/system prompt.
    pub system: Option<String>,
    /// Message history leading up to this turn.
    pub messages: Vec<Message>,
    /// Tool definitions available to the model.
    pub tools: Vec<ToolDef>,
    /// Tool calling constraint settings.
    pub tool_choice: ToolChoice,
    /// Optional limit on the number of generated tokens.
    pub max_output_tokens: Option<u64>,
    /// Optional temperature parameter.
    pub temperature: Option<f32>,
    /// Custom stop sequences.
    pub stop: Vec<String>,
    /// Reasoning effort or budget configuration.
    pub reasoning: ReasoningConfig,
    /// Requested formatting for model response (text or JSON).
    #[serde(default)]
    pub output_format: OutputFormat,
    /// Requested output modalities.
    pub output_modalities: OutputModalities,
    /// Compatibility mode for handling unsupported features.
    #[serde(default)]
    pub compatibility: CompatibilityMode,
    /// Prompt-cache retention preference; defaults to pi-compatible short retention.
    #[serde(default)]
    pub cache_retention: CacheRetention,
    /// Stable session identifier used for provider cache affinity.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Tool definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDef {
    /// Name of the tool.
    pub name: String,
    /// Description of what the tool does.
    pub description: String,
    /// JSON schema describing expected parameters.
    pub parameters: serde_json::Value,
}

/// Tool invocation constraint settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model decide whether to call a tool.
    #[default]
    Auto,
    /// Force the model to call at least one tool.
    Required,
    /// Prevent the model from calling any tools.
    None,
    /// Force the model to call the specified tool.
    Named(String),
}

/// Reasoning settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ReasoningConfig {
    /// Reasoning capability turned off.
    #[default]
    Off,
    /// Control reasoning via high-level effort presets.
    Effort(ReasoningEffort),
    /// Control reasoning via explicit token budget.
    Budget(u64),
}

/// High-level reasoning effort presets.
///
/// Variant declaration order is the semantic ordering (`Minimal` < … < `Max`);
/// the derived `Ord` is used to clamp a requested effort down to a model's
/// highest supported tier.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort (between `High` and `Max`).
    Xhigh,
    /// Maximum reasoning effort. On effort-controlled backends this selects the
    /// strongest tier the provider exposes through the ordinary effort
    /// parameter (e.g. engaging server-side subagents on `gpt-5.6-sol`).
    Max,
}

/// Requested output modalities.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "options", rename_all = "snake_case")]
pub enum OutputModalities {
    /// Text output only.
    #[default]
    Text,
    /// Text and audio output with options.
    TextAndAudio(AudioOutputOptions),
}

/// Audio generation configuration options.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioOutputOptions {
    /// Format of the output audio file.
    pub format: AudioFormat,
    /// Voice selection parameters.
    pub voice: AudioVoice,
}

/// Voice settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum AudioVoice {
    /// Standard named voice.
    Named(String),
    /// Opaque custom voice reference.
    ProviderRef(String),
}

/// Requested formatting of the model output.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "options", rename_all = "snake_case")]
pub enum OutputFormat {
    /// Unconstrained text output.
    #[default]
    Text,
    /// Output a JSON object (unconstrained schema).
    JsonObject,
    /// Output adhering strictly to the provided JSON Schema.
    JsonSchema(JsonSchemaFormat),
}

/// Format configuration for JSON Schema enforcement.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonSchemaFormat {
    /// Schema identifier (1-64 ASCII letters, digits, `_` or `-`).
    pub name: String,
    /// Optional schema description.
    pub description: Option<String>,
    /// JSON Schema object.
    pub schema: serde_json::Value,
    /// Whether schema adherence is strictly enforced.
    pub strict: bool,
}

/// Token billing counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens billed at standard rate (excluding hits).
    pub input_tokens: u64,
    /// Prompt tokens read from context cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to context cache.
    pub cache_write_tokens: u64,
    /// Prompt tokens written to cache with 1h TTL (subset of cache_write_tokens).
    pub cache_write_1h_tokens: u64,
    /// Generated output tokens.
    pub output_tokens: u64,
    /// Output tokens consumed for reasoning (subset of output_tokens).
    pub reasoning_tokens: u64,
    /// Total tokens processed.
    pub total_tokens: u64,
}

/// Successful generation outcome.
#[derive(Clone, Debug)]
pub struct Response {
    /// Generated assistant message.
    pub message: AssistantMessage,
    /// Termination reason for output generation.
    pub stop_reason: StopReason,
    /// Token billing counters.
    pub usage: Usage,
    /// Calculated cost of the request, if pricing was configured.
    pub cost: Option<crate::pricing::Cost>,
    /// Provider-assigned response identifier.
    pub response_id: Option<String>,
    /// Lossy mode diagnostics. Empty in Strict mode.
    pub diagnostics: Vec<crate::error::Diagnostic>,
}

/// Termination reason for output generation.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Natural completion (stop token / sequence).
    EndTurn,
    /// Token budget / maximum limit exceeded.
    MaxTokens,
    /// Model requested tool execution.
    ToolUse,
    /// Reached a custom stop sequence.
    StopSequence,
    /// Model output blocked/refused.
    Refusal,
    /// Claude-style turn pause.
    PauseTurn,
    /// Other custom/unknown reason.
    Other(String),
}

impl serde::Serialize for StopReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            StopReason::EndTurn => serializer.serialize_str("end_turn"),
            StopReason::MaxTokens => serializer.serialize_str("max_tokens"),
            StopReason::ToolUse => serializer.serialize_str("tool_use"),
            StopReason::StopSequence => serializer.serialize_str("stop_sequence"),
            StopReason::Refusal => serializer.serialize_str("refusal"),
            StopReason::PauseTurn => serializer.serialize_str("pause_turn"),
            StopReason::Other(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> serde::Deserialize<'de> for StopReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "end_turn" | "stop" => Ok(StopReason::EndTurn),
            "max_tokens" | "length" => Ok(StopReason::MaxTokens),
            "tool_use" | "tool_calls" => Ok(StopReason::ToolUse),
            "stop_sequence" => Ok(StopReason::StopSequence),
            "refusal" | "content_filter" => Ok(StopReason::Refusal),
            "pause_turn" => Ok(StopReason::PauseTurn),
            _ => Ok(StopReason::Other(s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::TokenRate;
    use std::time::SystemTime;

    #[test]
    fn inline_media_bytes_serialize_as_base64_strings() {
        let payload = bytes::Bytes::from(vec![137u8, 80, 78, 71, 13, 10]);
        let image = Media::image_bytes(payload.clone(), mime::IMAGE_PNG);
        let json = serde_json::to_string(&image).unwrap();
        assert!(
            json.contains("\"iVBORw0K\""),
            "inline image bytes must serialize as a base64 string, got: {json}"
        );

        let audio = Media::audio_bytes(payload.clone(), AudioFormat::Wav);
        let json = serde_json::to_string(&audio).unwrap();
        assert!(
            json.contains("\"iVBORw0K\""),
            "inline audio bytes must serialize as a base64 string, got: {json}"
        );

        let both = AudioPayload::InlineWithProviderRef {
            data: payload,
            reference: ProviderMediaRef {
                protocol: Protocol::OpenAiResponses,
                id: "ref".into(),
                expires_at: None,
            },
        };
        let json = serde_json::to_string(&both).unwrap();
        assert!(
            json.contains("\"iVBORw0K\""),
            "inline-with-ref bytes must serialize as a base64 string, got: {json}"
        );
    }

    #[test]
    fn inline_media_base64_round_trips() {
        let payload = bytes::Bytes::from(vec![0u8, 255, 128, 7]);
        let image = Media::image_bytes(payload.clone(), mime::IMAGE_PNG);
        let json = serde_json::to_string(&image).unwrap();
        let back: Media = serde_json::from_str(&json).unwrap();
        match back {
            Media::Image(image) => match image.source {
                ImageSource::Inline(data) => assert_eq!(data, payload),
                other => panic!("expected inline source, got {other:?}"),
            },
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn inline_media_accepts_legacy_number_array_form() {
        // Sessions written before the base64 representation stored inline
        // bytes as serde_json's default number array. They must stay readable.
        let legacy = r#"{"Image":{"source":{"Inline":[137,80,78,71]},"media_type":"image/png","detail":null}}"#;
        let media: Media = serde_json::from_str(legacy).unwrap();
        match media {
            Media::Image(image) => match image.source {
                ImageSource::Inline(data) => {
                    assert_eq!(data, bytes::Bytes::from(vec![137u8, 80, 78, 71]))
                }
                other => panic!("expected inline source, got {other:?}"),
            },
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn inline_media_json_overhead_is_base64_sized() {
        let payload = bytes::Bytes::from(vec![42u8; 100 * 1024]);
        let image = Media::image_bytes(payload, mime::IMAGE_PNG);
        let json = serde_json::to_vec(&image).unwrap();
        // base64 is ~1.34x the raw size; the old number-array form was ~4x.
        assert!(
            json.len() < 100 * 1024 * 3 / 2,
            "serialized inline image is {} bytes for 102400 raw bytes",
            json.len()
        );
    }

    #[test]
    fn test_modality_set_algebra() {
        let empty = ModalitySet::none();
        assert!(!empty.contains(Modality::Image));
        assert!(!empty.contains(Modality::Audio));

        let with_image = empty.with(Modality::Image);
        assert!(with_image.contains(Modality::Image));
        assert!(!with_image.contains(Modality::Audio));

        let with_both = with_image.with(Modality::Audio);
        assert!(with_both.contains(Modality::Image));
        assert!(with_both.contains(Modality::Audio));
    }

    #[test]
    fn test_model_spec_serde_round_trip() {
        let spec = ModelSpec {
            id: ModelId("test-model".to_string()),
            endpoint: EndpointId("test-endpoint".to_string()),
            api_name: "gpt-4o-mini".to_string(),
            display_name: None,
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none().with(Modality::Image),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: Some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: false,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::Standard,
                    min_effort: ReasoningEffort::Minimal,
                    max_effort: ReasoningEffort::High,
                }),
                structured_output: true,
            },
            limits: ModelLimits {
                context_window: 128000,
                max_output_tokens: 4096,
            },
            pricing: Some(Pricing {
                input: TokenRate(15),
                output: TokenRate(60),
                cache_read: TokenRate(7),
                cache_write_5m: TokenRate(15),
                cache_write_1h: None,
                reasoning: None,
                tiers: vec![],
            }),
            cache: CacheCompatibility::default(),
        };

        let serialized = serde_json::to_string(&spec).unwrap();
        let deserialized: ModelSpec = serde_json::from_str(&serialized).unwrap();
        assert_eq!(spec.id, deserialized.id);
        assert_eq!(spec.protocol, deserialized.protocol);
        assert_eq!(
            spec.capabilities.reasoning.unwrap().control,
            ReasoningControl::Effort
        );
    }

    #[test]
    fn test_message_serde_round_trip() {
        let now = SystemTime::now();

        // 1. User message with text, inline image, and URL image
        let user_msg = Message::User(UserMessage {
            content: vec![
                UserPart::Text("Hello".to_string()),
                UserPart::Media(Media::image_bytes(
                    bytes::Bytes::from("fake_png"),
                    "image/png".parse().unwrap(),
                )),
                UserPart::Media(Media::image_url(
                    url::Url::parse("https://example.com/img.jpg").unwrap(),
                    None,
                )),
            ],
        });

        let serialized = serde_json::to_string(&user_msg).unwrap();
        let _deserialized: Message = serde_json::from_str(&serialized).unwrap();

        // 2. User message with AudioPayload variants
        let audio_inline = Message::User(UserMessage {
            content: vec![UserPart::Media(Media::audio_bytes(
                bytes::Bytes::from("fake_wav"),
                AudioFormat::Wav,
            ))],
        });
        let serialized = serde_json::to_string(&audio_inline).unwrap();
        let _deserialized: Message = serde_json::from_str(&serialized).unwrap();

        let ref_msg = Message::User(UserMessage {
            content: vec![UserPart::Media(Media::audio_ref(
                ProviderMediaRef {
                    protocol: Protocol::OpenAiChat,
                    id: "ref_123".to_string(),
                    expires_at: Some(now),
                },
                AudioFormat::Mp3,
            ))],
        });
        let serialized = serde_json::to_string(&ref_msg).unwrap();
        let _deserialized: Message = serde_json::from_str(&serialized).unwrap();

        // 3. Assistant message with completed audio + transcript (InlineWithProviderRef)
        let assistant_msg = Message::Assistant(AssistantMessage {
            content: vec![
                AssistantPart::Text("Here is your speech".to_string()),
                AssistantPart::Media(Media::Audio(AudioMedia {
                    payload: AudioPayload::InlineWithProviderRef {
                        data: bytes::Bytes::from("speech_bytes"),
                        reference: ProviderMediaRef {
                            protocol: Protocol::OpenAiChat,
                            id: "audio_id_456".to_string(),
                            expires_at: Some(now),
                        },
                    },
                    format: AudioFormat::Wav,
                    transcript: Some("Here is your speech".to_string()),
                })),
            ],
            model: ModelId("gpt-4o-audio".to_string()),
            protocol: Protocol::OpenAiChat,
        });
        let serialized = serde_json::to_string(&assistant_msg).unwrap();
        let deserialized: Message = serde_json::from_str(&serialized).unwrap();
        if let Message::Assistant(msg) = deserialized {
            assert_eq!(msg.model, ModelId("gpt-4o-audio".to_string()));
            assert_eq!(msg.protocol, Protocol::OpenAiChat);
        } else {
            panic!("Expected assistant message");
        }

        // 4. Assistant message with reasoning state variants
        let reasoning_msg = Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Reasoning(ReasoningPart {
                text: Some("Let's think...".to_string()),
                state: Some(ReasoningState {
                    protocol: Protocol::AnthropicMessages,
                    model: ModelId("claude-3-5".to_string()),
                    kind: ReasoningStateKind::AnthropicSignature {
                        signature: "sig_abc".to_string(),
                    },
                }),
            })],
            model: ModelId("claude-3-5".to_string()),
            protocol: Protocol::AnthropicMessages,
        });
        let serialized = serde_json::to_string(&reasoning_msg).unwrap();
        let _deserialized: Message = serde_json::from_str(&serialized).unwrap();
    }

    #[test]
    fn test_tool_call_arguments_value() {
        let tc = ToolCall {
            id: ToolCallId("call_1".to_string()),
            name: "grep".to_string(),
            arguments_json: r#"{"pattern": "test"}"#.to_string(),
        };
        let parsed = tc.arguments_value().unwrap();
        assert_eq!(parsed["pattern"], "test");

        let tc_invalid = ToolCall {
            id: ToolCallId("call_2".to_string()),
            name: "grep".to_string(),
            arguments_json: r#""just a string""#.to_string(),
        };
        assert!(tc_invalid.arguments_value().is_err());
    }

    #[test]
    fn test_request_serde_round_trip() {
        let req = Request {
            system: Some("sys".to_string()),
            messages: vec![],
            tools: vec![ToolDef {
                name: "tool".to_string(),
                description: "desc".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(10),
            temperature: Some(0.7),
            stop: vec!["\n".to_string()],
            reasoning: ReasoningConfig::Off,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: crate::types::CacheRetention::Short,
            session_id: None,
        };
        let serialized = serde_json::to_string(&req).unwrap();
        let deserialized: Request = serde_json::from_str(&serialized).unwrap();
        assert_eq!(req.system, deserialized.system);
        assert_eq!(req.stop, deserialized.stop);
    }

    #[test]
    fn test_usage_default() {
        let usage = Usage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.cache_write_1h_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn test_stop_reason_custom_serde() {
        let stop = StopReason::EndTurn;
        let ser = serde_json::to_string(&stop).unwrap();
        assert_eq!(ser, "\"end_turn\"");

        let de: StopReason = serde_json::from_str("\"stop\"").unwrap();
        assert_eq!(de, StopReason::EndTurn);

        let de_other: StopReason = serde_json::from_str("\"something_else\"").unwrap();
        assert_eq!(de_other, StopReason::Other("something_else".to_string()));
    }
}
