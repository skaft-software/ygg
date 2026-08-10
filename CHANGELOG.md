# Changelog

All notable changes to Ygg are documented here. This project follows Semantic Versioning while pre-1.0 APIs may evolve rapidly.

## 0.4.0 — 2026-08-10

### Added

- Added an exception-driven Fleet command center that aggregates active and
  attention-needed sessions, supports task and project search, and returns
  directly to focused work.
- Added live session activity indicators in the Serve sidebar and line numbers
  in the graphical source-file viewer.
- Added checkout-aware self-documentation guidance to `/help` and the system
  prompt when Ygg is run from its own source tree.
- Added the `ygg_sdk` Rust library and `ygg-host` protocol-v1 NDJSON process
  interface for native integrations, including bounded streaming, inline and
  configured providers, typed media, seeded history, and durable sessions.
- Added first-use, owner-only Codex credential migration from legacy Codex and
  Hamr stores without modifying the source credentials.
- Added both Ygg binaries to deterministic, Git-tracked release archives,
  binary/source installers, containers, and release smoke tests.
- Added credential-free configured-provider acceptance for Serve covering
  authentication/model selection, streaming, tool replay, retries, explicit
  compaction, restart/resume, cancellation, and secret-safe failures, plus a
  separately gated live-provider release record.
- Added a deeply nested PDF regression proving Ygg's bounded iterative preflight
  rejects hostile nesting before parser entry.

### Changed

- Made graphical prompts submitted during an active run steer by default instead
  of becoming queued follow-ups.
- Removed the redundant interactive `/tool`, `/docs`, `/sessions`, and
  `/cycle-model` commands. The top-level `ygg sessions` command remains
  available.
- Rounded compact TUI footer costs to three significant figures.
- Made headless native-host interactions fail closed: tool confirmations are
  denied, typed input requests are cancelled, protocol frames are bounded, and
  session/image/provider inputs are validated at their system boundaries.
- Enforced dependency review, high-severity npm audit, plus `cargo audit` and
  `cargo deny` for both lockfiles on pull-request and release paths.

### Performance and reliability

- Replaced broad Serve session inventory replays with bounded, targeted catalog
  scans and carried an authorized resume session into the worker by descriptor.
  On the isolated 891-transcript fixture, startup improved from 43.01 seconds to
  19.41 seconds; direct resume of the large-session fixture completed in 2.39
  seconds.

### Fixed

- Made `/overview` bootstrap from session inventory without creating or opening
  a provisional task, including anchorless reconnect refresh and cancellation of
  stale session-selection navigation.
- Kept the focused session surface and route mode stable when session selection
  or creation fails or is retried.
- Hardened Serve catalog and resume selection against corrupt or symlinked
  transcripts, trashed sessions, unsafe sidecar metadata, inactive-branch
  configuration, and pathname replacement after resume authorization.
- Prevented no-color line truncation from appending an ANSI reset sequence to
  otherwise plain terminal output.
- Preserved launcher configuration, model selection, and provider credential
  environment variables when `ygg serve` dispatches to the exact-version
  first-party package runtime.

## 0.3.2-alpha — 2026-08-01 (experimental)

### Added

- Added target-specific prebuilt Ygg binaries and a version-pinned installer for
  GNU/Linux x86-64, macOS x86-64, and macOS Apple silicon; compiling with Cargo
  remains an explicit `--from-source` option.
- Added the minimal first-party application-extension workflow: `ygg extension
  install`, `list`, `update`, and `remove` download or accept a local package,
  verify its checksums and exact Ygg compatibility, and install it atomically.
- Added external `ygg serve` dispatch to the separately installed, loopback-only
  Ygg Serve runtime. The ordinary Ygg binary does not include the Serve backend
  or web application.
- Added source-located diagnostics for unknown global and trusted-project config
  keys, typo suggestions, and strict rejection through `--strict-config`,
  `strict_config`, or `YGG_STRICT_CONFIG`.
- Added `SessionRunOutcome` persistence, root-head checkpoints, durable run
  terminal state, and independent display metadata for steering and follow-up
  inputs.

### Ygg Serve (experimental, feature-gated)

