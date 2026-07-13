//! Model catalog, configuration loading, and the embedded snapshot.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::auth::CredentialResolverRegistry;
use crate::error::ConfigError;
use crate::pricing::Pricing;
use crate::types::{
    Capabilities, Endpoint, EndpointId, ModelId, ModelLimits, ModelSpec, OpenAiChatReasoningMode,
    Protocol, ReasoningControl,
};

fn default_timeout_secs() -> u64 {
    30
}

/// Serialization shape for the complete catalog configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogConfig {
    /// List of endpoint configurations.
    pub endpoints: Vec<EndpointConfig>,
    /// List of model configurations.
    pub models: Vec<ModelConfig>,
}

/// Configuration for an endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// Endpoint identifier.
    pub id: EndpointId,
    /// Base URL of the endpoint (must trailing-slash).
    pub base_url: url::Url,
    /// Auth strategy for the endpoint.
    pub auth: AuthConfig,
    /// Default headers to apply to outgoing requests.
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    /// Request timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

/// Serialization configuration for auth credentials.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthConfig {
    /// No authentication.
    None,
    /// Bearer token referenced by an environment variable.
    BearerEnv {
        /// Env var name.
        var: String,
    },
    /// Custom header credentials referenced by an environment variable.
    HeaderEnv {
        /// Header name.
        name: String,
        /// Env var name.
        var: String,
    },
    /// Dynamic token resolver bound at load-time.
    Dynamic {
        /// Registry identifier for the resolver.
        resolver_id: String,
    },
}

/// Configuration for a model specification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model identifier.
    pub id: ModelId,
    /// Identifier of the endpoint this model uses.
    pub endpoint: EndpointId,
    /// Wire-level API model name.
    pub api_name: String,
    /// Protocol used to communicate with this model.
    pub protocol: Protocol,
    /// Capabilities of this model.
    pub capabilities: Capabilities,
    /// Model limits.
    pub limits: ModelLimits,
    /// Pricing rates for this model.
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

/// Resolved binding of a model specification and its destination endpoint.
#[derive(Clone)]
pub struct Model {
    /// Canonical model specification.
    pub spec: Arc<ModelSpec>,
    /// Destination endpoint configuration.
    pub endpoint: Arc<Endpoint>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("spec", &self.spec.id)
            .field("endpoint", &self.endpoint.id)
            .finish()
    }
}

/// Registry of models and endpoints.
#[derive(Clone, Default)]
pub struct ModelCatalog {
    endpoints: HashMap<EndpointId, Arc<Endpoint>>,
    models: HashMap<ModelId, Arc<ModelSpec>>,
}

impl ModelCatalog {
    /// Parse and validate the embedded JSON model catalog snapshot.
    pub fn builtin() -> Result<Self, ConfigError> {
        let raw = include_str!("../models/catalog.json");
        let cfg: CatalogConfig =
            serde_json::from_str(raw).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Self::from_config(cfg)
    }

    /// Loads configurations containing static or env-based auth.
    ///
    /// Returns `ConfigError::MissingCredentialResolver` if any dynamic auth is declared.
    pub fn from_config(cfg: CatalogConfig) -> Result<Self, ConfigError> {
        Self::from_config_with_resolvers(cfg, &HashMap::new())
    }

    /// Loads configurations resolving dynamic auth providers from the registry.
    pub fn from_config_with_resolvers(
        cfg: CatalogConfig,
        resolvers: &CredentialResolverRegistry,
    ) -> Result<Self, ConfigError> {
        let mut catalog = Self::default();

        for ep_cfg in cfg.endpoints {
            let endpoint = translate_endpoint(ep_cfg, resolvers)?;
            catalog.register_endpoint(endpoint)?;
        }

        for m_cfg in cfg.models {
            let spec = ModelSpec {
                id: m_cfg.id,
                endpoint: m_cfg.endpoint,
                api_name: m_cfg.api_name,
                protocol: m_cfg.protocol,
                capabilities: m_cfg.capabilities,
                limits: m_cfg.limits,
                pricing: m_cfg.pricing,
            };
            catalog.register_model(spec)?;
        }

        Ok(catalog)
    }

    /// Registers a new endpoint.
    pub fn register_endpoint(&mut self, endpoint: Endpoint) -> Result<(), ConfigError> {
        if self.endpoints.contains_key(&endpoint.id) {
            return Err(ConfigError::DuplicateEndpoint(endpoint.id));
        }
        validate_endpoint(&endpoint)?;
        self.endpoints
            .insert(endpoint.id.clone(), Arc::new(endpoint));
        Ok(())
    }

    /// Registers a new model specification.
    pub fn register_model(&mut self, mut spec: ModelSpec) -> Result<(), ConfigError> {
        if self.models.contains_key(&spec.id) {
            return Err(ConfigError::DuplicateModel(spec.id.clone()));
        }
        if !self.endpoints.contains_key(&spec.endpoint) {
            return Err(ConfigError::UnknownEndpoint(spec.endpoint.clone()));
        }

        // Pricing is immutable once the spec is stored in the catalog. Keep
        // tiers canonical here so every response cost calculation can iterate
        // without cloning or sorting.
        if let Some(pricing) = spec.pricing.as_mut() {
            pricing
                .tiers
                .sort_unstable_by_key(|tier| tier.min_input_tokens);
        }
        validate_model_spec(&spec)?;

        self.models.insert(spec.id.clone(), Arc::new(spec));
        Ok(())
    }

