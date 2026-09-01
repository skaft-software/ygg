# Contributing to Ygg

Ygg welcomes focused bug fixes, protocol improvements, terminal-correct UX
work, provider compatibility updates, tests, and documentation corrections.
The project is pre-1.0, so small changes with strong evidence are easier to
review and safer to ship than broad rewrites.

## AI-assisted contributions

AI-generated issues and pull requests are welcome and will be considered. If
AI helped produce your contribution, please include a brief, genuine note from
you explaining what you observed, what you care about, or why the change
matters. Fully machine-generated requests without evidence of human review or
diligence may be treated as spam. I may not respond to messages that appear AI-
generated when I judge them to be unimportant or spam.

Please also include any relevant prompts used to reach the conclusions in an
issue or to generate a pull request. Include enough context to make clear what
you verified yourself, and redact secrets or private information. Requests for
the prompts behind a contribution are welcome too.

## Before opening a change

1. Search existing issues and pull requests for the same behavior.
2. Read [ROADMAP.md](ROADMAP.md) and the target milestone. Broad or unresolved
   ideas should begin in [Discussions](https://github.com/skaft-software/ygg/discussions)
   rather than a large implementation.
3. For security-sensitive findings, stop and use the private reporting path in
   [SECURITY.md](SECURITY.md). Do not open a public issue first.
4. Keep unrelated formatting, generated output, local notes, credentials, and
   editor state out of the change.
5. Explain the user-visible problem and the boundary the fix is intended to
   preserve.

## Roadmap and proposal lifecycle

`ROADMAP.md` is the high-level source of truth; the
[public Project](https://github.com/orgs/skaft-software/projects/5) shows accepted
work in motion, and milestones define compatibility/release buckets. Roadmap
placement is not an ETA or a promise that a specific implementation will merge.
Release notes remain authoritative for shipped behavior.

- `status/exploring`: worth discussing, not committed.
- `status/accepted`: accepted into a milestone, not actively implemented.
- `status/in-progress`: actively owned work.
- `status/shipped`: completed with release or merge evidence.

Core roadmap work must materially improve task success/reviewability, time to a
useful result, baseline/active-context footprint, integration depth/reliability,
or long-running operational reliability. Otherwise prefer deferral, an optional
package, or the extension edge. Measurements should connect systems evidence to
same-model task outcomes, human acceptance, context/schema token cost,
integration recovery, constrained-context local-model success, or
installation-to-first-success time; see the
[roadmap filter](ROADMAP.md#roadmap-filter).

A substantial proposal should state the user problem and evidence, the smallest
complete outcome, whether it belongs in core/an extension/provider data/a
frontend, explicit non-goals, a success measure, and a stop condition. Approval
of the problem does not approve a large prewritten patch. Existing external bug
reports remain product evidence: they are closed only with shipped evidence, a
verified duplicate, or an explicit product/non-goal decision.

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

Cargo does not garbage-collect stale fingerprints from old toolchains, feature
sets, or profiling runs. If `target/` grows unexpectedly, inspect it with
`du -sh target` and reclaim it with `cargo clean`; use an isolated
`CARGO_TARGET_DIR` for one-off instrumentation and benchmark builds. Build
artifacts are excluded from both Git and the Docker context.

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
cargo audit --file extensions/ygg-serve/Cargo.lock
cargo deny check
cargo deny --manifest-path extensions/ygg-serve/Cargo.toml check
(cd apps/web && npm ci && npm audit --audit-level=high)
git diff --check
```

Terminal changes should include a renderer, VT100, or PTY regression when the
behavior depends on cells, cursor movement, scrollback, styles, or shutdown.
Protocol changes should include exact wire fixtures and malformed-stream
coverage. Session changes should cover restart and torn-tail behavior.

The live multimodal test is intentionally ignored unless an explicitly
configured compatible endpoint is available. Stable Serve releases must pass the
disposable configured-provider matrix in ordinary CI. Maintainers may also run
the separately approved credentialed checks described in
[configured-provider acceptance](docs/experimental/ygg-serve/provider-acceptance.md)
against the immutable release SHA; that live check is temporarily optional and
the release workflow records an explicit waiver when it is not selected.

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
