# Contributing to Ygg

Ygg welcomes focused bug fixes, protocol improvements, terminal-correct UX
work, provider compatibility updates, tests, and documentation corrections.
The project is an alpha, so small changes with strong evidence are easier to
review and safer to ship than broad rewrites.

## Before opening a change

1. Search existing issues and pull requests for the same behavior.
2. For security-sensitive findings, stop and use the private reporting path in
   [SECURITY.md](SECURITY.md). Do not open a public issue first.
3. Keep unrelated formatting, generated output, local notes, credentials, and
   editor state out of the change.
4. Explain the user-visible problem and the boundary the fix is intended to
   preserve.

## Development setup

Ygg supports macOS and Linux and declares Rust 1.86 as its minimum supported
Rust version. Install Rust through [rustup](https://rustup.rs/) and install
`rg` (ripgrep).

```sh
git clone https://github.com/skaft-software/ygg.git
cd ygg
cargo check --workspace --all-targets --all-features --locked
```

Run the binary without installing it:

```sh
cargo run -p ygg-coding-agent --bin ygg -- --help
```

## Change guidelines

- Preserve the canonical request/session types unless the change explicitly
  requires a compatibility break.
- Keep provider-specific behavior in protocol or compatibility layers rather
  than leaking it into the agent loop.
- Treat provider output, repository content, terminal text, resource files,
  session records, and extension frames as untrusted bounded input.
- Never weaken workspace trust, tool-policy, no-follow path, cancellation,
  persistence, or redaction guarantees for convenience.
- Keep the default terminal experience stable across dark/light backgrounds,
  Unicode/ASCII, color/no-color, wide/narrow widths, and redirected output.
- Do not add network-dependent build steps. Checked-in model metadata is the
  deterministic build source.
- New dependencies need a clear product reason and must pass license,
  advisory, and source policy.

## Tests

Start with the narrowest regression that reproduces the behavior, then run the
affected crate. Before requesting review, run the full release gate:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo audit
cargo deny check
git diff --check
```

Terminal changes should include a renderer, VT100, or PTY regression when the
behavior depends on cells, cursor movement, scrollback, styles, or shutdown.
Protocol changes should include exact wire fixtures and malformed-stream
coverage. Session changes should cover restart and torn-tail behavior.

The live multimodal test is intentionally ignored unless an explicitly
configured compatible endpoint is available.

## Commits and pull requests

Use a short imperative commit subject that describes the behavior, for
example:

```text
fix: preserve tool output across reconnect
```

A pull request should state:

- what changed;
- why it was necessary;
- the user or developer impact;
- the root cause for a defect;
- the exact checks that passed;
- any known limitation or compatibility effect.

Keep generated build artifacts, local reports, credentials, sessions,
`AGENTS.md`, and private research notes out of commits. The repository
`.gitignore` contains the expected local-only paths.

## Licensing

By contributing, you agree that your contribution is distributed under the
project's [MIT License](LICENSE). Preserve upstream notices when changing
vendored or derived code; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