    /// Resolves a Model ID into its endpoint binding.
    pub fn resolve(&self, id: &ModelId) -> Result<Model, ConfigError> {
        let spec = self
            .models
            .get(id)
            .ok_or_else(|| ConfigError::UnknownModel(id.clone()))?;
        let endpoint = self
            .endpoints
            .get(&spec.endpoint)
            .ok_or_else(|| ConfigError::UnknownEndpoint(spec.endpoint.clone()))?;

        Ok(Model {
            spec: spec.clone(),
            endpoint: endpoint.clone(),
        })
    }

    /// Returns an iterator over all registered model specifications.
    pub fn models(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models.values().map(|m| m.as_ref())
    }
}

pub(crate) fn validate_model_spec(spec: &ModelSpec) -> Result<(), ConfigError> {
    if spec.api_name.is_empty()
        || !spec.capabilities.input_modalities.is_valid()
        || !spec.capabilities.output_modalities.is_valid()
        || spec.limits.context_window == 0
        || spec.limits.max_output_tokens == 0
        || spec.limits.max_output_tokens > spec.limits.context_window
    {
        return Err(ConfigError::InvalidModel(spec.id.clone()));
    }

    if let Some(reasoning) = &spec.capabilities.reasoning {
        let valid = match (reasoning.control, reasoning.effort_budgets) {
            (ReasoningControl::TokenBudget, Some(budgets)) => {
                budgets.minimal >= 1024
                    && budgets.minimal <= budgets.low
                    && budgets.low <= budgets.medium
                    && budgets.medium <= budgets.high
                    && budgets.high <= spec.limits.max_output_tokens
            }
            (ReasoningControl::Effort, None) => true,
            _ => false,
        };
        let protocol_matches = match spec.protocol {
            Protocol::AnthropicMessages => reasoning.control == ReasoningControl::TokenBudget,
            Protocol::OpenAiChat | Protocol::OpenAiResponses => {
                reasoning.control == ReasoningControl::Effort
            }
        };
        let chat_mode_matches = reasoning.openai_chat_mode == OpenAiChatReasoningMode::Standard
            || (spec.protocol == Protocol::OpenAiChat
                && reasoning.control == ReasoningControl::Effort
                && reasoning.exposes_text);
        if !valid || !protocol_matches || !chat_mode_matches {
            return Err(ConfigError::InvalidReasoningConfig(spec.id.clone()));
        }
    }
    if let Some(pricing) = &spec.pricing {
        let mut thresholds = std::collections::HashSet::new();
        if pricing
            .tiers
            .iter()
            .any(|tier| !thresholds.insert(tier.min_input_tokens))
            || pricing
                .tiers
                .windows(2)
                .any(|pair| pair[0].min_input_tokens > pair[1].min_input_tokens)
        {
            return Err(ConfigError::InvalidPricing(spec.id.clone()));
        }
    }
    Ok(())
}

pub(crate) fn validate_endpoint(endpoint: &Endpoint) -> Result<(), ConfigError> {
    validate_base_url(&endpoint.base_url)?;
    if endpoint.timeout.is_zero() {
        return Err(ConfigError::InvalidTimeout(endpoint.id.clone()));
    }
    if let Some(auth_header) = crate::auth::auth_header_name(&endpoint.auth) {
        if endpoint.default_headers.contains_key(&auth_header) {
            return Err(ConfigError::AuthHeaderCollision(auth_header));
        }
    }
    Ok(())
}

fn validate_base_url(url: &url::Url) -> Result<(), ConfigError> {
    if !url.cannot_be_a_base()
        && (url.scheme() == "http" || url.scheme() == "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path().ends_with('/')
    {
        Ok(())
    } else {
        Err(ConfigError::InvalidBaseUrl(url.to_string()))
    }
}

fn resolve_auth(
    auth_cfg: AuthConfig,
    resolvers: &CredentialResolverRegistry,
) -> Result<crate::auth::Auth, ConfigError> {
    match auth_cfg {
        AuthConfig::None => Ok(crate::auth::Auth::None),
        AuthConfig::BearerEnv { var } => Ok(crate::auth::Auth::bearer_env(var)),
        AuthConfig::HeaderEnv { name, var } => {
            let header_name = http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| ConfigError::InvalidHeader(e.to_string()))?;
            Ok(crate::auth::Auth::header_env(header_name, var))
        }
        AuthConfig::Dynamic { resolver_id } => {
            let resolver = resolvers
                .get(&resolver_id)
                .ok_or_else(|| ConfigError::MissingCredentialResolver(resolver_id.clone()))?;
            Ok(crate::auth::Auth::dynamic(resolver.clone()))
        }
    }
}

