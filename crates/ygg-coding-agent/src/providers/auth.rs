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
        EndpointAuthPresentation::Header(name) => Ok(Auth::header_env(
            http::HeaderName::from_bytes(name.as_bytes())?,
            credential.variable(),
        )),
        EndpointAuthPresentation::GoogleApiKeyHeader => Ok(Auth::header_env(
            http::HeaderName::from_static("x-goog-api-key"),
            credential.variable(),
        )),
        EndpointAuthPresentation::AwsSigV4 | EndpointAuthPresentation::Dynamic => {
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
        EndpointAuthPresentation::Header(name) => (
            http::HeaderName::from_bytes(name.as_bytes())?,
            credential.value().to_owned(),
        ),
        EndpointAuthPresentation::GoogleApiKeyHeader => (
            http::HeaderName::from_static("x-goog-api-key"),
            credential.value().to_owned(),
        ),
        EndpointAuthPresentation::AwsSigV4 | EndpointAuthPresentation::Dynamic => {
            anyhow::bail!("environment provider declaration has an invalid credential presentation")
        }
    };
    let mut value = http::HeaderValue::from_str(&value)?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(headers)
}

/// Return a request-signing auth strategy after confirming that the bounded AWS
/// credential chain has a usable source. The signer resolves the chain again on
/// each request, allowing ECS/EC2 metadata credentials to rotate without
/// leaking the private source into provider declarations.
pub(crate) fn aws_bedrock_auth(region: &str) -> anyhow::Result<Option<Auth>> {
    if resolve_aws_credentials()?.is_none() {
        return Ok(None);
    }
    Ok(Some(Auth::request_signer(std::sync::Arc::new(
        AwsBedrockSigner {
            region: region.to_owned(),
        },
    ))))
}

/// Resolve the regional Bedrock Runtime endpoint from bounded configuration.
pub(crate) fn aws_bedrock_base_url(region: &str) -> anyhow::Result<url::Url> {
    if let Some(override_url) = optional_bounded_env("YGG_BEDROCK_ENDPOINT")? {
        let mut url = url::Url::parse(&override_url)
            .map_err(|_| anyhow::anyhow!("invalid YGG_BEDROCK_ENDPOINT"))?;
        if !matches!(url.scheme(), "https" | "http")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            anyhow::bail!("invalid YGG_BEDROCK_ENDPOINT");
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        return Ok(url);
    }
    let domain = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    url::Url::parse(&format!("https://bedrock-runtime.{region}.{domain}/"))
        .map_err(|_| anyhow::anyhow!("invalid AWS region"))
}

/// Resolve the Bedrock region from product and standard AWS environment setup.
pub(crate) fn aws_bedrock_region() -> anyhow::Result<String> {
    for variable in ["YGG_BEDROCK_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Some(region) = optional_bounded_env(variable)? {
            return checked_aws_region(region, variable);
        }
    }
    if let Some(region) = aws_profile_value("region", true)? {
        return checked_aws_region(region, "AWS profile region");
    }
    Ok("us-east-1".to_owned())
}

#[derive(Debug)]
struct AwsBedrockSigner {
    region: String,
}

#[async_trait::async_trait]
impl ygg_ai::RequestSigner for AwsBedrockSigner {
    async fn sign(
        &self,
        request: &ygg_ai::SigningRequest,
    ) -> Result<ygg_ai::SignedRequestHeaders, ygg_ai::AuthError> {
        let credentials = tokio::task::spawn_blocking(resolve_aws_credentials)
            .await
            .map_err(|_| ygg_ai::AuthError::Resolve)?
            .map_err(|_| ygg_ai::AuthError::Resolve)?
            .ok_or(ygg_ai::AuthError::Resolve)?;
        let signer = ygg_ai::AwsSigV4Signer::new(credentials, self.region.clone(), "bedrock")?;
        ygg_ai::RequestSigner::sign(&signer, request).await
    }
}

const MAX_AWS_PROFILE_BYTES: usize = 64 * 1024;
const MAX_AWS_METADATA_BYTES: usize = 64 * 1024;
const AWS_METADATA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn resolve_aws_credentials() -> anyhow::Result<Option<ygg_ai::AwsCredentials>> {
    if let Some(credentials) = aws_environment_credentials()? {
        return Ok(Some(credentials));
    }
    if let Some(credentials) = aws_profile_credentials()? {
        return Ok(Some(credentials));
    }
    aws_metadata_credentials()
}

