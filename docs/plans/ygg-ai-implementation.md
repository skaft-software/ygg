# `ygg-ai` — Implementation Plan

> Dependency-ordered, independently verifiable tasks for building `crates/ygg-ai`.
> Companion to `docs/design/ygg-ai.md`. Execute tasks in order; each leaves the
> crate compiling and its own tests passing unless explicitly noted.
>
> **No architectural decisions are delegated to the implementer.** Every type,
> signature, mapping, and invariant is fixed by the design doc; cite it, don't
> reinvent it. Where this plan says "per design §N", that section is normative.

## Non-negotiable execution contract

1. Read `docs/design/ygg-ai.md` and this plan completely before editing. The
   design's precedence rules are binding; this plan cannot override them.
2. Implement the **entire completion path**, Tasks 1.1 through 16.2. There is no
   slice, MVP, or sanctioned partial-delivery target.
3. Maintain `docs/reports/ygg-ai-implementation-report.md` as an evidence ledger:
   task status, files changed, tests added, command and result, deviations, and
   unresolved blockers. Do not mark a task complete without its acceptance
   command passing.
4. Never guess provider JSON. Verify every DTO field against
   `docs/research/apidocs/`; cite the repository file and heading in fixture
   comments. If no documented mapping exists, follow Strict/Lossy policy.
5. No placeholders: reject `todo!`, `unimplemented!`, stub return values, ignored
   tests, weakened assertions, fake catalog capabilities/prices, or omitted task
   branches. Do not delete a test to get green.
6. After all tasks, run the full gate, inspect every warning/failure, perform an
   adversarial audit against the design, fix all findings, rerun the gate, then
   finalize the report. A green build without the specified behavior is failure.
7. Stay offline in tests. Never require credentials, contact a live provider, or
   print a secret. Do not modify research API documentation.

## Conventions used by every task

- **Completion command** is the exact command whose success gates the task.
  Baseline for all: `cargo build -p ygg-ai` then the task's `cargo test`/`clippy`.
- Lints: `cargo clippy -p ygg-ai --all-targets -- -D warnings` must pass from
  Task 1 onward (deny warnings).
- Formatting: `cargo fmt --check` must pass.
- Async tests use `#[tokio::test]` (dev-dep `tokio` with `rt`,`macros`).
- **Audio is first-class but protocol-scoped** (design §0.1, §6.4, §12): audio is
  **Chat-only** (input inline WAV/MP3; output via a **non-streaming** request →
  one completed `MediaCompleted` event). **Responses and Anthropic reject audio** with a
  structured `Unsupported` error in Strict mode; audio is **never silently
  removed**. There is **no `MediaDelta`** in v0.1. Do not invent
  `choices[].delta.audio` or `response.audio.*` fixtures.
- Secrets: never `Debug`/`Display`/`Serialize` a `Secret`; never place one in an
  error (design §9, §16).

---

## Completion path (READ FIRST)

This plan is comprehensive and intentionally has no sanctioned partial-delivery
path. Execute every task through 16.2 in the dependency order at the end of this
file. A compiling subset is useful only as transient progress and must never be
reported as completion.

---

## Phase 1 — Crate skeleton and dependencies

### Task 1.1 — Workspace + crate skeleton
- **Objective:** Create a compiling empty `ygg-ai` crate inside a new workspace.
- **Files:**
  - `Cargo.toml` (workspace root) — `[workspace] members = ["crates/ygg-ai"]`,
    shared `[workspace.package]` (edition 2021, rust-version), `[workspace.lints]`.
  - `crates/ygg-ai/Cargo.toml` — deps per design §18 (reqwest w/ `rustls-tls`,
    `stream`, `http2`, `default-features=false`; http; bytes; futures-core;
    futures-util; async-stream; serde+derive; serde_json; async-trait; thiserror;
    url+serde; base64; **mime**). Enable `bytes`'s `serde` feature. `mime::Mime`
    has no serde implementation, so use the private string adapter required by
    design §6.4; do not attempt to enable a nonexistent mime serde feature.
    Dev-deps: tokio(`rt`,`macros`), wiremock, pretty_assertions.
  - `crates/ygg-ai/src/lib.rs` — module declarations (empty modules) + crate docs
    + `pub enum CompatibilityMode { Strict, Lossy }` with `Default = Strict`.
  - Empty `src/{client,types,stream,auth,error,pricing,catalog}.rs` and
    `src/protocol/{mod,sse,openai_chat,openai_responses,anthropic}.rs`.
- **Types/functions:** only `CompatibilityMode` (+ `Default`).
- **Dependencies:** none.
- **Tests:** `tests/smoke.rs` — asserts `CompatibilityMode::default() == Strict`.
- **Completion command:** `cargo test -p ygg-ai && cargo clippy -p ygg-ai --all-targets -- -D warnings`
- **Acceptance:** workspace builds; clippy clean; smoke test passes; every module
  file exists and is declared.
- **Likely failure modes:** reqwest default TLS pulled in (set
  `default-features=false`); missing `resolver = "2"`; module declared but file
  missing.

---

## Phase 2 — IDs, endpoint, model, canonical content, request, response types

### Task 2.1 — IDs, protocol, capabilities, limits
- **Objective:** Newtype IDs and model-descriptor value types (design §6.1).
- **Files:** `src/types.rs`.
- **Types:** `EndpointId`, `ModelId`, `ToolCallId` (newtype over `String`, derive
  `Clone,PartialEq,Eq,Hash,Debug,Serialize,Deserialize`); `Protocol`;
  `Capabilities`; `ModalitySet` (`bits:u8`, `none`/`with`/`contains`); `Modality`
  (`#[non_exhaustive]` `Image,Audio`); `ReasoningCapability`; `ReasoningControl`;
  `ReasoningEffortBudgets`; `ModelLimits`; `ModelSpec`. Catalog validation later
  requires effort budgets iff reasoning control is `TokenBudget`. `Endpoint` is
  deliberately added with the real `Auth` in Task 5.1—do not create a stub auth
  type.
