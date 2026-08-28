# Terminal-Bench 2.1 token-efficiency audit: Ygg v0.6.2

**Date:** 2026-08-28
**Control:** Ygg `0.6.2` (`61677754bf69833a384bee2b29ef8eff29f37fc1`)
**Harbor job:** retained canonical `${JOB_ROOT}` on a Linux amd64 Docker host
**Workload:** 89 tasks, 5 attempts, 445 trials, GPT-5.6 Sol, reasoning `max`

## Verdict

**Mixed, but the mean is mostly normal cached long-context processing, not evidence of enormous waste. Confidence: high on accounting and cache composition; medium on behavioral attribution.**

The headline was mislabeled. The reproducible `1.134M` is **total processed tokens, including output**, not input alone:

```text
(498,229,083 Harbor prompt tokens + 6,445,391 output tokens) / 445
= 1,134,100 processed tokens/trial
```

Harbor prompt tokens already include cache hits. Adding the separate Harbor cache field would double-count them. The corresponding retained-artifact input-only mean is `1,119,616`; output is `14,484`.

A post-run audit of every native usage record finds `1,135,282` input and `14,575` output per trial (`1,149,857` total). The 1.38% difference from Harbor input is real unreported work, primarily model requests that kept running after Harbor timed out and finalized ten trials. This is a genuine cancellation/accounting defect, but it is not the explanation for the million-token scale.

Of complete provider-visible input, **92.80% was cache-read and 7.20% was uncached**. A typical trial made 26 requests. Replaying a growing 30–100K logical context over dozens of requests naturally produces a seven-figure processed-token sum without ever holding a million-token context. Median input was only 379K; the mean was pulled up by a long tail.

There is real tail inefficiency: verifier failures used 2.8x the input of passes, timeout failures used 6.5x the input of clean passes, and the highest quintile's raw success rate fell to 76.4% from 95.5% in the lowest quintile. However, exact unchanged repeats were tiny. The dominant avoidable patterns were post-timeout continuation, some continuation after explicit success, and long semantic exploration loops—not accidental cache double-counting or generic exact command loops.

Outcome labels overlap in the retained job. The 445 trials contain 391 verifier
passes, 53 verifier negatives, and one null reward; independently, process
classification is 425 completed, 19 timeout, and one provider failure. Five
timeouts passed the verifier and 14 failed it. Therefore the supplied
“391 pass + 19 timeout + 34 ordinary failure + 1 provider failure” partition is
not the artifact's actual disjoint classification; there are 39 non-timeout
verifier negatives.

## Evidence boundary

The canonical v0.6.2 run predates `ygg.telemetry.v1`. Its native sessions provide exact per-request token buckets and messages/tools, but not TTFT, explicit provider-retry timing, or reliable state-change labels. Consequently:

- token, request, tool, output-size, outcome, and duration figures are measured;
- TTFT and physical retry duplication are unavailable;
- tool-content composition is estimated from context growth and bounded observation bytes;
- no explicit compaction usage record occurred; prompt drops are called **context resets**, not proven compactions;
- the raw Codex comparison is aggregate-only because its per-trial job is unavailable.

## Correct token semantics and equations

### Provider response to Ygg

For OpenAI Responses, the wire fields have overlapping semantics:

```text
R_in       = response.usage.input_tokens                 # total prompt, cache included
R_cache    = input_tokens_details.cached_tokens          # subset of R_in
R_write    = input_tokens_details.cache_write_tokens     # subset of R_in when exposed
R_out      = response.usage.output_tokens                # output, reasoning included
R_reason   = output_tokens_details.reasoning_tokens      # subset of R_out
```

Ygg canonicalizes these into disjoint prompt buckets:

```text
U = max(R_in - R_cache - R_write, 0)  # uncached/standard-rate input
C = R_cache                           # cache reads
W = R_write                           # cache writes
I = U + C + W                         # provider-visible input
O = R_out                             # generated output
T = I + O                             # normalized canonical total
```

`cache_write_1h_tokens` is a subset of `W`, not another additive bucket. `reasoning_tokens` is a subset of `O`, not additional output. Ygg recomputes `T`; it does not preserve an inconsistent provider wire `total_tokens`.

