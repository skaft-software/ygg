# Upstream inspection record

Inspected 2026-08-29 UTC.  These are source facts and operational risks, not
Ygg results.

## Revisions

- `microsoft/SWE-bench-Live` Python-only branch:
  `ad79b850f15e33992e96f03f6e97f05ddf9aa0be`.
- Hugging Face dataset `SWE-bench-Live/SWE-bench-Live` main revision:
  `a637bd46829f3132e12938c8a0ca93173a977b8e`.
- Lite parquet LFS SHA-256:
  `7ee0a75c41bfc954fd441b67ce738fc5c1cbae00721c4e30e7db4d893057c9ab`.
- The pinned Lite parquet has 300 rows and 18 columns.  The schema includes
  public issue metadata plus evaluator-only `patch`, `test_patch`,
  `FAIL_TO_PASS`, `PASS_TO_PASS`, `test_cmds`, and parser fields.

Sources:

- https://github.com/microsoft/SWE-bench-Live/tree/python-only
- https://raw.githubusercontent.com/microsoft/SWE-bench-Live/python-only/README.md
- https://huggingface.co/datasets/SWE-bench-Live/SWE-bench-Live/tree/a637bd46829f3132e12938c8a0ca93173a977b8e
- https://huggingface.co/datasets/SWE-bench-Live/SWE-bench-Live/resolve/a637bd46829f3132e12938c8a0ca93173a977b8e/data/lite-00000-of-00001.parquet

## Official evaluation path

The Python-only README recommends:

```bash
python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench-Live/SWE-bench-Live \
  --split lite \
  --namespace starryzhang \
  --predictions_path <predictions> \
  --max_workers <N> \
  --run_id <run-id>
```

The fork's evaluator builds `TestSpec` values, uses the published instance
Docker image, applies the submitted `model_patch`, runs its generated evaluator
script, parses the test log, and requires the relevant fail-to-pass and
pass-to-pass semantics.  `evaluate.py` uses this module unchanged and only
patches the host `platform.machine()` result to `x86_64` on this Apple-silicon
host, because the published Python images are x86_64.  The override is recorded
in every result.

## Gold-validation guidance and known risks

The maintainers explicitly recommend three gold evaluations on a machine and
allow a denominator consisting of instances that pass gold validation there.
They describe old live instances becoming invalid as HTTP services disappear
and note that network/float behavior can be flaky.  Relevant issue records:

- https://github.com/microsoft/SWE-bench-Live/issues/47 — repeated gold
  validation can be flaky and machine-dependent (the report concerns a later
  MultiLang split, but the operational warning is relevant).
- https://github.com/microsoft/SWE-bench-Live/issues/18 — old live instances
  can fail because HTTP services disappear or Docker/container setup errors.
- https://github.com/microsoft/SWE-bench-Live/issues/15 — maintainers confirmed
  that some pass-to-pass cases in the then-current dataset were flaky.
- https://github.com/microsoft/SWE-bench-Live/issues/55 — the later migrated
  evaluator had an XFAIL parser correction; this run freezes the Python-only
  commit and records its behavior rather than silently substituting another
  evaluator.

This package therefore excludes a task only after the complete three-run gold
procedure records it as unresolved, flaky, or missing/error; it does not
pre-delete tasks based on an image probe.  Registry rate limits are reported as
unknown in `image-preflight.json` and are not conflated with a gold result.

## Docker/image behavior

The official fork names instance images in the `starryzhang` namespace.  On
this host all observed arm64 probes were unavailable, while x86_64 images were
published for the tested sample; the runner uses `linux/amd64` emulation and
records each pulled image ID/repository digest.  The upstream `latest` tag is
mutable, so a frozen run must retain the observed per-instance digest and must
not silently refresh it during the campaign.  Docker Hub rate limiting is
handled only as an acquisition concern: the runner/evaluator can use the
read-through `mirror.gcr.io` proxy, retag the result to the upstream image name,
and record the source.  An authenticated Docker Hub pull is preferable for a
published run.

No gold patch or evaluator test patch is used in the agent container.  The
agent sees a fresh image, resets to the row's base commit, and receives only the
problem statement.
