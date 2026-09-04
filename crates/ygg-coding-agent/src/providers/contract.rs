//! Provider-neutral declarations and setup-facing catalog contracts.
//!
//! Declarations describe public provider behavior only: codec families, model
//! discovery, compatibility, pricing policy, and setup requirements. Runtime
//! credentials and credential stores deliberately live behind the private auth
//! lifecycle module.

use std::fmt;

use ygg_ai::{
    ConfigError, EndpointTransport, ModelCatalog, OpenAiChatRuntimeProfile, Protocol,
    RequestBodyEncoding, RequestRuntime, ResponsesRuntimeProfile,
};

/// Authentication setup advertised by a provider declaration.
///
/// This deliberately contains identifiers and setup instructions rather than a
/// token, header value, resolver, or credential-store path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderAuthentication {
    /// One of the listed environment variables supplies an API credential.
    Environment {
        /// Variables checked in priority order by the private auth lifecycle.
        variables: &'static [&'static str],
    },
    /// A product-owned AWS credential chain supplies a request signer.
    Aws {
        /// Environment variables that document the standard AWS setup surface.
        /// The private resolver also supports profiles and task/instance metadata.
        variables: &'static [&'static str],
    },
    /// Application Default Credentials resolved by the private auth lifecycle.
    ApplicationDefaultCredentials,
    /// A product-owned subscription login supplies the dynamic credential.
    Subscription {
        /// Stable login selector shown in setup diagnostics.
        login: &'static str,
    },
    /// An embedding host owns sign-in and supplies a dynamic request credential.
    ///
    /// This keeps host-managed OAuth state out of Ygg's command-line login and
    /// credential stores while still making the setup boundary explicit.
    HostOwned {
        /// Stable host integration identifier shown in setup diagnostics.
        integration: &'static str,
    },
}

impl ProviderAuthentication {
    pub(crate) fn environment_variables(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Environment { variables } => Some(variables),
            Self::Aws { .. }
            | Self::ApplicationDefaultCredentials
            | Self::Subscription { .. }
            | Self::HostOwned { .. } => None,
        }
    }
}

/// How a declaration obtains model metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDiscovery {
    /// Use only the declaration's checked-in static models.
    Static,
    /// Query an OpenAI-compatible `GET /models` resource.
    OpenAiModels {
        /// Filter applied to returned API model identifiers.
        filter: ModelFilter,
    },
    /// Query an Anthropic-compatible `GET /models` resource.
    AnthropicModels {
        /// Filter applied to returned API model identifiers.
        filter: ModelFilter,
    },
    /// Query OpenRouter's catalog and retain its pricing metadata.
    OpenRouterModels,
    /// Query the DeepSeek-compatible inventory with its declared fallback
    /// metadata.
    DeepSeekModels,
    /// Query the authenticated Codex subscription catalog.
    CodexSubscription,
    /// Ask an embedding host for an authenticated subscription inventory.
    ///
    /// The host owns the OAuth state and transport; declarations only define
    /// the credential-free route and codec families.
    HostOwnedSubscription,
    /// Do not populate models automatically.
    None,
}

/// Filter applied to OpenAI-compatible model inventories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFilter {
    /// Accept every returned model id.
    All,
    /// Accept model ids beginning with any listed prefix.
    Prefix(&'static [&'static str]),
}

/// Conservative capability fallback for sparse model inventories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryCapabilityProfile {
    /// Trust inventory metadata plus Ygg's model-family fallback.
    Default,
    /// Treat documented GPT multimodal families as image-capable when sparse
    /// inventory metadata omits modalities.
    GptVisionFallback,
    /// Treat the provider's discovered Messages models as image-capable.
    AssumeImageInput,
}

impl DiscoveryCapabilityProfile {
    /// Whether a sparse API model should receive the GPT image fallback.
    pub(crate) fn gpt_vision_fallback(self, model_id: &str) -> bool {
        let model_id = model_id.rsplit('/').next().unwrap_or(model_id);
        matches!(self, Self::GptVisionFallback)
            && (model_id.starts_with("gpt-4o")
                || model_id.starts_with("gpt-4.1")
                || model_id.starts_with("gpt-5")
                || model_id.starts_with("gpt-6"))
    }

    /// Whether sparse models are assumed to accept image input.
    pub(crate) fn assumes_image_input(self) -> bool {
        matches!(self, Self::AssumeImageInput)
    }
}

/// Checked-in static model family owned by a declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticModelSet {
    /// No static models are supplied.
    None,
    /// Kimi Coding's supported Anthropic-compatible routes.
    KimiCoding,
    /// MiniMax's supported Anthropic-compatible routes.
    MiniMax,
    /// MiniMax China's supported Anthropic-compatible routes.
    MiniMaxChina,
    /// OpenCode Zen's supported routes.
    OpenCode,
    /// OpenCode Zen Go's compatible OpenAI Chat routes.
    OpenCodeGo,
    /// Vercel AI Gateway's Anthropic-compatible starter routes.
    VercelAiGateway,
    /// Xiaomi regional token-plan Anthropic-compatible routes.
    XiaomiTokenPlan,
    /// Mistral Chat Completions models.
    Mistral,
    /// Cloudflare Workers AI's OpenAI-compatible models.
    CloudflareWorkersAi,
    /// Cloudflare AI Gateway provider-routed models.
    CloudflareAiGateway,
    /// Amazon Bedrock Converse models with published conservative limits.
    Bedrock,
    /// Google Gemini and Vertex models using the native generateContent API.
    Google,
}

/// Whether a discovery cache is required before a provider can expose models or
/// may be refreshed as a supplemental catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryCacheMode {
    /// A missing or negative inventory does not expose discovery-only models.
    Required,
    /// Static models remain visible while discovery refreshes in the background.
    Supplemental,
}

/// Compatibility profile selected by a provider declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatibilityProfile {
    /// Conservative protocol defaults.
    Default,
    /// OpenAI Responses cache affinity.
    OpenAi,
    /// OpenRouter cache affinity and Anthropic cache controls.
    OpenRouter,
    /// Routes that reject long cache retention.
    ShortRetention,
    /// Fireworks' mixed OpenAI/Anthropic compatibility behavior.
    Fireworks,
    /// OpenCode's route-specific cache compatibility behavior.
    OpenCode,
    /// Google generateContent routes do not support Ygg cache-affinity headers.
    Google,
    /// User-configured endpoint metadata.
    Custom,
    /// Codex subscription cache affinity.
    Codex,
    /// Mistral's `x-affinity` prompt-cache routing.
    Mistral,
    /// Cloudflare's OpenAI-compatible Workers and Gateway routes.
    Cloudflare,
}

/// Pricing policy selected separately from provider availability.
///
/// A subscription route can expose price metadata for accounting while its
/// visibility remains controlled exclusively by authentication and catalog
/// setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PricingProfile {
    /// Checked-in provider-scoped reference pricing when available.
    Reference,
    /// OpenAI rate overrides plus checked-in reference pricing.
    OpenAi,
    /// Anthropic rate overrides plus checked-in reference pricing.
    Anthropic,
    /// DeepSeek rate overrides plus checked-in reference pricing.
    DeepSeek,
    /// MiniMax rate overrides plus checked-in reference pricing.
    MiniMax,
    /// OpenCode rate overrides plus checked-in reference pricing.
    OpenCode,
    /// Google model-rate overrides for Generative AI and Vertex routes.
    Google,
    /// Discovery-provided OpenRouter pricing plus checked-in fallback pricing.
    OpenRouter,
    /// User-configured pricing, defaulting to zero for local/self-hosted routes.
    Custom,
    /// Codex accounting metadata; not an availability signal.
    Subscription,
    /// Mistral's checked-in public rates.
    Mistral,
    /// Cloudflare Workers AI's checked-in public rates.
    CloudflareWorkersAi,
    /// Cloudflare AI Gateway's provider-routed rates.
    CloudflareAiGateway,
}

/// Secret-free credential presentation selected by an endpoint route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointAuthPresentation {
    /// Send the private environment credential as a bearer token.
    Bearer,
    /// Send the private environment credential in `x-api-key`.
    ApiKeyHeader,
    /// Forward the private environment credential through Cloudflare AI
    /// Gateway's `cf-aig-authorization: Bearer <token>` header.
    CloudflareAiGateway,
    /// Send the private environment credential in a declaration-selected header.
    Header(&'static str),
    /// Sign the exact request with a private AWS SigV4 credential chain.
    AwsSigV4,
    /// Send the private environment credential in `x-goog-api-key`.
    GoogleApiKeyHeader,
    /// Bind a private dynamic resolver owned by the authentication lifecycle.
    Dynamic,
}

/// One endpoint route using an existing `ygg-ai` codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderRoute {
    /// Endpoint identity stored in the canonical model catalog.
    pub endpoint_id: &'static str,
    /// Relative path appended to the declaration's resolved base URL.
    ///
    /// It is empty for ordinary endpoints and lets a gateway expose distinct
    /// provider protocol routes without exposing resolved URLs publicly.
    pub base_path: &'static str,
    /// Existing codec family selected by this route.
    pub protocol: Protocol,
    /// Secret-free presentation of the privately resolved credential.
    pub auth_presentation: EndpointAuthPresentation,
    /// Preferred streaming transport.
    pub transport: EndpointTransport,
    /// Endpoint-specific request runtime behavior.
    pub runtime: RequestRuntime,
}

