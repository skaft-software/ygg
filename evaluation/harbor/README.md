# Ygg + Harbor evaluation

This directory is the evaluation boundary for running Ygg through Harbor's
`BaseAgent` interface. It invokes the public headless CLI (`ygg --print`),
leaves Ygg's Rust runtime unchanged, and lets the Terminal-Bench verifier—not
the agent process exit code—decide task success.

## Pinned inputs

The adapter records its pins in [`config.py`](config.py):

| Input | Pin |
| --- | --- |
| Ygg source | `https://github.com/skaft-software/ygg.git` at `a2351bacd61311705c5480a714af65de1c6aaed6` (`0.3.2-alpha`) |
| Ygg binary | `x86_64-unknown-linux-musl`, SHA-256 `d1afd17c9b415dd2fb7b315e5ede2e17dc904fb752ee2645074a6f5dd40a3957` |
| Harbor source | `https://github.com/harbor-framework/harbor.git` at `e76f7e32f5644fb9f648cd23151aac5c67492ea0` |
| Dataset | `terminal-bench/terminal-bench-2@2.0` |
| Model | `openai/gpt-5.4` |
| Reasoning | `medium` |
| Provider credential | `OPENAI_API_KEY` |

The dataset is consumed as Harbor's published package; this adapter does not
copy, regenerate, or silently alter its tasks. Record the resolved dataset
manifest and task count from each Harbor job. A subset run must name its task
or use a local task directory explicitly.

## Prerequisites

- Linux Docker with the daemon running (the normal Terminal-Bench environment).
- Python 3.12 or newer and Harbor.
- Rust 1.86.0 and `ripgrep` when building Ygg locally.
- An OpenAI API key with access to `gpt-5.4`.
- A Linux Ygg executable built from the pinned source. Do not use an arbitrary
  `ygg` found on `PATH`.

For a fully pinned Harbor environment, use Harbor's own lock file:

```bash
git clone https://github.com/harbor-framework/harbor.git /tmp/harbor-ygg
git -C /tmp/harbor-ygg checkout e76f7e32f5644fb9f648cd23151aac5c67492ea0
cd /tmp/harbor-ygg
uv sync --frozen
```

If a checkout is not convenient, install the direct dependency in
[`requirements.txt`](requirements.txt). The checkout plus `uv.lock` is the
preferred reproducible installation because it pins Harbor's transitive
runtime dependencies too.

## Build and verify the Ygg binary

Build from a clean checkout at the Ygg pin. The release executable is built
for `x86_64-unknown-linux-musl` so it runs in Alpine and other minimal Linux
images:

```bash
git clone https://github.com/skaft-software/ygg.git /tmp/ygg-pinned
git -C /tmp/ygg-pinned checkout a2351bacd61311705c5480a714af65de1c6aaed6
rustup toolchain install 1.86.0
cd /tmp/ygg-pinned
rustup target add --toolchain 1.86.0 x86_64-unknown-linux-musl
cargo +1.86.0 build --locked --release --target x86_64-unknown-linux-musl \
  -p ygg-coding-agent --bin ygg
install -m 0755 target/x86_64-unknown-linux-musl/release/ygg /tmp/ygg-0.3.2-alpha
export YGG_BINARY=/tmp/ygg-0.3.2-alpha
export YGG_SHA256=$(sha256sum "$YGG_BINARY" | awk '{print $1}')
"$YGG_BINARY" --version
```

The repository's `deploy/Dockerfile.ygg` can build the same pinned checkout
when the host should not install Rust. Extract the executable from the image
and use that extracted file as `YGG_BINARY`:

```bash
cd /tmp/ygg-pinned
docker build --platform linux/amd64 -f deploy/Dockerfile.ygg -t ygg:0.3.2-alpha .
container=$(docker create --platform linux/amd64 ygg:0.3.2-alpha)
docker cp "${container}:/usr/local/bin/ygg" /tmp/ygg-0.3.2-alpha
docker rm "$container"
export YGG_BINARY=/tmp/ygg-0.3.2-alpha
export YGG_SHA256=$(sha256sum "$YGG_BINARY" | awk '{print $1}')
```