- **Dependencies:** 1.1.
- **Tests:** `ModalitySet` set algebra (none/with/contains round-trips for Image,
  Audio, both); serde round-trip for `ModelSpec` and both reasoning-control
  configurations.
- **Completion command:** `cargo test -p ygg-ai types::`
- **Acceptance:** all descriptor types serde round-trip; `ModalitySet` correct.
- **Failure modes:** forgetting `#[non_exhaustive]` on `Modality`; `ModalitySet`
  bit collisions.

### Task 2.2 — Media (modality-discriminated), messages, tool, reasoning content
- **Objective:** Canonical conversation content (design §6.2–6.4). Use the
  **modality-discriminated** media system; do **not** build a flat optional-bag
  `Media { modality, source, mime, transcript, provider_ref }`.
- **Files:** `src/types.rs`.
- **Types:** `Media` (enum `Image(ImageMedia)`/`Audio(AudioMedia)`);
  `ImageMedia { source: ImageSource, media_type: Option<mime::Mime>, detail }`;
  `ImageSource` (`Url`/`Inline(Bytes)`/`ProviderRef`); `ImageDetail`;
  `AudioMedia { payload: AudioPayload, format: AudioFormat, transcript }`;
  **`AudioPayload` (three variants): `Inline(Bytes)`, `ProviderRef(ProviderMediaRef)`,
  `InlineWithProviderRef { data: Bytes, reference: ProviderMediaRef }` — **no
  `Url`**. Bytes + a provider ref are NOT mutually exclusive (a completed Chat
  audio object carries `data`+`id`+`expires_at`+`transcript` together).**
  `ProviderMediaRef { protocol, id, expires_at }`; private string serde adapter
  for `Option<mime::Mime>`; `AudioFormat` (closed;
  `Wav,Aac,Mp3,Flac,Opus,Pcm16`); constructors `Media::image_url`,
  `Media::image_bytes`, `Media::audio_bytes` (input), `Media::audio_ref` (reuse).
  Plus `Message`,`UserMessage`,`AssistantMessage`; `UserPart`,`AssistantPart`;
  `ToolResult`,`ToolResultPart`; `ToolCall` (+ `arguments_value()`);
  `ReasoningPart`,`ReasoningState`,`ReasoningStateKind`.
- **Dependencies:** 2.1.
- **Tests:** serde round-trip for each `Message` variant incl. `Media::Image`
  (inline bytes + URL) and `Media::Audio` for **all three `AudioPayload` variants**
  (`Inline`; `ProviderRef`; `InlineWithProviderRef` with transcript — the completed
  Chat shape); each `ReasoningStateKind`; `ToolCall::arguments_value` parses object
  and errors on non-object. **Invariant tests by construction:** a compile-level
  note that `transcript` is unreachable on image and `AudioPayload` has no `Url`
  arm — the illegal states don't exist to test at runtime. **`voice` is NOT on
  `AudioMedia`** (it lives on `AudioOutputOptions`, Task 2.3).
- **Completion command:** `cargo test -p ygg-ai types::`
- **Acceptance:** round-trips stable (incl. `InlineWithProviderRef` preserving
  data+id+expires_at+transcript); invalid field combinations unrepresentable (no
  runtime mime/modality check); inline media stored as raw `bytes::Bytes`.
- **Failure modes:** re-introducing the flat `Media` struct; modeling bytes and
  provider-ref as mutually exclusive (must allow `InlineWithProviderRef`); adding a
  `Url` arm to `AudioPayload`; putting `voice` on `AudioMedia`; `arguments_value`
  accepting arrays/scalars; storing inline media pre-base64-encoded.

### Task 2.3 — Request, usage, and stop reason
- **Objective:** Canonical request envelope and accounting values (design §6.5–6.6).
- **Files:** `src/types.rs`.
- **Types:** `Request`; `ToolDef`; `ToolChoice`; `ReasoningConfig`;
  `ReasoningEffort`; `OutputModalities` (`Text`/`TextAndAudio(AudioOutputOptions)`);
  **`AudioOutputOptions { format: AudioFormat, voice: AudioVoice }`**;
  **`AudioVoice` (`Named(String)` open / `ProviderRef(String)`)** — voice is a
  generation option, never stored on `AudioMedia`; `OutputFormat`
  (`Text`/`JsonObject`/`JsonSchema(JsonSchemaFormat)`); `JsonSchemaFormat`; `Usage`; `StopReason`
  (`#[non_exhaustive]`). (`AudioFormat` defined in Task 2.2.) Add `Response` only
  in Task 6.1 after the real `Cost` and `Diagnostic` types exist; do not stub
  either dependency.
- **Dependencies:** 2.2.
- **Tests:** `Request` serde round-trip incl. `OutputModalities::TextAndAudio`
  and every `OutputFormat`; `Usage::default()` all-zero; unknown stop reasons map
  to `StopReason::Other`. `AudioFormat` is closed and deliberately has no
  `Other` variant.
- **Completion command:** `cargo test -p ygg-ai types::`
- **Acceptance:** full request types compile and round-trip.
- **Failure modes:** `compatibility` or `output_format` missing `#[serde(default)]`.

---

## Phase 3 — Validation and capability checks

### Task 3.1 — Conversation & capability validation
- **Objective:** Pure validation used by every codec before send (design §7).
- **Files:** `src/types.rs` (or `src/validate.rs`, declared in lib).
- **Types/functions:** `fn validate_request(req, caps, protocol, mode)
  -> Result<Vec<Diagnostic>, AiError>` implementing the §7 tables:
  tool-result↔tool-call pairing; args-parse-as-object; image-input gate;
  output-format capability + schema/name validation; output-token and finite
  temperature bounds; protocol/expiry validation for every provider media ref;
  **audio-input gate (always fails for Responses & Anthropic → structured
  `Unsupported(Audio)`)**; Chat audio-format gate (only `Wav`/`Mp3`); audio-output
  gate (Chat-only, capability-gated); `ImageSource::Url`-needs-inline gate; each
  check returning `Unsupported` (Strict) or a `Diagnostic` (Lossy). No runtime
  mime-vs-modality check exists — the `Media` enum guarantees agreement (design
  §7.4). Returns diagnostics for Lossy drops; drops are applied by codecs to
  derived DTOs, **not** to `req` (function takes `&Request`). **Audio is never
  silently removed** — Strict errors; Lossy always emits a `Diagnostic`.
