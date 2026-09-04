use super::*;

#[test]
fn discovered_reasoning_supports_chat_and_responses_models() {
    assert!(discovered_model_supports_reasoning(
        Protocol::OpenAiChat,
        "gemma-4-31b-it"
    ));
    assert!(discovered_model_supports_reasoning(
        Protocol::OpenAiResponses,
        "gpt-5.4"
    ));
    assert!(!discovered_model_supports_reasoning(
        Protocol::OpenAiChat,
        "gemma-3-27b-it"
    ));
    assert!(!discovered_model_supports_reasoning(
        Protocol::AnthropicMessages,
        "claude-sonnet-4"
    ));
}

#[test]
fn azure_openai_configuration_routes_deployments_through_versioned_responses_base() {
    let declaration = BUILTIN_PROVIDER_DECLARATIONS
        .iter()
        .find(|declaration| declaration.id == "azure-openai")
        .expect("Azure OpenAI declaration");
    let (base_url, deployment) = azure_openai_configuration_from_values(
        declaration,
        None,
        Some("enterprise-resource"),
        None,
        Some("production-gpt"),
    )
    .unwrap()
    .expect("Azure configuration");

    assert_eq!(deployment, "production-gpt");
    assert_eq!(
        base_url.as_str(),
        "https://enterprise-resource.openai.azure.com/openai/?api-version=2025-04-01-preview"
    );
}

#[test]
fn azure_openai_configuration_rejects_credential_bearing_endpoint_urls() {
    let declaration = BUILTIN_PROVIDER_DECLARATIONS
        .iter()
        .find(|declaration| declaration.id == "azure-openai")
        .expect("Azure OpenAI declaration");
    let error = azure_openai_configuration_from_values(
        declaration,
        Some("https://example.invalid/?api-key=secret"),
        None,
        None,
        Some("deployment"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid AZURE_OPENAI_ENDPOINT"));
}

#[test]
fn aws_runtime_registration_is_scheduled_without_a_static_environment_marker() {
    let declaration = BUILTIN_PROVIDER_DECLARATIONS
        .iter()
        .find(|declaration| declaration.id == "bedrock")
        .expect("Bedrock declaration");
    assert!(declaration_is_configured(declaration).unwrap());
}

#[test]
fn codex_compaction_respects_model_window_and_allows_smaller_caps() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), None);
    let catalog = base_model_catalog(true).unwrap();
    let mut model = catalog
        .resolve(&ModelId("gpt-4o-mini".to_owned()))
        .unwrap()
        .clone();
    Arc::make_mut(&mut model.endpoint).id = EndpointId(crate::auth::codex::ENDPOINT_ID.into());
    Arc::make_mut(&mut model.spec).limits.context_window = 872_000;

    // No route default: the full provider-advertised window is available.
    assert_eq!(
        effective_compaction_threshold_fraction(&config, &model),
        1.0
    );

    config.compaction.threshold_fraction = 0.25;
    assert_eq!(
        effective_compaction_threshold_fraction(&config, &model),
        0.25
    );

    config.compaction.threshold_fraction = 1.0;
    config.compaction.max_active_tokens = Some(200_000);
    assert_eq!(
        effective_compaction_threshold_fraction(&config, &model),
        200_000.0 / 872_000.0
    );

    config.compaction.max_active_tokens = Some(900_000);
    assert_eq!(
        effective_compaction_threshold_fraction(&config, &model),
        1.0
    );

    config.compaction.max_active_tokens = Some(0);
    assert_eq!(
        effective_compaction_threshold_fraction(&config, &model),
        1.0
    );

    config.compaction.max_active_tokens = None;
    Arc::make_mut(&mut model.endpoint).id = EndpointId("openai".into());
    assert_eq!(
        effective_compaction_threshold_fraction(&config, &model),
        1.0
    );
}

#[test]
fn custom_endpoint_startup_timeout_is_cold_start_safe_and_configurable() {
    assert_eq!(
        resolve_custom_startup_timeout(None, None).unwrap(),
        Duration::from_secs(15 * 60)
    );
    assert_eq!(
        resolve_custom_startup_timeout(Some(420), None).unwrap(),
        Duration::from_secs(420)
    );
    assert_eq!(
        resolve_custom_startup_timeout(Some(420), Some(" 600 ")).unwrap(),
        Duration::from_secs(600)
    );
    assert!(resolve_custom_startup_timeout(None, Some("0")).is_err());
    assert!(resolve_custom_startup_timeout(None, Some("not-a-number")).is_err());
}

#[test]
fn embedded_builtin_endpoints_use_provider_response_header_timeout() {
    let catalog = base_model_catalog(true).unwrap();
    for model_id in ["gpt-4o-mini", "claude-sonnet-4-6"] {
        let model = catalog.resolve(&ModelId(model_id.to_owned())).unwrap();
        assert_eq!(
            model.endpoint.timeout, PROVIDER_RESPONSE_HEADER_TIMEOUT,
            "{model_id} retained a stale embedded response-header timeout"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_clients_do_not_follow_authenticated_redirects() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let origin = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/sink", destination.uri())),
        )
        .mount(&origin)
        .await;

    let blocking_url = format!("{}/models", origin.uri());
    let blocking_status = tokio::task::spawn_blocking(move || {
        blocking_discovery_client(Duration::from_secs(2))
            .unwrap()
            .get(blocking_url)
            .header("x-api-key", "blocking-secret")
            .send()
            .unwrap()
            .status()
    })
    .await
    .unwrap();
    assert_eq!(blocking_status, reqwest::StatusCode::FOUND);

    let async_status = discovery_client(Duration::from_secs(2))
        .unwrap()
        .get(format!("{}/models", origin.uri()))
        .header("x-api-key", "async-secret")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(async_status, reqwest::StatusCode::FOUND);
    assert!(destination.received_requests().await.unwrap().is_empty());
}

use crate::config::{CompactionPolicy, Mode, ResumeSelector, SandboxPolicy};

fn config(directory: &std::path::Path, model: Option<&str>) -> Config {
    Config {
        workspace: directory.to_path_buf(),
        invocation_cwd: directory.to_path_buf(),
        model: model.map(|model| ModelId(model.to_owned())),
        model_explicit: model.is_some(),
        reasoning: ReasoningConfig::Off,
        reasoning_explicit: false,
        reasoning_mode: ygg_ai::ReasoningMode::Standard,
        reasoning_mode_explicit: false,
        cache_retention: ygg_ai::CacheRetention::Short,
        effect_policy: ygg_agent::EffectPolicy::Controlled,
        sandbox: SandboxPolicy::default(),
        theme: None,
        system_prompt: None,
        theme_paths: vec![],
        color: crate::config::ColorMode::Auto,
        plain: false,
        show_images: false,
        session_dir: directory.join("sessions"),
        compaction: CompactionPolicy::default(),
        max_cost_microdollars: None,
        cost_warning_microdollars: None,
        max_turns: Some(40),
        show_reasoning_in_print: false,
        initial_prompt: None,
        prompt_template: None,
        debug_prompt: false,
        prompt_paths: vec![],
        mode: Mode::Print {
            prompt: "hi".to_owned(),
        },
        resume: ResumeSelector::New,
        mouse: crate::config::MouseMode::Auto,
        skill_paths: vec![],
        extension_paths: vec![],
        enabled_extensions: vec![],
        extension_activation_overridden: false,
        trusted_extensions: vec![],
        invocation_trusted_extensions: vec![],
        experimental_streamable_http_mcp: false,
        extension_flag_values: Default::default(),
        tools: crate::config::ToolPolicy::default(),
        telemetry: None,
        context_files: true,
        offline: true,
        workspace_trusted: true,
    }
}

fn configured_test_extensions(_skills: Arc<dyn SkillRegistry>, config: &Config) -> ExtensionHost {
    let boot = bootstrap(config.clone()).unwrap();
    let model_id = config.model.as_ref().expect("test model");
    let model = boot.catalog.resolve(model_id).unwrap();
    let session = Session::create(config.workspace.join("tool-policy-test.jsonl")).unwrap();
    configured_extensions(config, &session, &model, &config.reasoning, &boot.sessions)
        .unwrap()
        .0
}

fn append_active_skill(session: &mut Session, id: &str, required_tools: &[&str]) {
    session
        .append(EntryValue::SkillActivated {
            descriptor: ygg_agent::SkillDescriptor {
                id: id.into(),
                name: id.into(),
                description: "test active skill".into(),
                license: None,
                compatibility: None,
                metadata: Default::default(),
                allowed_tools: vec![],
                disable_model_invocation: false,
                version: None,
                source: ygg_agent::SkillSource::BuiltIn,
                trust: ygg_agent::SkillTrust::BuiltIn,
                required_tools: required_tools
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
                tags: vec![],
            },
            instructions_hash: "test-hash".into(),
            instructions: "test instructions".into(),
        })
        .unwrap();
}

#[test]
fn configured_compaction_model_is_resolved_into_the_agent() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), Some("gpt-4o-mini"));
    config.compaction.compact_model = Some(ModelId("gpt-4o-mini".into()));
    let boot = bootstrap(config).unwrap();
    let app = build_app(
        boot,
        LaunchSelection {
            model: ModelId("gpt-4o-mini".into()),
            session: SessionSelection::CreateNew(directory.path().join("session.jsonl")),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
        },
        "system".into(),
    )
    .unwrap();
    assert_eq!(
        app.agent
            .compaction_model()
            .map(|model| model.spec.id.0.as_str()),
        Some("gpt-4o-mini")
    );
}

#[test]
fn native_compaction_rejects_non_responses_and_route_mismatch() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path(), Some("gpt-4o-mini"));
    let boot = bootstrap(config.clone()).unwrap();
    let chat = boot
        .catalog
        .resolve(config.model.as_ref().unwrap())
        .unwrap();
    let error =
        validate_compaction_route(CompactionMode::NativeResponses, &chat, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires an OpenAI Responses route"),
        "{error}"
    );

    let mut responses_spec = (*chat.spec).clone();
    responses_spec.protocol = Protocol::OpenAiResponses;
    let responses = Model {
        spec: Arc::new(responses_spec),
        endpoint: chat.endpoint.clone(),
    };
    validate_compaction_route(
        CompactionMode::NativeResponses,
        &responses,
        Some(&responses),
    )
    .unwrap();

    let mut other_spec = (*responses.spec).clone();
    other_spec.id = ModelId("other-responses-model".into());
    let other = Model {
        spec: Arc::new(other_spec),
        endpoint: responses.endpoint.clone(),
    };
    let error =
        validate_compaction_route(CompactionMode::NativeResponses, &responses, Some(&other))
            .unwrap_err();
    assert!(
        error.to_string().contains("exact route affinity"),
        "{error}"
    );
}

#[test]
fn model_resolution_has_cli_project_global_precedence() {
    let id = |value: &str| Some(ModelId(value.into()));
    assert_eq!(
        resolve_model_id(id("cli"), id("project"), id("global")),
        id("cli")
    );
    assert_eq!(
        resolve_model_id(None, id("project"), id("global")),
        id("project")
    );
    assert_eq!(resolve_model_id(None, None, id("global")), id("global"));
    assert_eq!(resolve_model_id(None, None, None), None);
}

