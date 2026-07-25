use super::*;

#[test]
fn custom_endpoint_startup_timeout_is_cold_start_safe_and_configurable() {
    assert_eq!(
        resolve_custom_startup_timeout(None, None).unwrap(),
        Duration::from_secs(300)
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
        sandbox: SandboxPolicy::default(),
        theme: None,
        theme_paths: vec![],
        color: crate::config::ColorMode::Auto,
        plain: false,
        session_dir: directory.join("sessions"),
        compaction: CompactionPolicy::default(),
        max_cost_microdollars: None,
        cost_warning_microdollars: None,
        show_turn_cost: false,
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
        trusted_extensions: vec![],
        invocation_trusted_extensions: vec![],
        tools: crate::config::ToolPolicy::default(),
        context_files: true,
        offline: true,
        workspace_trusted: true,
    }
}

fn configured_test_extensions(skills: Arc<dyn SkillRegistry>, config: &Config) -> ExtensionHost {
    let boot = bootstrap(config.clone()).unwrap();
    let model_id = config.model.as_ref().expect("test model");
    let model = boot.catalog.resolve(model_id).unwrap();
    let session = Session::create(config.workspace.join("tool-policy-test.jsonl")).unwrap();
    configured_extensions(
        skills,
        config,
        &session,
        &model,
        &config.reasoning,
        &boot.sessions,
    )
    .0
}

fn append_active_skill(session: &mut Session, id: &str, required_tools: &[&str]) {
    session
        .append(EntryValue::SkillActivated {
            descriptor: ygg_agent::SkillDescriptor {
                id: id.into(),
                name: id.into(),
                description: "test active skill".into(),
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
    assert_eq!(
        discovered_preset_binding(preset, "gpt-future"),
        Some(("opencode", Protocol::OpenAiResponses))
    );
    assert_eq!(
        discovered_preset_binding(preset, "claude-future"),
        Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages))
    );
    assert_eq!(
        discovered_preset_binding(preset, "qwen3.7-plus"),
        Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages))
    );
    assert_eq!(discovered_preset_binding(preset, "gemini-future"), None);
    assert_eq!(
        discovered_preset_binding(preset, "kimi-future"),
        Some(("opencode", Protocol::OpenAiChat))
    );
}

#[test]
fn opencode_static_models_use_protocol_specific_endpoints() {
    let mut catalog = ModelCatalog::default();
    let preset = &crate::providers::OPENCODE;
    register_preset_endpoint(&mut catalog, preset, "YGG_TEST_OPENCODE_KEY").unwrap();
    register_opencode(&mut catalog, preset, "YGG_TEST_OPENCODE_KEY").unwrap();

    let responses = catalog
        .resolve(&ModelId("opencode/gpt-5.6-sol".into()))
        .unwrap();
    assert_eq!(responses.spec.protocol, Protocol::OpenAiResponses);
    assert_eq!(responses.endpoint.id.0, "opencode");
    assert_eq!(
        responses.endpoint.base_url.as_str(),
        "https://opencode.ai/zen/v1/"
    );

    let anthropic = catalog
        .resolve(&ModelId("opencode/claude-sonnet-4-6".into()))
        .unwrap();
    assert_eq!(anthropic.spec.protocol, Protocol::AnthropicMessages);
    assert_eq!(anthropic.endpoint.id.0, OPENCODE_ANTHROPIC_ENDPOINT_ID);
    assert_eq!(
        anthropic.endpoint.base_url.as_str(),
        "https://opencode.ai/zen/v1/"
    );

    let chat = catalog
        .resolve(&ModelId("opencode/deepseek-v4-pro".into()))
        .unwrap();
    assert_eq!(chat.spec.protocol, Protocol::OpenAiChat);
    assert_eq!(chat.endpoint.id.0, "opencode");
    assert!(catalog
        .resolve(&ModelId("opencode/gemini-3.1-pro".into()))
        .is_err());
}

