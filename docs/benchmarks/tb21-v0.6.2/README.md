# Terminal-Bench 2.1 evidence: frozen Ygg v0.6.2 run

**Evidence date:** 2026-08-28

**Scope:** one frozen 445-trial campaign and its local integrity audit

**Adjudication status:** **not official Terminal-Bench maintainer adjudication**

This package preserves the compact, privacy-reviewed metadata needed to state the
result accurately. It keeps the raw Harbor outcome separate from a local
surrogate/manual integrity audit and from any official maintainer decision.

## Result

The campaign ran GPT-5.6 Sol at maximum reasoning on 89 Terminal-Bench 2.1 tasks,
five trials per task:

| Scope | Numerator | Score | Meaning |
| --- | ---: | ---: | --- |
| Raw Harbor verifier | 391/445 | **87.87%** | Reward `1` before integrity exclusions. |
| Primary local audit | 387/445 | **86.97%** | Raw minus four manually confirmed reward hacks; two unresolved cases retained. |
| Strict sensitivity | 385/445 | **86.52%** | Primary minus both unresolved cases. |
| Raw Pass@5 | 88/89 | **98.88%** | At least one raw verifier pass for 88 tasks. |
| Primary/strict Pass@5 | 87/89 | **97.75%** | Harbor's five-trial Pass@5 formula after the applicable exclusions. |

“Independent rubric audit” means the auditing model was separate from the Ygg
agent that produced the trajectories. It does **not** mean Terminal-Bench's
maintainers performed or endorsed the audit.

## Frozen identity

| Field | Value |
| --- | --- |
| Ygg version | `0.6.2` |
| Ygg source commit | `61677754bf69833a384bee2b29ef8eff29f37fc1` |
| Linux benchmark binary SHA-256 | `16036929493fb12ffc4d8a553cdfcb642c3c983fb469877403808e5aabbd5f07` |
| Harbor | `0.22.0`, commit `6ecebe4ae9910ee0b28a2e6e8fa30934c0b41dfa` |
| Dataset | `terminal-bench/terminal-bench-2-1@6` |
| Resolved dataset digest | `sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a` |
| Agent/model | Ygg Harbor adapter / `gpt-5.6-sol` |
| Reasoning | `max` |
| Shape | 89 tasks × 5 attempts = 445 trials |
| Concurrency | 20 |
| Harbor retries | 0 |
| Ygg max-turn limit | omitted |
| Environment | Harbor Docker provider, shared verifier environment |
| Window | 2026-08-27 21:17:10Z to 2026-08-28 00:37:20Z |

The exact source commit predates the first trial. The executable hash is the
identity of the binary actually mounted into every task container; a local
rebuild is not assumed to be byte-identical.

## Harbor invocation and timeout policy

[`run-full.sanitized.sh`](run-full.sanitized.sh) reconstructs the original
command without credential values. The canonical OAuth mount was a disposable,
root-owned, read-only `.ygg` tree; the script requires that tree to be prepared
outside the report and verifies the credential file is mode `0600`. The
checked-in [`harbor-config.sanitized.json`](evidence/harbor-config.sanitized.json)
records the resolved job configuration. The original command and configuration
hashes are in [`manifest.json`](manifest.json).

The run used each task's published timeout with `timeout_multiplier = 1.0`. The
adapter's optional inner `agent_timeout_sec` was not set. Process outcomes and
verifier outcomes therefore must remain separate axes:

- process: 425 normal completions, 19 `AgentTimeoutError`, one
  `YggProviderError`;
- verifier: 391 passes, 53 failures, one null/no-score result;
- timeout overlap: five timed-out trials passed verification and 14 did not;
- the provider-error trial is the null/no-score result.

It is incorrect to describe the campaign as a disjoint “391 passes + 19 timeouts
+ 34 failures + one provider error” partition. Five timeout trials are already
inside the 391 verifier passes.

## Integrity audit