- Added the loopback-only Ygg Serve backend under `extensions/ygg-serve/`, with
  bounded host/session contracts, deterministic snapshots and replay,
  authenticated HTTP/WebSocket transport, session supervision, evidence and
  attachment storage, document and test-result ingestion, repository context,
  project files, terminals, transcript search, runtime status, and prompt
  context.
- Added the React 19 and TypeScript web client under `apps/web/`, with responsive
  session navigation, transcript and activity views, composer controls,
  attachments, completion review, branching, project files, terminal access,
  usage and context views, settings, search, local themes and fonts, and
  Playwright acceptance coverage.
- Added the feature-gated adapter in
  `crates/ygg-coding-agent/src/extensions/serve.rs`, boundary enforcement,
  deterministic embedded assets, installed-runtime smoke coverage, and
  target-specific release packaging for GNU/Linux x86-64 and both supported
  macOS architectures.

### Documentation

- Added architecture, current-state, lifecycle safety, LAN pairing, native
  delivery, P0/P1 delivery, and web acceptance documentation for Ygg Serve.

### Changed

- Kept `auto` on the terminal-owned renderer so native scrollback, drag
  selection, logical-height chrome, stable-prefix suffix updates, and full
  retained-transcript replay on resize remain the defaults. `--mouse app`
  explicitly opts into the bounded, anchored semantic viewport.
- Preserved the `0.3.1-alpha` default prompt alignment, event rows, transcript
  surfaces, and composer spacing; this experiment changes terminal behavior,
  not the visual layout.
- Reconciled the vendored renderer's `0.3.1` package metadata and provenance with
  the Ygg workspace while keeping its unsynchronized standalone baseline explicit.

### Fixed

- Replaced line-based configuration updates with structural, comment-preserving
  TOML editing so multiline values, similarly prefixed keys, and table sections
  cannot be corrupted when Ygg persists model or reasoning selections.
- Rebuilt cached transcript rows whenever the requested width changes, even when
  no content block is dirty, preventing wide rows from leaking into a narrow
  render.
- Sanitized user input before Markdown parsing so terminal protocols cannot
  expand differently from the semantic copy projection or destabilize prompt
  geometry.
- Prevented OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages
  POSTs from being replayed after full transport timeouts or ambiguous failures
  while sending the request or awaiting response headers; replay-safe connection
  failures remain visible, cancellable, and bounded.

### Performance and reliability

- Added PTY coverage proving only explicit `app` mode negotiates mouse ownership;
  `auto`/`terminal`/`off` leave it to the terminal, and all four restore terminal
  state.
- Added regressions that grow one live Markdown block while scrolled above the
  tail and reflow cached transcript rows across consecutive wide and narrow
  renders.

### Security

- Enforced `#![forbid(unsafe_code)]` across the vendored `sexy-tui-rs` crate.

## 0.3.1-alpha — 2026-07-26

### Fixed

- Prevented finalized streamed output from being duplicated, omitted, or
  overwritten in native terminal scrollback when Markdown rows shrink, the
  terminal is resized, or scrolling and resizing overlap with generation.
- Preserved terminal-owned history across theme and disclosure repaints without
  clearing scrollback or replaying the committed transcript.

### Performance and reliability

- Replaced width-dependent commit row bookkeeping with stable semantic cursors
  that remap after reflow, including list-item and table-row boundaries for
  large streamed Markdown blocks.
- Kept deferred-history prepends and cancelled streaming retries on the same
  append-only semantic tape without renumbering retained commit identities.
- Added terminal-emulator regressions for streaming layout shrink, nonzero
  scrollback offsets, synchronized output, and width changes during generation.

## 0.3.0-alpha — 2026-07-25

### Added

- Failed interactive runs now retain the compact lifecycle row and show a
  bounded, terminal-safe diagnostic that can be copied for troubleshooting.

### Changed

- Made fenced Markdown code copy-safe with borderless, terminal-adaptive shading.
- Kept the default prompt composer unfilled and restrained at rest, with a
  model-colored perimeter shimmer only while work is active; explicit themes
  retain their authored chrome.
- Refreshed the project identity, terminal demo, installation references, and
  release documentation for the `0.3.0-alpha` line.

### Security and reliability

- Redacted request credentials and terminal controls from bounded provider and
  transport diagnostics before they can be persisted or printed.
- Serialized ChatGPT credential refresh across processes and bounded OAuth
  responses, update metadata, and other remote discovery inputs.
