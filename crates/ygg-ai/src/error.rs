//! Error taxonomy and diagnostic types.

use serde::{Deserialize, Serialize};

/// Provider-observed progress at the moment a provider stream fails.
///
/// These are the raw wire counts the client actually observed: provider
/// frames before protocol decoding, canonical events after decoding, and the
/// retained content bytes. They are diagnostic only and never affect retry
/// policy; they exist so that a mid-stream failure ("the response died after
/// 90 seconds") can be distinguished from a request that never produced
/// anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamProgress {
    /// Raw provider frames observed before protocol decoding.
    pub provider_events: usize,
    /// Canonical stream events decoded before the failure.
    pub decoded_events: usize,
    /// Content bytes retained in canonical parts at the time of failure.
    pub content_bytes: usize,
    /// Bytes held in compatibility buffers at the time of failure.
    pub buffered_bytes: usize,
    /// Whether at least one response-body byte was observed.
    pub first_body_seen: bool,
    /// Elapsed milliseconds from stream start until the failure.
    pub elapsed_ms: u64,
    /// Elapsed milliseconds from stream start until the last provider event.
    /// `None` means the stream failed before any provider event arrived.
    pub last_event_ms: Option<u64>,
}

/// Main error type for all ygg-ai operations.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// Configuration error.
    #[error("Config error: {0}")]
    Config(#[from] ConfigError),
    /// Authentication or credential resolution error.
    #[error("Auth error: {0}")]
    Auth(#[from] AuthError),
    /// Input validation error.
    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),
    /// Request uses a capability unsupported by the model/protocol.
    #[error("Unsupported error: {0}")]
    Unsupported(#[from] UnsupportedError),
    /// Non-2xx HTTP response from provider.
    #[error("HTTP error: {0}")]
    Http(#[from] HttpError),
    /// Transport/connection/network error.
    #[error("{0}")]
    Transport(#[from] TransportError),
    /// Provider error frame inside a 2xx stream.
    #[error("Provider error: {0}")]
    Provider(#[from] ProviderError),
    /// JSON/UTF-8 decoding error.
    #[error("Decode error: {0}")]
    Decode(#[from] DecodeError),
    /// Pricing calculation error.
    #[error("Pricing error: {0}")]
    Pricing(#[from] PricingError),
    /// Stream protocol state machine violation.
    #[error("Stream protocol error: {0}")]
    StreamProtocol(#[from] StreamProtocolError),
    /// A failure mid-provider-stream, annotated with how far the response
    /// had progressed before it died.
    #[error("{inner}")]
    StreamFailure {
        /// The underlying failure that ended the stream.
        inner: Box<AiError>,
        /// Provider-observed progress at the time of the failure.
        progress: StreamProgress,
    },
    /// The request was canceled by dropping the stream.
    #[error("Request canceled")]
    Canceled,
}

/// HTTP non-2xx error.
#[derive(Debug, thiserror::Error)]
#[error("HTTP {status} (request_id: {request_id:?}): {body_snippet:?}")]
pub struct HttpError {
    /// HTTP status code.
    pub status: http::StatusCode,
    /// Provider-supplied request identifier.
    pub request_id: Option<String>,
    /// Retry-after duration if supplied by the provider.
    pub retry_after: Option<std::time::Duration>,
    /// Provider-specific error code.
    pub provider_code: Option<String>,
    /// Truncated snippet of response body. Never contains sensitive headers/creds.
    pub body_snippet: Option<String>,
    /// Hint indicating if the request is safe to retry (pre-first-byte).
    pub retryable: bool,
}

impl HttpError {
    /// Checks if the error is safe to retry.
    pub fn is_safe_to_retry(&self) -> bool {
        self.retryable
            && matches!(
                self.status,
                // These statuses indicate that the request did not produce a
                // usable model response and are routinely transient at API
                // gateways. The agent only invokes this before any generated
                // bytes, so replaying the POST is safe there.
                http::StatusCode::REQUEST_TIMEOUT
                    | http::StatusCode::TOO_MANY_REQUESTS
                    | http::StatusCode::BAD_GATEWAY
                    | http::StatusCode::SERVICE_UNAVAILABLE
                    | http::StatusCode::GATEWAY_TIMEOUT
            )
    }
}

/// Network/connection error.
#[derive(Debug, thiserror::Error)]
#[error("Transport error in {phase:?} (timeout: {timeout}): {message}")]
pub struct TransportError {
    /// The phase during which the failure occurred.
    pub phase: TransportPhase,
    /// Whether the error was a timeout.
    pub timeout: bool,
    /// Sanitized error message. Never contains credentials/URL userinfo.
    pub message: String,
}

/// Phases of request execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportPhase {
    /// DNS, TCP, or TLS connection establishment before a request is accepted.
    Connect,
    /// Sending the request or waiting for response headers. The provider may
    /// already have accepted the request, so replay is not inherently safe.
    ResponseHeaders,
    /// Streaming/reading the response body.
    Body,
}

/// Provider-native error payload from a successful stream/response body.
#[derive(Debug, thiserror::Error)]
#[error("Provider error (code: {code:?}, kind: {kind:?}): {message}")]
pub struct ProviderError {
    /// Provider error code.
    pub code: Option<String>,
    /// Provider error category/type.
    pub kind: Option<String>,
    /// Error message.
    pub message: String,
    /// Request identifier.
    pub request_id: Option<String>,
}

/// Stream protocol state machine violations.
#[non_exhaustive]
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamProtocolError {
    /// Missing `Started` event.
    #[error("Missing Started event")]
    MissingStart,
    /// Duplicate `Started` event.
    #[error("Duplicate Started event")]
    DuplicateStart,
    /// Missing `Finished` event at stream end.
    #[error("Missing Finished event")]
    MissingFinish,
    /// Event received after the `Finished` event.
    #[error("Event received after Finished event")]
    EventAfterFinish,
    /// Stream part was not balanced (e.g. missing end event).
    #[error("Unbalanced part at index {index}")]
    UnbalancedPart {
        /// Canonical part index.
        index: usize,
    },
    /// Stream closed prematurely.
    #[error("Premature end-of-file")]
    PrematureEof,
    /// Unexpected event type received.
    #[error("Unexpected event: {0}")]
    UnexpectedEvent(String),
}

/// Description of unsupported capabilities.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnsupportedError {
    /// Images are not supported by the model or protocol.
    #[error("Image input is unsupported")]
    Image,
    /// Audio input is not supported.
    #[error("Audio input is unsupported")]
    Audio,
    /// Audio output is not supported.
    #[error("Audio output is unsupported")]
    AudioOutput,
    /// Tools are not supported.
    #[error("Tools are unsupported")]
    Tools,
    /// Specific tool choice configuration is not supported.
    #[error("Tool choice configuration is unsupported")]
    ToolChoice,
    /// Reasoning is not supported.
    #[error("Reasoning is unsupported")]
    Reasoning,
    /// The requested reasoning execution mode is not available on this route.
    #[error("Reasoning mode is unsupported")]
    ReasoningMode,
    /// Reasoning continuation state protocol mismatch.
    #[error("Reasoning state mismatch: have {have:?}, want {want:?}")]
    ReasoningStateMismatch {
        /// The protocol of the reasoning state.
        have: crate::types::Protocol,
        /// The target protocol.
        want: crate::types::Protocol,
    },
    /// Provider media references are not supported.
    #[error("Provider media references are unsupported")]
    ProviderMediaRef,
    /// Media within tool results is not supported.
    #[error("Media in tool results is unsupported")]
    ToolResultMedia,
    /// Structured output formats are not supported.
    #[error("Structured output format is unsupported")]
    StructuredOutput,
    /// Custom stop sequences are not supported by the target protocol.
    #[error("Stop sequences are unsupported")]
    StopSequences,
    /// Responses continuation/replay options are not supported by other
    /// protocols.
    #[error("Responses request options are unsupported")]
    ResponsesOptions,
}

/// Configuration loading or resolution error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Configuration parsing failure.
    #[error("Parse error: {0}")]
    Parse(String),
    /// Duplicate endpoint registration.
    #[error("Duplicate endpoint registered: {0:?}")]
    DuplicateEndpoint(crate::types::EndpointId),
    /// Duplicate model registration.
    #[error("Duplicate model registered: {0:?}")]
    DuplicateModel(crate::types::ModelId),
    /// Model references an unknown endpoint.
    #[error("Unknown endpoint referenced: {0:?}")]
    UnknownEndpoint(crate::types::EndpointId),
    /// Model resolution failure.
    #[error("Unknown model: {0:?}")]
    UnknownModel(crate::types::ModelId),
    /// Environment variable not set.
    #[error("Missing environment variable: {0}")]
    MissingEnv(String),
    /// Environment variable contained a value that is not valid Unicode.
    #[error("Invalid environment variable: {0}")]
    InvalidEnv(String),
    /// Environment variable value exceeded the accepted byte limit.
    #[error("Environment variable {var} exceeds the {max_bytes}-byte limit")]
    EnvironmentValueTooLarge {
        /// Name of the environment variable.
        var: String,
        /// Maximum accepted value size in bytes.
        max_bytes: usize,
    },
    /// Credential resolver is missing for dynamic auth.
    #[error("Missing credential resolver: {0}")]
    MissingCredentialResolver(String),
    /// Ordinary header collides with the auth header name.
    #[error("Authentication header collision: {0}")]
    AuthHeaderCollision(http::HeaderName),
    /// Invalid header name or value.
    #[error("Invalid header format: {0}")]
    InvalidHeader(String),
    /// Base URL violates the absolute trailing-slash constraint.
    #[error("Invalid base URL: {0}")]
    InvalidBaseUrl(String),
    /// Reasoning configuration is invalid for this model.
    #[error("Invalid reasoning configuration for model {0:?}")]
    InvalidReasoningConfig(crate::types::ModelId),
    /// Pricing configuration is invalid.
    #[error("Invalid pricing configuration for model {0:?}")]
    InvalidPricing(crate::types::ModelId),
    /// Model limits or names are invalid.
    #[error("Invalid model configuration for model {0:?}")]
    InvalidModel(crate::types::ModelId),
    /// Endpoint timeout is zero.
    #[error("Invalid timeout for endpoint {0:?}")]
    InvalidTimeout(crate::types::EndpointId),
}

impl ConfigError {
    /// Helper constructor to create a Parse config error.
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }
}

/// Authentication and credential resolution failures.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// A dynamic resolver failed. Details are intentionally not retained because
    /// third-party resolver errors may contain credentials.
    #[error("Credential resolution failed")]
    Resolve,
    /// An environment-backed credential was not available.
    #[error("Credential environment variable is missing: {0}")]
    MissingEnvironment(String),
    /// An environment-backed credential exceeded the accepted byte limit.
    #[error("Credential environment variable {var} exceeds the {max_bytes}-byte limit")]
    EnvironmentValueTooLarge {
        /// Name of the environment variable.
        var: String,
        /// Maximum accepted value size in bytes.
        max_bytes: usize,
    },
    /// A credential could not be represented as an HTTP header value.
    #[error("Credential contains invalid HTTP header bytes")]
    InvalidHeaderValue,
    /// Handshake rejected credentials.
    #[error("Invalid credentials supplied")]
    InvalidCredential,
}

/// Request/conversation semantic validation failures.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    /// Tool result was sent with no matching tool call.
    #[error("Orphan tool result for call ID {0:?}")]
    OrphanToolResult(crate::types::ToolCallId),
    /// Required tool call has no matching result.
    #[error("Missing tool result for call ID {0:?}")]
    MissingToolResult(crate::types::ToolCallId),
    /// Tool arguments do not parse as a JSON object.
    #[error("Tool arguments are not a JSON object for call ID {0:?}")]
    ToolArgumentsNotObject(crate::types::ToolCallId),
    /// Tool schema definition is malformed.
    #[error("Invalid tool schema: {0}")]
    InvalidToolSchema(String),
    /// Output schema definition is malformed.
    #[error("Invalid output schema: {0}")]
    InvalidOutputSchema(String),
    /// Structured output format name is malformed.
    #[error("Invalid output format name: {0}")]
    InvalidOutputFormatName(String),
    /// Audio output voice name/reference is empty.
    #[error("Audio output voice must not be empty")]
    InvalidAudioVoice,
    /// Requested output token limit exceeds model capability.
    #[error("Invalid max output tokens requested: {requested}, model max: {model_max}")]
    InvalidMaxOutputTokens {
        /// Requested token budget.
        requested: u64,
        /// Model spec limit.
        model_max: u64,
    },
    /// Requested temperature is invalid.
    #[error("Temperature must be finite and between 0.0 and 2.0")]
    InvalidTemperature,
    /// Requested reasoning budget is out of range.
    #[error("Reasoning budget is out of range")]
    ReasoningBudgetOutOfRange,
}

/// JSON/UTF-8 decoding errors.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// JSON parsing failure.
    #[error("JSON decode error: {0}")]
    Json(String),
    /// String is not valid UTF-8.
    #[error("Invalid UTF-8 sequence")]
    InvalidUtf8,
    /// Base64 decoding failure.
    #[error("Invalid Base64 sequence")]
    InvalidBase64,
    /// Tool arguments exceeded MAX_TOOL_ARGUMENT_BYTES.
    #[error("Tool arguments too large")]
    ToolArgumentsTooLarge,
    /// Aggregate streamed assistant content exceeded its hard byte limit.
    #[error("Streamed response content is too large")]
    ResponseTooLarge,
    /// One response emitted too many stream events.
    #[error("Streamed response emitted too many events")]
    TooManyStreamEvents,
    /// One response emitted too many independently indexed content parts.
    #[error("Streamed response emitted too many content parts")]
    TooManyResponseParts,
    /// Body exceeded MAX_COMPLETED_BODY_BYTES.
    #[error("Body too large")]
    BodyTooLarge,
    /// Counter subtraction underflowed during token normalisation.
    #[error("Usage underflow in token counting")]
    UsageUnderflow,
    /// Mandatory provider field is invalid or missing.
    #[error("Invalid provider field: {0}")]
    InvalidProviderField(String),
}

