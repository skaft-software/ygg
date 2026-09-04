//! Canonical model-catalog registration for declared providers.
//!
//! This module owns endpoint construction and static model registration. It
//! receives provider declarations plus private auth handles and writes directly
//! into the single `ygg_ai::ModelCatalog`; it does not maintain a second model
//! registry or expose credentials to contributors.

#[cfg(test)]
use std::collections::HashSet;
use std::time::Duration;

use ygg_ai::{
    Auth, Capabilities, Endpoint, EndpointId, ModalitySet, ModelCatalog, ModelId, ModelLimits,
    ModelSpec, Protocol, ReasoningCapability, ReasoningControl,
};

use super::auth::{environment_auth, EnvironmentCredential};
use super::compatibility::cache_compatibility;
use super::contract::{valid_public_header, ProviderDeclaration, ProviderRoute};
use super::models::{static_models, StaticModelPreset};
use super::pricing::pricing_for;

/// Register every unique route endpoint for an environment-authenticated
/// declaration.
pub(crate) fn register_environment_endpoints(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    credential: &EnvironmentCredential,
    timeout: Duration,
) -> anyhow::Result<()> {
    for route in declaration.routes {
        let auth = environment_auth(route, credential)?;
        let route_base_url = declaration.resolved_route_base_url(route)?;
        register_endpoint(catalog, declaration, route, auth, route_base_url, timeout)?;
    }
    Ok(())
}

/// Register every unique route endpoint at a validated configuration override.
///
/// Declarations still own endpoint identity, auth presentation, headers,
/// transport, and runtime. This exists for providers whose documented setup
/// supports a non-secret local base-URL override.
pub(crate) fn register_environment_endpoints_at_base_url(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    credential: &EnvironmentCredential,
    base_url: &url::Url,
    timeout: Duration,
) -> anyhow::Result<()> {
    for route in declaration.routes {
        let auth = environment_auth(route, credential)?;
        let route_base_url = base_url.join(route.base_path)?;
        register_endpoint(catalog, declaration, route, auth, route_base_url, timeout)?;
    }
    Ok(())
}

/// Register every unique route endpoint with an authentication strategy owned by
/// a private credential lifecycle (AWS request signing or subscription OAuth).
pub(crate) fn register_private_endpoints_at_base_url(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    auth: Auth,
    base_url: &url::Url,
    timeout: Duration,
) -> anyhow::Result<()> {
    for route in declaration.routes {
        let route_base_url = base_url.join(route.base_path)?;
        register_endpoint(
            catalog,
            declaration,
            route,
            auth.clone(),
            route_base_url,
            timeout,
        )?;
    }
    Ok(())
}

/// Register every unique route endpoint with declaration-owned dynamic auth.
/// The dynamic resolver itself is private to the product authentication
/// lifecycle; this catalog layer only receives its opaque [`Auth`] handle.
pub(crate) fn register_dynamic_endpoints_at_base_url(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    auth: Auth,
    base_url: &url::Url,
    timeout: Duration,
) -> anyhow::Result<()> {
    for route in declaration.routes {
        if route.auth_presentation != super::contract::EndpointAuthPresentation::Dynamic {
            anyhow::bail!("dynamic credential declaration has a non-dynamic route");
        }
        let route_base_url = base_url.join(route.base_path)?;
        register_endpoint(
            catalog,
            declaration,
            route,
            auth.clone(),
            route_base_url,
            timeout,
        )?;
    }
    Ok(())
}

/// Register a declared endpoint with a privately owned authentication strategy.
fn register_endpoint(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    route: &ProviderRoute,
    auth: Auth,
    route_base_url: url::Url,
    timeout: Duration,
) -> anyhow::Result<()> {
    let endpoint_id = EndpointId(route.endpoint_id.into());
    if catalog.has_endpoint(&endpoint_id) {
        return Ok(());
    }
    catalog.register_endpoint(Endpoint {
        id: endpoint_id,
        base_url: route_base_url,
        auth,
        default_headers: public_headers(declaration.extra_headers)?,
        transport: route.transport,
        runtime: route.runtime,
        timeout,
    })?;
    Ok(())
}

