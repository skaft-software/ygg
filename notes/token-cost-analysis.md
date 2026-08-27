# Ygg Token Cost Analysis

Date: 2025-08-26. Method: static analysis of prompt/tool/protocol/compaction code + empirical
telemetry mined from live session JSONLs (`~/.ygg/sessions/c4c8e202815bca65/`) — three
concurrent main sessions plus the `.delegation` store (525 historical teams, 35 with usage
records, Aug 17–26).

## 1. How tokens are consumed in Ygg

Every model request re-sends the **entire context**:

```
input tokens (per request) = system prompt + tool schemas + skill catalog
                             + [injected resources]
                             + full conversation history
                               ├─ user messages (small)
                               ├─ tool results   ← grows, kept full-size
                               ├─ tool call args ← grows, kept full-size
                               └─ reasoning blocks ← grows, re-sent as text (OpenAI protocol)

output tokens (per turn) = reasoning (dominant) + tool call args + prose
```

Prompt caching splits input into `cache_read` (~10x cheaper) vs fresh input. The stable
prefix (system + tools) is cached; the **growing history is the bill** — either at cache
price (warm) or full price (cold/evicted).

## 2. What the data shows

### 2.1 Composition of a live context (session 61405, post-compaction, 400KB ≈ 93k tokens)

| Component      | Bytes    | Share |
|----------------|----------|-------|
| Reasoning      | 183,352  | 46%   |
| Tool results   | 149,136  | 37%   |
| Tool call args | 27,951   | 7%    |
| Conversation text | 5,923 | 1.5%  |
| (prefix: system+tools ≈ 3.2–6k tokens) | | ~6% |

**83% of the live context is process byproduct** (the model's own reasoning + its own tool
outputs). Only ~2% is actual conversation. The model pays to re-read its own thinking and
tool output on **every** request.

### 2.2 Output side

Post-compaction window (E088→E127+): ~45–50k output tokens, of which **~40k (≈85%) is
reasoning** (1.5k–7k per turn at max effort; even mechanical bash/read turns).

### 2.3 The delegation fleet (35 measured teams, 10 days)

| Metric | Value |
|---|---|
| Total prompt tokens | ~1.13B |
| — of which cache-read | **1.093B (96.7%)** |
| — of which fresh input | ~36M |
| Total requests | ~11.5k |
| **Avg prompt re-read per request** | **~98k tokens** |
| Total output | ~3.64M |
| — of which reasoning | ~1.43M (39% historically; **73–83% in the most recent teams**) |

Largest team (Aug 20, 18 workers): 3,550 requests, 733M cache-read + 8.3M fresh input,
1.2M output tokens. Today's 21-agent audit swarm (gpt-5.6-sol): 846 requests, 81.4M prompt
tokens (95% cache-read), 215k output (73% reasoning).

**The dominant cost is steady-state context size × request count**, billed mostly at the
cache-read rate — but cache-read price is still real money at 1B+ tokens, and cache reads
are *multiplied* by every request.

### 2.4 Growth and compaction behavior (local qwen38 sessions, 1.5–2M window)

- Context grows monotonically with use: 6k→93k observed in one 3h local session;
  4.7k→212k in one 2.5h cloud session; 3.2k→89k in a 34-min subagent.
- Auto-compaction fired at the 85% threshold with a **one-request lag**: the request that
  crossed the threshold still paid ~91–93k; truncation took effect on the *next* request.
- Post-compaction floor ≈ 40k (≈43% of the pre-compaction context) — from
  `keep_recent_tokens = 20,000` + summary + prefix.