The mapping is implemented in `crates/ygg-ai/src/protocol/openai_responses.rs` and `crates/ygg-ai/src/responses.rs`. A controlled compact fixture maps wire input `120`, cache read `10`, and cache write `5` to canonical `U=105`, `C=10`, `W=5`, so provider input remains `120`. Production records independently validate that each request's canonical total equals `U+C+W+O`.

### Session, telemetry, Harbor, summary

- Native `usage` records persist the disjoint Ygg buckets per physical billable operation.
- A checkpoint stores the cumulative run sum; it must not be added to the individual usage records.
- `ygg.telemetry.v1` emits request buckets for `model_request_finished`, operation buckets for compaction, and cumulative snapshots for candidate/run records. The branch now labels these `usage_scope=request|operation|run_cumulative`.
- Harbor `prompt_tokens` / `n_input_tokens` is `U+C+W`.
- Harbor `cached_tokens` / `n_cache_tokens` is cache-read detail already contained in prompt input. It is **not additive**.
- Benchmark “processed tokens” is Harbor input plus output.

No cache writes occurred in this campaign. There was no `input + cached_input` double-count in the reported 1.134M. Such a mistake would have produced roughly 2.16M per trial, not 1.134M.

The old Harbor conversion could undercount in a different direction: it summed
usage only while attaching metrics to the converted active trajectory and it
omitted cache writes. In this campaign, most of the 6,971,387-token native gap
was written *after* Harbor conversion because timed-out processes kept running;
one completed trial also omitted 47.5K already-durable input. The adapter now
sums every usage operation present at conversion and includes cache-write input,
while the timeout-cancellation defect still needs an authoritative process kill.

## Distribution

### Trial-level complete native input

| Metric | Mean | Median | p75 | p90 | p95 | Maximum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Provider input | 1,135,282 | 379,028 | 1,402,939 | 3,086,097 | 4,964,615 | 15,702,597 |
| Uncached input | 81,797 | 58,809 | 112,095 | 167,460 | 213,518 | 458,248 |
| Cache-read input | 1,053,485 | 316,928 | 1,279,488 | 2,922,035 | 4,664,986 | 15,406,592 |
| Output | 14,575 | 10,875 | 19,522 | 32,128 | 38,291 | 92,041 |
| Requests | 26.15 | 19 | 36 | 54.6 | 67 | 120 |
| Maximum logical context | 50,751 | 36,835 | 75,255 | 111,242 | 130,541 | 219,838 |

Across all requests, the weighted average provider-visible context was `505,200,470 / 11,638 = 43,409` tokens. The mean per-trial maximum logical context was 50.8K. Total processed input was 22.4 times the sum of per-trial maximum contexts because those contexts were replayed over many requests.

### Task-level totals across five attempts

| Metric | Mean | Median | p75 | p90 | p95 | Maximum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Input/task | 5.676M | 2.570M | 7.905M | 14.749M | 21.553M | 38.432M |
| Mean input/trial within task | 1.135M | 514K | 1.581M | 2.950M | 4.311M | 7.686M |
| Uncached input/task | 409K | 332K | 585K | 813K | 955K | 1.453M |

The largest task totals were `extract-moves-from-video` (38.43M), `filter-js-from-html` (37.56M), `make-doom-for-mips` (29.12M), `train-fasttext` (27.40M), and `mailman` (22.93M).

## Composition

### Exact provider buckets

Complete native usage was:

```text
uncached input       36,399,446   7.20% of provider input
cache-read input    468,801,024  92.80% of provider input
cache-write input             0   0.00%
provider input      505,200,470
output                6,486,026   1.27% of total processed
reasoning output      3,990,961  61.53% of output (included, not additive)
```

This is predominantly healthy cached-prefix replay. The cache counters are credible: cached prefixes appear in 1,024-token units and retained request equations reconcile exactly.

For the embedded GPT-5.6 Sol rates used by this run, cache reads were priced at one tenth of uncached input (`$0.50/M` versus `$5/M`). The measured input therefore corresponds to about 187K uncached-price-equivalent input tokens/trial, not 1.135M. This is a pricing equivalence, **not** a claim that cache reads use exactly one tenth the server compute or latency. Canonical TTFT was not recorded, so no precise compute/latency conversion is defensible.

### Context-source estimate

