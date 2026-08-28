# Claims ledger

Every public performance or reliability statement must use one of these states:

- **VERIFIED** — directly supported within the wording's stated scope.
- **PRELIMINARY** — bounded evidence exists, but comparison, sample, or method is
  insufficient for a broad claim.
- **UNDER DEVELOPMENT** — implementation or evidence packaging is incomplete.
- **UNSUPPORTED** — no adequate evidence; do not present as established fact.

## Current claims

| Claim | Status | Defensible wording | Evidence and boundary |
| --- | --- | --- | --- |
| Frozen Terminal-Bench 2.1 raw result | **VERIFIED** | “Ygg v0.6.2 scored 391/445 raw (87.87%) with GPT-5.6 Sol/max over 89 tasks × 5 trials.” | Canonical result, identity, hashes, and timeout overlap are in the [evidence report](tb21-v0.6.2/README.md). This is the raw Harbor verifier result. |
| Local integrity audit | **VERIFIED** | “A local GLM-5.3 Flash surrogate rubric audit plus manual review yields 387/445 primary (86.97%) or 385/445 strict (86.52%).” | All 391 raw successes were audited; four confirmed and two uncertain cases are named. This is **not official maintainer adjudication**. |
| Audited Pass@5 | **VERIFIED** | “Primary and strict audited Pass@5 are 87/89 (97.75%).” | Recomputed from the five per-task outcomes after each audit policy; verified by the compact package. |
| Raw Codex comparison | **PRELIMINARY** | “In the matched published GPT-5.6 Sol/max run, Codex scored 83.37% raw; Ygg scored 87.87% raw.” | Codex raw is inferred from 339 official successes plus 32 published disqualifications. Dates/provider snapshots differ, Codex trajectories are unavailable here, and adjudication pipelines are not comparable. Never compare Ygg's 387/385 to Codex's 339. |
| Token traffic versus published Codex aggregate | **PRELIMINARY** | “Complete-native Ygg used 15.1% fewer processed tokens per raw success than the matched published Codex aggregate.” | Comparable aggregate buckets, but not a controlled dollar, compute, latency, or energy experiment; Ygg includes historical post-timeout leakage. |
| Local-first architecture | **VERIFIED** | “Sessions and configuration are local; model traffic goes to the configured endpoint; local OpenAI-compatible endpoints and offline startup are supported.” | Source, tests, and product docs verify the capability. It does not mean inference is always local or that optional features never use the network. |
| Optional local telemetry and `ygg doctor` | **VERIFIED** | “Ygg offers opt-in owner-only local telemetry and a read-mostly local diagnostics command.” | Implemented with bounded redacted records and focused tests. No passive or remote telemetry is implied. |
| Docker Harbor process-tree finalization | **VERIFIED** | “The v0.6.3 Docker adapter performs TERM→KILL process-group cleanup and verifies process death before artifact finalization.” | Deterministic Docker tests cover direct, tool-child, grandchild, cancellation, and post-finalization no-growth behavior. The paid v0.6.2 campaign was not rerun; non-Docker providers are outside this claim. |
| Bounded subagent delegation | **VERIFIED** | “Ygg supports bounded subagent delegation through an optional extension.” | Capability only. Accuracy, cost, throughput, or scaling advantage from multi-agent use remains unsupported. |
| “Tiny” as a comparative metric | **PRELIMINARY** | “The frozen benchmark executable is 25,239,440 bytes.” | Binary size is objective; “tiny” has no published threshold or matched installed/runtime-footprint comparison. |
| Broad local-model onboarding quality | **PRELIMINARY** | “Local OpenAI-compatible endpoints are first-class configuration targets.” | Capability is verified; independent onboarding across llama.cpp, vLLM, Ollama, SGLang, and LM Studio has not been completed. |
| Full public trajectory archive | **UNDER DEVELOPMENT** | “Compact score/audit metadata and checksums are public.” | The full campaign/judge trees remain outside Git pending privacy and redistribution review; the compact package cannot support fresh trajectory-by-trajectory adjudication. |
| Same-model 1/5/10/20 throughput superiority | **UNSUPPORTED** | No public superiority claim. | No controlled concurrent same-endpoint comparison exists. |
| Fastest, lowest-memory, or lowest-energy agent | **UNSUPPORTED** | No public claim. | No matched end-to-end latency, RSS/PSS, power, or battery dataset exists. |
| Universal Pi or MCP compatibility | **UNSUPPORTED** | State only the implemented inventory/extension capabilities. | Ygg does not claim universal Pi fidelity or universal MCP compatibility. |
| Official Terminal-Bench adjudication or leaderboard win | **UNSUPPORTED** | “Local surrogate/manual audit; not maintainer adjudication.” | Terminal-Bench maintainers have not adjudicated the Ygg run. |
| “Never degrades” or unqualified “better” | **UNSUPPORTED** | No public claim. | No experiment can establish universal non-degradation or broad product superiority. |

## Policy

Raw verifier results, local audit adjustments, and official maintainer outcomes
are different scopes and must always be labeled separately. Unsupported claims
do not move into README, release notes, or website copy. A status changes only
when a reproducible artifact satisfies the missing evidence boundary; the claim
wording must remain no broader than that artifact.
