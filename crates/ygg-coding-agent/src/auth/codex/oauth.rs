#![allow(missing_docs)]

//! Pure OAuth mechanics: PKCE, the authorize URL, token exchange/refresh, and
//! JWT claim decoding. Network calls use `reqwest`; everything else is pure and
//! unit-tested.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    now_unix, random_hex, AUTHORIZE_URL, CLIENT_ID, JWT_AUTH_CLAIM, ORIGINATOR, REDIRECT_URI, SCOPE,
    TOKEN_URL,
};

/// A PKCE verifier/challenge pair (S256).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE pair: verifier = base64url(32 random bytes), challenge =
/// base64url(SHA256(verifier)).
pub fn generate_pkce() -> Pkce {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

/// A random CSRF `state` value (128 bits, hex).
pub fn random_state() -> String {
    random_hex(16)
}

/// Build the browser authorization URL with PKCE + Codex's extra flow params.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid authorize url");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    url.to_string()
}

/// A token endpoint result with an absolute expiry (Unix seconds).
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResponseRaw {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

async fn post_token(
    client: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
    fallback_refresh: Option<&str>,
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
        bail!("token endpoint returned {status}: {body}");
    }
    let raw: TokenResponseRaw =
        serde_json::from_str(&body).context("token response was not valid JSON")?;
    let access = raw.access_token.context("token response missing access_token")?;
    // Refresh responses rotate the refresh token; if one is somehow absent, keep
    // the token we already had rather than losing the ability to refresh.
    let refresh = raw
        .refresh_token
        .or_else(|| fallback_refresh.map(str::to_owned))
        .context("token response missing refresh_token")?;
    let expires_in = raw.expires_in.context("token response missing expires_in")?;
    Ok(Tokens {
        access,
        refresh,
        expires_at: now_unix() + expires_in,
    })
}

/// Exchange an authorization code for tokens (PKCE).
pub async fn exchange_code(client: &reqwest::Client, code: &str, verifier: &str) -> Result<Tokens> {
    post_token(
        client,
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
        None,
    )
    .await
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
        Some(refresh_token),
    )
    .await
}

/// Decode a JWT payload's claims (no signature verification — we only read
/// non-authoritative claims like the account id, which the backend re-checks).
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Extract the ChatGPT account id from an access token.
pub fn decode_account_id(access: &str) -> Option<String> {
    decode_jwt_claims(access)?
        .get(JWT_AUTH_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parsed authorization input pasted by the user (raw code or a redirect URL).
pub struct AuthorizationInput {
    pub code: Option<String>,
    pub state: Option<String>,
}

/// Parse a pasted value: a full redirect URL, a bare `code=…&state=…` query, or
/// a raw code.
pub fn parse_authorization_input(input: &str) -> AuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return AuthorizationInput {
            code: None,
            state: None,
        };
    }
    if let Ok(url) = url::Url::parse(value) {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        return AuthorizationInput { code, state };
    }
    if value.contains("code=") {
        let mut code = None;
        let mut state = None;
        for pair in value.trim_start_matches('?').split('&') {
            if let Some(rest) = pair.strip_prefix("code=") {
                code = Some(rest.to_owned());
            } else if let Some(rest) = pair.strip_prefix("state=") {
                state = Some(rest.to_owned());
            }
        }
        return AuthorizationInput { code, state };
    }
    AuthorizationInput {
        code: Some(value.to_owned()),
        state: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_is_well_formed() {
        let a = generate_pkce();
        // base64url(32 bytes) -> 43 chars, no padding; challenge is base64url of a
        // 32-byte SHA-256 digest -> also 43 chars.
        assert_eq!(a.verifier.len(), 43);
        assert_eq!(a.challenge.len(), 43);
        assert!(!a.verifier.contains('=') && !a.verifier.contains('+') && !a.verifier.contains('/'));
        // Distinct verifier and a deterministic challenge for that verifier.
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(a.verifier.as_bytes()));
        assert_eq!(a.challenge, expect);
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier, "verifier must be random");
    }

    #[test]
    fn authorize_url_carries_all_required_params() {
        let url = authorize_url("CHAL", "STATE");
        let parsed = url::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["code_challenge"], "CHAL");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "STATE");
        assert_eq!(q["id_token_add_organizations"], "true");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(q["originator"], ORIGINATOR);
    }

    #[test]
    fn decode_account_id_reads_the_claim() {
        // A JWT with an unsigned payload carrying the auth claim.
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_123" }
        });
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("h.{encoded}.s");
        assert_eq!(decode_account_id(&token).as_deref(), Some("acct_123"));
        assert_eq!(decode_account_id("not-a-jwt"), None);
    }

    #[test]
    fn parse_authorization_input_forms() {
        let from_url =
            parse_authorization_input("http://localhost:1455/auth/callback?code=abc&state=xyz");
        assert_eq!(from_url.code.as_deref(), Some("abc"));
        assert_eq!(from_url.state.as_deref(), Some("xyz"));

        let from_query = parse_authorization_input("code=abc&state=xyz");
        assert_eq!(from_query.code.as_deref(), Some("abc"));
        assert_eq!(from_query.state.as_deref(), Some("xyz"));

        let raw = parse_authorization_input("  just-a-code  ");
        assert_eq!(raw.code.as_deref(), Some("just-a-code"));
        assert_eq!(raw.state, None);
    }
}
