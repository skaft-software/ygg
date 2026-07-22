//! Authentication model, secret redaction, and header composition.

use crate::error::{AuthError, ConfigError};

/// A wrapper for sensitive values (API keys, credentials) that prevents accidental exposure.
///
/// It overrides `Debug` and `Display` to redact the underlying secret, and does not implement
/// `Serialize` or `Deserialize`.
#[derive(Clone)]
pub struct Secret(Box<str>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl From<String> for Secret {
    fn from(val: String) -> Self {
        Self(val.into_boxed_str())
    }
}

impl From<&str> for Secret {
    fn from(val: &str) -> Self {
        Self(val.to_string().into_boxed_str())
    }
}

impl Secret {
    /// Loads a secret from the environment.
    pub fn from_env(var: &str) -> Result<Self, ConfigError> {
        match std::env::var(var) {
            Ok(val) => Ok(Self::from(val)),
            Err(_) => Err(ConfigError::MissingEnv(var.to_string())),
        }
    }

    /// Expose the underlying secret value. This is crate-private.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// Authentication methods supported by endpoints.
#[derive(Clone)]
pub enum Auth {
    /// No authentication.
    None,
    /// Standard Bearer token authentication (Authorization: Bearer `<secret>`).
    Bearer(Secret),
    /// Custom HTTP header authentication.
    Header {
        /// Name of the custom header.
        name: http::HeaderName,
        /// Secret value of the custom header.
        value: Secret,
    },
    /// Bearer token loaded from the environment per request.
    BearerEnv {
        /// Name of the environment variable containing the token.
        var: String,
    },
    /// Custom header auth where the value is loaded from the environment per request.
    HeaderEnv {
        /// Name of the custom header.
        name: http::HeaderName,
        /// Name of the environment variable containing the value.
        var: String,
    },
    /// Dynamic token resolver (e.g. OAuth flow, auto-refreshing keys).
    Dynamic(std::sync::Arc<dyn CredentialResolver>),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::None => write!(f, "None"),
            Auth::Bearer(secret) => f.debug_tuple("Bearer").field(secret).finish(),
            Auth::Header { name, value } => f
                .debug_struct("Header")
                .field("name", name)
                .field("value", value)
                .finish(),
            Auth::BearerEnv { var } => f.debug_struct("BearerEnv").field("var", var).finish(),
            Auth::HeaderEnv { name, var } => f
                .debug_struct("HeaderEnv")
                .field("name", name)
                .field("var", var)
                .finish(),
            Auth::Dynamic(_) => write!(f, "Dynamic(<resolver>)"),
        }
    }
}

impl Auth {
    /// Returns Auth::None.
    pub fn none() -> Self {
        Self::None
    }

    /// Returns Auth::Bearer.
    pub fn bearer(secret: impl Into<Secret>) -> Self {
        Self::Bearer(secret.into())
    }

    /// Returns Auth::BearerEnv.
    pub fn bearer_env(var: impl Into<String>) -> Self {
        Self::BearerEnv { var: var.into() }
    }

    /// Returns Auth::Header.
    pub fn header(name: http::HeaderName, secret: impl Into<Secret>) -> Self {
        Self::Header {
            name,
            value: secret.into(),
        }
    }

    /// Returns Auth::HeaderEnv.
    pub fn header_env(name: http::HeaderName, var: impl Into<String>) -> Self {
        Self::HeaderEnv {
            name,
            var: var.into(),
        }
    }

    /// Returns Auth::Dynamic.
    pub fn dynamic(r: std::sync::Arc<dyn CredentialResolver>) -> Self {
        Self::Dynamic(r)
    }

    /// Whether this authentication strategy has credentials available now.
    ///
    /// This is intentionally a lightweight, non-validating check: it avoids
    /// showing models backed by an unset environment variable while leaving
    /// actual credential validation to the request path. Static, unauthenticated,
    /// and dynamic credentials are usable by construction.
    pub fn is_configured(&self) -> bool {
        match self {
            Self::BearerEnv { var } | Self::HeaderEnv { var, .. } => std::env::var(var)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false),
            Self::None | Self::Bearer(_) | Self::Header { .. } | Self::Dynamic(_) => true,
        }
    }
}

/// Interface for dynamic credentials resolution.
#[async_trait::async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Resolves credentials for a request.
    async fn resolve(&self) -> Result<ResolvedCredential, AuthError>;
}

/// Dynamic credential resolution result.
pub struct ResolvedCredential {
    /// Authentication scheme.
    pub scheme: CredentialScheme,
    /// Sensitive credential value.
    pub value: Secret,
    /// Additional non-sensitive headers.
    pub extra_headers: http::HeaderMap,
}

/// Credential scheme type.
pub enum CredentialScheme {
    /// Bearer token scheme.
    Bearer,
    /// Custom HTTP header scheme.
    Header(http::HeaderName),
}

/// Registry of registered credential resolvers.
pub type CredentialResolverRegistry =
    std::collections::HashMap<String, std::sync::Arc<dyn CredentialResolver>>;