- **Dependencies:** 2.3 and Task 4.1 (`AiError`/`Diagnostic`). Execute Task 4.1
  before Task 3.1; do not create temporary error stubs.
- **Tests:** orphan tool result → error; missing tool result → Strict error /
  Lossy diagnostic; image on text-only model → Strict `Unsupported::Image` /
  Lossy drop diagnostic; **audio-in on Responses caps → Strict `Unsupported(Audio)`
  / Lossy drop+diagnostic**; **audio-in on Anthropic caps → same**; Chat audio
  with `Flac`/`Opus`/etc → reject/drop; audio-out on Responses/Anthropic → Strict
  `Unsupported(AudioOutput)` / Lossy downgrade+diagnostic; reasoning-state
  (proto/model mismatch) → reject/drop; tools without cap → reject/drop;
  malformed schema/name; structured output on an incapable model; expired or
  wrong-protocol media refs; zero/over-limit output tokens; NaN/out-of-range
  temperature. Assert one specific diagnostic per Lossy conversion.
- **Completion command:** `cargo test -p ygg-ai validate::`
- **Acceptance:** every §7 row has a passing test in both modes.
- **Failure modes:** mutating `req` (must be `&Request`); inventing placeholder
  text in Lossy (forbidden); pairing check missing the "same protocol requires
  pairing" nuance.

---

## Phase 4 — Error and secret-redaction infrastructure

### Task 4.1 — Error taxonomy
- **Objective:** `AiError` and sub-errors (design §16).
- **Files:** `src/error.rs`.
- **Types:** `AiError` (+ `thiserror` `Error`/`Display`); `HttpError`,
  `ProviderError`, `TransportError`, `TransportPhase`, `StreamProtocolError`
  (`#[non_exhaustive]`), `UnsupportedError`, `ConfigError`, `AuthError`,
  `ValidationError`, `DecodeError`; `Diagnostic`
  (Serialize). Helper ctors used elsewhere (`ConfigError::parse`,
  `HttpError::is_safe_to_retry`).
- **Dependencies:** 1.1.
- **Tests:** `Display` for each variant is non-empty and secret-free; transport
  errors preserve phase/timeout without inventing an HTTP status; a constructed
  `HttpError` with a bearer-looking string in `body_snippet` — assert
  the type has no field that stores request headers; `is_safe_to_retry` true only
  pre-first-byte statuses.
- **Completion command:** `cargo test -p ygg-ai error::`
- **Acceptance:** taxonomy matches §16; no variant holds a `Secret`.
- **Failure modes:** capturing full response headers into `HttpError`
  (Authorization leak); `Diagnostic` not `Serialize`.

### Task 4.2 — `Secret` redaction
- **Objective:** Hardened secret wrapper (design §9, §20).
- **Files:** `src/auth.rs`.
- **Types:** `Secret(Box<str>)` — manual `Debug`(`Secret(<redacted>)`) +
  `Display`(`<redacted>`); `From<String>`/`From<&str>`; `from_env`;
  `pub(crate) fn expose(&self) -> &str`. **No** `Serialize`/`Deserialize`.
- **Dependencies:** 4.1.
- **Tests:** `format!("{:?}", secret)` and `{}` contain neither the value nor
  substrings of it; a compile-fail doctest (or `trybuild`) asserting
  `serde_json::to_string(&secret)` does not compile (or simply document + unit
  test that `Secret` has no serde impl by not importing it).
- **Completion command:** `cargo test -p ygg-ai auth::secret`
- **Acceptance:** value never appears in debug/display; env constructor errors
  cleanly on missing var.
- **Failure modes:** deriving `Debug`; `expose` made `pub`.

---

## Phase 5 — Authentication resolvers

### Task 5.1 — `Auth`, `CredentialResolver`, header application
- **Objective:** Full auth model + safe header composition (design §9).
- **Files:** `src/auth.rs`, `src/types.rs` (`Endpoint`).
- **Types/functions:** `Auth` enum
  (`None`,`Bearer`,`Header`,`BearerEnv`,`HeaderEnv`,`Dynamic`) + constructors
  (`bearer`,`bearer_env`,`header`,`header_env`,`dynamic`,`none`); environment
  variants read their value per request and never retain it beyond header build;
  `#[async_trait] CredentialResolver`; `ResolvedCredential`; `CredentialScheme`;
  `pub(crate) async fn resolve_headers(&Auth) -> Result<HeaderMap, AuthError>`
  (applies scheme to a `HeaderMap` using `Secret::expose`);
  `pub(crate) fn auth_header_name(&Auth) -> Option<HeaderName>` (for collision
  check); real `Endpoint` type from design §6.1 with no secret-transparent
  serialization/debug implementation.
- **Dependencies:** 4.2, 2.1.
- **Tests:** each variant produces the right header (`Bearer` → `Authorization:
  Bearer …`; `Header` → configured name); a fake `CredentialResolver` returns a
  bearer + `extra_headers` and both appear; resolver error → `AuthError`; the
  primary auth header wins over a colliding extra header; secret header values
  are `set_sensitive(true)` and produced `HeaderMap` debug contains no secret.
- **Completion command:** `cargo test -p ygg-ai auth::`
- **Acceptance:** headers correct; environment values can rotate between requests;
  missing env is `AuthError`; dynamic resolver is the async seam; no secret
  leaks in any assertion output.
