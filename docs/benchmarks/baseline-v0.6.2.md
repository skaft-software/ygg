# Frozen v0.6.2 control

This file fingerprints the control used for subsequent experiments.  The
control is the annotated `v0.6.2` tag, not the working tree in the author's
main checkout.  The experiment branch is `mission/v0.6.3-next`, derived from
that commit without carrying unrelated local changes.

## Source and binary identity

| Field | Value |
| --- | --- |
| Git commit | `61677754bf69833a384bee2b29ef8eff29f37fc1` |
| Tags at commit | `v0.6.2`, `ygg-binaries-v0.6.2`, `ygg-serve-v0.6.2` |
| Workspace lockfile SHA-256 | `16df710cbe44746f9bb3961cb946b8229ad26937a40c71bc5ec2a0af463a7f63` |
| Source `resources.rs` SHA-256 | `c263faf1af37828facffcb1d13ec5c2c8aebc260dc5f0941e6e6cb447aa5b971` |
| Local release binary | `target/release/ygg` |
| Local binary SHA-256 | `51ccec35348db0b5de58fe646f699787b764f188ee6dba079a0d57fab2515603` |
| Local compiler | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Local host | Darwin 27.0, arm64, Mac15,13, 16 GiB |

The binary hash is a local rebuild identity, not the published release hash.
A published run must record its actual binary hash as well.

## Reported Terminal-Bench control

The following is the operator-supplied baseline in the mission brief.  It is
retained as reported evidence, not presented as independently reproduced in
this checkout:

- Terminal-Bench 2.1 canonical protocol
- 89 tasks, 5 attempts per task, 445 trials
- GPT-5.6 Sol, maximum reasoning
- 391 raw passes (`87.87%`); 88/89 tasks solved at least once
- raw Pass@5 `98.88%`
- 19 timeouts, 1 provider/server failure, 34 ordinary verifier failures
- approximately 458 seconds mean trial time and 3h20m campaign wall time
- approximately 1.134M reported processed input tokens and 14.5K output tokens
  per trial; the input accounting semantics required verification
- separate GLM-5.3 Flash audit: 4 confirmed reward-hacking successes, 2
  ambiguous Git-history cases, 0 harness-cheating cases, 0 refusals

The audited alternatives are `387/445` after confirmed reward hacks and
`385/445` after also removing the two ambiguous cases.  That audit was not an
official Terminal-Bench adjudication.

## Known control configuration gaps

The brief does not provide enough information to reproduce the campaign
exactly.  These fields remain **unknown** until the original Harbor run bundle
is attached:

- Harbor commit and exact Terminal-Bench 2.1 dataset revision
- benchmark timeout, concurrency, host image, and environment digest
- exact composed system prompt and any context files
- provider endpoint/account routing and server configuration
- raw trial result files and trajectory logs
- whether “processed input” included cache-read and cache-write buckets

The stated tool boundary is preserved: `read`, `write`, `edit`, and `bash` only,
with extensions disabled.  No task names, hidden solutions, verifier details,
or deleted target files are included here.

## Control verification run in this checkout

These checks were run before changes on the clean derived worktree:

```console
cargo check -p ygg-agent --all-targets --locked # passed
cargo test -p ygg-ai --lib --locked          # 229 passed
cargo test -p ygg-agent --lib --locked       # 408 passed
cargo test -p ygg-coding-agent --lib --locked # 824 passed
cargo build --release --locked -p ygg-coding-agent --bin ygg
```

The first full-workspace test invocation exceeded the execution host's 120
second command ceiling while compiling and was killed; it is not counted as a
pass.  Package-level results above are the observed control checks.

## Exact control commands

For a future canonical run, fill the missing fields first and record the final
command rather than relying on defaults.  The minimal identity checks are:

```console
git rev-parse HEAD
git describe --tags --always --dirty
sha256sum target/release/ygg   # or shasum -a 256 on macOS
./target/release/ygg --version
```

The control must be run from this commit or from the published v0.6.2 binary.
Improvements are measured separately on `mission/v0.6.3-next`.