- Prevented atomic new-file publication from replacing a concurrently created
  target and retired extension RPC connections after interrupted framed writes.
- Centralized terminal-safe human-facing command output and strengthened session
  export redaction without changing provider-visible conversation context.

## 0.2.0-alpha — 2026-07-25

### Added

- Added durable Responses replay and multimodal reads.
- Added persistence for authoritative raw output.
- Added native compaction transport and pro mode.
- Added support for labeled custom OpenAI providers, including tool calling
  with macOS 27 Apple Foundation Models: system and private cloud compute (PCC).
- Added PDF attachment handling that resolves to a workspace path for file tools
  instead of pretending PDFs are supported as multimodal payloads.

### Changed

- Refined reasoning status presentation and hardened native Responses integration.
- Improved bounded, sanitized tool-output projections and edit/write diffs in the
  TUI while keeping raw evidence out of transcript copy.
- Updated the startup identity and release metadata for the `0.2.0-alpha` line.

### Fixed

- Prevented the shell prompt from overwriting the inline composer or leaving a
  stale footer after exiting the interactive TUI.
- Fixed release-blocking image/audio ingestion and media capability handling.

### Documentation

- Showcased sexy-tui-rs themes and added the ygg demo to the README.
- Updated installation, security-support, and release references for this alpha.

## 0.1.1-alpha — 2026-07-24

### Added

- Restored the animated, model-tinted braille-tree startup identity. The startup
  card reports the package version, selected model, reasoning configuration, and
  workspace without taking over the terminal background.
- Added entitlement-gated GPT-5.6 Pro reasoning mode for ChatGPT OAuth Pro
  routes, with independent CLI, configuration, session persistence, picker, and
  OpenAI Responses wire support.
- Added `shell_path`, `--shell-path`, and `YGG_SHELL_PATH` for explicit
  Bash-compatible shell selection.
- Added syntax-aware inline Bash command rendering, including distinct command
  names, strings, operators, flags, and arguments.

### Changed

- Renamed the model command tool from `exec` to `bash`. Every command is now
  passed intact to one Bash-compatible shell with `-c`, matching Pi's Unix
  semantics: explicit `shell_path`, `/bin/bash`, `bash` on `PATH`, then `sh`.
  Ygg does not consult `$SHELL`.
- Renamed the primary execution limit to `bash_timeout_secs`,
  `--bash-timeout-secs`, and `YGG_BASH_TIMEOUT_SECS`. The prior configuration,
  CLI, and environment spellings remain compatibility aliases.
- Reworked the transcript hierarchy around a fixed two-row live reasoning
  status that exposes only the model's latest explicit Markdown heading, uses a
  blinking model-colored dot beside a plain model-colored label, and falls back
  to `Thinking`, alongside in-place activity, bold neutral tool names, restrained metadata, quieter
  collapsed-output hints, consistent spacing, and model-provenance user prompts.
- Tool lifecycle dots now blink in lockstep while work is active, settle dimly,
  use green only for successful Bash commands, and reserve red for failures.
- Completed reasoning disappears by default and remains available through the
  global verbose disclosure mode.
- Active context telemetry now accounts for newly persisted tool results before
  the next provider usage report, so an imminent auto-compaction no longer
  appears to trigger against a stale pre-tool token count.
- Ported Pi-compatible terminal input, selection, paste, key-repeat, and overlay
  behavior while preserving native terminal selection and scrollback.
- Long-session rendering now hydrates a bounded tail, caches stable transcript
  rows, and avoids replaying or repainting committed native scrollback.
- Simplified tool output presentation: Bash output remains neutral, file tools
  expose diffs when relevant, and completed tool evidence stays collapsed unless
  explicitly expanded.

### Compatibility and reliability

- Existing sessions containing historical `exec` calls continue to render as
  Bash events; new provider schemas advertise only `bash`.
- Command cancellation and timeouts retain process-group cleanup, bounded
  stdout/stderr capture, live progress, and detached-descendant supervision.
- Added regression coverage for shell selection and Bash expansion, Pro-mode
  entitlement and persistence, synchronized event-dot animation, startup version
  display, reasoning cleanup, command syntax styling, and transcript lifecycle.
- Reduced development-profile codegen units to limit incremental artifact
  accumulation without disabling incremental compilation.

## 0.1.0-alpha — 2026-07-22