- **Failure modes:** invalid header value from a stray newline (validate);
  `extra_headers` overriding the auth header (auth must win/last).

---

## Phase 6 — Pricing and usage

### Task 6.1 — Rates, pricing, integer cost
- **Objective:** Integer-microdollar cost accounting (design §6.6, §11-money,
  §15, §20).
- **Files:** `src/pricing.rs`.
- **Types/functions:** `TokenRate(u64)` (microdollars per 1e6 tokens); `Pricing`
  (`input`,`output`,`cache_read`,`cache_write_5m`,`cache_write_1h:Option`,
  `reasoning:Option`, `tiers:Vec<PricingTier>`); `PricingTier`
  (`min_input_tokens:u64`, + rate overrides); `Cost`
  (`input`,`output`,`cache_read`,`cache_write`,`total` — all `u64` µ$); add the
  final non-serializable `Response` type in `types.rs` using real `Cost` and
  `Diagnostic`;
  `fn cost_of(&Pricing, &Usage) -> Result<Cost, PricingError>` using `u128`
  intermediates, `floor`-rounded to `u64`, erroring on overflow or inconsistent
  subset buckets; tier selection by **this request's** input bucket
  (`input_tokens + cache_read_tokens + cache_write_tokens`), never cumulative.
- **Dependencies:** 2.3, 4.1 (`Diagnostic`).
- **Tests:** `Response` holds the final real types and intentionally does not
  implement serde; exact cost for a known rate/usage; tier boundary just-below vs
  just-above `min_input_tokens` picks the right tier; cache read/write priced
  separately; 5m writes are `cache_write_tokens - cache_write_1h_tokens` and 1h
  writes use their own rate; reasoning tokens use the reasoning rate when set and
  the remaining output uses the output rate; inconsistent subsets and final
  `u64` overflow return `PricingError`; no `f64` anywhere (grep test or clippy
  `float_arithmetic` deny in this module).
- **Completion command:** `cargo test -p ygg-ai pricing::`
- **Acceptance:** deterministic integer costs; tier selection per request usage;
  matches §15 bucket semantics.
- **Failure modes:** using cumulative tokens for tiers; `f64` sneaking in; tier
  ordering assumptions (sort by `min_input_tokens`).

### Task 6.2 — Model catalog, config loading, embedded snapshot
- **Objective:** Serializable catalog + resolution; `ygg-ai` owns schema, parser,
  validation, and the embedded snapshot (design §10, final decisions §24.2–24.3).
- **Files:** `src/catalog.rs`; `crates/ygg-ai/models/catalog.json` (checked-in
  snapshot; minimal seed for v0.1 — a few models per protocol).
- **Types/functions:** `CatalogConfig`, `EndpointConfig`, `ModelConfig`,
  `AuthConfig` (serde; secrets referenced by env var only — never inlined);
  `CredentialResolverRegistry`; `ModelCatalog` with `builtin()` (`include_str!("../models/catalog.json")` →
  parse+validate), `from_config`, `register_endpoint`, `register_model`,
  `resolve(&ModelId) -> Result<Model, ConfigError>`, `models()`, and exact
  `from_config`/`from_config_with_resolvers` behavior from design §10. Validate
  reasoning effort maps and the base URL contract (absolute http(s), no
  query/fragment, trailing slash). **No network, no refresh command.**
- **Dependencies:** 2.3, 5.1 (`Auth`), 6.1 (`Pricing` in `ModelConfig`).
- **Tests:** `builtin()` parses and validates the checked-in snapshot; `resolve`
  returns a `Model` binding spec+endpoint; unknown model → `ConfigError`;
  `builtin()` and `from_config` succeed with credential env vars unset, while
  request-time auth reports missing env; endpoint `default_headers`
  containing the auth header name → `ConfigError::AuthHeaderCollision` (design §9);
  a custom OpenAI-compatible endpoint (`protocol: OpenAiChat`, custom `base_url`)
  resolves with **config only** (no code); dynamic config fails through
  `from_config` and succeeds through `from_config_with_resolvers`; malformed base
  URLs fail; a gateway path prefix survives codec URL joining.
- **Completion command:** `cargo test -p ygg-ai catalog::`
- **Acceptance:** embedded snapshot loads; resolution + collision check work; zero
  network I/O; no refresh entry point exists.
- **Failure modes:** inlining a secret in `CatalogConfig` (forbidden — env refs
  only); fetching a registry; `catalog.json` path drift from `include_str!`.

---

## Phase 7 — Shared SSE decoder

### Task 7.1 — Push SSE decoder
- **Objective:** One robust byte→event decoder for all codecs (design §13-base,
  §19).
- **Files:** `src/protocol/sse.rs`.
- **Types/functions:** `struct SseDecoder { buf: Vec<u8>, .. }`;
  `struct SseEvent { event: Option<String>, data: String }`;
  `fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, DecodeError>`
  (buffers raw bytes, splits on `\n`, handles `\r\n`, accumulates multiline
  `data:`, ignores comment lines starting `:`, decodes only complete events as
  UTF-8 so multi-byte chars spanning chunks are safe);
  `fn finish(self) -> Result<Option<SseEvent>, DecodeError>` (flush trailing
  event without terminating newline). Enforce `MAX_SSE_EVENT_BYTES = 2 MiB`;
  invalid UTF-8 or overflow returns `DecodeError`, never panic or silent loss. Recognize `[DONE]` sentinel
  as a normal `data:` payload (codec decides terminality). Preserve Anthropic
  `event:` names.
- **Dependencies:** 1.1.
- **Tests (the full matrix):** arbitrary byte chunking (re-feed one fixture at
  every boundary → identical events); LF and CRLF; multiline `data:` concatenated
  with `\n`; comment/keep-alive lines ignored; empty events (blank line only)
  produce no `SseEvent`; UTF-8 char split across two `push` calls; `[DONE]`
  surfaced as data; Anthropic `event:` name captured; malformed line tolerated
  (no panic); transport closure via `finish()` flush.
