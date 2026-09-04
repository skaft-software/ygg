//! Private provider credential lifecycle.
//!
//! Only this module reads API-key environment values for catalog discovery or
//! turns an environment-variable name into an `ygg_ai::Auth`. Public provider
//! definitions expose setup labels and variable names, never this value type.

use std::fmt;

use ygg_ai::Auth;

#[cfg(test)]
use super::contract::ProviderDiagnostic;
use super::contract::{EndpointAuthPresentation, ProviderDeclaration, ProviderRoute};

/// A resolved API-key credential confined to provider catalog registration.
pub(crate) struct EnvironmentCredential {
    variable: &'static str,
    value: String,
}

impl fmt::Debug for EnvironmentCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentCredential")
            .field("variable", &self.variable)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl EnvironmentCredential {
    /// The private value used only for a provider's inventory request.
    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    pub(crate) fn for_test(variable: &'static str, value: impl Into<String>) -> Self {
        Self {
            variable,
            value: value.into(),
        }
    }

    fn variable(&self) -> &'static str {
        self.variable
    }
}

/// Resolve the first configured environment variable declared by a provider.
/// Invalid Unicode is an actionable configuration failure; oversized values are
/// rejected by `ygg_ai` before they reach a request header.
pub(crate) fn resolve_environment(
    declaration: &ProviderDeclaration,
) -> anyhow::Result<Option<EnvironmentCredential>> {
    let Some(variables) = declaration.authentication.environment_variables() else {
        return Ok(None);
    };
    for variable in variables {
        let value = match ygg_ai::auth::read_bounded_env(variable) {
            Ok(value) => value,
            Err(ygg_ai::ConfigError::InvalidEnv(_)) => {
                anyhow::bail!("could not read {variable}: invalid environment value")
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            return Ok(Some(EnvironmentCredential { variable, value }));
        }
    }
    Ok(None)
}

/// Build an endpoint auth strategy without copying the credential into a public
/// provider contract.
pub(crate) fn environment_auth(
    route: &ProviderRoute,
    credential: &EnvironmentCredential,
) -> anyhow::Result<Auth> {
    match route.auth_presentation {
        EndpointAuthPresentation::Bearer => Ok(Auth::bearer_env(credential.variable())),
        EndpointAuthPresentation::ApiKeyHeader => Ok(Auth::header_env(
            http::HeaderName::from_static("x-api-key"),
            credential.variable(),
        )),
        EndpointAuthPresentation::CloudflareAiGateway => Ok(Auth::header_bearer_env(
            http::HeaderName::from_static("cf-aig-authorization"),
            credential.variable(),
        )),
        EndpointAuthPresentation::Dynamic => {
            anyhow::bail!("environment provider declaration has an invalid credential presentation")
        }
    }
}

/// Build private discovery headers for an environment-authenticated route.
///
/// The resolved value remains inside the auth lifecycle; callers receive only
/// a sensitive request header map for the immediate inventory request.
pub(crate) fn environment_discovery_headers(
    route: &ProviderRoute,
    credential: &EnvironmentCredential,
) -> anyhow::Result<http::HeaderMap> {
    let mut headers = http::HeaderMap::new();
    let (name, value) = match route.auth_presentation {
        EndpointAuthPresentation::Bearer => (
            http::header::AUTHORIZATION,
            format!("Bearer {}", credential.value()),
        ),
        EndpointAuthPresentation::ApiKeyHeader => (
            http::HeaderName::from_static("x-api-key"),
            credential.value().to_owned(),
        ),
        EndpointAuthPresentation::CloudflareAiGateway => (
            http::HeaderName::from_static("cf-aig-authorization"),
            format!("Bearer {}", credential.value()),
        ),
        EndpointAuthPresentation::Dynamic => {
            anyhow::bail!("environment provider declaration has an invalid credential presentation")
        }
    };
    let mut value = http::HeaderValue::from_str(&value)?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(headers)
}

/// Return a credential-free diagnostic for an unavailable API-key declaration.
#[cfg(test)]
pub(crate) fn missing_environment_diagnostic(
    declaration: &ProviderDeclaration,
) -> ProviderDiagnostic {
    ProviderDiagnostic::missing_environment(&declaration.definition())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::contract::{ANTHROPIC, CLOUDFLARE_AI_GATEWAY, OPENAI};

    #[test]
    fn diagnostics_never_format_a_resolved_value() {
        let credential = EnvironmentCredential {
            variable: "TEST_PROVIDER_KEY",
            value: "secret-value-must-not-appear".to_owned(),
        };
        assert!(!format!("{credential:?}").contains(&credential.value));
        assert!(missing_environment_diagnostic(&OPENAI)
            .action()
            .contains("OPENAI_API_KEY"));
    }

    #[test]
    fn generated_presentation_selects_auth_header_without_a_secret() {
        let credential = EnvironmentCredential {
            variable: "TEST_PROVIDER_KEY",
            value: "not-formatted".to_owned(),
        };
        let auth = environment_auth(&ANTHROPIC.routes[0], &credential).unwrap();
        assert!(matches!(auth, Auth::HeaderEnv { .. }));
        let gateway = environment_auth(&CLOUDFLARE_AI_GATEWAY.routes[0], &credential).unwrap();
        assert!(matches!(
            gateway,
            Auth::HeaderBearerEnv { ref name, .. }
                if name == &http::HeaderName::from_static("cf-aig-authorization")
        ));
    }

    #[test]
    fn discovery_headers_are_sensitive_and_route_selected() {
        let credential = EnvironmentCredential {
            variable: "TEST_PROVIDER_KEY",
            value: "not-formatted".to_owned(),
        };
        let bearer = environment_discovery_headers(&OPENAI.routes[0], &credential).unwrap();
        assert!(bearer[http::header::AUTHORIZATION].is_sensitive());

        let api_key = environment_discovery_headers(&ANTHROPIC.routes[0], &credential).unwrap();
        assert!(api_key[http::HeaderName::from_static("x-api-key")].is_sensitive());

        let gateway =
            environment_discovery_headers(&CLOUDFLARE_AI_GATEWAY.routes[0], &credential).unwrap();
        let gateway_header = &gateway[http::HeaderName::from_static("cf-aig-authorization")];
        assert_eq!(gateway_header.to_str().unwrap(), "Bearer not-formatted");
        assert!(gateway_header.is_sensitive());
    }
}