fn translate_endpoint(
    cfg: EndpointConfig,
    resolvers: &CredentialResolverRegistry,
) -> Result<Endpoint, ConfigError> {
    validate_base_url(&cfg.base_url)?;
    if cfg.timeout_secs == 0 {
        return Err(ConfigError::InvalidTimeout(cfg.id));
    }

    let auth = resolve_auth(cfg.auth, resolvers)?;

    let mut default_headers = http::HeaderMap::new();
    for (k, v) in cfg.default_headers {
        let name = http::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| ConfigError::InvalidHeader(e.to_string()))?;
        let value = http::HeaderValue::from_str(&v)
            .map_err(|e| ConfigError::InvalidHeader(e.to_string()))?;
        default_headers.insert(name, value);
    }

    // Auth header collision check
    if let Some(auth_hdr) = crate::auth::auth_header_name(&auth) {
        if default_headers.contains_key(&auth_hdr) {
            return Err(ConfigError::AuthHeaderCollision(auth_hdr));
        }
    }

    Ok(Endpoint {
        id: cfg.id,
        base_url: cfg.base_url,
        auth,
        default_headers,
        timeout: std::time::Duration::from_secs(cfg.timeout_secs),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::CredentialResolver;

    #[test]
    fn test_builtin_catalog_loads_and_resolves() {
        let cat = ModelCatalog::builtin().unwrap();
        let model = cat.resolve(&ModelId("gpt-4o-mini".to_string())).unwrap();
        assert_eq!(model.spec.api_name, "gpt-4o-mini");
        assert_eq!(model.endpoint.id.0, "openai");
    }

    #[test]
    fn test_invalid_base_url_fails() {
        let endpoints = vec![EndpointConfig {
            id: EndpointId("invalid".to_string()),
            // Base URL has query parameter, which is forbidden
            base_url: url::Url::parse("https://api.openai.com/v1/?query=1").unwrap(),
            auth: AuthConfig::None,
            default_headers: BTreeMap::new(),
            timeout_secs: 10,
        }];

        let cfg = CatalogConfig {
            endpoints,
            models: vec![],
        };
        assert!(matches!(
            ModelCatalog::from_config(cfg),
            Err(ConfigError::InvalidBaseUrl(_))
        ));

        // Missing trailing slash
        let endpoints_slash = vec![EndpointConfig {
            id: EndpointId("invalid".to_string()),
            base_url: url::Url::parse("https://api.openai.com/v1").unwrap(),
            auth: AuthConfig::None,
            default_headers: BTreeMap::new(),
            timeout_secs: 10,
        }];
        let cfg_slash = CatalogConfig {
            endpoints: endpoints_slash,
            models: vec![],
        };
        assert!(matches!(
            ModelCatalog::from_config(cfg_slash),
            Err(ConfigError::InvalidBaseUrl(_))
        ));
    }

    #[test]
    fn test_auth_header_collision() {
        let mut default_headers = BTreeMap::new();
        // insert authorization header name, which will collide with BearerEnv
        default_headers.insert("authorization".to_string(), "Bearer foo".to_string());

        let cfg = CatalogConfig {
            endpoints: vec![EndpointConfig {
                id: EndpointId("ep".to_string()),
                base_url: url::Url::parse("https://api.openai.com/v1/").unwrap(),
                auth: AuthConfig::BearerEnv {
                    var: "KEY".to_string(),
                },
                default_headers,
                timeout_secs: 10,
            }],
            models: vec![],
        };
        assert!(matches!(
            ModelCatalog::from_config(cfg),
            Err(ConfigError::AuthHeaderCollision(_))
        ));
    }

    struct DummyResolver;
    #[async_trait::async_trait]
    impl CredentialResolver for DummyResolver {
        async fn resolve(
            &self,
        ) -> Result<crate::auth::ResolvedCredential, crate::error::AuthError> {
            // This resolver only needs to be *bound* during catalog loading; the
            // loading tests never call `resolve()`. Return a deterministic error
            // rather than panicking during an unexpected test invocation.
            Err(crate::error::AuthError::InvalidCredential)
        }
    }

    #[test]
    fn test_dynamic_resolver_loading() {
        let cfg = CatalogConfig {
            endpoints: vec![EndpointConfig {
                id: EndpointId("ep".to_string()),
                base_url: url::Url::parse("https://api.openai.com/v1/").unwrap(),
                auth: AuthConfig::Dynamic {
                    resolver_id: "dyn_id".to_string(),
                },
                default_headers: BTreeMap::new(),
                timeout_secs: 10,
            }],
            models: vec![],
        };

        // fails through from_config (no resolvers supplied)
        assert!(matches!(
            ModelCatalog::from_config(cfg.clone()),
            Err(ConfigError::MissingCredentialResolver(_))
        ));

        // succeeds when resolvers supplied
        let mut resolvers = CredentialResolverRegistry::new();
        resolvers.insert("dyn_id".to_string(), Arc::new(DummyResolver));
        assert!(ModelCatalog::from_config_with_resolvers(cfg, &resolvers).is_ok());
    }
}