- **Completion command:** `cargo test -p ygg-ai sse::`
- **Acceptance:** every matrix bullet has a passing test; invalid UTF-8 and the
  size boundary are tested; zero panics on hostile input.
- **Failure modes:** `String::from_utf8` on partial bytes (must buffer bytes, not
  strings); losing a trailing event without newline; treating `[DONE]` as decoder
  terminal.

---

## Phase 8 — Stream accumulator and invariants

### Task 8.1 — `StreamEvent`, `ResponseBuilder`, `StreamGuard`
- **Objective:** Public events + invariant-enforced assembly (design §8).
- **Files:** `src/stream.rs`.
- **Types/functions:** `StreamEvent` with arms: `Started`; `Text{Start,Delta,End}`;
  `Reasoning{Start,Delta,End}`; `ToolCall{Start,ArgsDelta,End}`; a **single
  completed** `MediaCompleted { index, media: Media }` event (design §8 — **no
  `MediaDelta`/`MediaStart`/`MediaEnd` in v0.1**); `Usage`; `Finished(Response)`.
  `pub type ResponseStream`; `pub(crate) struct ResponseBuilder` (per-index open
  parts; `on_event`; `finish()->Response` parses tool args once, assembles
  `AssistantMessage`, attaches usage/cost/stop_reason/diagnostics);
  `pub(crate) fn guard<S>(inner: S) -> ResponseStream` validating the §8 state
  machine (one Started; streamed-part lifecycle; a `MediaCompleted` event is self-contained
  at its own index; one Finished; no events after; →`StreamProtocolError`).
- **Dependencies:** 2.3, 4.1, 6.1.
- **Tests:** builder assembles text/reasoning/toolcall + a completed `Media(Audio)`
  event into a correct `Response`; canonical indices are globally unique across
  part kinds and final content follows first-observation order; guard rejects: missing Started, duplicate
  Started, event after Finished, unbalanced streamed part, missing Finished
  (`complete()` maps to `MissingFinish`); tool-arg parse failure at finish →
  `Decode`; `MAX_TOOL_ARGUMENT_BYTES = 16 MiB` guard trips at the exact boundary
  on oversized args.
- **Completion command:** `cargo test -p ygg-ai stream::`
- **Acceptance:** invariants enforced centrally; codecs won't need to re-check.
- **Failure modes:** cloning partials into events (forbidden — deltas only);
  parsing tool args mid-stream instead of at end.

---

## Phase 9 — OpenAI Chat codec and fixtures

### Task 9.1 — Chat request builder (DTOs + mapping)
- **Objective:** Canonical→Chat wire (design §12.1), incl. image input + **audio
  input (inline WAV/MP3)** + **audio output (non-streaming)**.
- **Files:** `src/protocol/openai_chat.rs`, `src/protocol/mod.rs`
  (`Protocol`-agnostic `HttpRequestParts { url, headers, body, streaming: bool }`).
- **Types/functions:** private `ChatRequest`/message/content/tool DTOs;
  `pub(crate) fn build_request(model, req) -> Result<HttpRequestParts, AiError>`
  calling `validate_request`, mapping system→system/developer; image →
  `image_url` (URL verbatim or `data:<mime>;base64,…` from inline bytes) + `detail`;
  **audio input → `input_audio{data: base64(bytes), format:"wav"|"mp3"}`** (inline
  only; `AudioPayload` has no URL; non-`Wav`/`Mp3` → reject/drop); prior assistant
  audio ref→assistant `audio:{id}` with protocol/expiry gating; tool calls/results
  (Chat tool results are text-only; media rejects/drops); `reasoning_effort`;
  JsonObject and JsonSchema response formats; **audio output → `modalities:["text","audio"]` +
  `audio:{voice,format}` and sets `streaming=false`** (a Text-only request sets
  `streaming=true` + `stream_options.include_usage`); `max_completion_tokens`.
- **Dependencies:** 3.1, 5.1, 8.1.
- **Tests:** golden-JSON assertions for: text-only (streaming); image inline bytes
  + image URL; **audio input** (`input_audio`, WAV); **audio-output request**
  (`modalities`+`audio`, `streaming=false`); tool defs + tool_choice variants;
  text tool result; media tool-result reject/drop; prior assistant-audio replay
  and wrong/expired ref handling; reasoning effort; both structured formats;
  Lossy drop diagnostics surfaced. Assert the URL is the base directory joined
  with `chat/completions` and preserves a custom path prefix. **No
  fixture invents `choices[].delta.audio`.**
- **Completion command:** `cargo test -p ygg-ai openai_chat::build`
- **Acceptance:** emitted JSON matches goldens byte-for-byte; audio-output request
  is flagged non-streaming.
- **Failure modes:** wrong data-URL mime; `max_tokens` vs `max_completion_tokens`;
  forgetting `include_usage`; emitting `streaming=true` for an audio-output request;
  accepting a non-WAV/MP3 audio format.

### Task 9.2 — Chat stream decoder + completed-audio decode + fixtures
- **Objective:** Chat SSE→`StreamEvent` for the streaming text/tool path, and
  **non-streaming `message.audio`→ one completed `MediaCompleted` event** (design §12.1,
  §13, §15).
- **Files:** `src/protocol/openai_chat.rs`,
  `tests/fixtures/openai_chat/*.sse`, `tests/fixtures/openai_chat/*.json`
  (non-streaming audio bodies), `tests/openai_chat.rs`.
- **Types/functions:** `pub(crate) fn decode(resp_body_stream, ctx)
  -> ResponseStream` for the streaming path (`SseDecoder`+`ResponseBuilder`+`guard`;
  tool-arg accumulation by `tool_calls[].index`; usage + finish_reason→stop_reason);
  and `pub(crate) fn decode_completed(body_json, ctx) -> ResponseStream` for the
  non-streaming audio path that emits `Started`, any `Text*` for `message.content`,
  **one `MediaCompleted { media: Media::Audio(..) }`** built from `message.audio`
  (`AudioPayload::InlineWithProviderRef { data, reference: ProviderMediaRef{ id,
  expires_at, .. } }` from base64 + `transcript`), then
  `Usage`+`Finished`. **There is no `MediaDelta`.**
