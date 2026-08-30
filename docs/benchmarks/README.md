# Benchmarking Ygg

This directory contains reproducibility evidence. A result is publishable only
when its losses, environment, binary identity, raw outputs, and any adjudication
exclusions are retained.

Current artifacts include the compact canonical Terminal-Bench 2.1
[evidence package](tb21-v0.6.2/README.md), the frozen
control fingerprint ([baseline-v0.6.2.md](baseline-v0.6.2.md)), the reconciled
failure report
([failure-report-v0.6.2-2026-08-28.md](failure-report-v0.6.2-2026-08-28.md)),
the complete token-efficiency audit
([token-efficiency-v0.6.2-2026-08-28.md](token-efficiency-v0.6.2-2026-08-28.md)),
the scoped coding-agent
[runtime-footprint comparison](runtime-footprint-2026-08-29.md), and the opt-in
beta protocol ([beta-protocol.md](beta-protocol.md)).

## Optional agent telemetry

Enable telemetry explicitly; normal sessions do not create it:

```console
ygg --telemetry ./artifacts/run.jsonl --model <model> "<task>"
```

`--telemetry` also accepts a relative path, resolved from the invocation
directory. `YGG_TELEMETRY` and `telemetry = "..."` in `~/.ygg/config.toml`
are equivalent configuration layers. The file is created with owner-only
permissions and contains bounded JSONL records under `ygg.telemetry.v1`.

Telemetry records:

- `run_started` — opaque session/run identity, model/endpoint/protocol,
  context limit, input byte counts and a SHA-256 input identity; no prompt text.
- `model_request_started` and `model_request_finished` — logical turn,
  attempts, wall latency, TTFT, generation time, context occupancy, output
  byte counts, stop reason, and provider usage.
- `provider_retry` — retry number, bounded sanitized diagnostic, and backoff.
- `tool_started` and `tool_finished` — tool name, hashed arguments, elapsed and
  result sizes, repeated-call count, status, known built-in state changes, and
  a conservative no-progress streak. Arguments and results are not retained.
- `compaction_started` and `compaction_finished` — reason and durable outcome.
- `candidate_rejected`, `steering_delivered`, `follow_up_delivered`, and
  `delegation_updated` — control-flow accounting.
- `run_finished` — terminal status and aggregate request/tool counters.

Usage semantics are explicit: `uncached_input_tokens` is the provider's
standard-rate input bucket. `cache_read_tokens` and `cache_write_tokens` are
disjoint additions; `cache_write_1h_tokens` is a subset of cache writes.
`provider_input_tokens` is the three disjoint prompt buckets' sum.
`reasoning_tokens` is a subset of output. `total_tokens` is Ygg's canonical
normalized sum, not a promise that an overlapping or omitted provider wire
`total_tokens` was preserved. Records with usage include `usage_scope`:
`request`, `operation`, or `run_cumulative`; never sum cumulative snapshots.

Telemetry is an observer, not a wire capture. It does not currently expose
provider request IDs, exact response-header timing for compaction/gate calls,
or raw context bodies. Those limitations must be stated in reports.

## Systems measurements

`scripts/bench-systems.py` uses only the Python standard library and real OS
process measurements. It reports medians and p95s over repeated runs, best
available RSS/PSS, CPU samples, direct-process concurrency totals, and parsed
Ygg telemetry:

```console
python3 scripts/bench-systems.py \
  --binary ./target/release/ygg \
  --repetitions 9 \
  --command sessions='./target/release/ygg --offline sessions list' \
  --output ./artifacts/systems/ygg.json
```

For a long-lived process, provide an explicit command whose stdin remains open:

```console
python3 scripts/bench-systems.py \
  --idle-command idle='./target/release/ygg --plain --model <local-model>' \
  --concurrency 1,2,4 \
  --repetitions 9 \
  --output ./artifacts/systems/ygg-idle.json
```

The runner never invokes command strings through a shell. Use `env`, a wrapper
script, or an absolute executable in the argument vector when setup is needed.
JSON output retains every startup run, every per-run idle sample and peak, and
every per-run concurrency sample and peak. `--skip-startup`, `--skip-idle`, and
`--skip-concurrency` can split a long campaign into raw files without reducing
the repetition count for the retained cases. PSS is reported only where the
operating system exposes it; RSS is not a PSS substitute. Direct children are
measured, so an inference server must be reported separately and excluded from
the agent-overhead number.

The command adapter deliberately does not pretend to measure UI rendering,
provider-to-tool scheduling, or resume latency from a generic `--version` case.
Those cases require a harness-specific driver and should be supplied with
`--command` or an additional checked-in adapter. A comparison must use the
same task, endpoint, model weights, context limit, timeout, hardware, and
concurrency for every harness.

## Failure taxonomy

Record each non-success trial in one primary class and optional secondary
causes:

| Class | Evidence to retain |
| --- | --- |
| `benchmark_timeout` | process deadline, last telemetry record, active operation, and OS command state |
| `provider_failure` | sanitized provider phase/status/request ID, retry history, and whether any output was generated |
| `context_failure` | model limit, estimated/provider input, compaction attempts, and durable boundary |
| `tool_failure` | tool name, exit/timeout status, bounded result, and whether the workspace changed |
| `agent_failure` | terminal reason, session checkpoint, and last successful state transition |
| `verifier_negative` | verifier result only; never infer cause from a failed score |
| `integrity_exclusion` | exact reason, reviewer evidence, and original raw result |

Do not collapse verifier negatives into timeouts. Do not call a provider failure a
model failure without evidence. A trajectory audit is separate from official
benchmark adjudication.

## Canonical-run checklist

Before starting a full campaign, record:

1. Ygg version, commit, binary hash, compiler and OS image.
2. Harbor version/commit, exact dataset revision, task count, attempts, timeout,
   concurrency, and retry policy.
3. Model identifier and weight digest, provider/server version and endpoint
   settings, reasoning effort, context/output limits, and cache policy.
4. Exact system prompt/configuration, enabled tools, disabled extensions,
   workspace image, environment digest, and benchmark command.
5. Raw result files, trajectories, telemetry, stdout/stderr, and verifier output.
6. Reward-hacking audit rubric, reviewers, confirmed exclusions, ambiguous cases,
   and an official-vs-unofficial score distinction.

Run a small generic regression sample before changing any heuristic. A complete
campaign is not evidence that an implementation is good if the protocol or
inputs changed between control and candidate.

## Same-model harness shootout

Use one immutable endpoint and one task manifest. For each harness, collect:
accuracy, success/hour, wall time per success, model requests, Ygg/tool calls or
the closest equivalent, provider input/output/cache buckets, retries, timeouts,
agent RSS/PSS, and crashes. Publish raw per-trial records and a table that
separates runtime overhead, UI latency, agentic efficiency, and successful-task
throughput. If a harness cannot expose a metric, mark it unavailable rather
than estimating it from incompatible logs.