/// Data-driven model-to-route selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRouteRule {
    /// Do not register an exact unsupported model identifier.
    ExcludeExact(&'static str),
    /// Do not register model identifiers with this prefix.
    #[allow(dead_code)] // Retained for accepted `exclude_prefix` manifest rules.
    ExcludePrefix(&'static str),
    /// Select a route for model identifiers with this prefix.
    SelectPrefix {
        /// Prefix to match.
        prefix: &'static str,
        /// Index into [`ProviderDeclaration::routes`].
        route: usize,
    },
    /// Select a route for identifiers containing a case-insensitive fragment.
    SelectAsciiInsensitiveContains {
        /// Lowercase fragment to match.
        fragment: &'static str,
        /// Index into [`ProviderDeclaration::routes`].
        route: usize,
    },
    /// Select a route for identifiers with both a prefix and suffix.
    SelectPrefixAndSuffix {
        /// Prefix to match.
        prefix: &'static str,
        /// Suffix to match.
        suffix: &'static str,
        /// Index into [`ProviderDeclaration::routes`].
        route: usize,
    },
    /// Fallback route. Every nonempty declaration has exactly one final default.
    Default {
        /// Index into [`ProviderDeclaration::routes`].
        route: usize,
    },
}

/// Non-secret endpoint construction selected by a built-in declaration.
///
/// This remains internal setup metadata and is intentionally omitted from
/// [`ProviderDefinition`], which never exposes endpoint URLs or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderRuntimeConfiguration {
    /// Use the declaration's fixed base URL.
    Default,
    /// Build a regional Amazon Bedrock Runtime endpoint and use SigV4.
    AwsBedrock,
    /// Build an Azure OpenAI Responses endpoint from resource/deployment setup.
    AzureOpenAi,
}

/// Data-only built-in provider declaration.
///
/// It contains no credential material. The private auth lifecycle translates a
/// declaration and a credential source into an `ygg_ai::Auth` only immediately
/// before endpoint and discovery work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderDeclaration {
    /// Stable provider identity used for model namespaces and inventory caches.
    pub id: &'static str,
    /// Human-readable provider label.
    pub name: &'static str,
    /// Versioned inference base URL.
    pub base_url: &'static str,
    /// Environment identifiers substituted into `{IDENTIFIER}` URL path
    /// placeholders at private catalog-registration time.
    pub base_url_environment: &'static [&'static str],
    /// Setup and authentication lifecycle kind.
    pub authentication: ProviderAuthentication,
    /// Non-secret endpoint construction selected by this declaration.
    pub runtime_configuration: ProviderRuntimeConfiguration,
    /// Model inventory source.
    pub model_discovery: ModelDiscovery,
    /// Conservative capability behavior for sparse inventory responses.
    pub discovery_capabilities: DiscoveryCapabilityProfile,
    /// Optional checked-in static model set.
    pub static_models: StaticModelSet,
    /// Discovery cache policy.
    pub inventory_cache: InventoryCacheMode,
    /// Supported endpoint routes.
    pub routes: &'static [ProviderRoute],
    /// Ordered model-to-route rules.
    pub route_rules: &'static [ModelRouteRule],
    /// Public, non-secret headers attached to every request.
    pub extra_headers: &'static [(&'static str, &'static str)],
    /// Compatibility metadata profile.
    pub compatibility: CompatibilityProfile,
    /// Pricing policy, independent from authentication availability.
    pub pricing: PricingProfile,
}

impl ProviderDeclaration {
    /// Resolve an API model identifier to a route without inspecting provider
    /// identity in the caller.
    pub fn route_for_model(&self, model_id: &str) -> Option<&ProviderRoute> {
        for rule in self.route_rules {
            match *rule {
                ModelRouteRule::ExcludeExact(value) if model_id == value => return None,
                ModelRouteRule::ExcludePrefix(value) if model_id.starts_with(value) => return None,
                ModelRouteRule::SelectPrefix { prefix, route } if model_id.starts_with(prefix) => {
                    return self.routes.get(route);
                }
                ModelRouteRule::SelectAsciiInsensitiveContains { fragment, route }
                    if contains_ascii_insensitive(model_id, fragment) =>
                {
                    return self.routes.get(route);
                }
                ModelRouteRule::SelectPrefixAndSuffix {
                    prefix,
                    suffix,
                    route,
                } if model_id.starts_with(prefix) && model_id.ends_with(suffix) => {
                    return self.routes.get(route);
                }
                ModelRouteRule::Default { route } => return self.routes.get(route),
                _ => {}
            }
        }
        None
    }

    /// Resolve a provider-advertised codec family to a declared route.
    ///
    /// Host-owned discovery uses this when each returned model carries its
    /// protocol explicitly instead of relying on a model-name heuristic.
    pub(crate) fn route_for_protocol(&self, protocol: Protocol) -> Option<&ProviderRoute> {
        self.routes.iter().find(|route| route.protocol == protocol)
    }

    /// Route whose declared fallback authentication is used for model
    /// inventory discovery.
    pub(crate) fn inventory_route(&self) -> Option<&ProviderRoute> {
        self.route_rules.iter().find_map(|rule| match *rule {
            ModelRouteRule::Default { route } => self.routes.get(route),
            _ => None,
        })
    }

    /// Return the data-only public definition used by custom and extension
    /// consumers. Endpoint URLs, headers, and credentials stay out of this
    /// contract.
    pub fn definition(&self) -> ProviderDefinition {
        ProviderDefinition {
            id: self.id.to_owned(),
            label: self.name.to_owned(),
            authentication: match self.authentication {
                ProviderAuthentication::Environment { variables }
                | ProviderAuthentication::Aws { variables } => ProviderAccess::Environment {
                    variables: variables.iter().map(|value| (*value).to_owned()).collect(),
                },
                ProviderAuthentication::ApplicationDefaultCredentials => {
                    ProviderAccess::ApplicationDefaultCredentials
                }
                ProviderAuthentication::Subscription { login } => ProviderAccess::Subscription {
                    login: login.to_owned(),
                },
                ProviderAuthentication::HostOwned { integration } => ProviderAccess::HostOwned {
                    integration: integration.to_owned(),
                },
            },
            catalog: ProviderCatalogKind::from(self.model_discovery),
            routes: self
                .routes
                .iter()
                .map(|route| ProviderRouteDefinition {
                    endpoint_id: route.endpoint_id.to_owned(),
                    protocol: route.protocol,
                    transport: route.transport,
                    runtime: route.runtime,
                })
                .collect(),
            compatibility: self.compatibility,
            pricing: self.pricing,
        }
    }

    /// Resolve the private runtime base URL. Placeholder values are validated as
    /// opaque URL-path identifiers and are never retained in public definitions
    /// or error messages.
    pub(crate) fn resolved_base_url(&self) -> Result<url::Url, ConfigError> {
        self.resolve_base_url_with(ygg_ai::auth::read_bounded_env)
    }