- **Dependencies:** 9.1, 7.1, 8.1.
- **Tests (fixtures):** plain text; multiple text blocks; streamed
  `reasoning_content`; one tool call; parallel tool calls interleaved; args split
  at arbitrary byte boundaries; malformed tool JSON→`Decode`; usage only at end;
  length stop; tool-use stop; **completed audio output** (non-streaming
  `message.audio` → single `MediaCompleted` event + transcript); premature EOF→
  `PrematureEof`. **No `choices[].delta.audio` fixture.**
- **Completion command:** `cargo test -p ygg-ai --test openai_chat`
- **Acceptance:** every fixture yields the expected event sequence + `Response`;
  audio output arrives as exactly one completed `MediaCompleted` event.
- **Failure modes:** tool index vs id confusion; dropping the final usage chunk;
  reasoning text mistakenly given state (Chat has none); inventing an audio delta
  event instead of one completed `Media`.

---

## Phase 10 — Anthropic Messages codec and fixtures

### Task 10.1 — Anthropic request builder
- **Objective:** Canonical→Messages wire (design §12.3), incl. thinking replay.
- **Files:** `src/protocol/anthropic.rs`.
- **Types/functions:** private DTOs; `build_request` mapping system→top-level
  param; merge adjacent user messages (alternation); image inline(base64)/url
  source; tool-result text/image mappings with provider refs rejected; structured
  JSON schema→`output_config.format`; JsonObject reject/drop; **audio → Strict `Unsupported(Audio)` / Lossy drop + `Diagnostic`
  (never silent)**; tool_use/tool_result; `thinking`/`redacted_thinking` replay
  gated on same protocol+model; `tools`+`input_schema`; `tool_choice`;
  `thinking{type:enabled,budget_tokens}` (effort uses the catalog's complete
  effort map); required effective `max_tokens`. Emit no cache-control request
  fields in v0.1.
- **Dependencies:** 3.1, 5.1, 8.1.
- **Tests:** goldens for text; image; message merging (text turn + tool results →
  one user message); tool call/result pairing; signature replay same-model vs
  drop/reject cross-model; **audio input → Strict `Unsupported(Audio)` error /
  Lossy drop+diagnostic (asserts a diagnostic is emitted — not silent)**; every
  effort maps to its configured budget; schema-output golden; JsonObject
  reject/drop; assert no cache-control field is emitted.
- **Completion command:** `cargo test -p ygg-ai anthropic::build`
- **Acceptance:** goldens match; alternation invariant holds; audio returns a
  structured error in Strict and is never silently removed in Lossy.
- **Failure modes:** silently dropping audio; not merging consecutive user
  messages; replaying a signature from a different model; missing `max_tokens`.

### Task 10.2 — Anthropic stream decoder + fixtures
- **Objective:** Messages SSE→`StreamEvent` (design §12.3, §13, §15).
- **Files:** `src/protocol/anthropic.rs`,
  `tests/fixtures/anthropic/*.sse`, `tests/anthropic.rs`.
- **Types/functions:** `decode` handling `message_start`, `content_block_start/
  delta/stop` (text/thinking/redacted/tool_use), `input_json_delta` accumulation
  by block index, `signature_delta`→`AnthropicSignature`, `message_delta`
  (stop_reason,usage), `message_stop`, `ping` ignored, `error`→`Provider`.
- **Dependencies:** 10.1, 7.1, 8.1.
- **Tests (fixtures):** text; streamed thinking + signature (opaque state
  preserved); redacted_thinking (no visible text); one tool call; parallel tool
  calls; args split at byte boundaries; malformed tool JSON; usage in
  `message_delta`; `end_turn`/`max_tokens`/`tool_use`/`stop_sequence`/`pause_turn`
  stop reasons; `error` event; premature EOF; `ping` keep-alive interleaved.
- **Completion command:** `cargo test -p ygg-ai --test anthropic`
- **Acceptance:** all fixtures pass; signature/redacted state captured as opaque,
  never as text.
- **Failure modes:** flattening thinking signature into text; miscounting cache
  buckets (design §15); treating `ping` as an event.

---

## Phase 11 — OpenAI Responses codec and fixtures

### Task 11.1 — Responses request builder
- **Objective:** Canonical→Responses wire (design §12.2), incl. reasoning replay.
- **Files:** `src/protocol/openai_responses.rs`.
- **Types/functions:** private input-item DTOs; `build_request` mapping system→
  developer/system item; user text→`input_text`; image→`input_image`(+`detail`);
  **audio input → Strict `Unsupported(Audio)` / Lossy drop+`Diagnostic`**; tool
  call→`function_call`; tool result→`function_call_output`; reasoning item replay
  (`encrypted_content`,`id`) gated same protocol+model with
  `include:["reasoning.encrypted_content"]`,`store:false`; `reasoning.effort`;
  JsonObject and JsonSchema→the exact `text.format` shapes; tool-result image
  URL/inline/same-protocol file-id mappings.
  **No audio-output mapping:** `OutputModalities::TextAndAudio` → Strict
  `Unsupported(AudioOutput)` / Lossy downgrade-to-Text+`Diagnostic`. Do **not**
  emit `modalities`/`audio`.
- **Dependencies:** 3.1, 5.1, 8.1.
- **Tests:** goldens for text; image; tool call/result; reasoning replay
  (encrypted item round-trips same-model; dropped/rejected cross-model); **audio
  input → `Unsupported(Audio)` (Strict) / drop+diagnostic (Lossy)**; **audio
  output requested → `Unsupported(AudioOutput)` (Strict) / downgrade+diagnostic
  (Lossy)**; effort mapping; both structured formats; tool-result media source
  matrix, including wrong-protocol refs.