#[test]
fn minimax_static_models_use_the_anthropic_protocol() {
    let mut catalog = ModelCatalog::default();
    let preset = &crate::providers::MINIMAX;
    register_preset_endpoint(&mut catalog, preset, "YGG_TEST_MINIMAX_KEY").unwrap();
    register_static_models(&mut catalog, preset.id, MINIMAX_MODELS).unwrap();

    let model = catalog
        .resolve(&ModelId("minimax/MiniMax-M3".into()))
        .unwrap();
    assert_eq!(model.spec.protocol, Protocol::AnthropicMessages);
    assert_eq!(model.endpoint.id.0, "minimax");
    assert_eq!(
        model.endpoint.base_url.as_str(),
        "https://api.minimax.io/anthropic/v1/"
    );
    assert!(model
        .spec
        .capabilities
        .input_modalities
        .contains(ygg_ai::Modality::Image));
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
    assert!(model_id_implies_vision("Qwen/Qwen2.5-VL-7B"));
    assert!(!model_id_implies_vision("Qwen/Qwen3-Coder-30B"));
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

    let models = openrouter_models_from_response(&response).unwrap();
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
fn openrouter_anthropic_routes_enable_anthropic_cache_markers() {
    let response = serde_json::json!({
        "data": [{
            "id": "anthropic/claude-sonnet-4-5",
            "context_length": 200_000,
            "top_provider": { "max_completion_tokens": 8_192 }
        }]
    });
    let models = openrouter_models_from_response(&response).unwrap();
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "access_token": access,
                "refresh_token": "refresh",
                "account_id": "acct_test"
            },
            "expires_at": u64::MAX
        }))
        .unwrap(),
    )
    .unwrap();
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
        assert_eq!(
            model.spec.limits.context_window,
            if model_id.starts_with("gpt-5.6-") {
                372_000
            } else {
                272_000
            }
        );
        assert_eq!(model.spec.limits.max_output_tokens, 128_000);
        assert!(model.spec.pricing.is_some());
        assert!(!model.spec.cache.supports_long_retention);
        assert!(!model.spec.cache.send_session_id_header);
        assert_eq!(
            model.spec.cache.session_affinity_format,
            Some(ygg_ai::SessionAffinityFormat::Codex)
        );
        assert_eq!(model.endpoint.transport, ygg_ai::EndpointTransport::Http);
    }
    let sol = catalog.resolve(&ModelId("gpt-5.6-sol".into())).unwrap();
    assert_eq!(crate::compaction::context_window(&sol), 372_000);

    // Pro is not in the fallback subscription catalog. Luna is included and
    // live account discovery can add or remove models independently of it.
    assert!(catalog.resolve(&ModelId("gpt-5.5-pro".into())).is_err());
    assert!(catalog.resolve(&ModelId("gpt-5.6-luna".into())).is_ok());
}

#[test]
fn offline_codex_registration_uses_account_cache_or_fallback_without_discovery() {
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
                    "max_output_tokens": 24_000
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
}

