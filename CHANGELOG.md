# Changelog

All notable changes to Ygg are documented here. This project follows Semantic Versioning while pre-1.0 APIs may evolve rapidly.

## 0.6.6 — 2026-09-02

### Added

- Add deterministic, no-lifecycle npm packages for the native launcher and
  supported runtimes, with exact global-update detection and protected trusted
  publishing/recovery gates.
- Add an offline Homebrew formula generator and protected tap handoff driven by
  signed immutable release metadata.
- Print an exact, shell-quoted session resume command after clean interactive
  exits and shutdown signals.

### Changed

- Cap authenticated Codex model and request budgeting at 272,000 tokens by
  default, matching Pi. Smaller provider windows remain authoritative while
  larger advertised maxima remain available as discovery metadata.
- Preserve DeepSeek V4 discovery context and modality metadata instead of
  replacing it with placeholder defaults.
- Reuse the durable session catalog and an incremental transcript-search index
  when Serve lists and searches session history.

### Fixed

- Bound environment credential reads, validate repaired tool arguments against
  their selected schemas, and retry transient remote reads with bounded
  backoff.
- Detect half-open Responses WebSockets, report closure progress, and prevent
  search subprocesses from stalling on saturated output pipes.
- Classify subagent turn-limit settlement as bounded completion and preserve the
  parent result.
- Isolate repository clean filters from status refreshes and serialize durable
  goal updates across independently opened processes.
- Resolve and launch Serve's GitHub CLI through a hardened environment and clean
  up its complete descendant process tree after cancellation or timeout.
- Keep web session selection and stalled resynchronization monotonic and
  bounded, without replaying stale snapshots over newer live state.
- Reconcile inline terminal scrollback after long transcript shrinkage without
  corrupting retained rows.

## 0.6.5 — 2026-08-30

### Added

- Added `/answer [instruction]` to request an immediate final response from
  gathered evidence. Idle runs begin without tools; active runs persist the
  directive at the next safe boundary and expose no tools thereafter.

### Changed

- Keep authenticated Codex routes on the provider's full advertised context
  window by default for in-context learning; `compaction.max_active_tokens`
  (for example 272000) optionally constrains the active working set.
- Reconcile `/context` with the agent's provider-reconciled semantic context
  breakdown, including adaptive capacity grids for large model windows.
- Raise the default model-visible tool-result cap from 16 KiB to 50 KiB while
  retaining bounded output capture and policy enforcement.

### Performance

- Keep the Responses Lite wire contract serial while host admission overlaps
  explicitly parallel-safe pure/workspace-read calls; arbitrary shell and
  mutating effects remain ordered.
- Reduce Codex tool-loop latency by enabling provider-advertised tool-call
  batching without relaxing host-side effect ordering.

### Fixed

- Correct GPT-5.6 Luna and Terra pricing across the OpenAI, OpenCode, and Codex
  pricing tables to OpenAI's published standard costs ($0.20/$1.20/$0.02/$0.25
  and $2/$12/$0.20/$2.50 per million tokens, with matching long-context tiers);
  previously displayed session costs were about 5x too high for Luna.
- Preserve GPT-5.4 Pro and GPT-5.5 Pro long-context pricing on Codex routes,
  exclude OpenAI's rejected base `gpt-5.6` alias, and refresh the checked-in
  models.dev pricing and display-name snapshots together.
- Retire a preferred Responses WebSocket before publishing a pre-generation
  connection-lifetime failure and retry that request through the HTTP fallback;
  post-generation disconnects remain terminal.
- Use the active model's adaptive accent consistently for slash completion,
  model selection, session resume, and other picker focus controls.

## 0.6.4 — 2026-08-30

### Added

- Added the optional extension `runtime_commands` negotiation feature so a
  compatibility process can expose its bounded initialization-time slash-command
  catalog without duplicating those names in a generated manifest.
- Added source-fingerprinted, version-pinned Pi compatibility links for the
  supported Pi 0.84.4 profile. Generated links remain disabled and untrusted
  until explicitly activated, reject changed source before import, and report
  unsupported compatibility surfaces instead of silently accepting them.

