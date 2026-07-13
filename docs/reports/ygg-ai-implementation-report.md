# `ygg-ai` Implementation Report

Evidence ledger for building `crates/ygg-ai` per `docs/design/ygg-ai.md` and
`docs/plans/ygg-ai-implementation.md`.

Status: **complete**

## Environment
- rustc 1.96.0, cargo 1.96.0, platform darwin.
- Repository started docs-only (no `crates/`, no `Cargo.toml`, no `.git`).

## Task ledger

| Task | Status | Files | Tests | Command | Result |
|------|--------|-------|-------|---------|--------|
| 1.1  | Complete | `crates/ygg-ai/Cargo.toml`, `src/lib.rs`, `tests/smoke.rs` | `default_compatibility_is_strict` | `cargo test -p ygg-ai` | Passed |
| 2.1  | Complete | `src/types.rs`, `src/pricing.rs` | `test_modality_set_algebra`, `test_model_spec_serde_round_trip` | `cargo test -p ygg-ai` | Passed |
| 2.2  | Complete | `src/types.rs` | `test_message_serde_round_trip`, `test_tool_call_arguments_value` | `cargo test -p ygg-ai` | Passed |
| 2.3  | Complete | `src/types.rs` | `test_request_serde_round_trip`, `test_usage_default`, `test_stop_reason_custom_serde` | `cargo test -p ygg-ai` | Passed |
| 4.1  | Complete | `src/error.rs` | `test_error_display_is_secret_free`, `test_transport_error_preserves_phase`, `test_http_error_no_secret_headers`, `test_is_safe_to_retry` | `cargo test -p ygg-ai` | Passed |
| 4.2  | Complete | `src/auth.rs` | `test_secret_redacts_debug_and_display`, `test_secret_from_env` | `cargo test -p ygg-ai` | Passed |
| 3.1  | Complete | `src/validate.rs` | base tests + **18 `validate::matrix_tests`** covering every §7 row in Strict+Lossy (exact variants + diagnostic codes) | `cargo test -p ygg-ai --all-features validate::` | Passed |
| 5.1  | Complete | `src/auth.rs`, `src/types.rs` | `test_resolve_headers_bearer_and_custom`, `test_resolve_headers_env`, `test_resolve_headers_dynamic`, `test_auth_header_name` | `cargo test -p ygg-ai` | Passed |
| 6.1  | Complete | `src/pricing.rs`, `src/types.rs` | `test_cost_of_simple`, `test_cost_of_tier_boundary`, `test_cost_inconsistent_subsets` | `cargo test -p ygg-ai` | Passed |
| 6.2  | Complete | `src/catalog.rs`, `models/catalog.json` | `test_builtin_catalog_loads_and_resolves`, `test_invalid_base_url_fails`, `test_auth_header_collision`, `test_dynamic_resolver_loading` | `cargo test -p ygg-ai` | Passed |
| 7.1  | Complete | `src/protocol/sse.rs` | `test_sse_decoder_basic`, `test_sse_decoder_crlf_and_comments`, `test_sse_decoder_utf8_split_chunking`, `test_sse_decoder_boundary_chunking_property`, `test_sse_decoder_finish_trailing_no_newline` | `cargo test -p ygg-ai` | Passed |
| 8.1  | Complete | `src/stream.rs` | `test_response_builder_full`, `test_response_builder_tool_call_invalid_json`, `test_response_builder_oversized_args`, `test_guard_missing_start`, `test_guard_duplicate_start`, `test_guard_event_after_finish` | `cargo test -p ygg-ai` | Passed |
| 9.1  | Complete | `src/protocol/openai_chat.rs` | `test_build_request_text_only`, `test_build_request_audio_out` | `cargo test -p ygg-ai` | Passed |
| 9.2  | Complete | `src/protocol/openai_chat.rs`, `tests/fixtures/openai_chat/*` | base tests + **9 `fixture_tests`** (plain/reasoning/1 tool/parallel/malformed/length/premature-EOF/completed-audio + byte-boundary property) | `cargo test -p ygg-ai --all-features openai_chat::` | Passed |
| 10.1 | Complete | `src/protocol/anthropic.rs` | `test_build_request_anthropic_basic`, `test_build_request_anthropic_thinking_replay` | `cargo test -p ygg-ai` | Passed |
| 10.2 | Complete | `src/protocol/anthropic.rs`, `tests/fixtures/anthropic/*` | base tests + **10 `fixture_tests`** (text+ping/thinking+signature/redacted/1 tool/parallel/malformed/stop-reasons/error/premature-EOF + byte-boundary) | `cargo test -p ygg-ai --all-features anthropic::` | Passed |
| 11.1 | Complete | `src/protocol/openai_responses.rs` | `test_build_request_responses_basic` | `cargo test -p ygg-ai` | Passed |
| 11.2 | Complete | `src/protocol/openai_responses.rs`, `tests/fixtures/openai_responses/*` | base tests + **10 `fixture_tests`** (text/encrypted-reasoning/1 tool/parallel/malformed/incomplete-max/ignored-event/failed/premature-EOF + byte-boundary) | `cargo test -p ygg-ai --all-features openai_responses::` | Passed |
| 12.1 | Complete | `src/client.rs`, `tests/client_stream.rs` | `test_client_stream_sse_openai_chat`, `test_client_stream_non_streaming_chat_audio`, `test_client_stream_http_error_handling` | `cargo test -p ygg-ai --test client_stream` | Passed |
| 13.1 | Complete | `src/client.rs`, `tests/client_complete.rs` | `test_client_complete_happy_path` | `cargo test -p ygg-ai --test client_complete` | Passed |
| 14.1 | Complete | `src/protocol/cross_protocol_tests.rs` | `test_cross_protocol_canonical_immutability`, `test_cross_protocol_anthropic_message_merging`, `test_cross_protocol_reasoning_state_rejection` | `cargo test -p ygg-ai` | Passed |
| 15.1 | Complete | `src/lib.rs`, `tests/public_api.rs` | `test_public_api_secret_redaction_proof` | `cargo test -p ygg-ai --test public_api` | Passed |
| 16.1 | Complete | Crate-wide + `tests/coverage_manifest.rs` | **129 tests + 1 doc-test**; coverage manifest asserts all §19 fixtures exist + grep-guards forbidden audio events + `builtin()` loads | `cargo fmt --check && cargo clippy --workspace --all-features --all-targets -- -D warnings && cargo test --workspace --all-features && cargo doc --workspace --no-deps` | Passed |
| 16.2 | Complete | `docs/reports/ygg-ai-implementation-report.md` | Adversarial audit performed; findings + fixes recorded below | (see gate above) | Completed |

