//! Authentication model, secret redaction, and header composition.

use crate::error::{AuthError, ConfigError};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn bounded_env_value(
    var: &str,
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, ConfigError> {
    match value {
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

/// Reads an environment variable while enforcing [`MAX_ENV_VALUE_BYTES`].
///
/// An unset variable returns `Ok(None)`. A value that is not valid Unicode
/// returns [`ConfigError::InvalidEnv`], and an otherwise valid value over the
/// byte limit returns [`ConfigError::EnvironmentValueTooLarge`].
pub fn read_bounded_env(var: &str) -> Result<Option<String>, ConfigError> {
    bounded_env_value(var, std::env::var(var))
}

fn secret_from_bounded_env(
    var: &str,
    value: Result<Option<String>, ConfigError>,
) -> Result<Secret, ConfigError> {
    match value {
        Ok(Some(value)) => Ok(Secret::from(value)),
        Ok(None) | Err(ConfigError::InvalidEnv(_)) => Err(ConfigError::MissingEnv(var.to_owned())),
        Err(error) => Err(error),
    }
}

impl Secret {
    /// Loads a secret from the environment.
    pub fn from_env(var: &str) -> Result<Self, ConfigError> {
        secret_from_bounded_env(var, read_bounded_env(var))
    }

    /// Whether this secret is empty or contains only ASCII/Unicode whitespace.
    ///
    /// This reveals no credential bytes and lets embedding integrations reject
    /// an unusable secret at their setup boundary.
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// Whether this secret fits within a caller-selected byte limit.
    ///
    /// This reveals only whether a bounded transport/storage boundary can
    /// accept the value, never any credential bytes.
    pub fn fits_within_bytes(&self, maximum: usize) -> bool {
        self.0.len() <= maximum
    }

    /// Whether this secret can be sent as an HTTP header value.
    ///
    /// The result permits host-owned authentication integrations to reject bad
    /// session material before it reaches a request or diagnostic boundary.
    pub fn is_valid_http_header_value(&self) -> bool {
        http::HeaderValue::from_str(&self.0).is_ok()
    }

    /// Expose the underlying secret value. This is crate-private.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// Fully prepared HTTP input passed to a request-aware authentication signer.
///
/// The client creates this only after all codec headers and any endpoint body
/// encoding have been applied, so signatures cover the exact bytes sent on the
/// wire.
#[derive(Clone, Debug)]
pub struct SigningRequest {
    method: http::Method,
    url: url::Url,
    body: bytes::Bytes,
    headers: http::HeaderMap,
}

impl SigningRequest {
    /// Creates signing input for an HTTP request.
    pub fn new(
        method: http::Method,
        url: url::Url,
        body: bytes::Bytes,
        headers: http::HeaderMap,
    ) -> Self {
        Self {
            method,
            url,
            body,
            headers,
        }
    }

    /// HTTP method that will be sent.
    pub fn method(&self) -> &http::Method {
        &self.method
    }

    /// Fully resolved request URL.
    pub fn url(&self) -> &url::Url {
        &self.url
    }

    /// Exact encoded request body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Headers composed before signing.
    pub fn headers(&self) -> &http::HeaderMap {
        &self.headers
    }
}

/// Headers returned by a request-aware signer.
///
/// `sensitive_values` are added to the request diagnostic redactor even when a
/// provider echoes them in an error body.
pub struct SignedRequestHeaders {
    headers: http::HeaderMap,
    sensitive_values: Vec<Secret>,
}

impl SignedRequestHeaders {
    /// Creates a signed-header result.
    pub fn new(headers: http::HeaderMap, sensitive_values: Vec<Secret>) -> Self {
        Self {
            headers,
            sensitive_values,
        }
    }
}

/// Signs an already prepared HTTP request.
#[async_trait::async_trait]
pub trait RequestSigner: Send + Sync {
    /// Returns authentication/signature headers for `request`.
    async fn sign(&self, request: &SigningRequest) -> Result<SignedRequestHeaders, AuthError>;
}

/// AWS credentials used to create a SigV4 signature.
#[derive(Clone)]
pub struct AwsCredentials {
    access_key_id: Secret,
    secret_access_key: Secret,
    session_token: Option<Secret>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .field("session_token", &self.session_token)
            .finish()
    }
}

impl AwsCredentials {
    /// Creates AWS credentials, rejecting empty credential components.
    pub fn new(
        access_key_id: impl Into<Secret>,
        secret_access_key: impl Into<Secret>,
        session_token: Option<Secret>,
    ) -> Result<Self, AuthError> {
        let access_key_id = access_key_id.into();
        let secret_access_key = secret_access_key.into();
        if access_key_id.expose().trim().is_empty() || secret_access_key.expose().trim().is_empty()
        {
            return Err(AuthError::Resolve);
        }
        if session_token
            .as_ref()
            .is_some_and(|token| token.expose().trim().is_empty())
        {
            return Err(AuthError::Resolve);
        }
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }
}

/// AWS Signature Version 4 request signer.
///
/// The signer is transport-independent and signs the complete prepared request.
/// It is suitable for Bedrock and other AWS JSON APIs that use ordinary SigV4
/// header authentication.
#[derive(Clone)]
pub struct AwsSigV4Signer {
    credentials: AwsCredentials,
    region: String,
    service: String,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl std::fmt::Debug for AwsSigV4Signer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsSigV4Signer")
            .field("credentials", &self.credentials)
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AwsSigV4Signer {
    /// Creates a SigV4 signer for one AWS region and service name.
    pub fn new(
        credentials: AwsCredentials,
        region: impl Into<String>,
        service: impl Into<String>,
    ) -> Result<Self, AuthError> {
        let region = region.into();
        let service = service.into();
        if !valid_signing_component(&region) || !valid_signing_component(&service) {
            return Err(AuthError::Resolve);
        }
        Ok(Self {
            credentials,
            region,
            service,
            clock: Arc::new(SystemTime::now),
        })
    }

    /// Replaces the signer clock. Primarily useful for deterministic tests.
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> SystemTime + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    fn sign_request(&self, request: &SigningRequest) -> Result<SignedRequestHeaders, AuthError> {
        let (date_stamp, amz_date) = aws_timestamp((self.clock)())?;
        let payload_hash = sha256_hex(request.body());
        let mut headers = request.headers().clone();
        headers.remove(http::header::AUTHORIZATION);
        headers.insert(
            http::HeaderName::from_static("x-amz-date"),
            header_value(&amz_date)?,
        );
        headers.insert(
            http::HeaderName::from_static("x-amz-content-sha256"),
            header_value(&payload_hash)?,
        );
        if let Some(token) = &self.credentials.session_token {
            let mut value = header_value(token.expose())?;
            value.set_sensitive(true);
            headers.insert(http::HeaderName::from_static("x-amz-security-token"), value);
        }

        let (canonical_headers, signed_headers) = canonical_headers(&headers, request.url())?;
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            request.method().as_str(),
            canonical_uri(request.url()),
            canonical_query(request.url()),
            canonical_headers,
            signed_headers,
            payload_hash
        );
        let credential_scope =
            format!("{date_stamp}/{}/{}/aws4_request", self.region, self.service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let date_key = hmac_sha256(
            format!("AWS4{}", self.credentials.secret_access_key.expose()).as_bytes(),
            date_stamp.as_bytes(),
        );
        let region_key = hmac_sha256(&date_key, self.region.as_bytes());
        let service_key = hmac_sha256(&region_key, self.service.as_bytes());
        let signing_key = hmac_sha256(&service_key, b"aws4_request");
        let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.credentials.access_key_id.expose()
        );
        let mut authorization_value = header_value(&authorization)?;
        authorization_value.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, authorization_value);

        let mut sensitive_values = vec![self.credentials.secret_access_key.clone()];
        if let Some(token) = &self.credentials.session_token {
            sensitive_values.push(token.clone());
        }
        sensitive_values.push(Secret::from(authorization));
        Ok(SignedRequestHeaders::new(headers, sensitive_values))
    }
}

#[async_trait::async_trait]
impl RequestSigner for AwsSigV4Signer {
    async fn sign(&self, request: &SigningRequest) -> Result<SignedRequestHeaders, AuthError> {
        self.sign_request(request)
    }
}

fn valid_signing_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn header_value(value: &str) -> Result<http::HeaderValue, AuthError> {
    http::HeaderValue::from_str(value).map_err(|_| AuthError::InvalidHeaderValue)
}

fn canonical_headers(
    headers: &http::HeaderMap,
    url: &url::Url,
) -> Result<(String, String), AuthError> {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in headers {
        let name = name.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "authorization" | "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        let value =
            std::str::from_utf8(value.as_bytes()).map_err(|_| AuthError::InvalidHeaderValue)?;
        values
            .entry(name)
            .or_default()
            .push(normalize_header_whitespace(value));
    }
    values.insert("host".to_owned(), vec![canonical_host(url)?]);

    let signed_headers = values.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers = values
        .into_iter()
        .map(|(name, values)| format!("{name}:{}\n", values.join(",")))
        .collect::<String>();
    Ok((canonical_headers, signed_headers))
}

fn canonical_host(url: &url::Url) -> Result<String, AuthError> {
    let host = url.host_str().ok_or(AuthError::Resolve)?;
    let port = match url.port() {
        Some(80) if url.scheme() == "http" => None,
        Some(443) if url.scheme() == "https" => None,
        Some(port) => Some(port),
        None => None,
    };
    Ok(port.map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
}

fn normalize_header_whitespace(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn canonical_uri(url: &url::Url) -> String {
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    aws_uri_encode(path, false)
}

fn canonical_query(url: &url::Url) -> String {
    let mut pairs = url
        .query_pairs()
        .map(|(name, value)| (aws_uri_encode(&name, true), aws_uri_encode(&value, true)))
        .collect::<Vec<_>>();
    pairs.sort_unstable();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_uri_encode(value: &str, encode_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(*byte));
        } else if *byte == b'/' && !encode_slash {
            encoded.push('/');
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(*byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(*byte & 0x0f)]));
        }
    }
    encoded
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut key_block = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = [0_u8; BLOCK_BYTES];
    let mut outer = [0_u8; BLOCK_BYTES];
    for (index, byte) in key_block.iter().enumerate() {
        inner[index] = byte ^ 0x36;
        outer[index] = byte ^ 0x5c;
    }
    let mut inner_hasher = Sha256::new();
    inner_hasher.update(inner);
    inner_hasher.update(message);
    let inner_digest = inner_hasher.finalize();
    let mut outer_hasher = Sha256::new();
    outer_hasher.update(outer);
    outer_hasher.update(inner_digest);
    outer_hasher.finalize().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(*byte >> 4)]));
        output.push(char::from(HEX[usize::from(*byte & 0x0f)]));
    }
    output
}