    fn resolve_base_url_with(
        &self,
        mut read_environment: impl FnMut(&str) -> Result<Option<String>, ConfigError>,
    ) -> Result<url::Url, ConfigError> {
        let mut rendered = self.base_url.to_owned();
        for variable in self.base_url_environment {
            let value = read_environment(variable)
                .map_err(|_| invalid_template_environment_error())?
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| ConfigError::MissingEnv((*variable).to_owned()))?;
            if !valid_url_path_identifier(&value) {
                return Err(invalid_template_environment_error());
            }
            let placeholder = format!("{{{variable}}}");
            if !rendered.contains(&placeholder) {
                return Err(ConfigError::InvalidBaseUrl(
                    "provider base URL template is invalid".to_owned(),
                ));
            }
            rendered = rendered.replace(&placeholder, &value);
        }
        if rendered.contains('{') || rendered.contains('}') {
            return Err(ConfigError::InvalidBaseUrl(
                "provider base URL template is invalid".to_owned(),
            ));
        }
        let url = url::Url::parse(&rendered)
            .map_err(|_| ConfigError::InvalidBaseUrl("provider base URL is invalid".to_owned()))?;
        if valid_base_url(&url) {
            Ok(url)
        } else {
            Err(ConfigError::InvalidBaseUrl(
                "provider base URL is invalid".to_owned(),
            ))
        }
    }

    /// Resolve the private runtime URL for one declared provider route.
    pub(crate) fn resolved_route_base_url(
        &self,
        route: &ProviderRoute,
    ) -> Result<url::Url, ConfigError> {
        self.resolved_base_url()?
            .join(route.base_path)
            .map_err(|_| ConfigError::InvalidBaseUrl("provider route URL is invalid".to_owned()))
    }

    /// Validate a declaration before it is used to construct catalog entries.
    pub fn validate(&self) -> Result<(), ProviderDefinitionError> {
        validate_identifier(self.id, "provider")?;
        if !valid_provider_label(self.name) {
            return Err(ProviderDefinitionError::new("provider label is invalid"));
        }
        validate_base_url_template(self.base_url, self.base_url_environment)?;
        match self.authentication {
            ProviderAuthentication::Environment { variables }
            | ProviderAuthentication::Aws { variables } => {
                if variables.is_empty() || variables.iter().any(|value| !valid_env_name(value)) {
                    return Err(ProviderDefinitionError::new(
                        "provider credential environment declaration is invalid",
                    ));
                }
            }
            ProviderAuthentication::ApplicationDefaultCredentials => {}
            ProviderAuthentication::Subscription { login } if !valid_provider_identifier(login) => {
                return Err(ProviderDefinitionError::new(
                    "provider subscription login declaration is invalid",
                ));
            }
            ProviderAuthentication::HostOwned { integration }
                if !valid_provider_identifier(integration) =>
            {
                return Err(ProviderDefinitionError::new(
                    "provider host integration declaration is invalid",
                ));
            }
            ProviderAuthentication::Subscription { .. }
            | ProviderAuthentication::HostOwned { .. } => {}
        }
        if self.routes.is_empty() {
            return Err(ProviderDefinitionError::new("provider has no routes"));
        }
        for (index, route) in self.routes.iter().enumerate() {
            validate_identifier(route.endpoint_id, "endpoint")?;
            if !valid_route_base_path(route.base_path) {
                return Err(ProviderDefinitionError::new(
                    "provider route base path is invalid",
                ));
            }
            if route.runtime.openai_chat_profile != OpenAiChatRuntimeProfile::Default
                && route.protocol != Protocol::OpenAiChat
            {
                return Err(ProviderDefinitionError::new(
                    "provider OpenAI Chat runtime profile requires a Chat route",
                ));
            }
            if let EndpointAuthPresentation::Header(name) = route.auth_presentation {
                if !name.bytes().all(is_http_token_byte) || name.is_empty() {
                    return Err(ProviderDefinitionError::new(
                        "provider credential header declaration is invalid",
                    ));
                }
            }
            if route.runtime.responses_profile != ResponsesRuntimeProfile::Default
                && route.protocol != Protocol::OpenAiResponses
            {
                return Err(ProviderDefinitionError::new(
                    "provider Responses runtime profile requires a Responses route",
                ));
            }
            if route.transport == EndpointTransport::WebSocketPreferred
                && route.protocol != Protocol::OpenAiResponses
            {
                return Err(ProviderDefinitionError::new(
                    "provider WebSocket transport requires a Responses route",
                ));
            }
            let presentation_is_valid = matches!(
                (self.authentication, route.auth_presentation),
                (
                    ProviderAuthentication::Environment { .. },
                    EndpointAuthPresentation::Bearer
                        | EndpointAuthPresentation::ApiKeyHeader
                        | EndpointAuthPresentation::CloudflareAiGateway
                        | EndpointAuthPresentation::Header(_)
                        | EndpointAuthPresentation::GoogleApiKeyHeader
                ) | (
                    ProviderAuthentication::Aws { .. },
                    EndpointAuthPresentation::AwsSigV4
                ) | (
                    ProviderAuthentication::ApplicationDefaultCredentials
                        | ProviderAuthentication::Subscription { .. }
                        | ProviderAuthentication::HostOwned { .. },
                    EndpointAuthPresentation::Dynamic
                )
            );
            if !presentation_is_valid {
                return Err(ProviderDefinitionError::new(
                    "provider credential presentation is invalid",
                ));
            }
            for previous in &self.routes[..index] {
                if previous.endpoint_id == route.endpoint_id
                    && (previous.base_path != route.base_path
                        || previous.auth_presentation != route.auth_presentation
                        || previous.transport != route.transport
                        || previous.runtime != route.runtime)
                {
                    return Err(ProviderDefinitionError::new(
                        "provider endpoint routes disagree on runtime configuration",
                    ));
                }
            }
        }
        if self
            .extra_headers
            .iter()
            .any(|(name, value)| !valid_public_header(name, value))
        {
            return Err(ProviderDefinitionError::new(
                "provider declaration contains a credential-like or invalid header",
            ));
        }
        let mut default_seen = false;
        for (index, rule) in self.route_rules.iter().enumerate() {
            let route = match *rule {
                ModelRouteRule::ExcludeExact(_) | ModelRouteRule::ExcludePrefix(_) => continue,
                ModelRouteRule::SelectPrefix { route, .. }
                | ModelRouteRule::SelectAsciiInsensitiveContains { route, .. }
                | ModelRouteRule::SelectPrefixAndSuffix { route, .. }
                | ModelRouteRule::Default { route } => route,
            };
            if route >= self.routes.len() {
                return Err(ProviderDefinitionError::new(
                    "provider route rule is invalid",
                ));
            }
            if matches!(rule, ModelRouteRule::Default { .. }) {
                if default_seen || index + 1 != self.route_rules.len() {
                    return Err(ProviderDefinitionError::new(
                        "provider route default must be final and unique",
                    ));
                }
                default_seen = true;
            }
        }
        if !default_seen {
            return Err(ProviderDefinitionError::new(
                "provider route default is missing",
            ));
        }
        Ok(())
    }
}

/// Public, credential-free provider definition for custom and extension
/// consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDefinition {
    id: String,
    label: String,
    authentication: ProviderAccess,
    catalog: ProviderCatalogKind,
    routes: Vec<ProviderRouteDefinition>,
    compatibility: CompatibilityProfile,
    pricing: PricingProfile,
}

impl ProviderDefinition {
    /// Build a custom OpenAI-compatible declaration without accepting any
    /// credential material.
    pub fn custom(
        id: impl Into<String>,
        label: impl Into<String>,
        endpoint_id: impl Into<String>,
    ) -> Result<Self, ProviderDefinitionError> {
        Self::new(
            id,
            label,
            ProviderAccess::Custom,
            ProviderCatalogKind::Custom,
            vec![ProviderRouteDefinition {
                endpoint_id: endpoint_id.into(),
                protocol: Protocol::OpenAiChat,
                transport: EndpointTransport::Http,
                runtime: RequestRuntime::default(),
            }],
            CompatibilityProfile::Custom,
            PricingProfile::Custom,
        )
    }

    /// Build an extension-owned declaration without accepting host credentials.
    pub fn extension(
        id: impl Into<String>,
        label: impl Into<String>,
        endpoint_id: impl Into<String>,
        protocol: Protocol,
    ) -> Result<Self, ProviderDefinitionError> {
        Self::new(
            id,
            label,
            ProviderAccess::Extension,
            ProviderCatalogKind::Extension,
            vec![ProviderRouteDefinition {
                endpoint_id: endpoint_id.into(),
                protocol,
                transport: EndpointTransport::Http,
                runtime: RequestRuntime::default(),
            }],
            CompatibilityProfile::Default,
            PricingProfile::Reference,
        )
    }

    /// Provider identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-facing provider label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Public setup classification.
    pub fn authentication(&self) -> &ProviderAccess {
        &self.authentication
    }

    /// Public model-catalog classification.
    pub fn catalog(&self) -> ProviderCatalogKind {
        self.catalog
    }

    /// Public routes, without URL/header/credential data.
    pub fn routes(&self) -> &[ProviderRouteDefinition] {
        &self.routes
    }

    /// Compatibility policy selected by this declaration.
    pub fn compatibility(&self) -> CompatibilityProfile {
        self.compatibility
    }

    /// Pricing policy selected independently from availability.
    pub fn pricing(&self) -> PricingProfile {
        self.pricing
    }

    fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        authentication: ProviderAccess,
        catalog: ProviderCatalogKind,
        routes: Vec<ProviderRouteDefinition>,
        compatibility: CompatibilityProfile,
        pricing: PricingProfile,
    ) -> Result<Self, ProviderDefinitionError> {
        let definition = Self {
            id: id.into(),
            label: label.into(),
            authentication,
            catalog,
            routes,
            compatibility,
            pricing,
        };
        definition.validate()?;
        Ok(definition)
    }

    fn validate(&self) -> Result<(), ProviderDefinitionError> {
        validate_identifier(&self.id, "provider")?;
        if !valid_provider_label(&self.label) {
            return Err(ProviderDefinitionError::new("provider label is invalid"));
        }
        if self.routes.is_empty() {
            return Err(ProviderDefinitionError::new("provider has no routes"));
        }
        for route in &self.routes {
            validate_identifier(&route.endpoint_id, "endpoint")?;
        }
        Ok(())
    }
}

/// Public setup classification with no credential payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderAccess {
    /// An extension or user can show the accepted environment variable names.
    Environment {
        /// Variables checked by the product-owned auth lifecycle.
        variables: Vec<String>,
    },
    /// Application Default Credentials are resolved from trusted local files.
    ApplicationDefaultCredentials,
    /// The provider is available after the named login is complete.
    Subscription {
        /// Login selector.
        login: String,
    },
    /// An embedding host owns sign-in, token storage, and refresh.
    HostOwned {
        /// Stable identifier for the embedding integration.
        integration: String,
    },
    /// A custom credential configuration owns setup.
    Custom,
    /// An extension owns setup and credentials.
    Extension,
    /// No credential is required.
    None,
}