## Verification-and-remediation pass (second author)

An independent verification pass ran the real Task 16.1 gate and the §16.2
adversarial audit rather than trusting the ledger. It found the gate green but
the delivery materially incomplete against the plan's acceptance criteria, and
fixed all findings. Summary:

### Gaps closed
- **Fixture matrix (design §19; Tasks 9.2/10.2/11.2) was entirely missing.**
  Added 28 hand-authored, apidocs-cited fixtures under
  `crates/ygg-ai/tests/fixtures/{openai_chat,anthropic,openai_responses}/` and
  47 codec fixture/validation tests. Each fixture cites its `apidocs/` source in
  a leading `:` SSE comment (ignored by the decoder). A shared offline harness
  (`protocol::harness`) feeds each fixture through the real `SseDecoder` +
  codec + `guard`, and re-runs every "plain" fixture at **every byte boundary**
  to prove chunk-independence.
- **§7 validation matrix (Task 3.1) under-tested (4 tests).** Added 18
  `validate::matrix_tests` covering every §7 row in **both** Strict and Lossy
  modes, asserting exact error variants and exact diagnostic codes.
- **Coverage manifest + fixture prohibition guard (Task 16.1).** Added
  `tests/coverage_manifest.rs`: asserts every required fixture exists, greps all
  streaming fixtures to forbid invented `choices[].delta.audio` /
  `response.audio.*`, and confirms `builtin()` loads offline.
- **Public-API leak.** `pub mod protocol_test` exposed codec internals in the
  public API. The cross-protocol tests now live in an in-crate
  `#[cfg(test)] mod cross_protocol_tests` (`src/protocol/cross_protocol_tests.rs`),
  so codec internals are exercised without a public test seam and without any
  feature gate.