#[test]
fn opencode_discovery_infers_supported_protocols_and_skips_gemini() {
    let preset = &crate::providers::OPENCODE;
    let binding = |model_id| {
        discovered_preset_binding(preset, model_id).map(|route| (route.endpoint_id, route.protocol))
    };
    assert_eq!(
        binding("gpt-future"),
        Some(("opencode", Protocol::OpenAiResponses))
    );
    assert_eq!(
        binding("claude-future"),
        Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages))
    );
    assert_eq!(
        binding("qwen3.7-plus"),
        Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages))
    );
    assert_eq!(
        binding("qwen3.7-instruct"),
        Some(("opencode", Protocol::OpenAiChat))
    );
    assert_eq!(binding("gemini-future"), None);
    assert_eq!(
        binding("kimi-future"),
        Some(("opencode", Protocol::OpenAiChat))
    );
}

#[test]
fn openai_discovery_skips_the_rejected_gpt_5_6_alias() {
    let preset = &crate::providers::OPENAI;
    assert_eq!(discovered_preset_binding(preset, "gpt-5.6"), None);
    assert_eq!(
        discovered_preset_binding(preset, "gpt-5.6-sol")
            .map(|route| (route.endpoint_id, route.protocol)),
        Some(("openai", Protocol::OpenAiResponses))
    );
}

#[test]
fn metadata_sparse_multimodal_model_ids_get_a_vision_fallback() {
    let response = serde_json::json!({
        "data": [{
            "id": "Intel/Qwen3.6-27B-int4-AutoRound",
            "max_model_len": 131_072
        }]
    });
    let models = api_models_from_response(&response).unwrap();
    assert_eq!(models.len(), 1);
    assert!(models[0].vision);
    assert!(model_id_implies_vision("gemini-2.5-pro"));
    assert!(model_id_implies_vision("anthropic/claude-sonnet-4-6"));
    assert!(model_id_implies_vision("deepseek-v4-flash-vision-exp"));
    assert!(!model_id_implies_vision("deepseek-v4-flash"));
    assert!(model_id_implies_vision("Qwen/Qwen2.5-VL-7B"));
    assert!(!model_id_implies_vision("Qwen/Qwen3-Coder-30B"));
}

#[test]
fn deepseek_v4_discovery_uses_documented_limits_when_inventory_is_sparse() {
    let response = serde_json::json!({
        "data": [
            {"id": "deepseek-v4-flash"},
            {"id": "deepseek-v4-flash-vision-exp"},
            {"id": "deepseek-v3"}
        ]
    });
    let models = api_models_from_response(&response).unwrap();
    assert!(models
        .iter()
        .find(|model| model.id == "deepseek-v4-flash-vision-exp")
        .is_some_and(|model| model.vision));
    let limits = models
        .iter()
        .map(|model| (model.id.as_str(), deepseek_discovered_limits(model)))
        .collect::<std::collections::BTreeMap<_, _>>();

    assert_eq!(
        limits["deepseek-v4-flash"],
        (
            DEEPSEEK_DEFAULT_CONTEXT_WINDOW,
            DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS
        )
    );
    assert_eq!(
        limits["deepseek-v4-flash-vision-exp"],
        (
            DEEPSEEK_DEFAULT_CONTEXT_WINDOW,
            DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS
        )
    );
    assert_eq!(limits["deepseek-v3"], (128_000, 64_000));
}

#[test]
fn model_inventory_normalizes_flattened_audio_modalities() {
    let response = serde_json::json!({
        "data": [{
            "id": "audio-model",
            "input_modalities": ["text", "audio"]
        }]
    });
    let models = api_models_from_response(&response).unwrap();
    assert_eq!(models.len(), 1);
    assert!(!models[0].vision);
    assert!(models[0].audio);
}

#[test]
fn custom_model_inventory_defaults_sparse_metadata_to_tool_capable() {
    let response = serde_json::json!({
        "data": [
            {"id": "unknown"},
            {"id": "parameters", "supported_parameters": ["tools"]},
            {"id": "empty-parameters", "supported_parameters": []},
            {
                "id": "capability-object",
                "capabilities": {"tool_calling": {"supported": true}}
            },
            {
                "id": "provider-metadata",
                "provider": {"capabilities": {"function_calling": true}}
            },
            {
                "id": "explicitly-disabled",
                "supports_tools": false,
                "supported_parameters": ["tools"]
            }
        ]
    });
    let models = api_models_from_response(&response).unwrap();
    let tools = models
        .iter()
        .map(|model| (model.id.as_str(), model.tools))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert!(tools["unknown"]);
    assert!(tools["parameters"]);
    assert!(!tools["empty-parameters"]);
    assert!(tools["capability-object"]);
    assert!(tools["provider-metadata"]);
    assert!(!tools["explicitly-disabled"]);
}

#[test]
fn configured_custom_model_metadata_overrides_discovered_sparse_inventory() {
    use crate::auth::custom::CustomModel;

    let configured = CustomModel {
        api_name: "system".into(),
        context_window: 4_096,
        max_output_tokens: 1_024,
        tools: true,
        reasoning: true,
        reasoning_configurable: false,
        ..Default::default()
    };

    let discovered_system = CustomModel {
        api_name: "system".into(),
        context_window: 262_144,
        max_output_tokens: 16_384,
        tools: true,
        ..Default::default()
    };

    let discovered_other = CustomModel {
        api_name: "other".into(),
        context_window: 8_192,
        ..Default::default()
    };

    let configured_missing = CustomModel {
        api_name: "configured-only".into(),
        context_window: 12_288,
        ..Default::default()
    };

    let merged = apply_configured_custom_model_overrides(
        vec![discovered_system, discovered_other],
        &[configured, configured_missing],
    );

    assert_eq!(merged[0].api_name, "system");
    assert_eq!(merged[0].context_window, 4_096);
    assert_eq!(merged[0].max_output_tokens, 1_024);
    assert!(merged[0].tools);
    assert!(merged[0].reasoning);
    assert!(!merged[0].reasoning_configurable);
    assert_eq!(
        custom_reasoning_capability(&merged[0]).unwrap().control,
        ReasoningControl::AlwaysOn
    );
    assert_eq!(merged[1].api_name, "other");
    assert_eq!(merged[1].context_window, 8_192);
    assert_eq!(merged[2].api_name, "configured-only");
    assert_eq!(merged[2].context_window, 12_288);
}

#[test]
fn apple_foundation_models_fill_sparse_inventory_from_embedded_metadata() {
    use crate::auth::custom::{CustomCredential, CustomModel};

    let cred = CustomCredential {
        base_url: APPLE_FM_BASE_URL.into(),
        api_key: String::new(),
        api_name: String::new(),
        headers: Vec::new(),
        models: Vec::new(),
        auto_discover: true,
    };
    let models = apply_known_custom_model_defaults(
        &cred,
        vec![
            CustomModel {
                api_name: "system".into(),
                context_window: 262_144,
                max_output_tokens: 16_384,
                reasoning: false,
                ..Default::default()
            },
            CustomModel {
                api_name: "pcc".into(),
                context_window: 262_144,
                max_output_tokens: 16_384,
                ..Default::default()
            },
        ],
    );

    assert_eq!(models[0].context_window, 8_192);
    assert_eq!(models[0].max_output_tokens, APPLE_FM_MAX_OUTPUT_TOKENS);
    assert!(models[0].tools);
    assert!(models[0].reasoning);
    assert!(!models[0].reasoning_configurable);
    assert_eq!(
        custom_reasoning_capability(&models[0]).unwrap().control,
        ReasoningControl::AlwaysOn
    );

    assert_eq!(models[1].context_window, 32_768);
    assert_eq!(models[1].max_output_tokens, APPLE_FM_MAX_OUTPUT_TOKENS);
    assert!(models[1].reasoning_configurable);
    assert_eq!(models[1].reasoning_values, ["low", "medium", "high"]);
    assert_eq!(models[1].reasoning_default, "medium");
    assert_eq!(
        custom_reasoning_capability(&models[1]).unwrap().control,
        ReasoningControl::Effort
    );
}

#[test]
fn configured_apple_metadata_overrides_embedded_defaults() {
    use crate::auth::custom::{CustomCredential, CustomModel};

    let cred = CustomCredential {
        base_url: APPLE_FM_BASE_URL.into(),
        api_key: String::new(),
        api_name: String::new(),
        headers: Vec::new(),
        models: Vec::new(),
        auto_discover: true,
    };
    let configured = CustomModel {
        api_name: "system".into(),
        context_window: 4_096,
        max_output_tokens: 512,
        tools: false,
        reasoning: false,
        ..Default::default()
    };
    let merged = apply_configured_custom_model_overrides(
        apply_known_custom_model_defaults(
            &cred,
            vec![CustomModel {
                api_name: "system".into(),
                ..Default::default()
            }],
        ),
        &[configured],
    );

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].context_window, 4_096);
    assert_eq!(merged[0].max_output_tokens, 512);
    assert!(!merged[0].tools);
    assert!(!merged[0].reasoning);
}

#[test]
fn apple_foundation_models_health_requires_the_native_server_shape() {
    assert!(apple_foundation_models_health_is_valid(
        &serde_json::json!({
            "status": "fm serve is running",
            "models": [{"name": "system", "available": true}]
        })
    ));
    assert!(!apple_foundation_models_health_is_valid(
        &serde_json::json!({
            "status": "ok",
            "models": [{"name": "system", "available": true}]
        })
    ));
    assert!(!apple_foundation_models_health_is_valid(
        &serde_json::json!({
            "status": "fm serve is running",
            "models": [{"name": "system", "available": false}]
        })
    ));
}

#[test]
fn apple_foundation_models_discovery_skips_an_absent_optional_server() {
    assert!(!custom_model_discovery_is_available(
        APPLE_FM_BASE_URL,
        || false
    ));
    assert!(custom_model_discovery_is_available(
        APPLE_FM_BASE_URL,
        || true
    ));
    assert!(custom_model_discovery_is_available(
        "http://127.0.0.1:8000/v1/",
        || panic!("non-Apple discovery must not probe Apple Foundation Models")
    ));
}

#[test]
fn custom_model_cache_fingerprint_changes_with_configured_metadata() {
    let credential = custom_credential_fingerprint("key", &http::HeaderMap::new());
    let empty = custom_model_cache_fingerprint(&credential, &[]);
    let configured = crate::auth::custom::CustomModel {
        api_name: "system".into(),
        context_window: APPLE_FM_SYSTEM_CONTEXT_WINDOW,
        max_output_tokens: APPLE_FM_MAX_OUTPUT_TOKENS,
        ..Default::default()
    };
    let with_override =
        custom_model_cache_fingerprint(&credential, std::slice::from_ref(&configured));
    let changed = custom_model_cache_fingerprint(
        &credential,
        &[crate::auth::custom::CustomModel {
            context_window: 4_096,
            ..configured
        }],
    );

    assert_ne!(empty, with_override);
    assert_ne!(with_override, changed);
}
#[test]
fn fixed_custom_reasoning_is_the_only_ygg_thinking_option() {
    use crate::auth::custom::CustomModel;

    let fixed = CustomModel {
        reasoning: true,
        reasoning_configurable: false,
        ..Default::default()
    };
    let capability = custom_reasoning_capability(&fixed).unwrap();
    assert_eq!(capability.control, ReasoningControl::AlwaysOn);

    let configurable = CustomModel {
        reasoning: true,
        ..Default::default()
    };
    assert!(custom_reasoning_capability(&configurable).is_some());
}