The binary must be made available inside each task container at
`/usr/local/bin/ygg`. For local Docker runs, a read-only bind mount is the
least invasive option:

```bash
export YGG_MOUNTS="$(python3 -c 'import json, os; print(json.dumps([{"type":"bind","source":os.environ["YGG_BINARY"],"target":"/usr/local/bin/ygg","read_only":True}]))')"
```

For a remote Harbor provider, publish or otherwise provision an image that
contains this exact executable instead of using a host bind mount. In either
case, pass `YGG_SHA256`; setup verifies the digest before copying the binary to
`/tmp/ygg` and verifies the reported version.

## Tests without a benchmark run

The command layer and redaction/session tests do not require Harbor. Harbor
adapter tests are skipped when the Harbor import is unavailable:

```bash
cd /path/to/ygg
PYTHONPATH=. python3 -m unittest discover -s evaluation/harbor/tests -v
```

With the pinned Harbor checkout active, run the same command with its Python:

```bash
PYTHONPATH=/path/to/ygg /tmp/harbor-ygg/.venv/bin/python \
  -m unittest discover -s /path/to/ygg/evaluation/harbor/tests -v
```

## Smoke run

First validate the dataset/environment independently with Harbor's oracle on
one downloaded task. Downloading to a temporary directory avoids changing the
published dataset or the working tree:

```bash
cd /tmp/harbor-ygg
export DATASET='terminal-bench/terminal-bench-2@2.0'
export SMOKE_ROOT=$(mktemp -d)
uv run harbor download "$DATASET" --output-dir "$SMOKE_ROOT"
export SMOKE_TASK=$(find "$SMOKE_ROOT" -type f -name task.toml -print -quit | xargs -r dirname)
test -n "$SMOKE_TASK"
uv run harbor trial start -p "$SMOKE_TASK" -a oracle
```

Run the same task with Ygg. Set `YGG_REPO` to the checkout containing this
adapter (it may be the same source tree used for the pinned binary):

```bash
export YGG_REPO=/path/to/ygg
export OPENAI_API_KEY='replace-me'
PYTHONPATH="$YGG_REPO" uv run harbor trial start \
  -p "$SMOKE_TASK" \
  -a evaluation.harbor.ygg_agent:Ygg \
  -m openai/gpt-5.4 \
  --mounts "$YGG_MOUNTS" \
  --agent-kwarg reasoning=medium \
  --agent-kwarg ygg_binary_sha256="$YGG_SHA256"
```

Run this from the Ygg checkout, or set `PYTHONPATH=/path/to/ygg` in the
command environment if Harbor is started from another directory. The first
run may take time to fetch the task image. The smoke run is only an adapter and
infrastructure check; it is not a benchmark score.

## Reproducible subset and full benchmark

For a named subset, use Harbor's exact task-name filter and repeat the same
binary/model/credential settings. Task names are the names in the resolved
Harbor package, not guessed filesystem names:

```bash
PYTHONPATH="$YGG_REPO" uv run harbor run \
  -d 'terminal-bench/terminal-bench-2@2.0' \
  --include-task-name '<resolved-task-name>' \
  -a evaluation.harbor.ygg_agent:Ygg \
  -m openai/gpt-5.4 \
  -n 1 \
  --mounts "$YGG_MOUNTS" \
  --agent-kwarg reasoning=medium \
  --agent-kwarg ygg_binary_sha256="$YGG_SHA256" \
  --job-name ygg-terminal-bench-subset
```

A full run removes `--include-task-name` and should start with
`-n 1` while validating the adapter. Increase concurrency only after the
single-trial result is healthy; provider rate limits and Docker resource
contention otherwise change failure rates. Keep `--n-attempts 1` for a single
reproducible pass, or report every attempt when estimating variance.