/// Register declaration-owned static models into the canonical catalog.
pub(crate) fn register_static_models(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    for model in static_models(declaration.static_models) {
        register_static_model(catalog, declaration, model)?;
    }
    Ok(())
}

fn register_static_model(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    model: &StaticModelPreset,
) -> anyhow::Result<()> {
    let catalog_id = format!("{}/{}", declaration.id, model.id);
    if has_model_id(catalog, &catalog_id) {
        return Ok(());
    }
    let route = declaration.route_for_model(model.id).ok_or_else(|| {
        anyhow::anyhow!(
            "provider declaration has no route for static model {}/{}",
            declaration.id,
            model.id
        )
    })?;
    if route.protocol != model.protocol {
        anyhow::bail!(
            "provider declaration route protocol disagrees with static model {}/{}",
            declaration.id,
            model.id
        );
    }
    catalog.register_model(ModelSpec {
        id: ModelId(catalog_id),
        endpoint: EndpointId(route.endpoint_id.into()),
        api_name: model.id.into(),
        display_name: Some(model.name.into()),
        protocol: route.protocol,
        capabilities: static_model_capabilities(model),
        limits: ModelLimits {
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
        },
        pricing: pricing_for(declaration, model.id),
        cache: cache_compatibility(declaration.compatibility, model.id, route.protocol),
    })?;
    Ok(())
}

/// Register one discovery-produced model with a declaration-selected route.
/// Discovery parsers supply their provider-specific capabilities and limits;
/// route, pricing, and cache policy remain declaration-owned.
pub(crate) fn register_discovered_model(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    api_name: &str,
    display_name: Option<String>,
    capabilities: Capabilities,
    limits: ModelLimits,
    pricing: Option<ygg_ai::Pricing>,
) -> anyhow::Result<()> {
    let catalog_id = format!("{}/{}", declaration.id, api_name);
    if has_model_id(catalog, &catalog_id) {
        return Ok(());
    }
    let Some(route) = declaration.route_for_model(api_name) else {
        return Ok(());
    };
    catalog.register_model(ModelSpec {
        id: ModelId(catalog_id),
        endpoint: EndpointId(route.endpoint_id.into()),
        api_name: api_name.to_owned(),
        display_name,
        protocol: route.protocol,
        capabilities,
        limits,
        pricing: pricing.or_else(|| pricing_for(declaration, api_name)),
        cache: cache_compatibility(declaration.compatibility, api_name, route.protocol),
    })?;
    Ok(())
}

/// Copy declared public headers into a request map. The generated manifest
/// rejects credential-like header names; this remains a defensive boundary for
/// malformed checked-in data.
pub(crate) fn public_headers(
    headers: &'static [(&'static str, &'static str)],
) -> anyhow::Result<http::HeaderMap> {
    let mut output = http::HeaderMap::new();
    for (name, value) in headers {
        if !valid_public_header(name, value) {
            anyhow::bail!("provider declaration contains a credential-like or invalid header");
        }
        output.insert(
            http::HeaderName::from_bytes(name.as_bytes())?,
            http::HeaderValue::from_str(value)?,
        );
    }
    Ok(output)
}

fn static_model_capabilities(model: &StaticModelPreset) -> Capabilities {
    Capabilities {
        input_modalities: if model.vision {
            ModalitySet::none().with(ygg_ai::Modality::Image)
        } else {
            ModalitySet::none()
        },
        output_modalities: ModalitySet::none(),
        tools: true,
        parallel_tool_calls: model.protocol != Protocol::OpenAiChat,
        reasoning: model.reasoning.then_some(ReasoningCapability {
            control: ReasoningControl::Effort,
            exposes_text: true,
            preserves_state: model.protocol != Protocol::OpenAiChat,
            effort_budgets: None,
            openai_chat_mode: model.openai_chat_reasoning_profile.openai_chat_mode(),
            min_effort: ygg_ai::ReasoningEffort::Minimal,
            max_effort: model.max_reasoning_effort,
        }),
        responses_lite: false,
        agent_delegation: None,
        structured_output: !matches!(
            model.protocol,
            Protocol::OpenAiChat | Protocol::BedrockConverse
        ),
        deferred_tool_loading: false,
    }
}