### Changed

- Estimate the complete next provider request before each model turn. The default
  compaction threshold now uses the context window with a fixed 16K coding-turn
  reserve (or larger advertised reasoning floor), while the provider's advertised
  output maximum remains the request ceiling and is reduced only by actual
  remaining context capacity.
- Reworked the default terminal presentation around one responsive horizontal
  grid for transcript blocks, prompt cards, composer, footer, and pickers.
  Submitted prompts retain their original model provenance colour, queued
  steering stays bounded, and narrow approval and picker layouts preserve the
  action or identity needed to use them safely.
- Keep one authoritative `Working` activity row until the owning run settles,
  including after public assistant text, and distinguish normal completion,
  completion with warnings, interruption, and failure after animation stops.

### Performance

- Advance context-capacity estimates incrementally across appended session
  messages and re-anchor them to authoritative provider usage, avoiding repeated
  whole-history reconstruction during ordinary multi-turn and tool-heavy runs.
- Reduced avoidable transcript reflow and kept active status invalidation local
  while preserving the complete retained-frame renderer contract.

### Fixed

- Never execute a tool call whose arguments may have been cut off by the
  provider's output-token limit. Ygg retains the call envelope, discards partial
  arguments, persists a paired failure result, and asks the model to reissue the
  complete call.
- Serialize durable goal-store transactions across independent handles and
  revalidate the lock identity before publication so concurrent goal turns and
  revisions cannot overwrite one another.
- Keep bounded actionable reasons visible for collapsed run and tool failures,
  preserve warning outcomes instead of painting them as success, and prevent an
  approval from being confirmed when its selected action is not visible.
- Bound nested Pi extension-manifest traversal and fail closed on incomplete or
  replaced migration inputs.

## 0.6.3 — 2026-08-28

### Added

- Added opt-in owner-only `ygg.telemetry.v1` JSONL measurements for model
  latency/TTFT, disjoint usage buckets, retries, tool timing, compaction, and
  bounded run outcomes without recording prompts, arguments, results, or
  provider payloads.
- Added `ygg doctor` for read-mostly local prerequisite, provider, and model
  visibility diagnostics.
- Added reproducible systems-benchmark and Harbor analysis tooling and a compact
  checksummed Terminal-Bench 2.1 evidence package with explicit
  surrogate-adjudication limits.

### Changed

- Render active `Working` and fixed `Thinking` headers with a bounded,
  model-adaptive shimmer. The latest explicit reasoning heading and plain
  `Ctrl+O` hint now stay on the subdued detail row without changing geometry.
- Reconciled provider, session, telemetry, and Harbor token accounting around
  disjoint uncached/cache-read/cache-write input buckets. Harbor totals now
  include every durable usage operation and cache writes without adding the
  cache-hit detail twice.
- Gave every physical retry its own request timing/TTFT lifecycle and labeled
  telemetry usage as request, operation, or cumulative scope.

### Fixed

- Fail normal terminal responses that contain no visible text, media, or tool
  call instead of silently reporting success.
- Keep a truthful TUI lifecycle for every active run: open with `Working`,
  promote to `Thinking` only on actual reasoning deltas, use visibly streaming
  assistant text as the liveness signal, restore `Working` after a completed
  turn when the run continues, and settle only at the authoritative run
  boundary.
- Replace transient tail activity with an incoming tool row without invalidating
  and reflowing long transcript history, keeping renderer animation and composer
  input responsive at the reasoning-to-tool boundary.
- Removed timer-only repaint from otherwise static transcript markers;
  intentional `Working`/`Thinking` animation invalidates only its active status
  block and leaves tool/shell dot cadence unchanged.
- Decode observed Codex Responses error envelopes with a nullable provider code
  while still rejecting a missing or nullable error message.