/// Pricing/cost calculation errors.
#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    /// Overflow during cost summation/multiplication.
    #[error("Pricing arithmetic overflow")]
    ArithmeticOverflow,
    /// Contradictory token buckets.
    #[error("Invalid usage buckets for pricing")]
    InvalidUsageBuckets,
}

/// Lossy-mode diagnostic info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Diagnostic code.
    pub code: String,
    /// Detailed diagnostic message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    #[test]
    fn test_error_display_is_secret_free() {
        let err = AiError::Auth(AuthError::Resolve);
        let display = format!("{}", err);
        assert!(display.contains("Credential resolution failed"));
        assert!(!display.contains("Bearer "));

        let debug = format!("{:?}", err);
        assert!(debug.contains("Resolve"));
    }

    #[test]
    fn test_environment_limit_errors_do_not_expose_values() {
        let sample = "oversized-secret-value";
        let config = ConfigError::EnvironmentValueTooLarge {
            var: "YGG_API_KEY".to_owned(),
            max_bytes: 4096,
        };
        let auth = AuthError::EnvironmentValueTooLarge {
            var: "YGG_API_KEY".to_owned(),
            max_bytes: 4096,
        };
        assert_eq!(
            config.to_string(),
            "Environment variable YGG_API_KEY exceeds the 4096-byte limit"
        );
        assert_eq!(
            auth.to_string(),
            "Credential environment variable YGG_API_KEY exceeds the 4096-byte limit"
        );
        assert!(!config.to_string().contains(sample));
        assert!(!auth.to_string().contains(sample));
    }

    #[test]
    fn test_transport_error_preserves_phase() {
        let err = AiError::Transport(TransportError {
            phase: TransportPhase::ResponseHeaders,
            timeout: true,
            message: "Sanitized error message".to_string(),
        });
        let AiError::Transport(transport) = &err else {
            unreachable!()
        };
        assert_eq!(transport.phase, TransportPhase::ResponseHeaders);
        assert!(transport.timeout);
        let display = err.to_string();
        assert_eq!(display.matches("Transport error").count(), 1, "{display}");
        assert!(!display.contains("http://secret-url.com"));
    }

    #[test]
    fn test_http_error_no_secret_headers() {
        let err = HttpError {
            status: StatusCode::UNAUTHORIZED,
            request_id: Some("req_123".to_string()),
            retry_after: None,
            provider_code: None,
            body_snippet: Some("Access denied".to_string()),
            retryable: false,
        };
        // Verify we don't have header fields in the struct
        let display = format!("{}", err);
        assert!(display.contains("401"));
        assert!(!display.contains("Authorization"));
    }

    #[test]
    fn test_is_safe_to_retry() {
        let err_429 = HttpError {
            status: StatusCode::TOO_MANY_REQUESTS,
            request_id: None,
            retry_after: None,
            provider_code: None,
            body_snippet: None,
            retryable: true,
        };
        assert!(err_429.is_safe_to_retry());

        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::BAD_GATEWAY,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let transient = HttpError {
                status,
                request_id: None,
                retry_after: None,
                provider_code: None,
                body_snippet: None,
                retryable: true,
            };
            assert!(transient.is_safe_to_retry(), "{status} should be retryable");
        }

        let err_500 = HttpError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            request_id: None,
            retry_after: None,
            provider_code: None,
            body_snippet: None,
            retryable: true,
        };
        assert!(!err_500.is_safe_to_retry());
    }

    #[test]
    fn test_stream_failure_display_delegates_to_inner() {
        let err = AiError::StreamFailure {
            inner: Box::new(AiError::Decode(DecodeError::Json(
                "unterminated string at line 1 column 20".into(),
            ))),
            progress: StreamProgress {
                provider_events: 412,
                decoded_events: 38,
                content_bytes: 18204,
                buffered_bytes: 96,
                first_body_seen: true,
                elapsed_ms: 97321,
                last_event_ms: Some(97_000),
            },
        };
        // Display delegates to the inner error so existing log lines stay
        // stable; the progress data travels in the struct, not the text.
        assert_eq!(
            err.to_string(),
            "Decode error: JSON decode error: unterminated string at line 1 column 20"
        );
    }

    #[test]
    fn test_stream_progress_serde_roundtrip() {
        let progress = StreamProgress {
            provider_events: 7,
            decoded_events: 3,
            content_bytes: 100,
            buffered_bytes: 4,
            first_body_seen: true,
            elapsed_ms: 1200,
            last_event_ms: Some(1_100),
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: StreamProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(progress, back);
    }
}
