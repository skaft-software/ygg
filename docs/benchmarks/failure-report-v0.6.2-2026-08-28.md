# Failure report: frozen v0.6.2 control

**Recorded:** 2026-08-28

**Evidence status:** reconciled against the retained Harbor result, audit
metadata, and native Ygg sessions.

The frozen control is commit
`61677754bf69833a384bee2b29ef8eff29f37fc1`. Canonical identities and checksums
are in the [evidence report](tb21-v0.6.2/README.md).

## Outcomes use two axes

The campaign ran 445 trials. Harbor process status and verifier reward overlap:

| Axis | Outcome | Trials |
| --- | --- | ---: |
| Verifier | reward `1` | 391 |
| Verifier | reward `0` | 53 |
| Verifier | null/no score | 1 |
| Process | normal completion | 425 |
| Process | `AgentTimeoutError` | 19 |
| Process | `YggProviderError` | 1 |

Five of the 19 timed-out trials received reward `1`; 14 received reward `0`.
The provider-error trial is the null/no-score result. Consequently, “391 pass +
19 timeout + 34 ordinary failure + one provider failure” is not a valid disjoint
partition. There are 39 verifier negatives outside the 14 failed timeout trials.

The raw score remains 391/445 (`87.87%`) because it records Harbor verifier
outcomes before integrity exclusions. A timeout with a passing verifier is not
silently converted to a failure, but its process race remains a reliability
limitation.

## Historical timeout race

The v0.6.2 adapter did not authoritatively kill the complete Ygg process group
before Harbor finalized artifacts. Ten timed-out executions later completed 48
model requests containing 6,923,867 provider-input tokens. All ten overlapped
verifier execution; seven completed at least one request after their verifier
finished. This created a possible workspace/verifier race and made the
Harbor-finalized token total incomplete.

Complete native input exceeds Harbor-finalized input by 6,971,387 tokens:
6,923,867 from post-finalization timeout work plus one 47,520-token conversion
omission in a completed trial. These are accounting/cancellation defects, not
speculative recovered task scores.

The v0.6.3 Docker adapter now creates an independently cleanable process group,
performs TERM→KILL cleanup, verifies group death, and finalizes artifacts only
after successful cleanup. Deterministic Docker tests cover TERM-resistant direct,
tool-child, and grandchild processes plus a no-growth verifier window. The paid
v0.6.2 campaign was not rerun, and non-Docker providers are outside that proof.

## Integrity audit

A separate GLM-5.3 Flash surrogate rubric audit reviewed all 391 raw successes;
manual review then resolved its seven flags:

- four confirmed reward hacks;
- two uncertain Git-history cases;
- one flag overturned to clean;
- zero harness-level cheating cases;
- zero refusals or invalid audit runs.

That yields 387/445 (`86.97%`) under the primary confirmed-only policy and
385/445 (`86.52%`) under the strict sensitivity policy. Both policies have
87/89 (`97.75%`) Pass@5. These are local audit figures, **not official
Terminal-Bench maintainer adjudication**.

## What remains unassigned

A verifier negative alone does not establish a model, provider, tool, context,
or agent root cause. This report therefore does not relabel the 39 ordinary
non-timeout verifier negatives without trajectory-specific evidence. Likewise,
the contamination audit found no Ygg source/runtime/harness contamination but
cannot inspect the provider model's pretraining corpus.

For the complete score policy, four confirmed cases, two uncertain cases, token
equations, original artifact hashes, and reproduction steps, use the
[evidence package](tb21-v0.6.2/README.md).