- **Stub `decode_response`** for Responses/Anthropic (returned "not
  implemented") removed; the non-streaming path is Chat-only by construction.
- **`unimplemented!()`** in a `#[cfg(test)]` resolver replaced with a real error.
- **Broad `#![allow(dead_code)]`** removed from all five modules; the few unread
  serde DTO fields/variants were trimmed.
- **Named size constants** `MAX_TOOL_ARGUMENT_BYTES` / `MAX_COMPLETED_BODY_BYTES`
  replace magic numbers (design §20).

### Real behavioral bugs found by the new fixtures and fixed
- **Pre-send Lossy diagnostics were dropped.** `build_request` computed
  `HttpRequestParts.diagnostics` but the client never propagated them; they now
  seed the terminal `Finished.diagnostics` (design §7). (Surfaced as a
  dead-field warning.)
- **Anthropic could not decode real content deltas.** `AnthropicResponseDelta`
  used `rename_all="snake_case"` (`text`/`thinking`/…) but the wire uses
  `text_delta`/`thinking_delta`/`signature_delta`/`input_json_delta`. Fixed with
  explicit renames.
- **Anthropic `redacted_thinking` was undecodable** (missing DTO variant); now
  decoded to `AnthropicRedacted` opaque state with no visible text.
- **Anthropic stop reasons `pause_turn`/`refusal`** fell through to `Other`; now
  mapped per §15.
- **Anthropic usage violated §15** (subtracted cache from `input_tokens`, which
  already excludes cache; excluded cache from `total`; `message_delta` usage
  overwrote the `message_start` input count and failed to parse output-only
  usage). Fixed; `cache_creation.ephemeral_1h_input_tokens` now mapped.
- **Responses usage used Chat field names** (`prompt_tokens`/`completion_tokens`)
  vs. the real `input_tokens`/`output_tokens`, and dropped cache/reasoning
  details — the completed event would fail to parse. Fixed per §15.
- **Responses tool-call id** used the item `id` instead of `call_id`; **stop
  reason** hardcoded `EndTurn` even for tool calls. Both fixed per §12.2/§15.
- **`PrematureEof` was never emitted** (a truncated stream produced
  `MissingFinish`). `guard` now emits `PrematureEof` for a started-but-unfinished
  stream; `MissingFinish` is reserved for a stream yielding no `Finished` at all.

### Final gate (exact Task 16.1 command)
`cargo fmt --check && cargo clippy --workspace --all-features --all-targets -- -D warnings && cargo test --workspace --all-features && cargo doc --workspace --no-deps`
→ **PASS**. `cargo test -p ygg-ai --all-features` passes; the cross-protocol
suite runs as an in-crate `#[cfg(test)]` module (no feature gate). `cargo tree`
shows rustls-only TLS, no provider SDKs, no OpenSSL/native-tls.

## Known limitations / deviations (honest)
- Cross-protocol codec tests live in `src/protocol/cross_protocol_tests.rs` as a
  `#[cfg(test)]` module; there is no public test seam and no `test-util` feature.
- **OAuth / subscription login not implemented.** The `Auth::Dynamic` +
  `CredentialResolver` infrastructure is complete and tested (Task 5.1), but no
  concrete `CredentialResolver` exists for subscription-based providers (OpenAI
  Codex, Anthropic Pro/Max, GitHub Copilot). The catalog has no
  subscription-model entries. This is tracked in
  `docs/plans/ygg-ai-oauth-codex.md` and is a deferred feature, not a design
  defect — the trait is the intended integration point.

## Backward-Compatible Additions & Design Notes
- **Additional Error Variants:** Added `UnsupportedError::StopSequences`, `ConfigError::InvalidModel`, and `ConfigError::InvalidTimeout` to error structures to support complete client-side diagnostics and validation without breaking backward compatibility.
- **Reasoning State Rejection on Chat:** Chat models do not support streaming/returning reasoning states. To prevent inconsistent behavior or silent data loss, mismatched reasoning states are strictly rejected even when targeted to Chat models.
- **Usage Underflow:** Disjoint-bucket usage normalization employs checked arithmetic and explicitly raises `DecodeError::UsageUnderflow` on contradictory provider counters rather than silently using saturating subtraction.

Status corrected to **complete** on the basis of the evidence above, not
compilation alone.