#[test]
fn openrouter_discovery_uses_live_ids_limits_and_capabilities() {
    let response = serde_json::json!({
        "data": [
            {
                "id": "zeta/model",
                "context_length": 64_000,
                "top_provider": { "max_completion_tokens": 8_000 },
                "architecture": { "input_modalities": ["text", "image", "audio"] },
                "supported_parameters": ["tools", "tool_choice", "reasoning", "reasoning.effort"],
                "pricing": {
                    "prompt": "0.00000015",
                    "completion": "0.00000060",
                    "input_cache_read": "0.000000075"
                }
            },
            {
                "id": "alpha/model",
                "context_length": 8_000,
                "top_provider": { "max_completion_tokens": 16_000 },
                "supported_parameters": []
            }
        ]
    });

    let models = openrouter_models_from_response(&crate::providers::OPENROUTER, &response).unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id.0, "openrouter/alpha/model");
    assert_eq!(models[1].id.0, "openrouter/zeta/model");
    assert_eq!(models[1].api_name, "zeta/model");
    assert_eq!(models[1].limits.context_window, 64_000);
    assert_eq!(models[1].limits.max_output_tokens, 8_000);
    assert!(models[1].capabilities.tools);
    assert!(models[1].capabilities.reasoning.is_some());
    assert_eq!(
        models[1]
            .capabilities
            .reasoning
            .as_ref()
            .unwrap()
            .openai_chat_mode,
        OpenAiChatReasoningMode::OpenRouter
    );
    let pricing = models[1].pricing.as_ref().expect("OpenRouter price");
    assert_eq!(pricing.input, TokenRate(150_000));
    assert_eq!(pricing.output, TokenRate(600_000));
    assert_eq!(pricing.cache_read, TokenRate(75_000));
    assert!(models[1]
        .capabilities
        .input_modalities
        .contains(ygg_ai::Modality::Image));
    assert!(models[1]
        .capabilities
        .input_modalities
        .contains(ygg_ai::Modality::Audio));
    // An advertised output limit cannot exceed the model context window.
    assert_eq!(models[0].limits.max_output_tokens, 8_000);
    assert!(!models[0].capabilities.tools);
}

#[test]
fn openrouter_discovery_requires_an_advertised_completion_ceiling() {
    let response = serde_json::json!({
        "data": [
            {
                "id": "missing/limit",
                "context_length": 64_000
            },
            {
                "id": "top-level/limit",
                "context_length": 64_000,
                "max_completion_tokens": 12_000
            }
        ]
    });

    let models = openrouter_models_from_response(&crate::providers::OPENROUTER, &response).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].api_name, "top-level/limit");
    assert_eq!(models[0].limits.max_output_tokens, 12_000);
}

#[test]
fn openrouter_anthropic_routes_enable_anthropic_cache_markers() {
    let response = serde_json::json!({
        "data": [{
            "id": "anthropic/claude-sonnet-4-5",
            "context_length": 200_000,
            "top_provider": { "max_completion_tokens": 8_192 }
        }]
    });
    let models = openrouter_models_from_response(&crate::providers::OPENROUTER, &response).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(
        models[0].cache.cache_control_format,
        Some(ygg_ai::CacheControlFormat::Anthropic)
    );
}

fn write_codex_credential(path: &std::path::Path, localhost: bool, plan: &str) {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let payload = serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_test",
            "chatgpt_plan_type": plan,
            "localhost": localhost
        }
    });
    let access = format!(
        "h.{}.s",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    );
    let bytes = serde_json::to_vec(&serde_json::json!({
        "tokens": {
            "access_token": access,
            "refresh_token": "refresh",
            "account_id": "acct_test"
        },
        "expires_at": u64::MAX
    }))
    .unwrap();
    ygg_agent::secure_fs::write_private_atomic(path, &bytes, 1024 * 1024).unwrap();
}

#[test]
fn codex_models_require_a_usable_credential_and_include_luna_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("codex.json");
    let store = crate::auth::codex::CredentialStore::new(&path);

    let mut catalog = base_model_catalog(true).unwrap();
    register_openai_codex(&mut catalog, store.clone(), false).unwrap();
    assert!(catalog.resolve(&ModelId("gpt-5.6-sol".into())).is_err());

    write_codex_credential(&path, true, "plus");
    let mut catalog = base_model_catalog(true).unwrap();
    let error = register_openai_codex(&mut catalog, store.clone(), false).unwrap_err();
    assert!(error.to_string().contains("localhost-only"));
    assert!(catalog.resolve(&ModelId("gpt-5.6-sol".into())).is_err());

    write_codex_credential(&path, false, "plus");
    let mut catalog = base_model_catalog(true).unwrap();
    register_openai_codex(&mut catalog, store, false).unwrap();
    for model_id in crate::auth::codex::MODELS {
        let model = catalog.resolve(&ModelId((*model_id).into())).unwrap();
        assert_eq!(model.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
        assert_eq!(model.spec.protocol, Protocol::OpenAiResponses);
        assert_eq!(model.spec.limits.context_window, 272_000);
        assert_eq!(model.spec.limits.max_output_tokens, 128_000);
        assert!(model.spec.pricing.is_some());
        assert!(!model.spec.cache.supports_long_retention);
        assert!(!model.spec.cache.send_session_id_header);
        assert_eq!(
            model.spec.cache.session_affinity_format,
            Some(ygg_ai::SessionAffinityFormat::Codex)
        );
        assert_eq!(
            model.endpoint.transport,
            ygg_ai::EndpointTransport::WebSocketPreferred
        );
        assert_eq!(
            model.endpoint.runtime.body_encoding,
            ygg_ai::RequestBodyEncoding::Zstd
        );
        assert_eq!(
            model.endpoint.runtime.responses_profile,
            ygg_ai::ResponsesRuntimeProfile::Codex
        );
    }
    let sol = catalog.resolve(&ModelId("gpt-5.6-sol".into())).unwrap();
    assert_eq!(crate::compaction::context_window(&sol), 272_000);

    // Pro is not in the fallback subscription catalog. Luna is included and
    // live account discovery can add or remove models independently of it.
    assert!(catalog.resolve(&ModelId("gpt-5.5-pro".into())).is_err());
    assert!(catalog.resolve(&ModelId("gpt-5.6-luna".into())).is_ok());
}

#[test]
fn offline_codex_registration_uses_cached_inventory_without_dynamic_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cached-codex.json");
    write_codex_credential(&path, false, "plus");
    let store = crate::auth::codex::CredentialStore::new(&path);
    let claims = crate::auth::codex::usable_subscription_claims(&store)
        .unwrap()
        .unwrap();
    let cached = CodexDiscovery {
        claims,
        models: codex_models_from_response(
            &serde_json::json!({
                "models": [{
                    "slug": "cached-account-model",
                    "context_window": 196_000,
                    "max_output_tokens": 24_000,
                    "use_responses_lite": true,
                    "multi_agent_version": "v2",
                    "supported_reasoning_levels": ["high", "ultra"]
                }]
            }),
            Some(&crate::auth::codex::ChatGptPlan::Plus),
        )
        .unwrap(),
    };
    save_codex_model_cache(&store, &cached).unwrap();

    let mut catalog = base_model_catalog(true).unwrap();
    register_openai_codex(&mut catalog, store, true).unwrap();
    let model = catalog
        .resolve(&ModelId("cached-account-model".into()))
        .unwrap();
    assert_eq!(model.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
    assert_eq!(model.spec.limits.context_window, 196_000);
    assert!(!model.spec.capabilities.responses_lite);
    assert_eq!(model.spec.capabilities.agent_delegation, None);
    assert_eq!(
        model
            .spec
            .capabilities
            .reasoning
            .as_ref()
            .unwrap()
            .max_effort,
        ygg_ai::ReasoningEffort::Max
    );

    let fallback_path = directory.path().join("fallback-codex.json");
    write_codex_credential(&fallback_path, false, "plus");
    let mut fallback_catalog = base_model_catalog(true).unwrap();
    register_openai_codex(
        &mut fallback_catalog,
        crate::auth::codex::CredentialStore::new(fallback_path),
        true,
    )
    .unwrap();
    let fallback = fallback_catalog
        .resolve(&ModelId("gpt-5.6-sol".into()))
        .unwrap();
    assert_eq!(fallback.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
    // GPT-5.6 uses OpenAI's published standard costs on the Codex route too.
    let luna = fallback_catalog
        .resolve(&ModelId("gpt-5.6-luna".into()))
        .unwrap();
    let luna_pricing = luna.spec.pricing.as_ref().expect("codex luna pricing");
    assert_eq!(luna_pricing.input, ygg_ai::TokenRate(200_000));
    assert_eq!(luna_pricing.output, ygg_ai::TokenRate(1_200_000));
    assert_eq!(luna_pricing.cache_read, ygg_ai::TokenRate(20_000));
    assert_eq!(luna_pricing.cache_write_5m, ygg_ai::TokenRate(250_000));
    assert_eq!(luna_pricing.tiers.len(), 1);
    assert_eq!(
        luna_pricing.tiers[0].input,
        Some(ygg_ai::TokenRate(400_000))
    );
}

#[test]
fn codex_pro_pricing_keeps_long_context_tiers() {
    for model_id in ["gpt-5.4-pro", "gpt-5.5-pro"] {
        let pricing = crate::providers::pricing_for(&crate::providers::CODEX, model_id)
            .expect("codex pro pricing");
        assert_eq!(pricing.input, ygg_ai::TokenRate(30_000_000));
        assert_eq!(pricing.output, ygg_ai::TokenRate(180_000_000));
        assert_eq!(pricing.tiers.len(), 1);
        let tier = &pricing.tiers[0];
        assert_eq!(tier.min_input_tokens, 272_001);
        assert_eq!(tier.input, Some(ygg_ai::TokenRate(60_000_000)));
        assert_eq!(tier.output, Some(ygg_ai::TokenRate(270_000_000)));
    }
}

#[test]
fn codex_fallback_never_infers_ultra_or_delegation_from_oauth_plan() {
    let directory = tempfile::tempdir().unwrap();
    for plan in ["pro", "plus"] {
        let path = directory.path().join(format!("{plan}-codex.json"));
        write_codex_credential(&path, false, plan);
        let mut catalog = base_model_catalog(true).unwrap();
        register_openai_codex(
            &mut catalog,
            crate::auth::codex::CredentialStore::new(path),
            true,
        )
        .unwrap();
        let model = catalog.resolve(&ModelId("gpt-5.6-sol".into())).unwrap();
        assert_ne!(
            model
                .spec
                .capabilities
                .reasoning
                .as_ref()
                .unwrap()
                .max_effort,
            ygg_ai::ReasoningEffort::Ultra
        );
        assert_eq!(model.spec.capabilities.agent_delegation, None);
        assert!(!model.spec.capabilities.responses_lite);
    }
}

#[test]
fn codex_spark_is_registered_as_image_capable() {
    assert!(codex_supports_image_input("gpt-5.3-codex-spark"));
    assert!(codex_supports_image_input("gpt-5.3-codex"));
    assert!(codex_supports_image_input("gpt-5.4-mini"));
    assert!(codex_supports_image_input("gpt-5.4-pro"));
    assert!(codex_supports_image_input("gpt-5.5"));
    assert!(codex_supports_image_input("gpt-5.5-pro"));
    assert!(codex_supports_image_input("gpt-5.6-sol"));
    assert!(codex_supports_image_input("gpt-5.6-luna"));
    assert!(codex_supports_image_input("gpt-5.1-codex"));
    assert!(codex_supports_image_input("gpt-5.1-codex-mini"));
    assert!(codex_supports_image_input("gpt-5.1-codex-max"));
    assert!(codex_supports_image_input("codex-mini-latest"));
    assert!(!codex_supports_image_input("gpt-5-codex"));
}

#[test]
fn codex_catalog_query_uses_the_implemented_schema_version() {
    let url = codex_models_url().unwrap();
    assert_eq!(url.path(), "/backend-api/codex/models");
    assert_eq!(
        url.query_pairs()
            .find(|(name, _)| name == "client_version")
            .map(|(_, value)| value.into_owned()),
        Some(CODEX_MODELS_CLIENT_VERSION.to_string())
    );
}

#[test]
fn codex_discovery_accepts_account_catalog_and_caps_live_context() {
    let body = serde_json::json!({
        "models": [
            {
                "slug": "gpt-5.6-luna",
                "context_window": 400_000,
                "max_output_tokens": 150_000,
                "use_responses_lite": true,
                "multi_agent_version": "v2",
                "supported_reasoning_levels": [
                    {"effort": "low"},
                    {"effort": "max"},
                    {"effort": "ultra"}
                ]
            },
            {"slug": "gpt-account-preview"},
            "gpt-string-preview",
            {"slug": "gpt-5.6-luna"}
        ]
    });
    let models = codex_models_from_response(&body, None).unwrap();
    assert_eq!(models.len(), 3, "duplicate slugs must be collapsed");
    let luna = models
        .iter()
        .find(|model| model.id == "gpt-5.6-luna")
        .unwrap();
    assert_eq!(luna.context_window, CODEX_CONTEXT_WINDOW_CAP);
    assert_eq!(luna.max_context_window, 400_000);
    assert_eq!(luna.max_output_tokens, 150_000);
    assert_eq!(luna.min_effort, ygg_ai::ReasoningEffort::Low);
    assert_eq!(luna.max_effort, ygg_ai::ReasoningEffort::Ultra);
    assert!(luna.responses_lite);
    assert_eq!(luna.agent_delegation, Some(ygg_ai::AgentDelegation::V2));
    assert_eq!(
        models
            .iter()
            .find(|model| model.id == "gpt-string-preview")
            .unwrap()
            .context_window,
        CODEX_LEGACY_CONTEXT_WINDOW
    );
}

#[test]
fn codex_discovery_never_exposes_ultra_without_complete_v2_metadata() {
    let models = codex_models_from_response(
        &serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-no-v2",
                    "supported_reasoning_levels": ["high", "ultra"]
                },
                {
                    "slug": "gpt-5.6-malformed-levels",
                    "supported_reasoning_levels": ["ultra", {"effort": 42}],
                    "use_responses_lite": "true",
                    "multi_agent_version": "v2"
                },
                {
                    "slug": "gpt-5.6-malformed-v2",
                    "supported_reasoning_levels": ["ultra"],
                    "multi_agent_version": 2
                }
            ]
        }),
        None,
    )
    .unwrap();

    let no_v2 = models
        .iter()
        .find(|model| model.id == "gpt-5.6-no-v2")
        .unwrap();
    assert_eq!(no_v2.max_effort, ygg_ai::ReasoningEffort::Max);
    assert_eq!(no_v2.agent_delegation, None);

    let malformed_levels = models
        .iter()
        .find(|model| model.id == "gpt-5.6-malformed-levels")
        .unwrap();
    assert_eq!(malformed_levels.max_effort, ygg_ai::ReasoningEffort::Max);
    assert!(!malformed_levels.responses_lite);
    assert_eq!(
        malformed_levels.agent_delegation,
        Some(ygg_ai::AgentDelegation::V2)
    );

    let malformed_v2 = models
        .iter()
        .find(|model| model.id == "gpt-5.6-malformed-v2")
        .unwrap();
    assert_eq!(malformed_v2.max_effort, ygg_ai::ReasoningEffort::Max);
    assert_eq!(malformed_v2.agent_delegation, None);
}

