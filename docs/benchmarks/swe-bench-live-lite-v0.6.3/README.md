# Ygg / SWE-bench-Live Lite v0.6.3 baseline

This directory is a reproducibility package, not a product claim.  The
benchmark is the frozen Python-only `lite` split, not the newer MultiLang
benchmark.

## Current status

- Ygg target: `mission/v0.6.3-next` / tag `v0.6.3`, commit
  `cb6be3686181de743905b115442bf090afb822e6`.
- Dataset: `SWE-bench-Live/SWE-bench-Live`, `lite`, Hugging Face revision
  `a637bd46829f3132e12938c8a0ca93173a977b8e`, 300 rows.  The parquet LFS
  SHA-256 is recorded in `data/dataset-manifest.json`.
- Official evaluator: Python-only branch commit
  `ad79b850f15e33992e96f03f6e97f05ddf9aa0be` (`swebench` 4.0.3).
- Model: `gpt-5.6-sol`, Codex OAuth provider, reasoning `max`, k=1.
- Serial exploratory baseline: one task at a time; no task retries.
- Parallel throughput runs use the isolated launcher with an explicitly recorded
  task concurrency (the remote pilot target is 20); they are not pooled with the
  serial run for a score claim.
- Agent timeout: 1,800 seconds.  Evaluator test timeout: 1,800 seconds.
- Published task images: upstream `starryzhang`, `latest`, x86_64.  The local
  exploratory host is Apple silicon and uses emulation.  The parallel throughput
  run is executed on `temper-inference` (`linux/amd64` Docker); the evaluator
  wrapper changes only the upstream host-architecture probe, and official
  grading code is not modified.

The authoritative phase state and results are in `manifest.json`,
`valid_instances.json`, `invalid_instances.json`, and the phase directories
under `artifacts/`.  Raw trajectories are intentionally local owner-only
artifacts and are ignored by Git.

## Important integrity boundary

The parquet rows contain gold `patch`, `test_patch`, `FAIL_TO_PASS`, and
`PASS_TO_PASS` fields.  `scripts/run_agent.py` reads the issue text on the host
but mounts none of those fields, the parquet file, the evaluator input, or the
gold-validation logs into a Ygg container.  Its only task-container writable
mount is that task's trajectory directory.  Each task starts a fresh container,
checks out the recorded `base_commit`, verifies a clean tree, runs one fresh
Ygg process, captures the diff, and removes the container.

Gold patches are used only by `scripts/validate_gold.py` through the upstream
evaluator.  They live under `private/` and must not be published.  Do not copy
them into an agent run directory.

## Reproduction setup

Use Python 3.12 and install the pinned evaluator checkout.  The upstream
checkout has no lock file on the Python-only branch, so `requirements.txt`
pins the direct runtime versions while the Git commit pins evaluator code.

```bash
uv venv --python 3.12 .venv-swebench
uv pip install --python .venv-swebench/bin/python -r \
  docs/benchmarks/swe-bench-live-lite-v0.6.3/requirements.txt

git clone https://github.com/microsoft/SWE-bench-Live.git /tmp/swe-bench-live-python-only
git -C /tmp/swe-bench-live-python-only checkout \
  ad79b850f15e33992e96f03f6e97f05ddf9aa0be
```

Fetch and verify the exact dataset, then generate the two deterministic
selection manifests:

```bash
.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/prepare_dataset.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --public-output docs/benchmarks/swe-bench-live-lite-v0.6.3/data/agent_tasks.jsonl \
  --full-output docs/benchmarks/swe-bench-live-lite-v0.6.3/private/lite-full.jsonl

.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/select_tasks.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --size 10 --seed 20260829-integration \
  --output docs/benchmarks/swe-bench-live-lite-v0.6.3/selection/integration-10.json

.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/select_tasks.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --size 50 --seed 20260829-pilot \
  --output docs/benchmarks/swe-bench-live-lite-v0.6.3/selection/pilot-50.json
```

Build the Linux binary from the exact Ygg commit with Rust 1.86 and record its
SHA-256.  On Apple silicon, an amd64 Rust container is the simplest route:

```bash
git worktree add --detach /tmp/ygg-v0.6.3-next \
  cb6be3686181de743905b115442bf090afb822e6

docker run --rm --platform linux/amd64 \
  -v /tmp/ygg-v0.6.3-next:/src -w /src rust:1.86-bookworm sh -c '
    export PATH=/usr/local/cargo/bin:$PATH
    apt-get update -qq && apt-get install -y --no-install-recommends musl-tools pkg-config
    rustup target add x86_64-unknown-linux-musl
    cargo +1.86.0 build --locked --release \
      --target x86_64-unknown-linux-musl -p ygg-coding-agent --bin ygg
  '
cp /tmp/ygg-v0.6.3-next/target/x86_64-unknown-linux-musl/release/ygg /tmp/ygg-v0.6.3-next-x86_64-musl
sha256sum /tmp/ygg-v0.6.3-next-x86_64-musl
```

Stage a disposable Codex credential source.  Only `codex.json` and the
optional `codex-models.json` are copied into each task container; the runner
never writes back to the source.

## Phases

### Gold validation and denominator

Run all 300 rows three complete times through the official evaluator.  A row is
valid only if `resolved == true` in all three runs.  This is intentionally
conservative and follows the upstream maintainers' recommendation for this
live dataset:

```bash
.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/validate_gold.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --evaluator-src /tmp/swe-bench-live-python-only \
  --output-dir docs/benchmarks/swe-bench-live-lite-v0.6.3/private/gold-validation
```

