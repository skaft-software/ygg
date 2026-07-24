#![allow(missing_docs)]

//! OpenAI device authorization, token exchange/refresh, and JWT claim
//! validation. The flow mirrors Pi's TypeScript OpenAI Codex OAuth provider.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;

use super::{
    now_unix, CLIENT_ID, DEVICE_REDIRECT_URI, DEVICE_TOKEN_URL, DEVICE_USER_CODE_URL,
    JWT_AUTH_CLAIM, TOKEN_URL,
};

/// A token endpoint result with an absolute expiry (Unix seconds).
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_at: u64,
}

/// ChatGPT account plan carried by the OAuth JWT. Keep unknown values intact so
/// a newly introduced plan never gets mistaken for a known entitlement tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChatGptPlan {
    Free,
    Go,
    Plus,
    Pro,
    ProLite,
    Team,
    SelfServeBusinessUsageBased,
    Business,
    EnterpriseCbpUsageBased,
    Enterprise,
    Edu,
    Unknown(String),
}

impl ChatGptPlan {
    fn from_raw(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        Some(match normalized.as_str() {
            "free" => Self::Free,
            "go" => Self::Go,
            "plus" => Self::Plus,
            "pro" => Self::Pro,
            "prolite" => Self::ProLite,
            "team" => Self::Team,
            "self_serve_business_usage_based" => Self::SelfServeBusinessUsageBased,
            "business" => Self::Business,
            "enterprise_cbp_usage_based" => Self::EnterpriseCbpUsageBased,
            "enterprise" | "hc" => Self::Enterprise,
            "education" | "edu" | "edu_plus" | "edu_pro" => Self::Edu,
            _ => Self::Unknown(normalized),
        })
    }

    pub(crate) fn raw_value(&self) -> &str {
        match self {
            Self::Free => "free",
            Self::Go => "go",
            Self::Plus => "plus",
            Self::Pro => "pro",
            Self::ProLite => "prolite",
            Self::Team => "team",
            Self::SelfServeBusinessUsageBased => "self_serve_business_usage_based",
            Self::Business => "business",
            Self::EnterpriseCbpUsageBased => "enterprise_cbp_usage_based",
            Self::Enterprise => "enterprise",
            Self::Edu => "edu",
            Self::Unknown(raw) => raw,
        }
    }

    /// Consumer Pro variants may activate the model's larger advertised
    /// context window. Other plans retain the backend's default window.
    pub(crate) fn uses_max_context_window(&self) -> bool {
        matches!(self, Self::Pro | Self::ProLite)
    }

    /// Consumer ChatGPT Pro variants enable GPT-5.6 pro mode.
    pub(crate) fn supports_pro_reasoning_mode(&self) -> bool {
        matches!(self, Self::Pro | Self::ProLite)
    }
}

/// Non-secret routing and entitlement claims derived from a subscription JWT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubscriptionClaims {
    pub account_id: String,
    pub plan: Option<ChatGptPlan>,
}

#[derive(Deserialize)]
struct TokenResponseRaw {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

fn safe_oauth_error_code(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    value
        .get("error")?
        .as_str()
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        .map(str::to_owned)
}

async fn post_token(
    client: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<Tokens> {
    let resp = client
        .post(token_url)
        .form(form)
        .send()
        .await
        .context("token request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if let Some(code) = safe_oauth_error_code(&body) {
            bail!("token endpoint returned {status} ({code})");
        }
        bail!("token endpoint returned {status}");
    }
    let raw: TokenResponseRaw =
        serde_json::from_str(&body).context("token response was not valid JSON")?;
    let access = raw
        .access_token
        .filter(|token| !token.is_empty())
        .context("token response missing access_token")?;
    // OpenAI rotates refresh tokens; accepting a response without the rotated
    // value would persist a credential that may no longer be usable.
    let refresh = raw
        .refresh_token
        .filter(|token| !token.is_empty())
        .context("token response missing refresh_token")?;
    let expires_in = raw
        .expires_in
        .context("token response missing expires_in")?;
    Ok(Tokens {
        access,
        refresh,
        expires_at: now_unix().saturating_add(expires_in),
    })
}

async fn exchange_code_with_redirect(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens> {
    post_token(
        client,
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ],
    )
    .await
}

/// Exchange a completed device authorization for tokens.
pub async fn exchange_device_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<Tokens> {
    exchange_code_with_redirect(client, TOKEN_URL, code, verifier, DEVICE_REDIRECT_URI).await
}

/// Device authorization details returned by OpenAI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAuth {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_seconds: u64,
}

/// Result of one device authorization poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Complete {
        authorization_code: String,
        code_verifier: String,
    },
}