- Retire a preferred Responses WebSocket before publishing a provider
  connection-lifetime error, then allow a bounded pre-generation retry through
  the HTTP fallback instead of racing the poisoned socket. Never replay a
  request after generated output has been observed, and close the active socket
  when its owning response is dropped.
- Keep repeated-call diagnostics out of same-response batches and preserve
  machine-readable JSON tool results.
- Run Harbor's Docker adapter in an independently cleanable process group,
  perform TERM→KILL descendant cleanup before artifact conversion/finalization,
  and fail closed if process death cannot be verified.

## 0.6.2 — 2026-08-27

### Fixed

- Preserved bounded subagent summaries, fatal errors, usage, and the complete
  sibling roster when owning-run cleanup removes the live host tree. Missing
  active workers now settle as explicit `orphaned` diagnostic records instead
  of making every worker disappear; an identical explicit retry can replace its
  matching orphaned cache entry.
- Skipped Apple Foundation Models `/v1/models` discovery when the optional
  `fm serve` health probe says the local server is absent, eliminating the
  routine loopback connection warning without hiding errors from other custom
  providers.
- Kept the interactive composer's hardware cursor visible in both retained-frame
  renderers, including after panels, resize replays, renderer resumes, and
  extreme narrow-width fallback rendering.
- Rendered every bounded subagent in the persistent TUI transcript event and the
  compatibility activity path regardless of ordinary `Ctrl+O` tool disclosure.
- Added the one-time v0.6.2 managed-package migration so v0.6.0/v0.6.1
  first-party bundles and Ygg Serve refresh to the exact hotfix version during
  startup.

## 0.6.1 — 2026-08-27

### Changed

- Replaced Ygg's experimental semantic-commit/native-scrollback renderer with a
  direct Rust port of Pi's retained-frame algorithm. First render, changed-range
  updates, append/shrink, resize and offscreen-change replay, cursor bookkeeping,
  CSI 2026 framing, and Kitty cleanup now follow the pinned Pi control flow;
  terminal-owned resume eagerly materializes its complete active branch.
- Removed dead public API surface across the workspace and obsolete unused TUI
  compatibility adapters in `sexy-tui-rs`.
- Flattened the internal tool trait stack: `Tool` became the object-safe
  `ErasedTool` form and `TypedTool` was removed.
- Retired the `show_turn_cost` flag.
- Disabled `/theme` and theme-file discovery; v0.6.1 exposes only the compiled
  default theme in terminal and graphical Serve surfaces.
- Hoisted protocol helpers that were duplicated across crates into `ygg-ai`.
- Fixed concurrency bottlenecks so `Session::persist` and `DelegationManager`
  journal writes no longer block under load.
- Removed roughly 19 GB of accumulated workspace junk from the repository tree.
- Fixed documentation drift: subagent worker tool-scope and limit wording in
  `README.md`, `docs/extensions.md`, and `docs/design/ygg-agent.md`, plus the
  supported-fixes target in `SECURITY.md`.
- Added a zero-token `ygg migrate pi --dry-run` inventory that resolves bounded
  Pi user/project packages and resources, hashes source and lock inputs, parses
  JavaScript/TypeScript API use without executing package code, and emits human
  or schema-versioned JSON compatibility classifications.
- Added `ygg pi install`/`list` and the persistent `ygg-pi-compat` subprocess for
  explicitly trusted local Pi tools, commands, lifecycle, notification, input,
  and confirmation compatibility.
## 0.6.0 — 2026-08-23

### Added

- Deferred tool-schema loading, ported from Pi's deferred-tools design:
  tool results can now carry `added_tool_names` — the set of tools that became
  available as a consequence of that execution (for example an extension or
  MCP server registering tools on first use). Models advertising the new
  `deferred_tool_loading` capability stop re-sending announced schemas in the
  static request tool set; providers without the capability are unaffected.
  This is the load-bearing prerequisite for lazily registered extension and
  MCP tool schemas.
- Delegated workers now expose a bounded rolling `recent_tools` activity ring
  (last six tool calls with flattened argument summaries, host timestamps, and
  an error flag) on every `agent/list` record, and the `/subagents` picker rows,
  inspect detail, and headless list render the latest action live — so each
  worker answers "what is it doing right now" without opening its transcript.