fn aws_timestamp(time: SystemTime) -> Result<(String, String), AuthError> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::Resolve)?
        .as_secs();
    let days = i64::try_from(seconds / 86_400).map_err(|_| AuthError::Resolve)?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(0..=9_999).contains(&year) {
        return Err(AuthError::Resolve);
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let date = format!("{year:04}{month:02}{day:02}");
    Ok((
        date.clone(),
        format!("{date}T{hour:02}{minute:02}{second:02}Z"),
    ))
}

// Gregorian civil date for days since 1970-01-01, adapted from Howard Hinnant's
// public-domain calendar algorithm.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
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
    /// Bearer token loaded from the environment in a custom header.
    ///
    /// This is used by proxy APIs such as Cloudflare AI Gateway that forward
    /// the downstream provider token outside the ordinary `Authorization`
    /// header.
    HeaderBearerEnv {
        /// Name of the custom header.
        name: http::HeaderName,
        /// Name of the environment variable containing the token.
        var: String,
    },
    /// Dynamic token resolver (e.g. OAuth flow, auto-refreshing keys).
    Dynamic(std::sync::Arc<dyn CredentialResolver>),
    /// Request-aware signer (for example AWS SigV4).
    RequestSigner(std::sync::Arc<dyn RequestSigner>),
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
            Auth::HeaderBearerEnv { name, var } => f
                .debug_struct("HeaderBearerEnv")
                .field("name", name)
                .field("var", var)
                .finish(),
            Auth::Dynamic(_) => write!(f, "Dynamic(<resolver>)"),
            Auth::RequestSigner(_) => write!(f, "RequestSigner(<signer>)"),
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

    /// Returns Auth::HeaderBearerEnv.
    pub fn header_bearer_env(name: http::HeaderName, var: impl Into<String>) -> Self {
        Self::HeaderBearerEnv {
            name,
            var: var.into(),
        }
    }

    /// Returns Auth::Dynamic.
    pub fn dynamic(r: std::sync::Arc<dyn CredentialResolver>) -> Self {
        Self::Dynamic(r)
    }

    /// Returns a request-aware signing authentication strategy.
    pub fn request_signer(signer: std::sync::Arc<dyn RequestSigner>) -> Self {
        Self::RequestSigner(signer)
    }

    /// Whether this authentication strategy has credentials available now.
    ///
    /// This is intentionally a lightweight, non-validating check: it avoids
    /// showing models backed by an unset environment variable while leaving
    /// actual credential validation to the request path. Static, unauthenticated,
    /// and dynamic credentials are usable by construction.
    pub fn is_configured(&self) -> bool {
        match self {
            Self::BearerEnv { var }
            | Self::HeaderEnv { var, .. }
            | Self::HeaderBearerEnv { var, .. } => Secret::from_env(var)
                .map(|secret| !secret.expose().trim().is_empty())
                .unwrap_or(false),
            Self::None
            | Self::Bearer(_)
            | Self::Header { .. }
            | Self::Dynamic(_)
            | Self::RequestSigner(_) => true,
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

fn resolve_env_secret_value(
    var: &str,
    value: Result<Option<String>, ConfigError>,
) -> Result<Secret, AuthError> {
    secret_from_bounded_env(var, value).map_err(|error| match error {
        ConfigError::MissingEnv(_) | ConfigError::InvalidEnv(_) => {
            AuthError::MissingEnvironment(var.to_owned())
        }
        ConfigError::EnvironmentValueTooLarge { var, max_bytes } => {
            AuthError::EnvironmentValueTooLarge { var, max_bytes }
        }
        _ => AuthError::Resolve,
    })
}

fn resolve_env_secret_with<F>(var: &str, read_env: &F) -> Result<Secret, AuthError>
where
    F: Fn(&str) -> Result<Option<String>, ConfigError>,
{
    resolve_env_secret_value(var, read_env(var))
}

/// Resolves authentication settings into concrete headers and a request-scoped
/// credential redactor.
pub(crate) async fn resolve_headers(auth: &Auth) -> Result<ResolvedHeaders, AuthError> {
    resolve_headers_with_env(auth, &read_bounded_env).await
}

/// Resolves regular credentials or invokes a signer against the exact prepared
/// request. Signers cannot be resolved through [`resolve_headers`] because the
/// body and final URL are part of their authentication contract.
pub(crate) async fn resolve_headers_for_request(
    auth: &Auth,
    method: http::Method,
    url: url::Url,
    body: bytes::Bytes,
    headers: http::HeaderMap,
) -> Result<ResolvedHeaders, AuthError> {
    let Auth::RequestSigner(signer) = auth else {
        return resolve_headers(auth).await;
    };
    let signed = signer
        .sign(&SigningRequest::new(method, url, body, headers))
        .await?;
    let mut redactor = CredentialRedactor::default();
    for value in signed.sensitive_values {
        redactor.insert(value);
    }
    redactor.include_header_values(&signed.headers);
    Ok(ResolvedHeaders {
        headers: signed.headers,
        redactor,
    })
}

async fn resolve_headers_with_env<F>(
    auth: &Auth,
    read_env: &F,
) -> Result<ResolvedHeaders, AuthError>
where
    F: Fn(&str) -> Result<Option<String>, ConfigError>,
{
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
            let secret = resolve_env_secret_with(var, read_env)?;
            redactor.insert(secret.clone());
            let bearer_str = format!("Bearer {}", secret.expose());
            let mut val = http::HeaderValue::from_str(&bearer_str)
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, val);
        }
        Auth::HeaderEnv { name, var } => {
            let secret = resolve_env_secret_with(var, read_env)?;
            redactor.insert(secret.clone());
            let mut val = http::HeaderValue::from_str(secret.expose())
                .map_err(|_| AuthError::InvalidHeaderValue)?;
            val.set_sensitive(true);
            headers.insert(name.clone(), val);
        }
        Auth::HeaderBearerEnv { name, var } => {
            let secret = resolve_env_secret_with(var, read_env)?;
            redactor.insert(secret.clone());
            let bearer_str = format!("Bearer {}", secret.expose());
            let mut val = http::HeaderValue::from_str(&bearer_str)
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
        Auth::RequestSigner(_) => return Err(AuthError::Resolve),
    }

    redactor.include_header_values(&headers);
    Ok(ResolvedHeaders { headers, redactor })
}