- **Completion command:** `cargo test -p ygg-ai openai_responses::build`
- **Acceptance:** goldens match; reasoning-state replay gated; audio in **and**
  out are structurally rejected in Strict; no `modalities`/`audio` ever emitted.
- **Failure modes:** sending `store:true`; emitting audio params; replaying
  encrypted content to a different model; wrong developer-vs-system role.

### Task 11.2 — Responses stream decoder + fixtures
- **Objective:** Responses SSE→`StreamEvent` (design §12.2, §13, §15).
- **Files:** `src/protocol/openai_responses.rs`,
  `tests/fixtures/openai_responses/*.sse`, `tests/openai_responses.rs`.
- **Types/functions:** `decode` handling `response.created`, `output_item.added/
  done`, `content_part.added/done`, `output_text.delta/done`, `reasoning_text.*`/
  `reasoning_summary_text.*`, reasoning item done→`OpenAiReasoning` state,
  `function_call_arguments.delta/done` (accumulate by `output_index`),
  `response.completed`/`incomplete`→usage+finish, `response.failed`→`Provider`;
  **skip `response.audio.*` and all other out-of-scope event families without
  error** (no audio mapping exists).
- **Dependencies:** 11.1, 7.1, 8.1.
- **Tests (fixtures):** plain text; multiple text parts; streamed reasoning_text;
  reasoning summary; encrypted reasoning at item done; one tool call; parallel
  tool calls; args split at byte boundaries; malformed tool JSON; usage at
  `completed`; `incomplete{max_output_tokens}`→MaxTokens; tool-use stop;
  `response.failed`→`Provider`; premature EOF; an out-of-scope event (e.g.
  `web_search_call.*`) ignored. **No `response.audio.*` fixture** — not documented
  as supported here; do not invent one.
- **Completion command:** `cargo test -p ygg-ai --test openai_responses`
- **Acceptance:** all fixtures pass; encrypted state captured opaque; no audio
  handling exists.
- **Failure modes:** keying tool args by the wrong index; treating an ignorable
  event as terminal; losing `encrypted_content` at item done; adding audio decode.

---

## Phase 12 — `AiClient::stream`

### Task 12.1 — Client dispatch + HTTP send + stream wiring
- **Objective:** End-to-end `stream()` (design §5, §8, §9, §17).
- **Files:** `src/client.rs`.
- **Types/functions:** `AiClient { http }` (+ `new`/`with_http_client`); uses the
  `Model` handle from Task 6.2; `pub async fn stream(&self, &Model, Request) ->
  Result<ResponseStream, AiError>` that: resolves `Auth`→headers (auth last; §9
  order), composes `default_headers`→protocol headers→credential extras→primary
  auth, `match`es `spec.protocol` to the codec `build_request`, joins the
  validated versioned base directory with the codec-relative path, sends via reqwest with
  `endpoint.timeout` (**for Chat, uses `HttpRequestParts.streaming` to choose the
  streaming vs non-streaming decode path — audio-output is non-streaming**), maps
  every non-2xx to `HttpError` (parse provider error body, capture
  status/request-id/retry-after; no secrets), request/body failures without a
  status to `TransportError`, and reserves `ProviderError` for error frames in a
  successful 2xx stream; then hands the body to the codec
  `decode`/`decode_completed` wrapped by `guard`.
- **Dependencies:** 9.2, 10.2, 11.2, 6.2 (`Model`/`resolve`), 5.1.
- **Tests (offline, wiremock):** happy-path SSE for each protocol yields events
  ending in `Finished`; **Chat non-streaming audio-output body yields one `Media`
  event then `Finished`**; non-2xx JSON error body→`AiError::Http` with
  status/request-id populated and no secret; connect/header and mid-body failures
  → phase-correct `AiError::Transport`; custom gateway path prefix is preserved; auth header present on the outbound
  request; header collision at endpoint config→`ConfigError`; drop mid-stream
  stops body polling (drop-sentinel).
- **Completion command:** `cargo test -p ygg-ai --test client_stream`
- **Acceptance:** all three protocols stream through the client offline; errors
  before first byte returned from `stream()` (not in-stream).
- **Failure modes:** applying auth before default_headers (override risk);
  reading an unbounded whole body (the completed response path must stream and
  enforce `MAX_COMPLETED_BODY_BYTES = 64 MiB`); retry logic sneaking in
  (forbidden).

---

## Phase 13 — `AiClient::complete`

### Task 13.1 — `complete()` over the same stream
- **Objective:** One-shot as consumed stream (design §5, §8).
- **Files:** `src/client.rs`.
- **Types/functions:** `pub async fn complete(&self, &Model, Request)
  -> Result<Response, AiError>` = drive `stream()` to `Finished`; error if no
  `Finished` (`StreamProtocolError::MissingFinish`); propagate the first `Err`.
- **Dependencies:** 12.1.
- **Tests:** `complete` returns the same `Response` as the terminal `Finished`
  from `stream` for an identical fixture (all three protocols); mid-stream error
  propagates; missing-finish maps correctly.
- **Completion command:** `cargo test -p ygg-ai --test client_complete`
- **Acceptance:** `complete` shares the exact streaming path; no second option
  system exists anywhere.
- **Failure modes:** re-implementing decoding for `complete`; swallowing errors.

---

## Phase 14 — Cross-protocol serialization tests

### Task 14.1 — Switching, replay, and mode diagnostics
- **Objective:** Verify serialization rules (design §11, §7, §14).
- **Files:** `tests/cross_protocol.rs` (+ small fixtures/goldens).
- **Types/functions:** none new — exercises existing `build_request` per protocol
  over one canonical history.