fn aws_environment_credentials() -> anyhow::Result<Option<ygg_ai::AwsCredentials>> {
    let access_key_id = optional_bounded_env("AWS_ACCESS_KEY_ID")?;
    let secret_access_key = optional_bounded_env("AWS_SECRET_ACCESS_KEY")?;
    let session_token = optional_bounded_env("AWS_SESSION_TOKEN")?;
    match (access_key_id, secret_access_key) {
        (None, None) => Ok(None),
        (Some(access_key_id), Some(secret_access_key)) => {
            aws_credentials(access_key_id, secret_access_key, session_token).map(Some)
        }
        _ => {
            anyhow::bail!("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be configured together")
        }
    }
}

fn aws_profile_credentials() -> anyhow::Result<Option<ygg_ai::AwsCredentials>> {
    let Some(values) = aws_profile_values(false)? else {
        return Ok(None);
    };
    let access_key_id = values.get("aws_access_key_id").cloned();
    let secret_access_key = values.get("aws_secret_access_key").cloned();
    let session_token = values
        .get("aws_session_token")
        .cloned()
        .or_else(|| values.get("aws_security_token").cloned());
    match (access_key_id, secret_access_key) {
        (None, None) => Ok(None),
        (Some(access_key_id), Some(secret_access_key)) => {
            aws_credentials(access_key_id, secret_access_key, session_token).map(Some)
        }
        _ => anyhow::bail!("AWS profile has incomplete static credentials"),
    }
}

fn aws_credentials(
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
) -> anyhow::Result<ygg_ai::AwsCredentials> {
    ygg_ai::AwsCredentials::new(
        access_key_id,
        secret_access_key,
        session_token.map(ygg_ai::Secret::from),
    )
    .map_err(|_| anyhow::anyhow!("AWS credential source is invalid"))
}

fn optional_bounded_env(variable: &str) -> anyhow::Result<Option<String>> {
    ygg_ai::auth::read_bounded_env(variable).map_err(|error| match error {
        ygg_ai::ConfigError::InvalidEnv(_) => {
            anyhow::anyhow!("could not read {variable}: invalid environment value")
        }
        other => other.into(),
    })
}

fn checked_aws_region(region: String, source: &str) -> anyhow::Result<String> {
    if region.len() > 128
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        anyhow::bail!("invalid {source}");
    }
    Ok(region)
}

fn aws_profile_name() -> anyhow::Result<String> {
    let profile = optional_bounded_env("AWS_PROFILE")?.unwrap_or_else(|| "default".to_owned());
    if profile.is_empty()
        || profile.len() > 128
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("invalid AWS_PROFILE");
    }
    Ok(profile)
}

fn aws_profile_credentials_path() -> anyhow::Result<Option<std::path::PathBuf>> {
    if let Some(path) = optional_bounded_env("AWS_SHARED_CREDENTIALS_FILE")? {
        return Ok(Some(std::path::PathBuf::from(path)));
    }
    Ok(dirs::home_dir().map(|home| home.join(".aws").join("credentials")))
}

fn aws_config_path() -> anyhow::Result<Option<std::path::PathBuf>> {
    if let Some(path) = optional_bounded_env("AWS_CONFIG_FILE")? {
        return Ok(Some(std::path::PathBuf::from(path)));
    }
    Ok(dirs::home_dir().map(|home| home.join(".aws").join("config")))
}

fn aws_profile_value(key: &str, config_file: bool) -> anyhow::Result<Option<String>> {
    let values = if config_file {
        aws_profile_config_values()?
    } else {
        aws_profile_values(false)?
    };
    Ok(values.and_then(|values| values.get(key).cloned()))
}