#[test]
fn codex_discovery_caps_default_and_max_plan_windows_at_272k() {
    let body = serde_json::json!({
        "models": [{
            "slug": "gpt-5.4",
            "context_window": 272_000,
            "max_context_window": 1_000_000
        }]
    });
    let plus = crate::auth::codex::ChatGptPlan::Plus;
    let pro = crate::auth::codex::ChatGptPlan::Pro;
    let pro_lite = crate::auth::codex::ChatGptPlan::ProLite;

    assert_eq!(
        codex_models_from_response(&body, Some(&plus)).unwrap()[0].context_window,
        272_000
    );
    assert_eq!(
        codex_models_from_response(&body, Some(&pro)).unwrap()[0].context_window,
        272_000
    );
    assert_eq!(
        codex_models_from_response(&body, Some(&pro_lite)).unwrap()[0].context_window,
        272_000
    );

    let smaller_body = serde_json::json!({
        "models": [{
            "slug": "gpt-small-window",
            "context_window": 128_000,
            "max_context_window": 200_000
        }]
    });
    assert_eq!(
        codex_models_from_response(&smaller_body, Some(&plus)).unwrap()[0].context_window,
        128_000
    );
    assert_eq!(
        codex_models_from_response(&smaller_body, Some(&pro)).unwrap()[0].context_window,
        200_000
    );
}

#[test]
fn codex_model_cache_is_scoped_to_account_and_plan() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::auth::codex::CredentialStore::new(directory.path().join("codex.json"));
    let plus = crate::auth::codex::ChatGptPlan::Plus;
    let claims = crate::auth::codex::SubscriptionClaims {
        account_id: "acct-a".into(),
        plan: Some(plus.clone()),
    };
    let body = serde_json::json!({
        "models": [{"slug": "gpt-5.6-sol", "context_window": 272_000}]
    });
    let discovery = CodexDiscovery {
        models: codex_models_from_response(&body, Some(&plus)).unwrap(),
        claims: claims.clone(),
    };
    save_codex_model_cache(&store, &discovery).unwrap();
    assert_eq!(
        load_codex_model_cache(&store, &claims).unwrap(),
        Some(discovery.models)
    );

    let upgraded = crate::auth::codex::SubscriptionClaims {
        account_id: "acct-a".into(),
        plan: Some(crate::auth::codex::ChatGptPlan::Pro),
    };
    assert!(load_codex_model_cache(&store, &upgraded).unwrap().is_none());
    let other_account = crate::auth::codex::SubscriptionClaims {
        account_id: "acct-b".into(),
        plan: Some(plus),
    };
    assert!(load_codex_model_cache(&store, &other_account)
        .unwrap()
        .is_none());
}

#[test]
fn codex_model_cache_fails_closed_when_stale_future_dated_or_incomplete() {
    let directory = tempfile::tempdir().unwrap();
    let plus = crate::auth::codex::ChatGptPlan::Plus;
    let claims = crate::auth::codex::SubscriptionClaims {
        account_id: "acct-a".into(),
        plan: Some(plus.clone()),
    };
    let models = codex_models_from_response(
        &serde_json::json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "context_window": 272_000,
                "max_output_tokens": 128_000,
                "supported_reasoning_levels": ["high", "ultra"],
                "use_responses_lite": true,
                "multi_agent_version": "v2"
            }]
        }),
        Some(&plus),
    )
    .unwrap();
    let valid = serde_json::to_vec(&CodexModelCache {
        version: CODEX_MODEL_CACHE_VERSION,
        account_id: claims.account_id.clone(),
        plan: codex_plan_cache_key(&claims).map(str::to_owned),
        models,
    })
    .unwrap();
    let cache_path = |credential_path: &std::path::Path| {
        let stem = credential_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap();
        credential_path.with_file_name(format!("{stem}-models.json"))
    };

    for (name, modified) in [
        (
            "stale",
            std::time::SystemTime::now()
                .checked_sub(CODEX_MODEL_CACHE_REFRESH_INTERVAL + Duration::from_secs(1))
                .unwrap(),
        ),
        (
            "future",
            std::time::SystemTime::now() + Duration::from_secs(60),
        ),
    ] {
        let credential_path = directory.path().join(format!("{name}.json"));
        let store = crate::auth::codex::CredentialStore::new(&credential_path);
        store.save_model_cache(&valid).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(cache_path(&credential_path))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(load_codex_model_cache(&store, &claims).unwrap().is_none());
    }

    let malformed_path = directory.path().join("malformed.json");
    let malformed = crate::auth::codex::CredentialStore::new(&malformed_path);
    malformed.save_model_cache(b"{").unwrap();
    assert!(load_codex_model_cache(&malformed, &claims).is_err());

    let valid_value: serde_json::Value = serde_json::from_slice(&valid).unwrap();

    let mut prior_schema = valid_value.clone();
    prior_schema["version"] = serde_json::json!(CODEX_MODEL_CACHE_VERSION - 1);
    let prior_schema_store =
        crate::auth::codex::CredentialStore::new(directory.path().join("prior-schema.json"));
    prior_schema_store
        .save_model_cache(&serde_json::to_vec(&prior_schema).unwrap())
        .unwrap();
    assert!(load_codex_model_cache(&prior_schema_store, &claims)
        .unwrap()
        .is_none());

    let mut cases = Vec::new();

    let mut missing_delegation = valid_value.clone();
    missing_delegation["models"][0]
        .as_object_mut()
        .unwrap()
        .remove("agent_delegation");
    cases.push(("missing-delegation", missing_delegation));

    let mut missing_responses_lite = valid_value.clone();
    missing_responses_lite["models"][0]
        .as_object_mut()
        .unwrap()
        .remove("responses_lite");
    cases.push(("missing-responses-lite", missing_responses_lite));

    let mut duplicate = valid_value.clone();
    let duplicate_model = duplicate["models"][0].clone();
    duplicate["models"]
        .as_array_mut()
        .unwrap()
        .push(duplicate_model);
    cases.push(("duplicate", duplicate));

    let mut empty_id = valid_value.clone();
    empty_id["models"][0]["id"] = serde_json::json!("");
    cases.push(("empty-id", empty_id));

    let mut inconsistent_limits = valid_value.clone();
    inconsistent_limits["models"][0]["max_output_tokens"] = serde_json::json!(300_000);
    cases.push(("inconsistent-limits", inconsistent_limits));

    let mut ultra_without_delegation = valid_value.clone();
    ultra_without_delegation["models"][0]["agent_delegation"] = serde_json::Value::Null;
    cases.push(("ultra-without-delegation", ultra_without_delegation));

    let mut invalid_effort_range = valid_value;
    invalid_effort_range["models"][0]["min_effort"] = serde_json::json!("ultra");
    invalid_effort_range["models"][0]["max_effort"] = serde_json::json!("high");
    cases.push(("invalid-effort-range", invalid_effort_range));

    for (name, contents) in cases {
        let path = directory.path().join(format!("{name}.json"));
        let store = crate::auth::codex::CredentialStore::new(path);
        store
            .save_model_cache(&serde_json::to_vec(&contents).unwrap())
            .unwrap();
        assert!(load_codex_model_cache(&store, &claims).is_err(), "{name}");
    }
}

#[test]
fn generic_discovery_accepts_openai_codex_and_bare_array_shapes() {
    for body in [
        serde_json::json!({"data": [{"id": "a", "context_length": 10}]}),
        serde_json::json!({"models": [{"slug": "b", "max_model_len": 20}]}),
        serde_json::json!([{"id": "c", "max_context_tokens": 30}]),
    ] {
        let models = api_models_from_response(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].context_window.is_some());
    }
}

#[test]
fn discovery_rejects_error_objects_instead_of_hiding_them_as_empty() {
    assert!(api_models_from_response(&serde_json::json!({
        "error": {"message": "unauthorized"}
    }))
    .is_err());
    assert!(codex_models_from_response(&serde_json::json!({"models": []}), None).is_err());
}

