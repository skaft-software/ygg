# GPT-6 Astra implementation report

## Status

Complete on branch `ygg-fast/gpt6-astra`, based on `d47dd3b64b5fe10f4f3f010056bf27e503fbd83e`.

Implementation commit: `827907b5837687c37396e929219653e03126ee85` (`feat(models): add GPT-6 Astra support`).

Documentation commit: `3789647fdfa870435a8ae414d27e3177a62a77ae` (`docs(models): update Codex Astra discovery contract`).

No provider or external-network calls, real credential access, dependency installs, pushes, generated provider-declaration edits, or other-worktree changes were made. Cargo dependency resolution ran offline with provider/session environment variables unset; test-only loopback fixtures remained local.

## Source observations encoded

- Direct OpenAI: Responses model `gpt-6-astra`; 1,050,000-token context; 128,000-token output; text/image input and text output; tools, parallel tools, and structured output; reasoning `low` through `max` with omitted reasoning left to the provider default; no Responses Lite or agent delegation.
- Direct pricing per million tokens is input/cache-read/cache-write/output `$10/$1/$12.50/$50`; above 272,000 input tokens it is `$20/$2/$25/$75` (tier starts at 272,001).
- The supplied Codex/Pi observation uses compatibility client `0.153.2`, object-form reasoning levels, default `low`, an 872K advertised input maximum, Responses Lite metadata, and `multi_agent_version = v2`. Ygg keeps a 272K active working budget.
- Ultra remains a host-side V2 tier: complete live reasoning metadata plus V2 is required, the Responses wire effort is capped at `max`, and V2 delegation uses `xhigh`. Offline/cache fallback strips Lite, V2, and Ultra.
- A successful live Codex inventory is authoritative. Astra is not injected when omitted; when present beside the direct route, it resolves as `codex/gpt-6-astra`.

## Implementation

- Added the complete direct OpenAI model contract and long-context pricing tier to the embedded `ygg-ai` catalog.
- Recognized GPT-6 (including provider-qualified IDs) in generic Responses reasoning and vision discovery.
- Added Astra to conservative Codex fallback metadata with image input, `low..max`, 272K active / 872K advertised context, and 128K output.
- Bumped the Codex model query compatibility version to `0.153.2` and cache schema to 4, invalidating schema-3 inventories.
- Parsed string/object live reasoning levels, promoted host Ultra only with complete live V2 metadata, retained live Lite only online, and preserved live-inventory entitlement/visibility behavior.
- Kept direct and Codex routes distinct and exercised the normal CLI and model-picker paths without a bespoke Astra command.
- Documented the model contract and Codex source interpretation in `README.md`, `crates/ygg-ai/models/SOURCES.md`, and `docs/design/ygg-coding-agent.md`.

## Verification

Passed:

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Focused `ygg-ai` Astra tests: 2 passed
- Focused Codex tests: 18 passed
- Focused CLI, picker, GPT-6 reasoning-discovery, and vision-discovery tests: 4 passed
- `git diff --check`

Full-suite observations:

- `cargo test -p ygg-ai --lib`: 265 passed; one unchanged baseline assertion failed at `crates/ygg-ai/src/protocol/cross_protocol_tests.rs:273` (`test_lossy_inserts_missing_tool_result_before_next_assistant`). The file is unchanged from the requested baseline and the failure reproduces in isolation.
- `cargo test -p ygg-coding-agent --lib`: 986 passed; one unchanged baseline assertion failed at `crates/ygg-coding-agent/src/app/bootstrap/tests.rs:395` (`opencode_discovery_infers_supported_protocols_and_skips_gemini`). The assertion and its route-selection implementation were not modified by this work.

All Astra-focused tests and workspace compile/lint checks pass.