The active trajectories contained 25.97MB of bounded tool observations: 85.2% bash, 11.2% read, and 3.6% edit/write by bytes. A marginal context-growth decomposition attributes the 498.23M retained Harbor input as:

| Gross replay source | Tokens | Share of retained input | Interpretation |
| --- | ---: | ---: | --- |
| First-request fixed/task context replay | 16.26M | 3.3% | System prompt, schemas, task prompt |
| Prior assistant output replay | 129.78M | 26.0% | Upper-bound allocation from generated output |
| Tool-result and other turn growth | 360.19M | 72.3% | Mostly observations plus framing |
| Later context drops | -8.00M | -1.6% | 20 drops in 19 trials |

This estimate uses prompt deltas and is not exact tokenizer attribution. It does establish that the million-token sum is mainly accumulated conversation/tool state replay, not the fixed system prompt.

449 bash observations were truncated at the existing bound and represented 25.2% of stored observation bytes. Their marginal replay allocation was 63.47M tokens, 12.7% of retained input. That is a lever, not proof of waste: build logs, source excerpts, and test failures can be necessary, and Ygg already capped them near 16KB.

### Confirmed or bounded waste

| Cause | Measured amount | Campaign share | Confidence |
| --- | ---: | ---: | --- |
| Post-timeout requests after artifact finalization | 6.92M input | 1.37% | High; ten timeout manifests changed afterward |
| All complete-native vs Harbor input gap | 6.97M input | 1.38% | High; includes one 47.5K completed-trial conversion omission |
| Work after last passing exposed Mailman evaluator | 5.58M model tokens | 1.1% | Medium; later hardening may be intentional |
| Four exact repeated calls with identical output | 198,740 direct request tokens | 0.04% | High |
| Every exact repeated invocation, including legitimate rebuilds, future-replay upper bound | 1.98M input | 0.40% | Upper bound |
| Provider retry/reconnect duplication | Unavailable | — | v0.6.2 did not persist retry lifecycle |
| Explicit compaction calls | 0 | 0% | High; all 11,638 native usage records were assistant turns |

There were 96 exact repeated tool invocations among 11,159 retained calls, only seven consecutive. Only four repeated calls returned identical output. Six repeated `read` calls all returned changed content. Exact rereading/repetition is therefore not a material explanation of the average.

## Representative per-request reconstruction

`crack-7z-hash__iPkS7BW` was a successful 25-request trajectory. It finished with only 31.9K logical context but accumulated 393K input, of which 331.8K was cached. `latency*` is completion timestamp minus the immediately preceding durable user/tool-result timestamp; TTFT was unavailable.