#[test]
fn provider_inventory_cache_is_private_and_scoped_to_provider_url_and_account() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/openrouter.json");
    let body = serde_json::json!({"data": [{"id": "model-a"}]});
    let first_key = credential_fingerprint("key-one");
    save_provider_inventory_cache(
        &path,
        "openrouter",
        "https://one.test/v1/models",
        &first_key,
        Some(&body),
    )
    .unwrap();
    match load_provider_inventory_cache(
        &path,
        "openrouter",
        "https://one.test/v1/models",
        &first_key,
    )
    .unwrap()
    {
        Some(CachedProviderInventory::Available(cached)) => assert_eq!(cached, body),
        _ => panic!("expected cached provider inventory"),
    }
    assert!(load_provider_inventory_cache(
        &path,
        "opencode",
        "https://one.test/v1/models",
        &first_key,
    )
    .unwrap()
    .is_none());
    assert!(load_provider_inventory_cache(
        &path,
        "openrouter",
        "https://two.test/v1/models",
        &first_key,
    )
    .unwrap()
    .is_none());
    assert!(
        load_provider_inventory_cache(
            &path,
            "openrouter",
            "https://one.test/v1/models",
            &credential_fingerprint("key-two"),
        )
        .unwrap()
        .is_none(),
        "changing accounts must invalidate the cached inventory"
    );
    save_provider_inventory_cache(
        &path,
        "openrouter",
        "https://one.test/v1/models",
        &first_key,
        None,
    )
    .unwrap();
    assert!(
        matches!(
            load_provider_inventory_cache(
                &path,
                "openrouter",
                "https://one.test/v1/models",
                &first_key,
            )
            .unwrap(),
            Some(CachedProviderInventory::Unavailable)
        ),
        "failed discovery must leave a reusable negative cache marker"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn provider_inventory_cache_names_and_future_timestamps_are_collision_safe() {
    assert_ne!(
        provider_inventory_cache_path("provider/a"),
        provider_inventory_cache_path("provider:a"),
    );
    assert!(cache_modified_is_stale(
        std::time::SystemTime::now() + Duration::from_secs(60),
        Duration::from_secs(1),
    ));
}

#[test]
fn negative_provider_cache_recovers_in_the_current_launch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/openrouter.json");
    let url = "https://openrouter.test/v1/models";
    let credential = "key-one";
    let fingerprint = credential_fingerprint(credential);
    save_provider_inventory_cache(&path, "openrouter", url, &fingerprint, None).unwrap();
    let recovered = serde_json::json!({"data": [{"id": "recovered-model"}]});

    let body = cached_provider_inventory_with_fetch(
        path.clone(),
        "openrouter",
        url.to_string(),
        http::HeaderMap::new(),
        credential,
        |_, _| Ok(recovered.clone()),
    )
    .unwrap()
    .expect("a foreground retry should recover the inventory");
    assert_eq!(body, recovered);
    assert!(matches!(
        load_provider_inventory_cache(&path, "openrouter", url, &fingerprint).unwrap(),
        Some(CachedProviderInventory::Available(body)) if body == recovered
    ));
}

#[test]
fn failed_provider_refresh_never_overwrites_last_good_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/openrouter.json");
    let url = "https://openrouter.test/v1/models";
    let fingerprint = credential_fingerprint("key-one");
    let last_good = serde_json::json!({"data": [{"id": "last-good"}]});
    save_provider_inventory_cache(&path, "openrouter", url, &fingerprint, Some(&last_good))
        .unwrap();

    let recovered = fetch_and_cache_provider_inventory_with(
        &path,
        "openrouter",
        url.to_string(),
        http::HeaderMap::new(),
        &fingerprint,
        |_, _| anyhow::bail!("transient failure"),
    )
    .expect("a transient refresh failure should retain last-good metadata");
    assert_eq!(recovered, last_good);
    assert!(matches!(
        load_provider_inventory_cache(&path, "openrouter", url, &fingerprint).unwrap(),
        Some(CachedProviderInventory::Available(body)) if body == last_good
    ));
}

#[test]
fn custom_registry_registers_labeled_providers_with_isolated_auth_and_models() {
    use crate::auth::custom::{
        CustomAuthConfig, CustomCredential, CustomModel, CustomProvider, CustomRegistry,
    };

    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let provider = |label: &str, base_url: &str, model_id: &str, auth| CustomProvider {
        label: label.into(),
        credential: CustomCredential {
            base_url: base_url.into(),
            api_key: String::new(),
            api_name: String::new(),
            headers: Vec::new(),
            models: vec![CustomModel {
                api_name: model_id.into(),
                ..Default::default()
            }],
            auto_discover: false,
        },
        auth,
        api_key_env: None,
        cache: None,
        startup_timeout_secs: None,
        lifecycle_feedback: false,
    };
    let mut registry = CustomRegistry::single(
        "apple-fm",
        provider(
            "Apple Foundation Models",
            "http://127.0.0.1:1976/v1/",
            "shared-model",
            Some(CustomAuthConfig::None),
        ),
    );
    registry
        .providers
        .get_mut("apple-fm")
        .unwrap()
        .lifecycle_feedback = true;
    registry.providers.insert(
        "home-server".into(),
        provider(
            "Home Server",
            "http://127.0.0.1:8000/v1/",
            "shared-model",
            Some(CustomAuthConfig::BearerEnv {
                var: "YGG_TEST_HOME_SERVER_KEY".into(),
            }),
        ),
    );
    // One provider declares explicit per-token pricing.
    let mut priced = provider(
        "Metered Gateway",
        "http://127.0.0.1:8500/v1/",
        "metered-model",
        Some(CustomAuthConfig::None),
    );
    priced.credential.models[0].pricing = Some(crate::auth::custom::CustomPricing {
        input: 75,
        output: 300,
        ..Default::default()
    });
    registry.providers.insert("metered-gateway".into(), priced);
    registry.providers.insert(
        "invalid/provider".into(),
        provider(
            "Invalid Provider",
            "http://127.0.0.1:9000/v1/",
            "invalid-model",
            Some(CustomAuthConfig::None),
        ),
    );
    store.save_registry(&registry).unwrap();

    let mut catalog = ModelCatalog::default();
    register_custom_openai_endpoints_from_store(&mut catalog, &store, true).unwrap();

    let apple = catalog
        .resolve(&ModelId("custom/apple-fm/shared-model".into()))
        .unwrap();
    assert_eq!(apple.endpoint.id.0, "custom-provider-8-apple-fm");
    assert_eq!(
        catalog.endpoint_label(&apple.endpoint.id),
        Some("Apple Foundation Models")
    );
    assert_eq!(
        apple.endpoint.base_url.as_str(),
        "http://127.0.0.1:1976/v1/"
    );
    assert!(matches!(apple.endpoint.auth, Auth::None));
    assert!(apple.endpoint.runtime.lifecycle_feedback);

    let home = catalog
        .resolve(&ModelId("custom/home-server/shared-model".into()))
        .unwrap();
    assert_eq!(home.endpoint.id.0, "custom-provider-11-home-server");
    assert_eq!(
        catalog.endpoint_label(&home.endpoint.id),
        Some("Home Server")
    );
    assert!(matches!(
        home.endpoint.auth,
        Auth::BearerEnv { ref var } if var == "YGG_TEST_HOME_SERVER_KEY"
    ));
    assert!(!home.endpoint.runtime.lifecycle_feedback);

    // Undeclared custom-model pricing defaults to trusted zero rates so
    // cost-ceiling guardrails (such as subagent budgets) stay enforceable.
    let default_pricing = home
        .spec
        .pricing
        .as_ref()
        .expect("custom models must carry trusted pricing");
    assert_eq!(default_pricing.input, TokenRate(0));
    assert_eq!(default_pricing.output, TokenRate(0));
    assert_eq!(default_pricing.cache_read, TokenRate(0));
    assert_eq!(default_pricing.cache_write_5m, TokenRate(0));

    // A declared pricing block is honored verbatim.
    let metered = catalog
        .resolve(&ModelId("custom/metered-gateway/metered-model".into()))
        .unwrap();
    let declared = metered.spec.pricing.as_ref().expect("declared pricing");
    assert_eq!(declared.input, TokenRate(75));
    assert_eq!(declared.output, TokenRate(300));
    assert_eq!(declared.cache_read, TokenRate(0));

    assert!(catalog
        .resolve(&ModelId("custom/invalid/provider/invalid-model".into()))
        .is_err());
}

#[test]
fn custom_model_cache_is_scoped_to_endpoint_and_reuses_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let models = vec![crate::auth::custom::CustomModel {
        api_name: "local-model".into(),
        display_name: "Local Model".into(),
        context_window: 262_144,
        max_output_tokens: 16_384,
        tools: true,
        parallel_tool_calls: true,
        vision: false,
        structured_output: false,
        reasoning: true,
        reasoning_configurable: true,
        reasoning_values: Vec::new(),
        reasoning_default: String::new(),
        reasoning_uses_system_message: true,
        pricing: None,
    }];
    let mut first_headers = http::HeaderMap::new();
    first_headers.insert("x-organization", "tenant-one".parse().unwrap());
    first_headers.insert("x-region", "north".parse().unwrap());
    let first_key = custom_credential_fingerprint("custom-key-one", &first_headers);
    save_custom_model_cache(&store, "http://one.test/v1/", &first_key, &models).unwrap();
    let Some(CachedCustomInventory::Available(loaded)) =
        load_custom_model_cache(&store, "http://one.test/v1/", &first_key).unwrap()
    else {
        panic!("expected positive custom inventory")
    };
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].api_name, "local-model");
    assert!(
        load_custom_model_cache(&store, "http://two.test/v1/", &first_key)
            .unwrap()
            .is_none(),
        "a cache from another endpoint must never populate this catalog"
    );
    assert!(
        load_custom_model_cache(
            &store,
            "http://one.test/v1/",
            &custom_credential_fingerprint("custom-key-two", &first_headers),
        )
        .unwrap()
        .is_none(),
        "a cache from another custom account must never populate this catalog"
    );
    let mut changed_headers = first_headers.clone();
    changed_headers.insert("x-organization", "tenant-two".parse().unwrap());
    assert!(
        load_custom_model_cache(
            &store,
            "http://one.test/v1/",
            &custom_credential_fingerprint("custom-key-one", &changed_headers),
        )
        .unwrap()
        .is_none(),
        "changing a tenant or authorization header must invalidate the inventory"
    );

    let mut reordered_headers = http::HeaderMap::new();
    reordered_headers.insert("x-region", "north".parse().unwrap());
    reordered_headers.insert("x-organization", "tenant-one".parse().unwrap());
    assert_eq!(
        first_key,
        custom_credential_fingerprint("custom-key-one", &reordered_headers),
        "header insertion order is not part of the credential scope"
    );

    save_custom_model_cache(&store, "http://one.test/v1/", &first_key, &[]).unwrap();
    assert!(
        matches!(
            load_custom_model_cache(&store, "http://one.test/v1/", &first_key).unwrap(),
            Some(CachedCustomInventory::Unavailable)
        ),
        "an empty inventory is a valid negative cache marker"
    );
}

