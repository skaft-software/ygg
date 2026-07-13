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
/// Authorization endpoint.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// Token endpoint (code exchange + refresh).
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Loopback redirect the authorization server sends the code to.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// Loopback callback port.
pub const CALLBACK_PORT: u16 = 1455;
/// OAuth scopes.
pub const SCOPE: &str = "openid profile email offline_access";
/// Originator tag sent to the authorization server and the backend.
pub const ORIGINATOR: &str = "ygg";
/// JWT claim namespace carrying the ChatGPT account id.
pub const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Subscription backend base URL (endpoint `base_url`, must end in `/`).
pub const BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api/codex/";
/// Endpoint id registered in the model catalog.
pub const ENDPOINT_ID: &str = "openai-codex";

/// Model ids offered once logged in. The first is the recommended default.
pub const MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.5-pro",
];

/// Seconds before nominal expiry at which a token is proactively refreshed.
pub const REFRESH_SKEW_SECS: u64 = 60;

/// Seconds since the Unix epoch.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lowercase hex of `n` cryptographically-random bytes (for `state`/`session-id`).
pub(crate) fn random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