| Req | Context | Uncached | Cached | Output | Reasoning | Latency* | Following tool | Observation bytes | Cumulative input |
| --: | --: | --: | --: | --: | --: | --: | --- | --: | --: |
| 1 | 1,206 | 1,206 | 0 | 64 | 31 | 2,401 ms | bash | 353 | 1,206 |
| 2 | 1,488 | 1,488 | 0 | 69 | 18 | 2,101 ms | bash | 519 | 2,694 |
| 3 | 1,878 | 1,878 | 0 | 107 | 31 | 2,814 ms | bash | 5,995 | 4,572 |
| 4 | 4,189 | 4,189 | 0 | 105 | 24 | 2,767 ms | bash | 338 | 8,761 |
| 5 | 4,547 | 4,547 | 0 | 102 | 30 | 2,720 ms | bash | 537 | 13,308 |
| 6 | 4,954 | 1,114 | 3,840 | 147 | 16 | 3,497 ms | bash | 104 | 18,262 |
| 7 | 5,295 | 1,455 | 3,840 | 152 | 84 | 3,803 ms | bash | 291 | 23,557 |
| 8 | 5,714 | 850 | 4,864 | 60 | 25 | 2,358 ms | bash | 1,107 | 29,271 |
| 9 | 6,490 | 1,626 | 4,864 | 155 | 118 | 3,611 ms | read | 13,461 | 35,761 |
| 10 | 10,792 | 4,904 | 5,888 | 82 | 18 | 2,263 ms | bash | 99 | 46,553 |
| 11 | 11,002 | 1,018 | 9,984 | 80 | 15 | 2,240 ms | bash | 1,834 | 57,555 |
| 12 | 11,753 | 1,769 | 9,984 | 350 | 259 | 7,491 ms | bash | 368 | 69,308 |
| 13 | 12,623 | 1,615 | 11,008 | 206 | 168 | 4,433 ms | read | 7,204 | 81,931 |
| 14 | 15,263 | 11,423 | 3,840 | 341 | 202 | 6,981 ms | bash | 901 | 97,194 |
| 15 | 16,456 | 1,352 | 15,104 | 654 | 592 | 12,573 ms | bash | 6,511 | 113,650 |
| 16 | 19,599 | 3,471 | 16,128 | 35 | 0 | 1,548 ms | read | 5,390 | 133,249 |
| 17 | 21,608 | 2,408 | 19,200 | 367 | 329 | 7,329 ms | read | 16,262 | 154,857 |
| 18 | 26,991 | 5,743 | 21,248 | 36 | 0 | 1,622 ms | read | 2,204 | 181,848 |
| 19 | 27,845 | 1,477 | 26,368 | 240 | 201 | 5,719 ms | bash | 69 | 209,693 |
| 20 | 28,362 | 970 | 27,392 | 612 | 212 | 11,939 ms | bash | 172 | 238,055 |
| 21 | 29,688 | 2,296 | 27,392 | 67 | 12 | 2,583 ms | bash | 825 | 267,743 |
| 22 | 30,102 | 662 | 29,440 | 630 | 326 | 12,417 ms | bash | 313 | 297,845 |
| 23 | 31,518 | 2,078 | 29,440 | 68 | 38 | 2,580 ms | write | 160 | 329,363 |
| 24 | 31,734 | 1,270 | 30,464 | 53 | 8 | 2,011 ms | bash | 78 | 361,097 |
| 25 | 31,877 | 389 | 31,488 | 17 | 0 | 1,258 ms | — | 0 | 392,974 |

This is the central distinction:

- **Logical context:** 31,877 tokens on the final request.
- **Incremental logical information:** roughly the growth from 1,206 to 31,877, not 393K.
- **Provider-processed input:** the sum of all 25 contexts, 392,974.
- **Uncached provider input:** 61,198; this is a billing/cache bucket, not purely new prose.

## Relationship to success, steps, and timeouts

| Class | Trials | Mean input | Median input | Mean requests | Mean max context | Mean wall time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| All verifier passes | 391 | 936K | 314K | 24.1 | 46.9K | 373s |
| All verifier failures | 53 | 2.626M | 1.042M | 41.6 | 80.2K | 749s |
| Completed pass | 386 | 898K | 291K | 23.7 | 45.9K | 366s |
| Completed verifier fail | 39 | 1.472M | 744K | 30.0 | 63.3K | 510s |
| Timeout that passed verifier | 5 | 3.866M | 3.105M | 53.6 | 118.3K | 900s |
| Timeout that failed verifier | 14 | 5.844M | 5.742M | 74.1 | 127.0K | 1,414s |

Input correlated strongly with requests (`r=0.913`), tool calls (`0.913`), maximum context (`0.893`), and duration (`0.788`). Correlation is not causation: harder tasks demand more work and fail more often. Still, the top tail is not benign in aggregate.

Raw success by equal-sized input quintile fell from 95.5%, 93.3%, 88.8%, and 85.4% to 76.4% in the top quintile. Sixteen of 19 timeouts were in that top quintile. High token use did not predict higher success.

No explicit compaction requests occurred, so zero- versus high-compaction comparison is unavailable. Eight large prompt resets had no exact tool-call rediscovery in the next five calls. There is no evidence supporting more aggressive compaction for this run.

## Efficiency units

Using complete native usage and 391 raw passes:

| Unit | Value |
| --- | ---: |
| Input in a successful trajectory | 936K average |
| Campaign input per raw success, including failed spend | 1.292M |
| Campaign uncached input per raw success | 93.1K |
| Campaign cached input per raw success | 1.199M |
| Campaign output per raw success | 16.6K |
| Campaign requests per raw success | 29.8 |
| Total processed tokens per raw success | 1.309M |
| Input per retained tool call | 45.3K |
| Input per aggregate agent-minute | 163K |
| Input per task solved at least once (88 tasks) | 5.741M |

