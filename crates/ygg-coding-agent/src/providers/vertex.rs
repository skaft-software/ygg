//! Google Vertex Application Default Credential resolution.
//!
//! The resolver accepts only owner-private local ADC files, constructs the
//! regional Vertex endpoint from validated project/location segments, and sends
//! refreshes exclusively to Google's fixed OAuth token authority. Credential
//! values are retained only in this module and in `ygg_ai::Auth`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use tokio::sync::Mutex;
use ygg_ai::{Auth, AuthError, CredentialResolver, CredentialScheme, ResolvedCredential, Secret};

const MAX_ADC_BYTES: usize = 128 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 32 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// A fully validated Vertex endpoint/auth binding. It has no `Debug`
/// implementation so accidental diagnostics cannot reveal dynamic credential
/// implementation details.
pub(crate) struct VertexConfiguration {
    pub(crate) auth: Auth,
    pub(crate) base_url: url::Url,
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

impl CachedToken {
    fn usable(&self) -> bool {
        Instant::now()
            .checked_add(TOKEN_REFRESH_SKEW)
            .is_some_and(|refresh_at| refresh_at < self.expires_at)
    }
}

enum AdcSource {
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
    ServiceAccount {
        client_email: String,
        signer: Arc<RsaKeyPair>,
    },
}

/// Refreshes a short-lived OAuth access token. The mutex intentionally spans
/// refresh I/O: all simultaneous inference requests share one token exchange
/// rather than stampeding the token authority.
struct VertexAdcResolver {
    source: AdcSource,
    http: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
    // Test-only local transport injection proves token handling without making
    // the production OAuth authority configurable.
    #[cfg(test)]
    token_url: Option<url::Url>,
}

#[derive(serde::Deserialize)]
struct AdcFile {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    client_email: Option<String>,
    #[serde(default)]
    private_key: Option<String>,
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

struct ParsedAdc {
    source: AdcSource,
    project_id: Option<String>,
}

/// Resolve a configured Vertex endpoint without performing a network request.
///
/// A missing ADC file means Vertex is simply unavailable, so its static models
/// stay out of the picker. Invalid configured values are returned as a safe,
/// credential-free error for bootstrap to report.
pub(crate) fn resolve_application_default_credentials(
) -> anyhow::Result<Option<VertexConfiguration>> {
    let Some((path, explicit_path)) = adc_path()? else {
        return Ok(None);
    };
    let Some(bytes) = crate::auth::read_bounded_private(&path, MAX_ADC_BYTES)? else {
        if explicit_path {
            anyhow::bail!("GOOGLE_APPLICATION_CREDENTIALS does not name a readable private file");
        }
        return Ok(None);
    };
    let parsed = parse_adc(&bytes)?;

    let project = first_environment_value(&["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"])?
        .or(parsed.project_id)
        .ok_or_else(|| anyhow::anyhow!("Vertex requires GOOGLE_CLOUD_PROJECT"))?;
    let location = first_environment_value(&["GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_REGION"])?
        .ok_or_else(|| anyhow::anyhow!("Vertex requires GOOGLE_CLOUD_LOCATION"))?;
    let base_url = vertex_base_url(&project, &location)?;
    let http = reqwest::Client::builder()
        .timeout(TOKEN_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("could not initialize Vertex credential transport")?;
    let resolver = VertexAdcResolver {
        source: parsed.source,
        http,
        cached: Mutex::new(None),
        #[cfg(test)]
        token_url: None,
    };
    Ok(Some(VertexConfiguration {
        auth: Auth::dynamic(Arc::new(resolver)),
        base_url,
    }))
}

fn adc_path() -> anyhow::Result<Option<(PathBuf, bool)>> {
    if let Some(value) = first_environment_value(&["GOOGLE_APPLICATION_CREDENTIALS"])? {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            anyhow::bail!("GOOGLE_APPLICATION_CREDENTIALS must be an absolute path");
        }
        return Ok(Some((path, true)));
    }

    let Some(config_dir) = dirs::config_dir() else {
        return Ok(None);
    };
    let path = config_dir.join("gcloud/application_default_credentials.json");
    if !path.is_absolute() {
        // An untrusted relative XDG config path must not redirect a private
        // credential read through the launch directory.
        anyhow::bail!("default ADC credential path is not absolute");
    }
    Ok(Some((path, false)))
}

fn first_environment_value(names: &[&str]) -> anyhow::Result<Option<String>> {
    for name in names {
        let value = match ygg_ai::auth::read_bounded_env(name) {
            Ok(value) => value,
            Err(ygg_ai::ConfigError::InvalidEnv(_)) => {
                anyhow::bail!("could not read {name}: invalid environment value")
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(value) = value.map(|value| value.trim().to_owned()) {
            if !value.is_empty() {
                return Ok(Some(value));
            }
        }
    }
    Ok(None)
}

fn parse_adc(bytes: &[u8]) -> anyhow::Result<ParsedAdc> {
    let file: AdcFile = serde_json::from_slice(bytes).map_err(|_| {
        anyhow::anyhow!("Google Application Default Credentials file is not valid JSON")
    })?;
    let project_id = file.project_id.filter(|project| !project.trim().is_empty());
    let source = match file.kind.as_str() {
        "authorized_user" => {
            let client_id = required_adc_string(file.client_id, "client_id")?;
            let client_secret = required_adc_string(file.client_secret, "client_secret")?;
            let refresh_token = required_adc_string(file.refresh_token, "refresh_token")?;
            AdcSource::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
            }
        }
        "service_account" => {
            let client_email = required_adc_string(file.client_email, "client_email")?;
            if client_email.len() > 1024 || !client_email.contains('@') {
                anyhow::bail!("ADC service-account client_email is invalid");
            }
            let private_key = required_adc_string(file.private_key, "private_key")?;
            let signer = Arc::new(parse_service_account_signer(&private_key)?);
            AdcSource::ServiceAccount {
                client_email,
                signer,
            }
        }
        _ => anyhow::bail!("ADC credential type is not supported for Vertex"),
    };
    Ok(ParsedAdc { source, project_id })
}

fn required_adc_string(value: Option<String>, field: &str) -> anyhow::Result<String> {
    let value = value.ok_or_else(|| anyhow::anyhow!("ADC credential is missing {field}"))?;
    if value.trim().is_empty() || value.len() > MAX_ADC_BYTES {
        anyhow::bail!("ADC credential {field} is invalid");
    }
    Ok(value)
}

fn parse_service_account_signer(private_key: &str) -> anyhow::Result<RsaKeyPair> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let body = private_key
        .strip_prefix(BEGIN)
        .and_then(|value| value.strip_suffix(END))
        .ok_or_else(|| anyhow::anyhow!("ADC service-account key must use PKCS#8 PEM"))?;
    let compact = body
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.is_empty() || compact.len() > MAX_ADC_BYTES {
        anyhow::bail!("ADC service-account key is invalid");
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|_| anyhow::anyhow!("ADC service-account key is invalid"))?;
    RsaKeyPair::from_pkcs8(&der)
        .map_err(|_| anyhow::anyhow!("ADC service-account key is not a usable RSA key"))
}

fn valid_project_segment(value: &str) -> bool {
    value.len() <= 63
        && value.len() >= 6
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn valid_location_segment(value: &str) -> bool {
    value.len() <= 63
        && value.len() >= 2
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

/// Construct the only Vertex inference authority accepted by this resolver.
/// Both caller-provided values must remain individual DNS/path-safe segments.
fn vertex_base_url(project: &str, location: &str) -> anyhow::Result<url::Url> {
    if !valid_project_segment(project) {
        anyhow::bail!("Vertex project identifier is invalid");
    }
    if !valid_location_segment(location) {
        anyhow::bail!("Vertex location is invalid");
    }
    // Neither the ADC file nor an environment value can select another host or
    // escape this fixed API path.
    url::Url::parse(&format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/"
    ))
    .context("could not construct Vertex endpoint")
}

impl VertexAdcResolver {
    fn token_url(&self) -> &str {
        #[cfg(test)]
        if let Some(token_url) = &self.token_url {
            return token_url.as_str();
        }
        #[cfg(not(test))]
        let _ = self;
        TOKEN_URL
    }

    async fn access_token(&self) -> Result<String, AuthError> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref().filter(|token| token.usable()) {
            return Ok(token.value.clone());
        }

        let token = self.refresh().await?;
        let value = token.value.clone();
        *cached = Some(token);
        Ok(value)
    }

    async fn refresh(&self) -> Result<CachedToken, AuthError> {
        let form = match &self.source {
            AdcSource::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
            } => vec![
                ("grant_type", "refresh_token".to_owned()),
                ("client_id", client_id.clone()),
                ("client_secret", client_secret.clone()),
                ("refresh_token", refresh_token.clone()),
            ],
            AdcSource::ServiceAccount {
                client_email,
                signer,
            } => vec![
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:jwt-bearer".to_owned(),
                ),
                (
                    "assertion",
                    service_account_assertion(client_email, signer)?,
                ),
            ],
        };
        let response = self
            .http
            .post(self.token_url())
            .form(&form)
            .send()
            .await
            .map_err(|_| AuthError::Resolve)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
        {
            return Err(AuthError::Resolve);
        }
        // Do not use `Response::bytes`: a peer that omits Content-Length could
        // otherwise force an unbounded allocation before we inspect the size.
        let mut response = response;
        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or_default()
                .min(MAX_TOKEN_RESPONSE_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Resolve)? {
            let Some(next_len) = body.len().checked_add(chunk.len()) else {
                return Err(AuthError::Resolve);
            };
            if next_len > MAX_TOKEN_RESPONSE_BYTES {
                return Err(AuthError::Resolve);
            }
            body.extend_from_slice(&chunk);
        }
        let response: TokenResponse =
            serde_json::from_slice(&body).map_err(|_| AuthError::Resolve)?;
        if response.access_token.trim().is_empty()
            || response.access_token.len() > MAX_ACCESS_TOKEN_BYTES
        {
            return Err(AuthError::Resolve);
        }
        let lifetime =
            Duration::from_secs(response.expires_in.unwrap_or(3600)).min(MAX_TOKEN_LIFETIME);
        let expires_at = Instant::now()
            .checked_add(lifetime)
            .ok_or(AuthError::Resolve)?;
        Ok(CachedToken {
            value: response.access_token,
            expires_at,
        })
    }
}

