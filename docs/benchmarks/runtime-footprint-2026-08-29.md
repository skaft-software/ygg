# Runtime footprint: Ygg vs OpenCode vs Pi vs Codex CLI (2026-08-29)

This is a scoped systems comparison of directly launched agent processes. It
separates settled headless-runtime overhead, generic `--version` startup, and
concurrent direct-process totals. It does **not** measure interactive UI
rendering, resume latency, provider-to-tool scheduling, task throughput, or
model inference.

## Results

Every cell is **median / p95 across 9 runs**. Memory and CPU values are the peak
observed in each run's two-second sampling window after a five-second settle.
P95 uses the runner's linearly interpolated quantile.

### Settled headless-runtime overhead

| Harness and measured mode | Peak RSS (MiB) | Peak PSS (MiB) | Peak CPU (%) |
| --- | ---: | ---: | ---: |
| Ygg 0.6.3 — RPC | **7.52 / 7.53** | unavailable | **0.00 / 0.00** |
| OpenCode 1.18.20 — ACP, `--pure` | **385.16 / 403.39** | unavailable | **36.20 / 41.64** |
| Pi 0.84.4 — RPC | **123.70 / 123.89** | unavailable | **0.00 / 0.00** |
| Codex CLI 0.149.0 — app server | **39.22 / 39.28** | unavailable | **0.00 / 0.00** |

CPU is macOS `ps`'s `pcpu` field, whose output here has 0.1 percentage-point
precision. `0.00 / 0.00` therefore means every retained sample printed `0.0`;
it is not a claim of zero CPU instructions. OpenCode performed periodic work in
its retained windows, so its per-run CPU peaks are materially higher than its
low samples; the raw samples preserve both.

PSS is **unavailable** because this macOS host does not expose Linux
`/proc/<pid>/smaps_rollup`. RSS is not used as a substitute.

### Startup latency

| Harness | Generic `--version` wall time (ms) |
| --- | ---: |
| Ygg 0.6.3 | **4.83 / 5.13** |
| OpenCode 1.18.20 | **239.14 / 354.04** |
| Pi 0.84.4 | **199.00 / 270.88** |
| Codex CLI 0.149.0 | **12.17 / 34.65** |

This is subprocess creation through successful process exit with stdout and
stderr discarded. It is startup of the version command only, not TUI rendering,
interactive readiness, or resume latency.

### Concurrent direct-process totals

Peak RSS totals:

| Harness/mode | 1 process (MiB) | 2 processes (MiB) | 4 processes (MiB) |
| --- | ---: | ---: | ---: |
| Ygg RPC | **7.50 / 7.52** | **15.06 / 15.10** | **30.19 / 30.29** |
| OpenCode ACP | **384.69 / 403.65** | **774.42 / 788.33** | **1554.69 / 1565.19** |
| Pi RPC | **123.61 / 124.50** | **254.42 / 255.65** | **505.36 / 516.62** |
| Codex app server | **39.20 / 39.29** | **78.45 / 78.48** | **156.91 / 157.02** |

Peak aggregate CPU totals:

| Harness/mode | 1 process (%) | 2 processes (%) | 4 processes (%) |
| --- | ---: | ---: | ---: |
| Ygg RPC | **0.00 / 0.00** | **0.00 / 0.00** | **0.00 / 0.00** |
| OpenCode ACP | **35.00 / 38.48** | **27.20 / 40.18** | **20.20 / 38.52** |
| Pi RPC | **0.00 / 0.00** | **0.00 / 0.00** | **0.00 / 0.00** |
| Codex app server | **0.00 / 0.00** | **0.00 / 0.00** | **0.00 / 0.00** |

These totals sum only the 1, 2, or 4 processes launched directly by the runner.
They do not include descendants. PSS totals are unavailable for every level on
this host.

## Methodology

The campaign used the repository's stdlib-only
[`scripts/bench-systems.py`](../../scripts/bench-systems.py), SHA-256
`726711574d778bd493cc2ff43bff0657a70f80a5ab1acf90a5fe660d568735b2`.
The runner passes argument vectors directly to `subprocess`; it never executes
the measured commands through a shell. The campaign parameters were:

- 9 repetitions for startup, runtime, and each concurrency level;
- isolated config roots initialized by unrecorded shakedown/probe runs, with no
  per-run cache reset; every retained run then used a five-second settle and
  two-second sampling window;
- concurrency levels 1, 2, and 4, measured in separate runner invocations with
  `--skip-startup --skip-idle` so each raw file still contains 9 complete runs;
- direct launched-process RSS from `ps`, PSS unavailable, and CPU from `ps pcpu`;
- the same repository workspace and hardware for every harness;
- stdin held open for all headless runtime modes; stdout/stderr discarded;
- no prompt or model request, and no inference server.

Each runtime used a private, otherwise empty configuration root under
`/private/tmp/ygg-runtime-footprint-2026-08-29`. Ygg used the checked-in static
[benchmark provider declaration](runtime-footprint-2026-08-29/ygg-custom-provider.json)
with discovery disabled and `--offline`; its unreachable placeholder endpoint
was never contacted. Pi used offline RPC mode with sessions, tools, extensions,
skills, prompt templates, themes, and context files disabled. OpenCode used its
ACP server with `--pure` and isolated HOME/XDG roots. Codex used its stdio app
server with an isolated `CODEX_HOME` and the `plugins`, `plugin_sharing`,
`remote_plugin`, and `recommended_plugins` features disabled. An eight-second
warm-up probe found no descendants for any of these exact commands.

