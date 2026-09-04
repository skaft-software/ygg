# Cargo build profiles

Ygg keeps Cargo's ordinary `dev`, `test`, and `release` behavior unchanged.
Two additive profiles make CI test artifacts and profiler builds explicit. The
root workspace and the independently rooted `extensions/ygg-serve` workspace
each declare them, because Cargo resolves profile definitions from the active
workspace root.

## `ci-test`

`ci-test` inherits Cargo's `test` profile, uses `debug = "limited"`, and
disables incremental compilation. It retains filename and module information
for useful CI backtraces without full test-profile debug data or an incremental
cache that a clean CI runner will not reuse.

The Rust test jobs in CI use this profile. To reproduce their profile locally:

```sh
cargo test --workspace --all-targets --all-features --profile ci-test --locked
cargo test --workspace --doc --profile ci-test --locked
cargo test --manifest-path extensions/ygg-serve/Cargo.toml --profile ci-test --locked
```

Omit `--profile ci-test` to keep Cargo's normal local test behavior.

## `profiling`

`profiling` inherits the active workspace's `release` profile, retains its
release-like optimization choices, and overrides only the settings needed for
analysis:

- `debug = "full"` supplies full source and variable debug information;
- `lto = "off"` disables both cross-crate and local ThinLTO; and
- `strip = "none"` retains symbols.

Build a profiler-friendly Ygg binary with:

```sh
cargo build --profile profiling --locked -p ygg-coding-agent --bin ygg
```

The binary is written to `target/profiling/ygg` (or
`$CARGO_TARGET_DIR/profiling/ygg` when that variable is set). Build the
independent Serve backend with:

```sh
cargo build --manifest-path extensions/ygg-serve/Cargo.toml --profile profiling --locked
```

On platforms that split debug information, keep the generated companion debug
files beside the profiling binary when handing it to a profiler.

## Measuring a profile change

Profiles use separate target subdirectories, so compare the same command under
both profiles without deleting existing artifacts. For example:

```sh
/usr/bin/time -p cargo test -p sexy-tui-rs --lib --locked
/usr/bin/time -p cargo test -p sexy-tui-rs --lib --profile ci-test --locked
du -sh target/debug target/ci-test
```

Run the same test selection in both commands and record the test result,
elapsed time, and output-directory size. Use a quiet machine or a dedicated
target directory when collecting a reproducible performance result.