- Terminal-gate rejections (`CandidateRejected`) now carry cumulative session
  cost alongside run cost, so delegated-worker spend tracks token usage between
  accepted turns instead of jumping at settlement.

- Added checksum-verified, bounded, atomic executable-extension bundles for the
  small first-party release catalog and offline/local archives. `ygg extension
  install`, `list`, `update`, and `remove` now manage API `0.2` bundles without
  enabling or trusting them; managed nested skills are discoverable but remain
  inactive until explicitly loaded.
- Added deterministic release packaging, complete tracked-file inclusion checks,
  local install/remove smoke coverage, and post-publication install/update smoke
  coverage for every catalog bundle.
- Added an interactive `/extensions` installed-bundle activation menu that
  persists only enablement, never trust, rebuilds the extension host at the idle
  boundary, and becomes read-only when a higher-precedence activation source is
  authoritative.
- Added a native `/subagents` worker browser with arrow-key selection,
  owner-bound authoritative live refresh, stable-ID focus, and scrollable,
  owner-authorized read-only delegated transcripts.
- Added `subagent_continue` to the first-party `ygg-subagents` extension:
  it steers an active worker through `agent/message` or resumes a settled
  worker as a new run of its durable session through `agent/follow_up`. A
  resumed worker keeps its conversation context, and the host clears the
  stale completion timestamp and re-anchors an elapsed wall deadline so the
  new run owns a fresh budget.
- Workers in `ygg-subagents` can now be granted `edit`, `write`, and `bash`
  per spawn through the spawn `tools` list. The host's scoped tool snapshot
  is the enforcement boundary; the default remains the read-only
  `read`/`search` pair.
- Cargo-installed binaries now embed the text documentation and materialize a
  versioned `share/ygg/` tree that refreshes after Cargo-channel updates.

### Changed

- Subagents now inherit the parent's full standard tool scope (`read`, `search`,
  `edit`, `write`, `bash`) by default, matching Claude Code's Task workers;
  pass `tools: [read, search]` to keep a worker read-only. Worker prompts,
  profiles, the spawn schema, and skill guidance were updated accordingly.
- The interactive composer's slash-command list refreshes when extension
  contributions change, and an unknown slash command now names enabled
  extensions that are not ready instead of failing with a bare error.
- Kept the target-specific Ygg Serve `package.toml` application archive distinct
  from generic executable-extension `extension.toml` bundles.
- Removed ambient executable-extension header/status/footer and presentation
  activity from TUI chrome; extension and worker state now appears only in
  explicit interactive views.
- Routed the coding product's in-harness child sessions through the trusted,
  owner-bound `ygg-subagents` extension. Ultra is now unavailable without its
  live observation service, and the root no longer receives a parallel native
  collaboration tool surface.
- Removed the fixed aggregate subagent token/cost reservation pool. First-party
  children now have fresh contexts and inherit the parent's context/output and
  optional session-token settings exactly; an unlimited parent remains unlimited.
- Raised first-party subagent limits from 2 active/16 retained children per
  owner to 8 active/32 retained, with explicit ceilings of 256 turns,
  50,000,000 microdollars, and 24 hours of wall time.
- Made per-child turn, cost, and wall-time ceilings optional in
  `agent/spawn.policy`: `null` (or omitted) inherits the parent session's
  ceiling, so an unlimited parent policy with no ceiling produces an unlimited
  child.
- Reworked the TUI composer-adjacent subagent activity strip: it appears only
  while workers are pending or running, uses `•`/`└` glyphs with model-matched
  colours, and `Ctrl+O` expands it from the two to the five most recent
  workers while it is visible (falling back to the verbose tool-output toggle
  only when no strip is shown).
- `agent/follow_up` on a settled extension child is now a resume instead of a
  rejection: the child's persistent worker task and durable transcript survive
  between runs, the stale completion timestamp is cleared, and an elapsed wall
  deadline is re-anchored from the child's requested timeout.