- **Tests:** one canonical conversation (system + user image + assistant tool call
  + user tool result + assistant reasoning-with-state) serialized to each
  protocol: system placement correct; Anthropic merges consecutive user messages;
  tool call/result IDs stay linked (incl. deterministic normalization where a
  protocol constrains ID format); structured output maps to all capable
  protocols and rejects/downgrades on incapable models; **same-protocol**
  reasoning-state replay emits
  the encrypted/signature block; **cross-protocol** reasoning-state is dropped
  (Lossy + `Diagnostic`) or rejected (Strict); image→text-only model Strict
  reject vs Lossy drop; audio→Anthropic Strict reject vs Lossy drop; canonical
  `Request.messages` is unchanged after every build (no mutation).
- **Completion command:** `cargo test -p ygg-ai --test cross_protocol`
- **Acceptance:** every §11/§14 rule has a passing assertion in both modes;
  immutability of canonical history proven.
- **Failure modes:** normalization breaking call/result pairing; reasoning text
  auto-converted to assistant text (forbidden).

---

## Phase 15 — Public API cleanup and documentation

### Task 15.1 — Surface audit + docs + redaction proof
- **Objective:** Compact, documented public API (design §5, §20).
- **Files:** `src/lib.rs`, all modules (doc comments), `README.md` (crate).
- **Types/functions:** finalize re-exports exactly per design §5; `#![deny(
  missing_docs)]`; ensure `protocol::*` is `pub(crate)`; add crate-level example
  mirroring design §22.
- **Dependencies:** 13.1, 14.1.
- **Tests:** `cargo doc` builds with no warnings; doctest for the §22 example
  compiles (`no_run`); a redaction test proving `Secret`/`Auth`/`Endpoint` debug
  output contains no secret; a `tests/public_api.rs` that imports every re-export
  (guards accidental removals).
- **Completion command:** `cargo test -p ygg-ai && cargo doc -p ygg-ai --no-deps`
- **Acceptance:** no missing-docs warnings; no wire DTO leaked publicly; example
  compiles.
- **Failure modes:** leaking a `protocol` type via a public signature; doctest
  drift from real API.

---

## Phase 16 — Full workspace integration and final verification

### Task 16.1 — Workspace build, lint, and full suite
- **Objective:** Green everything, offline (design §18, §19).
- **Files:** none (or CI config `./.github/workflows/ci.yml` if desired).
- **Dependencies:** all prior.
- **Tests:** entire suite; confirm every design §19 required fixture case exists
  (checklist test or a `tests/coverage_manifest.rs` listing fixture files); assert
  **no fixture file contains `choices[].delta.audio` or `response.audio.*`** (grep
  guard) and that the embedded `models/catalog.json` loads via `builtin()`.
- **Completion command:**
  `cargo fmt --check && cargo clippy --workspace --all-features --all-targets -- -D warnings && cargo test --workspace --all-features && cargo doc --workspace --no-deps`
- **Acceptance:** workspace formats, lints clean (deny warnings), all tests pass
  with no network access and no API keys; the §19 fixture checklist is complete
  (incl. image + Chat audio-in + **Chat completed audio-out** + audio
  reject-on-Responses/Anthropic + secret-redaction + tier-boundary +
  cross-protocol state).
- **Failure modes:** a fixture requiring network; a forbidden `delta.audio`/
  `response.audio.*` fixture; clippy `float_arithmetic` in `pricing`; a lingering
  `dbg!`/secret in output.

### Task 16.2 — Adversarial audit, cleanup, and implementation report
- **Objective:** Prove behavioral completeness, not merely compilation.
- **Files:** `docs/reports/ygg-ai-implementation-report.md`; implementation/tests
  as needed to fix findings.
- **Required audit:** walk every public item in design §5, every validation row in
  §7, every mapping row in §12, every terminal path in §8, every usage/stop row
  in §15, and every required fixture in §19. Link each to implementation and at
  least one test. Search for `todo!`, `unimplemented!`, `TODO`, `FIXME`, ignored
  tests, public protocol DTOs, accidental floats in pricing, and secret exposure.
  Inspect `cargo tree -p ygg-ai` for forbidden/default TLS dependencies. Confirm
  tests pass with credential environment variables unset and no network.
- **Fix loop:** fix every audit finding, rerun focused tests, then rerun Task
  16.1's complete gate. Repeat until no known finding remains.
- **Report contents:** executive summary; exact scope delivered; task checklist;
  public API inventory; protocol capability/mapping matrix; test/fixture inventory;
  commands with exit status; security/cancellation/pricing evidence; dependency
  inventory; deviations/blockers (must be empty for completion, or explicitly
  state that work is incomplete); and a reviewer guide naming the highest-risk
  files and exact checks to reproduce.
- **Acceptance:** report claims only evidenced behavior, includes the final green
  gate output summary, and the source tree has no known spec deviation.

---

## Global dependency graph (summary)

```
1.1
 ├─ 2.1 ─ 2.2 ─ 2.3 ─┬─ 3.1 (also needs 4.1)
 │                    └─ 6.1 (also needs 4.1) ─ 6.2 (also needs 5.1)
 ├─ 4.1 ─ 4.2 ─ 5.1
 ├─ 7.1
 └─ (2.3,4.1,6.1) ─ 8.1
8.1 + 3.1 + 5.1 ─┬─ 9.1 ─ 9.2
                 ├─ 10.1 ─ 10.2
                 └─ 11.1 ─ 11.2
(9.2,10.2,11.2,6.2) ─ 12.1 ─ 13.1 ─ 14.1 ─ 15.1 ─ 16.1 ─ 16.2
```

**Scheduling note:** Task 3.1 depends on 4.1's `AiError`/`Diagnostic`; Task 6.2
(catalog, which defines the `Model` handle) depends on 5.1 (`Auth`) + 6.1
(`Pricing`). Required dependency order: 1.1 → 2.1–2.3 → 4.1 → 4.2 → 3.1 → 5.1 → 6.1 → 6.2
→ 7.1 → 8.1 → 9–11 → 12 → 13 → 14 → 15 → 16.1 → 16.2. Phases are
numbered by design topic; execution interleaves 3/4 and 6.2 as noted.
