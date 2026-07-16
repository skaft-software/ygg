# Codex OAuth (Sign in with ChatGPT) — Design Spec

> **Status:** implemented and verified (2026-07-13), using Pi's current
> `packages/ai/src/utils/oauth/openai-codex.ts` device-code flow as the reference.

## Goal

Let users run supported OpenAI subscription models through `ygg` with their
ChatGPT subscription. Authentication must work from both the CLI and the TUI,
refresh safely, hide models when no usable credential exists, and never
advertise a model that Ygg's transport cannot run.

## Grounded constants

| Item | Value |
|---|---|
| client id | `app_EMoamEEZ73f0CkXaXp7hrann` |
| device start | `https://auth.openai.com/api/accounts/deviceauth/usercode` |
| device poll | `https://auth.openai.com/api/accounts/deviceauth/token` |
| verification page | `https://auth.openai.com/codex/device` |
| token exchange/refresh | `https://auth.openai.com/oauth/token` |
| device exchange redirect | `https://auth.openai.com/deviceauth/callback` |
| timeout | 15 minutes |
| backend base | `https://chatgpt.com/backend-api/codex/` |
| request | `POST …/codex/responses` over SSE |
| account id | JWT claim `['https://api.openai.com/auth'].chatgpt_account_id` |

The start request contains the public client id. Ygg displays the returned user
code, optionally opens the hosted verification page, and polls at the returned
interval. HTTP 403/404 and `deviceauth_authorization_pending` mean pending;
`slow_down` increases the interval by five seconds. The completed authorization
code and server-issued verifier are exchanged using the hosted device redirect.

This replaces Ygg's old localhost callback flow. Tokens marked
`['https://api.openai.com/auth'].localhost = true` are rejected because OpenAI
routes those credentials through a reduced model pool, which caused misleading
model 404 responses.

## Request protocol and supported models

The subscription backend works with Ygg's existing
`Protocol::OpenAiResponses` SSE codec. Codex-specific headers are composed around
that codec:

- Static endpoint headers: `OpenAI-Beta: responses=experimental`,
  `originator: ygg`, and `User-Agent: ygg/<version> (<os>)`.
- Dynamic resolver headers: `chatgpt-account-id: <account_id>` and one
  process-stable `session-id`.
- Request cache affinity: `prompt_cache_key` and `x-client-request-id` use the
  stable Ygg session key. Standard Responses' underscore `session_id` header and
  `prompt_cache_retention` are disabled for Codex because Pi's Codex transport
  does not send them.

The codec includes `stream: true`, does not synthesize
`max_output_tokens`, emits `store: false`, and requests encrypted reasoning
continuation data plus `reasoning.summary: "auto"` for visible thinking. Replayed
reasoning items always include `summary` (including `[]`), which newer Codex
models require for post-tool continuation. Completed assistant history is replayed
as `output_text` (never `input_text`), allowing subsequent user turns to start.
A developer-role input message is accepted for system instructions.

The advertised SSE-compatible model set is:

- `gpt-5.6-sol` (recommended default)
- `gpt-5.6-terra`
- `gpt-5.5`
- `gpt-5.4`
- `gpt-5.4-mini`

`gpt-5.6-luna` is deliberately hidden: Ygg's current SSE request semantics
return `Model not found`, while Codex's WebSocket transport and richer official
HTTP fallback request both succeed. `gpt-5.5-pro` is absent from the subscription
catalog and is explicitly rejected for ChatGPT-account Codex requests.

## Components

All concrete OAuth code remains in `ygg-coding-agent` and uses the public
`ygg_ai::CredentialResolver` interface.

### `src/auth/codex/oauth.rs`

- Starts and polls hosted device authorization.
- Exchanges the completed code and refreshes access tokens.
- Requires and persists OpenAI's rotated refresh token.
- Extracts and validates the ChatGPT account claim without treating it as an
  authorization boundary; the backend still validates the bearer token.
- Rejects malformed and localhost-only access tokens.

### `src/auth/codex/store.rs`

Stores
`{ tokens: { access_token, refresh_token, account_id }, expires_at }` at
`~/.ygg/credentials/codex.json`. Writes use owner-only mode `0600` on Unix.
Ygg never reads or persists Pi's refresh token.

### `src/auth/codex/resolver.rs`

Loads the credential for each request. Near expiry, refresh is serialized with a
double-checked async lock, the rotated access token is revalidated, and both
rotated tokens are persisted. Resolution returns a bearer credential plus the
Codex account and session headers.

### CLI and TUI

- `ygg --login codex` runs device login and opens the verification page.
- `ygg --login codex --headless` prints the URL/code without opening a browser.
- `ygg --logout codex` removes Ygg's credential.
- `/login [codex]` and `/logout [codex]` are first-class slash commands.
- During TUI login, Ygg temporarily restores the normal terminal so the code is
  visible, then re-enters the alternate screen and reloads the catalog.
- Commands submitted during a model run are queued for the next idle boundary.
- Logging out while a Codex model is active requires selecting a non-Codex
  replacement first; cancellation leaves the credential and model untouched.

### Catalog visibility

Codex models are registered only when Ygg's own credential file can be parsed,
has a refresh token, contains a ChatGPT account id, and is not marked localhost.
An expired access token may still register the models because the resolver can
refresh it before the next request. Missing, malformed, or legacy credentials do
not expose Codex models.

## Verification

Automated tests cover device start/pending/completion responses, hosted redirect
exchange, JWT validation, localhost rejection, secure store round trips, token
refresh and rotation, catalog visibility, slash-command parsing, and queued TUI
actions.

A live acceptance replay using Pi's access token in memory only confirmed that
Ygg's exact SSE request shape streams successfully for all five advertised
models. It also reproduced Luna's SSE-only 404 and `gpt-5.5-pro`'s explicit
subscription rejection. Pi credentials were neither copied nor persisted.

## Out of scope

- The richer Codex request/transport semantics needed to expose
  `gpt-5.6-luna` reliably.
- Anthropic or GitHub Copilot OAuth.
- OS keychain-backed token storage.
