#![allow(missing_docs)]

//! Opt-in smoke test for a real OpenAI-compatible multimodal endpoint.
//!
//! Required environment variables:
//! - `YGG_LIVE_BASE_URL` (normally ending in `/v1/`)
//! - `YGG_LIVE_API_KEY`
//! - `YGG_LIVE_MODEL`
//! - optional `YGG_LIVE_HEADERS_JSON`, an object of additional headers

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ygg_agent::{Agent, AgentConfig, ExtensionHost, InputPart, SandboxConfig, Session, UserInput};
use ygg_ai::{
    AiClient, Auth, CacheCompatibility, CacheRetention, Capabilities, Endpoint, EndpointId,
    EndpointTransport, Media, Modality, ModalitySet, Model, ModelId, ModelLimits, ModelSpec,
    Protocol, ReasoningConfig,
};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required for this ignored live test"))
}

#[tokio::test]
#[ignore = "requires an explicitly configured live multimodal endpoint"]
async fn live_openai_compatible_inline_png_reaches_the_model() {
    let base_url = required("YGG_LIVE_BASE_URL");
    let api_key = required("YGG_LIVE_API_KEY");
    let api_name = required("YGG_LIVE_MODEL");
    let mut headers = http::HeaderMap::new();
    if let Ok(raw) = std::env::var("YGG_LIVE_HEADERS_JSON") {
        let extra: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&raw).expect("YGG_LIVE_HEADERS_JSON must be a JSON object");
        for (name, value) in extra {
            headers.insert(
                http::HeaderName::try_from(name).expect("valid live header name"),
                http::HeaderValue::try_from(
                    value.as_str().expect("live header values are strings"),
                )
                .expect("valid live header value"),
            );
        }
    }

    let model = Model {
        spec: Arc::new(ModelSpec {
            id: ModelId(format!("live/{api_name}")),
            endpoint: EndpointId("live-openai".into()),
            api_name,
            display_name: None,
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none().with(Modality::Image),
                output_modalities: ModalitySet::none(),
                tools: false,
                parallel_tool_calls: false,
                reasoning: None,
                structured_output: false,
            },
            limits: ModelLimits {
                context_window: 131_072,
                max_output_tokens: 1_024,
            },
            pricing: None,
            cache: CacheCompatibility::default(),
        }),
        endpoint: Arc::new(Endpoint {
            id: EndpointId("live-openai".into()),
            base_url: url::Url::parse(&base_url).expect("valid live base URL"),
            auth: Auth::bearer(api_key),
            default_headers: headers,
            transport: EndpointTransport::Http,
            timeout: Duration::from_secs(120),
        }),
    };

    // Valid 1x1 PNG. Sending bytes (rather than a URL) verifies Ygg's inline
    // media persistence and OpenAI data-URL encoding on the real wire path.
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let session_path = directory.path().join("live-vision.jsonl");
    let mut agent = Agent::new(AgentConfig {
        client: AiClient::new(),
        model,
        session: Session::create(&session_path).unwrap(),
        system: "Answer the user's image question briefly.".into(),
        sandbox: SandboxConfig::new(directory.path()),
        extensions: ExtensionHost::new(),
        max_turns: Some(2),
        reasoning: ReasoningConfig::Off,
        cache_retention: CacheRetention::None,
        session_id: None,
    })
    .unwrap();

    let output = agent
        .complete(UserInput::from(vec![
            InputPart::Text(
                "This is a one-pixel test image. Reply with exactly VISION_OK if you can inspect the image input."
                    .into(),
            ),
            InputPart::Media(Media::image_bytes(
                bytes::Bytes::from(png),
                "image/png".parse().unwrap(),
            )),
        ]))
        .await
        .unwrap();

    assert!(
        output.text.contains("VISION_OK"),
        "live model did not acknowledge image input: {:?}",
        output.text
    );
    assert_eq!(agent.session().checkpoints().len(), 1);
    drop(agent);

    let reopened = Session::open(&session_path).unwrap();
    assert!(matches!(
        &reopened.context().unwrap()[0],
        ygg_ai::Message::User(user)
            if user.content.iter().any(|part| matches!(part, ygg_ai::UserPart::Media(_)))
    ));
}