The modes are the closest installed stdin-open headless service surfaces, but
they are not one common protocol: Ygg and Pi use RPC, OpenCode uses ACP, and
Codex uses its app-server protocol. Consequently, these figures compare the
named process modes only; they must not be generalized to full-screen UIs,
active agent turns, or identical loaded feature sets.

No inference server was started, so inference-server RSS/PSS/CPU is not
reported and no inference server is included in the agent-process figures. This
was a sequential run on a normal workstation without CPU pinning, cache
flushing, thermal controls, or a claim of an otherwise quiescent OS.

## Environment and pinned identities

| Item | Identity |
| --- | --- |
| Host | Apple M3, 8 physical/logical CPUs, 16 GiB RAM |
| OS | macOS 27.0, build 26A5416b, arm64 |
| Kernel | Darwin 27.0.0; `xnu-13432.1.9~3/RELEASE_ARM64_T8122` |
| Python | 3.14.7 |
| Workspace HEAD | `a161941442b0fa6e3ad335e9eb90549cc1661c54` (working tree was dirty before this task) |
| Ygg | 0.6.3; executable SHA-256 `55bb257792a7e13782ca016a7432ee9c6dbe5787665ad8885b0ba4abd4c09cf9` |
| OpenCode | 1.18.20; executable SHA-256 `9598c27bda0e2d88ce4db5f853e25504c20ac6152e10205785a1cf8f45559952` |
| Pi | `@earendil-works/pi-coding-agent` 0.84.4; bundle SHA-256 `5406c369954516fb56879d685e082ff9095cd6e06e41af406f394942377fd4bf` |
| Pi Node runtime | Node v26.7.0; executable SHA-256 `1ef99ea25fe70c9b67e7efe768ef8ee22148d3cabc703db6131b57aeb617d040` |
| Codex CLI | 0.149.0; executable SHA-256 `f4a74117b8142cda581c95ff753abf4508b5636d89682c1ed77e4a9249af8963` |

Resolved executable paths, exact argument vectors, full kernel text, and all
configuration-root paths are retained in the JSON. The machine-readable
[manifest](runtime-footprint-2026-08-29/manifest.json) records the same pins.

## Raw evidence

- Machine-readable aggregate: [`summary.json`](runtime-footprint-2026-08-29/summary.json)
- Environment, harness pins, and campaign manifest: [`manifest.json`](runtime-footprint-2026-08-29/manifest.json)
- Ygg: [`runtime`](runtime-footprint-2026-08-29/ygg-runtime.json), concurrency
  [`1`](runtime-footprint-2026-08-29/ygg-concurrency-1.json),
  [`2`](runtime-footprint-2026-08-29/ygg-concurrency-2.json),
  [`4`](runtime-footprint-2026-08-29/ygg-concurrency-4.json)
- OpenCode: [`runtime`](runtime-footprint-2026-08-29/opencode-runtime.json),
  concurrency [`1`](runtime-footprint-2026-08-29/opencode-concurrency-1.json),
  [`2`](runtime-footprint-2026-08-29/opencode-concurrency-2.json),
  [`4`](runtime-footprint-2026-08-29/opencode-concurrency-4.json)
- Pi: [`runtime`](runtime-footprint-2026-08-29/pi-runtime.json), concurrency
  [`1`](runtime-footprint-2026-08-29/pi-concurrency-1.json),
  [`2`](runtime-footprint-2026-08-29/pi-concurrency-2.json),
  [`4`](runtime-footprint-2026-08-29/pi-concurrency-4.json)
- Codex CLI: [`runtime`](runtime-footprint-2026-08-29/codex-runtime.json),
  concurrency [`1`](runtime-footprint-2026-08-29/codex-concurrency-1.json),
  [`2`](runtime-footprint-2026-08-29/codex-concurrency-2.json),
  [`4`](runtime-footprint-2026-08-29/codex-concurrency-4.json)
- Artifact checksums: [`SHA256SUMS`](runtime-footprint-2026-08-29/SHA256SUMS)

Runtime files contain every startup duration and every per-run memory/CPU sample.
Concurrency files contain every per-run direct-process total sample. All retained
9-run cases succeeded; incomplete shakedown invocations produced no JSON and are
not included as campaign trials.

## Safe public claim

**Scope — macOS/arm64 direct-process, isolated headless modes; 9 runs; five-second settle plus two-second sample window:** Ygg 0.6.3's median/p95 peak RSS was **7.52/7.53 MiB**, versus **385.16/403.39 MiB** for OpenCode 1.18.20 ACP, **123.70/123.89 MiB** for `@earendil-works/pi-coding-agent` 0.84.4 RPC, and **39.22/39.28 MiB** for Codex CLI 0.149.0 app-server; four-process peak-RSS totals were **30.19/30.29**, **1554.69/1565.19**, **505.36/516.62**, and **156.91/157.02 MiB**, respectively. In the separately scoped generic `--version` startup case, wall-time medians/p95s were **4.83/5.13 ms**, **239.14/354.04 ms**, **199.00/270.88 ms**, and **12.17/34.65 ms**; macOS PSS was unavailable, and median/p95 per-run peak `ps pcpu` was **0.00/0.00%** for Ygg, Pi, and Codex and **36.20/41.64%** for OpenCode at the command and sampling precision described above.
