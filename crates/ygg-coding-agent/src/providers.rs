//! Provider composition root.
//!
//! Built-in declarations are generated from `declarations.json`; authentication,
//! canonical catalog registration, compatibility, pricing, and static model
//! metadata have distinct owners below. The public SDK contract intentionally
//! exposes only credential-free definitions and catalog contribution hooks.

mod auth;
mod catalog;
mod compatibility;
mod contract;
mod copilot;
mod models;
mod pricing;
mod vertex;

pub use contract::{
    CompatibilityProfile, PricingProfile, ProviderAccess, ProviderAvailability,
    ProviderCatalogContributor, ProviderCatalogKind, ProviderDefinition, ProviderDefinitionError,
    ProviderDiagnostic, ProviderRouteDefinition,
};
pub use copilot::{
    CopilotAvailabilityError, CopilotCredentialScheme, CopilotDeviceLogin,
    CopilotDeviceLoginStatus, CopilotDynamicHeader, CopilotEndpoint, CopilotHost, CopilotModel,
    CopilotProvider, CopilotSession,
};

/// Return generated built-in and subscription provider definitions without
/// URLs, static headers, credentials, or credential-store handles.
pub fn builtin_provider_definitions() -> Vec<ProviderDefinition> {
    contract::ALL_PROVIDER_DECLARATIONS
        .iter()
        .map(ProviderDeclaration::definition)
        .collect()
}

pub(crate) use auth::{
    aws_bedrock_auth, aws_bedrock_base_url, aws_bedrock_region, environment_discovery_headers,
    resolve_environment, EnvironmentCredential,
};
pub(crate) use catalog::{
    public_headers, register_discovered_model, register_discovered_model_at_route,
    register_dynamic_endpoints_at_base_url, register_environment_endpoints,
    register_environment_endpoints_at_base_url, register_private_endpoints_at_base_url,
    register_static_models,
};
pub(crate) use compatibility::cache_compatibility;
pub(crate) use contract::{
    InventoryCacheMode, ModelDiscovery, ModelFilter, ProviderAuthentication, ProviderDeclaration,
    ProviderRoute, ProviderRuntimeConfiguration, BUILTIN_PROVIDER_DECLARATIONS, CODEX, DEEPSEEK,
};
#[cfg(test)]
pub(crate) use contract::{OPENAI, OPENCODE, OPENROUTER};
pub(crate) use pricing::pricing_for;
pub(crate) use vertex::resolve_application_default_credentials;