pub(crate) async fn start_device_auth_with_url(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<DeviceAuth> {
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .context("device authorization request failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!("OpenAI Codex device-code login is unavailable; verify the OpenAI auth endpoint");
        }
        bail!("device authorization endpoint returned {status}: {body}");
    }

    let value: serde_json::Value =
        serde_json::from_str(&body).context("device authorization response was not valid JSON")?;
    let device_auth_id = value
        .get("device_auth_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("device authorization response missing device_auth_id")?;
    let user_code = value
        .get("user_code")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("device authorization response missing user_code")?;
    let interval_seconds = value
        .get("interval")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
        })
        .context("device authorization response missing a valid interval")?;

    Ok(DeviceAuth {
        device_auth_id: device_auth_id.to_owned(),
        user_code: user_code.to_owned(),
        // Avoid a hot polling loop if the service ever returns zero.
        interval_seconds: interval_seconds.max(1),
    })
}

/// Start OpenAI's hosted device-code flow.
pub async fn start_device_auth(client: &reqwest::Client) -> Result<DeviceAuth> {
    start_device_auth_with_url(client, DEVICE_USER_CODE_URL).await
}

pub(crate) async fn poll_device_auth_with_url(
    client: &reqwest::Client,
    endpoint: &str,
    device: &DeviceAuth,
) -> Result<DevicePoll> {
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        }))
        .send()
        .await
        .context("device authorization poll failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    // OpenAI uses both statuses for the ordinary not-authorized-yet state.
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(DevicePoll::Pending);
    }
    if status.is_success() {
        let value: serde_json::Value =
            serde_json::from_str(&body).context("device token response was not valid JSON")?;
        let authorization_code = value
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .context("device token response missing authorization_code")?;
        let code_verifier = value
            .get("code_verifier")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .context("device token response missing code_verifier")?;
        return Ok(DevicePoll::Complete {
            authorization_code: authorization_code.to_owned(),
            code_verifier: code_verifier.to_owned(),
        });
    }

    let error_code = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("error").cloned())
        .and_then(|error| match error {
            serde_json::Value::String(code) => Some(code),
            serde_json::Value::Object(object) => object
                .get("code")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        });
    match error_code.as_deref() {
        Some("deviceauth_authorization_pending") => Ok(DevicePoll::Pending),
        Some("slow_down") => Ok(DevicePoll::SlowDown),
        _ => bail!("device authorization failed with status {status}: {body}"),
    }
}

/// Poll OpenAI once for completion of a device authorization.
pub async fn poll_device_auth(client: &reqwest::Client, device: &DeviceAuth) -> Result<DevicePoll> {
    poll_device_auth_with_url(client, DEVICE_TOKEN_URL, device).await
}

/// Refresh against the token endpoint (overridable for tests/proxies),
/// rotating the refresh token. Production callers pass [`TOKEN_URL`].
pub async fn refresh_with_url(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    post_token(
        client,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
    )
    .await
}

/// Decode a JWT payload's claims (no signature verification — we only read
/// non-authoritative claims like the account id, which the backend re-checks).
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Decode and validate the account-routing and plan claims from a ChatGPT
/// subscription token. Claims are advisory for local catalog selection; the
/// backend still authorizes every request and account entitlement.
pub(crate) fn subscription_claims(access: &str) -> Result<SubscriptionClaims> {
    let claims = decode_jwt_claims(access).context("access token was not a valid JWT")?;
    let auth = claims
        .get(JWT_AUTH_CLAIM)
        .and_then(serde_json::Value::as_object)
        .context("access token did not contain ChatGPT authorization claims")?;
    if auth
        .get("localhost")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        bail!(
            "stored OpenAI credential is localhost-only and cannot access the ChatGPT model pool; run `ygg --login codex` again"
        );
    }
    let account_id = auth
        .get("chatgpt_account_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .context("access token did not contain a chatgpt_account_id")?;
    let plan = auth
        .get("chatgpt_plan_type")
        .and_then(serde_json::Value::as_str)
        .and_then(ChatGptPlan::from_raw);
    Ok(SubscriptionClaims { account_id, plan })
}