/// Public catalog classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderCatalogKind {
    /// Checked-in static models.
    Static,
    /// OpenAI-compatible discovery.
    OpenAiCompatible,
    /// Anthropic-compatible discovery.
    AnthropicCompatible,
    /// OpenRouter's catalog schema.
    OpenRouter,
    /// Authenticated subscription discovery.
    Subscription,
    /// User-managed custom discovery.
    Custom,
    /// Extension-managed discovery.
    Extension,
    /// No automatic discovery.
    None,
}

impl From<ModelDiscovery> for ProviderCatalogKind {
    fn from(value: ModelDiscovery) -> Self {
        match value {
            ModelDiscovery::Static => Self::Static,
            ModelDiscovery::OpenAiModels { .. } | ModelDiscovery::DeepSeekModels => {
                Self::OpenAiCompatible
            }
            ModelDiscovery::AnthropicModels { .. } => Self::AnthropicCompatible,
            ModelDiscovery::OpenRouterModels => Self::OpenRouter,
            ModelDiscovery::CodexSubscription | ModelDiscovery::HostOwnedSubscription => {
                Self::Subscription
            }
            ModelDiscovery::None => Self::None,
        }
    }
}

/// Public route description without endpoint URLs, default headers, or auth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRouteDefinition {
    endpoint_id: String,
    protocol: Protocol,
    transport: EndpointTransport,
    runtime: RequestRuntime,
}

impl ProviderRouteDefinition {
    /// Catalog endpoint identity.
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// Existing codec family selected by this route.
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Preferred streaming transport.
    pub fn transport(&self) -> EndpointTransport {
        self.transport
    }

    /// Endpoint request-runtime metadata.
    pub fn runtime(&self) -> RequestRuntime {
        self.runtime
    }
}

/// Actionable, bounded setup diagnostic that contains no credential value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDiagnostic {
    provider_id: String,
    provider_label: String,
    action: String,
}

impl ProviderDiagnostic {
    /// Construct a missing-environment diagnostic from a credential-free
    /// declaration.
    pub fn missing_environment(definition: &ProviderDefinition) -> Self {
        let variables = match definition.authentication() {
            ProviderAccess::Environment { variables } => variables.join(" or "),
            _ => "the documented credential environment variable".to_owned(),
        };
        Self {
            provider_id: definition.id().to_owned(),
            provider_label: definition.label().to_owned(),
            action: bounded_setup_text(&format!("set {variables}")),
        }
    }

    /// Construct a subscription-login diagnostic from a credential-free
    /// declaration.
    pub fn login_required(definition: &ProviderDefinition) -> Self {
        let action = match definition.authentication() {
            ProviderAccess::Subscription { login } => format!("run ygg --login {login}"),
            ProviderAccess::HostOwned { integration } => {
                format!("complete {integration} sign-in in the embedding host")
            }
            _ => "complete provider sign-in".to_owned(),
        };
        Self {
            provider_id: definition.id().to_owned(),
            provider_label: definition.label().to_owned(),
            action: bounded_setup_text(&action),
        }
    }

    /// Construct a bounded setup action. Callers must pass a setup instruction,
    /// never a credential or provider response body.
    pub fn setup_action(definition: &ProviderDefinition, action: impl AsRef<str>) -> Self {
        Self {
            provider_id: definition.id().to_owned(),
            provider_label: definition.label().to_owned(),
            action: bounded_setup_text(action.as_ref()),
        }
    }

    /// Stable provider identity.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Human-facing provider label.
    pub fn provider_label(&self) -> &str {
        &self.provider_label
    }

    /// Bounded next action.
    pub fn action(&self) -> &str {
        &self.action
    }
}

impl fmt::Display for ProviderDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} is not available: {}",
            self.provider_label, self.action
        )
    }
}

/// Credential-free availability status used by setup, doctor, custom-provider,
/// and extension-provider consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderAvailability {
    /// Authentication and setup are sufficient to build catalog entries.
    Available,
    /// Models must stay absent until this action is completed.
    Unavailable(ProviderDiagnostic),
}

/// Consumer contract for custom and extension providers.
///
/// The host gives contributors only the canonical [`ModelCatalog`]. A
/// contributor owns its own credential lifecycle and cannot receive another
/// provider's credential through this interface.
pub trait ProviderCatalogContributor {
    /// Public, credential-free definition.
    fn definition(&self) -> &ProviderDefinition;
    /// Current availability and actionable setup state.
    fn availability(&self) -> ProviderAvailability;
    /// Register directly into Ygg's one canonical model catalog.
    fn register_models(&self, catalog: &mut ModelCatalog) -> anyhow::Result<()>;
}

/// Validation error for a public or generated provider declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDefinitionError {
    message: &'static str,
}

impl ProviderDefinitionError {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for ProviderDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderDefinitionError {}

fn validate_identifier(value: &str, kind: &str) -> Result<(), ProviderDefinitionError> {
    if valid_provider_identifier(value) {
        Ok(())
    } else if kind == "endpoint" {
        Err(ProviderDefinitionError::new(
            "provider endpoint id is invalid",
        ))
    } else {
        Err(ProviderDefinitionError::new("provider id is invalid"))
    }
}

fn valid_provider_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
}

fn valid_provider_label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn valid_version_query(url: &url::Url) -> bool {
    let Some(query) = url.query() else {
        return true;
    };
    query.len() <= 128
        && url.query_pairs().next().is_some_and(|(name, value)| {
            name == "api-version"
                && !value.is_empty()
                && value.len() <= 96
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        })
        && url.query_pairs().nth(1).is_none()
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'_')
}

fn valid_base_url(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && valid_version_query(url)
        && url.fragment().is_none()
        && url.path().ends_with('/')
}

fn validate_base_url_template(
    base_url: &str,
    environment: &[&str],
) -> Result<(), ProviderDefinitionError> {
    let mut rendered = base_url.to_owned();
    let mut seen = std::collections::HashSet::new();
    for variable in environment {
        let placeholder = format!("{{{variable}}}");
        if !valid_env_name(variable)
            || !seen.insert(*variable)
            || rendered.matches(&placeholder).count() != 1
        {
            return Err(ProviderDefinitionError::new(
                "provider base URL template is invalid",
            ));
        }
        rendered = rendered.replace(&placeholder, "placeholder");
    }
    if rendered.contains('{') || rendered.contains('}') {
        return Err(ProviderDefinitionError::new(
            "provider base URL template is invalid",
        ));
    }
    let url = url::Url::parse(&rendered)
        .map_err(|_| ProviderDefinitionError::new("provider base URL is invalid"))?;
    if valid_base_url(&url) {
        Ok(())
    } else {
        Err(ProviderDefinitionError::new("provider base URL is invalid"))
    }
}

fn valid_url_path_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn invalid_template_environment_error() -> ConfigError {
    ConfigError::InvalidBaseUrl("provider base URL environment value is invalid".to_owned())
}

fn valid_route_base_path(path: &str) -> bool {
    path.is_empty()
        || (path.ends_with('/')
            && !path.starts_with('/')
            && path
                .split('/')
                .filter(|segment| !segment.is_empty())
                .all(|segment| {
                    segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
                }))
}

/// Check that a declaration header is a bounded public request header rather
/// than an authentication or credential carrier.
pub(crate) fn valid_public_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(is_http_token_byte)
        && value.len() <= 1024
        && value.is_ascii()
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && !credential_like_header(name)
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub(crate) fn credential_like_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let compact: String = lower
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(char::from)
        .collect();
    lower.contains("auth")
        || compact.contains("key")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("credential")
        || lower.contains("cookie")
        || lower.contains("password")
}

fn contains_ascii_insensitive(value: &str, fragment: &str) -> bool {
    value
        .as_bytes()
        .windows(fragment.len())
        .any(|candidate| candidate.eq_ignore_ascii_case(fragment.as_bytes()))
}

fn bounded_setup_text(value: &str) -> String {
    const MAX_BYTES: usize = 512;
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > MAX_BYTES {
            break;
        }
        output.push(character);
    }
    output.trim().to_owned()
}

