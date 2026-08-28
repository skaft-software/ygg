# Failure report: frozen v0.6.2 control

**Recorded:** 2026-08-28
**Evidence status:** operator-supplied summary; not independently reproduced in
this checkout.

This report classifies the available aggregate evidence without inferring causes
from a score. The frozen control is commit
`61677754bf69833a384bee2b29ef8eff29f37fc1` (`v0.6.2`). Its source and binary
identity are recorded in [baseline-v0.6.2.md](baseline-v0.6.2.md).

## Reported aggregate

The campaign reportedly used Terminal-Bench 2.1, 89 tasks, five attempts per
task, GPT-5.6 Sol, and maximum reasoning. The reported 445 trials partition as
follows:

| Primary class | Trials | Evidence status |
| --- | ---: | --- |
| Raw pass | 391 | Reported aggregate only |
| `verifier_negative` | 34 | Count reported; individual verifier output unavailable |
| `benchmark_timeout` | 19 | Count reported; deadlines and last process state unavailable |
| `provider_failure` | 1 | Count reported; provider logs unavailable |
| **Total** | **445** | **391 / 445 raw passes (87.87%)** |

The report also states that 88 of 89 tasks were solved at least once and gives a
raw Pass@5 of 98.88%. Those are campaign-level figures, not evidence that any
particular failed trial belongs to a specific class beyond the aggregate labels
above.

## What cannot be assigned yet

No task-level rows, Harbor job bundle, trajectories, verifier output, timeout
configuration, provider request IDs, or environment digest are attached here.
Therefore this artifact does **not** assign any verifier negative to a model,
tool, timeout, context, or provider cause. It also does not count the following
separately observed reliability incident as one of the 445 trials:

- provider code: `websocket_connection_limit_reached`
- phase: response body
- detail: Responses WebSocket connection limit reached after 60 minutes; the
  provider requested a new WebSocket connection

The candidate now retires that poisoned preferred WebSocket and allows a bounded
pre-generation retry through the HTTP fallback. The incident remains separate
from benchmark scoring until a raw run bundle establishes whether it occurred in
the control campaign.

## Integrity audit reported separately

A separate GLM-5.3 Flash audit reportedly found four confirmed reward-hacking
successes, two ambiguous Git-history cases, and zero harness-cheating cases or
refusals. The corresponding unofficial adjusted totals were reported as 387/445
after confirmed exclusions and 385/445 after also excluding the ambiguous cases.
These are audit figures, not an official verifier replacement; the raw audit
materials and reviewer decisions are still required.

## Required evidence for a dated revision

Attach raw per-trial results, trajectories, verifier output, process deadlines,
provider/server logs, exact Harbor and dataset revisions, and the environment
identity. Then classify each non-pass using the primary taxonomy in
[README.md](README.md#failure-taxonomy), retain secondary causes separately, and
publish official and audited totals side by side. Never convert missing evidence
into a model-failure explanation.
