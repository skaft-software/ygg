# Ygg Token Economics — Complete Report
### Original findings + 6-week mother-session audit + full `pi` architectural comparison

Date: 2026-08-26 · Scope: repo `skaft-software/ygg`, live session telemetry (`~/.ygg/sessions`, `~/.pi/agent/sessions`), code-level comparison against `earendil-works/pi`
Companion to `notes/token-cost-analysis.md` (original Ygg-only analysis); this document supersedes it and corrects three of its estimates.

---

## 0. Verdict

The suspicion that Ygg has a serious token-cost inefficiency relative to its pi-derived architecture is **confirmed — with nuance**:

- Ygg's compaction **core is at parity with pi** (same 20k keep window, usage-based token accounting, split/oversized-turn handoff — the code literally says "ported from pi").
- **Ygg is objectively more expensive in five specific, code-verified ways**: (1) lossy tool-result truncation with no recovery path; (2) no escape hatch for reasoning replay — every historical reasoning block is re-sent in full, on both protocols; (3) the deferred-tools port of pi's cache-breakpoint mechanism is dormant (no model enables it) and missing from the Anthropic adapter; (4) compaction summaries drop the structured file-operations ledger that pi preserves; (5) a fixed 85% threshold plus compaction churn the pi design avoids — measured as **1,253 always-cold compaction runs on a paid model ($284.61 over 6 weeks) with zero persisted boundaries**.
- **Ygg is tighter in three ways**: 16KB default tool-result cap (pi: 50KB), deeper subagent integration + per-request cost telemetry, and a leaner fixed prefix.
- **The single biggest cost driver is shared by both systems**: nothing decays stale reasoning or tool results between compactions. Measured: **83–90% of Ygg's live context and 96.7% of pi's flagship post-compaction context is process byproduct** (reasoning + tool results + calls), with ~1–2% being actual user conversation. Cutting byproduct ~40% **doubles the effective intelligence** of the same window.

Headline numbers (all measured from live telemetry, not estimates):

| | **pi fleet** (120 sessions, Jul 10–Aug 23) | **Ygg mother session** (07-13 → 08-26, root) |
|---|---|---|
| Requests | 16,838 | 76,689 (+ 1,253 compactions) |
| Prompt tokens | 2.581B (97.2% cache-read) | 8.711B (96.7% cache-read) |
| Output tokens | 6.83M (**45% reasoning**) | 25.93M (**44.5% reasoning**) |
| Avg prompt / request | 153k | 113.8k (max single: 724,023) |
| Billed (their prices) | **$1,174** | **$5,635.93** (incl. $284.61 pure compaction) |

Cache economics are statistically indistinguishable (97.2% vs 96.7% cached). The divergence is in **churn**: compaction frequency, cold-cache re-billing, and what never gets pruned.

---

## 1. Original Ygg findings (tied back)

### 1.1 Fixed prefix (constant input cost)
- Subagent cold start measured at **3,176 tokens** (persona + 5 core tool schemas + profile).
- Core 5 tool schemas: ~9.4KB raw ≈ **2.4–2.9k tokens**; browser extension adds ~1.2–1.5k; live main-session prefix **4,713–6,489 tokens**.
- The skill catalog is injected into the system prompt every turn (`crates/ygg-coding-agent/src/resources.rs`).
- Verdict: real but small — a few % of context; not the problem.

### 1.2 Live context composition (session 61405, post-compaction, 93k tokens / 400KB)
| class | share |
|---|---|
| Reasoning (replayed) | **46%** |
| Tool results | **37%** |
| Tool calls | **7%** |
| Conversation text | **1.5%** |

**83–90% of live context is process byproduct.** User words: ~2%.

### 1.3 Output side
Post-compaction assistant output is **~85% reasoning** (1.5–7k tokens/turn at max effort). Reasoning is the dominant output cost driver on every session measured.

### 1.4 Fleet telemetry (35 delegation teams, Aug 17–26)
~11.5k requests; **1.13B prompt tokens (96.7% cache-read)**; 3.64M output (39% reasoning historically, **73–83% in recent teams**). Avg 98k prompt/request. Largest team (Aug 20): 733M cache-read tokens. 21-agent swarm: 846 requests, 81.4M prompt (95% cached), 215k output (73% reasoning).