fn has_model_id(catalog: &ModelCatalog, id: &str) -> bool {
    catalog.models().any(|model| model.id.0 == id)
}

/// Return all endpoint ids declared by a provider. Tests use this to ensure
/// static and discovery paths cannot silently invent a second registry.
#[cfg(test)]
pub(crate) fn declared_endpoint_ids(declaration: &ProviderDeclaration) -> HashSet<&'static str> {
    declaration
        .routes
        .iter()
        .map(|route| route.endpoint_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::contract::{
        CLOUDFLARE_AI_GATEWAY, CLOUDFLARE_WORKERS_AI, MINIMAX, MISTRAL, OPENCODE,
    };

    #[test]
    fn static_models_use_only_generated_route_endpoints() {
        let mut catalog = ModelCatalog::default();
        let credential = EnvironmentCredential::for_test("TEST_PROVIDER_KEY", "test-value");
        register_environment_endpoints(
            &mut catalog,
            &OPENCODE,
            &credential,
            Duration::from_secs(1),
        )
        .unwrap();
        register_static_models(&mut catalog, &OPENCODE).unwrap();
        let endpoints = declared_endpoint_ids(&OPENCODE);
        for model in catalog.models() {
            assert!(endpoints.contains(model.endpoint.0.as_str()));
        }
        let responses = catalog
            .resolve(&ModelId("opencode/gpt-5.6-sol".into()))
            .expect("OpenCode Responses model");
        assert_eq!(responses.spec.protocol, Protocol::OpenAiResponses);
        assert_eq!(responses.endpoint.id.0, "opencode");
        let anthropic = catalog
            .resolve(&ModelId("opencode/claude-sonnet-4-6".into()))
            .expect("OpenCode Messages model");
        assert_eq!(anthropic.spec.protocol, Protocol::AnthropicMessages);
        assert_eq!(anthropic.endpoint.id.0, "opencode-anthropic");
        let deepseek = catalog
            .resolve(&ModelId("opencode/deepseek-v4-pro".into()))
            .expect("OpenCode Chat model");
        assert_eq!(
            deepseek
                .spec
                .capabilities
                .reasoning
                .as_ref()
                .expect("reasoning capability")
                .openai_chat_mode,
            ygg_ai::OpenAiChatReasoningMode::DeepSeekThinking
        );

        let mut minimax = ModelCatalog::default();
        register_environment_endpoints(&mut minimax, &MINIMAX, &credential, Duration::from_secs(1))
            .unwrap();
        register_static_models(&mut minimax, &MINIMAX).unwrap();
        let minimax_m3 = minimax
            .resolve(&ModelId("minimax/MiniMax-M3".into()))
            .expect("MiniMax static model");
        assert_eq!(minimax_m3.spec.protocol, Protocol::AnthropicMessages);
        assert!(minimax_m3
            .spec
            .capabilities
            .input_modalities
            .contains(ygg_ai::Modality::Image));
    }

    #[test]
    fn alternate_provider_routes_keep_private_url_and_auth_details_in_catalog() {
        let credential = EnvironmentCredential::for_test("CLOUDFLARE_API_KEY", "test-value");
        let gateway_base =
            url::Url::parse("https://gateway.example.test/v1/account/gateway/").unwrap();
        let mut gateway = ModelCatalog::default();
        register_environment_endpoints_at_base_url(
            &mut gateway,
            &CLOUDFLARE_AI_GATEWAY,
            &credential,
            &gateway_base,
            Duration::from_secs(1),
        )
        .unwrap();
        register_static_models(&mut gateway, &CLOUDFLARE_AI_GATEWAY).unwrap();

        let claude = gateway
            .resolve(&ModelId("cloudflare-ai-gateway/claude-sonnet-4-5".into()))
            .unwrap();
        assert_eq!(claude.spec.protocol, Protocol::AnthropicMessages);
        assert_eq!(
            claude.endpoint.base_url.as_str(),
            "https://gateway.example.test/v1/account/gateway/anthropic/"
        );
        assert!(matches!(
            claude.endpoint.auth,
            Auth::HeaderBearerEnv { ref name, .. }
                if name == &http::HeaderName::from_static("cf-aig-authorization")
        ));

        let openai = gateway
            .resolve(&ModelId("cloudflare-ai-gateway/gpt-5.4".into()))
            .unwrap();
        assert_eq!(openai.spec.protocol, Protocol::OpenAiResponses);
        assert_eq!(
            openai.endpoint.base_url.as_str(),
            "https://gateway.example.test/v1/account/gateway/openai/"
        );
        let workers = gateway
            .resolve(&ModelId(
                "cloudflare-ai-gateway/workers-ai/@cf/moonshotai/kimi-k2.6".into(),
            ))
            .unwrap();
        assert_eq!(workers.spec.protocol, Protocol::OpenAiChat);
        assert_eq!(
            workers.endpoint.base_url.as_str(),
            "https://gateway.example.test/v1/account/gateway/compat/"
        );

        let mut mistral = ModelCatalog::default();
        register_environment_endpoints(&mut mistral, &MISTRAL, &credential, Duration::from_secs(1))
            .unwrap();
        register_static_models(&mut mistral, &MISTRAL).unwrap();
        let mistral_model = mistral
            .resolve(&ModelId("mistral/mistral-small-latest".into()))
            .unwrap();
        assert_eq!(
            mistral_model.endpoint.runtime.openai_chat_profile,
            ygg_ai::OpenAiChatRuntimeProfile::Mistral
        );

        let workers_base =
            url::Url::parse("https://workers.example.test/client/v4/accounts/account/ai/v1/")
                .unwrap();
        let mut workers_catalog = ModelCatalog::default();
        register_environment_endpoints_at_base_url(
            &mut workers_catalog,
            &CLOUDFLARE_WORKERS_AI,
            &credential,
            &workers_base,
            Duration::from_secs(1),
        )
        .unwrap();
        register_static_models(&mut workers_catalog, &CLOUDFLARE_WORKERS_AI).unwrap();
        assert!(workers_catalog
            .resolve(&ModelId(
                "cloudflare-workers-ai/@cf/openai/gpt-oss-120b".into(),
            ))
            .is_ok());
    }

    #[test]
    fn private_auth_static_models_use_the_declared_bedrock_endpoint() {
        use crate::providers::contract::BEDROCK;

        let mut catalog = ModelCatalog::default();
        let base_url = url::Url::parse(BEDROCK.base_url).unwrap();
        register_private_endpoints_at_base_url(
            &mut catalog,
            &BEDROCK,
            Auth::bearer("test-signer-secret"),
            &base_url,
            Duration::from_secs(1),
        )
        .unwrap();
        register_static_models(&mut catalog, &BEDROCK).unwrap();

        let model = catalog
            .resolve(&ModelId(
                "bedrock/anthropic.claude-3-7-sonnet-20250219-v1:0".into(),
            ))
            .expect("Bedrock static model");
        assert_eq!(model.spec.protocol, Protocol::BedrockConverse);
        assert!(!model.spec.capabilities.structured_output);
        assert_eq!(model.endpoint.id.0, "bedrock");
    }

    #[test]
    fn declaration_headers_reject_credential_carriers() {
        assert!(public_headers(&[("originator", "ygg")]).is_ok());
        assert!(public_headers(&[("x-provider-token", "value")]).is_err());
        assert!(public_headers(&[("bad:header", "value")]).is_err());
    }
}