“Processed tokens per productive state change” cannot be calculated honestly from v0.6.2: bash state changes were not classified. `ygg.telemetry.v1` now records conservative built-in state-change and no-progress signals for future runs.

## Outliers

1. **`filter-js-from-html__nB3CrKG` — 15.70M input, timeout/fail, 109 requests, 219.8K max context.** The trajectory built 14 temporary browser programs, repeatedly probed Selenium/fuzz behavior, and continued 23 turns after a “verifying file integrity and cleaning” point. Those 23 retained turns consumed 4.27M tokens. More importantly, cancellation failed: 2.93M input was generated after Harbor finalized the trajectory/manifest.
2. **`mailman__WsoBcJS` — 9.50M input, pass, 89 requests, 188.8K max context.** This is a legitimate long successful trajectory, but it had 20 >=10K observations and used 1.12M model tokens after its last passing exposed evaluator run. Across all five Mailman passes, post-last-pass work was 5.58M tokens.
3. **`filter-js-from-html__QgSycLM` — 9.35M input, completed verifier failure, 80 requests.** It had no exact repeated-call loop; the waste was semantic exploration that did not converge.
4. **`extract-moves-from-video__admuG9J` — 9.30M input, timeout/fail, 110 requests.** Three separate five-image read sequences alone accounted for 1.78M request tokens. Some of this is necessary multimodal inspection, so it is not classified wholesale as waste.
5. **`extract-moves-from-video__gz7rnDV` — 9.06M input, timeout/fail, 99 requests.** Another long high-context unsuccessful trajectory.

The maximum successful input was 9.50M. Therefore a blanket limit below the outliers would remove demonstrated capability.

## Codex + Sol comparison

The public Codex 0.144.0 + GPT-5.6 Sol/max submission reports 25,266,704 uncached input, 540,552,541 cached input, and 5,935,747 output across the same 445-trial/89-task shape. Its published processed total is:

```text
(25,266,704 + 540,552,541 + 5,935,747) / 445
= 1,284,843 tokens/trial
```

This is semantically comparable to Ygg's reported Harbor total, not to fresh input alone.

| Aggregate | Ygg reported | Ygg complete native | Codex published |
| --- | ---: | ---: | ---: |
| Processed tokens/trial | 1.134M | 1.150M | 1.285M |
| Raw passes | 391 (87.87%) | same | 371 (83.37%, inferred before 32 disqualifications) |
| Processed tokens/raw success | 1.291M | 1.309M | 1.541M |
| Cache share of input | 92.72% | 92.80% | 95.53% |
| Mean duration | 458.2s full / 417.1s agent | same | 431.0s full |

On reported aggregates, Ygg used **16.2% fewer processed tokens per raw success**; including Ygg's post-timeout native usage, it used **15.1% fewer**. Ygg also had a 4.5-point higher raw pass rate.

Do not overstate this. Codex per-trial trajectories/distributions are unavailable, its published run predates Ygg's, exact provider snapshots may differ, missing usage on errored trials would be counted as zero, and Codex received an official 32-trial reward-hack adjudication while Ygg's separate local audit had only four confirmed exclusions and two ambiguous cases. The raw token normalization is apples-to-apples; adjudicated efficiency is not.

Public Codex aggregate (pinned): <https://github.com/harbor-framework/terminal-bench-2-1/blob/67f1daf5b331fd10f5e8bc05bfc626aac26eeb39/leaderboard/submissions/2026-07-10-gpt-5-6-sol-max-codex.json>.

## Ranked real efficiency problems and recommendations

### 1. Stop work at the benchmark deadline

- **Evidence:** ten timed-out trials produced 6.92M input after Harbor finalized artifacts; 11 manifests drifted by 359KB.
- **Expected reduction:** 1.37% campaign input in this run; potentially more wall/provider occupancy under concurrency.
- **Latency benefit:** no completed-trial latency change; faster resource release after timeouts.
- **Risk:** low if termination happens only at the authoritative deadline.
- **Test:** cancellation integration test must prove the process group exits, session manifest remains immutable, and no provider/tool event appears after finalization. Configure an inner deadline below Harbor's outer cancellation until the environment can kill the command authoritatively.

### 2. Add a conservative explicit-success stop signal