### 1.5 Cache-miss storms (Jul 20 window)
Local endpoint KV cache evicts after ~2–3 min idle. The `context-cost` subagent hit full-price billing on **10 of 30 requests** (594,733 fresh tokens = 72% of its input). Main session: 3 storms of ~56k tokens each.

### 1.6 Compaction mechanics
- Defaults: threshold = **85% of context window** (`ygg-coding-agent/src/config.rs:377-382`), keep most-recent **20,000 tokens**.
- Trigger runs inside the autonomous loop after every tool result via `CompactionContext::ensure_capacity` (`ygg-agent/src/agent.rs:4590-4620`).
- **One-request lag**: compaction entry written 15:38:50, truncation effective on the *next* request (15:40); intervening requests paid the full pre-compaction context (~91–93k).
- System prompt + tool schemas survive compaction intact (re-processed unless cache covers them).
- Subagents fork from the **post-compaction** context (summary + recent window) → ~27k cold start vs the parent's un-truncated 91k.

### 1.7 The "1.58M token" mystery — **corrected**
A usage record showed a 1.58M-token "context" (825k fresh + 757k cached). It was an **aggregation artifact**: the sum of 30 subagent request usages, not one context. Confirmed by the mother-session audit (§2): no single request there ever exceeded **724,023 tokens**.

### 1.8 OpenAI-protocol reasoning replay
`ygg-ai/src/protocol/openai_chat.rs:621-625`: `AssistantPart::Reasoning` is converted to `AssistantPart::Text` and re-sent on **every** request (workaround for llama.cpp `content: null` errors). All historical reasoning is therefore resent as plain text, indefinitely, on OpenAI-protocol endpoints.

### 1.9 Original ranked strategies
#1 prune stale reasoning (−45% context) · #2 decay tool results (−25–30%) · #3 adaptive reasoning effort (−50–70% output) · #4 tighten compaction backstop · #5 edit-by-anchor · #6 cache-cold UX · #7 trim prefix.

**Status after this audit:** the ranking holds, but strategies #1–#4 must now be re-scoped against what pi actually does (§3) and against the 6-week billing data below, which shows where the money actually went.

---

## 2. New: 6-week mother-session audit (`c4c8e202815bca65`)

Session span **2026-07-13 → 2026-08-26**: 1,425 JSONL files, ~690MB, 189,392 entries, 77,873 usage records. This is the user's long-lived dogfooding session (Ygg developer running nightly builds).

### 2.1 Scale and billing by record kind
| kind | n | fresh input | cache-read | output | reasoning | billed | avg/run |
|---|---|---|---|---|---|---|---|
| `assistant_turn` (root) | 76,689 | 292.7M | 8.418B (96.7%) | 25.93M | 11.55M (44.5%) | **$5,251.51** | $0.0685 |
| `compaction` | **1,253** | 47.67M (**all cold**) | 0 | 4.41M | 0.98M | **$284.61** | $0.2271 |
| `delegated_agent` (mirrored) | 123 | 12.4M | 273M | 1.01M | 0.46M | $99.81 | $0.8115 |
| `terminal_gate` | 10 | 8.7k | 2k | 10 | 0 | $0.00 | — |

**Session total billed: $5,635.93.** Pure compaction housekeeping = **5.05% of spend for zero user-facing work**.

Model mix (paid era): `gpt-5.6-sol` 47,019 runs / **$5,006.15**; `gpt-5.6-luna` 12,378 / $369.94; `gpt-5.6-terra` 2,182 / $91.43; `gpt-5.3-codex-spark` 4,852 / $87.36; openrouter long tail ~$45; late-session local models (`qwen38-gptq-mtp4-stable` xhigh, `ox-alpha` via openrouter, `apple-fm`, free tiers) = **$0** — the user switched to local models in the final week.

### 2.2 The compaction anomaly (new, significant)
- **1,253 compaction runs** in a session whose *persisted* max context was **724,023 tokens** — far below the 85% × 2M-window threshold (1.7M) that governs its main models.
- Compaction input distribution: **min 3 / median 37,348 / max 246,872 tokens** — the median says *typical* compactions ran on ~37k-token contexts, i.e., well below any window's 85% line.
- **Zero persisted compaction boundary entries** (no `type: "compaction"` records at all — only usage records and 1,991 resume checkpoints). Whatever truncated the context left no `first_kept` boundary in the log, so post-compaction reconstruction from the persisted log is not verifiable.
- Every compaction request is **cache-cold by construction** (the compactor sees a novel context) — 47.67M full-price input tokens and $284.61 over 6 weeks, billed on the **paid** compactor endpoint (`gpt-5.6-sol` via `openai-codex`) even while the main model was local.
- Candidate explanations (not fully disambiguable from the log, which is itself the problem): (a) mid-session model switches re-baseline the threshold against the *active* model's window (44 distinct models appear in the session); (b) the trigger basis is stale/different from the documented 85%; (c) boundaries were lost to file rotation. **All three reduce to: the compaction subsystem churns far more than the pi design intends, is not observable after the fact, and is billed at full price every time.**

