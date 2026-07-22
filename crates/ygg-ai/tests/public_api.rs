#![allow(missing_docs)]

// Verify that all public API re-exports are accessible and compile.
#[allow(unused_imports)]
use ygg_ai::{
    AiClient, AiError, AssistantMessage, AssistantPart, AudioFormat, AudioMedia,
    AudioOutputOptions, AudioPayload, AudioVoice, Auth, AuthConfig, AuthError, Capabilities,
    CatalogConfig, CompatibilityMode, ConfigError, Cost, CredentialResolver,
    CredentialResolverRegistry, CredentialScheme, DecodeError, Diagnostic, Endpoint,
    EndpointConfig, EndpointId, HttpError, ImageDetail, ImageMedia, ImageSource, JsonSchemaFormat,
    Media, Message, Mime, Modality, ModalitySet, Model, ModelCatalog, ModelConfig, ModelId,
    ModelLimits, ModelSpec, OutputFormat, OutputModalities, Pricing, PricingError, PricingTier,
    Protocol, ProviderError, ProviderMediaRef, ReasoningCapability, ReasoningConfig,
    ReasoningControl, ReasoningEffort, ReasoningEffortBudgets, ReasoningPart, ReasoningState,
    ReasoningStateKind, Request, ResolvedCredential, Response, ResponseStream, Secret, StopReason,
    StreamEvent, StreamProtocolError, TokenRate, ToolCall, ToolCallId, ToolChoice, ToolDef,
    ToolResult, ToolResultPart, TransportError, TransportPhase, UnsupportedError, Usage,
    UserMessage, UserPart, ValidationError,
};

// A compile-time proof that every public re-export above is nameable. Referencing
// `ReasoningStateKind` here also guards against it silently dropping out of the
// public surface (previously absent from this test).
const _: fn() = || {
    fn assert_exported<T>() {}
    assert_exported::<ReasoningStateKind>();
    assert_exported::<ReasoningState>();
};

#[test]
fn test_public_api_secret_redaction_proof() {
    let secret = Secret::from("my-super-secret-key-12345");

    // Debug and Display must redact
    let debug_str = format!("{:?}", secret);
    let display_str = format!("{}", secret);

    assert!(!debug_str.contains("my-super-secret-key-12345"));
    assert!(!display_str.contains("my-super-secret-key-12345"));

    assert!(debug_str.contains("<redacted>"));
    assert!(display_str.contains("<redacted>"));

    // Auth enum containing secrets must redact in Debug
    let auth = Auth::bearer("my-super-secret-key-12345");
    let auth_debug = format!("{:?}", auth);
    assert!(!auth_debug.contains("my-super-secret-key-12345"));
    assert!(auth_debug.contains("<redacted>"));

    // Endpoint containing secrets must redact in Debug
    let ep = Endpoint {
        id: EndpointId("ep-1".to_string()),
        base_url: url::Url::parse("https://api.openai.com/").unwrap(),
        auth,
        default_headers: http::HeaderMap::new(),
        transport: ygg_ai::EndpointTransport::Http,
        timeout: std::time::Duration::from_secs(30),
    };
    let ep_debug = format!("{:?}", ep);
    assert!(!ep_debug.contains("my-super-secret-key-12345"));
}