include!(concat!(env!("OUT_DIR"), "/provider_declarations.rs"));

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    use serde::Deserialize;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use ygg_ai::{
        AiClient, Auth, AwsCredentials, AwsSigV4Signer, CacheRetention, Capabilities,
        CompatibilityMode, Message, ModalitySet, Model, ModelCatalog, ModelId, ModelLimits,
        OutputFormat, OutputModalities, ReasoningConfig, ReasoningMode, Request, ToolChoice,
        UserMessage, UserPart,
    };

    use super::*;
    use crate::providers::auth::EnvironmentCredential;
    use crate::providers::catalog::{
        register_discovered_model, register_dynamic_endpoints_at_base_url,
        register_environment_endpoints_at_base_url, register_private_endpoints_at_base_url,
        register_static_models,
    };

    const PINNED_PI_PROVIDER_IDS: &[&str] = &[
        "amazon-bedrock",
        "anthropic",
        "ant-ling",
        "azure-openai-responses",
        "cerebras",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "deepseek",
        "fireworks",
        "github-copilot",
        "google",
        "google-vertex",
        "groq",
        "huggingface",
        "kimi-coding",
        "minimax",
        "minimax-cn",
        "mistral",
        "moonshotai",
        "moonshotai-cn",
        "openai",
        "openai-codex",
        "opencode",
        "opencode-go",
        "openrouter",
        "vercel-ai-gateway",
        "xai",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "zai",
    ];

    #[derive(Debug, Deserialize)]
    struct PiProviderInventory {
        schema_version: u8,
        pi_package: String,
        expected_provider_ids: Vec<String>,
        providers: Vec<PiProviderFixture>,
    }

    #[derive(Debug, Deserialize)]
    struct PiProviderFixture {
        id: String,
        decision: PiProviderDecision,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum PiProviderDecision {
        Declared {
            fixture_id: String,
            provider_id: String,
            fixture: PiRouteFixture,
        },
        DeclaredSubset {
            fixture_id: String,
            provider_id: String,
            fixture: PiRouteFixture,
            excluded_surfaces: Vec<String>,
            missing_primitive: String,
            release_blocker: String,
        },
        Unsupported {
            fixture_id: String,
            missing_primitive: String,
            release_blocker: String,
            legacy_declaration: Option<String>,
        },
    }

    #[derive(Debug, Deserialize, Clone)]
    struct PiRouteFixture {
        registration: String,
        model_id: String,
        protocol: String,
        endpoint_id: String,
        auth_presentation: String,
        #[serde(default)]
        auth_header: Option<String>,
        base_url: String,
        #[serde(default)]
        configured_base_url: Option<String>,
        environment_variable: String,
    }

    fn fixture_protocol(value: &str) -> Protocol {
        match value {
            "anthropic_messages" => Protocol::AnthropicMessages,
            "openai_chat" => Protocol::OpenAiChat,
            "openai_responses" => Protocol::OpenAiResponses,
            "bedrock_converse" => Protocol::BedrockConverse,
            "google_generative_ai" => Protocol::GoogleGenerativeAi,
            other => panic!("unknown fixture protocol: {other}"),
        }
    }

    fn fixture_auth_presentation_matches(
        route: EndpointAuthPresentation,
        fixture: &PiRouteFixture,
    ) -> bool {
        match fixture.auth_presentation.as_str() {
            "api_key_header" => route == EndpointAuthPresentation::ApiKeyHeader,
            "aws_sigv4" => route == EndpointAuthPresentation::AwsSigV4,
            "bearer" => route == EndpointAuthPresentation::Bearer,
            "cloudflare_ai_gateway" => route == EndpointAuthPresentation::CloudflareAiGateway,
            "dynamic" => route == EndpointAuthPresentation::Dynamic,
            "google_api_key_header" => route == EndpointAuthPresentation::GoogleApiKeyHeader,
            "header" => matches!(
                route,
                EndpointAuthPresentation::Header(name)
                    if fixture.auth_header.as_deref() == Some(name)
            ),
            other => panic!("unknown fixture auth presentation: {other}"),
        }
    }

    fn fixture_capabilities() -> Capabilities {
        Capabilities {
            input_modalities: ModalitySet::none(),
            output_modalities: ModalitySet::none(),
            tools: true,
            parallel_tool_calls: true,
            reasoning: None,
            responses_lite: false,
            agent_delegation: None,
            structured_output: false,
            deferred_tool_loading: false,
        }
    }

    fn fixture_request() -> Request {
        Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("fixture request".to_owned())],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            // Subset routes intentionally exercise the portable request shape;
            // their legacy max-token profiles are recorded as exclusions.
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ReasoningMode::Standard,
            responses: None,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: CacheRetention::None,
            session_id: None,
        }
    }

    fn fixture_resolved_base_url(
        declaration: &ProviderDeclaration,
        fixture_id: &str,
        fixture: &PiRouteFixture,
    ) -> url::Url {
        let mut rendered = fixture
            .configured_base_url
            .as_deref()
            .unwrap_or(&fixture.base_url)
            .to_owned();
        for (index, variable) in declaration.base_url_environment.iter().enumerate() {
            let placeholder = format!("{{{variable}}}");
            assert!(
                rendered.contains(&placeholder),
                "{fixture_id}: declaration base URL template lost {placeholder}"
            );
            rendered = rendered.replace(&placeholder, &format!("fixture-identifier-{index}"));
        }
        url::Url::parse(&rendered)
            .unwrap_or_else(|error| panic!("{fixture_id}: invalid resolved fixture URL: {error}"))
    }

    fn fixture_route_base_url(
        base_url: &url::Url,
        route: &ProviderRoute,
        fixture_id: &str,
    ) -> url::Url {
        base_url
            .join(route.base_path)
            .unwrap_or_else(|error| panic!("{fixture_id}: invalid fixture route URL: {error}"))
    }

    fn fixture_endpoint_url(base_url: &url::Url, suffix: &str, fixture_id: &str) -> url::Url {
        let query = base_url.query().map(str::to_owned);
        let mut url = base_url
            .join(suffix)
            .unwrap_or_else(|error| panic!("{fixture_id}: invalid fixture request URL: {error}"));
        url.set_query(query.as_deref());
        url
    }

    fn fixture_request_url(
        base_url: &url::Url,
        route: &ProviderRoute,
        fixture_id: &str,
        fixture: &PiRouteFixture,
    ) -> url::Url {
        let route_base_url = fixture_route_base_url(base_url, route, fixture_id);
        match fixture.protocol.as_str() {
            "anthropic_messages" => fixture_endpoint_url(&route_base_url, "messages", fixture_id),
            "openai_chat" => fixture_endpoint_url(&route_base_url, "chat/completions", fixture_id),
            "openai_responses" => fixture_endpoint_url(&route_base_url, "responses", fixture_id),
            "google_generative_ai" => route_base_url
                .join(&format!(
                    "models/{}:streamGenerateContent?alt=sse",
                    fixture.model_id
                ))
                .unwrap_or_else(|error| {
                    panic!("{fixture_id}: invalid Google fixture request URL: {error}")
                }),
            "bedrock_converse" => {
                let mut url = route_base_url;
                {
                    let mut segments = url.path_segments_mut().unwrap_or_else(|_| {
                        panic!("{fixture_id}: Bedrock fixture URL cannot carry path segments")
                    });
                    segments.pop_if_empty();
                    segments.push("model");
                    segments.push(&fixture.model_id);
                    segments.push("converse-stream");
                }
                url
            }
            other => panic!("unknown fixture protocol: {other}"),
        }
    }

    fn fixture_base_at_server(server: &MockServer, base_url: &url::Url) -> url::Url {
        let mut output = url::Url::parse(&server.uri()).expect("wiremock URL");
        output.set_path(base_url.path());
        output.set_query(base_url.query());
        output
    }

    struct FixtureStreamResponse {
        body: Vec<u8>,
        response_id: Option<&'static str>,
        content_type: &'static str,
    }

    fn bedrock_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn bedrock_fixture_frame(headers: &[(&str, &str)], payload: serde_json::Value) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7);
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let payload = serde_json::to_vec(&payload).expect("Bedrock fixture JSON");
        let total = 16 + header_bytes.len() + payload.len();
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(&(total as u32).to_be_bytes());
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&bedrock_crc32(&bytes).to_be_bytes());
        bytes.extend_from_slice(&header_bytes);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&bedrock_crc32(&bytes).to_be_bytes());
        bytes
    }

    fn bedrock_fixture_stream() -> Vec<u8> {
        [
            bedrock_fixture_frame(
                &[(":message-type", "event"), (":event-type", "messageStart")],
                serde_json::json!({"role": "assistant"}),
            ),
            bedrock_fixture_frame(
                &[
                    (":message-type", "event"),
                    (":event-type", "contentBlockStart"),
                ],
                serde_json::json!({"contentBlockIndex": 0, "start": {}}),
            ),
            bedrock_fixture_frame(
                &[
                    (":message-type", "event"),
                    (":event-type", "contentBlockDelta"),
                ],
                serde_json::json!({"contentBlockIndex": 0, "delta": {"text": "fixture"}}),
            ),
            bedrock_fixture_frame(
                &[
                    (":message-type", "event"),
                    (":event-type", "contentBlockStop"),
                ],
                serde_json::json!({"contentBlockIndex": 0}),
            ),
            bedrock_fixture_frame(
                &[(":message-type", "event"), (":event-type", "messageStop")],
                serde_json::json!({"stopReason": "end_turn"}),
            ),
            bedrock_fixture_frame(
                &[(":message-type", "event"), (":event-type", "metadata")],
                serde_json::json!({"usage": {"inputTokens": 2, "outputTokens": 1, "totalTokens": 3}}),
            ),
        ]
        .concat()
    }

    fn fixture_stream_response(protocol: &str) -> FixtureStreamResponse {
        const ANTHROPIC: &str = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"fixture-anthropic\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"fixture\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        const OPENAI_CHAT: &str = concat!(
            "data: {\"id\":\"fixture-openai-chat\",\"choices\":[{\"delta\":{\"content\":\"fixture\"}}]}\n\n",
            "data: {\"id\":\"fixture-openai-chat\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        const OPENAI_RESPONSES: &str = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-openai-responses\"}}\n\n",
            "data: {\"type\":\"response.content_part.added\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"fixture\"}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"content_index\":0}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-openai-responses\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n",
        );
        const GOOGLE: &str = "data: {\"responseId\":\"fixture-google\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"fixture\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":1,\"totalTokenCount\":3}}\n\n";

        match protocol {
            "anthropic_messages" => FixtureStreamResponse {
                body: ANTHROPIC.as_bytes().to_vec(),
                response_id: Some("fixture-anthropic"),
                content_type: "text/event-stream",
            },
            "openai_chat" => FixtureStreamResponse {
                body: OPENAI_CHAT.as_bytes().to_vec(),
                response_id: Some("fixture-openai-chat"),
                content_type: "text/event-stream",
            },
            "openai_responses" => FixtureStreamResponse {
                body: OPENAI_RESPONSES.as_bytes().to_vec(),
                response_id: Some("fixture-openai-responses"),
                content_type: "text/event-stream",
            },
            "bedrock_converse" => FixtureStreamResponse {
                body: bedrock_fixture_stream(),
                response_id: None,
                content_type: "application/vnd.amazon.eventstream",
            },
            "google_generative_ai" => FixtureStreamResponse {
                body: GOOGLE.as_bytes().to_vec(),
                response_id: Some("fixture-google"),
                content_type: "text/event-stream",
            },
            other => panic!("unknown fixture protocol: {other}"),
        }
    }

    fn fixture_auth(fixture: &PiRouteFixture) -> Auth {
        match fixture.auth_presentation.as_str() {
            "api_key_header" => {
                Auth::header(http::HeaderName::from_static("x-api-key"), "fixture-secret")
            }
            "aws_sigv4" => {
                let credentials =
                    AwsCredentials::new("fixture-access-key", "fixture-secret-key", None)
                        .expect("valid fixture AWS credentials");
                let signer = AwsSigV4Signer::new(credentials, "us-east-1", "bedrock")
                    .expect("valid fixture Bedrock signer")
                    .with_clock(Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_700_000_000)));
                Auth::request_signer(Arc::new(signer))
            }
            "bearer" | "dynamic" => Auth::bearer("fixture-secret"),
            "cloudflare_ai_gateway" => Auth::header(
                http::HeaderName::from_static("cf-aig-authorization"),
                "Bearer fixture-secret",
            ),
            "google_api_key_header" => Auth::header(
                http::HeaderName::from_static("x-goog-api-key"),
                "fixture-secret",
            ),
            "header" => {
                let name = fixture
                    .auth_header
                    .as_deref()
                    .expect("header fixture requires an auth header");
                Auth::header(
                    http::HeaderName::from_bytes(name.as_bytes())
                        .expect("valid fixture auth header"),
                    "fixture-secret",
                )
            }
            other => panic!("request fixture does not support auth presentation: {other}"),
        }
    }

    fn assert_fixture_authentication(
        request: &wiremock::Request,
        fixture_id: &str,
        fixture: &PiRouteFixture,
    ) {
        if fixture.auth_presentation == "aws_sigv4" {
            let authorization = request
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_else(|| panic!("{fixture_id}: missing SigV4 authorization"));
            assert!(
                authorization.starts_with("AWS4-HMAC-SHA256 Credential=fixture-access-key/"),
                "{fixture_id}: request authentication presentation drifted"
            );
            assert!(
                authorization.contains("/us-east-1/bedrock/aws4_request"),
                "{fixture_id}: SigV4 scope drifted"
            );
            assert!(
                request.headers.contains_key("x-amz-date")
                    && request.headers.contains_key("x-amz-content-sha256"),
                "{fixture_id}: SigV4 request headers drifted"
            );
            return;
        }

        let (header, expected) = match fixture.auth_presentation.as_str() {
            "api_key_header" => ("x-api-key", "fixture-secret".to_owned()),
            "bearer" | "dynamic" => ("authorization", "Bearer fixture-secret".to_owned()),
            "cloudflare_ai_gateway" => ("cf-aig-authorization", "Bearer fixture-secret".to_owned()),
            "google_api_key_header" => ("x-goog-api-key", "fixture-secret".to_owned()),
            "header" => (
                fixture
                    .auth_header
                    .as_deref()
                    .expect("header fixture requires an auth header"),
                "fixture-secret".to_owned(),
            ),
            other => panic!("{fixture_id}: unsupported request auth presentation {other}"),
        };
        assert_eq!(
            request
                .headers
                .get(header)
                .and_then(|value| value.to_str().ok()),
            Some(expected.as_str()),
            "{fixture_id}: request authentication presentation drifted"
        );
    }

    fn assert_fixture_request_body(
        request: &wiremock::Request,
        fixture_id: &str,
        fixture: &PiRouteFixture,
    ) {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("fixture request JSON");
        match fixture.protocol.as_str() {
            "anthropic_messages" | "openai_chat" | "openai_responses" => {
                assert_eq!(
                    body["model"].as_str(),
                    Some(fixture.model_id.as_str()),
                    "{fixture_id}: request model drifted"
                );
                assert_eq!(
                    body["stream"].as_bool(),
                    Some(true),
                    "{fixture_id}: request must use streaming transport"
                );
            }
            "google_generative_ai" => {
                assert!(
                    body["contents"]
                        .as_array()
                        .is_some_and(|contents| !contents.is_empty()),
                    "{fixture_id}: Google request contents drifted"
                );
                assert!(
                    body.get("model").is_none() && body.get("stream").is_none(),
                    "{fixture_id}: Google model and streaming must be encoded in the route"
                );
            }
            "bedrock_converse" => {
                assert_eq!(
                    body["messages"][0]["role"].as_str(),
                    Some("user"),
                    "{fixture_id}: Bedrock request messages drifted"
                );
                assert!(
                    body["inferenceConfig"].is_object(),
                    "{fixture_id}: Bedrock inference configuration drifted"
                );
                assert!(
                    body.get("model").is_none() && body.get("stream").is_none(),
                    "{fixture_id}: Bedrock model and streaming must be encoded in the route"
                );
            }
            other => panic!("unknown fixture protocol: {other}"),
        }
    }

    fn register_fixture_model(
        declaration: &ProviderDeclaration,
        fixture_id: &str,
        fixture: &PiRouteFixture,
        base_url: &url::Url,
    ) -> Model {
        let credential = EnvironmentCredential::for_test("TEST_PROVIDER_KEY", "fixture-value");
        let mut catalog = ModelCatalog::default();
        match declaration.authentication {
            ProviderAuthentication::Environment { .. } => {
                register_environment_endpoints_at_base_url(
                    &mut catalog,
                    declaration,
                    &credential,
                    base_url,
                    Duration::from_secs(1),
                )
            }
            ProviderAuthentication::Aws { .. } => register_private_endpoints_at_base_url(
                &mut catalog,
                declaration,
                Auth::bearer("fixture-bootstrap-secret"),
                base_url,
                Duration::from_secs(1),
            ),
            ProviderAuthentication::ApplicationDefaultCredentials => {
                register_dynamic_endpoints_at_base_url(
                    &mut catalog,
                    declaration,
                    Auth::bearer("fixture-bootstrap-secret"),
                    base_url,
                    Duration::from_secs(1),
                )
            }
            ProviderAuthentication::Subscription { .. } | ProviderAuthentication::HostOwned { .. } => {
                panic!("{fixture_id}: subscription and host-owned fixtures do not register a local endpoint")
            }
        }
        .unwrap_or_else(|error| panic!("{fixture_id}: endpoint registration failed: {error}"));

        match fixture.registration.as_str() {
            "static" => register_static_models(&mut catalog, declaration).unwrap_or_else(|error| {
                panic!("{fixture_id}: static model registration failed: {error}")
            }),
            "configured" | "discovered" => register_discovered_model(
                &mut catalog,
                declaration,
                &fixture.model_id,
                None,
                fixture_capabilities(),
                ModelLimits {
                    context_window: 1_024,
                    max_output_tokens: 256,
                },
                None,
            )
            .unwrap_or_else(|error| panic!("{fixture_id}: model registration failed: {error}")),
            other => panic!("{fixture_id}: unknown fixture registration {other}"),
        }

        let catalog_id = format!("{}/{}", declaration.id, fixture.model_id);
        catalog
            .resolve(&ModelId(catalog_id))
            .unwrap_or_else(|_| panic!("{fixture_id}: model was not registered"))
            .clone()
    }

    fn declaration_for_fixture(provider_id: &str) -> &'static ProviderDeclaration {
        ALL_PROVIDER_DECLARATIONS
            .iter()
            .find(|declaration| declaration.id == provider_id)
            .unwrap_or_else(|| panic!("missing declaration for fixture provider {provider_id}"))
    }

    fn assert_declared_fixture(
        pi_provider_id: &str,
        fixture_id: &str,
        provider_id: &str,
        fixture: &PiRouteFixture,
    ) {
        let declaration = declaration_for_fixture(provider_id);
        assert_eq!(
            declaration.base_url, fixture.base_url,
            "{fixture_id}: {pi_provider_id} base URL drifted"
        );
        assert_eq!(
            fixture.registration == "configured",
            fixture.configured_base_url.is_some(),
            "{fixture_id}: configured fixtures must name exactly one configuration override"
        );
        assert_eq!(
            fixture.auth_presentation == "header",
            fixture.auth_header.is_some(),
            "{fixture_id}: header auth fixtures must name exactly one header"
        );
        let route = declaration
            .route_for_model(&fixture.model_id)
            .unwrap_or_else(|| {
                panic!(
                    "{fixture_id}: {pi_provider_id} has no route for {}",
                    fixture.model_id
                )
            });
        assert_eq!(
            route.protocol,
            fixture_protocol(&fixture.protocol),
            "{fixture_id}: {pi_provider_id} protocol drifted"
        );
        assert_eq!(
            route.endpoint_id, fixture.endpoint_id,
            "{fixture_id}: {pi_provider_id} endpoint drifted"
        );
        assert!(
            fixture_auth_presentation_matches(route.auth_presentation, fixture),
            "{fixture_id}: {pi_provider_id} auth presentation drifted"
        );

        match (
            declaration.authentication,
            fixture.auth_presentation.as_str(),
        ) {
            (
                ProviderAuthentication::Environment { variables },
                "api_key_header"
                | "bearer"
                | "cloudflare_ai_gateway"
                | "google_api_key_header"
                | "header",
            ) => {
                assert!(
                    variables
                        .iter()
                        .any(|variable| *variable == fixture.environment_variable),
                    "{fixture_id}: {pi_provider_id} environment variable drifted"
                );
            }
            (ProviderAuthentication::Aws { variables }, "aws_sigv4") => {
                assert!(
                    variables
                        .iter()
                        .any(|variable| *variable == fixture.environment_variable),
                    "{fixture_id}: {pi_provider_id} AWS environment documentation drifted"
                );
            }
            (ProviderAuthentication::ApplicationDefaultCredentials, "dynamic") => {
                assert!(
                    fixture.environment_variable.is_empty(),
                    "{fixture_id}: ADC fixtures must not name an environment credential"
                );
            }
            (ProviderAuthentication::Subscription { .. }, "dynamic") => {
                assert!(
                    fixture.environment_variable.is_empty(),
                    "{fixture_id}: subscription fixtures must not name an environment credential"
                );
            }
            _ => panic!("{fixture_id}: authentication kind and presentation disagree"),
        }

        if fixture.registration == "subscription" {
            assert!(matches!(
                declaration.authentication,
                ProviderAuthentication::Subscription { .. }
            ));
            return;
        }

        let base_url = fixture_resolved_base_url(declaration, fixture_id, fixture);
        let expected_route_base_url = fixture_route_base_url(&base_url, route, fixture_id);
        let resolved = register_fixture_model(declaration, fixture_id, fixture, &base_url);
        assert_eq!(resolved.spec.protocol, route.protocol);
        assert_eq!(resolved.endpoint.id.0, route.endpoint_id);
        assert_eq!(
            resolved.endpoint.base_url.as_str(),
            expected_route_base_url.as_str(),
            "{fixture_id}: {pi_provider_id} route base URL drifted"
        );
    }

    #[test]
    fn pinned_pi_provider_inventory_has_tested_decisions() {
        let inventory: PiProviderInventory =
            serde_json::from_str(include_str!("../../fixtures/providers/pi-0.84.4.json"))
                .expect("valid Pi provider compatibility fixture");
        assert_eq!(inventory.schema_version, 1);
        assert_eq!(
            inventory.pi_package,
            "@earendil-works/pi-coding-agent@0.84.4"
        );
        assert_eq!(
            inventory.expected_provider_ids,
            PINNED_PI_PROVIDER_IDS
                .iter()
                .map(|provider_id| (*provider_id).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(inventory.providers.len(), PINNED_PI_PROVIDER_IDS.len());
        assert_eq!(
            inventory
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            PINNED_PI_PROVIDER_IDS
        );

        let mut fixture_ids = HashSet::new();
        let mut declared_provider_ids = HashSet::new();
        for provider in &inventory.providers {
            match &provider.decision {
                PiProviderDecision::Declared {
                    fixture_id,
                    provider_id,
                    fixture,
                } => {
                    assert!(
                        fixture_ids.insert(fixture_id),
                        "duplicate fixture id: {fixture_id}"
                    );
                    assert!(
                        declared_provider_ids.insert(provider_id),
                        "duplicate declaration fixture: {provider_id}"
                    );
                    assert_declared_fixture(&provider.id, fixture_id, provider_id, fixture);
                }
                PiProviderDecision::DeclaredSubset {
                    fixture_id,
                    provider_id,
                    fixture,
                    excluded_surfaces,
                    missing_primitive,
                    release_blocker,
                } => {
                    assert!(
                        fixture_ids.insert(fixture_id),
                        "duplicate fixture id: {fixture_id}"
                    );
                    assert!(
                        declared_provider_ids.insert(provider_id),
                        "duplicate declaration fixture: {provider_id}"
                    );
                    assert!(
                        !excluded_surfaces.is_empty() && !missing_primitive.is_empty(),
                        "{fixture_id}: a subset decision requires an explicit missing primitive"
                    );
                    assert!(
                        !release_blocker.is_empty(),
                        "{fixture_id}: a subset decision requires a release blocker"
                    );
                    assert_declared_fixture(&provider.id, fixture_id, provider_id, fixture);
                }
                PiProviderDecision::Unsupported {
                    fixture_id,
                    missing_primitive,
                    release_blocker,
                    legacy_declaration,
                } => {
                    assert!(
                        fixture_ids.insert(fixture_id),
                        "duplicate fixture id: {fixture_id}"
                    );
                    assert!(
                        !missing_primitive.is_empty() && !release_blocker.is_empty(),
                        "{fixture_id}: unsupported providers require a primitive and release blocker"
                    );
                    if let Some(legacy_declaration) = legacy_declaration {
                        assert!(
                            ALL_PROVIDER_DECLARATIONS
                                .iter()
                                .any(|declaration| declaration.id == legacy_declaration),
                            "{fixture_id}: unsupported Pi provider {} references an unknown legacy declaration {legacy_declaration}",
                            provider.id
                        );
                    }
                    let has_direct_declaration = ALL_PROVIDER_DECLARATIONS
                        .iter()
                        .any(|declaration| declaration.id == provider.id);
                    assert!(
                        !has_direct_declaration
                            || legacy_declaration.as_deref() == Some(provider.id.as_str()),
                        "{fixture_id}: unsupported Pi provider {} acquired a declaration; name it as its legacy declaration or update its decision",
                        provider.id
                    );
                }
            }
        }
    }

    struct ExpectedFixtureRequest {
        fixture_id: String,
        path: String,
        query: Option<String>,
        fixture: PiRouteFixture,
    }

    #[tokio::test]
    async fn pinned_pi_provider_fixtures_send_declared_routes_without_network_access() {
        let inventory: PiProviderInventory =
            serde_json::from_str(include_str!("../../fixtures/providers/pi-0.84.4.json"))
                .expect("valid Pi provider compatibility fixture");
        let server = MockServer::start().await;
        let client = AiClient::new();
        let mut expected_requests = Vec::new();
        for provider in &inventory.providers {
            let (fixture_id, provider_id, fixture) = match &provider.decision {
                PiProviderDecision::Declared {
                    fixture_id,
                    provider_id,
                    fixture,
                }
                | PiProviderDecision::DeclaredSubset {
                    fixture_id,
                    provider_id,
                    fixture,
                    ..
                } if fixture.registration != "subscription" => (fixture_id, provider_id, fixture),
                PiProviderDecision::Declared { .. }
                | PiProviderDecision::DeclaredSubset { .. }
                | PiProviderDecision::Unsupported { .. } => continue,
            };
            let declaration = declaration_for_fixture(provider_id);
            let fixture_base_url = fixture_resolved_base_url(declaration, fixture_id, fixture);
            let base_url = fixture_base_at_server(&server, &fixture_base_url);
            let route = declaration
                .route_for_model(&fixture.model_id)
                .unwrap_or_else(|| panic!("{fixture_id}: missing fixture route"));
            let request_url = fixture_request_url(&base_url, route, fixture_id, fixture);
            let FixtureStreamResponse {
                body,
                response_id,
                content_type,
            } = fixture_stream_response(&fixture.protocol);
            Mock::given(method("POST"))
                .and(path(request_url.path().to_owned()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", content_type)
                        .set_body_bytes(body),
                )
                .mount(&server)
                .await;

            let mut model = register_fixture_model(declaration, fixture_id, fixture, &base_url);
            Arc::make_mut(&mut model.endpoint).auth = fixture_auth(fixture);
            let response = client
                .complete(&model, fixture_request())
                .await
                .unwrap_or_else(|error| panic!("{fixture_id}: request fixture failed: {error}"));
            assert_eq!(
                response.response_id.as_deref(),
                response_id,
                "{fixture_id}: stream response decoding drifted"
            );
            expected_requests.push(ExpectedFixtureRequest {
                fixture_id: fixture_id.clone(),
                path: request_url.path().to_owned(),
                query: request_url.query().map(str::to_owned),
                fixture: fixture.clone(),
            });
        }

        let received = server
            .received_requests()
            .await
            .expect("wiremock received requests");
        assert_eq!(received.len(), expected_requests.len());
        for (request, expected) in received.iter().zip(expected_requests) {
            assert_eq!(
                request.url.path(),
                expected.path,
                "{}: request route drifted",
                expected.fixture_id
            );
            assert_eq!(
                request.url.query(),
                expected.query.as_deref(),
                "{}: request query drifted",
                expected.fixture_id
            );
            assert_fixture_authentication(request, &expected.fixture_id, &expected.fixture);
            assert_fixture_request_body(request, &expected.fixture_id, &expected.fixture);
        }
    }

    #[test]
    fn generated_declarations_are_valid_and_route_only_by_data() {
        for declaration in ALL_PROVIDER_DECLARATIONS {
            declaration
                .validate()
                .unwrap_or_else(|error| panic!("{}: {error}", declaration.id));
        }

        assert_eq!(OPENAI.route_for_model("gpt-5.6"), None);
        assert_eq!(
            OPENCODE
                .inventory_route()
                .expect("OpenCode inventory route")
                .endpoint_id,
            "opencode"
        );
        assert_eq!(
            OPENCODE
                .route_for_model("claude-sonnet-4-6")
                .expect("Anthropic route")
                .protocol,
            Protocol::AnthropicMessages
        );
        assert_eq!(
            OPENCODE
                .route_for_model("qwen3.5-plus")
                .expect("Qwen plus route")
                .protocol,
            Protocol::AnthropicMessages
        );
        assert_eq!(
            OPENCODE
                .route_for_model("kimi-k2.7-code")
                .expect("Kimi Chat route")
                .protocol,
            Protocol::OpenAiChat
        );
        assert_eq!(
            OPENCODE
                .route_for_model("gpt-5.4")
                .expect("Responses route")
                .protocol,
            Protocol::OpenAiResponses
        );
        let codex = CODEX.inventory_route().expect("Codex route");
        assert_eq!(codex.transport, EndpointTransport::WebSocketPreferred);
        assert_eq!(codex.runtime.body_encoding, RequestBodyEncoding::Zstd);
        assert_eq!(
            codex.runtime.responses_profile,
            ResponsesRuntimeProfile::Codex
        );
        assert_eq!(
            CODEX.extra_headers,
            &[
                ("openai-beta", "responses=experimental"),
                ("originator", "ygg")
            ]
        );
        assert_eq!(
            BEDROCK
                .route_for_model("anthropic.claude-3-7-sonnet-20250219-v1:0")
                .expect("Bedrock default route")
                .protocol,
            Protocol::BedrockConverse
        );
        assert!(matches!(
            BEDROCK.authentication,
            ProviderAuthentication::Aws { .. }
        ));
        assert_eq!(
            AZURE_OPENAI
                .route_for_model("configured-deployment")
                .expect("Azure OpenAI default route")
                .auth_presentation,
            EndpointAuthPresentation::Header("api-key")
        );
        assert_eq!(
            AZURE_OPENAI.runtime_configuration,
            ProviderRuntimeConfiguration::AzureOpenAi
        );
        assert_eq!(
            FIREWORKS
                .route_for_model("accounts/fireworks/models/glm-5p2")
                .expect("Chat exception")
                .protocol,
            Protocol::OpenAiChat
        );
        assert_eq!(
            FIREWORKS
                .route_for_model("accounts/fireworks/models/kimi-k2p7-code")
                .expect("Anthropic default")
                .protocol,
            Protocol::AnthropicMessages
        );
        let gemini = GEMINI
            .route_for_model("gemini-2.5-flash")
            .expect("Gemini native route");
        assert_eq!(gemini.protocol, Protocol::GoogleGenerativeAi);
        assert_eq!(
            gemini.auth_presentation,
            EndpointAuthPresentation::GoogleApiKeyHeader
        );
        assert_eq!(
            VERTEX
                .route_for_model("gemini-2.5-flash")
                .expect("Vertex native route")
                .protocol,
            Protocol::GoogleGenerativeAi
        );
        assert!(matches!(
            VERTEX.definition().authentication(),
            ProviderAccess::ApplicationDefaultCredentials
        ));
    }

    #[test]
    fn cloudflare_base_url_templates_validate_values_without_exposing_them() {
        let account_id = "account_123";
        let workers_url = CLOUDFLARE_WORKERS_AI
            .resolve_base_url_with(|variable| {
                assert_eq!(variable, "CLOUDFLARE_ACCOUNT_ID");
                Ok(Some(account_id.to_owned()))
            })
            .unwrap();
        assert_eq!(
            workers_url.as_str(),
            "https://api.cloudflare.com/client/v4/accounts/account_123/ai/v1/"
        );

        let gateway_url = CLOUDFLARE_AI_GATEWAY
            .resolve_base_url_with(|variable| match variable {
                "CLOUDFLARE_ACCOUNT_ID" => Ok(Some(account_id.to_owned())),
                "CLOUDFLARE_GATEWAY_ID" => Ok(Some("gateway-456".to_owned())),
                _ => unreachable!("unexpected template variable"),
            })
            .unwrap();
        assert_eq!(
            gateway_url.as_str(),
            "https://gateway.ai.cloudflare.com/v1/account_123/gateway-456/"
        );
        assert!(!format!("{:?}", CLOUDFLARE_AI_GATEWAY.definition()).contains(account_id));

        let unsafe_value = "account/identifier-must-not-appear";
        let error = CLOUDFLARE_WORKERS_AI
            .resolve_base_url_with(|_| Ok(Some(unsafe_value.to_owned())))
            .unwrap_err();
        assert!(matches!(error, ConfigError::InvalidBaseUrl(_)));
        assert!(!error.to_string().contains(unsafe_value));
    }

    #[test]
    fn generated_declaration_validation_rejects_invalid_runtime_and_secret_headers() {
        const INVALID_RUNTIME_ROUTES: &[ProviderRoute] = &[ProviderRoute {
            endpoint_id: "invalid-runtime",
            base_path: "",
            protocol: Protocol::OpenAiChat,
            auth_presentation: EndpointAuthPresentation::Bearer,
            transport: EndpointTransport::Http,
            runtime: RequestRuntime {
                body_encoding: RequestBodyEncoding::Identity,
                responses_profile: ResponsesRuntimeProfile::Codex,
                openai_chat_profile: OpenAiChatRuntimeProfile::Default,
                lifecycle_feedback: false,
            },
        }];
        const DEFAULT_RULE: &[ModelRouteRule] = &[ModelRouteRule::Default { route: 0 }];
        const CREDENTIAL_HEADER: &[(&str, &str)] = &[("x-provider-token", "not-a-secret")];

        let invalid_runtime = ProviderDeclaration {
            routes: INVALID_RUNTIME_ROUTES,
            route_rules: DEFAULT_RULE,
            ..OPENAI
        };
        assert!(invalid_runtime.validate().is_err());

        let credential_header = ProviderDeclaration {
            extra_headers: CREDENTIAL_HEADER,
            ..OPENAI
        };
        assert!(credential_header.validate().is_err());
        assert!(valid_public_header("originator", "ygg"));
        assert!(!valid_public_header("bad:header", "ygg"));
        assert!(!valid_public_header("x-provider-auth", "ygg"));
        assert!(!valid_public_header("x-api_key", "ygg"));
        assert!(!valid_public_header("x-key", "ygg"));
        assert!(!valid_public_header("xkey", "ygg"));
        assert!(!valid_public_header("originator", "non-ascii-✓"));
    }

    #[test]
    fn custom_and_extension_definitions_have_no_credential_surface() {
        let custom = ProviderDefinition::custom("local", "Local", "custom-local").unwrap();
        let extension = ProviderDefinition::extension(
            "extension-example",
            "Extension example",
            "extension-example-route",
            Protocol::OpenAiChat,
        )
        .unwrap();
        let secret = "credential-that-must-not-cross-the-contract";
        assert!(!format!("{custom:?}{extension:?}").contains(secret));
        assert!(matches!(custom.authentication(), ProviderAccess::Custom));
        assert!(matches!(
            extension.authentication(),
            ProviderAccess::Extension
        ));
        assert!(matches!(custom.pricing(), PricingProfile::Custom));
        assert!(ProviderDefinition::custom("unsafe", "Unsafe\u{1b}[31m", "unsafe-route").is_err());
    }

    #[test]
    fn subscription_availability_is_not_pricing_availability() {
        let definition = CODEX.definition();
        assert!(matches!(
            definition.authentication(),
            ProviderAccess::Subscription { .. }
        ));
        assert_eq!(definition.pricing(), PricingProfile::Subscription);
        let diagnostic = ProviderDiagnostic::login_required(&definition);
        assert!(diagnostic.action().contains("--login codex"));
        assert!(!diagnostic.action().contains("price"));
    }

    #[test]
    fn setup_diagnostics_are_bounded_and_control_safe() {
        let definition = OPENAI.definition();
        let diagnostic =
            ProviderDiagnostic::setup_action(&definition, format!("\x1b{}", "x".repeat(600)));
        assert!(diagnostic.action().len() <= 512);
        assert!(!diagnostic.action().contains('\x1b'));
    }
}