#[test]
fn custom_model_cache_invalidates_when_configured_metadata_changes() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let base_url = "http://custom.test/v1/";
    let credential = custom_credential_fingerprint("key", &http::HeaderMap::new());
    let configured = crate::auth::custom::CustomModel {
        api_name: "system".into(),
        context_window: APPLE_FM_SYSTEM_CONTEXT_WINDOW,
        max_output_tokens: APPLE_FM_MAX_OUTPUT_TOKENS,
        ..Default::default()
    };
    let original = custom_model_cache_fingerprint(&credential, std::slice::from_ref(&configured));
    let changed = custom_model_cache_fingerprint(
        &credential,
        &[crate::auth::custom::CustomModel {
            context_window: 4_096,
            ..configured
        }],
    );
    save_custom_model_cache_for(
        &store,
        "provider",
        base_url,
        &original,
        &[crate::auth::custom::CustomModel {
            api_name: "system".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    assert!(matches!(
        load_custom_model_cache_for(&store, "provider", base_url, &original).unwrap(),
        Some(CachedCustomInventory::Available(_))
    ));
    assert!(
        load_custom_model_cache_for(&store, "provider", base_url, &changed)
            .unwrap()
            .is_none(),
        "changing configured metadata must force fresh discovery"
    );
}

#[test]
fn version_four_custom_cache_is_invalid_after_hlid_tool_fallback_change() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let base_url = "https://ai.watchyourtemper.com/v1/";
    let fingerprint = custom_credential_fingerprint("", &http::HeaderMap::new());
    let stale = CustomModelCache {
        version: 4,
        base_url: base_url.into(),
        credential_fingerprint: fingerprint.clone(),
        models: vec![crate::auth::custom::CustomModel {
            api_name: "qwen3.6-27b".into(),
            tools: false,
            ..Default::default()
        }],
    };
    store
        .save_model_cache(&serde_json::to_vec(&stale).unwrap())
        .unwrap();

    assert!(
        load_custom_model_cache(&store, base_url, &fingerprint)
            .unwrap()
            .is_none(),
        "v4 may contain tools=false from the pre-tri-state hlid path"
    );
}

#[test]
fn stale_positive_custom_cache_refreshes_the_current_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let cred = crate::auth::custom::CustomCredential {
        base_url: "http://custom.test/v1/".to_string(),
        api_key: "key".to_string(),
        api_name: String::new(),
        headers: Vec::new(),
        models: Vec::new(),
        auto_discover: true,
    };
    let fingerprint = custom_credential_fingerprint(&cred.api_key, &http::HeaderMap::new());
    let previous = crate::auth::custom::CustomModel {
        api_name: "previous-model".to_string(),
        ..Default::default()
    };
    save_custom_model_cache(
        &store,
        &cred.base_url,
        &fingerprint,
        std::slice::from_ref(&previous),
    )
    .unwrap();

    let fresh = refresh_stale_custom_models_with(
        &store,
        &cred,
        &fingerprint,
        vec![previous],
        Duration::ZERO,
        |_| {
            vec![crate::auth::custom::CustomModel {
                api_name: "currently-served-model".to_string(),
                ..Default::default()
            }]
        },
    );

    assert_eq!(fresh[0].api_name, "currently-served-model");
    assert!(matches!(
        load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
        Some(CachedCustomInventory::Available(models))
            if models[0].api_name == "currently-served-model"
    ));
}

#[test]
fn failed_stale_custom_refresh_retains_the_last_good_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let cred = crate::auth::custom::CustomCredential {
        base_url: "http://custom.test/v1/".to_string(),
        api_key: "key".to_string(),
        api_name: String::new(),
        headers: Vec::new(),
        models: Vec::new(),
        auto_discover: true,
    };
    let fingerprint = custom_credential_fingerprint(&cred.api_key, &http::HeaderMap::new());
    let previous = crate::auth::custom::CustomModel {
        api_name: "last-good-model".to_string(),
        ..Default::default()
    };
    save_custom_model_cache(
        &store,
        &cred.base_url,
        &fingerprint,
        std::slice::from_ref(&previous),
    )
    .unwrap();

    let retained = refresh_stale_custom_models_with(
        &store,
        &cred,
        &fingerprint,
        vec![previous],
        Duration::ZERO,
        |_| Vec::new(),
    );

    assert_eq!(retained[0].api_name, "last-good-model");
    assert!(matches!(
        load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
        Some(CachedCustomInventory::Available(models))
            if models[0].api_name == "last-good-model"
    ));
}

#[test]
fn hlid_llama_cpp_metadata_reports_the_served_context_window() {
    let entry = serde_json::json!({
        "id": "ornith-35b-q4km",
        "meta": {
            "n_ctx": 131_072,
            "n_ctx_train": 262_144
        }
    });

    assert_eq!(extract_ctx_from_model_entry(&entry), 131_072);
}

#[test]
fn sparse_custom_inventory_preserves_local_tools_but_honors_explicit_false() {
    // This is the live hlid shape: it advertises reasoning details but no
    // standardized tool capability field. A user-configured local OpenAI
    // endpoint keeps the historical tool-capable default.
    let sparse_hlid = serde_json::json!({
        "id": "qwen3.6-27b",
        "capabilities": {"reasoning": {
            "supported": true,
            "control": "binary",
            "values": ["none", "default"],
            "default": "default"
        }}
    });
    assert_eq!(model_metadata_tool_support(&sparse_hlid), None);
    assert!(custom_model_metadata_supports_tools(&sparse_hlid));
    assert!(!model_metadata_supports_tools(&sparse_hlid));

    let explicitly_disabled = serde_json::json!({
        "id": "text-only",
        "capabilities": {"tools": {"supported": false}}
    });
    assert_eq!(
        model_metadata_tool_support(&explicitly_disabled),
        Some(false)
    );
    assert!(!custom_model_metadata_supports_tools(&explicitly_disabled));

    let explicit_parameter_list = serde_json::json!({
        "id": "reasoning-only",
        "supported_parameters": ["reasoning_effort"]
    });
    assert_eq!(
        model_metadata_tool_support(&explicit_parameter_list),
        Some(false)
    );
    assert!(!custom_model_metadata_supports_tools(
        &explicit_parameter_list
    ));
}

#[test]
fn hlid_reasoning_metadata_controls_custom_capabilities_exactly() {
    let off_only = serde_json::json!({
        "capabilities": {"reasoning": {
            "supported": true,
            "control": "binary",
            "values": ["none"],
            "default": "none"
        }}
    });
    let (reasoning, values, default) = discovered_custom_reasoning(&off_only);
    assert!(!reasoning);
    assert_eq!(values, ["none"]);
    assert_eq!(default, "none");
    let off_model = crate::auth::custom::CustomModel {
        reasoning,
        reasoning_values: values,
        reasoning_default: default,
        ..Default::default()
    };
    assert!(custom_reasoning_capability(&off_model).is_none());

    let binary = serde_json::json!({
        "capabilities": {"reasoning": {
            "supported": true,
            "control": "binary",
            "values": ["none", "default"],
            "default": "default"
        }}
    });
    let (reasoning, values, default) = discovered_custom_reasoning(&binary);
    let binary_model = crate::auth::custom::CustomModel {
        reasoning,
        reasoning_values: values,
        reasoning_default: default,
        reasoning_uses_system_message: true,
        ..Default::default()
    };
    let binary_capability = custom_reasoning_capability(&binary_model).unwrap();
    assert_eq!(binary_capability.control, ReasoningControl::Toggle);
    assert!(matches!(
        binary_capability.openai_chat_mode,
        OpenAiChatReasoningMode::ProviderValues {
            values,
            default: Some(default),
            system_message: true,
        } if values == ["none", "default"] && default == "default"
    ));

    let levels = serde_json::json!({
        "capabilities": {"reasoning": {
            "supported": true,
            "control": "levels",
            "values": ["none", "low", "medium", "high"],
            "default": "medium"
        }}
    });
    let (reasoning, values, default) = discovered_custom_reasoning(&levels);
    let level_model = crate::auth::custom::CustomModel {
        reasoning,
        reasoning_values: values,
        reasoning_default: default,
        ..Default::default()
    };
    let level_capability = custom_reasoning_capability(&level_model).unwrap();
    assert_eq!(level_capability.control, ReasoningControl::Effort);
    assert_eq!(level_capability.min_effort, ygg_ai::ReasoningEffort::Low);
    assert_eq!(level_capability.max_effort, ygg_ai::ReasoningEffort::High);
}

#[test]
fn negative_custom_cache_recovers_without_another_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        crate::auth::custom::CredentialStore::new(directory.path().join("credentials/custom.json"));
    let cred = crate::auth::custom::CustomCredential {
        base_url: "http://custom.test/v1/".to_string(),
        api_key: "key".to_string(),
        api_name: String::new(),
        headers: Vec::new(),
        models: Vec::new(),
        auto_discover: true,
    };
    let fingerprint = custom_credential_fingerprint(&cred.api_key, &http::HeaderMap::new());
    save_custom_model_cache(&store, &cred.base_url, &fingerprint, &[]).unwrap();
    assert!(matches!(
        load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
        Some(CachedCustomInventory::Unavailable)
    ));
    let recovered = crate::auth::custom::CustomModel {
        api_name: "recovered-local".to_string(),
        ..Default::default()
    };

    let models = discover_and_cache_custom_models_with(&store, &cred, &fingerprint, false, |_| {
        vec![recovered.clone()]
    });
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].api_name, "recovered-local");
    assert!(matches!(
        load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
        Some(CachedCustomInventory::Available(models))
            if models.len() == 1 && models[0].api_name == "recovered-local"
    ));
}

#[test]
fn deepseek_v4_pro_is_registered_as_openai_chat_with_env_auth() {
    let directory = tempfile::tempdir().unwrap();
    let boot = bootstrap(config(directory.path(), Some(DEEPSEEK_MODEL_ID))).unwrap();
    let model = boot
        .catalog
        .resolve(&ModelId(DEEPSEEK_MODEL_ID.into()))
        .unwrap();
    assert_eq!(model.spec.protocol, Protocol::OpenAiChat);
    assert_eq!(
        model.endpoint.id.0,
        crate::providers::DEEPSEEK.routes[0].endpoint_id
    );
    assert_eq!(
        model.spec.api_name,
        std::env::var("YGG_DEEPSEEK_MODEL").unwrap_or_else(|_| DEEPSEEK_MODEL_ID.into())
    );
    assert!(model.spec.capabilities.tools);
    assert!(matches!(
        model.spec.capabilities.reasoning.as_ref(),
        Some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            openai_chat_mode: OpenAiChatReasoningMode::DeepSeekThinking,
            ..
        })
    ));
    assert_eq!(
        model.spec.limits.context_window,
        std::env::var("YGG_DEEPSEEK_CONTEXT_WINDOW")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEEPSEEK_DEFAULT_CONTEXT_WINDOW)
    );
    assert_eq!(
        model.spec.limits.max_output_tokens,
        std::env::var("YGG_DEEPSEEK_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS)
    );
}

#[test]
fn deepseek_v4_pro_accepts_high_reasoning_at_startup() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), Some(DEEPSEEK_MODEL_ID));
    config.reasoning = ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High);
    let boot = bootstrap(config).unwrap();
    let launch = resolve_launch_print(&boot, "test-session").unwrap();
    let app = build_app(boot, launch, "system".into()).unwrap();
    assert_eq!(
        app.reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
    );
}

#[test]
fn print_launch_errors_without_model() {
    let directory = tempfile::tempdir().unwrap();
    let boot = bootstrap(config(directory.path(), None)).unwrap();
    let error = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap_err();
    assert!(error.to_string().contains("no model configured"));
}

