# Built-in catalog sources

The built-in endpoint/protocol catalog is a checked-in snapshot. Last audited: **2026-07-10**.

Human-facing model identity is generated separately by `crates/ygg-ai/build.rs`. Each compile of `ygg-ai` attempts to fetch the provider-agnostic catalog from:

```text
https://models.dev/models.json
```

The build script emits a compact, sorted Rust lookup table into `OUT_DIR`; the application never performs a network request for model names at runtime. If the build host is offline or models.dev is unavailable, it uses `models/models-dev-names.json`. Set `YGG_MODELS_DEV_OFFLINE=1` to force that reproducible offline path. The fallback contains only canonical IDs and display names, not endpoint credentials or transport policy.

- Protocol request/response capabilities are constrained by the repository API docs under `docs/research/apidocs/`.
- `gpt-4o-mini` text pricing uses $0.15/M input, $0.60/M output, and $0.075/M cached input, represented as 150,000 / 600,000 / 75,000 microdollars per million tokens.
- `gpt-5.4-mini` pricing uses $0.75/M input, $4.50/M output, and $0.075/M cached input. OpenAI does not separately bill cache writes for this route.
- Anthropic text pricing records the published input/output/cache-read/5-minute-write rates and the explicit one-hour write rate of 2× input: Sonnet 4.5 and 4.6 use $3/$15/$0.30/$3.75/$6; Opus 4.8 uses $5/$25/$0.50/$6.25/$10; Fable 5 uses $10/$50/$1/$12.50/$20, all per million tokens.
- The audio seed entry intentionally has `pricing: null` because this repository has no authoritative price snapshot for its separate text/audio token classes.

Endpoint auth, protocol compatibility, and pricing remain explicit Ygg configuration; models.dev supplies presentation metadata rather than silently changing runtime transport behavior. Applications requiring guaranteed-current commercial transport metadata should supply `CatalogConfig` explicitly.