### Added

- Interactive TUI, chronological plain mode, and response-only print mode.
- OpenAI Chat, OpenAI Responses, and Anthropic Messages protocol support.
- Local OpenAI-compatible endpoint configuration and cloud/provider discovery.
- Branchable append-only sessions, usage/cost records, checkpoints, resume, and compaction.
- Bounded `read`, `search`, `edit`, `write`, and `exec` tools plus skill discovery/activation tools.
- Complete CLI tool allowlist/deny controls, offline startup, context-file disable switch, workspace trust gate, and `--version`.
- Deterministic checked-in model metadata and Unix containment profile.

### Security and reliability

- Project configuration/resources are ignored unless the workspace is explicitly trusted; project settings cannot relax global authority floors.
- Disabled tools are absent from provider schemas and execution dispatch.
- `--no-edit` disables both mutation tools.
- Descriptor-relative no-follow file operations close parent-symlink replacement races and compare target state immediately before rename.
- File/context/config/credential/session/discovery/provider-stream inputs have hard byte/count limits; special files are rejected.
- Arbitrary process and shell execution use one truthful authority gate.
- Unresolved mutating calls are never replayed after a crash.
- Session appends use interprocess locking, stale-generation detection, private permissions, and synced writes; listing is read-only.
- Cancellation propagates through autonomous compaction and prevents post-cancel summary/usage commits.
- TTY print output neutralizes terminal control sequences.

### Performance and usability

- Session resume hydrates and paints only a bounded tail instead of cloning, parsing, and rendering the entire transcript; older history materializes on demand for PageUp/PageDown, wheel navigation, selection, and semantic copy.
- Session discovery uses bounded lightweight metadata scans, and direct resume-by-id avoids parsing unrelated session bodies.
- TUI redraws emit exact changed rows, clear stale Kitty images, coalesce composer border colour runs, anchor scrolled readers while output arrives, and repeat only editing/navigation keys (never submit, close, or toggle actions).
- Provider model inventories use private, scoped cache-first startup. Built-in inventories refresh in the background; stale custom inventories refresh before catalog construction so the current launch sees server changes while retaining last-known-good models on failure.
- Connection setup and response headers have separate bounds. Custom endpoints have a configurable cold-start header allowance, while non-timeout network loss retries visibly and cancellably up to five times; a full transport timeout is not multiplied automatically.
- Ordinary final answers no longer trigger a hidden second completion-confirmation inference.
- Request sizing and transformation avoid temporary whole-history buffers and redundant context reconstruction during resume and send.
- Codex Responses requests use zstd compression, low text verbosity, and capability-gated parallel tool-call declarations without changing generic OpenAI-compatible routes.
- Streaming parsers use bounded linear scans and aggregate response budgets, including adversarial one-byte compatibility streams, pre-ID tool arguments, and Anthropic signatures.
- Interactive shell commands drain stdout and stderr concurrently under a fixed output budget, enforce the execution timeout, and terminate the complete process group on cancellation.
- Native terminal selection and scrollback are the default again; stable-prefix frame updates avoid redrawing committed history, while application-owned semantic mouse behavior remains available through `--mouse app`.
- Semantic transcript blocks use one consistent breathing row between actions without separating a tool header from its result or diff.
- Custom hlid/llama.cpp discovery reads the active nested `meta.n_ctx` context window instead of falling back to training limits or a generic default.
- Custom endpoint reasoning controls are authoritative: off-only, binary, and level-based metadata produce exactly the corresponding picker choices and wire values.
- Reasoning is collapsed by default into a stable two-line, model-colored status that surfaces only explicit model-emitted Markdown headings, falls back to `Thinking`, disappears on completion, and expands with `Ctrl+O`.
- Every bundled theme retains its authored palette, while the compiled default follows the selected model lab and resets cleanly after theme switches.
- Batched tool results retain independent bounded output allowances so a large early result cannot starve later calls in the same turn.

### Release engineering

- Added root installation/security documentation, MIT and third-party notices, checked-in architecture docs, reproducible release gates, dependency policy, a fuzz target, and complete package metadata.
- Release builds enable ThinLTO, one codegen unit, symbol stripping, and abort-on-panic to reduce startup work and binary/RSS footprint.
- The alpha release target is macOS and Linux; command execution is explicitly Unix-only.