#[test]
fn codex_pro_mode_is_exposed_only_to_pro_oauth_accounts_and_gpt_5_6() {
    let directory = tempfile::tempdir().unwrap();

    let pro_path = directory.path().join("pro-codex.json");
    write_codex_credential(&pro_path, false, "pro");
    let mut pro_catalog = base_model_catalog(true).unwrap();
    register_openai_codex(
        &mut pro_catalog,
        crate::auth::codex::CredentialStore::new(pro_path),
        true,
    )
    .unwrap();
    let pro_model = pro_catalog.resolve(&ModelId("gpt-5.6-sol".into())).unwrap();
    assert!(
        pro_model
            .spec
            .capabilities
            .reasoning
            .as_ref()
            .unwrap()
            .supports_pro_mode
    );
    let older = pro_catalog.resolve(&ModelId("gpt-5.5".into())).unwrap();
    assert!(
        !older
            .spec
            .capabilities
            .reasoning
            .as_ref()
            .unwrap()
            .supports_pro_mode
    );

    let plus_path = directory.path().join("plus-codex.json");
    write_codex_credential(&plus_path, false, "plus");
    let mut plus_catalog = base_model_catalog(true).unwrap();
    register_openai_codex(
        &mut plus_catalog,
        crate::auth::codex::CredentialStore::new(plus_path),
        true,
    )
    .unwrap();
    let plus_model = plus_catalog
        .resolve(&ModelId("gpt-5.6-sol".into()))
        .unwrap();
    assert!(
        !plus_model
            .spec
            .capabilities
            .reasoning
            .as_ref()
            .unwrap()
            .supports_pro_mode
    );
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
fn codex_discovery_accepts_account_catalog_and_uses_live_metadata() {
    let body = serde_json::json!({
        "models": [
            {
                "slug": "gpt-5.6-luna",
                "context_window": 400_000,
                "max_output_tokens": 150_000,
                "supported_reasoning_levels": [
                    {"effort": "low"},
                    {"effort": "max"}
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
    assert_eq!(luna.context_window, 400_000);
    assert_eq!(luna.max_context_window, 400_000);
    assert_eq!(luna.max_output_tokens, 150_000);
    assert_eq!(luna.min_effort, ygg_ai::ReasoningEffort::Low);
    assert_eq!(luna.max_effort, ygg_ai::ReasoningEffort::Max);
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
fn codex_discovery_selects_default_or_max_window_from_oauth_plan() {
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
        1_000_000
    );
    assert_eq!(
        codex_models_from_response(&body, Some(&pro_lite)).unwrap()[0].context_window,
        1_000_000
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
    assert_eq!(apple.endpoint.id.0, "custom-apple-fm");
    assert_eq!(
        catalog.endpoint_label(&apple.endpoint.id),
        Some("Apple Foundation Models")
    );
    assert_eq!(
        apple.endpoint.base_url.as_str(),
        "http://127.0.0.1:1976/v1/"
    );
    assert!(matches!(apple.endpoint.auth, Auth::None));

    let home = catalog
        .resolve(&ModelId("custom/home-server/shared-model".into()))
        .unwrap();
    assert_eq!(home.endpoint.id.0, "custom-home-server");
    assert_eq!(
        catalog.endpoint_label(&home.endpoint.id),
        Some("Home Server")
    );
    assert!(matches!(
        home.endpoint.auth,
        Auth::BearerEnv { ref var } if var == "YGG_TEST_HOME_SERVER_KEY"
    ));
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
    assert_eq!(model.endpoint.id.0, DEEPSEEK_ENDPOINT_ID);
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
fn skill_requirements_use_the_filtered_core_and_extension_registry() {
    let directory = tempfile::tempdir().unwrap();
    let skill_root = directory.path().join(".ygg/skills/reviewer");
    std::fs::create_dir_all(&skill_root).unwrap();
    std::fs::write(
        skill_root.join("SKILL.md"),
        "---\nid: reviewer\nname: Reviewer\ndescription: Review code\n---\nReview carefully.",
    )
    .unwrap();
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
    assert_eq!(registered, vec!["read", "load_skill"]);

    let available = ygg_agent::SkillDescriptor {
        id: "available".into(),
        name: "Available".into(),
        description: "test".into(),
        version: None,
        source: ygg_agent::SkillSource::BuiltIn,
        trust: ygg_agent::SkillTrust::BuiltIn,
        required_tools: vec!["read".into(), "load_skill".into()],
        tags: vec![],
    };
    assert!(crate::resources::validate_skill_requirements(&available, &registered).is_ok());

    let unavailable = ygg_agent::SkillDescriptor {
        required_tools: vec!["edit".into()],
        ..available
    };
    assert!(matches!(
        crate::resources::validate_skill_requirements(&unavailable, &registered),
        Err(ygg_agent::SkillLoadError::MissingRequiredTools(missing)) if missing == vec!["edit"]
    ));
}

#[test]
fn initial_build_rejects_an_active_skill_missing_from_the_final_tool_registry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("active-skill.jsonl");
    let mut session = Session::create(&path).unwrap();
    append_active_skill(&mut session, "editor", &["edit"]);
    drop(session);

    let mut config = config(directory.path(), Some("gpt-4o-mini"));
    config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();
    let boot = bootstrap(config).unwrap();
    let error = match build_app(
        boot,
        LaunchSelection {
            model: ModelId("gpt-4o-mini".into()),
            session: SessionSelection::OpenExisting(path),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
        },
        "system".into(),
    ) {
        Ok(_) => panic!("an incompatible active skill must fail before the app starts"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("active skill \"editor\""), "{message}");
    assert!(
        message.contains("Missing required tools: [\"edit\"]"),
        "{message}"
    );
    assert!(message.contains("available tools: read"), "{message}");
    assert!(message.contains("sandbox capabilities"), "{message}");
}

#[test]
fn rebuild_revalidates_active_skills_after_the_tool_policy_changes() {
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

    let error = match rebuild_app(app, None, None, None, None) {
        Ok(_) => panic!("rebuild must revalidate persisted active skills"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("active skill \"editor\""), "{message}");
    assert!(
        message.contains("Missing required tools: [\"edit\"]"),
        "{message}"
    );
    assert!(message.contains("available tools: read"), "{message}");
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

    let error = validate_explicit_tool_policy(&config, &extensions, &model).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("edit, missing-extension"), "{message}");
    assert!(
        message.contains("allowlists, sandbox gates, and extension registration"),
        "{message}"
    );
    assert!(message.contains("available tools: read"), "{message}");
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
    let skills: Arc<dyn SkillRegistry> =
        Arc::new(FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], false).unwrap());
    let (extensions, _) = configured_extensions(
        skills.clone(),
        &default_config,
        &session,
        &model,
        &default_config.reasoning,
        &boot.sessions,
    );
    assert!(extensions.tool_definitions().is_empty());
    validate_explicit_tool_policy(&default_config, &extensions, &model).unwrap();

    default_config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();
    let explicit_session =
        Session::create(directory.path().join("no-tools-explicit.jsonl")).unwrap();
    let (extensions, _) = configured_extensions(
        skills,
        &default_config,
        &explicit_session,
        &model,
        &default_config.reasoning,
        &boot.sessions,
    );
    let error = validate_explicit_tool_policy(&default_config, &extensions, &model).unwrap_err();
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