### Fixed

- Clear composer-adjacent subagent activity when hydrating a different session,
  so worker telemetry cannot persist across a session switch.

## 0.5.0 — 2026-08-19

### Added

- Added executable-extension API `0.2` with exact feature negotiation,
  host-capped concurrency, one serialized writer, cooperative cancellation with
  bounded tombstones/escalation, request-scoped progress, and correlated child
  requests, including ephemeral text/secret input that is never logged or
  persisted.
- Added typed text/image/audio tool-result parts, declared output schemas,
  validated structured content and retained metadata, plus generation-scoped
  artifact publication from bounded inline data or a verified scratch path.
- Added best-effort session, turn, and tool lifecycle observations; host-owned
  policy-intent classification (currently default-deny without a domain
  adapter); optional original-intent-bound single-use approval redemption;
  manifest-allowlisted, owner-scoped secret brokerage; host-derived
  session/process ownership; inspectable process health; and deadline-bounded
  drain/reload.
- Added transactional `tools/register` and `tools/unregister`, per-process
  catalog epochs, provider-turn schema/implementation snapshots, and stable
  owner ordering so long-lived extensions can publish changing tool catalogs
  without rebuilding the agent.
- Added optional extension-to-host child-agent sessions with scoped spawn,
  message, follow-up, list, wait, and interrupt operations. Ownership is derived
  from the parent request rather than trusted child JSON.
- Added automatic supervision after a successful extension handshake: crashes
  withdraw live tools, restart with bounded jittered exponential backoff, and
  remain fenced from explicit shutdown and stale generations.
- Added dependency-free Python SDK `0.2` support for negotiation, concurrent
  dispatch, cancellation tokens, progress, artifacts, lifecycle handlers,
  policy/approval requests, secret lookup, live tool catalogs, child-agent
  sessions, and graceful drain while retaining API `0.1` wire support.
- Migrated the Caffeinate example to a supervised API `0.2` lifecycle
  extension that reference-counts active turns. Sleep inhibition is no longer
  native agent-kernel behavior.
- Added durable, provider-neutral session goals: a per-session objective with a
  bounded turn budget, stored owner-only in the session's private `.serve/goals`
  directory, that the agent continues toward after each settled turn until it
  reports explicit completion or becomes blocked. `/goal <objective>`, `/goal
  status`, `/goal pause`, `/goal resume`, and `/goal clear` manage the goal in
  the terminal, and the graphical Serve shows the same goal with a badge and a
  composer command.
- Added metadata-gated Ultra reasoning with automatic bounded V2 task
  delegation, including spawn, follow-up, peer messaging, race-free waiting,
  interruption, descendant cancellation, and inheritance of the root's approved
  tools and execution policies.
- Added isolated child sessions and descriptor-relative private team storage
  with synced `provenance.jsonl`; delegation fails closed if provenance cannot be
  persisted.
- Added `ygg update` with install-method detection, release checks, and pinned
  installer or Cargo execution outside the running process.

### Changed

- Froze executable-extension API `0.1` as a backward-compatible,
  text-oriented contract. Its optional tool metadata remains accepted but is
  discarded, and its `after_response` hook remains completion-only.
- Superseded the earlier host-owned capability design with a tiny agent kernel:
  JSON-RPC subprocess extensions own MCP, browser, web-search, computer-use,
  memory, LSP, subagent-orchestration, and caffeinate domain behavior.
- Replaced the obsolete OpenAI Codex Pro wire mode with provider-advertised
  `ultra` effort. Persisted Pro selections remain readable and migrate only when
  the route advertises Ultra plus V2 collaboration and the host can execute it;
  no codec emits `reasoning.mode`.
- Updated authenticated Codex discovery to parse advertised reasoning levels,
  `use_responses_lite`, and `multi_agent_version: "v2"` using cache schema 2 and
  client version `0.147.0`. Offline or incomplete metadata does not infer those
  capabilities from model names or OAuth plans.
