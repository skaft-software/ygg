# Pi runtime evidence harness

This is a reproducible, credential-free **harness**, not a published performance
result. It exercises the checked-in Pi compatibility fixture and writes bounded
`ygg.pi.runtime.evidence.v1` JSON. It exists to establish the evidence shape and
to catch regressions while the API 0.3 runtime manager is still being built.

The emitted release decision is always `hold`. Do not turn a fixture result into
a performance or release claim.

## Run

Use an immutable candidate identifier: normally the exact Git commit of a clean
candidate checkout or an immutable build digest. The value is recorded verbatim,
so it must not contain a credential, local path, or other private identifier.

```console
python3 scripts/bench-pi-runtime.py \
  --candidate 0123456789abcdef0123456789abcdef01234567 \
  --repetitions 9 \
  --sample-interval-ms 20 \
  --max-resource-samples 64 \
  --output ./artifacts/pi-runtime/linux-amd64
```

The script uses only the Python standard library and passes Node argument vectors
directly to `subprocess`; it does not execute a shell command. It creates a
fresh temporary HOME/XDG configuration tree, retains only a small allowlist of
locale/time/PATH variables, does not read normal Ygg/Pi configuration, does not
inherit provider credentials, and never launches or contacts a model/provider.
It requires a local Node executable because the measured compatibility fixture is
Node-based. It makes no package-manager or network request.

`results.json` and `SHA256SUMS` are written to the output directory. The
artifact includes the candidate identifier; script and bridge SHA-256 values;
checked-in fake-Pi package identity/integrity; fixture source and lock
fingerprints; Node/Python/platform/hardware metadata; parameters; raw bounded
resource samples; per-profile median/p95 summaries; and an explicit hold-only
release decision.

Run its contract test with:

```console
python3 -m unittest discover -s scripts/tests -p 'test_bench_pi_runtime.py'
```

## Profiles and attribution

Every invocation performs the same number of repetitions for these profiles:

| Profile | Current fixture meaning |
| --- | --- |
| `no_extension` | Minimal checked-in idle JSON-RPC process; baseline for bridge-overhead attribution. |
| `legacy_eager` | One Pi compatibility bridge initialized during startup. |
| `lazy` | Baseline starts first; the bridge starts only for first activation. |
| `shared_workspace` | One bridge is reused across two synthetic session journeys. |
| `pi_aggregate` | Two ordered Pi sources load through one bridge and one fake Pi `ExtensionRunner`. |

Each run retains startup readiness, first activation, warm call, and process
replacement readiness timings. `shared_workspace` additionally retains reuse
time. `process_restart_readiness_ms` is deliberately a process replacement;
it is not a claim of manager-owned hot reload. The `release_decision` computes a
startup-median delta only from `no_extension` and `pi_aggregate`, so it does not
mislabel the other fixture profiles as a production baseline.

The profile names model the intended lifecycle shapes, but this driver is not an
API 0.3 runtime-manager adapter. In particular it does not establish production
lazy activation, cross-workspace sharing, reload policy, FD limits, or
multi-session governance.

## Resources and platform limits

Resource samples cover the root process and its descendants, and each process
phase retains no more than `--max-resource-samples` samples (1–256). Linux reads
`/proc` for RSS, PSS where `smaps_rollup` is available, cumulative CPU ticks,
threads, and file descriptors. macOS uses `ps` for RSS, instantaneous CPU
percent, and threads; macOS PSS and FD count are recorded as unavailable rather
than estimated. Other platforms return unavailable resource fields.

Agent-process measurements are always separate from inference-server resources.
The default result says no inference server was launched or contacted. If an
already-running, independently managed inference process must be documented,
pass `--inference-pid PID`; the harness takes one separate process-tree snapshot
without connecting to, configuring, or stopping that process. It remains a
hold-only result. Portable GPU collection is intentionally unavailable; attach a
platform-specific collector and document its method, version, cadence, and
attribution before making a GPU claim.

## What is still required for release evidence

A candidate-release campaign needs a checked-in runtime-manager adapter that
uses the real aggregate plan/evidence seam, a clean immutable candidate build,
and separately retained Linux and macOS runs. It must state exact model/server
identity and digest when inference is included, retain agent and inference
resources separately, use a defined cold/warm/cache policy, and document all
failures. This fixture has no model, server, credentials, provider requests, or
GPU result, so those fields are explicitly unavailable rather than inferred.

Before publishing any capture, follow the [publication boundary](README.md#publication-boundary):
review the candidate field and hardware metadata, replace unneeded host/path
identity with stable placeholders, remove secrets rather than redacting them,
record the sanitation, and recompute `SHA256SUMS`. Retain raw samples and failed
trials; do not publish only summaries.