- **Evidence:** 5.58M tokens (1.1% of campaign processing) followed the last passing exposed Mailman evaluator across five passes.
- **Expected reduction:** about 1% on this campaign; task-dependent.
- **Latency benefit:** proportional to removed post-pass turns.
- **Risk:** medium. A visible test can be incomplete or flaky; automatic stopping can reduce robustness.
- **Test:** A/B only on tasks with an authoritative, repeatable success command; require clean state plus a fresh pass, and compare verifier score before generalization.

### 3. Make large observations recoverable, then decay selectively

- **Evidence:** tool/other growth drove about 72% of replay; truncated observations alone had a 12.7% marginal replay allocation. Current output is already bounded, so “add a cap” is not the answer.
- **Expected reduction:** a global 16KB-to-8KB cut has a rough upper lever near 6%, but is not recommended. Recoverable spill plus aging may safely capture 3–6%; this requires an experiment.
- **Latency benefit:** fewer cached-prefix tokens on later turns; exact TTFT benefit unknown.
- **Risk:** medium without recovery, low-to-medium with a full-output artifact and model-visible pointer.
- **Test:** compare current head/tail output with artifact-backed truncation and N-turn decay on the high-output task subset; gate on unchanged pass rate and fewer reruns.

### 4. Detect semantic no-progress; do not block all repeats

- **Evidence:** exact repeats were at most 0.4% of input and most changed state/output. The clear waste was long non-convergent exploration in the filter task.
- **Expected reduction:** <0.1% from exact duplicate blocking alone; potentially larger from a well-calibrated no-progress intervention, currently unquantified.
- **Latency benefit:** small for exact duplicates.
- **Risk:** high if repeated builds/tests are blocked; many are necessary.
- **Test:** use telemetry's argument hash, state-change signal, output hash/size, and no-progress streak. Intervene only after identical calls with identical output and no state change; replay the known four cases and a flaky-retry control.

### Not recommended from this evidence

- Do not lower the context window or impose a low global turn/token cap: 9.5M-input successful trajectories exist.
- Do not aggressively summarize or compact: there were zero explicit compaction calls and no proven post-reset rediscovery.
- Do not globally suppress reasoning: reasoning was 61.5% of output but only 0.78% of total processed tokens; capability risk is unmeasured.
- Do not optimize the fixed prompt first: its gross replay allocation was only 3.3%.
- Do not add cache detail to prompt totals: that would create the double-counting bug this audit ruled out.

## Controlled checks and changes from this audit

Observed checks:

- OpenAI Responses usage fixture passed, including cache-detail normalization.
- Native Responses compact fixture passed (`120 = 105 uncached + 10 read + 5 write`).
- Telemetry unit tests passed with disjoint provider input and explicit usage scopes.
- Retry integration test passed with two `TurnStarted` events for two physical attempts.
- Harbor session tests passed with complete-run usage, inactive-branch accounting, cache-write inclusion, and active-branch-only presentation.
- `scripts/analyze-harbor-job.py` reproduced all 445 remote trials, 505,200,470 native input tokens, the 6,971,387 Harbor gap, and 11 drifting manifests.

Generic fixes on `mission/v0.6.3-next`:

1. Harbor run totals now include every durable usage operation and cache writes; cache hits remain a non-additive subset.
2. Telemetry now labels usage scope.
3. A retried provider request receives its own timing/TTFT lifecycle after backoff.
4. The systems telemetry parser skips a malformed line without dropping later valid records.
5. Documentation now calls `total_tokens` Ygg-normalized rather than provider-preserved.
6. A standard-library Harbor job analysis script provides reproducible distributions and reconciliation.

## Plain-English answer

**Should I actually be worried about Ygg using ~1.1M processed tokens per Terminal-Bench trial?**

**No—not about the 1.1M average itself.** It is a cumulative long-horizon traffic counter, 93% cache replay, and it is lower than the matched Codex aggregate. Ygg is not secretly putting 1.1M tokens in one context, and the metric was not doubled by adding cached input twice.

**You should worry about the high-token tail.** Timeouts and non-convergent failures consume several times the tokens of successes, cancellation leaked another 1.37% after deadlines, and a few trajectories continued after strong success evidence. Fix those targeted behaviors. Do not shrink context or reasoning merely to make the headline number look smaller.