fn aws_profile_values(
    config_file: bool,
) -> anyhow::Result<Option<std::collections::BTreeMap<String, String>>> {
    let path = if config_file {
        aws_config_path()?
    } else {
        aws_profile_credentials_path()?
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(contents) = read_bounded_aws_profile(&path)? else {
        return Ok(None);
    };
    let profile = aws_profile_name()?;
    let section = if config_file && profile != "default" {
        format!("profile {profile}")
    } else {
        profile
    };
    Ok(Some(parse_aws_ini_section(&contents, &section)?))
}

fn aws_profile_config_values() -> anyhow::Result<Option<std::collections::BTreeMap<String, String>>>
{
    aws_profile_values(true)
}

fn read_bounded_aws_profile(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!("AWS profile source is not a regular file");
    }
    if metadata.len() > MAX_AWS_PROFILE_BYTES as u64 {
        anyhow::bail!("AWS profile source exceeds the byte limit");
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > MAX_AWS_PROFILE_BYTES {
        anyhow::bail!("AWS profile source exceeds the byte limit");
    }
    String::from_utf8(bytes).map(Some).map_err(Into::into)
}

fn parse_aws_ini_section(
    contents: &str,
    wanted_section: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut selected = false;
    let mut values = std::collections::BTreeMap::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            selected = section.trim() == wanted_section;
            continue;
        }
        if !selected {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key.len() <= 128
            && value.len() <= ygg_ai::auth::MAX_ENV_VALUE_BYTES
            && key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            && !value.is_empty()
            && !value.chars().any(char::is_control)
        {
            values.insert(key, value.to_owned());
        }
    }
    Ok(values)
}

fn aws_metadata_credentials() -> anyhow::Result<Option<ygg_ai::AwsCredentials>> {
    if optional_bounded_env("AWS_EC2_METADATA_DISABLED")?
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        return Ok(None);
    }
    if let Some(url) = ecs_metadata_url()? {
        return metadata_credentials_from_url(&url, true).map(Some);
    }
    ec2_metadata_credentials()
}

fn metadata_http_client() -> anyhow::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(AWS_METADATA_TIMEOUT)
        .timeout(AWS_METADATA_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Into::into)
}

fn ecs_metadata_url() -> anyhow::Result<Option<url::Url>> {
    if let Some(relative) = optional_bounded_env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")? {
        if relative.len() > 2048 || !relative.starts_with('/') || relative.contains(['\r', '\n']) {
            anyhow::bail!("invalid AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
        }
        return url::Url::parse(&format!("http://169.254.170.2{relative}"))
            .map(Some)
            .map_err(Into::into);
    }
    let Some(full) = optional_bounded_env("AWS_CONTAINER_CREDENTIALS_FULL_URI")? else {
        return Ok(None);
    };
    let url = url::Url::parse(&full)
        .map_err(|_| anyhow::anyhow!("invalid AWS_CONTAINER_CREDENTIALS_FULL_URI"))?;
    let allowed_host = matches!(
        url.host_str(),
        Some("169.254.170.2" | "169.254.170.23" | "localhost")
    );
    if !matches!(url.scheme(), "http" | "https")
        || !allowed_host
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("invalid AWS_CONTAINER_CREDENTIALS_FULL_URI");
    }
    Ok(Some(url))
}

fn metadata_credentials_from_url(
    url: &url::Url,
    ecs: bool,
) -> anyhow::Result<ygg_ai::AwsCredentials> {
    let client = metadata_http_client()?;
    let mut request = client.get(url.clone());
    if ecs {
        if let Some(token) = optional_bounded_env("AWS_CONTAINER_AUTHORIZATION_TOKEN")? {
            request = request.header("Authorization", token);
        }
    }
    let response = request
        .send()
        .map_err(|_| anyhow::anyhow!("AWS metadata credential request failed"))?;
    if !response.status().is_success() {
        anyhow::bail!("AWS metadata credential request failed");
    }
    credentials_from_metadata_body(read_bounded_response(response)?)
}

fn ec2_metadata_credentials() -> anyhow::Result<Option<ygg_ai::AwsCredentials>> {
    let client = metadata_http_client()?;
    let token_response = match client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send()
    {
        Ok(response) if response.status().is_success() => response,
        Ok(_) | Err(_) => return Ok(None),
    };
    let token = read_bounded_response(token_response)?;
    if token.is_empty() || token.len() > 512 || token.chars().any(char::is_control) {
        return Ok(None);
    }
    let role_response = match client
        .get("http://169.254.169.254/latest/meta-data/iam/security-credentials/")
        .header("X-aws-ec2-metadata-token", &token)
        .send()
    {
        Ok(response) if response.status().is_success() => response,
        Ok(_) | Err(_) => return Ok(None),
    };
    let role = read_bounded_response(role_response)?;
    let role = role.lines().next().unwrap_or("").trim();
    if role.is_empty()
        || role.len() > 128
        || !role.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'+' | b'=' | b',' | b'.' | b'@')
        })
    {
        return Ok(None);
    }
    let url = format!("http://169.254.169.254/latest/meta-data/iam/security-credentials/{role}");
    let response = match client
        .get(url)
        .header("X-aws-ec2-metadata-token", token)
        .send()
    {
        Ok(response) if response.status().is_success() => response,
        Ok(_) | Err(_) => return Ok(None),
    };
    credentials_from_metadata_body(read_bounded_response(response)?).map(Some)
}