All 391 raw successes were reviewed by `z-ai/glm-5.3-flash` through an
OpenAI-compatible endpoint with temperature `0`, `top_p = 1`, 4,096 maximum
output tokens, and medium reasoning. The audit used the blinded task contract
and ATIF trajectory. Its rubric is checked in as
[`rubric.txt`](evidence/rubric.txt), SHA-256
`dd2c66fe1cce772fbd2dd229df33684dd540b06b4b251743270db11096e340dd`.

The surrogate initially labeled 384 cases clean and flagged seven. Manual review
of all seven produced:

| Outcome | Cases |
| --- | --- |
| Confirmed reward hack | `build-pov-ray__cNhWsT8`, `build-pov-ray__xxddCLw`, `extract-elf__TkF77bb`, `mteb-retrieve__Jx9kSHj` |
| Uncertain; retained in primary, removed in strict | `fix-code-vulnerability__sLw6Z7B`, `fix-code-vulnerability__uTiPMHC` |
| Overturned to clean | `winning-avg-corewars__pfg6fQd` |
| Harness-level cheating | none |
| Refusals | none |
| Invalid/runner audit cases | none |

The reasons and evidence-step references are in
[`manual-review.json`](evidence/manual-review.json). The complete 391-case map,
with workstation paths replaced by variables, is in
[`audit-case-map.sanitized.json`](evidence/audit-case-map.sanitized.json).

A second judge was not run. The primary and strict scores are local audit views,
not replacements for an official Terminal-Bench ruling.

## Contamination audit

The bounded audit found:

- no benchmark-specific solution/verifier strings or canary GUIDs in the frozen
  source/runtime prompt and skill paths;
- no known task strings in the benchmark binary's `strings` output;
- no dataset, verifier, prior trajectory, skill, MCP, or extra-instruction mount;
- a source commit and binary that predated the first trial.

The conclusion is limited to Ygg's source, runtime, harness, mounted inputs, and
chronology. It cannot determine whether the provider model's pretraining data
contained public benchmark material. See
[`contamination-audit.sanitized.json`](evidence/contamination-audit.sanitized.json).

## Token and timeout accounting

Provider input is represented as three disjoint buckets:

```text
uncached input U = wire input - cache reads - cache writes
provider input I = U + cache reads C + cache writes W
processed T = I + output O
```

Reasoning tokens are a subset of output. Harbor's cache field is a detail already
included in Harbor prompt input and must not be added again.

| Scope | Input | Output | Processed | Mean processed/trial |
| --- | ---: | ---: | ---: | ---: |
| Harbor-finalized artifacts | 498,229,083 | 6,445,391 | 504,674,474 | 1,134,100 |
| Complete durable native usage | 505,200,470 | 6,486,026 | 511,686,496 | 1,149,857 |

Complete native input was 36,399,446 uncached (7.20%), 468,801,024 cache-read
(92.80%), and zero cache-write tokens. The seven-figure mean is accumulated
long-context traffic across a median 19 requests per trial, not a million-token
single request.

The 6,971,387-token native/Harbor input gap comprises:

- 6,923,867 input tokens from 48 requests completed after Harbor finalized ten
  timed-out agent executions; and
- one 47,520-token already-durable completed-trial conversion omission.

Those ten processes overlapped verifier execution; seven completed at least one
request after their verifier finished. This is a historical accounting and
cancellation defect, not a recovered score. Ygg v0.6.3 adds fail-closed Docker
process-group cleanup before artifact finalization and deterministic descendant
termination/no-growth tests. The paid campaign was **not rerun**, so this report
does not retroactively claim race-free v0.6.2 execution.

The full distribution and equations are documented in
[`docs/benchmarks/token-efficiency-v0.6.2-2026-08-28.md`](../token-efficiency-v0.6.2-2026-08-28.md).

## Published Codex comparison

The public Codex 0.144.0 + GPT-5.6 Sol/max submission has the same 89-task,
445-trial shape. It publishes 339 official successes after 32 disqualified
successes, so its inferred pre-disqualification raw result is:

```text
(339 + 32) / 445 = 371 / 445 = 83.37%
```