#[test]
fn interactive_launch_replaces_an_unavailable_persisted_model() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), None);
    config.model_explicit = false;
    let boot = bootstrap(config).unwrap();
    let unavailable = ModelId("provider-model-without-a-credential".into());

    assert!(should_pick_interactive_model(
        &boot.config,
        &boot.catalog,
        Some(&unavailable),
    ));
    assert!(!should_pick_interactive_model(
        &boot.config,
        &boot.catalog,
        Some(&ModelId("gpt-4o-mini".into())),
    ));

    let mut explicit = boot.config.clone();
    explicit.model_explicit = true;
    assert!(!should_pick_interactive_model(
        &explicit,
        &boot.catalog,
        Some(&unavailable),
    ));
}

#[test]
fn print_launch_creates_new_session_path_with_model() {
    let directory = tempfile::tempdir().unwrap();
    let boot = bootstrap(config(directory.path(), Some("gpt-4o-mini"))).unwrap();
    let launch = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap();
    assert_eq!(launch.model.0, "gpt-4o-mini");
    assert!(matches!(launch.session, SessionSelection::CreateNew(_)));
}

#[test]
fn print_resume_restores_session_model_and_reasoning_unless_cli_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let mut process_config = config(directory.path(), None);
    process_config.resume = ResumeSelector::Continue;
    process_config.model_explicit = false;
    process_config.reasoning_explicit = false;
    let boot = bootstrap(process_config).unwrap();
    let path = boot.sessions.new_path("2026-07-12T00-00-00Z");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut session = Session::create(&path).unwrap();
    session
        .append(EntryValue::Config {
            model: Some("gpt-5.4-mini-responses".to_string()),
            reasoning: Some("high".to_string()),
            reasoning_mode: None,
        })
        .unwrap();
    session
        .append(EntryValue::Message(ygg_ai::Message::User(
            ygg_ai::UserMessage {
                content: vec![ygg_ai::UserPart::Text("resumable prompt".into())],
            },
        )))
        .unwrap();
    drop(session);

    let launch = resolve_launch_print(&boot, "unused").unwrap();
    assert_eq!(launch.model.0, "gpt-5.4-mini-responses");
    assert_eq!(
        launch.reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
    );

    let mut overridden = config(directory.path(), Some("gpt-4o-mini"));
    overridden.resume = ResumeSelector::Continue;
    overridden.model_explicit = true;
    overridden.reasoning = ReasoningConfig::Off;
    overridden.reasoning_explicit = true;
    let launch = resolve_launch_print(&bootstrap(overridden).unwrap(), "unused").unwrap();
    assert_eq!(launch.model.0, "gpt-4o-mini");
    assert_eq!(launch.reasoning, ReasoningConfig::Off);
}

#[test]
fn explicit_reasoning_clears_a_persisted_legacy_pro_mode() {
    let directory = tempfile::tempdir().unwrap();
    let mut process_config = config(directory.path(), Some("gpt-5.4-mini-responses"));
    process_config.resume = ResumeSelector::Continue;
    process_config.reasoning = ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High);
    process_config.reasoning_explicit = true;
    process_config.reasoning_mode_explicit = false;
    let boot = bootstrap(process_config).unwrap();
    let path = boot.sessions.new_path("2026-07-12T00-00-00Z");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut session = Session::create(&path).unwrap();
    session
        .append(EntryValue::Config {
            model: Some("gpt-5.4-mini-responses".to_string()),
            reasoning: Some("max".to_string()),
            reasoning_mode: Some("pro".to_string()),
        })
        .unwrap();
    session
        .append(EntryValue::Message(ygg_ai::Message::User(
            ygg_ai::UserMessage {
                content: vec![ygg_ai::UserPart::Text("resumable prompt".into())],
            },
        )))
        .unwrap();
    drop(session);

    let launch = resolve_launch_print(&boot, "unused").unwrap();
    assert_eq!(
        launch.reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
    );
    assert_eq!(launch.reasoning_mode, ReasoningMode::Standard);
}

#[test]
fn launch_configuration_parts_returns_the_preopened_resume_session() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), None);
    config.model_explicit = false;
    config.reasoning_explicit = false;
    let path = directory.path().join("preopened.jsonl");
    let mut session = Session::create(&path).unwrap();
    session
        .append(EntryValue::Config {
            model: Some("gpt-5.4-mini-responses".to_owned()),
            reasoning: Some("high".to_owned()),
            reasoning_mode: None,
        })
        .unwrap();
    drop(session);

    let (prepared, model, reasoning, reasoning_mode) =
        launch_configuration_parts(&config, &SessionSelection::OpenExisting(path.clone())).unwrap();

    assert_eq!(prepared.as_ref().map(Session::path), Some(path.as_path()));
    assert_eq!(model, Some(ModelId("gpt-5.4-mini-responses".into())));
    assert_eq!(
        reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
    );
    assert_eq!(reasoning_mode, ReasoningMode::Standard);
}

#[test]
fn disabled_tools_are_absent_from_both_schema_and_execution_registry() {
    let directory = tempfile::tempdir().unwrap();
    let skills: Arc<dyn SkillRegistry> =
        Arc::new(FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], false).unwrap());
    let mut config = config(directory.path(), Some("gpt-4o-mini"));
    config.sandbox.allow_edit = false;
    config.sandbox.allow_write = false;
    config.sandbox.allow_process = false;
    config.sandbox.allow_shell = false;
    let extensions = configured_test_extensions(skills, &config);
    let names = extensions
        .tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["read"]);
}

#[test]
fn legacy_skill_tools_are_not_registered_by_default() {
    let directory = tempfile::tempdir().unwrap();
    let skills: Arc<dyn SkillRegistry> =
        Arc::new(FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], true).unwrap());
    let mut config = config(directory.path(), Some("gpt-4o-mini"));
    config.tools =
        crate::config::ToolPolicy::only(["read".to_owned(), "load_skill".to_owned()]).unwrap();
    config.sandbox.allow_edit = false;
    let extensions = configured_test_extensions(skills, &config);
    let registered = extensions
        .tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(registered, vec!["read"]);
}

#[test]
fn initial_build_ignores_legacy_active_skill_tool_requirements() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("active-skill.jsonl");
    let mut session = Session::create(&path).unwrap();
    append_active_skill(&mut session, "editor", &["edit"]);
    drop(session);

    let mut config = config(directory.path(), Some("gpt-4o-mini"));
    config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();
    let boot = bootstrap(config).unwrap();
    let app = build_app(
        boot,
        LaunchSelection {
            model: ModelId("gpt-4o-mini".into()),
            session: SessionSelection::OpenExisting(path),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
        },
        "system".into(),
    )
    .unwrap();
    assert!(!app.system.contains("test instructions"));
}

#[test]
fn rebuild_ignores_legacy_active_skill_tool_requirements() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("active-skill-rebuild.jsonl");
    let mut session = Session::create(&path).unwrap();
    append_active_skill(&mut session, "editor", &["edit"]);
    drop(session);

    let config = config(directory.path(), Some("gpt-4o-mini"));
    let boot = bootstrap(config).unwrap();
    let mut app = build_app(
        boot,
        LaunchSelection {
            model: ModelId("gpt-4o-mini".into()),
            session: SessionSelection::OpenExisting(path),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
        },
        "system".into(),
    )
    .unwrap();
    app.config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();

    let rebuilt = rebuild_app(app, None, None, None, None).unwrap();
    assert!(!rebuilt.system.contains("test instructions"));
}

#[test]
fn explicit_unavailable_tools_report_final_available_names_and_policy_gates() {
    let directory = tempfile::tempdir().unwrap();
    let skills: Arc<dyn SkillRegistry> =
        Arc::new(FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], false).unwrap());
    let mut config = config(directory.path(), Some("gpt-4o-mini"));
    config.tools = crate::config::ToolPolicy::only([
        "read".to_owned(),
        "edit".to_owned(),
        "missing-extension".to_owned(),
    ])
    .unwrap();
    config.tools.exclude("edit").unwrap();
    config.sandbox.allow_edit = false;
    let extensions = configured_test_extensions(skills, &config);
    let boot = bootstrap(config.clone()).unwrap();
    let model = boot
        .catalog
        .resolve(config.model.as_ref().unwrap())
        .unwrap();

    let error = validate_explicit_tool_policy(&config, &extensions, &model, false).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("edit, missing-extension"), "{message}");
    assert!(
        message.contains("allowlists, sandbox gates, and extension registration"),
        "{message}"
    );
    assert!(message.contains("available tools: read"), "{message}");
    let mut dynamic_config = config.clone();
    dynamic_config.tools =
        crate::config::ToolPolicy::only(["read".to_owned(), "missing-extension".to_owned()])
            .unwrap();
    validate_explicit_tool_policy(&dynamic_config, &extensions, &model, true)
        .expect("a negotiated live catalog may publish explicitly allowed names later");
}

#[test]
fn model_without_tool_capability_gets_no_default_surface_and_rejects_explicit_tools() {
    let directory = tempfile::tempdir().unwrap();
    let mut default_config = config(directory.path(), Some("gpt-4o-mini"));
    let boot = bootstrap(default_config.clone()).unwrap();
    let resolved = boot
        .catalog
        .resolve(default_config.model.as_ref().unwrap())
        .unwrap();
    let mut spec = (*resolved.spec).clone();
    spec.capabilities.tools = false;
    spec.capabilities.parallel_tool_calls = false;
    let model = Model {
        spec: Arc::new(spec),
        endpoint: resolved.endpoint,
    };
    let session = Session::create(directory.path().join("no-tools-default.jsonl")).unwrap();
    let (extensions, _) = configured_extensions(
        &default_config,
        &session,
        &model,
        &default_config.reasoning,
        &boot.sessions,
    )
    .unwrap();
    assert!(extensions.tool_definitions().is_empty());
    validate_explicit_tool_policy(&default_config, &extensions, &model, false).unwrap();

    default_config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();
    let explicit_session =
        Session::create(directory.path().join("no-tools-explicit.jsonl")).unwrap();
    let (extensions, _) = configured_extensions(
        &default_config,
        &explicit_session,
        &model,
        &default_config.reasoning,
        &boot.sessions,
    )
    .unwrap();
    let error =
        validate_explicit_tool_policy(&default_config, &extensions, &model, false).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("gpt-4o-mini does not support tools"),
        "{message}"
    );
    assert!(
        message.contains("explicit tool policy requested: read"),
        "{message}"
    );
}

