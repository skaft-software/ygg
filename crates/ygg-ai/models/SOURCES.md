# Built-in catalog sources

The built-in catalog is a checked-in snapshot, not a live registry. Last audited: **2026-07-10**.

- Protocol request/response capabilities are constrained by the repository API docs under `docs/research/apidocs/`.
- `gpt-4o-mini` text pricing uses $0.15/M input, $0.60/M output, and $0.075/M cached input, represented as 150,000 / 600,000 / 75,000 microdollars per million tokens.
- `claude-sonnet-4-5-20250929` pricing uses $3/M input, $15/M output, $0.30/M cache read, and $3.75/M 5-minute cache write, represented as 3,000,000 / 15,000,000 / 300,000 / 3,750,000 microdollars per million tokens.
- Audio and Responses seed entries intentionally have `pricing: null` where this repository has no authoritative price snapshot.

Workspace tooling should update this file and `catalog.json` together when refreshing model names, limits, capabilities, or prices. Applications requiring guaranteed-current commercial metadata should supply `CatalogConfig` explicitly.
