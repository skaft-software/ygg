//! Authentication model, secret redaction, and header composition.

use crate::error::{AuthError, ConfigError};

/// Maximum number of bytes accepted from an environment variable.
pub const MAX_ENV_VALUE_BYTES: usize = 4096;

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

/// Reads an environment variable while enforcing [`MAX_ENV_VALUE_BYTES`].
///
/// An unset variable returns `Ok(None)`. A value that is not valid Unicode
/// returns [`ConfigError::InvalidEnv`], and an otherwise valid value over the
/// byte limit returns [`ConfigError::EnvironmentValueTooLarge`].
pub fn read_bounded_env(var: &str) -> Result<Option<String>, ConfigError> {
    match std::env::var(var) {
        Ok(value) if value.len() > MAX_ENV_VALUE_BYTES => {
            Err(ConfigError::EnvironmentValueTooLarge {
                var: var.to_owned(),
                max_bytes: MAX_ENV_VALUE_BYTES,
            })
        }
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidEnv(var.to_owned())),
    }
}

impl Secret {
    /// Loads a secret from the environment.
    pub fn from_env(var: &str) -> Result<Self, ConfigError> {
        match read_bounded_env(var) {
            Ok(Some(value)) => Ok(Self::from(value)),
            Ok(None) | Err(ConfigError::InvalidEnv(_)) => {
                Err(ConfigError::MissingEnv(var.to_owned()))
            }
            Err(error) => Err(error),
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
            Self::BearerEnv { var } | Self::HeaderEnv { var, .. } => Secret::from_env(var)
                .map(|secret| !secret.expose().trim().is_empty())
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

/// Resolved request headers paired with the credential values that must never
/// reappear in provider or transport diagnostics.
pub(crate) struct ResolvedHeaders {
    pub(crate) headers: http::HeaderMap,
    pub(crate) redactor: CredentialRedactor,
}

/// Exact-match redactor for credentials used by one request.
///
/// The values remain wrapped in [`Secret`] so accidental `Debug`/`Display`
/// formatting cannot expose them while the response stream is alive.
#[derive(Clone, Default)]
pub(crate) struct CredentialRedactor {
    values: Vec<Secret>,
}

impl CredentialRedactor {
    fn insert(&mut self, value: Secret) {
        if value.expose().is_empty()
            || self
                .values
                .iter()
                .any(|existing| existing.expose() == value.expose())
        {
            return;
        }
        self.values.push(value);
    }

    /// Treat endpoint-default header values as sensitive configuration. These
    /// headers are already fully redacted from [`crate::types::Endpoint`]'s
    /// `Debug` output and commonly carry gateway API keys.
    pub(crate) fn include_header_values(&mut self, headers: &http::HeaderMap) {
        for value in headers.values() {
            // `HeaderValue::to_str` accepts only visible ASCII, while valid
            // HTTP field values may contain UTF-8 bytes. Preserve every value
            // that can reappear verbatim in a provider's UTF-8 diagnostic.
            if let Ok(value) = std::str::from_utf8(value.as_bytes()) {
                self.insert(Secret::from(value));
            }
        }
    }

    /// Replaces every exact credential occurrence without rescanning the
    /// replacement marker. Longest matches win when credentials overlap.
    pub(crate) fn redact(&self, input: &str) -> String {
        const MARKER: &str = "[REDACTED]";

        let mut output = String::with_capacity(input.len());
        let mut position = 0usize;
        while position < input.len() {
            let next = self
                .values
                .iter()
                .filter_map(|value| {
                    let value = value.expose();
                    input[position..]
                        .find(value)
                        .map(|offset| (position + offset, value.len()))
                })
                .min_by(|(left_start, left_len), (right_start, right_len)| {
                    left_start
                        .cmp(right_start)
                        .then_with(|| right_len.cmp(left_len))
                });
            let Some((start, length)) = next else {
                output.push_str(&input[position..]);
                break;
            };
            output.push_str(&input[position..start]);
            output.push_str(MARKER);
            position = start + length;
        }
        output
    }
}

fn resolve_env_secret(var: &str) -> Result<Secret, AuthError> {
    Secret::from_env(var).map_err(|error| match error {
        ConfigError::MissingEnv(_) | ConfigError::InvalidEnv(_) => {
            AuthError::MissingEnvironment(var.to_owned())
        }
        ConfigError::EnvironmentValueTooLarge { var, max_bytes } => {
            AuthError::EnvironmentValueTooLarge { var, max_bytes }
        }
        _ => AuthError::Resolve,
    })
}

/// Resolves authentication settings into concrete headers and a request-scoped
/// credential redactor.
pub(crate) async fn resolve_headers(auth: &Auth) -> Result<ResolvedHeaders, AuthError> {
    let mut headers = http::HeaderMap::new();
    let mut redactor = CredentialRedactor::default();

    match auth {
        Auth::None => {}
        Auth::Bearer(secret) => {
            redactor.insert(secret.clone());
            let bearer_str = format!("Bearer {}", secret.expose());
            let mut val = http::HeaderValue::from_str(&bearer_str)
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, val);
        }
        Auth::Header { name, value } => {
            redactor.insert(value.clone());
            let mut val = http::HeaderValue::from_str(value.expose())
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(name.clone(), val);
        }
        Auth::BearerEnv { var } => {
            let secret = resolve_env_secret(var)?;
            redactor.insert(secret.clone());
            let bearer_str = format!("Bearer {}", secret.expose());
            let mut val = http::HeaderValue::from_str(&bearer_str)
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, val);
        }
        Auth::HeaderEnv { name, var } => {
            let secret = resolve_env_secret(var)?;
            redactor.insert(secret.clone());
            let mut val = http::HeaderValue::from_str(secret.expose())
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(name.clone(), val);
        }
        Auth::Dynamic(resolver) => {
            let cred = resolver.resolve().await?;
            redactor.insert(cred.value.clone());

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

    redactor.include_header_values(&headers);
    Ok(ResolvedHeaders { headers, redactor })
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
        let resolved = resolve_headers(&auth_bearer).await.unwrap();
        assert_eq!(
            resolved
                .headers
                .get(AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer my-key"
        );
        assert_eq!(
            resolved.redactor.redact("provider echoed my-key"),
            "provider echoed [REDACTED]"
        );

        let auth_hdr = Auth::header(CONTENT_TYPE, "app-json");
        let resolved = resolve_headers(&auth_hdr).await.unwrap();
        assert_eq!(
            resolved
                .headers
                .get(CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "app-json"
        );
    }

    #[tokio::test]
    async fn test_resolve_headers_env() {
        let var_name = "YGG_RESOLVER_VAR";
        std::env::set_var(var_name, "env-key");

        let auth = Auth::bearer_env(var_name);
        let resolved = resolve_headers(&auth).await.unwrap();
        assert_eq!(
            resolved
                .headers
                .get(AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
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
        let resolved = resolve_headers(&auth).await.unwrap();

        // Check extra header is present
        assert_eq!(
            resolved
                .headers
                .get(CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "extra-val"
        );
        // Check primary auth header won the collision
        assert_eq!(
            resolved
                .headers
                .get(AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer dynamic-secret"
        );
        let redacted = resolved
            .redactor
            .redact("extra-val dynamic-secret Bearer dynamic-secret");
        assert!(!redacted.contains("extra-val"), "{redacted}");
        assert!(!redacted.contains("dynamic-secret"), "{redacted}");
    }

    #[test]
    fn credential_redaction_prefers_longest_overlapping_value() {
        let mut redactor = CredentialRedactor::default();
        redactor.insert(Secret::from("key"));
        redactor.insert(Secret::from("key-long"));
        assert_eq!(redactor.redact("key-long/key"), "[REDACTED]/[REDACTED]");
    }

    #[test]
    fn credential_redaction_includes_utf8_header_values() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-gateway-key",
            http::HeaderValue::from_bytes("clé-secrète".as_bytes()).unwrap(),
        );
        let mut redactor = CredentialRedactor::default();
        redactor.include_header_values(&headers);
        assert_eq!(
            redactor.redact("provider echoed clé-secrète"),
            "provider echoed [REDACTED]"
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