The adapter's optional inner timeout can be set with
`--agent-kwarg agent_timeout_sec=<seconds>`. Keep it below Harbor's task agent
timeout so the adapter's `timeout` wrapper gets a chance to terminate Ygg and
retain its artifacts. A timeout is classified as `benchmark_timeout`, not as a
provider failure. For long-running shell commands, pass
`--agent-kwarg bash_timeout_secs=<seconds>` to raise Ygg's per-command ceiling
(up to the CLI's 3,600-second limit); this is separate from Harbor's outer task
timeout. Record this setting when comparing runs.

## Baselines and interpretation

Use the pinned dataset with `oracle` before comparing model scores:

```bash
uv run harbor run \
  -d 'terminal-bench/terminal-bench-2@2.0' \
  -a oracle -n 1 --job-name terminal-bench-2-oracle-baseline
```

Oracle is an environment/verifier baseline: it should establish that the
published task tests and Docker setup are usable. It is not a model baseline
and should not be treated as Ygg's expected score. Compare Ygg rewards only
against runs using the same dataset pin, task subset, model, reasoning,
provider configuration, timeout settings, and concurrency. Do not report a
score from this repository until the corresponding Harbor job result has been
run; provider-side model updates can make repeated runs non-identical.

A process exit of zero means only that Ygg completed its run. Harbor's task
verifier remains authoritative for reward/task success. Non-zero outcomes are
recorded separately as `provider_failure`, `agent_failure`, or
`benchmark_timeout` so infrastructure failures are not silently reported as
failed task solutions.

## Artifacts and credential handling

Each trial's `agent/` directory retains the following evidence:

- `invocation.json` — shell-safe argv without the prompt, prompt byte length and
  SHA-256, model/reasoning/defaults, binary pin, and session path.
- `setup-*.txt` — setup output, status, and version/hash failure evidence.
- `stdout.txt`, `stderr.txt`, `exit-status.txt`, `failure-classification.txt`,
  and `run-metadata.json` — redacted execution evidence and timing.
- `sessions/**/*.jsonl` — the native append-only Ygg session, retained as the
  authoritative replay/debug artifact. Only the active branch is converted.
- `trajectory.json` — conservative ATIF-v1.6 conversion of durable user,
  assistant, tool-call, tool-result, and usage records. Harbor's `Trajectory`
  model validates it when Harbor is installed.
- `native-session-manifest.json` — paths, byte counts, and SHA-256 hashes of the
  redacted native JSONL files.
- `session-*-error.txt` — non-fatal conversion/retention diagnostics when an
  interrupted or malformed session cannot be converted.

Configured provider values and common credential-shaped tokens are redacted
from text and recursively from JSON/JSONL before conversion. The invocation
file stores only a prompt hash, not prompt text. Native JSONL is preserved as
JSONL, including a recoverable torn tail; opaque provider sidecars and records
not representable in ATIF remain in that native artifact. Do not configure
`--agent-exclude-logs` to omit `sessions/`, `trajectory.json`, or the manifest
when collecting results.

Harbor also scrubs sensitive trial files after the run. Treat job directories
as sensitive until that cleanup has completed, and do not publish raw provider
logs or API keys.

## Troubleshooting

- **`YggSetupError: version mismatch`**: check that the mounted executable was
  built from the Ygg commit above and that `--version` reports exactly
  `0.3.2-alpha`.
- **`test -x /usr/local/bin/ygg` fails**: use an absolute Linux executable,
  verify the bind mount target, or build a remote-provider image containing the
  binary.
- **No `trajectory.json`**: inspect `session-conversion-error.txt` and retain
  the native JSONL. A torn final line or an unknown provider record should not
  be “fixed” by discarding the native session.
- **`provider_failure`**: check `OPENAI_API_KEY`, model entitlement, network
  policy, rate limits, and `stderr.txt`; the adapter does not retry or turn it
  into a task result.
- **`benchmark_timeout`**: inspect `run-metadata.json` and the native session;
  increase the Harbor task timeout or reduce concurrency only after confirming
  the provider is healthy.
- **Docker permission errors while reading sessions**: allow Harbor to run its
  normal mounted-log preparation, or use a non-mounted provider so Harbor
  downloads logs after the container exits.

There is no root npm project. Rust verification is run from the repository root
with Cargo; Python adapter checks are the commands above.