fn service_account_assertion(client_email: &str, signer: &RsaKeyPair) -> Result<String, AuthError> {
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::Resolve)?
        .as_secs();
    let header =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": TOKEN_URL,
        "iat": issued_at,
        "exp": issued_at.saturating_add(3600),
    });
    let claims = serde_json::to_vec(&claims).map_err(|_| AuthError::Resolve)?;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    let signing_input = format!("{header}.{claims}");
    let mut signature = vec![0; signer.public().modulus_len()];
    signer
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| AuthError::Resolve)?;
    Ok(format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature)
    ))
}

#[async_trait::async_trait]
impl CredentialResolver for VertexAdcResolver {
    async fn resolve(&self) -> Result<ResolvedCredential, AuthError> {
        let value = self.access_token().await?;
        Ok(ResolvedCredential {
            scheme: CredentialScheme::Bearer,
            value: Secret::from(value),
            extra_headers: http::HeaderMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_segments_cannot_escape_the_google_authority() {
        let endpoint = vertex_base_url("project-123", "us-central1").unwrap();
        assert_eq!(
            endpoint.as_str(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/project-123/locations/us-central1/publishers/google/"
        );
        assert!(vertex_base_url("project/../other", "us-central1").is_err());
        assert!(vertex_base_url("project-123", "https://example.invalid").is_err());
    }

    #[test]
    fn service_account_key_requires_pkcs8_pem() {
        assert!(parse_service_account_signer("not a key").is_err());
    }

    #[tokio::test]
    async fn authorized_user_refresh_uses_offline_transport_and_caches() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_id=fake-client"))
            .and(body_string_contains("refresh_token=fake-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fake-access-token",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let resolver = VertexAdcResolver {
            source: AdcSource::AuthorizedUser {
                client_id: "fake-client".to_owned(),
                client_secret: "fake-secret".to_owned(),
                refresh_token: "fake-refresh".to_owned(),
            },
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            cached: Mutex::new(None),
            token_url: Some(url::Url::parse(&format!("{}/token", server.uri())).unwrap()),
        };

        let first = resolver.resolve().await.unwrap();
        assert!(matches!(first.scheme, CredentialScheme::Bearer));
        assert_eq!(first.value.to_string(), "<redacted>");
        let second = resolver.resolve().await.unwrap();
        assert!(matches!(second.scheme, CredentialScheme::Bearer));
    }
}