- Matched current Responses Lite ordinary and compact requests, including
  explicit `parallel_tool_calls: false`, developer-message instructions,
  input-item `additional_tools`, `reasoning.context: "all_turns"`, and narrow
  removal of image-detail hints.
- Swapped the safety default to full host access (`UnsafeHost`) and replaced
  `--safe` with canonical `--safe-mode` for approval-required execution. The
  obsolete `--yolo` flag and its configuration/environment aliases are no longer
  accepted; `--safe` remains a hidden compatibility alias for `--safe-mode`.
- Rewrote bash safety classification with tree-sitter parsing: a strict
  word-only command allowlist joined by `&&`, `||`, `;`, and `|`, with bounded
  recursion through shell wrapper invocations, decides which `bash` commands
  the `Controlled` profile auto-approves as read-only; anything else still
  requires one-shot approval.
- Reworked local compaction around Pi-style token retention: the most recent
  20,000 tokens of conversation are kept verbatim
  (`compaction.keep_recent_tokens`), a turn that crosses the retention boundary
  is split so its older prefix is summarized with its own bounded output budget
  while its recent suffix is retained, and the checkpoint summary uses a
  bounded output budget. Legacy compaction mode and policy settings remain
  accepted.
- Moved TUI liveness out of the composer: the composer now sits in a static
  model-accent frame while the transcript owns animation, inline slash, file,
  and `@` completions render below the composer, and reasoning presentation
  uses compact `Working` and `Compacting context` labels instead of animated
  shimmer.

### Performance

- Session discovery now keeps JSONL transcripts authoritative while caching
  bounded title projections in a disposable workspace SQLite catalog, and
  streams transcript replay and metadata scans so large sessions no longer need
  a second whole-file buffer.
- Reduced per-frame rendering work in long TUI sessions.

### Fixed

- Kept slash/path completion, panel, and report surfaces out of
  terminal-owned history, and reconciled streamed-layout contractions without
  punching blank rows into the live grid. `/context` now repaints completely
  after a tall slash popup while finalized transcript rows remain exactly once
  in native scrollback.
- Made delegated mailbox delivery transactional: bounded UTF-8 pages remain
  leased until an untruncated tool result is durably appended, and failed or
  truncated persistence restores the page.
- Preserved accepted steering, queued prompt messages, and follow-ups across
  interruption, backpressure, and failed runs, retaining queue reservations until
  durable prompt delivery; rejected oversize or overflowing durable tasks and
  messages instead of truncating, evicting, or silently releasing them.
- Rejected stale, future-dated, malformed, incomplete, and inconsistent Codex
  cache metadata before capability activation, stripped dynamic capabilities
  offline, and applied legacy-Pro migration precedence consistently at rebuilds.
- Propagated the effective current root prompt to newly spawned children and
  securely rolled failed delegation activation back to its exact empty private
  team directory.
- Omitted `tool_choice` from OpenAI Chat requests when no tools are enabled so
  the field is no longer sent without a tool list.
- Made the signed binary installer portable across supported macOS and Linux
  targets, and added replacement coverage proving a v0.4.0 installation upgrades
  both binaries and packaged documentation without touching user data.
- Surfaced terminal provider failures in the Serve web UI and retained
  failed-provider diagnostics after later work.
- Bounded the Caffeinate example's sleep inhibition: if Ygg cannot report a
  terminal outcome, such as an interrupted run, the inhibitor now expires after
  30 minutes.

The `0.2` foundation does not yet ship first-party MCP, browser, web-search,
computer-use, memory, LSP, or subagent-orchestration packages. Supervision
begins only after a successful initial handshake and does not yet detect a hung
but still-open child; a full application rebuild still recreates extension
processes. The coding product does not yet configure an approval UI adapter or
secret provider. OS-level CPU/RSS/FD/PID quotas also remain future kernel work.

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
  compaction, restart/resume, cancellation, and secret-safe failures, plus an
  optional protected live-provider acceptance workflow.
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