### 2.3 Cold-cache share (30-day rollup of the session)
- **1,783 of 76,626** root requests (2.3%) were cache-cold (`cache_read = 0`) and absorbed **93.6M fresh tokens = 32% of all root fresh input**.
- Adding the always-cold compaction runs (47.7M): **≈40% of every fresh input token in this session was billed at full (uncached) price.**

### 2.4 Tool-result cap — as actually run (corrects the "no cap" assumption)
Ygg *does* cap tool results: `sandbox.max_output_bytes`, **default 16KB** (`crates/ygg-agent/src/sandbox.rs:74`), applied via `lower_tool_result` (`agent.rs:1665+`) with head/tail retention and a `[tool output truncated]` marker (`truncate_tool_text`, `agent.rs:1490-1515`); the comment at `agent.rs:5735-5738` confirms all tools share the cap.

The log shows exactly what that means in practice:
- On **most days the max persisted tool-result entry is 16–18KB** — the 16KB cap signature, clearly active.
- On **9 days the cap was effectively much looser**: max entries of 121KB (08-10), 186KB (08-11), 191KB (08-07), **448KB (08-20)**, 301KB (08-23), 213KB (08-25), 226KB (07-31), 1.38MB (07-25), and **5.54MB in a single tool result on 07-26**. Consistent with a dogfooded binary whose `max_output_bytes` was raised for heavy tasks (and not reverted), or cap logic varying across nightly builds.
- **Zero media parts** across all 189,392 entries → the 724k context peak is pure text; media inflation is ruled out.

So the corrected statement is: *Ygg's default cap is stricter than pi's 50KB, but the cap is (a) lossy — truncated bytes are gone, and (b) config-labile — in real use it was exceeded/loosened on the busiest days, and nothing ever decays what was kept.*

### 2.5 Other Ygg root sessions in the window
| session | period | root reqs | prompt | cached | output | reasoning | max ctx |
|---|---|---|---|---|---|---|---|
| `43769f70c3a939ad` | Aug 19 | 166 | 56.9M | 98.9% | 66.9k | 67% | — |
| `451dab6dfa25c90` | Aug 20 | 39 + 1 compaction | 2.1M | 91% | 46.6k | 55% | — |
| `451ddb6dfa261a9` | Aug 20 | 39 | 1.5M | 93% | 28k | 78% | — |
| `e2590490b9ca47ed` | Aug 21 | 42 | 0.32M | 79% | 16k | 85% | — |
| `57f5e5307bf6638d` | **Aug 23–24 (swarm day)** | 250 + 4 compactions | **18.65M** | **95.8%** | **146.9k** | **17%** | 226,057 |

Note the swarm-day root: only **17% of output was reasoning** — the root spent its output budget *delegating*, not thinking. The 44–70%+ reasoning shares live in the sessions that do the work directly.

---

## 3. Pi comparison — code-level

Provenance: Ygg's `crates/ygg-coding-agent/src/compaction.rs` is documented as **ported from pi (MIT)** — same 20k keep window (`DEFAULT_CONTEXT_TOKENS: number = 20000` in pi's `agent/compaction/index.ts` vs `keep_recent_tokens: 20000`), same usage-based token accounting, same split/oversized-turn prefix summarization, same 5-section summary skeleton.

### 3.1 Mechanism-by-mechanism

