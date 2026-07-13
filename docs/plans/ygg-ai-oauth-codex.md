# ygg-ai — OAuth / Subscription Login Implementation Plan

> **Status:** deferred (not in v1). Infrastructure ready in `crates/ygg-ai`.
> **Depends on:** `ygg-ai` (complete), `ygg-coding-agent` (v1 complete).

## Motivation

Subscription-based providers (OpenAI Codex, Anthropic Pro/Max, GitHub Copilot)
use OAuth instead of API keys. The `ygg-ai` crate already has the integration
point (`Auth::Dynamic` + `CredentialResolver` trait, design §9) and the PI
reference documents the full OAuth pattern. What's missing is a concrete
`CredentialResolver` implementation and catalog entries.

## What's already built

| Component | Location | Status |
|-----------|----------|--------|
| `Auth::Dynamic(Arc<dyn CredentialResolver>)` | `crates/ygg-ai/src/auth.rs:77` | ✅ |
| `CredentialResolver` trait (`async fn resolve()`) | `crates/ygg-ai/src/auth.rs:141-145` | ✅ |
| `CredentialResolverRegistry` | `crates/ygg-ai/src/auth.rs:166-167` | ✅ |
| `AuthConfig::Dynamic { resolver_id }` in catalog config | `crates/ygg-ai/src/catalog.rs:63-66` | ✅ |
| `from_config_with_resolvers()` | `crates/ygg-ai/src/catalog.rs:131-134` | ✅ |
| DeepSeek programmatic registration pattern | `crates/ygg-coding-agent/src/app/bootstrap.rs:73-117` | ✅ |
| PI reference: OAuth flow, credential store, refresh | `docs/research/refImpls/ai/pi-ai.md` §"Auth Subsystem" | ✅ |

## What needs building

### 1. Concrete `CredentialResolver` — `CodexResolver` (or generic `OAuthResolver`)

Implement `CredentialResolver` for at least OpenAI Codex. The PI reference
pattern:

```
OAuthAuth {
  login(callbacks) -> OAuthCredential     // browser OAuth flow
  refresh(credential) -> OAuthCredential  // token rotation
  toAuth(credential) -> { apiKey, headers, baseUrl }
}
```

In Rust terms:

```rust
// crates/ygg-ai/src/auth/codex.rs (or in ygg-coding-agent)
pub struct CodexResolver {
    // credential store (file-backed or in-memory)
    store: Arc<Mutex<dyn CredentialStore>>,
    // OAuth client config
    client_id: String,
    token_url: url::Url,
    // …
}

#[async_trait::async_trait]
impl CredentialResolver for CodexResolver {
    async fn resolve(&self) -> Result<ResolvedCredential, AuthError> {
        // 1. Read stored credential
        // 2. If expired, refresh under double-checked lock
        // 3. Map to ResolvedCredential { scheme: Bearer, value, extra_headers }
    }
}
```

**Key design decisions:**
- The `CredentialResolver::resolve()` is called per request — it must be
  **fast** (read-cached token, only refresh when near expiry).
- Token persistence: file-backed credential store in `~/.ygg/credentials/` or
  platform keychain. The `Secret` wrapper prevents log leaks.
- The first-time login flow (open browser, user authorizes, exchange code for
  token) is an **async initialization step** that runs before the resolver is
  registered. It is NOT part of `resolve()`.
- Double-checked locking for refresh: optimistic expiry check outside lock, then
  re-check inside lock to avoid thundering-herd on refresh.

### 2. Codex catalog entries

Add to `crates/ygg-ai/models/catalog.json` (or register programmatically like
DeepSeek):

```json
{
  "id": "openai-codex",
  "base_url": "https://api.openai.com/v1/",
  "auth": { "kind": "dynamic", "resolver_id": "openai-codex" },
  "default_headers": {},
  "timeout_secs": 60
}
```

Plus model entries: `gpt-5.3-codex`, `codex-mini-latest`, etc. These use
`Protocol::OpenAiResponses` (per the PI reference, Codex uses the Responses API
with a dedicated `openai-codex-responses.ts` module).

