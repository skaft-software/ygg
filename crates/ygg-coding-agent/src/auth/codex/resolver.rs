#![allow(missing_docs)]

//! The per-request credential resolver. Fast in the common (unexpired) case;
//! refreshes under a double-checked lock when the token is near expiry.

use anyhow::{anyhow, Result};
use tokio::sync::Mutex;
use ygg_ai::{AuthError, CredentialResolver, CredentialScheme, ResolvedCredential, Secret};

use super::store::{CredentialFile, CredentialStore, Tokens};
use super::{now_unix, oauth, REFRESH_SKEW_SECS, TOKEN_URL};

/// Resolves a Codex bearer token and the Codex-specific dynamic headers.
pub struct CodexResolver {
    store: CredentialStore,
    http: reqwest::Client,
    /// Token endpoint for refresh (overridable in tests/proxies).
    token_url: String,
    /// Serializes token refreshes so racing requests don't stampede the token
    /// endpoint (the inner re-check makes it a proper double-checked lock).
    refresh_lock: Mutex<()>,
}

impl CodexResolver {
    pub fn new(store: CredentialStore) -> Self {
        Self {
            store,
            http: super::http_client(),
            token_url: TOKEN_URL.to_owned(),
            refresh_lock: Mutex::new(()),
        }
    }

    /// Load a non-expired credential, refreshing if necessary.
    async fn load_valid(&self) -> Result<CredentialFile> {
        let cred = self
            .store
            .load()?
            .ok_or_else(|| anyhow!("not signed in to OpenAI Codex; run `ygg --login codex`"))?;
        if now_unix() + REFRESH_SKEW_SECS < cred.expires_at {
            return Ok(cred);
        }

        // Near/after expiry: refresh under the lock, re-checking inside in case
        // another task already refreshed while we waited.
        let _guard = self.refresh_lock.lock().await;
        let cred = self
            .store
            .load()?
            .ok_or_else(|| anyhow!("credential removed during refresh"))?;
        if now_unix() + REFRESH_SKEW_SECS < cred.expires_at {
            return Ok(cred);
        }

        let tokens =
            oauth::refresh_with_url(&self.http, &self.token_url, &cred.tokens.refresh_token)
                .await?;
        // Re-validate every rotated token. Keeping the old account id when the
        // new token lacks subscription claims would silently reintroduce the
        // localhost/free-routing 404 failure.
        let account_id = oauth::validate_subscription_token(&tokens.access)?;
        let refreshed = CredentialFile {
            tokens: Tokens {
                access_token: tokens.access,
                refresh_token: tokens.refresh,
                account_id,
            },
            expires_at: tokens.expires_at,
        };
        self.store.save(&refreshed)?;
        Ok(refreshed)
    }

    /// Resolve the complete header set and subscription identity needed by
    /// catalog discovery.
    ///
    /// Discovery runs before the normal AI client exists, but it must still use
    /// the same refresh, account routing, and plan detection as inference.
    pub(crate) async fn discovery_headers(
        &self,
    ) -> Result<(http::HeaderMap, oauth::SubscriptionClaims)> {
        let cred = self.load_valid().await?;
        let claims = oauth::subscription_claims(&cred.tokens.access_token)?;
        let mut headers = http::HeaderMap::new();

        let mut authorization =
            http::HeaderValue::from_str(&format!("Bearer {}", cred.tokens.access_token))?;
        authorization.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, authorization);
        headers.insert(
            http::HeaderName::from_static("chatgpt-account-id"),
            http::HeaderValue::from_str(&claims.account_id)?,
        );
        Ok((headers, claims))
    }
}

#[async_trait::async_trait]
impl CredentialResolver for CodexResolver {
    async fn resolve(&self) -> Result<ResolvedCredential, AuthError> {
        // AuthError deliberately drops details (they may contain credentials);
        // the actionable "run `ygg --login codex`" guidance is surfaced at
        // registration time, not here.
        let cred = self.load_valid().await.map_err(|_| AuthError::Resolve)?;
        let account_id = oauth::validate_subscription_token(&cred.tokens.access_token)
            .map_err(|_| AuthError::Resolve)?;

        let mut extra_headers = http::HeaderMap::new();
        let account =
            http::HeaderValue::from_str(&account_id).map_err(|_| AuthError::InvalidHeaderValue)?;
        extra_headers.insert(http::HeaderName::from_static("chatgpt-account-id"), account);
        Ok(ResolvedCredential {
            scheme: CredentialScheme::Bearer,
            value: Secret::from(cred.tokens.access_token),
            extra_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(expires_at: u64) -> (tempfile::TempDir, CredentialStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("codex.json"));
        store
            .save(&CredentialFile {
                tokens: Tokens {
                    access_token: jwt_with_account("acct_9"),
                    refresh_token: "r".into(),
                    account_id: "acct_9".into(),
                },
                expires_at,
            })
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn resolves_unexpired_token_without_network() {
        let (_dir, store) = store_with(now_unix() + 3600);
        let resolver = CodexResolver::new(store);
        let cred = resolver.resolve().await.unwrap();
        assert_eq!(cred.value.to_string(), "<redacted>");
        assert_eq!(
            cred.extra_headers
                .get("chatgpt-account-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "acct_9"
        );
        assert!(cred.extra_headers.get("session-id").is_none());
    }

    #[tokio::test]
    async fn missing_credential_is_a_resolve_error() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = CodexResolver::new(CredentialStore::new(dir.path().join("codex.json")));
        assert!(matches!(resolver.resolve().await, Err(AuthError::Resolve)));
    }

    /// A minimal unsigned JWT whose payload carries the ChatGPT account claim,
    /// so refresh validation can derive the account header.
    fn jwt_with_account(account: &str) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account }
        });
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        format!("h.{encoded}.s")
    }

    #[tokio::test]
    async fn expired_token_refreshes_and_persists_rotation() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let new_access = jwt_with_account("acct_refreshed");
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains(format!(
                "client_id={}",
                super::super::CLIENT_ID
            )))
            .and(body_string_contains("refresh_token=old-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": new_access,
                "refresh_token": "rotated-refresh",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("codex.json"));
        store
            .save(&CredentialFile {
                tokens: Tokens {
                    access_token: "stale-access".into(),
                    refresh_token: "old-refresh".into(),
                    account_id: "acct_old".into(),
                },
                expires_at: now_unix().saturating_sub(10), // already expired
            })
            .unwrap();

        let mut resolver = CodexResolver::new(store);
        resolver.token_url = format!("{}/token", server.uri());

        let cred = resolver.resolve().await.unwrap();
        // The resolved bearer is the rotated access token…
        assert_eq!(cred.value.to_string(), "<redacted>");
        assert_eq!(
            cred.extra_headers
                .get("chatgpt-account-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "acct_refreshed",
            "account id is re-derived from the refreshed token",
        );
        // …and the rotation is persisted, including the new refresh token.
        let persisted = resolver.store.load().unwrap().unwrap();
        assert_eq!(persisted.tokens.refresh_token, "rotated-refresh");
        assert_eq!(persisted.tokens.access_token, new_access);
        assert!(persisted.expires_at > now_unix());
    }
}
