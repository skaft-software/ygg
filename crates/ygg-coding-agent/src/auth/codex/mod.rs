#![allow(missing_docs)]

//! OpenAI Codex ("Sign in with ChatGPT") OAuth.
//!
//! Constants and the flow are grounded in the live Codex CLI and cross-checked
//! against Pi's reference implementation (see
//! `docs/superpowers/specs/2026-07-13-codex-oauth-design.md`). The subscription
//! backend is `https://chatgpt.com/backend-api/codex/`, wire-compatible with
//! ygg's existing `Protocol::OpenAiResponses` codec; the Codex-specific headers
//! are injected via the endpoint's static headers plus this resolver's dynamic
//! `extra_headers`.

mod login;
mod oauth;
mod resolver;
mod store;

pub use login::{login, logout};
pub use resolver::CodexResolver;
pub use store::{default_path, CredentialStore};

/// Public OAuth client id of the Codex CLI (not a secret).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Token endpoint (device-code exchange + refresh).
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// OAuth device authorization start endpoint.
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
/// OAuth device authorization polling endpoint.
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// OpenAI-hosted device verification page.
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
/// Redirect used when exchanging a completed device authorization.
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// Device codes expire after fifteen minutes.
pub const DEVICE_CODE_TIMEOUT_SECS: u64 = 15 * 60;
/// Originator tag sent to the authorization server and the backend.
pub const ORIGINATOR: &str = "ygg";
/// JWT claim namespace carrying the ChatGPT account id.
pub const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Subscription backend base URL (endpoint `base_url`, must end in `/`).
pub const BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex/";
/// Endpoint id registered in the model catalog.
pub const ENDPOINT_ID: &str = "openai-codex";

/// Models supported by ChatGPT's SSE Codex endpoint. The first is the
/// recommended default. `gpt-5.6-luna` is intentionally absent: Ygg's current
/// SSE request semantics receive `Model not found`, while Codex's WebSocket and
/// richer HTTP fallback requests succeed. Ygg must not advertise a model its
/// transport cannot run.
pub const MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
];

/// Seconds before nominal expiry at which a token is proactively refreshed.
pub const REFRESH_SKEW_SECS: u64 = 60;

/// Return whether the store contains a usable ChatGPT subscription credential.
/// Legacy browser credentials marked `localhost` are rejected because OpenAI
/// routes them as local/free credentials and returns misleading model 404s.
pub fn has_usable_credential(store: &CredentialStore) -> anyhow::Result<bool> {
    let Some(credential) = store.load()? else {
        return Ok(false);
    };
    if credential.tokens.refresh_token.trim().is_empty() {
        anyhow::bail!("OpenAI Codex credential has no refresh token; sign in again");
    }
    oauth::validate_subscription_token(&credential.tokens.access_token)?;
    Ok(true)
}

/// Seconds since the Unix epoch.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lowercase hex of `n` cryptographically random bytes for `session-id`.
pub(crate) fn random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
