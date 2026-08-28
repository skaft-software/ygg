# Frozen v0.6.2 control

This file fingerprints the source control used by the canonical Terminal-Bench
2.1 campaign. The detailed result, sanitized metadata, audit decisions, and
checksums are in the
[durable evidence report](tb21-v0.6.2/README.md).

## Source and binary identity

| Field | Value |
| --- | --- |
| Git commit | `61677754bf69833a384bee2b29ef8eff29f37fc1` |
| Tags at commit | `v0.6.2`, `ygg-binaries-v0.6.2`, `ygg-serve-v0.6.2` |
| Workspace lockfile SHA-256 | `16df710cbe44746f9bb3961cb946b8229ad26937a40c71bc5ec2a0af463a7f63` |
| Source `resources.rs` SHA-256 | `c263faf1af37828facffcb1d13ec5c2c8aebc260dc5f0941e6e6cb447aa5b971` |
| Canonical Linux benchmark binary SHA-256 | `16036929493fb12ffc4d8a553cdfcb642c3c983fb469877403808e5aabbd5f07` |
| Earlier local Darwin rebuild SHA-256 | `51ccec35348db0b5de58fe646f699787b764f188ee6dba079a0d57fab2515603` |
| Local compiler used for that Darwin rebuild | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Local Darwin host | Darwin 27.0, arm64, Mac15,13, 16 GiB |

The benchmark binary hash—not the separate Darwin rebuild—is the executable
identity mounted into the 445 canonical trials. A rebuild is not assumed to be
byte-identical.

## Canonical Terminal-Bench control

| Field | Value |
| --- | --- |
| Harbor | `0.22.0`, commit `6ecebe4ae9910ee0b28a2e6e8fa30934c0b41dfa` |
| Dataset | `terminal-bench/terminal-bench-2-1@6` |
| Dataset digest | `sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a` |
| Model/reasoning | GPT-5.6 Sol / `max` |
| Shape | 89 tasks, 5 attempts, 445 trials |
| Concurrency/retries | 20 / 0 |
| Environment | Harbor Docker provider; shared verifier environment |
| Campaign window | 2026-08-27 21:17:10Z to 2026-08-28 00:37:20Z |

Observed scores:

- 391/445 raw Harbor passes (`87.87%`);
- 387/445 primary local surrogate/manual audit (`86.97%`);
- 385/445 strict sensitivity (`86.52%`);
- 87/89 primary and strict Pass@5 (`97.75%`).

The audit used GLM-5.3 Flash over all 391 raw successes followed by manual review.
It is not official Terminal-Bench maintainer adjudication.

## Campaign boundaries

The Ygg adapter exposed `read`, `write`, `edit`, and `bash`; extensions were
disabled. The benchmark configuration mounted the frozen binary and a disposable
read-only credential stage. It did not mount the dataset, verifier, prior
trajectories, extra instructions, skills, or MCP servers into the agent runtime.

The run did not configure Ygg's optional inner `agent_timeout_sec`. Harbor's task
timeouts remained authoritative at multiplier `1.0`. Nineteen process timeouts
overlapped the verifier outcomes: five had reward `1` and 14 had reward `0`.
One provider failure had no reward. See the evidence report for the timeout race
and complete-native token reconciliation.

## Control verification in the derived checkout

Before candidate changes, the clean derived worktree passed:

```console
cargo check -p ygg-agent --all-targets --locked
cargo test -p ygg-ai --lib --locked             # 229 passed
cargo test -p ygg-agent --lib --locked          # 408 passed
cargo test -p ygg-coding-agent --lib --locked   # 824 passed
cargo build --release --locked -p ygg-coding-agent --bin ygg
```

An earlier full-workspace invocation exceeded that execution host's 120-second
command ceiling while compiling and was not counted as a pass.

## Identity checks

```console
git checkout 61677754bf69833a384bee2b29ef8eff29f37fc1
git rev-parse HEAD
git describe --tags --always --dirty
sha256sum /path/to/ygg-0.6.2
/path/to/ygg-0.6.2 --version
```

A campaign is a reproduction of this control only when source, binary, Harbor,
dataset, model, reasoning, task shape, timeout, and concurrency identities are
reported. Use the sanitized invocation in the evidence report rather than
relying on defaults.
