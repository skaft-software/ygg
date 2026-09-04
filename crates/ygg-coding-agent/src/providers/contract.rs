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
}

impl ProviderAuthentication {
    pub(crate) fn environment_variables(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Environment { variables } => Some(variables),
            Self::Aws { .. } | Self::ApplicationDefaultCredentials | Self::Subscription { .. } => {
                None
            }
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
        matches!(self, Self::GptVisionFallback)
            && (model_id.starts_with("gpt-4o")
                || model_id.starts_with("gpt-4.1")
                || model_id.starts_with("gpt-5"))
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
    /// MiniMax's supported Anthropic-compatible routes.
    MiniMax,
    /// OpenCode Zen's supported routes.
    OpenCode,
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
            ProviderAuthentication::Subscription { .. } => {}
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
                        | ProviderAuthentication::Subscription { .. },
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
            ModelDiscovery::CodexSubscription => Self::Subscription,
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
        let login = match definition.authentication() {
            ProviderAccess::Subscription { login } => login.as_str(),
            _ => "provider",
        };
        Self {
            provider_id: definition.id().to_owned(),
            provider_label: definition.label().to_owned(),
            action: bounded_setup_text(&format!("run ygg --login {login}")),
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
    use super::*;

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