/// Resolves authentication settings into a concrete HeaderMap.
pub(crate) async fn resolve_headers(auth: &Auth) -> Result<http::HeaderMap, AuthError> {
    let mut headers = http::HeaderMap::new();

    match auth {
        Auth::None => {}
        Auth::Bearer(secret) => {
            let bearer_str = format!("Bearer {}", secret.expose());
            let mut val = http::HeaderValue::from_str(&bearer_str)
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, val);
        }
        Auth::Header { name, value } => {
            let mut val = http::HeaderValue::from_str(value.expose())
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(name.clone(), val);
        }
        Auth::BearerEnv { var } => {
            let val_str =
                std::env::var(var).map_err(|_| AuthError::MissingEnvironment(var.clone()))?;
            let bearer_str = format!("Bearer {}", val_str);
            let mut val = http::HeaderValue::from_str(&bearer_str)
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, val);
        }
        Auth::HeaderEnv { name, var } => {
            let val_str =
                std::env::var(var).map_err(|_| AuthError::MissingEnvironment(var.clone()))?;
            let mut val =
                http::HeaderValue::from_str(&val_str).map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(name.clone(), val);
        }
        Auth::Dynamic(resolver) => {
            let cred = resolver.resolve().await?;

            // 1. Apply extra headers first
            for (k, v) in cred.extra_headers.iter() {
                headers.insert(k.clone(), v.clone());
            }

            // 2. Apply primary auth header last (so it wins)
            match cred.scheme {
                CredentialScheme::Bearer => {
                    let bearer_str = format!("Bearer {}", cred.value.expose());
                    let mut val = http::HeaderValue::from_str(&bearer_str)
                        .map_err(|_| AuthError::InvalidHeaderValue)?;
                    val.set_sensitive(true);
                    headers.insert(http::header::AUTHORIZATION, val);
                }
                CredentialScheme::Header(ref name) => {
                    let mut val = http::HeaderValue::from_str(cred.value.expose())
                        .map_err(|_| AuthError::InvalidHeaderValue)?;
                    val.set_sensitive(true);
                    headers.insert(name.clone(), val);
                }
            }
        }
    }

    Ok(headers)
}

/// Returns the primary header name that this Auth configuration targets, if statically known.
pub(crate) fn auth_header_name(auth: &Auth) -> Option<http::HeaderName> {
    match auth {
        Auth::None => None,
        Auth::Bearer(_) | Auth::BearerEnv { .. } => Some(http::header::AUTHORIZATION),
        Auth::Header { name, .. } | Auth::HeaderEnv { name, .. } => Some(name.clone()),
        Auth::Dynamic(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{AUTHORIZATION, CONTENT_TYPE};

    struct FakeResolver {
        value: String,
        extra: http::HeaderMap,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl CredentialResolver for FakeResolver {
        async fn resolve(&self) -> Result<ResolvedCredential, AuthError> {
            if self.fail {
                return Err(AuthError::Resolve);
            }
            Ok(ResolvedCredential {
                scheme: CredentialScheme::Bearer,
                value: Secret::from(self.value.clone()),
                extra_headers: self.extra.clone(),
            })
        }
    }

    #[tokio::test]
    async fn test_resolve_headers_bearer_and_custom() {
        let auth_bearer = Auth::bearer("my-key");
        let headers = resolve_headers(&auth_bearer).await.unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer my-key"
        );

        let auth_hdr = Auth::header(CONTENT_TYPE, "app-json");
        let headers_hdr = resolve_headers(&auth_hdr).await.unwrap();
        assert_eq!(
            headers_hdr.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "app-json"
        );
    }

    #[tokio::test]
    async fn test_resolve_headers_env() {
        let var_name = "YGG_RESOLVER_VAR";
        std::env::set_var(var_name, "env-key");

        let auth = Auth::bearer_env(var_name);
        let headers = resolve_headers(&auth).await.unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer env-key"
        );

        std::env::remove_var(var_name);
    }

    #[tokio::test]
    async fn test_resolve_headers_dynamic() {
        let mut extra = http::HeaderMap::new();
        extra.insert(CONTENT_TYPE, http::HeaderValue::from_static("extra-val"));
        // insert colliding AUTHORIZATION to see if primary wins
        extra.insert(
            AUTHORIZATION,
            http::HeaderValue::from_static("extra-auth-colliding"),
        );

        let resolver = std::sync::Arc::new(FakeResolver {
            value: "dynamic-secret".to_string(),
            extra,
            fail: false,
        });

        let auth = Auth::dynamic(resolver);
        let headers = resolve_headers(&auth).await.unwrap();

        // Check extra header is present
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "extra-val"
        );
        // Check primary auth header won the collision
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer dynamic-secret"
        );
    }

    #[test]
    fn test_auth_header_name() {
        assert_eq!(auth_header_name(&Auth::none()), None);
        assert_eq!(auth_header_name(&Auth::bearer("a")), Some(AUTHORIZATION));
        assert_eq!(
            auth_header_name(&Auth::header(CONTENT_TYPE, "a")),
            Some(CONTENT_TYPE)
        );
    }
}