#[test]
fn initial_build_records_configuration_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let boot = bootstrap(config(directory.path(), Some("gpt-4o-mini"))).unwrap();
    let launch = resolve_launch_print(&boot, "initial-config").unwrap();
    let app = build_app(boot, launch, "system".to_string()).unwrap();
    assert_eq!(
        app.agent.completion_policy(),
        ygg_agent::CompletionPolicy::Natural,
        "ordinary coding turns must not pay for a hidden second inference"
    );
    assert!(matches!(
        app.agent.session().entries().first().map(|entry| &entry.value),
        Some(EntryValue::Config {
            model: Some(model),
            reasoning: Some(reasoning),
            reasoning_mode: Some(reasoning_mode),
        }) if model == "gpt-4o-mini" && reasoning == "off" && reasoning_mode == "standard"
    ));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unknown_api_03_provider_model_preflights_restarts_and_reloads_with_fresh_routes() {
    let node = std::process::Command::new("node")
        .arg("--version")
        .status()
        .expect("node is required for the checked-in Pi bridge fixture");
    assert!(
        node.success(),
        "node must be usable for the Pi bridge fixture"
    );

    let directory = tempfile::tempdir().unwrap();
    let extension_root = directory.path().join("extensions");
    let provider = extension_root.join("pi-provider");
    std::fs::create_dir_all(&provider).unwrap();
    let launcher = provider.join("launch-bridge.sh");
    std::fs::write(&launcher, "#!/bin/sh\nexec node \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(&launcher).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&launcher, permissions).unwrap();
    let repository = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("coding-agent crate has a repository root")
        .to_owned();
    let bridge = repository
        .join("extensions/ygg-pi-compat/bridge.mjs")
        .to_string_lossy()
        .into_owned();
    let provider_extension = repository
        .join("extensions/ygg-pi-compat/tests/fixtures/provider-extension.mjs")
        .to_string_lossy()
        .into_owned();
    let fake_pi = repository
        .join("extensions/ygg-pi-compat/tests/fixtures/fake-pi")
        .to_string_lossy()
        .into_owned();
    std::fs::write(
        provider.join("extension.toml"),
        format!(
            r#"name = "pi-provider"
version = "0.3.0"
api_version = "0.3"

[entrypoint]
command = "launch-bridge.sh"
args = [{bridge:?}, "--extension", {provider_extension:?}, "--pi-package", {fake_pi:?}, "--api-version", "0.3"]
env = {{ YGG_PI_FIXTURE_API_VERSION = "0.3", YGG_PI_FIXTURE_PROVIDER_AUTH = "none", YGG_PI_FIXTURE_PROVIDER_REGISTER_DELAY_MS = "75" }}

[contributes]
tools = ["pi"]
providers = true
"#
        ),
    )
    .unwrap();

    let model_id = "fixture-provider/fixture-model";
    let mut config = config(directory.path(), Some(model_id));
    config.effect_policy = ygg_agent::EffectPolicy::UnsafeHost;
    config.extension_paths = vec![extension_root];
    config.enabled_extensions = vec!["pi-provider".into()];
    config.invocation_trusted_extensions = vec!["pi-provider".into()];

    let boot = bootstrap(config).unwrap();
    boot.preflight_extension_providers().unwrap();
    let preflight_catalog = boot.catalog_with_extension_providers();
    let preflight_status = boot
        .prestarted_extensions
        .borrow()
        .as_ref()
        .map(|(_, extensions)| (extensions.status_summary(), extensions.summaries()));
    assert!(
        preflight_catalog.resolve(&ModelId(model_id.into())).is_ok(),
        "preflight did not project {model_id}; startup: {preflight_status:#?}"
    );
    let launch = resolve_launch_print(&boot, "delayed-provider-model").unwrap();
    assert_eq!(launch.model.0, model_id);

    let mut app = build_app(boot, launch, "system".into()).unwrap();
    assert_eq!(app.model.spec.id.0, model_id);
    assert!(app.catalog.resolve(&ModelId(model_id.into())).is_ok());

    let old_endpoint = app.model.endpoint.id.clone();
    let reloads = app.executable_extensions.reload().await;
    assert!(
        reloads
            .iter()
            .any(|message| message.starts_with("reloaded pi-provider (generation ")),
        "provider reload failed: {reloads:?}"
    );
    app.synchronize_extension_provider_catalog();
    let refreshed = app.catalog.resolve(&ModelId(model_id.into())).unwrap();
    assert_ne!(refreshed.endpoint.id, old_endpoint);
    let error = app
        .synchronize_extension_provider_catalog_for_request()
        .unwrap_err();
    assert!(error.to_string().contains("route changed"), "{error}");

    app.executable_extensions.shutdown().await;
}

#[test]
fn tool_schema_reserve_is_positive_and_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let skills: Arc<dyn SkillRegistry> =
        Arc::new(FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], true).unwrap());
    let config = config(directory.path(), Some("gpt-4o-mini"));
    let extensions = configured_test_extensions(skills, &config);
    let definitions = extensions.tool_definitions();
    let names = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["read", "edit", "write", "bash"]);
    let default_reserve = tool_schema_reserve(&definitions);
    assert!(default_reserve > 0);
    assert_eq!(default_reserve, tool_schema_reserve(&definitions));

    let mut all_core = ExtensionHost::new();
    all_core.load(&CoreTools);
    let all_core_definitions = all_core.tool_definitions();
    assert_eq!(
        all_core_definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read", "edit", "write", "bash", "search"]
    );
    assert!(tool_schema_reserve(&all_core_definitions) > default_reserve);
}

fn fresh_app(directory: &std::path::Path) -> App {
    let boot = bootstrap(config(directory, Some("gpt-4o-mini"))).unwrap();
    let launch = resolve_launch_print(&boot, "test-session").unwrap();
    build_app(boot, launch, "system".into()).unwrap()
}

#[cfg(any(unix, windows))]
#[test]
fn explicit_rebuild_reasoning_clears_a_persisted_legacy_pro_mode() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = fresh_app(directory.path());
    let base = app
        .catalog
        .resolve(&ModelId("gpt-5.4-mini-responses".into()))
        .unwrap();
    let mut spec = (*base.spec).clone();
    spec.id = ModelId("rebuild-ultra-test".into());
    let capability = spec.capabilities.reasoning.as_mut().unwrap();
    capability.max_effort = ygg_ai::ReasoningEffort::Ultra;
    spec.capabilities.responses_lite = true;
    spec.capabilities.agent_delegation = Some(ygg_ai::AgentDelegation::V2);
    app.catalog.register_model(spec).unwrap();

    let target = directory.path().join("legacy-pro-target.jsonl");
    let mut session = Session::create(&target).unwrap();
    session
        .append(EntryValue::Config {
            model: Some("rebuild-ultra-test".into()),
            reasoning: Some("max".into()),
            reasoning_mode: Some("pro".into()),
        })
        .unwrap();
    drop(session);

    let rebuilt = rebuild_app(
        app,
        None,
        Some(ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)),
        None,
        Some(SessionSelection::OpenExisting(target)),
    )
    .unwrap();
    assert_eq!(
        rebuilt.reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
    );
    assert_eq!(rebuilt.reasoning_mode, ReasoningMode::Standard);
}

#[test]
fn rebuild_same_session_preserves_history_without_redundant_config_write() {
    use ygg_ai::{Message, UserMessage, UserPart};

    let directory = tempfile::tempdir().unwrap();
    let mut app = fresh_app(directory.path());
    let entry = app
        .agent
        .session_mut()
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text("keep me".into())],
        })))
        .unwrap();
    let path = app.agent.session().path().to_owned();
    let entries_before = app.agent.session().entries().len();
    let bytes_before = std::fs::metadata(&path).unwrap().len();
    let app = rebuild_app(app, None, None, None, None).unwrap();
    assert!(app.agent.session().entry(&entry).is_some());
    assert_eq!(app.agent.session().entries().len(), entries_before);
    assert_eq!(std::fs::metadata(path).unwrap().len(), bytes_before);
}

#[test]
fn rebuild_restores_the_target_sessions_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let app = fresh_app(directory.path());
    let target = directory.path().join("target.jsonl");
    let mut session = Session::create(&target).unwrap();
    session
        .append(EntryValue::Config {
            model: Some("gpt-5.4-mini-responses".to_string()),
            reasoning: Some("medium".to_string()),
            reasoning_mode: None,
        })
        .unwrap();
    drop(session);

    let app = rebuild_app(
        app,
        None,
        None,
        None,
        Some(SessionSelection::OpenExisting(target)),
    )
    .unwrap();
    assert_eq!(app.model.spec.id.0, "gpt-5.4-mini-responses");
    assert_eq!(
        app.reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Medium)
    );
}

#[test]
fn rebuild_new_session_has_empty_context_and_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let app = fresh_app(directory.path());
    let new_path = directory.path().join("new.jsonl");
    let app = rebuild_app(
        app,
        None,
        None,
        None,
        Some(SessionSelection::CreateNew(new_path)),
    )
    .unwrap();
    assert!(app.agent.session().context().unwrap().is_empty());
    assert_eq!(app.agent.session().entries().len(), 1);
    assert!(matches!(
        app.agent.session().entries()[0].value,
        EntryValue::Config { .. }
    ));
}

#[test]
fn rebuild_validates_native_compaction_against_the_candidate_model() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), Some("gpt-5.4-mini-responses"));
    config.compaction.mode = CompactionMode::NativeResponses;
    let boot = bootstrap(config).unwrap();
    let launch = resolve_launch_print(&boot, "native-rebuild").unwrap();
    let app = build_app(boot, launch, "system".into()).unwrap();
    let chat = app.catalog.resolve(&ModelId("gpt-4o-mini".into())).unwrap();

    let error = match rebuild_app(app, Some(chat), None, None, None) {
        Ok(_) => panic!("candidate Chat model must fail native compaction validation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("native Responses compaction requires an OpenAI Responses route"),
        "{error:#}"
    );
}

#[test]
fn rebuild_prevalidates_native_replay_before_replacing_the_agent() {
    let directory = tempfile::tempdir().unwrap();
    let boot = bootstrap(config(directory.path(), Some("gpt-5.4-mini-responses"))).unwrap();
    let launch = resolve_launch_print(&boot, "native-replay-rebuild").unwrap();
    let mut app = build_app(boot, launch, "system".into()).unwrap();
    app.agent
        .session_mut()
        .append(EntryValue::Message(ygg_ai::Message::User(
            ygg_ai::UserMessage {
                content: vec![ygg_ai::UserPart::Text("legacy prompt".into())],
            },
        )))
        .unwrap();
    app.agent
        .session_mut()
        .append(EntryValue::Message(ygg_ai::Message::Assistant(
            ygg_ai::AssistantMessage {
                content: vec![ygg_ai::AssistantPart::Text("legacy answer".into())],
                model: app.model.spec.id.clone(),
                protocol: Protocol::OpenAiResponses,
            },
        )))
        .unwrap();
    app.config.compaction.mode = CompactionMode::NativeResponses;

    let error = match rebuild_app(app, None, None, None, None) {
        Ok(_) => panic!("legacy Responses history must fail native replay prevalidation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("requires complete route-affine opaque replay"),
        "{error:#}"
    );
}

#[test]
fn fork_launch_copies_the_source_head_and_records_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let session_root = directory.path().join("sessions");
    let store = SessionStore::new(&session_root, directory.path());
    std::fs::create_dir_all(store.dir()).unwrap();
    let source = store.new_path("source");
    let mut session = Session::create(&source).unwrap();
    session
        .append(EntryValue::Message(ygg_ai::Message::User(
            ygg_ai::UserMessage {
                content: vec![ygg_ai::UserPart::Text("source prompt".into())],
            },
        )))
        .unwrap();
    session
        .append(EntryValue::Message(ygg_ai::Message::Assistant(
            ygg_ai::AssistantMessage {
                content: vec![ygg_ai::AssistantPart::Text("source answer".into())],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            },
        )))
        .unwrap();
    let source_head = session.head().unwrap();
    drop(session);

    let destination = store.new_path("fork");
    let path = fork_session_into(&store, &source, destination).unwrap();
    let forked = Session::open_read_only(&path).unwrap();
    assert_eq!(forked.head(), Some(source_head.clone()));
    assert_eq!(forked.context().unwrap().len(), 2);
    let destination_id = path.file_stem().unwrap().to_str().unwrap();
    let metadata = store.load_metadata(destination_id).unwrap();
    assert_eq!(
        metadata.forked_from_session_id,
        source
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    );
    assert_eq!(metadata.forked_from_entry_id, Some(source_head.0));
}