| Mechanism | **pi** (code-verified) | **Ygg** (code-verified) | Verdict |
|---|---|---|---|
| Tool-result ingestion cap | **50KB / 2000 lines** (`agent/session/tool-result-truncation.ts:12-15`) | **16KB** default (`sandbox.rs:74`), head/tail + `[tool output truncated]` | Ygg tighter at default; **Ygg lossy** — see next row |
| Recovery for oversized results | Full output written to a **temp file; the result carries the path** (`agent/tools/bash.ts:1036-1065`) — the model can re-read exactly what it needs | **None.** Truncated bytes are discarded; the marker says "truncated" with no artifact behind it | **pi wins — material gap.** A 200KB diff or 5MB build log is *available* in pi, *gone* in Ygg (forces re-run of the command) |
| Reasoning replay (Anthropic) | Opt-in **thinking display** (`agent/thinking-display.ts:135-137`): when display = *omitted*, assistant turns are sent with an **empty thinking body + the original signature** (`api/anthropic-messages.ts:211-224`) → signature continuity preserved, **text never re-sent** | Always re-sends full reasoning text, gated only on `protocol == Anthropic && model == current model` (`ygg-ai/src/protocol/anthropic.rs:132-136`). **No display knob exists** | **pi has an escape hatch; Ygg has none** (and see OpenAI row) |
| Reasoning replay (OpenAI protocol) | n/a (pi's mechanism is protocol-specific to Anthropic) | `AssistantPart::Reasoning → AssistantPart::Text` on **every** request (`openai_chat.rs:621-625`) to dodge llama.cpp `content: null` | **Worst-case bloat path is Ygg-only** — on local servers, all historical reasoning is re-typed as plain text forever, and no setting changes that |
| Deferred tools / prefix cache | Splits tools into **immediate** (used or core: read/search/agent) and **deferred** (added, unused); cache breakpoints at system, end-of-immediates, last user message (`utils/deferred-tools.ts:29-68`, `api/openai-completions.ts:349-357`) — the deferred set can change **without invalidating the prefix** | Port exists: `deferred_tool_loading` flag (`ygg-ai/src/types.rs`), `openai_chat.rs:690-713` with the comment "Mirrors pi's deferred-tools handling" (announced-but-unused tools excluded from the tools array) — but **no model spec sets the flag (dormant)** and **the Anthropic adapter has no equivalent** | **pi works; Ygg's port is inert** in practice |
| Compaction trigger | `context_window − reserveTokens` (reserve = **16k**, `agent/session/manager.ts:422-425`) | **Fixed 85% of window** (`config.rs:377-382`) | Different shapes: Ygg always fires earlier in *absolute* headroom (at 2M: 1.7M vs pi's 1.98M) — pi lets you use the full window minus a safety reserve; Ygg burns the top 15% of large windows on a forced compaction |
| Summary content | Preserves a **structured file-operations ledger** (filesRead / filesModified / filesCreated with line counts, `agent/compaction/compaction.ts:333-393, 501-535, 575-579`) | Prose-only summary prompt (`resources.rs`, COMPACT_SYSTEM_PROMPT) | **pi wins** — after Ygg compaction, the model has no structured record of what it already read → re-read storms (measurable: the 37% tool-result class is re-generated after each compaction) |
| Keep window | 20k tokens | 20k tokens | Parity (ported) |
| Output budget | Clamped to model max (`utils/output-budget.ts:56-86`) | Same pattern | Parity |
| Fixed prefix size | ≈ **6.5–7.5k tokens** (base ~4.5KB + AGENTS.md 10.7KB + 4 tool schemas ~9KB + 5 prompt templates + 1 skill) | ≈ **4.7–6.5k** (measured 3,176 cold start; no AGENTS.md in the repo; 6-skill catalog) | Ygg marginally leaner — **not a material differentiator** |

### 3.2 Live telemetry comparison

**pi fleet** (120 session files, Jul 10 – Aug 23, all repos):
- 16,838 requests · 2.581B prompt (fresh 72.77M + cache 2.508B = **97.2%**) · 6.83M output (**45% reasoning**) · **$1,174** · avg **153k prompt/request**
- Flagship Aug 17 (13.6MB, 1,447 requests, `gpt-5.3-codex-spark` @ xhigh): 101.6M cache-read (97.2%), 216k output (**70% reasoning**), maxCtx 127k, **18 compactions** all pinned at 123–135k, $25.95
- Aug 23–24 (3 sessions, `stealth/ox-alpha` 1M window): 1,382 requests, 244.3M prompt (96.8% cached), 406k output, maxCtx 424k, **0 compactions**, $0

**Ygg fleet** (see §2): mother session 8.71B prompt / 96.7% cached / 76.6k root requests / avg 113.8k / max 724k / **1,253 compactions** / $5,636; plus 35 swarm teams (Aug 17–26) 1.13B prompt / 96.7% cached / 3.64M output / avg 98k.

**Same-day A/B (Aug 23–24, same repo, both tools):**
| | pi (3 sessions, ox-alpha 1M) | Ygg (57f5e5307bf6638d root + swarm) |
|---|---|---|
| Requests | 1,382 | 250 root (+11,057 workers across 35 teams) |
| Prompt tokens | 244.3M (96.8% cached) | 18.65M root (95.8%) + 1.13B swarm |
| Output tokens | 406k (rsn not reported) | 146.9k root (17% rsn) + 3.64M swarm (39–73% rsn) |
| Max context | 424k | 226k root |
| Compactions | **0** | 4 (root) — each a cold, full-price run |

Reading: on the *root* lane, Ygg ran **6× more requests at 1/13th the prompt volume** — because Ygg's architecture **delegates to subagents while pi does everything in-thread**. pi pays for the whole conversation in one ever-growing (but compaction-pinned) context; Ygg forks cheap post-compaction contexts (§1.6) and pays in the swarm instead. Neither is "cheaper" outright — the costs relocate. What pi does that Ygg's swarm doesn't: keep a single continuous, compaction-managed thread where the file ledger survives compaction.

### 3.3 Composition comparison (post-last-compaction)

| | **pi flagship** (codex-spark) | **Ygg** (61405) |
|---|---|---|
| assistant text / byproduct | **58.6%** assistant_text | 1.5% text |
| reasoning | **38.1% encrypted signature blobs** (`thinkingSignature`; plaintext text = 0 in this setup) | **46% reasoning text** |
| tool results | 0% stored in user messages as parts (plain-text style) | **37%** |
| tool calls | 3.2% | **7%** |
| user words | ~0.01% | ~2% |
| **Total byproduct** | **96.7%** | **83–90%** |

Both systems converge on the same finding: **the window is almost entirely agent byproduct.** pi's codex-spark sessions are even worse on paper (96.7%) because the stealth model family stores reasoning as opaque encrypted signature blobs that must still be re-sent; Ygg's 46% reasoning class is at least human-readable text that *could* be summarized or dropped — which is exactly why strategy #1 is cheaper for Ygg to implement.

### 3.4 Corrections to earlier findings sofar
1. **"1.58M token context"** → aggregation artifact (sum of 30 subagent usages). No single Ygg request exceeded 724k in 6 weeks of data.
2. **"2.3M-token file read"** → not reproducible. Under the 16KB cap a single `read` result is ~4k tokens; the largest *persisted single tool result* found is 196KB (~46k tokens, Aug 7) and one 5.54MB entry in the loose-cap era (~1.3M tokens, Jul 26). The 2.3M figure is treated as an artifact of the same aggregation confusion.
3. **"Ygg has no tool-result cap"** → false. 16KB default cap exists and is log-verified active on most days. The real gaps are **recoverability** (no temp-file spill), **config-lability** (9 spike days at 121KB–5.5MB), and **no decay**.
4. **pi's thinkingDisplay escape hatch** → exists in pi's code but is **dormant in this user's pi config** (unset); and in the user's actual pi sessions (stealth/codex family) thinking is empty-text + encrypted signature, so pi *also* replays ~38% of context as opaque blobs. The escape hatch is latent, not exercised.
5. **Cache-miss storms** → previously estimated from a 2-day window (3 storms); the 6-week rollup shows it's structural: **2.3% of requests are cold but carry 32% of fresh billing** (≈40% of all fresh input once always-cold compactions are included).

---

## 4. Consolidated strategy list (ranked by measured impact)

| # | Strategy | Measured lever | pi precedent | Effort |
|---|---|---|---|---|
| 1 | **Prune/decay stale reasoning in the input** — replace N-turns-old reasoning blocks with a one-line stub (or empty-body + signature on Anthropic) | Ygg: −45% of live context; pi flagship: 38% is signature blobs | Yes — `thinking-display.ts` omitted-mode is exactly this mechanism (anthropic-messages.ts:211-224); port it, plus add an OpenAI-protocol variant replacing the blanket Reasoning→Text rewrite (openai_chat.rs:621-625) | Medium |
| 2 | **Tool-result decay + recovery** — (a) when the 16KB cap truncates, spill the full output to a temp file and put the path in the result (pi parity); (b) age out kept results: >N turns old → one-line stub + pointer | Ygg: −25–30% of context (37% tool-result class); fixes the "gone forever" gap | Yes — temp-file spill is pi's core design (bash.ts:1036-1065); decay is **new** (pi doesn't have it either — implementing in Ygg is a differentiator) | Medium |
| 3 | **Adaptive reasoning effort** — default below max; escalate on signals (multi-file edits, failing tests, explicit "think hard") | Output: −50–70% (reasoning is 44.5% of Ygg output fleet-wide, 73–83% in recent swarm teams, $ at max-effort prices) | No — neither system does this | Medium |
| 4 | **Compaction backstop** — (a) persist the `first_kept` boundary (mother session: 1,253 runs, **0 persisted boundaries** — a resume/reconstruction correctness risk); (b) make the trigger model-window-aware when models switch mid-session (median compaction input 37k vs documented 85% threshold); (c) make the compactor context cacheable or batch runs — every run is cold ($0.227 × 1,253 = **$284.61**); (d) optionally adopt pi's window−reserve shape over fixed 85% | Eliminates the $284.61/6wk housekeeping line and the churn anomaly; removes the 1-request full-price lag | Yes for (d) and for boundary semantics (pi's `firstKeptEntryId` is persisted); (a)-(c) are Ygg-specific fixes | Low–Med |
| 5 | **Structured file-ops ledger in compaction summaries** (filesRead/Modified/Created survive the cut) | Prevents post-compaction re-read storms (the 37% tool-result class is re-generated after every compaction) | Yes — pi's compaction.ts:333-579; direct port | Low |
| 6 | **Edit-by-anchor / write-diff** — anchor + hunk edits instead of full old/new text re-typing | Output: removes the redundant-context re-emit in `edit`/`write` (second-largest output driver after reasoning) | No | Medium |
| 7 | **Cache-cold UX** — surface "cache cold — this response bills at full price" (and for local models: "first token will be slow") | 2.3% of requests carry 32–40% of fresh billing; visibility enables behavior (reorder work, batch after idle) | No (pi doesn't surface this either) | Low |
| 8 | **Finish the deferred-tools port** — enable per model spec + add the Anthropic-adapter equivalent | Small (the user's fleet rarely adds tools mid-session); correctness/parity win | Yes — the whole mechanism exists in pi (deferred-tools.ts) | Low |

**Expected compound effect** (strategies 1+2 on the measured 61405 composition): live context 93k → ~45–50k, i.e. **~2× effective intelligence per window** at identical model, plus 15–20% fewer compactions (strategy 4), 50–70% lower output spend (strategy 3), and elimination of the ~$285/6wk compaction housekeeping line.

---

## 5. Caveats
- **Byte→token conversion**: composition shares computed at 3.4–4.3 B/token; shares are robust to the exact factor, absolute token counts from bytes are ±20%.
- **Clock skew**: persisted timestamps in these logs are unreliable for wall-clock timelines (documented earlier: a 14:22-stamped record covering 15:06–15:40 activity). Token counts are unaffected; per-day file bucketing uses file-creation epochs, which are sound.
- **pi thinking text is encrypted** in this user's setup (stealth model family): pi's "38% reasoning" is measured on signature-blob bytes, not readable text — its true replay cost profile differs from Ygg's plaintext reasoning.
- **Cost attribution** uses persisted per-request `usage` + `cost` fields (provider-reported). The `session_cost` field in Ygg records is per-run (resets on process restart), not lifetime — lifetime figures here are sums of per-record costs.
- **pi flagship compactions show $0 usage** (codex endpoint billing is opaque to the session log) — pi's true compaction spend is unmeasurable from the log and likely non-trivial.
- The mother session ran **44 distinct models** including nightly dogfooded builds; behavioral differences between builds (cap settings, trigger basis, part schemas — the persisted entry schema visibly changed mid-session) are folded into the "churn anomaly" rather than attributed to a single build.
- `maxTR` figures are whole-JSON-line sizes (entry overhead included), so a "16KB entry" ≈ 15KB of result text — still clearly at the cap.

---

## Appendix: measurement scripts (reproducible)
- `notes/agg_compare.py` — pi fleet totals + flagship composition + Ygg per-kind rollups
- `notes/probe_mother.py` / `probe_mother2.py` — mother-session media scan, largest entries, cold-cache rollup, per-model billing, tool-result cap timeline
- Raw data: `~/.ygg/sessions/c4c8e202815bca65/` (1,425 files), `~/.pi/agent/sessions/` (120 files)
