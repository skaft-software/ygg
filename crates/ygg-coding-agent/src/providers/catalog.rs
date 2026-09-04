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
    let base_url = url::Url::parse(declaration.base_url)?;
    register_environment_endpoints_at_base_url(catalog, declaration, credential, &base_url, timeout)
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
        register_endpoint(catalog, declaration, route, auth, base_url, timeout)?;
    }
    Ok(())
}

/// Register a declared endpoint with a privately owned authentication strategy.
fn register_endpoint(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    route: &ProviderRoute,
    auth: Auth,
    base_url: &url::Url,
    timeout: Duration,
) -> anyhow::Result<()> {
    let endpoint_id = EndpointId(route.endpoint_id.into());
    if catalog.has_endpoint(&endpoint_id) {
        return Ok(());
    }
    catalog.register_endpoint(Endpoint {
        id: endpoint_id,
        base_url: base_url.clone(),
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
        structured_output: model.protocol != Protocol::OpenAiChat,
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
    use crate::providers::contract::{MINIMAX, OPENCODE};

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
    fn declaration_headers_reject_credential_carriers() {
        assert!(public_headers(&[("originator", "ygg")]).is_ok());
        assert!(public_headers(&[("x-provider-token", "value")]).is_err());
        assert!(public_headers(&[("bad:header", "value")]).is_err());
    }
}