/// Validate that an access token represents a real ChatGPT subscription
/// session rather than the localhost-only token produced by the old Ygg
/// browser flow. OpenAI routes the latter through a reduced/free model pool,
/// which is why selecting otherwise valid subscription models yielded 404s.
pub fn validate_subscription_token(access: &str) -> Result<String> {
    Ok(subscription_claims(access)?.account_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_token_validation_reads_account_and_rejects_localhost_credentials() {
        // A JWT with an unsigned payload carrying the auth claim.
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123",
                "chatgpt_plan_type": "pro"
            }
        });
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("h.{encoded}.s");
        assert_eq!(validate_subscription_token(&token).unwrap(), "acct_123");
        assert_eq!(
            subscription_claims(&token).unwrap().plan,
            Some(ChatGptPlan::Pro)
        );
        assert!(validate_subscription_token("not-a-jwt").is_err());
        assert!(validate_subscription_token(&format!("h.{encoded}")).is_err());

        let localhost = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123",
                "localhost": true
            }
        });
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&localhost).unwrap());
        let token = format!("h.{encoded}.s");
        assert!(validate_subscription_token(&token)
            .unwrap_err()
            .to_string()
            .contains("localhost-only"));
    }

    #[test]
    fn plan_claims_cover_pro_lite_aliases_and_preserve_unknown_values() {
        assert_eq!(ChatGptPlan::from_raw("prolite"), Some(ChatGptPlan::ProLite));
        assert!(ChatGptPlan::Pro.uses_max_context_window());
        assert!(ChatGptPlan::ProLite.uses_max_context_window());
        assert!(!ChatGptPlan::Plus.uses_max_context_window());
        assert!(ChatGptPlan::Pro.supports_pro_reasoning_mode());
        assert!(ChatGptPlan::ProLite.supports_pro_reasoning_mode());
        assert!(!ChatGptPlan::Plus.supports_pro_reasoning_mode());
        assert_eq!(
            ChatGptPlan::from_raw(" Future_Tier "),
            Some(ChatGptPlan::Unknown("future_tier".into()))
        );
        assert_eq!(
            ChatGptPlan::from_raw("education").unwrap().raw_value(),
            "edu"
        );
    }

    #[tokio::test]
    async fn device_flow_parses_start_pending_and_completion_responses() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/start"))
            .and(body_json(serde_json::json!({ "client_id": CLIENT_ID })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "device-1",
                "user_code": "ABCD-1234",
                "interval": "5"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/pending"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": { "code": "deviceauth_authorization_pending" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/pending-json"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": { "code": "deviceauth_authorization_pending" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/slow-down"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "error": "slow_down"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/complete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authorization_code": "oauth-code",
                "code_verifier": "device-verifier"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let device = start_device_auth_with_url(&client, &format!("{}/start", server.uri()))
            .await
            .unwrap();
        assert_eq!(
            device,
            DeviceAuth {
                device_auth_id: "device-1".into(),
                user_code: "ABCD-1234".into(),
                interval_seconds: 5,
            }
        );
        assert_eq!(
            poll_device_auth_with_url(&client, &format!("{}/pending", server.uri()), &device)
                .await
                .unwrap(),
            DevicePoll::Pending
        );
        assert_eq!(
            poll_device_auth_with_url(&client, &format!("{}/pending-json", server.uri()), &device,)
                .await
                .unwrap(),
            DevicePoll::Pending
        );
        assert_eq!(
            poll_device_auth_with_url(&client, &format!("{}/slow-down", server.uri()), &device,)
                .await
                .unwrap(),
            DevicePoll::SlowDown
        );
        assert_eq!(
            poll_device_auth_with_url(&client, &format!("{}/complete", server.uri()), &device)
                .await
                .unwrap(),
            DevicePoll::Complete {
                authorization_code: "oauth-code".into(),
                code_verifier: "device-verifier".into(),
            }
        );
    }

    #[tokio::test]
    async fn device_code_exchange_uses_the_hosted_redirect() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=oauth-code"))
            .and(body_string_contains("code_verifier=device-verifier"))
            .and(body_string_contains(
                "redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let tokens = exchange_code_with_redirect(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "oauth-code",
            "device-verifier",
            DEVICE_REDIRECT_URI,
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "access");
        assert_eq!(tokens.refresh, "refresh");
    }

    #[tokio::test]
    async fn token_errors_do_not_echo_response_secrets() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "leaked-secret-value"
            })))
            .mount(&server)
            .await;

        let error = match post_token(&reqwest::Client::new(), &server.uri(), &[]).await {
            Err(error) => error,
            Ok(_) => panic!("failing token endpoint unexpectedly returned credentials"),
        };
        let message = error.to_string();
        assert!(message.contains("invalid_grant"), "{message}");
        assert!(!message.contains("leaked-secret-value"), "{message}");
    }

    #[tokio::test]
    async fn token_response_requires_a_rotated_refresh_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let error = match refresh_with_url(
            &reqwest::Client::new(),
            &format!("{}/token", server.uri()),
            "old-refresh",
        )
        .await
        {
            Ok(_) => panic!("missing rotated refresh token must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("missing refresh_token"));
    }
}