The helper defaults to two workers for the model-free check.  The remote
capacity probe attempted 20 workers and is preserved as an
infrastructure-pressure artifact.  The canonical remote validation run records
`--evaluator-workers 8` after measuring concurrent image-store pressure; use the
higher value only on a host with equivalent capacity, and set it to 1 when
diagnosing machine-dependent failures.

This writes the public-safe denominator files at the benchmark root.  The
private evaluator directories retain every run's logs and reasons.  A missing
report is not silently counted as a model failure.

### Agent integration and pilot

Run each phase once, in selection order, with the same binary and credentials.
The run refuses to overwrite a non-empty output directory:

```bash
.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/run_agent.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --selection docs/benchmarks/swe-bench-live-lite-v0.6.3/selection/integration-10.json \
  --binary /tmp/ygg-v0.6.3-next-x86_64-musl \
  --credential-dir "$HOME/.ygg/credentials" \
  --output-dir docs/benchmarks/swe-bench-live-lite-v0.6.3/artifacts/integration-10 \
  --run-id integration-10 \
  --ygg-source /tmp/ygg-v0.6.3-next

.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/evaluate.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --predictions docs/benchmarks/swe-bench-live-lite-v0.6.3/artifacts/integration-10/predictions.jsonl \
  --selection docs/benchmarks/swe-bench-live-lite-v0.6.3/selection/integration-10.json \
  --evaluator-src /tmp/swe-bench-live-python-only \
  --output-dir docs/benchmarks/swe-bench-live-lite-v0.6.3/artifacts/integration-10-evaluation \
  --run-id integration-10-evaluation
```

After the integration gate is clean, repeat the two commands with
`selection/pilot-50.json` and fresh `pilot-50` output directories.  The pilot
is exploratory and must not be presented as the 300-task score.

For the remote parallel throughput run, use the isolated launcher.  It starts
one `run_agent.py` process and one fresh task container per selected instance;
`--concurrency 20` changes only inter-task scheduling and is recorded in the
run manifest:

```bash
.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/run_parallel.py \
  --parquet docs/benchmarks/swe-bench-live-lite-v0.6.3/data/lite.parquet \
  --selection docs/benchmarks/swe-bench-live-lite-v0.6.3/selection/pilot-50.json \
  --binary /tmp/ygg-v0.6.3-next-x86_64-musl \
  --credential-dir "$HOME/.ygg/credentials" \
  --output-dir docs/benchmarks/swe-bench-live-lite-v0.6.3/artifacts/pilot-50-c20 \
  --run-id pilot-50-c20 \
  --concurrency 20 \
  --ygg-source /tmp/ygg-v0.6.3-next
```

This is a distinct throughput configuration: do not combine its outcomes with
the serial exploratory run or change concurrency after the pilot is frozen.

Analyze a phase with:

```bash
.venv-swebench/bin/python docs/benchmarks/swe-bench-live-lite-v0.6.3/scripts/analyze.py \
  --run-dir docs/benchmarks/swe-bench-live-lite-v0.6.3/artifacts/pilot-50 \
  --evaluation-results docs/benchmarks/swe-bench-live-lite-v0.6.3/artifacts/pilot-50-evaluation/results.json
```

## Metrics and limitations

`--telemetry` is a Ygg v0.6.3 opt-in operational JSONL trace.  It contains
hashed tool arguments and sizes, request/tool timings, provider usage buckets,
retries, and terminal counters, not raw provider payloads.  Native Ygg session
JSONL plus a conservative ATIF conversion is retained per task for qualitative
review.  `analyze.py` reports model-request elapsed time, tool execution time,
and residual agent time separately; request elapsed time is not provider GPU
inference time.

It also reports model calls/turns/tools, read/search/edit/write/bash counts,
multiple-tool turns, interval-derived concurrent fanout, compound-shell
heuristics, tokens and cache buckets, cost when the provider exposes it, task
wall percentiles, timeout/provider/no-patch rates, and best-effort Ygg/process
RSS.  Remote model-server memory is excluded.  Unavailable values remain
`null`; no metric is inferred from a score.

A score is official only in the limited sense that it is produced by the
upstream evaluator at the pinned commit.  It is not a leaderboard submission or
maintainer adjudication.  Any full-run result must report both `resolved /
gold-valid` and `resolved / 300 nominal`, with the invalid list and all raw
reports retained.

## Artifact map

- `data/dataset-manifest.json`: immutable dataset revision, schema, row count,
  and parquet hash. Original local absolute paths are represented under the
  stable public `/workspace/ygg` placeholder; content hashes and row data are
  unchanged.
- `data/agent_tasks.jsonl`: public-safe task metadata; no evaluator labels.
- `selection/*.json`: deterministic selections and seeds.
- `scripts/`: dataset, serial and isolated-parallel runners, official evaluator
  wrapper, validation, and analysis scripts.
- `manifest.json`: frozen run identity/configuration.
- `valid_instances.json`, `invalid_instances.json`: denominator evidence.
- `artifacts/<phase>/instances/<id>/`: metadata, diff, status, stdout/stderr,
  native sessions, telemetry, and trajectory conversion.
- `artifacts/<phase>/predictions.jsonl`: evaluator-compatible predictions.
- `artifacts/<phase>-evaluation/`: official evaluator logs and `results.json`.
- `aggregate-telemetry.json`: machine-readable efficiency aggregate.
- `artifacts/temper-inference-host.json`: sanitized remote host/resource identity.
- `artifacts/integration-10-projection.json`: pre-approval pilot/full cost and
  token projection; not a benchmark result.
- `failure-taxonomy.md`: conservative failure analysis.

Raw `private/` and `artifacts/` contents can contain issue text, source code,
provider diagnostics, or gold evaluator material.  Review and redact before
sharing.
