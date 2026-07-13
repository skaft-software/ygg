# Codex OAuth (Sign in with ChatGPT) — Design Spec

> **Status:** approved for implementation (2026-07-13). Self-reviewed against Pi's
> reference implementation in `~/github/earendil-works/pi` (`packages/ai/src/utils/oauth/openai-codex.ts`,
> `packages/ai/src/api/openai-codex-responses.ts`, `packages/ai/src/providers/openai-codex.*`).

## Goal

Let a user drive OpenAI subscription models (GPT-5.6 family) through `ygg` using
their ChatGPT (Codex) subscription — the same "Sign in with ChatGPT" OAuth the
Codex CLI uses — with a first-class `ygg` login flow (browser + headless), token
storage, and automatic refresh.

## Grounded constants (extracted from the live Codex CLI + JWT, cross-checked with Pi)

| Item | Value |
|---|---|
| client_id | `app_EMoamEEZ73f0CkXaXp7hrann` |
| authorize | `https://auth.openai.com/oauth/authorize` |
| token | `https://auth.openai.com/oauth/token` |
| redirect | `http://localhost:1455/auth/callback` (server binds `127.0.0.1:1455`) |
| scope | `openid profile email offline_access` |
| authorize extra params | `code_challenge_method=S256`, `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `originator=ygg`, `state` |
| backend base | `https://chatgpt.com/backend-api/codex/` (endpoint base_url; **not** api.openai.com) |
| request | `POST …/codex/responses` (SSE) |
| account id | JWT access-token claim `["https://api.openai.com/auth"].chatgpt_account_id` |
| models | `gpt-5.6-sol` (default), `gpt-5.5`, `gpt-5.6-luna`, `gpt-5.6-terra`, `gpt-5.5-pro` — 272k ctx / 128k out, reasoning=Effort |

## Key architectural decision: reuse `Protocol::OpenAiResponses`, no frozen `ygg-ai` changes

The subscription-Codex request is wire-compatible with ygg's existing
`OpenAiResponses` codec:

- `build_request` does `base_url.join("responses")`; with base `…/codex/` this
  yields exactly `…/codex/responses`.
- It already emits `store: false`, `include: ["reasoning.encrypted_content"]`,
  the `input`/tools/`reasoning:{effort}` shapes, and `developer`-role system.
- The codec returns an empty header map; `client.rs` composes
  `endpoint.default_headers` → codec headers → auth headers, so all Codex
  headers inject without touching the codec.

Header placement:
- **Static** (endpoint `default_headers`): `OpenAI-Beta: responses=experimental`,
  `originator: ygg`, `User-Agent: ygg/<ver> (<os>)`.
- **Dynamic** (resolver `extra_headers`, per credential/session):
  `chatgpt-account-id: <account_id>`, `session-id: <uuid>` (note the hyphen).

### Live-test outcome (2026-07-13): two proven blockers, both frozen-codec fixes

A real `gpt-5.6-sol` turn surfaced two backend rejections — each a *proven
blocker*, and each also a genuine correctness fix for standard OpenAI Responses,
so both were applied to the frozen `openai_responses` codec (all 116 frozen
tests remain green):

1. `{"detail":"Stream must be set to true"}` — the codec set streaming only as a
   transport flag, never `"stream": true` in the body. Now emitted
   unconditionally (the codec is always-streamed by design).
2. `{"detail":"Unsupported parameter: max_output_tokens"}` — the codec
   synthesized a default `max_output_tokens` from the local capacity limit. Now
   only an *explicit* caller cap is forwarded; the default is never synthesized.

The predicted **`instructions`** risk did **not** materialize: the ChatGPT Codex
backend accepts the system prompt as a `developer` message in `input`. After the
two fixes, the end-to-end turn returned `OK`. No `Protocol::OpenAiCodexResponses`
variant was needed.

## Components (all in `ygg-coding-agent`; `ygg-ai` stays frozen)

`src/auth/codex/`:
- **`oauth.rs`** — constants; PKCE (`verifier` = base64url(32 rand bytes),
  `challenge` = base64url(SHA256(verifier))); `authorize_url`; `exchange_code`
  and `refresh` (reqwest form POST to the token endpoint); JWT `account_id` +
  `expires_at` decode. Refresh **rotates** the refresh token — the new one is
  persisted.
- **`store.rs`** — JSON credential file at `~/.ygg/credentials/codex.json`, mode
  `0600`. `{ tokens: { access_token, refresh_token, account_id }, expires_at }`.
  `load`/`save`/`delete`, plus an async `modify` guarded by a mutex for
  double-checked-lock refresh.
- **`resolver.rs`** — `CodexResolver` implements the public
  `ygg_ai::CredentialResolver`. `resolve()`: load token; if `now + skew >=
  expires_at`, refresh under lock (re-check inside); return `Bearer(access)` +
  `{chatgpt-account-id, session-id}`. Fast when unexpired.
- **`login.rs`** — browser flow: local `127.0.0.1:1455` `TcpListener`, open the
  system browser, await the `?code=&state=` callback (state-verified), exchange.
  Headless / fallback: print the URL and read a pasted `code` or redirect URL
  from stdin. Writes the store.

`src/cli.rs` — `--login <provider>` / `--logout <provider>` flags (avoids
clashing with the positional prompt), plus `--headless`.

`src/main.rs` — dispatch `--login`/`--logout` before building the run config.

`src/app/bootstrap.rs` — `register_openai_codex`: build the endpoint (backend
base + static headers + `Auth::dynamic(CodexResolver)`) and register the models,
**only if** a credential file exists (so unlogged-in users simply don't see the
models, and `ygg --login codex` is the guided path). Mirrors the DeepSeek
programmatic-registration pattern.

## Testing

- Unit: PKCE shape, `authorize_url` params, callback-request parsing, JWT
  account-id/expiry decode, store round-trip + `0600`, resolver expiry→refresh
  (mock token endpoint via the existing `wiremock` dev-dep).
- Manual acceptance gate: `ygg --login codex`, then a real `gpt-5.6-sol` turn —
  this is the live confirmation of the `instructions` risk above.

## Out of scope (this pass)

Device-code headless flow (Pi has one; browser+paste covers headless here),
Anthropic/Copilot OAuth, keychain storage, `Protocol::OpenAiCodexResponses`
variant (only if the live test forces it).