fn read_bounded_response(response: reqwest::blocking::Response) -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    response
        .take((MAX_AWS_METADATA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_AWS_METADATA_BYTES {
        anyhow::bail!("AWS metadata response exceeds the byte limit");
    }
    String::from_utf8(bytes).map_err(Into::into)
}

fn credentials_from_metadata_body(body: String) -> anyhow::Result<ygg_ai::AwsCredentials> {
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| anyhow::anyhow!("AWS metadata returned invalid credentials"))?;
    let access_key_id = value
        .get("AccessKeyId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= ygg_ai::auth::MAX_ENV_VALUE_BYTES)
        .ok_or_else(|| anyhow::anyhow!("AWS metadata returned incomplete credentials"))?
        .to_owned();
    let secret_access_key = value
        .get("SecretAccessKey")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= ygg_ai::auth::MAX_ENV_VALUE_BYTES)
        .ok_or_else(|| anyhow::anyhow!("AWS metadata returned incomplete credentials"))?
        .to_owned();
    let token = value
        .get("Token")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= ygg_ai::auth::MAX_ENV_VALUE_BYTES)
        .map(str::to_owned);
    aws_credentials(access_key_id, secret_access_key, token)
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
    use crate::providers::contract::{ANTHROPIC, CLOUDFLARE_AI_GATEWAY, GEMINI, OPENAI};

    #[test]
    fn aws_profile_parser_selects_only_the_requested_bounded_section() {
        let contents = "\
[default]
aws_access_key_id = default-access
aws_secret_access_key = default-secret

[profile enterprise]
aws_access_key_id = enterprise-access
aws_secret_access_key = enterprise-secret
aws_session_token = enterprise-token
ignored key = ignored
";
        let values = parse_aws_ini_section(contents, "profile enterprise").unwrap();
        assert_eq!(
            values.get("aws_access_key_id").map(String::as_str),
            Some("enterprise-access")
        );
        assert_eq!(
            values.get("aws_secret_access_key").map(String::as_str),
            Some("enterprise-secret")
        );
        assert_eq!(
            values.get("aws_session_token").map(String::as_str),
            Some("enterprise-token")
        );
        assert!(!values.contains_key("ignored key"));
    }

    #[test]
    fn aws_metadata_credentials_are_bounded_and_require_both_key_components() {
        assert!(credentials_from_metadata_body(
            r#"{"AccessKeyId":"metadata-access","SecretAccessKey":"metadata-secret","Token":"metadata-token"}"#
                .to_owned(),
        )
        .is_ok());
        assert!(
            credentials_from_metadata_body(r#"{"AccessKeyId":"metadata-access"}"#.to_owned(),)
                .is_err()
        );
        let oversized = "x".repeat(ygg_ai::auth::MAX_ENV_VALUE_BYTES + 1);
        assert!(credentials_from_metadata_body(format!(
            r#"{{"AccessKeyId":"{oversized}","SecretAccessKey":"metadata-secret"}}"#
        ))
        .is_err());
    }

    #[test]
    fn aws_region_validation_rejects_untrusted_endpoint_components() {
        assert_eq!(
            checked_aws_region("eu-west-1".to_owned(), "test").unwrap(),
            "eu-west-1"
        );
        assert!(checked_aws_region("eu/west-1".to_owned(), "test").is_err());
    }

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
                if name == http::HeaderName::from_static("cf-aig-authorization")
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

        let google = environment_discovery_headers(&GEMINI.routes[0], &credential).unwrap();
        assert!(google[http::HeaderName::from_static("x-goog-api-key")].is_sensitive());
    }
}