/// Returns the primary header name that this Auth configuration targets, if statically known.
pub(crate) fn auth_header_name(auth: &Auth) -> Option<http::HeaderName> {
    match auth {
        Auth::None => None,
        Auth::Bearer(_) | Auth::BearerEnv { .. } => Some(http::header::AUTHORIZATION),
        Auth::Header { name, .. }
        | Auth::HeaderEnv { name, .. }
        | Auth::HeaderBearerEnv { name, .. } => Some(name.clone()),
        Auth::Dynamic(_) | Auth::RequestSigner(_) => None,
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

    #[test]
    fn bounded_env_values_preserve_missing_and_invalid_unicode() {
        assert_eq!(
            bounded_env_value("MISSING", Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        let error = bounded_env_value(
            "INVALID",
            Err(std::env::VarError::NotUnicode(std::ffi::OsString::from(
                "not-utf8",
            ))),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidEnv(var) if var == "INVALID"
        ));
        assert!(matches!(
            secret_from_bounded_env("MISSING", Ok(None)),
            Err(ConfigError::MissingEnv(var)) if var == "MISSING"
        ));
        assert!(matches!(
            secret_from_bounded_env("INVALID", Err(ConfigError::InvalidEnv("INVALID".into()))),
            Err(ConfigError::MissingEnv(var)) if var == "INVALID"
        ));
    }

    #[test]
    fn bounded_env_values_enforce_a_byte_limit() {
        let at_limit = "x".repeat(MAX_ENV_VALUE_BYTES);
        assert_eq!(
            bounded_env_value("BOUNDARY", Ok(at_limit.clone())).unwrap(),
            Some(at_limit)
        );

        let utf8_at_limit = "é".repeat(MAX_ENV_VALUE_BYTES / 2);
        assert_eq!(utf8_at_limit.len(), MAX_ENV_VALUE_BYTES);
        assert!(bounded_env_value("UTF8_BOUNDARY", Ok(utf8_at_limit)).is_ok());

        let error =
            bounded_env_value("TOO_LARGE", Ok("x".repeat(MAX_ENV_VALUE_BYTES + 1))).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::EnvironmentValueTooLarge { var, max_bytes }
                if var == "TOO_LARGE" && max_bytes == MAX_ENV_VALUE_BYTES
        ));
    }

    #[tokio::test]
    async fn env_auth_headers_accept_the_limit_and_reject_oversized_values() {
        let at_limit = "k".repeat(MAX_ENV_VALUE_BYTES);
        let read_at_limit = |var: &str| -> Result<Option<String>, ConfigError> {
            bounded_env_value(var, Ok(at_limit.clone()))
        };

        let bearer = resolve_headers_with_env(&Auth::bearer_env("BEARER_KEY"), &read_at_limit)
            .await
            .unwrap();
        assert_eq!(
            bearer.headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            format!("Bearer {at_limit}")
        );
        assert!(bearer.headers.get(AUTHORIZATION).unwrap().is_sensitive());

        let header = resolve_headers_with_env(
            &Auth::header_env(http::HeaderName::from_static("x-api-key"), "HEADER_KEY"),
            &read_at_limit,
        )
        .await
        .unwrap();
        assert_eq!(
            header.headers.get("x-api-key").unwrap().to_str().unwrap(),
            at_limit
        );
        assert!(header.headers.get("x-api-key").unwrap().is_sensitive());

        let gateway = resolve_headers_with_env(
            &Auth::header_bearer_env(
                http::HeaderName::from_static("cf-aig-authorization"),
                "GATEWAY_KEY",
            ),
            &read_at_limit,
        )
        .await
        .unwrap();
        assert_eq!(
            gateway
                .headers
                .get("cf-aig-authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {at_limit}")
        );
        assert!(gateway
            .headers
            .get("cf-aig-authorization")
            .unwrap()
            .is_sensitive());

        let read_over_limit = |var: &str| -> Result<Option<String>, ConfigError> {
            bounded_env_value(var, Ok("x".repeat(MAX_ENV_VALUE_BYTES + 1)))
        };
        for auth in [
            Auth::bearer_env("BEARER_KEY"),
            Auth::header_env(http::HeaderName::from_static("x-api-key"), "HEADER_KEY"),
            Auth::header_bearer_env(
                http::HeaderName::from_static("cf-aig-authorization"),
                "GATEWAY_KEY",
            ),
        ] {
            let error = resolve_headers_with_env(&auth, &read_over_limit)
                .await
                .err()
                .expect("oversized environment credentials must fail");
            assert!(matches!(
                error,
                AuthError::EnvironmentValueTooLarge { var, max_bytes }
                    if (var == "BEARER_KEY" || var == "HEADER_KEY" || var == "GATEWAY_KEY")
                        && max_bytes == MAX_ENV_VALUE_BYTES
            ));
        }
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

    #[tokio::test]
    async fn sigv4_signs_a_fixed_request_against_the_aws_fixture() {
        let credentials = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
        )
        .unwrap();
        let signer = AwsSigV4Signer::new(credentials, "us-east-1", "iam")
            .unwrap()
            .with_clock(Arc::new(|| {
                UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160)
            }));
        let mut headers = http::HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded; charset=utf-8"),
        );
        let request = SigningRequest::new(
            http::Method::GET,
            url::Url::parse("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08")
                .unwrap(),
            bytes::Bytes::new(),
            headers,
        );

        let signed = signer.sign(&request).await.unwrap();
        assert_eq!(
            signed.headers.get(AUTHORIZATION).unwrap(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=dd479fa8a80364edf2119ec24bebde66712ee9c9cb2b0d92eb3ab9ccdc0c3947"
        );
        assert_eq!(
            signed.headers.get("x-amz-date").unwrap(),
            "20150830T123600Z"
        );
        assert!(signed.headers.get(AUTHORIZATION).unwrap().is_sensitive());
    }

    #[tokio::test]
    async fn sigv4_session_credentials_are_sensitive_and_redacted() {
        let signer = AwsSigV4Signer::new(
            AwsCredentials::new(
                "session-access",
                "session-secret",
                Some(Secret::from("session-token")),
            )
            .unwrap(),
            "us-east-1",
            "bedrock",
        )
        .unwrap()
        .with_clock(Arc::new(|| UNIX_EPOCH));
        let resolved = resolve_headers_for_request(
            &Auth::request_signer(Arc::new(signer)),
            http::Method::POST,
            url::Url::parse(
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/example/converse-stream",
            )
            .unwrap(),
            bytes::Bytes::new(),
            http::HeaderMap::new(),
        )
        .await
        .unwrap();

        assert!(resolved.headers["x-amz-security-token"].is_sensitive());
        let authorization = resolved.headers[AUTHORIZATION].to_str().unwrap();
        let redacted = resolved
            .redactor
            .redact(&format!("session-secret session-token {authorization}"));
        assert!(!redacted.contains("session-secret"));
        assert!(!redacted.contains("session-token"));
        assert!(!redacted.contains("Credential=session-access"));
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