**Protocol question:** The PI reference has a separate
`openai-codex-responses.ts` module. Does Codex require a new
`Protocol::OpenAiCodexResponses` variant, or is it wire-compatible with
`OpenAiResponses`? The API docs under `docs/research/apidocs/openai-responses/`
mention `gpt-5.3-codex` models — if the wire protocol is identical to standard
Responses, no new `Protocol` variant is needed. If it differs (WebSocket, custom
headers, different SSE event set), a new variant + codec module is required.

### 3. Bootstrap registration in `ygg-coding-agent`

Following the DeepSeek pattern in `bootstrap.rs:73-117`:

```rust
fn register_openai_codex(
    catalog: &mut ModelCatalog,
    resolvers: &mut CredentialResolverRegistry,
) -> anyhow::Result<()> {
    // 1. Instantiate the resolver (loads stored token or triggers login)
    let resolver = CodexResolver::new(credential_store, oauth_config)?;
    resolvers.insert("openai-codex".into(), Arc::new(resolver));

    // 2. Register endpoint + models
    let endpoint = Endpoint {
        id: EndpointId("openai-codex".into()),
        base_url: url::Url::parse("https://api.openai.com/v1/")?,
        auth: Auth::dynamic(resolvers.get("openai-codex").unwrap().clone()),
        default_headers: http::HeaderMap::new(),
        timeout: Duration::from_secs(60),
    };
    catalog.register_endpoint(endpoint)?;
    // register models...
}
```

### 4. First-time login UX (deferred design)

The OAuth login flow (browser redirect → callback → token exchange) needs a
user-facing trigger. Options:

- **CLI command:** `ygg login openai-codex` — opens browser, starts local
  redirect server, exchanges code, stores credential.
- **TUI command:** `/login codex` — same flow triggered from the interactive
  shell.
- **Auto-trigger:** On first `resolve()` with no stored credential, print a URL
  and pause for the callback.

This is the part the design doc calls "browser OAuth UI" and explicitly defers.
The resolver implementation can start with a CLI-only bootstrap path before a
TUI integration is designed.

## Implementation order

| Step | Where | Effort |
|------|-------|--------|
| 1. Verify Codex wire protocol vs standard Responses | Research `apidocs/` + PI ref | Small |
| 2. Add `Protocol::OpenAiCodexResponses` if needed | `ygg-ai/src/types.rs` | Conditional |
| 3. Implement `CodexResolver` (token store + refresh + `resolve()`) | `ygg-ai/src/auth/codex.rs` | Medium |
| 4. Add Codex endpoint + models to `catalog.json` (or programmatic) | `ygg-ai/models/` or `bootstrap.rs` | Small |
| 5. Register resolver + endpoint in bootstrap | `ygg-coding-agent/src/app/bootstrap.rs` | Small |
| 6. CLI login command (`ygg login`) | `ygg-coding-agent/src/cli.rs` | Medium |
| 7. Integration test with real Codex subscription | Manual | Small |

## Open questions

1. **Does Codex need a separate `Protocol` variant?** The PI reference has
   `openai-codex-responses.ts` as a separate module. Determine whether this is
   because the wire differs or because PI organizes by provider, not protocol.
2. **Credential store format.** JSON file in `~/.ygg/credentials/` is simplest.
   Platform keychain (keyring crate) is more secure but adds a native dep.
3. **Token refresh concurrency.** If multiple `resolve()` calls race on expiry,
   the double-checked lock pattern prevents duplicate refresh calls. The PI
   reference uses `credentials.modify()` for this.
4. **Anthropic Pro/Max and GitHub Copilot.** Same pattern, different OAuth
   endpoints. A generic `OAuthResolver` configurable by provider may be better
   than one struct per provider.

## References

- PI OAuth subsystem: `docs/research/refImpls/ai/pi-ai.md` §"Auth Subsystem"
- PI `openai-codex-responses.ts`: `docs/research/refImpls/ai/pi-ai.md` L442
- `ygg-ai` auth design: `docs/design/ygg-ai.md` §9
- `ygg-ai` catalog config: `docs/design/ygg-ai.md` §10
- DeepSeek registration pattern: `crates/ygg-coding-agent/src/app/bootstrap.rs:73-117`