- **Cache-miss storms on the local endpoint**: after ~2–3 min idle the local KV cache
  evicts; the next request pays full price for the *entire* context. Observed: 3 storms in
  the main session (~56k full-price each) and **10 of 30 requests in the context-cost
  subagent** (594,733 full-price tokens = 72% of that subagent's 824,985 total input).
- A 1.58M-token context observed in-usage vs ~90k persisted is consistent with the 2M
  window (85% = 1.7M threshold) — in-memory growth beyond persisted entries, not
  necessarily a bug (see §5 caveats).

### 2.5 Fixed prefix (paid on 100% of requests, even cache hits)

Measured subagent cold start: **3,176 tokens** = BASE_PERSONA + tool preference + work
style + skill catalog + 5 core tool schemas (raw: read 1.8KB, bash 2.1KB, search 2.1KB,
edit 2.1KB, write 1.3KB ≈ 2.4–2.9k tokens) + subagent policy. Each heavy extension adds
~1.2–1.5k (browser ≈ 4.7KB raw). Small in percentage terms (~3–6%), but it multiplies by
every request ever sent.

## 3. Ranked cost drivers

### Input side

| # | Driver | Mechanism (code) | Evidence |
|---|--------|------------------|----------|
| 1 | Steady-state context size × request count | Every request resends all history; `session.rs:298-299` truncates only at compaction | 96.7% of 1.13B fleet tokens are cache-read; avg 98k/request |
| 2 | Reasoning accumulation in input (OpenAI protocol only) | `openai_chat.rs:605-625`: `AssistantPart::Reasoning` re-emitted as `Text` every request (llama.cpp `content:null` workaround); no decay. Anthropic path only re-sends with valid signatures (`anthropic.rs:153-165`) | 46% of live context is reasoning |
| 3 | Tool-result accumulation | Results kept full-size until compaction | 37% of live context |
| 4 | Late compaction + large retention | `config.rs:377-382`: `threshold_fraction 0.85`, `keep_recent_tokens 20,000`; triggered in-loop at `agent.rs:4590-4620` with one-request lag | Pays full growth 0→85%; post-compaction floor ≈ 43% of pre-compaction |
| 5 | Cache-miss storms (local endpoints) | Local KV cache evicts after ~2–3 min idle → next request full-price on whole context | 3 storms main + 10/30 requests in subagent at full price (72% of its input) |
| 6 | Fixed prefix | `prompts.rs` persona + all tool schemas + skill catalog every turn | 3.2k base + ~1.5k per heavy extension, × every request |

### Output side

| # | Driver | Evidence |
|---|--------|----------|
| 1 | Reasoning tokens at max effort | 73–90% of output tokens; 1.5–7k/turn; billed at 3–5x input price — the largest unit-price cost in the system |
| 2 | `edit` re-typing old text | Model outputs exact old text (already in context) → same bytes paid as output, then re-paid as input forever (`tools/edit.rs`) |
| 3 | `write` full-file re-output | Whole file re-emitted into context permanently |
| 4 | Prose narration | Already 1.5–2% of context — base-prompt concision works; not a driver |

## 4. Strategies (ranked by measured impact)

**Framing:** the goal stated — "maximum intelligence per token" — is not just cost. With
83% of the context occupied by byproduct, a 95k-token window holds only ~15–20k tokens of
fresh, decision-relevant evidence. Cutting byproduct ~40–50% **increases the effective
intelligence of the same window ~2x**, in addition to cutting the bill.

### Input side

1. **Decay tool results progressively** (instead of all-or-nothing at 85%).
   Keep the most recent 2–3 results full-size; replace older ones with a one-line stub
   (`path — N bytes — re-read with read(offset,limit)`). The model can recover content on
   demand; paying for a re-read on one request is far cheaper than paying for it on every
   remaining request.
   *Expected: −25–30% steady-state context (tool results are 37%).*

2. **Prune stale reasoning from the input context** (highest single impact).
   Keep only the last 1–2 reasoning blocks per turn (or last N overall); stub the rest.
   Implement as a context-assembly/protocol-conversion transform, in the same pattern as
   the existing protocol gating (`anthropic.rs:153-165`, `openai_chat.rs:605-625`).
   Preserve the DeepSeek special case (requires previous turn's `reasoning_content` —
   keep last 1; guard already at `openai_chat.rs:600-608`).
   *Expected: −40–45% steady-state context (reasoning is 46%).*
   Combined with #1: **40–50% smaller steady-state context** — against a 1.09B-token
   fleet cache-read history that is hundreds of millions of tokens, and a smaller context
   means fewer local KV evictions → fewer full-price storms (also addresses driver 5).
   Risk is low: old reasoning is process, not deliverable; the text decisions it produced
   are retained.

3. **Tighten compaction as the backstop**: `threshold_fraction` 0.85 → 0.60–0.70 (cost:
   compaction requests ~2–3x more frequent; each measured ~27k — cheap) and
   `keep_recent_tokens` 20k → 8–12k (post-compaction floor 93k→40k becomes 93k→~20–25k).

4. **Local cache-cold UX**: detect `cache_read=0` on a large-context request and surface
   it in the TUI ("this request paid full price — cache was cold"); keep
   `prompt_cache_key` stable per session. Cheap, and users currently can't see the 10x.

5. **Trim the fixed prefix** (second order, zero risk): tighten tool *descriptions* (they
   overlap with what parameter names already say — keep the schema fields themselves; the
   model needs them to call correctly, and that's the intelligence-critical part). Persona/
   work-style prose can shed ~30–40%.

6. **Keep subagent forks small** (already good): cold start measured at 3.2k tokens;
   fork respects compaction boundary (`session.rs:1484-1486`). Keep the 8k output cap and
   `max_turns`; add a brief budget (≤1k tokens) — orchestrator context is the other
   multiplier (the 21-agent swarm's parent spent 215k output tokens, 73% reasoning, while
   orchestrating).

### Output side

1. **Adaptive reasoning effort** (largest unit-price lever): default to Medium effort with
   a per-turn budget (~2k), escalating on turns that actually need deliberation (design,
   debugging, "think hard"), rather than uniform max. The `ReasoningConfig` API
   (Off/On/Effort/Budget) already supports this — it's a defaults/policy change, not new
   machinery. *Expected: −50–70% output tokens on typical coding sessions.*
   (Note: this cuts *production* of reasoning; strategy 4.2 cuts *accumulation* — they
   compound but are independent.)

2. **`edit` by anchor instead of re-typing**: `edit(path, anchor, new_text)` where anchor
   is a region hash/line-range (the file-wide `expected_hash` already exists — extend the
   idea to the replaced region). Model outputs only the new text: ~50% fewer edit-arg
   tokens and elimination of exact-match mismatch failures.

3. **`write` with append/replace-range modes** for large files to avoid whole-file re-output.

4. Keep prose concise (already effective — leave the base prompt's concision instructions).

## 5. Caveats

- Byte→token conversion estimated at 3.4–4.3 B/token depending on content class
  (code/JSON tokenizes denser than prose); shares are robust to this.
- This machine shows **clock skew** in persisted timestamps** (usage record stamped 14:22 for
  activity that occurred 15:06–15:40; root-file mirror record of the subagent's aggregate
  824,985/7,417/757,120 carries the skewed stamp). Fleet totals are token counts from
  usage records — unaffected — but wall-clock timelines in the raw logs are unreliable.
- The 1.58M-token in-memory context vs 90k persisted is consistent with the 2M window's
  85% threshold; flagged for follow-up, not asserted as a bug.
- `debug_prompt` expands templates only; it does not dump the wire request body, so wire
  sizes here are inferred from persisted entry bytes + provider usage records, not a
  captured HTTP body.