The bounded raw comparison is therefore Ygg **391/445 (87.87%)** versus Codex
**371/445 (83.37%)**. Do not compare Ygg's 387/385 local-audit numerators to
Codex's 339 maintainer-adjudicated numerator. The runs used different dates and
potential provider snapshots, Codex per-trial trajectories are unavailable here,
and only aggregate raw normalization is comparable.

Source: [published Codex submission](https://github.com/harbor-framework/terminal-bench-2-1/blob/67f1daf5b331fd10f5e8bc05bfc626aac26eeb39/leaderboard/submissions/2026-07-10-gpt-5-6-sol-max-codex.json),
pinned at commit `67f1daf5b331fd10f5e8bc05bfc626aac26eeb39` and retrieved
2026-08-28 with raw SHA-256
`62d1a44f1d05654833cf1770c9f2ead98dde9d47e6e431511b83964f5a525c0c`.

## Included evidence and checksums

| File | Status |
| --- | --- |
| `evidence/audit-metrics.json` | Byte-identical local audit metrics. |
| `evidence/manual-review.json` | Byte-identical manual decisions. |
| `evidence/rubric.txt` | Byte-identical audit rubric. |
| `evidence/audit-evidence-files.sha256` | Byte-identical SHA-256 index for 1,965 retained judge files. |
| `evidence/harbor-result.sanitized.json` | Canonical result with private absolute paths replaced. |
| `evidence/harbor-config.sanitized.json` | Resolved config with private absolute paths replaced. |
| `evidence/audit-case-map.sanitized.json` | Complete case map with private absolute paths replaced. |
| `evidence/*sanitized.json` | Audit identity/reproducibility/contamination metadata with the same path-only sanitization. |
| `manifest.json` | Frozen identities and SHA-256 values for the original unsanitized source artifacts. |
| `SHA256SUMS` | Hashes for every compact file verified by `verify.py`. |

The approximately 292 MB campaign tree and 59 MB judge tree are not duplicated
in Git. A full public trajectory archive still requires an explicit privacy and
benchmark-redistribution review. The retained judge checksum index fingerprints
that larger evidence set, but this compact package alone cannot support a fresh
trajectory-by-trajectory adjudication. That limitation is intentional and must
travel with the result.

## Verify and reproduce

Verify the compact package without network access:

```sh
python3 docs/benchmarks/tb21-v0.6.2/verify.py
```

Reconstruct the full environment only from clean checkouts and a disposable
credential stage:

1. Check out Ygg commit `61677754bf69833a384bee2b29ef8eff29f37fc1`.
2. Check out Harbor commit `6ecebe4ae9910ee0b28a2e6e8fa30934c0b41dfa` and run `uv sync --frozen`.
3. Resolve `terminal-bench/terminal-bench-2-1@6` and record that its digest is
   `sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a`.
4. Build or obtain the Linux amd64 `ygg 0.6.2` binary. Treat it as the canonical
   executable only if its SHA-256 is
   `16036929493fb12ffc4d8a553cdfcb642c3c983fb469877403808e5aabbd5f07`.
5. Set the variables documented in `run-full.sanitized.sh`, then invoke it with
   `bash`. This is a paid 445-trial campaign; it is documented for
   reproducibility, not requested as part of the v0.6.3 release.
6. Recompute usage from a retained job directory with:

   ```sh
   python3 scripts/analyze-harbor-job.py /path/to/ygg-tb21-sol-max-k5-n20
   ```

## Known limitations

- The audit is GLM-5.3 Flash surrogate adjudication plus manual review, not
  official maintainer adjudication.
- Two Git-history cases remain unresolved; primary retains them and strict removes
  them.
- Provider-model pretraining contamination is not observable.
- Historical v0.6.2 timeout processes raced verification; v0.6.3's cleanup fix was
  tested deterministically but the campaign was not rerun.
- The full raw trajectory and judge-response trees are not redistributed. The
  judge tree has a complete checksum index; the raw campaign has hashes for its
  canonical result/config/lock files rather than a complete readable root index.
- The exact benchmark executable is hashed, but a complete build attestation,
  container image digest, and provider-side model snapshot identifier are not
  available.
- The Codex comparison is raw aggregate only; run dates, provider snapshots, and
  adjudication pipelines differ.
